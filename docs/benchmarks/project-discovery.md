# Project discovery performance record

**Status: current release-build development measurement recorded; dependency
comparison and cancellation evidence pending.** The measured rows below name a
dirty development candidate and a synthetic warm-filesystem fixture. They are
useful latency evidence for the shipped `std::fs` design, but are not sufficient
to approve a traversal dependency change. `pending` is not a zero or estimate.

This record compares project-discovery implementations without weakening the
filesystem, cancellation, or publication contract in
[ADR 0030](../decisions/0030-bounded-project-discovery.md). A faster walker is
not interchangeable unless it discovers the same repositories, applies the
same exclusions and filesystem boundaries, reports the same incomplete cases,
and preserves the previous complete SQLite generation after failure.

## Candidate identity

This identity describes the 2 September 2026 development measurement. The
binary digest is authoritative for the timed artifact; the source was not a
clean commit.

| Field | Recorded value |
| --- | --- |
| Candidate commit and tree | Base `453d9fc9b1a92071694eac29f2b957884b6791d1`; dirty tracked-crates patch SHA-256 `c791cb4ba76f6b5ff9f529970d7ba8b6ccd2c63b672a660508e01e1796a7691c`; untracked `projects.rs` SHA-256 `cd5097e6bf4b4b56f5e1a4aca43b84b9593907973c94bdfb98d21ea4af5f77f8` |
| `Cargo.lock` digest | SHA-256 `6bad8f78605c1e0e05a2a2e7623372a3a75e5c1171a20bd317018da71ca89167` |
| Release artifact digest and byte size | `target/release/quirl`: SHA-256 `87ca420fd5ec9a2fa7fb904c49eb3518f847b4d0b26b10ba8d9f3f181b03b5c1`; 10,462,592 bytes |
| Rust and Cargo versions | `rustc 1.97.1 (8bab26f4f 2026-07-14)`; `cargo 1.97.1 (c980f4866 2026-06-30)` |
| Target triple and release profile | `aarch64-apple-darwin`; Cargo `release`, optimization level `z`, unwind panic strategy |
| Hardware, memory, operating system, and filesystem | Apple M2 Pro; 32 GiB; macOS 15.7.9 (24G830); local journaled APFS |
| CPU/load/thermal controls | No controls; machine uptime 17 days and load averages 2.29, 3.43, 2.96 before measurement |
| Filesystem-cache preparation | OS caches were not flushed. Fixture creation and one correctness probe preceded timing, so results are new-index and warm-filesystem measurements, not cold-filesystem claims. No timed sample was discarded. |
| Measurement timestamp and timezone | 2026-09-02, 14:49–14:56 CEST |

Results without this identity are development observations, not evidence for
changing the runtime implementation.

## Workloads and correctness gate

Use deterministic fixtures with a recorded generator seed and digest. At
minimum, exercise:

- a broad, shallow repository layout;
- a deep mixed tree containing repository and non-repository branches;
- both `.git` directories and bounded `.git` files used by linked worktrees;
- top-level hidden repositories plus nested hidden, cache, Trash,
  application-data, package-cache, and build-output trees that must be pruned;
- symlink cycles, a second filesystem when the platform can provide one,
  permission failures, disappearing entries, and configured exclusions; and
- enough entries to measure cancellation and at least one configured resource
  boundary without publishing a partial generation.

Before timing, every in-process candidate must produce the same canonical set
of retained repository paths and the same complete/incomplete classification
as the reference implementation. Record directories inspected, entries
inspected, repositories retained, retained path bytes, and the published
generation alongside elapsed time. A sample that returns a different set,
crosses an excluded boundary, misses cancellation, or publishes partial state
is invalid rather than fast.

The candidates are:

1. the current explicit `std::fs` breadth-first walker;
2. a sequential `ignore::WalkBuilder` adapter, if implemented;
3. a parallel `ignore::WalkBuilder` adapter, if implemented; and
4. the `rg` executable as a diagnostic traversal baseline only.

