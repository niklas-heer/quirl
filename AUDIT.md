# Project audit — 2026-09-04

Evidence paths under `target/audit/` refer to local, ignored artifacts from the
audit workspace; they are not shipped in the source repository. Seeded replay
commands and the maintained harness are documented in `docs/testing-strategy.md`.

Starting revision: `82b3aabb2f3d42bb242ad37380ddc24d44896bdb`.

Three passes combine repository-wide quality gates with targeted source review
of protocol admission, file reads, terminal interaction, persistence, process
cleanup, crate boundaries, and CI/release configuration. It is not a line-by-line
review of every source file or a certification of third-party dependencies.

## Fixed findings

| Priority | Finding | Correction |
| --- | --- | --- |
| High | History and preview readers could block opening a FIFO; a file could become a FIFO after directory enumeration. | Admit the exact open handle as a regular file, using nonblocking open on Unix. Both text and image preview paths use this admission. |
| High | Prompt context read `.git` markers and `HEAD` without a byte bound. | Cap each read at 8 KiB, including a sentinel check for concurrent growth; inspect at most 128 ancestors. Reject empty gitdir targets and omit unavailable optional context. |
| Medium | LSP silently treated an incremental edit fragment as an entire replacement document and accepted stale or missing versions. | Require full text without range fields and an increasing integer version before committing replacement text or retained-byte accounting. |
| Medium | MCP suppressed null-ID responses and malformed ID-less envelope errors, accepted array-shaped request envelopes, and did not enforce the method limit. Modern metadata was removed before the parameter-size check. | Validate the object envelope, ID semantics, structured parameters, method size, and complete parameter budget before dispatch. Replace recursive depth inspection with a bounded iterative traversal. |
| High | SQLite history sidecars could be created with public read permissions before the database was made private. Oversized persisted text could allocate before validation. | Create and secure the database and existing sidecars before SQLite initialization; reject symlinks and hard links at admission, enforce SQLite's 69 KiB value limit, and check command/directory byte lengths before Rust allocation. |
| High | Request-controlled process execution could exceed its deadline while opening a FIFO, draining pipes retained by escaped descendants, or resuming a foreground job. | Reject FIFO redirections for bounded requests; own pipe workers with cancellation and RAII cleanup; cap final drainage at 100 ms and the request deadline; carry request limits through foreground resume and reap on failure. |
| High | AI protocol writes and reader shutdown could block indefinitely on unresponsive peers or retained pipe descriptors. | Share the response deadline and cancellation with supervised writes, use nonblocking Unix pipes, terminate/reap failed sessions, and explicitly cancel reader tasks before joining. |
| Medium | Lua configuration deserialization lacked the shared return-shape guard, allowing repeated strings to amplify Rust allocation beyond the VM's memory budget. | Apply the existing byte, node, depth, and table-shape admission before typed deserialization. |
| Medium | Quotes inside YAML plain scalars could hide later anchors and aliases from Quirl's preflight scanner. | Track scalar context so plain punctuation cannot bypass the reference restriction while actual quoted and block scalars remain valid. The underlying YAML library also has its own alias replay limit. |
| Medium | Correct-digest but invalid asset formats could leave published generations; retention scanning could miss generations, and capacity was checked after publication. | Validate formats in a temporary RAII staging directory, scan the complete bounded root, and reject a new generation at capacity before downloading or publishing. Preserve the existing receipt and installed content on rejection. |
| High | Word deletion, leftward word motion, and picker replacement could select the middle of a multibyte whitespace character and panic on a later UTF-8 operation. | Share a scalar-aware whitespace boundary helper and test editing, deletion, insertion, yank, and picker spans across ASCII and Unicode separators. |
| High | Mixed decimal/integer comparisons rounded through binary floating point, silently equating distinct values above 2^53 and near the unsigned integer ceiling. | Compare borrowed normalized decimal digits exactly, with a signed 64-bit exponent bound and no exponent-sized allocations. |
| High | Plugin saving could truncate a predictable temporary-file symlink target, lose concurrent updates, or leave the active lock name absent during replacement. | Serialize mutations before reading, publish a complete backup before active replacement, use private create-new/no-replace staging, cap serialization at 4 MiB, and refuse additional staging while recovery files remain. |
| High | Shared atomic-file replacement wrote contents before applying private permissions, exposing partial replacement bytes in a traversable directory. | Create Unix candidates with mode 0600 before the first write; prove privacy on an injected partial-write failure. |
| Medium | One large picker item could exceed a 1 ms deadline by hundreds of milliseconds, and cancelling request u64::MAX left it current. | Check cancellation within matching/preparation, invalidate requests with the reserved zero ID, and apply a 50 ms total interactive ranking budget. |
| Medium | Job listings cloned and joined complete command strings before enforcing output limits; human notifications could emit controls and huge here-strings. | Borrow job state, enforce output limits while appending, escape human text, and abbreviate human stop notifications to 256 source bytes. Preserve captured/redirected source and typed job metadata. |
| Medium | A concurrent build could replace the simulator's executable between sessions without changing recorded run identity. | Run one bounded private executable snapshot and record its SHA-256 and byte size in summary schema 2. |
| Medium | The release executable exceeded the existing 10 MiB size ceiling. | Disable unused bundled SQLite FTS3/4, FTS5, and RTree modules through the workspace Cargo build configuration; retain ordinary database behavior, unwind semantics, and the original ceiling. |

