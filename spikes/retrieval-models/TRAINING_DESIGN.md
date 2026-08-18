# Retrieval-model tuning design

This research pipeline tunes token importance in the existing
`potion-base-8M` static embedding model. It does not train a command generator,
change Quirl's automatic model, or grant model output execution authority.

## Failure model

- The source model, catalog database, external dataset, evaluation fixture, and
  generated model files can be missing, malformed, oversized, stale, replaced,
  or mutually inconsistent. Every input is admitted through a fixed file,
  record, text, and dimension bound before training.
- Training can overfit represented utilities, leak evaluation queries, produce
  non-finite weights, select on the test set, or regress unseen-command and
  destructive slices. The official Quirl fixture is evaluation-only. Trial
  selection uses a deterministic validation partition of a separately pinned
  dataset, and entire configured utility groups are absent from training.
- Sparse optimization can diverge or create an incompatible static model.
  Gradients and log weights are clipped, checkpoints require finite validation
  scores, and every export is reloaded before publication. Quirl independently
  validates its deny-unknown identity manifest and file hashes.
- Dataset or dependency downloads can drift or hang. Dataset URLs pin one
  upstream revision and SHA-256 digest; dependency versions and the Python
  version are locked. Downloads have byte and wall-time bounds. Quirl inference
  remains local and network-free regardless of this offline research step.
- A better embedding rank can still choose the wrong command. The tuned model
  remains retrieval-only and cannot bypass proposal validation, exact preview,
  ordinary acceptance, or the distinct high-risk confirmation.

## Resource sketch

| Resource | Bound |
| --- | ---: |
| Catalog database | 128 MiB |
| Command documents | 4,096 |
| One document / query | 16 KiB / 4 KiB |
| Retained document and query text | 32 MiB |
| NL2Bash input | 20,000 aligned pairs; 2 MiB per file |
| Training examples | 64 per command; 256 commands |
| Model vocabulary / dimensions | 65,536 / 2,048 |
| Model files | 69 MiB aggregate |
| Query tokens | 256 |
| Hard negatives | 4 per command |
| Trials / epochs | 3 / 200 maximum |
| Training batch | 32 command groups |
| Training device | local CPU only |

The stock 29 MiB float model retains one frozen token-vector table. Training
adds one sparse scalar log-weight per token plus bounded batches, candidate
vectors, and optimizer state. Expected peak memory is below 1 GiB on the
current 32 GiB development host.

## Invariants

1. Evaluation fixture text is never a training example, catalog document, hard
   negative, or selection signal. An exact normalized-substring scan proves the
   boundary before optimization.
2. `rm`, `rmdir`, and `dig` are whole-command holdouts. No query paired with
   those utilities enters training or validation.
3. Dataset splitting, balancing, negative mining, trial ordering, and tie
   breaking are deterministic for the recorded seed and identities.
4. Only token weights are trainable in the first production candidate. The
   pretrained token vectors and tokenizer remain unchanged.
5. Trial selection uses validation Recall@10 and then MRR. Quirl fixture and
   separately retained dataset test results are reported only after selection.
6. Exported files carry repository, revision, source hashes, dimensions,
   training schema, dataset identities, and the training-script/environment
   lock identity. File hashes are recomputed after the final bytes are
   installed.
7. A trained model is admitted only through `QUIRL_MODEL_PATH`; it does not
   replace the automatic default. Embeddings from different identities are
   never mixed.
8. A ranking improvement never authorizes execution. The complete
   `CommandProposal` validation and confirmation path remains unchanged.
