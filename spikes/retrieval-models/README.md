# Quirl local retrieval evaluation

This isolated spike measures the production `quirl ai search` command without
training, downloading, indexing, or executing a returned command. It uses only
the Python standard library and requires explicit local paths for the Quirl
binary and SQLite command database. Supplying `--model` is optional; omitting it
forces the product's deterministic lexical fallback instead of consulting an
ambient model directory.

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
  > target/retrieval-evaluation-v1.json
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
  > target/retrieval-evaluation-v1.json
```

To measure the installed pinned automatic model while preserving its automatic
selection semantics, use `--automatic-model` instead of `--model`. The flag does
not download or change the automatic default.

The report is a versioned JSON envelope containing Recall@1/5/10, MRR,
multilingual/destructive/unseen-command and split slices, individual cold
subprocess latency, aggregate latency percentiles, peak child RSS, and SHA-256
identities for the fixture, binary, database, and optional bounded model tree.
`quirl ai status` must also confirm that network loading is disabled.

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
