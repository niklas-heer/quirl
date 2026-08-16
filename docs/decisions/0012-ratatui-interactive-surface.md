# ADR 0012: Ratatui is the default capable-terminal surface

- Status: Accepted
- Date: 2026-08-16
- Extends: [ADR 0002](0002-crate-layering.md), [ADR 0008](0008-protocol-freeze-and-migrations.md), [ADR 0010](0010-unix-first-release-scope.md)

## Context

Quirl's Reedline editor established durable history, semantic completion, modal
keymaps, and simple-terminal behavior, but Reedline's prompt and menu painter
cannot provide the persistent context, diagnostics, completion documentation,
and status regions specified by the product design without taking ownership of
the renderer.

Quirl must preserve ordinary terminal scrollback and release the terminal before
foreground execution, suspension, or PTY handoff. It must also retain a stable
line-oriented path for dumb, redirected, very short, or explicitly simple
terminals. Removing Reedline at the same time as introducing the rich renderer
would combine two independently risky changes.

The rich surface also adds public configuration. The prior config contract was
v1, while unversioned configuration is the legacy v0 form. ADR 0008 requires a
version change and a deterministic migration rather than silently changing v1.

## Decision

Quirl uses a Ratatui inline viewport as the default interactive surface on
capable Linux and macOS TTYs. `ui.surface = "auto"` is the default and selects
the rich surface when stderr is a TTY, `TERM` is not `dumb`, and terminal height
is at least five rows. `ui.surface = "simple"`, a non-TTY stderr, `TERM=dumb`,
or a shorter terminal selects the Reedline surface. `ui.surface = "rich"`
requests rich behavior but still obeys the hard terminal-capability checks.
`NO_COLOR` keeps the rich layout and disables color styling.

The rich surface:

- renders an inline frame to stderr, leaving command output in normal
  scrollback;
- owns its editor state, grapheme-aware motion/deletion, bounded undo/redo,
  bracketed paste, prefix history, autosuggestions, and Emacs/Vim/Helix modes;
- renders prompt context, syntax spans, advisory diagnostics, a persistent
  textual status line, catalog/plugin completion documentation, and typed
  history/file/directory/palette overlays;
- uses the existing bounded completion worker and catalog rather than defining
  a second completion protocol;
- drops the viewport, restores cooked mode and cursor state, and disables
  bracketed paste before execution, suspension, mode handoff, or exit; and
- escapes Quirl-owned and extension-owned terminal text before rendering.

The Reedline editor remains the `simple` fallback for this release. This ADR
does **not** accept the former M5 proposal to remove Reedline or claim feature
parity between the two editor cores. Retirement requires separate conformance,
real-terminal, accessibility, and dependency-removal evidence.

Configuration moves to schema v2. It adds `prompt.transient`, `ui.surface`,
`ui.statusline.hints`, `completion.auto`, and `completion.min_chars`. Missing
fields receive v2 defaults. Legacy v0 and explicit v1 documents migrate to v2
before authoritative validation; versions newer than v2 fail closed.

The crate boundary remains unchanged: `quirl-ui` owns both terminal surfaces,
`quirl-lua` owns the config schema and migration, and `quirl-cli` selects and
composes the active surface.

## Consequences

- Capable TTY users receive the rich inline surface without opting in, while
  simple terminals retain a tested fallback and the same execution grammar.
- Foreground applications and child output continue to own the terminal while
  they run; Quirl does not become an alternate-screen shell.
- Config v2 and its v0/v1 migration become reviewed protocol-freeze evidence.
- Ratatui and Reedline coexist temporarily, increasing dependency and
  conformance work but keeping fallback retirement independently reversible.
- Real Linux/macOS terminal testing must cover rich selection, resize,
  release/re-entry, suspension, `NO_COLOR`, and automatic fallback before
  release sign-off.
