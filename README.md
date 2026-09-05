<div align="center">
  <img src="assets/logo.png" alt="Quirl logo" width="128" height="128">

  # Quirl

  **A well-stirred shell.**

  Bash muscle memory, typed data pipelines, and one Lua SDK —
  folded into a single fast Rust binary.

  [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
  [![Rust](https://img.shields.io/badge/rust-1.97.1%2B-orange.svg)](rust-toolchain.toml)
  [![Status](https://img.shields.io/badge/status-0.1_Unix_line-blue.svg)](#status)
</div>

---

<div align="center">
  <a href="https://quirl.vercel.app/">
    <img src="assets/quirl-demo.gif?raw=1" width="1200" alt="Earlier 0.1-era Quirl demo: shell commands, typed data, historical local AI suggestions, Bash compatibility, Lua, and a historical performance card">
  </a>
  <br>
  <a href="https://quirl.vercel.app/quirl-demo.mp4">Watch the MP4</a>
  ·
  <a href="https://quirl.vercel.app/blog/a-shell-with-a-richer-vocabulary">Read the feature tour</a>
  <p>
    Earlier 0.1-era recording; its local AI flow and performance card are historical.
    See the <a href="https://quirl.vercel.app/docs/getting-started/first-session">current first session</a>
    and <a href="https://quirl.vercel.app/docs/project/release-0.1/performance-record">historical artifact evidence</a>.
  </p>
</div>

> [!IMPORTANT]
> Quirl's **0.1 Unix line** supports interactive Linux and macOS. Treat a build
> as the official `0.1.0` release only when it comes from the immutable
> [`v0.1.0` GitHub Release](https://github.com/niklas-heer/quirl/releases/tag/v0.1.0);
> other source checkouts are candidate or development builds. Windows is
> best-effort, contract-tested portability work.

Full documentation—guides, reference material, architecture records, research,
and release evidence—lives on the [Quirl website](website/), generated from
this repository's canonical Markdown sources.

- [Documentation home](website/content/docs/index.mdx)
- [Product and language design](docs/language-design.md)
- [0.1 release checklist](docs/release-checklist.md)

## Why Quirl

In German, a *Quirl* is the humble wooden whisk: a simple tool that takes
ingredients which do not naturally mix and folds them into something smooth.
Quirl does that for the command line.

- **Familiar normal mode** keeps the quoting, redirects, byte pipes, boolean
  lists, and jobs you already use, with explicit Bash and Zsh islands for
  syntax outside Quirl's frozen native core.
- **Typed data pipelines** add an explicit data mode for records, tables,
  filtering, selection, sorting, and structured output—without pretending byte
  streams and values are the same thing.
- **One Lua SDK** powers configuration, scripts, prompt segments, completion
  providers, and trusted plugins inside a restricted, resource-budgeted Lua 5.5.1
  runtime.
- **One semantic catalog** drives completion, contextual help, generated
  documentation, and AI-facing command metadata so those surfaces do not drift.

Rust owns the parser, executor, process lifecycle, data runtime, and every
performance-critical path. Values crossing the Lua boundary are deserialized
into typed structures and validated before the rest of the shell can use them.

## Quick start

Install the official Quirl 0.1.0 release, then start it inside your existing
terminal:

```console
brew install niklas-heer/tap/quirl
quirl
```

You start in **Normal mode**. Try a familiar byte pipeline:

```text
printf '%s\n' hello | tr a-z A-Z
```

It prints `HELLO`. Now type `mode data` and press Enter, then paste this
self-contained pipeline:

```text
[{"service":"api","region":"eu","status":"failed"},{"service":"web","region":"us","status":"ok"}] | where status == "failed" | select service region
```

The result is one row: service `api`, region `eu`. No sample file, account, or
network request is needed. Type `mode normal` to return to familiar commands.
Press **Ctrl-D on an empty input line** to return to your previous shell.

**Tab** opens completion, **F1** explains the command under your cursor, and
**Ctrl-R** searches history. Current source builds also provide `help` as a
starting point and **Alt-Q**, then **g**, to jump among discovered Git projects. The
[five-minute first session](website/content/docs/getting-started/first-session.mdx)
shows expected output, mode shortcuts, and optional AI planning.

In current source builds, AI mode requires an installed, authenticated Codex CLI and sends your intent
and bounded command-catalog context to OpenAI. It keeps proposals for review;
nothing runs automatically. Normal commands and the example above work offline.

See [installation](website/content/docs/getting-started/installation.mdx) for
checksummed native archives, source builds, downloadable completion/model assets,
and uninstall instructions. The supported release is the immutable
[`v0.1.0` GitHub Release](https://github.com/niklas-heer/quirl/releases/tag/v0.1.0).

For development, the repository pins Rust 1.97.1; no system Lua installation is
required:

```console
git clone https://github.com/niklas-heer/quirl.git
cd quirl
cargo run -p quirl-cli
```

After cloning, `cargo xtask demo` runs the deterministic text tour against the
Homebrew-installed release. Release operators should use
[the Rust-native release procedure](docs/releasing.md).

## Status

The current development implementation has native C1-core command
execution on Linux and macOS; a bounded, focused typed-data runtime; a
restricted Lua 5.5.1 runner and SDK; permission-locked trusted-Lua plugin command
dispatch; a semantic catalog and language service; and rich/simple terminal
surfaces with explicit process and recovery boundaries. Repository tests cover
these behaviors; release evidence and support attach only to the exact commit
and artifacts named by an immutable release. The runtime contracts live in
`Catalog::builtin()` and `HOST_API`; the generated references and website are
projections, not competing specifications.

The supported `v0.1.0` artifacts retain their recorded Lua 5.4 runtime; the
Lua 5.5.1 upgrade is currently unreleased source-tree behavior.

The supported release is
[`v0.1.0`](https://github.com/niklas-heer/quirl/releases/tag/v0.1.0), published
from immutable commit `168f9f2e2f2899f7910ca64831561c8885d9ef24`. The
performance block below remains deliberately separate: it records an older
measured artifact and is not retroactively attributed to the published binary.

Config schema v4 includes 30 curated dark themes plus `ansi`, accepts bounded
custom semantic palettes shared by both terminal surfaces, enables completion
after one character by default, uses the compact welcome banner, and adds the
active Rust toolchain to the default right prompt. Legacy unversioned and
explicit v1/v2/v3 configurations migrate deterministically to v4. Tokyo Night
is the default. `quirl config web` exposes the same validated palettes through
a bounded, no-JavaScript preview gallery. See
[ADR 0013](docs/decisions/0013-lua-config-themes.md) and
[ADR 0015](docs/decisions/0015-bounded-theme-preview-gallery.md).

The current source supports `editor.keymap = "emacs"` (the default), `"vim"`,
and `"helix"` in Lua configuration. Both terminal surfaces accept bracketed
paste as editable text: pasted newlines require an explicit Enter to execute.
Commands are limited to 64 KiB. Rich mode rejects an oversized edit with a
status notice and preserves the previous input; simple mode exits with a
resource diagnostic and restores the terminal. Excessive terminal input or an
unfinished escape/paste sequence lasting 30 seconds also ends the session.
See [the interactive surface](docs/tui-design.md) for editing and recovery
behavior, including the separate terminal transport limits.

Important current limits:

- A source checkout is not a supported release merely because its workspace
  version says `0.1.0`; the immutable tag and release assets must identify the
  same candidate, and the publication record must disclose its evidence scope.
- Wasm packages validate but do not execute.
- Package publishing is a local dry run, not a remote registry operation.
- Bash/Zsh here-documents, process substitution, loops, functions, and dialect
  control forms remain explicit reference-shell islands.
- Windows interactive terminal behavior is outside the 0.1 release gate.

<!-- BEGIN QUIRL RELEASE EVIDENCE STATUS -->
> **Release evidence status — historical.** Artifact evidence for measured candidate `23fd5d36907fc816bdafd9aa3c2dcb3afb69feb5` and artifact `9a893a5f1a0b49d62712f331c88966113d910d94efa9651dc4feffe9fd55b637` is historical.
> Evidence commit `14e70939d039d96c195f57452a0e1ec3928194af` documents that measurement. It is evidence only for that named artifact, not for a later candidate.
> This historical record does not assert the release-readiness or human-review state of a later candidate.
<!-- END QUIRL RELEASE EVIDENCE STATUS -->

The operational requirements and commands remain in the human
[release checklist](docs/release-checklist.md).

| Platform | Support level | Promise |
| --- | --- | --- |
| Linux | Supported release target | Interactive shell, PTY handoff, job control, and release smoke tests |
| macOS | Supported release target | Interactive shell, PTY handoff, job control, and release smoke tests |
| Windows | Best effort | Cross-compiled, contract-tested process portability only |

## Documentation

The website mirrors all canonical project documentation into a designed,
searchable Fumadocs hierarchy while keeping repository sources authoritative.
It includes:

- getting started and practical usage;
- the typed data runtime and sandboxed Lua SDK;
- plugins, events, live views, agents, packages, LSP, and MCP;
- protocol, catalog, and generated-artifact reference;
- the complete product specification and all architecture decisions;
- contribution, security, testing, release, adoption, and changelog material;
- source studies, language-selection evidence, and historical benchmarks.

From `website/`, run:

```console
npm ci
npm run dev
```

`npm run sync:docs` refreshes the website mirror after canonical Markdown,
LuaLS stubs, examples, or the protocol-freeze fixture changes. See
[`website/README.md`](website/README.md) for the website maintenance workflow,
including `npm run sync:reference` for the compiled CLI and Lua API pages.
Use `npm run check` for the non-mutating website release gate; it checks mirror
freshness, lint, types, and the production build using `package-lock.json`.

## Contributing

The canonical local quality gate is:

```console
cargo xtask check
```

Replayable stateful compatibility swarms compare Quirl with clean Bash and Zsh
references and retain bounded reports and failure artifacts:

```console
cargo xtask simulate --seed 123456789 --sessions 2048 --steps 12
```

Keyboard-driven Unix PTY sessions exercise editing, pickers, help, typed data,
and resizing, with replay traces and an offline screen gallery:

```console
cargo build -p quirl-cli
cargo xtask session-soak --seed 2026090501 --sessions 100 --journeys 60
```

For a quick check, use `--sessions 4 --journeys 12`. The command above runs
6,000 journeys: 100 modeled active hours at 60 journeys per hour, with think
time removed. It does not establish 100 hours of continuous uptime. Each run
keeps its executable hash, replay trace, measured counters, and an `index.html`
gallery under `target/session-soak`. The styled SVGs model terminal cells and
colors; they do not reproduce terminal fonts, IME behavior, or exact pixels.

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and pull-request guidance,
[AGENTS.md](AGENTS.md) for the engineering contract, and
[docs/testing-strategy.md](docs/testing-strategy.md) for the layered test model.
Security-sensitive reports follow [SECURITY.md](SECURITY.md).

## License

Quirl is licensed under the [MIT License](LICENSE).
