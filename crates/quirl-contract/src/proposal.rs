use crate::stable_hash;
use quirl_catalog::{ArgumentKind, Catalog, CommandSpec, CompletionSource, Effect};
use quirl_core::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};

/// Current version of the planner-to-host command proposal contract.
pub const COMMAND_PROPOSAL_SCHEMA_VERSION: u32 = 1;
/// Maximum bytes accepted in one serialized command proposal.
pub const COMMAND_PROPOSAL_SOURCE_BYTES_MAX: usize = 64 * 1024;
/// Maximum argument occurrences accepted in one command proposal.
pub const COMMAND_PROPOSAL_ARGUMENTS_MAX: usize = 256;
/// Maximum catalog arguments inspected for one proposed command.
pub const COMMAND_PROPOSAL_CATALOG_ARGUMENTS_MAX: usize = 1_024;
/// Maximum bytes accepted in one resolved argument value.
pub const COMMAND_PROPOSAL_VALUE_BYTES_MAX: usize = 16 * 1024;
/// Maximum aggregate bytes accepted across resolved argument values.
pub const COMMAND_PROPOSAL_VALUES_BYTES_MAX: usize = 64 * 1024;
/// Maximum bytes accepted in a planner explanation.
pub const COMMAND_PROPOSAL_EXPLANATION_BYTES_MAX: usize = 8 * 1024;
/// Maximum bytes accepted in a proposal producer identity.
pub const COMMAND_PROPOSAL_PRODUCER_BYTES_MAX: usize = 1024;
/// Maximum bytes accepted in a natural-language planning intent.
pub const COMMAND_PLANNING_INTENT_BYTES_MAX: usize = 16 * 1024;
/// Maximum bytes emitted by trusted command rendering.
pub const COMMAND_PROPOSAL_RENDER_BYTES_MAX: usize = 512 * 1024;

/// Canonical structural description used to fingerprint [`CommandProposal`].
pub const COMMAND_PROPOSAL_SCHEMA_DESCRIPTOR: &str = "quirl.command-proposal@1{CommandProposal{deny_unknown;schema_version:1;command_id:string;arguments:array<CommandProposalArgument>;explanation:string;provenance:CommandProposalProvenance};CommandProposalArgument:tag(kind)[positional{deny_unknown;name:string;value:CommandProposalValue}|option{deny_unknown;name:string;value:CommandProposalValue}|flag{deny_unknown;name:string}];CommandProposalValue:tag(type)[unresolved{deny_unknown}|text{deny_unknown;value:string}|path{deny_unknown;value:string}|integer{deny_unknown;value:i64}|unsigned{deny_unknown;value:u64}|boolean{deny_unknown;value:bool}];CommandProposalProvenance{deny_unknown;source:planner|retrieval_fallback;producer:string};limits:source<=65536,arguments<=256,catalog_arguments<=1024,value<=16384,values<=65536,explanation<=8192,producer<=1024,render<=524288;render:catalog-path-and-canonical-argument-names-plus-single-quoted-literal-values;authority:none;confirmation:always;ordinary_risk:explicit_read_filesystem_only;high_risk:unknown|write_filesystem|spawn_process|change_directory}";

/// Return the stable structural identity of the command proposal schema.
pub fn command_proposal_schema_hash() -> String {
    stable_hash(COMMAND_PROPOSAL_SCHEMA_DESCRIPTOR.as_bytes())
}

/// Planner output describing one catalog command without carrying shell source.
///
/// The planner supplies only a stable command identifier and typed argument
/// bindings. Validation resolves executable spelling and effects from the
/// caller-supplied [`Catalog`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandProposal {
    /// Exact proposal schema version; readers accept only version 1.
    pub schema_version: u32,
    /// Exact stable [`CommandSpec::id`] selected from the supplied catalog.
    pub command_id: String,
    /// Ordered typed argument occurrences, never pre-rendered shell text.
    pub arguments: Vec<CommandProposalArgument>,
    /// Human-readable reason for selecting this command and its arguments.
    pub explanation: String,
    /// Attribution for the component that produced this proposal.
    pub provenance: CommandProposalProvenance,
}

/// One positional, valued option, or present flag proposed for a command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandProposalArgument {
    /// One positional argument occurrence.
    Positional {
        /// Catalog argument name used to resolve this occurrence.
        name: String,
        /// Resolved typed value or an explicit unresolved slot.
        value: CommandProposalValue,
    },
    /// One named option occurrence that consumes a value.
    Option {
        /// Catalog option spelling used to resolve this occurrence.
        name: String,
        /// Resolved typed value or an explicit unresolved slot.
        value: CommandProposalValue,
    },
    /// One present boolean flag.
    Flag {
        /// Catalog flag spelling used to resolve this occurrence.
        name: String,
    },
}

