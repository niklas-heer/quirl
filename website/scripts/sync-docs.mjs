import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  convertProjectionMarkersToMdx,
  loadEvidence,
  renderWebsiteEvidenceNotice,
  synchronizeProjectionFiles,
} from './release-evidence.mjs';

const websiteRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(websiteRoot, '..');
const contentRoot = join(websiteRoot, 'content', 'docs');
const manifestPath = join(websiteRoot, 'content', '.generated-docs.json');

const documents = [
  ['CONTRIBUTING.md', 'contributing/project-guide.mdx'],
  ['SECURITY.md', 'project/security.mdx'],
  ['CHANGELOG.md', 'project/changelog.mdx'],
  ['AGENTS.md', 'contributing/engineering-contract.mdx'],
  ['crates/quirl-contract/README.md', 'architecture/crates/contract.mdx'],
  ['crates/quirl-process/MANUAL_JOB_CONTROL.md', 'contributing/manual-job-control.mdx'],
  ['docs/data-runtime.md', 'guides/typed-data-runtime.mdx'],
  ['docs/language-service.md', 'tooling/language-service.mdx'],
  ['docs/mcp.md', 'tooling/mcp.mdx'],
  ['docs/plugin-platform.md', 'extensions/plugin-platform.mdx'],
  ['docs/extension-events-and-live-views.md', 'extensions/events-and-live-views.mdx'],
  ['docs/agent-and-package-contracts.md', 'extensions/agent-and-package-contracts.mdx'],
  ['docs/catalog-schema.md', 'reference/catalog-schema.mdx'],
  ['docs/async-interaction-protocol.md', 'reference/async-interaction-protocol.mdx'],
  ['docs/protocol-compatibility.md', 'reference/protocol-compatibility.mdx'],
  ['docs/language-design.md', 'architecture/product-and-language-design.mdx'],
  ['docs/tui-design.md', 'architecture/interactive-surface.mdx'],
  ['docs/embedded-language-decision.md', 'architecture/why-lua.mdx'],
  ['docs/documentation-system.md', 'contributing/documentation-system.mdx'],
  ['docs/testing-strategy.md', 'contributing/testing-strategy.mdx'],
  ['docs/releasing.md', 'project/release-0.1/operations.mdx'],
  ['docs/release-checklist.md', 'project/release-0.1/release-checklist.mdx'],
  ['docs/security-accessibility-audit-v0.1.md', 'project/release-0.1/security-accessibility-audit.mdx'],
  ['docs/adoption-plan.md', 'project/adoption-plan.mdx'],
  ['docs/bash-zsh-source-study.md', 'research/bash-zsh-source-study.mdx'],
  ['docs/benchmarks/release-v1.0.md', 'project/release-0.1/performance-record.mdx'],
  ['docs/benchmarks/preview-v0.1.md', 'research/benchmarks/preview-v0.1.mdx'],
  ['docs/benchmarks/embedded-language-selection.md', 'research/benchmarks/embedded-language-selection.mdx'],
  ['docs/benchmarks/steel-lua-fennel.md', 'research/benchmarks/steel-lua-fennel.mdx'],
  ['docs/decisions/0001-lua-extension-language.md', 'architecture/decisions/0001-lua-extension-language.mdx'],
  ['docs/decisions/0002-crate-layering.md', 'architecture/decisions/0002-crate-layering.mdx'],
  ['docs/decisions/0003-preview-runtime-layers.md', 'architecture/decisions/0003-preview-runtime-layers.mdx'],
  ['docs/decisions/0004-phase-2-contract-and-language-service-layers.md', 'architecture/decisions/0004-phase-2-contract-and-language-service-layers.mdx'],
  ['docs/decisions/0005-plugin-platform-layer.md', 'architecture/decisions/0005-plugin-platform-layer.mdx'],
  ['docs/decisions/0006-platform-process-and-recovery-boundaries.md', 'architecture/decisions/0006-platform-process-and-recovery-boundaries.mdx'],
  ['docs/decisions/0007-semantic-catalog-v4.md', 'architecture/decisions/0007-semantic-catalog-v4.mdx'],
  ['docs/decisions/0008-protocol-freeze-and-migrations.md', 'architecture/decisions/0008-protocol-freeze-and-migrations.mdx'],
  ['docs/decisions/0009-isolated-process-adapter-v1.md', 'architecture/decisions/0009-isolated-process-adapter-v1.mdx'],
  ['docs/decisions/0010-unix-first-release-scope.md', 'architecture/decisions/0010-unix-first-release-scope.mdx'],
  ['docs/decisions/0011-deterministic-testing-and-bounded-engineering.md', 'architecture/decisions/0011-deterministic-testing-and-bounded-engineering.mdx'],
  ['docs/decisions/0012-ratatui-interactive-surface.md', 'architecture/decisions/0012-ratatui-interactive-surface.mdx'],
  ['docs/decisions/0013-lua-config-themes.md', 'architecture/decisions/0013-lua-config-themes.mdx'],
  ['docs/decisions/0014-external-history-provider-boundary.md', 'architecture/decisions/0014-external-history-provider-boundary.mdx'],
  ['docs/decisions/0015-bounded-theme-preview-gallery.md', 'architecture/decisions/0015-bounded-theme-preview-gallery.mdx'],
  ['docs/decisions/0016-runtime-layering-contract.md', 'architecture/decisions/0016-runtime-layering-contract.mdx'],
  ['docs/decisions/0017-shared-execution-contract.md', 'architecture/decisions/0017-shared-execution-contract.mdx'],
  ['docs/decisions/0018-typed-lua-runner-abi.md', 'architecture/decisions/0018-typed-lua-runner-abi.mdx'],
  ['docs/decisions/0019-isolated-lua-worker-deadlines.md', 'architecture/decisions/0019-isolated-lua-worker-deadlines.mdx'],
  ['docs/decisions/0020-owned-unix-process-group-anchor.md', 'architecture/decisions/0020-owned-unix-process-group-anchor.mdx'],
  ['docs/decisions/0021-sqlite-local-command-intelligence.md', 'architecture/decisions/0021-sqlite-local-command-intelligence.mdx'],
  ['docs/decisions/0022-persistent-rich-session-transcript.md', 'architecture/decisions/0022-persistent-rich-session-transcript.mdx'],
  ['docs/decisions/0023-rust-1.97-compatibility.md', 'architecture/decisions/0023-rust-1.97-compatibility.mdx'],
  ['docs/decisions/0024-kdl-native-command-catalog.md', 'architecture/decisions/0024-kdl-native-command-catalog.mdx'],
  ['docs/decisions/0025-fine-tuned-command-retrieval-model.md', 'architecture/decisions/0025-fine-tuned-command-retrieval-model.mdx'],
  ['docs/decisions/0026-rust-native-releases-and-runtime-assets.md', 'architecture/decisions/0026-rust-native-releases-and-runtime-assets.mdx'],
];

