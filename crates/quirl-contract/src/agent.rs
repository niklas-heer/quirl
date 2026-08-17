use crate::package::package_manifest_schema_hash;
use crate::stable_hash;
use quirl_catalog::{
    ArgumentKind, Catalog, CommandSpec, CompletionSource, Confidence, Effect, IoContract,
    Provenance, Trust,
};
use quirl_core::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};

/// Current version of every agent-facing JSON document owned by this crate.
pub const AGENT_SCHEMA_VERSION: u32 = 2;
/// Maximum bytes accepted by an agent-document validation entry point.
pub const AGENT_DOCUMENT_BYTES_MAX: usize = 4 * 1024 * 1024;
/// Default maximum estimated token count used when selecting agent context.
pub const DEFAULT_TOKEN_BUDGET: usize = 6_000;
/// Smallest accepted context budget, below which useful schema context cannot fit.
pub const MINIMUM_TOKEN_BUDGET: usize = 64;

const PROVENANCE_SCHEMA: &str = "AgentProvenance{deny_unknown;source:enum[builtin,external,lua,plugin,fish,bash,zsh,help,man];confidence:enum[low,medium,high,exact];trust:enum[builtin,trusted,declared,imported,heuristic];origin:null|string;fingerprint:null|string;generated_at:null|string}";
const OPTION_SCHEMA: &str = "AgentOption{deny_unknown;names:array<string>;kind:enum[positional,option,flag];value_type:string;required:bool;repeatable:bool;values:null|CompletionSource;conflicts:array<string>;documentation:string;examples:array<string>;provenance:AgentProvenance}";
const COMPLETION_SCHEMA: &str =
    "CompletionSource{deny_unknown;tag=kind;static{values:array<string>};dynamic{provider:string}}";
const IO_SCHEMA: &str = "IoContract{deny_unknown;input:string;output:string;streaming:bool}";
const COMMAND_SCHEMA: &str = "AgentCommand{deny_unknown;id:string;version:null|string;path:string;aliases:array<string>;parent:null|string;signature:string;summary:string;details:string;options:array<AgentOption>;examples:array<string>;io:IoContract;effects:array<enum[read_filesystem,write_filesystem,spawn_process,change_directory]>;exit_codes:map<i32,string>;provenance:AgentProvenance}";
const HOST_SCHEMA: &str = "HostCapability{deny_unknown;path:string;summary:string;parameters:array<HostParameter{deny_unknown;name:string;value_type:string}>;returns:string;capability:null|string}";
const CAPABILITY_SCHEMA: &str = "InstalledCapability{deny_unknown;name:string;version:u32;schema_hash:string;providers:array<string>}";
/// Canonical structural description used to fingerprint [`AgentCatalog`] documents.
pub const AGENT_CATALOG_SCHEMA_DESCRIPTOR: &str = "AgentCatalog{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;quirl_version:string;catalog_schema_version:u32;catalog_hash:string;host_api_schema_version:u32;host_api_hash:string;commands:array<AgentCommand>;host_api:array<HostCapability>;capabilities:array<InstalledCapability>}";
/// Canonical structural description used to fingerprint [`AgentContext`] documents.
pub const AGENT_CONTEXT_SCHEMA_DESCRIPTOR: &str = "AgentContext{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;query:string;token_budget:u64;estimated_tokens:u64;token_estimator:string;truncated:bool;catalog_hash:string;host_api_hash:string;commands:array<AgentCommand>;host_api:array<HostCapability>}";
/// Historical context shape emitted by agent schema version 1.
pub const AGENT_CONTEXT_SCHEMA_V1_DESCRIPTOR: &str = "AgentContext{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;query:string;token_budget:usize;estimated_tokens:usize;token_estimator:string;truncated:bool;catalog_hash:string;host_api_hash:string;commands:array<AgentCommand>;host_api:array<HostCapability>}";
const MANIFEST_COMPONENT_SCHEMA: &str = "AgentManifestComponents{SchemaDescriptor{deny_unknown;name:string;version:u32;schema_hash:string;content_hash:string};AgentTool{deny_unknown;name:string;version:string;summary:string;effects:array<Effect>};AgentValidator{deny_unknown;name:string;command:string;schema_version:u32;schema_hash:string}}";
/// Canonical structural description used to fingerprint [`AgentManifest`] documents.
pub const AGENT_MANIFEST_SCHEMA_DESCRIPTOR: &str = "AgentManifest{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;content_hash:string;quirl_version:string;schemas:array<SchemaDescriptor>;capabilities:array<InstalledCapability>;tools:array<AgentTool>;validators:array<AgentValidator>}";

/// Computes the structural schema identity expected in an [`AgentCatalog`].
pub fn agent_catalog_schema_hash() -> String {
    structural_schema_hash(&[
        AGENT_CATALOG_SCHEMA_DESCRIPTOR,
        COMMAND_SCHEMA,
        OPTION_SCHEMA,
        COMPLETION_SCHEMA,
        IO_SCHEMA,
        PROVENANCE_SCHEMA,
        HOST_SCHEMA,
        CAPABILITY_SCHEMA,
    ])
}

/// Computes the structural schema identity expected in an [`AgentContext`].
pub fn agent_context_schema_hash() -> String {
    structural_schema_hash(&[
        AGENT_CONTEXT_SCHEMA_DESCRIPTOR,
        COMMAND_SCHEMA,
        OPTION_SCHEMA,
        COMPLETION_SCHEMA,
        IO_SCHEMA,
        PROVENANCE_SCHEMA,
        HOST_SCHEMA,
    ])
}

/// Computes the structural schema identity expected in an [`AgentManifest`].
pub fn agent_manifest_schema_hash() -> String {
    structural_schema_hash(&[
        AGENT_MANIFEST_SCHEMA_DESCRIPTOR,
        MANIFEST_COMPONENT_SCHEMA,
        CAPABILITY_SCHEMA,
    ])
}

fn structural_schema_hash(parts: &[&str]) -> String {
    let mut descriptor = Vec::new();
    for part in parts {
        descriptor.extend_from_slice(part.as_bytes());
        descriptor.push(0x1f);
    }
    stable_hash(&descriptor)
}

/// Describes one typed input accepted by a Lua host API function.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostParameter {
    /// Parameter name exposed to Lua and agent tooling.
    pub name: String,
    /// Stable, human-readable value type used by generated documentation.
    pub value_type: String,
}

/// Machine-readable description of one callable Lua host API function.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCapability {
    /// Dot-separated host API path, such as `quirl.fs.read`.
    pub path: String,
    /// Concise description suitable for context selection and tool discovery.
    pub summary: String,
    /// Ordered parameters in the function's call contract.
    pub parameters: Vec<HostParameter>,
    /// Stable, human-readable description of the returned value.
    pub returns: String,
    /// Authority that must be granted before the host function may be used.
    pub capability: Option<String>,
}