/// Typed value of one positional or valued option occurrence.
///
/// Unknown catalog domain types use [`Self::Text`]. Recognized catalog types
/// are checked conservatively: `path`, signed integers, unsigned counts and
/// limits, and Boolean values require their corresponding variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum CommandProposalValue {
    /// Required information is not yet available and must be resolved before rendering.
    Unresolved,
    /// Literal UTF-8 text for a catalog domain without a narrower recognized type.
    Text(String),
    /// Literal UTF-8 filesystem path.
    Path(String),
    /// Signed 64-bit integer.
    Integer(i64),
    /// Unsigned 64-bit integer, including counts, limits, byte sizes, and ports.
    Unsigned(u64),
    /// Boolean value for a value-consuming argument declared as Boolean.
    Boolean(bool),
}

/// Mechanism that produced a command proposal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandProposalSource {
    /// A planner selected the command and constructed typed bindings.
    Planner,
    /// Retrieval selected only a catalog entry and left required values unresolved.
    RetrievalFallback,
}

/// Attribution retained with a command proposal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandProposalProvenance {
    /// Mechanism that produced the proposal.
    pub source: CommandProposalSource,
    /// Stable planner, retriever, model, or adapter identity.
    pub producer: String,
}

/// Confirmation class derived exclusively from trusted catalog effects.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandProposalRisk {
    /// The proposal still requires ordinary explicit user confirmation.
    Ordinary,
    /// Unknown, mutating, process, or session-state effects require high-risk confirmation.
    High,
}

/// Bounded natural-language request passed to a [`CommandPlanner`].
#[derive(Debug, Clone, Copy)]
pub struct CommandPlanningRequest<'a> {
    intent: &'a str,
}

impl<'a> CommandPlanningRequest<'a> {
    /// Validate a non-empty planning intent within the fixed byte limit.
    pub fn new(intent: &'a str) -> Result<Self, ShellError> {
        if intent.trim().is_empty() {
            return Err(validation_error(
                "command planning intent is empty",
                "Describe the task the catalog command should perform",
            ));
        }
        validate_limit(
            "command planning intent bytes",
            intent.len(),
            COMMAND_PLANNING_INTENT_BYTES_MAX,
        )?;
        Ok(Self { intent })
    }

    /// Return the exact natural-language intent supplied by the caller.
    pub const fn intent(&self) -> &'a str {
        self.intent
    }
}

/// Planner boundary that can select catalog identities and typed arguments.
///
/// Implementations cannot return shell source through this interface. Callers
/// must validate the returned [`CommandProposal`] against their admitted
/// catalog before rendering or presenting it for confirmation.
pub trait CommandPlanner {
    /// Produce one inert command proposal for a bounded planning request.
    fn propose(
        &self,
        request: &CommandPlanningRequest<'_>,
        catalog: &Catalog,
    ) -> Result<CommandProposal, ShellError>;
}

/// Proposal that resolved against a specific catalog snapshot.
///
/// Fields remain private so only validation can construct this trust marker.
/// Rendering does not grant execution authority and every risk class still
/// requires explicit confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCommandProposal {
    proposal: CommandProposal,
    command_path: String,
    arguments: Vec<ValidatedArgument>,
    effects: Vec<Effect>,
    risk: CommandProposalRisk,
}

impl CommandProposal {
    /// Decode a bounded deny-unknown JSON proposal without consulting a catalog.
    ///
    /// Call [`Self::validate`] before treating any decoded field as catalog-backed.
    pub fn from_json(source: &str) -> Result<Self, ShellError> {
        validate_limit(
            "serialized command proposal bytes",
            source.len(),
            COMMAND_PROPOSAL_SOURCE_BYTES_MAX,
        )?;
        serde_json::from_str(source).map_err(|error| {
            ShellError::new(ErrorCode::Validation, "command proposal JSON is invalid")
                .with_context(error.to_string())
                .with_help("Emit exactly the documented deny-unknown command proposal schema")
        })
    }

