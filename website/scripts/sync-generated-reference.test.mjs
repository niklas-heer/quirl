import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { executableFromBuildOutput, referenceEnvironment, snapshotExecutable, synchronizeReferences } from './sync-generated-reference.mjs';

function fixture(run) {
  const root = mkdtempSync(join(tmpdir(), 'quirl-reference-test-'));
  try { run(root); } finally { rmSync(root, { recursive: true, force: true }); }
}

test('runtime_profile_excludes_inherited_assets_plugins_and_network_settings', () => {
  const environment = referenceEnvironment('/private/reference');
  assert.equal(environment.HOME, '/private/reference');
  for (const name of ['QUIRL_ASSET_DATA_DIR', 'QUIRL_ASSET_CACHE_DIR', 'QUIRL_PLUGIN_HOME', 'QUIRL_CONFIG_DIR', 'QUIRL_INDEX_PATH']) {
    assert.ok(environment[name].startsWith('/private/reference/'));
  }
  for (const name of ['QUIRL_ASSET_MANIFEST_URL', 'QUIRL_ASSET_MANIFEST_FILE', 'QUIRL_LUA_WORKER', 'HTTP_PROXY', 'CARGO_HOME', 'RUSTUP_HOME']) {
    assert.equal(environment[name], undefined);
  }
  assert.equal(environment.PATH, '/usr/bin:/bin');
});

test('cargo_artifact_resolution_uses_the_reported_binary_and_rejects_ambiguity', () => {
  const artifact = (executable) => JSON.stringify({ reason: 'compiler-artifact', target: { name: 'quirl', kind: ['bin'] }, executable });
  assert.equal(executableFromBuildOutput(`${artifact('/target/quirl')}\n`), '/target/quirl');
  assert.throws(() => executableFromBuildOutput('{}\n'), /did not report/);
  assert.throws(() => executableFromBuildOutput(`${artifact('/a')}\n${artifact('/b')}`), /multiple/);
});

test('snapshot_preserves_exact_source_and_owner_only_permissions_after_replacement', () => fixture((root) => {
  const source = join(root, 'source');
  const target = join(root, 'snapshot');
  writeFileSync(source, 'abcd');
  snapshotExecutable(source, target, 4);
  writeFileSync(source, 'changed');
  assert.equal(readFileSync(target, 'utf8'), 'abcd');
  assert.equal(statSync(target).mode & 0o777, 0o500);
}));

test('snapshot_rejects_first_excess_byte_and_preserves_existing_destination', () => fixture((root) => {
  const source = join(root, 'source');
  const target = join(root, 'snapshot');
  writeFileSync(source, 'abcde');
  assert.throws(() => snapshotExecutable(source, target, 4), /byte limit/);
  assert.equal(existsSync(target), false);
  writeFileSync(source, 'abcd');
  writeFileSync(target, 'preserved');
  assert.throws(() => snapshotExecutable(source, target, 4), /EEXIST/);
  assert.equal(readFileSync(target, 'utf8'), 'preserved');
}));

test('snapshot_rejects_symlinks_and_fifo_without_waiting_for_a_writer', () => fixture((root) => {
  const source = join(root, 'source');
  const target = join(root, 'snapshot');
  writeFileSync(source, 'abcd');
  symlinkSync(source, join(root, 'link'));
  assert.throws(() => snapshotExecutable(join(root, 'link'), target, 4));
  const fifo = join(root, 'fifo');
  const made = spawnSync('/usr/bin/mkfifo', [fifo], { timeout: 1000 });
  assert.equal(made.status, 0);
  assert.throws(() => snapshotExecutable(fifo, target, 4), /regular file/);
  assert.equal(existsSync(target), false);
}));

test('unknown_options_fail_before_building_or_allocating_a_profile', () => {
  assert.throws(() => synchronizeReferences(['--wrong']), /usage:/);
});

test('snapshot_cleanup_preserves_first_error_and_closes_each_descriptor_once', () => fixture((root) => {
  const source = join(root, 'source');
  writeFileSync(source, 'abcd');
  for (const phase of ['read', 'close']) {
    // Fault injection stays inside a child; the test runner's filesystem API
    // and cleanup remain untouched even when every injected cleanup fails.
    const result = spawnSync(process.execPath, ['--input-type=module', '-e', `
      import assert from 'node:assert/strict';
      import fs from 'node:fs';
      import { syncBuiltinESMExports } from 'node:module';
      const { snapshotExecutable } = await import(${JSON.stringify(new URL('./sync-generated-reference.mjs', import.meta.url).href)});
      const closed = [];
      const close = fs.closeSync;
      if (${JSON.stringify(phase)} === 'read') {
        fs.readSync = () => { throw new Error('copy-primary'); };
      }
      fs.closeSync = (descriptor) => {
        closed.push(descriptor);
        close(descriptor);
        throw new Error('close-secondary');
      };
      fs.rmSync = () => { throw new Error('remove-secondary'); };
      syncBuiltinESMExports();
      assert.throws(
        () => snapshotExecutable(${JSON.stringify(source)}, ${JSON.stringify(join(root, phase))}, 4),
        { message: ${JSON.stringify(phase === 'read' ? 'copy-primary' : 'close-secondary')} },
      );
      assert.equal(closed.length, 2);
      assert.equal(new Set(closed).size, 2);
    `], { encoding: 'utf8', maxBuffer: 64 * 1024, timeout: 5000 });
    assert.equal(result.status, 0, result.stderr);
  }
}));
