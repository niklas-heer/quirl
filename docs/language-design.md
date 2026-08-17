# Quirl language and product design

**Product direction with an evidence-bounded current contract · 16 August 2026**

> **The development environment in your terminal.** Your shell should feel as smart as your editor.

Quirl combines familiar Bash/Zsh command entry, typed data pipelines, Rust
performance, and a well-tooled Lua extension language with an IDE-grade editing
experience. Completion, diagnostics, documentation, prompt context, and
interactive views are one product—not a pile of plugins.

## Current 0.1 candidate contract

This is the implemented contract in the integrated source tree, not a claim
that a release artifact has been measured or published:

- On Linux and macOS, the native C1-core includes byte pipelines, redirects,
  boolean lists, bounded expansion/substitution, jobs, and explicit Bash/Zsh
  islands for unsupported control syntax. Windows remains best-effort process
  portability rather than a supported interactive target.
- Data mode is a focused, bounded runtime rather than a general type system. It
  supports the sources, bridges, transforms, materialization points, limits,
  and exclusions documented in [the data runtime](data-runtime.md); bytes and
  values only cross at named boundaries.
- Restricted Lua 5.4 runs configuration, scripts, and trusted-Lua plugin
  commands under the typed runner ABI v1. `HOST_API` is the complete executable
  Lua interface; generated stubs and prose are projections of it.
- Managed plugins have locked grants, bounded event/panel/completion work, and
  typed command dispatch. Wasm components validate but do not execute.
  MCP exposes builtin tools only and neither reads nor executes installed
  plugins.
- The semantic catalog, LSP, rich/simple terminal surfaces, process lifecycle,
  recovery, and their bounded tests are present. `Catalog::builtin()` is the
  complete builtin-command contract; this document does not duplicate it.

The current release remains **unreleased 0.1**. In this document, “1.0” is
either a historical/accepted scope label from an ADR or a long-term direction;
it is not a current version, distribution, or support claim. Historical
benchmark records apply only to their named candidates, artifacts, environments,
and methods. The source-of-truth and evidence order is defined in [the
documentation system](documentation-system.md).

<!-- BEGIN QUIRL RELEASE EVIDENCE STATUS -->
> **Release evidence status — historical.** Artifact evidence for measured candidate `23fd5d36907fc816bdafd9aa3c2dcb3afb69feb5` and artifact `9a893a5f1a0b49d62712f331c88966113d910d94efa9651dc4feffe9fd55b637` is historical.
> Evidence commit `14e70939d039d96c195f57452a0e1ec3928194af` documents that measurement. It is not evidence for the corrected implementation, which has no fresh exact-candidate measurement.
> Human review on named Linux and macOS terminals, remote-PTY review, and real-terminal demo review remain incomplete.
<!-- END QUIRL RELEASE EVIDENCE STATUS -->

## Long-term direction and illustrative design

The numbered sections below preserve intended experience, alternatives, target
budgets, and illustrative syntax. Except where a statement is repeated in the
current contract above or an accepted ADR/runtime source, it is direction—not
landed behavior or release evidence.

```text
quirl · ~/src/payments                         command · git: feature/typed-runners · rust 1.88
> quirl run deploy.lua --env staging
  hint: incomplete value; Tab accepts `staging` declared by deploy.lua:12
▦ ls services | where status == "degraded" | preview
  3 rows
```

## 1. Product thesis

### One shell, three promises

1. **Arrive — old knowledge still works.** External commands and common interactive Bash/Zsh syntax should paste and run without translation. Migration begins at zero, not at a tutorial.
2. **Grow — new power is explicit.** A visible data mode changes the pipeline contract from bytes to values. Quirl never guesses which grammar the user meant.
3. **Create — developer experience is the product.** The prompt is a semantic editor: live diagnostics, typed completion, hover docs, previews, command palette, history, and project intelligence ship together.

> **Central design decision:** do not create one magical grammar that heuristically guesses between Bash, Zsh, Nu, and a programming language. Create two legible modes, explicit bridges, and one shared runtime.

## 2. Principles and boundaries

Compatibility, structured data, rich UI, and a real extension language pull in different directions. The design works only if it names the boundary of each promise.

### Must be true

- Instant, predictable interactive startup.
- The first launch is already a great shell—no framework, prompt, fuzzy finder, or plugin pack required.
- Plain command execution stays boring.
- Mode is always visible and keyboard-switchable.
- Values remain values until an explicit boundary.
- Failures carry type, span, cause, and recovery hint.
- Rich UI degrades to clean text over dumb terminals, logs, and SSH.
- Everything built in is scriptable through a stable API.

### Non-goals for 1.0

- Bit-for-bit emulation of every Bash and Zsh edge case.
- Sourcing framework-sized `.bashrc` or `.zshrc` files into Quirl state.
- Replacing every Unix utility.
- Owning terminal scrollback or becoming a terminal emulator.
- Making native in-process plugins a stable ABI.
- Guessing schemas from arbitrary text.
- Promising a native Windows interactive experience without a maintained
  Windows test environment.

> **Product anchor: Helix’s coherence, Bun’s completeness.** Quirl ships one deliberately integrated workflow: install one binary and immediately get editing, completion, fuzzy discovery, prompt context, structured tools, scripting, testing, formatting, documentation, and configuration. “Batteries included” means designed together and supported together—not a bundle of unrelated replacements for every Unix tool.

> **Name decision: Quirl.** A *Quirl* is a German kitchen whisk—roughly pronounced “kvirl”—from a word family associated with turning and stirring. Product name: **Quirl**; binary: `quirl`; native scripts: canonical `.qrl` (with readable `.quirl` and novelty `.🌀` input aliases); Lua scripts and configuration: `.lua`; environment prefix: `QUIRL_`.

## 3. Interaction model

### A modal shell, not a modal trap

Like Vim, Quirl gains power by changing what syntax means in a visible mode. Unlike Vim, every action has a textual form so sessions are reproducible and accessible. Open Quirl's leader menu with `Alt-Q`, then press `n`, `d`, or `i` for normal, data, or AI mode.

| Mode | Contract | Examples | Explicit entry |
| --- | --- | --- | --- |
| Normal `❯` | Bytes and processes; commands resolve through the session `PATH`; non-zero exits set status; familiar process control | `docker ps \| grep healthy`, `ls -al`, `false \|\| echo recovered` | `mode normal` (legacy: `mode command`) |
| Data `▦` | Unambiguous Quirl grammar; typed, lazy values; external programs require an adapter or `^command`; failures are `Result` values | `ps \| where cpu > 20 \| sort cpu desc`, `open users.json \| get users \| select name email` | `mode data`; one-shot `data { ... }` |
| AI `✧` | Live command and option discovery over the SQLite catalog; the pinned potion-base-8M model downloads and indexes automatically after first paint. Enter inserts the selected suggestion into normal mode for review and never executes it directly. | `copy a directory while preserving permissions`, `find the option that follows symlinks` | `mode ai`; `Alt-Q i` selects it. `mode natural`, `mode nl`, and `mode human` remain aliases. |

```quirl
▦ http get /health | match {
    { status: 200, .. } => "up"
    error => error?
  }
```

> **Scripts never depend on invisible mode.** Interactive mode is session state. In native Quirl files, `data { ... }` and `command { ... }` make every grammar boundary explicit. Their opening and closing delimiters occupy their own lines, with the closing `}` aligned to the opener; commands inside a `command` block run one non-comment line at a time. The older one-line `data <expression>` and command form remains accepted for compatibility. Lua remains the general-purpose scripting language.

```quirl
data {
  [1, 2, 3] | length
}

command {
  printf 'native command block\n'
}
```

