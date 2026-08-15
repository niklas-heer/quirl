use crate::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

pub const EXTENSION_PROTOCOL_VERSION: u32 = 1;
pub const MAX_EXTENSION_DEADLINE_MS: u64 = 250;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    SessionRestore,
    DirectoryChanged,
    CommandPlan,
    ExecutionProgress,
    Output,
    Cancellation,
    Result,
    Error,
}

impl EventKind {
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
pub enum ExtensionEventData {
    SessionStart {
        restored: bool,
    },
    SessionRestore {
        session_id: String,
    },
    DirectoryChanged {
        previous: String,
        current: String,
    },
    CommandPlan {
        source: String,
        effects: Vec<String>,
    },
    ExecutionProgress {
        completed: u64,
        total: Option<u64>,
        message: Option<String>,
    },
    Output {
        stream: OutputStream,
        bytes: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Cancellation {
        reason: String,
    },
    Result {
        status: i32,
        duration_ms: u64,
    },
    Error {
        error: ShellError,
    },
}

impl ExtensionEventData {
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
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEvent {
    pub protocol_version: u32,
    pub sequence: u64,
    pub data: ExtensionEventData,
}

impl ExtensionEvent {
    pub fn new(sequence: u64, data: ExtensionEventData) -> Self {
        Self {
            protocol_version: EXTENSION_PROTOCOL_VERSION,
            sequence,
            data,
        }
    }

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
        if let ExtensionEventData::Cancellation { reason } = &self.data {
            reject_terminal_controls("extension cancellation reason", reason)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionCapability {
    EventsObserve,
    PlanRewrite,
    EnvironmentMutate,
    OutputRead,
    ExecutionBlock,
    CatalogContribute,
    CompletionContribute,
    UiPanel,
}

impl ExtensionCapability {
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
pub enum ExtensionAction {
    Diagnose { message: String },
    RewritePlan { source: String },
    SetEnvironment { name: String, value: String },
    BlockExecution { reason: String },
    AnnotateResult { key: String, value: Value },
}

impl ExtensionAction {
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
pub struct EventSubscription {
    pub name: String,
    pub events: Vec<EventKind>,
    pub capabilities: Vec<ExtensionCapability>,
    pub deadline_ms: u64,
}

impl EventSubscription {
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
pub enum ContributionKind {
    Catalog,
    Completion,
    Panel,
}

impl ContributionKind {
    pub const ALL: [Self; 3] = [Self::Catalog, Self::Completion, Self::Panel];
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContributionRegistration {
    pub kind: ContributionKind,
    pub name: String,
    pub deadline_ms: u64,
    #[serde(default)]
    pub plain_fallback: Option<String>,
}

impl ContributionRegistration {
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

/// Recursively validate every object key and string leaf before an extension
/// value reaches catalog, completion, or UI consumers.
pub fn reject_json_terminal_controls(context: &str, value: &Value) -> Result<(), ShellError> {
    match value {
        Value::String(value) => reject_terminal_controls(context, value),
        Value::Array(values) => {
            for value in values {
                reject_json_terminal_controls(context, value)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            for (key, value) in values {
                reject_terminal_controls(context, key)?;
                reject_json_terminal_controls(context, value)?;
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
    }
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
        assert!(action
            .validate(&[ExtensionCapability::EventsObserve])
            .is_err());
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
}
