# ADR 0016: Reconcile runtime layering and ownership contracts

- Status: Accepted
- Date: 2026-08-16
- Decision owners: Quirl maintainers
- Supersedes: [ADR 0002](0002-crate-layering.md) and [ADR 0003](0003-preview-runtime-layers.md)
- Supersedes in part: the picker/core layering statement and protocol-change
  evidence in [ADR 0008](0008-protocol-freeze-and-migrations.md)
- Reconciles: the Windows process boundary in
  [ADR 0006](0006-platform-process-and-recovery-boundaries.md)
- Extends: [ADR 0010](0010-unix-first-release-scope.md)
- Constrains: proposed [ADR 0014](0014-external-history-provider-boundary.md)

## Context

The accepted architecture record no longer describes the implemented Cargo
graph. ADR 0002 predates `quirl-contract`, `quirl-lsp`, `quirl-picker`,
`quirl-plugin`, and `quirl-process`; ADR 0008 says that the picker cannot depend
on core even though it uses core's version and error contracts. ADR 0003 assigns
native execution to `quirl-process`, but the temporary `CommandRunner` still
spawns a shell from core and is used by process-layer built-ins. The blanket
cross-crate `ShellError` rule conflicts with the deliberately core-independent
syntax crate, and the claim that no unsafe Rust exists conflicts with the
accepted Windows Job Object wrapper.

The protocol golden inventory also proves only that checked-in descriptor text
has not changed. It cannot prove that the text still describes the Rust API.
The runner descriptor currently names command grammar v1 while the syntax crate
exports grammar v2, and it names backend methods that differ from the current
`ProcessBackend` trait. This ADR records that defect instead of treating its
unchanged hash as compatibility evidence.

This decision makes the current Cargo graph the accepted graph, assigns the
intended ownership inside that graph, and defines the evidence required for
future protocol changes. It does not move code or change a protocol identity.

## Decision

### The accepted crate graph is the manifest graph

The following table is the complete allowed set of direct dependencies between
Quirl workspace crates. An empty entry means that the crate has no dependency
on another Quirl crate. External library dependencies are reviewed under the
workspace's dependency policy; there is no longer a literal "serde-level
libraries only" ceiling.

| Crate | Direct Quirl dependencies |
| --- | --- |
| `quirl-catalog` | none |
| `quirl-core` | none |
| `quirl-syntax` | none |
| `quirl-contract` | `quirl-catalog`, `quirl-core` |
| `quirl-data` | `quirl-core` |
| `quirl-lua` | `quirl-core` |
| `quirl-picker` | `quirl-core` |
| `quirl-process` | `quirl-core`, `quirl-syntax` |
| `quirl-lsp` | `quirl-catalog`, `quirl-core`, `quirl-lua`, `quirl-syntax` |
| `quirl-plugin` | `quirl-catalog`, `quirl-contract`, `quirl-core` |
| `quirl-ui` | `quirl-catalog`, `quirl-core`, `quirl-lua`, `quirl-syntax` |
| `quirl-cli` | `quirl-catalog`, `quirl-contract`, `quirl-core`, `quirl-data`, `quirl-lsp`, `quirl-lua`, `quirl-picker`, `quirl-plugin`, `quirl-process`, `quirl-syntax`, `quirl-ui` |

`quirl-catalog`, `quirl-core`, and `quirl-syntax` are foundation peers with no
inter-Quirl dependency. `quirl-cli` is the sole product composition root and
the only product crate allowed to assemble every layer. The graph remains
acyclic, no product crate may depend on `quirl-cli`, and a foundation crate may
not depend on a service or runtime crate.

`quirl-bench` remains research-only, non-published tooling outside the product
graph. Its current direct Quirl dependencies are catalog, Lua, syntax, and UI;
no product crate may depend on it. `spikes/` remain separate workspaces.
Removing an edge is allowed when contracts and tests remain intact. Adding or
reversing an edge, adding a product crate, or moving composition ownership
requires a new accepted ADR.

### Syntax diagnostics are inert foundation values

`quirl-syntax` owns the command graph, parser, and `CommandSyntaxError`.
`CommandSyntaxError` is an effect-free diagnostic value carrying a message,
UTF-8 byte span, and help. Producing it performs no I/O, rendering, process
work, persistence, or exit-status selection, so syntax does not depend on core
merely to construct a `ShellError`.

Consumers map this value at the boundary they own:

- execution, file ingestion, persistence, or another shell-effect boundary
  converts it to `ShellError`, preserving its message, source span, help, and
  relevant command or source identity;
- a read-only presentation or protocol adapter, such as the LSP, may map it
  directly to that adapter's diagnostic value without manufacturing an
  operating error.

All other fallible cross-crate service and effect boundaries continue to use
`Result<T, quirl_core::ShellError>`. This exception is narrow: it permits
owned, inert foundation diagnostics to preserve the graph, not parallel error
stacks for I/O, resource exhaustion, process work, Lua, plugins, or persistence.

### Native process execution belongs to `quirl-process`

`quirl-core` owns the passive cross-layer contracts `ProcessRequest`,
`ProcessHost`, and `CommandOutcome`, along with `ShellError`. These values let
data and Lua callers receive a bounded capability without depending on its
implementation. Core must not select an interpreter, spawn or reap a child,
own a process group or Job Object, wire native pipelines, or manage terminal
handoff.

`quirl-process` owns execution of the native Quirl command graph, built-in
execution side effects, pipes and redirections, process-tree containment,
jobs, cancellation, deadlines, output bounds, and child cleanup. The CLI owns
composition, user-facing policy, recovery persistence, and selection of
explicit compatibility or adapter modes. A specialized CLI adapter may define
exact argv, environment, and protocol policy, but it uses process-owned RAII
containment and may not create a second unmanaged child-lifecycle model.

