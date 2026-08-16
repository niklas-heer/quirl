# Quirl release checklist

This checklist turns the current review evidence into a repeatable Unix release.
It applies to the exact candidate commit and artifact; a pass from an older
commit is useful history, not release evidence. Linux and macOS are the
supported interactive platforms. Windows is explicitly outside this release
gate under [ADR 0010](decisions/0010-unix-first-release-scope.md).
The capable-TTY default and retained Reedline fallback are governed by
[ADR 0012](decisions/0012-ratatui-interactive-surface.md); shared theme behavior
is governed by [ADR 0013](decisions/0013-lua-config-themes.md).

This checklist uses two explicit revisions:

- **candidate commit A** is the source revision used to build, measure, record,
  tag, and publish the binary. The version tag and source archives point to A.
- **evidence commit B** is an optional later documentation-only commit that may
  record A's measured results or link reviewed release assets. B never replaces,
  rebuilds, or relabels A. Any code, manifest, generated-contract, dependency,
  or release-profile change requires a new candidate A and a fresh gate.

## 1. Freeze the candidate

- [ ] The worktree is clean and `main` is synchronized with its release remote.
- [ ] `Cargo.lock`, `rust-toolchain.toml`, the release profile, version strings,
  generated SDK, protocol manifest, and user-facing status text describe the
  candidate being shipped.
- [ ] The reviewed config descriptor is schema v3, its golden fingerprint
  matches the candidate, and v0/v1/v2-to-v3 migration tests pass.
- [ ] The supported compatibility boundary is unchanged: C0/C1-core is native
  on Linux/macOS; here-documents, process substitution, loops, functions,
  conditionals, and dialect control forms are explicit Bash/Zsh islands.
- [ ] Deferred and best-effort work is visible in the README and release notes.
  In particular, do not present native Windows terminal behavior or Wasm
  execution as supported.

Record the candidate revision before building:

```sh
git status --short
QUIRL_CANDIDATE_COMMIT=$(git rev-parse HEAD)
export QUIRL_CANDIDATE_COMMIT
printf 'candidate A: %s\n' "$QUIRL_CANDIDATE_COMMIT"
git log -1 --format='%h %s'
```

## 2. Run the automated gate

- [ ] `cargo xtask check` passes with the pinned Rust 1.88 toolchain.
- [ ] The generated Lua SDK has no diff after `cargo xtask sdk`.
- [ ] Website dependencies were installed with `npm ci --prefix website` and
  `cargo xtask website-check` passes. This non-mutating gate checks generated
  mirror/reference freshness, lint, route type checking, and the production
  build using the committed `website/package-lock.json`.
- [ ] The deterministic product tour passes against the candidate binary.
- [ ] The release binary and benchmark harness are built together from the
  clean candidate.

```sh
cargo xtask check
cargo xtask sdk
git diff --exit-code -- docs/quirl.lua
npm ci --prefix website
cargo xtask website-check
cargo xtask demo
cargo xtask release-preview
```

The SDK command intentionally writes a generated file. If it changes, review
and commit the source-of-truth `HOST_API` change and regenerated output, then
restart this checklist from a clean candidate.

## 3. Verify the supported terminals

Run this section once on named Linux hardware and once on named macOS hardware.
Record the OS version, architecture, terminal name/version, and whether a plain
font or a Nerd Font was used. A glyph difference must never change behavior.

- [ ] With `ui.surface = "auto"`, the shell reaches the Ratatui inline frame in
  a normal capable TTY; context, input, diagnostics, and textual status remain
  legible across redraw and resize.
- [ ] The default `tokyo-night` theme and one Lua-configured custom theme apply
  the same semantic roles on rich and simple surfaces; `NO_COLOR` suppresses
  foreground and background colors for both.
- [ ] `config web` renders bounded no-JavaScript preview cards for built-in and
  configured custom themes, and selecting a card updates only the validated
  `ui.theme` literal through the existing backup transaction.
- [ ] The mode indicator is always visible; `Alt-M` switches command/data
  mode and the text labels remain understandable without relying on color.
