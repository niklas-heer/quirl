+++
schema_version = 1
id = "01M43VRB9PNBGEJHG0FF0M67PY"
title = "Rust 1.99 compatibility and selective adoption"
date = "2026-10-04"
status = "accepted"
tags = ["rust"]
supersedes = ["01M2XHZ70RA4C5EHWFZ8JHTZ1D"]
superseded_by = []
depends_on = []
related_to = []
+++
- Baseline: Rust 1.99.0, Edition 2024, commit
  `b940084d7` (2026-09-28)

## Context

This audit continues the [Rust 1.97 record](2026-08-18_192326104_rust-1-97-compatibility-and-selective-adoption.md), which pinned
1.97.1 and adopted only changes that made a bound, parser transition,
diagnostic, or invariant clearer. Its adopted decisions (`array_windows::<2>`
for two-token parser transitions, `assert_matches!` in tests, and the
`dead_code_pub_in_binary` deny) remain in force. The audit covers Rust,
Cargo, and Clippy 1.98.0, 1.98.1, and 1.99.0 using the official
[Rust release notes](https://doc.rust-lang.org/stable/releases.html).

## Release audit

| Release | Quirl-relevant finding | Disposition |
| --- | --- | --- |
| [1.98.0](https://doc.rust-lang.org/stable/releases.html#version-1980-2026-08-20) | `bool::ok_or_else`, `str::strip_circumfix`, `String::from_utf16le`, and `Send`/`Sync` for `CommandArgs` stabilized. `std::env::Vars` is no longer `Send`/`Sync`; `assert_eq!` gained a temporary scope; `derive(PartialOrd)` takes an `Ord` fast path. | Validate: Quirl captures the environment into owned maps before crossing threads, and every `PartialOrd` derive is consistent with its `Ord`. No new API improves an existing invariant enough to adopt. |
| [1.98.1](https://doc.rust-lang.org/stable/releases.html#version-1981-2026-09-03) | Fixed a miscompilation when generating vtables. | Required. Quirl routes completion, extension, picker, and terminal providers through trait objects, so the pin must include this fix. |
| [1.99.0](https://doc.rust-lang.org/stable/releases.html#version-1990-2026-10-01) | `String::from_utf8_lossy_owned` stabilized; atomic `fetch_update` is deprecated in favor of `try_update`; LLVM 23; legacy integral modules fully deprecated. Clippy added `map_or_identity`, `assert_is_empty` (pedantic), a `chunks_exact`-to-`as_chunks` suggestion, and stricter `branches_sharing_code`. | Adopt `from_utf8_lossy_owned` where captured process output is decoded, rename `fetch_update` to `try_update`, decode embeddings with `as_chunks::<4>`, and fix the `map_or_identity` and shared-tail findings. Allow `assert_is_empty`. |

## Decision

Pin Rust 1.99.0 in `rust-toolchain.toml`, the workspace `rust-version`,
and the Linux check image (by digest).

Decode captured stdout and stderr with `String::from_utf8_lossy_owned`. The
capture already owns its bytes, and the previous
`from_utf8_lossy(&bytes).into_owned()` copied up to 1 MiB per stream even
when the output was valid UTF-8.

Decode embedding vectors with `as_chunks::<4>()`. The fixed-size chunks
replace a fallible `try_into` whose fallback silently produced zero bytes;
the existing length check guarantees an empty remainder.

Allow `clippy::assert_is_empty` workspace-wide. Its suggested replacement,
`assert_ne!(value, [] as [T; 0])`, is harder to read than
`assert!(!value.is_empty())` across 114 test assertions, and tests that need
the value on failure already print it explicitly.

Do not adopt `bool::ok_or_else` or `str::strip_circumfix` broadly: the
existing early returns name their errors at the decision point, and quote
handling already validates both delimiters separately.

## Consequences

- The MSRV is exactly 1.99.0.
- Captured output no longer pays a full copy on the common valid-UTF-8 path.
- The vendored `vt100` copy uses `u8::MAX` instead of the deprecated
  `u8::max_value()`.
- Future compiler upgrades repeat this audit.
