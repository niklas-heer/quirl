# Quirl

Everything you need, mixed in.

Quirl is a proposed modern shell and terminal development environment: familiar
Bash/Zsh command entry, typed data pipelines, a well-tooled Lua extension
language, semantic completions, structured errors, rich built-in UI, and a
capability-based plugin system in one fast Rust application. Lua 5.4 is the sole
embedded language for configuration, automation, and trusted plugins; Rust
remains the implementation language and validates every host boundary.

The project now has a runnable vertical-slice prototype. Read the interactive
[language and product design](docs/language-design.html) for the intended product,
the [footprint, health, and complexity decision report](docs/embedded-language-decision.html),
the [accepted architecture decision](docs/decisions/0001-lua-extension-language.md),
and the earlier
[runtime selection spike](docs/benchmarks/embedded-language-selection.md) for
latency details.

## Next milestone

The first Lua vertical slice is now runnable: a pinned Lua 5.4 runtime through
`mlua`, generated runtime bindings and LuaLS-compatible stubs, and the same
Rust-validated API powering configuration, scripts, and prompt/completion plugin
registration. Lua prompt segments and completion providers now feed the live
editor through one persistent, resource-limited VM per plugin. Data mode uses
Quirl's native Rust evaluator. The earlier Steel prototype and all of its runtime
dependencies have been removed.

## Try the prototype

The repository pins Rust 1.88 and installs it automatically through rustup.

```console
cargo run -p quirl-cli
```

Inside the prompt, ordinary commands run through the configured compatibility
shell, `ls` is Quirl-native, Tab opens a documented semantic completion menu,
and the mode is always visible. Use `mode data` for native structured pipelines
or bridge explicitly with `lua return 20 + 22`.
Quirl discovers `config.lua` and sorted `plugins/*.lua` under
`$QUIRL_CONFIG_DIR`, `$XDG_CONFIG_HOME/quirl`, or `~/.config/quirl`.

Useful non-interactive surfaces:

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
`--fennel /path/to/fennel.lua`. Isolated reproducible spikes cover
TypeScript/QuickJS-NG, Luau, PocketPy, and isolated size/RSS probes without mixing
mutually exclusive runtime features into the shell.

## Workspace

- `quirl-cli` — binary, REPL, script runner, and machine-facing commands
- `quirl-core` — compatibility execution, native `ls`, values, and errors
- `quirl-data` — native structured sources and `where`/`select`/`get`/`first`/`length` transforms
- `quirl-lua` — restricted Lua 5.4 runtime, Rust schemas, resource budgets, and SDK generation
- `quirl-syntax` — explicit command/data-mode interaction grammar
- `quirl-catalog` — one schema for completion, help, docs, validation, and AI
- `quirl-ui` — semantic highlighting, IDE completion menu, prompt, diagnostics
- `quirl-bench` — reproducible Lua/Rhai/Fennel runtime spike
- `spikes/` — isolated runtime, type-checking, binary-size, and peak-RSS measurements