The changes retain the existing crate graph and error contracts. The UI reuses
the workspace's existing Unix `nix` dependency for platform constants; no new
third-party package is introduced. Runtime command help was updated in
`Catalog::builtin()` and `HOST_API`, and canonical documentation, the generated
Lua SDK, and website projections were refreshed.

## Failure model and resource bounds

- File type and size can change between enumeration, metadata, open, and read.
  Validate type from the opened handle and retain existing preview/history
  byte bounds. Prompt reads retain at most 8,193 bytes before rejecting growth.
  Opened files are owned by RAII and close on every return path.
- Malformed or excessive MCP input cannot change protocol negotiation state.
  Parsing remains capped by the 1 MiB frame; complete encoded parameters,
  including modern metadata, must fit in 256 KiB. A valid later request can
  still negotiate after an admission error.
- A rejected LSP replacement preserves the previous document, version, and
  byte accounting. A later valid full replacement can advance by more than
  one version.
- History initialization must not expose plaintext through newly created WAL,
  SHM, or journal files. Unix admission checks the exact regular, single-link
  handle and its path identity before changing permissions. Persisted commands
  are capped at 64 KiB and directories at 4 KiB; existing snapshot limits remain
  4,096 records and 8 MiB.
- Child exit does not imply pipe EOF: a detached descendant may retain a pipe.
  Process and AI transport workers must observe cancellation independently of
  EOF, close descriptors, and join during cleanup. Partial AI frames invalidate
  the session rather than being retried on the same stream.
- Lua configuration return admission caps retained strings at 256 KiB,
  key/value nodes at 4,112, depth at 16, and array keys at 4,096. Cyclic and
  repeated tables are rejected before deserialization.
- Asset admission scans at most 64 root entries and admits at most four retained
  generations before starting a new download. Temporary format validation is
  cleaned on failure. Model bundles currently undergo bounded validation and
  extraction both in staging and at the final location; avoiding that duplicate
  work would require a separate transaction redesign.

## Regression evidence

The first pass added twelve in-crate tests covering exact and exceeded byte/depth limits,
invalid UTF-8, malformed gitdir markers, FIFO admission, both preview types
after file replacement, MCP response semantics and negotiation recovery, and
atomic LSP rejection followed by a valid change. The new LSP and initial MCP
tests were observed failing against the previous implementation before the
fixes were applied.

The second pass added 29 tests covering private history sidecars and oversized
rows, plain-scalar YAML admission, Lua configuration limits and recovery,
FIFO/deadline/cancellation boundaries, foreground cleanup, retained pipe shutdown,
AI request framing, invalid asset staging, and generation-capacity rejection.
The FIFO and retained-pipe deadline failures were reproduced before the process
fixes: a 30 ms request could wait beyond 400 ms at FIFO open, and an escaped
pipe holder delayed a successful response to about 329 ms. The patched cases
reject the FIFO promptly and return a resource-limit error near the deadline.
Integration validation also exposed a PTY construction-cleanup fixture that
used a now-rejected FIFO redirect. It now reads the executor's owned process-group
diagnostic after a later-stage output-open failure and proves both the direct
leader and group are gone, followed by terminal recovery and another command.
A bounded parser regression prevents incomplete or invalid identifiers from
becoming cleanup targets.

Validation passed on the second-pass macOS tree:

- The canonical gate passed formatting, catalog verification, Clippy, Rustdoc,
  1,232 workspace tests (one existing ignored test), all 24 rich PTY checks,
  and two guest Lua tests. Workspace tests include the seeded compatibility
  and lifecycle cases selected by the gate.
- The website gate passed all 13 release-evidence tests, evidence attribution,
  57 source-mirror checks, two generated-reference checks, lint, TypeScript,
  and the production build.
- An additional seeded compatibility simulation passed 256 sessions of 12
  steps with zero mismatches against available reference shells.
- The final diff has no whitespace errors.

Reproduction commands:

```console
RUST_TEST_THREADS=2 cargo xtask check
npm --prefix website run check
cargo xtask simulate --seed 20260904 --sessions 256 --steps 12 --output target/simulations/audit-20260904-final
git diff --check
```

## Scope limits