/// Agent-facing argument metadata for a command-line option or positional value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentOption {
    /// Accepted spellings, with the canonical or positional name first.
    pub names: Vec<String>,
    /// Whether the argument is positional, value-taking, or a flag.
    pub kind: ArgumentKind,
    /// Stable, human-readable type of the accepted value.
    pub value_type: String,
    /// Whether omission makes the invocation invalid.
    pub required: bool,
    /// Whether the argument may occur more than once.
    pub repeatable: bool,
    /// Optional source from which valid or suggested values can be completed.
    pub values: Option<CompletionSource>,
    /// Other option names that cannot be used in the same invocation.
    pub conflicts: Vec<String>,
    /// Detailed usage guidance beyond the containing command's summary.
    pub documentation: String,
    /// Copyable examples focused on this argument.
    pub examples: Vec<String>,
    /// Origin and reliability metadata for these facts.
    pub provenance: AgentProvenance,
}

/// Records where a machine-contract fact came from and how strongly to trust it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentProvenance {
    /// Mechanism or source class that supplied the fact.
    pub source: Provenance,
    /// Estimated certainty that the fact accurately describes the command.
    pub confidence: Confidence,
    /// Trust classification applied before exposing the fact to an agent.
    pub trust: Trust,
    /// Optional source-specific location, command, or file identifier.
    pub origin: Option<String>,
    /// Optional source fingerprint for freshness and drift detection.
    pub fingerprint: Option<String>,
    /// Optional generation timestamp supplied by the source.
    pub generated_at: Option<String>,
}

/// Complete machine-readable invocation contract for one command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentCommand {
    /// Stable catalog identifier, independent of presentation ordering.
    pub id: String,
    /// Optional version of an external command or provider.
    pub version: Option<String>,
    /// Canonical space-separated command path used to invoke the command.
    pub path: String,
    /// Alternate invocations accepted for the same command.
    pub aliases: Vec<String>,
    /// Parent command path for hierarchical command catalogs.
    pub parent: Option<String>,
    /// Compact invocation grammar intended for display and planning.
    pub signature: String,
    /// One-line description used in discovery and relevance ranking.
    pub summary: String,
    /// Extended behavioral contract and usage guidance.
    pub details: String,
    /// Typed positional arguments, options, and flags.
    pub options: Vec<AgentOption>,
    /// Copyable examples of valid invocations.
    pub examples: Vec<String>,
    /// Typed pipeline input, output, and streaming behavior.
    pub io: IoContract,
    /// Observable side effects an agent must account for before invocation.
    pub effects: Vec<Effect>,
    /// Process exit statuses mapped to their stable meanings.
    pub exit_codes: BTreeMap<i32, String>,
    /// Origin and reliability metadata for the command contract.
    pub provenance: AgentProvenance,
}

/// Installed authority together with the host functions that provide it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledCapability {
    /// Stable capability identifier referenced by package requests.
    pub name: String,
    /// Version of the capability contract, not of an individual provider.
    pub version: u32,
    /// Structural/content identity of the sorted provider set.
    pub schema_hash: String,
    /// Sorted host API paths available under this authority.
    pub providers: Vec<String>,
}

/// Deterministic snapshot of commands and Lua host functions available to agents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentCatalog {
    /// Discriminator; valid catalogs use `quirl.agent.catalog`.
    pub document_type: String,
    /// Version governing this outer document contract.
    pub schema_version: u32,
    /// Structural identity computed by [`agent_catalog_schema_hash`].
    pub schema_hash: String,
    /// Quirl release that produced the snapshot.
    pub quirl_version: String,
    /// Version of the source command catalog schema.
    pub catalog_schema_version: u32,
    /// Content identity of the normalized command list.
    pub catalog_hash: String,
    /// Version of the adapted Lua host API contract.
    pub host_api_schema_version: u32,
    /// Content identity of the normalized host API list.
    pub host_api_hash: String,
    /// Commands sorted into deterministic canonical order.
    pub commands: Vec<AgentCommand>,
    /// Lua host functions sorted by path.
    pub host_api: Vec<HostCapability>,
    /// Authorities derived from the installed host API.
    pub capabilities: Vec<InstalledCapability>,
}

/// Relevance-selected subset of an [`AgentCatalog`] bounded by an estimated token budget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentContext {
    /// Discriminator; valid context documents use `quirl.agent.context`.
    pub document_type: String,
    /// Version governing this outer document contract.
    pub schema_version: u32,
    /// Structural identity computed by [`agent_context_schema_hash`].
    pub schema_hash: String,
    /// Original query used to rank catalog entries.
    pub query: String,
    /// Maximum token estimate requested by the caller.
    pub token_budget: u64,
    /// Estimate for the selected query and payload, never above the budget.
    pub estimated_tokens: u64,
    /// Identifier for the estimation algorithm so consumers can interpret the bound.
    pub token_estimator: String,
    /// Whether otherwise-relevant entries were omitted to respect the budget.
    pub truncated: bool,
    /// Source catalog content identity used as a freshness anchor.
    pub catalog_hash: String,
    /// Source host API content identity used as a freshness anchor.
    pub host_api_hash: String,
    /// Selected command contracts in deterministic path order.
    pub commands: Vec<AgentCommand>,
    /// Selected host functions in deterministic path order.
    pub host_api: Vec<HostCapability>,
}

/// Identifies a versioned schema and the installed content represented through it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchemaDescriptor {
    /// Stable document-type name.
    pub name: String,
    /// Version of the named document contract.
    pub version: u32,
    /// Structural identity of the schema.
    pub schema_hash: String,
    /// Identity of the installed content available under the schema.
    pub content_hash: String,
}

/// Command exposed to an agent as an executable Quirl tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentTool {
    /// Canonical command path used for invocation.
    pub name: String,
    /// Quirl version providing the command.
    pub version: String,
    /// Concise behavioral description for tool discovery.
    pub summary: String,
    /// Observable effects requiring planning or confirmation.
    pub effects: Vec<Effect>,
}

/// Validator an agent can invoke to check a generated machine document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentValidator {
    /// Stable document type accepted by the validator.
    pub name: String,
    /// Copyable Quirl command that performs validation.
    pub command: String,
    /// Supported version of the validated document.
    pub schema_version: u32,
    /// Structural identity expected by the validator.
    pub schema_hash: String,
}

/// Compact discovery document for schemas, capabilities, tools, and validators.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentManifest {
    /// Discriminator; valid manifests use `quirl.agent.manifest`.
    pub document_type: String,
    /// Version governing this outer document contract.
    pub schema_version: u32,
    /// Structural identity computed by [`agent_manifest_schema_hash`].
    pub schema_hash: String,
    /// Identity of the manifest payload for freshness checks.
    pub content_hash: String,
    /// Quirl release whose installed surface is described.
    pub quirl_version: String,
    /// Machine schemas agents may request or validate.
    pub schemas: Vec<SchemaDescriptor>,
    /// Installed authorities and their provider functions.
    pub capabilities: Vec<InstalledCapability>,
    /// Quirl commands suitable for agent tool invocation.
    pub tools: Vec<AgentTool>,
    /// Commands that validate generated contract documents.
    pub validators: Vec<AgentValidator>,
}

