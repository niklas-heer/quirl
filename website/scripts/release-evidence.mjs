import { existsSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const headerBegin = '<!-- quirl-release-evidence:v1';
const headerEnd = '-->';
const projectionBegin = '<!-- BEGIN QUIRL RELEASE EVIDENCE STATUS -->';
const projectionEnd = '<!-- END QUIRL RELEASE EVIDENCE STATUS -->';
const sourceBytesMax = 1024 * 1024;
const gitOutputBytesMax = 256 * 1024;
const gitTimeoutMs = 10_000;

export const evidenceSourcePath = 'docs/benchmarks/release-v1.0.md';
export const generatedEvidenceMirrorPath =
  'website/content/docs/project/release-0.1/performance-record.mdx';

export const projectionPaths = [
  'README.md',
  'CHANGELOG.md',
  'docs/language-design.md',
  'docs/release-checklist.md',
  'docs/security-accessibility-audit-v0.1.md',
  'website/content/docs/index.mdx',
  'website/content/docs/getting-started/status-and-platforms.mdx',
];

export const generatedProjectionPaths = [
  'website/content/docs/project/changelog.mdx',
  'website/content/docs/architecture/product-and-language-design.mdx',
  'website/content/docs/project/release-0.1/release-checklist.mdx',
  'website/content/docs/project/release-0.1/security-accessibility-audit.mdx',
];

export const evidenceOnlyPaths = new Set([
  ...projectionPaths,
  ...generatedProjectionPaths,
  evidenceSourcePath,
  generatedEvidenceMirrorPath,
]);

function readBounded(path) {
  const size = statSync(path).size;
  if (size > sourceBytesMax) {
    throw new Error(`${path} exceeds the ${sourceBytesMax}-byte release-evidence input limit`);
  }
  const contents = readFileSync(path, 'utf8');
  if (Buffer.byteLength(contents, 'utf8') > sourceBytesMax) {
    throw new Error(`${path} exceeds the ${sourceBytesMax}-byte release-evidence input limit`);
  }
  return contents.replaceAll('\r\n', '\n');
}

function exactUtcTimestamp(value) {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/.test(value)) return false;
  const instant = new Date(value);
  return !Number.isNaN(instant.valueOf()) && instant.toISOString() === value.replace('Z', '.000Z');
}

export function parseEvidenceMetadata(markdown) {
  if (Buffer.byteLength(markdown, 'utf8') > sourceBytesMax) {
    throw new Error(`release evidence source exceeds ${sourceBytesMax} bytes`);
  }

  const beginIndex = markdown.indexOf(headerBegin);
  if (beginIndex < 0) throw new Error('release evidence metadata header is missing');
  if (markdown.indexOf(headerBegin, beginIndex + headerBegin.length) >= 0) {
    throw new Error('release evidence metadata header is duplicated');
  }
  const endIndex = markdown.indexOf(headerEnd, beginIndex + headerBegin.length);
  if (endIndex < 0) throw new Error('release evidence metadata header is not terminated');

  const header = markdown.slice(beginIndex, endIndex + headerEnd.length);
  const lines = header.split('\n');
  const expectedKeys = [
    'status',
    'measured-candidate-commit',
    'evidence-documentation-commit',
    'artifact-sha256',
    'measured-at',
    'measurement-platform-scope',
  ];
  if (lines[0] !== headerBegin || lines.at(-1) !== headerEnd) {
    throw new Error('release evidence metadata delimiters must occupy complete lines');
  }
  if (lines.length !== expectedKeys.length + 2) {
    throw new Error('release evidence metadata must contain exactly the v1 fields');
  }

  const values = {};
  for (const [index, key] of expectedKeys.entries()) {
    const prefix = `${key}: `;
    const line = lines[index + 1];
    if (!line.startsWith(prefix)) {
      throw new Error(`release evidence metadata field ${index + 1} must be ${key}`);
    }
    const value = line.slice(prefix.length);
    if (value.length === 0) throw new Error(`release evidence metadata field ${key} is empty`);
    values[key] = value;
  }

  if (!['historical', 'current'].includes(values.status)) {
    throw new Error('release evidence status must be historical or current');
  }
  const commitPattern = /^[0-9a-f]{40}$/;
  if (!commitPattern.test(values['measured-candidate-commit'])) {
    throw new Error('measured candidate must be a full lowercase 40-digit commit');
  }
  const evidenceCommit = values['evidence-documentation-commit'];
  if (evidenceCommit !== 'none' && !commitPattern.test(evidenceCommit)) {
    throw new Error('evidence commit must be none or a full lowercase 40-digit commit');
  }
  if (!/^[0-9a-f]{64}$/.test(values['artifact-sha256'])) {
    throw new Error('artifact digest must be a full lowercase 64-digit SHA-256');
  }
  if (!exactUtcTimestamp(values['measured-at'])) {
    throw new Error('measurement time must be a valid whole-second UTC RFC3339 timestamp');
  }
  if (!/^[a-z0-9][a-z0-9.-]{0,159}$/.test(values['measurement-platform-scope'])) {
    throw new Error('measurement platform scope must be a bounded lowercase slug');
  }

  return {
    status: values.status,
    measuredCandidateCommit: values['measured-candidate-commit'],
    evidenceDocumentationCommit: evidenceCommit === 'none' ? null : evidenceCommit,
    artifactSha256: values['artifact-sha256'],
    measuredAt: values['measured-at'],
    measurementPlatformScope: values['measurement-platform-scope'],
  };
}

