+++
schema_version = 1
id = "01M2XHZ6XVXTAP30HMCW9N5TMV"
title = "Freeze public protocols with owner-defined descriptors"
date = "2026-08-15"
status = "accepted"
tags = ["rust", "release"]
supersedes = []
superseded_by = []
depends_on = ["01M2XHZ6WRQNS9VTHY13TCBSBE", "01M2XHZ6XNA29RS8DBD0MYCT9V"]
related_to = ["01M2XHZ6Z9746X4SH1P1PET9C7"]
+++
- Extends: [ADR 0002](2026-08-15_192325976_enforce-one-way-crate-layering.md), [ADR 0007](2026-08-15_192326005_make-the-semantic-catalog-the-authoritative-command-contract.md)
- Superseded in part by: [ADR 0016](2026-08-16_192326057_reconcile-runtime-layering-and-ownership-contracts.md)

## Context

Quirl's public machine surfaces are spread across deliberately layered crates.
Serde derives and numeric constants alone do not prove that a version still
means the same shape: a field or enum can change while its version stays fixed.
Conversely, a composition-root snapshot must not become a second source of
truth for schemas owned by lower crates.

Several persisted formats also predate the freeze. Catalog caches have an
explicit v2/v3 to v4 migration, but plugin locks and recovery snapshots did not
have equivalent readers, and Lua configuration was unversioned.

## Decision

Each owning crate publishes a canonical, complete structural descriptor beside
its protocol version. The descriptor covers serialized fields, closed variants,
important ordering and validation invariants, and known migration bounds. A
named FNV-1a fingerprint identifies that descriptor. This fingerprint detects
accidental compatibility drift; it is not an authenticity or supply-chain
checksum.

`quirl-core` owns the common version policy and fingerprint algorithm. Crates
that cannot depend on core, such as catalog, syntax, and picker, expose plain
descriptor strings; the CLI hashes them while assembling the reviewed
`protocol-freeze-v1.json` golden inventory. This preserves every dependency
arrow in ADR 0002. The inventory is evidence and a drift detector, not a schema
definition.

Compatibility is explicit per surface:

- `frozen_major` accepts only the declared version. A structural or semantic
  change requires a new version and either a migration or a fail-closed error.
- `migrated_range` accepts only a documented inclusive range and deterministically
  projects older documents to the current shape before validation.

Catalog v2/v3 migrates to v4. Historically, plugin lock v1 migrated to v2 without changing
identity, checksums, requested permissions, grants, or enabled state; the new
runtime-schema hash is derived from the locked runtime. Recovery v1 migrates to
v2 with explicit unavailable markers for facts v1 never stored and never
reconstructs secrets or executable commands. Lua config schema v2 adds the
Ratatui-surface, status-line, transient-prompt, and completion-policy fields
accepted in [ADR 0012](2026-08-16_192326034_ratatui-is-the-default-capable-terminal-surface.md). An absent config
version is legacy v0; both v0 and explicit v1 deterministically receive v2
defaults and become v2 before authoritative validation. Config versions newer
than v2, like future versions of every frozen-major surface, fail closed.

ADR 0013 subsequently moves configuration to schema v3 for bounded built-in
and custom semantic themes. Legacy v0/v1/v2 documents receive the Tokyo Night
default before v3 validation; versions newer than v3 fail closed.

The integrated 0.1.0 candidate subsequently advances configuration to schema
v4 for user-facing defaults: a compact banner, automatic completion after one
character, and the active Rust toolchain in the right prompt. Legacy
v0/v1/v2/v3 documents migrate deterministically to those v4 defaults; versions
newer than v4 fail closed. The historical v2 and v3 descriptors remain pinned
as migration evidence.

ADR 0030 subsequently advances configuration to schema v5 for bounded project
discovery. Legacy v0/v1/v2/v3/v4 configurations migrate to v5 defaults before
validation; unknown future versions fail closed. The v4 descriptor remains
pinned alongside the earlier descriptors as migration evidence.

The executable plugin-I/O transition later supersedes that historical lock
migration under ADR 0016's fail-closed rule. Plugin manifest v2 gives command
I/O a closed Lua ABI-v1 interpretation, and lock v3 binds that identity. Lock
v1/v2 bytes are still decoded and authenticated for precise diagnostics, but
are not readable as current state because their metadata cannot prove the new
reviewed command contract. Users preserve the old lock under a legacy filename
and re-add reviewed manifest-v2 plugins; Quirl does not synthesize a v3 runtime
schema hash from older lock data.

## Consequences

- A reviewed golden change is required when any public contract identity moves.
- Persisted migrations are deterministic and tested for permission, checksum,
  redaction, source preservation, and config-default preservation.
- Descriptor fingerprints do not replace SHA-256 content integrity in plugin
  locks and do not establish trust.
- The manifest is a **1.0 freeze candidate**, not proof that a release artifact
  passed every independent gate. Command grammar remains a documented bounded
  subset; completion and picker
  have separately frozen asynchronous envelopes with bounded worker and
  stale-result evidence. Runner output remains text-shaped; Wasm remains
  non-executing and its full WIT structural binding is pending, while the
  separate process-adapter v1 initialization handshake is executable.
  Performance, security, accessibility, and compatibility evidence remain
  separate release gates. ADR 0010 freezes Linux/macOS as the supported
  interactive platforms and keeps Windows terminal behavior best effort.
