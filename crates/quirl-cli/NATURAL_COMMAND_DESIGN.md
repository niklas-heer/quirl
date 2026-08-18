# Natural-command safe vertical slice

This note is the pre-implementation failure model for the first natural-command
slice. ADR 0016 keeps persistence, local inference, catalog composition, and
user-facing policy in `quirl-cli`; catalog records remain the executable source
of truth, and the existing process layer remains the sole native executor.

## Failure model

- The query, catalog cache, discovered metadata, model directory, stored
  embeddings, planner response, terminal input, and accepted argument values are
  untrusted. Any can be malformed, stale, mismatched, oversized, replaced while
  being read, cancelled, or crafted to consume CPU or memory.
- Lexical or semantic ranking can be wrong. A rank is never authority to execute.
  The retrieval-only planner may select only a current catalog command ID and
  validated argument slots; it cannot return command text.
- Catalog contents can change between retrieval, proposal construction,
  preview, confirmation, and execution. Every transition re-resolves the stable
  command ID and revalidates arguments against the current catalog.
- Model loading or encoding can fail, panic inside the dependency, exceed its
  deadline, observe cancellation, or disagree with stored embeddings. These are
  bounded operating failures. Search falls back to complete lexical ranking and
  never treats partial semantic results as current.
- A generated command can read or modify user state, start a process, signal a
  process, transfer data, or exercise authority not represented precisely in an
  incomplete external catalog. Every proposal therefore needs ordinary explicit
  acceptance, and only an explicitly read-only effect set stays ordinary;
  unknown, write, process, or session-changing effects conservatively need a
  separate high-risk confirmation.
- Terminal confirmation can be interrupted or encounter I/O failure. No
  execution begins until confirmation completes; the existing terminal and
  child-process RAII paths own cleanup after execution begins.
- Evaluation fixtures can leak targets across command groups or copy benchmark
  queries into indexed documents. The evaluator rejects overlapping groups and
  version mismatches; derived documents use catalog facts only.

## Resource sketch

| Resource | Initial bound | Ownership and enforcement |
| --- | ---: | --- |
| Query bytes | 4 KiB | Before tokenization, retrieval, or planning. |
| Query terms/model tokens | 64 lexical / 256 model | During deterministic token admission. |
| Semantic documents | 65,536 | SQLite generation and session admission. |
| One document / retained document text | 16 KiB / 32 MiB | Document generation and session admission. |
| Embedding dimensions / retained vector bytes | 2,048 / 128 MiB | Model and SQLite admission. |
| Lexical and semantic candidate pools | 65,536 each | Admitted document ceiling before reciprocal-rank fusion. |
| Fused/final candidates | 65,536 / 100 | Fusion map and public result validation. |
| Proposal arguments / catalog arguments | 256 / 1,024 | Planner and current-catalog admission. |
| One proposal value / all proposal values | 16 KiB / 64 KiB | Proposal validation before preview. |
| Preview bytes | 16 KiB | Trusted renderer before terminal output. |
| Retrieval deadline | 750 ms | Checked before and after bounded local encoding and ranking. |
| Worker queue | one replaceable query generation | Existing latest-generation completion worker. |
| Model files | three files, 69 MiB aggregate maximum | Path/handle admission before loading. |
| Database image | 128 MiB | Existing hardened SQLite admission/publication. |
| Evaluation queries / retained fixture bytes | 4,096 / 8 MiB | Versioned fixture admission. |

Expected interactive work is one preloaded model query plus two linear scans of
the admitted document count and bounded sorting of candidate pools. Catalog
documents and their embeddings are generated once per catalog/model identity,
never per keystroke. Model memory is expected to remain near the current
potion-base-8M footprint (about 30 MiB for the automatic default or about 8 MiB
for the experimental int8 files), plus bounded document and vector storage.

## Invariants

1. Canonical `Catalog` records are never overwritten by derived summaries,
   intent phrases, embeddings, evaluation queries, or planner output. Every
   derived field records its generation version and source provenance.
2. One search generation uses exactly one model identity and one document/index
   fingerprint. Repository, revision, file hashes, dimensions, and document
   generation version all participate in that identity. Mismatch means lexical
   fallback, never mixed vectors.
3. Lexical ranking is always computed from the complete admitted document set.
   Semantic absence, staleness, invalidity, cancellation, panic, or timeout
   cannot remove lexical candidates.
4. Reciprocal-rank fusion and exact-name/path/alias/option/type boosts are
   deterministic. Equal scores use stable catalog/document identities.
5. `CommandProposal` is deny-unknown and versioned. It contains a catalog
   command ID, typed values, unresolved slots, explanation, and provenance; it
   contains no shell source or executable path supplied by a model.
6. Trusted Rust code resolves the current catalog record, validates slot names,
   kinds, types, cardinality, static values, conflicts, and required arguments,
   then renders an exact quoted command. Preview bytes are exactly the bytes
   later submitted to the existing parser/execution path.
7. No proposal executes automatically. Ordinary acceptance is distinct from
   proposal selection; high-risk confirmation is an additional state transition
   and cannot be satisfied by the ordinary acceptance event.
8. Cancellation and every error before execution leave terminal state and the
   existing catalog/model/database generation unchanged. Once execution starts,
   existing process containment, recovery, and terminal cleanup remain the only
   lifecycle owner.
9. The evaluation corpus is not an indexing source. Command-group train,
   validation, and test partitions are disjoint, and reports bind schema,
   document generation, catalog, model, index, fixture, and executable
   identities to Recall@1/5/10, MRR, required slices, latency, RSS, and model
   bytes.