/// Selects the typed schema used to validate an agent JSON document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDocumentKind {
    /// A complete installed command and host API snapshot.
    Catalog,
    /// A query-focused, token-bounded catalog subset.
    Context,
    /// A compact discovery manifest for the installed agent surface.
    Manifest,
}

/// Trusted installed-content identities used to reject stale agent documents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentValidationAnchors {
    /// Expected normalized command catalog identity.
    pub catalog_hash: String,
    /// Expected normalized Lua host API identity.
    pub host_api_hash: String,
}

impl From<&AgentCatalog> for AgentValidationAnchors {
    fn from(catalog: &AgentCatalog) -> Self {
        Self {
            catalog_hash: catalog.catalog_hash.clone(),
            host_api_hash: catalog.host_api_hash.clone(),
        }
    }
}

/// Importance of a validation finding when deciding whether a document is usable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    /// Contract violation that makes the document invalid.
    Error,
    /// Non-fatal condition that callers should review.
    Warning,
}

/// Structured, path-addressable explanation of one contract validation finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationDiagnostic {
    /// Stable machine-readable identifier for the validation rule.
    pub code: String,
    /// Whether the finding invalidates the document.
    pub severity: DiagnosticSeverity,
    /// Human-readable description of the observed problem.
    pub message: String,
    /// Logical field path associated with the finding.
    pub path: String,
    /// Actionable guidance for correcting the document.
    pub help: String,
}

/// Complete deterministic result of validating a machine-contract document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationReport {
    /// Type of validation result, distinct from the input document discriminator.
    pub document_type: String,
    /// Contract version used by the validator.
    pub schema_version: u32,
    /// `true` exactly when no error-severity diagnostics were produced.
    pub valid: bool,
    /// All findings in deterministic validation order.
    pub diagnostics: Vec<ValidationDiagnostic>,
}

/// Builds a canonical installed-surface snapshot from catalog and host API facts.
///
/// Serialization failures while hashing normalized content are returned as
/// [`ShellError`] values. The result is sorted so identical inputs have stable hashes.
pub fn build_agent_catalog(
    catalog: &Catalog,
    host_api: &[HostCapability],
    quirl_version: &str,
) -> Result<AgentCatalog, ShellError> {
    let mut commands = catalog
        .commands
        .iter()
        .map(AgentCommand::from)
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| left.path.cmp(&right.path));
    for command in &mut commands {
        command
            .options
            .sort_by(|left, right| left.names.cmp(&right.names));
        command.examples.sort();
        command.effects.sort_by_key(effect_key);
    }
    let mut host_api = host_api.to_vec();
    host_api.sort_by(|left, right| left.path.cmp(&right.path));
    let catalog_hash = hash_json(&commands, "command catalog")?;
    let host_api_hash = hash_json(&host_api, "host API")?;
    let capabilities = installed_capabilities(&host_api)?;
    Ok(AgentCatalog {
        document_type: "quirl.agent.catalog".to_owned(),
        schema_version: AGENT_SCHEMA_VERSION,
        schema_hash: agent_catalog_schema_hash(),
        quirl_version: quirl_version.to_owned(),
        catalog_schema_version: catalog.schema_version,
        catalog_hash,
        host_api_schema_version: 1,
        host_api_hash,
        commands,
        host_api,
        capabilities,
    })
}

