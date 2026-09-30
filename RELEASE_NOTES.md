# Quirl 0.4.0

### Added

- Tab completes like Zsh. A unique match is inserted at once with a trailing
  space, or a `/` for directories. Ambiguous matches extend the word by their
  shared prefix while the menu lists the alternatives. Accepting a menu entry
  adds the separating space too. Further Tabs cycle through the menu and put
  each candidate on the line, like Zsh's menu completion.
- Explicit Tab asks the installed Zsh's own completion functions for argument
  values. Git branches, Make targets, SSH hosts, process IDs, and every other
  Zsh-supported argument now complete in Quirl, with Zsh's descriptions in the
  menu. Requests run inside the existing bounded completion boundary without
  user startup files, and automatic as-you-type completion never starts a
  shell. Results keep Zsh's display order: groups in creation order, each
  sorted unless Zsh added it unsorted. When Zsh answers, its choice replaces the plain file listing,
  so `git add` offers changed files only. Set `QUIRL_ZSH_COMPLETION=off` to
  disable the live source.
- `quirl complete` includes the same live Zsh argument candidates.
- `$NAME` and `${NAME` complete environment variable names, showing each
  value. As with Zsh's `AUTO_PARAM_SLASH`, a variable that names a directory
  completes with a trailing `/`.

### Changed

- Completion ranks exact-case prefixes above case-insensitive prefixes and
  fuzzy matches, and shows only the best tier, alphabetically with an exact
  match first. Command completion advances one word at a time: `gi` offers
  `git`, not every `git …` subcommand.
- Ctrl-C at the rich prompt leaves the abandoned input in the transcript,
  marked `^C`, and sets `$?` to 130, as in Zsh. It no longer prints a separate
  cancellation block.

### Fixed

- A command could hang forever on macOS when another thread started a
  process at the same moment: a pipe that was briefly inheritable leaked into
  the other child, so the reader never saw end-of-file. Process launches and
  pipe and terminal creation are now serialized process-wide.
- Catalog discovery no longer fails on typical Homebrew installations.
  Fingerprinting and importing completion sources used to charge the same
  bytes twice against one budget. An unreadable or oversized completion
  source is now skipped with a diagnostic instead of disabling all discovered
  completions.
- `quirl index build` follows symlinked completion files, as automatic
  discovery already did, and tolerates man pages whose searchable description
  exceeds 16 KiB.
- The completion cache can live below root-owned system links such as macOS's
  `/tmp` and `/var`. Links owned by other users are still rejected.
- Command completion no longer offers shell expressions such as `$1` or
  `2>/dev/null` that Bash completion scripts register as computed names.
- Valid flags such as `git log --oneline` no longer draw an "unknown flag"
  warning when a command's options come from an imported, possibly partial,
  declaration. Only built-in, Lua-declared, and plugin-declared option lists
  are treated as complete.
- BSD man pages (mdoc) produce readable option summaries: nested `Fl Fl`
  becomes `--name`, and enclosure macros, `Ns`, standards references, escaped
  literals, and delimiters render as `mandoc` shows them.
