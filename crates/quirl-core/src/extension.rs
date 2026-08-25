use crate::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

/// Current wire version for extension events, actions, and registrations.
pub const EXTENSION_PROTOCOL_VERSION: u32 = 2;
/// Inclusive upper bound, in milliseconds, for one extension callback.
pub const MAX_EXTENSION_DEADLINE_MS: u64 = 250;
/// Canonical structural description used to fingerprint the extension protocol.
///
/// Change this descriptor whenever a serialized shape, semantic rule, or bound
/// changes; [`extension_schema_hash`] derives the advertised identity from it.
pub const EXTENSION_SCHEMA_DESCRIPTOR: &str = "quirl.extension@2{ExtensionEvent{deny_unknown;protocol_version:u32;sequence:u64;data:ExtensionEventData};ExtensionEventData:tag(kind)[session_start{restored:bool}|session_restore{session_id:string}|directory_changed{previous:string,current:string}|command_plan{source:string,effects:array<string>}|execution_progress{completed:u64,total:null|u64,message:null|terminal-safe-string}|output{stream:stdout|stderr,bytes:u64,text:null|terminal-safe-string}|cancellation{reason:terminal-safe-string}|result{status:i32,duration_ms:u64}|error{error:ShellError}];ExtensionAction:tag(action)[diagnose{message:string}|rewrite_plan{source:string}|set_environment{name:string,value:string}|block_execution{reason:string}|annotate_result{key:string,value:Value{nodes<=100000;depth<=64;text_bytes<=8388608;terminal-safe-keys-and-strings}}];EventSubscription{deny_unknown;name:string;events:unique-array<EventKind>;capabilities:array<ExtensionCapability>;deadline_ms:1..250};ContributionRegistration{deny_unknown;kind:catalog|completion|panel;name:string;deadline_ms:1..250;plain_fallback:null|string};capabilities:events_observe|plan_rewrite|environment_mutate|output_read|execution_block|catalog_contribute|completion_contribute|ui_panel;event_sequence:strictly-increasing}";
/// Historical extension protocol descriptor retained for version-1 identity checks.
pub const EXTENSION_SCHEMA_V1_DESCRIPTOR: &str = "quirl.extension@1{ExtensionEvent{deny_unknown;protocol_version:u32;sequence:u64;data:ExtensionEventData};ExtensionEventData:tag(kind)[session_start{restored:bool}|session_restore{session_id:string}|directory_changed{previous:string,current:string}|command_plan{source:string,effects:array<string>}|execution_progress{completed:u64,total:null|u64,message:null|string}|output{stream:stdout|stderr,bytes:usize,text:null|string}|cancellation{reason:string}|result{status:i32,duration_ms:u64}|error{error:ShellError}];ExtensionAction:tag(action)[diagnose{message:string}|rewrite_plan{source:string}|set_environment{name:string,value:string}|block_execution{reason:string}|annotate_result{key:string,value:Value}];EventSubscription{deny_unknown;name:string;events:unique-array<EventKind>;capabilities:array<ExtensionCapability>;deadline_ms:1..250};ContributionRegistration{deny_unknown;kind:catalog|completion|panel;name:string;deadline_ms:1..250;plain_fallback:null|string};capabilities:events_observe|plan_rewrite|environment_mutate|output_read|execution_block|catalog_contribute|completion_contribute|ui_panel;event_sequence:strictly-increasing}";
/// Maximum container and scalar nodes scanned in one untrusted JSON value.
pub const JSON_TERMINAL_VALUE_NODES_MAX: usize = 100_000;
/// Maximum nesting depth scanned in one untrusted JSON value.
pub const JSON_TERMINAL_VALUE_DEPTH_MAX: usize = 64;
/// Maximum aggregate UTF-8 bytes scanned across JSON keys and string leaves.
pub const JSON_TERMINAL_VALUE_TEXT_BYTES_MAX: usize = 8 * 1024 * 1024;