/// Derives the compact agent discovery manifest from an installed catalog snapshot.
///
/// The tool list contains Quirl's own command namespace plus exact, trusted,
/// versioned plugin commands. Imported and heuristic external catalog facts
/// remain discoverable in [`AgentCatalog`] but never become executable tools.
///
/// Returns a [`ShellError`] only if the deterministic manifest payload cannot be
/// serialized for its content hash.
pub fn build_agent_manifest(catalog: &AgentCatalog) -> Result<AgentManifest, ShellError> {
    let mut tools = catalog
        .commands
        .iter()
        .filter(|command| {
            command.path.starts_with("quirl ")
                || (command.provenance.source == Provenance::Plugin
                    && command.provenance.confidence == Confidence::Exact
                    && command.provenance.trust == Trust::Trusted
                    && command.id.starts_with("plugin:")
                    && command.version.is_some())
        })
        .map(|command| AgentTool {
            name: command.path.clone(),
            version: command
                .version
                .clone()
                .unwrap_or_else(|| catalog.quirl_version.clone()),
            summary: command.summary.clone(),
            effects: command.effects.clone(),
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let schemas = vec![
        SchemaDescriptor {
            name: "quirl.agent.catalog".to_owned(),
            version: AGENT_SCHEMA_VERSION,
            schema_hash: agent_catalog_schema_hash(),
            content_hash: catalog.catalog_hash.clone(),
        },
        SchemaDescriptor {
            name: "quirl.agent.context".to_owned(),
            version: AGENT_SCHEMA_VERSION,
            schema_hash: agent_context_schema_hash(),
            content_hash: catalog.catalog_hash.clone(),
        },
        SchemaDescriptor {
            name: "quirl.agent.manifest".to_owned(),
            version: AGENT_SCHEMA_VERSION,
            schema_hash: agent_manifest_schema_hash(),
            content_hash: catalog.host_api_hash.clone(),
        },
        SchemaDescriptor {
            name: "quirl.package.manifest".to_owned(),
            version: 1,
            schema_hash: package_manifest_schema_hash(),
            content_hash: catalog.host_api_hash.clone(),
        },
    ];
    let validators = vec![
        AgentValidator {
            name: "agent-contract".to_owned(),
            command: "quirl agent validate <file> --kind <catalog|context|manifest> --format json"
                .to_owned(),
            schema_version: AGENT_SCHEMA_VERSION,
            schema_hash: agent_manifest_schema_hash(),
        },
        AgentValidator {
            name: "lua".to_owned(),
            command: "quirl check <file> --format json".to_owned(),
            schema_version: 1,
            schema_hash: catalog.host_api_hash.clone(),
        },
        AgentValidator {
            name: "package".to_owned(),
            command: "quirl package build --format json".to_owned(),
            schema_version: 1,
            schema_hash: package_manifest_schema_hash(),
        },
    ];
    let content_hash = hash_json(
        &(
            &catalog.quirl_version,
            &schemas,
            &catalog.capabilities,
            &tools,
            &validators,
        ),
        "agent manifest content",
    )?;
    Ok(AgentManifest {
        document_type: "quirl.agent.manifest".to_owned(),
        schema_version: AGENT_SCHEMA_VERSION,
        schema_hash: agent_manifest_schema_hash(),
        content_hash,
        quirl_version: catalog.quirl_version.clone(),
        schemas,
        capabilities: catalog.capabilities.clone(),
        tools,
        validators,
    })
}

/// Selects the most relevant catalog entries that fit within `token_budget`.
///
/// Budgets below [`MINIMUM_TOKEN_BUDGET`] fail with
/// [`quirl_core::ErrorCode::ResourceLimit`]. Serialization failures encountered by
/// the documented token estimator are also returned as [`ShellError`] values.
pub fn build_agent_context(
    catalog: &AgentCatalog,
    query: &str,
    token_budget: usize,
) -> Result<AgentContext, ShellError> {
    if token_budget < MINIMUM_TOKEN_BUDGET {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("agent token budget must be at least {MINIMUM_TOKEN_BUDGET}"),
        )
        .with_help(format!(
            "Pass --token-budget {MINIMUM_TOKEN_BUDGET} or a larger value"
        )));
    }
    let query = query.trim();
    if query.is_empty() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "agent context query must not be empty",
        )
        .with_help("Describe the task whose installed commands and capabilities are needed"));
    }

    let mut candidates = Vec::new();
    for command in &catalog.commands {
        let score = command_score(query, command);
        if score > 0 {
            candidates.push(ContextCandidate::Command {
                score,
                key: format!("command:{}", command.path),
                value: Box::new(command.clone()),
            });
        }
    }
    for capability in &catalog.host_api {
        let score = host_score(query, capability);
        if score > 0 {
            candidates.push(ContextCandidate::Host {
                score,
                key: format!("host:{}", capability.path),
                value: capability.clone(),
            });
        }
    }
    candidates.sort_by(|left, right| {
        right
            .score()
            .cmp(&left.score())
            .then_with(|| left.key().cmp(right.key()))
    });

    let token_budget_wire = u64::try_from(token_budget).map_err(|_| {
        ShellError::new(
            ErrorCode::ResourceLimit,
            "agent token budget cannot be represented by the fixed-width contract",
        )
        .with_context(format!("observed token budget: {token_budget}"))
        .with_help("Use a token budget no greater than u64::MAX")
    })?;
    let mut context = AgentContext {
        document_type: "quirl.agent.context".to_owned(),
        schema_version: AGENT_SCHEMA_VERSION,
        schema_hash: agent_context_schema_hash(),
        query: query.to_owned(),
        token_budget: token_budget_wire,
        estimated_tokens: 0,
        token_estimator: "canonical-json-unicode-scalars-divided-by-4-v1".to_owned(),
        truncated: false,
        catalog_hash: catalog.catalog_hash.clone(),
        host_api_hash: catalog.host_api_hash.clone(),
        commands: Vec::new(),
        host_api: Vec::new(),
    };
    let base_estimate = estimate_payload_tokens(&context)?;
    if base_estimate > token_budget {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("agent query alone requires {base_estimate} estimated tokens"),
        )
        .with_help("Shorten the query or increase --token-budget"));
    }
    context.estimated_tokens = token_count_to_wire(base_estimate);
    let candidate_count = candidates.len();
    let mut content_truncated = false;
    for candidate in candidates {
        match candidate {
            ContextCandidate::Command { value, .. } => {
                let mut proposed = context.clone();
                proposed.commands.push((*value).clone());
                sort_context(&mut proposed);
                let estimated = estimate_payload_tokens(&proposed)?;
                if estimated <= token_budget {
                    context = proposed;
                    context.estimated_tokens = token_count_to_wire(estimated);
                    continue;
                }

                // Preserve the highest-ranked command when its complete catalog
                // record exceeds a small context budget. The context-wide
                // `truncated` bit distinguishes this deterministic projection
                // from the authoritative record referenced by `catalog_hash`.
                let mut compact = context.clone();
                compact.commands.push(compact_agent_command(*value));
                sort_context(&mut compact);
                let estimated = estimate_payload_tokens(&compact)?;
                if estimated <= token_budget {
                    context = compact;
                    context.estimated_tokens = token_count_to_wire(estimated);
                    content_truncated = true;
                }
            }
            ContextCandidate::Host { value, .. } => {
                let mut proposed = context.clone();
                proposed.host_api.push(value);
                sort_context(&mut proposed);
                let estimated = estimate_payload_tokens(&proposed)?;
                if estimated <= token_budget {
                    context = proposed;
                    context.estimated_tokens = token_count_to_wire(estimated);
                }
            }
        }
    }
    sort_context(&mut context);
    context.estimated_tokens = token_count_to_wire(estimate_payload_tokens(&context)?);
    context.truncated =
        content_truncated || context.commands.len() + context.host_api.len() < candidate_count;
    Ok(context)
}

fn compact_agent_command(mut command: AgentCommand) -> AgentCommand {
    command.options.clear();
    command.examples.truncate(1);
    command.aliases.clear();
    command
}

/// Renders an agent context as deterministic, human-readable Markdown.
pub fn render_context_markdown(context: &AgentContext) -> String {
    let mut output = format!(
        "# Quirl agent context\n\nTask: {}\n\nCatalog: `{}`  \nHost API: `{}`  \nBudget: {} estimated tokens ({})\n\n",
        context.query,
        context.catalog_hash,
        context.host_api_hash,
        context.estimated_tokens,
        if context.truncated {
            "truncated"
        } else {
            "complete"
        }
    );
    if !context.commands.is_empty() {
        output.push_str("## Relevant commands\n\n");
        for command in &context.commands {
            output.push_str(&format!(
                "### `{}`\n\n{}\n\n{}\n\nInput: `{}`  \nOutput: `{}`  \nLive streaming: `{}`\n\n",
                command.signature,
                command.summary,
                command.details,
                command.io.input,
                command.io.output,
                command.io.streaming
            ));
            if !command.examples.is_empty() {
                output.push_str("Examples:\n\n");
                for example in &command.examples {
                    output.push_str(&format!("- `{example}`\n"));
                }
                output.push('\n');
            }
            if !command.effects.is_empty() {
                output.push_str(&format!("Effects: `{:?}`\n\n", command.effects));
            }
        }
    }
    if !context.host_api.is_empty() {
        output.push_str("## Relevant Lua host API\n\n");
        for capability in &context.host_api {
            output.push_str(&format!(
                "- `{}` → `{}` — {}{}\n",
                capability.path,
                capability.returns,
                capability.summary,
                capability
                    .capability
                    .as_ref()
                    .map_or_else(String::new, |name| format!(" Capability: `{name}`."))
            ));
        }
    }
    output
}

/// Validates a bounded agent JSON document's syntax, type contract, and internal hashes.
///
/// Inputs larger than [`AGENT_DOCUMENT_BYTES_MAX`] fail before JSON parsing. This
/// form does not compare content hashes with the currently installed catalog;
/// unanchored catalogs remain useful for self-consistency checks, while context
/// and manifest documents require installed anchors.
pub fn validate_agent_document(source: &[u8], kind: AgentDocumentKind) -> ValidationReport {
    validate_agent_document_with_anchors(source, kind, None)
}