The `rg` baseline does not implement Quirl's SQLite transaction, linked
worktree parsing, activity metadata, cancellation ownership, or complete-scan
deletion rules. Report it separately and never treat its timing as semantic
equivalence.

## Cold and warm definitions

“Cold” and “warm” are ambiguous unless both database and operating-system
cache state are named:

- **Cold index:** a new private `projects.sqlite3` is created, the complete
  fixture is scanned, activity metadata is observed, and one generation is
  committed. Record whether filesystem caches were actually flushed; if they
  were not, call the run “new-index” rather than “cold filesystem.”
- **Warm cache read:** an existing complete SQLite generation is loaded and
  ranked without requesting traversal. This measures readiness of
  `Alt-Q g`, not scan throughput.
- **Warm refresh:** the existing database and fixture are rescanned and a new
  complete generation is published. Record whether the process and filesystem
  caches were warm.
- **Cancellation:** cancellation is requested at a deterministic admitted-entry
  count. Measure request-to-worker-stop latency and verify that the last
  complete generation remains readable.

Use a release build and enough independent samples to report minimum, median,
P95, maximum, valid sample count, and failures. Keep setup and fixture creation
outside the timed region. Do not discard warmups silently; state the warmup
policy and retain raw machine-readable output with the evidence commit.

## Results

### End-to-end Quirl path

These measurements include traversal where applicable, activity metadata,
SQLite publication or cache loading, and project-item construction.

| Workload | Candidate | Valid samples | Minimum | P50 | P95 | Maximum | Outcome |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| New index, warm filesystem | `std::fs` breadth-first | 20/20 | 118.250 ms | 120.589 ms | 125.952 ms | 413.729 ms | 1,200 repositories committed in every sample; the first post-build sample is the retained maximum outlier |
| Warm cache read | SQLite snapshot | 30/30 | 12.013 ms | 12.219 ms | 12.874 ms | 13.017 ms | 1,200 ranked projects loaded without traversal |
| Warm refresh | `std::fs` breadth-first | 20/20 | 116.966 ms | 120.879 ms | 125.946 ms | 129.282 ms | 1,200 repositories retained; generation advanced from 1 to 21 |
| Cancellation latency | `std::fs` breadth-first | pending | pending | pending | pending | pending | pending measurement |

For context, 30 release-binary `quirl --build-info` subprocesses measured a
4.303 ms P50 and 5.944 ms P95 on the same machine. The cache-read row is an
end-to-end CLI measurement, so it includes process startup, argument parsing,
SQLite admission and ranking, and project-item construction.

### Traversal candidate comparison

Add rows only for adapters that pass the correctness gate. The standalone `rg`
row remains diagnostic and must name its exact command and semantic omissions.

| Workload | Candidate | Repository-set match | Valid samples | P50 | P95 | Entries/second | Notes |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- |
| Synthetic fixtures | `std::fs` breadth-first | 1,200/1,200 paths matched the diagnostic marker set | 20 | 120.879 ms | 125.946 ms | not exposed | Includes activity reads and SQLite publication |
| Synthetic fixtures | sequential `ignore` adapter | pending | pending | pending | pending | pending | not implemented/measured |
| Synthetic fixtures | parallel `ignore` adapter | pending | pending | pending | pending | pending | not implemented/measured |
| Diagnostic only | ripgrep 15.2.0 | not semantically equivalent; its 1,200 marker paths matched the committed repository paths | 20 | 79.712 ms | 85.089 ms | not comparable | `rg --files --hidden -g '**/.git/HEAD'`; no activity or SQLite work |

The roughly 41 ms P50 gap between warm Quirl refresh and the `rg` diagnostic
does not justify adding `ignore`: the baseline omits Quirl's linked-worktree
parser, activity metadata reads, bounded error semantics, and transactional
cache update. It does show that traversal remains a meaningful fraction of the
refresh path and supplies a baseline for a future equivalent adapter.

