# Shell workload benchmarks

Scoped engineering measurements of everyday shell work: loops over builtins,
arithmetic, function calls, `case`, and starting external programs. They
compare Quirl's native executor with dash, Bash, and Zsh on one machine. They
are not release evidence and say nothing about other hardware.

## Method

[`scripts/bench-shell-workloads.py`](../../scripts/bench-shell-workloads.py)
runs each workload as one program, through `quirl exec` for Quirl and `-c` for
the reference shells, and reports the median wall time over fresh processes,
including startup. Interactive latency comes from `quirl-bench preview`, which
drives a real PTY.

```console
cargo build --release -p quirl-cli -p quirl-bench
scripts/bench-shell-workloads.py target/release/quirl 9
target/release/quirl-bench preview --quirl target/release/quirl --pty-samples 51
```

## 2026-11-08: faster process spawning

Measured on an Apple M4 running macOS 27.0, with both builds made back to
back with the same settings (release, opt-level 3, no LTO, 16 codegen units).
The baseline is `main` at `ac09361`, which already runs loops, functions, and
aliases natively. The candidate is the `perf/fast-process-spawn` branch. The
reference shells are the system `/bin/dash`, `/bin/bash` 3.2, and `/bin/zsh`.

Median milliseconds over 9 runs:

| Workload | Quirl `ac09361` | Quirl candidate | dash | Bash | Zsh |
| --- | ---: | ---: | ---: | ---: | ---: |
| startup (`true`) | 5.7 | 5.1 | 1.8 | 2.1 | 3.0 |
| arithmetic `while`, 10k iterations | 44.1 | 33.9 | 21.4 | 38.5 | 19.1 |
| `for` with `echo`, 10k iterations | 14.0 | 7.7 | 17.2 | 24.4 | 22.2 |
| function calls, 5k | 25.2 | 21.0 | 13.6 | 24.1 | 25.6 |
| `case`, 10k iterations | 38.6 | 31.2 | 19.0 | 37.8 | 19.3 |
| 200 external commands | 1,576.6 | 475.4 | 247.7 | 251.5 | 258.8 |

Interactive latency, `quirl-bench preview`, 51 PTY samples:

| Measurement | `ac09361` P50 / P95 | candidate P50 / P95 |
| --- | ---: | ---: |
| cold start to editable prompt | 7.32 / 10.44 ms | 6.48 / 7.14 ms |
| keystroke to frame | 0.42 / 0.53 ms | 0.42 / 0.53 ms |
| process startup to `--version` exit | 4.79 / 4.88 ms | 3.25 / 3.40 ms |

What changed:

- Pipelines start their trusted staging and anchor scripts in `/bin/dash`
  instead of the macOS `/bin/sh` shim, and start the anchor in parallel with
  the staged leader. See
  [the decision](../decisions/2026-11-08_041414983_start-process-group-anchors-with-a-trusted-fast-shell-in-par.md).
- Child and staging polls back off from 25 µs instead of sleeping 1 ms.
- Reedline no longer links CoreFoundation, which cost about 0.7 ms per launch.
- Arithmetic operator lookup and pipeline expansion allocate and compare less.

Each external command still costs about 2.4 ms against 1.3 ms in the
reference shells, because every pipeline starts two small trusted shells to
own its process group. Removing the staging shell would need a separate
decision about which process leads the group.
