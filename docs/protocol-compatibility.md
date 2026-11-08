# Protocol compatibility

Quirl records its reviewed public contract identities in
[`protocol-freeze-v1.json`](protocol-freeze-v1.json). Schema definitions remain
in their lowest owning crates; the JSON file is a composition-level golden
fixture that prevents a field, enum, invariant, or migration policy from
changing unnoticed.

## Change policy

| Policy | Reader behavior | Required change |
| --- | --- | --- |
| `frozen_major` | Accept exactly the current version | Increment the protocol version; add an explicit migration or fail-closed diagnostic |
| `migrated_range` | Accept only the recorded inclusive range | Keep a deterministic migration fixture for every readable version |

Unknown fields and future versions are rejected at authoritative Rust
boundaries. Descriptor hashes use named FNV-1a for deterministic identity, not
security. Plugin file integrity continues to use SHA-256 and explicit grants.

Runner protocol v4 binds command grammar v4 and the actual `ProcessBackend`
methods. Grammar v4 adds a compound `Program` representation alongside the
existing `CommandList`, so loops, conditionals, functions, and aliases can be
parsed as a whole program; `CommandList` remains for simple-command lists, and
consumers that only need those keep using `parse_command_list`. The runner
descriptor binds `quirl.command-grammar@4` and declares the bounds those forms
need: program pipelines, function-call depth, session function and alias counts,
bytes per function definition, and word fields. It keeps the v3 guarantees:
bounded job retention, wrap-safe non-colliding job IDs, and source-ordered input
redirects. Runner v1 is retained as an exact historical descriptor and job-state
fixture, but fails closed because its descriptor named grammar v1 and methods
that did not match the public trait; there is no safe semantic migration from
that false contract. Runner v2 and v3 also fail closed: v2's grammar flattened
unquoted physical newlines into argument whitespace, and v3 predates the compound
program representation. Both `runner` and `command_grammar` are `frozen_major`
contracts, so a reader that receives an older or future version does not migrate
it. `validate_runner_protocol_version` in `crates/quirl-process/src/lib.rs`
rejects every version except the current one with a `Validation` error that names
the supported version. The serialized command graph for simple commands and the
job-state shapes are unchanged.

Common ABI v2 describes the actual tagged `StructuredValue` variants and uses
fixed-width `u64` diagnostic offsets on the wire. Extension protocol v2 uses a
fixed-width output byte count and freezes bounded iterative annotation scans
plus terminal-safe progress text. Agent schema v2 uses fixed-width `u64`
context budgets and estimates. Their version-1 descriptors remain pinned for
identity evidence, and version-1 documents fail closed rather than receiving a
lossy migration.

## Persisted migrations

| Document | Current | Oldest readable | Migration guarantee |
| --- | ---: | ---: | --- |
| Catalog cache | 4 | 2 | v2/v3 facts are assigned explicit lower-confidence defaults, then current builtins merge |
| Plugin lock | 3 | 3 | v1/v2 identities remain authenticated for diagnostics but fail closed; preserve the old file as `plugins.lock.json.legacy-v1`/`.legacy-v2`, then re-add after manifest-v2 review because an old lock cannot prove executable command I/O |
| Recovery snapshot | 2 | 1 | Existing redacted output/errors are preserved; unavailable command/cwd/environment facts stay unavailable and replay is never inferred |
| Lua config | 5 | legacy unversioned (v0) | Missing `schema_version` and explicit v1/v2/v3/v4 documents migrate deterministically to v5 defaults, including bounded project discovery, before Rust validation; explicit unknown/future versions fail |

## Deliberate 1.0 boundaries

The freeze manifest names limitations rather than hiding them. Native command
grammar v4 records the quote-aware Linux/macOS C1-core executor and explicit C2
dialect islands. Here-documents, process substitution, `[[`, `((`, `select`,
subshells, and dialect control forms remain reference islands for 1.0, rather
than an implied future native compatibility promise. Interactive POSIX control
flow, functions, and aliases run natively under grammar and runner protocol
version 4. Picker and
completion use separately frozen asynchronous request/cancellation/response
envelopes with bounded workers, deadlines, and stale-result suppression. The
runner result remains
text-only, Wasm remains validation-only, and the process-adapter v1 handshake
is executable under its scoped launch grant. The MCP stdio surface freezes its
bounded source-only tool set and keeps the modern 2026-07-28 discovery era
strictly separate from explicitly negotiated legacy sessions. Linux and macOS
are the supported interactive platforms. Windows remains a best-effort portable
process target, so native Windows terminal and suspend validation is not a 1.0
release gate. Differential conformance, performance, accessibility, and
security evidence remain independently tracked from schema identity. See
[ADR 0010](decisions/2026-08-16_192326022_freeze-the-1-0-release-scope-around-unix-and-explicit-dialec.md).
