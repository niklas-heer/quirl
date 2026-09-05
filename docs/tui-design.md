# Quirl interactive surface — Ratatui TUI design

Status: **Implemented baseline and forward design specification.** This
document turns the §10/§12 vision in
[language-design.md](language-design.md) into an
implementation contract. [ADR 0012](decisions/0012-ratatui-interactive-surface.md)
accepts Ratatui as the capable-TTY default and retains Reedline as the simple
fallback. [ADR 0022](decisions/0022-persistent-rich-session-transcript.md)
supersedes ADR 0012's per-command screen release with a persistent, bounded
rich-session transcript. Sections marked as future work remain design targets
rather than claims about the current binary.

Audience: implementers (human or LLM sessions) picking up any milestone below.
Read [AGENTS.md](../AGENTS.md) first; every rule there applies. Do not invent
parallel mechanisms — this design reuses `ShellError`, `Catalog`, the frozen
completion protocol, `QuirlConfig`, and the existing prompt scheduler.

---

## 1. Summary

Quirl's default capable-terminal shell is a **Ratatui-rendered full-screen
session**: a dirty-tracked UI on the alternate screen. It owns a bounded
command transcript, scrolling and selection, the prompt, a syntax-highlighted
editor line, a completion popup with a documentation pane, a diagnostics row,
and a persistent bottom status bar. Ordinary foreground commands on the rich
surface return bounded captured outcomes to that transcript without leaving
the alternate screen. The normal screen is restored when the rich session
suspends, exits, or hands the real terminal to a supported full-screen command.

What is **kept** from today's implementation:

- `QuirlPrompt` segment model and `PromptContextScheduler` (async git/cwd,
  stale-while-refresh, per-segment deadlines).
- `Catalog::complete`, `quirl-picker` ranking, and the frozen completion
  protocol v1 (`CompletionWorker`, request/cancel envelopes, ≤250 ms deadline,
  ≤1000 results).
- Lua extension prompt segments, completion providers, and asynchronously
  cached `PanelModel` regions. Panel callbacks run only on the fixed extension
  workers; rendering consumes completed immutable snapshots.
- Durable history (`QUIRL_HISTORY` / `$XDG_STATE_HOME/quirl/history`), bounded
  to 50 000 retained entries, 8 MiB decoded data, and a 32 MiB recent-file
  read/compaction window.
- All accessibility contracts: `NO_COLOR`, `TERM=dumb`, escape filtering,
  symbol profiles, keyboard-only operation.

What is **replaced**:

- On the rich path, Reedline's painter, `IdeMenu`, hinter, and edit-mode
  plumbing are replaced by a Quirl-owned editor core plus Ratatui rendering.
  Reedline stays in the tree as the `simple` fallback; its removal is not part
  of the accepted baseline.
- The heuristic `SemanticHighlighter` is replaced by real lexer-driven spans
  from `quirl-syntax` (new public API, §6).

---

## 2. Goals and non-goals

Goals:

1. IDE-grade editing at the prompt: real syntax highlighting, inline
   diagnostics before Enter, rich completion with docs and provenance.
2. A persistent bottom status bar showing mode, keymap state, key hints, and
   transient notices.
3. The rich surface owns a bounded transcript, viewport, selection, and copy
   model. The simple surface retains native scrollback and inherited execution.
4. Meet the §12 budgets: keystroke-to-frame ≤8 ms P95, first prompt paint
   ≤21 ms P95, cold start ≤25 ms P50.
5. Graceful degradation to a line-oriented experience with the same parser and
   completion data (§12 terminal contract).

Non-goals:

- No embedded interactive terminal in the current stage. The rich surface
  captures ordinary noninteractive foreground commands; faithfully embedding `vim`,
  `less`, `top`, REPLs, or another alternate-screen child inside the transcript
  requires the future PTY/VT contract in ADR 0022. Curated standalone full-screen
  commands instead take over the real terminal (§3.3); arbitrary interactive
  workflows remain on the simple surface.
- No rich-surface background jobs in the current stage. They are rejected
  before spawn because their uncaptured asynchronous output could race and
  corrupt later frames. Use the simple surface for background-job workflows.
- No mouse requirement. Mouse support is an enhancement, never a dependency.
- No plugin-drawn raw widgets in v1. Plugins contribute styled values and
  panel models; Quirl owns layout, focus, theme, and cleanup (§11 of
  language-design.md).
- No Windows interactive support (ADR 0010). Tier 1 is Linux and macOS.

---

## 3. Architecture

### 3.1 Crate placement

Per ADR 0016, all of this lives in `quirl-ui` (which may use catalog, core,
lua, syntax) with composition in `quirl-cli`. No new crate, no inverted edges.

Workspace dependencies to add (root `Cargo.toml`):

```toml
ratatui = { version = "0.30", default-features = false,
            features = ["crossterm_0_29", "scrolling-regions"] }
# crossterm 0.29 is already present; ratatui renders through its crossterm backend.
```

Module layout inside `crates/quirl-ui/src/`:

```
surface/
  mod.rs         # Surface: terminal lifecycle, event loop, dirty tracking
  frame.rs       # FrameModel + render(): layout of all rows, cursor placement
  editor.rs      # EditorState: buffer, cursor, undo, kill ring, keymaps
  highlight.rs   # span cache, catalog-aware command resolution, theme mapping
  completion.rs  # CompletionState: popup model, CompletionWorker wiring
  transcript.rs  # bounded lines, viewport anchors, selection, copy extraction
  statusbar.rs   # StatusBarModel + segments
  overlay.rs     # picker overlays (history/files/palette) on the same frame
  theme.rs       # Theme: named roles -> ratatui Style, NO_COLOR-aware
  degrade.rs     # capability probe: rich | simple decision, width/height tiers
```

Everything stays behind `#[cfg(test)] mod tests` in the same files, per
project convention. `ratatui::backend::TestBackend` is the snapshot-test
backend (§11).

### 3.2 Terminal lifecycle: one persistent full-screen session

The frame uses a `Viewport::Fixed` rectangle covering the complete validated
terminal after entering its alternate screen. Quirl re-measures and manually
resizes that rectangle before each draw, so Ratatui never allocates from an
unvalidated dimension and layout always uses the current terminal rectangle:

```rust
/// Owns the ratatui Terminal and its alternate-screen lifecycle.
struct SurfaceTerminal {
    terminal: Option<ratatui::Terminal<CrosstermBackend<Stderr>>>,
    alternate_screen: bool,
}
impl SurfaceTerminal {
    fn draw(&mut self, frame: &FrameModel) -> Result<(), ShellError>;
    /// Restore the main screen, cooked mode, and input features on handoff or exit.
    fn release_session(&mut self) -> Result<(), ShellError>;
}
```

Accepting an ordinary foreground command does not call `release_session`. The
rich session retains terminal ownership while the CLI runs a bounded streaming
capture request, admits sanitized lines as they arrive, and commits the final
status after both readers drain. A background
pipeline is rejected before spawn. `release_session` remains the single cleanup
path for suspension, EOF, fatal errors, explicit compatibility handoff, and
normal exit.

Rules:

- Draw to **stderr**, not stdout. A rich ordinary foreground child inherits neither
  stream; its bounded captured stdout and stderr cross the terminal-text filter
  before transcript admission. Non-interactive Quirl invocations keep stable,
  undecorated, control-sequence-free stdout (§12).
