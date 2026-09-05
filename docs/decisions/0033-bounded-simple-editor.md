# ADR 0033: Bound and safely render the simple editor

- Status: Accepted
- Date: 2026-09-05
- Decision owners: Quirl maintainers

## Failure model and invariants

The simple surface is a supported fallback, so it must preserve the same safety
properties as the rich interface. Testing found that bracketed paste was not
enabled, allowing pasted newlines to submit commands. Enabling it exposed a
second issue: raw control characters in pasted source could reach terminal
output as commands, including OSC 52 clipboard writes. History is another
source of the same untrusted text. Separately, repeated small inputs can grow
Reedline's buffer and undo history beyond the per-event transport bounds in
ADR 0032.

Paste must remain source until explicit submission. Displayed source must not
control the terminal. Display escaping must happen after splitting raw text
at the cursor, so source byte positions remain valid. Growth failure must not
leave a truncated command or a cursor outside the original buffer. Undo and
redo must retain valid state while respecting explicit resource limits.

## Decision

Enable Reedline's bracketed-paste guard in Quirl's simple-editor constructor.
Retain a narrow patch of Reedline 0.49 in `vendor/reedline`, selected through
the existing workspace Cargo patch mechanism, without new runtime dependencies
or product features.

The owning line buffer admits at most 64 KiB before insertion, replacement, or
buffer assignment. Failed growth records a typed limit diagnostic and prevents
dependent cursor changes. The editor's mutation boundary restores its bounded
pre-edit snapshot before the engine returns an I/O error. Rollback is per failed
edit command or callback: earlier successful commands in a compound event remain
committed, but no rejected input can be submitted afterward. Quirl maps the typed
cause to `ShellError` with `ErrorCode::ResourceLimit`; matching diagnostic text
alone is insufficient. The simple session exits on this admission failure,
restoring terminal state without executing the rejected command. The rich
editor instead reports a status notice and lets the user continue editing.

Undo retains the newest at most 128 states and 8 MiB of source text. Case
conversion may use a bounded temporary expansion of at most three times the
64 KiB input, but cannot retain an oversized line. All growth paths must return
through the same owning admission contract; imposing a limit on only paste
would leave typing, completion, history, and undo paths inconsistent.

Editor display fragments are escaped after splitting at the raw cursor position
and before applying trusted styles. The default history hint and uncolored
history-search result use the same safe display contract. Source bytes remain
unchanged for editing and execution. Quirl's existing completion display labels
and descriptions are already sanitized at their own boundary.

Vi command prefixes admit at most 64 characters. Decimal parsing and count
multiplication use checked arithmetic, and expanded editing actions admit at
most 1,024 actions before allocating the repeated event list. Dot replay counts
the prior event tree too, so multiplying an earlier repeated action cannot
bypass the bound. A typed error passes through the edit-mode boundary and is
checked before the rejected event takes effect. Aggregate expansion admits at most
1,024 actions per processing batch. Raw collection yields after 1,024 events or 20 ms, and rejects batches
containing more than 256 KiB of input text, so a continuous writer cannot
monopolize the editor. Events are parsed and applied in order under their current mode; a later
Escape must not retroactively turn earlier insertions into normal-mode edits.
Repainting is deferred to the end of the batch. Helix's shipped bindings do not
implement numeric-count expansion.

## Validation and maintenance

Real PTY regressions cover multiline paste admission, cancellation, exact
single submission, raw OSC isolation, resource-limit diagnostics, and terminal
restoration. In-file editor and retention tests cover valid boundaries,
rejection, rollback, cursor integrity, and undo/redo. These tests complement the
shared Crossterm transport checks; they do not establish native clipboard or
IME integration.

The vendored package retains its upstream license and source provenance, and
the release inventory identifies the local source. No new unsafe code is
introduced. The patch is owned by the terminal/editor layer and should be
removed when upstream provides equivalent admission and display guarantees,
after replaying the same regression tests. Windows remains untested.