    /// Construct a retrieval-only fallback with every required value left unresolved.
    ///
    /// The command identifier is resolved exactly against `catalog`. Optional
    /// arguments are omitted; required flags are present, while required
    /// positional and valued options become explicit unresolved slots.
    pub fn retrieval_fallback(
        catalog: &Catalog,
        command_id: impl Into<String>,
        explanation: impl Into<String>,
        producer: impl Into<String>,
    ) -> Result<Self, ShellError> {
        let command_id = command_id.into();
        let command = resolve_exact_command(catalog, &command_id)?;
        validate_catalog_argument_count(command)?;
        let mut arguments = Vec::new();
        for argument in command.options.iter().filter(|argument| argument.required) {
            let name = canonical_argument_name(command, argument)?.to_owned();
            arguments.push(match argument.kind {
                ArgumentKind::Positional => CommandProposalArgument::Positional {
                    name,
                    value: CommandProposalValue::Unresolved,
                },
                ArgumentKind::Option => CommandProposalArgument::Option {
                    name,
                    value: CommandProposalValue::Unresolved,
                },
                ArgumentKind::Flag => CommandProposalArgument::Flag { name },
            });
        }
        let proposal = Self {
            schema_version: COMMAND_PROPOSAL_SCHEMA_VERSION,
            command_id,
            arguments,
            explanation: explanation.into(),
            provenance: CommandProposalProvenance {
                source: CommandProposalSource::RetrievalFallback,
                producer: producer.into(),
            },
        };
        proposal.validate(catalog)?;
        Ok(proposal)
    }

    /// Validate identity, bounds, arguments, and catalog-derived effects.
    pub fn validate(&self, catalog: &Catalog) -> Result<ValidatedCommandProposal, ShellError> {
        validate_proposal_envelope(self)?;
        let command = resolve_exact_command(catalog, &self.command_id)?;
        validate_catalog_argument_count(command)?;
        validate_command_path(command)?;

        let mut occurrence_counts = vec![0_usize; command.options.len()];
        let mut aggregate_value_bytes = 0_usize;
        let mut arguments = Vec::with_capacity(self.arguments.len());
        for proposed in &self.arguments {
            let (name, kind, value) = proposed_parts(proposed);
            validate_nonempty_bounded(
                "proposal argument name",
                name,
                COMMAND_PROPOSAL_PRODUCER_BYTES_MAX,
            )?;
            let (argument_index, specification) = resolve_argument(command, name, kind)?;
            occurrence_counts[argument_index] = occurrence_counts[argument_index]
                .checked_add(1)
                .ok_or_else(|| {
                    resource_error(
                        "proposal argument occurrence count overflowed",
                        "Reduce repeated argument occurrences",
                    )
                })?;
            if occurrence_counts[argument_index] > 1 && !specification.repeatable {
                return Err(argument_error(
                    command,
                    format!("argument `{name}` is not repeatable"),
                    "Remove the duplicate argument occurrence",
                ));
            }

            let canonical_name = canonical_argument_name(command, specification)?.to_owned();
            let validated = match (kind, value) {
                (ArgumentKind::Flag, None) => ValidatedArgument::Flag {
                    name: canonical_name,
                },
                (ArgumentKind::Positional, Some(value)) => {
                    validate_value(command, specification, value, &mut aggregate_value_bytes)?;
                    ValidatedArgument::Positional {
                        name: canonical_name,
                        value: value.clone(),
                    }
                }
                (ArgumentKind::Option, Some(value)) => {
                    validate_value(command, specification, value, &mut aggregate_value_bytes)?;
                    ValidatedArgument::Option {
                        name: canonical_name,
                        value: value.clone(),
                    }
                }
                (ArgumentKind::Flag, Some(_))
                | (ArgumentKind::Positional | ArgumentKind::Option, None) => {
                    return Err(argument_error(
                        command,
                        format!("argument `{name}` has an inconsistent proposal shape"),
                        "Use flag without a value, or positional/option with a typed value",
                    ));
                }
            };
            arguments.push(validated);
        }

        for (index, specification) in command.options.iter().enumerate() {
            if specification.required && occurrence_counts[index] == 0 {
                let name = canonical_argument_name(command, specification)?;
                return Err(argument_error(
                    command,
                    format!("required argument `{name}` is missing"),
                    "Supply a resolved value or an explicit unresolved slot",
                ));
            }
            if occurrence_counts[index] == 0 {
                continue;
            }
            for conflict in &specification.conflicts {
                if let Some((conflict_index, _)) = command
                    .options
                    .iter()
                    .enumerate()
                    .find(|(_, candidate)| candidate.names.iter().any(|name| name == conflict))
                    && occurrence_counts[conflict_index] > 0
                {
                    let name = canonical_argument_name(command, specification)?;
                    return Err(argument_error(
                        command,
                        format!("argument `{name}` conflicts with `{conflict}`"),
                        "Remove one of the conflicting argument occurrences",
                    ));
                }
            }
        }

        let effects = command.effects.clone();
        let risk = classify_risk(&effects);
        Ok(ValidatedCommandProposal {
            proposal: self.clone(),
            command_path: command.path.clone(),
            arguments,
            effects,
            risk,
        })
    }
}

