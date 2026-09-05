# Quirl's Crossterm 0.29 input patch

See [ADR 0031](../../docs/decisions/0031-bounded-terminal-input.md) for the
failure model, bounds, selected feature path, and removal criteria.

`UPSTREAM.json` records the crates.io archive checksum and original individual
source hashes. `Cargo.toml.orig` and `LICENSE` are unchanged upstream files.
The normalized manifest omits upstream examples and dev dependencies; no
runtime dependency was added. Original Windows source is retained unchanged.

Intentional runtime deviations:

- `event/source/unix/input_buffer.rs`: byte/deadline admission and queue limits,
  with in-file, standard-library-only tests.
- `event/source/unix.rs`: the private admission module.
- `event/source/unix/tty.rs`: bounded pending input, caller poll deadlines,
  one readiness-authorized TTY read per turn, bounded signal draining, and
  already-queued input flush on limit rejection. Zero-duration polls still
  inspect the kernel and already parsed events once.
- `event/read.rs`: one owning filtered-event queue, count/byte admission, and
  sticky typed failure that discards pending input. The original reader methods
  remain selected on platforms/features other than Unix `use-dev-tty`.
- `event/source.rs`: a private default cleanup hook; the selected TTY source
  clears parser events and flushes kernel input after admission failure.
- `event.rs`: typed admission-error classification for shell consumers.
- `terminal/sys/unix.rs`: remove redundant parentheses reported by the pinned
  compiler when the registry warning cap is no longer applied.

No new unsafe block is introduced. Formatting changes in patched files follow
Rustfmt. The canonical Quirl gate executes admission unit tests and real PTY
integration tests; it does not claim to run every upstream platform/example
test.

`event/quirl_read_tests.rs` is a standard-library-only platform-fixture harness:
it includes the actual reader, filters, timeout and admission code with inert
event/source contracts. Its tests prove ordering, exact admission, sticky errors
and discard-hook invocation, while PTY tests prove real fd/parser integration.
The canonical gate builds this harness with `use-dev-tty` and `bracketed-paste`.

The canonical `zero_poll` PTY helper additionally tests xtask's linked Crossterm
with two explicitly queued bytes and a private gate. It verifies kernel input,
already parsed input and idle zero-duration polling, separately from the pinned
Quirl CLI check count. The real CLI input-limit check includes one 30-second
idle unterminated paste; no subsequent byte is sent to wake that deadline.