- The bounded active region (context, editor, diagnostics, completion, and
  panels) reserves its own rows above the status bar. Opening a panel moves
  the editor upward and scrolls older output out of the visible tail instead
  of drawing over history. It is clipped to the current screen, while
  the status bar is always rendered at `height - 1`. Tiny dimensions use saturating layout and the
  capability probe keeps terminals shorter than five rows on the simple path.
- Width is bounded to 512 columns and height to 256 rows (131,072 cells) at
  initial selection and again before every resized draw. An oversized runtime
  resize returns `ResourceLimit` only after restoring the terminal guard.
- Raw mode is enabled while the rich input loop owns input. Stage 1 may pause
  raw mode and input features while a captured command runs, but it does not
  leave the alternate screen. Bracketed paste is enabled through crossterm;
  raw mode, alternate screen, bracketed paste, cursor shape,
  and cursor visibility are restored by the same terminal guard. Kitty keyboard and
  synchronized-output negotiation remain future progressive enhancements;
  `Shift-Tab` works today through crossterm's Shift-Tab/BackTab events.

### 3.3 The ordinary-foreground-command cycle (transcript contract)

```
┌──────────────────────────── rich session ──────────────────────────────────┐
│ 1. enter raw mode + alternate screen once; build the FrameModel            │
│ 2. edit loop: keys → EditorState; async events → completion/segments       │
│ 3. on Accept: freeze the command; pause input, not the alternate screen    │
│ 4. classify foreground rich execution → bounded streaming capture; reject &│
│ 5. sanitize ≤8 KiB chunks; admit complete progress lines and redraw         │
│ 6. drain/reap; append final status, redraw, and return to step 2             │
│ 7. on suspend/EOF/fatal error/exit: restore the main screen and raw state   │
└─────────────────────────────────────────────────────────────────────────────┘
```

Ordinary native stdout and stderr appear while the child runs. Reader threads
retain at most 1 MiB per stream and publish only retained chunks of at most
8 KiB; the executor's owning thread performs every UI callback after process
graph construction. The same capture owner drains and reaps on success,
failure, cancellation, observer failure, and resource overflow. Command input
remains paused, and concurrent stdout/stderr delivery is not a byte-ordering
guarantee.

Stage 1 rejects every command graph containing a background pipeline before
spawn. The existing process engine intentionally leaves background streams
uncaptured; accepting such a graph would let asynchronous bytes escape the
transcript boundary and corrupt a later frame. Background job execution remains
available on the simple surface.

A curated set of full-screen commands, including `vim`, `less`, `man`, and
`top`, uses a separate whole-terminal takeover. Only a single foreground
command without redirects, pipelines, or command-list connectors qualifies.
Quirl releases its terminal guards, executes with inherited streams, then
reacquires and repaints the rich viewport on success or failure. The child's
screen contents are not captured into the transcript. Unlisted interactive
programs and more complex interactive workflows should use the simple surface.
Embedded PTY input, VT parsing, child screen state, and replay remain future
work under ADR 0022; this takeover delegates those responsibilities to the
actual terminal.

The post-0.2.0 source also recognizes `tokscale` directly and through
`bunx tokscale@latest` or `npx tokscale@latest`, including pinned versions and
the `@tokscale/cli` package. This preserves terminal detection, keyboard input,
and alternate-screen rendering instead of triggering Tokscale's static report.

The same handoff supports `tdx`, its positional Markdown file or `--file`
selection, `last`, and numbered `recent` views, including their read-only and
display options. Its scriptable commands (`list`, `add`, `toggle`, `edit`,
`delete`, and unnumbered `recent`) retain captured transcript output. Unknown
options remain captured rather than guessing their argument meanings.
Recognized launcher flags before the package are Bun's `--bun` and npx's
`--yes`/`-y` and `--no-install`; `--` may delimit the package. Unknown launcher
options are not skipped because their values could be mistaken for the package.
Other packages retain captured output, and the same command-graph exclusions
apply. Released 0.2.0 and earlier builds need `ui.surface = "simple"` for these
wrapped TUI sessions until this fix is included in a release.

### 3.4 Transcript, scrolling, selection, and copy

The transcript begins at the top and pushes the context/editor downward like a
normal shell session. Once the viewport fills, the live context/editor remains
immediately above the status bar on the physical bottom row and the transcript
scrolls behind it. A scrolled-away viewport gives the transcript the full body.
Completion, documentation, and picker panels reserve a bounded region below
the editor. The visible transcript shrinks from its older end, keeping the most
recent output readable above the input; no panel paints over those rows. Closing
a panel restores the larger transcript viewport. Scrolling away from the tail
hides input panels and gives history the available body. See
[ADR 0037](decisions/0037-navigation-completion-space.md).

One active transcript record accumulates terminal-safe lines equivalent to:

```rust
struct TranscriptEntry {
    command: String,
    status: i32,
    duration: Duration,
    stdout_lines: Vec<String>,
    stderr_lines: Vec<String>,
    rendered_error: Option<String>,
}
```

The concrete type uses bounded retained lines rather than this illustrative
shape. The command is admitted before spawn-visible progress, complete lines
are appended as chunks arrive, and status/duration commit only after drain and
reap. Control-sequence filtering and UTF-8 repair happen before retained byte
accounting, and visible loss is marked rather than silently discarded.

The transcript retains at most 16 MiB of terminal-safe text and 50,000 logical
lines. Admission computes both costs before changing visible state. When a new
line does not fit, oldest complete logical lines are evicted and one bounded
omission marker reports that fact. Eviction never removes the active editor,
splits a UTF-8 sequence, or duplicates discarded bytes in metadata.

Scrolling is application-owned. The shipped paths provide page up/down,
mouse-wheel steps, a proportional draggable scrollbar, and return-to-tail
navigation:

- `scroll_line`, `scroll_page`, `scroll_to_start`, and `scroll_to_end` operate
  on logical transcript positions, not the host terminal's scrollback;
- follow mode tracks new entries only while the viewport is already at the
  logical end;
- manual keyboard scrolling disables follow mode until the user
  returns to the end; and
- resize recomputes visual wrapping from retained logical rows and preserves
  the nearest retained logical anchor. A frame builds only visible rows and a
  bounded layout margin.

Quirl exposes two bounded selection models. Output-focus mode uses logical
transcript positions for keyboard line selection. Repaint, wrapping, and
scrolling may change those cells but not the selected text while its source
entry remains retained; eviction clamps the selection to retained content.

A mouse drag instead selects exact grapheme-safe cells from the last complete
rich frame. It may cross the transcript, context row (including Git and the
right-side prompt), live input, completion or panel regions, and bottom status
bar. This makes every visible user-facing value selectable even though Quirl
owns the alternate screen. Releasing a completed drag copies immediately and
returns keyboard focus to the prompt while keeping the highlight available.

Copy removes Ratatui styling and terminal control sequences. Logical
transcript copy emits semantic lines without layout padding; full-frame copy
emits the selected visible text, preserves meaningful internal spacing, trims
trailing row padding, and inserts one newline between selected rows. One copy
is capped at 1 MiB and fails before an oversized allocation. Mouse release,
`y`, and delivered Ctrl/Cmd-C events use the same OSC 52 transport; a clipboard
error keeps the selection and never changes command status. Paste remains
governed by the 64 KiB editor bound in §4 and is not a PTY input stream.

