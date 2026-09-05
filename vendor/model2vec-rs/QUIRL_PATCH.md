# Quirl patch of model2vec-rs 0.2.1

The upstream license is retained in `LICENSE`. `UPSTREAM.json` records the
registry archive checksum and original hashes of retained upstream files.
The workspace selects this source through `[patch.crates-io]`; the distribution
inventory identifies its vendored origin.
Trailing whitespace is removed from the upstream README and CLI documentation.

## Failure model and invariants

Model loading formerly serialized the complete tokenizer into a JSON value just
to inspect `model.unk_token`. That duplicated vocabulary/processor data in memory
and retained their serialization implementations in the release binary. Quirl
already bounds and authenticates model files before this dependency is called;
this patch removes avoidable work without narrowing accepted tokenizer models.

Read the same field through the existing typed model API: WordPiece and
WordLevel expose the string, BPE exposes an optional string, and Unigram has no
`unk_token` field (its `unk_id` is intentionally not substituted). Empty strings
remain real tokens. A named token absent from the vocabulary still returns the
same error. Vocabulary median computation, normalization, pooling, tokenization,
and inference remain unchanged. This lookup borrows one field and allocates no
JSON tree; existing vocabulary-length collection remains the resource owner.

No dependencies, features, unsafe code, or public APIs are added. The focused
in-file test compares the old serialization algorithm with the typed lookup for
all four models, including absent/empty/missing unknown tokens and empty
vocabularies. The reference algorithm is test-only; run it from an isolated copy
of this package using `cargo test --lib --no-default-features --features local-only,onig`.
Cargo does not run nonmember dependency dev-tests from the parent workspace.

Consumer tests in `quirl-cli` load all four tokenizer variants through
`StaticModel::from_bytes`, verify exact embeddings and unknown-token filtering,
and exercise absent/empty/missing unknown tokens and empty vocabularies. These
run in the canonical workspace gate. For focused validation:

```console
cargo test -p quirl-cli model_metadata
```

Remove this patch when upstream provides equivalent direct metadata access,
after rerunning model identity/inference tests and measuring the release binary.
