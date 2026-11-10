# Quirl 0.6.1 native release evidence

Recorded 2026-11-10 for the published [v0.6.1 release](https://github.com/niklas-heer/quirl/releases/tag/v0.6.1). All four native artifacts passed the enforcing release benchmark on their first attempt, with 101 successful PTY samples each. These results measure the exact published executables on the hosted runners below; they do not establish universal latency guarantees.

## Release identity

- Candidate and resolved tag commit: `d09b2da951998108f58ac05216ad33722c4e0494`.
- Native build and performance run: [42885038548](https://github.com/niklas-heer/quirl/actions/runs/42885038548).
- GitHub release ID: `440839408`; published at `2026-11-10T19:53:55Z` as a nondraft, non-prerelease, immutable release.
- [Release manifest](https://github.com/niklas-heer/quirl/releases/download/v0.6.1/release-manifest-v1.json) SHA-256: `ceadf72846af98c1a46e20cbf50c85cf25e867c340d566b596484f0050f77183`. Release notes SHA-256: `0199c0c476f377d8d2990c637cb963b03539be6e483a24fc638340b87e61a41f`.
- Every entry in the bundle's `SHA256SUMS` verified before publication was approved. All 15 published assets were downloaded afterwards and matched the reviewed bundle byte for byte, and the four formula hashes in [tap PR 46](https://github.com/niklas-heer/homebrew-tap/pull/46) matched the archives.

Each report records the candidate as its clean source revision, with artifact digest, profile, source, and harness identity checks passing.

## Native results

All values below are milliseconds except executable bytes. Limits were startup P50 ≤25 ms, first-prompt paint P95 ≤21 ms, and keystroke-to-frame P95 ≤8 ms. Each row completed 101 of 101 requested PTY samples with no recorded sample failure.

| Target | Executable bytes | Startup P50 | First-prompt P95 | Keystroke P95 | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Linux ARM64 | 12,820,968 | 6.063741 | 4.408816 | 0.532893 | Pass |
| Linux x86_64 | 14,969,368 | 6.347754 | 4.851904 | 0.522728 | Pass |
| macOS ARM64 | 10,878,192 | 12.129875 | 17.857458 | 1.051292 | Pass |
| macOS x86_64 | 13,146,472 | 19.512153 | 18.372692 | 1.305465 | Pass |

Size follows [ADR 0036](../decisions/2026-09-05_192326199_track-release-binary-size-without-a-project-hard-ceiling.md): there is no default executable-size ceiling, and every size above the advisory 8 MiB threshold retained its warning.

The macOS x86_64 startup P50 rose from 16.37 ms in 0.6.0 to 19.51 ms, still under its 25 ms limit. The source changes between the releases are a lexer fix and a TLS dependency update, neither on the startup path, so the reports alone do not attribute the difference to Quirl. The Linux x86_64 runner also reported a different CPU than for 0.6.0.

## Runners and toolchain

| Target | GitHub runner label | CPU reported by harness | Logical CPUs | Operating system |
| --- | --- | --- | ---: | --- |
| Linux ARM64 | `ubuntu-24.04-arm` | unknown | 4 | Ubuntu 24.04.5 LTS |
| Linux x86_64 | `ubuntu-24.04` | INTEL(R) XEON(R) PLATINUM 8573C | 4 | Ubuntu 24.04.5 LTS |
| macOS ARM64 | `macos-15` | Apple M1 (Virtual) | 3 | macOS 15.8.2 (24H232) |
| macOS x86_64 | `macos-15-intel` | Intel(R) Core(TM) i7-8700B CPU @ 3.20GHz | 4 | macOS 15.7.9 (24G830) |

All builds used Rust 1.99.0 (b940084d7 2026-09-28) and LLVM 23.1.1 with the official release profile: optimization `z`, fat LTO, one codegen unit, stripped symbols, and `panic = "unwind"`.

## Exact report files

These are byte-for-byte copies of the reports uploaded by the passing native jobs.

| Target and exact report | JSON SHA-256 | Native job |
| --- | --- | --- |
| [aarch64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.6.1/aarch64-unknown-linux-gnu.json) | `7b1f28088a9324a13aac464b5d67569f29abb4c0187c6c825307b7935043649e` | [130076157760](https://github.com/niklas-heer/quirl/actions/runs/42885038548/job/130076157760) |
| [x86_64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.6.1/x86_64-unknown-linux-gnu.json) | `61577f7beb9731d39a5f8b8eba1d84e8298d2282d4412c9bb6ec6bfe5b878915` | [130076157741](https://github.com/niklas-heer/quirl/actions/runs/42885038548/job/130076157741) |
| [aarch64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.6.1/aarch64-apple-darwin.json) | `e756a65e0b80451f227deb1806e178d16edbf3676aa7d21d58f92d50dcf5a0c8` | [130076157677](https://github.com/niklas-heer/quirl/actions/runs/42885038548/job/130076157677) |
| [x86_64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.6.1/x86_64-apple-darwin.json) | `ff86ff1903c78699a4196612290f74a24afed725601f9487e6b7409813449276` | [130076157780](https://github.com/niklas-heer/quirl/actions/runs/42885038548/job/130076157780) |

## Published executable hashes

Each digest below is the `bin/quirl` executable inside the published archive. It equals the digest the passing report measured.

| Target | Executable SHA-256 |
| --- | --- |
| aarch64-unknown-linux-gnu | `5100a0c170c295d7d54d7baeee4aaf6f1ede6bf4760a45604225b4a16b978274` |
| x86_64-unknown-linux-gnu | `2fd04cd3dc9aafd44b4ba2198ea04c7e753966955e7c25a8dd32d2bf456cac17` |
| aarch64-apple-darwin | `7eb0d9af0e6c985a1a8e99e41b5e0fd15399fa684d0a7f48046d7c95d7ab0383` |
| x86_64-apple-darwin | `eb88011f3c425793727a4b54e620cbfaa4e9a1b61a5ae1e0efb43e105e935906` |

## Method and limitations

The PTY harness uses a private, initially empty home, configuration, history, catalog, project database, and XDG directories shared across its samples. Each sample starts a fresh native process in a 120×40 terminal, answers cursor-position queries, reconstructs the screen, checks the prompt and exact binary identity, then proves editability. Percentiles use nearest-rank selection. Hosted scheduling, virtualization, CPU frequency, other machine load, and filesystem caches are not controlled beyond the documented indexing and settling steps. The reconstructed PTY frame does not measure physical terminal-emulator scheduling or display. This automated evidence does not cover every terminal, visual accessibility scenario, clipboard, IME, or sustained human session, and no human checklist item is inferred from it.

The website [recording](https://quirl.vercel.app/quirl-demo.mp4), its poster, and the README and website screenshots were recorded from the published macOS ARM64 executable of this release, SHA-256 `7eb0d9af0e6c985a1a8e99e41b5e0fd15399fa684d0a7f48046d7c95d7ab0383`, through `scripts/record-demo.sh`; [their provenance](https://quirl.vercel.app/quirl-demo-provenance.json) records every digest.
