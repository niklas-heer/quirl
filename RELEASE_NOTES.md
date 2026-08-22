# Quirl 0.1.0

### Added

- A native C1-core command graph on Linux and macOS with byte pipelines,
  redirects, boolean lists, bounded expansions, background jobs, and job
  control.
- An explicit typed-data mode, shared semantic catalog, completion and picker
  surfaces, durable history, generated documentation, and a stdio language
  server.
- One sandboxed Lua 5.4 SDK for configuration, scripts, prompt segments,
  completions, tests, and trusted in-process plugins.
- Permission-locked plugin packages, typed extension events, live views,
  recovery records, bounded out-of-process adapter initialization, and typed
  trusted-Lua plugin command dispatch.
- Plain, Unicode, and opt-in Nerd Font prompt profiles with `NO_COLOR` and
  `TERM=dumb` fallbacks.
- A Ratatui inline surface selected by default on capable TTYs, with a
  Quirl-owned editor, context and status rows, syntax highlighting, advisory
  diagnostics, documented completion, typed overlays, autosuggestions, and
  transient prompts that preserve normal scrollback and PTY handoff.
- Reproducible performance gates, a text-only product tour, and a real-PTY demo
  recipe for release review.
- A repository-local Cargo `xtask` with typed check, test, SDK, demo, and
  release commands; no separate task-runner installation is required.
- Bounded seeded native C1 generation with reproducible differential comparison
  against available Bash and Zsh reference shells.
- `get` and `where` cell paths accept a non-negative integer segment to index
  into lists (`get items.0.name`) and may end with a trailing `?` to make the
  path optional, returning `Nothing` (for `get`) or a non-match (for `where`)
  instead of failing when a field or index is absent.

### Security

- Lua VMs enforce memory, instruction, deadline, and cancellation policies and
  expose only a restricted standard library.
- Plugin sources are integrity checked and receive only their locked grants;
  isolated adapters have exact launch grants, deadlines, output bounds, and
  process-tree containment.
- Terminal-derived and extension-derived text is sanitized before rendering.

### Changed

- The capable-TTY default is now the rich Ratatui inline frame. Reedline remains
  the explicit and automatic simple-terminal fallback; its removal is not part
  of this change.
- Advanced the release-candidate config contract to v4. It retains v3's
  validated shared semantic themes, changes the polished defaults to a compact
  welcome banner, automatic completion after one character, and an active Rust
  toolchain right-prompt segment, and deterministically migrates unversioned v0
  plus explicit v1/v2/v3 configurations before validation. Future versions
  fail closed.
- Added a release-only website gate with non-mutating generated-mirror
  freshness, semantic release-evidence attribution, lint, route type checking,
  and a production build. It uses the exact `website/package-lock.json`
  dependency graph and is not part of narrow Rust-only checks.
- Runtime assets now use an exact-version website channel with immutable,
  content-addressed payloads and per-asset provenance. Completion knowledge can
  be refreshed independently of binary releases while retaining the compatible
  command model, required license notice, the previous valid local generation,
  and strict binary-version, size, format, and SHA-256 admission checks.
- `Record` is now an insertion-ordered mapping instead of an alphabetized one:
  `select` emits fields in the requested order and table rendering unions
  columns across rows in first-seen order, instead of both always sorting
  columns alphabetically.
- `where` now fails with a diagnostic when a row does not have a field named
  in its predicate, instead of silently treating the row as a non-match; use
  the new `?` optional-path marker to opt back into tolerant matching.
- `quirl config web` no longer requires an explicit `<file>` argument; omitting
  it defaults to the discovered `config.lua` (`QUIRL_CONFIG_DIR`,
  `XDG_CONFIG_HOME`, or `~/.config/quirl`), matching the resolution the shell
  already uses at startup.
- Typing a `quirl` CLI-only subcommand name (`config`, `plugin`, `catalog`,
  `sdk`) or a bare `quirl` directly in the interactive shell now fails with a
  specific hint instead of a generic "not found on PATH" message.
- The rich surface now tails bounded stdout and stderr while a foreground
  command runs. Its input row shows a dimmed animated spinner instead of the
  prompt indicator, silent commands still repaint elapsed time on liveness
  ticks, and carriage-return progress updates replace their live line rather
  than filling the transcript.

### Known limitations

- Interactive 0.1 support targets Linux and macOS. Windows remains a
  best-effort portability target without native terminal validation.
- Here-documents, process substitution, loops, functions, conditionals, and
  dialect control forms require an explicit Bash or Zsh island.
- Wasm components are validated but not executed, and publishing remains a
  local dry-run workflow rather than a remote registry.
- The rich and Reedline editor cores intentionally coexist. Rich-keymap parity
  and a replacement minimal fallback are required before Reedline can be
  removed.
- Native JSON literals and `from json` still normalize field order to
  `serde_json`'s default (alphabetical) parse order; enabling its
  `preserve_order` feature to carry source JSON field order all the way
  through is a separate, wider-blast-radius change affecting every
  `serde_json::Value` consumer in the workspace, not yet made.

<!-- BEGIN QUIRL RELEASE EVIDENCE STATUS -->
> **Release evidence status — historical.** Artifact evidence for measured candidate `23fd5d36907fc816bdafd9aa3c2dcb3afb69feb5` and artifact `9a893a5f1a0b49d62712f331c88966113d910d94efa9651dc4feffe9fd55b637` is historical.
> Evidence commit `14e70939d039d96c195f57452a0e1ec3928194af` documents that measurement. It is not evidence for the corrected implementation, which has no fresh exact-candidate measurement.
> Human review on named Linux and macOS terminals, remote-PTY review, and real-terminal demo review remain incomplete.
<!-- END QUIRL RELEASE EVIDENCE STATUS -->
