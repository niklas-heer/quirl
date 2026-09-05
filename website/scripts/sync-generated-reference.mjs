import { spawnSync } from 'node:child_process';
import {
  chmodSync,
  closeSync,
  constants,
  existsSync,
  fstatSync,
  mkdirSync,
  openSync,
  mkdtempSync,
  readFileSync,
  readSync,
  rmSync,
  writeFileSync,
  writeSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const websiteRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const repositoryRoot = resolve(websiteRoot, '..');
const outputBytesMax = 8 * 1024 * 1024;
const timeoutMs = 120_000;
const executableBytesMax = 256 * 1024 * 1024;

// Failure model: a normal `quirl doc` includes compatible installed completion
// assets and plugin metadata. Isolating only the index silently made the checked
// reference depend on the developer's current installed version. Generate the
// compiled builtin contracts in an empty private profile instead. Cargo keeps
// its own toolchain/cache environment and resolves locked dependencies. The
// product receives no inherited environment, plugin/config paths, manifest
// overrides, or network configuration. Neither command downloads assets.
export function referenceEnvironment(profile) {
  return {
    HOME: profile,
    PATH: '/usr/bin:/bin',
    LANG: 'C',
    LC_ALL: 'C',
    TERM: 'dumb',
    NO_COLOR: '1',
    TMPDIR: join(profile, 'tmp'),
    XDG_CONFIG_HOME: join(profile, 'config'),
    XDG_CACHE_HOME: join(profile, 'cache'),
    XDG_DATA_HOME: join(profile, 'data'),
    XDG_STATE_HOME: join(profile, 'state'),
    QUIRL_CONFIG_DIR: join(profile, 'config', 'quirl'),
    QUIRL_PLUGIN_HOME: join(profile, 'plugins'),
    QUIRL_INDEX_DIR: join(profile, 'index'),
    QUIRL_INDEX_PATH: join(profile, 'index', 'catalog.sqlite3'),
    QUIRL_ASSET_DATA_DIR: join(profile, 'assets'),
    QUIRL_ASSET_CACHE_DIR: join(profile, 'asset-cache'),
  };
}

export function executableFromBuildOutput(output) {
  let executable;
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    const event = JSON.parse(line);
    if (event.reason === 'compiler-artifact' && event.target?.name === 'quirl'
        && event.target?.kind?.includes('bin') && typeof event.executable === 'string') {
      if (executable && executable !== event.executable) {
        throw new Error('Cargo reported multiple Quirl executables');
      }
      executable = event.executable;
    }
  }
  if (!executable) throw new Error('Cargo did not report a compiled Quirl executable');
  return executable;
}

function buildBinary() {
  const result = spawnSync('cargo', [
    'build', '--locked', '--quiet', '-p', 'quirl-cli', '--message-format=json',
  ], {
    cwd: repositoryRoot,
    encoding: 'utf8',
    maxBuffer: outputBytesMax,
    timeout: timeoutMs,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`Quirl reference build failed with status ${result.status}:\n${result.stderr}`);
  }
  return executableFromBuildOutput(result.stdout);
}

// Pin the exact completed build for both commands. Concurrent rebuilds must not
// change the tested executable halfway through generation. Copy one validated
// regular handle with a 64 KiB buffer, reject growth beyond 256 MiB, and give
// only the profile owner permission to execute the completed snapshot.
export function snapshotExecutable(source, target, bytesMax = executableBytesMax) {
  if (!Number.isSafeInteger(bytesMax) || bytesMax < 1 || bytesMax > executableBytesMax) {
    throw new Error('Reference executable byte limit is invalid');
  }
  const input = openSync(source, constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK);
  let output;
  let created = false;
  let failure;
  try {
    const before = fstatSync(input);
    if (!before.isFile()) throw new Error('Reference executable must be a regular file');
    if (before.size > bytesMax) throw new Error('Reference executable exceeds its byte limit');
    output = openSync(target, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL, 0o600);
    created = true;
    const buffer = Buffer.alloc(64 * 1024);
    let copied = 0;
    for (;;) {
      const read = readSync(input, buffer, 0, Math.min(buffer.length, bytesMax - copied + 1), null);
      if (read === 0) break;
      copied += read;
      if (copied > bytesMax) throw new Error('Reference executable exceeds its byte limit');
      let written = 0;
      while (written < read) {
        const count = writeSync(output, buffer, written, read - written);
        if (count === 0) throw new Error('Reference executable copy made no progress');
        written += count;
      }
    }
    const after = fstatSync(input);
    if (before.size !== copied || before.size !== after.size
        || before.mtimeMs !== after.mtimeMs || before.ctimeMs !== after.ctimeMs) {
      throw new Error('Reference executable changed during copying; finish the build and retry');
    }
    const completedOutput = output;
    output = undefined;
    closeSync(completedOutput);
    chmodSync(target, 0o500);
  } catch (error) {
    failure = error;
  }
  // Consume each descriptor once, even if close reports an error. Cleanup must
  // preserve the first failure and still attempt the other owned resources.
  for (const descriptor of [output, input]) {
    if (descriptor === undefined) continue;
    try { closeSync(descriptor); } catch (error) { failure ??= error; }
  }
  if (failure && created) {
    try { rmSync(target, { force: true }); } catch { /* Preserve the original error. */ }
  }
  if (failure) throw failure;
}

