# Embedded language selection — spike 3

> **Superseded recommendation:** this latency-focused spike selected
> TypeScript/QuickJS-NG provisionally. The subsequent
> [complete-system footprint, health, and complexity report](../embedded-language-decision.md)
> includes the TypeScript checker/toolchain cost and the Rust-hosted plugin/config
> scope. The final product decision selects Lua 5.4 for its familiarity,
> longevity, footprint, and mature Rust bridge; see [ADR 0001](../decisions/0001-lua-extension-language.md).

Date: 2026-08-15

Machine: Apple M2 Pro, arm64, macOS 15.7.9

Build: Rust 1.88.0, release mode

## Recommendation

Use **strict TypeScript 7 checked by the native compiler and executed by
QuickJS-NG as the front-runner for Quirl's one privileged embedded language**.
This pairing gives Quirl the language most likely to be immediately familiar,
the strongest existing editor and AI ecosystem in the candidate set, a real
ahead-of-execution checker, and a lightweight runtime without embedding V8.
QuickJS-NG still clears the sub-millisecond startup gate comfortably.

This reopened the earlier Steel-first decision; it does not silently create a
two-language core. If the TypeScript/QuickJS-NG vertical slice passes the remaining
module, host-API, cancellation, and analyzer-service gates, scripts,
configuration, commands, and trusted plugins should all move together to
TypeScript. Steel can remain an optional runner, not a second privileged
configuration/plugin language. **Strict Luau is the lightweight fallback** if
the native TypeScript compiler's distribution and memory cost is unacceptable.

Use **WebAssembly components as the isolation and portable extension ABI**.
AssemblyScript is a leading TypeScript-shaped guest-language experiment, but
it is not a drop-in TypeScript runtime and its compiler/host-data workflow is
heavier than an embedded interpreter. MoonBit deserves the parallel typed-Wasm
spike because it has first-class component-model support and native typed error
handling; its younger ecosystem and compiler/toolchain licensing need explicit
product review before bundling.

## Hard gates

The core language must pass every gate. Fast dynamic execution alone is not
enough.

| Gate | Required behavior |
| --- | --- |
| Check before run | Parse, resolve names, and type-check without executing; CI and AI receive structured spans and fixes |
| IDE engine | Incremental diagnostics, hover types, definitions, references, and context-aware completion |
| Generated host types | Quirl's Rust catalog generates the language's host declarations and docs; runtime bindings and checker declarations share a schema hash |
| Interactive speed | VM startup P95 below 1 ms, warm scalar host call P95 below 1 µs, and no VM on the first-prompt path |
| Safe embedding | Memory/instruction budgets, interruption, sandboxed globals, controlled module loading, and capability-only host access |
| One product language | The same language configures Quirl, writes scripts, defines commands, and implements trusted plugins |
| Maintainable bridge | A supportable Rust runtime binding and a supportable way to embed or invoke the analyzer |

## Measured runtime baseline

These are microbenchmarks, not application throughput claims. Steel, Lua, Rhai,
and Fennel share the Rust harness. Luau and QuickJS-NG use isolated Rust builds to
avoid mutually exclusive engine features and keep dependency cost visible.
PocketPy uses its official C11 amalgamated release; its cold initialization is
timed inside fresh processes, excluding process launch. Differences around a
few hundredths of a microsecond are timer and harness noise.

| Runtime | VM/compiler startup median | P95 | Parse + evaluate median | Cached function → host median |
| --- | ---: | ---: | ---: | ---: |
| Lua 5.4 | 34.166 µs | 59.458 µs | 1.709 µs | 0.042 µs |
| Luau 0.728 | 56.750 µs | 89.125 µs | 8.791 µs | 0.166 µs |
| Rhai 1.25.1 | 146.041 µs | 225.417 µs | 0.833 µs | 0.291 µs |
| QuickJS-NG 0.15.1 via rquickjs 0.12.2 | 180.584 µs | 288.208 µs | 4.041 µs | 0.166 µs |
| PocketPy 2.1.8 | 543.916 µs | 636.333 µs | 0.958 µs | 0.041 µs |
| Fennel 1.6.1 | 4.934 ms compiler load | 5.189 ms | 54.500 µs compile + evaluate | 0.042 µs on Lua |
| Steel 0.8.2 | 64.715 ms | 67.207 ms | 35.875 µs | 0.125 µs |

TypeScript 7.0.2 caught the included intentional error before execution:

```text
spikes/typescript-quickjs/examples/type-error.ts(14,3): error TS2322:
Type 'string' is not assignable to type 'number'.
```