`quirl check` and `quirl lint` validate native block delimiters and command
grammar without running source. They also use `quirl-data`'s bounded,
side-effect-free parser for data bodies, so syntax diagnostics preserve the
data-statement span without opening files, invoking adapters, resolving the
current directory, or executing `^external`. Formatting and data-token
highlighting consume the same AST. Evaluation semantics beyond the focused
surface remain owned by the later typed evaluator work.

## 4. Command intelligence

### Completion is how users learn the interface

Quirl builds a versioned semantic catalog of commands, subcommands, arguments, types, examples, effects, and documentation. Completion, highlighting, hover, validation, generated docs, AI discovery, and plugins all consume that catalog.

**Command intelligence path:** discover interfaces → ingest schemas → normalize `CommandSpec` → index a versioned cache → serve every consumer (prompt, docs, LSP, AI, plugins).

```text
type CommandSpec = {
  id: CommandId, version: String?, aliases: List<String>,
  summary: String, documentation: Markdown,
  subcommands: List<CommandSpec>,
  arguments: List<{
    names: List<String>, kind: positional | option | flag,
    value_type: Type, required: Bool, repeatable: Bool,
    values: CompletionSource?, conflicts: List<ArgumentId>,
    documentation: Markdown, examples: List<Example>,
  }>,
  io: { input: Type, output: Type, streaming: Bool },
  effects: Set<Effect>, exit_codes: Map<Int, String>,
  provenance: { source, trust, fingerprint, generated_at },
}
```

| Moment | Behavior |
| --- | --- |
| At the cursor | Suggestions show signature, type, summary, default, examples, conflicts, pipeline input/output, side effects, and provenance. The documentation panel follows selection without stealing focus. |
| As you type | Highlight built-ins, external and script commands, flags, enum values, paths, variables, types, redirects, and errors. Existence, deprecation, trust, and destructive effects are visible without color alone. |
| Ranking | Type compatibility, current subcommand, project, cwd, platform, recent local use, and exact prefix outrank global frequency. History can rank a valid item; it cannot invent an interface. |
| Performance | The memory-mapped catalog answers immediately. Filesystem and project providers stream additions with cancellation. Selection never jumps when late results arrive. |

### Digest the completion ecosystem we already have

Quirl imports existing definitions at index time and records source and confidence. It does not execute arbitrary shell startup files on every keystroke.

| Source | Ingestion strategy | Trust and fallback |
| --- | --- | --- |
| Native Quirl commands | Rust attributes, doc comments, types, generated `CommandSpec` | Exact and compile-time checked |
| Lua script commands | Generated SDK annotations, docstrings, argument descriptors, exports, package manifest | Host schema is exact and module-versioned |
| Zsh completions | Translate common `_arguments`, `_describe`, and `_values` forms in an isolated Zsh worker | Cache translation; bounded worker fallback for dynamic providers |
| Bash completions | Read `complete -p`; instrument common helpers and capture candidate metadata | Never implicitly source user RC files; sandbox dynamic functions |
| Fish completions | Import declarative `complete` definitions and descriptions | High-confidence translation; worker for dynamic values |
| Help and man pages | Parse usage, headings, options, subcommands, defaults, enums, examples | Heuristic, labeled with provenance and confidence |
| Project manifests | Cargo, package scripts, task runners, containers, project-local `.qrl` commands | Scoped to the trusted project root |

```sh
quirl index build                  # scan PATH and project sources
quirl index watch                  # refresh changed commands in the background
quirl index explain cargo test     # show every fact and its provenance
quirl complete --buffer "git sw" --cursor 6 --format json
quirl describe cargo.test --format terminal
quirl describe cargo.test --format markdown
```

> **Dynamic completion is contained.** An imported provider runs in a disposable compatibility worker with a deadline, cancellation, restricted environment, and no network by default. Results are cached by executable version, provider fingerprint, cwd class, and relevant environment keys.

### Documentation that cannot drift

In Rust, source prose is written as doc comments such as `///`; attributes and derive/procedural macros attach machine-readable command metadata. The build emits one catalog into the binary. Script docstrings and checked signatures emit the same schema.

```rust
#[quirl::command(
  name = "deploy",
  input = "Stream<Service>",
  output = "Result<Deployment>",
  effects = ["process", "network"]
)]
/// Deploy services to a named environment.
///
/// Production requires interactive confirmation unless --yes is present.
fn deploy(
  /// Target environment discovered from the project manifest.
  #[quirl(values = environments)] environment: Environment,
  #[quirl(long, conflicts_with = "dry_run")] yes: bool,
) -> Result<Deployment, DeployError> { ... }
```

| Generated from the same source | Views |
| --- | --- |
| Humans | Terminal help, hover cards, examples, error hints, searchable command palette |
| Documents | Markdown, man pages, static HTML, package documentation |
| Machine clients | Canonical JSON, JSON Schema, completion protocol, LSP metadata |
| Other shells | Generated Bash, Zsh, Fish, PowerShell completion definitions |
| AI agents | Token-budgeted context, tool manifest, validation schemas, optional MCP server |

### A deterministic interface for AI

Agents should not depend on training data or scrape styled help. Quirl exports exactly the installed capabilities, versions, types, examples, permissions, and validators in stable formats.

```sh
quirl catalog list --format json
quirl describe deploy --format json
quirl agent context "deploy the billing service" --format markdown --token-budget 6000
quirl agent manifest --format json
quirl check deploy.lua --format json
quirl lint deploy.lua --format json
quirl fmt deploy.lua --check
quirl serve mcp --capabilities catalog,complete,check,format
```

- **Discovery:** manifests contain only installed commands and plugins, with schema hashes and versions; agents can request the smallest relevant subtree.
- **Validation:** `check` parses, resolves, lints, validates annotations, modules, capabilities, and known host schemas without executing Lua. JSON diagnostics include codes, spans, fixes, effects, and documentation links, without claiming sound static typing for arbitrary Lua.
- **Safety:** catalog discovery does not grant authority. Agent execution uses the same permission, preview, confirmation, and audit path as human commands.
- **Freshness:** catalog generation belongs to compilation and package validation. Public commands without summaries, argument docs, types, examples, effects, or error codes fail the project’s quality gate.

### The complete authoring toolchain ships with the shell

```sh
quirl new script
quirl new script --lang quirl
quirl check src/main.lua
quirl run src/main.lua
quirl fmt .
quirl check .
quirl lint .
quirl test
quirl doc --open
quirl package build
quirl package publish --dry-run
```

## 5. Compatibility contract

### “Compatible” needs a testable meaning

Quirl targets the copy-and-paste surface first. Full Bash and Zsh are separate evolving languages with contradictory semantics; claiming complete dual compatibility would make the new language permanently ambiguous.

| Level | Promise | Examples | Implementation |
| --- | --- | --- | --- |
| C0 · Commands | Required for preview | `git status`, quoting, redirects, byte pipes, jobs | Native process engine and POSIX-like lexer |
| C1 · Common interactive | Required for 1.0 | `&&`, `||`, globs, bounded substitutions, here-strings, `export NAME=value` | Native compatibility AST on Linux/macOS; corpus against Bash and Zsh |
| C2 · Dialect islands | Required for 1.0 | `bash { ... }`, `zsh { ... }`, shebangs | Reference-interpreter subprocess with structured capture |
| C3 · Source state | Deliberately partial | Import cwd, exported env, aliases when representable | State-diff protocol; reject traps and opaque functions |
| C4 · Exact emulation | No promise | Every option, trap, framework, obscure expansion | Run the requested `bash` or `zsh` |