/// Validates an agent JSON document and optionally enforces installed-content anchors.
///
/// Anchors let callers reject a structurally valid but stale catalog, context, or
/// manifest without trusting hash values supplied by that same document. Inputs
/// larger than [`AGENT_DOCUMENT_BYTES_MAX`] fail before JSON parsing. Passing no
/// anchors permits catalog self-consistency validation but makes context and
/// manifest validation fail with `agent.trusted_anchor_required`.
pub fn validate_agent_document_with_anchors(
    source: &[u8],
    kind: AgentDocumentKind,
    anchors: Option<&AgentValidationAnchors>,
) -> ValidationReport {
    match kind {
        AgentDocumentKind::Catalog => {
            validate_typed::<AgentCatalog>(source, |catalog, diagnostics| {
                validate_catalog(catalog, anchors, diagnostics);
            })
        }
        AgentDocumentKind::Context => {
            validate_typed::<AgentContext>(source, |context, diagnostics| {
                validate_context(context, anchors, diagnostics);
            })
        }
        AgentDocumentKind::Manifest => {
            validate_typed::<AgentManifest>(source, |manifest, diagnostics| {
                validate_manifest(manifest, anchors, diagnostics);
            })
        }
    }
}

impl From<&CommandSpec> for AgentCommand {
    fn from(command: &CommandSpec) -> Self {
        Self {
            id: command.id.clone(),
            version: command.version.clone(),
            path: command.path.clone(),
            aliases: command.aliases.clone(),
            parent: command.parent.clone(),
            signature: command.signature.clone(),
            summary: command.summary.clone(),
            details: command.details.clone(),
            options: command
                .options
                .iter()
                .map(|option| AgentOption {
                    names: option.names.clone(),
                    kind: option.kind,
                    value_type: option.value_type.clone(),
                    required: option.required,
                    repeatable: option.repeatable,
                    values: option.values.clone(),
                    conflicts: option.conflicts.clone(),
                    documentation: option.documentation.clone(),
                    examples: option.examples.clone(),
                    provenance: AgentProvenance::from(&option.provenance),
                })
                .collect(),
            examples: command.examples.clone(),
            io: command.io.clone(),
            effects: command.effects.clone(),
            exit_codes: command.exit_codes.clone(),
            provenance: AgentProvenance::from(&command.provenance),
        }
    }
}

impl From<&quirl_catalog::ProvenanceInfo> for AgentProvenance {
    fn from(provenance: &quirl_catalog::ProvenanceInfo) -> Self {
        Self {
            source: provenance.source,
            confidence: provenance.confidence,
            trust: provenance.trust,
            origin: provenance.origin.clone(),
            fingerprint: provenance.fingerprint.clone(),
            generated_at: provenance.generated_at.clone(),
        }
    }
}

enum ContextCandidate {
    Command {
        score: i32,
        key: String,
        value: Box<AgentCommand>,
    },
    Host {
        score: i32,
        key: String,
        value: HostCapability,
    },
}

impl ContextCandidate {
    fn score(&self) -> i32 {
        match self {
            Self::Command { score, .. } | Self::Host { score, .. } => *score,
        }
    }

    fn key(&self) -> &str {
        match self {
            Self::Command { key, .. } | Self::Host { key, .. } => key,
        }
    }
}

fn installed_capabilities(
    host_api: &[HostCapability],
) -> Result<Vec<InstalledCapability>, ShellError> {
    let mut grouped = BTreeMap::<String, Vec<String>>::new();
    for host in host_api {
        if let Some(capability) = &host.capability {
            grouped
                .entry(capability.clone())
                .or_default()
                .push(host.path.clone());
        }
    }
    grouped
        .into_iter()
        .map(|(name, mut providers)| {
            providers.sort();
            Ok(InstalledCapability {
                name,
                version: 1,
                schema_hash: hash_json(&providers, "capability providers")?,
                providers,
            })
        })
        .collect()
}

fn command_score(query: &str, command: &AgentCommand) -> i32 {
    relevance_score(
        query,
        &command.path,
        &command.summary,
        &format!(
            "{} {} {}",
            command.details,
            command.examples.join(" "),
            command
                .options
                .iter()
                .map(|option| { format!("{} {}", option.names.join(" "), option.documentation) })
                .collect::<Vec<_>>()
                .join(" ")
        ),
    )
}

fn host_score(query: &str, capability: &HostCapability) -> i32 {
    relevance_score(
        query,
        &capability.path,
        &capability.summary,
        capability.capability.as_deref().unwrap_or_default(),
    )
}

fn relevance_score(query: &str, title: &str, summary: &str, details: &str) -> i32 {
    let title = title.to_lowercase();
    let summary = summary.to_lowercase();
    let details = details.to_lowercase();
    let query = query.to_lowercase();
    let terms = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .collect::<BTreeSet<_>>();
    let exact = i32::from(title == query) * 10_000;
    exact
        + terms
            .into_iter()
            .map(|term| {
                i32::from(title.contains(term)) * 400
                    + i32::from(summary.contains(term)) * 100
                    + i32::from(details.contains(term)) * 25
            })
            .sum::<i32>()
}

fn sort_context(context: &mut AgentContext) {
    context
        .commands
        .sort_by(|left, right| left.path.cmp(&right.path));
    context
        .host_api
        .sort_by(|left, right| left.path.cmp(&right.path));
}

fn estimate_payload_tokens(context: &AgentContext) -> Result<usize, ShellError> {
    #[derive(Serialize)]
    struct Payload<'a> {
        query: &'a str,
        commands: &'a [AgentCommand],
        host_api: &'a [HostCapability],
    }
    let json = serde_json::to_string(&Payload {
        query: &context.query,
        commands: &context.commands,
        host_api: &context.host_api,
    })
    .map_err(|error| serialization_error("agent context", error))?;
    Ok(json.chars().count().div_ceil(4))
}

fn token_count_to_wire(count: usize) -> u64 {
    u64::try_from(count).map_or(u64::MAX, |count| count)
}

fn validate_typed<T: DeserializeOwned>(
    source: &[u8],
    validate: impl FnOnce(&T, &mut Vec<ValidationDiagnostic>),
) -> ValidationReport {
    let mut diagnostics = Vec::new();
    if source.len() > AGENT_DOCUMENT_BYTES_MAX {
        diagnostics.push(ValidationDiagnostic {
            code: "agent.resource_limit".to_owned(),
            severity: DiagnosticSeverity::Error,
            message: format!(
                "agent document exceeds its byte limit; limit: {AGENT_DOCUMENT_BYTES_MAX}; observed: {}",
                source.len()
            ),
            path: "$".to_owned(),
            help: "Reduce the document or regenerate a bounded installed-surface projection"
                .to_owned(),
        });
    } else if let Err(error) = serde_json::from_slice::<T>(source).map(|document| {
        validate(&document, &mut diagnostics);
    }) {
        diagnostics.push(ValidationDiagnostic {
            code: "agent.schema".to_owned(),
            severity: DiagnosticSeverity::Error,
            message: error.to_string(),
            path: format!("line {}, column {}", error.line(), error.column()),
            help:
                "Use the matching `quirl agent ... --format json` schema and remove unknown fields"
                    .to_owned(),
        });
    }
    ValidationReport {
        document_type: "quirl.agent.validation".to_owned(),
        schema_version: AGENT_SCHEMA_VERSION,
        valid: diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != DiagnosticSeverity::Error),
        diagnostics,
    }
}

