# Quirl v0.1 security and accessibility release audit

Date: 2026-08-16
Scope: Linux/macOS v0.1 release candidate plus portable Windows contracts
Result: exact-candidate automated checks pass; release readiness remains blocked
on named human macOS and actual Linux terminal evidence

Exact automated evidence in this refresh belongs only to candidate
`7bf188344ca61798a3cd8657787eacb8ec26ef84` and release artifact SHA-256
`81cd33388cf610a7aac23a9781dbf2771b5dfb6b01b17522c2257cd3676d0ae6`.
Evidence commit B is documentation only and is not the measured artifact.

This audit treats Lua source, plugin packages and registrations, child-process
output, recovery files, event traces, catalog text, and terminal capabilities
as untrusted inputs. It covers denial of service, capability escalation, path
escape, terminal-control injection, cancellation, and text-only operation. It
does not claim that a trusted local plugin is isolated from the Quirl process;
that is deliberately not the `trusted_lua` contract.

Linux and macOS are the supported interactive platforms. Windows evidence in
this audit covers cross-compilation and portable contract behavior only; it is
not a claim of native terminal validation or a Windows release gate. See
[ADR 0010](decisions/0010-unix-first-release-scope.md).
The rich/default and simple/fallback terminal split follows
[ADR 0012](decisions/0012-ratatui-interactive-surface.md).

The candidate ran on actual Apple Mac14,9 hardware with an Apple M2 Pro,
macOS 15.7.9 (24G830), Darwin 24.6.0, and the `aarch64-apple-darwin`
Rust target. The release artifact passed all nine programmatic real-PTY
scenarios at 120×40 and the security/accessibility-focused commands below.
No human reviewer or graphical terminal application was used, and no actual
Linux hardware was available. This automated result therefore does not close
the release checklist's named terminal or Linux signoffs.

## Release evidence

| Boundary | Enforced v0.1 property | Adversarial evidence |
| --- | --- | --- |
| Lua VM | Restricted standard library; typed deny-unknown registrations; exact grants; memory, instruction, callback, wall-time, and cancellation budgets | `quirl-lua` tests for unavailable modules, unknown fields, scoped process grants, deadlines, cancellation recovery, return shapes, and oversized source |
| Script input | File and stdin source is UTF-8 and at most 4 MiB before parsing or Lua compilation | `oversized_source_is_rejected_before_lua_compilation`; `script_reader_rejects_source_beyond_the_runtime_limit` |
| Plugin package | Manifest is at most 256 KiB; entry is at most 4 MiB; lexical parent traversal, absolute paths, and canonical symlink escapes are rejected; trusted Lua executes the verified bytes, and Unix process adapters launch a private staged snapshot of those bytes | `plugin_entry_symlink_cannot_escape_package_directory`; `managed_activation_rejects_checksum_matching_entry_symlink_escape`; `managed_activation_executes_the_exact_bytes_that_passed_integrity_verification`; `isolated_adapter_executes_the_verified_snapshot_after_package_replacement`; oversized manifest/entry tests |
| Plugin capability boundary | Requested/granted capabilities are sorted, validated, locked, and checked again at each Lua host callback; scoped process grants reject shell operators and control characters | plugin manifest/lock capability tests and `scoped_process_capability_cannot_smuggle_shell_syntax` |
| Native/reference process | Unix process groups and Windows Job Objects contain normal child lifecycles; interactive native output streams directly to the terminal; programmatic native capture retains at most 1 MiB per stream and drains excess with exact discard accounting; Bash/Zsh reference capture retains 64 KiB per stream | `quirl-process` backend contract/job, interactive streaming, and bounded-capture tests plus script reference-runner bounded capture/cancellation tests |
| Recovery | Snapshot capture is truncated before persistence; reads and files are bounded; count/byte retention is enforced; environment/argument secrets, authorization headers, credentialed URLs, and high-confidence token shapes are redacted; text display neutralizes ANSI, OSC, C1, CR, and BEL; symlink escapes are rejected | recovery atomic/bounds/structured-redaction, ANSI/OSC, retention, oversized-read, ID traversal, and symlink tests |
| Events and live views | Typed deny-unknown event documents, strictly increasing sequences, action capability validation, callback deadlines, 4 MiB trace input, bounded live buffers, and cooperative cancellation | core event/action tests, event trace bound/order tests, UI live-buffer cancellation test |
| Picker input | Standard input is UTF-8, at most 4 MiB, and at most 20,000 newline-delimited values before fuzzy selection | `picker_stdin_rejects_oversized_input_and_item_counts_before_selection` |
| Terminal output | Untrusted diagnostic, catalog, completion, plugin, picker, rich-frame, agent, index, package, config, authoring documentation, recovery, and JSON text cannot emit active control bytes; JSON escaping preserves parsed values | core terminal escaping tests, picker C0/C1 test, author stdout C0/C1 test, Ratatui hostile editor/completion buffer test, UI hostile-error test, recovery ANSI/OSC test |
| Accessibility | `auto` selects Ratatui only for a capable TTY; `simple`, non-TTY stderr, `TERM=dumb`, or height below five rows selects Reedline; `NO_COLOR` retains rich layout without color; dumb or non-UTF-8 terminals use ASCII automatically; patched-font glyphs require explicit opt-in; mode/editor state is textual; panels require a plain fallback; noninteractive structured output has no decoration | `explicit_simple_surface_always_degrades`; rich frame/status buffer tests; `terminal_styles_require_an_interactive_color_capable_terminal`; `auto_symbols_only_use_unicode_for_a_unicode_locale`; prompt injection/profile tests; panel fallback/control tests; manual capability checks below |

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
cargo xtask check
```

The exact-candidate refresh additionally ran these focused commands after the
canonical gate and before any evidence document was edited:

```sh
python3 scripts/check-rich-pty.py target/release/quirl
cargo test -p quirl-process --lib
cargo test -p quirl-data --lib
cargo test -p quirl-lua --lib
cargo test -p quirl-cli \
  extensions::tests::installed_command_snapshot_is_nonexecuting_and_typed_dispatch_uses_the_scheduler \
  -- --exact
