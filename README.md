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
    <img src="assets/quirl-demo.gif?raw=1" width="1200" alt="A narrated Quirl terminal session: a native pipeline and semantic completion in Normal mode; typed JSON flowing through open, where, sort, and select in Data mode; local AI suggestions; an explicit Bash island; sandboxed Lua; and measured release proof">
  </a>
  <br>
  <a href="https://quirl.vercel.app/quirl-demo.mp4">Watch the MP4</a>
  ·
  <a href="https://quirl.vercel.app/blog/a-shell-with-a-richer-vocabulary">Read the feature tour</a>
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

Install the official Quirl 0.1.0 release from the Homebrew tap:

```console
brew install niklas-heer/tap/quirl
```

The formula installs the `quirl` binary plus its redistribution license files;
its offline test never fetches the command model or completion database. Quirl
acquires those separately in
the background when available and remains usable in degraded mode while
offline. Compatible completion knowledge can advance through Quirl's
version-scoped website channel without forcing an otherwise empty binary
release; every database generation has immutable, digest-named bytes and its
own source identity. Native archives and checksums are also available from the
immutable [`v0.1.0` GitHub Release](https://github.com/niklas-heer/quirl/releases/tag/v0.1.0).
Build from source when contributing or testing current development work. The
repository pins Rust 1.97.1 through `rust-toolchain.toml`; no system Lua
installation is required.

```console
git clone https://github.com/niklas-heer/quirl.git
cd quirl
cargo run -p quirl-cli
```

Run the deterministic text tour against the Homebrew-installed release to see
Normal, Data, and AI modes, semantic completion, sandboxed Lua, and the shared
command catalog:

```console
cargo xtask demo
```

![Quirl's deterministic text tour: typed-data filtering, semantic completion, sandboxed Lua evaluation, and generated documentation](assets/quirl-tour.svg)

Inside an interactive session, <kbd>Tab</kbd> opens semantic completion,
<kbd>Shift-Tab</kbd> expands it into the picker, <kbd>F1</kbd> opens contextual
help, and <kbd>Ctrl-R</kbd> or <kbd>Up</kbd> searches cwd-aware history.
<kbd>Alt-Q</kbd> opens Quirl's leader menu: then press <kbd>n</kbd>,
<kbd>d</kbd>, or <kbd>i</kbd> for Normal, Data, or AI mode, or <kbd>f</kbd> for
the file picker. <kbd>Alt-Q e</kbd> opens the full-screen Environment Explorer:
developer-oriented categories lead into variables, and `PATH` drills into its
ordered directories, executables, winning resolutions, and shadowed commands.
Use <kbd>/</kbd> to filter, <kbd>w</kbd> to resolve a command across all of
`PATH`, <kbd>y</kbd> to copy, or <kbd>i</kbd> to insert a safe reference for
review. A bounded health scan starts immediately; Health reads `scanning…`,
`clean`, or the final finding count instead of treating unchecked state as zero.
Findings explain the concrete PATH entry and jump directly to it. AI mode
searches local command knowledge as you type;
Enter inserts the selected command into Normal mode and never executes it.

For requirements and a guided first session, see the website's
[getting-started section](website/content/docs/getting-started/index.mdx).
Release operators should use [the Rust-native release procedure](docs/releasing.md).

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

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and pull-request guidance,
[AGENTS.md](AGENTS.md) for the engineering contract, and
[docs/testing-strategy.md](docs/testing-strategy.md) for the layered test model.
Security-sensitive reports follow [SECURITY.md](SECURITY.md).

## License

Quirl is licensed under the [MIT License](LICENSE).