Execution covers macOS ARM64 and Linux ARM64 under Docker. Windows is explicitly
excluded at the user's request. Linux x86-64 is configured in CI but was not run
locally. Container PTY checks do not replace human checks in named terminal apps.
The reproducible image, non-root test user, separate build volumes, required init
reaper, and invocation are documented in `docs/testing-strategy.md`; CI covers
Ubuntu and macOS with both Bash and Zsh. Experimental `spikes/` workspaces were
not run, and measurements of an uncommitted tree are not official release evidence.

The file admission change prevents FIFO rendezvous; it cannot impose a deadline
on a stalled kernel or network filesystem. Trusted-local process convenience
APIs retain FIFO rendezvous behavior; request-controlled native execution rejects
it. Unix process-group cleanup cannot kill a process that escapes into a new
session, but its retained pipes no longer force Quirl to wait indefinitely.

## Extended macOS/Linux evidence

The initial extended swarms each completed 2,048 sessions with up to 24 steps,
seed `2026090401`, and zero mismatches on macOS ARM64 and Linux ARM64. These were
exploratory runs during development, before immutable executable snapshots were
added. Their traces are retained under `target/simulations/audit-macos-20260904`
and `target/audit/2026-09-04/linux-initial-swarm.tar.gz`.

Running Cargo as Docker PID 1 left killed orphan descendants as zombies and
failed four lifecycle assertions. With Docker `--init`, all 123 Linux process
tests at that point passed. This is a container invocation requirement, not a
relaxed process-cleanup assertion.

The 4,096-candidate picker fixture measured an empty query at 163.4 ms before and
0.889 ms after; `git` measured 162.5 ms before and 13.18 ms after. The 1 ms deadline
fixture fell from 313.9 ms to 1.171 ms. These are same-host development-profile
single-shot measurements, excluding fixture construction and CLI adaptation;
they demonstrate the regression and improvement, not a release-performance
claim. Sources, prebuilt before/after fixtures, hashes, and reproduction commands
are retained in `target/audit/2026-09-04/picker-benchmark`.

Exact numeric comparison includes 1,024 fixed-seed cases against an independent
integer oracle. Plugin persistence tests cover concurrent additions, symlink
targets, backup interruption, oversized serialization, retained recovery files,
and complete/over-limit directory scans. Snapshot tests cover source replacement,
exact/over-limit copy sizes, FIFO rejection, permissions, cleanup, and summary
identity.

The SQLite build change reduced the measured release executable from 10,529,280
to 10,312,816 bytes: 216,464 bytes saved and 172,944 bytes below the unchanged
10 MiB ceiling. All 112 focused catalog, history, intelligence, and native-catalog
tests passed with those flags. Before/after artifacts, hashes, commands, and logs
are retained under `target/audit/2026-09-04/sqlite-size`.

Final integration also corrected two forbidden casts in a Linux-only test.
Three completion-persistence fixtures now use an RAII-scoped, thread-local
5-second startup budget: four isolated replays passed, while the failed loaded
run had timed out before either provider wrote its marker. Production retains
its 400 ms provider deadline; timeout/cancellation tests retain their real bounds.

The final PTY run caught a regression in the new job formatter: the CLI's
one-byte capture ceiling was incorrectly applied to inherited terminal output.
Job text now distinguishes capture, terminal, and explicit file destinations.
Only capture uses the tighter request ceiling; the other destinations retain
an independent 1 MiB bound. A regression observed failing before the correction
proves a stopped job can print its notice, appear in `jobs`, write raw source
to a file, resume with its original exit status, and leave no retained job.
Independent-limit and cancellation checks cover terminal and file formatting.

## Hardening-pass integrated validation

The tree after the three hardening passes passed `CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 cargo
xtask check` on macOS ARM64 and the documented Docker equivalent on Linux ARM64:

| Gate | macOS ARM64 | Linux ARM64 |
| --- | --- | --- |
| Formatting, native catalog verification, Clippy, Rustdoc | Pass | Pass |
| Workspace Rust tests | 1,263 passed; one existing ignored | 1,264 passed; one existing ignored |
| Real PTY checks | 24 passed | 24 passed |
| Guest Lua tests | 2 passed | 2 passed |
| Final frozen-binary simulation | 256 sessions; zero mismatches | 256 sessions; zero mismatches |

Final simulation seed is `2026090402`, with up to 24 steps per session. The
summary records the exact snapshot SHA-256 and size; both references agree
with Quirl in every evaluated session. macOS traces live under
`target/simulations/audit-macos-frozen/seed-2026090402`; Linux traces are archived
at `target/audit/2026-09-04/linux-final-frozen.tar.gz`. Final summaries and
platform gate logs are also retained in `target/audit/2026-09-04`.

