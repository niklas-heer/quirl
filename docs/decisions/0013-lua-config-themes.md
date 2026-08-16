# ADR 0013: Themes are bounded semantic palettes in Lua configuration

- Status: Accepted
- Date: 2026-08-16
- Extends: [ADR 0001](0001-lua-extension-language.md), [ADR 0008](0008-protocol-freeze-and-migrations.md), [ADR 0012](0012-ratatui-interactive-surface.md)

## Context

Quirl's rich and simple terminal surfaces used separate hard-coded color
choices. That made a theme change inconsistent across the prompt, syntax
highlighting, diagnostics, completion chrome, and status line. It also left no
typed way for users to add a palette through the existing Lua configuration
system.

Theme input is untrusted configuration. Names, retained strings, and the number
of palettes must be bounded. Theme evaluation must never run a Lua callback on
the per-keystroke or paint path, and `NO_COLOR` must remain authoritative.

## Decision

Configuration schema v3 adds `ui.theme` and `ui.themes`. `ui.theme` selects a
built-in or configured palette and defaults to `tokyo-night`. Quirl ships 30
popular dark palettes curated from the maintained
[Gogh terminal-theme catalog](https://github.com/Gogh-Co/Gogh), plus `ansi` as
a conservative compatibility alternative. `ui.themes` is a Lua table of
immutable semantic palettes; it is evaluated once with the rest of `config.lua`,
deserialized into deny-unknown Rust structures, validated, and then passed to
both terminal surfaces as ordinary data.

Each palette supplies a fixed set of semantic roles. Quirl owns style
modifiers, selection behavior, layout, and terminal cleanup. Palette names are
safe ASCII identifiers of at most 64 bytes, at most 32 custom palettes are
retained, and every color is exactly `#RRGGBB`. Custom palettes may not shadow
built-ins. Unknown selections, malformed names, missing or unknown roles, and
invalid colors reject the complete configuration. Count and retained-string
overflows return `ErrorCode::ResourceLimit` with the observed and configured
limits.

The resolved palette is copied into the editor at a safe prompt boundary. No
Lua code runs while rendering. When styling is disabled by `NO_COLOR`, a dumb
terminal, or a non-TTY, foreground and background colors are both suppressed
while textual labels and safe modifiers remain.

Legacy v0, v1, and v2 configurations migrate deterministically to v3 defaults.
Future versions fail closed. The crate boundary remains unchanged:
`quirl-lua` owns the schema, validation, built-ins, and migration;
`quirl-ui` maps semantic roles to backend styles; `quirl-cli` composes the
active config.

## Consequences

- Tokyo Night is coherent across the Ratatui and Reedline surfaces by default.
- Thirty built-in palettes cover widely used editor and terminal theme
  families without adding render-time lookup or third-party dependencies.
- Users can add computed themes using the same bounded Lua configuration that
  already owns prompt and UI settings.
- Themes cannot execute paint-time code, emit terminal controls, grow without
  an explicit bound, or replace Quirl-owned layout and modifiers.
- The config protocol advances to v3 and its generated SDK, migration evidence,
  examples, configuration views, and freeze manifest must change together.
