# Quirl command-retrieval tuning results

This report records the first leakage-safe local token-weight adaptation of
`minishlab/potion-base-8M`. The run used seed 42, CPU only, 721 training phrases
across 125 commands, 66 validation queries, and 66 held-out NL2Bash test
queries. The complete `rm`, `rmdir`, and `dig` groups were excluded. The 13
Quirl fixture queries were checked for exact normalized leakage and used only
after the learning-rate trial was selected. Training and product measurements
ran on 2026-08-18 on a 12-core Apple M2 Pro with 32 GiB RAM and macOS 15.7.9.

## Candidate identity

- Revision: `quirl-command-v1-01519636c9aac9c7`
- Selected trial: sparse token weights, learning rate 0.03, epoch 130
- Source revision: `bf8b056651a2c21b8d2565580b8569da283cab23`
- Source weights SHA-256:
  `f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2`
- int8 weights SHA-256:
  `59467ed9e687a9d1f08ef48960508eccc2b0fa360720548f47541fc820b1e9cf`
- Product model identity:
  `sha256:33c542e331a95b1b977dbb38e6cb1806bd1817820a04ac9695fe3e5905912c58`
- int8 admitted asset bytes: 8,125,836; evaluator model-tree bytes: 8,130,351

The export embeds the pinned NL2Bash revision and hashes, stock-model identity,
catalog database hash, fixture's evaluation-only role, training script hash,
locked dependency hash, held-out commands, selected trial, and epoch. Its
deny-unknown `quirl-model.json` separately binds repository, revision,
dimensions, and the three product-loaded asset hashes.

## Semantic-only held-out test

| Metric | Stock | Tuned float32 | Delta |
| --- | ---: | ---: | ---: |
| Recall@1 | 15.2% | 78.8% | +63.6 pp |
| Recall@5 | 42.4% | 93.9% | +51.5 pp |
| Recall@10 | 54.5% | 98.5% | +44.0 pp |
| MRR | 0.276 | 0.861 | +0.585 |

These 66 test queries were not used for trial selection. The separately held
validation split moved from 53.0% to 98.5% Recall@10 and from 0.277 to 0.857
MRR.

## Production hybrid retrieval

The production evaluator rebuilt all 2,227 document embeddings for each model
and ran the unchanged bounded reciprocal-rank fusion path in one cold Quirl
process per query.

| Metric | Stock float32 | Tuned int8 | Delta |
| --- | ---: | ---: | ---: |
| Recall@1 | 7.7% | 15.4% | +7.7 pp |
| Recall@5 | 30.8% | 53.8% | +23.0 pp |
| Recall@10 | 46.2% | 53.8% | +7.6 pp |
| MRR | 0.161 | 0.272 | +0.111 |
| Cold latency p50 | 383.9 ms | 119.7 ms | -264.2 ms |
| Cold latency p95 | 389.7 ms | 123.8 ms | -265.9 ms |
| Peak child RSS | 137.3 MB | 120.4 MB | -16.9 MB |
| Model-tree bytes | 30.9 MB | 8.1 MB | -22.8 MB |

The held-out split improved from 0% to 50% Recall@10. Destructive Recall@5
improved from 25% to 50%, while destructive Recall@10 regressed from 75% to
50%. Multilingual Recall@10 remained 33.3%. The four-query unseen-command slice
kept 50% Recall@5 but regressed from 100% to 50% Recall@10; its MRR was nearly
flat (0.211 to 0.208). These small-slice regressions are why this candidate is
installed only through an explicit local path and does not replace the
automatic default.

## Remaining limitations

- The Quirl fixture has only 13 queries, so slice percentages have high
  variance and are regression evidence rather than a population estimate.
- Training data is English. German and Spanish gains will require separately
  licensed, command-group-separated data and a multilingual source model or
  distillation strategy.
- Only token weights are tuned. The tokenizer and 256-dimensional token vectors
  stay frozen, which controls overfitting and export size but limits adaptation.
- Product evaluation compares the tuned int8 artifact with the stock float32
  automatic model; latency and size deltas therefore include quantization as
  well as tuning. A tuned float32 check produced the same aggregate Recall
  values and MRR 0.285, at 232.9 ms p50.
- The artifact remains retrieval-only. Catalog proposal validation, trusted
  command rendering, exact preview, ordinary acceptance, and distinct high-risk
  confirmation remain mandatory.
