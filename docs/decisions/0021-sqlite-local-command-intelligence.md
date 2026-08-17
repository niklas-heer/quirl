# ADR 0021: SQLite local command intelligence

- Status: Accepted
- Date: 2026-08-17
- Extends: [ADR 0016](0016-runtime-layering-contract.md)

## Context

Declarative Fish, Bash, and Zsh completion sources and imported help/man text
produce command facts that must support completion, help, agent context, option
intent lookup, and natural-language discovery. A JSON catalog plus a separate
discovery sidecar gives readers an exact snapshot, but it cannot efficiently
query normalized options or retain a local semantic index. Keeping independent
stores for completion and AI would allow facts and provenance to drift.

Quirl also needs semantic inference without Python, ONNX, a GPU, a network
service, or an unrestricted model runtime. Model input and output cross an
untrusted filesystem boundary, and model work cannot enter the per-keystroke
catalog construction path.

## Decision

`quirl-cli`, the product composition root, owns a bundled SQLite database via
`rusqlite`. Foundation crates remain unaware of persistence. A database stores
one exact serialized `Catalog` snapshot plus normalized commands, aliases,
arguments, values, conflicts, examples, effects, exit codes, provenance,
semantic documents, embeddings, and discovery state. The exact snapshot is the
reader source of truth; normalized rows are query projections written in the
same transaction.

Database construction occurs in memory. The completed image must fit the
128 MiB bound before the existing hardened atomic replacement installs it.
Readers reject non-regular, linked, over-permissive, oversized, wrong
application-id, wrong-version, malformed, or catalog-incompatible files.
Discovery retains its existing source, record, byte, deadline, cancellation,
and refresh bounds. Legacy JSON catalog schemas remain read-only migration
inputs; all writes use SQLite. If bounded first-run discovery fails before any
valid database exists, interactive initialization atomically publishes a
builtin-only SQLite image. A valid prior database is never replaced by this
fallback, and a later successful discovery generation replaces the fallback
through the normal publication path.

Semantic inference uses `model2vec-rs` with only its `local-only` and `onig`
features and the `minishlab/potion-base-8M` model. After the interactive catalog
is admitted, the composition root starts one session-owned worker. If the
default model is absent or corrupt, that worker downloads the three files from
revision `bf8b056651a2c21b8d2565580b8569da283cab23` over bounded rustls HTTPS,
streams them into a private staging directory, verifies exact byte counts and
SHA-256 digests, and publishes the directory with one rename and parent sync.
An invalid auto-owned destination is quarantined before a clean install;
explicit `QUIRL_MODEL_PATH` destinations are never replaced automatically.

The model files total 30,920,628 bytes. Redirects, connect/global/body waits,
channel retention, staging-name attempts, directory depth, file bytes, and
activity text are bounded. The network reader publishes at most two 64 KiB
chunks ahead, so cancellation disconnects the installer promptly even if an
underlying HTTPS call remains blocked until its own timeout. Encoding panics
from the dependency are caught at the boundary. Document count, document
bytes, query bytes, result count, token length, batch size, vector dimensions,
serialized bytes, and finite float values are validated.

The worker builds embeddings automatically after initial catalog admission and
again only after a refreshed database has been published. It skips work when
every bounded semantic document already has a matching model id and source
fingerprint. Automatic encoding checks cancellation between 32-document
batches and publishes only if both the request generation and source database
remain current. `quirl ai index` remains an explicit diagnostic/refresh tool;
it is not required for normal setup. `quirl ai search`, `quirl ai related`, and
interactive natural mode read the same database. Missing model files or
embeddings select deterministic lexical ranking. Natural mode and AI
subcommands only display suggestions and never execute one.

## Failure model and invariants

- A failed discovery or embedding build cannot replace the last complete
  database; transaction rollback and atomic replacement preserve it. When no
  complete database exists, failed discovery publishes an indexable builtin
  SQLite fallback so automatic embedding construction still has a source.
- Catalog and normalized SQL rows describe one generation because they commit
  together.
- Discovery state cannot claim freshness for different catalog bytes because it
  records and revalidates the canonical catalog fingerprint.
- Stale embeddings are ignored because each row carries the source document
  fingerprint and model id.
- Download, hashing, model loading, and SQLite work never run on render or
  per-keystroke threads. The UI polls only generation-numbered immutable cache
  snapshots and rejects stale generations.
- Partial, short, oversized, or hash-mismatched assets remain staging files and
  never become model input. Failure preserves lexical search and the last
  complete database.
- Initial indexing starts only after catalog admission. Refresh indexing is
  requested only after refreshed database publication; database publication is
  serialized and an embedding result revalidates its exact source bytes.
- Background work never owns terminal state. Cancellation is visible between
  download chunks and embedding batches; terminal restoration precedes the
  bounded worker join.
- No SQLite connection or model is shared across threads. Each bounded
  operation owns its connection and model lifetime.
- No AI output crosses into the execution planner without a separate explicit
  user action.

## Consequences

- The CLI binary gains bundled SQLite and local Model2Vec dependencies and
  corresponding compile-time/binary-size cost.
- A default database can answer structured command and option queries without
  reparsing completion sources or man text.
- Embedding generation and model installation are automatic background
  control-plane work, never prompt latency.
- Semantic ranking degrades predictably to lexical ranking during setup and
  after any failed download or rebuild.
