# ADR 0007: Make the semantic catalog the authoritative command contract

- Status: Accepted
- Date: 2026-08-15
- Extends: [ADR 0002](0002-crate-layering.md), [ADR 0005](0005-plugin-platform-layer.md)

## Context

The earlier catalog schema represented paths, prose, examples, effects, and
completion options, but it could not express the complete command-intelligence
contract in language-design §4. Downstream consumers were forced either to
lose types and provenance or invent their own metadata. Indexed v2/v3 caches
also need an explicit transition instead of being silently accepted as current
knowledge.

## Decision

Catalog schema v4 adds stable command identity/version/aliases/parent relation,
typed argument records, typed streaming IO, exit-code descriptions, and richer
trust/fingerprint/time provenance. `quirl-catalog` remains a foundation crate
and owns normalization, deterministic merge, migration, completion projections,
explanations, and the exact-metadata quality audit.

The graph remains flat on disk with a stable parent id rather than recursively
nesting subcommands. This keeps deterministic merging and cache lookup simple
while preserving the semantic relation. Consumers project from this record:
the agent contract serializes it, LSP and completion display it, and
`quirl-plugin` normalizes validated package commands into it. No dependency
arrow is added or inverted.

Compiled builtins must pass the exact-fact quality test. Legacy and external
records retain their original confidence and receive explicit unknown/empty
defaults for facts their source did not establish. Optional `generated_at` is
not populated from wall-clock time. Cache v2/v3 migration occurs before current
builtins are merged at the CLI composition boundary.

Mechanically projected positional arguments are attributable declarations,
not exact facts. Exact IO is declared per builtin family; the catalog does not
use a universal streaming bytes-to-bytes placeholder. Parent, alias, conflict,
and completion-source relations are validated before exact metadata passes.

## Consequences

- Help, completion, LSP, agent discovery, and plugins share typed command facts
  and fact-level provenance.
- Exact declarations fail tests when required metadata is absent.
- Imported declarations remain useful without overstating their completeness.
- Schema v4 is a breaking serialized change; only cache v2/v3 receive an
  explicit compatibility migration.
- Default values, deprecation, platform constraints, and executable dynamic
  providers remain future schema work rather than inferred facts.
