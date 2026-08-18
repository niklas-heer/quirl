# ADR 0021: SQLite local command intelligence

- Status: Accepted
- Date: 2026-08-17
- Extends: [ADR 0016](0016-runtime-layering-contract.md)

## Context

Declarative Fish, Bash, and Zsh completion sources and imported help/man text
produce command facts that must support completion, help, agent context, option
intent lookup, and natural-language discovery. A JSON catalog plus a separate
discovery sidecar gives readers an exact snapshot, but it cannot efficiently
query normalized options or retain a local semantic index. Keeping independent
stores for completion and AI would allow facts and provenance to drift.

Quirl also needs semantic inference without Python, ONNX, a GPU, a network
service, or an unrestricted model runtime. Model input and output cross an
untrusted filesystem boundary, and model work cannot enter the per-keystroke
catalog construction path.

## Decision

`quirl-cli`, the product composition root, owns a bundled SQLite database via
`rusqlite`. Foundation crates remain unaware of persistence. A database stores
one exact serialized `Catalog` snapshot plus normalized commands, aliases,
arguments, values, conflicts, examples, effects, exit codes, provenance,
semantic documents, embeddings, and discovery state. The exact snapshot is the
reader source of truth; normalized rows are query projections written in the
same transaction.

Database construction occurs in memory. The completed image must fit the
128 MiB bound before the existing hardened atomic replacement installs it.
Readers reject non-regular, linked, over-permissive, oversized, wrong
application-id, wrong-version, malformed, or catalog-incompatible files.
Discovery retains its existing source, record, byte, deadline, cancellation,
and refresh bounds. Legacy JSON catalog schemas remain read-only migration
inputs; all writes use SQLite. If bounded first-run discovery fails before any
valid database exists, interactive initialization atomically publishes a
builtin-only SQLite image. A valid prior database is never replaced by this
fallback, and a later successful discovery generation replaces the fallback
through the normal publication path.

Catalog admission also starts one session-owned discovery worker immediately;
it does not wait for `read_line` to return. Its full scan has an explicit
30-second background deadline and a 60-second periodic interval, while accepted
input only requests an additional coalesced scan. Discovery activity is cached
for the fixed bottom status row, including the bounded primary error and first
context field when a generation fails. The source-file ceiling is 4,096 within
the separate 8,192 directory-entry, 1 MiB retained-path, 16 MiB source-byte,
and 16 MiB canonical-catalog bounds; this admits contemporary macOS
installations with more than 2,000 PATH commands plus declarative completions.
Missing or permission-denied individual
PATH candidates are skipped because the shell could not execute them, while
other filesystem failures remain visible diagnostics. A completed database
publication sets the catalog-changed bit and requests embedding for that new
generation before main adopts the catalog at its next safe prompt boundary.

That same worker owns local shell-provider discovery. Initial discovery probes
only the top-level command name of a bounded subset of PATH executables; it
does not recursively walk possible command paths. An editor-side observer may
enqueue an exact unknown path for a narrow follow-up probe. Enqueueing performs
no filesystem or process work, retains at most 64 paths of at most eight
256-byte segments, and coalesces duplicates. The worker admits at most two
Fish/Zsh requests concurrently. Each request receives one absolute sub-deadline
within the generation's 30-second deadline, the process boundary's shared
cancellation flag, at most 256 candidates, 256 frames, 256 KiB of combined
output, 16 admitted roots, and no retries. Queue overflow is a bounded
diagnostic and never delays editing.

Provider children receive an empty environment plus an explicit PATH assembled
from admitted PATH roots; the process adapter fixes the locale. Fish starts
with `--no-config`, Zsh starts with `-d -f` and disables both user and global rc
loading, and both see only admitted completion roots and scripts. Bash remains
a typed later-provider contract in the SQLite overlay and composition tiers; it
is not executed by this generation. Carapace is neither selected nor linked.
Provider candidates are normalized before persistence: leading-dash candidates
become flags, non-dash candidates from a command-path probe become one-segment
child commands, controls and path separators are rejected, duplicates are
coalesced deterministically, and useful provider descriptions remain attached
to their individual provenance.

Positive and negative observations are keyed by the native-catalog identity,
the exact PATH executable fingerprint, the Fish/Zsh shell plus admitted-source
fingerprint, the controlled environment fingerprint, and cwd class. A warm
positive result or a negative hit still inside its retry window prevents a new
process. Executable, provider-source, environment, or native-catalog changes
therefore invalidate without deleting unrelated rows; negative expiry uses the
caller's fixed-width timestamp. SQLite images preserve valid overlay rows when
the central catalog generation is rebuilt, and every overlay-only publication
uses the same catalog lock and hardened atomic replacement path.

