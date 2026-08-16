use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Stable categories shared by human diagnostics and machine-readable output.
///
/// Callers should branch on this value rather than matching the error message,
/// whose wording may improve without a protocol version change.
pub enum ErrorCode {
    /// The requested command name or command form is not supported.
    InvalidCommand,
    /// A recognized command received an invalid argument or option value.
    InvalidArgument,
    /// Structured input could not be parsed, converted, or evaluated.
    Data,
    /// A filesystem, stream, or other host I/O operation failed.
    Io,
    /// The host could not create or start a requested process.
    ProcessSpawn,
    /// Script source could not be read from its declared origin.
    ScriptRead,
    /// Lua parsing, evaluation, or host-boundary processing failed.
    Lua,
    /// Input crossed a schema, protocol, or semantic validation boundary.
    Validation,
    /// An explicit byte, count, depth, instruction, or time bound was exceeded.
    ResourceLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// A diagnostic annotation over a half-open byte range in optional source text.
///
/// Producers must ensure `start <= end` and that both offsets are UTF-8
/// boundaries when `source` is present. Consumers should tolerate zero-length
/// labels, which identify a location without highlighting source bytes.
pub struct ErrorLabel {
    /// Complete source text containing the labelled range, when it is safe to retain.
    pub source: Option<String>,
    /// Inclusive start byte offset into [`Self::source`].
    pub start: usize,
    /// Exclusive end byte offset into [`Self::source`].
    pub end: usize,
    /// Explanation associated with this source range.
    pub message: String,
}

/// Stable, serializable error data before it becomes terminal decoration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellError {
    /// Stable machine-readable error category.
    pub code: ErrorCode,
    /// Concise primary diagnostic suitable for plain-text display.
    pub message: String,
    /// Optional structured annotations flattened into the serialized error object.
    #[serde(flatten)]
    pub details: Box<ErrorDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
/// Optional diagnostic context shared by JSON and terminal renderers.
///
/// Vectors preserve insertion order so callers can present the most relevant
/// context and recovery advice first.
pub struct ErrorDetails {
    /// Source annotations for precise diagnostics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<ErrorLabel>,
    /// Supporting facts such as underlying host errors or observed limit usage.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<String>,
    /// Actionable recovery suggestions for the user.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub help: Vec<String>,
    /// Command source associated with the failure, when applicable and safe to expose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Child-process exit status associated with the failure, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

impl ShellError {
    /// Construct an error with a stable category and primary message.
    ///
    /// Optional diagnostic fields begin empty and can be appended with the
    /// builder methods on this type.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Box::default(),
        }
    }

    /// Append an actionable recovery suggestion while preserving prior advice.
    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.details.help.push(help.into());
        self
    }

    /// Append a supporting diagnostic fact while preserving prior context.
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.details.context.push(context.into());
        self
    }

    /// Associate the error with the command source that was being processed.
    ///
    /// This replaces any command already attached to the error.
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.details.command = Some(command.into());
        self
    }

    /// Append a source annotation over the half-open byte range `start..end`.
    ///
    /// Callers are responsible for providing ordered offsets on UTF-8
    /// boundaries when `source` is present; renderers consume the stored range
    /// without repairing it.
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
