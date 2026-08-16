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

> [!IMPORTANT]
> Quirl is an **unreleased 0.1 Unix release candidate** and a fast-moving
> prototype. Linux and macOS are the supported interactive targets. Windows is
> best-effort, contract-tested portability work rather than a supported
> interactive platform.

The complete Fumadocs website—landing page, guides, reference material,
architecture records, research, and release evidence—lives in
[`website/`](website/). It is the new documentation experience and can be
refreshed from the repository's canonical Markdown sources with one command.

- [Explore the website source](website/)
- [Start with the documentation](website/content/docs/index.mdx)
- [Read the product and language design](docs/language-design.md)
- [See the 0.1 release checklist](docs/release-checklist.md)

## Why Quirl

In German, a *Quirl* is the humble wooden whisk: a simple tool that takes
ingredients which do not naturally mix and folds them into something smooth.
Quirl does that for the command line.

- **Familiar command mode** keeps the quoting, redirects, byte pipes, boolean
  lists, and jobs you already use, with explicit Bash and Zsh islands for
  syntax outside Quirl's frozen native core.
- **Typed data pipelines** add an explicit data mode for records, tables,
  filtering, selection, sorting, and structured output—without pretending byte
  streams and values are the same thing.
- **One Lua SDK** powers configuration, scripts, prompt segments, completion
  providers, and trusted plugins inside a restricted, resource-budgeted Lua 5.4
  runtime.
- **One semantic catalog** drives completion, contextual help, generated
  documentation, and AI-facing command metadata so those surfaces do not drift.

Rust owns the parser, executor, process lifecycle, data runtime, and every
performance-critical path. Values crossing the Lua boundary are deserialized
into typed structures and validated before the rest of the shell can use them.

## Quick start

The repository pins Rust 1.88.0 through `rust-toolchain.toml`. No system Lua
installation is required.

```console
git clone https://github.com/niklasheer/quirl.git
cd quirl
cargo run -p quirl-cli
```

Run the deterministic text tour to see typed data, semantic completion,
sandboxed Lua, diagnostics, and generated command documentation:

```console
cargo xtask demo
```

Inside an interactive session, <kbd>Tab</kbd> opens semantic completion,
<kbd>F1</kbd> opens contextual help, <kbd>Alt-M</kbd> changes mode,
<kbd>Ctrl-R</kbd> searches history, and <kbd>Ctrl-T</kbd> opens the file picker.

For requirements and a guided first session, see the website's
[getting-started section](website/content/docs/getting-started/index.mdx).

## Status

Quirl 0.1 runs end to end as a Unix release candidate. The native C1-core
command subset, typed data runtime, Lua authoring stack, permission-locked
plugins, language service, semantic catalog, terminal surfaces, process
lifecycle, recovery, and compatibility contracts are implemented and tested.

Config schema v3 includes 30 curated dark themes plus `ansi`, and accepts
bounded custom semantic palettes shared by both terminal surfaces. Tokyo Night
is the default. `quirl config web` exposes the same validated palettes through a
bounded, no-JavaScript preview gallery. See [ADR 0013](docs/decisions/0013-lua-config-themes.md)
and [ADR 0015](docs/decisions/0015-bounded-theme-preview-gallery.md).

Important current limits:

- No stable version or supported package-manager distribution has shipped.
- Wasm packages validate but do not execute.
- Package publishing is a local dry run, not a remote registry operation.
- Bash/Zsh here-documents, process substitution, loops, functions, and dialect
  control forms remain explicit reference-shell islands.
- Windows interactive terminal behavior is outside the 0.1 release gate.

The remaining candidate work is to refresh the named performance record against
the exact release artifact and complete the human
[release checklist](docs/release-checklist.md).

| Platform | Support level | Promise |
| --- | --- | --- |
| Linux | Supported candidate | Interactive shell, PTY handoff, job control, and release smoke tests |
| macOS | Supported candidate | Interactive shell, PTY handoff, job control, and release smoke tests |
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
npm install
npm run dev
```

`npm run sync:docs` refreshes the website mirror after canonical Markdown,
LuaLS stubs, examples, or the protocol-freeze fixture changes. See
[`website/README.md`](website/README.md) for the website maintenance workflow,
including `npm run sync:reference` for the compiled CLI and Lua API pages.

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
