# Quirl 0.5.1 native release evidence

Recorded 2026-10-04 for the published [v0.5.1 release](https://github.com/niklas-heer/quirl/releases/tag/v0.5.1). All four native artifacts passed the enforcing release benchmark with 101 successful PTY samples each. These results measure the exact published executables on the hosted runners below; they do not establish universal latency guarantees.

## Release identity

- Candidate and resolved tag commit: `3f0064cbd2af644630b7a50e75a57103186b8a91`.
- Native build and performance run: [37223859049](https://github.com/niklas-heer/quirl/actions/runs/37223859049).
- GitHub release ID: `403155381`; published at `2026-10-04T18:39:51Z` as a nondraft, non-prerelease, immutable release.
- [Release manifest](https://github.com/niklas-heer/quirl/releases/download/v0.5.1/release-manifest-v1.json) SHA-256: `31c6e8315ded507d6a4e0d061f71cbaf06340056376a308f60a5fd1e9a680688`.
- Every entry in the bundle's `SHA256SUMS` verified before publication was approved, and the four formula hashes in [tap PR 35](https://github.com/niklas-heer/homebrew-tap/pull/35) matched the archives byte for byte.

Each report records the candidate as its clean source revision, with artifact digest, profile, source, and harness identity checks passing.

## Native results

All values below are milliseconds except executable bytes. Limits were startup P50 ≤25 ms, first-prompt paint P95 ≤21 ms, and keystroke-to-frame P95 ≤8 ms. Each row completed 101 of 101 requested PTY samples with no recorded sample failure.

| Target | Executable bytes | Startup P50 | First-prompt P95 | Keystroke P95 | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Linux ARM64 | 12,689,872 | 6.011165 | 3.980095 | 0.638966 | Pass |
| Linux x86_64 | 14,776,760 | 7.573389 | 5.312338 | 0.698016 | Pass |
| macOS ARM64 | 10,745,104 | 11.854042 | 17.672583 | 0.768000 | Pass |
| macOS x86_64 | 12,969,792 | 17.144970 | 18.666514 | 1.353579 | Pass |

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
| [aarch64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.5.1/aarch64-unknown-linux-gnu.json) | `c512d1cc74b37aae4d3eb172fc68bec94bc3d7d9e5853e37ee6511568cac756a` | [111500706290](https://github.com/niklas-heer/quirl/actions/runs/37223859049/job/111500706290) |
| [x86_64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.5.1/x86_64-unknown-linux-gnu.json) | `39d3421ad93b0e1ba9b0986a0b2835cba49f151d026c80c489084cd784a5ce1a` | [111500706277](https://github.com/niklas-heer/quirl/actions/runs/37223859049/job/111500706277) |
| [aarch64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.5.1/aarch64-apple-darwin.json) | `e37a0725580184e33958631c95c66dc8b8eb508530b7bffa1e9e710e07fd59ed` | [111500706310](https://github.com/niklas-heer/quirl/actions/runs/37223859049/job/111500706310) |
| [x86_64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.5.1/x86_64-apple-darwin.json) | `2610158c82b64a77312640ba24cdb67d92ed217596b0ac4c3ba16bd886274012` | [111500706274](https://github.com/niklas-heer/quirl/actions/runs/37223859049/job/111500706274) |

## Published executable hashes

Each digest below is the `bin/quirl` executable inside the published archive, as the passing report verified it.

| Target | Executable SHA-256 |
| --- | --- |
| aarch64-unknown-linux-gnu | `fd344d512eb348f03e655f080cc118978246ae8a7df7a10fef4ef4079e69dfc0` |
| x86_64-unknown-linux-gnu | `c382c2a159a21d54e935415c1b518b9a08d5e259562d047773b53b379b45c4f8` |
| aarch64-apple-darwin | `09674fb203ca10a367a435a3450f44feb20997a436fe908c0bf9a59c7818400a` |
| x86_64-apple-darwin | `5674c1b86567d897c3f199794fa24eaaca345d55117085b65c738ddd55199417` |

## Method and limitations

The PTY harness uses a private, initially empty home, configuration, history, catalog, project database, and XDG directories shared across its samples. Each sample starts a fresh native process in a 120×40 terminal, answers cursor-position queries, reconstructs the screen, checks the prompt and exact binary identity, then proves editability. Percentiles use nearest-rank selection. Hosted scheduling, virtualization, CPU frequency, other machine load, and filesystem caches are not controlled beyond the documented indexing and settling steps. The reconstructed PTY frame does not measure physical terminal-emulator scheduling or display. This automated evidence does not cover every terminal, visual accessibility scenario, clipboard, IME, or sustained human session, and no human checklist item is inferred from it.

The [website recording](https://quirl.vercel.app/quirl-demo.mp4) and the README and website screenshots were recorded from the published macOS ARM64 executable of this release; [their provenance](https://quirl.vercel.app/quirl-demo-provenance.json) records every digest. Historical [0.5.0 measurements](release-v0.5.0.md) retain their original artifact identities, methodology, limits, and outcomes.
