# Steel, Lua, and Fennel — historical spike 2

> Historical evidence only. Steel was rejected and its runtime, dependencies,
> executable probes, and integration code have been removed from Quirl.

> Historical baseline: the static-analysis requirement reopened the core
> language decision. See the newer
> [embedded language selection spike](embedded-language-selection.md).

Date: 2026-08-15

Machine: Apple M2 Pro, arm64, macOS 15.7.9

Build: Rust 1.88.0, `--release`, Steel 0.8.2, Lua 5.4 via mlua 0.12.0,
Fennel 1.6.1

Steel remains Quirl's core language. Lua is the reference runtime; Fennel tests
whether a Lisp that compiles to Lua is a compelling optional second language.

| Runtime | Case | Samples | Median | P95 |
| --- | --- | ---: | ---: | ---: |
| Steel | VM construction | 40 | 63.918 ms | 66.189 ms |
| Lua | VM construction | 40 | 0.033 ms | 0.046 ms |
| Fennel | Lua VM + compiler load | 40 | 4.983 ms | 5.525 ms |
| Steel | Parse and evaluate `(+ 20 22)` | 400 | 36.250 µs | 44.083 µs |
| Lua | Parse and evaluate `20 + 22` | 400 | 1.792 µs | 2.292 µs |
| Fennel | Compile and evaluate `(+ 20 22)` | 400 | 53.000 µs | 77.250 µs |
| Steel | Cached function → Rust host call | 10,000 | 0.125 µs | 0.167 µs |
| Lua | Cached function → Rust host call | 10,000 | 0.042 µs | 0.084 µs |
| Fennel | Cached function → Rust host call | 10,000 | 0.042 µs | 0.084 µs |

Fennel's cached result equals Lua because Fennel has no separate runtime: it
compiles to Lua. The compiler was loaded from the official 301,522-byte
`fennel-1.6.1.lua` distribution with SHA-256
`c3d45602041e7d8ef8a212563573df040c48a85c648a29fb4597ebed4bc38ec2`.

## Decision

Steel VM construction still misses the design's 10 ms P95 target. It must stay
off the first editable-prompt path; Quirl creates it on first Steel/data use and
then retains it.

Fennel is the strongest optional second-language candidate. Its compiler fits
inside the 10 ms target on this machine, but interactive compilation should
still be lazy. Installed `.fnl` scripts and plugins should compile ahead of time
to cached Lua; their runtime startup and host-call cost then become Lua's.

This does not replace Steel as Quirl's core language. Steel gives Quirl one
Rust-native value/error contract, immutable-first semantics, contracts, and a
single configuration/plugin ecosystem. Fennel inherits Lua's dynamic and
mutable data model, so exposing the same typed Quirl host API requires an
adapter and a second tooling contract. If Quirl later ships Lua, supporting
both `.lua` and `.fnl` through one optional engine is the preferred design.

The next spike should measure cached Steel executables, record/list conversion,
resident memory, release-binary size, and Fennel diagnostic/source-map quality.
Results are local baselines, not cross-machine comparisons.

## Reproduce

Download the pinned official one-file Fennel library outside the repository,
then pass it explicitly to the benchmark:

```console
curl -fsSL https://fennel-lang.org/downloads/fennel-1.6.1.lua \
  -o /tmp/quirl-fennel-1.6.1.lua

cargo run --release -p quirl-bench -- \
  --fennel /tmp/quirl-fennel-1.6.1.lua

cargo run --release -p quirl-bench -- \
  --fennel /tmp/quirl-fennel-1.6.1.lua --json
```

Without `--fennel`, the same binary runs the original Steel/Lua subset.