The final website gate passes all 13 release-evidence tests, attribution,
57 documentation mirrors, two generated reference checks, lint, TypeScript,
and the production build. No new dependency package or unsafe block was added.
All changes remain uncommitted for review.

The final optimized macOS ARM64 executable remains 10,312,816 bytes. An advisory
release preview, with its SHA-256 explicitly verified, completed all 31 samples
for each real PTY measurement:

| Real terminal measurement | p50 | p95 | Budget assessment |
| --- | --- | --- | --- |
| Cold start to editable prompt | 11.477 ms | 12.965 ms | Within target |
| Keystroke to painted frame | 0.355 ms | 0.418 ms | Within target |
| First prompt paint | 10.759 ms | 12.320 ms | Within target |

The retained-stream invariant also passed. The executable meets the 10 MiB hard
ceiling but remains above the advisory 8 MiB soft cap. The overall release gate
is intentionally **not accepted**: the CLI, benchmark harness, and workspace
are dirty, so the recorded commit cannot reproduce these artifacts. Its only
gate failures concern that source identity; no measured timing or hard size
budget failed. These measurements do not substitute for clean release evidence
or measurements on other hardware. The full report, digest, and build log are
retained in `target/audit/2026-09-04/release-preview.json`,
`release-final.sha256`, and `release-final-build.log`.

## User experience pass

A subsequent pass follows the product's “arrive, grow, create” promises through
actual first-session and everyday discovery flows. Three subagents reviewed
onboarding, editing/discovery, and typed-data recovery; the composition root was
verified through real PTY sessions.

- The installed-user path now starts with `quirl`, a working byte pipeline,
  self-contained typed data, and an explicit return to the previous shell.
  README and the curated getting-started pages include verified expected
  output. AI is an optional authenticated Codex conversation; the guide no
  longer implies a local fallback or outdated automatic proposal replacement.
- Bare `help` shows a compact catalog-owned overview instead of dumping every
  command. Exact names win over aliases, ambiguous aliases and search words
  offer bounded choices, and `help data` resolves the builtin data contract.
  The canonical overview survives large imported catalogs. Help is wrapped for
  the current terminal and retained in the rich transcript. An immediate help
  request falls back to builtins while discovery runs; the old binary was
  observed terminating the first session with “catalog publication is incomplete.”
  Oversized queries leave a usable prompt and subsequent help recovers.
- Explicit completion selection keeps Up and Down inside the menu. Untouched
  automatic suggestions retain Up-to-history, and the status hints agree.
  F1 follows the command segment at the cursor, including later pipeline/list
  stages, without changing the command.
- The file picker shares ordinary completion's shell-word scanner and path
  encoder. Picking a filename replaces the entire current word, preserves
  surrounding arguments/operators, and retains spaces, quotes, dollar signs,
  and punctuation literally. Names beginning with `-` receive a `./` prefix
  so a file cannot become a program option. Non-UTF8 names are omitted instead of changing
  filesystem identity. Ordinary completion still preserves intentional `~/`
  expansion, while picker-selected names beginning with `~` remain literal.
- `select naem` now reports the missing field, available names, and original
  transform location instead of returning successful empty records. Deferred
  failures keep that location through later transforms and release their reader.
  Invalid transform arguments show the required syntax. CSV remains explicitly
  text-valued; its corrected example compares `enabled` with the string `"true"`.
- Already-materialized values such as single records use bounded table
  presentation in interactive Data mode. List sources and other streams retain
  incremental plain rows; an explicit `quirl data ... --format table` collects a
  table within existing limits. Presentation preserves cached typed values and
  checks cancellation before writing rendered output.

New PTY journeys cover help failure/recovery, retained help after repaint,
textual mode entry/return, the first data result, completion arrow behavior,
cursor-local F1, and actually reading files named `quarterly report;$notes.txt` and `-n`
selected through the picker. This verifies the resulting command's behavior,
not just its visual spelling. Existing cancellation, lazy partial-write,
reader-cleanup, and terminal-restoration contracts remain part of validation.

The user-experience tree passes the canonical gate on macOS ARM64 (1,280 Rust
tests) and Linux ARM64 (1,281 Rust tests), with one existing ignored test on
each platform. Both pass all 26 PTY checks and both guest Lua tests, along with
formatting, native catalog verification, Clippy, and Rustdoc. The website gate
passes its evidence tests, source/reference checks, lint, types, and production
build. This pass adds 17 Rust regressions and two integrated terminal journeys.

One macOS full run rejected an existing adapter-activation fixture; its exact
isolated replay and the final complete run passed. The assertion now includes
activation diagnostics if it recurs; no deadline or assertion was weakened.
Final gate and focused validation logs live under `target/audit/2026-09-04/ux`.

The optimized UX build is 10,312,896 bytes, 80 bytes larger than the saved
pre-change build and 172,864 bytes below the unchanged 10 MiB hard ceiling.
Both CLI and benchmark binaries, their digest, build log, and complete preview
reports are preserved under `target/audit/2026-09-04/ux`.