impl ValidatedCommandProposal {
    /// Return the validated inert proposal.
    pub const fn proposal(&self) -> &CommandProposal {
        &self.proposal
    }

    /// Return the exact catalog command path selected during validation.
    pub fn command_path(&self) -> &str {
        &self.command_path
    }

    /// Return catalog effects used for confirmation policy.
    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    /// Return the confirmation class derived from catalog effects.
    pub const fn risk(&self) -> CommandProposalRisk {
        self.risk
    }

    /// Return whether any argument value remains unresolved.
    pub fn has_unresolved_slots(&self) -> bool {
        self.arguments.iter().any(ValidatedArgument::is_unresolved)
    }

    /// Render one deterministic literal command after all slots are resolved.
    ///
    /// Command words and canonical argument spellings come from the catalog.
    /// Every emitted word is POSIX-compatible single-quoted text, including
    /// empty values and values containing shell operators. Rendering grants no
    /// execution authority and does not replace the required confirmation.
    pub fn render_trusted(&self) -> Result<String, ShellError> {
        let mut words = self
            .command_path
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for argument in &self.arguments {
            match argument {
                ValidatedArgument::Positional { name, value } => {
                    let value = resolved_value(value)
                        .ok_or_else(|| unresolved_error(&self.command_path, name))?;
                    words.push(value);
                }
                ValidatedArgument::Option { name, value } => {
                    let value = resolved_value(value)
                        .ok_or_else(|| unresolved_error(&self.command_path, name))?;
                    words.push(name.clone());
                    words.push(value);
                }
                ValidatedArgument::Flag { name } => words.push(name.clone()),
            }
        }

        let mut rendered = String::new();
        for (index, word) in words.iter().enumerate() {
            if index > 0 {
                rendered.push(' ');
            }
            push_single_quoted(&mut rendered, word);
            validate_limit(
                "rendered command proposal bytes",
                rendered.len(),
                COMMAND_PROPOSAL_RENDER_BYTES_MAX,
            )?;
        }
        Ok(rendered)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ValidatedArgument {
    Positional {
        name: String,
        value: CommandProposalValue,
    },
    Option {
        name: String,
        value: CommandProposalValue,
    },
    Flag {
        name: String,
    },
}

impl ValidatedArgument {
    fn is_unresolved(&self) -> bool {
        matches!(
            self,
            Self::Positional {
                value: CommandProposalValue::Unresolved,
                ..
            } | Self::Option {
                value: CommandProposalValue::Unresolved,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeclaredValueKind {
    Text,
    Path,
    Integer,
    Unsigned,
    Boolean,
}

fn validate_proposal_envelope(proposal: &CommandProposal) -> Result<(), ShellError> {
    if proposal.schema_version != COMMAND_PROPOSAL_SCHEMA_VERSION {
        return Err(validation_error(
            &format!(
                "unsupported command proposal schema version {}",
                proposal.schema_version
            ),
            "Use command proposal schema version 1",
        ));
    }
    let encoded = serde_json::to_vec(proposal).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "command proposal could not be encoded for bounded validation",
        )
        .with_context(error.to_string())
        .with_help("Use only values supported by the documented command proposal schema")
    })?;
    validate_limit(
        "serialized command proposal bytes",
        encoded.len(),
        COMMAND_PROPOSAL_SOURCE_BYTES_MAX,
    )?;
    validate_nonempty_bounded(
        "command proposal id",
        &proposal.command_id,
        COMMAND_PROPOSAL_PRODUCER_BYTES_MAX,
    )?;
    validate_limit(
        "command proposal arguments",
        proposal.arguments.len(),
        COMMAND_PROPOSAL_ARGUMENTS_MAX,
    )?;
    validate_nonempty_bounded(
        "command proposal explanation",
        &proposal.explanation,
        COMMAND_PROPOSAL_EXPLANATION_BYTES_MAX,
    )?;
    validate_nonempty_bounded(
        "command proposal producer",
        &proposal.provenance.producer,
        COMMAND_PROPOSAL_PRODUCER_BYTES_MAX,
    )
}

fn resolve_exact_command<'a>(
    catalog: &'a Catalog,
    command_id: &str,
) -> Result<&'a CommandSpec, ShellError> {
    let mut matches = catalog
        .commands
        .iter()
        .filter(|command| command.id == command_id);
    let Some(command) = matches.next() else {
        return Err(validation_error(
            &format!("command proposal references unknown catalog id `{command_id}`"),
            "Select an exact command id from the supplied catalog",
        ));
    };
    if matches.next().is_some() {
        return Err(validation_error(
            &format!("catalog id `{command_id}` is ambiguous"),
            "Repair duplicate stable command ids before accepting proposals",
        ));
    }
    Ok(command)
}

fn validate_catalog_argument_count(command: &CommandSpec) -> Result<(), ShellError> {
    validate_limit(
        "catalog arguments inspected for command proposal",
        command.options.len(),
        COMMAND_PROPOSAL_CATALOG_ARGUMENTS_MAX,
    )
}

fn validate_command_path(command: &CommandSpec) -> Result<(), ShellError> {
    validate_nonempty_bounded(
        "catalog command path",
        &command.path,
        COMMAND_PROPOSAL_VALUE_BYTES_MAX,
    )?;
    if command.path.contains('\0') || command.path.split_whitespace().next().is_none() {
        return Err(validation_error(
            &format!(
                "catalog command `{}` has an invalid executable path",
                command.id
            ),
            "Repair the catalog command path before accepting proposals",
        ));
    }
    Ok(())
}

fn proposed_parts(
    argument: &CommandProposalArgument,
) -> (&str, ArgumentKind, Option<&CommandProposalValue>) {
    match argument {
        CommandProposalArgument::Positional { name, value } => {
            (name, ArgumentKind::Positional, Some(value))
        }
        CommandProposalArgument::Option { name, value } => {
            (name, ArgumentKind::Option, Some(value))
        }
        CommandProposalArgument::Flag { name } => (name, ArgumentKind::Flag, None),
    }
}

fn resolve_argument<'a>(
    command: &'a CommandSpec,
    name: &str,
    proposed_kind: ArgumentKind,
) -> Result<(usize, &'a quirl_catalog::ArgumentSpec), ShellError> {
    let mut matches = command
        .options
        .iter()
        .enumerate()
        .filter(|(_, argument)| argument.names.iter().any(|candidate| candidate == name));
    let Some((index, argument)) = matches.next() else {
        return Err(argument_error(
            command,
            format!("unknown argument `{name}`"),
            "Use an argument spelling from the selected catalog command",
        ));
    };
    if matches.next().is_some() {
        return Err(argument_error(
            command,
            format!("argument name `{name}` is ambiguous"),
            "Repair duplicate argument names in the catalog",
        ));
    }
    if argument.kind != proposed_kind {
        return Err(argument_error(
            command,
            format!(
                "argument `{name}` was proposed as {proposed_kind:?} but the catalog declares {:?}",
                argument.kind
            ),
            "Use the proposal argument variant declared by the catalog",
        ));
    }
    Ok((index, argument))
}

