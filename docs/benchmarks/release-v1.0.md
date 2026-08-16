# 1.0 release performance record

> **Historical evidence — not valid for current HEAD.** The measured artifact
> used `panic=abort`; commit `0fb047d` changed the release profile to
> `panic=unwind` so Lua callback panics remain recoverable. Rebuild and rerun the
> enforcing gate before using this record for a release decision. Benchmark
> report schema v4 now records the source commit, tracked and untracked dirty
> state and exact binary SHA-256. The measured `quirl` binary reports its own
> profile, optimization level, panic strategy, operating system, architecture, source commit, and
> build-time dirty state. The gate verifies them against `quirl-bench` and the
> current clean checkout before accepting evidence. Future evidence therefore
> fails closed instead of silently drifting.

**Measured:** 15 August 2026 at 21:49:13 UTC
**Source state:** uncommitted Phase 4 worktree based on
`0607b7f0d2ce76380c9e5e455906aa8ebb029751`
**Historical release gate:** **passed — three independent 101-sample enforcing gates within target**

This is a release-build record from the named machine below. The harness runs
the actual Quirl binary in a fresh pseudo-terminal for each end-to-end sample.
It reconstructs terminal frames from the PTY byte stream; it does not claim
terminal-emulator scheduling, GPU composition, or monitor-scanout latency.

## Reference machine

| Field | Value |
| --- | --- |
| Hardware | Apple M2 Pro, 12 logical CPUs, 32 GiB memory |
| Platform | macOS 15.7.9 (24G830), `aarch64` |
| Rust | `rustc 1.88.0 (6b00bc388 2025-06-23)`, LLVM 20.1.5 |
| Cargo | `cargo 1.88.0 (873a06493 2025-05-10)` |
| Build | Cargo `release`; fat LTO; one codegen unit; symbols stripped; `panic=abort` |
| Quirl | `quirl 0.1.0`, 4,718,960-byte executable |
| PTY | 120 columns × 40 rows, `TERM=xterm-256color`, truecolor advertised |

The machine was not isolated. CPU frequency, thermal state, scheduler load,
and filesystem cache state were not controlled.

## End-to-end release budgets

Times are wall-clock milliseconds. Enforcing release gates use nearest rank
across 101 independent fresh processes; every sample completed before the
2,000 ms phase timeout.

| Measurement | Valid samples | P50 | P95 | Target | Outcome |
| --- | ---: | ---: | ---: | --- | --- |
| Process start to editable prompt | 101/101 | 12.152 | 15.081 | P50 ≤25 ms | **Within** |
| Final keystroke to corresponding frame | 101/101 | 0.125 | 0.161 | P95 ≤8 ms | **Within** |
| Process start to first prompt frame | 101/101 | 11.995 | 14.876 | ≤16 ms | **Within (conservative P95)** |
| Release executable size | — | 4,718,960 bytes | — | ≤5,242,880 bytes | **Within: 523,920 bytes headroom** |

The language specification leaves the first-prompt percentile unspecified.
The gate uses P95 conservatively rather than treating the P50 as sufficient.
The 5 MiB binary budget is a Phase 4 release-tool default; it may be overridden
explicitly for another frozen release budget.

The measured release profile used `panic = "abort"`, `codegen-units = 1`, fat
LTO, and stripped symbols. Together those settings reduced the executable from
the earlier 7.4 MB measurement to 4,718,960 bytes. Current Quirl intentionally
uses `panic = "unwind"`, so the size and timing figures above are historical
rather than inferred evidence for the current artifact.

## Repeat stability audit

The interactive composition path now discovers and loads the extension host
once, reusing that host to build the catalog and start the REPL. This removes a
duplicate source scan before the first prompt. The enforcing release default is
101 independent PTY sessions: nearest-rank P95 is rank 96, rather than rank 30
of 31, so it does not let a single scheduler outlier determine the tail.
`preview` retains its 31-session diagnostic default, and `--pty-samples`
remains an explicit override for either mode.

Three independent, sequential 101-sample enforcing gates passed. Their
first-prompt P95 values were 15.195, 14.500, and 14.876 ms; cold-start P50,
keystroke P95, bounded-stream evidence, and binary size passed on every run.
Earlier 31-sample diagnostic runs exposed occasional scheduler-tail misses.
The target remains 16 ms P95; increasing the independent sample count makes
the estimate steadier without changing the threshold. The machine remained
non-isolated, so these figures are named-hardware evidence rather than a
cross-machine guarantee.

The readiness probe writes a single character after the first prompt rather
than a multi-key marker, and it fails the sample if that character or the
later `git commit --amend` text is already on screen. This measures the
required editable state without accidentally measuring Reedline's intentional
multi-event paste coalescing. The earlier 100+ ms readiness reading was
therefore a probe artifact, not evidence that a single newly painted prompt
cannot accept input.

## Stream retention evidence

`LiveBuffer` received 100,000 bounded typed samples at each supported window
capacity. Retained samples equal `min(input, capacity)` and dropped samples
equal `input - capacity`; this demonstrates O(window) **sample retention**. It
is not an RSS, allocator, record-size, or producer-backpressure claim.

| Window capacity | Retained | Dropped | Serialized snapshot bytes |
| ---: | ---: | ---: | ---: |
| 1 | 1 | 99,999 | 109 |
| 16 | 16 | 99,984 | 845 |
| 256 | 256 | 99,744 | 12,606 |

## Supplementary diagnostics

These probes are recorded for diagnosis and cannot satisfy an end-to-end
release gate.

| Probe | Samples | P50 | P95 | Scope |
| --- | ---: | ---: | ---: | --- |
| `quirl --version` subprocess | 31 | 2.824 ms | 4.487 ms | Process/loading/argument lower bound |
| Completion, semantic-highlight proxy, prompt render | 2,000 | 0.0119 ms | 0.0135 ms | Headless CPU only |
| Fresh prompt construction and render | 500 | 0.0067 ms | 0.0070 ms | Headless CPU only |

## Reproduce

```sh
cargo build --release -p quirl-cli -p quirl-bench
target/release/quirl-bench release \
  --quirl target/release/quirl \
  --json
```

`release` exits non-zero when any release budget misses, while still emitting
the complete JSON evidence. `preview` emits the same report without enforcing
the final pass/fail exit status. `release` defaults to 101 PTY samples;
`preview` defaults to 31. Both use 31 version samples, 2,000 headless edit
samples, 500 prompt samples, and 100,000 stream samples for each tested window.