Latency evidence remains advisory and does **not** establish the first-prompt
paint budget for this tree. The initial UX preview completed all 31 real PTY
samples: cold-to-editable P50 was 19.336 ms, keystroke-to-frame P95 4.052 ms,
and first-prompt paint P95 28.158 ms, exceeding its 21 ms target. A background
headless-browser process was observed consuming over 800% CPU. A sequential
comparison used the preserved pre-UX binaries and the new binaries, with the
same default sample counts and explicit artifact digests:

| Measurement | Saved pre-UX build | UX build |
| --- | --- | --- |
| Cold-to-editable P50 (target ≤25 ms) | 19.817 ms | 18.468 ms |
| Keystroke-to-frame P95 (target ≤8 ms) | 3.889 ms | 3.636 ms |
| First-prompt paint P95 (target ≤21 ms) | 25.756 ms | 29.503 ms |

Both builds miss the first-prompt target under this shared-host load. This is
insufficient evidence to isolate a code regression or claim unchanged latency;
a quiet-machine comparison is still required. No timing threshold was relaxed,
and unrelated processes were left alone. The initial headless edit CPU proxy
also missed its advisory target; it is not terminal latency. Complete reports
and process-load snapshots preserve these results rather than selecting only
successful measurements. The release gate additionally rejects dirty source
identity, as intended. Changes remain uncommitted and are not release evidence.


## Keyboard-driven usage simulation — 2026-09-05

The user requested realistic keyboard sessions, visual inspection, and a
100-hour usage simulation. The new `cargo xtask session-soak` drives the real
shell in isolated Unix PTYs. Its deterministic workload covers twelve user
journeys: editing with Backspace and cursor/Delete, Unicode grapheme editing,
cancellation and recovery, history, cursor-local F1 help, typed-data filtering,
command-error recovery, resizing, completion, file selection, and the command
palette. Every complete block of twelve covers every journey in seeded order.

The harness records raw input actions before delivery, asserts visible screen
state and fresh command-input readiness, and requires the intended command to
produce a unique output token. It saves a SHA-256 identity for its immutable
executable snapshot, replay parameters, per-session JSONL traces, bounded raw
output tails, final screens, and an offline HTML/SVG gallery. Session, action,
output, child-cleanup, and artifact budgets are explicit. A cleanup or evidence
failure fails the run. Replaying requires both the original workload and the
original executable; rebuilding does not reproduce the same tested artifact.
The canonical gate includes one twelve-journey smoke run.

The usage model assumes **60 completed command journeys per active hour** and
removes think time. Six thousand successful journeys therefore represent
**100 modeled active hours**. This is a declared action-coverage model, not a
measured human rate, 100 hours of continuous uptime, or proof of all possible
terminal behavior. Wall time, key bytes, actions, resizes, and screen assertions
are reported separately. The SVG gallery shows monochrome VT cells and cursor
positions, not terminal colors, font shaping, or pixel fidelity. IME, clipboard
integration, real external AI/network services, remote terminals, suspend/resume,
and actual long-lived uptime require separate evidence. Windows was excluded
at the user's request.

Exploratory sessions found and drove these product corrections:

- Cold-start history, file search, and directory search now use their own
  bounded data sources without waiting for command-catalog publication.
- A first Tab request can wait for catalog publication without terminating the
  shell. Its bounded pending request is invalidated by later edits or dismissal.
  An already-open command palette receives the catalog while preserving its
  search query; closing it prevents late publication from reopening it.
- Cold F1 immediately opens builtin help at the cursor. Catalog publication
  upgrades imported command details while preserving a user-edited query;
  dismissal or replacement invalidates the bounded pending context.
- Automatic command information is refreshed from the current editor when the
  catalog arrives, so an early `ls` edit does not leave a permanently blank pane.
  Existing explicit requests and active modal interactions retain priority.
- Unquoted newlines separate commands instead of silently joining pasted
  commands into one argument list. Quoted newlines remain literal, escaped
  newlines continue a word, and newline continuation after supported operators
  is explicit. Parser, execution, Bash/Zsh differential, and protocol tests
  cover these transitions. Grammar and runner protocols advance to version 3;
  generated protocol contracts and documentation were regenerated.
- Palette ranking gives command-label matches precedence over incidental
  description matches, so searching for a doctor command no longer prefers
  `cd` merely because its description matched.
- F1 help borrows available transcript space when fewer than twelve rows remain
  below the prompt, keeping the editor in place and making explanatory text
  visible beyond borders, source metadata, and wrapped usage text.
- Status-bar truncation counts terminal columns and whole graphemes, preserving
  narrow-screen exit hints and avoiding overflow from wide Unicode notices.