cargo test -p quirl-lsp --lib
cargo test -p quirl-cli mcp::tests
cargo test -p quirl-cli config::tests
cargo test -p quirl-cli recovery::tests
cargo test -p quirl-ui surface::degrade::tests
cargo test -p quirl-ui surface::tests
```

Every listed test passed. The release-artifact PTY run passed rich editing,
completion, integrated runtime behavior, rich regressions, native job control,
noninteractive dialect islands, suspend/resume, rich/simple fallbacks, and
`NO_COLOR` semantic-hint behavior. These checks exercise actual PTY ownership
on the local macOS kernel, but they do not establish usability in a named
terminal emulator or on Linux.

Manual text-only smoke checks:

```sh
NO_COLOR=1 TERM=dumb cargo run -q -p quirl-cli -- help
NO_COLOR=1 TERM=dumb cargo run -q -p quirl-cli -- complete 'git c'
printf 'return "plain"\n' | NO_COLOR=1 TERM=dumb cargo run -q -p quirl-cli
```

The first two commands must contain no ANSI control sequences. `TERM=dumb`
selects the Reedline fallback with ASCII separators/indicators. In a capable
TTY, `NO_COLOR=1` keeps the Ratatui inline layout while removing color styling;
mode and editor state remain textual. The piped command is noninteractive and
prints only `plain` plus a newline.

The candidate's release binary passed these three smoke checks. A Unicode-aware
scan found zero active C0/C1 controls in all three outputs; the piped result was
exactly `plain\n`.

## Explicit residual risks

1. **Checksums are integrity records, not publisher authentication.** A plugin
   lock detects changes relative to the reviewed local source. v0.1 has no signed
   registry, transparency log, or publisher identity.

2. **Adapter snapshots are hardening, not same-user isolation.** Trusted Lua
   closes its former load-time path race by executing the verified bytes directly.
   Unix process adapters copy verified bytes to a random owner-only directory and
   execute a non-writable snapshot. This prevents later package-path replacement
   from changing the launched code, but it is not an OS sandbox against another
   process already running as the same user. The temporary filesystem must permit
   execution, and adapters must resolve sidecars from the package working
   directory instead of relying on the relocated executable path. Windows remains
   best effort and launches the verified package path directly.

3. **Secret redaction is heuristic.** Recovery redacts values whose environment
   keys look secret, secret arguments and query parameters, authorization headers,
   credentialed URLs, and several high-confidence token shapes. Derived, encoded,
   very short, fragmented, or unusually shaped secrets can remain. Recovery is
   local, quota-limited, private on Unix, and should still be treated as sensitive.

4. **Requested child output is intentionally raw.** Like other shells, Quirl
   allows a foreground external command to control its terminal. Quirl-owned
   metadata and diagnostics are sanitized; output from a command the user chose
   to execute is not. Running an untrusted program therefore carries normal
   terminal-emulator risk.

5. **The simple surface still depends on Reedline.** Ratatui is the default on
   capable TTYs, while explicit `simple`, non-TTY stderr, `TERM=dumb`, and very
   short terminals select Reedline. This provides a reduced, line-oriented
   fallback but is not yet an independently implemented minimal editor. A
   terminal that cannot support Reedline's basic cursor protocol should use the
   stable noninteractive commands or piped stdin. Reedline removal remains
   deferred and is not claimed by ADR 0012.

6. **Windows interactive behavior is best effort.** The backend models normal
   foreground/background lifecycle, cancellation, recovery, and Job Object
   containment, but has not completed native terminal validation. Unix terminal
   process groups and Ctrl-Z suspension have an explicit unsupported diagnostic,
   and there is a small spawn-to-Job-assignment race before containment becomes
   active. These constraints keep Windows outside the supported 1.0 interactive
   scope rather than blocking the Linux/macOS release.

These residuals are visible constraints, not claims of completed isolation.
They should be re-audited when plugins gain a distribution channel or stronger
publisher identity, isolated adapters gain a platform-stable executable handle,
or the simple terminal layer gains a dedicated minimal line editor.
