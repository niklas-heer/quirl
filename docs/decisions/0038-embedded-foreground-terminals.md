# ADR 0038: Embedded foreground terminals

Status: Accepted (Unix foreground scope).

## Decision

Rich native execution offers every expanded external foreground pipeline a PTY,
independent of executable name, wrapper, or package manager. Parent execution
still owns expansion, conditional lists, `cd`, `export`, and extension hooks.
Explicit pipeline edges and redirections retain their byte-stream semantics.
Simple mode continues to inherit the physical terminal. Explicit dialect islands
and structured/plugin execution retain their existing execution contracts.

`quirl-process` owns the PTY and its session leader. A private Quirl worker runs
one prepared graph using the existing native process-group/anchor ownership.
Arguments cross a separate local control channel as literal data and are never
expanded a second time. `quirl-ui` owns a bounded VT screen. The CLI composes
process events, physical input, local terminal replies, and drawing.

## Failure model and invariants

The worker may fail before connecting, during graph construction, after partial
output, or before its final response. Output, input, replies, and control messages
may be fragmented, oversized, malformed, stalled, or closed early. Terminal size
may change during any state. Rendering, socket I/O, cancellation, and cleanup may
fail independently. None may strand the real terminal or release a live child
without an owner.

- Create private control storage with mode 0700 and a socket with mode 0600 before
  spawning. Use random names and bounded startup/handshake deadlines. Descriptors
  are close-on-exec; child terminal bytes cannot become control messages.
- Validate prepared graphs on both sides. Control frames have fixed-width lengths,
  a version, strict schemas, and byte ceilings checked before allocation.
- Only the parent mutates shell state. A worker receives expanded external stages;
  it cannot accidentally lose a successful `cd` or `export` on a later failure.
- Native group anchors remain alive until child cleanup. PTY session identity is
  retained until group signaling completes. Close the master before bounded reap;
  a bounded deferred reaper owns kernel-delayed children. Unix `setsid` escapes
  are outside the existing containment guarantee.
- Poll cancellation/deadline every bounded event-loop turn. Handle partial writes
  with a fixed input queue; never block UI on a full child input buffer.
- Forward ordinary keys, paste, mouse events, and resize to the child. Quirl editor
  leaders do not intercept child keys. Stopped jobs remain owned until explicitly
  resumed or cancelled; they must not disappear at worker destruction.
- After the native graph finishes, the worker requests input handoff. The parent
  stops reads/writes before acknowledging. The worker drains at most 64 KiB of
  unused slave input without flushing canonical partial lines, restores termios,
  and reports it. Combined with pending parent input, at most 64 KiB becomes an
  editable prefill requiring Enter. Pending local replies are kept separate from
  user input. A bounded recovery decoder removes VT controls and invalid bytes
  with an explicit notice; process-origin text never submits a command.
- Child controls affect only the emulator. Never forward OSC/DCS, clipboard,
  window operations, or terminal queries to the physical terminal. Answer supported
  queries from local state. Unknown sequences are safely discarded.
- On completion retain the bounded primary terminal snapshot in session history,
  restore Quirl's editor, and report the actual exit status. Error paths restore
  the same terminal modes and retain the original error through cleanup.

## Resource sketch

One active foreground worker per rich session. Screen at most 512 columns by 256
rows, plus 256 scrollback rows; fixed-width bounded cells. Process output reads
are at most eight 8 KiB reads per event-loop turn. Input queue at most 64 KiB plus 12 paste-framing bytes, replies at most 8 KiB per
parser call. Output-active turns poll input without waiting; idle input polls
wait at most 20 ms, and continuous output draws at most once per 16 ms. ANSI parameter/string scanning and UTF-8 fragments are explicitly
bounded. Graph arguments/paths at most 1 MiB and 65,536 items, 64 stages; private
environment at most 16 MiB and 65,536 variables. Control frames at most 8 MiB,
responses at most 384 KiB, with strict diagnostic fields (512 bytes each, four
context/help items). Request framing is bounded before allocation; even malformed
JSON is limited to an 8 MiB frame and Serde's 128-level nesting guard. Startup and
control transfer and input handoff each use absolute 2-second budgets. Post-exit
output drain has a 2-second deadline and reports failure if output or the control
result does not close, rather than silently dropping a buffered tail. There
is no cumulative output limit for an interactive terminal: old rows are evicted.
Trusted foreground execution has no overall wall deadline; sandboxed/request-backed
execution retains its supplied deadline and cancellation flag. Stopped workers
wait in 100 ms input turns with EOF cleanup. Final-response-to-worker-exit cleanup
has a 2-second watchdog. The session transcript retains its existing
16 MiB/50,000-line bounds; evicted child history is explicitly marked.

VT admission caps control retention at 256 bytes, discarded string scanning at
64 KiB, and each parser call at 1,048,576 cell/edit work units. A narrow vendored
vt100 patch fixes wide-cell truncation, saved cursors after resize, and deep
scrollback iteration; the consumer tests reproduce those upstream faults.

Existing locked `portable-pty` supplies audited platform terminal creation without
introducing unsafe product Rust; `filedescriptor`/`rustix` supply safe descriptor
and unreaped-status operations. Existing locked `vt100` supplies screen semantics;
an admission parser guards its variable control strings and operation counts.
These dependencies have process/UI owners and replace fragile application lists.

## Evidence required

Unit tests cover parser fragmentation, invalid/oversized controls, resizing,
wide/combining cells, query responses, keyboard modes, process startup/partial I/O,
and cleanup. PTY journeys use an unrecognized program plus wrappers: all three
stdio descriptors are terminals, alternate-screen redraw replaces cells, keys and
queries work, resize arrives, Ctrl-C/nonzero exit restore the prompt, ordinary
output survives, and explicit pipes/redirections stay nonterminal. Existing
navigation, persistence, process, and rich-session gates remain required.
