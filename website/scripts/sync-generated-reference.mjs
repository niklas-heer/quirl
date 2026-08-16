import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(websiteRoot, '..');
const outputBytesMax = 8 * 1024 * 1024;
const timeoutMs = 120_000;
const arguments_ = process.argv.slice(2);
const checkMode = arguments_.includes('--check');

if (arguments_.some((argument) => argument !== '--check')) {
  throw new Error('usage: node scripts/sync-generated-reference.mjs [--check]');
}

const references = [
  {
    arguments: ['doc', '--format', 'markdown'],
    description:
      'Generated documentation for every installed command in Quirl’s semantic catalog.',
    target: 'content/docs/reference/cli-command-catalog.mdx',
    title: 'CLI command catalog',
  },
  {
    arguments: ['sdk', '--format', 'markdown'],
    description:
      'Generated function signatures, capabilities, and contracts for the Quirl Lua SDK.',
    target: 'content/docs/extensions/lua-api-reference.mdx',
    title: 'Lua SDK API reference',
  },
];

function generate(commandArguments) {
  const result = spawnSync(
    'cargo',
    ['run', '--quiet', '-p', 'quirl-cli', '--', ...commandArguments],
    {
      cwd: repositoryRoot,
      encoding: 'utf8',
      maxBuffer: outputBytesMax,
      timeout: timeoutMs,
    },
  );

  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(
      `Quirl reference generation failed with status ${result.status}:\n${result.stderr}`,
    );
  }

  return result.stdout.replaceAll('\r\n', '\n').replace(/^#\s+.+\n+/, '').trim();
}

const driftedFiles = [];

for (const reference of references) {
  const body = generate(reference.arguments);
  const target = join(websiteRoot, reference.target);
  const rendered = `---\ntitle: ${JSON.stringify(reference.title)}\ndescription: ${JSON.stringify(reference.description)}\n---\n\n{/* Generated from the compiled Quirl catalog. Run npm run sync:reference; do not edit this page by hand. */}\n\n${body}\n`;

  if (checkMode) {
    if (!existsSync(target) || readFileSync(target, 'utf8') !== rendered) {
      driftedFiles.push(relative(repositoryRoot, target));
    }
  } else {
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, rendered);
  }
}

if (driftedFiles.length > 0) {
  throw new Error(
    `generated website references are stale: ${driftedFiles.join(', ')}; run npm run sync:reference`,
  );
}

console.log(`${checkMode ? 'Checked' : 'Generated'} ${references.length} compiled reference pages.`);
