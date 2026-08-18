# Quirl local retrieval evaluation

This isolated spike measures the production `quirl ai search` command and can
reproducibly tune the pinned `potion-base-8M` model's token weights. Evaluation
uses only the Python standard library and never trains, downloads, indexes, or
executes a returned command. Training has a separate locked Python 3.12
environment, performs one pinned and hash-verified dataset download, and remains
CPU-only.

The v1 fixture defines disjoint command-group splits. Its original evaluation
queries are not training data, intents, or indexed documents. Before measuring,
the evaluator opens the supplied database read-only and rejects a normalized
query that appears as a substring of any `semantic_documents.title` or
`semantic_documents.body` value.

## Reproduce

From the repository root:

```sh
python3 spikes/retrieval-models/evaluate.py --self-test
cargo build -p quirl-cli
QUIRL_FISH_PATH= QUIRL_BASH_PATH= QUIRL_ZSH_PATH= \
  target/debug/quirl index build \
  --output target/retrieval-evaluation-catalog.sqlite3 \
  --format json
python3 spikes/retrieval-models/evaluate.py \
  --quirl target/debug/quirl \
  --database target/retrieval-evaluation-catalog.sqlite3 \
  --fixture spikes/retrieval-models/fixture-v1.json \
  > target/retrieval-evaluation-v2.json
```

The empty completion-path variables make this reproduction independent of
machine-specific Fish, Bash, and Zsh completion installations. The resulting
database still contains Quirl's compiled builtin command contracts. To measure
a richer reviewed catalog, pass its already-built intelligence database
directly instead.

To evaluate an already-installed local Model2Vec export, add its explicit
directory. Explicit models must include the bounded `quirl-model.json` identity
manifest required by the product:

```sh
python3 spikes/retrieval-models/evaluate.py \
  --quirl target/release/quirl \
  --database /absolute/path/to/catalog.sqlite3 \
  --model /absolute/path/to/potion-base-8M \
  > target/retrieval-evaluation-v2.json
```

To measure the installed pinned automatic model while preserving its automatic
selection semantics, use `--automatic-model` instead of `--model`. The flag does
not download or change the automatic default.

The report is a versioned JSON envelope containing Recall@1/5/10, MRR,
multilingual/destructive/unseen-command and split slices, individual cold
subprocess latency, aggregate latency percentiles, peak child RSS, and SHA-256
identities for the fixture, binary, database, and optional bounded model tree.
Version 2 also records the full loaded-model manifest identity and the stored
embedding generation identity rather than a repository label alone. `quirl ai
status` must also confirm that network loading is disabled.

## Train a command-retrieval candidate

Read [TRAINING_DESIGN.md](TRAINING_DESIGN.md) before changing the training
contract. From this directory, reproduce the v1 token-weight sweep with:

```sh
uv sync --locked
uv run python -m unittest -v test_train.py
uv run python train.py \
  --database ../../target/retrieval-current.sqlite3 \
  --model "$HOME/.local/share/quirl/models/potion-base-8M" \
  --fixture fixture-v1.json \
  --output training-output/current-v1 \
  --epochs 160
```

The output path must not already exist. `train.py` admits only the exact pinned
stock model, holds out the complete `rm`, `rmdir`, and `dig` command groups,
selects a trial only with a deterministic NL2Bash validation split, and consults
the Quirl fixture only after selection. It freezes all token vectors and tunes
one bounded scalar weight per token. Both float32 and int8 exports carry an
explicit `quirl-model.json`; neither replaces Quirl's automatic default.

To use the int8 experiment, build its embeddings once and pass the same explicit
path to subsequent commands:

```sh
export QUIRL_MODEL_PATH="$PWD/training-output/current-v1/selected-int8"
quirl ai index --format json
quirl ai search "find the largest files" --format json
```

The committed [TRAINING_RESULTS.md](TRAINING_RESULTS.md) records every promotion
version's identities, benchmark deltas, and known regressions. Intermediate
models and full reports remain ignored. The selected v3 int8 artifact is the
only model copied into `models/` and pinned as the automatic product default.

Promotion-quality reproduction uses the whole-utility and product-aligned
pipelines after the v1 run:

```sh
uv run python train_v2.py \
  --database ../../target/retrieval-current.sqlite3 \
  --model "$HOME/.local/share/quirl/models/potion-base-8M" \
  --output training-output/v2-promotion \
  --epochs 280
uv run python train_v3.py \
  --database ../../target/retrieval-current.sqlite3 \
  --model "$HOME/.local/share/quirl/models/potion-base-8M" \
  --output training-output/v3-promotion \
  --epochs 180
```

`train_v3.py` exports seven independently identified int8 candidates and
separate generated validation/test fixtures. Run the production evaluator on
validation for every candidate, select exactly one, and only then read the test
fixture. The complete frozen protocol and promotion exception are in
[TRAINING_DESIGN.md](TRAINING_DESIGN.md) and ADR 0025.

## Bounds and interpretation

- fixture: 1 MiB, 16 splits, 256 command groups, 512 queries;
- query: 4 KiB and at least six words;
- results: 10–100 per query;
- stdout and stderr: 512 KiB each per subprocess;
- deadline: at most 300 seconds per subprocess;
- database: 128 MiB and 65,536 semantic documents;
- model: 128 MiB, 64 regular files, depth 8, with no symlinks;
- binary: 256 MiB.

Latency includes one cold Quirl process per query, including database and model
loading. `peak_child_rss_bytes` is the platform-normalized
`resource.getrusage(RUSAGE_CHILDREN).ru_maxrss` high-water mark for all evaluator
children, including metadata probes. The fixture is intentionally small; its
metrics are regression evidence, not a population estimate. A good retrieval
score does not make a result executable or safe.
