# Quirl 0.5.0

### Added

- Quirl works as a login shell for SSH, file transfer, Git, editors, and
  agents. `quirl -c 'code' [name [arg...]]` runs the code exactly as
  `/bin/sh -c` would, so `ssh host cmd`, `scp`, `rsync`, and Git over SSH work
  against a host whose shell is Quirl. `-l`, `--login`, `-i`, `-s`, and
  clusters such as `-ilc` are accepted, as editors use to read your
  environment.
- An interactive login session (`-quirl` as argv0, or `quirl -l`) adopts the
  environment `/bin/sh -l` sets up from `/etc/profile` and `~/.profile`, such
  as macOS `path_helper` entries.
- `eval` and `source` (or `.`) run shell setup code in `/bin/sh` and keep the
  exported variables and working directory it leaves behind. `eval
  "$(ssh-agent -s)"`, `eval "$(brew shellenv)"`, `source .venv/bin/activate`,
  and `source .env` now work. Functions and aliases the code defines stay
  inside it.
- Pasted loops, conditionals, `{ ...; }` groups, `[[ ... ]]`, and brace
  expansion such as `{a,b}` run in Normal mode without a `bash { ... }`
  wrapper. Quirl runs the line in `/bin/sh` and keeps the exports and working
  directory it leaves. Function definitions explain that functions do not
  persist.
- `$PPID` names the shell's parent process.
- `help ` completes command names instead of file names.
- `clear` empties the interactive shell's output, and Ctrl-L clears the screen
  as in Zsh while earlier output stays reachable with PageUp. Both used to
  leave the transcript in place.
- Data tables in the interactive shell color their values: numbers and sizes
  in the number color, times in the secondary color, record keys in the
  accent, and row numbers, empty values, and nested summaries dimmed.
- `NAME=value` sets a shell variable, `NAME=value command` sets a variable for
  one command, `export NAME` exports an existing variable, and `unset NAME`
  removes one.
- Data mode speaks more of Nushell's vocabulary: `sort-by <field> [-r]`,
  `first <n>`, `last [n]`, `skip <n>`, `reverse`, `reject <field>...`, `uniq`,
  `group-by <field>`, `columns`, and `math sum|min|max|avg`. `math` adds
  exactly, without floating-point rounding, and sizes keep their unit.
- `where` accepts size literals: `ls | where size > 10kB`. Decimal (`kB`, `MB`)
  and binary (`KiB`, `MiB`) units are understood.
- Tab completes `docker`, `kubectl`, `gh`, `helm`, `podman`, and other
  Cobra-based tools by asking the program itself (`PROGRAM __complete`) when
  Zsh has no function for it. Only a fixed list of known Cobra programs is
  asked, within 1.5 seconds; set `QUIRL_TOOL_COMPLETION=off` to disable it.
- Live Zsh completion finds more completion functions: rustup's `_cargo`
  (`cargo b` offers `build` and `bench`), Debian's `vendor-completions`, Nix
  profiles, and the per-user `~/.zfunc`, `~/.zsh/completions`, and
  `~/.local/share/zsh/site-functions` directories.

### Changed

- Quirl builds with Rust 1.99.0. Captured command output is decoded without
  an extra copy of the bytes.
- `${NAME:=word}` and `${NAME=word}` set an unexported shell variable, as in
  `sh`, instead of exporting it.
- Subcommands such as `quirl catalog | head` end quietly with status 141 when
  the reader closes the pipe, instead of panicking.
- Git branches and other Zsh answers that contain `/` no longer show a
  folder icon unless they name an existing path.
- Comparing values of different kinds explains itself, such as "cannot
  compare a size with a string", and suggests writing `10kB` without quotes.
- A lone size or time, such as the result of `math sum` over file sizes,
  prints like its table cell (`213.1 kB`) instead of raw bytes.
- A script piped into a bare `quirl` now runs as POSIX shell code through
  `/bin/sh -s`, as with any shell. It used to run as Lua; use
  `quirl run --lang lua -` for that.
- Data mode in the interactive shell renders records as tables, as
  `quirl data` already did, instead of one JSON line per row. Record streams
  render in batches of up to 256 rows, so the first rows appear early.
- Data tables fit the terminal. Wide columns shrink and end in `…`, and
  columns that still do not fit collapse into one `…` column. Redirected
  `quirl data` output keeps every cell intact.
- A single record renders as a vertical key/value table, and nested values in
  cells read `{record 2 fields}`, `[table 3 rows]`, or `[list 4 items]`
  instead of raw JSON.
- `ls` tables show the `kind` column with its real values, such as
  `directory`, so `where kind == "directory"` filters what the table shows.
  The column was labeled `type` and abbreviated values to `dir` and `link`.
- `quirl complete` now runs the interactive completion engine, so it lists
  what Tab offers, including directories for `cd` and `$VARIABLE` names.

### Fixed

- `open` keeps the field order of JSON and TOML documents, as YAML already
  did, instead of sorting fields alphabetically, so table columns appear in
  the order the file lists them.
- `mode d` and other commands whose argument has a fixed set of values no
  longer offer file names next to those values. Their menu descriptions read
  "One of `normal`, `data`, `ai`, or `toggle`" instead of internal wording.