The same discovery generation scans admitted macOS, Homebrew, local, and
configured `man1` roots without invoking `man`. Only pages whose normalized
sectionless name matches a discovered PATH command are candidates; at most 512
plain pages are retained, each remains under the 1 MiB documentation bound, and
the existing entry, aggregate-source, path, record, diagnostic, and deadline
bounds still apply. Section and compression suffixes such as `cp.1` and
`cp.1.gz` normalize to `cp`; configured `.man`, `.man.txt`, and `.txt` sources
retain their existing name normalization. Symlink aliases resolve to a regular
page and are deduplicated by target. The importer recognizes a bounded,
non-interpreting
subset of BSD mdoc (`.Nm`, `.Nd`, and `.It Fl`) to retain descriptions and
option prose. Compressed-only pages are reported and skipped: adding a gzip
implementation or dependency is not justified while supported macOS system
pages are plain files. Malformed, oversized, inaccessible, unresolved, and
non-regular individual pages become bounded discovery diagnostics rather than
aborting the catalog generation.

Semantic inference uses `model2vec-rs` with only its `local-only` and `onig`
features and the `minishlab/potion-base-8M` model. After the interactive catalog
is admitted, the composition root starts one session-owned worker. If the
default model is absent or corrupt, that worker downloads the three files from
revision `bf8b056651a2c21b8d2565580b8569da283cab23` over bounded rustls HTTPS,
streams them into a private staging directory, verifies exact byte counts and
SHA-256 digests, and publishes the directory with one rename and parent sync.
An invalid auto-owned destination is quarantined before a clean install;
explicit `QUIRL_MODEL_PATH` destinations are never replaced automatically.

The model files total 30,920,628 bytes. Redirects, connect/global/body waits,
channel retention, staging-name attempts, directory depth, file bytes, and
activity text are bounded. The network reader publishes at most two 64 KiB
chunks ahead, so cancellation disconnects the installer promptly even if an
underlying HTTPS call remains blocked until its own timeout. Encoding panics
from the dependency are caught at the boundary. Document count, document
bytes, query bytes, result count, token length, batch size, vector dimensions,
serialized bytes, and finite float values are validated.

The worker builds embeddings automatically after initial catalog admission and
again only after a refreshed database has been published. It skips work when
every bounded semantic document already has a matching model id and source
fingerprint. Automatic encoding checks cancellation between 32-document
batches and publishes only if both the request generation and source database
remain current. If database bytes change without a matching request while an
embedding build is in flight, the worker retries against the newest bytes
instead of sleeping until user input. `quirl ai index` remains an explicit diagnostic/refresh tool;
it is not required for normal setup. `quirl ai search`, `quirl ai related`, and
interactive AI mode read the same database. Missing model files or embeddings
select deterministic lexical ranking. AI mode inserts a selected suggestion
into normal mode for review, while AI subcommands only display suggestions;
neither path executes one directly.

## Failure model and invariants

- Local completion code is untrusted local code. It may hang, fork descendants,
  ignore termination, emit malformed or excessive frames, return excessive or
  misleading candidates, disappear after admission, or fail after partial
  output. These are operating failures or bounded diagnostics, never panics.
- The local-discovery resource sketch is constant in process concurrency (two
  process-owned containment groups) and worker ownership (the existing catalog
  worker plus at most two scoped provider calls). Retained memory is bounded by
  the 64-path queue, per-request input/output/candidate ceilings, the existing
  4,096-row/1,024-negative-hit/4-MiB overlay ceilings, and the 128-MiB database
  image ceiling. Initial work is top-level only; nested work is exact and
  demand-driven, so the worker never performs exhaustive recursive command-path
  crawling.
- Queue insertion and cache polling may occur for successive editor revisions,
  but they only replace/coalesce bounded passive path values. Shell startup,
  completion-source admission, SQLite mutation, and atomic publication occur
  off render and per-keystroke threads. Results become visible only through the
  existing complete catalog-generation swap between editor turns.
- One provider attempt has one deadline and no retry. The catalog cancellation
  flag is shared with every request. Shutdown records cancellation before join;
  the process boundary kills the owned process group, drains bounded pipes,
  reaps the direct child, and returns before its scoped call and catalog worker
  can join. No CLI code spawns an uncontained completion child.
- Only Fish and Zsh have executable adapters. Bash is represented in typed
  persistence/composition metadata for a later adapter and cannot be reported
  as a successful runtime provider. Carapace is not a runtime, build, or test
  dependency.