### 3.5 Event loop

Single render thread; workers publish through bounded/latest-value state where
possible. No async runtime is introduced: the implementation uses standard
channels/condition variables for `PromptContextScheduler`, `CompletionWorker`,
the extension-completion worker, and the PATH snapshot worker.

The current loop polls crossterm at ≤16 ms and separately polls the completion,
extension-completion, and bounded PATH workers. Input or a published worker
snapshot marks the frame dirty; idle polls do not redraw. It performs at most
one draw per observed event/worker publication. A unified `SurfaceEvent`
channel with ticks, prompt-segment updates, notices, and multi-event batch
draining remains a future refactor. Cursor motion redraws the frame while
reusing the revision-keyed syntax analysis. Every draw records a rolling P95
(§10).

---

## 4. Editor core

`EditorState` is Quirl-owned (no Reedline types). Requirements:

- **Buffer**: a 64 KiB-bounded `String` + grapheme-aware cursor (reuse `unicode-segmentation` /
  `unicode-width`, already workspace deps). Multi-line editing: when
  `quirl-syntax` reports recoverable-incomplete input on Accept (open quote,
  trailing `|`, `&&`), insert a newline and continue instead of executing;
  continuation rows render with a `∙` gutter.
- **Undo/redo**: linear stacks bounded to 256 states and 8 MiB each. Keystroke
  coalescing and an undo tree are later enhancements.
- **Kill ring**: `Ctrl-U`/`Ctrl-W`/`Ctrl-Y` semantics in the Emacs keymap;
  `Alt-Q p` opens the shipped palette, so Emacs kill-to-end and registers remain
  future keymap-parity work.
- **Paste safety**: bracketed paste inserts literally — newlines in pasted text
  never trigger execution; the frame shows `⇪ pasted 3 lines` in the status
  bar until the next keystroke. Oversized paste is rejected as a whole before
  changing the buffer, cursor, or undo history; the status bar reports observed
  bytes, available capacity, and the 64 KiB limit. The simple surface also enables
  bracketed paste, so pasted newlines require explicit submission. Terminal
  transport admission is separately bounded as recorded in ADR 0032: unfinished
  escape sequences admit 4,096 raw bytes; bracketed paste admits 262,156 raw
  bytes including delimiters. Either crossing that limit or leaving a sequence
  unfinished for 30 seconds ends the session with a resource diagnostic and
  restores terminal state. These limits apply before editor admission. Pasted
  control characters remain source text and are escaped for display; they
  cannot issue terminal commands through the editor. In simple mode, exceeding
  the 64 KiB editor bound also ends the session without submitting rejected
  input; it does not provide rich mode's recoverable status notice.
- **Keymaps**: `emacs` (default), `helix`, `vim` — the existing
  `editor.keymap` config values. The baseline centralizes bindings in
  `EditorState::apply_key`, but still uses explicit match branches. Data-driven
  `(mode, key) -> EditAction` tables and user remapping are future work. Helix
  and Vim modal states (`NOR`, `INS`, and Vim `VIS`) appear in the status bar.
  The simple surface uses Reedline's corresponding modes. Its Vi prefixes are
  limited to 64 characters and checked numeric counts; expanded actions are
  limited to 1,024 per processing batch, including dot replay. Rapid input is
  applied in order under the current mode, so a later Escape cannot reinterpret
  earlier insertion. Exceeding admission ends the simple session with a resource
  diagnostic before rejected actions execute (ADR 0033). Reedline removal still
  requires shared keymap conformance and accessibility evidence.
- **History recall**: `Up`/`Down` prefix-aware cycling; `Ctrl-R` opens the
  history picker overlay. Inline autosuggestion (dim text after cursor) comes
  from the most recent matching history entry; `→` at end-of-line accepts it.

The SQLite history database and existing journal/WAL sidecars are secured before
initialization; Unix files use mode `0600`. Link aliases and special files are
rejected. Readers enforce persisted command and directory byte limits before
allocating text, with a 69 KiB SQLite row limit and an 8 MiB snapshot text budget.

```rust
enum EditAction {
    None, Insert(char), Backspace, Delete,
    MoveLeft, MoveRight, MoveHome, MoveEnd, MoveWordLeft, MoveWordRight,
    KillToStart, KillWord, Yank, HistoryPrev, HistoryNext,
    Accept, ForceNewline, Complete, ExpandCompletionPicker, Dismiss,
    ToggleGrammarMode, OpenPicker(PickerKind), Cancel, ClearScreen, Suspend,
    Eof, Undo, Redo,
}
```

### Keybindings (shipped defaults; not yet user-remappable)

| Key | Action | Notes |
| --- | --- | --- |
| `Tab` | Open/advance completion popup | preserves today's behavior |
| `Shift-Tab` | Expand completion into full picker | §10 contract |
| `Enter` | Accept (or newline if input incomplete) | |
| `Alt-Enter` | Force newline | |
| `Alt-Q`, then a mnemonic | Quirl leader for modes, pickers, jobs, and results | collision-free internal command namespace |
| `Ctrl-Space` | Command/data mode toggle compatibility alias | some terminals cannot distinguish this from NUL |
| `Ctrl-R` or unselected `Up` | Cwd-aware fuzzy history | `Up` navigates within an explicitly selected completion menu |
| `Alt-Q f/c/p/j/r/e` | Files / directory explorer / palette / jobs / results / Environment Explorer | Quirl leader namespace; both explorers preserve the edit buffer, while only the directory explorer can commit `cd` |
| `Ctrl-G` / `Alt-D` | Active jobs / cached typed-data picker | snapshots only; selection inserts a revalidated command or data expression |
| `Ctrl-C` | Clear line, dismiss popup; never exits | |
| `Ctrl-D` | EOF on empty line → exit | |
| `Ctrl-Z` | Release terminal, `SIGTSTP` self | resume redraws frame |
| `Ctrl-L` | Clear screen above frame, redraw | |
| `Esc` | Popup dismiss → helix `NOR` | layered: popup first |

### Directory explorer

`Alt-Q c` opens a modal Miller-column explorer. At 96 columns or wider it shows
the parent, current directory, and selected-entry preview; medium terminals keep
the current and preview columns, and narrow terminals prioritize navigation.
The explorer preserves the unfinished command buffer while it is open.

`j`/`k` or arrows select, `h`/`l` or left/right move between directories,
`Enter` changes the shell directory, and `Esc` cancels without changing it.
`/` filters the current snapshot, `.` toggles hidden entries, `s` cycles name,
size, modification-time, and kind sorting, `r` reloads, and `~` opens the home
directory. Source previews use 75 bundled syntax definitions plus Quirl's native
TOML highlighter. Grammar scopes map to the active Quirl theme's semantic roles,
so built-in and custom themes stay coherent across the prompt and preview. GIF,
JPEG, PNG, and WebP render as bounded color half-block thumbnails; terminals
honoring `NO_COLOR` retain image metadata without emitting pixel colors. Other
binary data uses a hex view. Every preview is bounded and never executes file
content. The exact state, resource, and terminal invariants are recorded in
[ADR 0028](decisions/0028-bounded-miller-column-explorer.md).

---

## 5. Visual specification