- [ ] Tab completion, `Ctrl-R` history, `Ctrl-T` files, `Alt-C` directories,
  and `Ctrl-K` catalog actions are keyboard navigable and return the selected
  typed value in the rich surface.
- [ ] `ui.surface = "simple"`, `TERM=dumb`, a non-TTY stderr, and terminal
  height below five rows select the Reedline fallback rather than attempting
  the Ratatui frame.
- [ ] A foreground command receives the terminal and returns it cleanly.
- [ ] The rich frame releases raw mode, bracketed paste, viewport, and cursor
  state before foreground execution and `Ctrl-Z`, then reconstructs cleanly.
- [ ] `command &`, `jobs`, `fg`, `Ctrl-Z`, and `bg` show coherent job state.
- [ ] `&&`, `||`, redirects, here-strings, bounded command substitution, and
  pathname expansion execute through the native C1-core graph.
- [ ] Unsupported process substitution and compound control syntax produce an
  actionable dialect-island diagnostic.
- [ ] An explicit Bash island and Zsh island execute without loading user RC
  files. If one interpreter is not installed, record it as an optional missing
  reference runner rather than silently changing dialect.
- [ ] `NO_COLOR=1` and `TERM=dumb` produce legible, control-safe text with ASCII
  separators and no dependence on patched-font glyphs. `NO_COLOR` retains the
  rich layout on a capable TTY; `TERM=dumb` uses the simple fallback.
- [ ] A narrow terminal, a resized terminal, and SSH or a comparable remote PTY
  remain usable without clipped essential state.

The exact commands and adversarial text-only checks are also recorded in the
[security and accessibility audit](security-accessibility-audit-v0.1.md).

## 4. Refresh performance evidence

Do not edit measurements by hand or reuse the previous digest. On an idle,
named supported machine, compute the candidate binary's SHA-256 independently,
then pass that exact digest to the enforcing harness:

```sh
if command -v sha256sum >/dev/null 2>&1; then
  QUIRL_EXPECTED_SHA256=$(sha256sum target/release/quirl | awk '{print $1}')
else
  QUIRL_EXPECTED_SHA256=$(shasum -a 256 target/release/quirl | awk '{print $1}')
fi
export QUIRL_EXPECTED_SHA256
printf 'candidate SHA-256: %s\n' "$QUIRL_EXPECTED_SHA256"
target/release/quirl-bench release \
  --quirl target/release/quirl \
  --expected-sha256 "$QUIRL_EXPECTED_SHA256" \
  --json
```

`cargo xtask release-gate "$QUIRL_EXPECTED_SHA256"` is the concise enforcing form
when the human-readable report is sufficient; it also runs the explicit website
gate and therefore requires the locked website dependencies to be installed.
Use the direct command above to capture canonical JSON evidence.

- [ ] The harness accepts the clean revision embedded independently in both
  `quirl` and `quirl-bench`, their matching source identity, the artifact
  profile, panic strategy, operating system, architecture, and independent
  digest.
- [ ] All enforced PTY latency, first-prompt, binary-size, and bounded-history
  budgets pass, or the release stops with the miss preserved in the record.
- [ ] The exact release binary is at or below the 10 MiB hard ceiling
  (10,485,760 bytes). At or below 5 MiB (5,242,880 bytes) is ideal; a binary
  above the 8 MiB soft cap (8,388,608 bytes) records a warning without weakening
  the hard gate. `--max-binary-bytes` may tighten but never raise the ceiling.
- [ ] Rich-surface draw and edit latency are measured on the selected
  candidate; `QUIRL_UI_TIMINGS=1` is diagnostic evidence, not a substitute for
  the enforcing PTY harness.
- [ ] The release evidence names candidate A's exact revision, artifact digest,
  hardware, OS, Rust version, sample counts, results, and limitations. It may be
  captured as a release asset first and checked into
  `docs/benchmarks/release-v1.0.md` later in evidence commit B.
- [ ] The performance record and the binary intended for publication describe
  the same artifact. Any source or release-profile change invalidates the run.

## 5. Capture the real-terminal demo