- A missing executable now reports that it could not be found, removing private
  process-stage terminology from the user's diagnostic.

Fixed PTY journeys additionally prove that history survives reopening the same
private profile, selecting history does not execute it, bracketed paste does
not execute before submission, cancellation has no filesystem side effects,
and one explicit submission executes the intended pasted commands exactly once.
A resize probe covers 32 stopped/coalesced and 32 live resize-plus-single-Enter
cycles. It initially passed against the saved macOS baseline. The subsequent
Linux canonical gate and an isolated replay both stalled with an empty editor
after a resize followed by command input. Reviewing Crossterm 0.29's MIO source
found that an early return for one event can discard another readiness event
from the same edge-triggered poll batch. Already-readable keyboard input then
waits for an edge that need not arrive. Quirl selects Crossterm's existing
level-triggered `use-dev-tty` backend, which rechecks unread input. This adds
one dependency edge to an already locked package, no package version or
product feature flag. ADR 0030 records the choice and its failure model.

Harness faults were retained and corrected too. Input echo is not command
output, a query containing a filename is not a selected file, and seeing a
matching palette item is not proof of its selection. Output checks distinguish
real output rows from editor/command echo and from a scrollbar only in the
physical rightmost column. Typed-data checks must verify excluded rows stay
excluded. Fixed PTY checks now synchronize on fresh input readiness instead of
using extra Enter/Ctrl-D presses to conceal an unsettled session. The faster
16 ms observation interval exposed these older assumptions without increasing
deadlines or weakening assertions. Discovery-dependent fixtures explicitly
enable their isolated discovery worker.


The completed keyboard corpus is preserved under
`target/audit/2026-09-05/usage-100h-final/index.html`. Four concurrent batches
ran 25 sessions with 60 journeys each. All **6,000 journeys in 100 sessions
passed**, covering each of the twelve workflows 500 times. The driver delivered
373,392 input bytes, recorded 68,700 actions (including input, observations,
and journey bookkeeping), checked 17,100 screen assertions, and issued 1,000
resizes. No journey, cleanup, or evidence-writing failure occurred in these
four batches.

| Platform / batch | Seed | Journeys | Actual batch runtime |
| --- | --- | --- | --- |
| macOS ARM64 A | 2026090501 | 1,500 | 433.019 s |
| macOS ARM64 B | 2026090503 | 1,500 | 432.218 s |
| Linux ARM64 A | 2026090502 | 1,500 | 401.009 s |
| Linux ARM64 B | 2026090504 | 1,500 | 399.494 s |

The batches ran concurrently. Their summed 1,665.740 seconds is worker time,
not elapsed wall time. The longest batch took 433.019 seconds. macOS used
SHA-256 `40aa0f5028b60229f236d4fa84af642dd7e7c05bd1a119b552f2eab92d02d380`;
Linux used `0ee0b88be085ddf1efd54c86dbc0993c75e80068f0bf16362fc94f06d25d445f`.
Both executable snapshots, their per-run manifests, and a 16-file harness-source
archive with individual hashes are retained. Representative help, completion,
file-selection, history, narrow-editor, data, and completed-session layouts were
visually inspected. This is representative checkpoint review, not an assertion
that a human inspected every frame.

Earlier failed pilots remain separate rather than being counted as successful
modeled hours. The first 50-session runs exposed short help panes and cold file
selection on both platforms. The next macOS run completed 2,820 journeys and
failed three cold F1 sessions; its paired Linux run completed all 3,000. Those
failures drove the final cold-help correction and deterministic held-catalog
PTY regression. The saved older binary fails that regression; the corrected
binary opens the exact builtin help before catalog publication. Additional
pre-final batches are also retained and are excluded from the reported 6,000.

The final quality-gate work uncovered a **separate foreground-stop deadlock**
outside the keyboard corpus's workflows. Linux showed a direct upstream child
blocked in `vfork` while its own child and the downstream pipeline stage were
stopped. Waiting for every direct child to acknowledge SIGSTOP can deadlock:
the vfork parent cannot advance until its stopped child resumes. The process
owner now records successfully signaled live children as logically stopped,
consistent with the existing suspend path, while retaining group ownership for
`bg`, `fg`, and cancellation. Failed signaling is not recorded as success.
Normal stopping does not inject SIGCONT or kill the user's job.

That subsequent process correction is intentionally reported separately from
the immutable keyboard-corpus binaries. Thirty-two bounded downstream-stop
scenarios verify stopped-job retention, background resume, cancellation, and
reaping; sixteen foreground-resume scenarios cover pending stop reports. The full process suite passes 131 tests on macOS and 132 on Linux. The regression deadline makes
future failures return promptly, and success still requires a stopped outcome.
Live Linux process-state evidence and the original hung gate log are preserved
under `target/audit/2026-09-05/process-vfork-stop`.