```sh
# Ordinary input: native, fast, familiar
cargo test --workspace && echo 'tests passed'

# Dialect island: explicit and exact
bash {
  for crate in crates/*; do
    cargo test -p "${crate##*/}" || break
  done
}

# A script keeps its own authority
./legacy-deploy.sh        # kernel/shebang chooses its interpreter
bash ./legacy-deploy.sh   # explicit always wins
```

- Valid C1 input executes natively.
- Bash- or Zsh-only constructs produce a mismatch diagnostic and the exact dialect form; Quirl never silently reinterprets them.
- Here-documents, process substitution, loops, functions, conditionals, and
  dialect control syntax are intentionally C2 islands for 1.0, not an unfinished
  native-syntax promise. See [ADR 0010](decisions/0010-unix-first-release-scope.md).
- `source` accepts Quirl modules and a portable subset. `source --bash` and `source --zsh` import only representable state.
- Behavior is versioned in a machine-readable matrix and backed by differential tests.

## 6. Command and data surface

### Small shell syntax. Strong values. Streaming by default.

Quirl’s interactive data notation is expression-oriented and gradually typed, but deliberately remains a focused shell surface. It is parseable, formattable, and lintable; general-purpose modules and scripts use Lua 5.4.

**Pipeline stages:** producer → `Stream<T>` with provenance → lazy, backpressured transform → `Result` → table/JSON/text/TUI/API renderer.

| Family | Types | Notes |
| --- | --- | --- |
| Scalar | `Bool`, `Int`, `Decimal`, `String`, `Bytes`, `Nothing` | No implicit string-to-number conversion |
| Domain | `Path`, `Duration`, `Size`, `DateTime`, `Pattern` | Units and paths are first-class, platform-aware values |
| Structured | `List<T>`, `Record`, `Table`, `Stream<T>` | Records may be open interactively and closed in typed signatures |
| Effects | `Result<T,E>`, `Option<T>`, `Task<T>`, `Command` | Failure, absence, concurrency, and processes are not magic globals |

```quirl
# Literals carry meaning
let cutoff = now() - 7days
let limit: Size = 500mb

# Pipelines stream typed records
ls ./logs
| where modified < cutoff and size > limit
| select name size modified
| sort size desc
| take 20

# Reusable commands declare their contract
def stale-logs(root: Path = ".") -> Stream<Entry> {
  ls root | where type == file and modified < now() - 30days
}

# Destructuring and exhaustive matching
match (http get "https://service.test/health") {
  ok({ status: 200, body, .. }) => body,
  ok({ status, .. })            => error("unexpected status", status),
  err(e)                        => e.context("health check failed")?,
}
```

- `|` has exactly one meaning per mode: byte pipe in command mode, value pipe in data mode. In files the surrounding block fixes it; runtime inspection never changes syntax.
- `from json`, `to json`, `lines`, and `^external` make byte/value boundaries visible. Automatic conversion is limited to declared schemas.
- Values may retain spans, file origins, and command origins so diagnostics can trace a pipeline without contaminating equality or serialization.
- Start type checking with command input/output signatures and local inference; decide later whether generic user types and traits belong before 1.0.

## 7. A first-class `ls`

### The directory is data, not terminal paint

Data mode’s `ls` is an alias for the built-in `files` producer of `Entry`
records. Rendering occurs only at the terminal boundary, so the same typed
source can power a grid, table, JSON, filter, or interactive browser. Normal
mode never intercepts the name: `ls` there is the executable discovered through
the session `PATH`, with that executable's flags and byte output.

```text
type Entry = {
  name: String, path: Path,
  kind: file | directory | symlink | other,
  size: Size, modified: DateTime?,
  hidden: Bool, readonly: Bool,
  target: Path?,
}

▦ ls                         # adaptive human view
▦ ls | where kind == directory # structured filter
▦ ls src | sort size desc    # typed path argument and transform
▦ ls | view tree             # renderer chosen explicitly
❯ ls | grep Cargo            # command mode emits compatible text
❯ ls -la                     # system flags pass through unchanged
```

- **Bounded fast path:** names, kind, size, modified time, hidden state, and
  readonly state are collected non-recursively under an explicit entry limit;
  the data runtime owns that bound rather than presenting system `ls` flags.
- **Stable values:** JSON and typed pipelines receive undecorated entry values;
  human output escapes terminal controls and stays one row per entry. The 0.1
  string ABI lossily represents non-UTF-8 Unix names and records that limit
  rather than claiming byte-perfect paths.
- **Deterministic rendering:** `--plain`/`--format plain`, long rows, JSON,
  sorting, reversal, and directory grouping are stable across filesystem
  iteration order.
- **Explicit residual:** `ls --browse`, Git/owner/MIME enrichment, and adaptive
  grid rendering remain future UI work; the existing directory panel is a
  safe read-only view, not a substitute claim.

## 8. Error model

### Failures should be values before they become messages

The runtime uses a Rust-like `Result` contract. Interactive command mode preserves traditional exit-status behavior; data mode and Quirl scripts make failure explicit, composable, and hard to ignore accidentally.

| Context | Failure behavior |
| --- | --- |
| Command mode | Compatible exit status, stderr, job state, and a rich diagnostic when Quirl owns the error |
| Data mode | `Result<T, ShellError>` moves through the pipeline until handled, propagated with `?`, or rendered |
| Lua/scripts | Typed error values cross the host boundary and retain context, labels, spans, and recovery help |
| JSON tooling | The same `ShellError` serializes with codes, labels, context, and help |

```text
E204 · external command failed
× health check returned exit status 22
  ┌─ deploy.lua:18:10
18│ return quirl.process.run("curl", { "--fail", endpoint })
  │          ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ command exited 22
  = help: inspect the endpoint, or handle the `Result` before continuing
```

```text
type ShellError = {
  code: ErrorCode, message: String,
  labels: List<Label>, context: List<ContextFrame>, help: String?,
}
```

Diagnostic rendering supports human, compact, JSON, and GitHub Actions formats from the same error value. Secrets are redacted at capture boundaries.

## 9. Embedded language and runners

### One familiar extension language, chosen on evidence

Quirl needs one canonical general-purpose language for scripts, configuration, custom commands, prompt components, keymaps, and trusted UI extensions. The concise command/data notation remains the interactive shell surface; it does not grow into a second general-purpose language.

> **Accepted: Lua 5.4.** Rust remains Quirl’s implementation language; Lua is the sole first-class language for configuration, scripts, and trusted plugins. Familiarity, a measured 0.58 MiB runtime probe, longevity, and the mature `mlua` bridge outweigh Luau’s stronger analyzer for this extension-only role. Read the [decision report](embedded-language-decision.md) and [ADR 0001](decisions/0001-lua-extension-language.md).

The first Lua slice embeds restricted Lua 5.4; generates LuaLS/JSON/Markdown SDK views; validates configuration with Rust schemas; atomically reloads config and plugins at safe prompt boundaries; and applies keymap, prompt, picker, and extension settings to the editor. The rejected prototype runtime and all executable/dependency paths are removed.

Quirl generates LuaLS-compatible annotations, completion metadata, docs, and AI schemas from Rust host definitions. `quirl check` parses, lints, resolves modules and capabilities, and validates known schemas without execution. Rust validates every host value before mutation, but Quirl does not promise sound static typing for arbitrary Lua.

