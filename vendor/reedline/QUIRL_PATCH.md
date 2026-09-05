# Quirl patch of Reedline 0.49.0

The upstream MIT license is retained in `LICENSE`. `UPSTREAM.json` records the
registry archive checksum and original hashes of retained upstream files.
The workspace selects this source through `[patch.crates-io]`; the release
inventory identifies its vendored origin.

Quirl owns these terminal/editor changes:

- Admit editable source at 64 KiB before mutation and roll back failed edits.
- Retain at most 128 undo states and 8 MiB of source text.
- Escape source-origin terminal controls after splitting at raw cursor positions.
- Bound Vi prefixes, count arithmetic, repeated actions, and raw input batches.
- Apply parsed events in sequence so later mode changes cannot corrupt earlier
  insertions from the same batch.
- Return typed input-limit errors through the edit-mode and engine boundaries.

See [ADR 0033](../../docs/decisions/0033-bounded-simple-editor.md) for the failure
model, limits, cleanup contract, and upstream removal criteria. No new runtime
dependencies or unsafe code are introduced. Consumer tests in `quirl-ui` exercise
public buffer, editor, renderer, and edit-mode contracts. Real macOS/Linux PTY
checks exercise paste, Unicode editing, Vi transitions, resource rejection, and
terminal restoration through the product. These checks run in `cargo xtask check`;
they do not claim coverage of every optional upstream integration or Windows.
