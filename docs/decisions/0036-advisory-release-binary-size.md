# ADR 0036: Track release binary size without a project hard ceiling

- Status: Accepted
- Date: 2026-09-05
- Decision owners: Quirl maintainers
- Supersedes in part: the executable ceiling in [ADR 0034](0034-release-binary-budget.md)

## Context

The maintainer explicitly removed the project binary-size limit for now,
prioritizing shell usability and preserving all features. Size remains measured
and reviewed: growth without a clear user benefit warrants investigation,
but a fixed executable ceiling must not drive feature removal or compromise
process cleanup, runtime safety, or the user experience.

Historical evidence remains unchanged. The native Linux x86_64 artifact of
candidate `4b3a71277c0c0c560fa50254d2b4d59ab3cd5605` measured 14,251,864 bytes
in [release run 33985651970](https://github.com/niklas-heer/quirl/actions/runs/33985651970).
Its independently verified package and performance report agreed on the bytes
and SHA-256; it failed the then-applicable 12,582,912-byte ceiling.

Earlier bounded x86_64 experiments on commit `52eb48d` retained all features,
PIE, Rust unwinding, and the pinned toolchain. The smallest diagnostic preview
measured 14,176,880 bytes with machine outlining forced on and its benefit
threshold raised to 64, only 53,248 bytes below that experiment's baseline.
It still missed the old ceiling. This was a nonofficial cross-compiled preview,
not release evidence; those flags were not adopted. ADR 0034 retains the
separate historical ARM64 experiments and their original limits.

Removing the size gate does not turn prior failed reports into passing evidence.
The same `4b3a712` run completed 101 of 101 Intel Mac samples, but first-prompt
paint P95 was 21.832696 ms, above the unchanged 21 ms limit. Startup P50 was
20.324783 ms and passed its 25 ms limit. Publication still requires a fresh,
clean candidate to pass every applicable gate.

## Decision and invariants

- Default release validation has **no project executable-size ceiling** on any
  supported native target. Linux and macOS on x86_64 and aarch64 remain the
  release matrix; this decision does not expand platform support.
- Continue recording exact executable bytes, independent SHA-256, candidate
  identity, and build provenance. Retain the 5 MiB ideal and the advisory
  warning above 8 MiB. Compare named candidates on the same target and review
  unexplained growth; size warnings alone do not block a release.
- A caller may explicitly impose a positive `--max-binary-bytes` limit. Exactly
  that limit passes size admission and one byte above fails. Omitting the
  option leaves size advisory; it never disables missing-evidence validation.
- Preserve all features, the canonical release profile, Rust unwinding, PIE,
  and current runtime requirements. No experimental compiler flag is adopted.
  Artifact staging, scanning, archive, and downloaded-asset resource bounds
  remain enforced independently of this advisory product-size policy.
- Keep every latency, sample-count, artifact identity, and cleanup gate.
  An artifact must still match its native harness architecture. The 25 ms
  startup P50, 21 ms first-prompt P95, and all other enforcing requirements
  remain unchanged.

## Failure model and validation

Treating absent size evidence as zero, measuring a different executable, or
silently ignoring an explicit caller limit would hide a failed validation.
The harness must retain exact-artifact measurement and identity checks even
when size is advisory. Reports must distinguish no requested maximum from an
explicit maximum, and preserve warnings and observed bytes in both cases.
The report leaves the project ceiling absent. Without an explicit caller
maximum, the enforced limit and size-gate verdict are also absent; these are
not zero-byte limits or claims that missing measurements passed.

Tests cover advisory size above the old ceilings, warning boundaries, exact
and one-byte-over caller limits, invalid caller limits, and missing evidence.
Both native operating-system canonical gates, website documentation checks,
and fresh exact-artifact release measurements remain required. Historical
reports, including failed candidates, retain their original policy and result.
