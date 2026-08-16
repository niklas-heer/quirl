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

## Persisted migrations

| Document | Current | Oldest readable | Migration guarantee |
| --- | ---: | ---: | --- |
| Catalog cache | 4 | 2 | v2/v3 facts are assigned explicit lower-confidence defaults, then current builtins merge |
| Plugin lock | 2 | 1 | Identity, sources, checksums, permissions, grants, and enable state are preserved; runtime schema identity is derived deterministically |
| Recovery snapshot | 2 | 1 | Existing redacted output/errors are preserved; unavailable command/cwd/environment facts stay unavailable and replay is never inferred |
| Lua config | 3 | legacy unversioned (v0) | Missing `schema_version` and explicit v1/v2 documents migrate deterministically to v3 defaults before Rust validation; v3 adds the closed built-in `ui.theme` catalog, and explicit unknown/future versions fail |

## Deliberate 1.0 boundaries

The freeze manifest names limitations rather than hiding them. Native command
grammar v2 records the quote-aware Linux/macOS C1-core executor and explicit C2
dialect islands. Here-documents, process substitution, loops, functions,
conditionals, and dialect control forms remain reference islands for 1.0,
rather than an implied future native compatibility promise. Picker and
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
[ADR 0010](decisions/0010-unix-first-release-scope.md).
