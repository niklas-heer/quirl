//! One semantic catalog powers Quirl's completion, help, validation, docs, and AI API.

#![cfg_attr(
    test,
    allow(
        dead_code_pub_in_binary,
        reason = "the libtest harness is an executable, but these public items remain library API"
    )
)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Schema version emitted by [`Catalog`] serialization and by built-in catalogs.
pub const CATALOG_SCHEMA_VERSION: u32 = 4;
/// Oldest catalog schema version that [`Catalog::from_json`] can migrate.
pub const CATALOG_OLDEST_READABLE_VERSION: u32 = 2;
/// Wire version for asynchronous completion request and response envelopes.
pub const COMPLETION_PROTOCOL_VERSION: u32 = 1;
/// Maximum UTF-8 byte length accepted for one asynchronous completion query.
pub const MAX_COMPLETION_QUERY_BYTES: usize = 4 * 1024;
/// Maximum number of completion candidates one request may ask a worker to return.
pub const MAX_COMPLETION_RESULTS: usize = 1_000;
/// Maximum completion-worker deadline, in milliseconds.
pub const MAX_COMPLETION_DEADLINE_MS: u64 = 250;
/// Canonical structural descriptor whose fingerprint identifies the catalog schema.
///
/// This is a protocol description, not a JSON Schema document. Readers use it to
/// bind caches and agent contracts to the exact field and migration policy.
pub const CATALOG_SCHEMA_DESCRIPTOR: &str = "quirl.catalog@4{Catalog{deny_unknown;schema_version:4;commands:array<CommandSpec>};CommandSpec{deny_unknown;id:string;version:null|string;path:string;aliases:array<string>;parent:null|string;signature:string;summary:string;details:string;arguments:array<ArgumentSpec>;examples:array<string>;io:IoContract;effects:array<Effect>;exit_codes:map<i32,string>;provenance:ProvenanceInfo};ArgumentSpec{deny_unknown;names:array<string>;kind:positional|option|flag;value_type:string;required:bool;repeatable:bool;values:null|CompletionSource;conflicts:array<string>;documentation:string;examples:array<string>;provenance:ProvenanceInfo};CompletionSource:tag(kind)[static{values:array<string>}|dynamic{provider:string}];IoContract{deny_unknown;input:string;output:string;streaming:bool};Effect:read_filesystem|write_filesystem|spawn_process|change_directory;ProvenanceInfo{deny_unknown;source:builtin|external|lua|plugin|fish|bash|zsh|help|man;confidence:low|medium|high|exact;trust:builtin|trusted|declared|imported|heuristic;origin:null|string;fingerprint:null|string;generated_at:null|string};migration:read-v2-v3-to-v4}";
/// Canonical structural descriptor for completion items and worker envelopes.
///
/// The descriptor includes query, result, and deadline bounds so a fingerprint
/// change also captures changes to completion resource policy.
pub const COMPLETION_SCHEMA_DESCRIPTOR: &str = "quirl.completion@1{Completion{deny_unknown;value:string;display:string;summary:string;detail:string;replace_start:usize;replace_end:usize;match_indices:array<usize>};CompletionRequest{deny_unknown;protocol_version:u32;request_id:u64(strictly-increasing);line:utf8<=4096-bytes;cursor:usize(char-boundary);limit:usize<=1000;deadline_ms:1..250};CompletionCancellation{deny_unknown;protocol_version:u32;request_id:u64};CompletionResponse{deny_unknown;protocol_version:u32;request_id:u64;outcome:CompletionOutcome};CompletionOutcome:tag(status);content(data)[ready{items:array<Completion>}|cancelled{}|deadline_exceeded{}];policy:frozen-major-v1;ordering:score-desc-then-display-value;catalog_source:quirl.catalog@4;static_values:CompletionSource.static;dynamic_values:provider-identity-only;worker:newer-request-or-cancellation-never-overwrites-newer-result}";

mod import;
mod native;

