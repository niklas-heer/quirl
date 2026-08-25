# ADR 0027: Advance the embedded runtime to Lua 5.5.1

- Status: Accepted
- Date: 2026-08-26
- Decision owners: Quirl maintainers
- Supersedes in part: the Lua 5.4 version pin in
  [ADR 0001](0001-lua-extension-language.md)
- Extends: [ADR 0019](0019-isolated-lua-worker-deadlines.md)

## Context

Lua 5.5.1 is the current Lua release. Lua 5.5 adds global declarations, named
vararg tables, read-only loop variables, compact arrays, and incremental major
garbage collections. It also changes the C API and precompiled-chunk format
from Lua 5.4. Quirl embeds source-compatible guest programs through `mlua` and
does not admit precompiled chunks, but a version move still crosses language,
allocator, hook, serialization, and native-library boundaries.

`mlua` 0.12.0 supports Lua 5.5 through its `lua55` feature. Its published
`mlua-sys` 0.11.0 manifest restricts vendored `lua-src` to the 550.x packaging
line, whose Lua 5.5 selection is 5.5.0. Exact Lua 5.5.1 requires `lua-src`
551.0.0 until upstream widens that build-dependency range.

Historical footprint and latency measurements used Lua 5.4. They remain exact
evidence for their recorded artifacts and must not be relabeled as Lua 5.5.1
measurements.

## Decision

Quirl pins `mlua`'s `lua55` feature and vendored `lua-src` 551.0.0, which
contains upstream Lua 5.5.1. `Cargo.lock` is the exact patch-level source of
truth; `_VERSION` provides the independent runtime assertion for the Lua 5.5
language line.

Until a published `mlua-sys` accepts `lua-src` 551.x, Quirl carries a
source-identical copy of `mlua-sys` 0.11.0 under `vendor/mlua-sys` with only the
`lua-src` constraint changed to `=551.0.0`. Cargo's patch table selects that
copy. The vendored package remains in the release dependency and license audit
with an explicit repository-relative source identity. Once upstream publishes
the compatible range, the Cargo patch and vendored directory must be removed
together.

The runtime boundary does not expand:

- only table, string, math, and UTF-8 libraries are installed;
- `io`, `os`, `debug`, `package`, `require`, dynamic loaders, coroutines,
  printing, warnings, and native pattern entry points remain unavailable;
- allocator, instruction, cancellation, callback, and wall-time budgets keep
  their existing limits and structured `ShellError` mapping;
- executable product paths remain behind the supervised worker process, whose
  parent owns deadlines, termination, process-tree containment, and reap;
- all Lua-to-Rust values still cross deny-unknown typed structures and bounded
  validation before state changes.

Lua 5.5's source incompatibilities are accepted for the unreleased development
line. In particular, scripts must not assign to numeric or generic `for`
control variables. Lua's float-to-string output may also differ because 5.5
prints enough decimal digits for round trips. Config schemas, the runner ABI,
and the generated Quirl host SDK do not change.

## Failure and compatibility invariants

- Runtime initialization either installs the same restricted library set or
  fails without exposing a partially initialized VM.
- Guest code cannot catch and clear allocator, instruction, cancellation, or
  worker-deadline termination.
- The persistent VM recovers after ordinary guest failures and after an
  explicitly cleared cancellation, as before.
- Lua 5.4 precompiled chunks are not migrated or accepted. Quirl loads bounded
  source text only, and all C API users are rebuilt with the vendored runtime.
- The full `quirl-lua` contract suite must pass under Lua 5.5.1, including
  hostile returns, restricted globals, source bounds, callback deadlines,
  allocator refusal, instruction termination, and protected-call behavior.
- Release measurements must be rerun before attributing footprint or latency
  claims to a Lua 5.5.1 artifact.

## Consequences

Quirl gains the maintained Lua language line, Lua 5.5's more compact arrays,
and upstream 5.5.1 fixes. Existing scripts that mutate loop control variables
need source changes. The temporary `mlua-sys` packaging patch adds a small
review and maintenance obligation, but keeps the patch level exact, offline,
and auditable instead of silently shipping Lua 5.5.0 under a 5.5.1 claim.

The next release review must run the normal functional, adversarial, website,
packaging, and performance gates. Until new performance evidence is recorded,
Lua 5.4 benchmark results remain historical.