All mockups assume a 78-column terminal, `unicode` symbol profile, defaults.
Glyphs come from `PromptSymbols` profiles: `auto` promotes to private-use icons
only for terminals with documented built-in Nerd symbols, while `plain`
substitutes ASCII (`D`, `AI`, `*`, `!`) and remains universally safe. The live
input marker is `>` in the plain profile and the solid Unicode `❯` in the
Unicode and Nerd profiles. No path, right-prompt, continuation, or status-bar
separator uses a thin Powerline/Nerd chevron: Unicode and Nerd context segments
use ` · `, continuation lines use `∙`, and status zones use ` │ `.
Accepted transcript command records retain a solid `❯` as their semantic
history marker; it is not profile-dependent chrome.

### 5.1 Frame at rest (command mode)

```
 ~/projects/quirl  main ✚2                                 1 job · 412ms · ✘1
❯ cargo build --release▌
 NORMAL   Alt-Q Quirl · Tab complete · ↑ / Ctrl-R history             quirl
```

Row 1 — **context row**: existing prompt segments. Left list (`directory`,
`git_branch`, `git_state`, plugin segments) left-aligned; right list (`jobs`,
`duration`, `status`) right-aligned, separated by ` · `. Right side truncates
first. The prompt producer already compacts the home directory; segment-aware
`…/` truncation inside an overlong left side remains future work. The current
surface consumes escaped, rendered left/right `QuirlPrompt` strings and gives
the branch suffix a secondary style rather than retaining one styled span per
source segment.

Row 2 — **input row**: the profile-appropriate `>` or `❯` always marks the
editable buffer. Data (`▦`) and AI (`✧`) add their mode indicator after the
chevron; the hardware cursor remains at the edit position
(`Frame::set_cursor_position`). Inline history autosuggestion renders dim after
the cursor. The simple/Reedline surface follows the same glyph policy after its
textual mode label, for example `normal >` or `normal ❯`.

Rows below the editor and any active overlay form the bounded transcript
viewport (§3.4). The physical bottom row is always the **status bar** (§8).

### 5.2 Completion popup open

```
 ~/projects/quirl  main                                            412ms
git che▌
  ┌ completions ────────────────────────┬ git checkout ─────────────────────┐
  │ ▸ checkout     switch branches      │ git checkout <branch>             │
  │   cherry       find unmerged commits│                                   │
  │   cherry-pick  apply commits        │ Switch branches or restore        │
  │                                     │ working tree files.               │
  │                                     │ source: fish-import · trusted     │
  └─────────────────────────────────────┴───────────────────────────────────┘
 command · 3 results (catalog) · streaming…       ↑↓ move · Enter accept
```

- Popup anchors its left edge to the column where the completed token starts
  (`replace_start`), clamped to fit the terminal.
- Left pane: `display` value with `match_indices` highlighted in the accent
  color, then summary text. Kind glyph column (command `λ`, flag `–`, path
  `/`, value `≡`, history `↺`; ASCII fallbacks `c f p v h`). Max 10 rows,
  virtualized scrolling with a 1-cell scrollbar when overflowing.
- Right pane (docs): `detail`, catalog-derived I/O/effect capabilities, and a provenance footer
  (`source · trust`, derived from matching catalog provenance). Hidden for a
  normal popup when terminal width < 72. Narrow mode retains the list and
  count/source status; showing the selected summary in the status bar remains
  future polish.
- Always-on context: typing an exact catalog command opens its explanation;
  typing a flag prefix after a known command opens that command's options. This
  happens even when broad `completion.auto` fuzzy matching is disabled. An
  untouched informational command/flag popup leaves Enter bound to command
  execution; Tab or Down converts it to a selectable menu. Filesystem candidates
  are selectable immediately: Enter inserts the highlighted path, and a directory
  selection opens its children for another Enter. Escape then Enter executes the
  chosen intermediate directory; accepting a file closes the menu, so the next
  Enter executes. An already complete filename leaves Enter bound to execution,
  even when other files share its prefix; Tab still offers those alternatives.
  Up and Down navigate selectable menus; untouched informational
  completion keeps Up bound to history. These navigation changes follow 0.2.0.
  Ctrl-R remains the explicit history entrypoint in either state.
- Streaming: catalog and extension completion run on separate workers. Catalog
  results normally paint first; later extension results merge without moving a
  still-present selected value. `streaming…` shows while either source remains
  outstanding. Every buffer edit cancels the frozen catalog request and
  suppresses stale extension results. The ≤8 ms first-result target still needs
  release evidence rather than being assumed from this architecture.

### 5.3 Diagnostics row

```
gti status▌
  ✘ unknown command `gti` — did you mean `git`?                 quirl.invalid-command
 NOR · command …
```

- Produced by the same continuous parse that drives highlighting plus catalog
  and asynchronous PATH resolution. The analyzer currently emits parse/unknown
  command errors and high/exact-confidence unknown-flag warnings. Rendering
  supports `✘` error (red), `▲` warning (yellow), and the reserved `ℹ` hint
  (blue), with ASCII `E W H`; no hint producer has shipped yet.
- At most one row; highest severity wins; the offending span is underlined
  (`Modifier::UNDERLINED`) in the input row.
- Never blocks Enter. Diagnostics are advisory before execution; `explain`
  remains the deep-preview path.

### 5.4 Data mode

Identical layout; the mode indicator becomes `▦`, the accent color switches to
the data accent (one accent per mode, §7), and the status bar reads
`· data ·`. Highlighting uses the data-expression lexer once it exposes spans;
until then data mode renders with the plain style rather than wrong guesses.

### 5.5 Completed ordinary foreground command

On Accept the alternate screen remains active. After bounded execution
completes, the accepted command and its complete result are admitted as one
transcript entry, and a fresh editor remains at the top:

```
 ~/projects/quirl  main                                            2.31s
▌
  ┌ cargo build --release                                  ✔ 0 · 2.31s
  │ Compiling quirl-core v0.1.0 ...
  └ Finished `release` profile

 NORMAL   Alt-Q Quirl · PgUp scroll · copy selection               quirl
```

The header records the terminal-safe accepted command, exit status, and
duration. Stdout, stderr, and a structured error stay distinguishable even
when the theme presents them with compact shared chrome. A capture-limit error
replaces a success footer and states the configured and observed byte counts;
it never shows a partial capture as successful.

The current rich path admits stdout and stderr incrementally through bounded
8 KiB chunks while the process owner retains lifecycle and capture limits. A
spinner and elapsed-time notice repaint on liveness ticks even when a command
is silent; carriage-return progress replaces one live transcript line. Status
is committed only after both readers drain, and overflow or cleanup failure is
never presented as success. Interactive child input remains a separate terminal
takeover boundary. `prompt.transient` remains for compatibility and still
applies to the simple surface; the rich surface always records accepted
commands in its bounded transcript.

### 5.6 Picker overlays

`Ctrl-R`/`Up` and the `Alt-Q` picker chords reuse the frame: the popup region becomes a
picker (query row + virtualized result list + optional preview pane),
honoring `picker.layout`:

- `adaptive` / `bottom`: inside the shared full-screen frame, max 10 result rows.
- `full`: terminal-height picker using the same full-screen frame and RAII
  lifecycle as ordinary editing.