const references = [
  {
    arguments: ['doc', '--format', 'markdown'],
    description:
      'Generated builtin command contracts from Quirl; installed completion packs, plugins, and local discovery are excluded.',
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

function escapeMdxProse(markdown) {
  let fenced = false;
  return markdown
    .split('\n')
    .map((line) => {
      if (line.trimStart().startsWith('```')) {
        fenced = !fenced;
        return line;
      }
      if (fenced) return line;
      return line
        .split(/(`[^`]*`)/g)
        .map((segment, index) => {
          if (index % 2 === 1) return segment;
          return segment
            .replaceAll(/<([A-Za-z][^>\n]*)>/g, '&lt;$1&gt;')
            .replaceAll('{', '&#123;')
            .replaceAll('}', '&#125;');
        })
        .join('');
    })
    .join('\n');
}

function generate(binary, environment, commandArguments) {
  const result = spawnSync(
    binary,
    commandArguments,
    {
      cwd: environment.HOME,
      encoding: 'utf8',
      env: environment,
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

  const markdown = result.stdout
    .replaceAll('\r\n', '\n')
    .replace(/^#\s+.+\n+/, '')
    .trim();
  // Angle-bracket and brace placeholders are prose, but MDX treats them as JSX.
  return escapeMdxProse(markdown);
}

export function synchronizeReferences(arguments_) {
  if (arguments_.some((argument) => argument !== '--check')) {
    throw new Error('usage: node scripts/sync-generated-reference.mjs [--check]');
  }
  const checkMode = arguments_.includes('--check');
  const profile = mkdtempSync(join(tmpdir(), 'quirl-reference-'));
  const driftedFiles = [];
  let failure;
  try {
    const environment = referenceEnvironment(profile);
    mkdirSync(environment.TMPDIR, { mode: 0o700 });
    const binary = join(profile, 'quirl');
    snapshotExecutable(buildBinary(), binary);
    for (const reference of references) {
      const body = generate(binary, environment, reference.arguments);
      const target = join(websiteRoot, reference.target);
      const rendered = `---\ntitle: ${JSON.stringify(reference.title)}\ndescription: ${JSON.stringify(reference.description)}\n---\n\n{/* Generated from Quirl's compiled builtin contracts in an empty profile. Run npm run sync:reference; do not edit this page by hand. */}\n\n${body}\n`;

      if (checkMode) {
        if (!existsSync(target) || readFileSync(target, 'utf8') !== rendered) {
          driftedFiles.push(relative(repositoryRoot, target));
        }
      } else {
        mkdirSync(dirname(target), { recursive: true });
        writeFileSync(target, rendered);
      }
    }
  } catch (error) {
    failure = error;
  }
  try { rmSync(profile, { recursive: true, force: true }); } catch (error) { failure ??= error; }
  if (failure) throw failure;

  if (driftedFiles.length > 0) {
    throw new Error(
      `generated website references are stale: ${driftedFiles.join(', ')}; run npm run sync:reference`,
    );
  }
  console.log(`${checkMode ? 'Checked' : 'Generated'} ${references.length} compiled reference pages.`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  synchronizeReferences(process.argv.slice(2));
}
