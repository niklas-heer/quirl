# Natural-command safe vertical slice

This note is the pre-implementation failure model for the first natural-command
slice. ADR 0016 keeps persistence, local inference, catalog composition, and
user-facing policy in `quirl-cli`; catalog records remain the executable source
of truth, and the existing process layer remains the sole native executor.

## Failure model

- The query, catalog cache, discovered metadata, planner response, terminal
  input, and accepted argument values are
  untrusted. Any can be malformed, stale, mismatched, oversized, replaced while
  being read, cancelled, or crafted to consume CPU or memory.
- Codex may select the wrong command or arguments from the compact complete
  catalog. Noninteractive planning may return only a current catalog command ID
  and typed arguments. The rich conversational path may instead return one
  bounded editor submission containing native command source or a `lua` chunk;
  Quirl parses or syntax-checks it before display and never executes it on
  acceptance.
- The Codex executable, inherited authentication state, long-lived app-server,
  protocol messages, model response, and diagnostics can be absent, stale,
  malicious, oversized, or slow. Interactive planning uses one ephemeral
  conversation thread per open AI session over one contained connection. Tool features are disabled, read-only
  and never-approve policies are explicit, and any tool lifecycle item is
  rejected. Protocol lines, events, queues, input, temporary files, cancellation,
  process cleanup, and wall time are bounded. Failure has no local-planner
  fallback and never grants execution authority.
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
| Codex catalog commands / arguments | 8,192 / 65,536 | Compact complete-catalog projection before serialization. |
| Codex catalog request / protocol line | 1 MiB / 2 MiB | Bounded serialization before launch or JSONL transmission. |
| Codex protocol turn / event count | 8 MiB / 4,096 | Fresh response and notification admission for each request or turn. |
| Codex conversation turns / user bytes | 16 / 128 KiB | Before appending another turn to the ephemeral AI session. |
| Codex prepare-and-plan / update queues | 2 / 32 | Bounded synchronous channels between editor, worker, and protocol reader. |
| Codex wall deadline | 90 s | Polling supervisor terminates and reaps the complete process tree. |
| Proposal arguments / catalog arguments | 256 / 1,024 | Noninteractive planner and current-catalog admission. |
| One proposal value / rich source | 16 KiB / 64 KiB | Typed-value validation or editor-source admission before preview. |
| Preview bytes | 16 KiB | Trusted renderer before terminal output. |
| Retrieval deadline | 750 ms | Checked before and after bounded local encoding and ranking. |
| Worker queue | one replaceable query generation | Existing latest-generation completion worker. |
| Model files | three files, 69 MiB aggregate maximum | Path/handle admission before loading. |
| Database image | 128 MiB | Existing hardened SQLite admission/publication. |
| Evaluation queries / retained fixture bytes | 4,096 / 8 MiB | Versioned fixture admission. |

Expected planning work is one linear compact projection of the admitted
catalog, one bounded serialization, and one Codex turn. The rich session pays
app-server initialization and model discovery once in a background worker that
starts with the first rich frame, prefers advertised Luna access at high effort,
and creates one ephemeral thread for the open AI session. Other models use
their advertised default effort. The first turn sends the complete catalog,
while follow-up turns send only the new message and reuse the thread's bounded
history. AI mode does not load the local command
model or rank a local candidate pool.

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
5. Noninteractive `CommandProposal` remains deny-unknown and versioned, with a
   catalog command ID, typed values, explanation, and provenance rather than
   model-supplied source. Rich AI mode uses a separate deny-unknown response
   containing an outcome, a short message, and at most 64 KiB of source.
6. A rich proposal may compose admitted commands with native pipes, lists, and
   redirects, or use the explicit `lua` bridge. It must satisfy the requested
   final postcondition rather than substitute related intermediate output.
7. Trusted Rust validates noninteractive catalog records and typed arguments as
   before. For rich proposals it rejects control characters and editor actions,
   parses native command graphs, and syntax-checks Lua without running it.
8. No proposal executes automatically. Tab or empty Enter transfers rich source
   into the normal editor; execution requires a later, separate Enter after the
   user can inspect or edit it. Noninteractive high-risk confirmation remains a
   separate state transition.
9. Cancellation and every error before execution leave terminal state and the
   existing catalog/model/database generation unchanged. Once execution starts,
   existing process containment, recovery, and terminal cleanup remain the only
   lifecycle owner.
10. The evaluation corpus is not an indexing source. Command-group train,
   validation, and test partitions are disjoint, and reports bind schema,
   document generation, catalog, model, index, fixture, and executable
   identities to Recall@1/5/10, MRR, required slices, latency, RSS, and model
   bytes.