The file picker replaces the complete shell word under the cursor and preserves
surrounding arguments and operators. It shares the ordinary completion path
encoder: spaces, quotes, dollar signs, and other shell punctuation retain their
literal filename meaning. Names beginning with `-` receive a `./` prefix so
the selected file cannot become a program option. An unfinished quote is closed
on selection, and
filenames that cannot be represented as UTF-8 are omitted instead of inserting
a different path. F1 searches the command segment at the cursor, including a
later pipeline or command-list stage, without changing the edit buffer.

`Alt-Q e` opens a dedicated full-screen Environment Explorer. Its source is the
process executor's private, generation-tracked environment rather than the host
process environment, so session exports and authorized extension updates appear
at the next prompt. Wide terminals show Miller-style category, variable/path,
and detail columns; narrower layouts collapse to the focused list plus details.
Developer-oriented categories keep terminal integrations such as cmux, Ghostty,
and Atuin separate from toolchains, project context, lookup paths, locale, and
sensitive values. `PATH` sorts first in command lookup and drills through:

```text
Command lookup → PATH → ordered directory → executable
```

The executable view identifies the effective winner and every retained shadowed
candidate. Its filesystem scan runs away from the input loop and enforces fixed
bounds on directories, entries per directory, total commands, and retained name
bytes. `/` filters only the focused column. `y` copies, `i` inserts a safe
reference into the preserved command buffer, `v` reveals a selected secret, and
`r` refreshes PATH. `w` enters a global resolved-command column: its rows name
the winning PATH position and shadowed-candidate count, while details show the
complete retained resolution order. Health rows show their concrete path and a
remediation hint; accepting one focuses the exact affected PATH entry. The
bounded PATH worker starts on explorer entry, so Health renders `scanning…`
until one complete snapshot arrives, `clean` only after a complete zero-finding
scan, and otherwise the final finding count. A masked secret must be revealed
before copying. Inspection does not execute a candidate, modify the environment,
or change the grammar mode.

The shipped `Alt-Q p` command palette always requests a terminal-height
region and positions its bounded adaptive content against the bottom edge. It
does not perform another screen transition. The existing terminal guard retains
the shared alternate screen across ordinary foreground captured execution and releases it
for suspension, explicit compatibility handoff, fatal error cleanup, or exit.

`Alt-Q g` opens the Git-project picker from the last complete bounded
`projects.sqlite3` snapshot. Accepting a repository returns its exact path to
the composition root, which revalidates the directory and `.git` marker before
changing directory while preserving unfinished input. The picker never waits
for discovery: one session-owned worker refreshes after first paint, on a
bounded periodic interval, and after coalesced directory, Git-command,
configuration, and stale-picker hints. Automatic home discovery prunes found
repositories, symlinks, other filesystems, hidden/cache/application-data trees,
and configured exclusions. Partial or failed scans retain the previous complete
generation. Before fuzzy matching, the cached snapshot is ordered by the newest
of its last Quirl open and bounded repository-activity timestamp, then by open
count and stable path. An empty query therefore presents recent active work
first; with a query, fuzzy relevance remains primary and cached activity order
breaks equal-score ties. Selecting a repository updates its Quirl open signals.
See [ADR 0030](decisions/0030-bounded-project-discovery.md).

The picker engine, ranking, and typed-value return stay in `quirl-picker`;
the surface uses it through the `PickerRanker` composition adapter. Source
items are capped at 4 096 and 2 MiB retained data, queries at 1 024 bytes, and
ranked visible results at 256; rendering virtualizes the current window.
Interactive ranking has a 50 ms total budget, including request conversion and
history bias. An invalid or expired request clears results instead of publishing
partial or stale rankings. The engine checks cancellation between query terms
and during Unicode preparation, so one large candidate cannot consume the entire
turn unchecked. Query terms are prepared once, and empty queries skip Unicode
search mapping. Word movement, deletion, and picker replacement ranges preserve
complete UTF-8 whitespace characters.
Job entries come from `NativeExecutor::jobs()` after its refresh/prune step and
retain only stable IDs, status, command text, and state-valid `fg`/`bg`
commands. Data entries come only from the bounded cache of successful typed
rows already rendered in the session; opening the picker never reruns a source.

---

## 6. Syntax highlighting

### 6.1 New public API in `quirl-syntax`

The current UI highlighter guesses. Replace it with lexer-truth. Add to
`quirl-syntax` (foundation crate — pure function, serde-only deps, no UI
types):

```rust
pub struct HighlightSpan { pub range: core::ops::Range<usize>, pub kind: HighlightKind }

pub enum HighlightKind {
    Command,        // first word of each pipeline stage (resolution happens in the UI)
    Flag,           // words starting with `-` in option position
    Argument,
    PathLike,       // contains `/`, `~`, or glob metacharacters
    StringSingle, StringDouble, Escaped,
    Operator,       // | && || ; &
    Redirect,       // < > >> <<< and fd forms
    Expansion,      // $VAR ${...} $(...) $((...))
    Number,
    Error,          // unterminated quote, dangling operator
}

/// Total over arbitrary input: incomplete/broken lines still yield spans
/// covering every byte (recoverable parse, §10 interaction contract).
pub fn highlight(line: &str, mode: Mode) -> Vec<HighlightSpan>;
```

- Spans are byte ranges into the original line, non-overlapping, sorted, and
  jointly exhaustive (uncategorized bytes get `Argument`-style default).
- Must be lossless against the existing lexer: implement it on the same
  `TokenKind`/`Word`/`Quoting` machinery inside `lex_command`, not a second
  tokenizer. A property test asserts `highlight` never disagrees with
  `parse_command_list` about quoting boundaries.
- `Mode::Data` may return a single default span until the data grammar exposes
  its own lexer; wire the enum now so the API doesn't change later.

### 6.2 Catalog-aware resolution (in `quirl-ui`)

`surface::highlight` post-processes spans each edit:

- `Command` spans resolve against `Catalog` (+ alias table + `$PATH` lookup
  cache). Lexer command spans use the known-command style; after a complete
  PATH snapshot proves absence, an unknown command becomes a red, underlined
  diagnostic span and offers the closest catalog name using bounded edit
  distance. Reusing the picker scorer for did-you-mean remains future cleanup.
- `Flag` spans check the resolved command's `ArgumentSpec`s: undeclared flags
  render as `flag.unknown` (yellow underline, warning severity) when the
  command's catalog provenance is high-confidence; otherwise stay neutral —
  never punish commands we merely don't know.
- Budget: lex + resolve + style ≤8 ms P95 on a 4 KiB line (§12). Cache the
  span vector keyed on buffer revision. The shipped `$PATH` cache is a complete
  bounded snapshot warmed off-thread, not an LRU: at most 256 directories,
  4 096 entries per directory, 65 536 executable names, and 1 MiB retained
  name bytes. It refreshes when PATH changes at a prompt boundary and stays
  conservative if scanning is truncated or uncertain. The editor itself is
  bounded to 64 KiB. The 8 ms budget is instrumented but not yet enforced as a
  release gate.

---

## 7. Theme

One theme struct centralizes semantic styles; widgets do not choose colors
directly. The table is the intended role vocabulary. The baseline exposes
methods for accents, lexer kinds, severity, context, selection, and chrome;
known/unknown command and unknown-flag distinctions are currently composed by
patching the lexer style with a diagnostic severity style.

