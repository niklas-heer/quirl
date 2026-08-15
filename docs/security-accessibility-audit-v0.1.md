# Quirl v0.1 security and accessibility release audit

Date: 2026-08-15
Scope: Phase 4 release candidate
Result: pass for the v0.1 prototype, with the explicit residual risks below

This audit treats Lua source, plugin packages and registrations, child-process
output, recovery files, event traces, catalog text, and terminal capabilities
as untrusted inputs. It covers denial of service, capability escalation, path
escape, terminal-control injection, cancellation, and text-only operation. It
does not claim that a trusted local plugin is isolated from the Quirl process;
that is deliberately not the `trusted_lua` contract.

## Release evidence

| Boundary | Enforced v0.1 property | Adversarial evidence |
| --- | --- | --- |
| Lua VM | Restricted standard library; typed deny-unknown registrations; exact grants; memory, instruction, callback, wall-time, and cancellation budgets | `quirl-lua` tests for unavailable modules, unknown fields, scoped process grants, deadlines, cancellation recovery, return shapes, and oversized source |
| Script input | File and stdin source is UTF-8 and at most 4 MiB before parsing or Lua compilation | `oversized_source_is_rejected_before_lua_compilation`; `script_reader_rejects_source_beyond_the_runtime_limit` |
| Plugin package | Manifest is at most 256 KiB; entry is at most 4 MiB; lexical parent traversal, absolute paths, and canonical symlink escapes are rejected | `plugin_entry_symlink_cannot_escape_package_directory`; `managed_activation_rejects_checksum_matching_entry_symlink_escape`; oversized manifest/entry tests |
| Plugin capability boundary | Requested/granted capabilities are sorted, validated, locked, and checked again at each Lua host callback; scoped process grants reject shell operators and control characters | plugin manifest/lock capability tests and `scoped_process_capability_cannot_smuggle_shell_syntax` |
| Native/reference process | Unix process groups and Windows Job Objects contain normal child lifecycles; cancellation is direct; Bash/Zsh reference output retains only a 64 KiB window per stream with exact discard counts | `quirl-process` backend contract/job tests and script reference-runner bounded capture/cancellation tests |
| Recovery | Snapshot capture is truncated before persistence; reads and files are bounded; count/byte retention is enforced; secret-like values are redacted; text display neutralizes ANSI, OSC, C1, CR, and BEL; symlink escapes are rejected | recovery atomic/bounds/redaction, ANSI/OSC, retention, oversized-read, ID traversal, and symlink tests |
| Events and live views | Typed deny-unknown event documents, strictly increasing sequences, action capability validation, callback deadlines, 4 MiB trace input, bounded live buffers, and cooperative cancellation | core event/action tests, event trace bound/order tests, UI live-buffer cancellation test |
| Picker input | Standard input is UTF-8, at most 4 MiB, and at most 20,000 newline-delimited values before fuzzy selection | `picker_stdin_rejects_oversized_input_and_item_counts_before_selection` |
| Terminal output | Untrusted diagnostic, catalog, completion, plugin, picker, agent, index, package, config, authoring documentation, recovery, and JSON text cannot emit active control bytes; JSON escaping preserves parsed values | core terminal escaping tests, picker C0/C1 test, author stdout C0/C1 test, UI hostile-error test, recovery ANSI/OSC test |
| Accessibility | Editor styling is enabled only for a color-capable terminal when `NO_COLOR` is absent; `TERM=dumb` uses ASCII prompt separators and indicators; panels require a plain fallback; noninteractive structured output has no decoration | `color_requires_a_terminal_and_no_color_must_be_absent`; `terminal_styles_require_an_interactive_color_capable_terminal`; `dumb_terminal_prompt_join_uses_ascii_only`; panel fallback/control tests |

Recovery state is private on Unix: Quirl forces the journal directory to mode
`0700` and newly created snapshots to `0600`. Windows relies on the ACL inherited
from the selected user state directory.

