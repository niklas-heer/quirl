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
| Lua config | 1 | legacy unversioned | Missing `schema_version` becomes 1 before Rust validation; explicit unknown/future versions fail |

## Deliberate pre-1.0 surfaces

The freeze manifest names limitations rather than hiding them. Native command
grammar still advertises a preview compatibility subset, picker/completion lack
full versioned asynchronous request envelopes, the runner result is text-only,
and Wasm/out-of-process boundaries are non-executing. These contracts may be
versioned again before the Phase 4 release gate is accepted. Windows suspend,
full C1 differential conformance, performance, accessibility, and security
evidence are tracked independently from schema identity.
