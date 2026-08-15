# AGENTS.md — working on Quirl

Quirl is a Rust workspace implementing a shell with typed data pipelines and
an embedded, sandboxed Lua 5.4 extension runtime. This file captures the
project-specific rules that generic Rust knowledge won't give you.

## Architecture: respect the layering

Dependency direction is strict and one-way, codified in
[ADR 0002](docs/decisions/0002-crate-layering.md):

- Foundation crates — `quirl-core`, `quirl-catalog`, `quirl-syntax` — depend
  only on serde-level libraries. They must never depend on `quirl-ui`,
  `quirl-cli`, `quirl-data`, or `quirl-lua`.
- `quirl-data` and `quirl-lua` sit on `quirl-core` only.
- `quirl-ui` may use catalog, core, lua, and syntax.
- `quirl-cli` is the only composition root; it is the only crate that sees
  everything.

When adding functionality, put it in the lowest crate that can own it, and
never invert an arrow to make something compile.

## One error type, everywhere

All fallible cross-crate paths return `Result<T, quirl_core::ShellError>`.
`ShellError` carries an `ErrorCode`, message, labels, context, and help, and
derives `Serialize` so `--format json` works for free.

- Do not introduce `anyhow`, new error enums, or `thiserror` derives. Errors are
  hand-built for serialization control.
- Map new failure domains onto an existing `ErrorCode`, or extend the enum in
  `crates/quirl-core/src/error.rs` if genuinely new.
- Every error must render well both as JSON and through
  `quirl_ui::render_error`. Write the `help` text; a diagnostic without a
  suggested fix is half done.
- `clippy::unwrap_used` and `clippy::expect_used` are denied by workspace
  lints (tests are exempt via `clippy.toml`). Rare true invariants such as
  mutex poisoning may carry a targeted `#[allow]` with a reason.

## The Lua boundary is a security boundary

All Lua embedding lives in `quirl-lua`. Rules that must hold:

- Every VM runs under a `LuaPolicy` (memory limit, instruction budget, wall
  deadline, cancellation). Never create an unrestricted `mlua::Lua`.
- The stdlib is restricted (`TABLE|STRING|MATH|UTF8`); `io`, `os`, `debug`,
  `require`, and `package` stay removed. Do not re-expose them.
- Values crossing Lua → Rust are deserialized into typed structs with
  `#[serde(deny_unknown_fields)]` and then validated. Never consume raw
  `mlua::Value` in higher crates; convert at the boundary.
- A misbehaving script must fail with a `ShellError` (`Lua`, `Validation`, or
  `ResourceLimit`) — it must never panic the host or hang the session.

## Generated artifacts: edit the source, not the output

- The Lua SDK (LuaLS stubs, JSON schema, Markdown docs) is generated from the
  single `HOST_API` table in `quirl-lua`. To change the host API, edit
  `HOST_API`, then run `mask sdk` to regenerate `docs/quirl.lua` (a test
  asserts the checked-in file matches `sdk_lua()` exactly). Never hand-edit
  `docs/quirl.lua`.
- Command metadata (help, completions, docs, AI export) comes from
  `Catalog::builtin()` in `quirl-catalog`. New commands and flags are added
  there once — never hardcode help strings or completion lists elsewhere.

## Workspace hygiene

- Toolchain is pinned to Rust 1.88 via `rust-toolchain.toml`; don't use
  features from newer compilers.
- `spikes/` directories are intentionally separate Cargo workspaces so that
  mutually exclusive engine features (Luau, QuickJS, …) never unify with the
  shell's Lua 5.4 build. Never add spikes as workspace members or import
  their dependencies into `crates/`.
- `quirl-bench` is research tooling (`publish = false`), not product code.
- No `unsafe` exists in `crates/` today. Adding any requires a comment
  explaining why it is sound and should be a last resort.
- No feature flags on the main crates; keep it that way unless an ADR says
  otherwise.

## Testing

- Tests live in-crate as `#[cfg(test)] mod tests` in the same file — no
  separate `tests/` directories. Follow that pattern.
- Name tests as behavior sentences in snake_case, e.g.
  `instruction_budget_stops_runaway_code`.
- `mask test` runs the workspace tests plus the guest-side Lua tests
  (`cargo run -p quirl-cli -- test examples/lua_tests.lua`); run it for any
  change touching the Lua runtime or SDK.
- Sandbox changes need adversarial tests: prove the budget, limit, or
  restriction actually trips.

## Process

- `mask check` is the canonical local quality gate and must pass before every
  commit. Do not add CI workflows; local mask tasks deliberately replace CI
  while project traffic is low.
- Conventional commits (`feat`, `fix`, `docs`, `refactor`, `chore`, `bench`),
  present tense, optionally scoped, e.g. `feat(lua): add completion budgets`.
- Significant design choices go through an ADR in `docs/decisions/`.
  ADR 0001 is the standing contract: Lua is the only extension language,
  Rust validates everything at the boundary. ADR 0002 fixes the crate
  dependency graph. `docs/language-design.md` is the product specification;
  its §13 delivery sequence and acceptance gates define what each phase
  must prove.
- Quirl is a prototype moving fast: prefer extending the existing patterns
  (`ShellError`, `HOST_API`, `Catalog::builtin`, `LuaPolicy`) over inventing
  parallel mechanisms. Breaking changes are fine; drift is not.
