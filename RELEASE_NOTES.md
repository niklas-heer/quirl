# Quirl 0.2.0

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
  a separate bounded startup budget.
  Release jobs now enforce size and latency budgets on every native artifact
  and preserve the measured results even when a budget fails.
- Release performance sessions isolate project discovery and persisted state
  from the developer's home while retaining the default enabled behavior.
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

- The reviewed native-executable hard ceiling is now 12 MiB, preserving all
  current features and Rust panic cleanup. The 8 MiB warning remains; every
  native release job enforces the cap and existing latency budgets against its
  exact artifact. See ADR 0034 for the measurements and decision.
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
