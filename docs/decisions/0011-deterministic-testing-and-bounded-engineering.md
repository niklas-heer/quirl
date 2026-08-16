# ADR 0011: Deterministic testing and bounded engineering

- Status: Accepted
- Date: 2026-08-16
- Extends: [ADR 0003](0003-preview-runtime-layers.md), [ADR 0006](0006-platform-process-and-recovery-boundaries.md), [ADR 0008](0008-protocol-freeze-and-migrations.md)

## Context

Quirl implements a parser, process graph, interactive editor, extension host,
and persistence boundaries. Example tests catch known cases, but closing the
behavior gap with mature shells also requires broad reproducible exploration of
combinations and failure paths.

[TigerStyle](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md)
offers useful principles: prioritize safety, performance, and developer
experience; put explicit limits on resources; fail fast on violated invariants;
explain why; and make randomized testing reproducible through deterministic
seeds. TigerBeetle's Zig-specific static-allocation and zero-dependency rules do
not transfer directly to an interactive Rust shell.

## Decision

Quirl adopts the following engineering contract:

1. Safety and correctness precede predictable performance, which precedes
   convenience. Developer experience remains a product requirement, not an
   excuse to weaken a boundary.
2. External input, retained output, queues, recursion/depth, test generation,
   deadlines, and persistent data have named upper bounds. Each boundary has an
   adversarial test that crosses the bound.
3. Generated tests use an explicit 64-bit seed and bounded case count. A failure
   reports the seed, case index, reference implementation, and source so the
   exact case can be replayed.
4. Native C1 behavior is tested differentially against clean Bash and Zsh
   processes when those interpreters are available. Frozen fixtures cover
   contractual edge cases; seeded generation explores combinations.
5. Unit and contract tests are supplemented by fault-injection tests at process,
   plugin, recovery, and terminal boundaries; real-PTY interaction tests; exact
   golden protocol fixtures; guest-side Lua tests; and measured release budgets.
6. Assertions are for internal programmer invariants. User input, unavailable
   resources, and operating failures remain structured `ShellError` values.
7. Dependencies must justify their boundary, maintenance, binary-size, and
   supply-chain costs. A zero-dependency rule is not adopted. The Cargo-native
   `xtask` uses approved workspace dependencies only: `clap` for its typed
   interface, `xshell` for readable injection-safe command orchestration, and
   Serde/JSON for replayable simulation artifacts. Lifecycle-sensitive capture
   and timeout paths continue to use `std::process::Command` directly.
8. A scheduled compatibility swarm is the narrow CI exception while repository
   traffic remains low. It invokes only the typed `xtask` simulator, requires
   both clean references, uploads its evidence, and opens an issue only for a
   completed report that proves a mismatch. The canonical pre-commit gate stays
   local.

The canonical commands are `cargo xtask check` and `cargo xtask test`. Both use
the stable default seed. A reported failure can be replayed with:

```console
cargo xtask test --seed <seed> --cases <case-count>
```

## Consequences

- Local and release evidence is reproducible without installing a separate task
  runner.
- Differential coverage can grow by adding generators and oracles without
  turning tests into nondeterministic flakes.
- Daily exploration varies the recorded seed without making a single run
  nondeterministic, and produces durable evidence that can be inspected or
  replayed locally.
- Every new simulator or generator must remain bounded and emit replay data.
- We deliberately do not require no allocation after startup, architecture-wide
  fixed-width integers, an assertion quota, or zero external dependencies.
  Those rules solve TigerBeetle's storage-engine constraints rather than
  Quirl's terminal and extension-runtime constraints.
