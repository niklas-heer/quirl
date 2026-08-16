# Documentation system

Quirl has two deliberately connected documentation layers. They serve different
consumers, and each has one authoritative source.

## Truth and evidence hierarchy

When sources disagree, use this order rather than copying the most attractive
claim into another document:

1. The engineering contract and accepted ADRs define enduring ownership,
   support, safety, and release-policy decisions. Proposed ADRs remain proposals
   and do not describe delivered behavior.
2. Runtime canonical sources define executable interfaces: `Catalog::builtin()`
   for commands and `HOST_API` for Lua. Their generated SDK/catalog outputs are
   projections, never independent contracts.
3. Integrated implementation and behavioral tests establish what the current
   source tree actually does. User-facing prose must not outrun them.
4. README, changelog, guides, and website entry pages are user-facing
   projections of the first three levels. Website mirrors are generated from
   repository sources and must not be edited by hand.
5. Benchmarks, recordings, and release reports are exact-artifact evidence.
   Their result applies only to the recorded revision, digest, environment, and
   method; it never transfers automatically to a later candidate.

`docs/language-design.md` deliberately contains two kinds of material: its
current-implementation contract is explicitly labeled, while its remaining
sections preserve labeled long-term direction and illustrative designs. Neither
historical research nor future direction can be used as a current release claim.

## Rust API documentation

Public Rust APIs use ordinary Rust doc comments: `//!` for crates and modules,
and `///` for public items. A useful contract states what the item represents or
does and records relevant invariants, units, limits, side effects, errors, and
security assumptions. It does not just expand the item's name into a sentence.

The workspace denies Rust's `missing_docs` lint. It also denies broken intra-doc
links, bare URLs, and malformed Rust code blocks. The existing canonical gate
runs the documentation build automatically:

```console
cargo xtask check
```

For a faster documentation-only iteration, run:

```console
cargo xtask docs
```

That command builds every workspace crate with dependencies excluded and all
Rustdoc warnings denied. Rust examples in doc comments are exercised by the
workspace tests as doctests. Generated HTML lives under `target/doc` and is not
checked in.

## Command and extension documentation

Rust has no stable runtime reflection for `///` comments, so Quirl does not
scrape source files or pretend Rustdoc can supply runtime command metadata.
Instead, the versioned semantic catalog is the command-level equivalent of a
docstring:

| Interface | Authoritative source | Consumers |
| --- | --- | --- |
| Builtin commands and arguments | `Catalog::builtin()` in `quirl-catalog` | interactive help, completion, `quirl catalog`, `quirl describe`, `quirl doc`, LSP, MCP catalog output, and `quirl agent` |
| Lua host API | `HOST_API` in `quirl-lua` | LuaLS stubs, JSON, Markdown, LSP, and the AI capability catalog |
| Imported or plugin commands | validated `CommandSpec` records with provenance | the same catalog projections after composition |

Exact catalog records must include stable identity, version, signature,
summary, details, typed arguments and their documentation, examples, I/O,
effects, and exit-code descriptions. `Catalog::quality_issues()` and its tests
reject incomplete records. The CLI contract test independently checks that
every visible Clap leaf and argument agrees structurally with the catalog, so a
new command cannot silently disappear from generated documentation or AI
discovery.

Clap doc comments remain short parser-facing navigation text. Full command
contracts belong only in the catalog. Changing a catalog record automatically
updates every catalog consumer; changing `HOST_API` requires
`cargo xtask sdk` to refresh the checked-in Lua stub.

## Website mirrors and release validation

The website's canonical Markdown mirrors are produced only by
`website/scripts/sync-docs.mjs`; compiled catalog and Lua reference pages are
produced only by `website/scripts/sync-generated-reference.mjs`. Never repair a
generated MDX page directly. Run the appropriate sync command, review the
canonical source and generated diff, then run both syncs a second time to prove
byte idempotence.

Release evidence status has an additional semantic source of truth: the strict
`quirl-release-evidence:v1` header in
`docs/benchmarks/release-v1.0.md`. The shared website parser accepts only the
closed `historical`/`current` states, exact commit and digest shapes, a UTC
measurement timestamp, and a bounded platform scope. `sync-docs.mjs` derives
the marked README, changelog, language-design, checklist, audit, and website
status regions from that header. The release-attribution check also verifies
named Git objects and, for current evidence, the evidence-only direct child of
the measured candidate. A byte-fresh mirror cannot therefore override or
contradict the canonical evidence state.

An evidence commit cannot embed its own Git object ID. A new `current` record
may therefore use `evidence-documentation-commit: none`; normal generation may
project that transitional form before commit, but it does not attest it. The
post-commit check resolves the checked-out `HEAD` as the evidence commit and
proves its parent and diff. Once named, an evidence commit must use its full
lowercase ID and exist locally; any later path through `HEAD` must remain
evidence-only. This keeps the next evidence transition possible without
weakening the relationship check or inventing a self-hash.

With dependencies installed by the lock-preserving `npm ci --prefix website`,
`npm --prefix website run check:generated` is the deterministic, non-mutating
freshness and semantic-attribution check. `npm --prefix website run check` adds
lint, Next route type checking, and the production build. `cargo xtask
website-check` exposes that website gate explicitly, and `cargo xtask
release-gate` runs it as part of the release workflow; `cargo xtask check`
intentionally remains Rust-focused and does not require Node.

## Adding or changing an interface

1. Add `///` documentation with the public Rust item.
2. If it is a command or argument, update `Catalog::builtin()` in the same
   change. If it is a Lua host capability, update `HOST_API` and regenerate the
   SDK.
3. Add behavioral tests for the contract, including limits and errors where
   relevant.
4. Run `cargo xtask check`. No separate documentation checklist or personal
   skill is required; the repository gate remembers the rules.
