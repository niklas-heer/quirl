<div align="center">
  <img src="assets/logo.png" alt="Quirl logo" width="128" height="128">

  # Quirl

  **A well-stirred shell.**

  Bash muscle memory, typed data pipelines, and one Lua SDK —
  folded into a single fast Rust binary.

  [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
  [![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](rust-toolchain.toml)
  [![Status](https://img.shields.io/badge/status-prototype-informational.svg)](#status)
</div>

---

In German, a *Quirl* is the humble wooden whisk — a simple tool that takes
ingredients which don't naturally mix and folds them into something smooth.
That's exactly what this shell does.

Every shell makes you choose. Keep two decades of Bash muscle memory and
settle for grepping text. Switch to a data-first shell and relearn
everything you know. Or script your environment and hope a misbehaving
plugin doesn't take the whole session down with it. Quirl refuses the
trade-off: the commands you already type keep working, an explicit data
mode gives you typed, structured pipelines when text-wrangling isn't
enough, and one well-tooled Lua 5.4 SDK powers configuration, scripts,
prompts, completions, and plugins alike.

Rust implements the shell, the parser, the executor, and every
performance-critical path — and it validates every value that crosses the
Lua boundary, so a broken extension fails safely instead of ruining the
batter. The result is a single fast binary where your shell finally feels
as smart as your editor.

> [!IMPORTANT]
> Quirl is in an early **prototyping phase**. It's a runnable vertical slice,
> not a daily-driver shell yet — expect rapid, breaking changes while the
> design settles. See [Status](#status) for what's actually implemented today.

## Features

- **Familiar command mode** — ordinary Bash/Zsh-style commands run through a
  configured compatibility shell; `ls` is Quirl-native.
- **Typed data pipelines** — an explicit `data` mode for native, structured
  sources and transforms (`where` comparisons with `and`/`or`, nested `get`,
  `select`, `sort`, `take`, `first`, and `length`), with a Lua bridge
  (`lua return 20 + 22`) when you need it.
- **One Lua SDK, everywhere** — the same restricted Lua 5.4 runtime and
  Rust-validated API powers `config.lua`, scripts, prompt segments, and
  completion providers.
- **Semantic completions** — Tab opens a documented, IDE-style completion
  menu backed by one command catalog shared with docs, help, and AI tooling.
- **Structured, helpful errors** — diagnostics are machine-readable (JSON)
  and human-readable, with `--format` support across the CLI.
- **Capability-based plugins** — trusted Lua plugins run in-process with
  restricted module loading, instruction/time/memory budgets, and
  cancellation, so a runaway script can't take down your session.
- **Generated everything** — LuaLS-compatible stubs, Markdown docs, and JSON
  schemas are generated from the same Rust host definitions that power the
  runtime, so the SDK, editor completions, and AI context never drift apart.

## Status

Quirl is a runnable vertical-slice prototype, not a daily-driver shell yet.
The architecture and decisions behind it are documented in depth:

- [Language & product design](docs/language-design.html) — the intended
  product and interaction model.
- [Embedded-language decision report](docs/embedded-language-decision.html) —
  footprint, health, and complexity comparison across candidate runtimes.
- [ADR 0001: Lua is Quirl's extension language](docs/decisions/0001-lua-extension-language.md) —
  the accepted decision and its implementation status.
- [Runtime selection spike](docs/benchmarks/embedded-language-selection.md) —
  the earlier latency benchmarks that fed the decision.

The first Lua vertical slice is runnable today: a pinned Lua 5.4 runtime
through `mlua`, generated runtime bindings and LuaLS-compatible stubs, and one
Rust-validated API powering configuration, scripts, and prompt/completion
plugin registration. Lua prompt segments and completion providers run in
persistent, resource-limited VMs and feed the live editor. Config and plugins
reload atomically while the shell is running; invalid edits keep the complete
last-known-good generation. Editor keymap, semantic hints, prompt composition,
and picker settings are applied at the next safe prompt boundary. Data mode
uses Quirl's native Rust evaluator. The earlier Steel prototype and all of its
runtime dependencies have been removed.

## Quick start

The repository pins Rust 1.88 and installs it automatically through
[`rustup`](https://rustup.rs) via `rust-toolchain.toml`.

```console
git clone git@github.com:niklas-heer/quirl.git
cd quirl
cargo run -p quirl-cli
```

Inside the prompt, ordinary commands run through the configured
compatibility shell, `ls` is Quirl-native, <kbd>Tab</kbd> opens a documented
semantic completion menu, and the current mode is always visible. Use
`mode data` for native structured pipelines, or bridge explicitly with
`lua return 20 + 22`.

Quirl discovers `config.lua` and sorted `plugins/*.lua` under
`$QUIRL_CONFIG_DIR`, `$XDG_CONFIG_HOME/quirl`, or `~/.config/quirl`. It watches
their contents during an interactive session and installs a changed config and
plugin set only after the whole generation validates.

### Non-interactive surfaces

```console
cargo run -p quirl-cli -- run examples/hello.lua Codex
cargo run -p quirl-cli -- data '[{"name":"api","status":"up"}] | get name'
cargo run -p quirl-cli -- check examples/hello.lua --format json
cargo run -p quirl-cli -- config check examples/config.lua --format json
cargo run -p quirl-cli -- plugin check examples/plugin.lua --format json
cargo run -p quirl-cli -- test examples/lua_tests.lua
cargo run -p quirl-cli -- sdk --format text
cargo run -p quirl-cli -- complete 'git commit --am'
cargo run -p quirl-cli -- catalog --format json
cargo run --release -p quirl-bench
```

The main benchmark accepts an official Fennel single-file library with
`--fennel /path/to/fennel.lua`. Isolated, reproducible spikes cover
TypeScript/QuickJS-NG, Luau, PocketPy, and standalone size/RSS probes,
without mixing mutually exclusive runtime features into the shell itself.

## Configuring Quirl

Configuration, scripts, and plugins share the same Lua 5.4 SDK. A minimal
`config.lua` looks like this:

```lua
---@type quirl.Config
local config = quirl.config {
  editor = { keymap = "helix", semantic_hints = true },
  picker = { layout = "adaptive", preview = true },
  prompt = {
    left = { "directory", "git_branch", "git_state" },
    right = { "jobs", "duration", "status" },
  },
}

return config
```

Plugins register prompt segments and completion providers against the same
capability-checked API:

```lua
quirl.prompt.add_segment {
  name = "project",
  deadline_ms = 8,
  render = function(ctx)
    return ctx.project_name
  end,
}

quirl.completion.add_provider {
  command = "deploy --environment",
  complete = function(_ctx)
    return { "staging", "production" }
  end,
}
```

Run `cargo run -p quirl-cli -- sdk --format markdown` to see the full,
generated API reference for what's available inside `quirl.*`.

## Workspace

Quirl is organized as a Cargo workspace of small, focused crates:

| Crate           | Responsibility                                                          |
| ---------------- | ------------------------------------------------------------------------ |
| `quirl-cli`      | Binary, REPL, script runner, and machine-facing commands                |
| `quirl-core`     | Compatibility execution, native `ls`, values, and errors                |
| `quirl-data`     | Native structured sources, predicates, projection, sorting, and limits   |
| `quirl-lua`      | Restricted Lua 5.4 runtime, Rust schemas, resource budgets, SDK generation |
| `quirl-syntax`   | The explicit command/data-mode interaction grammar                      |
| `quirl-catalog`  | One schema for completion, help, docs, validation, and AI                |
| `quirl-ui`       | Semantic highlighting, IDE completion menu, prompt, diagnostics          |
| `quirl-bench`    | Reproducible Lua/Rhai/Fennel runtime spike                               |
| `spikes/`        | Isolated runtime, type-checking, binary-size, and peak-RSS measurements |

## Contributing

Quirl is early and moving fast; the architecture decision records in
[`docs/decisions`](docs/decisions) are the best place to see what's settled
and what's still open. Issues and discussion are welcome while the design
solidifies.

## License

Quirl is licensed under the [MIT License](LICENSE).