The native compiler executable was 23 MB on arm64 macOS. A fresh one-file check
took about 190 ms and peaked around 90 MB; Quirl must therefore load a persistent
language-service worker lazily and keep it off the first-prompt path. The
embedded QuickJS-NG spike executable was 1.4 MB.

The official Luau analyzer also caught the equivalent intentional error:

```text
spikes/luau/examples/type-error.luau(16,15): TypeError:
Expected this to be 'number', but got 'string'
```

The standalone release analyzer was 6.9 MB and completed this one-file check in
about 10 ms at process precision. Quirl should keep an analyzer service warm so
interactive edits do not repeatedly pay process startup.

## Developer-experience assessment

This is a product-fit assessment, not a developer popularity survey. Language
familiarity is only a proxy; the remaining validation should include user tests
with shell, Python, JavaScript/TypeScript, Lua, and Lisp users.

The broad adoption signal strongly favors TypeScript- and Python-shaped syntax:
[GitHub's 2025 Octoverse](https://github.blog/news-insights/octoverse/octoverse-a-new-developer-joins-github-every-second-as-ai-leads-typescript-to-1/)
reported TypeScript as its most-used language by monthly contributors, with
Python second, while the
[2025 Stack Overflow survey](https://survey.stackoverflow.co/2025/) reported
another seven-point year-over-year increase for Python. That argues for familiar
syntax and strong typing, but does not make V8 or CPython-shaped compatibility
free. PocketPy cannot promise CPython's ecosystem, and AssemblyScript is a
TypeScript variant rather than TypeScript itself. TypeScript 7 plus QuickJS-NG is
the first candidate in this spike that preserves the real TypeScript language
and checker without carrying V8.

| Candidate | Before-run safety | Familiarity and editor story | Embedding and performance | Product fit |
| --- | --- | --- | --- | --- |
| **TypeScript 7 + QuickJS-NG** | Mature strict checker, still unsound around `any`, assertions, and unchecked libraries | Broadest familiarity and strongest editor/AI ecosystem; Quirl generates `quirl.d.ts` | 181 µs runtime startup and 0.166 µs warm host call; native checker is a lazy 23 MB sidecar; no Node/Bun APIs by default | **Front-runner in this latency-only round**; later demoted by complete-system complexity |
| **Luau strict** | Strong gradual checker with inference; strict mode can still be escaped through dynamic types and casts | Familiar Lua-shaped syntax; analyzer powers warnings and autocomplete; community LSP exists; no native Rust-style `Result` propagation operator | Designed for embedding; 57 µs startup, sandboxing, interruption and optional JIT | **Lightweight fallback**; best integrated runtime, pending Rust analyzer integration and error ergonomics |
| AssemblyScript → Wasm | Strict ahead-of-time checking; no `any`, but not full TypeScript | Very familiar surface for TypeScript users; separate build step; smaller ecosystem than TypeScript | Fast cached Wasm; rich strings/records need generated ABI glue; compiler is a substantial toolchain | Best typed, isolated plugin experiment; not the default config/REPL language yet |
| MoonBit → Wasm | Static types, declared error effects, `Result`, and generated WIT component bindings | Modern integrated formatter/checker/test/docs workflow, but a new language with little training data | First-class Wasm/component targets and compact output | Strong technical plugin candidate; evaluate maturity, governance, and redistribution terms before adoption |
| TypeScript + V8 | Same checker as the QuickJS-NG design | Adds Node-compatible runtime expectations and a huge ecosystem | V8 materially increases binary, memory, build, and cold-start costs | Rejected for the base shell; use QuickJS-NG or an external Node/Bun runner |
| Steel | Contracts are primarily runtime checks, not the requested static guarantee | Powerful Scheme/macros and an LSP, but unfamiliar to most developers and pre-1.0 | Excellent Rust boundary and warm calls; 64.7 ms VM construction in this spike | Keep as research/optional runner unless its static-analysis story changes |
| PocketPy | Accepts annotations but does not provide a comparable built-in static checker | Python syntax is highly approachable; PocketPy is not CPython and cannot promise the full PyPI/C-extension ecosystem | Compact C11 runtime and sub-millisecond startup; Rust bridge is low-level and current crates lag releases | Attractive optional Python-shaped runner, not the typed core |
| Rhai | Dynamic; Rust-like syntax does not make script values statically typed | Pleasant for Rust users and an excellent Rust registration API; smaller editor/ecosystem footprint | Very fast startup and evaluation; pure Rust integration | Great embedder, but it fails the main safety gate and adds another unfamiliar language |
| Lua / Fennel | Dynamic | Mature Lua tooling; Fennel adds expressive macros but is niche | Smallest mature runtime and fastest boundary in this spike | Runtime reference or optional compatibility runner, not the typed core |

## Why TypeScript 7 + QuickJS-NG won the latency-focused round

1. It uses real TypeScript rather than a look-alike, so existing editor
   knowledge, AI training data, syntax, declaration files, and diagnostics
   transfer directly.
2. The native TypeScript 7 compiler and language service are roughly an order of
   magnitude faster than the former JavaScript implementation and can live in a
   lazy worker. Quirl generates `quirl.d.ts` from the same semantic catalog used
   by runtime bindings, docs, completions, and AI discovery.
3. QuickJS-NG is a small embeddable interpreter. Its measured runtime startup is
   below 0.3 ms P95, so config and small scripts keep an immediate edit-run loop
   without shipping V8 in the shell process.
4. TypeScript has native discriminated unions and familiar `Result<T, E>`
   narrowing, which fits Quirl's structured error contract better than
   exception-only scripting APIs.
5. The separation is clean: TypeScript checks and emits cached JavaScript;
   QuickJS-NG executes only after the check passes and receives no ambient Node,
   filesystem, process, or network APIs—only explicit Quirl capabilities.

The important caveats are honesty and cost. TypeScript is not sound; Quirl
should require strict flags, reject unchecked emission for packages and CI,
lint explicit `any` and unsafe assertions, and make capability effects explicit
in generated host types. QuickJS-NG is not Node or Bun, so Quirl must not imply npm
package compatibility beyond audited runtime-independent modules. The compiler
worker's roughly 23 MB executable and 90 MB cold-check peak are real costs. If
those costs are unacceptable, strict Luau is the fallback. If the project
requires sound static typing with no dynamic escape hatches, choose an
ahead-of-time Wasm language and accept a build step.

## Next acceptance spike

Before changing the configuration and plugin contract, build one thin
TypeScript/QuickJS-NG vertical slice and the matching Luau control:

- generate declarations for `Command`, `Value`, `Result`, `ShellError`, streams,
  and capability handles from the Rust schema;
- generate `quirl.d.ts` and `quirl.d.luau`, then prove ergonomic, statically
  narrowed error propagation with the same discriminated `Result<T, E>` model;
- type-check a multi-file script, request completion after `quirl.process.`, and
  return structured diagnostics without spawning a process;
- execute equivalent scripts through QuickJS-NG and Luau with matching runtime
  bindings and cached checked bytecode;
- verify deadline interruption, memory limits, sandboxed libraries, async
  command cancellation, and deterministic module loading;
- measure record/list/result conversion, a 100,000-row transform, warm analyzer
  latency, resident memory, and binary-size delta;
- repeat runtime measurements with the optional Luau JIT on arm64 and x64;
- compile the same WIT plugin in AssemblyScript and MoonBit, then compare check
  latency, artifact size, typed `Result` ergonomics, generated host glue, and
  the licensing/support implications of bundling each toolchain.

Only then should the review board settle TypeScript/QuickJS-NG, strict Luau, or
retaining Steel. Maintaining more than one as a privileged configuration and
trusted-plugin language is not the recommended outcome.

## Reproduce

The Rhai measurements above are retained as historical selection evidence. The
active workspace no longer builds Rhai; its isolated footprint probe remains in
`spikes/footprint` for explicit research runs.

```console
# Lua and optional Fennel
cargo run --release -p quirl-bench -- \
  --fennel /tmp/quirl-fennel-1.6.1.lua --json

# Luau in an isolated mlua build
cargo run --release --manifest-path spikes/luau/Cargo.toml

# QuickJS-NG runtime and the native TypeScript 7 checker
cargo run --release --manifest-path spikes/typescript-quickjs/Cargo.toml
npx --yes --package typescript@7.0.2 tsc \
  --project spikes/typescript-quickjs/tsconfig.json --pretty false

# The same official Luau 0.728 analyzer used for the type-error probe
git clone --depth 1 --branch 0.728 \
  https://github.com/luau-lang/luau.git /tmp/quirl-luau-0.728
make -C /tmp/quirl-luau-0.728 config=release luau-analyze -j4
/tmp/quirl-luau-0.728/luau-analyze \
  spikes/luau/examples/type-error.luau

# PocketPy 2.1.8; downloads and checksum-verifies official release sources
python3 spikes/pocketpy/run.py
```

The earlier detailed Fennel experiment remains in
[spike 2](steel-lua-fennel.md).