| Candidate | Strength | Core risk | Position |
| --- | --- | --- | --- |
| Lua 5.4 | Recognizable syntax, tiny mature VM, established tooling and `mlua` bridge | Dynamic semantics require annotations, linting, schemas, boundary diagnostics | Accepted core extension language |
| Luau strict | Fast embedding, inference, checker, analyzer/autocomplete, sandboxing | New language identity, C++ analyzer bridge, version coordination | Revisit only if Lua evidence demands it |
| Rhai | Excellent pure-Rust API, resource controls, tiny startup | Smaller user ecosystem and less terminal-tool familiarity | Research result |
| TypeScript 7 + QuickJS-NG | Familiar typed surface, official checker/service, tiny JS VM | Go checker sidecar, TS→JS cache, source maps, three upstreams, ≈31 MiB installed and ≈99 MiB checker RSS | Demoted on total complexity |
| AssemblyScript → Wasm | Strict TypeScript-shaped syntax, AOT compilation, portable isolation | Not full TypeScript; build step and rich host-data glue | Typed plugin spike |
| MoonBit → Wasm | Static types, typed errors, integrated tooling, first-class WIT components | Young ecosystem; toolchain redistribution terms need review | Typed plugin spike |
| TypeScript + V8 | Node-compatible expectations and package reach | Runtime/build/cold-start tax; ambient APIs conflict with capability isolation | Rejected for base shell |
| Steel / Scheme | Rust integration, immutable values, contracts, macros, LSP | Contracts, 64.7 ms VM startup, unfamiliar syntax, pre-1.0 | Rejected; implementation removed |
| PocketPy | Familiar Python surface, portable C11 runtime, sub-ms startup | Annotations are not a static checker; not CPython/PyPI compatible | Optional runner candidate |
| Fennel | Lisp syntax and macros on selected Lua runtime | Another language identity and diagnostic-mapping layer | Possible optional runner |
| Native Quirl only | One syntax and one toolchain | Forces shell DSL to become a general language | Rejected for extensions |

Target-adjacent familiarity matters: TypeScript and Python dominate broad population measures, but Neovim and WezTerm show Lua is credible for terminal-tool configuration and plugins. Quirl chooses a language users can recognize and search for without inheriting Node or CPython expectations.

### One runner, multiple language engines

`quirl run` is the stable script entry point. It detects a shebang or known
extension, accepts `--lang`, and lowers once into the same validated execution
plan as other front doors. Every engine receives the same bounded arguments,
environment, working directory, input/output intent, absolute deadline,
cancellation identity, effects, source identity, and structured outcome.

```lua
#!/usr/bin/env -S quirl run
---@param ctx quirl.Context
---@return quirl.RunnerResult
local function main(ctx)
  if ctx.cancellation.is_cancelled() then
    return {
      abi_version = 1, ok = false,
      error = {
        code = "resource_limit", message = "deployment was cancelled",
        help = { "Retry with a fresh execution request" },
      },
    }
  end
  return {
    abi_version = 1, ok = true, status = 0,
    output = {
      kind = "value",
      value = { type = "string", value = ctx.env.QUIRL_DEPLOY_ENV or ctx.args[1] },
    },
  }
end

return { abi_version = 1, main = main }
```

Runner ABI v1 captures bounded arguments, a UTF-8 environment view, cwd, typed
input/output intent, declared effects, and one shared cancellation identity.
The host's monotonic plan deadline covers VM construction and `main`; Lua's
memory, instruction, and callback policies may narrow it but never restart it.
Results use the shared tagged value and structured `ShellError` representations.
Only finite value batches of at most 512 items cross this ABI; live streams stay
engine-owned and are never collected merely to fit a Lua or JSON table.
Unversioned modules remain readable through the bounded v0 migration adapter,
while unknown and future versions fail before `main` runs. ADR 0018 is the
normative ABI and migration record.

```sh
quirl check deploy.lua --format json
quirl run deploy.lua --env staging
quirl run --lang lua - < generated.lua
quirl eval --lang lua "return 20 + 22"
quirl run --lang bash legacy.sh
quirl run --lang zsh release.zsh
```

| Script kind | Execution model | Status |
| --- | --- | --- |
| `.lua` | Pinned Lua 5.4 through `mlua`; generated annotations/bindings; restricted modules and explicit capabilities | Core commitment |
| `.qrl` | Focused command/data scripts lowered to the shared execution plan; `.quirl` and `.🌀` are input aliases | Core commitment |
| `.luau` / `.rhai` / `.ts` | Measured alternatives; no second core SDK or config ecosystem | Research or future optional runners |
| `.fnl` | Compile to cached Lua, then use the Lua engine and same host adapter | Companion to Lua |
| `.py` | Optional PocketPy with explicit compatibility manifest; never presented as full CPython | Research candidate |
| `.wasm` | Typed component boundary for isolated plugins; compare AssemblyScript with MoonBit Result/WIT | Platform direction |
| `.sh` / `.bash` / `.zsh` | Initially dispatch to reference interpreter; native execution grows by compatibility level | Staged compatibility |

| Contract | Decision |
| --- | --- |
| Annotated host capabilities | Rust schema emits runtime bindings, LuaLS stubs, completion metadata, docs, and AI schemas for prompt, commands, filesystem, process, network, data, and TUI. Schema hashes must match before code runs. |
| One value ABI | Shared tagged `Value`, bounded finite value batches, `Result`, and structured `ShellError` cross native commands, scripts, embedded code, and plugins; live streams remain engine-owned. |
| Capability manifest | Packages declare effects; first use prompts once; CI can enforce lockfile policy. Untrusted packages run out of process or as WebAssembly. |
| One Lua SDK | Configuration, scripts, and trusted plugins share one generated `quirl` module; the earlier prototype runtime is fully removed. |

```lua
-- ~/.config/quirl/plugins/project.lua
quirl.prompt.add_segment {
  name = "project", deadline_ms = 8,
  render = function(ctx)
    if ctx.git_root then return quirl.style(ctx.project_name, { fg = "accent" }) end
  end,
}

quirl.command.define {
  name = "ports", output = quirl.type.stream("Port"),
  run = function() return quirl.system.listening_ports() end,
}
```

The runtime spike evaluates cold start, warm invocation, host-boundary record/`Result` round trips, 100,000-record data work, binary/RSS/cached-module footprint, and tooling-load time. The Lua integration acceptance gates include parse/lint without execution, generated completion/hover metadata, P95 initialization ≤1 ms, warm scalar host boundary P95 ≤1 µs, memory/instruction budgets and interruption, restricted module loading, last-known-good configuration, and one SDK for scripts, config, commands, and trusted plugins.

## 10. Developer experience

### The shell is a semantic development environment

The input buffer is parsed continuously, understands command schemas and annotated Lua exports, and can explain what will happen before execution. Ratatui composes the active prompt region and opt-in panels without stealing normal terminal scrollback. The implementation contract for this surface — frame layout, editor core, completion popup, status bar, highlighting spans, degradation tiers, and milestones — is [tui-design.md](tui-design.md).

> **Release criterion:** a feature is incomplete until it has completion metadata, inline help, diagnostics, keyboard navigation, accessible text output, and timing instrumentation. Lua-defined commands participate through the same Rust-validated schemas as native commands.

**Editing loop:** incremental mode-specific parse → resolve command schema, inferred type, project → complete and diagnose → preview effects → execute and observe with progress, cancellation, and structured result.

| Surface | Capability |
| --- | --- |
| Edit | Tree-aware selection, syntax highlighting, paired delimiters, snippets, multi-cursor edits, Emacs and Vim keymaps, undo tree, paste safety |
| Know | Options, values, paths, history, man pages, project tasks, and types; every suggestion gives source and effect |
| See | Unknown flags, broken quotes, type mismatches, destructive-glob previews, and likely typos before Enter |
| Move | One keyboard-first palette for commands, history, directories, jobs, settings, snippets, documentation |
| Watch | Pin a pipeline as an updating table, sparkline, log view, or task panel; panels consume `Stream<T>` |
| Ask | `help`, hover, completion, examples, error codes, and schemas share a bundled documentation index |

