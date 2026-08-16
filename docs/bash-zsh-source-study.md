# Bash and Zsh source study: lessons for Quirl

Status: research note

Date: 2026-08-16

This note compares current upstream Bash and Zsh with Quirl. It is not a plan
to reproduce either shell. The purpose is to identify mature implementation
patterns, expensive compatibility traps, and concrete work that would make
Quirl safer and more useful within its deliberately smaller Unix-first scope.

## Sources and method

The study used read-only clones in a temporary directory at these exact
upstream revisions:

- [GNU Bash commit `b4608166`](https://git.savannah.gnu.org/cgit/bash.git/commit/?id=b460816602167718f78a6233164e8875f49b75b2),
  Bash 5.3 patch 15, dated 2026-06-10.
- [Zsh commit `53de4623`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/),
  development version 5.9.999.3-test, dated 2026-08-14.

The measurements below count the complete tracked repositories unless a more
specific scope is stated. They are orientation, not a productivity comparison:
generated files, translations, test fixtures, documentation, and coding style
make line counts inherently approximate.

| Repository | Tracked files | Tracked bytes | C code | Total code |
| --- | ---: | ---: | ---: | ---: |
| Bash | 1,603 | 37.3 MB | about 137,000 lines | about 397,000 lines |
| Zsh | 1,724 | 14.2 MB | about 111,000 lines | about 122,000 lines |

The combined physical size of selected parser, executor, expansion, job,
signal, editor, and completion files is about 77,600 lines. Zsh's completion
library alone contains 1,018 tracked files and about 108,800 physical lines.
The important conclusion is not that Quirl needs more code. It is that broad
shell compatibility and exhaustive command-specific completion are effectively
separate products.

## Executive conclusions

1. **Quirl's bounded native grammar is the right product boundary.** Bash and
   Zsh spend a large part of their complexity on interactions among quoting,
   expansion, redirects, compound commands, traps, functions, and historical
   status rules. Explicit Bash/Zsh islands are safer than slowly recreating
   that compatibility surface.
2. **Process startup is a transaction.** Mature shells treat signal masking,
   process-group creation, job registration, terminal ownership, and cleanup
   as one critical sequence. Quirl already has useful RAII guards; it should
   make the no-callback critical section explicit and test adversarial races.
3. **Expansion needs a visible stage model.** Expansion bugs arise from order
   and context more often than from the individual operations. Quirl should
   document and test its native expansion stages as a matrix rather than add
   more implicit rules.
4. **Semantic completion is a strategic advantage.** Bash often reconstructs
   context from the editable text. Zsh has a powerful but enormous completion
   ecosystem. Quirl can stay smaller by retaining parsed request snapshots,
   catalog metadata, provenance, cancellation, and typed extension responses.
5. **Do not copy native extension ABIs.** Bash loadable builtins and Zsh
   modules are powerful because they can reach almost everything. Quirl's
   restricted Lua runtime and isolated executable adapters provide a much more
   defensible security and compatibility boundary.
6. **The next investment should be a deterministic PTY torture suite, not more
   syntax.** Process races, terminal restoration, cancellation, descriptor
   leaks, and stopped-job transitions deserve repeated real-terminal tests on
   Linux and macOS.

## 1. Parsing and intermediate representation

### Bash: explicit trees plus pervasive word flags

Bash represents commands as a tagged tree covering simple commands,
connections, loops, conditionals, functions, arithmetic, subshells, and more.
Its word nodes carry many flags recording assignment, quoting, splitting,
globbing, and contextual behavior. See
[`command.h`](https://git.savannah.gnu.org/cgit/bash.git/tree/command.h?id=b460816602167718f78a6233164e8875f49b75b2)
and
[`parse.y`](https://git.savannah.gnu.org/cgit/bash.git/tree/parse.y?id=b460816602167718f78a6233164e8875f49b75b2).

This is a warning about accidental scope. Each new native construct can add
cross-products rather than isolated parser work: a word inside an assignment,
redirect, conditional, substitution, or array does not necessarily have the
same expansion semantics.

### Zsh: compact executable wordcode

Zsh parses into a compact wordcode representation with optimized forms for
common lists and sublists. The representation covers pipelines, redirects,
assignments, simple commands, and compound control structures. See
[`Src/parse.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/parse.c)
and
[`Src/zsh.h`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/zsh.h).

Zsh demonstrates that a compiled command representation can pay off after the
language is stable. It does not justify prematurely compacting Quirl's current
IR. Readable, typed plans are more valuable while the grammar and diagnostic
contracts are still evolving.

### Recommendation for Quirl

- Keep C1 native syntax intentionally bounded and preserve explicit reference
  dialect islands for compound Bash/Zsh programs.
- Continue using structured syntax objects in `quirl-syntax`; do not make the
  executor reinterpret display strings.
- Give every future grammar proposal an interaction budget: enumerate quoting,
  expansion, redirect, pipeline, cancellation, completion, and diagnostic
  behavior before accepting it.
- Consider serialized or cached command plans only after the native grammar
  and protocol are stable and profiling shows parsing is material.

## 2. Expansion is an ordered pipeline

Bash's core expansion path in
[`subst.c`](https://git.savannah.gnu.org/cgit/bash.git/tree/subst.c?id=b460816602167718f78a6233164e8875f49b75b2)
is driven by word context and followed by splitting and pathname expansion.
Zsh's `prefork` path in
[`Src/subst.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/subst.c)
similarly coordinates parameter, command, process, brace, tilde, equals, and
quote-related transformations.

The lesson is not to reproduce those stages. It is to make Quirl's smaller set
unambiguous. Today Quirl expands structured word parts before spawning and
separately bounds command substitution. That is a good base, but the behavior
should be expressed as a public stage table.

Recommended native expansion contract:

| Stage | Inputs | Required properties |
| --- | --- | --- |
| Parse | source text | preserves quoting and source spans |
| Value expansion | eligible word parts | cancellation-aware and bounded |
| Command substitution | explicit substitution nodes | bounded bytes, exact status policy |
| Pathname expansion | unquoted eligible results | deterministic ordering and failure policy |
| Redirect resolution | redirect targets | one path per target, no implicit word splitting |
| Spawn plan | expanded typed pipeline | immutable during process construction |

Add table-driven tests for each stage and important combinations. In
particular, test empty results, non-UTF-8-facing boundaries, glob no-match
behavior, nested substitution, cancellation, size limits, redirect targets,
and the previous-status value.

## 3. Execution, signals, and job control

### What the mature shells protect

Bash's executor dispatch in
[`execute_cmd.c`](https://git.savannah.gnu.org/cgit/bash.git/tree/execute_cmd.c?id=b460816602167718f78a6233164e8875f49b75b2)
coordinates subshell decisions, pipelines, asynchronous execution, redirects,
traps, and status policy. Its job-control implementation in
[`jobs.c`](https://git.savannah.gnu.org/cgit/bash.git/tree/jobs.c?id=b460816602167718f78a6233164e8875f49b75b2)
blocks relevant signals around process creation, establishes the process group,
registers the child, and transfers the terminal under controlled conditions.

Zsh is especially explicit about the danger. Its pipeline executor queues
signals while a job is being initialized because handling child notifications
before the job table is complete can lose state and lead to waits that never
finish. See
[`Src/exec.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/exec.c),
[`Src/jobs.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/jobs.c),
and
[`Src/utils.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/utils.c).

### Current Quirl position

Quirl already has several strong primitives in
[`quirl-process`](../crates/quirl-process/src/lib.rs):

- `SpawnGuard` kills and reaps children if pipeline construction fails.
- `ForegroundTerminal` restores the shell's foreground process group and
  terminal modes through RAII.
- terminal-control signals are blocked around `tcsetpgrp` operations.
- pipeline output is drained concurrently and retained under explicit budgets.
- extension planning and observation occur at the CLI composition layer before
  or after the native executor call, not from inside the spawn loop.

That last point is important: the current architecture already avoids allowing
Lua callbacks to observe a half-created process graph. Preserve it as a
contract. Future progress events, hooks, tracing, or plugin APIs must not be
inserted into the construction window.

### Recommended hardening

1. Introduce a named internal `PipelineConstructionGuard` or equivalent
   invariant spanning first spawn through complete job registration or
   foreground terminal handoff. It may wrap existing guards rather than add a
   new public abstraction.
2. Document that no extension callback, event dispatch, prompt evaluation, or
   cancellation callback executes synchronously inside that window.
3. Add deterministic fault injection after each spawn and process-group step;
   prove all started children are terminated and reaped and all descriptors are
   closed.
4. Add repeated real-PTY tests on Linux and macOS for:
   - a child exiting immediately during multi-stage startup;
   - stop and continue during startup;
   - SIGCHLD arriving around job registration;
   - Ctrl-C and Ctrl-Z against foreground pipelines;
   - terminal closure while a foreground job owns the TTY;
   - foregrounding a stopped multi-process pipeline;
   - cancellation during nested command substitution;
   - terminal modes restored after normal exit, signal exit, spawn failure,
     and wait failure;
   - stable open-file-descriptor counts across hundreds of iterations.
5. Record per-process status internally for pipelines. Expose it only if it
   improves `jobs`, recovery, or diagnostics; the last-stage status can remain
   the simple command contract.

Current implementation evidence closes part of this list. The partial-resource
owner is now explicitly named `PipelineConstructionGuard`, and its construction
window documents the no-callback invariant. Seeded Unix tests inject a failure
after each of the first four spawn checkpoints and prove every started child is
killed and reaped. A separate seeded job-state simulator schedules reordered,
duplicate, and stale lifecycle notifications, then freezes faults and requires
the healthy process-group core to converge within a fixed step bound. Terminal
handoff and wait fault injection still require real-PTY coverage; the simulator
does not claim to emulate kernel terminal semantics.

## 4. Traps and event delivery

Bash signal handlers queue work and execute traps later at controlled points;
the trap machinery also preserves command status and handles re-entrancy. See
[`trap.c`](https://git.savannah.gnu.org/cgit/bash.git/tree/trap.c?id=b460816602167718f78a6233164e8875f49b75b2).

Quirl does not need Bash-compatible traps. It does need the same separation
between asynchronous notification and user code:

- signal handlers and wait loops should only record state or cancellation;
- Lua and adapter callbacks should run at explicit CLI safe points;
- events should describe a committed state transition, not an in-progress
  mutation;
- recursive event dispatch needs an explicit depth or generation guard;
- an extension failure must never prevent terminal restoration or child reaping.

The existing extension host's bounded dispatch and the executor/CLI layering
are aligned with this model. Add a regression test that a hostile result or
error observer cannot interrupt process cleanup.

## 5. Completion and line editing

Bash's completion entry point in
[`bashline.c`](https://git.savannah.gnu.org/cgit/bash.git/tree/bashline.c?id=b460816602167718f78a6233164e8875f49b75b2)
examines the Readline buffer to infer command position, quoting, substitutions,
and redirection context before programmable completion runs. This is necessary
for compatibility, but it couples completion to heuristic reparsing.

Zsh separates line editing, screen refresh, core completion, and shell-scripted
completion across
[`Src/Zle/zle_main.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/Zle/zle_main.c),
[`Src/Zle/zle_refresh.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/Zle/zle_refresh.c),
and
[`Src/Zle/compcore.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/Zle/compcore.c).
Its completion ecosystem shows both the value and maintenance cost of rich
command knowledge.

Quirl should lean into the design it already has:

- one immutable completion request containing buffer, cursor, mode, revision,
  and deadline;
- syntax-aware context rather than parsing rendered suggestions;
- catalog-backed command and option facts with provenance;
- cancellation and stale-response suppression;
- terminal rendering isolated from semantic providers;
- bounded Lua results validated at the extension boundary.

The scalable next feature is not hundreds of hand-authored completion files.
It is a documented importer/provider contract for external command metadata,
with cache invalidation, provenance, conflict resolution, `explain`, and strict
latency/output budgets. A small set of first-party high-quality providers can
prove the model.

## 6. Builtins, metadata, and extensions

Bash's `builtins/*.def` files combine implementation declarations, short help,
usage, and generation directives in one source. For an example, see
[`builtins/cd.def`](https://git.savannah.gnu.org/cgit/bash.git/tree/builtins/cd.def?id=b460816602167718f78a6233164e8875f49b75b2).
This prevents some documentation drift, although the C implementation and
generated outputs remain tightly coupled.

Quirl's `Catalog::builtin()` is already the source for help, completion, docs,
and AI discovery, while Clap owns executable argument parsing. The existing
catalog/Clap parity tests are the correct near-term control. Avoid a code
generator until maintaining those two declarations becomes a demonstrated
cost.

Bash loadable builtins and Zsh modules can add executable behavior directly to
the shell process. Zsh modules may contribute builtins, conditions, parameters,
math functions, wrappers, and lifecycle hooks; see
[`Src/Modules/example.c`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Src/Modules/example.c).

Quirl should intentionally offer less ambient authority:

- keep Lua as the only in-process extension language;
- retain mandatory memory, instruction, time, cancellation, and output budgets;
- keep contribution types declarative and versioned;
- require explicit capabilities for process, filesystem, environment, or
  network access;
- run native executable adapters out of process from verified immutable
  snapshots;
- never expose a general native ABI that can mutate parser, executor, editor,
  or job-table internals.

## 7. Status and error semantics

Traditional shells accumulate complexity where `errexit`, negation,
subshells, functions, traps, pipelines, and `&&`/`||` interact. The relevant
branches are visible throughout Bash's `execute_command_internal` and Zsh's
`execlist`/`execpline` paths in the executor sources linked above.

Quirl should not import `set -e` behavior into typed data or native scripting:

- command mode may retain conventional integer status and last-stage pipeline
  behavior;
- typed data pipelines should return explicit values or structured errors;
- scripts should make propagation and recovery visible;
- Bash/Zsh islands own the reference dialect's exact status semantics;
- every boundary should convert failure to `ShellError` without losing the
  original command/status context.

This separation is a product benefit, not merely an implementation shortcut.

## 8. Testing strategy

Bash has hundreds of tracked test files. Zsh's `.ztst` suites are organized by
behavior: grammar, quoting, redirects, execution, assignments, traps, globbing,
substitution, options, history, jobs, line editing, completion, and modules.
Zsh's job tests also use a pseudo-terminal and explicitly acknowledge that
some stop-signal races are hard to exercise safely. See
[`Test/W02jobs.ztst`](https://sourceforge.net/p/zsh/code/ci/53de4623539a227b1eacc82399bdf76189dc679f/tree/Test/W02jobs.ztst)
and Bash's
[`tests/jobs.tests`](https://git.savannah.gnu.org/cgit/bash.git/tree/tests/jobs.tests?id=b460816602167718f78a6233164e8875f49b75b2).

Quirl should retain its in-crate Rust tests and add a behavior matrix that maps
each public contract to tests at four levels:

| Level | Purpose | Examples |
| --- | --- | --- |
| Pure | deterministic syntax and state rules | parsing, expansion stages, job transitions |
| Fault-injected | cleanup at every failure boundary | spawn, pipe, setpgid, terminal handoff, wait |
| PTY integration | real interactive semantics | Ctrl-C/Z, fg/bg, terminal modes, resize |
| Release terminal | named OS and terminal signoff | Linux/macOS terminals, locale/font fallbacks |

Race tests should run a scenario repeatedly and assert both the visible result
and invariants: no live descendant, no leaked descriptor, correct foreground
process group, restored termios, consistent job state, and bounded completion
or callback latency.

The lifecycle simulator follows TigerBeetle's
[safety-to-liveness split](https://tigerbeetle.com/blog/2023-07-06-simulation-testing-for-liveness/):
first explore arbitrary fault schedules, then freeze faults outside a healthy
core and require bounded progress. This matters for Quirl because continuously
random stop/continue or restart events could otherwise rescue a livelock and
hide it from the test.

## Proposed work order

### Before 1.0

1. Write the native expansion-stage contract and add its table-driven tests.
2. Make the pipeline-construction/no-callback invariant explicit in code and
   architecture documentation.
3. Build the Linux/macOS PTY torture harness and the highest-risk job-control
   scenarios.
4. Add fault injection around spawn, grouping, terminal handoff, readers,
   writers, and waits.
5. Map every release-critical process and terminal invariant into the release
   checklist.
6. Preserve the current syntax freeze and reference-island diagnostics.

### After a stable 1.0

1. Add metadata provider/import infrastructure for scalable semantic
   completion, beginning with one flagship workflow.
2. Evaluate per-process pipeline status in `jobs`, structured diagnostics, and
   recovery snapshots.
3. Consider cached typed command plans only with profiling evidence.
4. Expand PTY and compatibility fixtures from real bug reports rather than
   speculative syntax breadth.

### Explicit non-goals

- Bash- or Zsh-complete native syntax.
- Bash-compatible `set -e`, traps, functions, or startup-file semantics.
- An in-process C/C++ plugin ABI.
- Assuming a Nerd Font or patched terminal font by default.
- Recreating Zsh's completion library command by command inside the repository.
- Treating best-effort Windows portability as a Unix 1.0 release blocker.

## Bottom line

Bash and Zsh are valuable references precisely because they show the cost of
decades of compatibility. Quirl should borrow their mature lifecycle
invariants—not their entire language surface. Its strongest route is a smaller,
explicit native shell with typed data, trustworthy cleanup, semantic tooling,
safe extensions, and honest escape hatches to the reference shells.