pub use import::{
    ImportDiagnostic, ImportReport, import_bash, import_fish, import_help, import_man, import_zsh,
};
pub use native::{
    NATIVE_DATABASE_APPLICATION_ID, NATIVE_DATABASE_SCHEMA_VERSION, NativeArgument, NativeCatalog,
    NativeCatalogDiagnostic, NativeCatalogLimits, NativeCatalogReader, NativeCommand,
    NativeCompletionAction, NativeCompletionCandidate, NativeCompletionKind, NativeDiagnosticKind,
    NativeFlag, NativePlatform, NativeProvenance, NativeSemanticHit, compile_native_catalog,
    compile_native_catalog_source, parse_native_catalog, publish_native_catalog,
    publish_native_catalog_source,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Versioned collection of normalized command contracts.
///
/// Built-in, imported, and extension records share this representation so help,
/// completion, generated documentation, and machine clients consume identical facts.
pub struct Catalog {
    /// Version governing the serialized shape and reader migration policy.
    pub schema_version: u32,
    /// Normalized command contracts retained by this catalog.
    pub commands: Vec<CommandSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Complete semantic contract for one command path.
///
/// Exact records must satisfy [`Catalog::quality_issues`]; imported observations
/// may explicitly retain unknown or incomplete facts at lower confidence.
pub struct CommandSpec {
    /// Stable semantic identity, independent of display aliases.
    #[serde(default)]
    pub id: String,
    /// Version of the declaring command/package when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Space-separated command path, such as `git commit`.
    pub path: String,
    /// Alternative invocations that resolve to this same semantic command.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Stable [`CommandSpec::id`] of the containing command, when one is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Human-readable usage form, including argument and option placeholders.
    pub signature: String,
    /// Short description suitable for lists, completion rows, and tool manifests.
    pub summary: String,
    /// Longer behavioral documentation suitable for help and hover views.
    pub details: String,
    /// Positional arguments, valued options, and flags accepted by the command.
    ///
    /// The serialized field is named `arguments`; `options` remains the Rust name
    /// for compatibility with earlier catalog APIs.
    #[serde(default, rename = "arguments", alias = "options")]
    pub options: Vec<ArgumentSpec>,
    /// Complete invocation examples that consumers may render verbatim as code.
    pub examples: Vec<String>,
    /// Typed input, output, and streaming behavior at the command boundary.
    #[serde(default)]
    pub io: IoContract,
    /// Observable authority or state effects that callers should account for.
    pub effects: Vec<Effect>,
    /// Process exit statuses mapped to stable human-readable meanings.
    #[serde(default)]
    pub exit_codes: BTreeMap<i32, String>,
    /// Attribution, trust, and confidence for command-level facts.
    pub provenance: ProvenanceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Semantic contract for one positional argument, valued option, or flag.
pub struct ArgumentSpec {
    /// Accepted spellings, such as `-f` and `--format`, or a positional name.
    pub names: Vec<String>,
    /// How the parser places or consumes this argument.
    pub kind: ArgumentKind,
    /// Stable semantic type name for the consumed value, or `Bool` for a flag.
    pub value_type: String,
    /// Whether a valid invocation must supply this argument.
    pub required: bool,
    /// Whether the argument may occur more than once in one invocation.
    pub repeatable: bool,
    /// Optional finite values or identity of a bounded dynamic provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<CompletionSource>,
    /// Argument names that cannot be supplied together with this argument.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Help and completion prose describing the argument's semantics.
    pub documentation: String,
    /// Complete command invocations demonstrating this argument.
    #[serde(default)]
    pub examples: Vec<String>,
    /// Attribution, trust, and confidence for argument-level facts.
    pub provenance: ProvenanceInfo,
}

/// Compatibility name for [`ArgumentSpec`] retained for callers using the v3 API.
pub type OptionSpec = ArgumentSpec;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Parser role of an [`ArgumentSpec`].
pub enum ArgumentKind {
    /// Value selected by its position rather than by a leading option name.
    Positional,
    /// Named argument that consumes a value.
    Option,
    /// Named boolean switch that consumes no separate value.
    Flag,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
/// Source from which completion values for an argument may be obtained.
pub enum CompletionSource {
    /// Finite values embedded in the catalog and safe to return without execution.
    Static {
        /// Finite candidate values returned without invoking a provider.
        values: Vec<String>,
    },
    /// External provider identified for a bounded completion worker to invoke.
    Dynamic {
        /// Stable provider identity; this field is not executable source code.
        provider: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Typed data contract at a command's standard-input and standard-output boundary.
pub struct IoContract {
    /// Type accepted from the preceding pipeline stage, or `Nothing` when absent.
    pub input: String,
    /// Type produced for the following pipeline stage.
    pub output: String,
    /// Whether output values can be consumed incrementally rather than after collection.
    pub streaming: bool,
}

impl Default for IoContract {
    fn default() -> Self {
        Self {
            input: "Unknown".to_owned(),
            output: "Unknown".to_owned(),
            streaming: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Observable resource or process effect declared by a command.
pub enum Effect {
    /// Reads filesystem data without intentionally changing it.
    ReadFilesystem,
    /// Creates, replaces, removes, or otherwise changes filesystem data.
    WriteFilesystem,
    /// Starts or delegates work to an operating-system process.
    SpawnProcess,
    /// Changes the working directory used by subsequent commands.
    ChangeDirectory,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
/// Kind of source from which a catalog fact was obtained.
pub enum Provenance {
    /// Command contract compiled into Quirl itself.
    Builtin,
    /// External executable observed through non-native metadata.
    External,
    /// Contract declared at Quirl's validated Lua boundary.
    Lua,
    /// Contract contributed by a validated Quirl plugin.
    Plugin,
    /// Declarative Fish completion definition.
    Fish,
    /// Declarative Bash completion definition.
    Bash,
    /// Declarative Zsh completion definition.
    Zsh,
    /// Supplied command-help text parsed heuristically.
    Help,
    /// Supplied rendered or simple roff manual text parsed heuristically.
    Man,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
/// Strength of evidence supporting a catalog fact.
///
/// Variant order is meaningful: merge resolution prefers greater confidence.
pub enum Confidence {
    /// Weak observation that may be incomplete or ambiguous.
    Low,
    /// Useful heuristic observation without a declarative contract.
    Medium,
    /// Declarative fact that is reliable but not owned by Quirl's exact schema.
    High,
    /// Fact validated against an authoritative, versioned Quirl contract.
    Exact,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Trust relationship between Quirl and the producer of a catalog fact.
#[derive(Default)]
pub enum Trust {
    /// Fact is compiled into the Quirl binary.
    Builtin,
    /// Fact crossed a validated trusted-language or plugin boundary.
    Trusted,
    /// Fact came from a declarative external completion format.
    Declared,
    /// Fact was imported from an external observation.
    Imported,
    /// Fact was inferred from prose or another ambiguous representation.
    #[default]
    Heuristic,
}

/// Attribution for a catalog fact. Imported command options retain their own
/// provenance when multiple sources contribute to the same command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceInfo {
    /// Format or runtime boundary that produced the fact.
    pub source: Provenance,
    /// Strength of evidence used when resolving merges and quality requirements.
    pub confidence: Confidence,
    /// Authority relationship with the producer, independent of evidence strength.
    #[serde(default)]
    pub trust: Trust,
    /// Human-readable source path or identity, when the fact was imported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Stable content identity of the source used for cache invalidation and explanation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Deterministic source timestamp, when one is supplied by the producer.
    ///
    /// Importers omit ambient wall-clock time so unchanged catalogs remain byte-stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

impl ProvenanceInfo {
    /// Construct attribution for a command known directly by the running composition.
    ///
    /// Built-in, Lua, and plugin sources become exact facts; external formats receive
    /// confidence and trust appropriate to their declaration or inference mechanism.
    pub fn builtin(source: Provenance) -> Self {
        let confidence = match source {
            Provenance::Builtin | Provenance::Lua | Provenance::Plugin => Confidence::Exact,
            Provenance::External => Confidence::Medium,
            Provenance::Fish | Provenance::Bash | Provenance::Zsh => Confidence::High,
            Provenance::Help | Provenance::Man => Confidence::Medium,
        };
        Self {
            source,
            confidence,
            trust: match source {
                Provenance::Builtin => Trust::Builtin,
                Provenance::Lua | Provenance::Plugin => Trust::Trusted,
                Provenance::External => Trust::Imported,
                Provenance::Fish | Provenance::Bash | Provenance::Zsh => Trust::Declared,
                Provenance::Help | Provenance::Man => Trust::Heuristic,
            },
            origin: None,
            fingerprint: None,
            generated_at: None,
        }
    }

    /// Construct attribution for content imported from an identified source.
    ///
    /// `origin` should identify the file or logical provider, while `fingerprint`
    /// should change whenever the imported bytes that support the fact change.
    pub fn imported(
        source: Provenance,
        confidence: Confidence,
        origin: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            source,
            confidence,
            trust: match source {
                Provenance::Fish | Provenance::Bash | Provenance::Zsh => Trust::Declared,
                Provenance::Help | Provenance::Man => Trust::Heuristic,
                _ => Trust::Imported,
            },
            origin: Some(origin.into()),
            fingerprint: Some(fingerprint.into()),
            generated_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Provenance-expanded view of all retained facts for one command.
pub struct CatalogExplanation {
    /// Canonical command path whose facts are explained.
    pub command: String,
    /// Command- and argument-level facts in deterministic presentation order.
    pub facts: Vec<FactExplanation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// One rendered catalog fact paired with its source attribution.
pub struct FactExplanation {
    /// Stable category name such as `signature` or `argument_documentation`.
    pub fact: String,
    /// Human-readable value retained for this fact.
    pub value: String,
    /// Source, confidence, trust, and optional import identity for the value.
    pub provenance: ProvenanceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// One deterministic completion candidate and its replacement span.
pub struct Completion {
    /// Exact text to insert into the input buffer.
    pub value: String,
    /// Human-readable candidate label, which may include a usage placeholder.
    pub display: String,
    /// Short documentation shown alongside the candidate.
    pub summary: String,
    /// Longer signature, type, provenance, or behavioral context.
    pub detail: String,
    /// Inclusive UTF-8 byte offset at which replacement begins.
    pub replace_start: usize,
    /// Exclusive UTF-8 byte offset at which replacement ends.
    pub replace_end: usize,
    /// Character indices in [`Completion::value`] that contributed to the fuzzy match.
    pub match_indices: Vec<usize>,
}

/// Versioned completion work submitted by an interactive client. The owning UI
/// validates bounds and drives cancellation before calling the catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompletionRequest {
    /// Completion wire version; must equal [`COMPLETION_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Strictly increasing client identifier used to suppress stale worker results.
    pub request_id: u64,
    /// Complete UTF-8 input buffer, bounded by [`MAX_COMPLETION_QUERY_BYTES`].
    pub line: String,
    /// UTF-8 byte offset of the cursor. Protocol boundaries reject offsets that
    /// are not character boundaries; [`Catalog::complete`] also clamps defensively.
    pub cursor: usize,
    /// Maximum requested candidates, no greater than [`MAX_COMPLETION_RESULTS`].
    pub limit: usize,
    /// Requested wall deadline in milliseconds, from 1 through
    /// [`MAX_COMPLETION_DEADLINE_MS`].
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Request to stop outstanding completion work for one request identifier.
pub struct CompletionCancellation {
    /// Completion wire version; must equal [`COMPLETION_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Identifier of the request whose result must no longer be published.
    pub request_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status", content = "data")]
/// Terminal state of one bounded asynchronous completion request.
pub enum CompletionOutcome {
    /// Completion finished within its cancellation and deadline constraints.
    Ready {
        /// Deterministically ranked candidates, capped by the request limit.
        items: Vec<Completion>,
    },
    /// Client cancellation became observable before a result was committed.
    Cancelled,
    /// The configured wall deadline elapsed before completion finished.
    DeadlineExceeded,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Versioned response envelope correlated with one [`CompletionRequest`].
pub struct CompletionResponse {
    /// Completion wire version; must equal [`COMPLETION_PROTOCOL_VERSION`].
    pub protocol_version: u32,
    /// Request identifier copied unchanged from the corresponding request.
    pub request_id: u64,
    /// Result state produced by the completion worker.
    pub outcome: CompletionOutcome,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCatalogV3 {
    #[serde(rename = "schema_version")]
    _schema_version: u32,
    commands: Vec<LegacyCommandV3>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyCommandV3 {
    path: String,
    signature: String,
    summary: String,
    details: String,
    options: Vec<LegacyOptionV3>,
    examples: Vec<String>,
    effects: Vec<Effect>,
    provenance: ProvenanceInfo,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyOptionV3 {
    names: Vec<String>,
    value: Option<String>,
    summary: String,
    provenance: ProvenanceInfo,
}

impl Catalog {
    /// Decode the current schema or explicitly migrate the prior indexed
    /// completion schemas. Older facts retain their original confidence and
    /// receive unknown/default fields rather than fabricated exact metadata.
    pub fn from_json(source: &str) -> Result<Self, serde_json::Error> {
        let schema_version = serde_json::from_str::<serde_json::Value>(source)?
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| unsupported_catalog_schema_error("<missing or non-integer>"))?;
        if schema_version == u64::from(CATALOG_SCHEMA_VERSION) {
            return serde_json::from_str(source);
        }
        if matches!(schema_version, 2 | 3) {
            let legacy = serde_json::from_str::<LegacyCatalogV3>(source)?;
            return Ok(Self {
                schema_version: CATALOG_SCHEMA_VERSION,
                commands: legacy
                    .commands
                    .into_iter()
                    .map(|command| {
                        let path = command.path;
                        CommandSpec {
                            id: command_id(&path),
                            version: None,
                            aliases: Vec::new(),
                            parent: path.rsplit_once(' ').map(|(parent, _)| command_id(parent)),
                            path,
                            signature: command.signature,
                            summary: command.summary,
                            details: command.details,
                            options: command
                                .options
                                .into_iter()
                                .map(|option| {
                                    imported_argument(
                                        option.names,
                                        option.value,
                                        option.summary,
                                        option.provenance,
                                    )
                                })
                                .collect(),
                            examples: command.examples,
                            io: IoContract::default(),
                            effects: command.effects,
                            exit_codes: BTreeMap::new(),
                            provenance: command.provenance,
                        }
                    })
                    .collect(),
            });
        }
        Err(unsupported_catalog_schema_error(
            &schema_version.to_string(),
        ))
    }

    /// Build the deterministic catalog compiled into this Quirl version.
    ///
    /// Exact built-in records include version, typed I/O, examples, effects, and
    /// exit-code metadata. Callers may subsequently merge imported or plugin facts
    /// without allowing lower-confidence observations to replace exact contracts.
    pub fn builtin() -> Self {
        let mut catalog = Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            commands: vec![
                command(
                    "help",
                    "help [command]",
                    "Explore commands and their contracts",
                    "Reads this same catalog used by completion and AI discovery.",
                    vec![],
                    &["help git commit"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "mode",
                    "mode <normal|data|ai|toggle>",
                    "Switch the visible interactive grammar",
                    "Normal mode carries bytes and process status. Data mode evaluates Quirl's native structured values and pipelines. AI mode searches the local command database and inserts suggestions for review without executing them.",
                    vec![],
                    &["mode data", "mode ai", "mode normal", "mode toggle"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl data",
                    "quirl data <source> [| transform ...] [--format table|plain|json]",
                    "Evaluate a native structured-data pipeline",
                    "Sources are `pwd`, `files [path]` (or `ls`), `open <path>`, JSON, or explicit `^external <command>`. Values stay typed through streams and transforms: filesystem rows retain Path, Size, and DateTime; tar retains Path and Size; TOML retains DateTime. JSON/YAML/TOML and bytes are explicit conversion boundaries, and `to json` documents its lossy domain encoding. The CLI injects a deadline-, cancellation-, and output-bounded external host; library runtimes fail closed without one. CSV/tar rows are pulled lazily and all adapters enforce byte, row, field, depth, node, retained-text, and materialization limits. `sort`, table output, envelopes, and collected convenience APIs intentionally materialize within those limits. Transforms include typed `where` comparisons with `and`/`or`, dotted `get`, `select`, `sort`, `take`, Option-producing `first`, and `length`. Task remains declarative state; HTTP is not an implicit source.",
                    vec![option_with_static_values(
                        &["--format"],
                        "table|plain|json",
                        &["table", "plain", "json"],
                        "Choose a human table/plain renderer or the stable typed JSON envelope",
                    )],
                    &[
                        "mode data",
                        "files . | select name kind size",
                        "ls . | where kind == file and size > 1024 | sort size desc | take 10",
                        "quirl data '[1,2,3] | length'",
                        "quirl data 'open users.csv' --format table",
                        "quirl data '^external printf \"{\\\"ok\\\":true}\" | from json'",
                    ],
                    &[Effect::ReadFilesystem, Effect::SpawnProcess],
                    Provenance::Builtin,
                ),
                command(
                    "quirl data ls",
                    "quirl data ls [path]",
                    "List a directory as typed entries in Data mode",
                    "`ls` is a Data-mode source alias for `files`. It produces typed filesystem entries for structured transforms. Normal mode does not use this contract: an unqualified `ls` there resolves through PATH and uses the installed system command's contract.",
                    vec![],
                    &[
                        "mode data",
                        "ls . | where kind == file | sort size desc | take 10",
                        "quirl data 'ls . | select name kind size'",
                    ],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "cd",
                    "cd [path]",
                    "Change the shell working directory",
                    "Changes Quirl's process directory so later commands and prompt context follow it.",
                    vec![],
                    &["cd .."],
                    &[Effect::ChangeDirectory],
                    Provenance::Builtin,
                ),
                command(
                    "lua",
                    "lua <expression>",
                    "Evaluate Lua without leaving command mode",
                    "Runs an expression in the persistent restricted Lua 5.4 VM.",
                    vec![],
                    &["lua return 20 + 22"],
                    &[],
                    Provenance::Lua,
                ),
                command(
                    "quirl new",
                    "quirl new <name> [--lang lua|quirl] [--directory path]",
                    "Create a checked script",
                    "Writes a deterministic Lua or native Quirl script with create-new semantics, so an existing script is never overwritten. Lua is the default; `--lang quirl` generates the canonical `.qrl` extension. `.quirl` and `.🌀` remain accepted input aliases.",
                    vec![
                        option_with_static_values(
                            &["--lang"],
                            "lua|quirl",
                            &["lua", "quirl"],
                            "Choose Lua (the default) or native Quirl (`.qrl`)",
                        ),
                        option(
                            &["--directory"],
                            Some("path"),
                            "Choose the destination directory",
                        ),
                    ],
                    &["quirl new script", "quirl new script --lang quirl"],
                    &[Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl describe",
                    "quirl describe <command> [--format text|json|markdown|html]",
                    "Describe one installed command",
                    "Renders one exact entry from the same semantic catalog used by completion, documentation, language services, and agents.",
                    vec![option(
                        &["--format"],
                        Some("text|json|markdown|html"),
                        "Choose a deterministic documentation view",
                    )],
                    &["quirl describe 'quirl run' --format markdown"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl doc",
                    "quirl doc [--format text|json|markdown|html] [--output path] [--open]",
                    "Generate installed command documentation",
                    "Generates deterministic human or machine documentation from the installed catalog, writes requested files atomically, and can open an explicit output in the platform viewer.",
                    vec![
                        option(
                            &["--format"],
                            Some("text|json|markdown|html"),
                            "Choose a deterministic documentation view",
                        ),
                        option(
                            &["--output"],
                            Some("path"),
                            "Atomically write the generated view",
                        ),
                        option(
                            &["--open"],
                            None,
                            "Open the explicit output in the default viewer",
                        ),
                    ],
                    &["quirl doc --format html --output target/quirl-docs/catalog.html --open"],
                    &[Effect::WriteFilesystem, Effect::SpawnProcess],
                    Provenance::Builtin,
                ),
                command(
                    "quirl run",
                    "quirl run <file|-> [--lang lua|quirl|bash|zsh] [arguments...]",
                    "Run a script through its explicit language engine",
                    "Reads at most 4 MiB from an admitted regular file, or bounded standard input. Selects a language by explicit flag, shebang, or extension; Lua uses the restricted VM, `.qrl` uses native executors, and `.quirl` plus `.🌀` are accepted native aliases. Bash/Zsh use reference interpreters with startup files disabled and structured capture.",
                    vec![option(
                        &["--lang"],
                        Some("lua|quirl|bash|zsh"),
                        "Select the language explicitly, including for stdin",
                    )],
                    &[
                        "quirl run scripts/deploy.lua -- staging",
                        "quirl run --lang lua -",
                        "quirl run --lang bash legacy.sh",
                        "quirl run --lang zsh release.zsh",
                    ],
                    &[Effect::ReadFilesystem, Effect::SpawnProcess],
                    Provenance::Builtin,
                ),
                command(
                    "quirl check",
                    "quirl check <file|directory> [--format text|json]",
                    "Validate scripts without executing them",
                    "Deterministically discovers Lua and Quirl scripts through a bounded, non-recursive traversal that skips links and build/VCS directories; checks Lua syntax, annotations, modules, and restricted APIs plus Quirl statement structure; and aggregates structured diagnostics without executing source.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose diagnostic output",
                    )],
                    &["quirl check scripts/deploy.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl fmt",
                    "quirl fmt <file|directory> [--check]",
                    "Format Lua and Quirl scripts deterministically",
                    "Deterministically discovers Lua and native `.qrl` scripts (including `.quirl` and `.🌀` aliases), formats them through their bounded formatters, and installs changed source through a synchronized same-directory atomic replacement that rejects links, aliases, special files, and observed concurrent changes. `--check` never writes.",
                    vec![option(&["--check"], None, "Report drift without writing")],
                    &["quirl fmt examples/config.lua --check"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl lint",
                    "quirl lint <file|directory> [--format text|json]",
                    "Lint scripts without executing them",
                    "Aggregates annotation and capability diagnostics for scripts found through the same bounded non-recursive discovery contract and rejects ambient APIs that bypass Quirl capabilities.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose diagnostic output",
                    )],
                    &["quirl lint examples/plugin.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl test",
                    "quirl test [file|directory]",
                    "Run a Lua test module under resource limits",
                    "Discovers conventional Lua test modules through the bounded non-recursive authoring traversal and runs every returned `test_*` function in an isolated restricted runtime.",
                    vec![],
                    &["quirl test", "quirl test examples/lua_tests.lua"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config check",
                    "quirl config check <file> [--format text|json]",
                    "Validate Lua configuration through Rust schemas",
                    "Evaluates under config restrictions and preserves the active last-known-good value on failure.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose output format",
                    )],
                    &["quirl config check examples/config.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config get",
                    "quirl config get <file> <key>",
                    "Read one evaluated configuration value",
                    "Evaluates the complete restricted Lua configuration, validates it through Rust schemas, and prints one recognized typed field.",
                    vec![],
                    &["quirl config get ~/.config/quirl/config.lua editor.keymap"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config set",
                    "quirl config set <file> <key> <value>",
                    "Safely patch one literal configuration value",
                    "Changes only a recognized literal field in `quirl.config`, including `prompt.symbols` (`auto`, `plain`, `unicode`, or explicit `nerd_font`), validates the complete candidate before an atomic replacement, and retains the previous source as `.bak`.",
                    vec![],
                    &[
                        "quirl config set ~/.config/quirl/config.lua picker.preview false",
                        "quirl config set ~/.config/quirl/config.lua prompt.symbols nerd_font",
                    ],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config web",
                    "quirl config web <file> [--port <port>]",
                    "Open the local schema-backed configuration form",
                    "Serves an accessible configuration form only on IPv4 loopback. The private session URL carries a CSRF token; saves validate the complete Lua configuration, retain a `.bak`, preserve non-overlapping concurrent source edits, and reject conflicting or code-controlled fields.",
                    vec![option(
                        &["--port"],
                        Some("port"),
                        "Loopback port; 0 selects an available port",
                    )],
                    &[
                        "quirl config web ~/.config/quirl/config.lua",
                        "quirl config web examples/config.lua --port 8787",
                    ],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config tui",
                    "quirl config tui <file>",
                    "Inspect schema-backed configuration in the terminal",
                    "Shows current editor, picker, and prompt symbol values, allowed values, and textual editing guidance in an accessible line-oriented view.",
                    vec![],
                    &["quirl config tui ~/.config/quirl/config.lua"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config fmt",
                    "quirl config fmt <file> [--check]",
                    "Format a Lua configuration deterministically",
                    "Validates the authoritative configuration, then applies Quirl's deterministic Lua formatter. `--check` reports drift and never writes.",
                    vec![option(
                        &["--check"],
                        None,
                        "Report formatting drift without writing",
                    )],
                    &["quirl config fmt ~/.config/quirl/config.lua --check"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config export",
                    "quirl config export <file> [--format text|json]",
                    "Export evaluated schema-backed configuration",
                    "Reads and validates config.lua, then emits a deterministic terminal-safe text view or versioned JSON document without modifying source.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose output format",
                    )],
                    &["quirl config export ~/.config/quirl/config.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config diff",
                    "quirl config diff <file> <other> [--format text|json]",
                    "Compare two evaluated configurations",
                    "Loads both authoritative Lua files under the restricted schema policy and reports deterministic field-level differences without changing either file.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose output format",
                    )],
                    &["quirl config diff personal.lua work.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config migrate",
                    "quirl config migrate <file> --dry-run [--format text|json]",
                    "Preview a schema migration without rewriting configuration",
                    "0.1.0 only previews the unversioned-to-v1 schema insertion. `--dry-run` is required and no source or backup file is written.",
                    vec![
                        option(
                            &["--dry-run"],
                            None,
                            "Require a non-mutating migration preview",
                        ),
                        option(&["--format"], Some("text|json"), "Choose output format"),
                    ],
                    &["quirl config migrate ~/.config/quirl/config.lua --dry-run"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config doctor",
                    "quirl config doctor <file> [--format text|json]",
                    "Diagnose configuration schema and editability",
                    "Validates the authoritative configuration and reports which recognized settings are safe literal patches versus code-controlled expressions; it never writes source.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose output format",
                    )],
                    &["quirl config doctor ~/.config/quirl/config.lua"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin check",
                    "quirl plugin check <file> [--format text|json]",
                    "Validate Lua plugin registrations",
                    "Reads at most 4 MiB from an admitted regular file, then loads a trusted plugin with process access denied and validates prompt and completion callbacks.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose output format",
                    )],
                    &["quirl plugin check examples/plugin.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin add",
                    "quirl plugin add <source> [--allow capability]... [--format text|json]",
                    "Install a local plugin with explicit permission approval",
                    "Reads the manifest and entry only from admitted regular files under their 256 KiB and 4 MiB limits. It validates the versioned runtime boundary, shows the permission diff, records SHA-256 source checksums, and atomically installs a disabled permission lock without implicit network access.",
                    vec![
                        repeatable_option(
                            &["--allow"],
                            "capability",
                            "Approve one requested capability after review; repeat as needed",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable machine JSON",
                        ),
                    ],
                    &[
                        "quirl plugin add ./kubernetes-workbench --allow commands.register --format json",
                    ],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin permissions",
                    "quirl plugin permissions <name> [--format text|json]",
                    "Inspect requested and granted plugin authority",
                    "Reads the permission lock and emits requested, granted, added, removed, and unchanged capabilities without loading plugin code.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl plugin permissions kubernetes-workbench"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin enable",
                    "quirl plugin enable <name> [--format text|json]",
                    "Enable a checksum-verified installed plugin",
                    "Runs doctor and the declared non-executing or budgeted trusted boundary before atomically enabling the existing locked plugin.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl plugin enable kubernetes-workbench"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin disable",
                    "quirl plugin disable <name> [--format text|json]",
                    "Disable a plugin while retaining its permission lock",
                    "Atomically changes only enabled state; source, checksums, versions, and granted capabilities remain locked for inspection and recovery.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl plugin disable kubernetes-workbench"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin doctor",
                    "quirl plugin doctor <name> [--format text|json]",
                    "Diagnose plugin schema, source integrity, permissions, and runtime boundaries",
                    "Verifies lock schema/API versions and SHA-256 checksums, then validates trusted Lua registrations or isolated Wasm/out-of-process boundaries with accessible diagnostics.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl plugin doctor kubernetes-workbench --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin update",
                    "quirl plugin update --locked [--format text|json]",
                    "Verify installed plugins without changing locked authority",
                    "Re-resolves every local source and rejects any version, checksum, API, or permission change; platform v0.1 does not perform network updates.",
                    vec![
                        option(
                            &["--locked"],
                            None,
                            "Forbid changes to versions, checksums, API versions, and capabilities",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable machine JSON",
                        ),
                    ],
                    &["quirl plugin update --locked --format json"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin remove",
                    "quirl plugin remove <name> [--format text|json]",
                    "Remove an installed plugin lock without deleting source",
                    "Atomically removes the named permission record; external source directories are never deleted by the plugin manager.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl plugin remove kubernetes-workbench"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl sdk",
                    "quirl sdk [--format text|json|markdown]",
                    "Export the generated Lua extension SDK",
                    "LuaLS stubs, AI JSON, and human documentation are generated from the same Rust host API definitions.",
                    vec![option(
                        &["--format"],
                        Some("text|json|markdown"),
                        "Choose the generated SDK view",
                    )],
                    &["quirl sdk --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl catalog",
                    "quirl catalog [--format text|json|markdown]",
                    "Export installed command knowledge for humans or AI",
                    "Emits the versioned semantic catalog bundled with this binary.",
                    vec![option(
                        &["--format"],
                        Some("text|json|markdown"),
                        "Choose a stable output format",
                    )],
                    &["quirl catalog --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl agent catalog",
                    "quirl agent catalog [--format text|json]",
                    "Export installed commands and Lua host capabilities",
                    "Emits a versioned deny-unknown schema with deterministic catalog and HOST_API content hashes, provenance, installed capabilities, and their versions.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl agent catalog --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl agent context",
                    "quirl agent context <query...> [--token-budget count] [--format markdown|json]",
                    "Build deterministic token-budgeted agent context",
                    "Ranks only installed command and HOST_API facts, selects the smallest relevant subtree within a documented deterministic token estimate, and records truncation and source hashes.",
                    vec![
                        option(
                            &["--token-budget"],
                            Some("count"),
                            "Bound the canonical context payload",
                        ),
                        option(
                            &["--format"],
                            Some("markdown|json"),
                            "Choose agent Markdown or stable machine JSON",
                        ),
                    ],
                    &[
                        "quirl agent context 'deploy the billing service' --format markdown --token-budget 6000",
                    ],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl agent manifest",
                    "quirl agent manifest [--format text|json]",
                    "Export installed tools, versions, schemas, and validators",
                    "Lists only tools and capabilities installed in this Quirl composition, with schema/content hashes and validation commands grounded in the semantic catalog and generated Lua HOST_API.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl agent manifest --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl agent validate",
                    "quirl agent validate <file> --kind catalog|context|manifest [--format text|json]",
                    "Validate a versioned agent contract without execution",
                    "Reads at most 4 MiB from an admitted regular file, then rejects unknown fields, unsupported schema versions, tampered content hashes, nondeterministic ordering, and context payloads that exceed their declared token budget.",
                    vec![
                        required_option(
                            &["--kind"],
                            "catalog|context|manifest",
                            "Select the deny-unknown document schema",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible diagnostics or stable JSON",
                        ),
                    ],
                    &["quirl agent validate agent-context.json --kind context --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl ai status",
                    "quirl ai status [--format text|json]",
                    "Inspect local command-intelligence readiness",
                    "Reports the SQLite command database and potion-base-8M file locations without loading a model or contacting the network.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl ai status --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl ai index",
                    "quirl ai index [--format text|json]",
                    "Build local semantic command and option embeddings",
                    "Loads potion-base-8M from local files, embeds bounded command and option documents, validates every vector, and atomically replaces the SQLite database only after a complete transaction.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl ai index"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl ai search",
                    "quirl ai search <query...> [--limit count] [--kind all|command|option] [--format text|json]",
                    "Find commands and options by task intent",
                    "Always ranks the complete bounded lexical document set and fuses it with matching local Model2Vec embeddings when the exact model, document-generation version, dimensions, and index fingerprint agree. Missing, stale, cancelled, invalid, or mismatched semantic state falls back to lexical results. Search is local, CPU-only, network-free, and never executes a result.",
                    vec![
                        option(
                            &["--limit"],
                            Some("count"),
                            "Bound returned suggestions to at most 100",
                        ),
                        option_with_static_values(
                            &["--kind"],
                            "all|command|option",
                            &["all", "command", "option"],
                            "Restrict results to commands, options, or both",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable machine JSON",
                        ),
                    ],
                    &["quirl ai search 'copy a directory preserving permissions'"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl ai run",
                    "quirl ai run <query...>",
                    "Preview and explicitly confirm one catalog-backed command",
                    "Uses bounded hybrid retrieval and the retrieval-only CommandProposal fallback to select a current catalog command ID. Quirl asks the user for each unresolved required value and parses it as the catalog-declared type; neither retrieval nor the slot resolver may invent shell text. Trusted Rust code revalidates the completed proposal, renders the exact quoted command, and requires explicit acceptance before the existing execution path is entered. Unknown effects, filesystem mutation, process spawning, and session changes show their reason and require a distinct second high-risk confirmation.",
                    vec![],
                    &["quirl ai run 'show the current working directory'"],
                    &[
                        Effect::ReadFilesystem,
                        Effect::WriteFilesystem,
                        Effect::SpawnProcess,
                        Effect::ChangeDirectory,
                    ],
                    Provenance::Builtin,
                ),
                command(
                    "quirl ai related",
                    "quirl ai related <command...> [--limit count] [--format text|json]",
                    "Suggest related commands and options",
                    "Uses the same local semantic index as AI mode, excludes the requested command, and returns bounded suggestions without execution.",
                    vec![
                        option(
                            &["--limit"],
                            Some("count"),
                            "Bound returned suggestions to at most 100",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable machine JSON",
                        ),
                    ],
                    &["quirl ai related git commit"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl package manifest",
                    "quirl package manifest [--manifest path] [--format text|json]",
                    "Parse a versioned project package manifest",
                    "Reads a deny-unknown plugin.toml schema and shows normalized package identity, Quirl compatibility, requested capabilities, and contributions without loading its Lua entry.",
                    vec![
                        option(
                            &["--manifest"],
                            Some("path"),
                            "Read a manifest other than ./plugin.toml",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable machine JSON",
                        ),
                    ],
                    &["quirl package manifest --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl package build",
                    "quirl package build [--manifest path] [--format text|json]",
                    "Validate and build a deterministic package contract",
                    "Checks the entry path, Quirl version range, installed capabilities, and the public-command quality gate for summaries, argument docs and types, examples, effects, and error codes; it returns content hashes without executing Lua.",
                    vec![
                        option(
                            &["--manifest"],
                            Some("path"),
                            "Build a manifest other than ./plugin.toml",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible diagnostics or stable JSON",
                        ),
                    ],
                    &["quirl package build --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl package publish",
                    "quirl package publish --dry-run [--manifest path] [--format text|json]",
                    "Preview a deterministic network-free package publication",
                    "Runs the complete package build quality gate and emits the files, build hash, and requested permissions that would be published. Phase 2 performs no network publication.",
                    vec![
                        option(
                            &["--dry-run"],
                            None,
                            "Require a network-free publication plan",
                        ),
                        option(
                            &["--manifest"],
                            Some("path"),
                            "Read a manifest other than ./plugin.toml",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable JSON",
                        ),
                    ],
                    &["quirl package publish --dry-run --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl lsp",
                    "quirl lsp",
                    "Serve generated Lua and native Quirl (`.qrl`) editor intelligence",
                    "Speaks a deterministic LSP subset over stdio, using the generated Lua HOST_API and semantic command catalog for diagnostics, completion, hover, signatures, and module docs without evaluating documents.",
                    vec![],
                    &["quirl lsp"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl serve mcp",
                    "quirl serve mcp --capabilities catalog|complete|check|format",
                    "Serve explicitly granted source intelligence over MCP stdio",
                    "Supports modern 2026-07-28 discovery and explicitly negotiated legacy clients. Each process exposes only the requested bounded catalog, completion, source-check, and source-format tools; it grants no filesystem, network, plugin, or command-execution authority.",
                    vec![required_repeatable_option_with_static_values(
                        &["--capabilities"],
                        "catalog|complete|check|format",
                        &["catalog", "complete", "check", "format"],
                        "Grant one or more comma-separated MCP tools",
                    )],
                    &["quirl serve mcp --capabilities catalog,complete,check,format"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl eval",
                    "quirl eval <lua-expression>",
                    "Evaluate Lua and print the returned value",
                    "Runs one expression in the same restricted, budgeted Lua runtime used by scripts.",
                    vec![],
                    &["quirl eval 'return 20 + 22'"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl complete",
                    "quirl complete <input> [--format text|json]",
                    "Query the semantic completion engine",
                    "Returns the same attributed completion items used by the interactive editor.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose stable text or JSON output",
                    )],
                    &["quirl complete 'git commit --am' --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl exec",
                    "quirl exec <source> [--format text|json]",
                    "Execute Quirl's native command graph",
                    "Accepts complete Quirl source as exactly one outer-shell argument and passes it unchanged to extension planning. Without an explicit extension rewrite, the same source is parsed, diagnosed, and captured for bounded, redacted recovery. Quoting, empty arguments, byte pipes, redirects, boolean lists, and background jobs must be written inside that source operand; additional argv elements are rejected rather than joined into executable syntax.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose human-readable or stable ShellError output for failures",
                    )],
                    &[
                        "quirl exec \"printf '%s' 'hello world'\"",
                        "quirl exec \"printf '<%s>' ''\"",
                        "quirl exec \"printf '%s\\n' first second | grep second\"",
                        "quirl exec \"printf '%s' output > result.txt\"",
                    ],
                    &[Effect::SpawnProcess, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl recover",
                    "quirl recover <list|show> [--format text|json]",
                    "Inspect recoverable command-failure snapshots",
                    "Lists or displays versioned atomic snapshots containing a redacted command, working directory, environment diff, bounded captured output, timing, status, and error chain.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose stable text or JSON output",
                    )],
                    &["quirl recover list", "quirl recover show --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl recover list",
                    "quirl recover list [--format text|json]",
                    "List recoverable command-failure snapshots",
                    "Lists versioned snapshot identifiers newest first without reading or replaying command contents.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose stable text or JSON output",
                    )],
                    &["quirl recover list --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl recover show",
                    "quirl recover show [id] [--format text|json]",
                    "Inspect one recoverable command-failure snapshot",
                    "Displays an explicit snapshot or the newest available snapshot, including redacted context and bounded captured output; it never replays the command.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose stable text or JSON output",
                    )],
                    &[
                        "quirl recover show",
                        "quirl recover show 1786826467026-5841-0 --format json",
                    ],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl pick",
                    "quirl pick [--source stdin|history|files|actions] [--query text] [--multi] [--limit count] [--root path] [--format text|json]",
                    "Select typed values with Quirl's shared fuzzy engine",
                    "The same deterministic exact/fuzzy/inverse query model ranks history, files, actions, jobs, completions, and data while returning the original value.",
                    vec![
                        option(
                            &["--source"],
                            Some("stdin|history|files|actions"),
                            "Choose the typed provider",
                        ),
                        option(&["--query"], Some("text"), "Set the initial fuzzy query"),
                        option(&["--multi"], None, "Return multiple selected values"),
                        option(&["--limit"], Some("count"), "Bound multi-selection output"),
                        option(&["--root"], Some("path"), "Set the file provider root"),
                        option(&["--format"], Some("text|json"), "Choose stable output"),
                    ],
                    &[
                        "quirl pick --source history --query cargo",
                        "quirl pick --source files --query src",
                        "quirl pick --source actions --query index",
                    ],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl events schema",
                    "quirl events schema [--format text|json]",
                    "Describe the installed typed extension protocol",
                    "Lists immutable lifecycle, directory, plan, progress, output, cancellation, result, and error events together with declared mutation capabilities and the composed catalog, completion, and panel contribution kinds.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or versioned machine JSON",
                    )],
                    &["quirl events schema --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl events validate",
                    "quirl events validate <file> [--format text|json]",
                    "Validate an immutable extension event trace",
                    "Reads at most 4 MiB from an admitted regular file, then checks the deny-unknown versioned envelope, protocol version, safe output text, and strictly increasing event sequence without invoking a plugin.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable validation JSON",
                    )],
                    &["quirl events validate trace.json --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl view directory",
                    "quirl view directory [path] [--all] [--format text|json]",
                    "Render a directory as an escape-safe typed panel",
                    "Produces a line-oriented name, kind, byte-size, and modification view with a plain fallback; raw terminal control bytes are rejected.",
                    vec![
                        option(&["--all"], None, "Include hidden entries"),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose the plain view or typed panel model",
                        ),
                    ],
                    &[
                        "quirl view directory .",
                        "quirl view directory --format json",
                    ],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl view processes",
                    "quirl view processes [--format text|json]",
                    "Render the process table as an escape-safe typed panel",
                    "Reads the platform process table into typed pid, parent, state, and command cells and renders an accessible plain fallback without accepting terminal escapes.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose the plain view or typed panel model",
                    )],
                    &["quirl view processes"],
                    &[Effect::SpawnProcess],
                    Provenance::Builtin,
                ),
                command(
                    "quirl view panel",
                    "quirl view panel <name> [--format text|json]",
                    "Render an enabled plugin's typed panel contribution",
                    "Invokes one permission-locked, deadline-bounded panel provider, validates its escape-free PanelModel and declared plain fallback, then renders accessible text or JSON.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose the plain view or typed panel model",
                    )],
                    &[
                        "quirl view panel cluster",
                        "quirl view panel cluster --format json",
                    ],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl watch",
                    "quirl watch <expression> [--samples count] [--interval-ms ms] [--capacity count] [--format text|json]",
                    "Watch a typed data pipeline with bounded live samples",
                    "Re-evaluates the native data expression until the declared sample bound or Ctrl-C, checks cancellation between stages and during refresh waits, retains a bounded completed-sample history, and reports older samples dropped from retention.",
                    vec![
                        option(&["--samples"], Some("count"), "Set a bounded sample count"),
                        option(
                            &["--interval-ms"],
                            Some("ms"),
                            "Set the cancellable refresh interval",
                        ),
                        option(
                            &["--capacity"],
                            Some("count"),
                            "Bound retained samples to at most 256",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose live lines or a bounded JSON snapshot",
                        ),
                    ],
                    &["quirl watch pwd --samples 3 --interval-ms 250"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "jobs",
                    "jobs",
                    "List structured background job state",
                    "Shows Quirl job ids, running/stopped/done state, and the original command.",
                    vec![],
                    &["jobs"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "fg",
                    "fg [%job]",
                    "Resume a job in the foreground",
                    "Transfers terminal ownership to the selected process group and waits until it exits or stops again.",
                    vec![],
                    &["fg", "fg %2"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "bg",
                    "bg [%job]",
                    "Resume a stopped job in the background",
                    "Sends SIGCONT to the selected process group without transferring terminal ownership.",
                    vec![],
                    &["bg", "bg %2"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "export",
                    "export NAME=value...",
                    "Set environment variables for later commands",
                    "The Preview grammar accepts explicit NAME=value assignments without shell expansion.",
                    vec![],
                    &["export RUST_LOG=debug"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl index build",
                    "quirl index build [--fish path]... [--bash path]... [--zsh path]... [--help path]... [--man path]... [--output path] [--format text|json]",
                    "Build the attributed completion index",
                    "Imports declarative Fish, Bash, and Zsh completions from admitted regular files up to 4 MiB each, retaining at most 256 commands per declaration, 2,048 commands, 4,096 candidates, and 4 MiB of normalized catalog text per file. Help/man text is limited to 1 MiB each. It never sources or executes providers, commands, or man, then atomically writes a versioned catalog.",
                    vec![
                        repeatable_option(
                            &["--fish"],
                            "path",
                            "Import a Fish completion file or directory",
                        ),
                        repeatable_option(
                            &["--bash"],
                            "path",
                            "Import a Bash completion file or directory",
                        ),
                        repeatable_option(
                            &["--zsh"],
                            "path",
                            "Import a Zsh completion file or directory",
                        ),
                        repeatable_option(
                            &["--help"],
                            "path",
                            "Parse supplied command-help text without executing its command",
                        ),
                        repeatable_option(
                            &["--man"],
                            "path",
                            "Parse supplied rendered/raw man text without invoking man",
                        ),
                        option(
                            &["--output"],
                            Some("path"),
                            "Write a specific index instead of the default cache",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose the build report format",
                        ),
                    ],
                    &[
                        "quirl index build",
                        "quirl index build --zsh completions/_tool",
                        "quirl index build --help captured/tool-help.txt --man docs/tool.man",
                    ],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl index explain",
                    "quirl index explain <command...> [--index path] [--format text|json]",
                    "Explain where indexed command facts came from",
                    "Reads an admitted regular catalog file up to 4 MiB, then shows source kind, confidence, origin, and fingerprint for command metadata and each retained option.",
                    vec![
                        option(&["--index"], Some("path"), "Read a specific catalog index"),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose the explanation format",
                        ),
                    ],
                    &[
                        "quirl index explain git",
                        "quirl index explain cargo --format json",
                    ],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "git commit",
                    "git commit [options]",
                    "Record changes to the repository",
                    "External command metadata demonstrates imported completion knowledge.",
                    vec![
                        option(
                            &["-m", "--message"],
                            Some("message"),
                            "Use the given commit message",
                        ),
                        option(
                            &["-a", "--all"],
                            None,
                            "Stage modified and deleted tracked files",
                        ),
                        option(&["--amend"], None, "Replace the tip of the current branch"),
                        option(
                            &["--no-verify"],
                            None,
                            "Bypass pre-commit and commit-msg hooks",
                        ),
                    ],
                    &["git commit -m \"Explain the change\""],
                    &[Effect::WriteFilesystem, Effect::SpawnProcess],
                    Provenance::External,
                ),
                command(
                    "git status",
                    "git status [--short]",
                    "Show repository and working-tree status",
                    "External command metadata can eventually be imported from generated specs and help output.",
                    vec![option(
                        &["-s", "--short"],
                        None,
                        "Use the compact status format",
                    )],
                    &["git status --short"],
                    &[Effect::ReadFilesystem, Effect::SpawnProcess],
                    Provenance::External,
                ),
            ],
        };
        let ids = catalog
            .commands
            .iter()
            .map(|command| command.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for command in &mut catalog.commands {
            if command
                .parent
                .as_ref()
                .is_some_and(|parent| !ids.contains(parent))
            {
                command.parent = None;
            }
        }
        catalog
    }

    /// Complete command paths or options using a deterministic fuzzy subsequence score.
    ///
    /// A cursor past the input or inside a UTF-8 code point is clamped backward to
    /// the nearest valid boundary so malformed protocol input cannot create a panic.
    pub fn complete(&self, input: &str, cursor: usize) -> Vec<Completion> {
        let cursor = clamped_cursor(input, cursor);
        let before = &input[..cursor];
        let (segment_start, segment) = current_command_segment(before);
        let leading_whitespace = segment.len() - segment.trim_start().len();
        let query_start = segment_start + leading_whitespace;
        let query = segment.trim_start();

        if let Some((command, option, token_start, token, values)) =
            self.static_value_context(query, query_start)
        {
            let mut choices = values
                .iter()
                .filter_map(|value| {
                    fuzzy_match(token, value).map(|(score, indices)| {
                        (
                            score,
                            Completion {
                                value: value.clone(),
                                display: value.clone(),
                                summary: option.documentation.clone(),
                                detail: format!("{} · {}", command.signature, option.value_type),
                                replace_start: token_start,
                                replace_end: cursor,
                                match_indices: indices,
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();
            choices.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then_with(|| left.1.value.cmp(&right.1.value))
            });
            return choices.into_iter().map(|(_, item)| item).collect();
        }

        if let Some((command, token_start, token)) = self.option_context(query, query_start) {
            let mut choices = command
                .options
                .iter()
                .flat_map(|option| option.names.iter().map(move |name| (name, option)))
                .filter_map(|(name, option)| {
                    fuzzy_match(token, name).map(|(score, indices)| {
                        (
                            score,
                            Completion {
                                value: name.clone(),
                                display: match option.kind {
                                    ArgumentKind::Flag => name.clone(),
                                    _ => format!("{name} <{}>", option.value_type),
                                },
                                summary: option.documentation.clone(),
                                detail: command.signature.clone(),
                                replace_start: token_start,
                                replace_end: cursor,
                                match_indices: indices,
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();
            choices.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then_with(|| left.1.value.cmp(&right.1.value))
            });
            return choices.into_iter().map(|(_, item)| item).collect();
        }

        let mut choices = self
            .commands
            .iter()
            .flat_map(|command| {
                std::iter::once(&command.path)
                    .chain(command.aliases.iter())
                    .filter_map(move |candidate| {
                        fuzzy_match(query, candidate).map(|(score, indices)| {
                            (
                                score,
                                Completion {
                                    value: candidate.clone(),
                                    display: command.signature.clone(),
                                    summary: command.summary.clone(),
                                    detail: format!(
                                        "{} · {:?}",
                                        command.details, command.provenance
                                    ),
                                    replace_start: query_start,
                                    replace_end: cursor,
                                    match_indices: indices,
                                },
                            )
                        })
                    })
            })
            .collect::<Vec<_>>();
        choices.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.value.cmp(&right.1.value))
        });
        choices.into_iter().map(|(_, item)| item).collect()
    }

    /// Resolve a trimmed command path or alias, falling back to the first path prefix.
    ///
    /// Exact path and alias matches take precedence. The prefix fallback preserves
    /// catalog order and is intended for interactive help, not stable identity lookup.
    pub fn find(&self, topic: &str) -> Option<&CommandSpec> {
        let topic = topic.trim();
        self.commands
            .iter()
            .find(|command| {
                command.path == topic || command.aliases.iter().any(|alias| alias == topic)
            })
            .or_else(|| {
                self.commands
                    .iter()
                    .find(|command| command.path.starts_with(topic))
            })
    }

    /// Merge imported commands without discarding the provenance of individual
    /// options. Existing higher-confidence facts win deterministic ties.
    pub fn merge(&mut self, imported: impl IntoIterator<Item = CommandSpec>) {
        for mut incoming in imported {
            if let Some(existing) = self
                .commands
                .iter_mut()
                .find(|command| command.path == incoming.path)
            {
                if incoming.provenance.confidence > existing.provenance.confidence {
                    existing.id = incoming.id;
                    if incoming.version.is_some() {
                        existing.version = incoming.version;
                    }
                    if !incoming.aliases.is_empty() {
                        existing.aliases = incoming.aliases;
                    }
                    existing.parent = incoming.parent;
                    existing.signature = incoming.signature;
                    existing.summary = incoming.summary;
                    existing.details = incoming.details;
                    if incoming.io != IoContract::default() {
                        existing.io = incoming.io;
                    }
                    if !incoming.exit_codes.is_empty() {
                        existing.exit_codes = incoming.exit_codes;
                    }
                    existing.provenance = incoming.provenance;
                }
                for option in incoming.options.drain(..) {
                    merge_option(&mut existing.options, option);
                }
                existing.options.sort_by(|left, right| {
                    left.names
                        .first()
                        .cmp(&right.names.first())
                        .then_with(|| left.names.cmp(&right.names))
                });
            } else {
                incoming
                    .options
                    .sort_by(|left, right| left.names.cmp(&right.names));
                self.commands.push(incoming);
            }
        }
        self.commands
            .sort_by(|left, right| left.path.cmp(&right.path));
    }

    /// Merge all commands from an import report and return its diagnostics unchanged.
    ///
    /// Command facts follow [`Catalog::merge`] confidence rules; diagnostics remain
    /// available to the caller even when useful partial metadata was retained.
    pub fn merge_report(&mut self, report: ImportReport) -> Vec<ImportDiagnostic> {
        self.merge(report.commands);
        report.diagnostics
    }

    /// Return deterministic metadata-quality failures for exact command facts.
    /// Imported/heuristic records may retain explicit unknown fields without
    /// being promoted to exact knowledge.
    pub fn quality_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        let mut ids = BTreeMap::<&str, &str>::new();
        let known_ids = self
            .commands
            .iter()
            .filter(|command| !command.id.is_empty())
            .map(|command| command.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let mut names = BTreeMap::<&str, &str>::new();
        for command in &self.commands {
            if !command.id.is_empty()
                && let Some(previous) = ids.insert(&command.id, &command.path)
            {
                issues.push(format!(
                    "{} duplicates stable id {} from {previous}",
                    command.path, command.id
                ));
            }
            if command.provenance.confidence != Confidence::Exact {
                continue;
            }
            for (field, value) in [
                ("id", command.id.as_str()),
                ("path", command.path.as_str()),
                ("signature", command.signature.as_str()),
                ("summary", command.summary.as_str()),
                ("details", command.details.as_str()),
                ("io.input", command.io.input.as_str()),
                ("io.output", command.io.output.as_str()),
            ] {
                if value.trim().is_empty() || value == "Unknown" {
                    issues.push(format!("{} has incomplete {field}", command.path));
                }
            }
            if command.version.as_deref().is_none_or(str::is_empty) {
                issues.push(format!("{} has no declaring version", command.path));
            }
            if command.examples.is_empty() {
                issues.push(format!("{} has no examples", command.path));
            }
            if let Some(parent) = &command.parent
                && (parent == &command.id || !known_ids.contains(parent.as_str()))
            {
                issues.push(format!("{} has invalid parent {parent}", command.path));
            }
            for name in std::iter::once(&command.path).chain(command.aliases.iter()) {
                if name.trim().is_empty() {
                    issues.push(format!("{} has an empty alias", command.path));
                } else if let Some(previous) = names.insert(name, &command.path)
                    && previous != command.path.as_str()
                {
                    issues.push(format!(
                        "{} command name `{name}` conflicts with {previous}",
                        command.path
                    ));
                }
            }
            if command
                .aliases
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != command.aliases.len()
            {
                issues.push(format!("{} has duplicate aliases", command.path));
            }
            if command.exit_codes.is_empty()
                || command
                    .exit_codes
                    .values()
                    .any(|summary| summary.trim().is_empty())
            {
                issues.push(format!(
                    "{} has incomplete exit-code metadata",
                    command.path
                ));
            }
            let mut argument_names = std::collections::BTreeSet::new();
            for argument in &command.options {
                if argument.names.is_empty()
                    || argument.names.iter().any(|name| name.trim().is_empty())
                    || argument.value_type.trim().is_empty()
                    || argument.documentation.trim().is_empty()
                    || argument.examples.is_empty()
                {
                    issues.push(format!(
                        "{} has incomplete argument metadata for {:?}",
                        command.path, argument.names
                    ));
                }
                for name in &argument.names {
                    if !argument_names.insert(name) {
                        issues.push(format!("{} repeats argument name `{name}`", command.path));
                    }
                }
                for conflict in &argument.conflicts {
                    if argument.names.contains(conflict)
                        || !command
                            .options
                            .iter()
                            .any(|candidate| candidate.names.contains(conflict))
                    {
                        issues.push(format!(
                            "{} has invalid conflict `{conflict}` for {:?}",
                            command.path, argument.names
                        ));
                    }
                }
                match &argument.values {
                    Some(CompletionSource::Static { values }) => {
                        let unique = values.iter().collect::<std::collections::BTreeSet<_>>();
                        if values.is_empty()
                            || unique.len() != values.len()
                            || values.iter().any(|value| value.trim().is_empty())
                        {
                            issues.push(format!(
                                "{} has invalid static values for {:?}",
                                command.path, argument.names
                            ));
                        }
                    }
                    Some(CompletionSource::Dynamic { provider }) if provider.trim().is_empty() => {
                        issues.push(format!(
                            "{} has an empty dynamic provider for {:?}",
                            command.path, argument.names
                        ));
                    }
                    _ => {}
                }
            }
        }
        issues.sort();
        issues
    }

    /// Explain the source of every command-level and option-level fact currently
    /// retained in the catalog.
    pub fn explain(&self, path: &str) -> Option<CatalogExplanation> {
        let command = self.commands.iter().find(|command| command.path == path)?;
        let mut facts = vec![
            FactExplanation {
                fact: "command_id".to_owned(),
                value: command.id.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "command_path".to_owned(),
                value: command.path.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "signature".to_owned(),
                value: command.signature.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "summary".to_owned(),
                value: command.summary.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "details".to_owned(),
                value: command.details.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "io".to_owned(),
                value: format!(
                    "{} -> {} (streaming={})",
                    command.io.input, command.io.output, command.io.streaming
                ),
                provenance: command.provenance.clone(),
            },
        ];
        for example in &command.examples {
            facts.push(FactExplanation {
                fact: "example".to_owned(),
                value: example.clone(),
                provenance: command.provenance.clone(),
            });
        }
        for effect in &command.effects {
            facts.push(FactExplanation {
                fact: "effect".to_owned(),
                value: format!("{effect:?}"),
                provenance: command.provenance.clone(),
            });
        }
        for option in &command.options {
            facts.push(FactExplanation {
                fact: "option_names".to_owned(),
                value: option.names.join(", "),
                provenance: option.provenance.clone(),
            });
            if !matches!(option.kind, ArgumentKind::Flag) {
                facts.push(FactExplanation {
                    fact: "argument_type".to_owned(),
                    value: option.value_type.clone(),
                    provenance: option.provenance.clone(),
                });
            }
            facts.push(FactExplanation {
                fact: "argument_documentation".to_owned(),
                value: option.documentation.clone(),
                provenance: option.provenance.clone(),
            });
        }
        Some(CatalogExplanation {
            command: command.path.clone(),
            facts,
        })
    }

    /// Render command summaries, details, and argument documentation as Markdown.
    ///
    /// Commands retain catalog order. The returned text is not terminal-sanitized;
    /// terminal consumers must escape imported content at their output boundary.
    pub fn to_markdown(&self) -> String {
        let mut output = String::from("# Quirl command catalog\n\n");
        for command in &self.commands {
            output.push_str(&format!(
                "## `{}`\n\n{}\n\n",
                command.signature, command.summary
            ));
            output.push_str(&format!("{}\n\n", command.details));
            output.push_str(&format!(
                "- Input: `{}`\n- Output: `{}`\n- Live streaming: `{}`\n\n",
                command.io.input, command.io.output, command.io.streaming
            ));
            if !command.options.is_empty() {
                output.push_str("Options:\n\n");
                for option in &command.options {
                    output.push_str(&format!(
                        "- `{}` — {}\n",
                        option.names.join("`, `"),
                        option.documentation
                    ));
                }
                output.push('\n');
            }
        }
        output
    }

    fn option_context<'catalog, 'query>(
        &'catalog self,
        query: &'query str,
        leading_whitespace: usize,
    ) -> Option<(&'catalog CommandSpec, usize, &'query str)> {
        let token_start = query
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + whitespace_width_at(query, index));
        let token = &query[token_start..];
        if !token.starts_with('-') {
            return None;
        }
        let command_text = query[..token_start].trim_end();
        let command = self
            .commands
            .iter()
            .filter(|command| {
                command_text == command.path
                    || command_text.starts_with(&format!("{} ", command.path))
            })
            .max_by_key(|command| command.path.len())?;
        Some((command, leading_whitespace + token_start, token))
    }

    fn static_value_context<'catalog, 'query>(
        &'catalog self,
        query: &'query str,
        leading_whitespace: usize,
    ) -> Option<(
        &'catalog CommandSpec,
        &'catalog ArgumentSpec,
        usize,
        &'query str,
        &'catalog [String],
    )> {
        let token_start = query
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + whitespace_width_at(query, index));
        let token = &query[token_start..];
        let (option_name, value_query, replace_start, command_text) =
            if let Some((name, value)) = token.split_once('=') {
                (
                    name,
                    value,
                    leading_whitespace + token_start + name.len() + 1,
                    query[..token_start].trim_end(),
                )
            } else {
                let preceding = query[..token_start].trim_end();
                let option_start = preceding
                    .rfind(char::is_whitespace)
                    .map_or(0, |index| index + whitespace_width_at(preceding, index));
                (
                    &preceding[option_start..],
                    token,
                    leading_whitespace + token_start,
                    preceding[..option_start].trim_end(),
                )
            };
        if !option_name.starts_with('-') {
            return None;
        }
        let command = self
            .commands
            .iter()
            .filter(|command| {
                command_text == command.path
                    || command_text.starts_with(&format!("{} ", command.path))
            })
            .max_by_key(|command| command.path.len())?;
        let option = command
            .options
            .iter()
            .find(|option| option.names.iter().any(|name| name == option_name))?;
        let CompletionSource::Static { values } = option.values.as_ref()? else {
            return None;
        };
        Some((command, option, replace_start, value_query, values))
    }
}

fn unsupported_catalog_schema_error(found: &str) -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "catalog schema version {found} is unsupported; expected {CATALOG_OLDEST_READABLE_VERSION}..={CATALOG_SCHEMA_VERSION}",
        ),
    ))
}

fn clamped_cursor(input: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(input.len());
    while !input.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

fn current_command_segment(input: &str) -> (usize, &str) {
    #[derive(Clone, Copy)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let mut quote = Quote::None;
    let mut escaped = false;
    let mut segment_start = 0;

    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }

        match quote {
            Quote::Single => {
                if character == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => match character {
                '\\' => escaped = true,
                '"' => quote = Quote::None,
                _ => {}
            },
            Quote::None => match character {
                '\\' => escaped = true,
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '|' | '&' | ';' | '\n' => segment_start = index + character.len_utf8(),
                _ => {}
            },
        }
    }

    (segment_start, &input[segment_start..])
}

fn whitespace_width_at(input: &str, index: usize) -> usize {
    input[index..].chars().next().map_or(0, char::len_utf8)
}

fn fuzzy_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    let query = query.to_lowercase();
    let mut candidate_lower = String::new();
    let mut original_indices = Vec::new();
    for (original_index, character) in candidate.chars().enumerate() {
        for lowercase in character.to_lowercase() {
            candidate_lower.push(lowercase);
            original_indices.push(original_index);
        }
    }
    if query.is_empty() {
        return Some((0, vec![]));
    }
    if candidate_lower.starts_with(&query) {
        let mut indices = original_indices
            .iter()
            .take(query.chars().count())
            .copied()
            .collect::<Vec<_>>();
        indices.dedup();
        return Some((10_000 - candidate.len() as i32, indices));
    }

    let mut indices = Vec::new();
    let mut candidate_chars = candidate_lower.chars().enumerate();
    for wanted in query.chars() {
        let (folded_index, _) = candidate_chars.find(|(_, actual)| *actual == wanted)?;
        let original_index = *original_indices.get(folded_index)?;
        if indices.last() != Some(&original_index) {
            indices.push(original_index);
        }
    }
    let spread = indices.last().copied().unwrap_or_default() as i32;
    Some((1_000 - spread - candidate.len() as i32, indices))
}

fn option(names: &[&str], value: Option<&str>, summary: &str) -> OptionSpec {
    OptionSpec {
        names: names.iter().map(|name| (*name).to_owned()).collect(),
        kind: if value.is_some() {
            ArgumentKind::Option
        } else {
            ArgumentKind::Flag
        },
        value_type: value.unwrap_or("Bool").to_owned(),
        required: false,
        repeatable: false,
        values: value.and_then(static_values),
        conflicts: Vec::new(),
        documentation: summary.to_owned(),
        examples: Vec::new(),
        provenance: ProvenanceInfo::builtin(Provenance::Builtin),
    }
}

fn static_values(value: &str) -> Option<CompletionSource> {
    let values = value
        .split('|')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    (values.len() > 1).then_some(CompletionSource::Static { values })
}

fn option_with_static_values(
    names: &[&str],
    value_type: &str,
    values: &[&str],
    summary: &str,
) -> OptionSpec {
    let mut option = option(names, Some(value_type), summary);
    option.values = Some(CompletionSource::Static {
        values: values.iter().map(|value| (*value).to_owned()).collect(),
    });
    option
}

fn repeatable_option(names: &[&str], value_type: &str, summary: &str) -> OptionSpec {
    let mut option = option(names, Some(value_type), summary);
    option.repeatable = true;
    option
}

fn required_option(names: &[&str], value_type: &str, summary: &str) -> OptionSpec {
    let mut option = option(names, Some(value_type), summary);
    option.required = true;
    option
}

fn required_repeatable_option_with_static_values(
    names: &[&str],
    value_type: &str,
    values: &[&str],
    summary: &str,
) -> OptionSpec {
    let mut option = option_with_static_values(names, value_type, values, summary);
    option.required = true;
    option.repeatable = true;
    option
}

fn merge_option(options: &mut Vec<OptionSpec>, incoming: OptionSpec) {
    let duplicate = options.iter_mut().find(|existing| {
        existing
            .names
            .iter()
            .any(|name| incoming.names.iter().any(|candidate| candidate == name))
    });
    if let Some(existing) = duplicate {
        for name in incoming.names {
            if !existing.names.contains(&name) {
                existing.names.push(name);
            }
        }
        existing.names.sort();
        if incoming.provenance.confidence > existing.provenance.confidence {
            existing.kind = incoming.kind;
            existing.value_type = incoming.value_type;
            existing.required = incoming.required;
            existing.repeatable = incoming.repeatable;
            existing.values = incoming.values;
            existing.conflicts = incoming.conflicts;
            existing.documentation = incoming.documentation;
            existing.examples = incoming.examples;
            existing.provenance = incoming.provenance;
        }
    } else {
        options.push(incoming);
    }
}

#[allow(clippy::too_many_arguments)]
fn command(
    path: &str,
    signature: &str,
    summary: &str,
    details: &str,
    options: Vec<OptionSpec>,
    examples: &[&str],
    effects: &[Effect],
    provenance: Provenance,
) -> CommandSpec {
    let provenance = ProvenanceInfo::builtin(provenance);
    let mut options = options
        .into_iter()
        .map(|mut option| {
            option.provenance = provenance.clone();
            if option.examples.is_empty() {
                option.examples = examples
                    .iter()
                    .map(|example| (*example).to_owned())
                    .collect();
            }
            option
        })
        .collect::<Vec<_>>();
    options.extend(positional_arguments(path, signature, examples, &provenance));
    options.sort_by(|left, right| {
        argument_kind_key(left.kind)
            .cmp(&argument_kind_key(right.kind))
            .then_with(|| left.names.cmp(&right.names))
    });
    let exact = matches!(
        provenance.source,
        Provenance::Builtin | Provenance::Lua | Provenance::Plugin
    );
    CommandSpec {
        id: command_id(path),
        version: exact.then(|| env!("CARGO_PKG_VERSION").to_owned()),
        path: path.to_owned(),
        aliases: Vec::new(),
        parent: path.rsplit_once(' ').map(|(parent, _)| command_id(parent)),
        signature: signature.to_owned(),
        summary: summary.to_owned(),
        details: details.to_owned(),
        options,
        examples: examples
            .iter()
            .map(|example| (*example).to_owned())
            .collect(),
        effects: effects.to_vec(),
        io: builtin_io(path),
        exit_codes: if exact {
            BTreeMap::from([
                (0, "completed successfully".to_owned()),
                (1, "reported a command failure".to_owned()),
            ])
        } else {
            BTreeMap::new()
        },
        provenance,
    }
}

fn command_id(path: &str) -> String {
    format!(
        "command:{}",
        path.split_whitespace().collect::<Vec<_>>().join("/")
    )
}

fn builtin_io(path: &str) -> IoContract {
    match path {
        "quirl data" => IoContract {
            input: "Nothing".to_owned(),
            output: "Value".to_owned(),
            streaming: true,
        },
        "quirl data ls" => IoContract {
            input: "Nothing".to_owned(),
            output: "Stream<Entry>".to_owned(),
            streaming: true,
        },
        "quirl watch" => IoContract {
            input: "Nothing".to_owned(),
            output: "Stream<Value>".to_owned(),
            streaming: true,
        },
        _ => IoContract {
            input: "Nothing".to_owned(),
            output: "Bytes".to_owned(),
            streaming: false,
        },
    }
}

fn positional_arguments(
    path: &str,
    signature: &str,
    examples: &[&str],
    provenance: &ProvenanceInfo,
) -> Vec<ArgumentSpec> {
    let mut declared = provenance.clone();
    if declared.confidence == Confidence::Exact {
        declared.confidence = Confidence::High;
        declared.trust = Trust::Declared;
    }
    let path_parts = path.split_whitespace().count();
    signature
        .split_whitespace()
        .skip(path_parts)
        .filter_map(|token| positional_argument(token, examples, &declared))
        .collect()
}

fn positional_argument(
    token: &str,
    examples: &[&str],
    provenance: &ProvenanceInfo,
) -> Option<ArgumentSpec> {
    if token.starts_with('-') || token.starts_with("[-") || matches!(token, "[options]" | "[|") {
        return None;
    }
    let required = token.starts_with('<');
    let optional = token.starts_with('[');
    if !required && !optional {
        return None;
    }
    let value = token
        .trim_matches(|character| matches!(character, '<' | '>' | '[' | ']' | ','))
        .trim_end_matches("...");
    if value.is_empty() || value == "options" || value.starts_with('-') {
        return None;
    }
    Some(ArgumentSpec {
        names: vec![value.to_owned()],
        kind: ArgumentKind::Positional,
        value_type: value.to_owned(),
        required,
        repeatable: token.contains("..."),
        values: None,
        conflicts: Vec::new(),
        documentation: format!("Positional `{value}` declared by the builtin command signature."),
        examples: examples
            .iter()
            .map(|example| (*example).to_owned())
            .collect(),
        provenance: provenance.clone(),
    })
}

fn argument_kind_key(kind: ArgumentKind) -> u8 {
    match kind {
        ArgumentKind::Positional => 0,
        ArgumentKind::Option => 1,
        ArgumentKind::Flag => 2,
    }
}

pub(crate) fn imported_argument(
    names: Vec<String>,
    value: Option<String>,
    documentation: String,
    provenance: ProvenanceInfo,
) -> ArgumentSpec {
    ArgumentSpec {
        names,
        kind: if value.is_some() {
            ArgumentKind::Option
        } else {
            ArgumentKind::Flag
        },
        value_type: value.unwrap_or_else(|| "Bool".to_owned()),
        required: false,
        repeatable: false,
        values: None,
        conflicts: Vec::new(),
        documentation,
        examples: Vec::new(),
        provenance,
    }
}

pub(crate) fn imported_command(
    path: String,
    signature: String,
    summary: String,
    details: String,
    options: Vec<ArgumentSpec>,
    provenance: ProvenanceInfo,
) -> CommandSpec {
    CommandSpec {
        id: command_id(&path),
        version: None,
        aliases: Vec::new(),
        parent: path.rsplit_once(' ').map(|(parent, _)| command_id(parent)),
        path,
        signature,
        summary,
        details,
        options,
        examples: Vec::new(),
        io: IoContract::default(),
        effects: vec![Effect::SpawnProcess],
        exit_codes: BTreeMap::new(),
        provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_command_completion_discovers_subcommands() {
        let completions = Catalog::builtin().complete("git c", 5);
        assert_eq!(completions[0].value, "git commit");
        assert!(completions[0].summary.contains("Record"));
    }

    #[test]
    fn option_completion_uses_command_context() {
        let completions = Catalog::builtin().complete("git commit --am", 15);
        assert_eq!(completions[0].value, "--amend");
        assert_eq!(completions[0].replace_start, 11);
    }

    #[test]
    fn command_and_option_completion_use_the_current_command_segment() {
        for separator in ["|", "||", "&&", ";", "&", "\n"] {
            let command_line = format!("echo prior {separator} git c");
            let commands = Catalog::builtin().complete(&command_line, command_line.len());
            assert_eq!(commands[0].value, "git commit", "separator {separator:?}");
            assert_eq!(
                commands[0].replace_start,
                command_line.len() - "git c".len(),
                "separator {separator:?}"
            );

            let option_line = format!("echo prior {separator} git commit --am");
            let options = Catalog::builtin().complete(&option_line, option_line.len());
            assert_eq!(options[0].value, "--amend", "separator {separator:?}");
            assert_eq!(
                options[0].replace_start,
                option_line.len() - "--am".len(),
                "separator {separator:?}"
            );
        }
    }

    #[test]
    fn quoted_separators_do_not_start_a_command_segment() {
        for separator in ["|", "||", "&&", ";", "&", "\n"] {
            for quote in ['\'', '"'] {
                let line = format!("git commit {quote}left{separator}right{quote} --am");
                let completions = Catalog::builtin().complete(&line, line.len());
                assert_eq!(
                    completions[0].value, "--amend",
                    "separator {separator:?} in {quote:?} quotes"
                );
                assert_eq!(completions[0].replace_start, line.len() - "--am".len());
            }
        }
    }

    #[test]
    fn escaped_separator_bytes_do_not_start_a_command_segment() {
        for separator in ["|", "||", "&&", ";", "&", "\n"] {
            let escaped_separator = separator
                .chars()
                .map(|character| format!("\\{character}"))
                .collect::<String>();
            let line = format!("git commit left{escaped_separator}right --am");
            let completions = Catalog::builtin().complete(&line, line.len());
            assert_eq!(
                completions[0].value, "--amend",
                "escaped separator {separator:?}"
            );
            assert_eq!(completions[0].replace_start, line.len() - "--am".len());
        }
    }

    #[test]
    fn chained_completion_spans_remain_absolute_utf8_byte_offsets() {
        let command_line = "echo café | git c";
        let commands = Catalog::builtin().complete(command_line, command_line.len());
        assert_eq!(commands[0].value, "git commit");
        assert_eq!(commands[0].replace_start, "echo café | ".len());
        assert_eq!(commands[0].replace_end, command_line.len());

        let option_line = "echo 東京 && git commit --am";
        let options = Catalog::builtin().complete(option_line, option_line.len());
        assert_eq!(options[0].value, "--amend");
        assert_eq!(options[0].replace_start, "echo 東京 && git commit ".len());
        assert_eq!(options[0].replace_end, option_line.len());

        let value_line = "echo 🌀; quirl index build --format j";
        let values = Catalog::builtin().complete(value_line, value_line.len());
        assert_eq!(values[0].value, "json");
        assert_eq!(
            values[0].replace_start,
            "echo 🌀; quirl index build --format ".len()
        );
        assert_eq!(values[0].replace_end, value_line.len());
    }

    #[test]
    fn typed_ls_is_qualified_to_data_mode_and_does_not_shadow_path_ls() {
        let catalog = Catalog::builtin();
        assert!(catalog.find("ls").is_none());
        let command = catalog.find("quirl data ls").unwrap();
        assert_eq!(command.parent.as_deref(), Some("command:quirl/data"));
        assert_eq!(command.io.output, "Stream<Entry>");
        assert_eq!(command.options.len(), 1);
        assert_eq!(command.options[0].names, ["path"]);
        assert_eq!(command.options[0].kind, ArgumentKind::Positional);
        assert!(
            command
                .examples
                .iter()
                .any(|example| example.starts_with("ls . |"))
        );
    }

    #[test]
    fn static_argument_values_are_completed_after_space_or_equals() {
        let spaced = Catalog::builtin().complete("quirl index build --format j", 28);
        assert_eq!(spaced[0].value, "json");
        assert_eq!(spaced[0].replace_start, 27);
        let equals = Catalog::builtin().complete("quirl index build --format=j", 28);
        assert_eq!(equals[0].value, "json");
        assert_eq!(equals[0].replace_start, 27);
    }

    #[test]
    fn unicode_whitespace_preserves_completion_spans_in_every_context() {
        for whitespace in ['\u{00a0}', '\u{3000}', '\u{2003}'] {
            let command_line = format!("{whitespace}git c");
            let commands = Catalog::builtin().complete(&command_line, command_line.len());
            assert_eq!(commands[0].value, "git commit");
            assert_eq!(commands[0].replace_start, whitespace.len_utf8());

            let option_line = format!("git commit{whitespace}--am");
            let options = Catalog::builtin().complete(&option_line, option_line.len());
            assert_eq!(options[0].value, "--amend");
            assert_eq!(
                options[0].replace_start,
                "git commit".len() + whitespace.len_utf8()
            );

            let value_line = format!("quirl index build --format{whitespace}j");
            let values = Catalog::builtin().complete(&value_line, value_line.len());
            assert_eq!(values[0].value, "json");
            assert_eq!(
                values[0].replace_start,
                "quirl index build --format".len() + whitespace.len_utf8()
            );
        }
    }

    #[test]
    fn malformed_protocol_cursor_clamps_before_a_partial_code_point() {
        let line = "git \u{00e9}";
        let inside_e_acute = line.len() - 1;
        let completions = Catalog::builtin().complete(line, inside_e_acute);
        assert!(!completions.is_empty());
        assert!(
            completions
                .iter()
                .all(|completion| completion.replace_end == "git ".len())
        );
    }

    #[test]
    fn fuzzy_unicode_indices_address_the_original_display_characters() {
        assert_eq!(fuzzy_match("i", "\u{0130}").unwrap().1, [0]);
        assert_eq!(fuzzy_match("x", "\u{0130}x").unwrap().1, [1]);
    }

    #[test]
    fn catalog_is_machine_readable() {
        let json = serde_json::to_string(&Catalog::builtin()).unwrap();
        assert!(json.contains("\"schema_version\":4"));
        assert!(json.contains("\"arguments\""));
        assert!(json.contains("confidence"));
        assert!(json.contains("git commit"));
    }

    #[test]
    fn every_exact_builtin_has_complete_semantic_metadata() {
        let issues = Catalog::builtin().quality_issues();
        assert!(issues.is_empty(), "catalog quality failures: {issues:#?}");
    }

    #[test]
    fn quality_audit_rejects_invalid_relations_aliases_conflicts_and_values() {
        let mut catalog = Catalog::builtin();
        let command = catalog
            .commands
            .iter_mut()
            .find(|command| command.path == "quirl index build")
            .unwrap();
        command.parent = Some("command:missing".to_owned());
        command.aliases = vec!["index-build".to_owned(), "index-build".to_owned()];
        let argument = command
            .options
            .iter_mut()
            .find(|argument| argument.names == ["--format"])
            .unwrap();
        argument.conflicts = vec!["--missing".to_owned()];
        argument.values = Some(CompletionSource::Static {
            values: vec!["json".to_owned(), "json".to_owned()],
        });
        let issues = catalog.quality_issues().join("\n");
        assert!(issues.contains("invalid parent"));
        assert!(issues.contains("duplicate aliases"));
        assert!(issues.contains("invalid conflict"));
        assert!(issues.contains("invalid static values"));
    }

    #[test]
    fn legacy_catalogs_migrate_without_fabricating_exact_facts() {
        let provenance = ProvenanceInfo::imported(
            Provenance::Fish,
            Confidence::High,
            "demo.fish",
            "sha256:demo",
        );
        let source = serde_json::json!({
            "commands": [{
                "path": "demo",
                "signature": "demo [--output FILE]",
                "summary": "Imported demo",
                "details": "Imported declarative completion metadata.",
                "options": [{
                    "names": ["--output"],
                    "value": "FILE",
                    "summary": "Write output",
                    "provenance": provenance,
                }],
                "examples": [],
                "effects": ["spawn_process"],
                "provenance": provenance,
            }]
        });
        for schema_version in [2, 3] {
            let mut source = source.clone();
            source["schema_version"] = serde_json::json!(schema_version);
            let migrated = Catalog::from_json(&source.to_string()).unwrap();
            let command = migrated.find("demo").unwrap();
            assert_eq!(migrated.schema_version, CATALOG_SCHEMA_VERSION);
            assert_eq!(command.id, "command:demo");
            assert_eq!(command.version, None);
            assert_eq!(command.io, IoContract::default());
            assert_eq!(command.provenance.confidence, Confidence::High);
            assert_eq!(command.options[0].value_type, "FILE");
            assert_eq!(command.options[0].documentation, "Write output");
            assert!(command.options[0].examples.is_empty());
        }
    }

    #[test]
    fn catalog_reader_fails_closed_for_expired_future_or_unversioned_documents() {
        for source in [
            serde_json::json!({"schema_version": 1, "commands": []}),
            serde_json::json!({"schema_version": 5, "commands": []}),
            serde_json::json!({"commands": []}),
        ] {
            let error = Catalog::from_json(&source.to_string()).unwrap_err();
            assert!(error.to_string().contains("unsupported"));
        }
    }

    #[test]
    fn language_service_is_discoverable_from_the_catalog() {
        let catalog = Catalog::builtin();
        let command = catalog.find("quirl lsp").unwrap();
        assert!(command.details.contains("without evaluating documents"));
        assert_eq!(command.provenance.source, Provenance::Builtin);
    }

    #[test]
    fn imported_system_ls_owns_the_unqualified_normal_mode_contract() {
        let mut catalog = Catalog::builtin();
        let diagnostics = catalog.merge_report(import_fish(
            "complete -c ls -l color -d 'Colorize output'",
            "ls.fish",
        ));
        assert!(diagnostics.is_empty());
        let command = catalog.find("ls").unwrap();
        assert_eq!(command.provenance.source, Provenance::Fish);
        let color = command
            .options
            .iter()
            .find(|option| option.names.contains(&"--color".to_owned()))
            .unwrap();
        assert_eq!(color.provenance.source, Provenance::Fish);
        assert!(catalog.find("quirl data ls").is_some());
    }

    #[test]
    fn explain_attributes_each_retained_fact() {
        let mut catalog = Catalog::builtin();
        catalog.merge_report(import_bash(
            "complete -W '--frozen --locked' cargo",
            "cargo.bash",
        ));
        let explanation = catalog.explain("cargo").unwrap();
        assert!(
            explanation
                .facts
                .iter()
                .any(|fact| fact.value == "--frozen" && fact.provenance.source == Provenance::Bash)
        );
        assert!(explanation.facts.iter().all(|fact| !fact.value.is_empty()));
    }

    #[test]
    fn agent_and_package_surfaces_have_complete_catalog_metadata() {
        let catalog = Catalog::builtin();
        for path in [
            "quirl agent catalog",
            "quirl agent context",
            "quirl agent manifest",
            "quirl agent validate",
            "quirl package manifest",
            "quirl package build",
            "quirl package publish",
        ] {
            let command = catalog
                .commands
                .iter()
                .find(|command| command.path == path)
                .unwrap();
            assert!(!command.signature.is_empty(), "{path}");
            assert!(!command.summary.is_empty(), "{path}");
            assert!(!command.details.is_empty(), "{path}");
            assert!(!command.examples.is_empty(), "{path}");
            assert_eq!(command.provenance.confidence, Confidence::Exact);
        }
    }

    #[test]
    fn async_completion_envelopes_reject_unknown_fields() {
        let request = r#"{"protocol_version":1,"request_id":1,"line":"git c","cursor":5,"limit":10,"deadline_ms":25,"future":true}"#;
        assert!(serde_json::from_str::<CompletionRequest>(request).is_err());
        let response = r#"{"protocol_version":1,"request_id":1,"outcome":{"status":"cancelled"},"future":true}"#;
        assert!(serde_json::from_str::<CompletionResponse>(response).is_err());
    }

    #[test]
    fn completion_descriptor_matches_the_tagged_serialized_outcome() {
        let response = CompletionResponse {
            protocol_version: 1,
            request_id: 9,
            outcome: CompletionOutcome::Ready { items: Vec::new() },
        };
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["outcome"]["status"], "ready");
        assert_eq!(value["outcome"]["data"]["items"], serde_json::json!([]));
        assert!(COMPLETION_SCHEMA_DESCRIPTOR.contains("tag(status);content(data)"));
    }
}
