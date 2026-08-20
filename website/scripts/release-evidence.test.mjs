import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import {
  evidenceSourcePath,
  parseEvidenceMetadata,
  renderProjection,
  repositoryGit,
  validateAttribution,
  validateProjection,
  validateRecordIdentity,
} from './release-evidence.mjs';

const candidate = '7bf188344ca61798a3cd8657787eacb8ec26ef84';
const evidence = '05df4b09349394cfd34f24d514e0e005365d0ed8';
const artifact = '81cd33388cf610a7aac23a9781dbf2771b5dfb6b01b17522c2257cd3676d0ae6';

function header(overrides = {}) {
  const values = {
    status: 'historical',
    measuredCandidate: candidate,
    evidenceCommit: evidence,
    artifact,
    measuredAt: '2026-08-16T18:51:07Z',
    platform: 'macos-15.7.9-24g830-apple-m2-pro-aarch64-apple-darwin-automated-pty',
    ...overrides,
  };
  return `<!-- quirl-release-evidence:v1
status: ${values.status}
measured-candidate-commit: ${values.measuredCandidate}
evidence-documentation-commit: ${values.evidenceCommit}
artifact-sha256: ${values.artifact}
measured-at: ${values.measuredAt}
measurement-platform-scope: ${values.platform}
-->`;
}

function fakeGit({
  existing = [candidate, evidence],
  head = evidence,
  parents = [candidate],
  paths = [evidenceSourcePath],
  isAncestor = true,
  pathsTouchedBetween = [],
} = {}) {
  return {
    commitExists: (commit) => existing.includes(commit),
    headCommit: () => head,
    commitParents: () => parents,
    commitPaths: () => paths,
    isAncestor: () => isAncestor,
    pathsTouchedBetween: () => pathsTouchedBetween,
  };
}

test('parser_accepts_exact_historical_metadata', () => {
  const metadata = parseEvidenceMetadata(header());
  assert.equal(metadata.status, 'historical');
  assert.equal(metadata.measuredCandidateCommit, candidate);
  assert.equal(metadata.evidenceDocumentationCommit, evidence);
  assert.equal(metadata.artifactSha256, artifact);
});

test('parser_rejects_malformed_unknown_and_non_lowercase_metadata', () => {
  assert.throws(() => parseEvidenceMetadata(header().replace('artifact-sha256:', 'digest:')));
  assert.throws(() => parseEvidenceMetadata(header({ status: 'superseded' })), /historical or current/);
  assert.throws(
    () => parseEvidenceMetadata(header({ measuredCandidate: candidate.toUpperCase() })),
    /full lowercase/,
  );
  assert.throws(
    () => parseEvidenceMetadata(header({ artifact: artifact.slice(0, -1) })),
    /full lowercase/,
  );
});

test('historical_metadata_requires_named_commits_to_exist', () => {
  const metadata = parseEvidenceMetadata(header());
  assert.throws(
    () => validateAttribution(metadata, fakeGit({ existing: [candidate] })),
    /evidence documentation commit does not exist/,
  );
  assert.throws(
    () => validateAttribution(metadata, fakeGit({ existing: [evidence] })),
    /measured candidate commit does not exist/,
  );
});

test('current_metadata_requires_direct_single_parent_evidence_only_diff', () => {
  const metadata = parseEvidenceMetadata(header({ status: 'current' }));
  assert.throws(
    () => validateAttribution(metadata, fakeGit({ parents: ['1'.repeat(40)] })),
    /direct single-parent child/,
  );
  assert.throws(
    () => validateAttribution(metadata, fakeGit({ parents: [candidate, '2'.repeat(40)] })),
    /direct single-parent child/,
  );
  assert.throws(
    () => validateAttribution(metadata, fakeGit({ paths: [evidenceSourcePath, 'crates/quirl-cli/src/main.rs'] })),
    /non-evidence paths/,
  );
  assert.throws(
    () => validateAttribution(metadata, fakeGit({ paths: ['README.md'] })),
    /must change docs\/benchmarks\/release-v1.0.md/,
  );
});

test('current_metadata_accepts_exact_named_and_implicit_evidence_commits', () => {
  const named = parseEvidenceMetadata(header({ status: 'current' }));
  assert.doesNotThrow(() => validateAttribution(named, fakeGit()));

  const implicit = parseEvidenceMetadata(
    header({ status: 'current', evidenceCommit: 'none' }),
  );
  assert.equal(implicit.evidenceDocumentationCommit, null);
  assert.doesNotThrow(() => validateAttribution(implicit, fakeGit()));
});

test('named_current_metadata_is_invalidated_by_later_runtime_changes_even_when_reverted', () => {
  const metadata = parseEvidenceMetadata(header({ status: 'current' }));
  // The Git seam reports every touched path, so a final tree matching the
  // evidence commit cannot conceal an intervening runtime edit and revert.
  assert.throws(
    () =>
      validateAttribution(
        metadata,
        fakeGit({
          head: 'f'.repeat(40),
          pathsTouchedBetween: ['crates/quirl-cli/src/main.rs'],
        }),
      ),
    /runtime changes after current evidence commit invalidate it/,
  );
  assert.throws(
    () =>
      validateAttribution(
        metadata,
        fakeGit({ head: 'f'.repeat(40), isAncestor: false }),
      ),
    /ancestor of HEAD/,
  );
  assert.doesNotThrow(() =>
    validateAttribution(
      metadata,
      fakeGit({ head: 'f'.repeat(40), pathsTouchedBetween: ['README.md'] }),
    ),
  );
});

