# Quirl 0.3.0

### Added

- Managed Git project cloning through `quirl projects clone`, using GHQ-compatible
  `<root>/<host>/<repository-path>` directories and existing `GHQ_ROOT` or
  `ghq.root` settings. `quirl projects root` shows the effective parent directory.
  Existing matching checkouts are reused without pulling or overwriting them.
- Rich Normal mode offers an optional managed location for a straightforward
  `git clone URL`. The original command remains the default; users can choose a
  managed location once, opt in for future eligible clones, or dismiss further
  suggestions. Explicit destinations and scripts keep their Git behavior.
  Completed projects are immediately available in the project picker, with an
  explicit **Alt-Q u** action to open them. `quirl projects policy` inspects or
  sets `ask`, `managed`, or `off` without editing Lua configuration.

### Fixed

- Interactive native commands retain the previous command's exit status in `$?`
  across prompt turns, including status 130 after cancellation. Internal project
  metadata probes do not replace the user-visible status.
- Rich Normal mode on Unix runs foreground programs in an embedded terminal,
  including unknown tools, wrappers, `tdx`, and `bunx tokscale@latest`. Screen
  redraws, keyboard input, local terminal queries, resizing, and Ctrl-C work
  without an application allowlist. Explicit pipes and redirections retain their
  semantics; completed primary-screen output remains in the transcript and
  unread type-ahead returns as editable prompt text.
- Native Unix commands expand unquoted `~` and `~/` from the session home,
  including `cd` arguments and redirection paths, while preserving quoted and
  escaped literal tildes and enforcing the existing expansion limit.
- Filesystem completion accepts Enter immediately and opens children after a
  directory selection. Escape then Enter executes the selected directory;
  already complete filenames still execute on the first Enter. Home-directory
  candidates follow the session's current `HOME` value.
  Background catalog loading preserves an Escape-dismissed popup until the
  next edit or explicit completion request.
- Completion and documentation panels reserve space below the editor, scrolling
  older transcript lines upward instead of covering recent command output.