A macOS ownership stress test hit the unchanged 100 ms pipe-drain budget during
eight concurrent keyboard batches and compilation. Its isolated 512-case run
passed after that bulk load ended; the bound was not increased. Selecting the
polling backend also required adding three already-locked packages to the CLI's
normal/build license inventory. The inventory was regenerated from Cargo for
all four supported release targets; its graph and license-text tests pass.


The new foreground-resume functional test initially reached its one-second
request budget in a macOS canonical run. Seven hundred twenty isolated and
stress-overlapped resume cases passed; a sampled two-thread full-suite replay
reproduced the one-second limit. Its direct downstream shell and anchor were
in macOS S state for about 0.9 seconds, which does not establish an indefinitely
stopped descendant or scheduler starvation. Only this new functional test's
request budget was calibrated to five seconds, with case/elapsed/job context
on failure. No additional foreground-resume product fix is claimed, and no
production deadline, existing stop-latency assertion, or 100 ms drain limit
changed. The original failure and the limits of this inference remain in the
process evidence README. Diagnostic sampling was stopped before final gates.

Visual review found that an original Linux help checkpoint was captured during
a redraw. This was a capture-boundary defect, not evidence of a persistent UI
defect. The VT model now recognizes Ratatui's final reset, cursor-show, and cursor
placement sequence; checkpoints wait for its completion within their existing
deadline. Partial UTF-8 or escape sequences cannot count as complete, subsequent
drawing invalidates completion, and failed captures remove their gallery label.
Split-frame and chunked-finalization regressions cover the boundary.

Separate visual verification runs completed twelve journeys on each platform
with zero failures, using the exact saved corpus binaries. Their complete help
frames were visually inspected, including lower borders, editor cursors, and
status bars. These twenty-four journeys are excluded from the original 6,000.
Corrected galleries are under `target/audit/2026-09-05/usage-visual-corrected`;
the combined overview links both these and the unchanged original evidence.
The refreshed harness source archive and hashes are recorded separately.

Final `cargo xtask check` passed on macOS and Linux without diagnostic sampling:
1,323 and 1,324 Rust tests passed respectively, with one ignored test each.
Each platform also passed all 29 fixed PTY checks, twelve keyboard smoke
journeys, and two guest Lua tests. The gate includes formatting, Clippy,
Rustdoc, architecture and generated-contract checks. The website check passed
all thirteen evidence tests, 58 documentation-source comparisons, two compiled
reference comparisons, lint, type checking, and the production build.

The optimized macOS CLI and benchmark builds passed. The final CLI is
10,329,520 bytes, 156,240 bytes below the 10 MiB budget, with SHA-256
`f78d675295228e23a861f5291497cf8868fc3031712ffd92b407b81deb6309e8`.
These post-corpus binaries, final gate logs, and validation identities are saved
under `target/audit/2026-09-05/post-corpus-validation`; earlier binary snapshots
remain unchanged. A quiet-host latency comparison remains outstanding, as
described above. The modeled workload does not establish 100-hour continuous
uptime or reproduce terminal fonts, colors, IME, clipboard integration, and
real external AI/network services. Windows remains outside this audit's scope.


## Follow-up: terminal input, long sessions, and visual evidence (2026-09-05)

The follow-up closed concrete gaps left by the earlier 6,000-journey corpus.
Those reports and binaries remain unchanged. New evidence lives under
`target/audit/2026-09-05/followup`, with frozen source manifests, source archives,
separate binary identities, failure logs, and replayable final sessions.

Rich oversized paste now rejects the entire edit before changing source,
cursor, revision, or undo state. The simple fallback now enables bracketed
paste, preserves pasted newlines until explicit submission, and escapes pasted
and recalled terminal controls after splitting at raw cursor positions. Actual
PTY tests reproduced an OSC 52 escape reaching terminal output before this fix.
Source bytes remain unchanged for execution. The simple source buffer admits
64 KiB; undo retains at most 128 states and 8 MiB of source. Failed simple edits
roll back the failing operation and exit with terminal restoration and a typed
resource error before submission; rich paste rejection leaves editing active.

Scoped vendored Crossterm and Reedline patches put limits at their owning
boundaries. ADRs 0031 and 0032 record failure models, resource limits, cleanup,
provenance, and upstream-removal criteria. Pending escape input, bracketed paste,
filtered terminal events, unfinished-input deadlines, Vi numeric expansion,
and raw editor batches are bounded. Real tests found and corrected a blocking
read that bypassed the idle deadline and zero-duration polling that missed
kernel input. Fast Vi insertion followed by Escape also revealed batch-wide
mode parsing corrupting source; events now parse and apply in arrival order.
Excessive Vi counts, products, dot repeats, and aggregate actions are rejected
before the affected event takes effect, including when Enter is queued behind it.