### One fuzzy picker, everywhere

fzf proves fuzzy selection is a terminal primitive. Quirl includes a native, typed picker in the line editor and exposes the same engine to commands, scripts, and plugins. Users should not have to install a finder and wire shell-specific bindings merely to search history or files.

- **Typed selection:** displays labels, highlights, metadata, and previews but returns the original value. Picking a `Process`, `Path`, history entry, or plugin record never round-trips through lossy display text.
- **Shared muscle memory:** `Ctrl-R` or `Up` opens fuzzy history, `Alt-Q` owns Quirl-internal chords (`f` files, `c` directories, `p` actions, `j` jobs, `r` results), and `Shift-Tab` expands completion into the full picker. Conventional shell editing chords remain available.
- **Interactive contract:** single/multi-select, exact/fuzzy/inverse terms, source switching, live reload, safe previews, named actions, cancellable/backpressured providers, and virtualized rendering.
- **Script contract:** `pick` accepts byte lines or typed streams and emits selected values. `--query`, `--multi`, `--preview`, `--format`, and non-interactive fallback rules are testable.

```quirl
▦ ls **/*.rs | pick --display path --preview source | open
▦ ps | where cpu > 5 | pick --multi --preview process | kill --confirm
❯ git branch --format='%(refname:short)' | pick --query feature
```

### Lua configuration with synchronized views

`quirl config web <file>` opens a polished local configuration app; `quirl config tui <file>` presents the same model in the terminal. `config.lua` is the source of truth. Browser and TUI are schema-backed views of that file—not separate stores. The web view refreshes from the file on each page load and checks the source again before saving; it does not claim a background file watcher.

```lua
---@type quirl.Config
local config = quirl.config {
  schema_version = 3,
  editor = { keymap = "emacs", semantic_hints = true, banner = "full" },
  picker = { layout = "adaptive", preview = true },
  prompt = {
    symbols = "auto",
    left = { "directory", "git_branch", "git_state" },
    right = { "jobs", "duration", "status" },
  },
  ui = { theme = "tokyo-night" },
}

if quirl.project.is_rust() then
  config.prompt.right = { "rust_version", "jobs", "duration", "status" }
end

return config
```

| Concern | Contract |
| --- | --- |
| One language | Settings, prompt logic, keymaps, hooks, commands, extensions use Lua 5.4. `quirl.config` is ordinary Lua backed by generated annotations and authoritative Rust schemas. |
| Round trip | A concrete-syntax-tree patcher changes only recognized literal arguments, preserving comments, layout, unknown plugin forms, and surrounding code. Writes are atomic and retain a recoverable prior version. |
| Live synchronization | Browser reads refresh from the authoritative file and saves re-check it before writing. Unsaved browser changes use a three-way merge; conflicts show a diff instead of silently winning. No background watcher is claimed. |
| Validation | Versioned schema supplies types, ranges, deprecations, platform support, examples, plugin settings. `quirl config check` parses/lints Lua, validates known annotations and returned config through Rust schema, and never activates invalid state. |
| Prompt symbols | `auto` chooses Unicode only for a UTF-8 locale and otherwise uses ASCII; `plain` and `unicode` are explicit; `nerd_font` opts into Powerline/private-use glyphs. Patched fonts are never required, and `TERM=dumb` always wins with the plain profile. |
| Preview | Theme, prompt, keymap, completion, picker, accessibility changes render against sample and live contexts before Apply. |
| Dynamic values | UI shows evaluated value, source span, documentation, and marks code-controlled expressions; “Open in code” never silently replaces one. |
| Safe reload | Restricted evaluation with declared capabilities; parse/contract/runtime failures retain last-known-good config and show span diagnostic with rollback. |
| Security | Loopback random port, unguessable session token, strict Origin checks, no shell-execution endpoint, exits with the command. |
| Portability | `export`, `diff`, `migrate`, `doctor` work without a browser; synchronization is file ↔ UI, never mandatory cloud storage. |

```sh
quirl config web config.lua  # local loopback web app; config.lua remains authoritative
quirl config tui             # same schema, terminal presentation
quirl config get editor.keymap
quirl config set picker.layout adaptive
quirl config diff personal.lua work.lua
quirl config check --format json
quirl config fmt config.lua --check
quirl config export config.lua --format json
quirl config migrate config.lua --dry-run
quirl config doctor config.lua
```

`quirl config web <file>` uses a tokenized, loopback-only session and renders
the complete current schema as an accessible HTML form. It does not create a
second configuration store: every save conservatively patches literal Lua
values, validates the full candidate, preserves a backup, and reports a
conflict if the source changed since the form loaded. Code-computed values are
never overwritten by the form.

> **No first-run wizard tax.** Quirl starts with a carefully chosen default theme, prompt, keymap, picker, and compatibility profile. Configuration helps explore and personalize a working product; it never assembles one.

### Interaction and visual contracts

- Incomplete input always yields a recoverable syntax tree, so highlighting, selection, indentation, and completion continue mid-command.
- Completion items include type, docs, provenance, side effects, deprecation, and the edit they apply—not merely a string.
- The palette searches actions, commands, history, directories, jobs, settings, snippets, docs, and embedded-language symbols.
- `explain` previews expansion, executable resolution, redirections, capabilities, destructive paths, pipeline types, and likely output shape.
- Checked code brings diagnostics, inferred hover types, module docs, command signatures, stack traces, and package capabilities into the prompt UI.
- Every panel has keyboard control and linear text output; mouse, icons, animation, color, and Unicode are enhancements.

Editing targets 8 ms P95 for local highlighting and structural edits. Completion shows cached local results immediately and streams slower providers without reordering selection. The visual system uses one accent for mode, one severity system for diagnostics, opt-in panels, and compact transient prompts. Generated or history-derived commands display their source; capability-crossing commands are inspectable before Enter. Failures preserve command, cwd, environment diff, captured output, timing, and error chain as an editable command block.

### Prompt engine and included capabilities

Quirl ships an asynchronous prompt scheduler inspired by Starship and Oh My Posh: segments run concurrently, cache by dependency, respect deadlines, and update without delaying the editable prompt.

```yaml
prompt:
  left: [directory, git_branch, git_state]
  right: [jobs, duration, status]
  transient: true
  first_paint_budget: 8ms
  slow_segment_policy: stale_while_refresh
```

| Included | Default capability |
| --- | --- |
| Navigation | Smart `cd`, directory history, bookmarks, frecency, native typed picker |
| Discovery | Semantic completion, man/help indexing, fuzzy history/files/actions, previews, `which --explain` |
| Data | JSON, YAML, TOML, CSV, SQLite, archive inspection, HTTP client |
| Views | Tables, trees, diffs, logs, progress, charts, file previews |
| Developer context | Git, toolchains, env/direnv-compatible loading, project tasks |
| Operations | Jobs, process table, watch, retry, timeout, parallel map |
| Personalization | Great defaults, prompt/theme/keymap profiles, web and terminal configuration, schema validation |

“Included” means maintained as part of the release and available offline. Expensive or networked features remain lazy and explicit.

## 11. Plugin platform

### The shell is an environment you can grow

Quirl takes Emacs and Neovim extensibility seriously. Trusted-language plugins use the same typed values, command catalog, completion engine, UI compositor, diagnostics, and documentation as built-ins. Core may have privileged implementations for performance or job control, but schemas, events, renderers, completions, docs, and user-facing composition use the public extension model.

