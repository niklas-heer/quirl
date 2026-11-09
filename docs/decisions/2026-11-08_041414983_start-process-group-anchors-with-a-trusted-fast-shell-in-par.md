+++
schema_version = 1
id = "01M6WNTCE7XKBEZ5RK3D9VW04N"
title = "Start process-group anchors with a trusted fast shell in parallel"
date = "2026-11-08"
status = "accepted"
tags = ["process", "performance", "security"]
supersedes = []
superseded_by = []
depends_on = ["01M2XHZ703RAK1JA9X3N0NFH2R"]
related_to = []
+++

## Decision

The owned process-group anchor protocol keeps every invariant of the
[owned Unix process-group anchors decision](2026-08-17_192326083_owned-unix-process-group-anchors.md)
and changes three implementation details:

- The fixed staging and anchor scripts run in `/bin/dash` when it is a
  root-owned regular executable that neither its group nor others can write,
  and otherwise in `/bin/sh` as before. The choice is made once per process.
  The scripts are unchanged and POSIX-only.
- The anchor starts immediately after the staged leader is spawned, in the
  leader's process group, instead of after the leader's `SIGSTOP` has been
  observed. Quirl then waits for the stop, verifies the group, and awaits the
  anchor's readiness byte before sending `SIGCONT`.
- Waits for the staged leader's stop and for foreground children poll with an
  exponential backoff that starts at 25 µs, capped at 200 µs and 1 ms
  respectively, instead of a fixed 1 ms sleep.

## Context

Every native pipeline paid for two shell startups and up to two millisecond
sleeps before and after the guest ran. On macOS `/bin/sh` is a shim that execs
another shell, so each startup cost twice as much. Running 200 trivial external
commands took about 1.5 s against about 0.25 s in Bash, dash, and Zsh, which made
scripts and loops that call programs feel sluggish.

The original decision fixed absolute `/bin/sh` because the anchor must be a
trusted program that runs only a fixed script. A root-owned `/bin/dash` that
others cannot modify meets the same trust bar, and `/bin/sh` remains the
fallback, so systems without dash behave exactly as before.

Starting the anchor early does not weaken ownership. The staged leader is a
direct child that Quirl has not reaped, so even if it exits first its process
group identity cannot be reused before the anchor joins or fails to join. No
guest instruction runs before `SIGCONT`, which still requires the stop, the
group check, and the anchor's readiness. If the anchor cannot start, Quirl
reports the leader's outcome, such as a missing executable, in preference to
the anchor error.

Alternatives considered:

- Letting a pre-spawned anchor lead the group, with the guest joining it,
  would remove the staging shell entirely. It changes which process leads the
  group, which job control and programs that inspect their group observe, so it
  needs its own decision and review.
- Blocking `waitpid` instead of polling cannot honour request deadlines and
  cancellation.

## Consequences

- External commands start about three times faster on macOS: with the
  shipped release profile, 200 trivial commands in a loop take about 0.48 s
  instead of 1.5 s, and a single command about 6.5 ms instead of 12.7 ms. See
  the [shell workload benchmarks](../benchmarks/shell-workloads.md). Linux
  gains less, since its `/bin/sh` is usually dash already.
- Fast commands no longer wait out a millisecond sleep; long-running commands
  settle at the previous poll interval and use no more wakeups.
- The anchor's trust now rests on file ownership and permissions checked at
  first use rather than a fixed path. A tampered system shell is outside
  Quirl's threat model in either design, since it runs as the same user.