fn validate_catalog(
    catalog: &AgentCatalog,
    anchors: Option<&AgentValidationAnchors>,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    validate_header(
        &catalog.document_type,
        "quirl.agent.catalog",
        catalog.schema_version,
        &catalog.schema_hash,
        &agent_catalog_schema_hash(),
        diagnostics,
    );
    validate_unique_sorted(
        catalog.commands.iter().map(|command| command.path.as_str()),
        "commands",
        diagnostics,
    );
    validate_unique_sorted(
        catalog.host_api.iter().map(|host| host.path.as_str()),
        "host_api",
        diagnostics,
    );
    if let Ok(expected) = hash_json(&catalog.commands, "command catalog") {
        validate_hash(
            &catalog.catalog_hash,
            &expected,
            "catalog_hash",
            diagnostics,
        );
    }
    if let Ok(expected) = hash_json(&catalog.host_api, "host API") {
        validate_hash(
            &catalog.host_api_hash,
            &expected,
            "host_api_hash",
            diagnostics,
        );
    }
    if let Some(anchors) = anchors {
        validate_hash(
            &catalog.catalog_hash,
            &anchors.catalog_hash,
            "catalog_hash",
            diagnostics,
        );
        validate_hash(
            &catalog.host_api_hash,
            &anchors.host_api_hash,
            "host_api_hash",
            diagnostics,
        );
    }
    for capability in &catalog.capabilities {
        if let Ok(expected) = hash_json(&capability.providers, "capability providers") {
            validate_hash(
                &capability.schema_hash,
                &expected,
                &format!("capabilities.{}.schema_hash", capability.name),
                diagnostics,
            );
        }
    }
}

fn validate_context(
    context: &AgentContext,
    anchors: Option<&AgentValidationAnchors>,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    validate_header(
        &context.document_type,
        "quirl.agent.context",
        context.schema_version,
        &context.schema_hash,
        &agent_context_schema_hash(),
        diagnostics,
    );
    if context.query.trim().is_empty() {
        push_error(
            diagnostics,
            "agent.context.query",
            "query must not be empty",
            "query",
            "Regenerate context with a concrete task query",
        );
    }
    match estimate_payload_tokens(context) {
        Ok(expected) if token_count_to_wire(expected) != context.estimated_tokens => push_error(
            diagnostics,
            "agent.context.estimate",
            &format!(
                "estimated_tokens is {}, but canonical payload estimate is {expected}",
                context.estimated_tokens
            ),
            "estimated_tokens",
            "Regenerate the document instead of editing its budget metadata",
        ),
        Ok(expected) if token_count_to_wire(expected) > context.token_budget => push_error(
            diagnostics,
            "agent.context.budget",
            &format!(
                "context uses {expected} estimated tokens, exceeding its {} token budget",
                context.token_budget
            ),
            "token_budget",
            "Regenerate with a larger budget or a smaller relevant subtree",
        ),
        _ => {}
    }
    validate_anchors(
        anchors,
        &context.catalog_hash,
        &context.host_api_hash,
        diagnostics,
    );
}

fn validate_manifest(
    manifest: &AgentManifest,
    anchors: Option<&AgentValidationAnchors>,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    validate_header(
        &manifest.document_type,
        "quirl.agent.manifest",
        manifest.schema_version,
        &manifest.schema_hash,
        &agent_manifest_schema_hash(),
        diagnostics,
    );
    if let Ok(expected) = hash_json(
        &(
            &manifest.quirl_version,
            &manifest.schemas,
            &manifest.capabilities,
            &manifest.tools,
            &manifest.validators,
        ),
        "agent manifest content",
    ) {
        validate_hash(
            &manifest.content_hash,
            &expected,
            "content_hash",
            diagnostics,
        );
    }
    validate_unique_sorted(
        manifest.schemas.iter().map(|schema| schema.name.as_str()),
        "schemas",
        diagnostics,
    );
    validate_unique_sorted(
        manifest.tools.iter().map(|tool| tool.name.as_str()),
        "tools",
        diagnostics,
    );
    let expected_schemas = BTreeMap::from([
        ("quirl.agent.catalog", agent_catalog_schema_hash()),
        ("quirl.agent.context", agent_context_schema_hash()),
        ("quirl.agent.manifest", agent_manifest_schema_hash()),
        ("quirl.package.manifest", package_manifest_schema_hash()),
    ]);
    if manifest.schemas.len() != expected_schemas.len() {
        push_error(
            diagnostics,
            "agent.manifest.schemas",
            "manifest must contain exactly the installed structural schemas",
            "schemas",
            "Regenerate the manifest from the currently installed Quirl binary",
        );
    }
    for schema in &manifest.schemas {
        match expected_schemas.get(schema.name.as_str()) {
            Some(expected) => validate_hash(
                &schema.schema_hash,
                expected,
                &format!("schemas.{}.schema_hash", schema.name),
                diagnostics,
            ),
            None => push_error(
                diagnostics,
                "agent.manifest.schema_unknown",
                &format!("unknown installed schema `{}`", schema.name),
                &format!("schemas.{}", schema.name),
                "Regenerate the manifest from the currently installed Quirl binary",
            ),
        }
    }
    if let Some(anchors) = anchors {
        for schema in &manifest.schemas {
            let expected_content = if matches!(
                schema.name.as_str(),
                "quirl.agent.catalog" | "quirl.agent.context"
            ) {
                &anchors.catalog_hash
            } else {
                &anchors.host_api_hash
            };
            validate_hash(
                &schema.content_hash,
                expected_content,
                &format!("schemas.{}.content_hash", schema.name),
                diagnostics,
            );
        }
    } else {
        missing_anchors(diagnostics);
    }
    validate_unique_sorted(
        manifest.capabilities.iter().map(|item| item.name.as_str()),
        "capabilities",
        diagnostics,
    );
    for capability in &manifest.capabilities {
        if let Ok(expected) = hash_json(&capability.providers, "capability providers") {
            validate_hash(
                &capability.schema_hash,
                &expected,
                &format!("capabilities.{}.schema_hash", capability.name),
                diagnostics,
            );
        }
    }
}

fn validate_header(
    actual_type: &str,
    expected_type: &str,
    version: u32,
    actual_hash: &str,
    expected_hash: &str,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    if actual_type != expected_type {
        push_error(
            diagnostics,
            "agent.document_type",
            &format!("expected document_type `{expected_type}`, found `{actual_type}`"),
            "document_type",
            "Validate the document with its matching --kind value",
        );
    }
    if version != AGENT_SCHEMA_VERSION {
        push_error(
            diagnostics,
            "agent.schema_version",
            &format!("unsupported schema version {version}"),
            "schema_version",
            &format!("Regenerate using schema version {AGENT_SCHEMA_VERSION}"),
        );
    }
    validate_hash(actual_hash, expected_hash, "schema_hash", diagnostics);
}