| Extension point | A plugin can contribute | Composition rule |
| --- | --- | --- |
| Commands | Typed commands, aliases, subcommands, examples, effects, error codes | Namespaced by default; explicit approval to shadow |
| Completion | Static values, contextual providers, ranking signals, docs, fixes | Merge into command graph with provenance |
| Analysis | Syntax rules, diagnostics, lint checks, code actions, formatters | Incremental, cancellable, severity-governed |
| Views | Renderers, inspectors, previews, tables, charts, Ratatui panels | Draw only inside assigned regions; plain-text fallback required |
| Shell UI | Prompt segments, status items, keymaps, snippets, palette actions | Deadline/collision policies visible in `doctor` |
| Events | Lifecycle, directory, plan, execution, result, error handlers | Capabilities and mutation rights explicit per hook |
| Languages | Script engines, parsers, language services, validators, package adapters | Implement stable runner and diagnostic protocols |
| Knowledge | Command-index sources, docs, examples, project detectors, AI schemas | Versioned, attributable, invalidatable catalog entries |

**Typed event flow:** session start/restore → directory changes → command plan → execution progress/output/cancellation → result rendering/history/diagnostics. Hooks receive immutable typed event records and return declared actions. Observation is default; plan rewrites, environment changes, output reads, or execution blocking need specific capability. Slow handlers become asynchronous; failed handlers are isolated and diagnosed.

```toml
# plugin.toml
[plugin]
name = "kubernetes-workbench"
version = "0.1.0"
entry = "plugin.lua"
quirl = ">=0.1, <0.2"

[capabilities]
request = [
  "commands.register", "completion.register", "ui.panel",
  "process.spawn:kubectl", "filesystem.read:./.kube",
]

[contributes]
commands = ["kube"]
panels = ["cluster"]
indexers = ["kubernetes-contexts"]
```

```lua
-- plugin.lua
quirl.plugin.command {
  name = "kube pods", summary = "List pods as typed records",
  output = quirl.type.stream("Pod"), run = list_pods,
}
quirl.completion.provider {
  for_argument = "kube pods --namespace",
  returns = quirl.type.completion("Namespace"), run = cached_namespaces,
}
quirl.ui.panel { name = "cluster", plain = render_cluster_text, render = render_cluster_panel }
```

| Concern | Contract |
| --- | --- |
| Runtime | Trusted Lua plugins run in process with capability handles, cancellation, allocation budgets, and per-callback deadlines. Generated annotations make the SDK discoverable; Rust validation remains authoritative. |
| Isolation | WebAssembly or out-of-process plugins use the same value and catalog protocols. Native Rust ABI is not stabilized; crashes cannot take down the shell. |
| UI safety | Plugins return view trees or styled values; they cannot write arbitrary escapes into the active editor. Quirl owns layout, focus, theme, accessibility, cleanup. |
| Supply chain | Lockfile records package, checksum, source, capabilities, resolved API version. Install/update show permission diffs; policy can deny unapproved sources/effects. |

```sh
quirl plugin add github:acme/kubernetes-workbench
quirl plugin permissions kubernetes-workbench
quirl plugin enable kubernetes-workbench
quirl plugin doctor kubernetes-workbench
quirl plugin update --locked
quirl plugin disable kubernetes-workbench
quirl plugin remove kubernetes-workbench
```

> **Long-term direction:** a focused plugin can add commands, data types, views, scripts, completions, docs, and AI-discoverable capabilities together. External programs stay first class; plugins integrate the Unix ecosystem rather than forcing a rewrite.

## 12. Architecture and budgets

### One semantic core, several front doors

The parser boundary preserves compatibility without leaking it into the value runtime. Every front end lowers into a shared execution plan with common diagnostics, cancellation, provenance, and observability.

**Architecture:** line editor (command parser, data parser, embedded runtime) → typed IR (processes, streams, effects, spans) → executor (jobs, backpressure, cancellation, PTY) → runtime (value, result, schema, adapters) → renderer (text, table, JSON, Ratatui, LSP).

| Principle | Implication |
| --- | --- |
| Rust core | Implemented boundaries ([ADR 0002](decisions/0002-crate-layering.md)): foundations `quirl-core`, `quirl-catalog`, `quirl-syntax`; `quirl-data` and `quirl-lua` on core only; `quirl-ui`; `quirl-cli` as sole composition root. Candidate crates as the surface grows: `quirl-compat`, `quirl-plan`, `quirl-process`, `quirl-picker`, `quirl-config`, `quirl-docs`, `quirl-plugin`, `quirl-lsp`. Adding a layer or inverting an edge requires a new ADR. |
| Conch lesson | Compatibility and data ASTs lower to executor interfaces without the executor knowing their surface grammar. |
| Flyline lesson | Ratatui supplies responsive prompt widgets, fuzzy suggestions, tooltips, selection, panels while preserving scrollback and a plain fallback. |
| Nu lesson | Built-ins exchange typed streams. External commands remain byte-oriented and cross visible adapters with bounded buffering. |

### Performance acceptance budgets

| Budget | Target |
| --- | --- |
| Cold start | ≤25 ms to editable prompt, P50 on reference hardware |
| Keystroke to frame | ≤8 ms P95 |
| First prompt paint | ≤21 ms P95; slow segments refresh later |
| Stream memory | `O(window)` unless explicitly collected |
| Release binary | ≤5 MiB ideal; >8 MiB emits a warning; >10 MiB fails the release gate. One MiB is exactly 1,048,576 bytes |

These are targets, not current benchmark claims. Each release records cold/warm startup, completion/render latency, pipeline throughput, peak memory, and binary size on named hardware.

**Terminal contract:** Tier 1 is Linux and macOS with native job control behind
one interface. Windows is a best-effort portable-process target, not a supported
interactive terminal. On Tier 1, negotiate color, Unicode, keyboard protocol,
mouse, hyperlinks, clipboard, and synchronized updates; give remote/dumb
terminals a line-oriented experience using the same parser/completion data;
honor `NO_COLOR`, reduced motion, accessible output, and never require a mouse;
keep non-interactive stdout stable, undecorated, and control-sequence-free unless
asked.

## 13. Delivery sequence

### Prove the risky seams first

This delivery table is a design tracker. “Implemented” identifies integrated
code and tests; it does not mean the current source has a tagged release,
completed human checklist, or fresh exact-artifact evidence.

| Phase | Status | Deliverable |
| --- | --- | --- |
| 0 | **Complete** | Lua runtime and generated SDK views; config validation; atomic live config/plugin reload; applied editor/prompt/picker settings; live prompt/completion callbacks; resource budgets; command graph; mode switch; focused native data grammar; C0 execution; byte pipes; Data-mode `ls`/`files` source; error spans. |
| 1 | **Accepted · Preview** | Job control, redirects, indexed completions, Zsh/Bash/Fish and help/man ingestion, history/file/action picker, adaptive prompt, typed config forms with web/TUI views, C1 subset, structured core commands, plain fallbacks, Linux/macOS. |
| 2 | **Complete · Scriptable** | Lua scripts and computed config, `quirl run`, formatter, annotation-aware checker, linter, tests, docs, language service, agent catalog/validation formats, package manifests, generated host API, deterministic tests. |
| 3 | **Accepted · Platform** | Trusted-language and isolated-plugin contracts, permissions/lockfile, catalog/completion/UI/event extension points, Bash/Zsh runners, directory/process panels, bounded live sampling, portable process contracts, recovery. |
| 4 | **Integrated candidate work** | Automated contracts, compatibility dispositions, performance harnesses, and security/accessibility audits are present. A release still requires fresh exact-artifact evidence and the human Linux/macOS checklist; explicit reference-shell and best-effort platform boundaries remain outside the current 0.1 support claim. |

