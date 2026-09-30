# Quirl 0.4.0 native release evidence

Recorded 2026-09-30 for the published [v0.4.0 release](https://github.com/niklas-heer/quirl/releases/tag/v0.4.0). All four native artifacts passed the enforcing release benchmark with 101 successful PTY samples each. These results measure the exact published executables on the hosted runners below; they do not establish universal latency guarantees.

## Release identity

- Candidate and resolved tag commit: `595f1536e908227a1076d274ea5107c7dfefda7f`.
- Native build and performance run: [36668853231](https://github.com/niklas-heer/quirl/actions/runs/36668853231), attempt 1.
- GitHub release ID: `399726821`; published at `2026-09-30T04:49:11Z` as a nondraft, non-prerelease, immutable release.
- [Release manifest](https://github.com/niklas-heer/quirl/releases/download/v0.4.0/release-manifest-v1.json) SHA-256: `91e3b71bedfc9a0cd9769a22ebea635655186fc6c4714c7c48c9992e76343b36`.
- Every entry in the bundle's `SHA256SUMS` verified before publication was approved, and the four formula hashes in [tap PR 28](https://github.com/niklas-heer/homebrew-tap/pull/28) matched the archives byte for byte.

Each report records the candidate as its clean source revision, with artifact digest, profile, source, and harness identity checks passing.

## Native results

All values below are milliseconds except executable bytes. Limits were startup P50 ≤25 ms, first-prompt paint P95 ≤21 ms, and keystroke-to-frame P95 ≤8 ms. Each row completed 101 of 101 requested PTY samples with no recorded sample failure.

| Target | Executable bytes | Startup P50 | First-prompt P95 | Keystroke P95 | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Linux ARM64 | 12,558,664 | 5.595313 | 3.948135 | 0.644879 | Pass |
| Linux x86_64 | 14,629,976 | 7.523809 | 4.842963 | 0.633379 | Pass |
| macOS ARM64 | 10,629,040 | 10.767375 | 16.382625 | 1.632875 | Pass |
| macOS x86_64 | 12,879,512 | 20.511895 | 19.843916 | 1.263846 | Pass |

Size follows [ADR 0036](../decisions/2026-09-05_192326199_track-release-binary-size-without-a-project-hard-ceiling.md): there is no default executable-size ceiling. All four sizes exceeded the advisory 8 MiB threshold and retained warnings; macOS ARM64 grew by 16,704 bytes over 0.3.0.

## Runners and toolchain

| Target | GitHub runner label | CPU reported by harness | Logical CPUs | Operating system |
| --- | --- | --- | ---: | --- |
| Linux ARM64 | `ubuntu-24.04-arm` | unknown | 4 | Ubuntu 24.04.5 LTS |
| Linux x86_64 | `ubuntu-24.04` | AMD EPYC 7763 64-Core Processor | 4 | Ubuntu 24.04.5 LTS |
| macOS ARM64 | `macos-15` | Apple M1 (Virtual) | 3 | macOS 15.7.9 (24G830) |
| macOS x86_64 | `macos-15-intel` | Intel(R) Core(TM) i7-8700B CPU @ 3.20GHz | 4 | macOS 15.7.9 (24G830) |

All builds used Rust 1.97.1 (`8bab26f4f`, 2026-07-14), LLVM 22.1.6, and Cargo 1.97.1 (`c980f4866`, 2026-06-30) with the official release profile: optimization `z`, fat LTO, one codegen unit, stripped symbols, and `panic = "unwind"`.

## Exact report files

These are byte-for-byte copies of the schema 8 reports uploaded by the native jobs.

| Target and exact report | JSON SHA-256 | Native job |
| --- | --- | --- |
| [aarch64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.4.0/aarch64-unknown-linux-gnu.json) | `b0573373550c6f366c1fb674fdd5aa936d3bc187340c9210e63757a79990cf5f` | [109740946686](https://github.com/niklas-heer/quirl/actions/runs/36668853231/job/109740946686) |
| [x86_64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.4.0/x86_64-unknown-linux-gnu.json) | `d253c87458fe68ff2dd303d6e1541f145ba05a716a52c443b7cd594e45dfccf4` | [109740946809](https://github.com/niklas-heer/quirl/actions/runs/36668853231/job/109740946809) |
| [aarch64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.4.0/aarch64-apple-darwin.json) | `00925905e1b1e17259fd9dcf2b4f0af776e4cde585fe4e591d9ac5d982c13d30` | [109740946793](https://github.com/niklas-heer/quirl/actions/runs/36668853231/job/109740946793) |
| [x86_64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.4.0/x86_64-apple-darwin.json) | `02ff74e2770ef45655b6adf5c8f7bcc21857f289fa695fc016ade920f61b25a7` | [109740946681](https://github.com/niklas-heer/quirl/actions/runs/36668853231/job/109740946681) |

## Published executable hashes

Each digest below was computed from the `bin/quirl` bytes inside the published archive.

| Target | Executable SHA-256 |
| --- | --- |
| aarch64-unknown-linux-gnu | `9ed58974d6fe7ab57d6d3f3179f3072fdc4ebec90e4ed05bdb187ac5c3391100` |
| x86_64-unknown-linux-gnu | `384dfd9529dfbbaa3b31197a51df9aae94680707c76af73cab2b5352575de005` |
| aarch64-apple-darwin | `a60744e7ab1a937a5c763c1a7253656f30f0d320a81e58b67aedba5974cd5588` |
| x86_64-apple-darwin | `381a592deb607e7f29e16c97e6b50b65a5e686bda609ed5309bbbffab44ef582` |

## Preserved earlier attempts

Earlier candidates did not publish a tag or release:

- [Run 36657374419](https://github.com/niklas-heer/quirl/actions/runs/36657374419), candidate `75c53fe595b6ab5322b07d4db64b9981ac1e4bc5`, passed three targets but missed the macOS ARM64 first-prompt P95 on all three bounded attempts: 24.994959 ms ([attempt 1](https://quirl.vercel.app/release-evidence/v0.4.0/prior-candidates/75c53fe-macos-arm64-attempt-1.json)), 26.093459 ms ([attempt 2](https://quirl.vercel.app/release-evidence/v0.4.0/prior-candidates/75c53fe-macos-arm64-attempt-2.json)), and 23.983458 ms ([attempt 3](https://quirl.vercel.app/release-evidence/v0.4.0/prior-candidates/75c53fe-macos-arm64-attempt-3.json)), against the unchanged 21 ms limit. Their report SHA-256 values are `03cc21293e4d3777922b51cc9cdcb96befcc77d27af7fe8dfae7d9605acc6803`, `d1b611ba9edb2398c4cbb45ad14aafc23e25d1482f23db01bc2630cfb0622525`, and `6af92fc56da70607116e29f81e2958dd8be0cb698307be9942fc215cb724375e`.
- [Run 36665750746](https://github.com/niklas-heer/quirl/actions/runs/36665750746), candidate `56debdea32e10c153c0368a10a7ec3763e2a0969`, added a bounded wait for the load average to settle; the runner stayed at load 7.61 on 3 CPUs after four minutes and missed again (28.21 ms).

To separate a code regression from the environment, temporary A/B runs on the same runner type built the published v0.3.0 source and the 0.4.0 candidate and measured them interleaved. Straight after compilation both missed (v0.3.0 23.25 ms, 0.4.0 23.79 ms). After the load settled both passed (v0.3.0 12.65-18.42 ms, 0.4.0 12.17-14.54 ms). A diagnostic run then showed `mds`, `mds_stores`, and `mdworker` using over 200% CPU while Spotlight indexed the checkout and the fresh build; with `mdutil -a -i off` the load average fell from about 10 to 1.4 within 90 seconds. The release workflow now turns off Spotlight indexing on macOS runners before building and waits for a settled load before measuring, as documented in [the release procedure](../releasing.md). No size or latency budget was relaxed.

## Method and limitations

The PTY harness uses a private, initially empty home, configuration, history, catalog, project database, and XDG directories shared across its samples. Each sample starts a fresh native process in a 120×40 terminal, answers cursor-position queries, reconstructs the screen, checks the prompt and exact binary identity, then proves editability. Percentiles use nearest-rank selection. Hosted scheduling, virtualization, CPU frequency, other machine load, and filesystem caches are not controlled beyond the documented indexing and settling steps. The reconstructed PTY frame does not measure physical terminal-emulator scheduling or display. This automated evidence does not cover every terminal, visual accessibility scenario, clipboard, IME, or sustained human session, and no human checklist item is inferred from it.

The website [recording](https://quirl.vercel.app/quirl-demo.mp4) still shows the v0.3.0 release package; it was not re-recorded for 0.4.0.

Historical [0.3.0 measurements](release-v0.3.0.md) retain their original artifact identities, methodology, limits, and outcomes.
