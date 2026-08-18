# Local completion process boundary

This note records the failure model, resource sketch, and invariants used to
implement the reusable Zsh and Fish local-completion boundary. It is kept next
to the owning crate because these constraints are part of the process contract,
not composition-root discovery policy.

## Failure model

Completion code is untrusted local code. An admitted completion script or a
shell function may hang, fork descendants, ignore ordinary termination, emit
unbounded or malformed bytes, close protocol descriptors early, return invalid
UTF-8, or exit after leaving descendants alive. The filesystem may change
between validation and shell startup. The shell may be absent, non-executable,
or missing the requested provider. Cancellation and the deadline may arrive at
any initialization or execution point, including after the direct shell exits.
Pipe-reader startup and process-group cleanup may also fail after partial
initialization. Zsh additionally needs a sourceable initialization adapter;
temporary-file creation, writing, sourcing, or removal may fail.

Missing shell executables and missing registered providers are typed
`Unavailable` results. Invalid requests, filesystem failures, spawn failures,
timeouts, cancellation, malformed/truncated frames, excessive output, and
cleanup failures are `ShellError` values. A cleanup failure is never hidden by
a successful provider exit.

## Resource sketch

One accepted request owns one process-group anchor, one direct shell, two
nonblocking output descriptors, and shell-created descendants in the same
group. The boundary has a configured active-request slot limit. Request input
is bounded by command-path depth, argument/root/script/environment counts, and
per-field plus aggregate environment bytes. Runtime is bounded by an absolute
deadline and observable cancellation. Both pipes are drained, while total
retained output and total observed output are checked against one byte limit.
The decoder uses a four-byte stream magic (`QLB1` for byte lengths or `QLU1`
for Unicode-scalar lengths) followed by two eight-digit hexadecimal field
lengths per record. It then checks record count, field bytes, candidate count,
exact truncation, and UTF-8 before allocating each field. Zsh uses byte lengths;
Fish uses scalar lengths because Fish exposes string length in characters.
The fixed Zsh adapter is written once to a mode-0600, create-new temporary file
using at most 32 candidate names. Its path length and source size are bounded,
and an RAII guard removes it after success or any initialization failure. The
normal path reports a removal failure as `ShellError`; `Drop` is the unwind
fallback.

The normal interactive path therefore uses constant process/thread counts and
memory proportional only to the configured retained-output and candidate
limits. Polling performs one status check per millisecond so each event-loop
turn is bounded and cancellation remains observable.

## Invariants

1. The caller supplies an absolute shell path and explicitly admitted
   completion roots, scripts, and environment. Child environments are cleared;
   Zsh user/global startup files and Fish user configuration are disabled.
2. Every spawned shell is assigned to an already-owned containment group before
   guest completion code runs. The ownership guard kills the group and reaps
   the direct child on success, error, cancellation, timeout, and unwind.
3. A successful call means both output descriptors were drained through a
   bounded nonblocking turn, the direct child was reaped, and the descendant
   group was terminated. Partial pipe initialization remains inside the RAII
   child guard, so failure immediately terminates and reaps the process tree.
   The Zsh adapter-file guard is acquired before spawn and removed after the
   child guard completes, including spawn and protocol failures.
4. Zsh completion runs in an interactive `zsh/zpty` child and captures
   `compadd` through a dedicated inherited descriptor. PTY display bytes never
   enter the machine protocol.
5. Fish uses its documented `complete --do-complete` interface. The Fish-side
   adapter immediately converts provider records into Quirl frames; Rust never
   infers candidate/description boundaries from a delimiter.
6. The Quirl frame is length-prefixed and EOF-terminated. No candidate or
   description byte can be interpreted as structure, and incomplete headers or
   fields fail closed.
7. Provider output is machine data. It is never executed, and diagnostic
   excerpts are terminal-escaped before entering `ShellError` context.
