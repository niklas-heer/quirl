# ADR 0025: Fine-tuned command retrieval model

- Status: Accepted
- Date: 2026-08-18
- Extends: [ADR 0021](0021-sqlite-local-command-intelligence.md)

## Context

ADR 0021 selected the general-purpose 30.9 MB float32
`minishlab/potion-base-8M` model as Quirl's first automatic semantic retriever.
The first safe natural-command slice proved that bounded local hybrid retrieval
can feed a deny-unknown catalog proposal and trusted confirmation path. The
stock model is not command-specialized, however, and cold loading dominates
query latency.

A first token-weight adaptation improved a 13-query development fixture but
used query-level NL2Bash splits and regressed its four-query unseen slice. A
second whole-utility version improved semantic retrieval but failed the
production hybrid Recall@5/MRR gate. Neither is eligible as an automatic
default. The final experiment therefore uses a fresh stratified whole-utility
split and selects exactly one of six mild anchored adaptations plus a quantized
stock control through production hybrid validation.

## Decision

Quirl pins `niklas-heer/quirl-command-v3-int8` revision
`quirl-command-v3-9bc5efbd14096b54` as its automatic local model. The model
freezes the source tokenizer and 256-dimensional token vectors, tunes one
bounded scalar token-weight delta, anchors tuned query/document geometry to the
source model, and exports the selected checkpoint with global int8
quantization. Training is CPU-only and deterministic. Its config binds the
source assets, pinned NL2Bash revision, complete training pipeline and lockfile,
whole-utility splits, hyperparameters, and checkpoint.

The v3 protocol reserves six represented product utilities plus disjoint
auxiliary utilities for validation and eight represented product utilities
plus disjoint auxiliary utilities for test. `rm`, `rmdir`, and `dig` are forced
test utilities. Candidate selection uses only the 51-query product validation
fixture. The 61-query product test fixture is evaluated once after selection.

Against stock float32 on that final test, the selected model changes Recall@1
from 34.4% to 42.6%, Recall@5 from 65.6% to 70.5%, MRR from 0.490 to 0.543,
cold p50 latency from 389.3 ms to 118.6 ms, peak child RSS from 143.4 MB to
119.4 MB, and model-tree bytes from 30.9 MB to 8.1 MB. Destructive Recall@5
remains 61.5%; multilingual Recall@5/10 remains 100%. Recall@10 changes from
82.0% to 78.7%, a two-query regression concentrated in deep results for `head`
and `mv`.

The Recall@10 result is an explicit exception to the initial v3 promotion
sketch, not an omitted metric. Quirl's retrieval-only natural-command fallback
selects rank 1, then revalidates and requires confirmation; it does not execute
one of ten candidates. Rank-1, top-five, reciprocal-rank, latency, memory, and
model-size improvements therefore justify promotion, while the R@10 regression
remains a recorded limitation and future regression target.

Model assets are checked into `models/` and served from the repository's raw
`main` URL. They are not embedded in the executable because that would violate
the independent 10 MiB release-binary ceiling. The automatic installer retains
ADR 0021's bounded HTTPS, exact byte/hash verification, private staging,
quarantine, atomic rename, cancellation, and lexical-fallback behavior. A
mutable branch URL cannot change admitted bytes because all three files are
cryptographically pinned in Rust. Explicit `QUIRL_MODEL_PATH` directories are
never replaced.

## Consequences

- First-run setup downloads about 8.1 MB instead of 30.9 MB, and inference
  remains local, CPU-only, bounded, and network-free after installation.
- Existing stock model directories remain untouched. The new default uses a
  distinct `quirl-command-v3-int8` local directory and triggers one new index
  generation because model identities cannot mix.
- Repository history grows by about 7.8 MB for the auditable model assets. The
  release executable does not include those bytes.
- The trained model remains retrieval-only. Arbitrary shell text is still
  impossible at the planner boundary, and every generated command still
  requires exact preview and acceptance, with separate high-risk confirmation.
- English training data and the small multilingual diagnostic slice remain
  limitations. Future promotion must use a new versioned split or external
  corpus rather than reselecting on the consumed v3 test fixture.
