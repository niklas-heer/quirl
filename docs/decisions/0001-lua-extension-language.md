# ADR 0001: Lua is Quirl's extension language

- Status: Accepted
- Date: 2026-08-15
- Decision owners: Quirl maintainers
- Applies to: configuration, automation scripts, trusted plugins, prompt components, completion providers, and keymaps

## Context

Quirl's shell, parser, executor, built-ins, structured value model, terminal UI,
and performance-critical paths are implemented in Rust. The embedded language is
therefore an extension surface rather than Quirl's implementation language.

The evaluated candidates included Steel, Lua, Fennel, Rhai, Luau, PocketPy,
Gluon, and TypeScript on QuickJS-NG. Strict Luau offered the strongest balance
when ahead-of-time type checking was treated as a hard requirement. That choice
also introduced a second language identity, a C++ analyzer bridge, revision
coordination between analyzer and VM, and an explanation burden for users who
already recognize Lua.

For Quirl's actual extension scope, approachability, longevity, a small runtime,
and a mature Rust embedding API are more valuable than making the guest language
soundly statically typed. Adjacent developer tools such as Neovim and WezTerm
also make Lua a familiar configuration and plugin language for the target
audience.

## Decision

Quirl will use **Lua 5.4** as its only first-class embedded language for the
initial product. Rust remains the implementation language.

- `~/.config/quirl/config.lua` is the canonical configuration source.
- `.lua` is the native extension for Quirl scripts and trusted plugins.
- Quirl embeds Lua through `mlua`, with the Lua implementation vendored or
  otherwise pinned for reproducible builds.
- Rust definitions are the source of truth for the host API. The build generates
  Lua runtime bindings, LuaLS-compatible annotations/stubs, completion metadata,
  human documentation, and AI-readable schemas from the same definitions.
- `quirl check` performs parsing, linting, module/capability validation, annotation-
  aware analysis where available, and schema validation without executing the
  script. It does not claim sound static typing for arbitrary Lua programs.
- Every value crossing the Lua/Rust boundary is validated by Rust. Configuration
  is constructed through schema-backed builders and is committed only after full
  validation; failure preserves the last-known-good configuration.
- Trusted Lua runs in-process with explicit capability handles, restricted module
  loading, instruction/time budgets, memory limits, cancellation, and structured
  error conversion. Untrusted or portable plugins use WebAssembly or an
  out-of-process boundary.
- Luau, Rhai, Steel, TypeScript, and other engines remain research results or
  possible optional runners. They are not additional core configuration or
  plugin languages.

## Consequences

Positive consequences:

- Users see a well-known language rather than a Quirl-specific dialect choice.
- The runtime has the smallest measured binary and memory footprint in the spike.
- `mlua` provides a mature Rust integration surface.
- Existing Lua knowledge, editor support, formatters, documentation, and examples
  are useful immediately.
- Quirl owns one extension SDK and one set of examples across configuration,
  scripts, and plugins.

Accepted costs:

- Lua is dynamically typed. Annotations, linting, and schema checks catch many
  mistakes before execution, but do not prove arbitrary program correctness.
- Quirl must make generated SDK annotations and boundary diagnostics unusually
  good to deliver the intended IDE experience.
- Sandboxing requires an intentionally restricted standard library and module
  loader; plain Lua is not secure merely because it is embedded.
- The local configuration editor must patch supported literal forms conservatively
  and treat computed expressions as code-controlled.

## Next implementation milestone

Build one vertical slice and remove the existing prototype runtime:

1. Add a `quirl-lua` crate using pinned Lua 5.4 through `mlua`.
2. Define a small Rust host schema and generate both runtime bindings and
   LuaLS-compatible `quirl` stubs from it.
3. Run `examples/config.lua`, `examples/hello.lua`, and one prompt/completion
   plugin through the same persistent VM and structured `Result` boundary.
4. Implement `quirl check`, `quirl fmt`, `quirl lint`, and `quirl test` for that
   slice, returning the same terminal and JSON diagnostics.
5. Prove restricted module loading, capability denial, cancellation, instruction
   and memory budgets, last-known-good config reload, and useful stack traces.
6. Record cold start, reload latency, host-call latency, binary delta, and peak RSS
   in CI. The existing Lua footprint result is the baseline, not the acceptance
   result for the complete SDK.

The slice is accepted when one documented Lua API powers configuration, a script,
and a plugin; editor completion and hover work from generated metadata; invalid
host values fail before state changes; and resource limits stop runaway code
without taking down the shell.

## Implementation status

The initial vertical slice landed with this decision:

- `quirl-lua` embeds pinned, vendored Lua 5.4 through `mlua`.
- `quirl run`, `eval`, `check`, `fmt`, `lint`, `test`, `config check`,
  `plugin check`, and `sdk` expose the first authoring workflow.
- Rust host definitions generate LuaLS stubs, Markdown, and JSON SDK views; the
  checked-in `docs/quirl.lua` is verified against the generator by tests.
- Example configuration, script, prompt segment, completion provider, and Lua
  tests exercise the same restricted runtime.
- Ambient `io`, `os`, `debug`, file-loading, and package-loading APIs are removed.
  Process execution is capability-gated and denied during configuration/plugin
  validation.
- Memory, instruction, wall-clock, and cancellation checks protect the VM.
- Configuration is deserialized into Rust schemas, and a failed reload leaves the
  last-known-good configuration unchanged.
- Registered prompt and completion callbacks remain in persistent VMs and feed
  the live Reedline prompt and IDE completion menu through typed Rust adapters.
- Interactive data mode now evaluates native Rust-owned structured sources and
  transforms.
- The prototype runtime, CLI bridge, workspace crate, dependencies, examples,
  and benchmark executable paths have been removed. It remains mentioned only
  in historical evaluation evidence documenting why Lua was selected.

The next integration step is live configuration watching and atomic session
reload, applying the selected keymap/prompt/picker settings to the running UI,
and expanding the native data grammar without turning it into a second general-
purpose language.

## Revisit conditions

Reconsider Luau only if real Quirl plugins show that Lua annotations and schema
validation are insufficient, and only if Luau can preserve a Lua-compatible user
story without creating two core ecosystems. Reconsider the Lua version when the
chosen `mlua` release and the surrounding tooling ecosystem make an upgrade
clearly lower-risk than remaining on 5.4.