fn canonical_argument_name<'a>(
    command: &CommandSpec,
    argument: &'a quirl_catalog::ArgumentSpec,
) -> Result<&'a str, ShellError> {
    argument
        .names
        .first()
        .map(String::as_str)
        .filter(|name| !name.trim().is_empty() && !name.contains('\0'))
        .ok_or_else(|| {
            argument_error(
                command,
                "catalog argument has no valid canonical name".to_owned(),
                "Repair the catalog argument before accepting proposals",
            )
        })
}

fn validate_value(
    command: &CommandSpec,
    specification: &quirl_catalog::ArgumentSpec,
    value: &CommandProposalValue,
    aggregate_value_bytes: &mut usize,
) -> Result<(), ShellError> {
    let Some(literal) = resolved_value(value) else {
        return Ok(());
    };
    if literal.contains('\0') {
        return Err(argument_error(
            command,
            format!(
                "argument `{}` contains an interior NUL byte",
                canonical_argument_name(command, specification)?
            ),
            "Remove the NUL byte from the resolved value",
        ));
    }
    validate_limit(
        "command proposal argument value bytes",
        literal.len(),
        COMMAND_PROPOSAL_VALUE_BYTES_MAX,
    )?;
    *aggregate_value_bytes = aggregate_value_bytes
        .checked_add(literal.len())
        .ok_or_else(|| {
            resource_error(
                "command proposal aggregate value bytes overflowed",
                "Reduce the number or size of resolved values",
            )
        })?;
    validate_limit(
        "command proposal aggregate value bytes",
        *aggregate_value_bytes,
        COMMAND_PROPOSAL_VALUES_BYTES_MAX,
    )?;

    let expected = declared_value_kind(&specification.value_type);
    let observed = proposal_value_kind(value);
    if expected != observed {
        return Err(argument_error(
            command,
            format!(
                "argument `{}` expects {} but the proposal supplied {}",
                canonical_argument_name(command, specification)?,
                declared_kind_name(expected),
                declared_kind_name(observed)
            ),
            "Use the typed proposal value required by the catalog value_type",
        ));
    }
    if let Some(CompletionSource::Static { values }) = &specification.values
        && !values.iter().any(|candidate| candidate == &literal)
    {
        return Err(argument_error(
            command,
            format!(
                "argument `{}` value `{literal}` is not in its static catalog set",
                canonical_argument_name(command, specification)?
            ),
            "Choose one of the static values declared by the catalog",
        ));
    }
    Ok(())
}

