# Quirl command retrieval model v3

This directory contains Quirl's pinned automatic local retrieval model. It is
an int8 Model2Vec export derived from `minishlab/potion-base-8M` revision
`bf8b056651a2c21b8d2565580b8569da283cab23`. Training freezes the source token
vectors and tunes bounded token-weight deltas on the pinned MIT-licensed
NL2Bash corpus plus deterministic catalog phrases.

Identity:

- model revision: `quirl-command-v3-9bc5efbd14096b54`;
- dimensions: 256;
- config SHA-256:
  `24d184c2ccaf32274ad9f9be69dab83cb6ab4403b9b0dc2a8f7dc4c03608db8f`;
- tokenizer SHA-256:
  `273ca9e28ec6990aea6206b0364443754d87e87a5dd28e94026ea9999ba3bf62`;
- weights SHA-256:
  `1ea0c56bae3f10dd172f7e4997a7038193c4633fbe0fdeb0528f89d75b801c30`.

`config.json` records the source hashes, training scripts and lockfile hashes,
whole-utility splits, selected hyperparameters, and checkpoint epoch. The
deny-unknown `quirl-model.json` is the explicit-path product manifest. Quirl's
automatic installer independently pins the same byte counts and hashes in Rust.

The model only ranks catalog documents. It cannot produce shell text or bypass
catalog proposal validation, exact trusted rendering, preview, user acceptance,
or high-risk confirmation.

See
[`spikes/retrieval-models/TRAINING_RESULTS.md`](../../../spikes/retrieval-models/TRAINING_RESULTS.md)
and [ADR 0025](../../../docs/decisions/0025-fine-tuned-command-retrieval-model.md)
for methodology, measurements, and limitations.