The README demo must be a recording of the release binary in a real PTY, never
an animation assembled to resemble terminal output. Use a fresh temporary
Quirl config/state directory so personal history, paths, repository remotes,
usernames, and secrets cannot appear. Set the terminal to 120×32 or another
documented fixed size and keep the take short enough to understand without
narration.

[`scripts/demo.tape`](../scripts/demo.tape) is the reproducible VHS capture
recipe, while `cargo xtask demo` is the accessible text-only companion. Record only the
already-built, measured candidate A artifact:

```sh
scripts/record-demo.sh target/release/quirl "$QUIRL_EXPECTED_SHA256"
```

The wrapper verifies the digest before launching VHS, requires `vhs`, `ttyd`,
`ffmpeg`, and `JetBrainsMono Nerd Font`, and uses the same private, disposable
Quirl environment as the text tour. The recipe may automate keystrokes and
timing, but every visible frame must still come from a real invocation of the
candidate binary. A missing recording prerequisite is a preflight failure, not
permission to substitute fabricated terminal output.

Recommended shot list:

1. Start the exact release binary and show that `auto` selected the inline
   Ratatui frame with context and textual status.
2. Type part of a real command, open semantic completion, move once, and close
   it without hiding the prompt.
3. Run a short native byte pipeline and a boolean list.
4. Switch to data mode and run a compact structured pipeline whose typed result
   fits on screen.
5. Open the history or file overlay and select an item with the keyboard.
6. End on one brief explicit Bash/Zsh island or a helpful unsupported-syntax
   diagnostic, making the compatibility boundary visible rather than implied.

Before publishing the capture:

- [ ] Watch the complete recording at normal speed and inspect individual
  frames around menus and mode switches.
- [ ] Verify that every command and result came from the candidate binary.
- [ ] Verify there is no secret, personal path, username, hostname, private
  repository, or unrelated shell history in the capture.
- [ ] Verify the experience remains understandable in the repository's static
  fallback image or alt text.
- [ ] Verify prompt symbols are decorative: a plain-font capture remains
  readable, and the recording does not tell users a Nerd Font is required.
- [ ] Add the reviewed artifact to the README with its capture environment and
  a nearby link to the text-only product tour.

## 6. Package and publish

- [ ] Return to candidate A and confirm its worktree, revision, artifact digest,
  and embedded source identity. If A changed, stop and choose a new candidate.
- [ ] Do not rebuild after measuring. The distributable, terminal recording,
  checksums, and version tag all identify the already-gated A artifact.
- [ ] Draft release notes with supported platforms, native compatibility scope,
  reference-shell behavior, known residual risks, and upgrade/migration notes.
  For 0.1, note that config schema v3 adds shared semantic themes on top of the
  v2 rich-surface settings; legacy unversioned v0 and explicit v1/v2 config
  migrate to v3 defaults, and no published config contract is being silently
  reinterpreted.
- [ ] Create an annotated version tag only after every required Linux/macOS gate
  above is signed off, and point it explicitly at A, even if HEAD has moved to
  evidence commit B: `git tag -a v0.1.0 "$QUIRL_CANDIDATE_COMMIT"`.
- [ ] If desired, create evidence commit B after tagging A. Restrict B to
  measured records, reviewed demo links, checksums, and release-note evidence;
  never use B's revision as the artifact source identity.
- [ ] Publish the measured artifacts and checksums, then verify a clean install
  starts and reports A's expected version and source identity. Attach evidence
  produced after A as release assets or link it from B without rebuilding A.
- [ ] Keep the tag and release immutable. Corrections use a new version rather
  than replacing a measured artifact in place.

## Not release blockers

The following work can improve Quirl later but is not silently attached to this
Unix 1.0 gate:

- native Windows terminal handoff, suspension semantics, and Windows hardware
  validation;
- native implementation of here-documents, process substitution, loops,
  functions, conditionals, or other Bash/Zsh dialect control forms;
- a Wasm execution engine, remote plugin registry, publisher identity, or
  signed transparency log;
- exact emulation of Bash/Zsh option state or framework-sized startup files.
- removal of Reedline or replacement of the current simple-terminal fallback.

These are explicit support boundaries. A future project may promote one only
with its own implementation, adversarial tests, documentation, and release
evidence.