const pageBySource = new Map(documents);
const generatedBanner =
  '{/* Generated by website/scripts/sync-docs.mjs. Edit the repository source, not this copy. */}';
let releaseEvidenceMetadata;

function firstHeading(markdown, sourcePath) {
  const match = markdown.match(/^#\s+(.+)$/m);
  if (match) return match[1].replaceAll('`', '').trim();

  return sourcePath
    .split('/')
    .at(-1)
    .replace(/\.md$/, '')
    .split('-')
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join(' ');
}

function rewriteLinks(markdown, sourcePath) {
  return markdown.replace(/\]\(([^)#]+\.md)(#[^)]+)?\)/g, (match, href, hash = '') => {
    if (/^[a-z]+:/i.test(href)) return match;

    const resolved = relative(
      repositoryRoot,
      resolve(repositoryRoot, dirname(sourcePath), href),
    );
    const target = pageBySource.get(resolved);
    if (!target) return match;

    return `](/docs/${target.replace(/\.mdx$/, '')}${hash})`;
  });
}

function evidenceNotice(sourcePath) {
  if (sourcePath === 'docs/benchmarks/release-v1.0.md') {
    return renderWebsiteEvidenceNotice(releaseEvidenceMetadata);
  }

  return '';
}

function renderDocument(sourcePath) {
  const absoluteSource = join(repositoryRoot, sourcePath);
  let body = readFileSync(absoluteSource, 'utf8').replaceAll('\r\n', '\n');
  const title = firstHeading(body, sourcePath);
  body = body.replace(/^#\s+.+\n+/, '');
  body = rewriteLinks(body, sourcePath);
  // Shiki has no Quirl grammar yet. Keep the source label in a visible title
  // while using plain-text highlighting until a grammar is published.
  body = body.replaceAll('```quirl', '```text title="Quirl"');
  body = body.replaceAll('](protocol-freeze-v1.json)', '](/reference/protocol-freeze-v1.json)');
  body = body.replaceAll('](quirl.lua)', '](/reference/quirl.lua)');
  body = convertProjectionMarkersToMdx(body);
  if (sourcePath === 'docs/benchmarks/release-v1.0.md') {
    body = body.replace(/<!-- quirl-release-evidence:v1\n[\s\S]*?\n-->\n+/, '');
  }

  const description = `Canonical Quirl project documentation synced from ${sourcePath}.`;
  return `---\ntitle: ${JSON.stringify(title)}\ndescription: ${JSON.stringify(description)}\n---\n\n${generatedBanner}\n\n${evidenceNotice(sourcePath)}${body.trim()}\n`;
}

const previousFiles = existsSync(manifestPath)
  ? JSON.parse(readFileSync(manifestPath, 'utf8'))
  : [];
const arguments_ = process.argv.slice(2);
const checkMode = arguments_.includes('--check');

if (arguments_.some((argument) => argument !== '--check')) {
  throw new Error('usage: node scripts/sync-docs.mjs [--check]');
}

({ metadata: releaseEvidenceMetadata } = loadEvidence(repositoryRoot, {
  allowImplicitCurrentBeforeCommit: !checkMode,
}));
synchronizeProjectionFiles(repositoryRoot, releaseEvidenceMetadata, checkMode);

const driftedFiles = [];
const staleFiles = [];

function writeOrCheck(target, contents) {
  if (checkMode) {
    if (!existsSync(target) || readFileSync(target, 'utf8') !== contents) {
      driftedFiles.push(relative(repositoryRoot, target));
    }
    return;
  }

  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, contents);
}

const expectedGeneratedFiles = new Set(documents.map(([, targetPath]) => targetPath));

for (const previousFile of previousFiles) {
  const absoluteTarget = join(contentRoot, previousFile);
  if (!existsSync(absoluteTarget) || expectedGeneratedFiles.has(previousFile)) continue;
  const contents = readFileSync(absoluteTarget, 'utf8');
  if (!contents.slice(0, 500).includes(generatedBanner)) continue;

  if (checkMode) {
    staleFiles.push(relative(repositoryRoot, absoluteTarget));
  } else {
    rmSync(absoluteTarget);
  }
}

for (const [sourcePath, targetPath] of documents) {
  const absoluteTarget = join(contentRoot, targetPath);
  writeOrCheck(absoluteTarget, renderDocument(sourcePath));
}

const publicReference = join(websiteRoot, 'public', 'reference');
writeOrCheck(
  join(publicReference, 'protocol-freeze-v1.json'),
  readFileSync(join(repositoryRoot, 'docs', 'protocol-freeze-v1.json'), 'utf8'),
);
writeOrCheck(
  join(publicReference, 'quirl.lua'),
  readFileSync(join(repositoryRoot, 'docs', 'quirl.lua'), 'utf8'),
);

const publicExamples = join(websiteRoot, 'public', 'examples');
for (const example of ['config.lua', 'hello.lua', 'lua_tests.lua', 'plugin.lua']) {
  writeOrCheck(
    join(publicExamples, example),
    readFileSync(join(repositoryRoot, 'examples', example), 'utf8'),
  );
}

writeOrCheck(
  manifestPath,
  `${JSON.stringify(documents.map(([, targetPath]) => targetPath), null, 2)}\n`,
);

if (driftedFiles.length > 0 || staleFiles.length > 0) {
  const files = [...driftedFiles, ...staleFiles].join(', ');
  throw new Error(`generated website files are stale: ${files}; run npm run sync:docs`);
}

console.log(`${checkMode ? 'Checked' : 'Synced'} ${documents.length} documentation sources.`);
