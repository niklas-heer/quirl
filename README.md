<div align="center">
  <img src="assets/logo.png" alt="Quirl logo" width="128" height="128">

  # Quirl

  **A well-stirred shell.**

  Your Bash and Zsh habits, Zsh-grade completion, Nushell-style typed data,
  and one sandboxed Lua SDK, folded into a single fast Rust binary.

  [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
  [![Rust](https://img.shields.io/badge/rust-1.99.0%2B-orange.svg)](rust-toolchain.toml)
  [![Release](https://img.shields.io/github/v/release/niklas-heer/quirl?label=release)](https://github.com/niklas-heer/quirl/releases/latest)
  [![CI](https://github.com/niklas-heer/quirl/actions/workflows/ci.yml/badge.svg)](https://github.com/niklas-heer/quirl/actions/workflows/ci.yml)

  [Website](https://quirl.vercel.app/) ·
  [Documentation](https://quirl.vercel.app/docs) ·
  [First session](https://quirl.vercel.app/docs/getting-started/first-session) ·
  [Changelog](CHANGELOG.md)
</div>

---

<div align="center">
  <a href="https://quirl.vercel.app/">
    <img src="assets/quirl-demo.gif?raw=1" width="1200" alt="Quirl v0.5.1 demo: familiar commands with project-aware completion, pasted Bash loops and setup scripts, typed data tables with sort-by, group-by, and math, and sandboxed Lua">
  </a>
  <br>
  <a href="https://quirl.vercel.app/quirl-demo.mp4">Watch the MP4</a>
  ·
  <a href="https://quirl.vercel.app/blog/a-shell-with-a-richer-vocabulary">Read the feature tour</a>
  ·
  <a href="https://quirl.vercel.app/quirl-demo-provenance.json">Recording provenance</a>
</div>

## Why Quirl

In German, a *Quirl* is the humble wooden whisk: a simple tool that folds
ingredients that do not naturally mix into something smooth. Quirl does that
for the command line.

- **Your habits keep working.** Quoting, redirects, pipes, jobs, and variables
  run natively. Pasted loops and `{ ...; }` groups, `source .env`, and
  `eval "$(ssh-agent -s)"` run through `/bin/sh` and keep their variables.
- **Safe as a login shell.** `ssh host cmd`, `scp`, `rsync`, Git over SSH,
  editors, and coding agents get exact POSIX `sh` behavior from `quirl -c`.
- **Completion that behaves like Zsh.** Tab inserts a unique match, extends
  ambiguous ones to their shared prefix, and asks your installed Zsh, or tools
  like `docker`, `kubectl`, and `gh` themselves, for branches, targets, hosts,
  and subcommands, with documentation next to every candidate.
- **Typed data pipelines.** An explicit data mode speaks Nushell's verbs
  (`where`, `sort-by`, `group-by`, `math`, ...) over JSON, YAML, TOML, CSV, and
  files, in colored tables that fit your terminal.
- **One Lua SDK.** Configuration, scripts, prompt segments, completion
  providers, and trusted plugins share one restricted, resource-budgeted
  Lua 5.5.1 runtime.
- **One semantic catalog.** The same command knowledge drives completion,
  contextual help (F1), generated documentation, the language server, and
  AI-facing metadata, so those surfaces never drift apart.

Rust owns the parser, executor, process lifecycle, data runtime, and every
performance-critical path. Everything crossing the Lua boundary is validated
before the rest of the shell can use it.

## See it

<table>
  <tr>
    <td width="50%"><img src="assets/screenshots/completion.png" alt="Tab after git checkout fe lists the project's feature branches with documentation beside them"><br><sub><b>Completion</b> asks Zsh and your tools, and documents every candidate.</sub></td>
    <td width="50%"><img src="assets/screenshots/posix-habits.png" alt="source .env sets variables that a pasted for-loop then uses"><br><sub><b>Pasted Bash</b> runs, and setup scripts keep their variables.</sub></td>
  </tr>
  <tr>
    <td width="50%"><img src="assets/screenshots/data-table.png" alt="open services.json renders a colored table with service, region, latency, and status columns"><br><sub><b>Typed data</b> from a file, in a table that fits.</sub></td>
    <td width="50%"><img src="assets/screenshots/data-pipeline.png" alt="A where, sort-by, and select pipeline, a group-by, and an exact math avg"><br><sub><b>Readable pipelines</b> with Nushell's verbs and exact math.</sub></td>
  </tr>
</table>

## Install

```console
brew install niklas-heer/tap/quirl
quirl --version
```

Homebrew installs a SHA-256-pinned native binary for macOS or Linux on ARM64
or x86_64. You can also download a checksummed archive from the
[latest GitHub Release](https://github.com/niklas-heer/quirl/releases/latest),
install `main` through the [Nix flake](flake.nix), or build from source.
[Installation](https://quirl.vercel.app/docs/getting-started/installation)
covers every option, upgrades, and uninstalling.

Trying Quirl does not change your login shell: run `quirl` inside your
current terminal and press **Ctrl-D** on an empty line to return.

## A one-minute tour

You start in **normal mode**. Familiar commands work as they do elsewhere:

```text
printf '%s\n' hello | tr a-z A-Z
```

Press **Tab** anywhere to complete. `cd cr` becomes `cd crates/`, `ls Car`
becomes `ls Cargo.`, `git checkout f` offers your branches, and `$HO` offers
`$HOME/`. **F1** explains the command under the cursor, and **Ctrl-R** searches
history for the current directory first.

Paste a loop straight from a tutorial; it runs through `/bin/sh`:

```text
for f in *.md; do echo "doc: $f"; done
```

Type `mode data`, press Enter, and try typed data on your files:

```text
ls | where size > 10kB | sort-by size -r | select name size
ls | group-by kind
ls | get size | math sum
```

Tables fit your terminal, and `quirl data 'ls' --format json` shows the typed
values behind them. Type `mode normal` to return, and `clear` or **Ctrl-L** to
clear the screen.

A few more things to try:

| Keys or command | What it does |
| --- | --- |
| `help` | Start here for commands, keys, and modes |
| **Alt-Q**, then **c** | Browse directories in a Miller-column explorer |
| **Alt-Q**, then **g** | Jump between discovered Git projects |
| **Alt-Q**, then **e** | Inspect and search the environment |
| `quirl projects clone URL` | Clone into a GHQ-style `~/Projects/<host>/<owner>/<repo>` layout |
| `quirl config web` | Preview the 30 built-in themes and custom palettes |
| `chsh -s "$(command -v quirl)"` | Make Quirl your [login shell](https://quirl.vercel.app/docs/getting-started/installation#use-quirl-as-your-login-shell) once you are ready |

The [first-session guide](https://quirl.vercel.app/docs/getting-started/first-session)
walks through all of this with expected output. AI mode is optional: it needs
an installed, authenticated Codex CLI, sends your intent and bounded catalog
context to OpenAI, and only ever proposes commands for you to review.

## Documentation

The [Quirl website](https://quirl.vercel.app/docs) presents the full
documentation, generated from this repository's Markdown sources:

- [Completion](docs/completion.md): Tab behavior, sources, ranking, and live
  Zsh arguments.
- [Typed data runtime](docs/data-runtime.md): values, streams, transforms, and
  explicit crossings.
- [Lua SDK](docs/quirl.lua) and [plugin platform](docs/plugin-platform.md).
- [Interactive surface](docs/tui-design.md): editor, pickers, transcript, and
  terminal handling.
- [Product and language design](docs/language-design.md), the canonical
  specification.
- [Architecture decisions](docs/decisions/) and the
  [testing strategy](docs/testing-strategy.md).

## Status and platforms

| Platform | Support level | Promise |
| --- | --- | --- |
| Linux | Supported release target | Interactive shell, PTY handoff, job control, and release smoke tests |
| macOS | Supported release target | Interactive shell, PTY handoff, job control, and release smoke tests |
| Windows | Best effort | Cross-compiled, contract-tested process portability only |

A build is an official release only when it comes from an immutable GitHub
tag and release that name its exact commit; other checkouts are development
builds. Current limits worth knowing:

- Loops, conditionals, `{ ...; }` groups, and brace expansion run through
  `/bin/sh` automatically and keep their exports and `cd`. Shell functions do
  not persist; here-documents and process substitution need an explicit
  `bash { ... }` or `zsh { ... }` block.
- `quirl -c` and piped scripts run in `/bin/sh`, and `eval` and `source` keep
  the variables shell setup code exports, so Quirl is safe as a login shell
  for SSH, `scp`, `rsync`, Git, and editors. Functions and aliases from
  sourced code stay inside it.
- Wasm packages validate but do not execute, and package publishing is a local
  dry run.
- Windows interactive terminal behavior is outside the supported targets.

Every published release links its
[release measurements](docs/benchmarks/) and notes. Historical measurements
apply only to the artifacts they name:

<!-- BEGIN QUIRL RELEASE EVIDENCE STATUS -->
> **Release evidence status — historical.** Artifact evidence for measured candidate `23fd5d36907fc816bdafd9aa3c2dcb3afb69feb5` and artifact `9a893a5f1a0b49d62712f331c88966113d910d94efa9651dc4feffe9fd55b637` is historical.
> Evidence commit `14e70939d039d96c195f57452a0e1ec3928194af` documents that measurement. It is evidence only for that named artifact, not for a later candidate.
> This historical record does not assert the release-readiness or human-review state of a later candidate.
<!-- END QUIRL RELEASE EVIDENCE STATUS -->

## Contributing

The repository pins Rust 1.99.0; no system Lua is required.

```console
git clone https://github.com/niklas-heer/quirl.git
cd quirl
cargo run -p quirl-cli
cargo xtask check   # the canonical quality gate
```

`cargo xtask check` runs formatting, Clippy, Rustdoc, the workspace tests,
seeded Bash/Zsh differential cases, and real-terminal PTY journeys.
Replayable compatibility swarms and keyboard-session soaks go further:

```console
cargo xtask simulate --seed 123456789 --sessions 2048 --steps 12
cargo xtask session-soak --seed 2026090501 --sessions 4 --journeys 12
```

Soak runs keep a replay trace and an offline screen gallery under
`target/session-soak`. The website lives in [`website/`](website/README.md)
(`npm ci && npm run dev`).

Read [CONTRIBUTING.md](CONTRIBUTING.md) for setup and pull requests,
[AGENTS.md](AGENTS.md) for the engineering contract, and
[docs/releasing.md](docs/releasing.md) for the release procedure. Report
security issues as described in [SECURITY.md](SECURITY.md).

## License

Quirl is licensed under the [MIT License](LICENSE).
