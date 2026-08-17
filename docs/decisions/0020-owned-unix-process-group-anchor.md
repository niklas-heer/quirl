# ADR 0020: Owned Unix process-group anchors

- Status: Accepted
- Date: 2026-08-17
- Extends: [ADR 0006](0006-platform-process-and-recovery-boundaries.md),
  [ADR 0016](0016-runtime-layering-contract.md)

## Context

Quirl used the first guest process as a Unix pipeline's process-group leader.
The parent could reap that direct child before cleanup addressed descendants.
On Darwin, the kernel may then reuse the numeric process-group identifier.
Neither `killpg` nor bounded existence probes can distinguish a credential-
changed owned descendant from an unrelated group that acquired the reused
identifier. Signaling after the last owned identity is released can therefore
target processes that Quirl did not create.

Safe kqueue registration after exit reports only `EV_ERROR`/`ESRCH`, Darwin has
not supported `NOTE_TRACK` since macOS 10.5, the pinned safe Rust interfaces do
not expose Apple `waitid(WNOWAIT)`, and the audited crate policy prohibits a new
unsafe FFI boundary. Retrying `EPERM`, accepting it, or probing for eventual
`ESRCH` cannot establish ownership.

## Decision

Every Unix process group created by `quirl-process` has a dedicated direct-child
anchor member before guest code can execute. Quirl keeps the anchor unreaped
until the group is terminated or confirmed absent. A live group containing the
owned anchor prevents its numeric PGID from being reused while any group signal
is possible. No code may signal the group after releasing the anchor.

Native pipelines preserve the first guest as group leader. Quirl initially
spawns absolute `/bin/sh` in a fresh group with a fixed staging script and the
guest executable plus arguments as inert arguments. The script performs only a
bounded executable lookup, stops itself with `SIGSTOP`, and has not executed any
guest instruction when the parent observes that stop. Quirl then starts the
anchor directly in the staged leader's group, verifies readiness, and sends
`SIGCONT`; the staging shell `exec`s the guest in the same PID. Later stages
join the already-anchored group normally. A missing first executable exits the
trusted stage before its stop and remains a `ProcessSpawn` error; later spawn
failures retain the operating-system spawn error.

The anchor is absolute `/bin/sh` running a fixed, argument-free script with an
empty environment. Standard input is a private keepalive pipe, standard output
is a private readiness pipe, and standard error is null, so it never reads or
writes the user terminal. Before reporting ready it ignores `SIGHUP`, `SIGINT`,
`SIGQUIT`, `SIGTERM`, `SIGTSTP`, `SIGTTIN`, and `SIGTTOU`. `SIGKILL` and
`SIGSTOP` retain their kernel-defined behavior, allowing deterministic cleanup
and explicit whole-job suspension. A one-byte handshake must arrive within two
seconds and process-group membership is verified before any guest instruction
can run.

`ProcessGroupAnchor` owns the child and keepalive descriptor. Its cleanup sends
at most one group `SIGKILL` while the anchor is unreaped, directly kills and
reaps the anchor as a fallback, and then irrevocably releases the PGID. Group
errors remain errors; `EPERM` is never accepted generically. Direct guest
children are still killed and reaped individually so one group-operation
failure cannot skip resources the parent can address. Existing terminal leases
continue to restore the shell foreground group and termios before an operating
error escapes.

For single-root `ChildProcessTree` containment, the anchor itself leads the
group and the guest joins it. Callers must construct containment and apply its
command configuration before spawn; post-spawn assignment only verifies
membership and cannot establish containment retroactively.

## Bounds and failure model

One live group adds one process and two pipes. Startup temporarily adds one
reader thread and retains one byte; the thread is joined on success, malformed
readiness, EOF, timeout, or spawn/setup failure. Each anchor handshake waits at
most two seconds. Native construction also waits at most two seconds for the
trusted leader stage to stop, bounding complete native setup to four seconds;
that stage becomes the guest and is not an additional retained process. Native
pipeline-stage and retained-job limits bound the number of live anchors, and
each `ChildProcessTree` owns exactly one anchor and one guest root.

Failure before readiness kills and reaps the anchor without starting guest
code. Failure after any guest spawn keeps the original operating error while a
construction guard signals the still-anchored group, kills and reaps every
direct guest, and releases the anchor last. Normal exit, nonzero exit,
cancellation, deadline, stop/continue, backgrounding, foregrounding, terminal
handoff failure, explicit termination, and `Drop` all converge on the same
ownership sequence. If group signaling fails, Quirl reports or records the
cleanup failure, directly cleans what it still owns, releases the anchor last,
and performs no later PGID operation.

## Consequences

- Darwin PGID reuse cannot redirect Quirl's cleanup signal to an unrelated
  process group.
- Each native or adapter process tree costs one additional short-lived process.
- Unix containment setup can now fail before the requested executable is
  spawned when the trusted anchor cannot be created or verified.
- The process crate remains safe Rust and adds no dependency or unsafe FFI.
