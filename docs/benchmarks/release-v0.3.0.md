# Quirl 0.3.0 native release evidence

Recorded 2026-09-06 for the published [v0.3.0 release](https://github.com/niklas-heer/quirl/releases/tag/v0.3.0). All four native artifacts passed the enforcing release benchmark with 101 successful PTY samples each. These results measure the exact published executables on the hosted runners below; they do not establish universal latency guarantees.

## Release identity

- Candidate and resolved tag commit: `dfb43e54320d166ebe195bb5a0255b8aacbb11e5`.
- Native build and performance run: [34054071682](https://github.com/niklas-heer/quirl/actions/runs/34054071682), attempt 1.
- GitHub release ID: `383694213`; published at `2026-09-06T19:34:54Z` as a nondraft, non-prerelease, immutable release, verified as latest at publication.
- [Release manifest](https://github.com/niklas-heer/quirl/releases/download/v0.3.0/release-manifest-v1.json) SHA-256: `583a36d0d4ae5ae85e336d71695480bee2624778a41f7f52ff7bdd3ec2820329`.
- All 15 published asset digests and byte counts, and the exact release body, matched the independently verified release bundle. Each archive executable matched its native benchmark report.

The binary and benchmark independently reported this same clean source revision. Artifact digest, profile, source identity, and harness identity checks passed on every target.

## Native results

All values below are milliseconds except executable bytes. Limits were startup P50 ≤25 ms, first-prompt paint P95 ≤21 ms, and keystroke-to-frame P95 ≤8 ms. Each row completed 101 of 101 requested PTY samples with no recorded sample failure.

| Target | Executable bytes | Startup P50 | First-prompt P95 | Keystroke P95 | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| Linux ARM64 | 12,493,128 | 6.168840 | 3.939968 | 0.622613 | Pass |
| Linux x86_64 | 14,575,576 | 5.809547 | 4.159942 | 0.475904 | Pass |
| macOS ARM64 | 10,612,336 | 10.456500 | 13.393500 | 1.814000 | Pass |
| macOS x86_64 | 12,846,664 | 15.887156 | 15.620783 | 1.153274 | Pass |

Size follows [ADR 0036](../decisions/0036-advisory-release-binary-size.md): the project has no default executable-size ceiling. All four sizes exceeded the advisory 8 MiB threshold and retained warnings. Schema 8 records `hard_ceiling_bytes`, `enforced_limit_bytes`, and `hard_gate_passed` as `null`; exact measured bytes remain present. No caller maximum was requested. Latency, identity, sample completeness, stream retention, and cleanup remained enforced.

For actual timing measurements, schema 8's per-metric `release_gate_accepted` records valid measurement evidence. Numerical acceptance is separately represented by `target_result` and the overall `performance_gate_passed`; a valid measurement that misses a budget still fails the overall gate. All four published reports pass both validity and numerical checks.

## Runners and toolchain

| Target | GitHub runner label | CPU reported by harness | Logical CPUs | Operating system |
| --- | --- | --- | ---: | --- |
| Linux ARM64 | `ubuntu-24.04-arm` | unknown | 4 | Ubuntu 24.04.4 LTS |
| Linux x86_64 | `ubuntu-24.04` | Intel(R) Xeon(R) 6973P-C | 4 | Ubuntu 24.04.4 LTS |
| macOS ARM64 | `macos-15` | Apple M1 (Virtual) | 3 | macOS 15.7.9 (24G830) |
| macOS x86_64 | `macos-15-intel` | Intel(R) Core(TM) i7-8700B CPU @ 3.20GHz | 4 | macOS 15.7.9 (24G830) |

The Linux ARM64 CPU model was unavailable to the harness and remains `unknown`. Exact hostnames, memory counts, timestamps, and toolchain output remain in the original JSON reports. macOS ARM64 used a virtual Apple M1.

All builds used Rust 1.97.1 (`8bab26f4f`, 2026-07-14), LLVM 22.1.6, and Cargo 1.97.1 (`c980f4866`, 2026-06-30). The official release profile used optimization `z`, fat LTO, one codegen unit, stripped symbols, and `panic = "unwind"`.

## Exact report files

These are byte-for-byte copies of the schema 8 reports uploaded by the native jobs. Their SHA-256 values identify JSON bytes, including whitespace, separately from executable and archive hashes.

| Target and exact report | JSON SHA-256 | Native job |
| --- | --- | --- |
| [aarch64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.3.0/aarch64-unknown-linux-gnu.json) | `eef1859b25a7aec9cc6367ea4be2509fa7fd7e44b94a6882962f3889bde3aeba` | [101543835792](https://github.com/niklas-heer/quirl/actions/runs/34054071682/job/101543835792) |
| [x86_64-unknown-linux-gnu](https://quirl.vercel.app/release-evidence/v0.3.0/x86_64-unknown-linux-gnu.json) | `4ad393566de773b3ce71c050aef5d403985026667974786e5fe7fc40585bdd9a` | [101543835802](https://github.com/niklas-heer/quirl/actions/runs/34054071682/job/101543835802) |
| [aarch64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.3.0/aarch64-apple-darwin.json) | `082c1afceaa1a48900a3306e5552df841ded357e78392babffb0f7eb25112fa4` | [101543835793](https://github.com/niklas-heer/quirl/actions/runs/34054071682/job/101543835793) |
| [x86_64-apple-darwin](https://quirl.vercel.app/release-evidence/v0.3.0/x86_64-apple-darwin.json) | `0c6e10dc4350719ada239acbeb19e31b3b8ca9ff5d4126dea33ab224396431a2` | [101543835800](https://github.com/niklas-heer/quirl/actions/runs/34054071682/job/101543835800) |

## Published executable hashes

Each digest below was checked against the actual `bin/quirl` bytes in its tar archive. Each published archive digest and byte count then matched the release manifest and GitHub asset metadata.

| Target | Executable SHA-256 |
| --- | --- |
| aarch64-unknown-linux-gnu | `5690be71c04f5c1326c87d3bab41bea96ddf59cf1ad9c826e9b5b25d6acf8000` |
| x86_64-unknown-linux-gnu | `315d498e1d541d68cdad013b719ad8fee4765a71ad50144e4e2d99149977f435` |
| aarch64-apple-darwin | `30a6edce509a6220cfb4c505fe0192598d56174336a8720ffd0e2042be1c53d8` |
| x86_64-apple-darwin | `ba16f6054e4f033b9b6a8fc2398e755c2972629054936dcc0c632c9655b58cdc` |

## Preserved earlier attempts

Earlier candidates did not publish a tag or release. Their failures remain separate from the published candidate:

- [Run 34033659127](https://github.com/niklas-heer/quirl/actions/runs/34033659127), candidate `d55033b703314e25d6ec69c9faedf55352f510af`, and [run 34035129106](https://github.com/niklas-heer/quirl/actions/runs/34035129106), candidate `c45ae469b057056c949ba21d502421c04d7cc075`, failed the initial canonical gate before packaging.
- [Run 34036368955](https://github.com/niklas-heer/quirl/actions/runs/34036368955), candidate `bee307192d55c7d69058410b09f5be76b322c9bc`, passed three native targets but failed Intel first-prompt P95 twice: 28.309796 ms on [attempt 1](https://quirl.vercel.app/release-evidence/v0.3.0/prior-candidates/bee3071-intel-attempt-1.json), then 22.811006 ms on its single bounded [attempt 2](https://quirl.vercel.app/release-evidence/v0.3.0/prior-candidates/bee3071-intel-attempt-2.json), against the unchanged 21 ms limit. Both completed all 101 PTY samples. The executable, archive, provenance, methodology, and thresholds matched byte for byte across those attempts. Both original failure reports remain unchanged.
- [Run 34040112452](https://github.com/niklas-heer/quirl/actions/runs/34040112452), candidate `d2d5070e271adb5a1ada5ca4ac987cb63d2adad9`, failed a dialect-island cancellation PTY check before packaging. Its captured screen showed the 30-second execution deadline and status 1 instead of the expected cancellation state. No native performance artifacts were produced.

The two failed Intel reports have SHA-256 values `cb2bcc55eeff8dbd1b1a5c02742168ee071b0cfd5d5924a8af61db8a364f59e5` and `7c2588d66bf52c2d80f702f033d1d387a793ac15eec69d3ddbd8e101f0651111`, respectively. Their timings demonstrate variability for identical bytes, not an infrastructure-only cause. The published candidate's passing measurements do not retroactively pass earlier candidates or isolate the cause of every difference. No size or latency budget was relaxed to obtain this result.

## Distribution and recording

The composed release run completed publication and its protected Homebrew job successfully. [Tap PR 8](https://github.com/niklas-heer/homebrew-tap/pull/8) merged as `57a937fe2b674dd29e7475522889f3bc77c88711`; the formula on tap `main` matched the reviewed four archive URLs and hashes. The [website asset run 34055349465](https://github.com/niklas-heer/quirl/actions/runs/34055349465) passed from the released candidate and produced asset evidence commit `cffebda4b43ad57f18dd3c1e63260ef381c059af`.

The fresh [terminal recording](https://quirl.vercel.app/quirl-demo.mp4) and [provenance](https://quirl.vercel.app/quirl-demo-provenance.json) use the exact published macOS ARM64 executable, SHA-256 `30a6edce509a6220cfb4c505fe0192598d56174336a8720ffd0e2042be1c53d8`. VHS recorded 71.12 seconds in an isolated real PTY with key input; the source revision, package, report, runtime assets, media digests, and sampled visual review are recorded separately. These local recording timings are not benchmark measurements. Provenance explicitly records no human or physical-terminal QA signoff.

## Method and limitations

The PTY harness uses a private initially empty home, configuration, history, catalog, project database, and XDG directories, shared across its samples. Each sample starts a fresh native process in a 120×40 terminal, answers cursor-position queries, reconstructs the screen, checks the prompt and exact binary identity, then proves editability. Startup ends at the editable-marker frame; first-prompt paint and the final representative keystroke use separate endpoints. Percentiles use nearest-rank selection.

After the measurement endpoints, successful samples clear the input and request normal EOF exit within the original two-second sample deadline. Normal exit and bounded process cleanup finish before the next sample. Shutdown and screen stabilization are excluded from first-paint latency. Failed samples retain forced cleanup and cannot produce a passing partial report. Native job logs retain all 101 ordered timing and shutdown observations per target.

Hosted scheduling, virtualization, CPU frequency, other machine load, and filesystem caches are not controlled. The reconstructed PTY frame does not measure physical terminal-emulator scheduling, GPU composition, or monitor scanout. This automated evidence does not cover every terminal, visual accessibility scenario, clipboard, IME, or sustained human session. Separate contract, cancellation, restoration, and long-session checks complement it; no human checklist item is inferred from these results.

Historical [0.2.0 measurements](release-v0.2.0.md) and [earlier release research](release-v1.0.md) retain their original artifact identities, methodology, limits, and outcomes.
