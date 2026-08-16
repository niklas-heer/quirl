# ADR 0013: Built-in themes share semantic roles across configuration surfaces

- Status: Accepted
- Date: 2026-08-16
- Extends: [ADR 0008](0008-protocol-freeze-and-migrations.md), [ADR 0012](0012-ratatui-interactive-surface.md)

## Context

Quirl's local configuration server was secure and file-backed, but it exposed a
plain form and could not preview or select a terminal color theme. The rich
surface, simple Reedline prompt, and semantic highlighter also carried separate
hard-coded colors. A web-only gallery would therefore create a second visual
source of truth and preview a result the shell could not apply.

## Decision

Config schema v3 adds the closed `ui.theme` field. Legacy v0, v1, and v2
documents migrate deterministically to v3 with `quirl` as the default. Quirl
ships eight immutable palettes: Quirl, Catppuccin Mocha, Dracula, Gruvbox Dark,
Nord, Solarized Dark, Tokyo Night, and One Dark.

The palette catalog lives in `quirl-ui` and maps colors onto semantic roles:
command and data accents, primary and secondary context, foreground, dim text,
strings, errors, warnings, hints, and the status background. Both Ratatui and
the Reedline prompt/highlighter consume those roles. `NO_COLOR`, `TERM=dumb`,
plain symbols, textual status, and non-interactive output remain authoritative;
a theme never becomes the only carrier of state.

`quirl config web` renders every preview card from that same static catalog.
The gallery executes no JavaScript, Lua, shell command, plugin callback, or
remote asset. Selecting a theme uses the existing token, request bounds,
three-way merge, full-config validation, atomic replacement, and backup path.
When the source predates v3, this explicit selection also updates the literal
schema version and inserts the new literal into a literal `ui` table.

## Consequences

- Adding or renaming a theme is a reviewed config-protocol change, not an
  unversioned CSS tweak.
- Arbitrary user palettes, remote CSS, and uploaded theme files are outside the
  v3 contract. They would require separate validation and accessibility rules.
- The web preview uses deterministic sample context. It does not claim to run
  plugins or inspect the live repository.
- The protocol inventory, generated Lua SDK, examples, and migration evidence
  move together with schema v3.
