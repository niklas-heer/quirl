import { spawnSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(websiteRoot, '..');
const outputBytesMax = 8 * 1024 * 1024;
const timeoutMs = 120_000;

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

function generate(arguments_) {
  const result = spawnSync(
    'cargo',
    ['run', '--quiet', '-p', 'quirl-cli', '--', ...arguments_],
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

for (const reference of references) {
  const body = generate(reference.arguments);
  const target = join(websiteRoot, reference.target);
  const rendered = `---\ntitle: ${JSON.stringify(reference.title)}\ndescription: ${JSON.stringify(reference.description)}\n---\n\n{/* Generated from the compiled Quirl catalog. Run npm run sync:reference; do not edit this page by hand. */}\n\n${body}\n`;

  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, rendered);
}

console.log(`Generated ${references.length} compiled reference pages.`);
