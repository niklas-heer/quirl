# Task: Polish the README and codify the architecture

You are an LLM agent working on Quirl, a Rust workspace implementing a shell
with typed data pipelines and a sandboxed Lua 5.4 extension runtime.

Before doing anything, read `AGENTS.md` at the repo root. It defines the
crate layering, the single `ShellError` contract, the Lua sandbox rules, the
generated artifacts you must never hand-edit, and the testing conventions.
All work below must follow it.

## Ground rules

- Work directly on `master`/`main`. Do **not** set up GitHub Actions or any
  CI — that is a deliberate decision for now (traffic is low and CI would
  burn requests); local tooling replaces it, see the maskfile task.
- Use conventional commits (`feat`, `fix`, `docs`, `refactor`, `chore`), one
  commit per logical change, not one big commit.
- After each code change run the verification commands (via the maskfile
  once it exists) and make sure everything passes before committing.

## 1. Create a `maskfile.md` (do this first)

We use [mask](https://github.com/jacobdeichert/mask) as the task runner. Its
config is a `maskfile.md` at the repo root. Since we have no CI, this file is
the canonical set of commands to run for every change. Define at least:

- `mask fmt` — `cargo fmt --all`
- `mask lint` — `cargo clippy --workspace --all-targets -- -D warnings`
- `mask test` — `cargo test --workspace` followed by the Lua guest tests:
  `cargo run -p quirl-cli -- test examples/lua_tests.lua`
- `mask check` — fmt (check mode), lint, and test in sequence; the "run this
  before every commit" command
- `mask sdk` — regenerate `docs/quirl.lua` from the Rust `HOST_API`
  definitions (see `quirl-lua`), so the golden-file test never drifts
- `mask run` — `cargo run -p quirl-cli`

After creating it, add a short note to `AGENTS.md` (Process section) and the
README (Contributing section) saying that `mask check` must pass before every
commit, replacing any expectation of CI.

## 2. Codify architecture decisions

1. **Workspace lints**: add a `[workspace.lints]` block to the root
   `Cargo.toml` (and `[lints] workspace = true` in each crate) denying
   `clippy::unwrap_used` and `clippy::expect_used`, with
   `allow-unwrap-in-tests = true` / `allow-expect-in-tests = true` in a
   `clippy.toml`. Fix any violations this surfaces, following the
   `ShellError` conventions in `AGENTS.md`. Mutex-poison `expect`s may keep a
   targeted `#[allow]` with a reason.
2. **ADR 0002 — crate layering**: write
   `docs/decisions/0002-crate-layering.md` in the same format as ADR 0001,
   recording the one-way dependency graph (foundation crates
   `quirl-core`/`quirl-catalog`/`quirl-syntax` → `quirl-data`/`quirl-lua` →
   `quirl-ui` → `quirl-cli` as sole composition root; `spikes/` stay separate
   workspaces; `quirl-bench` is research-only).
3. **Drop dead dependency**: remove `thiserror` from the workspace and from
   `quirl-core` — it is declared but never used, and `AGENTS.md` documents
   that errors are hand-built.

## 3. README improvements

1. **Fix the doc links**: `docs/language-design.html` and
   `docs/embedded-language-decision.html` render as raw HTML source on
   GitHub. Convert them to Markdown (preferred) or link a rendered version.
   Update the links in the README and in ADR 0001 if it references them.
2. **Add a comparison section** ("How is Quirl different?") answering the
   obvious question against Zsh/Bash, Nushell, and Fish in a few sentences
   each: Quirl keeps Bash muscle memory (unlike Nushell), adds typed data
   pipelines (unlike Zsh/Fish), and has exactly one sandboxed extension
   language with a generated, typed SDK.
3. **Add a short roadmap** under the Status section: list concretely what
   "daily-driver ready" requires (e.g. job control, redirects/pipe operators
   in command mode, history, scripting surface) with checkboxes reflecting
   current state. Derive the actual state from the code, don't guess.
4. **Demo placeholder**: add a commented-out demo section near the top of
   the README (`<!-- TODO: asciinema/VHS recording -->`) noting that a
   recording of the completion menu and a data-mode pipeline should go
   there. Do not fake a recording.

## Acceptance criteria

- `mask check` exists and passes cleanly on the final state.
- The `docs/quirl.lua` golden-file test still passes (regenerate via
  `mask sdk` if the host API changed — it should not need to).
- No new crates, no new error types, no CI files, no changes under `spikes/`.
- README renders correctly on GitHub with no raw-HTML links.
