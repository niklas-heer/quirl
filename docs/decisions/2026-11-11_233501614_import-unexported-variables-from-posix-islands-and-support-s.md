+++
schema_version = 1
id = "01M76FDZQEBTFXPXZTS7WFCQRD"
title = "Import unexported variables from POSIX islands and support set -a natively"
date = "2026-11-11"
status = "accepted"
tags = ["compatibility", "process", "interactive"]
supersedes = []
superseded_by = []
depends_on = []
related_to = ["01M42KGFQXE3Q6FB1DY16BMBYW", "01M6WEGFK5C8F7D3B4KZ3FEMMH"]
+++
## Decision

`eval`, `source`, `.`, and the implicit `/bin/sh` islands that interactive
Normal mode creates for syntax outside the native subset now import the
unexported variables their code changes, and `set -a`/`set +a` become native.
A plain `source .env` makes its variables available to later commands as in
Bash and Zsh, and `set -a; source .env; set +a` exports them.

**Unexported import.** Exports keep their existing contract: the final
`env -0` replaces the session's exported variables. In addition:

- Every scalar shell variable set when the island exits, absent from that
  `env -0`, whose value or export attribute differs from a baseline taken after
  the session locals are injected and before the code runs, becomes a session
  local. `unset X; X=same` on an exported `X` therefore leaves an unexported
  `X`, as in Bash.
- Session locals that the baseline held and the code unset are removed.
- Shell bookkeeping never counts: `_`, `SHLVL`, `PPID`, `UID`, `EUID`,
  `OPTIND`, `OPTARG`, `LINENO`, `RANDOM`, `SRANDOM`, `SECONDS`,
  `EPOCHREALTIME`, `EPOCHSECONDS`, `PIPESTATUS`, `FUNCNAME`, `GROUPS`,
  `DIRSTACK`, `HISTCMD`, `SHELLOPTS`, `POSIXLY_CORRECT`, `BASH`, `BASHOPTS`,
  `BASHPID`, every name beginning with `BASH_`, and the wrapper's `__quirl_*`
  scratch variables. Look-alikes such as `BASHFUL`, `COLUMNS`, and `LINES`
  count like any other variable. `PWD` and `OLDPWD` reach the session through the existing
  directory handling. `IFS`, `PS1`, and other shell defaults count only when
  the code changes them, because the baseline already holds them.
- Functions, aliases, arrays, readonly attributes, traps, positional
  parameters, and shell options stay inside the island.

The mechanism never parses or evaluates values. `set` output only nominates
candidate names: the text before the first `=` of each line, kept only when it
is a shell name. The shell confirms each candidate with `${NAME+x}` and prints
`NAME=value` NUL-terminated with its builtin `printf`; a multi-line value that
contains a line such as `FAKE=spoofed` nominates `FAKE`, which is dropped
unless it is really set. Bash lists arrays as `NAME=(...)` and quotes every
scalar beginning with `(`, so those lines are skipped; because a line inside a
multi-line value can also nominate an array's name with a scalar-looking value,
every name some line lists as `NAME=(` is checked with Bash's own
`declare -p`, and real arrays are skipped. The `EXIT` trap first turns off
`-x` and `-v`, so tracing the code left on never prints values, and removes
every alias and any function shadowing `printf`, `unalias`, `builtin`, or
`declare`; in POSIX mode the special builtins it also uses cannot be shadowed
by functions, and it quotes their names against aliases. Code that leaves POSIX
mode and redefines `set`, or protects a function with `readonly -f`, can defeat
that scrub; such code is inside the trust boundary below, and a final dump with
no variables, which a real shell never writes, counts as malformed. It then saves `IFS` and
`LC_ALL` and turns off `-a`, `-e`, and `-u` so options the code left on cannot
stop the dump. `env -0` reports the exports before the dump changes `IFS`, so
an exported `IFS` keeps the code's value. Both dumps run under `LC_ALL=C`,
which keeps the name check byte-wise in every locale; the baseline restores the
session's `IFS` and `LC_ALL` before the code runs.

The trust boundary is the code, not the dump. Sourced code is trusted: it runs
with the user's rights and can already set any variable it likes, so the import
does not try to defend against it. What the import guarantees is narrower:
variable values are data, so no value can spoof another variable's name or
inject code into the session, and a broken or interrupted dump imports nothing.
Both dumps are framed by fixed header and trailer records, and Rust revalidates
every name. A truncated or malformed dump (for example after the code ran
`readonly IFS` or `unset PWD`), a dump above its byte bound, or a shell that
ended before its trap wrote the dump (killed by a signal, `exec`, or a
replaced `EXIT` trap) imports nothing: exports, locals, and the directory stay
as they were, and the session prints a one-line warning saying no variables
were imported and why. Every variable change commits in one transaction that
checks the session's variable and byte limits, so an import over a limit also
changes nothing.