| Role | Default | Used by |
| --- | --- | --- |
| `accent.command` | green | popup selection and match highlights |
| `accent.data` | magenta | indicator `▦` and all accent uses in data mode |
| `command.known` / `command.unknown` | green / red | input row |
| `flag` | cyan | input row |
| `string` | yellow | quoted regions |
| `operator` / `redirect` | white bold | `\|`, `&&`, `>` … |
| `expansion` | blue | `$VAR`, `$(...)` |
| `suggestion` | dark gray italic | inline history hint |
| `severity.error/warn/hint` | red / yellow / blue | diagnostics, status bar |
| `chrome.border` / `chrome.dim` | dark gray | popup borders, secondary text |

Rules (unchanged contracts): colors only when stderr is a TTY, `NO_COLOR`
unset, `TERM != dumb`. Under `NO_COLOR` the theme degrades to
bold/underline/dim modifiers only — layout is identical. One accent per mode,
one severity system (§10 visual contract). Theme customization via config is a
later phase; ship the roles first.

---

## 8. Bottom status bar

The status bar is Quirl-owned chrome. Plugin status items are a future protocol
addition; current plugins do not contribute status-bar values and never draw.

Layout: `left │ center(flexible) │ right`, single row, `chrome.dim`
background tint when colors are on. Unicode and Nerd profiles use the ordinary
vertical bar, never a Powerline chevron; the plain profile uses ` | `.

| Zone | Content | Rules |
| --- | --- | --- |
| Left | Keymap state (`NOR`/`INS`/`VIS` for helix, `∅` hidden for emacs) + mode name (`command`/`data`) in the mode accent | always visible, never truncated |
| Center | Contextual: fixed shipped key hints at rest; result count + source + `streaming…` while completing; `⇪ pasted n lines`; editor resource-limit notices; compact-terminal diagnostics | truncates first |
| Right | Contextual completion hints (`↑↓ move · Enter accept`), timing P95 when enabled, else short brand | truncates second |

The shipped hints are fixed strings matching the current bindings because
keymaps are not yet data-driven or user-remappable. Deriving hints from future
live keymap tables remains the release criterion for remapping.
`ui.statusline.hints = false` hides hint text but keeps the bar. Width < 60
columns drops the center zone; the left zone stays.

Implementation status: the baseline status row, mode/editor labels, completion
counts, paste/resource notices, compact diagnostics, width tiers, draw/highlight
P95, and hints toggle are landed. Timed asynchronous job/config/plugin notices
and live-keymap-derived hints remain follow-up work.

---

## 9. Degradation and accessibility

Decision made once at startup (and on `SIGWINCH` only for width tiers), in
`surface::degrade`:

| Condition | Behavior |
| --- | --- |
| stderr not a TTY, `TERM=dumb`, terminal height < 5, or `ui.surface = "simple"` | **Simple surface**: current Reedline path — plain prompt and Reedline menus; completion also remains available through `quirl complete`; identical parser and catalog |
| `NO_COLOR` | Rich layout, modifier-only theme (§7) |
| width < 72 | Normal completion documentation pane hidden; the list and result count remain. Picker previews require width 100, or width 72 in terminal-height full layout, and enabled preview config |
| width < 60 | Status bar center dropped; context right side is dropped when it collides with the left side |

Popup height is clamped to available rows, and terminals below eight rows move
the diagnostic text into the status row. Synchronized-output and kitty-keyboard
negotiation remain planned refinements, not current capability claims.

Hard rules carried over: every piece of information in the shipped frame has a
linear text equivalent (diagnostics render through `render_error` on demand;
pinned panel models require `plain_fallback`); plugin-provided strings pass the existing
control-sequence escape filter before entering any buffer; no functionality is
mouse-only or color-only; screen-reader users get a stable, minimally-redrawn
simple surface rather than a chatty rich one.

---

## 10. Performance and instrumentation

Budgets (restating §12 as per-component obligations):

| Measure | Budget | Owner |
| --- | --- | --- |
| Keystroke → frame flushed | ≤8 ms P95 | event loop; one draw per batch |
| Lex + resolve + style | ≤8 ms P95 | §6 cache |
| First prompt paint | ≤21 ms P95 | context row paints with cached/stale segments; scheduler fills in |
| Cold start → editable | ≤25 ms P50 | rich catalog construction starts on a worker after the first flush; input stays responsive and `$PATH` warmup remains lazy |
| Completion: local results visible | ≤8 ms | catalog worker publishes independently; extensions merge later |
| Memory | 16 MiB/50,000-line transcript; 1 MiB selection/copy; virtualized popup/picker; 64 KiB editor; bounded undo/history | |

The first-paint budget is a P95 wall-clock bound over fresh PTY processes. It
includes alternate-screen entry and process scheduling,
so 21 ms is the smallest stable boundary demonstrated by the rich surface on
the release reference machine. Lazy bounded workers keep the median near one
60 Hz frame without hiding tail behavior or weakening the full welcome default.

Instrumentation is part of the release criterion, not optional: the surface
records draw-time and highlight-time histograms in-process, exposed through
the existing benchmark/evidence flow (`cargo xtask`, release checklist), and a
debug overlay (`QUIRL_UI_TIMINGS=1`) renders the rolling P95 in the status bar
right zone.

Implementation status: rolling draw and highlight-analysis P95 values are
landed and shown together by the debug status. A deterministic 4 KiB analysis
test guards totality/cache reuse with a generous non-flaky ceiling. Enforcing
the 8/16/25 ms targets in named Linux/macOS release evidence remains work; the
instrumentation is not itself proof that every budget passes.

---

## 11. Testing strategy

In-crate `#[cfg(test)]` modules, behavior-sentence names, run by `cargo xtask check`:

- **Rendering snapshots**: `ratatui::backend::TestBackend` + buffer assertions
  for every mockup in §5 (rest, popup, narrow width, `NO_COLOR`, plain
  symbols, data mode, diagnostics row). Snapshots compare styled cells, not
  just text, e.g. `unknown_command_renders_red_with_did_you_mean`.
- **Editor conformance**: one table-driven suite of `(keys, expected buffer,
  expected cursor)` cases executed against all three keymaps, e.g.
  `helix_normal_mode_w_moves_by_word`. Reedline removal is gated on this
  suite.
- **Highlight totality**: property-style corpus over arbitrary valid UTF-8 edit
  strings —
  `highlight()` returns sorted, non-overlapping, exhaustive spans and never
  panics; agreement test against `parse_command_list` quoting.
- **Adversarial**: plugin segment/completion strings containing escape
  sequences, RTL text, zero-width joiners, and 4 KiB tokens must render
  filtered and width-correct (extends the existing escape-filter tests).
- **Protocol**: completion popup honors cancel-on-edit, stale-response
  suppression, and the 250 ms deadline using the existing `CompletionWorker`
  test harness.
- **Degradation**: each row of the §9 table has a test fixing the decision.
- **Transcript bounds**: exact-byte and exact-line admission, oldest-complete
  eviction, omission-marker accounting, UTF-8 boundaries, and atomic failure
  paths at 16 MiB/50,000 lines.
