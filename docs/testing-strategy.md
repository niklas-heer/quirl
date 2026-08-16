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
| Contract and golden | Serialized protocols, schemas, catalogs, and generated SDKs do not drift silently | Exact JSON/SDK fixtures and hashes |
| Seeded differential | Supported native command composition agrees with clean Bash and Zsh behavior across generated combinations | Seed, case index, source, status, stdout, stderr |
| Seeded lifecycle simulation | Process-group state remains safe under reordered/stale events and converges after faults freeze | Seed, case index, step, child, transition, bounded convergence steps |
| Adversarial and fault | Limits, cancellation, symlink checks, recovery, plugin integrity, and terminal escaping fail closed | Tests that cross every declared boundary |
| Real PTY | Editing, deletion, mode changes, completion, wrapping, cursor queries, and Ctrl-D work through a terminal | PTY smoke and release matrix |
| Guest runtime | Lua code sees the documented sandbox and API rather than Rust-only test shortcuts | `examples/lua_tests.lua` |
| Release evidence | Startup, repaint latency, retention, binary size, digest, and source identity meet budgets | `cargo xtask release-preview` and `release-gate` |

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
and cleanup schedules cheap to explore and replay; `scripts/check-rich-pty.py`
separately proves native Ctrl-C/Ctrl-Z, jobs/bg/fg transitions, process-group
construction cleanup, foreground ownership, stopped-job and prompt termios
restoration, dialect-island noninteraction, and rendering against the operating
system.

## Adding coverage

- Add a frozen fixture for a discovered regression before fixing it.
- Extend a seeded generator only with syntax inside the supported native C1
  contract; unsupported Bash/Zsh syntax belongs in explicit-island tests.
- Prefer a real implementation or a small independent model as the oracle.
- Test both success and failure, including cancellation and cleanup after a
  partially completed operation.
- For a liveness claim, separate a fault-exploration phase from a healthy phase
  with a named progress bound. Do not let later random recovery hide a stuck
  state.
- Bound generated input size, case count, subprocess output, and elapsed time.
- For terminal work, verify `NO_COLOR`, `TERM=dumb`, narrow widths, Unicode
  graphemes, wrapped lines, and hostile control characters.

The rationale and non-goals are recorded in
[ADR 0011](decisions/0011-deterministic-testing-and-bounded-engineering.md).
