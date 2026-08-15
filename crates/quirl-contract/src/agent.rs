use crate::package::package_manifest_schema_hash;
use crate::stable_hash;
use quirl_catalog::{Catalog, CommandSpec, Confidence, Effect, Provenance};
use quirl_core::{ErrorCode, ShellError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const AGENT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_TOKEN_BUDGET: usize = 6_000;
pub const MINIMUM_TOKEN_BUDGET: usize = 64;

const PROVENANCE_SCHEMA: &str = "AgentProvenance{deny_unknown;source:enum[builtin,external,lua,fish,bash,zsh,help,man];confidence:enum[low,medium,high,exact];origin:null|string;fingerprint:null|string}";
const OPTION_SCHEMA: &str = "AgentOption{deny_unknown;names:array<string>;value:null|string;summary:string;provenance:AgentProvenance}";
const COMMAND_SCHEMA: &str = "AgentCommand{deny_unknown;path:string;signature:string;summary:string;details:string;options:array<AgentOption>;examples:array<string>;effects:array<enum[read_filesystem,write_filesystem,spawn_process,change_directory]>;provenance:AgentProvenance}";
const HOST_SCHEMA: &str = "HostCapability{deny_unknown;path:string;summary:string;parameters:array<HostParameter{deny_unknown;name:string;value_type:string}>;returns:string;capability:null|string}";
const CAPABILITY_SCHEMA: &str = "InstalledCapability{deny_unknown;name:string;version:u32;schema_hash:string;providers:array<string>}";
const CATALOG_SCHEMA: &str = "AgentCatalog{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;quirl_version:string;catalog_schema_version:u32;catalog_hash:string;host_api_schema_version:u32;host_api_hash:string;commands:array<AgentCommand>;host_api:array<HostCapability>;capabilities:array<InstalledCapability>}";
const CONTEXT_SCHEMA: &str = "AgentContext{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;query:string;token_budget:usize;estimated_tokens:usize;token_estimator:string;truncated:bool;catalog_hash:string;host_api_hash:string;commands:array<AgentCommand>;host_api:array<HostCapability>}";
const MANIFEST_COMPONENT_SCHEMA: &str = "AgentManifestComponents{SchemaDescriptor{deny_unknown;name:string;version:u32;schema_hash:string;content_hash:string};AgentTool{deny_unknown;name:string;version:string;summary:string;effects:array<Effect>};AgentValidator{deny_unknown;name:string;command:string;schema_version:u32;schema_hash:string}}";
const MANIFEST_SCHEMA: &str = "AgentManifest{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;content_hash:string;quirl_version:string;schemas:array<SchemaDescriptor>;capabilities:array<InstalledCapability>;tools:array<AgentTool>;validators:array<AgentValidator>}";

fn catalog_schema_hash() -> String {
    structural_schema_hash(&[
        CATALOG_SCHEMA,
        COMMAND_SCHEMA,
        OPTION_SCHEMA,
        PROVENANCE_SCHEMA,
        HOST_SCHEMA,
        CAPABILITY_SCHEMA,
    ])
}

fn context_schema_hash() -> String {
    structural_schema_hash(&[
        CONTEXT_SCHEMA,
        COMMAND_SCHEMA,
        OPTION_SCHEMA,
        PROVENANCE_SCHEMA,
        HOST_SCHEMA,
    ])
}

fn manifest_schema_hash() -> String {
    structural_schema_hash(&[
        MANIFEST_SCHEMA,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostParameter {
    pub name: String,
    pub value_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCapability {
    pub path: String,
    pub summary: String,
    pub parameters: Vec<HostParameter>,
    pub returns: String,
    pub capability: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentOption {
    pub names: Vec<String>,
    pub value: Option<String>,
    pub summary: String,
    pub provenance: AgentProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentProvenance {
    pub source: Provenance,
    pub confidence: Confidence,
    pub origin: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentCommand {
    pub path: String,
    pub signature: String,
    pub summary: String,
    pub details: String,
    pub options: Vec<AgentOption>,
    pub examples: Vec<String>,
    pub effects: Vec<Effect>,
    pub provenance: AgentProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstalledCapability {
    pub name: String,
    pub version: u32,
    pub schema_hash: String,
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentCatalog {
    pub document_type: String,
    pub schema_version: u32,
    pub schema_hash: String,
    pub quirl_version: String,
    pub catalog_schema_version: u32,
    pub catalog_hash: String,
    pub host_api_schema_version: u32,
    pub host_api_hash: String,
    pub commands: Vec<AgentCommand>,
    pub host_api: Vec<HostCapability>,
    pub capabilities: Vec<InstalledCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentContext {
    pub document_type: String,
    pub schema_version: u32,
    pub schema_hash: String,
    pub query: String,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub token_estimator: String,
    pub truncated: bool,
    pub catalog_hash: String,
    pub host_api_hash: String,
    pub commands: Vec<AgentCommand>,
    pub host_api: Vec<HostCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SchemaDescriptor {
    pub name: String,
    pub version: u32,
    pub schema_hash: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentTool {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub effects: Vec<Effect>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentValidator {
    pub name: String,
    pub command: String,
    pub schema_version: u32,
    pub schema_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentManifest {
    pub document_type: String,
    pub schema_version: u32,
    pub schema_hash: String,
    pub content_hash: String,
    pub quirl_version: String,
    pub schemas: Vec<SchemaDescriptor>,
    pub capabilities: Vec<InstalledCapability>,
    pub tools: Vec<AgentTool>,
    pub validators: Vec<AgentValidator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDocumentKind {
    Catalog,
    Context,
    Manifest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentValidationAnchors {
    pub catalog_hash: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationDiagnostic {
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub path: String,
    pub help: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ValidationReport {
    pub document_type: String,
    pub schema_version: u32,
    pub valid: bool,
    pub diagnostics: Vec<ValidationDiagnostic>,
}

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
        schema_hash: catalog_schema_hash(),
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

pub fn build_agent_manifest(catalog: &AgentCatalog) -> Result<AgentManifest, ShellError> {
    let mut tools = catalog
        .commands
        .iter()
        .filter(|command| command.path.starts_with("quirl "))
        .map(|command| AgentTool {
            name: command.path.clone(),
            version: catalog.quirl_version.clone(),
            summary: command.summary.clone(),
            effects: command.effects.clone(),
        })
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    let schemas = vec![
        SchemaDescriptor {
            name: "quirl.agent.catalog".to_owned(),
            version: AGENT_SCHEMA_VERSION,
            schema_hash: catalog_schema_hash(),
            content_hash: catalog.catalog_hash.clone(),
        },
        SchemaDescriptor {
            name: "quirl.agent.context".to_owned(),
            version: AGENT_SCHEMA_VERSION,
            schema_hash: context_schema_hash(),
            content_hash: catalog.catalog_hash.clone(),
        },
        SchemaDescriptor {
            name: "quirl.agent.manifest".to_owned(),
            version: AGENT_SCHEMA_VERSION,
            schema_hash: manifest_schema_hash(),
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
            schema_hash: manifest_schema_hash(),
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
        schema_hash: manifest_schema_hash(),
        content_hash,
        quirl_version: catalog.quirl_version.clone(),
        schemas,
        capabilities: catalog.capabilities.clone(),
        tools,
        validators,
    })
}

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
                value: command.clone(),
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

    let mut context = AgentContext {
        document_type: "quirl.agent.context".to_owned(),
        schema_version: AGENT_SCHEMA_VERSION,
        schema_hash: context_schema_hash(),
        query: query.to_owned(),
        token_budget,
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
    context.estimated_tokens = base_estimate;
    let candidate_count = candidates.len();
    for candidate in candidates {
        let mut proposed = context.clone();
        match candidate {
            ContextCandidate::Command { value, .. } => proposed.commands.push(value),
            ContextCandidate::Host { value, .. } => proposed.host_api.push(value),
        }
        sort_context(&mut proposed);
        let estimated = estimate_payload_tokens(&proposed)?;
        if estimated <= token_budget {
            context = proposed;
            context.estimated_tokens = estimated;
        }
    }
    sort_context(&mut context);
    context.estimated_tokens = estimate_payload_tokens(&context)?;
    context.truncated = context.commands.len() + context.host_api.len() < candidate_count;
    Ok(context)
}

pub fn render_context_markdown(context: &AgentContext) -> String {
    let mut output = format!(
        "# Quirl agent context\n\nTask: {}\n\nCatalog: `{}`  \nHost API: `{}`  \nBudget: {} estimated tokens ({})\n\n",
        context.query,
        context.catalog_hash,
        context.host_api_hash,
        context.estimated_tokens,
        if context.truncated { "truncated" } else { "complete" }
    );
    if !context.commands.is_empty() {
        output.push_str("## Relevant commands\n\n");
        for command in &context.commands {
            output.push_str(&format!(
                "### `{}`\n\n{}\n\n{}\n\n",
                command.signature, command.summary, command.details
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

pub fn validate_agent_document(source: &[u8], kind: AgentDocumentKind) -> ValidationReport {
    validate_agent_document_with_anchors(source, kind, None)
}

pub fn validate_agent_document_with_anchors(
    source: &[u8],
    kind: AgentDocumentKind,
    anchors: Option<&AgentValidationAnchors>,
) -> ValidationReport {
    match kind {
        AgentDocumentKind::Catalog => validate_typed::<AgentCatalog>(source, validate_catalog),
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
            path: command.path.clone(),
            signature: command.signature.clone(),
            summary: command.summary.clone(),
            details: command.details.clone(),
            options: command
                .options
                .iter()
                .map(|option| AgentOption {
                    names: option.names.clone(),
                    value: option.value.clone(),
                    summary: option.summary.clone(),
                    provenance: AgentProvenance::from(&option.provenance),
                })
                .collect(),
            examples: command.examples.clone(),
            effects: command.effects.clone(),
            provenance: AgentProvenance::from(&command.provenance),
        }
    }
}

impl From<&quirl_catalog::ProvenanceInfo> for AgentProvenance {
    fn from(provenance: &quirl_catalog::ProvenanceInfo) -> Self {
        Self {
            source: provenance.source,
            confidence: provenance.confidence,
            origin: provenance.origin.clone(),
            fingerprint: provenance.fingerprint.clone(),
        }
    }
}

enum ContextCandidate {
    Command {
        score: i32,
        key: String,
        value: AgentCommand,
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
                .map(|option| format!("{} {}", option.names.join(" "), option.summary))
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

fn validate_typed<T: DeserializeOwned>(
    source: &[u8],
    validate: impl FnOnce(&T, &mut Vec<ValidationDiagnostic>),
) -> ValidationReport {
    let mut diagnostics = Vec::new();
    match serde_json::from_slice::<T>(source) {
        Ok(document) => validate(&document, &mut diagnostics),
        Err(error) => diagnostics.push(ValidationDiagnostic {
            code: "agent.schema".to_owned(),
            severity: DiagnosticSeverity::Error,
            message: error.to_string(),
            path: format!("line {}, column {}", error.line(), error.column()),
            help:
                "Use the matching `quirl agent ... --format json` schema and remove unknown fields"
                    .to_owned(),
        }),
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

fn validate_catalog(catalog: &AgentCatalog, diagnostics: &mut Vec<ValidationDiagnostic>) {
    validate_header(
        &catalog.document_type,
        "quirl.agent.catalog",
        catalog.schema_version,
        &catalog.schema_hash,
        &catalog_schema_hash(),
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
        &context_schema_hash(),
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
        Ok(expected) if expected != context.estimated_tokens => push_error(
            diagnostics,
            "agent.context.estimate",
            &format!(
                "estimated_tokens is {}, but canonical payload estimate is {expected}",
                context.estimated_tokens
            ),
            "estimated_tokens",
            "Regenerate the document instead of editing its budget metadata",
        ),
        Ok(expected) if expected > context.token_budget => push_error(
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
        &manifest_schema_hash(),
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
        ("quirl.agent.catalog", catalog_schema_hash()),
        ("quirl.agent.context", context_schema_hash()),
        ("quirl.agent.manifest", manifest_schema_hash()),
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
    fn context_is_deterministic_and_stays_inside_token_budget() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let first = build_agent_context(&catalog, "commit changes", 500).unwrap();
        let second = build_agent_context(&catalog, "commit changes", 500).unwrap();
        assert_eq!(first, second);
        assert!(first.estimated_tokens <= 500);
        assert!(first
            .commands
            .iter()
            .any(|command| command.path == "git commit"));
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
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == "catalog_hash"));
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
        assert!(unanchored
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "agent.trusted_anchor_required"));

        context.catalog_hash = "fnv1a64:0000000000000000".to_owned();
        let tampered = validate_agent_document_with_anchors(
            &serde_json::to_vec(&context).unwrap(),
            AgentDocumentKind::Context,
            Some(&anchors),
        );
        assert!(!tampered.valid);
        assert!(tampered
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == "catalog_hash"));
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
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == "content_hash"));
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path.ends_with("schema_hash")));
    }

    #[test]
    fn manifest_lists_only_installed_quirl_commands_and_capabilities() {
        let catalog = build_agent_catalog(&Catalog::builtin(), &host_api(), "0.1.0").unwrap();
        let manifest = build_agent_manifest(&catalog).unwrap();
        assert!(manifest
            .tools
            .iter()
            .all(|tool| tool.name.starts_with("quirl ")));
        assert_eq!(manifest.capabilities[0].name, "process.spawn");
    }
}
