# ADR 0034: Preserve features with a 12 MiB release-binary ceiling

- Status: Accepted
- Date: 2026-09-05
- Decision owners: Quirl maintainers
- Supersedes in part: the 10 MiB binary ceiling in the language design and ADR 0025
- Superseded in part by: [ADR 0036](0036-advisory-release-binary-size.md),
  which makes executable size advisory by default; the measurements below
  retain the policy used at the time.

## Context

The 0.2 candidate combines project discovery, typed pipelines, bounded Lua,
native execution, command intelligence, and the interactive explorer. The
official ARM64 Linux build of commit `94194b1` measured 12,230,952 bytes, above
the previous 10,485,760-byte ceiling. Its Rust unwinding tables are necessary
for cleanup and the Lua callback boundary. Removing them, switching to aborting
panics, or removing supported features is not an acceptable size fix.

Bounded experiments on the pinned Rust 1.97.1 toolchain retained all features,
PIE, and `panic = "unwind"`. LLD with safe identical-code folding saved only
19,224 bytes. Disabling machine outlining increased size; a higher outlining
benefit threshold reduced a development preview to 11,837,736 bytes but still
missed the old ceiling. The `s` optimization profile increased size to
13,672,744 bytes. Not forcing unwind tables did not reduce the binary. These
are diagnostic previews, not measurements of the final release artifact.

The maintainer explicitly approved a 12 MiB ceiling to preserve all current
features after reviewing the 11.66 MiB baseline and 11.29 MiB smallest preview.
This is a reviewed budget change, not a claim that the old ceiling was met.

## Decision and invariants

- The hard ceiling is **12,582,912 bytes (12 MiB)** for each supported native
  executable. The archive and separately downloaded runtime assets have their
  own admission limits; their sizes do not substitute for executable size.
- The 5 MiB ideal and 8 MiB advisory warning remain. A caller may enforce a
  stricter ceiling with `--max-binary-bytes`, but cannot raise the project cap.
- Keep the canonical `z`, fat LTO, one codegen unit, stripped-symbol release
  profile and Rust unwinding. None of the experimental compiler flags is
  adopted. Keep the existing Linux runtime/loader requirements.
- Avoid the model loader's full-tokenizer serialization for metadata lookup.
  Typed access retains all four model variants and inference behavior; its
  allocation reduction is worthwhile independently of binary-size savings.
- Every native release job runs the existing enforcing benchmark against the
  exact packaged executable and an independently calculated SHA-256. Size or
  latency failures stop aggregation. Failed reports remain inspectable.

## Failure model and validation

The release must fail before publication when any native artifact exceeds the
cap or fails an existing latency or evidence gate. Gate overrides cannot admit
a larger binary. An experimental build's measurements cannot certify a later
candidate. Historical records retain their original limits and identities.

Boundary tests cover the ideal and warning tiers, the exact 12 MiB ceiling,
one byte above it, and stricter caller limits. The Lua regression proves that
a Rust callback panic drops both callback and caller guards, preserves the
original panic, and leaves the restricted VM usable after the panic is caught.
Both macOS and Linux canonical gates, fresh exact-artifact performance reports,
and the release checklist still apply. This budget decision does not waive any
other release requirement.