test('implicit_current_metadata_can_generate_before_commit_but_default_check_fails', () => {
  const implicit = parseEvidenceMetadata(
    header({ status: 'current', evidenceCommit: 'none' }),
  );
  const preCommitGit = fakeGit({ head: candidate, parents: ['0'.repeat(40)] });
  assert.doesNotThrow(() =>
    validateAttribution(implicit, preCommitGit, {
      allowImplicitCurrentBeforeCommit: true,
    }),
  );
  assert.throws(
    () => validateAttribution(implicit, preCommitGit),
    /direct single-parent child/,
  );
});

test('projection_validation_rejects_stale_status_and_accepts_exact_status', () => {
  const metadata = parseEvidenceMetadata(header());
  const exact = renderProjection(metadata);
  assert.doesNotThrow(() => validateProjection(exact, metadata, 'fixture.md'));
  assert.throws(
    () => validateProjection(exact.replace('historical', 'current'), metadata, 'fixture.md'),
    /stale/,
  );
  assert.throws(
    () => validateProjection(`${exact}\n> **Current exact-candidate evidence.**\n`, metadata, 'fixture.md'),
    /outside the canonical projection/,
  );
});

test('historical_record_cannot_use_current_visible_status', () => {
  const metadata = parseEvidenceMetadata(header());
  const record = `${header()}
> **Current exact-candidate evidence.**
**Measured:** 16 August 2026 at 18:51:07 UTC
**Candidate A:** \`${candidate}\`
\`${artifact}\``;
  assert.throws(
    () => validateRecordIdentity(record, metadata),
    /visible status disagrees/,
  );
});

test('record_identity_rejects_visible_artifact_time_and_platform_mismatches', () => {
  const metadata = parseEvidenceMetadata(header());
  const exactRecord = `${header()}
> **Historical artifact evidence.**
**Measured:** 16 August 2026 at 18:51:07 UTC
**Candidate A:** \`${candidate}\`
**Artifact SHA-256:**
\`${artifact}\`
**Measurement platform scope:**
\`${metadata.measurementPlatformScope}\``;
  assert.doesNotThrow(() => validateRecordIdentity(exactRecord, metadata));
  assert.throws(
    () => validateRecordIdentity(exactRecord.replaceAll(artifact, 'a'.repeat(64)), metadata),
    /artifact prose disagrees/,
  );
  assert.throws(
    () => validateRecordIdentity(exactRecord.replaceAll('18:51:07', '18:51:08'), metadata),
    /measurement time prose disagrees/,
  );
  assert.throws(
    () => validateRecordIdentity(exactRecord.replaceAll('automated-pty', 'human-terminal'), metadata),
    /platform scope prose disagrees/,
  );
  assert.throws(
    () =>
      validateRecordIdentity(
        `${exactRecord}\n**Artifact SHA-256:**\n\`${'b'.repeat(64)}\``,
        metadata,
      ),
    /artifact field must occur exactly once/,
  );
  assert.throws(
    () =>
      validateRecordIdentity(
        `${exactRecord}\n**Measured:** 16 August 2026 at 18:51:08 UTC`,
        metadata,
      ),
    /measurement time field must occur exactly once/,
  );
  assert.throws(
    () =>
      validateRecordIdentity(
        `${exactRecord}\n**Measurement platform scope:**\n\`other-platform\``,
        metadata,
      ),
    /platform scope field must occur exactly once/,
  );
});

function runGitOrThrow(cwd, arguments_) {
  const result = spawnSync('git', arguments_, { cwd, encoding: 'utf8' });
  if (result.status !== 0) {
    throw new Error(`git ${arguments_.join(' ')} failed: ${result.stderr}`);
  }
  return result.stdout.trim();
}

test('ensure_full_history_deepens_a_shallow_clone_so_ancestor_commits_resolve', () => {
  const origin = mkdtempSync(join(tmpdir(), 'quirl-evidence-origin-'));
  const clone = mkdtempSync(join(tmpdir(), 'quirl-evidence-clone-'));
  try {
    runGitOrThrow(origin, ['init', '--quiet']);
    runGitOrThrow(origin, ['config', 'user.email', 'test@example.com']);
    runGitOrThrow(origin, ['config', 'user.name', 'Test']);
    writeFileSync(join(origin, 'a.txt'), 'a');
    runGitOrThrow(origin, ['add', 'a.txt']);
    runGitOrThrow(origin, ['commit', '--quiet', '-m', 'first']);
    const rootCommit = runGitOrThrow(origin, ['rev-parse', 'HEAD']);
    writeFileSync(join(origin, 'b.txt'), 'b');
    runGitOrThrow(origin, ['add', 'b.txt']);
    runGitOrThrow(origin, ['commit', '--quiet', '-m', 'second']);

    // A plain local path triggers git's local-clone optimization, which
    // hard-links objects and ignores --depth. Force the git:// transport
    // semantics with a file:// URL so the clone is genuinely shallow.
    spawnSync('git', ['clone', '--quiet', '--depth', '1', `file://${origin}`, clone], {
      encoding: 'utf8',
    });
    assert.equal(runGitOrThrow(clone, ['rev-parse', '--is-shallow-repository']), 'true');
    assert.notEqual(
      spawnSync('git', ['cat-file', '-e', `${rootCommit}^{commit}`], { cwd: clone }).status,
      0,
      'the root commit must be absent before deepening, or the test proves nothing',
    );

    repositoryGit(clone).ensureFullHistory();

    assert.equal(runGitOrThrow(clone, ['rev-parse', '--is-shallow-repository']), 'false');
    assert.equal(
      spawnSync('git', ['cat-file', '-e', `${rootCommit}^{commit}`], { cwd: clone }).status,
      0,
    );
  } finally {
    rmSync(origin, { recursive: true, force: true });
    rmSync(clone, { recursive: true, force: true });
  }
});
