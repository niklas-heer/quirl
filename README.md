<div align="center">
  <img src="assets/logo.png" alt="Quirl logo" width="128" height="128">

  # Quirl

  **A well-stirred shell.**

  Bash muscle memory, typed data pipelines, and one Lua SDK —
  folded into a single fast Rust binary.

  [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
  [![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](rust-toolchain.toml)
  [![Status](https://img.shields.io/badge/status-0.1_Unix_release_candidate-blue.svg)](#status)
</div>

---

<!-- The release checklist defines the real-terminal recording shot list.
     Publish the captured artifact here only after it has been reviewed. -->

## Product tour

Run `cargo xtask demo` for a deterministic, text-only tour of typed data, semantic
completion, sandboxed Lua, diagnostics, and generated command documentation.
The checked-in [`scripts/demo.tape`](scripts/demo.tape) recipe drives the real
interactive binary in a PTY for the visual capture; it does not substitute
prewritten output for the shell. The
[release checklist](docs/release-checklist.md#5-capture-the-real-terminal-demo)
defines the review and privacy checks before a recording is published here.
`scripts/record-demo.sh <release-binary> <expected-sha256>` verifies the exact
prebuilt artifact plus the VHS, ttyd, ffmpeg, and font prerequisites, then runs
the capture inside a disposable Quirl home and state environment.

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
> Quirl 0.1 is a **Unix release candidate**. Linux and macOS are the supported
> interactive platforms. The Windows backend remains cross-compiled and
> contract-tested as a portability layer, but native Windows terminal behavior
> is not part of the 1.0 release promise or release gate. See [Status](#status)
> and the [support-scope ADR](docs/decisions/0010-unix-first-release-scope.md)
> for the exact contract.

## Features

- **Familiar command mode** — quoting, redirects, byte pipes, boolean lists,
  bounded expansions, background jobs, and `ls` run through one native process
  graph; here-documents, process substitution, loops, functions, and other
  dialect control forms use an explicit Bash or Zsh island instead of a partial
  native imitation.
- **Typed data pipelines** — an explicit `data` mode for native, structured
  sources and transforms (`where` comparisons with `and`/`or`, nested `get`,
  `select`, `sort`, `take`, `first`, and `length`), with a Lua bridge
  (`lua return 20 + 22`) when you need it.
- **One Lua SDK, everywhere** — the same restricted Lua 5.4 runtime and
  Rust-validated API powers `config.lua`, scripts, prompt segments, and
  completion providers.
- **Semantic completions** — Tab opens a documented, IDE-style completion
  menu backed by one command catalog shared with docs, help, and AI tooling.
- **Rich inline terminal surface** — capable Linux/macOS TTYs get a
  Ratatui-rendered context row, syntax-aware editor, advisory diagnostics,
  completion documentation, overlays, and persistent textual status without
  trapping command output outside normal scrollback.
- **Durable searchable history** — commands persist across sessions and
  <kbd>Ctrl-R</kbd> opens a keyboard-first, line-oriented reverse search that
  remains usable on simple terminals.
- **Structured, helpful errors** — diagnostics are machine-readable (JSON)
  and human-readable, with `--format` support across the CLI.
- **Capability-based plugins** — trusted Lua plugins run in-process with
  restricted module loading, instruction/time/memory budgets, and
  cancellation, so a runaway script can't take down your session.
- **Generated everything** — LuaLS-compatible stubs, Markdown docs, and
  versioned JSON metadata are generated from the same Rust host definitions that power the
  runtime, so the SDK, editor completions, and AI context never drift apart.
- **Built-in language service** — `quirl lsp` gives Lua and native Quirl (`.qrl`,
  with `.quirl` and `.🌀` input aliases) files
  deterministic diagnostics, completion, hover, signatures, and generated
  module docs over stdio without evaluating the document.
- **Explicit agent and package contracts** — versioned deny-unknown JSON,
  token-budgeted installed context, schema/content hashes, and package metadata
  quality gates are available without executing extensions or publishing.
- **Permission-locked plugin platform** — local trusted Lua, Wasm-component,
  and out-of-process packages use versioned manifests, requested/granted
  capabilities, SHA-256 source locks, atomic lifecycle changes, and doctor
  diagnostics; isolated boundaries are validated without pretending to
  execute a Wasm runtime that is not yet selected.
- **Typed extension events and live views** — immutable ordered event records,
  explicit mutation grants, isolated deadline-bounded Lua handlers, composed
  catalog/completion/panel providers, escape-safe directory/process panels,
  and cancellable capacity-bounded `watch` sample history.

## How is Quirl different?

### Bash and Zsh

Quirl keeps the command syntax and muscle memory of a Bourne-family shell by
routing the frozen C0/C1-core subset through a native process graph.
Unlike Bash and Zsh, it also has an explicit data mode where pipelines carry
typed values instead of requiring every structured task to become text processing.

### Nushell

Nushell makes structured data the primary shell language. Quirl keeps familiar
Bash-style command entry as the default and makes the typed grammar an explicit
mode, so existing command-line habits and snippets remain useful. Quirl's data
grammar is intentionally much smaller today.

### Fish

Quirl shares Fish's emphasis on discoverability, rich completion, and a helpful
interactive experience. It adds typed data pipelines and standardizes config,
scripts, prompts, completions, and trusted plugins on exactly one sandboxed Lua
5.4 language with a generated, Rust-validated SDK. Fish is a mature daily-driver
shell; Quirl is a Unix release candidate and not yet comparably mature.

## Status

Quirl 0.1 is a runnable Unix release candidate: the Preview shell, scriptable
authoring stack, permission-locked trusted-Lua extensions, typed extension
protocols, reference-shell runners, panels, bounded watch history, recovery,
and portable process backend work end to end. Public contract identities and
migrations are reviewed in one golden manifest; compatibility, release
performance, security, and accessibility evidence runs under the local gate.
Wasm components remain validation-only. Checksummed out-of-process adapters now
execute a narrow, versioned initialization handshake with exact launch grants,
deadlines, output bounds, and process-tree containment. The native compatibility
surface is now explicit: the frozen C1-core subset is native on Linux and macOS,
while here-documents, process substitution, loops, functions, and dialect
control forms are intentionally C2 Bash/Zsh islands for 1.0. Windows is a
best-effort portability target, not an interactive release blocker. The
remaining release task is to refresh the named performance record against the
exact candidate artifact and complete the human release checklist.
The architecture and decisions behind it are documented in depth:

- [Language & product design](docs/language-design.md) — the intended
  product and interaction model.
- [Embedded-language decision report](docs/embedded-language-decision.md) —
  footprint, health, and complexity comparison across candidate runtimes.
- [ADR 0001: Lua is Quirl's extension language](docs/decisions/0001-lua-extension-language.md) —
  the accepted decision and its implementation status.
- [ADR 0002: one-way crate layering](docs/decisions/0002-crate-layering.md) —
  the dependency graph and composition boundaries for the Rust workspace.
- [Agent and package contracts](docs/agent-and-package-contracts.md) — stable
  AI discovery, validation, `plugin.toml`, build, and dry-run publish formats.
- [ADR 0004: Phase 2 product layers](docs/decisions/0004-phase-2-contract-and-language-service-layers.md) —
  one-way boundaries for contracts and the language service.
- [Plugin platform v0.1](docs/plugin-platform.md) and
  [ADR 0005](docs/decisions/0005-plugin-platform-layer.md) — permission locks,
  trusted Lua grants, isolation manifests, and lifecycle recovery.
- [Semantic command catalog schema v4](docs/catalog-schema.md) and
  [ADR 0007](docs/decisions/0007-semantic-catalog-v4.md) — stable identities,
  typed arguments and IO, exact metadata quality, provenance, and cache migration.
- [Extension events, typed views, and live pipelines](docs/extension-events-and-live-views.md) —
  immutable records, declared actions, contribution gates, terminal safety,
  plain fallbacks, and bounded streaming behavior.
- [Protocol compatibility](docs/protocol-compatibility.md),
  [ADR 0008](docs/decisions/0008-protocol-freeze-and-migrations.md), and the
  [reviewed freeze manifest](docs/protocol-freeze-v1.json) — public identities,
  reader policies, and deterministic migrations.
- [ADR 0010: Unix-first release scope](docs/decisions/0010-unix-first-release-scope.md) —
  supported platforms, best-effort Windows portability, and the final native
  versus reference-shell compatibility boundary.
- [ADR 0012: Ratatui interactive surface](docs/decisions/0012-ratatui-interactive-surface.md) —
  capable-TTY default selection, inline-terminal lifecycle, Reedline fallback,
  and config schema v2 migration.
- [ADR 0013: Lua-configured themes](docs/decisions/0013-lua-config-themes.md) —
  bounded semantic palettes shared by both terminal surfaces, with Tokyo Night
  as the default.
- [ADR 0014: external history provider boundary](docs/decisions/0014-external-history-provider-boundary.md) —
  the bounded, failure-isolated contract required before tools such as Atuin
  can augment native history.
- [ADR 0015: bounded theme preview gallery](docs/decisions/0015-bounded-theme-preview-gallery.md) —
  no-JavaScript previews derived from the same validated palettes the terminal
  surfaces consume.
- [Security and accessibility audit](docs/security-accessibility-audit-v0.1.md)
  and [1.0 performance record](docs/benchmarks/release-v1.0.md) — adversarial
  boundaries, text fallbacks, named hardware, reproducible budgets, and outcomes.
- [Release checklist](docs/release-checklist.md) — exact automated and manual
  gates, artifact checks, and the real-terminal demo recording shot list.
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

### Roadmap to a daily driver

- [x] Familiar command execution through a native command graph.
- [x] Explicit typed data mode with a focused native pipeline grammar.
- [x] Sandboxed Lua configuration, scripts, tests, and trusted extensions.
- [x] Semantic command catalog, completions, and atomic live config reload.
- [x] Attributed Fish/Bash/Zsh and supplied help/man ingestion with build and explain commands.
- [x] Native Linux/macOS job control and background-process lifecycle management.
- [x] One command-mode execution graph for redirects and byte pipes across
  native built-ins and external commands.
- [x] Durable, searchable command history with the shared typed Ctrl-R picker.
- [x] A shared typed picker spanning history, files, palette actions, jobs, and data.
- [x] Named end-to-end PTY performance measurements with misses recorded.
- [x] Deterministic Lua/native-Quirl (`.qrl`) run, format, check, lint, test, documentation,
  language-service, package, and agent-contract tooling.
- [x] Permission-locked trusted-Lua plugins, typed events/actions, catalog,
  completion and panel contributions, and validated isolated-runtime boundaries.
- [x] Bash/Zsh reference runners, bounded directory/process views and watch
  history, versioned recovery, and a contract-tested Windows process backend.
- [x] Reviewed protocol identities and migrations, real Bash/Zsh differential
  fixtures, terminal-safety and accessibility audits, and a reproducible named
  1.0 performance-gate harness.
- [ ] Refresh the named 1.0 performance record for the current `panic=unwind`
  release profile.
- [x] Executing isolated process adapters and versioned asynchronous
  picker/completion request, cancellation, deadline, and stale-result contracts.
- [x] Freeze the 1.0 support scope: Linux/macOS interactive support, best-effort
  Windows portability, and explicit Bash/Zsh islands for non-native dialect forms.
- [ ] Run and sign off the [release checklist](docs/release-checklist.md), then
  tag and publish the exact measured candidate.

### Platform support

| Platform | 1.0 support level | Promise |
| --- | --- | --- |
| Linux | Supported | Interactive editor, PTY handoff, job control, native C1-core command execution, and release smoke tests |
| macOS | Supported | Interactive editor, PTY handoff, job control, native C1-core command execution, and release smoke tests |
| Windows | Best effort | The portable process contract cross-compiles and remains contract-tested; native terminal handoff, suspend semantics, and hands-on release validation are not promised |

Unsupported native Bash/Zsh syntax is not a platform gap. Use an explicit
`bash { ... }` or `zsh { ... }` island interactively, or `quirl run --lang
bash` / `quirl run --lang zsh` for a script. Quirl starts the selected reference
shell without user startup files and keeps capture, cancellation, and process
lifetime bounded.

## Quick start

The repository pins Rust 1.88 and installs it automatically through
[`rustup`](https://rustup.rs) via `rust-toolchain.toml`.

```console
git clone git@github.com:niklas-heer/quirl.git
cd quirl
cargo run -p quirl-cli
```

Inside the prompt, ordinary commands and native `ls` share one process graph,
<kbd>Tab</kbd> opens semantic completion, and the current mode is always visible.
On a capable TTY, the default Ratatui inline frame keeps working-directory and
Git context above the highlighted input, adds advisory diagnostics and
completion documentation when useful, and keeps textual mode/editor state in a
bottom status row. The frame is released before every foreground command, so
output, vim/less, suspension, and scrollback remain normal terminal behavior.
`TERM=dumb`, a non-TTY or very short stderr, and `ui.surface = "simple"` select
the Reedline fallback. Neither surface requires a patched font.
Press <kbd>Alt-M</kbd> to toggle command/data mode, <kbd>Ctrl-R</kbd> for
history, <kbd>Ctrl-T</kbd> for files, and <kbd>Ctrl-K</kbd> for catalog actions.
Press <kbd>F1</kbd> to inspect contextual help from the same command catalog:
type a command or prefix first, use <kbd>Tab</kbd> (or <kbd>F1</kbd> again) to
cycle matching entries, use left/right to scroll long details, and press
<kbd>Esc</kbd> or <kbd>Enter</kbd> to close without changing the current input.
The picker retains original typed values; `quirl pick` provides the same exact,
fuzzy, and inverse query engine as a line-oriented/scriptable fallback.

Quirl discovers `config.lua` under `$QUIRL_CONFIG_DIR`,
`$XDG_CONFIG_HOME/quirl`, or `~/.config/quirl`. Interactive plugins come only
from enabled entries in the permission lock under `QUIRL_PLUGIN_HOME` (or the
configuration directory's `plugins` state folder); each source is
integrity-checked and receives exactly its locked grants. Quirl watches the
active sources and installs a changed config/plugin generation only after the
whole candidate validates.

Interactive history is stored at `$QUIRL_HISTORY` when set, otherwise at
`$XDG_STATE_HOME/quirl/history` or `~/.local/state/quirl/history`. Quirl honors
`NO_COLOR` for its banner, diagnostics, semantic highlighting, and rich-surface
theme while preserving the inline layout; typed picker shortcuts and textual
mode commands remain usable without color.

### Non-interactive surfaces

```console
cargo run -p quirl-cli -- run examples/hello.lua Codex
cargo run -p quirl-cli -- new script
cargo run -p quirl-cli -- new script --lang quirl
cargo run -p quirl-cli -- check . --format json
cargo run -p quirl-cli -- fmt . --check
cargo run -p quirl-cli -- test
cargo run -p quirl-cli -- data '[{"name":"api","status":"up"}] | get name'
cargo run -p quirl-cli -- check examples/hello.lua --format json
cargo run -p quirl-cli -- config check examples/config.lua --format json
cargo run -p quirl-cli -- plugin check examples/plugin.lua --format json
cargo run -p quirl-cli -- test examples/lua_tests.lua
cargo run -p quirl-cli -- sdk --format text
cargo run -p quirl-cli -- complete 'git commit --am'
cargo run -p quirl-cli -- pick --source history --query cargo
cargo run -p quirl-cli -- pick --source files --query src
cargo run -p quirl-cli -- catalog --format json
cargo run -p quirl-cli -- describe 'quirl run' --format markdown
cargo run -p quirl-cli -- doc --format html --output target/quirl-docs/catalog.html
cargo run -p quirl-cli -- agent manifest --format json
cargo run -p quirl-cli -- agent context 'deploy the billing service' --token-budget 6000
cargo run -p quirl-cli -- package build --manifest examples/package/plugin.toml
cargo run -p quirl-cli -- package publish --dry-run --manifest examples/package/plugin.toml
cargo run -p quirl-cli -- lsp
cargo run -p quirl-cli -- index build
cargo run -p quirl-cli -- index explain git commit
cargo run -p quirl-cli -- events schema --format json
cargo run -p quirl-cli -- view directory .
cargo run -p quirl-cli -- view processes
cargo run -p quirl-cli -- view panel cluster
cargo run -p quirl-cli -- watch 'ls . | length' --samples 3 --interval-ms 250
cargo run --release -p quirl-bench
```

`quirl index build` reads standard Fish, Bash, and Zsh completion directories
without sourcing or executing them. `--fish`, `--bash`, and `--zsh` accept
repeatable explicit files or directories. Common static Zsh `_arguments`,
`_describe`, and `_values` forms are translated; dynamic providers are recorded
but never run. Repeatable `--help PATH` and `--man PATH` inputs heuristically
ingest options from bounded, already-supplied text/files—Quirl never invokes the
documented command or `man`. Every imported fact records its source, confidence,
origin, and fingerprint for `quirl index explain`. Because `--help` is an input
flag on `index build`, use `quirl index build -h` for that subcommand's usage.

`quirl lsp` speaks standard LSP `Content-Length` framing over stdin/stdout.
Lua editor intelligence is generated from the same `HOST_API` as the runtime;
Native Quirl command intelligence is generated from the loaded semantic catalog.
`.qrl` is the canonical extension; `.quirl` and `.🌀` are accepted input aliases.
The server compiles and lints Lua but never invokes the compiled chunk or
executes native Quirl commands. See the [language-service protocol and editor
contract](docs/language-service.md).

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
  schema_version = 3,
  editor = { keymap = "emacs", semantic_hints = true, banner = "full" },
  picker = { layout = "adaptive", preview = true },
  prompt = {
    symbols = "auto",
    left = { "directory", "git_branch", "git_state" },
    right = { "jobs", "duration", "status" },
    transient = true,
  },
  ui = { theme = "tokyo-night", surface = "auto", statusline = { hints = true } },
  completion = { auto = false, min_chars = 2 },
}

return config
```

Emacs editing is the production default and provides the complete familiar
shell key set, including history, word editing, and Ctrl-D to exit. Vim remains
a supported opt-in. The rich surface owns its Emacs, Vim, and Helix editor
states; the simple surface still uses Reedline, whose Helix keymap remains an
experimental compatibility option.
The welcome is shown on every interactive launch by default; set
`editor.banner` to `compact` or `none` if you prefer less startup chrome.

Config schema v3 adds named semantic themes shared by both terminal surfaces;
Tokyo Night is the default. Quirl includes 30 widely used dark themes, with
palettes curated from the maintained [Gogh terminal-theme catalog](https://github.com/Gogh-Co/Gogh),
plus `ansi` as a conservative compatibility alternative:

```text
ayu-dark             catppuccin-mocha  cobalt-2         dracula
everforest-dark-medium  github-dark    gotham           gruvbox-dark
horizon-dark         kanagawa-wave     material         monokai-dark
moonfly              night-owl         nord             oceanic-next
one-dark             one-half-black    oxocarbon-dark   palenight
papercolor-dark      rose-pine-moon    snazzy           solarized-dark
sonokai              srcery            synthwave        tokyo-night
tomorrow-night       zenburn
```

Unversioned v0 and explicit v1/v2 configuration migrate deterministically to
v3 defaults before validation; future versions fail closed. `ui.surface = "auto"` prefers Ratatui only when
terminal capabilities are sufficient, `rich` requests that same rich path, and
`simple` selects Reedline explicitly.

Custom themes are ordinary, bounded Lua data under `ui.themes`. Each theme must
provide every generated `quirl.ThemeColors` role as an exact `#RRGGBB` value;
select it by assigning its table key to `ui.theme`. Quirl validates the complete
palette before activation and never calls Lua while painting the terminal.

```lua
config.ui.themes["my-theme"] = {
  accent_command = "#a6e3a1", accent_data = "#cba6f7",
  context_primary = "#89dceb", context_secondary = "#f5c2e7",
  muted = "#6c7086", border = "#45475a", status_background = "#313244",
  error = "#f38ba8", warning = "#f9e2af", hint = "#89b4fa",
  string = "#a6e3a1", operator = "#cdd6f4",
  expansion = "#89b4fa", number = "#fab387",
}
config.ui.theme = "my-theme"
```

`prompt.symbols` makes font support explicit:

- `auto` (the default) uses broadly supported Unicode only in a UTF-8 locale
  and falls back to ASCII; it never assumes a patched font.
- `plain` forces ASCII, while `unicode` forces the standard Unicode profile.
- `nerd_font` is an opt-in Powerline/Nerd Font profile with compact symbols and
  separators. `TERM=dumb` still forces the legible plain profile.

Symbols are decoration, not state: mode, status, job count, duration, Git state,
and directory context retain textual meaning. `NO_COLOR` independently disables
styling without removing prompt information.

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

The current typed configuration CLI is deliberately file-backed and
line-oriented:

```console
cargo run -p quirl-cli -- config check examples/config.lua
cargo run -p quirl-cli -- config get examples/config.lua editor.keymap
cargo run -p quirl-cli -- config set examples/config.lua picker.preview false
cargo run -p quirl-cli -- config tui examples/config.lua
cargo run -p quirl-cli -- config web examples/config.lua
cargo run -p quirl-cli -- config fmt examples/config.lua --check
cargo run -p quirl-cli -- config export examples/config.lua --format json
cargo run -p quirl-cli -- config diff personal.lua work.lua
cargo run -p quirl-cli -- config migrate examples/config.lua --dry-run
cargo run -p quirl-cli -- config doctor examples/config.lua
```

`config set` and `config web` patch only recognized literal fields, preserve
surrounding Lua source, validate the complete candidate before activation,
replace the file atomically, and retain the prior source as `config.lua.bak`.
Values controlled by Lua expressions must be edited in code. `config web`
renders bounded no-JavaScript cards for every built-in and configured custom
theme, binds only to `127.0.0.1`, prints a private tokenized URL, sends no-cache
headers, bounds requests and read time, merges non-overlapping external edits,
and rejects stale or conflicting source changes instead of silently
overwriting them. Use `--port 8787` to choose a loopback port; the default
selects one automatically.

`config export`, `diff`, and `doctor` read only the authoritative Lua source
and offer deterministic terminal-safe text or JSON where applicable. `config
migrate` is deliberately preview-only in 0.1.0 and requires `--dry-run`; it
never rewrites a configuration. `config fmt` writes only when explicitly
invoked without `--check`.

## Workspace

Quirl is organized as a Cargo workspace of small, focused crates:

| Crate           | Responsibility                                                          |
| ---------------- | ------------------------------------------------------------------------ |
| `quirl-cli`      | Binary, REPL, script runner, and machine-facing commands                |
| `quirl-core`     | Shared process/value DTOs, native `ls`, and serializable errors          |
| `quirl-data`     | Native structured sources, predicates, projection, sorting, and limits   |
| `quirl-lua`      | Restricted Lua 5.4 runtime, Rust schemas, resource budgets, SDK generation |
| `quirl-syntax`   | The explicit command/data-mode interaction grammar                      |
| `quirl-catalog`  | One schema for completion, help, docs, validation, and AI                |
| `quirl-contract` | Versioned agent/package schemas, budgets, hashes, and quality gates      |
| `quirl-lsp`      | Deterministic stdio language service over catalog and Lua metadata       |
| `quirl-picker`   | Typed exact/fuzzy/inverse selection shared across providers              |
| `quirl-process`  | Native command graph, pipes, redirects, process groups, and jobs         |
| `quirl-ui`       | Ratatui rich surface, Reedline fallback, completion, prompt, diagnostics  |
| `quirl-bench`    | Runtime research plus reproducible Preview PTY performance gates         |
| `spikes/`        | Isolated runtime, type-checking, binary-size, and peak-RSS measurements |

## Contributing

Quirl is a fast-moving prototype with a frozen Unix-first release scope. The
architecture decision records in [`docs/decisions`](docs/decisions) are the
best place to see what is settled and which future changes require new evidence.
Issues and discussion are welcome.

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for setup, architecture expectations,
testing, and pull-request guidance. Security-sensitive reports follow the
private process in [`SECURITY.md`](SECURITY.md); user-visible release notes live
in [`CHANGELOG.md`](CHANGELOG.md).

The repository-local Cargo `xtask` is the task runner, so development requires
no tool beyond the pinned Rust toolchain. Run `cargo xtask check` before every
commit; it is the canonical quality gate while the project deliberately
operates without CI.

Generated native-shell cases are deterministic and replayable:

```console
cargo xtask test --seed 123456789 --cases 2048
```

See the [testing strategy](docs/testing-strategy.md) and
[bounded-engineering decision](docs/decisions/0011-deterministic-testing-and-bounded-engineering.md)
for the differential, adversarial, PTY, guest-runtime, and release layers.

## License

Quirl is licensed under the [MIT License](LICENSE).
