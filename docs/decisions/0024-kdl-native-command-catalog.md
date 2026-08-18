# ADR 0024: Compile curated native command specifications from KDL

- Status: Accepted
- Date: 2026-08-18
- Extends: [ADR 0007](0007-semantic-catalog-v4.md),
  [ADR 0016](0016-runtime-layering-contract.md), and
  [ADR 0021](0021-sqlite-local-command-intelligence.md)
- Decision owners: Quirl maintainers

## Context

Quirl can discover external command facts from shell completions, help text, and
manual pages, but those observations are incomplete and vary by host. A small
curated corpus can provide higher-quality subcommand, flag, argument, platform,
and intent metadata before local discovery completes. That corpus needs a
reviewable human format, deterministic release artifacts, explicit attribution,
and a runtime reader that does not parse authoring syntax on an interactive
path.

Carapace maintains broad command knowledge and can accelerate draft creation.
Its data and generators are nevertheless an external supply-chain and licensing
boundary, not Quirl's schema or authority. Treating Carapace as a runtime
provider, vendored intermediary, or automatically trusted source would make
Quirl's behavior depend on upstream code and facts that have not received
Quirl's semantic, platform, safety, and licensing review.

ADR 0007 makes the versioned `CommandSpec` graph authoritative for composed
runtime command intelligence. ADR 0016 makes `quirl-catalog` a foundation crate
and prohibits it from depending on a higher runtime layer. ADR 0021 assigns the
mutable local intelligence database, discovery state, and embeddings to the CLI
composition root. A native-spec compiler must preserve those decisions while
giving curated external facts their own authoring and artifact contract.

## Decision

### KDL is the sole human-authored native-spec truth

Curated external native command specifications are authored only in the strict
KDL schema documented in [the catalog schema](../catalog-schema.md). Reviewers
change KDL, not generated JSON, normalized SQL rows, or SQLite bytes. The schema
is closed: unknown nodes and properties, typed KDL annotations, unexpected
positional values, duplicate properties, duplicate identifiers, and invalid
field combinations fail validation.

This decision applies to metadata for external native executables. It does not
move Quirl's own builtin command contract out of `Catalog::builtin()`, replace
`HOST_API`, or make KDL an execution manifest. Native specs describe completion
and discovery facts; they grant no process, filesystem, environment, or network
authority.

### SQLite is the deterministic runtime format

The native compiler parses and validates the complete KDL document, constructs
a canonical typed tree, and compiles it in memory into SQLite application
`QCNC`, schema version 1. One transaction writes both an exact typed snapshot
and normalized provenance, command, alias, platform, intent, flag, argument,
and semantic-document projections. Canonical ordering, explicit identifiers,
fixed schema pragmas, and the absence of wall-clock data make equal typed input
produce byte-identical database images.

Runtime readers accept only a bounded, regular, unlinked, permission-safe
database with the expected identity and an `ok` SQLite integrity check. They
deserialize and validate the exact snapshot, deterministically recompile it,
and require every byte to match the supplied image. A normalized row therefore
cannot diverge from the authoritative snapshot without invalidating the entire
database. Readers use query-only mode with trusted schemas disabled and
explicit SQLite limits.

SQLite here is an immutable compiled resource format owned by the foundation
catalog crate. This narrows ADR 0021's statement that foundation crates are
unaware of persistence: `quirl-catalog` may construct, validate, query, and
atomically publish this static native-catalog format, but it does not own the
user cache path, discovery scheduling, refresh state, embeddings, network
model installation, or runtime composition policy. ADR 0021's CLI-owned local
intelligence database remains a separate database identity and lifecycle.

### Carapace proposes drafts and never enters runtime

Carapace may be used only by an explicit build-time import operation at one
pinned immutable upstream revision. Its repository, generators, and generated
facts are untrusted inputs subject to source-byte, record, depth, duration, and
output bounds. Import produces a draft KDL file in a review workspace; it never
writes directly to the curated source set or release database.

There is no runtime Carapace dependency, subprocess, plugin, data lookup,
fallback, schema intermediary, or compatibility promise. The compiled database
does not contain or execute Carapace code. Quirl's KDL model is deliberately
smaller; unsupported upstream concepts must be omitted or represented only
after the Quirl schema is extended through review. The importer must not invent
summaries, descriptions, intent phrases, platforms, actions, or provenance to
make a draft pass.

### Trust boundaries are explicit

The workflow crosses five independently validated boundaries:

- **Upstream to draft:** the pinned Carapace checkout and every upstream fact
  are untrusted build input. Import is bounded and cannot write curated source.
- **Draft to curated KDL:** a maintainer performs semantic, platform,
  provenance, and license review. No automated confidence score crosses this
  boundary.
- **KDL to compiled bytes:** the closed parser, typed validation, configured
  bounds, and deterministic compiler reject partial or ambiguous input.
- **Filesystem to runtime reader:** file type, size, link count, Unix write
  permissions, path/handle identity, SQLite identity/integrity, exact snapshot,
  and normalized projection bytes are revalidated.
- **Catalog fact to completion:** the composition root applies platform and
  provenance policy. A native completion action is data, not authority; its
  implementation remains separately bounded and cannot execute merely because
  the catalog named it.

Passing one boundary never exempts the next. In particular, a valid SQLite
checksum proves artifact identity, not licensing, upstream correctness, host
availability, or permission to execute a suggested command.

### Provenance and licensing are admission requirements

Every KDL catalog carries nonempty `author`, `license`, immutable `revision`,
and absolute HTTP(S) `source` fields. These fields attribute the facts in that
catalog and are retained in both the exact snapshot and normalized database.
They do not by themselves prove that redistribution is lawful.

