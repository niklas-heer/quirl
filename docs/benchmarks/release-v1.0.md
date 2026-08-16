# 0.1.0 release performance record

**Measured:** 16 August 2026 at 00:07:33 UTC

**Source:** `598c085ddd1821b4f8c9b725a9974a035f3d6ddc` (clean)

**Artifact SHA-256:** `7b51a637862c4d7387317f4a1ac0e985781d888e202ac5f745f384922562c608`

**Release gate:** **passed all release budgets**

This record measures the exact `quirl 0.1.0` artifact named above. Report
schema v4 rejects tracked or untracked source changes, verifies the artifact's
embedded source revision, profile, optimization level, panic strategy,
operating system, and architecture, and hashes the measured executable. The
artifact therefore cannot be silently replaced by a different build while
retaining a passing report.

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
| Quirl | `quirl 0.1.0`, 3,861,712-byte executable |
| PTY | 120 columns × 40 rows, `TERM=xterm-256color`, truecolor advertised |

The machine was not isolated. CPU frequency, thermal state, scheduler load,
and filesystem cache state were not controlled.

## End-to-end release budgets

Times are wall-clock milliseconds. The enforcing gate uses nearest rank over
101 independent fresh processes; all samples completed before the 2,000 ms
phase timeout.

| Measurement | Valid samples | P50 | P95 | Target | Outcome |
| --- | ---: | ---: | ---: | --- | --- |
| Process start to editable prompt | 101/101 | 11.963 | 13.235 | P50 ≤25 ms | **Within** |
| Final keystroke to corresponding frame | 101/101 | 0.188 | 0.228 | P95 ≤8 ms | **Within** |
| Process start to first prompt frame | 101/101 | 11.768 | 13.051 | ≤16 ms | **Within (conservative P95)** |
| Release executable size | — | 3,861,712 bytes | — | ≤5,242,880 bytes | **Within: 1,381,168 bytes headroom** |

The language specification leaves the first-prompt percentile unspecified, so
the gate conservatively enforces P95. The 5 MiB binary limit is the frozen
release-tool default. `opt-level=z` reduces the complete artifact below that
limit while the measured interactive timings retain substantial headroom.

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
| `quirl --version` subprocess | 31 | 3.238 ms | 5.263 ms | Process/loading/argument lower bound |
| Completion, semantic-highlight proxy, prompt render | 2,000 | 0.0173 ms | 0.0205 ms | Headless CPU only |
| Fresh prompt construction and render | 500 | 0.0069 ms | 0.0092 ms | Headless CPU only |

## Reproduce

```sh
cargo build --release -p quirl-cli -p quirl-bench
target/release/quirl-bench release \
  --quirl target/release/quirl \
  --json
```

`release` exits non-zero when any release budget misses, while still emitting
the complete JSON evidence. `preview` emits the same schema without enforcing
the final exit status. The release run defaults to 101 PTY samples; preview
uses 31. Both use 31 version samples, 2,000 headless edit samples, 500 prompt
samples, and 100,000 stream samples for every tested window.
