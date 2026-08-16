# Historical 0.1.0 release performance record

> **Historical evidence only.** This record applies solely to commit
> `c5a8d757a35a92a9a269686a1cd166c5a486e2b3` and the SHA-256-named artifact
> below. It is not release evidence for the current candidate; a candidate must
> be measured again from its exact clean source and artifact.

**Measured:** 16 August 2026 at 00:49:55 UTC

**Source:** `c5a8d757a35a92a9a269686a1cd166c5a486e2b3` (clean)

**Artifact SHA-256:** `12989bce23eccda4330cd815f5b234fb7c59e5c5cc23f5f93987592af3f0341e`

**Release gate:** **passed all release budgets**

This record measures the exact `quirl 0.1.0` artifact named above. Report
schema v5 rejects tracked or untracked source changes, requires an independently
supplied SHA-256, copies the artifact into a private read-only staging directory,
and verifies the staged copy before any execution. It also verifies the
artifact's embedded source revision, profile, optimization level, panic
strategy, operating system, and architecture. The report therefore does not
trust the artifact's self-description as its identity and uses one read-only
staged copy for metadata, timing, and size measurements.

The harness runs the actual Quirl binary in a fresh pseudo-terminal for each
end-to-end sample and reconstructs terminal frames from the PTY byte stream.
It does not claim terminal-emulator scheduling, GPU composition, or physical
monitor-scanout latency.

## Reference machine and artifact

| Field | Value |
| --- | --- |
| Hardware | Apple M2 Pro, 12 logical CPUs, 32 GiB memory |
| Platform | macOS 15.7.9 (24G830), `aarch64` |
| Rust | `rustc 1.88.0 (6b00bc388 2025-06-23)`, LLVM 20.1.5 |
| Cargo | `cargo 1.88.0 (873a06493 2025-05-10)` |
| Build | Cargo `release`; `opt-level=z`; fat LTO; one codegen unit; symbols stripped; `panic=unwind` |
| Quirl | `quirl 0.1.0`, 3,861,808-byte executable |
| PTY | 120 columns × 40 rows, `TERM=xterm-256color`, truecolor advertised |

The machine was not isolated. CPU frequency, thermal state, scheduler load,
and filesystem cache state were not controlled.

## End-to-end release budgets

Times are wall-clock milliseconds. The enforcing gate uses nearest rank over
101 independent fresh processes; all samples completed before the 2,000 ms
phase timeout.

| Measurement | Valid samples | P50 | P95 | Target | Outcome |
| --- | ---: | ---: | ---: | --- | --- |
| Process start to editable prompt | 101/101 | 12.093 | 13.323 | P50 ≤25 ms | **Within** |
| Final keystroke to corresponding frame | 101/101 | 0.187 | 0.237 | P95 ≤8 ms | **Within** |
| Process start to first prompt frame | 101/101 | 11.936 | 13.145 | ≤16 ms | **Within (conservative P95)** |
| Release executable size | — | 3,861,808 bytes | — | Historical v5 gate: ≤5,242,880 bytes; current policy: 5 MiB ideal, 8 MiB soft cap, 10 MiB hard ceiling | **Within historical gate and current ideal tier: 1,381,072 bytes below 5 MiB** |

The gate conservatively enforces first-prompt P95. The schema-v5 gate used for
this frozen run enforced a 5 MiB hard limit and passed; that historical result
is unchanged. Current schema v7 defines binary-size units as MiB, where one MiB
is exactly 1,048,576 bytes: 5 MiB (5,242,880 bytes) is ideal, more than 8 MiB
(8,388,608 bytes) records a warning, and more than 10 MiB (10,485,760 bytes) is
a hard release failure. Under the current policy the same measured artifact is
in the ideal tier. `opt-level=z` keeps it compact while the measured interactive
timings retain substantial headroom.

## Stream retention evidence

`LiveBuffer` received 100,000 bounded typed samples at each supported window
capacity. Retained samples equal `min(input, capacity)` and dropped samples
equal `input - capacity`; this demonstrates O(window) sample retention. It is
not an RSS, allocator, record-size, or producer-backpressure claim.

| Window capacity | Retained | Dropped | Serialized snapshot bytes |
| ---: | ---: | ---: | ---: |
| 1 | 1 | 99,999 | 109 |
| 16 | 16 | 99,984 | 845 |
| 256 | 256 | 99,744 | 12,606 |

## Supplementary diagnostics

These probes are diagnostic only and cannot satisfy an end-to-end release
gate.

| Probe | Samples | P50 | P95 | Scope |
| --- | ---: | ---: | ---: | --- |
| `quirl --version` subprocess | 31 | 3.571 ms | 5.325 ms | Process/loading/argument lower bound |
| Completion, semantic-highlight proxy, prompt render | 2,000 | 0.0170 ms | 0.0214 ms | Headless CPU only |
| Fresh prompt construction and render | 500 | 0.0072 ms | 0.0100 ms | Headless CPU only |

## Reproduce

```sh
cargo build --release -p quirl-cli -p quirl-bench
shasum -a 256 target/release/quirl
# Copy the independently recorded digest; do not derive it from the binary's
# self-reported metadata.
QUIRL_EXPECTED_SHA256=<64-hex-digit-digest>
target/release/quirl-bench release \
  --quirl target/release/quirl \
  --expected-sha256 "$QUIRL_EXPECTED_SHA256" \
  --json
```

The commands above use the current schema-v7 harness; they reproduce the method
against a newly built artifact rather than rewriting this frozen schema-v5
record. `release` exits non-zero when any release budget misses, while still
emitting the complete JSON evidence. It requires the independently recorded
SHA-256, copies the supplied binary into a private read-only staging directory,
verifies the copy before executing it, and uses only that staged copy throughout
the run. `preview` emits the same schema without enforcing the final exit status
or requiring a digest. The release run defaults to 101 PTY samples; preview
uses 31. Both use 31 version samples, 2,000 headless edit samples, 500 prompt
samples, and 100,000 stream samples for every tested window.