Before a draft becomes curated, a reviewer must:

1. identify the upstream project and every material source used by the draft;
2. verify that the recorded revision is immutable and that the recorded
   license permits Quirl's intended use and redistribution;
3. preserve notices or attribution outside the schema when the license requires
   more than the four provenance fields can express;
4. compare the draft with authoritative upstream command documentation or the
   named executable version; and
5. record or resolve any generated, ambiguous, platform-dependent, or
   unsupported facts during normal code review.

If provenance or licensing is unclear, the command remains a draft or is
removed. Similarity to an upstream command is not permission to copy its prose.
Quirl-authored summaries and descriptions must still be checked for semantic
accuracy and attribution obligations.

### Curated updates are reviewable transactions

Draft generation and curated maintenance are separate states:

1. Update the importer pin to an immutable Carapace revision in the tooling's
   reviewed pin source. Do not use a floating branch, tag resolution at runtime,
   or an unrecorded local checkout.
2. Fetch or provide that exact source through the build-time tooling and verify
   its expected identity before parsing it.
3. Import selected commands into a separate draft area under all importer
   bounds. A partial import is a failure and cannot modify curated KDL.
4. Format the draft canonically, then review the KDL diff for schema meaning,
   command version, flag/argument shape, aliases, platform scope, completion
   actions, intent wording, provenance, and license obligations.
5. Move only approved KDL changes into the curated source set. Run formatting,
   strict checking, deterministic compilation, reader validation, and tests.
6. Review the compiled database checksum change alongside the KDL change. An
   unexplained checksum change with unchanged canonical input is a compiler
   defect, not a generated-file refresh.

The integration tooling exposes distinct `cargo xtask catalog
import-carapace`, `cargo xtask catalog fmt`, `cargo xtask catalog check`, and
`cargo xtask catalog build` operations. Import requires an explicit local
checkout and exact 40-character pinned revision; format supports the
non-mutating `--check` mode. The concrete paths and contributor procedure live
in [`../catalog-schema.md`](../catalog-schema.md).

### Publication and runtime fallback fail closed

Local publication compiles completely before staging. A uniquely named staging
file is created without replacement, uses mode `0600` on Unix, is content-synced,
reread, and compared with the encoded bytes. If a previous destination exists,
it must itself be admitted and must remain byte-identical until rename; a newly
appearing destination also aborts publication. One atomic rename installs the
complete image. An RAII owner removes the staging file on validation, write,
sync, contention, cancellation, and unwind failures. A post-rename parent sync
is durability hardening; a failure there does not destroy the newly visible
valid image.

CI compiles canonical KDL twice, proves byte equality, opens the result through
the hardened reader, and publishes the SQLite image plus a cryptographic
checksum associated with the exact source revision. Release packaging must not
silently rebuild a different image or publish an unreviewed draft. The release
record names the source revision, native database schema identity, artifact
name, byte length, and checksum.

At runtime, the composition root selects only a database whose compiled
platform facts include the host platform. A missing, incompatible, corrupt,
oversized, unsafe, or semantically mismatched native database is unavailable
knowledge, never permission to relax validation. Quirl continues with its
existing builtin metadata and bounded local discovery/intelligence cache; it
does not invoke Carapace or parse a draft as a fallback. Native catalog failure
must not prevent terminal startup, command execution, cancellation, or clean
shutdown.

## Failure model and invariants

- KDL and upstream imports are untrusted inputs. Parsing and traversal are
  bounded before recursive construction, and every configured limit must be in
  `1..=hard_max`.
- Diagnostics are inert foundation values with a kind, source identity,
  optional UTF-8 byte span, actionable help, and bounded limit/observation
  context. Effect-owning consumers map them to `ShellError` without dropping
  those facts.
- Canonical formatting may change layout only. It must be idempotent and must
  not infer, delete, promote, or reorder meaning-bearing positional arguments.
- Build determinism excludes timestamps, host paths, locale, filesystem order,
  random identifiers, and network state from the database image.
- Platform selection is metadata filtering. `any` is exclusive, child support
  cannot exceed parent support, and an empty intersection is invalid.
- Completion actions are closed declarations. Compilation and lookup never run
  them; a runtime provider applies its own bounds and capability policy.
- The database is an all-or-nothing generation. Snapshot, normalized rows, and
  semantic documents cannot be mixed across builds.
- Query bytes, semantic documents scanned, and returned rows are bounded before
  caller-controlled work can grow without limit.
- A failed build or publication preserves the previous admitted database and
  removes the current process's staging file. A crash may leave one uniquely
  named unpublished staging file, never a partially installed destination.
- Curated KDL is never overwritten by import. Human semantic and licensing
  review is the only transition from draft to curated.

## Consequences

- External native command knowledge gains a readable, strict, attributable
  source and a compact deterministic runtime representation.
- The repository carries two clearly separated catalog database roles: this
  immutable native artifact and ADR 0021's mutable CLI intelligence cache.
- `quirl-catalog` gains SQLite-format responsibility without gaining user-state,
  network, worker, or composition ownership and without changing the Quirl
  crate dependency graph.
- Carapace breadth can reduce draft authoring cost without becoming a trusted
  dependency or user-visible runtime requirement.
- Updates require pin, provenance, license, semantic, deterministic-build, and
  artifact review; broad automatic imports are intentionally rejected.
- The initial KDL schema does not encode typed IO, effects, exit codes, dynamic
  providers, or execution policy. Those remain in the composed `CommandSpec`
  contract or require a future versioned native-schema decision.