function evidenceCommitClause(metadata) {
  return metadata.evidenceDocumentationCommit
    ? ` Evidence commit \`${metadata.evidenceDocumentationCommit}\` documents that measurement.`
    : '';
}

function projectionMarkers(format) {
  if (format === 'mdx') {
    return {
      begin: '{/* BEGIN QUIRL RELEASE EVIDENCE STATUS */}',
      end: '{/* END QUIRL RELEASE EVIDENCE STATUS */}',
    };
  }
  return { begin: projectionBegin, end: projectionEnd };
}

function projectionFormat(path) {
  return path.endsWith('.mdx') ? 'mdx' : 'markdown';
}

export function renderProjection(metadata, format = 'markdown') {
  const markers = projectionMarkers(format);
  const identity = `candidate \`${metadata.measuredCandidateCommit}\` and artifact \`${metadata.artifactSha256}\``;
  const statusLines =
    metadata.status === 'historical'
      ? [
          `> **Release evidence status — historical.** P14 evidence for measured ${identity} is historical.`,
          `>${evidenceCommitClause(metadata)} It is not evidence for the corrected implementation, which has no fresh exact-candidate measurement.`,
        ]
      : [
          `> **Release evidence status — current.** The record for measured ${identity} is current exact-candidate evidence.`,
          ...(metadata.evidenceDocumentationCommit
            ? [`>${evidenceCommitClause(metadata)}`]
            : []),
        ];
  statusLines.push(
    '> Human review on named Linux and macOS terminals, remote-PTY review, and real-terminal demo review remain incomplete.',
  );
  return `${markers.begin}\n${statusLines.join('\n')}\n${markers.end}`;
}

export function renderWebsiteEvidenceNotice(metadata) {
  const label = metadata.status === 'historical' ? 'Historical artifact evidence' : 'Current exact-candidate evidence';
  const relationship =
    metadata.status === 'historical'
      ? 'It is not evidence for the corrected implementation.'
      : 'It is the current exact-candidate automated performance record.';
  return `> **${label}:** This record measures candidate \`${metadata.measuredCandidateCommit}\` and artifact \`${metadata.artifactSha256}\`. ${relationship} Human Linux/macOS terminal, remote-PTY, and real-terminal demo review remain incomplete.\n\n`;
}

export function replaceProjection(contents, metadata, path) {
  const format = projectionFormat(path);
  const markers = projectionMarkers(format);
  const beginIndex = contents.indexOf(markers.begin);
  if (beginIndex < 0) throw new Error(`release evidence projection is missing from ${path}`);
  if (contents.indexOf(markers.begin, beginIndex + markers.begin.length) >= 0) {
    throw new Error(`release evidence projection is duplicated in ${path}`);
  }
  const endIndex = contents.indexOf(markers.end, beginIndex + markers.begin.length);
  if (endIndex < 0) throw new Error(`release evidence projection is not terminated in ${path}`);
  const afterEnd = endIndex + markers.end.length;
  return `${contents.slice(0, beginIndex)}${renderProjection(metadata, format)}${contents.slice(afterEnd)}`;
}

export function convertProjectionMarkersToMdx(contents) {
  const markdown = projectionMarkers('markdown');
  const mdx = projectionMarkers('mdx');
  return contents.replaceAll(markdown.begin, mdx.begin).replaceAll(markdown.end, mdx.end);
}