fn declared_value_kind(value_type: &str) -> DeclaredValueKind {
    match value_type.trim().to_ascii_lowercase().as_str() {
        "path" => DeclaredValueKind::Path,
        "int" | "integer" | "i64" | "status" => DeclaredValueKind::Integer,
        "uint" | "unsigned" | "u64" | "count" | "limit" | "bytes" | "port" => {
            DeclaredValueKind::Unsigned
        }
        "bool" | "boolean" => DeclaredValueKind::Boolean,
        _ => DeclaredValueKind::Text,
    }
}

fn proposal_value_kind(value: &CommandProposalValue) -> DeclaredValueKind {
    match value {
        CommandProposalValue::Unresolved | CommandProposalValue::Text(_) => DeclaredValueKind::Text,
        CommandProposalValue::Path(_) => DeclaredValueKind::Path,
        CommandProposalValue::Integer(_) => DeclaredValueKind::Integer,
        CommandProposalValue::Unsigned(_) => DeclaredValueKind::Unsigned,
        CommandProposalValue::Boolean(_) => DeclaredValueKind::Boolean,
    }
}

fn declared_kind_name(kind: DeclaredValueKind) -> &'static str {
    match kind {
        DeclaredValueKind::Text => "text",
        DeclaredValueKind::Path => "path",
        DeclaredValueKind::Integer => "integer",
        DeclaredValueKind::Unsigned => "unsigned integer",
        DeclaredValueKind::Boolean => "Boolean",
    }
}

fn resolved_value(value: &CommandProposalValue) -> Option<String> {
    match value {
        CommandProposalValue::Unresolved => None,
        CommandProposalValue::Text(value) | CommandProposalValue::Path(value) => {
            Some(value.clone())
        }
        CommandProposalValue::Integer(value) => Some(value.to_string()),
        CommandProposalValue::Unsigned(value) => Some(value.to_string()),
        CommandProposalValue::Boolean(value) => Some(value.to_string()),
    }
}

fn classify_risk(effects: &[Effect]) -> CommandProposalRisk {
    if !effects.is_empty()
        && effects
            .iter()
            .all(|effect| matches!(effect, Effect::ReadFilesystem))
    {
        CommandProposalRisk::Ordinary
    } else {
        CommandProposalRisk::High
    }
}

fn push_single_quoted(output: &mut String, value: &str) {
    output.push('\'');
    for character in value.chars() {
        if character == '\'' {
            output.push_str("'\\''");
        } else {
            output.push(character);
        }
    }
    output.push('\'');
}

fn unresolved_error(command_path: &str, name: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("command proposal argument `{name}` is unresolved"),
    )
    .with_command(command_path)
    .with_help("Resolve every explicit slot before rendering the command")
}

fn validate_nonempty_bounded(label: &str, value: &str, limit: usize) -> Result<(), ShellError> {
    if value.trim().is_empty() {
        return Err(validation_error(
            &format!("{label} is empty"),
            "Supply a non-empty value",
        ));
    }
    validate_limit(&format!("{label} bytes"), value.len(), limit)
}

fn validate_limit(label: &str, observed: usize, limit: usize) -> Result<(), ShellError> {
    if observed <= limit {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{label} exceeded its limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Reduce the proposal to fit within the documented bound"))
}

fn validation_error(message: &str, help: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help(help)
}

fn argument_error(command: &CommandSpec, message: String, help: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message)
        .with_command(&command.path)
        .with_context(format!("catalog command id: {}", command.id))
        .with_help(help)
}