- **Scroll/selection**: follow mode disengages on manual scroll, resize preserves
  a logical anchor, the proportional scrollbar uses the actual retained and
  visible line counts, keyboard and mouse selection survive repaint, exact
  UTF-8 mouse ranges copy through a real PTY, and a 1 MiB copy succeeds while
  the next byte fails before allocation.
- **Lifecycle**: repeated ordinary foreground commands never emit
  alternate-screen exit;
  suspension, EOF, fatal render failure, and normal exit each restore the main
  screen exactly once.
- **Execution separation**: rich ordinary foreground commands select the 1
  MiB-per-stream streaming capture path and commit status only after drain. Rich mode
  rejects a background pipeline before spawn. Simple mode inherits streams and
  retains background-job compatibility. Curated full-screen takeover checks
  require real-terminal handoff and rich-frame restoration on success and spawn
  failure; they do not claim embedded PTY emulation.

Sandbox/budget claims need adversarial proof per AGENTS.md; any new Lua-facing
surface (status items) gets deny-unknown-fields structs at the boundary.

Current evidence includes styled TestBackend checks for rest/data/diagnostic/
completion/picker/compact/adversarial frames; shared keymap and Shift-Tab
conformance; stale/cancelled asynchronous completion tests; 4 KiB highlighting;
and explicit editor, completion, picker, PATH, undo, and history bounds. The
fixed `cargo xtask rich-pty` matrix covers deletion, wrapping, Alt-Q leader
repaint, completion, repeated captured execution, transcript scrolling/copy,
Ctrl-D, takeover restoration, and semantic hints with `NO_COLOR` on real Unix
PTYs. It also checks paste, committed Unicode, Vi repetition, and terminal-input
limits. The seeded session soak adds replayable navigation and colored SVG cell
models. These checks do not establish every degradation permutation, native
font/IME/clipboard behavior, or named-terminal accessibility support; those
remain separate release evidence. See [the testing strategy](testing-strategy.md).

---

## 12. Configuration

Additions to `QuirlConfig` (Lua `config.lua`). The config schema fingerprint
is frozen under ADR 0008. The interactive-surface fields shipped as config
schema v2; theme selection and bounded custom palettes advanced v3, and the
default active Rust toolchain segment advanced v4. Schema v5 adds project
discovery with a deterministic v0/v1/v2/v3/v4-to-v5 migration. The existing
interactive-surface fields remain:

```lua
local config = quirl.config {
  editor = { keymap = "emacs", semantic_hints = true, banner = "compact" },
  picker = { layout = "adaptive", preview = true },        -- existing
  prompt = {
    symbols = "auto",                                      -- existing
    left  = { "directory", "git_branch", "git_state" },
    right = { "rust_version", "jobs", "duration", "status" },
    transient = true,  -- schema-v2 compatibility; simple surface only (§5.5)
  },
  ui = {                                                   -- new
    theme = "tokyo-night",        -- one of 30 built-ins, or a key in ui.themes
    themes = {},                  -- bounded semantic #RRGGBB palettes
    surface = "auto",              -- auto | rich | simple
    statusline = { hints = true },
  },
  completion = {
    auto = true,                   -- automatic semantic completion by default
    min_chars = 1,                 -- threshold when auto is enabled
  },
}
```

ADR 0013 added bounded built-in and custom semantic themes as config schema v3.
Schema v4 changes only polished defaults: a compact banner,
automatic completion after one character, and `rust_version` in the right
prompt. Current schema v5 adds bounded project-discovery settings under
[ADR 0030](decisions/0030-bounded-project-discovery.md). Unversioned v0 and
explicit v1/v2/v3/v4 documents migrate to v5 before validation, retaining the
Tokyo Night theme default.

`ui.surface = "auto"` applies the §9 probe. Everything else in the frame
derives from existing config (keymap, picker layout, prompt segments,
symbols).

---

## 13. Implementation status and remaining delivery

The baseline implementation keeps `cargo xtask check` green and ships with
catalog metadata, advisory diagnostics, keyboard navigation, accessible text
fallbacks, and optional draw/highlight timing (`QUIRL_UI_TIMINGS=1`). The rich
surface is now selected by `ui.surface = "auto"` on capable TTYs. The table
distinguishes landed behavior from remaining parity and release-evidence work.

| Milestone | Current status | Remaining acceptance work |
| --- | --- | --- |
| **M1 — Frame + transcript** | Landed editor baseline: full-screen alternate viewport, bottom status, Quirl-owned 64 KiB grapheme editor, bounded undo/history, Emacs/Helix/Vim states, flowing transcript/context/input rows, prefix history, autosuggestion, bounded streaming captured foreground commands, proportional keyboard/mouse scrolling, exact mouse/keyboard selection, and bounded copy | Keep interactive PTY/VT support outside this milestone |
| **M2 — Highlighting + diagnostics** | Landed baseline: revision-cached `quirl_syntax::highlight`, bounded asynchronous executable-PATH snapshot, parse/unknown-command/unknown-flag diagnostics, severity styling, draw/highlight P95, and Ratatui/adversarial 4 KiB tests | Expand generated totality coverage and record evidence that the 4 KiB/first-paint budgets pass on release terminals |
| **M3 — Completion popup** | Landed: always-on exact-command information and flag-prefix options, bounded catalog and extension workers/results, catalog-first asynchronous merge, selection stability, stale suppression, docs/provenance pane, token anchoring, match styling, virtualization, and narrow list-only rendering | Record named ≤8 ms first-result evidence and broader provider fault/terminal snapshots |
| **M4 — Overlays + keymaps** | Landed: history/files/directories/palette overlays use the shared `quirl-picker` ranker through a composition-root adapter; queries are bounded and editable; Shift-Tab expands completion; adaptive/bottom and terminal-height full layouts honor preview config; Emacs/Helix/Vim editor modes remain available | Decide kitty/synchronized-output negotiation and gather named real-terminal layout evidence |
| **M5 — Fallback retirement** | **Not accepted or implemented.** ADR 0012 flips `auto` to rich but deliberately retains Reedline for `simple` | Separate decision, full conformance and accessibility evidence, minimal fallback replacement, and removal of Reedline from `Cargo.lock` |

Bounded extension panels are now pinned into the full-screen frame below the editor
when no completion/picker overlay is active. `F6` cycles focus, at most six
rows are visible, and `LiveBuffer` retains four completed generations per
panel. Typed command output enters the same bounded transcript rather than
turning the surface into an unbounded watch application.

---

## 14. Open questions and recorded decisions

1. **Persistent full-screen lifecycle**: record one alternate-screen entry,
   repeated captured ordinary foreground commands without screen exit, resize,
   suspend/resume, EOF, and failure cleanup evidence on Ghostty, Terminal.app,
   iTerm2, and a Linux VTE terminal before calling the behavior release-proven.
2. **Embedded interactive PTY/VT applications**: a separate ADR must define
   terminal emulation, input/signal ownership, bounded replay, resize ordering,
   and cleanup before the rich surface advertises `vim`, `less`, `top`, or
   interactive REPL compatibility.
3. **Data-mode lexer spans**: extend `highlight()` when the data grammar
   exposes tokens; until then plain styling (§5.4).
4. **Status items from plugins**: reuse `ContributionKind` or add a
   `StatusItem` kind — needs a protocol-compatibility check under ADR 0008
   before exposing to Lua.
5. **Editing-time notices**: add a bounded event queue and transcript admission
   path that cannot corrupt terminal state or delay cancellation.
