use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidCommand,
    InvalidArgument,
    Data,
    Io,
    ProcessSpawn,
    ScriptRead,
    Lua,
    Validation,
    ResourceLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorLabel {
    pub source: Option<String>,
    pub start: usize,
    pub end: usize,
    pub message: String,
}

/// Stable, serializable error data before it becomes terminal decoration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(flatten)]
    pub details: Box<ErrorDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ErrorDetails {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<ErrorLabel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub help: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

impl ShellError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Box::default(),
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.details.help.push(help.into());
        self
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.details.context.push(context.into());
        self
    }

    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.details.command = Some(command.into());
        self
    }

    pub fn with_label(
        mut self,
        source: Option<String>,
        start: usize,
        end: usize,
        message: impl Into<String>,
    ) -> Self {
        self.details.labels.push(ErrorLabel {
            source,
            start,
            end,
            message: message.into(),
        });
        self
    }
}

impl fmt::Display for ShellError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ShellError {}