/// Return the deterministic compatibility fingerprint for the extension protocol.
pub fn extension_schema_hash() -> String {
    crate::schema_fingerprint(EXTENSION_SCHEMA_DESCRIPTOR)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
/// Event categories that an extension may subscribe to.
pub enum EventKind {
    /// A new session has reached a stable point where callbacks may run.
    SessionStart,
    /// Persisted session state has been restored and identified.
    SessionRestore,
    /// The process working directory changed successfully.
    DirectoryChanged,
    /// A command plan is complete and available for observation or rewriting.
    CommandPlan,
    /// A bounded execution unit reported incremental progress.
    ExecutionProgress,
    /// A process produced stdout or stderr data.
    Output,
    /// Cancellation was requested or observed at a safe point.
    Cancellation,
    /// Execution completed with a process-style status.
    Result,
    /// Execution or extension processing produced a structured error.
    Error,
}

impl EventKind {
    /// Every event category in stable declaration order.
    pub const ALL: [Self; 9] = [
        Self::SessionStart,
        Self::SessionRestore,
        Self::DirectoryChanged,
        Self::CommandPlan,
        Self::ExecutionProgress,
        Self::Output,
        Self::Cancellation,
        Self::Result,
        Self::Error,
    ];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
/// Typed payload carried by an [`ExtensionEvent`].
///
/// Events are immutable observations of committed host state. The host must not
/// dispatch them while a process graph, terminal handoff, or persisted record is
/// only partially committed.
pub enum ExtensionEventData {
    /// Announces a ready session.
    SessionStart {
        /// Whether the session incorporated previously persisted state.
        restored: bool,
    },
    /// Identifies a restored persisted session.
    SessionRestore {
        /// Stable host-assigned identifier for the restored session.
        session_id: String,
    },
    /// Reports an atomic working-directory transition.
    DirectoryChanged {
        /// Directory active before the successful transition.
        previous: String,
        /// Directory active after the successful transition.
        current: String,
    },
    /// Exposes a complete command plan at the pre-execution safe point.
    CommandPlan {
        /// Command source that the host proposes to execute.
        source: String,
        /// Declared effect names associated with the plan.
        effects: Vec<String>,
    },
    /// Reports bounded progress for an in-flight execution.
    ExecutionProgress {
        /// Number of work units completed so far.
        completed: u64,
        /// Total work units when the producer can determine the bound.
        total: Option<u64>,
        /// Optional terminal-safe progress description.
        message: Option<String>,
    },
    /// Reports process output metadata and optional decoded text.
    Output {
        /// Stream that produced the data.
        stream: OutputStream,
        /// Number of raw bytes represented by this event.
        ///
        /// Serialization uses a fixed-width `u64`; this in-memory count is a
        /// checked projection of the wire value.
        #[serde(with = "crate::error::wire_usize")]
        bytes: usize,
        /// Decoded, terminal-safe text when the host elects to expose content.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Reports why cancellation was requested or observed.
    Cancellation {
        /// Terminal-safe human-readable cancellation reason.
        reason: String,
    },
    /// Reports final command completion.
    Result {
        /// Process-style exit status; zero conventionally indicates success.
        status: i32,
        /// Observed wall duration in milliseconds.
        duration_ms: u64,
    },
    /// Carries a structured host or extension failure.
    Error {
        /// Stable diagnostic data without terminal decoration.
        error: ShellError,
    },
}

impl ExtensionEventData {
    /// Return the subscription category corresponding to this payload variant.
    pub fn kind(&self) -> EventKind {
        match self {
            Self::SessionStart { .. } => EventKind::SessionStart,
            Self::SessionRestore { .. } => EventKind::SessionRestore,
            Self::DirectoryChanged { .. } => EventKind::DirectoryChanged,
            Self::CommandPlan { .. } => EventKind::CommandPlan,
            Self::ExecutionProgress { .. } => EventKind::ExecutionProgress,
            Self::Output { .. } => EventKind::Output,
            Self::Cancellation { .. } => EventKind::Cancellation,
            Self::Result { .. } => EventKind::Result,
            Self::Error { .. } => EventKind::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Process output stream identified by an [`ExtensionEventData::Output`] event.
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// Versioned, strictly ordered observation delivered to extension handlers.
pub struct ExtensionEvent {
    /// Wire version used to encode this event.
    pub protocol_version: u32,
    /// Monotonically increasing sequence number within the producing session.
    pub sequence: u64,
    /// Immutable event payload.
    pub data: ExtensionEventData,
}

impl ExtensionEvent {
    /// Construct an event using the current protocol version.
    ///
    /// The caller owns sequence allocation and must later call
    /// [`Self::validate_after`] at the reader boundary.
    pub fn new(sequence: u64, data: ExtensionEventData) -> Self {
        Self {
            protocol_version: EXTENSION_PROTOCOL_VERSION,
            sequence,
            data,
        }
    }

    /// Validate the protocol version, sequence ordering, and exposed terminal text.
    ///
    /// `previous_sequence` is the last accepted sequence for the same session;
    /// `None` marks the first event. Returns [`ErrorCode::Validation`] when the
    /// version is unsupported, the sequence does not increase strictly, or an
    /// progress/output/cancellation string contains terminal controls.
    pub fn validate_after(&self, previous_sequence: Option<u64>) -> Result<(), ShellError> {
        if self.protocol_version != EXTENSION_PROTOCOL_VERSION {
            return Err(extension_error(format!(
                "unsupported extension event protocol version {}",
                self.protocol_version
            )));
        }
        if previous_sequence.is_some_and(|previous| self.sequence <= previous) {
            return Err(extension_error(format!(
                "extension event sequence {} is not greater than its predecessor",
                self.sequence
            )));
        }
        if let ExtensionEventData::Output {
            text: Some(text), ..
        } = &self.data
        {
            reject_terminal_controls("extension output", text)?;
        }
        if let ExtensionEventData::ExecutionProgress {
            message: Some(message),
            ..
        } = &self.data
        {
            reject_terminal_controls("extension progress message", message)?;
        }
        if let ExtensionEventData::Cancellation { reason } = &self.data {
            reject_terminal_controls("extension cancellation reason", reason)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
/// Explicit authority that an extension may request and a host may grant.
///
/// Possessing a value describes authority; it does not itself perform the
/// operation. Hosts must validate every action at the boundary.
pub enum ExtensionCapability {
    /// Observe subscribed host events and attach passive diagnostics or annotations.
    EventsObserve,
    /// Replace a complete command plan before execution begins.
    PlanRewrite,
    /// Add or replace environment entries on a pending plan.
    EnvironmentMutate,
    /// Observe process output content rather than byte counts alone.
    OutputRead,
    /// Prevent a pending execution from starting.
    ExecutionBlock,
    /// Add validated semantic command metadata.
    CatalogContribute,
    /// Supply bounded completion candidates.
    CompletionContribute,
    /// Supply typed user-interface panel content.
    UiPanel,
}

impl ExtensionCapability {
    /// Every extension capability in stable declaration order.
    pub const ALL: [Self; 8] = [
        Self::EventsObserve,
        Self::PlanRewrite,
        Self::EnvironmentMutate,
        Self::OutputRead,
        Self::ExecutionBlock,
        Self::CatalogContribute,
        Self::CompletionContribute,
        Self::UiPanel,
    ];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
/// A requested host mutation or passive observation result from an extension.
///
/// The composition root applies actions only after [`Self::validate`] confirms
/// both the required grant and the action-specific data constraints.
pub enum ExtensionAction {
    /// Attach a non-fatal diagnostic to the current event.
    Diagnose {
        /// Terminal-safe diagnostic text.
        message: String,
    },
    /// Replace the pending command plan as one atomic action.
    RewritePlan {
        /// Complete terminal-safe command source for the replacement plan.
        source: String,
    },
    /// Add or replace one environment entry on the pending plan.
    SetEnvironment {
        /// Portable entry name using the extension identifier character set.
        name: String,
        /// Entry value, which must not contain a NUL byte.
        value: String,
    },
    /// Prevent the pending execution from starting.
    BlockExecution {
        /// Terminal-safe explanation presented to the caller.
        reason: String,
    },
    /// Add typed metadata to the eventual execution result.
    AnnotateResult {
        /// Portable annotation identifier.
        key: String,
        /// JSON value whose object keys and strings must be terminal-safe.
        value: Value,
    },
}

impl ExtensionAction {
    /// Return the grant that authorizes this action.
    ///
    /// Passive diagnostics and result annotations require event-observation
    /// authority; every mutating action has its own narrower capability.
    pub fn required_capability(&self) -> ExtensionCapability {
        match self {
            Self::Diagnose { .. } | Self::AnnotateResult { .. } => {
                ExtensionCapability::EventsObserve
            }
            Self::RewritePlan { .. } => ExtensionCapability::PlanRewrite,
            Self::SetEnvironment { .. } => ExtensionCapability::EnvironmentMutate,
            Self::BlockExecution { .. } => ExtensionCapability::ExecutionBlock,
        }
    }

    /// Validate authority and untrusted values before applying this action.
    ///
    /// Returns [`ErrorCode::Validation`] if the required capability is absent,
    /// identifier syntax is invalid, environment data contains NUL, or text/JSON
    /// could inject terminal controls. This method never applies the action.
    pub fn validate(&self, granted: &[ExtensionCapability]) -> Result<(), ShellError> {
        let required = self.required_capability();
        if !granted.contains(&required) {
            return Err(extension_error(format!(
                "extension action requires undeclared capability `{}`",
                capability_name(required)
            )));
        }
        match self {
            Self::Diagnose { message } => reject_terminal_controls("diagnostic", message),
            Self::RewritePlan { source } => reject_terminal_controls("rewritten plan", source),
            Self::SetEnvironment { name, value } => {
                validate_name("environment name", name)?;
                if value.contains('\0') {
                    return Err(extension_error("environment value contains a NUL byte"));
                }
                Ok(())
            }
            Self::BlockExecution { reason } => reject_terminal_controls("block reason", reason),
            Self::AnnotateResult { key, value } => {
                validate_name("annotation key", key)?;
                reject_json_terminal_controls("result annotation", value)
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Bounded declaration of one extension event handler.
pub struct EventSubscription {
    /// Portable handler identifier, unique within its declaring extension.
    pub name: String,
    /// Non-empty, duplicate-free event categories delivered to the handler.
    pub events: Vec<EventKind>,
    /// Authorities declared by the handler; event observation is mandatory.
    pub capabilities: Vec<ExtensionCapability>,
    /// Per-callback wall-time budget in milliseconds, inclusive range `1..=250`.
    pub deadline_ms: u64,
}

impl EventSubscription {
    /// Validate handler naming, event uniqueness, deadline bounds, and authority.
    ///
    /// Returns [`ErrorCode::Validation`] for malformed declarations. Successful
    /// validation does not grant capabilities; the host remains responsible for
    /// comparing this declaration with the approved policy.
    pub fn validate(&self) -> Result<(), ShellError> {
        validate_name("event handler name", &self.name)?;
        if self.events.is_empty() {
            return Err(extension_error(
                "event handler must subscribe to at least one event",
            ));
        }
        if !(1..=MAX_EXTENSION_DEADLINE_MS).contains(&self.deadline_ms) {
            return Err(extension_error(format!(
                "event handler deadline_ms must be between 1 and {MAX_EXTENSION_DEADLINE_MS}"
            )));
        }
        let unique = self.events.iter().copied().collect::<HashSet<_>>();
        if unique.len() != self.events.len() {
            return Err(extension_error(
                "event handler contains duplicate event kinds",
            ));
        }
        if !self
            .capabilities
            .contains(&ExtensionCapability::EventsObserve)
        {
            return Err(extension_error(
                "event handler must declare the `events_observe` capability",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
/// Semantic surface to which an extension may contribute validated data.
pub enum ContributionKind {
    /// Command catalog metadata consumed by help, completion, docs, and agents.
    Catalog,
    /// Completion candidates or providers.
    Completion,
    /// Typed user-interface panel content with a plain-text fallback.
    Panel,
}

impl ContributionKind {
    /// Every contribution kind in stable declaration order.
    pub const ALL: [Self; 3] = [Self::Catalog, Self::Completion, Self::Panel];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Bounded declaration of one named extension contribution.
pub struct ContributionRegistration {
    /// Surface that consumes the contribution.
    pub kind: ContributionKind,
    /// Portable name, unique among registrations of the same [`Self::kind`].
    pub name: String,
    /// Per-callback wall-time budget in milliseconds, inclusive range `1..=250`.
    pub deadline_ms: u64,
    /// Terminal-safe fallback for non-panel hosts; mandatory and non-empty for panels.
    #[serde(default)]
    pub plain_fallback: Option<String>,
}

impl ContributionRegistration {
    /// Validate identifier syntax, deadline bounds, and fallback safety.
    ///
    /// Returns [`ErrorCode::Validation`] when the declaration is malformed. Set
    /// uniqueness is intentionally checked by [`validate_contribution_set`].
    pub fn validate(&self) -> Result<(), ShellError> {
        validate_name("contribution name", &self.name)?;
        if !(1..=MAX_EXTENSION_DEADLINE_MS).contains(&self.deadline_ms) {
            return Err(extension_error(format!(
                "contribution deadline_ms must be between 1 and {MAX_EXTENSION_DEADLINE_MS}"
            )));
        }
        if matches!(self.kind, ContributionKind::Panel)
            && self.plain_fallback.as_deref().is_none_or(str::is_empty)
        {
            return Err(extension_error(
                "panel contributions require a non-empty plain_fallback",
            ));
        }
        if let Some(plain) = &self.plain_fallback {
            reject_terminal_controls("plain fallback", plain)?;
        }
        Ok(())
    }
}

/// Validate registrations and reject duplicate `(kind, name)` identities.
///
/// Validation stops at the first malformed or duplicate registration and
/// returns [`ErrorCode::Validation`]. Work and memory are linear in the caller-
/// supplied slice; callers must bound the registration count at ingestion.
pub fn validate_contribution_set(
    registrations: &[ContributionRegistration],
) -> Result<(), ShellError> {
    let mut names = HashSet::new();
    for registration in registrations {
        registration.validate()?;
        if !names.insert((registration.kind, registration.name.as_str())) {
            return Err(extension_error(format!(
                "duplicate {:?} contribution `{}`",
                registration.kind, registration.name
            )));
        }
    }
    Ok(())
}

/// Reject text that could alter terminal state when rendered without escaping.
///
/// Newline and tab are permitted for readable plain text. Other Unicode control
/// characters, including the C1 control-sequence introducer, return
/// [`ErrorCode::Validation`]. `context` is used only to identify the field in
/// the diagnostic.
pub fn reject_terminal_controls(context: &str, value: &str) -> Result<(), ShellError> {
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        || value.contains('\u{009b}')
    {
        return Err(extension_error(format!(
            "{context} contains forbidden terminal control bytes"
        ))
        .with_help("Return plain text or typed styled values; Quirl owns terminal control"));
    }
    Ok(())
}

/// Render untrusted text without allowing it to change terminal state.
///
/// Newlines and tabs remain readable. Other C0/C1 controls are written as
/// visible Rust-style escapes so text output is safe while structured output
/// can retain the original value.
pub fn escape_terminal_controls(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        if (character.is_control() && !matches!(character, '\n' | '\t')) || character == '\u{009b}'
        {
            rendered.extend(character.escape_default());
        } else {
            rendered.push(character);
        }
    }
    rendered
}

/// Render `value` as terminal-safe single-line text. Control bytes are
/// escaped, and newlines and tabs become visible escape sequences.
pub fn escape_terminal_line(value: &str) -> String {
    escape_terminal_controls(value)
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

/// Escape terminal controls in an already serialized JSON document while
/// preserving its parsed value. Pretty-printing whitespace remains intact.
pub fn escape_json_terminal_controls(serialized: &str) -> String {
    let mut rendered = String::with_capacity(serialized.len());
    for character in serialized.chars() {
        if character.is_control() && !matches!(character, '\n' | '\t') {
            rendered.push_str(&format!("\\u{:04x}", u32::from(character)));
        } else {
            rendered.push(character);
        }
    }
    rendered
}

/// Iteratively validate every object key and string leaf before an extension
/// value reaches catalog, completion, or UI consumers.
///
/// Traversal rejects values above [`JSON_TERMINAL_VALUE_NODES_MAX`],
/// [`JSON_TERMINAL_VALUE_DEPTH_MAX`], or
/// [`JSON_TERMINAL_VALUE_TEXT_BYTES_MAX`] before scheduling excess work.
pub fn reject_json_terminal_controls(context: &str, value: &Value) -> Result<(), ShellError> {
    let mut stack = vec![(value, 0_usize)];
    let mut scheduled_nodes = 1_usize;
    let mut text_bytes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        if depth > JSON_TERMINAL_VALUE_DEPTH_MAX {
            return Err(json_terminal_value_limit_error(
                context,
                "depth",
                JSON_TERMINAL_VALUE_DEPTH_MAX,
                depth,
            ));
        }
        match value {
            Value::String(value) => {
                text_bytes = checked_json_text_bytes(context, text_bytes, value.len())?;
                reject_terminal_controls(context, value)?;
            }
            Value::Array(values) => schedule_json_children(
                context,
                values.iter().rev(),
                depth,
                &mut scheduled_nodes,
                &mut stack,
            )?,
            Value::Object(values) => {
                schedule_json_children(
                    context,
                    values.values().rev(),
                    depth,
                    &mut scheduled_nodes,
                    &mut stack,
                )?;
                for key in values.keys() {
                    text_bytes = checked_json_text_bytes(context, text_bytes, key.len())?;
                    reject_terminal_controls(context, key)?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    Ok(())
}

fn schedule_json_children<'a>(
    context: &str,
    children: impl ExactSizeIterator<Item = &'a Value>,
    depth: usize,
    scheduled_nodes: &mut usize,
    stack: &mut Vec<(&'a Value, usize)>,
) -> Result<(), ShellError> {
    let child_count = children.len();
    let observed = scheduled_nodes.saturating_add(child_count);
    if observed > JSON_TERMINAL_VALUE_NODES_MAX {
        return Err(json_terminal_value_limit_error(
            context,
            "nodes",
            JSON_TERMINAL_VALUE_NODES_MAX,
            observed,
        ));
    }
    let child_depth = depth.saturating_add(1);
    if child_count > 0 && child_depth > JSON_TERMINAL_VALUE_DEPTH_MAX {
        return Err(json_terminal_value_limit_error(
            context,
            "depth",
            JSON_TERMINAL_VALUE_DEPTH_MAX,
            child_depth,
        ));
    }
    *scheduled_nodes = observed;
    stack.extend(children.map(|value| (value, child_depth)));
    Ok(())
}

fn checked_json_text_bytes(
    context: &str,
    current: usize,
    additional: usize,
) -> Result<usize, ShellError> {
    let observed = current.saturating_add(additional);
    if observed > JSON_TERMINAL_VALUE_TEXT_BYTES_MAX {
        return Err(json_terminal_value_limit_error(
            context,
            "text bytes",
            JSON_TERMINAL_VALUE_TEXT_BYTES_MAX,
            observed,
        ));
    }
    Ok(observed)
}

fn json_terminal_value_limit_error(
    context: &str,
    kind: &str,
    limit: usize,
    observed: usize,
) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{context} exceeds its JSON {kind} limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Return a smaller and shallower extension value")
}

fn validate_name(context: &str, value: &str) -> Result<(), ShellError> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-/".contains(character))
    {
        return Err(extension_error(format!(
            "{context} `{value}` contains unsupported characters"
        ))
        .with_help("Use ASCII letters, digits, dot, underscore, dash, or slash"));
    }
    Ok(())
}

fn capability_name(capability: ExtensionCapability) -> &'static str {
    match capability {
        ExtensionCapability::EventsObserve => "events_observe",
        ExtensionCapability::PlanRewrite => "plan_rewrite",
        ExtensionCapability::EnvironmentMutate => "environment_mutate",
        ExtensionCapability::OutputRead => "output_read",
        ExtensionCapability::ExecutionBlock => "execution_block",
        ExtensionCapability::CatalogContribute => "catalog_contribute",
        ExtensionCapability::CompletionContribute => "completion_contribute",
        ExtensionCapability::UiPanel => "ui_panel",
    }
}

fn extension_error(message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Validation, message)
        .with_help("Fix the extension declaration before loading it")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXTENSION_EVENT_V1_FIXTURE: &str = r#"{"protocol_version":1,"sequence":1,"data":{"kind":"output","stream":"stdout","bytes":1}}"#;

    #[test]
    fn event_sequences_are_strictly_ordered() {
        let first = ExtensionEvent::new(1, ExtensionEventData::SessionStart { restored: false });
        let second = ExtensionEvent::new(
            2,
            ExtensionEventData::DirectoryChanged {
                previous: "/a".to_owned(),
                current: "/b".to_owned(),
            },
        );
        first.validate_after(None).unwrap();
        second.validate_after(Some(first.sequence)).unwrap();
        assert!(second.validate_after(Some(second.sequence)).is_err());
    }

    #[test]
    fn actions_require_explicit_mutation_rights() {
        let action = ExtensionAction::RewritePlan {
            source: "echo safe".to_owned(),
        };
        assert!(
            action
                .validate(&[ExtensionCapability::EventsObserve])
                .is_err()
        );
        action
            .validate(&[
                ExtensionCapability::EventsObserve,
                ExtensionCapability::PlanRewrite,
            ])
            .unwrap();
    }

    #[test]
    fn panels_need_safe_plain_fallbacks_and_unique_names() {
        let unsafe_panel = ContributionRegistration {
            kind: ContributionKind::Panel,
            name: "cluster".to_owned(),
            deadline_ms: 20,
            plain_fallback: Some("\u{1b}[31mraw".to_owned()),
        };
        assert!(unsafe_panel.validate().is_err());

        let safe_panel = ContributionRegistration {
            plain_fallback: Some("cluster unavailable".to_owned()),
            ..unsafe_panel
        };
        assert!(validate_contribution_set(&[safe_panel.clone(), safe_panel]).is_err());
    }

    #[test]
    fn nested_contribution_strings_reject_terminal_controls() {
        let nested = serde_json::json!({"rows": [["safe", {"value": "\u{1b}[31mraw"}]]});
        assert!(reject_json_terminal_controls("contribution", &nested).is_err());
    }

    #[test]
    fn nested_json_validation_accepts_exact_depth_and_rejects_the_next_level() {
        let mut exact = Value::Null;
        for _ in 0..JSON_TERMINAL_VALUE_DEPTH_MAX {
            exact = Value::Array(vec![exact]);
        }
        reject_json_terminal_controls("contribution", &exact).unwrap();

        let excess = Value::Array(vec![exact]);
        let error = reject_json_terminal_controls("contribution", &excess).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error.details.context[0].contains(&format!("limit: {JSON_TERMINAL_VALUE_DEPTH_MAX}"))
        );
        assert!(
            error.details.context[0]
                .contains(&format!("observed: {}", JSON_TERMINAL_VALUE_DEPTH_MAX + 1))
        );
    }

    #[test]
    fn broad_json_validation_rejects_nodes_before_scheduling_excess_work() {
        let mut exact = Value::Array(vec![Value::Null; JSON_TERMINAL_VALUE_NODES_MAX - 1]);
        reject_json_terminal_controls("contribution", &exact).unwrap();
        let Value::Array(values) = &mut exact else {
            panic!("the test constructs an array")
        };
        values.push(Value::Null);
        let excess = exact;
        let error = reject_json_terminal_controls("contribution", &excess).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error.details.context[0].contains(&format!("limit: {JSON_TERMINAL_VALUE_NODES_MAX}"))
        );
        assert!(
            error.details.context[0]
                .contains(&format!("observed: {}", JSON_TERMINAL_VALUE_NODES_MAX + 1))
        );
    }

    #[test]
    fn json_validation_rejects_text_above_the_scan_limit() {
        let mut exact = "x".repeat(JSON_TERMINAL_VALUE_TEXT_BYTES_MAX);
        reject_json_terminal_controls("contribution", &Value::String(exact.clone())).unwrap();
        exact.push('x');
        let excess = Value::String(exact);
        let error = reject_json_terminal_controls("contribution", &excess).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error.details.context[0]
                .contains(&format!("limit: {JSON_TERMINAL_VALUE_TEXT_BYTES_MAX}"))
        );
    }

    #[test]
    fn progress_messages_reject_terminal_controls_but_allow_layout_text() {
        let unsafe_event = ExtensionEvent::new(
            1,
            ExtensionEventData::ExecutionProgress {
                completed: 1,
                total: Some(2),
                message: Some("working\u{1b}[31m".to_owned()),
            },
        );
        assert_eq!(
            unsafe_event.validate_after(None).unwrap_err().code,
            ErrorCode::Validation
        );

        let safe_event = ExtensionEvent::new(
            1,
            ExtensionEventData::ExecutionProgress {
                completed: 1,
                total: Some(2),
                message: Some("working\nnext\titem".to_owned()),
            },
        );
        safe_event.validate_after(None).unwrap();
    }

    #[test]
    fn extension_v1_fails_closed_and_byte_counts_use_u64_wire_values() {
        let mut previous = ExtensionEvent::new(
            1,
            ExtensionEventData::Output {
                stream: OutputStream::Stdout,
                bytes: usize::MAX,
                text: None,
            },
        );
        let encoded = serde_json::to_value(&previous).unwrap();
        assert_eq!(
            encoded["data"]["bytes"],
            serde_json::json!(u64::try_from(usize::MAX).unwrap())
        );
        assert_eq!(
            serde_json::from_value::<ExtensionEvent>(encoded).unwrap(),
            previous
        );

        previous.protocol_version = 1;
        assert_eq!(
            previous.validate_after(None).unwrap_err().code,
            ErrorCode::Validation
        );
        assert_eq!(
            crate::schema_fingerprint(EXTENSION_SCHEMA_V1_DESCRIPTOR),
            "fnv1a64:cedb09998598e80a"
        );
        assert_eq!(extension_schema_hash(), "fnv1a64:bfddb053a448bfe8");

        let historical: ExtensionEvent = serde_json::from_str(EXTENSION_EVENT_V1_FIXTURE).unwrap();
        assert_eq!(
            historical.validate_after(None).unwrap_err().code,
            ErrorCode::Validation
        );
    }

    #[test]
    fn terminal_escaping_preserves_layout_but_neutralizes_ansi_osc_and_c1() {
        let hostile = "ok\n\t\u{1b}[31mred\u{1b}]8;;https://example.invalid\u{7}link\u{9b}2J\r";
        let escaped = escape_terminal_controls(hostile);
        assert_eq!(
            escaped,
            "ok\n\t\\u{1b}[31mred\\u{1b}]8;;https://example.invalid\\u{7}link\\u{9b}2J\\r"
        );
        assert!(!escaped.contains('\u{1b}'));
        assert!(!escaped.contains('\u{009b}'));
        assert!(!escaped.contains('\r'));
    }

    #[test]
    fn json_terminal_escaping_preserves_the_parsed_value() {
        let value = serde_json::json!({"hostile": "\u{009b}2J\u{007f}"});
        let serialized = serde_json::to_string_pretty(&value).unwrap();
        let escaped = escape_json_terminal_controls(&serialized);
        assert!(!escaped.contains('\u{009b}'));
        assert!(!escaped.contains('\u{007f}'));
        assert_eq!(serde_json::from_str::<Value>(&escaped).unwrap(), value);
    }
}
