# ADR 0019: Isolate executable Lua behind supervised workers

- Status: Accepted
- Date: 2026-08-17
- Decision owners: Quirl maintainers
- Supersedes in part: the in-process execution decision in
  [ADR 0001](0001-lua-extension-language.md)
- Extends: [ADR 0016](0016-runtime-layering-contract.md),
  [ADR 0017](0017-shared-execution-contract.md), and
  [ADR 0018](0018-typed-lua-runner-abi.md)

## Context

Lua instruction hooks enforce instruction, cancellation, and elapsed-time
checks only while the interpreter dispatches Lua instructions. Lua 5.4 library
functions implemented in native C can run without dispatching a hook. A host
watchdog thread cannot safely close or otherwise access that thread's
`lua_State`, and abandoning the thread would retain unbounded work. Therefore
an in-process VM cannot provide the hard wall-clock bound promised by
`LuaPolicy` for arbitrary admitted native calls.

Protected calls add a second failure mode. Guest `pcall` and `xpcall` can turn
ordinary Lua errors into values. Policy termination must remain Rust-owned and
sticky for the invocation so guest code cannot catch a resource-limit error
and continue.

## Decision

### Executable CLI Lua runs in a worker process

`quirl-cli`, the composition root, launches a worker mode in the current Quirl
executable. The worker is persistent for the interactive VM and each active
plugin so globals and registered callback closures retain their existing
lifetime. Config, eval, run, test, plugin activation, prompt, completion,
contribution, command, and event execution all cross this boundary. Static
linting, parsing, formatting, SDK generation, and pure Rust theme selection do
not execute guest functions and remain analysis paths rather than worker-state
owners.

The worker exclusively owns `lua_State`. The parent never accesses or closes
it. The parent owns the absolute deadline, cancellation observation, protocol
pipes, process-tree containment, direct-child reap, and reader-thread join. A
timeout, cancellation, crash, malformed frame, oversized frame, or identity
mismatch terminates and reaps the worker and poisons that worker instance.
Reload installs a new validated extension generation; code must never reuse a
poisoned VM.

Workers receive piped stdin and protocol stderr while stdout is closed. They
never inherit the terminal and never own a foreground process group.

### Process capability is parent-proxied

A worker does not spawn operating-system children. `quirl.process.run` emits a
typed host-call frame and waits for its matching response. The parent executes
the request through `quirl-process::isolated_process_host`, which closes stdin,
never transfers terminal ownership, rejects background and stateful job-control
forms, and applies cancellation, deadline, output, process-group, and reap
bounds. This keeps all guest-selected process ownership in `quirl-process` and
prevents a worker kill from orphaning a separately grouped child.

### Worker protocol v1 is frozen

The internal protocol is `quirl.lua-worker@1`. Frames have a four-byte
big-endian length followed by compact JSON and are capped before allocation.
Every envelope, operation, host call, host response, policy, and diagnostic wire
record denies unknown fields. Requests and responses carry a `u64` request ID;
host calls additionally carry a bounded `u32` call ID. A request may issue at
most sixteen host calls.

Policy fields use fixed-width integers and are checked by both peers:

- memory: 1 through 64 MiB;
- instructions: 1 through 100,000,000;
- wall time: 1 through 60,000 ms;
- frame bytes: source maximum plus 1 MiB of bounded protocol context.

The canonical descriptor and exact current request/response fixtures live with
the owner in `quirl-cli/src/lua_worker.rs`. V1 has no predecessor to migrate.
Expired, future, malformed, oversized, and unknown-field inputs fail closed.
The protocol is private between identical Quirl executables and is not added to
the public protocol-freeze manifest; making it externally interoperable would
require a new version, public fixtures, and a manifest update.

### In-process defenses remain

Every VM still uses restricted standard libraries, allocator and instruction
budgets, cancellation, typed ABI validation, capability grants, and
`ShellError`. Rust-owned sticky policy termination propagates through guest
`pcall` and `xpcall`. Dynamic `load` and native Lua pattern entry points remain
disabled as defense in depth. These measures improve early failure and error
quality but are not cited as the hard wall-clock mechanism.

## Failure and cleanup invariants

- Partial spawn or pipe/reader initialization kills and reaps the direct child.
- Cleanup preserves the original timeout, cancellation, or protocol error and
  appends cleanup failure as context.
- No worker, descendant, protocol reader, or extension scheduler thread is
  detached on final destruction.
- Frame length is checked before allocation; decoded values repeat their domain
  validation at the parent boundary.
- A response must match the current version and request/call identity and must
  contain exactly one success or error representation.
- The accepted Windows spawn-to-Job-Object assignment limitation remains the
  best-effort boundary documented by ADR 0016; the worker waits for its first
  request before constructing a VM or requesting process work.

## Consequences

Executable Lua pays process and protocol overhead, including one persistent
process per active plugin runtime. This is bounded by the existing plugin
cardinality limit. Native C calls can now be preempted by killing the worker,
and extension callback cancellation unblocks host scheduler threads so final
shutdown can join them rather than leak work.

Direct users of the lower-level `quirl-lua::LuaRuntime`, including static
analysis inside `quirl-lsp`, retain an in-process library API. They must not
claim a hard wall-clock bound for guest execution. Product composition that
executes untrusted Lua belongs in the CLI worker boundary defined here.