export function validateProjection(contents, metadata, path) {
  const expected = replaceProjection(contents, metadata, path);
  if (expected !== contents) throw new Error(`release evidence projection is stale in ${path}`);

  const format = projectionFormat(path);
  const markers = projectionMarkers(format);
  const beginIndex = contents.indexOf(markers.begin);
  const endIndex = contents.indexOf(markers.end, beginIndex + markers.begin.length);
  const outsideProjection = `${contents.slice(0, beginIndex)}${contents.slice(endIndex + markers.end.length)}`;
  const explicitStatusClaims = [
    /\brelease evidence (?:status )?(?:is|remains|—|:)\s*(?:historical|current)\b/i,
    /\b(?:benchmark|performance) record (?:is|remains) (?:historical|current)\b/i,
    /\*\*(?:current exact-candidate evidence|historical P14 evidence|historical artifact evidence)[.:]\*\*/i,
    /\bexact-candidate automated checks pass\b/i,
  ];
  if (explicitStatusClaims.some((pattern) => pattern.test(outsideProjection))) {
    throw new Error(`release evidence status claim exists outside the canonical projection in ${path}`);
  }
}

export function synchronizeProjectionFiles(repositoryRoot, metadata, checkMode) {
  const stale = [];
  for (const path of projectionPaths) {
    const absolutePath = join(repositoryRoot, path);
    if (!existsSync(absolutePath)) throw new Error(`release evidence projection file is missing: ${path}`);
    const contents = readBounded(absolutePath);
    const expected = replaceProjection(contents, metadata, path);
    if (expected === contents) {
      if (checkMode) validateProjection(contents, metadata, path);
      continue;
    }
    if (checkMode) stale.push(path);
    else writeFileSync(absolutePath, expected);
  }
  if (stale.length > 0) {
    throw new Error(`release evidence projections are stale: ${stale.join(', ')}`);
  }
}

function extractSingleField(markdown, pattern, field) {
  const matches = [...markdown.matchAll(pattern)];
  if (matches.length !== 1) {
    throw new Error(`benchmark ${field} field must occur exactly once`);
  }
  return matches[0][1];
}