Structured JSON output retains the original semantic strings. Terminal control
code points that JSON permits unescaped (notably C1) are emitted as JSON Unicode
escapes, so parsing yields the exact original value without activating terminal
behavior.

The `pick`, `agent`, `index`, `package`, `config`, and authoring-documentation command surfaces apply the
same rule at their final output boundary: text uses visible control escapes and
JSON is escaped only after serialization. This avoids changing the parsed JSON
value or double-escaping ordinary text. `quirl doc --output` deliberately writes
the selected documentation bytes unchanged because it targets a file rather than
a terminal.

## Reproduce

Run from the workspace root with the pinned toolchain:

```sh
cargo test -p quirl-core
cargo test -p quirl-lua
cargo test -p quirl-plugin
cargo test -p quirl-process
cargo test -p quirl-ui
cargo test -p quirl-cli
cargo clippy -p quirl-core -p quirl-lua -p quirl-plugin -p quirl-process -p quirl-ui -p quirl-cli --all-targets -- -D warnings
mask check
```

Manual text-only smoke checks:

```sh
NO_COLOR=1 TERM=dumb cargo run -q -p quirl-cli -- help
NO_COLOR=1 TERM=dumb cargo run -q -p quirl-cli -- complete 'git c'
printf 'return "plain"\n' | NO_COLOR=1 TERM=dumb cargo run -q -p quirl-cli
```

The first two commands must contain no ANSI control sequences; the interactive
prompt uses ASCII separators/indicators under `TERM=dumb`, and editor hints and
semantic highlighting are unstyled for `NO_COLOR`, dumb, or noninteractive
output. The piped command is noninteractive and prints only `plain` plus a
newline.

## Explicit residual risks

1. **Explicit native capture is not window-bounded.** `NativeExecutor::execute_capture`
   drains concurrently to avoid pipe deadlock, but retains the complete stdout
   and stderr until the child exits. Reference Bash/Zsh capture and persisted
   recovery data are bounded. Callers must use inherited/streaming execution for
   untrusted high-volume commands. A future API should make capture limits
   mandatory and return discard accounting.

2. **Local package verification has a time-of-check/time-of-use window.** Quirl
   canonicalizes package files, enforces containment, bounds them, and checks
   locked hashes before loading by path. Another process running as the same user
   can still replace a file between verification and Lua loading. Closing this
   fully requires loading the already verified bytes or a platform-specific
   stable file-handle design.

3. **Checksums are integrity records, not publisher authentication.** A plugin
   lock detects changes relative to the reviewed local source. v0.1 has no signed
   registry, transparency log, or publisher identity.

4. **Secret redaction is heuristic.** Recovery redacts values whose environment
   keys look secret and common secret-shaped command arguments. Derived,
   encoded, very short, or unusually named secrets can remain. Recovery is local,
   quota-limited, private on Unix, and should still be treated as sensitive.

5. **Requested child output is intentionally raw.** Like other shells, Quirl
   allows a foreground external command to control its terminal. Quirl-owned
   metadata and diagnostics are sanitized; output from a command the user chose
   to execute is not. Running an untrusted program therefore carries normal
   terminal-emulator risk.

6. **Dumb-terminal editing is reduced, not a separate line editor.** Prompt
   glyphs become ASCII and `NO_COLOR` disables semantic highlighting, while the
   interactive editor remains Reedline. Terminals that cannot support its basic
   cursor protocol should use the stable noninteractive commands or piped stdin.

7. **Windows suspension differs by design.** Normal foreground/background,
   cancellation, recovery, and Job Object containment work, but Unix terminal
   process groups and Ctrl-Z suspension have an explicit unsupported diagnostic.
   There is also a small spawn-to-Job-assignment race before containment becomes
   active.

These residuals are visible constraints, not claims of completed isolation.
They should be re-audited when native capture becomes a public streaming API,
plugins gain a distribution channel, or the terminal layer gains a dedicated
minimal line editor.
