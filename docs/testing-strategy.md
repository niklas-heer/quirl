# Testing strategy

Quirl tests the same implementation at progressively wider boundaries. The
canonical gate is:

```console
cargo xtask check
```

It checks Rust and Quirl formatting, denies every Clippy warning, rejects
undocumented public Rust APIs, builds workspace Rustdoc with warnings denied,
runs the full workspace, executes 128 deterministic generated C1 differential
cases against each available reference shell, and runs guest-side Lua tests.

## Layers

| Layer | What it proves | Typical evidence |
| --- | --- | --- |
| Unit and model | Parsers, migrations, ranking, state transitions, and renderers satisfy local invariants | In-crate behavior tests |
| Contract and golden | Serialized protocols, schemas, catalogs, and generated SDKs do not drift silently | Exact JSON/SDK/KDL fixtures, deterministic SQLite images, and hashes |
| Seeded differential | Supported native command composition agrees with clean Bash and Zsh behavior across generated combinations | Seed, case index, source, status, stdout, stderr |
| Seeded lifecycle simulation | Process-group state remains safe under reordered/stale events and converges after faults freeze | Seed, case index, step, child, transition, bounded convergence steps |
| Adversarial and fault | Limits, cancellation, symlink checks, recovery, plugin integrity, and terminal escaping fail closed | Tests that cross every declared boundary |
| Real PTY | Editing, deletion, mode changes, completion, wrapping, cursor queries, and Ctrl-D work through a terminal | PTY smoke and release matrix |
| External-command compatibility | Real argv, status, stdout/stderr, progress timing, ANSI filtering, and completion subcommand scope match the supported shell contract | Deterministic GHQ-shaped fixtures plus Bash/Zsh differential cases |
| Guest runtime | Lua code sees the documented sandbox and API rather than Rust-only test shortcuts | `examples/lua_tests.lua` |
| Website release gate | Mirrors and release-evidence attribution match canonical sources, and the documentation site lints, type-checks, and builds | `npm --prefix website run check` / `cargo xtask website-check` |
| Release contracts | Version planning, preparation, package aggregation, strict manifests, checksums, provenance, and Homebrew rendering are deterministic and fail closed | Focused `xtask` contract tests and `cargo xtask release verify` |
| Downloadable assets | Missing, corrupt, incompatible, cancelled, and transient downloads preserve degraded use and the previous valid generation | Injected offline downloader tests with bounded fake inputs |
| Release evidence | Startup, repaint latency, retention, binary size, digest, and source identity meet budgets | `cargo xtask release-preview` and `release-gate` |

## Release and asset contract evidence

Release tests operate on temporary repositories and fixtures. They cover the
first-release `0.1.0` rule; patch, minor, pre-1.0 breaking-minor, and stable
breaking-major Conventional Commit histories; unreachable and malformed tags;
non-releasing commits retained in notes; idempotent preparation; curated
changelog preservation; and refusal to rewrite protocol, schema, plugin API,
fixture, or historical version literals.

Package tests require stable entry ordering and metadata, explicit target
admission, exact candidate version/commit reporting, bounded reads, duplicate
target rejection, and byte-identical aggregation. Strict versioned JSON fixtures
must reject unknown fields, oversized collections and strings, invalid hashes,
path traversal, unsupported schema versions, missing targets, and candidate
identity disagreement. Formula tests exercise all four OS/architecture branches
and prove the offline test invokes only the installed binary.

Runtime asset tests use an injected downloader or bounded local server, never
the internet. Cross every byte, deadline, redirect, retry, manifest-entry, and
retained-state boundary. Prove that the first prompt does not wait; duplicate
background requests coalesce; cancellation and every partial-write fault remove
staging; corrupt, truncated, incompatible, and unexpected files are rejected;
permanent errors stop automatic retries; transient retry state remains bounded
and survives restart; and an admitted old generation remains usable until a
fully verified replacement is atomically installed.

## Native command catalog evidence

The curated external native catalog has a source/artifact boundary distinct
from `Catalog::builtin()` and the mutable CLI intelligence cache. Its tests must
prove all of the following:

- valid strict KDL produces the expected typed command tree, while malformed
  KDL retains source identity, UTF-8 byte spans, and actionable help;
- unknown nodes/properties, duplicate properties and identifiers, extra
  positional values, typed KDL annotations, invalid platforms/actions, and
  impossible flag or argument combinations fail closed;