export function validateRecordIdentity(markdown, metadata) {
  const expectedStatus = metadata.status === 'historical'
    ? 'Historical P14 evidence'
    : 'Current exact-candidate evidence';
  const visibleStatus = extractSingleField(
    markdown,
    /^> \*\*(Historical P14 evidence|Current exact-candidate evidence)\.\*\*/gm,
    'visible status',
  );
  if (visibleStatus !== expectedStatus) {
    throw new Error('benchmark visible status disagrees with canonical metadata');
  }
  const visibleCandidate = extractSingleField(
    markdown,
    /^\*\*Candidate A:\*\* `([^`\n]+)`$/gm,
    'candidate',
  );
  if (visibleCandidate !== metadata.measuredCandidateCommit) {
    throw new Error('benchmark candidate prose disagrees with canonical metadata');
  }
  const visibleArtifact = extractSingleField(
    markdown,
    /^\*\*Artifact SHA-256:\*\*\n`([^`\n]+)`$/gm,
    'artifact',
  );
  if (visibleArtifact !== metadata.artifactSha256) {
    throw new Error('benchmark artifact prose disagrees with canonical metadata');
  }
  const measuredDate = metadata.measuredAt.slice(0, 10);
  const [year, month, day] = measuredDate.split('-');
  const monthNames = [
    'January', 'February', 'March', 'April', 'May', 'June',
    'July', 'August', 'September', 'October', 'November', 'December',
  ];
  const humanTimestamp = `${Number(day)} ${monthNames[Number(month) - 1]} ${year} at ${metadata.measuredAt.slice(11, 19)} UTC`;
  const visibleTimestamp = extractSingleField(
    markdown,
    /^\*\*Measured:\*\* (.+)$/gm,
    'measurement time',
  );
  if (visibleTimestamp !== humanTimestamp) {
    throw new Error('benchmark measurement time prose disagrees with canonical metadata');
  }
  const visiblePlatform = extractSingleField(
    markdown,
    /^\*\*Measurement platform scope:\*\*\n`([^`\n]+)`$/gm,
    'platform scope',
  );
  if (visiblePlatform !== metadata.measurementPlatformScope) {
    throw new Error('benchmark platform scope prose disagrees with canonical metadata');
  }
}

export function validateAttribution(
  metadata,
  git,
  { allowImplicitCurrentBeforeCommit = false } = {},
) {
  if (!git.commitExists(metadata.measuredCandidateCommit)) {
    throw new Error(`measured candidate commit does not exist: ${metadata.measuredCandidateCommit}`);
  }
  if (
    metadata.evidenceDocumentationCommit &&
    !git.commitExists(metadata.evidenceDocumentationCommit)
  ) {
    throw new Error(`evidence documentation commit does not exist: ${metadata.evidenceDocumentationCommit}`);
  }
  if (metadata.status !== 'current') return;
  if (!metadata.evidenceDocumentationCommit && allowImplicitCurrentBeforeCommit) return;

  const evidenceCommit = metadata.evidenceDocumentationCommit ?? git.headCommit();
  if (!git.commitExists(evidenceCommit)) {
    throw new Error(`current evidence commit does not exist: ${evidenceCommit}`);
  }
  const parents = git.commitParents(evidenceCommit);
  if (parents.length !== 1 || parents[0] !== metadata.measuredCandidateCommit) {
    throw new Error('current evidence commit must be the direct single-parent child of the measured candidate');
  }
  const changedPaths = git.commitPaths(evidenceCommit);
  if (!changedPaths.includes(evidenceSourcePath)) {
    throw new Error(`current evidence commit must change ${evidenceSourcePath}`);
  }
  const forbidden = changedPaths.filter((path) => !evidenceOnlyPaths.has(path));
  if (forbidden.length > 0) {
    throw new Error(`current evidence commit changes non-evidence paths: ${forbidden.join(', ')}`);
  }

  const headCommit = git.headCommit();
  if (headCommit === evidenceCommit) return;
  if (!git.isAncestor(evidenceCommit, headCommit)) {
    throw new Error('current evidence commit must be an ancestor of HEAD');
  }
  const laterForbidden = git
    .pathsTouchedBetween(evidenceCommit, headCommit)
    .filter((path) => !evidenceOnlyPaths.has(path));
  if (laterForbidden.length > 0) {
    throw new Error(`runtime changes after current evidence commit invalidate it: ${laterForbidden.join(', ')}`);
  }
}

function runGit(repositoryRoot, arguments_) {
  const result = spawnSync('git', arguments_, {
    cwd: repositoryRoot,
    encoding: 'utf8',
    maxBuffer: gitOutputBytesMax,
    timeout: gitTimeoutMs,
  });
  return result;
}

export function repositoryGit(repositoryRoot) {
  return {
    commitExists(commit) {
      const result = runGit(repositoryRoot, ['cat-file', '-e', `${commit}^{commit}`]);
      return result.status === 0;
    },
    headCommit() {
      const result = runGit(repositoryRoot, ['rev-parse', 'HEAD']);
      if (result.status !== 0) throw new Error(`cannot resolve HEAD: ${result.stderr.trim()}`);
      return result.stdout.trim();
    },
    commitParents(commit) {
      const result = runGit(repositoryRoot, ['show', '-s', '--format=%P', commit]);
      if (result.status !== 0) throw new Error(`cannot inspect evidence parent: ${result.stderr.trim()}`);
      const parents = result.stdout.trim();
      return parents.length === 0 ? [] : parents.split(' ');
    },
    commitPaths(commit) {
      const result = runGit(repositoryRoot, [
        'diff-tree', '--root', '--no-commit-id', '--name-only', '-r', commit,
      ]);
      if (result.status !== 0) throw new Error(`cannot inspect evidence diff: ${result.stderr.trim()}`);
      return result.stdout.trim().split('\n').filter(Boolean);
    },
    isAncestor(ancestor, descendant) {
      return runGit(repositoryRoot, ['merge-base', '--is-ancestor', ancestor, descendant]).status === 0;
    },
    pathsTouchedBetween(ancestor, descendant) {
      const result = runGit(repositoryRoot, [
        'log', '-m', '--format=', '--name-only', `${ancestor}..${descendant}`,
      ]);
      if (result.status !== 0) throw new Error(`cannot inspect changes after evidence: ${result.stderr.trim()}`);
      return result.stdout.trim().split('\n').filter(Boolean);
    },
  };
}

export function loadEvidence(
  repositoryRoot,
  { allowImplicitCurrentBeforeCommit = false } = {},
) {
  const source = readBounded(join(repositoryRoot, evidenceSourcePath));
  const metadata = parseEvidenceMetadata(source);
  validateRecordIdentity(source, metadata);
  validateAttribution(metadata, repositoryGit(repositoryRoot), {
    allowImplicitCurrentBeforeCommit,
  });
  return { metadata, source };
}

export function validateRepositoryProjections(repositoryRoot, metadata) {
  synchronizeProjectionFiles(repositoryRoot, metadata, true);
  for (const path of generatedProjectionPaths) {
    validateProjection(readBounded(join(repositoryRoot, path)), metadata, path);
  }
  const mirror = readBounded(join(repositoryRoot, generatedEvidenceMirrorPath));
  const notice = renderWebsiteEvidenceNotice(metadata);
  const first = mirror.indexOf(notice);
  if (first < 0 || mirror.indexOf(notice, first + notice.length) >= 0) {
    throw new Error(`generated mirror status is not exact in ${generatedEvidenceMirrorPath}`);
  }
}