**`set -a` and `set +a`.** The session keeps a native allexport flag. While it
is on, every assignment (bare `NAME=value`, `for` variables, `${NAME:=word}`,
and arithmetic assignments) sets and exports; existing unexported variables
stay private until assigned again. Islands start their code with `set -a` after
the session locals are injected, so their assignments come back exported.
`set +a` stops automatic export without removing the export attribute from
variables it already exported, as in Bash. `set -o allexport` and
`set +o allexport` are the same options. Other `set` forms in a plain command
list, and `set` as a pipeline stage, fail with a diagnostic naming the
supported forms, instead of `exec: set: not found`.

## Amendments

This amends the "Import variables set without `export`" alternative that the
[POSIX invocation decision](2026-10-04_044214653_non-interactive-invocations-and-shell-setup-code-run-in-posi.md)
rejected, and its statement that "variables set without `export` ... stay inside
the island". Its other clauses, including running `eval` and `source` in
`/bin/sh` and the export import, still apply.

It also amends two clauses of the
[native control-flow decision](2026-11-08_020630501_run-posix-control-flow-functions-and-aliases-natively-in-int.md):
`set` with only allexport operands no longer sends a compound program to the
island, because the executor runs it natively; other `set` forms still do. Of
its "two intended differences" between native and island execution, unexported
variables assigned in a loop no longer differ, because islands now import them
too. Only persistent functions remain a difference.

## Context

Issue #25: a user of Quirl 0.6.0 ran `source .env` on a dotenv file without
`export` lines and found every variable empty, directly and inside a loop,
while Bash and Zsh set them. The documented workaround, `set -a; source .env;
set +a`, failed with `exec: set: not found`. The `source` help even used
`source .env` as its example.

The POSIX invocation decision rejected importing unexported variables because
`sh` has no portable, unambiguous way to list them, and because children would
not see them in Bash or Zsh either. Since 0.6.0 Quirl keeps unexported shell
variables natively, so the second reason no longer holds: an imported local
behaves exactly as in Bash. The first reason is answered by using `set` output
only for names and letting the shell itself confirm and print each value.

Alternatives considered:

- **Keep the behavior and support only `set -a`.** Rejected: plain
  `source .env` would keep silently doing nothing, which is the surprising part.
- **Fix only the help text.** Rejected for the same reason.
- **Parse `set` output values in Rust.** Rejected: quoting differs between
  Bash and Dash, and a parser of shell quoting is exactly the approximation the
  island exists to avoid. Values never leave the shell except through `printf`.
- **Import every unexported variable, without a baseline.** Rejected: the
  shell's own defaults (`PS1`, `IFS`, `OPTIND`, `HOSTNAME`, …) would flood the
  session.

## Consequences

- `source .env`, `eval 'X=1'`, and pasted island-only lines set session
  variables as Bash and Zsh do, verified by differential tests against
  `/bin/sh` (Bash 3.2 in POSIX mode on macOS) and Dash.
- Each island costs two `set` snapshots and two dump loops. Measured on an
  Apple Silicon Mac with 40 to 100 environment variables, 20 session locals,
  and a 30-line dotenv file, the median island grew from 4.9–5.2 ms to
  10.8–12.8 ms under `/bin/sh` (Bash 3.2), and from 3.3–3.4 ms to 5.9–7.1 ms
  under Dash. The cost is linear in the number of set variables; Bash 3.2's
  per-variable loop dominates, at roughly 10 to 15 ms per island for a 30-line
  dotenv file including the array check, and running the loop under
  `LC_ALL=C` cut it by about 40%. Checking array-looking names
  with `declare -p` adds one subshell per such name, which only Bash arrays and
  spoofing values produce.
- The dump file can hold the session environment twice, so its byte bound is
  twice the session byte limit plus framing.
- A variable the code sets to the value it already had, and that was not
  exported before, is indistinguishable from an untouched one and is not
  re-imported; the session already holds that value.
- `BASH`, `BASHOPTS`, `BASHPID`, names beginning with `BASH_` or `__quirl_`,
  and the other bookkeeping names cannot be set through an island. Session
  locals named `__quirl_*` are not passed in, and neither are the shell's
  read-only `PPID`, `UID`, `EUID`, `SHELLOPTS`, `BASHOPTS`, and
  `BASH_VERSINFO`, which would otherwise stop every island.
- A sourced file that redefines `printf` or aliases `eval` still imports its
  real values. One that leaves the shell unable to report (`readonly IFS`,
  `unset PWD`, its own `EXIT` trap) imports nothing and says so.