fn validate_anchors(
    anchors: Option<&AgentValidationAnchors>,
    catalog_hash: &str,
    host_api_hash: &str,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    let Some(anchors) = anchors else {
        missing_anchors(diagnostics);
        return;
    };
    validate_hash(
        catalog_hash,
        &anchors.catalog_hash,
        "catalog_hash",
        diagnostics,
    );
    validate_hash(
        host_api_hash,
        &anchors.host_api_hash,
        "host_api_hash",
        diagnostics,
    );
}

fn missing_anchors(diagnostics: &mut Vec<ValidationDiagnostic>) {
    push_error(
        diagnostics,
        "agent.trusted_anchor_required",
        "subset/content hashes require trusted installed catalog and HOST_API anchors",
        "catalog_hash",
        "Validate through `quirl agent validate`, which supplies anchors from the running binary",
    );
}

fn validate_hash(
    actual: &str,
    expected: &str,
    path: &str,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    if actual != expected {
        push_error(
            diagnostics,
            "agent.hash_mismatch",
            &format!("expected `{expected}`, found `{actual}`"),
            path,
            "Regenerate the document from the currently installed Quirl binary",
        );
    }
}

fn validate_unique_sorted<'a>(
    values: impl Iterator<Item = &'a str>,
    path: &str,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    let values = values.collect::<Vec<_>>();
    let mut sorted = values.clone();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted != values {
        push_error(
            diagnostics,
            "agent.order",
            "entries must be unique and sorted for deterministic output",
            path,
            "Regenerate the document with Quirl instead of reordering it manually",
        );
    }
}

fn push_error(
    diagnostics: &mut Vec<ValidationDiagnostic>,
    code: &str,
    message: &str,
    path: &str,
    help: &str,
) {
    diagnostics.push(ValidationDiagnostic {
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        message: message.to_owned(),
        path: path.to_owned(),
        help: help.to_owned(),
    });
}

fn hash_json(value: &impl Serialize, label: &str) -> Result<String, ShellError> {
    serde_json::to_vec(value)
        .map(|bytes| stable_hash(&bytes))
        .map_err(|error| serialization_error(label, error))
}

fn serialization_error(label: &str, error: serde_json::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not serialize deterministic {label}"),
    )
    .with_context(error.to_string())
    .with_help("Report this as a Quirl schema generation defect")
}

