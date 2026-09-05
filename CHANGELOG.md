# Changelog

Notable user-visible changes to Quirl are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases use
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Content under `[Unreleased]` remains candidate work. Release preparation moves
it into a versioned entry for review; publication follows only after the exact
candidate and artifacts pass the [release checklist](docs/release-checklist.md).
The [GitHub Releases](https://github.com/niklas-heer/quirl/releases) page records
which versions are published.

## [Unreleased]

### Fixed

- Native Unix commands expand unquoted `~` and `~/` from the session home,
  including `cd` arguments and redirection paths, while preserving quoted and
  escaped literal tildes and enforcing the existing expansion limit.
- Filesystem completion accepts Enter immediately and opens children after a
  directory selection. Escape then Enter executes the selected directory;
  already complete filenames still execute on the first Enter. Home-directory
  candidates follow the session's current `HOME` value.
- Completion and documentation panels reserve space below the editor, scrolling
  older transcript lines upward instead of covering recent command output.
- Rich Normal mode gives Tokscale a real terminal when launched directly or
  through `bunx`/`npx`, including versioned packages. Its interactive UI now
  receives keyboard input instead of falling back to a static table. Other
  package commands retain captured output; redirects, pipelines, and background
  commands keep their existing routing.

## [0.2.0] - 2026-09-05

### Changed

- Quirl now embeds vendored Lua 5.5.1 through `mlua`'s
  `lua55` bindings. The restricted standard library, typed Rust boundary,
  execution budgets, and supervised-worker containment remain unchanged.
  Lua 5.5 language incompatibilities apply to scripts, including read-only
  numeric and generic `for` control variables.

### Added

- Conversational Codex command assistance with typed plans, executable-plan
  validation, cancellation, and explicit review before shell execution.
- A Miller-column directory explorer and an environment explorer in the rich
  terminal interface, plus native Quirl LSP completion and Neovim integration.
- A project picker discovers Git repositories in the background, keeps a bounded
  local cache, and jumps to a selected repository without blocking typing.
- Replayable keyboard-session simulation with seeded workflows, styled terminal
  captures, resize and Unicode input, precise clipboard-protocol checks, and
  bounded sustained-session resource checks on macOS and Linux.
- A recorded VHS terminal demo and an asciinema/`asg` text-tour SVG, embedded
  in the README and website to show semantic completion, native pipelines,
  typed data, and Lua in action. `scripts/record-tour.sh` reproduces the tour
  recording; `scripts/demo.sh` takes an opt-in `QUIRL_DEMO_PACE_SECONDS` pace
  for it without changing `cargo xtask demo`'s default (unpaced) behavior. The
  VHS take emits synchronized GIF, MP4, and WebM assets and gives typed JSON,
  its `open`/`where`/`sort`/`select` pipeline, local AI, explicit compatibility
  boundaries, and measured release evidence their own visual beats.
- A feature-tour blog post explains how Normal, Data, and AI modes, explicit
  Bash compatibility, and the bounded Lua SDK fit together.

### Fixed

- Zsh completion initialization skips insecure function directories instead of
  waiting for an interactive security confirmation inside the capture PTY.
  Safe completions continue to work within the existing request deadline.
- Model loading reads unknown-token metadata directly instead of serializing
  the entire tokenizer, preserving inference across all four supported models
  while avoiding a temporary copy of the vocabulary and processing graph.
- CI verifies coordination retries and asynchronous completion ordering
  independently of scheduler delays, and gives the real Zsh completion fixture
  a separate bounded startup budget. Trusted-plugin contract tests use an
  explicit fixture deadline while retaining production resource limits and
  verifying registration parity and process-grant rejection.
  Release jobs now enforce size and latency budgets on every native artifact
  and preserve the measured results even when a budget fails.
- Release performance sessions isolate project discovery and persisted state
  from the developer's home while retaining the default enabled behavior.
  Each sample now shares one total deadline, terminal I/O is nonblocking, and
  cleanup does not write to a terminal after its child exits. Failed samples
  stop sampling and leave the evidence gate failed.
  Successful samples now exit normally after their timing endpoints, within
  the original deadline, before the next session reuses the fixture state.
  Failed measurements preserve the exact packaged executable for investigation
  while continuing to block aggregation and publication.
- Terminal dimension queries use the native platform API directly. Redirected
  or unavailable terminals no longer trigger an external `tput` fallback during
  prompt rendering or surface selection.
- The rich editor repaints when it regains input control after command execution,
  removing kernel-echoed type-ahead that could leave stale characters in spaces.
  Queued input remains intact and ordinary edits retain incremental rendering.
- Contained subprocess groups terminate when their owner's keepalive pipe
  closes, including abrupt shell termination. Background probes and joined
  pipeline stages no longer depend on the parent reaching Rust cleanup.
- The recording recipe installs verified native command assets and waits for
  interactive discovery before rebuilding the local search index. Its search
  scene requires actual hybrid retrieval and presents suggestions as candidates
  for review.
- Background catalog lock contention preserves queued refresh generations and
  local completion requests. A bounded, cancellable retry replaces the lost
  work and minute-long delay caused by treating contention as completion.
- Rich paste rejects oversized edits atomically. The simple fallback preserves
  pasted newlines until submission, renders pasted terminal controls safely,
  bounds source and undo retention, and restores the terminal on input failure.
- Rapid Vi typing followed by Escape preserves source order. Numeric repeats,
  terminal escape/paste accumulation, input queues and editor batches now have
  explicit resource bounds; incomplete terminal sequences expire when idle.
- Interactive startup remains usable while command intelligence loads. Help,
  completion, resizing, retained output, history recovery, and secondary-text
  contrast now have focused user-session regressions.
  The project worker now admits its database and publishes cached repositories
  before scanning, with an explicit loading state in the picker. Initial Git
  and Rust prompt probes start after the first rich frame is flushed.
- Child-process cancellation, stopped jobs, pipeline cleanup, runtime protocol
  admission, SQLite history limits, atomic file writes, data parsing, extension
  boundaries, and plugin persistence reject invalid or excessive input earlier.
- The PTY harness handles bidirectional backpressure without extending deadlines
  and checks visible labels in reconstructed screens instead of raw byte layout.
- Demo recipes report the actual Lua runtime and distinguish local command
  search from authenticated Codex planning. Earlier embedded recordings and
  their performance cards are explicitly labeled as historical.
- The homepage demo now exposes playback controls when autoplay or reduced
  motion leaves it paused, reserves its aspect ratio, and shares a stable
  content rail with centered hero copy. The decorative headline treatment no
  longer moves text. README playback now has an explicit raw GIF plus MP4 link.
- `scripts/demo.tape`'s startup wait condition, which previously matched only
  an official `v0.1.0` release build and timed out against development
  builds; it now matches either build identity. The closing beat now ends on
  a successful `bash { ... }` dialect island instead of the native-mode
  process-substitution error.

### Upgrade notes

- Release binary size is advisory by default, prioritizing shell usability
  and retaining all features. Exact byte counts, independent digests, and the
  warning above 8 MiB remain; growth without a clear user benefit warrants
  review. An explicit `--max-binary-bytes` still enforces a caller maximum.
  Every latency, identity, and cleanup gate remains unchanged. See ADR 0036.
- Linux and macOS remain the supported interactive platforms. Windows and Wasm
  execution are not part of this release scope.
- Lua extensions should be reviewed for Lua 5.5 language changes. The sandbox
  still exposes only the restricted SDK and standard libraries.
- Configuration schema v5 adds project-discovery settings. Older configurations
  retain their migration path; `quirl config migrate path/to/config.lua --dry-run`
  previews the required source changes without rewriting the file.
- Resource-limit failures never authorize execution of rejected input. Rich
  oversized paste preserves the edit; simple-editor input overflow exits the
  session after restoring terminal modes.
- Accelerated session tests and styled terminal captures are supporting evidence,
  not a claim of continuous 100-hour uptime or native IME/clipboard compatibility.

## [0.1.0] - 2026-08-22

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
> Evidence commit `14e70939d039d96c195f57452a0e1ec3928194af` documents that measurement. It is evidence only for that named artifact, not for a later candidate.
> This historical record does not assert the release-readiness or human-review state of a later candidate.
<!-- END QUIRL RELEASE EVIDENCE STATUS -->