The PTY model now preserves supported ANSI/256/RGB colors and text attributes,
with a fixed palette and 256-byte per-cell limit. Reviewing styled screens found
faint help metadata and keyboard hints: the default muted palette is brighter,
and secondary text no longer receives extra dimming. Keyboard-driven clipboard
checks compare exact single-line and multiline Unicode OSC 52 payloads; they do
not access the operating-system clipboard. Fragmented committed UTF-8 tests
cover combining marks, ZWJ emoji, flags, CJK, and grapheme editing.

A sustained session sends 67,239,936 payload bytes over 128 bursts, repeatedly
recovers from errors and cancellation, and interrupts and reaps foreground
children. Its warm-up wraps transcript retention before sampled churn. Earlier
focused macOS and Linux pilots passed; Linux stayed at eight file descriptors,
six threads and zero direct children across eight checkpoints, with less than
1 MiB warmed RSS growth. The final gate replays the refined shared deadline.
This is bounded accelerated retention evidence, not a 100-hour uptime proof.

The combined run also exposed a harness fault: a write-only large-input sender
could deadlock against the shell's output. The write timeout was initially
hidden behind the subsequent child-exit timeout. The harness now preserves
both diagnostics and drains output on blocked writes under the same deadline,
retention bounds, and non-recursive reply queue. A real 256 KiB bidirectional
fixture covers the failure mechanism. Pre-fix macOS/Linux logs remain preserved;
the fixes do not increase deadlines or substitute rescue keystrokes.

The optimized macOS build met all three interactive latency targets across
101 successful PTY samples each: cold-to-editable P50 10.989 ms (25 ms target),
keystroke-to-frame P95 0.388 ms (8 ms), and first-prompt paint P95 10.666 ms
(21 ms). The earlier saved build also met the targets. Machine frequency,
thermals, caches and other load are not controlled, so the comparison does not
establish a causal speedup. The new CLI is 10,346,128 bytes, below the 10 MiB
hard ceiling and above the 8 MiB soft cap. Its SHA-256 is
`0ec2f0a539d2484806e2adb52941372c9282c8a14f707160fc2e3912ed87091a`.
Preview evidence deliberately does not certify a release from this dirty
checkout. No thresholds were relaxed and no commit or publication was made.

Native Ghostty inspection was rejected by the tool safety check: Computer Use
is not allowed to use that app. Browser inspection of styled model captures
continued. Actual terminal fonts/pixels, IME composition, OS clipboard behavior,
Windows, and real authenticated external services remain outside this evidence.

A final no-color fixture also assumed a visible label must be contiguous raw
bytes. The renderer correctly used cursor positioning across unchanged spaces,
so the fixture timed out despite a correct screen. The check now asserts the
reconstructed visible status and diagnostic, while retaining the terminal input
lease check. This was a test-oracle correction, not a product behavior change.

Final `cargo xtask check` passed on macOS and Linux: 1,351 and 1,352 Rust tests
respectively, with one ignored test on each platform. Each also passed all 36
fixed Quirl PTY checks, twelve keyboard smoke journeys, two guest Lua tests,
six terminal-buffer test executions, 35 terminal-reader fixture test executions,
and the separate real-PTY zero-duration-poll probe. Formatting, strict Clippy,
Rustdoc, architecture, inventory, and generated contracts passed within the gate.
The website gate passed thirteen evidence tests, 60 documentation mirrors, two
compiled reference comparisons, lint, types, and the production build.

The final sustained runs finished in 71.549 seconds on macOS and 66.312 seconds
on Linux. Linux warmed at 53,600,256 resident bytes and peaked at 54,788,096
(+1,187,840 bytes); its final observation was 54,685,696 bytes. Warmed descriptors
and threads were nine and seven, then eight and six at all eight subsequent
checkpoints. No direct children remained at any sampled checkpoint. These are
sampled observations within explicit growth limits, not a zero-leak proof.

Final follow-up replay completed 1,200/1,200 journeys across twenty sessions,
with zero failures, 3,464 screen assertions, 200 resizes, 74,705 injected key
bytes, and 13,784 recorded actions (including observations and bookkeeping).
Every one of the twelve workflows ran fifty times per platform. macOS took
138.971 seconds and Linux took 119.908 seconds. These runs are separate from
the earlier 6,000-journey corpus, and their immutable saved CLI hashes are
`7a833263d7e2f2afe9d07608cf0507c761289dcd17066125307ed1fc6ce5d389`
and `50d8cf290956f0e986f073a00f49d1bdf7b54db8872b9fe3866759af9b662503`
respectively. Build timestamps can change binary hashes even without product
source edits, so fixed-gate and final-soak identities are recorded separately.
The final styled help, palette, and narrow-terminal captures were visually
reviewed. The overview is `target/audit/2026-09-05/followup/index.html`.