- source bytes, string bytes, total commands/flags/arguments/documents,
  per-command values, root-inclusive depth, database bytes, query bytes,
  documents scanned, and result counts trip at the declared boundary and report
  configured plus observed usage;
- compiling the same typed tree twice yields byte-identical `QCNC` SQLite, and
  reader admission rejects wrong identity, corrupt integrity, unknown snapshot
  fields, or any normalized-row change relative to the exact snapshot;
- command/alias, flag, argument, platform, and semantic projections have stable
  ordering; child platforms narrow rather than widen parent support; lexical
  ranking is platform-filtered and deterministic; and no completion action runs
  during compilation or lookup;
- publication fault injection after staging creation, write, content sync,
  staged reread, destination recheck, and before rename preserves the previous
  admitted image and removes the current staging file; contention and unsafe
  file shapes fail without partial publication; and
- a pinned Carapace import is bounded and writes only a review draft. Tests must
  show that dirty pinned files, duplicate facts, cross-command action leakage,
  and invented positional metadata fail or become explicit omissions; a
  partial/invalid import cannot change curated KDL; formatting is
  byte-idempotent; unchanged canonical KDL preserves the database checksum; and
  no runtime test requires Carapace or network access.

CI should run the implemented format-check, strict-check, and build operations,
compile twice, compare exact bytes, reopen through the hardened reader, and
publish the database checksum with the source revision. The tooling command
names and corpus paths are deliberately deferred to the integration change;
tests and docs must use the four operation roles from
[the catalog contract](catalog-schema.md) until then.

## Reproducing generated failures

The default generator is intentionally stable. To increase exploration locally:

```console
cargo xtask test --seed 123456789 --cases 2048
```

Case counts are restricted to `1..=10000`. A failure message contains everything
needed to replay it. Never seed from wall-clock time inside a test; a scheduled
or external swarm may choose seeds, but it must record them before execution.

## Stateful compatibility simulation

The inspectable compatibility swarm is a separate, wider gate:

```console
cargo xtask simulate --seed 123456789 --sessions 2048 --steps 12
```

Each generated session has `3..=steps` stateful operations. It starts in an
isolated directory, changes into a session workspace, exports a value, and then
mixes bounded C1 command lists, pipelines, redirects, append operations,
parameter and arithmetic expansion, command substitution, here-strings,
status propagation, stdout, and stderr. The final observation exposes retained
environment and filesystem state. Quirl, clean Bash, and clean Zsh each receive
an identical fresh filesystem and an environment reduced to explicit locale,
path, home, temporary-directory, and terminal values.

Both references are required. A Bash/Zsh disagreement is reported as
`reference_divergence`, because the generator has escaped the promised common
subset or a reference changed. When the references agree and Quirl differs in
status, exact output, or the bounded filesystem manifest, the result is
`native_mismatch`. No majority vote can hide a divergence.

The seed-specific directory under `target/simulations/` contains:

- `summary.json`, including result counts, exact interpreter paths and
  versions, and the first divergent session;
- `report.jsonl`, with every source, step list, outcome, filesystem manifest,
  deadline result, and classification;
- `failures/session-N/`, with standalone `.sh` and `.qrl` replay sources plus
  the complete case record; and
- `issue.md`, only on mismatch, with the exact focused replay command.

To rerun only a reported session while preserving generator position:

```console
cargo xtask simulate --seed 123456789 --sessions 2048 --steps 12 --session 731 --output target/simulations/replay
```

Sessions are bounded to `1..=10000`, steps to `3..=32`, source and retained
output to 64 KiB, the filesystem manifest to 64 files and 64 KiB at depth eight,
and every interpreter to five seconds. Timeouts terminate the isolated process
group before the result is recorded.

The daily GitHub Actions swarm chooses the explicit seed from the workflow run
ID, installs both references, runs 2,048 sessions, and uploads the trace. It
opens a public issue only when a completed `summary.json` proves a compatibility
mismatch; checkout, installation, build, or runner failures fail the workflow
without being mislabeled as shell defects.

## Lifecycle simulation

