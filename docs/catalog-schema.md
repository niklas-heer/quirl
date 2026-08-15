# Semantic command catalog schema v4

Quirl's command intelligence is one deny-unknown, versioned JSON document.
Completion, generated documentation, the language service, agent context, and
plugin contributions consume the same `CommandSpec` records from
`quirl-catalog`; none of those projections maintains a parallel command list.

## Command records

Schema v4 gives every command a stable `id`, optional declaring `version`,
display `path`, aliases, and an optional stable parent id. `signature`,
`summary`, `details`, and examples describe the interface. `io` records typed
input, output, and streaming behavior. Effects and an integer exit-code map
make execution consequences and failures machine-readable.

The JSON field is named `arguments`. Each argument records names, its
`positional`, `option`, or `flag` kind, value type, required/repeatable state,
an optional static or dynamic completion source, conflicts, documentation,
examples, and fact-level provenance.

Provenance contains source, confidence, trust, optional origin, optional
fingerprint, and optional `generated_at`. Builtins are exact/builtin; validated
plugin declarations are exact/trusted. Fish, Bash, and Zsh declarations remain
attributed declared imports. Help/man extraction remains heuristic. Timestamps
are omitted unless a producer supplies a deterministic source timestamp, so
rebuilding an unchanged catalog stays byte-stable.

## Quality and migration

`Catalog::quality_issues` rejects incomplete exact records: stable identity,
declaring version, command and argument documentation, types, examples, IO,
and exit-code descriptions are mandatory. It also checks parent ids, alias and
argument-name uniqueness, resolvable conflicts, and nonempty static/dynamic
completion sources. Imported records deliberately may
carry `Unknown` IO, no version, no examples, and no exit-code map; Quirl does
not promote incomplete external observations into exact facts.

`Catalog::from_json` accepts v4 and migrates cache schemas 2 and 3. Migration
preserves paths, prose, effects, confidence, origin, and fingerprints, converts
legacy options into arguments, derives stable ids/parents, and marks new fields
unknown or empty. The CLI merges migrated cache records underneath current
compiled builtins, so an old cache cannot remove or overwrite an exact builtin.
Unknown schema versions fail validation and should be rebuilt with
`quirl index build`.

Plugin commands are normalized only after manifest validation. Platform v0.1
requires the plugin name as the command namespace, preventing implicit builtin
shadowing. Normalized records carry the package version, declared typed IO,
arguments, effects, numeric exit codes, source fingerprint, and trusted plugin
provenance.

Builtin signatures are the declared source for positional argument shapes;
their mechanically projected argument provenance is `high`/`declared`, not
`exact`. Builtin CLI byte output is non-streaming with no typed input unless a
command declares a stronger contract (`quirl data`, `ls`, and `quirl watch`).
Static enum values are served directly by completion after either a space or
`--option=`.
