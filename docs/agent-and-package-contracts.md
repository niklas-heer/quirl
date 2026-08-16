# Agent and package contracts

Quirl 0.1 exposes installed command and Lua host knowledge through explicit,
versioned formats. Agents do not need to scrape terminal help or assume that a
capability exists from training data.

## Agent surfaces

```sh
quirl agent catalog --format json
quirl agent context "deploy the billing service" \
  --format markdown --token-budget 6000
quirl agent manifest --format json
quirl agent validate context.json --kind context --format json
```

Every JSON document has a `document_type`, integer `schema_version`, and named
`fnv1a64:` schema/content hashes. FNV-1a is used only as a stable freshness
checksum; it is not an authenticity or security claim. Schema hashes cover
deterministic structural descriptors for every nested field, enum, default,
and deny-unknown boundary in the contract. They are schema fingerprints, not
published JSON Schema documents. Catalog and manifest exports contain only
commands and capabilities present in the loaded catalog and generated Lua
`HOST_API` of the running binary.

Context ranking is deterministic: task terms match command paths, summaries,
details/options/examples, and Lua host paths/capabilities with fixed weights;
ties use stable names. The budget estimator is the Unicode-scalar length of the
canonical compact JSON payload divided by four and rounded up. The envelope
records the estimator, requested budget, estimated use, source hashes, and
whether facts were truncated. A generated payload never exceeds its declared
budget. If the highest-ranked complete command record does not fit, context
keeps a deterministic compact projection with stable identity, signature,
summary, typed IO, effects, exits, and provenance, omits its argument list and
all but one example, and sets `truncated = true`; the catalog hash still points
to the authoritative complete record.

Validation rejects unknown fields, incompatible schema versions, hash drift,
nondeterministic ordering, and context budget drift. Catalog hashes are
recomputed from their serialized command and host content. Context and
manifest documents are subsets/projections, so `quirl agent validate` compares
their catalog and host-API hashes with trusted anchors rebuilt from the running
binary; the lower-level unanchored validator rejects those document kinds.
Manifest content and nested capability hashes are also recomputed. Validation
parses data only and does not grant authority or execute commands/Lua.

## Package manifest

Project packages use `plugin.toml` by default:

```toml
schema_version = 1

[package]
name = "deploy-tools"
version = "0.1.0"
entry = "plugin.lua"
quirl = ">=0.1, <0.2"
summary = "Deploy services with explicit safeguards"
license = "MIT"

[capabilities]
request = ["process.spawn"]

[contributes]
commands = ["deploy-tools deploy"]
panels = []
indexers = []

[[public_commands]]
path = "deploy-tools deploy"
signature = "deploy-tools deploy <environment>"
summary = "Deploy a service"
details = "Deploys one service after validation."
input_type = "Nothing"
output_type = "Result<Deployment>"
examples = ["deploy-tools deploy staging"]
effects = ["spawn_process"]
error_codes = { "0" = "deployed", "1" = "deployment failed" }

[[public_commands.arguments]]
names = ["environment"]
kind = "positional"
value_type = "Environment"
required = true
documentation = "Target deployment environment"
```

All tables deny unknown fields. Package entry paths must be relative `.lua`
paths without parent traversal. Capability requests must be sorted, unique, and
present in `quirl agent manifest`. Contributed command names must match exactly
one `public_commands` record. Public command metadata must include a summary,
detailed contract, typed input/output and arguments, examples, explicit
non-empty effects, and documented numeric error codes. Panel and indexer
contributions use the separately versioned Phase 3 extension contracts and
their explicit capability gates; they are not inferred from command metadata.

```sh
quirl package manifest --format json
quirl package build --format json
quirl package publish --dry-run --format json
```

`package build` reads and hashes the manifest/entry but never evaluates the
entry. The CLI parses and lints the Lua entry under the restricted Lua layer,
then conservatively recognizes direct generated `HOST_API` calls outside
ordinary comments and string literals. Statically observed capabilities and
effects must be declared; requested capabilities that are not directly seen
produce a warning because indirect use cannot be proved from source text.
Package file lists contain normalized package-relative paths, so invoking the
build with an absolute or relative manifest path produces the same build
record. The record includes the resolved Quirl version and installed host-API
hash.
`package publish` is deliberately limited to `--dry-run` in v0.1: it emits a
deterministic plan and performs no network operation.