fn effect_key(effect: &Effect) -> u8 {
    match effect {
        Effect::ReadFilesystem => 0,
        Effect::WriteFilesystem => 1,
        Effect::SpawnProcess => 2,
        Effect::ChangeDirectory => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_catalog::Catalog;

    const AGENT_CONTEXT_V1_FIXTURE: &str = r#"{"document_type":"quirl.agent.context","schema_version":1,"schema_hash":"fnv1a64:7b0dacacc4a5e71d","query":"fixture","token_budget":64,"estimated_tokens":2,"token_estimator":"canonical-json-unicode-scalars-divided-by-4-v1","truncated":false,"catalog_hash":"fnv1a64:catalog","host_api_hash":"fnv1a64:host","commands":[],"host_api":[]}"#;

    fn host_api() -> Vec<HostCapability> {
        vec![
            HostCapability {
                path: "quirl.process.run".to_owned(),
                summary: "Run a command".to_owned(),
                parameters: vec![HostParameter {
                    name: "command".to_owned(),
                    value_type: "string".to_owned(),
                }],
                returns: "quirl.Result".to_owned(),
                capability: Some("process.spawn".to_owned()),
            },
            HostCapability {
                path: "quirl.cwd".to_owned(),
                summary: "Get cwd".to_owned(),
                parameters: Vec::new(),
                returns: "string".to_owned(),
                capability: None,
            },
        ]
    }

    #[test]
    fn catalog_and_capability_hashes_are_deterministic() {
        let first = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let second = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        assert_eq!(first, second);
        assert_eq!(first.host_api[0].path, "quirl.cwd");
        assert_eq!(first.capabilities[0].name, "process.spawn");
    }

    #[test]
    fn command_documentation_flows_unchanged_into_ai_catalog_and_context() {
        let source = Catalog::builtin();
        let source_command = source.find("quirl doc").unwrap();
        let catalog = build_agent_catalog(&source, &host_api(), "0.1.0").unwrap();
        let exported = catalog
            .commands
            .iter()
            .find(|command| command.path == "quirl doc")
            .unwrap();

        assert_eq!(exported.summary, source_command.summary);
        assert_eq!(exported.details, source_command.details);
        assert_eq!(exported.io, source_command.io);
        assert_eq!(
            exported.options[0].documentation,
            source_command.options[0].documentation
        );

        let context =
            build_agent_context(&catalog, "generate installed documentation", 1_000).unwrap();
        let selected = context
            .commands
            .iter()
            .find(|command| command.path == "quirl doc")
            .unwrap();
        assert_eq!(selected.details, source_command.details);
        assert_eq!(selected.io, source_command.io);
        let markdown = render_context_markdown(&context);
        assert!(markdown.contains(&source_command.details));
        assert!(markdown.contains(&format!("Input: `{}`", source_command.io.input)));
        assert!(markdown.contains(&format!("Output: `{}`", source_command.io.output)));
    }

    #[test]
    fn context_is_deterministic_and_stays_inside_token_budget() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let first = build_agent_context(&catalog, "commit changes", 500).unwrap();
        let second = build_agent_context(&catalog, "commit changes", 500).unwrap();
        assert_eq!(first, second);
        assert!(first.estimated_tokens <= 500);
        assert!(
            first
                .commands
                .iter()
                .any(|command| command.path == "git commit")
        );
    }

    #[test]
    fn context_excludes_candidates_with_no_positive_relevance() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let context = build_agent_context(&catalog, "zyxwvutsrqponmlkjihgfedcba", 500).unwrap();
        assert!(context.commands.is_empty());
        assert!(context.host_api.is_empty());
    }

    #[test]
    fn tiny_context_budget_is_rejected_with_help() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let error = build_agent_context(&catalog, "commit", 8).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn validation_rejects_unknown_fields() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let mut value = serde_json::to_value(catalog).unwrap();
        value["unknown"] = serde_json::json!(true);
        let report = validate_agent_document(
            &serde_json::to_vec(&value).unwrap(),
            AgentDocumentKind::Catalog,
        );
        assert!(!report.valid);
        assert_eq!(report.diagnostics[0].code, "agent.schema");
    }

    #[test]
    fn validation_detects_tampered_content_hash() {
        let mut catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        catalog.catalog_hash = "fnv1a64:0000000000000000".to_owned();
        let report = validate_agent_document(
            &serde_json::to_vec(&catalog).unwrap(),
            AgentDocumentKind::Catalog,
        );
        assert!(!report.valid);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == "catalog_hash")
        );
    }

    #[test]
    fn catalog_validation_uses_supplied_anchors_but_allows_unanchored_self_checks() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let anchors = AgentValidationAnchors::from(&catalog);
        let matching = validate_agent_document_with_anchors(
            &serde_json::to_vec(&catalog).unwrap(),
            AgentDocumentKind::Catalog,
            Some(&anchors),
        );
        assert!(matching.valid, "{:?}", matching.diagnostics);

        let mut stale = catalog;
        stale.commands[0].summary.push_str(" stale");
        stale.catalog_hash = hash_json(&stale.commands, "command catalog").unwrap();
        let source = serde_json::to_vec(&stale).unwrap();
        let unanchored = validate_agent_document(&source, AgentDocumentKind::Catalog);
        assert!(unanchored.valid, "{:?}", unanchored.diagnostics);

        let anchored = validate_agent_document_with_anchors(
            &source,
            AgentDocumentKind::Catalog,
            Some(&anchors),
        );
        assert!(!anchored.valid);
        assert!(
            anchored
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == "catalog_hash")
        );
    }

    #[test]
    fn agent_document_byte_limit_accepts_exact_and_rejects_valid_plus_one() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let mut source = serde_json::to_vec(&catalog).unwrap();
        source.resize(AGENT_DOCUMENT_BYTES_MAX, b' ');
        let exact = validate_agent_document(&source, AgentDocumentKind::Catalog);
        assert!(exact.valid, "{:?}", exact.diagnostics);

        source.push(b' ');
        let excess = validate_agent_document(&source, AgentDocumentKind::Catalog);
        assert!(!excess.valid);
        assert_eq!(excess.diagnostics[0].code, "agent.resource_limit");
        assert!(
            excess.diagnostics[0]
                .message
                .contains(&format!("limit: {AGENT_DOCUMENT_BYTES_MAX}"))
        );
        assert!(
            excess.diagnostics[0]
                .message
                .contains(&format!("observed: {}", AGENT_DOCUMENT_BYTES_MAX + 1))
        );
    }

    #[test]
    fn context_wire_budget_is_fixed_width_and_previous_major_fails_closed() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let anchors = AgentValidationAnchors::from(&catalog);
        let mut context = build_agent_context(&catalog, "git commit", 500).unwrap();
        context.token_budget = u64::MAX;
        let source = serde_json::to_vec(&context).unwrap();
        let decoded: AgentContext = serde_json::from_slice(&source).unwrap();
        assert_eq!(decoded.token_budget, u64::MAX);

        context.schema_version = 1;
        let previous = validate_agent_document_with_anchors(
            &serde_json::to_vec(&context).unwrap(),
            AgentDocumentKind::Context,
            Some(&anchors),
        );
        assert!(!previous.valid);
        assert!(
            previous
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "agent.schema_version")
        );
        let previous_hash = structural_schema_hash(&[
            AGENT_CONTEXT_SCHEMA_V1_DESCRIPTOR,
            COMMAND_SCHEMA,
            OPTION_SCHEMA,
            COMPLETION_SCHEMA,
            IO_SCHEMA,
            PROVENANCE_SCHEMA,
            HOST_SCHEMA,
        ]);
        assert_eq!(previous_hash, "fnv1a64:7b0dacacc4a5e71d");
        assert_ne!(previous_hash, agent_context_schema_hash());

        let historical = validate_agent_document_with_anchors(
            AGENT_CONTEXT_V1_FIXTURE.as_bytes(),
            AgentDocumentKind::Context,
            Some(&AgentValidationAnchors {
                catalog_hash: "fnv1a64:catalog".to_owned(),
                host_api_hash: "fnv1a64:host".to_owned(),
            }),
        );
        assert!(
            historical
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "agent.schema_version")
        );
    }

    #[test]
    fn generated_context_and_manifest_validate_against_their_exact_schemas() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let context = build_agent_context(&catalog, "git commit", 500).unwrap();
        let anchors = AgentValidationAnchors::from(&catalog);
        let context_report = validate_agent_document_with_anchors(
            &serde_json::to_vec(&context).unwrap(),
            AgentDocumentKind::Context,
            Some(&anchors),
        );
        assert!(context_report.valid, "{:?}", context_report.diagnostics);

        let manifest = build_agent_manifest(&catalog).unwrap();
        let manifest_report = validate_agent_document_with_anchors(
            &serde_json::to_vec(&manifest).unwrap(),
            AgentDocumentKind::Manifest,
            Some(&anchors),
        );
        assert!(manifest_report.valid, "{:?}", manifest_report.diagnostics);
    }

    #[test]
    fn context_validation_requires_installed_anchors_and_rejects_tampering() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let anchors = AgentValidationAnchors::from(&catalog);
        let mut context = build_agent_context(&catalog, "git commit", 500).unwrap();
        let unanchored = validate_agent_document(
            &serde_json::to_vec(&context).unwrap(),
            AgentDocumentKind::Context,
        );
        assert!(
            unanchored
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "agent.trusted_anchor_required")
        );

        context.catalog_hash = "fnv1a64:0000000000000000".to_owned();
        let tampered = validate_agent_document_with_anchors(
            &serde_json::to_vec(&context).unwrap(),
            AgentDocumentKind::Context,
            Some(&anchors),
        );
        assert!(!tampered.valid);
        assert!(
            tampered
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == "catalog_hash")
        );
    }

    #[test]
    fn manifest_validation_recomputes_content_and_structural_hashes() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let anchors = AgentValidationAnchors::from(&catalog);
        let mut manifest = build_agent_manifest(&catalog).unwrap();
        manifest.tools[0].summary.push_str(" tampered");
        manifest.schemas[0].schema_hash = "fnv1a64:0000000000000000".to_owned();
        let report = validate_agent_document_with_anchors(
            &serde_json::to_vec(&manifest).unwrap(),
            AgentDocumentKind::Manifest,
            Some(&anchors),
        );
        assert!(!report.valid);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == "content_hash")
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path.ends_with("schema_hash"))
        );
    }

    #[test]
    fn manifest_lists_builtin_tools_and_validated_plugin_commands() {
        let mut source = Catalog::builtin();
        let mut plugin = source.find("quirl data ls").unwrap().clone();
        plugin.id = "plugin:demo/demo/run".to_owned();
        plugin.path = "demo run".to_owned();
        plugin.signature = "demo run".to_owned();
        plugin.version = Some("2.3.4".to_owned());
        plugin.parent = None;
        plugin.provenance.source = Provenance::Plugin;
        plugin.provenance.confidence = Confidence::Exact;
        plugin.provenance.trust = Trust::Trusted;
        source.merge(vec![plugin]);
        let catalog = build_agent_catalog(&source, &host_api(), "0.1.0").unwrap();
        let manifest = build_agent_manifest(&catalog).unwrap();
        assert_eq!(
            manifest
                .tools
                .iter()
                .find(|tool| tool.name == "demo run")
                .unwrap()
                .version,
            "2.3.4"
        );
        assert!(
            manifest
                .tools
                .iter()
                .all(|tool| tool.name.starts_with("quirl ") || tool.name == "demo run")
        );
        assert!(!manifest.tools.iter().any(|tool| tool.name == "ls"));
        assert_eq!(manifest.capabilities[0].name, "process.spawn");
    }
}
