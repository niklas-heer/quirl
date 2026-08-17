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
inputs; all writes use SQLite.

Semantic inference uses `model2vec-rs` with only its `local-only` and `onig`
features and the `minishlab/potion-base-8M` model. The editor never downloads a
model. Three admitted regular files have explicit limits: 1 MiB configuration,
4 MiB tokenizer, and 64 MiB weights. Encoding panics from the dependency are
caught at the boundary. Document count, document bytes, query bytes, result
count, token length, batch size, vector dimensions, serialized bytes, and
finite float values are validated.

`quirl ai index` is the explicit control-plane operation that builds
embeddings. `quirl ai search`, `quirl ai related`, and interactive natural mode
read the same database. Missing model files or embeddings select deterministic
lexical ranking; malformed installed model files are operating errors. Natural
mode and AI subcommands only display suggestions and never execute one.

## Failure model and invariants

- A failed discovery or embedding build cannot replace the last complete
  database; transaction rollback and atomic replacement preserve it.
- Catalog and normalized SQL rows describe one generation because they commit
  together.
- Discovery state cannot claim freshness for different catalog bytes because it
  records and revalidates the canonical catalog fingerprint.
- Stale embeddings are ignored because each row carries the source document
  fingerprint and model id.
- No SQLite connection or model is shared across threads. Each bounded
  operation owns its connection and model lifetime.
- No AI output crosses into the execution planner without a separate explicit
  user action.

## Consequences

- The CLI binary gains bundled SQLite and local Model2Vec dependencies and
  corresponding compile-time/binary-size cost.
- A default database can answer structured command and option queries without
  reparsing completion sources or man text.
- Embedding generation is explicit control-plane work, not prompt latency.
- Semantic ranking degrades predictably to lexical ranking when optional model
  assets are absent.
