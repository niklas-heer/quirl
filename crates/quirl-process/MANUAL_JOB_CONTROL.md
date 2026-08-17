# Manual PTY job-control check

## Failure model and invariants

The native executor treats pipeline construction as a transaction and terminal
ownership as a lease. These are the failure cases that every implementation and
real-PTY check must preserve:

- **Process-group identity:** every Unix group has a dedicated direct-child
  anchor member before guest code can run. The anchor stays unreaped while any
  group signal is possible, so the live owned group prevents its PGID from
  being reused for an unrelated group. Cleanup signals the group once while the
  anchor is owned, then kills and reaps the anchor and never addresses that PGID
  again.
- **Native leader staging:** the first native guest starts as absolute
  `/bin/sh` running a fixed staging script in a fresh group. It validates the
  executable, stops before `exec`, and is continued only after the separate
  anchor has joined and reported ready. The same PID then executes the guest,
  preserving the first stage as process-group leader and the foreground TTY
  contract without allowing guest instructions before anchored ownership.
- **Anchor construction:** Quirl starts absolute `/bin/sh` with a fixed script,
  an empty environment, piped standard input, and null terminal-facing output.
  The script ignores interrupt, termination, and background-terminal signals,
  but retains the default `SIGTSTP` disposition before its one-byte readiness
  handshake. The foreground wait polls that owned anchor as a stop sentinel;
  observing it stopped causes one anchored group `SIGSTOP`, covering a Darwin
  PTY delivery where the anchor stops but a guest misses the terminal
  `SIGTSTP`. Spawn, process-group verification, early exit, malformed
  readiness, and each two-second setup wait all unwind by killing and reaping
  the anchor before an error is returned.
- **Partial spawn:** every child and descriptor is owned immediately. Failure
  after any spawn kills the process group, kills each direct child as a
  fallback, reaps every direct child, closes pending pipe ends, and returns the
  original construction error.
- **Leader exit before group formation:** the parent verifies each child's
  process-group membership after spawn. It never continues with an assumed
  group, and cleanup addresses both the group and each direct child so a fast
  leader cannot strand later stages.
- **Terminal handoff failure:** children remain inside the construction guard
  until handoff succeeds. A failed handoff terminates and reaps them. Once
  handoff succeeds, an RAII lease restores Quirl's foreground group and saved
  termios on success, stop, cancellation, wait failure, and unwinding.
- **Stopped child:** observing one stopped pipeline member stops every live
  member before the job is committed. A stopped job retains its children and
  bounded drain tasks until `fg`, `bg`, cancellation, completion, or executor
  destruction owns the next transition.
- **Cancellation and deadline:** each bounded polling turn observes the request
  state. Cancellation or expiry terminates the whole contained process tree,
  reaps direct children, drains or closes capture streams, and returns the
  cancellation error rather than a cleanup error. Public hosted execution uses
  `NativeExecutor::execute_interactive_request` or `execute_capture_request`;
  the frozen `ProcessBackend` string methods are trusted-local conveniences
  with no host cancellation flag or deadline.
- **Drains:** capture readers continuously drain child output while retaining
  only the configured byte budget. Here-string writers remain owned by the job
  while stopped and are joined after completion or termination. When direct
  children finish first, refresh best-effort terminates surviving group members
  before joining either kind of task, so inherited pipe ends cannot hang the
  shell.
- **Expansion:** one pipeline retains at most 1 MiB of expanded command words
  and redirect targets. The budget is cumulative across fragments, words, and
  stages; the first byte beyond it fails with `ResourceLimit` before that byte
  is appended. NUL bytes are rejected as invalid command data before spawn.
- **Cleanup failure:** cleanup is best-effort across every owned resource; one
  failed group operation does not skip direct-child cleanup. When an operating
  error already exists, cleanup cannot replace it. A standalone explicit
  lifecycle operation reports its own cleanup failure with actionable context.
  Even when group signaling fails, the direct-child anchor is killed and reaped
  last; no retry or probe may use the released PGID.
- **Retained jobs:** a `NativeExecutor` retains at most 1,024 job records. At
  capacity it refreshes state and removes completed records before accepting a
  new job; 1,024 still-live records fail early with `ResourceLimit`.
- **Job IDs:** zero is never issued. Allocation wraps from `u32::MAX` to one and
  scans the bounded retained table, so an ID cannot collide with a visible job.
- **Redirects:** redirect targets are opened in source order and the last input
  redirect supplies standard input. Earlier opens still take effect or fail,
  matching shell descriptor-order semantics.
- **Reference dialects:** Bash and Zsh compatibility runners are explicitly
  noninteractive. Their standard input is closed; they remain process-group or
  Job-Object contained, continuously drain bounded captures, and observe
  cancellation. Scripts that need interactive reads must be run directly in a
  terminal outside this compatibility boundary.

Every wait above is either request-bounded or advances in bounded polling turns.
Foreground interactive commands may intentionally run until they exit, stop,
or receive a terminal signal; they do not retain output in memory.

Each live Unix process group adds exactly one anchor process, two bounded pipes,
one retained readiness byte, and one reader thread for at most the two-second
anchor handshake. Native groups also have a preceding two-second leader-stop
wait, for at most four seconds of total setup; the trusted staging shell becomes
the first guest rather than adding another live process. The reader is joined on
every startup outcome. Native stage and retained-job limits therefore also
bound anchors; established groups retain no anchor reader thread or output
buffer. Foreground waits poll at most 16 queued anchor status transitions per
turn before yielding to child polling and cancellation.

The canonical Unix PTY harness automates foreground-group ownership, native
`Ctrl-Z`/`Ctrl-C`, `jobs`/`bg`/`fg`, fast-leader and construction-failure
races, stopped-job termios preservation, prompt restoration, and the explicit
noninteractive dialect-island policy:

```sh
cargo build -p quirl-cli
cargo xtask rich-pty
```

The following remains a useful release smoke check in a maintainer's terminal
after the automated harness passes.

```sh
cargo run -p quirl-cli
```

At the Quirl prompt:

1. Run `sleep 30`, press `Ctrl-Z`, then run `jobs`. The job must be `stopped`
   and the prompt must accept input normally.
2. Run `bg %1`, then `jobs`. The same job id must be `running`.
3. Run `fg %1`, then press `Ctrl-C`. Quirl must regain the terminal and the
   next prompt must be usable without an extra keypress.
4. Run `sh -c 'printf out; printf err >&2' | cat`. Both streams must complete;
   Quirl must not hang.
5. Run `printf hidden > /tmp/quirl-process-manual | cat`, then
   `cat /tmp/quirl-process-manual`. The pipeline prints nothing and the file
   contains `hidden`.

Repeat steps 1–3 on both Linux and macOS before calling the Preview job-control
gate complete.