The current code is in a documented transition: `quirl-core::CommandRunner`
still performs shell execution and `quirl-process` still calls it for `cd` and
`ls`. It is not an accepted second executor and gains no new call sites or
features. Its retirement is ordered as follows:

1. Replace the process layer's `CommandRunner` delegation with process-owned
   built-in execution, reusing passive bounded value or directory helpers from
   core only where their ownership remains appropriate.
2. Route every executable process capability through `NativeExecutor` or an
   injected process-owned `ProcessHost`; move or delete the core runner's
   external-spawn behavior and tests.
3. Remove the public `CommandRunner` export and implementation, then remove
   core's `shlex` dependency when it has no remaining core owner.

Each step must preserve bounds and child cleanup and pass the canonical gate.
The `quirl-process` and `quirl-core` maintainers own this retirement. No release
or security claim may rely on the unbounded legacy runner while it remains.

Proposed ADR 0014 remains proposed. Its CLI ownership means provider policy,
correlation, and fallback composition; before that ADR can be accepted, its
child launch, timeout, reaping, and containment mechanism must be expressed
through a bounded `quirl-process` boundary rather than a new CLI-owned process
lifecycle. Quirl maintainers own that prerequisite.

### `quirl-picker` may depend on `quirl-core`

`quirl-picker` is a terminal-independent service crate, not a foundation peer.
Its dependency on core for `VersionPolicy`, `ShellError`, and `ErrorCode` is
sanctioned. Core may not depend on picker. A protocol-owning crate may expose a
plain descriptor string whether or not it can depend on core; descriptor
assembly is not a reason to invert an edge.

### Windows unsafe Rust is one audited boundary

Unsafe Rust remains prohibited in product crates except inside the private,
`cfg(windows)` Job Object FFI wrapper in `quirl-process`. That wrapper may call
only the Win32 operations required to create, configure, assign, terminate,
and close the Job Object. Every unsafe block has a local safety explanation.

The wrapper must maintain all of these invariants:

- construction rejects a null handle and configures kill-on-close before the
  handle can contain children;
- the wrapper uniquely owns the live Job Object handle and closes it exactly
  once;
- assignment borrows a live child process handle without consuming it;
- every assignment caller treats failure as fatal, kills and reaps the direct
  child, and never continues without containment;
- FFI structure types and byte lengths exactly match the selected Win32
  information class.

The disclosed spawn-to-assignment descendant race remains accepted only under
ADR 0010's best-effort Windows scope. Expanding the unsafe surface, changing
its ownership invariants, or claiming supported Windows containment requires a
new ADR, targeted failure-path tests, native Windows evidence, and a security
review.

### Descriptor changes require versioned owner evidence

ADR 0008's owner-defined descriptor and named-fingerprint model remains in
force with the following mandatory change workflow. A protocol change includes
any change to serialized shape, closed variants, interpretation, ordering,
validation, resource bounds, migration bounds, or a referenced protocol
identity.

For every such change, the owning crate must, in one logical change:

1. increment the protocol or schema version and update its canonical descriptor;
   changing only descriptor text, a fingerprint, or the golden inventory is
   prohibited;
2. retain an exact inline or checked-in encoded fixture and descriptor for
   every historical version the reader still accepts, plus a valid current
   fixture;
3. test current validation and rejection of future, expired, malformed, and
   unknown-field input where the format is deny-unknown;
4. for `migrated_range`, test a deterministic migration from every readable
   historical fixture through current validation and prove the named safety,
   permission, checksum, redaction, source-preservation, and default-preservation
   invariants that apply;
5. for `frozen_major`, either introduce an explicit migration and declare a
   migrated range, or test that the previous version fails closed with an
   actionable error; a migration must not invent unavailable secrets,
   executable commands, permissions, or authority;
6. update `docs/protocol-freeze-v1.json` only after the owner version,
   descriptor, fixtures, and reader tests agree. The golden remains composition
   evidence, never the schema source of truth.

A descriptor that names another protocol version must itself version when that
referenced identity changes. Fingerprint stability does not excuse a stale
semantic claim.

The existing runner descriptor is a known violation: runner v1 names
`quirl.command-grammar@1` while grammar v2 is current, and its described backend
methods do not match the current `ProcessBackend` trait. This task cannot repair
that identity without changing a protocol hash. The defect is assigned to the
`quirl-process` protocol owner and must be corrected before any further runner
descriptor or interface evolution: the next runner version must bind the
current grammar, describe the actual public contract, retain runner-v1
evidence, add a migration or tested fail-closed transition, and then update the
golden inventory.

## Consequences

- The allowed dependency graph can be derived directly from one accepted table
  and matches the manifests at this decision's base commit.
- Pure syntax analysis remains reusable without weakening the shell's
  `ShellError` contract at effectful boundaries.
- Native process ownership has one target layer and a reviewable retirement
  path instead of an undocumented permanent exception in core.
- Picker validation can use the common error and version policy without
  pretending picker is a foundation peer.
- The existing Windows unsafe code is acknowledged, narrow, and reviewable;
  the Unix-first support claim is unchanged.
- A golden fingerprint can detect drift but can no longer be cited as evidence
  that a stale descriptor matches its implementation.
- ADRs 0006, 0008, and 0010 remain authoritative for lifecycle behavior,
  protocol policy not amended here, and platform support scope. Where their
  layering wording conflicts with this decision, this ADR controls.