### Phase 1 acceptance gates

Phase 1 ("Preview") is accepted. Like the Lua gates in §9, these preserve the
evidence required for that decision; they are not a current task list:

- **Job control.** Background/foreground/suspend (`&`, `Ctrl-Z`, `jobs`,
  `fg`, `bg`) work through one native lifecycle interface on Linux and
  macOS, and structured job state is visible to the prompt and the picker.
- **One execution graph.** Redirects and byte pipes in command mode run
  through a single native plan across built-ins and external processes;
  the C0 surface (`ls | grep …`, `> file`, quoting) no longer round-trips
  through the compatibility shell.
- **Durable history and picker.** A separate bounded SQLite database records command, working directory, status, duration, and mode across sessions. Same-directory entries receive a ranking preference; `Ctrl-R` or `Up` opens the typed picker over history entries, and the same engine
  serves files and palette actions with a plain-text fallback.
- **Completion ingestion.** Fish declarative completions plus at least one
  of Bash/Zsh translate into `CommandSpec` entries with provenance and
  confidence recorded, and the index can explain every fact's source.
- **C1 core with evidence.** On Unix, the quote-aware command IR supports
  lists, standard-descriptor redirects, here-strings, bounded command
  substitution, parameter/arithmetic expansion, and pathname expansion. The
  matrix records exact Bash/Zsh evidence; here-documents, process substitution,
  loops, functions, conditionals, and dialect control forms remain explicit
  bounded `bash { ... }` / `zsh { ... }` islands for 1.0 rather than an implied
  native promise. Windows retains a best-effort portable process graph and does
  not share the supported interactive Unix contract.
- **Budgets measured.** Cold start, keystroke-to-frame, and first prompt
  paint are measured against the §12 budgets on named hardware and
  recorded — including where they currently miss.

Every gate lands with catalog metadata, diagnostics, keyboard navigation,
and accessible text output, per the release criterion in §10.

### Phase 2 acceptance evidence

- **One script entry point.** `quirl run` selects Lua or the line-oriented
  native Quirl grammar by explicit `--lang`, shebang, or extension. `.qrl` is
  canonical; `.quirl` and `.🌀` are accepted aliases. It accepts stdin,
  preserves Lua resource policy, and reports labeled native command/data
  failures. Bash/Zsh runners subsequently landed as the explicit C2 surface.
- **Deterministic authoring tools.** `fmt`, `check`, `lint`, and `test` accept
  files or deterministic project discovery. They skip build/VCS directories
  and symlink directories, aggregate every diagnostic, never execute checks,
  and isolate test modules under the script policy. Discovery uses an explicit
  stack and filesystem identities rather than recursion: depth is capped at 32,
  directories at 4,096, entries per directory at 4,096, total entries at
  65,536, supported files at 8,192, live retained path state at 4 MiB, and
  scanned filename bytes at 4 MiB. A bind-mount or other directory alias is
  rejected; permission and disappearing-entry failures are reported rather
  than silently producing a partial project view.
- **Crash-safe formatting.** Changed Lua and native Quirl sources are written to
  a create-new sibling, flushed and synchronized, assigned the original
  permissions, and atomically installed only after the complete bounded output
  is durable. Formatting rejects symlinks, special files, and observed
  concurrent changes; Unix also rejects pre-existing hard-link aliases, while
  platforms without a stable link-count API replace only the named entry. A
  synchronized same-directory recovery link retains the original across
  replacement and parent-directory sync; returned failures roll back and clean
  transaction files. On Unix each namespace transition is
  directory-synchronized. Other platforms retain the synchronized-file
  guarantee but depend on the operating system's rename durability because
  portable Rust cannot sync a directory.
- **Generated knowledge.** `new`, `describe`, and `doc` use checked templates
  and the semantic catalog. Lua annotations, editor completion, hover,
  signatures, module docs, runtime bindings, and SDK output derive from the
  same `HOST_API` definitions.
- **Language service.** `quirl lsp` implements a bounded stdio LSP subset with
  UTF-16 positions and deterministic document state. Lua diagnostics compile
  and lint without invoking chunks; native Quirl intelligence consumes the loaded
  catalog without executing commands.
- **Agent and package contracts.** Versioned deny-unknown documents carry
  installed versions, named schema/content hashes, capabilities, validators,
  and token-budgeted context. Package builds enforce public-command metadata,
  entry/version/capability constraints, and reproducible output;
  `publish --dry-run` proves that no registry or network action occurs.
- **Evidence stays executable.** In-crate protocol, policy, schema,
  determinism, traversal, quality-gate, and tamper tests run under the canonical
  `cargo xtask check`, alongside the generated-SDK exactness and guest Lua suite.

### Phase 3 acceptance evidence

- **Permission lock is runtime authority.** Managed trusted-Lua plugins load
  only when enabled, integrity-checked, and granted the exact locked
  capabilities. Lifecycle writes are versioned, validated, flushed, recoverable,
  and reject permission or source drift.
- **Extension protocols are composed.** Versioned immutable events and typed
  actions cover the execution lifecycle. Catalog, completion, and panel
  contributions reach real consumers through deny-unknown, terminal-safe
  boundaries with deadlines, collision checks, and failure isolation.
- **Portable isolation is honest.** Out-of-process adapters execute the bounded,
  versioned `quirl.plugin.v1` initialization handshake with an exact scoped
  launch grant, deny-unknown messages, output/deadline limits, and containment.
  Wasm components remain validated but disabled until a component runtime is
  selected; the checked-in WIT world and its hash bind that future work.
- **Platform processes and recovery are bounded.** Bash/Zsh runners preserve
  arguments, environment, status, the plan deadline and cancellation identity,
  and the exact validated per-stream capture ceiling. Inherited native output
  retains no copy while remaining deadline- and cancellation-bound.
  Recovery is versioned, atomic, quota-limited, terminal-safe in text mode, and
  retains exact stored values in JSON.
- **Windows has a best-effort lifecycle backend.** Cross-target checks exercise
  byte pipelines, redirects, boolean graphs, jobs, foregrounding, cancellation,
  and Job Object tree containment. Native terminal handoff, suspension, and
  Windows hardware validation are outside the supported 1.0 release contract.
- **Evidence stays executable.** The canonical `cargo xtask check` covers metadata
  quality, capability smuggling, event ordering and action grants, contribution
  composition, output and recovery bounds, terminal controls, plugin tampering,
  runners, panels, watch cancellation, and generated-SDK exactness.

### Phase 4 review evidence

- **Contract changes are reviewable.** A checked-in golden manifest binds the
  grammar matrix, catalog, picker/completion shapes, agent/package documents,
  runner, plugin/WIT, events, config, and recovery contracts to named versions,
  hashes, reader policies, and migration ranges. Future or expired documents
  fail closed; every readable legacy version has deterministic migration tests.
- **CLI and semantic metadata agree.** Composition tests recursively compare
  every visible Clap leaf's options, positionals, requiredness, repeatability,
  fixed value domains, and signature with the exact catalog contract.
- **Compatibility evidence executes both sides.** The frozen matrix gives every
  C0/C1-core/C2 form a native, reference-runner, or explicitly deferred
  disposition.
  Composition tests compare Quirl's status, stdout, stderr, and redirected file
  effects with Bash and Zsh for the supported native subset.