## Reproduction

The measured deterministic fixture has 6,003 directories and 2,400 files:
1,200 repositories at `home/Code/repoNNNN/.git/HEAD` and 1,200 equally broad
non-repository branches at `home/Archive/noiseNNNN/a/b/data`. Its sorted path
listing has SHA-256
`4a664d4edff789683e4729ba033109f5bf6a0ff5f098272b667fa3140a739021`.
No random seed is involved.

The fixture and one new-index sample can be recreated in Zsh with:

```sh
cargo build --release -p quirl-cli
benchmark_root=$(mktemp -d /tmp/quirl-project-bench.XXXXXX)
for index in {0000..1199}; do
  mkdir -p \
    "$benchmark_root/home/Code/repo${index}/.git" \
    "$benchmark_root/home/Archive/noise${index}/a/b"
  touch \
    "$benchmark_root/home/Code/repo${index}/.git/HEAD" \
    "$benchmark_root/home/Archive/noise${index}/a/b/data"
done
mkdir -p "$benchmark_root/config" "$benchmark_root/db"

env \
  HOME="$benchmark_root/home" \
  QUIRL_CONFIG_DIR="$benchmark_root/config" \
  QUIRL_PROJECTS_DB="$benchmark_root/db/projects.sqlite3" \
  target/release/quirl pick \
    --source projects --refresh --query __quirl_no_match__
```

Each table sample timed only the child command with Perl `Time::HiRes::time`;
fixture construction and release compilation were outside the timed region.
New-index samples used distinct database paths, cache reads omitted `--refresh`,
and warm refreshes reused one database. Nearest-rank percentiles use
`ceil(sample_count × percentile) - 1` after sorting. The diagnostic command was:

```sh
rg --files --hidden -g '**/.git/HEAD' "$benchmark_root/home"
```

The repository-set check sorted the SQLite `path` column and compared it with
the `rg` paths after removing `/.git/HEAD`; all 1,200 paths matched. This does
not establish semantic equivalence on exclusions, errors, linked worktrees,
filesystem boundaries, cancellation, or publication.

Raw elapsed seconds follow. All status codes were zero:

```json
{
  "new_index": [
    0.413729, 0.124599, 0.125906, 0.123649, 0.125952,
    0.119824, 0.119101, 0.120106, 0.125687, 0.124433,
    0.119568, 0.122979, 0.120589, 0.122846, 0.120015,
    0.119335, 0.118370, 0.118250, 0.118638, 0.122432
  ],
  "warm_cache": [
    0.012458, 0.012267, 0.013017, 0.012317, 0.012199,
    0.012165, 0.012346, 0.012301, 0.012455, 0.012166,
    0.012193, 0.012463, 0.012760, 0.012296, 0.012013,
    0.012052, 0.012178, 0.012038, 0.012434, 0.012075,
    0.012188, 0.012284, 0.012209, 0.012021, 0.012874,
    0.012232, 0.012219, 0.012057, 0.012305, 0.012120
  ],
  "warm_refresh": [
    0.120398, 0.122588, 0.120495, 0.118400, 0.117863,
    0.121327, 0.120879, 0.122376, 0.116966, 0.125946,
    0.122500, 0.120123, 0.118506, 0.120955, 0.122460,
    0.118357, 0.129282, 0.124133, 0.121183, 0.118304
  ],
  "ripgrep_marker_scan": [
    0.075013, 0.081484, 0.080639, 0.077558, 0.079712,
    0.081990, 0.080773, 0.070072, 0.076544, 0.084154,
    0.082870, 0.076387, 0.080843, 0.085089, 0.073830,
    0.078618, 0.072511, 0.070536, 0.080770, 0.085489
  ]
}
```

Cancellation and adversarial-fixture timings still need a checked-in harness
that can trigger cancellation at an admitted-entry count and emit scan counters.
Until those rows and equivalent `ignore` adapters exist, this record makes no
dependency-selection claim.