6. **Terminal protocols**: decide whether measured Tier 1 benefit justifies
   kitty keyboard and synchronized-output negotiation, with RAII cleanup and
   fallback tests required before enabling either.
7. **Keymap data**: replace explicit binding branches with validated tables,
   then generate status hints from the live mapping before advertising user
   remapping.

---

## 15. Interactive runtime integration failure model

The rich surface may present typed output, native jobs, cached data values, and
extension panels, but it does not become an execution owner. Native jobs remain
owned by `quirl-process`, live data readers remain owned by `quirl-data`, and
Lua callbacks remain owned by the CLI extension scheduler. The UI receives
only immutable snapshots, terminal-safe typed models, and bounded completed
capture outcomes.

The integration maintains these invariants:

- **Cancellation during pull or render.** Interactive data output uses the
  shared execution request and cancellation identity. Plain rows are pulled and
  written one at a time, with cancellation checked before every pull and write.
  A cancellation or write failure after partial output remains a `ShellError`;
  already-admitted transcript rows are not reclassified as a successful value.
- **Ordinary native capture.** The rich surface requests at most 1 MiB for each
  of stdout and stderr through the existing process owner. Spawn failure,
  cancellation, or overflow kills and reaps the complete process graph before
  transcript admission. A resource-limit entry may describe discarded bytes
  but must not present partial capture as successful output.
- **Background rejection.** Before process creation, the rich path rejects a
  parsed command graph if any pipeline is marked background. This prevents an
  uncaptured descendant from surviving the command turn and writing across a
  later renderer frame. The rejection is recorded as a bounded command error;
  it does not create a job entry. The simple surface retains normal background
  execution.
- **Transcript admission.** The UI computes terminal-safe bytes and logical
  lines before mutating visible state. Oldest-complete eviction, the omission
  marker, and new-entry admission form one state transition. Failure preserves
  the prior transcript and reports bounded status text.
- **Resize or suspension during a frame.** Resize invalidates the prepared
  layout before the next draw. Terminals below five rows hide panels, previews,
  and diagnostics in that order and keep the editor/status fallback usable.
  Suspension releases the alternate screen, bracketed paste, cursor shape, and
  raw mode before the process receives `SIGTSTP`; resume reacquires a newly
  measured viewport. A frame prepared for an older size is never deliberately
  written after a resize event has been observed. If stage 1 execution delays
  event polling, output repaint remeasures the viewport and the next input turn
  applies any queued resize before accepting input.
- **Provider failure or removal.** Panel providers execute only on the existing
  extension workers. First paint consumes the last complete cache, a failure
  preserves that last complete per-provider value, and a newer installed
  extension generation removes providers absent from its complete snapshot.
  The render path never locks a Lua VM or calls a plugin.
- **Stale generations.** Runtime and panel generations increase monotonically.
  The UI ignores an update older than the active generation; installing a newer
  complete generation atomically replaces the visible provider set. Exhausting
  a generation counter is `ErrorCode::ResourceLimit`, never wraparound.
- **Terminal write failure or partial frame.** Ratatui/crossterm write errors
  keep the terminal restoration guard armed. Cleanup restores cooked mode,
  cursor visibility, bracketed-paste state, and the alternate screen on explicit
  return and again best-effort from `Drop`. The original write error wins over
  cleanup errors.
- **Post-flush catalog admission.** Extension discovery, active configuration,
  theme, keymap, runtime activation, terminal guards, cursor negotiation, and
  the initial empty-editor frame remain eager. Rich mode then invokes one
  bounded catalog loader on one worker after the first successful flush and
  continues polling input. The surface thread publishes one immutable
  `Arc<Catalog>` generation to analysis and completion before exposing it to
  picker/help and the REPL. Typing, ordinary command execution, builtin help,
  filesystem pickers, and history remain usable while discovery runs. Explicit
  completion resumes against the current input after publication; an open
  palette/help overlay refreshes without overwriting an edited query. Loader
  failure preserves the catalog error while the existing drop guards restore
  cooked mode, cursor visibility and shape, bracketed paste, and the alternate
  screen. Simple/degraded mode keeps eager catalog construction.
- **Queue or output flood.** One UI turn polls at most one provider snapshot,
  applies at most eight panel updates, and performs at most sixteen data pulls
  before checking cancellation and scheduler state again. The panel queue holds
  at most 32 updates; overflow drops the oldest pending update and records one
  bounded notice. Transcript admission independently enforces 16 MiB and
  50,000 logical lines. Repaints are coalesced to at most one per 16 ms poll
  turn.
- **Non-TTY and tiny-terminal fallback.** Non-TTY, `TERM=dumb`, explicitly
  simple, and initially sub-five-row terminals use the bounded Reedline/simple
  path. A rich terminal resized below five rows uses the minimal editor/status
  layout and suppresses optional regions until space returns. Provider failure
  never changes command execution or native history.
- **Shutdown with blocked workers.** The surface owns no extension worker.
  Shutdown cancels the current generation through the existing scheduler and
  waits only for its bounded safe point; an uncooperative callback is detached
  by scheduler-owned `Arc` state and cannot delay terminal restoration.
- **Stale job selection.** Picker entries contain a stable numeric job ID and
  insert an explicit `fg` or `bg` command. Selection never retains a
  process handle. The process owner revalidates the ID and state at execution,
  so a pruned or changed job produces the normal bounded stale-job diagnostic.
- **Oversized typed values.** Data values are validated by the data runtime
  before rendering. Picker retention then caps labels, previews, value depth,
  field count, and encoded bytes independently. A value that is too deep, wide,
  or large may appear in the transcript through bounded incremental rendering but
  is omitted from the picker cache with a resource notice.

Concrete UI limits are eight panels, sixteen columns and 128 rows per panel,
4 KiB per title/heading/cell, 512 KiB retained panel text, 32 queued updates,
eight applied updates per turn, six visible panel rows, four retained live
generations per panel, 128 cached typed data items, 512 KiB cached data text,
256 display columns per data label, and 16 pulls per interactive data turn.
Job snapshots retain at most 256 action items and 512 KiB of terminal-safe
text. All optional regions are virtualized; offscreen rows stay within the
declared snapshot bounds and are never rebuilt from Lua during a frame.
The session transcript separately retains at most 16 MiB and 50,000 logical
lines, and one selection/copy operation retains at most 1 MiB of plain text.

### Interactive help and first success

Textual `help` opens a compact getting-started overview from `Catalog::builtin()`.
It works before background command discovery finishes. Exact command names and
`quirl`-qualified shorthand open the corresponding contract; partial names and
search words list up to 12 choices, and ambiguous aliases never select an
arbitrary command. Help scans up to 4,096 admitted commands, accepts 256-byte
queries, and retains at most 64 KiB of wrapped, terminal-safe text. Large results
show a refinement hint. Help output belongs to the rich transcript, so repainting
and returning to the prompt preserve it; the simple surface prints the same text.

Already-materialized Data-mode values, such as a single record, use the same
bounded table renderer as `quirl data`. List sources, including inline JSON
arrays, become streams and keep incremental plain-row output so inspection
does not force a whole stream into memory. Use `quirl data ... --format table`
for an explicitly collected table. Both paths retain the original typed
values for the results picker; presentation never changes their data contract.
