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

## Version 2 promotion protocol

Version 1 proved that sparse token-weight adaptation can help, but it is not a
promotion-quality experiment. Its NL2Bash validation and test partitions shared
command identities, and its training objective used only the root command
document while production retrieval ranks command and option documents. The v1
Quirl fixture has also been observed and is henceforth diagnostic rather than a
model-selection signal.

### Additional failure model

- Query-level splitting can memorize utility-specific vocabulary and overstate
  generalization. Version 2 assigns entire utilities to exactly one of train,
  validation, or test. `rm`, `rmdir`, and `dig` remain forced test utilities.
- Training against a synthetic representation can improve an internal score
  while regressing the production hybrid ranker. Version 2 samples positives
  from the same bounded command and option documents that production embeds,
  and runs the product evaluator against a generated, ignored holdout fixture.
- Aggressive token weights can distort unrelated and unseen commands. Every
  trial includes a fixed source-geometry anchor, bounds each weight delta, and
  is selected on whole-utility validation Recall@10, then MRR, then Recall@1.
- Looking at several test candidates turns the test set into validation. Only
  the selected validation winner is scored on the whole-utility test set and
  exported. A failed promotion gate ends the version; it does not trigger
  another v2 hyperparameter choice.
- Repository model assets can bloat the executable or drift from runtime pins.
  The int8 files are checked in as separately hashed data, never embedded in the
  Rust binary, and automatic installation verifies exact sizes and SHA-256
  values before atomically publishing them to the private local model directory.

### Additional resource sketch

| Resource | Version 2 bound |
| --- | ---: |
| NL2Bash utilities | 256 |
| Examples per utility | 96 |
| Documents per command | 256 |
| Retained training text | 48 MiB |
| Hyperparameter trials | 6 |
| Epochs per trial | 320 |
| Geometry-anchor candidates per batch | 192 |
| Checked-in automatic model assets | 16 MiB aggregate maximum |
| Generated product holdout fixture | 256 queries, ignored |

The frozen 29 MiB token-vector table remains the dominant training allocation.
Dense scalar token weights, their optimizer state, bounded query/document
batches, and source anchor vectors keep the expected peak below 1.5 GiB.

### Additional invariants

1. A utility appears in exactly one query split. Test query text is not read by
   an optimizer or trial selector, and only one selected model is test-scored.
2. Production catalog documents may be known for every utility, as they are at
   inference, but no validation/test query or derived paraphrase is a training
   example.
3. Auxiliary documents contain only a normalized utility name and validated
   option spellings. Raw shell programs, operands, URLs, paths, substitutions,
   and benchmark queries never enter indexed product documents or model output.
4. Trial selection uses only the whole-utility validation split. The NL2Bash
   test split and v1 Quirl fixture are reported after selection and never alter
   the selected configuration.
5. The promoted artifact must beat stock overall Recall@5, Recall@10, and MRR;
   must not regress destructive or unseen Recall@5; and must remain within the
   model, latency, RSS, and release-binary bounds. Otherwise the stock automatic
   default remains pinned.
6. The repository asset hashes, Rust automatic-model constants, explicit model
   manifest, generated training report, and post-install `ai status` identity
   must all agree exactly.

The bounded 40-epoch pipeline preflight used split seed 20260818 and therefore
consumed that seed's test partition. The promotion run advances exactly once to
seed 20260819 before training, retains the already-declared trials unchanged,
and does not permit another split or hyperparameter iteration from its result.

## Version 3 product-aligned calibration

The frozen v2 promotion candidate passed its 225-query semantic holdout but
failed the production hybrid gate: Recall@10 improved while Recall@5 and MRR
regressed. Version 2 is therefore permanently rejected. Version 3 uses the next
fixed seed, 20260820, and changes one design dimension before any v3 metrics are
observed: model selection happens through the production hybrid evaluator on a
validation-only product fixture rather than through semantic similarity alone.

The split is stratified by whether a utility has a production catalog command.
Eight represented product utilities (including `rm`, `rmdir`, and `dig`) and a
bounded auxiliary share are test-only; six other represented product utilities
and a disjoint auxiliary share are validation-only. All remaining utilities and
catalog-only commands may train. Trial candidates use the already reviewed
anchored objective at deliberately milder learning rates, plus a quantized stock
control. Every candidate receives a distinct hashed manifest and embedding
database. The product evaluator selects on validation Recall@5, Recall@10, and
MRR without reading the generated test fixture. Only that winner is then run
once against the v3 product test fixture.

The v3 test result is final for this training effort. Promotion requires all of
the following against the stock float automatic model on the same fixture:

1. overall Recall@5 and Recall@10 do not regress, and at least one improves;
2. overall MRR does not regress by more than 0.01;
3. destructive and unseen Recall@5 do not regress;
4. multilingual Recall@10 does not regress;
5. cold p50 latency, peak RSS, and model bytes improve;
6. the release binary remains below its independent 10 MiB hard ceiling.

If v3 fails, no tuned model is promoted in this change. The automatic stock
model remains safer than selecting again on consumed test evidence.