- **Release budgets are enforced.** A named-hardware PTY harness measures 101
  fresh processes for cold-to-editable P50, keystroke-to-frame P95, and first
  prompt P95; production live-buffer retention proves O(window), and the exact
  stripped release binary is classified against the 5 MiB ideal, warns above
  the 8 MiB soft cap, and fails strictly above the 10 MiB hard ceiling.
- **Security and accessibility claims are adversarial.** Bounds, symlink and
  path containment, capability smuggling, cancellation, hostile C0/C1 terminal
  text, JSON semantic preservation, `NO_COLOR`, `TERM=dumb`, plain fallbacks,
  and private recovery storage have executable tests and a checked-in audit.
- **The release line is explicit.** Process adapters execute a deliberately
  narrow bounded v1 handshake, and picker/completion envelopes are frozen with
  cancellation, deadlines, and stale-result evidence. Linux and macOS are the
  supported interactive platforms. Windows terminal behavior is best effort,
  and non-native dialect forms remain explicit reference-shell islands; neither
  is a hidden release blocker. The exact candidate still requires fresh
  performance evidence and the human release checklist.

## 14. Review decisions

On 15 August 2026, the following decisions were reviewed and agreed:

- **Three explicit interactive modes:** normal mode uses bytes and compatibility syntax; data mode uses typed value pipelines; AI mode searches local command knowledge and inserts a selected command for review without execution.
- **C0–C4 compatibility levels:** optimize copy/paste and common interactive syntax; use reference interpreters for exact dialect behavior.
- **Result-based errors outside command mode:** retain traditional status interactively; require explicit failure handling in data pipelines and scripts.
- **Lua 5.4 as the extension language:** Rust implements the product; one pinned Lua runtime and generated `quirl` SDK serve config, scripts, trusted plugins; annotations improve tooling while Rust schemas enforce boundaries.
- **Batteries-included scope:** ship coherent navigation, discovery, data, views, developer context, operations.
- **One semantic command catalog:** completion, highlighting, docs, validation, AI discovery, plugins consume one versioned `CommandSpec` graph.
- **Zero-setup local command intelligence:** first paint precedes bounded background model installation, full catalog discovery (including non-executing PATH-gated system man-page semantics), and SQLite embedding even while the shell remains idle; the bottom status row reports cached activity, failures retain lexical search, and refreshed catalogs re-index automatically.
- **Self-describing AI interface:** export installed capabilities as token-budgeted Markdown or canonical JSON with check/format/lint and optional MCP access.
- **Lua plugin platform plus Wasm isolation:** trusted Lua gets generated SDK metadata and explicit capabilities; untrusted/portable plugins use the same capability model through WebAssembly components.
- **Native typed fuzzy picker:** history, files, completion, commands, data values, plugins share one previewable, scriptable selection engine.
- **Lua configuration with synchronized views:** `config.lua` remains source of truth; generated annotations, Rust schema validation, local web/TUI views remain synchronized without another config store.

### Decisions closed since Draft 0.9

The Draft 0.9 questions now have implemented dispositions:

1. Rust `HOST_API` definitions generate the LuaLS annotations, runtime bindings,
   Markdown, and JSON views; Rust boundary validation remains authoritative.
2. Quirl pins vendored Lua 5.4. A Lua 5.5 move requires a deliberate runtime,
   SDK, compatibility, and benchmark review rather than an automatic upgrade.
3. Lua receives only the restricted table, string, math, and UTF-8 libraries;
   ambient `io`, `os`, `debug`, `require`, and `package` stay unavailable.
4. `LuaPolicy` combines memory, instruction, wall-clock, callback, and
   cancellation budgets; violations become structured errors rather than host
   panics or hangs.
5. Wasm is a validated, disabled plugin boundary until an execution engine can
   preserve the existing value, catalog, capability, and isolation contracts.
   It is not a first-class script runner.
6. Command and data remain two explicit, visible interactive modes; scripts mark
   grammar boundaries in source.
7. Fish/Bash/Zsh completion ingestion translates bounded declarative forms and
   records dynamic providers without executing them.
8. Agent discovery ships deterministic CLI/JSON/Markdown contracts plus a
   bounded, source-only optional MCP surface.
9. Plugin actions are typed, capability-gated, validated, and composed at the
   Rust boundary; arbitrary command-plan mutation is not ambient authority.
10. Native C1 stops at the frozen bounded core. Here-documents, process
    substitution, loops, functions, conditionals, and dialect control forms are
    explicit C2 reference-shell islands under ADR 0010.
11. The picker exposes deterministic exact, fuzzy, and inverse matching through
    typed values and versioned asynchronous cancellation/deadline envelopes.
12. The first preview includes the editor, prompt, semantic completion, history,
    picker, catalog, configuration views, authoring tools, generated SDK, and
    plain terminal fallbacks described above.

Future work is tracked as new scoped proposals rather than reopening the draft:
a production Wasm engine, authenticated plugin distribution, promotion of
Windows to a supported interactive platform, or expansion of the frozen native
compatibility matrix each requires its own implementation and evidence.

## References and prior art

- [Nushell](https://www.nushell.sh/book/types_of_data.html) — structured values, pipelines, domain types, span diagnostics.
- [Xonsh](https://xon.sh/tutorial.html) — a language beside subprocess invocation, and a warning about grammar ambiguity.
- [Flyline](https://github.com/HalFrgrd/flyline) — Rust line editing, Ratatui rendering, prompts, tooltips, fuzzy history inside Bash.
- [Ratatui](https://ratatui.rs/) — immediate-mode terminal rendering, widgets, responsive layouts.
- [Steel](https://github.com/mattwparas/steel) and the [Helix Steel proposal](https://github.com/helix-editor/helix/pull/8675) — embedded Scheme and a concrete extension-system exploration.
- [Fennel](https://fennel-lang.org/), [TypeScript 7](https://devblogs.microsoft.com/typescript/announcing-typescript-7-0/), [QuickJS-NG](https://github.com/quickjs-ng/quickjs), [Luau](https://luau.org/), [AssemblyScript](https://www.assemblyscript.org/), [MoonBit](https://docs.moonbitlang.com/en/stable/), [PocketPy](https://github.com/pocketpy/pocketpy), [Rhai](https://rhai.rs/), and [Wasmtime](https://docs.wasmtime.dev/) — language and isolation alternatives.
- [Starship](https://starship.rs/) and [Oh My Posh](https://ohmyposh.dev/) — prompt customization, caching, broad context.
- [Conch runtime](https://docs.rs/conch-runtime/latest/conch_runtime/) — AST-independent POSIX execution architecture.
- [Bash manual](https://www.gnu.org/software/bash/manual/), [Zsh compatibility](https://zsh.sourceforge.io/Doc/Release/Compatibility.html), [Zsh completion](https://zsh.sourceforge.io/Doc/Release/Completion-System.html), [Bash programmable completion](https://www.gnu.org/software/bash/manual/html_node/Programmable-Completion.html), [Fish completions](https://fishshell.com/docs/current/completions.html) — the compatibility and completion surface.
- [Rust doc comments](https://doc.rust-lang.org/stable/reference/comments.html#doc-comments), [fzf](https://github.com/junegunn/fzf), [Fish web configuration](https://fishshell.com/docs/current/cmds/fish_config.html), [Bun](https://bun.sh/), [Helix](https://helix-editor.com/), and [Duden: Quirl](https://www.duden.de/rechtschreibung/Quirl) — documentation, discovery, configuration, completeness, coherence, and the name.

---

The long-term sections preserve product intent rather than implementation
claims. Performance numbers there are targets, and illustrative syntax does
not expand the current focused grammar or runtime contract.