fn resource_error(message: &str, help: &str) -> ShellError {
    ShellError::new(ErrorCode::ResourceLimit, message).with_help(help)
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_catalog::{ArgumentSpec, Confidence, IoContract, Provenance, ProvenanceInfo, Trust};
    use std::collections::BTreeMap;

    fn provenance() -> ProvenanceInfo {
        ProvenanceInfo {
            source: Provenance::Builtin,
            confidence: Confidence::Exact,
            trust: Trust::Builtin,
            origin: None,
            fingerprint: None,
            generated_at: None,
        }
    }

    fn argument(
        names: &[&str],
        kind: ArgumentKind,
        value_type: &str,
        required: bool,
        repeatable: bool,
    ) -> ArgumentSpec {
        ArgumentSpec {
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            kind,
            value_type: value_type.to_owned(),
            required,
            repeatable,
            values: None,
            conflicts: Vec::new(),
            documentation: "Fixture argument".to_owned(),
            examples: vec!["demo run value".to_owned()],
            provenance: provenance(),
        }
    }

    fn catalog(effects: Vec<Effect>) -> Catalog {
        let mut count = argument(&["--count"], ArgumentKind::Option, "count", false, false);
        count.values = Some(CompletionSource::Static {
            values: vec!["1".to_owned(), "2".to_owned()],
        });
        let mut force = argument(&["--force"], ArgumentKind::Flag, "Bool", false, false);
        force.conflicts = vec!["--dry-run".to_owned()];
        Catalog {
            schema_version: quirl_catalog::CATALOG_SCHEMA_VERSION,
            commands: vec![CommandSpec {
                id: "command:demo/run".to_owned(),
                version: Some("1.0.0".to_owned()),
                path: "demo run".to_owned(),
                aliases: Vec::new(),
                parent: None,
                signature: "demo run <path> [--count count] [--force] [--dry-run]".to_owned(),
                summary: "Run the proposal fixture".to_owned(),
                details: "Exercises typed proposal validation.".to_owned(),
                options: vec![
                    argument(&["path"], ArgumentKind::Positional, "path", true, false),
                    count,
                    force,
                    argument(&["--dry-run"], ArgumentKind::Flag, "Bool", false, false),
                ],
                examples: vec!["demo run .".to_owned()],
                io: IoContract {
                    input: "Nothing".to_owned(),
                    output: "Bytes".to_owned(),
                    streaming: false,
                },
                effects,
                exit_codes: BTreeMap::from([(0, "success".to_owned())]),
                provenance: provenance(),
            }],
        }
    }

    fn base_proposal() -> CommandProposal {
        CommandProposal {
            schema_version: COMMAND_PROPOSAL_SCHEMA_VERSION,
            command_id: "command:demo/run".to_owned(),
            arguments: vec![CommandProposalArgument::Positional {
                name: "path".to_owned(),
                value: CommandProposalValue::Path("fixture".to_owned()),
            }],
            explanation: "Use the exact catalog fixture command.".to_owned(),
            provenance: CommandProposalProvenance {
                source: CommandProposalSource::Planner,
                producer: "fixture-planner-v1".to_owned(),
            },
        }
    }

    #[test]
    fn valid_proposal_renders_only_catalog_words_and_single_quoted_literals() {
        let mut proposal = base_proposal();
        proposal.arguments = vec![
            CommandProposalArgument::Positional {
                name: "path".to_owned(),
                value: CommandProposalValue::Path("a'b;$(touch nope)".to_owned()),
            },
            CommandProposalArgument::Option {
                name: "--count".to_owned(),
                value: CommandProposalValue::Unsigned(2),
            },
            CommandProposalArgument::Flag {
                name: "--force".to_owned(),
            },
        ];
        let validated = proposal
            .validate(&catalog(vec![Effect::ReadFilesystem]))
            .unwrap();
        assert_eq!(validated.risk(), CommandProposalRisk::Ordinary);
        assert!(!validated.has_unresolved_slots());
        assert_eq!(
            validated.render_trusted().unwrap(),
            "'demo' 'run' 'a'\\''b;$(touch nope)' '--count' '2' '--force'"
        );
    }

    #[test]
    fn invalid_identity_type_conflict_cardinality_and_static_values_fail_closed() {
        let source = catalog(vec![]);

        let mut unknown = base_proposal();
        unknown.command_id = "command:missing".to_owned();
        assert_eq!(
            unknown.validate(&source).unwrap_err().code,
            ErrorCode::Validation
        );

        let mut wrong_type = base_proposal();
        wrong_type.arguments.push(CommandProposalArgument::Option {
            name: "--count".to_owned(),
            value: CommandProposalValue::Text("1".to_owned()),
        });
        assert!(
            wrong_type
                .validate(&source)
                .unwrap_err()
                .message
                .contains("expects")
        );

        let mut invalid_static = base_proposal();
        invalid_static
            .arguments
            .push(CommandProposalArgument::Option {
                name: "--count".to_owned(),
                value: CommandProposalValue::Unsigned(3),
            });
        assert!(
            invalid_static
                .validate(&source)
                .unwrap_err()
                .message
                .contains("static")
        );

        let mut repeated = base_proposal();
        repeated.arguments.extend([
            CommandProposalArgument::Flag {
                name: "--force".to_owned(),
            },
            CommandProposalArgument::Flag {
                name: "--force".to_owned(),
            },
        ]);
        assert!(
            repeated
                .validate(&source)
                .unwrap_err()
                .message
                .contains("repeatable")
        );

        let mut conflicting = base_proposal();
        conflicting.arguments.extend([
            CommandProposalArgument::Flag {
                name: "--force".to_owned(),
            },
            CommandProposalArgument::Flag {
                name: "--dry-run".to_owned(),
            },
        ]);
        assert!(
            conflicting
                .validate(&source)
                .unwrap_err()
                .message
                .contains("conflicts")
        );
    }

    #[test]
    fn proposal_json_rejects_unknown_fields_at_every_nested_boundary() {
        let valid = serde_json::to_value(base_proposal()).unwrap();
        for path in ["proposal", "argument", "value", "provenance"] {
            let mut value = valid.clone();
            match path {
                "proposal" => value["future"] = serde_json::json!(true),
                "argument" => value["arguments"][0]["future"] = serde_json::json!(true),
                "value" => value["arguments"][0]["value"]["future"] = serde_json::json!(true),
                "provenance" => value["provenance"]["future"] = serde_json::json!(true),
                _ => unreachable!(),
            }
            assert!(
                CommandProposal::from_json(&value.to_string()).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn future_proposal_schema_version_fails_closed() {
        let mut proposal = base_proposal();
        proposal.schema_version = COMMAND_PROPOSAL_SCHEMA_VERSION + 1;
        let error = proposal.validate(&catalog(vec![])).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("unsupported"));
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn unresolved_slots_validate_but_cannot_render() {
        let proposal = CommandProposal::retrieval_fallback(
            &catalog(vec![]),
            "command:demo/run",
            "Retrieval selected the closest catalog entry.",
            "fixture-retriever-v1",
        )
        .unwrap();
        assert_eq!(
            proposal.provenance.source,
            CommandProposalSource::RetrievalFallback
        );
        let validated = proposal.validate(&catalog(vec![])).unwrap();
        assert!(validated.has_unresolved_slots());
        assert!(
            validated
                .render_trusted()
                .unwrap_err()
                .message
                .contains("unresolved")
        );
    }

    #[test]
    fn proposal_value_and_source_limits_report_resource_errors() {
        let mut proposal = base_proposal();
        proposal.arguments[0] = CommandProposalArgument::Positional {
            name: "path".to_owned(),
            value: CommandProposalValue::Path("x".repeat(COMMAND_PROPOSAL_VALUE_BYTES_MAX + 1)),
        };
        let error = proposal.validate(&catalog(vec![])).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("observed"));

        let oversized = " ".repeat(COMMAND_PROPOSAL_SOURCE_BYTES_MAX + 1);
        assert_eq!(
            CommandProposal::from_json(&oversized).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn only_explicit_read_only_effects_are_ordinary_risk() {
        for effects in [
            Vec::new(),
            vec![Effect::WriteFilesystem],
            vec![Effect::SpawnProcess],
            vec![Effect::ChangeDirectory],
            vec![Effect::ReadFilesystem, Effect::SpawnProcess],
        ] {
            assert_eq!(
                base_proposal().validate(&catalog(effects)).unwrap().risk(),
                CommandProposalRisk::High
            );
        }
        assert_eq!(
            base_proposal()
                .validate(&catalog(vec![Effect::ReadFilesystem]))
                .unwrap()
                .risk(),
            CommandProposalRisk::Ordinary
        );
    }
}