The process lifecycle simulator adapts TigerBeetle's
[safety-to-liveness simulation](https://tigerbeetle.com/blog/2023-07-06-simulation-testing-for-liveness/)
to a local shell process group:

1. A bounded safety phase schedules stop, continue, exit, duplicate, and stale
   child notifications in a seeded order. Every step checks that invalid
   transitions leave committed state unchanged and that the aggregate job state
   matches the child states.
2. The liveness phase freezes completed children permanently, heals the live
   core by continuing stopped children, stops injecting faults, and requires the
   process group to reach `done` within at most two transitions per live child.

The same seed also schedules real construction failures after each of the first
four process spawns on Unix. Dropping `PipelineConstructionGuard` must kill and
reap every child already owned by the incomplete transaction. These OS-process
cases are capped at 32 per run even when the pure simulator is asked to run more
cases; both bounds are intentional so the canonical gate remains predictable.

The model is not a replacement for real PTY evidence. It makes state-machine
and cleanup schedules cheap to explore and replay; `cargo xtask rich-pty`
separately proves native Ctrl-C/Ctrl-Z, jobs/bg/fg transitions, process-group
construction cleanup, foreground ownership, stopped-job and prompt termios
restoration, dialect-island noninteraction, and rendering against the operating
system.

The PTY checks live in `xtask/src/pty.rs` and `xtask/src/rich_pty.rs`. The
Rust-owned driver reuses the workspace's existing `nix` and `unicode-width`
dependencies and requires no separate scripting-language toolchain. Its small
VT screen model answers cursor-position queries from modeled screen state, so
assertions distinguish visible cells from stale bytes in the raw transcript.
Each session limits retained output to 16 MiB, each read/write/wait to five
seconds by default, the screen to 262,144 cells, and forced child cleanup to two
seconds. Session teardown kills both the foreground process group and the PTY
session group before reaping the leader.

`external-command-compatibility` installs an isolated GHQ-shaped executable and
Fish completion file. It proves that an scp-style repository argument reaches
the child unchanged, colored progress becomes visible before a one-second child
finishes, carriage-return progress replaces its prior value, alternate-screen
ownership remains stable, and completion exposes real subcommand paths and
descriptions rather than importer implementation names. Fixtures use no network
and mark completion independently so a final-output-only renderer cannot pass
by racing the assertion.

`local-completion-discovery` uses the same GHQ shape with controlled fake Fish
and Zsh executables. Neither host shell nor host completion root is consulted.
The check proves that initial top-level discovery persists framed subcommands,
flags, descriptions, and provider attribution, then that an editor-observed
unknown nested path is coalesced, probed off the render thread, published, and
adopted only after the next editor boundary. The focused CLI model tests cover
warm positive and negative restarts, expiry and identity invalidation,
malformed/excessive frames, deadlines, cancellation, descendant cleanup, and
queue/concurrency limits without requiring Fish, Zsh, Carapace, or network.

Run the harness model tests and one focused end-to-end interaction with:

```console
cargo build -p quirl-cli
cargo xtask rich-pty --check mode-switch-and-palette-screen
cargo xtask rich-pty --check external-command-compatibility
cargo xtask rich-pty --check local-completion-discovery
```

The focused interaction sends the legacy-terminal encoding for Alt-Q and
Alt-Q leader chords exactly. It asserts that repeated mode switches retain the edit buffer
without feedback scrollback, the palette status is on the physical bottom row,
and dismissal erases the expanded viewport before restoring the compact prompt.

## Adding coverage

- Add a frozen fixture for a discovered regression before fixing it.
- Extend a seeded generator only with syntax inside the supported native C1
  contract; unsupported Bash/Zsh syntax belongs in explicit-island tests.
- Prefer a real implementation or a small independent model as the oracle.
- Test both success and failure, including cancellation and cleanup after a
  partially completed operation.
- Formatting fault tests inject failure after temporary creation, partial
  output, flush, content sync, permission update, metadata sync, original
  retention, rename, and parent sync. Every returned failure must preserve the
  original source bytes and remove transaction files. Authoring discovery tests
  cross every depth, directory, per-directory entry, total entry, supported-file,
  retained-path-byte, and scanned-name-byte limit and cover symlink, permission,
  and disappearing-entry behavior without recursive traversal.
- For a liveness claim, separate a fault-exploration phase from a healthy phase
  with a named progress bound. Do not let later random recovery hide a stuck
  state.
- Bound generated input size, case count, subprocess output, and elapsed time.
- For terminal work, verify `NO_COLOR`, `TERM=dumb`, narrow widths, Unicode
  graphemes, wrapped lines, and hostile control characters.
- For external-command regressions, assert an intermediate state before child
  exit as well as final bytes/status. Run the same bounded argv/output fixture
  through clean Bash and Zsh when the claim concerns shell semantics; use the
  PTY model when the claim concerns visibility or terminal state.

The rationale and non-goals are recorded in
[ADR 0011](decisions/0011-deterministic-testing-and-bounded-engineering.md).
