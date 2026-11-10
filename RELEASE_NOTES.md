# Quirl 0.6.0

Quirl now runs the shell code you already carry around. Loops,
conditionals, `case`, functions, and aliases run in Quirl's own executor
instead of being handed to `/bin/sh`, so a pasted loop keeps the variables it
sets, functions and aliases last for the whole session, and
`~/.config/quirl/init.qrl` can define them at startup in plain shell syntax.

### Highlights

- **Functions and aliases work.** In 0.5.1, defining a function was an error.
  Now `greet() { printf 'hi %s\n' "$1"; }` and `alias ll='ls -l'` behave as in
  `sh`, with `local`, `return`, `shift`, and `$@`.
- **Loops over builtins run in the shell itself.** `echo`, `test`, `[`, and
  arithmetic no longer start a process. A 10,000-iteration `for` loop that
  echoes each number takes 9 ms, against 51 ms in 0.5.1 and 26 ms in Bash.
- **Programs start about twice as fast.** Running `/usr/bin/true` drops from
  12.5 ms to 6.5 ms, and a two-stage pipe from 12.7 ms to 6.6 ms.
- **Errors from `$(...)` are no longer swallowed**, and `$(...)` can return up
  to 1 MiB, so `for f in $(git ls-files)` works in large repositories.

These timings come from the repository's
[shell workload benchmark](https://github.com/niklas-heer/quirl/blob/main/docs/benchmarks/shell-workloads.md):
one program per run through `quirl exec`, median of 15 to 31 runs, startup
included, on an Apple M4, comparing the published 0.5.1 binary with a
release-profile build of this version. Bash is the system Bash 3.2.

**Check before upgrading:** unquoted expansions now split on `IFS`, as in
`sh` and Bash, so `ls $FLAGS` passes each word separately. Quote the expansion
to keep it whole. A loop that starts many external programs is slower than it
was in 0.5.1, which handed the whole loop to `/bin/sh`: 200 trivial commands
take about 0.48 s, against 0.30 s. Here-documents, `[[`, subshells, and other
forms listed below still run in `/bin/sh` and cannot see your functions or
aliases.

### Added

- Loops, conditionals, and `case` run natively in Normal mode: `if`, `for`,
  `while`, `until`, `case`, `{ ...; }` groups, `!`, and `#` comments no longer
  fork `/bin/sh`. Loops over builtins are fast, and variables they set persist
  in the session.
- Shell functions, `name() { ...; }` and `function name { ...; }`, persist for
  the session. They take `$1` through `$9`, `${N}`, `$#`, `$@`, `$*`, and
  support `local`, `return`, `shift`, and `unset -f`.
- `alias` and `unalias` expand at command position as POSIX specifies. A
  value ending in a blank also expands the next word.
- `echo`, `test`, `[`, `true`, `false`, `:`, `break`, `continue`, and `type`
  run inside the shell without starting a process.
- Arithmetic follows POSIX with variables and assignment: `$((i += 1))`,
  comparisons, `&&`, `||`, `?:`, hex and octal literals, and Bash `**`.
- `init.qrl` in the configuration directory runs at interactive startup, so
  aliases, functions, and variables can be defined in shell syntax. A failing
  statement is reported with its file and line, and the rest still runs.
- A function or builtin can start a foreground pipeline, as in
  `myfunc | grep x`.
- Loops and groups can redirect their output, as in `done > list.txt` or
  `{ make; make test; } > build.log 2>&1`, and output still streams.
- `>&2` sends a command's output to standard error, so `echo "error" >&2`
  works for builtins, functions, and programs.
- The editor highlights reserved words, **Enter** continues an unfinished
  construct, and **Ctrl-C** stops a loop even when it never starts a process.

### Changed

- External commands start about twice as fast on macOS: each pipeline's
  trusted helper shells now use `/bin/dash` when available and start in
  parallel, and Quirl notices a finished command within microseconds instead
  of a millisecond. Running `/usr/bin/true` through `quirl exec` drops from
  12.5 ms in 0.5.1 to 6.5 ms.
- Quirl starts about 1 ms sooner on macOS because it no longer links
  CoreFoundation.
- A loop that starts many external programs now runs each one natively
  instead of handing the whole loop to `/bin/sh`. Each command still costs
  about 2.4 ms against about 1.3 ms in Bash, so 200 trivial commands in a loop
  take about 0.48 s, against 0.30 s in 0.5.1 and 0.28 s in Bash.
- Unquoted parameter and command expansions split into fields on `IFS`, as in
  `sh` and Bash. `ls $FLAGS` now passes each word separately, unlike Zsh; quote
  the expansion to keep one field. Empty unquoted expansions disappear.
- The command grammar and native runner protocols are now version 4.
- `$(...)` may produce up to 1 MiB of output instead of 16 KiB, so
  `for f in $(git ls-files)` works in large repositories.
- Here-documents, `[[`, `((`, `select`, subshells, brace expansion, process
  substitution, and compound commands used as pipeline stages or with input
  redirections still run in the `/bin/sh` island. Aliases and functions are
  not visible inside an island, and a line that defines a function and also
  needs the island is rejected.
- `quirl -c`, piped scripts, `eval`, and `source` still run exact POSIX `sh`.

### Fixed

- Errors a `$(...)` substitution writes to standard error now appear, as in
  Bash and Zsh. They were discarded, so a failing `eval "$(ssh-agent -s)"`
  ended silently.