- Catalog publication and model installation use separate, dedicated sibling
  lock files whose names do not change when the SQLite file or model directory
  is atomically replaced. Locking the replaceable data path is insufficient:
  after rename, a future opener would name a different file identity and could
  enter the same critical section.
- A lock file is admitted as an unlinked regular file in an already admitted
  private directory. Symlinks, hard links, special files, unsafe Unix write
  permissions, and open or metadata failures are operating errors. Admission
  validates both the pathname and opened handle, and validates again after the
  lock is acquired. Unix uses no-follow opening and device/inode comparison;
  Windows uses the portable `std::fs` checks inside the private namespace.
- Lock acquisition only uses nonblocking `std::fs::File` attempts. Explicit
  `quirl index build` and `quirl ai index` operations retry for a fixed count
  with a fixed delay and then return `ResourceLimit`; interactive discovery,
  download, and embedding workers make one attempt and defer immediately so a
  second shell never adds lock latency to prompt or shutdown work.
- Acquiring a lock after any possible contention grants permission to inspect,
  not permission to act on an earlier observation. The winner rereads and
  validates the catalog/model state under the lock. Automatic work skips a
  now-current generation, while explicit work applies its requested rebuild to
  the newly admitted state.
- One process-local registry and the OS file lock cover same-process threads and
  separately opened processes respectively. The critical section includes
  SQLite encoding/embedding and publication, or model validation, quarantine,
  download, verification, and installation, so at most one writer or downloader
  performs that bounded work for a target at a time.
- The lock is owned by an RAII guard. Every return, cancellation error, unwind,
  and ordinary drop unlocks and closes it; process exit or crash closes the
  descriptor and the operating system releases the lock. Lock files are kept,
  not deleted, because unlinking would split future contenders across file
  identities. Unix and Windows use the same standard-library API and never rely
  on advisory locking of the replaceable data file.
- A failed discovery or embedding build cannot replace the last complete
  database; transaction rollback and atomic replacement preserve it. When no
  complete database exists, failed discovery publishes an indexable builtin
  SQLite fallback so automatic embedding construction still has a source.
- Catalog and normalized SQL rows describe one generation because they commit
  together.
- Discovery state cannot claim freshness for different catalog bytes because it
  records and revalidates the canonical catalog fingerprint.
- Stale embeddings are ignored because each row carries the source document
  fingerprint and model id.
- Download, hashing, model loading, and SQLite work never run on render or
  per-keystroke threads. The UI polls only generation-numbered immutable cache
  snapshots and rejects stale generations.
- Partial, short, oversized, or hash-mismatched assets remain staging files and
  never become model input. Failure preserves lexical search and the last
  complete database. Normal errors and cancellation remove the current
  process's authenticated staging directory through its RAII owner. A crash can
  leave a uniquely named, bounded staging directory, but never publishes it;
  later instances use a new bounded name and can progress. An invalid
  auto-owned model is quarantined only while holding the model lock, and a
  valid installed model is never quarantined or overwritten.
- Initial indexing starts only after catalog admission. Refresh indexing is
  requested only after refreshed database publication; database publication is
  serialized and an embedding result revalidates its exact source bytes. Full
  discovery and its follow-up embedding run while an untouched shell is idle.
- Background work never owns terminal state. Cancellation is visible between
  download chunks and embedding batches; terminal restoration precedes the
  bounded worker join.
- No SQLite connection or model is shared across threads. Each bounded
  operation owns its connection and model lifetime.
- SQLite catalog publication and embedding publication share the catalog lock.
  Embedding writers reread the exact source bytes under that lock and recheck
  cancellation plus request generation before atomic replacement, preventing a
  stale embedding image from overwriting a newer catalog generation.
- No AI output crosses into the execution planner without a separate explicit
  user action.

## Consequences

- The CLI binary gains bundled SQLite and local Model2Vec dependencies and
  corresponding compile-time/binary-size cost.
- A default database can answer structured command and option queries without
  reparsing completion sources or man text.
- Embedding generation and model installation are automatic background
  control-plane work, never prompt latency.
- Semantic ranking degrades predictably to lexical ranking during setup and
  after any failed download or rebuild.

## Later reconciliation

[ADR 0024](0024-kdl-native-command-catalog.md) defines a separate immutable
`QCNC` SQLite build artifact for curated external native specifications. This
ADR continues to govern the CLI-owned mutable intelligence cache, discovery
state, embeddings, workers, and model lifecycle. The native artifact has its
own database identity, exact snapshot, bounds, and deterministic compiler; it
is an admitted input to composition, not a replacement for this cache and not a
second owner of its runtime state.
