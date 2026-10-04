# Quirl 0.5.0 native release evidence

Recorded 2026-10-04 for the published [v0.5.0 release](https://github.com/niklas-heer/quirl/releases/tag/v0.5.0). All four native artifacts passed the enforcing release benchmark with 101 successful PTY samples each. These results measure the exact published executables on the hosted runners below; they do not establish universal latency guarantees.

## Release identity

- Candidate and resolved tag commit: `54911ba9297af0919b6a2d8ecfe3953fd3d11222`.
- Native build and performance run: [37217737349](https://github.com/niklas-heer/quirl/actions/runs/37217737349).
- GitHub release ID: `403127823`; published at `2026-10-04T17:27:22Z` as a nondraft, non-prerelease, immutable release.
- [Release manifest](https://github.com/niklas-heer/quirl/releases/download/v0.5.0/release-manifest-v1.json) SHA-256: `e0ba442584c2fa940a6feba7a60b7f7928e811c3ac6762acef3094731252f57b`.
- Every entry in the bundle's `SHA256SUMS` verified before publication was approved, and the four formula hashes in [tap PR 34](https://github.com/niklas-heer/homebrew-tap/pull/34) matched the archives byte for byte.

Each report records the candidate as its clean source revision, with artifact digest, profile, source, and harness identity checks passing.

## Native results

All values below are milliseconds except executable bytes. Limits were startup P50 ≤25 ms, first-prompt paint P95 ≤21 ms, and keystroke-to-frame P95 ≤8 ms. Each row completed 101 of 101 requested PTY samples with no recorded sample failure.

| Target | Executable bytes | Startup P50 | First-prompt P95 | Keystroke P95 | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Linux ARM64 | 12,624,336 | 5.986666 | 4.070020 | 0.664128 | Pass |
| Linux x86_64 | 14,776,376 | 7.672771 | 5.342172 | 0.716603 | Pass |
| macOS ARM64 | 10,761,648 | 14.070750 | 19.662375 | 1.173875 | Pass |
| macOS x86_64 | 12,973,880 | 15.689708 | 17.144745 | 1.286758 | Pass |

Size follows [ADR 0036](../decisions/2026-09-05_192326199_track-release-binary-size-without-a-project-hard-ceiling.md): there is no default executable-size ceiling, and every size above the advisory 8 MiB threshold retained its warning.

## Runners and toolchain

| Target | GitHub runner label | CPU reported by harness | Logical CPUs | Operating system |
| --- | --- | --- | ---: | --- |
| Linux ARM64 | `ubuntu-24.04-arm` | unknown | 4 | Ubuntu 24.04.5 LTS |
| Linux x86_64 | `ubuntu-24.04` | AMD EPYC 7763 64-Core Processor | 4 | Ubuntu 24.04.5 LTS |
| macOS ARM64 | `macos-15` | Apple M1 (Virtual) | 3 | macOS 15.7.9 (24G830) |
| macOS x86_64 | `macos-15-intel` | Intel(R) Core(TM) i7-8700B CPU @ 3.20GHz | 4 | macOS 15.7.9 (24G830) |

All builds used Rust 1.99.0 (b940084d7 2026-09-28), LLVM 23.1.1, and Cargo 1.99.0 (5f94df478 2026-08-27) with the official release profile: optimization `z`, fat LTO, one codegen unit, stripped symbols, and `panic = "unwind"`.

## Exact report files

These are byte-for-byte copies of the reports uploaded by the passing native jobs.

| Target and exact report | JSON SHA-256 | Native job |
| --- | --- | --- |
| [aarch64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.5.0/aarch64-unknown-linux-gnu.json) | `6531f5a2a26392523ade344730b1c8855add89a9b622f8d97b2daf19f0444152` | [111487404408](https://github.com/niklas-heer/quirl/actions/runs/37217737349/job/111487404408) |
| [x86_64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.5.0/x86_64-unknown-linux-gnu.json) | `34fd658b410352bfc382ad9cc5f5a38b28c6b9b57ae4dbc1a3d64d7f92c9e411` | [111487429860](https://github.com/niklas-heer/quirl/actions/runs/37217737349/job/111487429860) |
| [aarch64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.5.0/aarch64-apple-darwin.json) | `ec302ebe99a3a1d1ca9a9098b210f14f24555670d840c823f3107e82116dcc3c` | [111487404264](https://github.com/niklas-heer/quirl/actions/runs/37217737349/job/111487404264) |
| [x86_64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.5.0/x86_64-apple-darwin.json) | `7f17ff1e8167d6af0097fff5b1e19a8d2c86b75a1b6a88932cd771d82fe18f69` | [111487403466](https://github.com/niklas-heer/quirl/actions/runs/37217737349/job/111487403466) |

## Published executable hashes

Each digest below is the `bin/quirl` executable inside the published archive, as the passing report verified it.

| Target | Executable SHA-256 |
| --- | --- |
| aarch64-unknown-linux-gnu | `941df2f9c617fcdc64176e439aa73cfddfb093c39aedfa94796ff5f4a14abfe6` |
| x86_64-unknown-linux-gnu | `c99924f58f1ab6a115cc196313e918250446a7ddf6145ed61cb06e0820b0ddb1` |
| aarch64-apple-darwin | `0d272606d564c6da97b98fec3b35145f15b3fd56bd7950952c5d3950ba09dac3` |
| x86_64-apple-darwin | `3629ec4af1ea3ed8b3ba4d671f0f27e6a380f881b680153bba4fb62584cc5250` |

## Preserved earlier attempts

- The first attempt of the macOS x86_64 job, [111483130747](https://github.com/niklas-heer/quirl/actions/runs/37217737349/job/111483130747), measured the same executable (`3629ec4a…`) at a first-prompt paint P95 of 24.249 ms against the unchanged 21 ms limit and stopped aggregation. Its [report](https://quirl.vercel.app/release-evidence/v0.5.0/prior-candidates/54911ba-macos-x86_64-attempt-1.json) (SHA-256 `535765e5c757f8d78df9ab8ec40502441800932a771f4f005fb91ab63f8bc219`) is preserved. Before rerunning only that job, a local interleaved A/B on an Apple Silicon Mac measured the published v0.4.0 source and this candidate with the same harness: first-prompt P50 6.68 and 6.65 ms for v0.4.0 against 6.76 and 6.70 ms for the candidate, within 1%. The rerun on a fresh runner passed at 17.145 ms. No budget was relaxed.

## Method and limitations

The PTY harness uses a private, initially empty home, configuration, history, catalog, project database, and XDG directories shared across its samples. Each sample starts a fresh native process in a 120×40 terminal, answers cursor-position queries, reconstructs the screen, checks the prompt and exact binary identity, then proves editability. Percentiles use nearest-rank selection. Hosted scheduling, virtualization, CPU frequency, other machine load, and filesystem caches are not controlled beyond the documented indexing and settling steps. The reconstructed PTY frame does not measure physical terminal-emulator scheduling or display. This automated evidence does not cover every terminal, visual accessibility scenario, clipboard, IME, or sustained human session, and no human checklist item is inferred from it.

The website recording was not re-recorded for 0.5.0: reviewing it exposed defects fixed in 0.5.1, and the demo comes from that release. Historical [0.4.0 measurements](release-v0.4.0.md) retain their original artifact identities, methodology, limits, and outcomes.
