//! Passive, bounded contracts shared by Quirl execution front doors.
//!
//! This module owns no parser, process, VM, stream, terminal, or output sink.
//! Engines retain those resources and the CLI composition root selects an
//! engine only after a request validates into an [`ExecutionPlan`].

use crate::{CommandOutcome, ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// Maximum UTF-8 bytes retained for one execution source.
pub const EXECUTION_SOURCE_BYTES_MAX: usize = 4 * 1024 * 1024;
/// Maximum UTF-8 bytes retained for one logical source name.
pub const EXECUTION_SOURCE_NAME_BYTES_MAX: usize = 4 * 1024;
/// Maximum arguments retained by one request.
pub const EXECUTION_ARGUMENTS_MAX: usize = 1_024;
/// Maximum aggregate UTF-8 bytes retained across request arguments.
pub const EXECUTION_ARGUMENT_BYTES_MAX: usize = 1024 * 1024;
/// Maximum bytes retained in one request input or outcome byte stream.
pub const EXECUTION_BYTES_MAX: usize = 8 * 1024 * 1024;
/// Maximum retained bytes permitted separately for stdout and stderr capture.
///
/// Engines may use a smaller default when no plan is present, but a validated
/// plan's requested ceiling is authoritative throughout this complete range.
pub const EXECUTION_CAPTURE_BYTES_MAX: usize = 1024 * 1024;
/// Maximum nodes accepted in one structured execution value.
pub const EXECUTION_VALUE_NODES_MAX: usize = 100_000;
/// Maximum nesting depth accepted in one structured execution value.
pub const EXECUTION_VALUE_DEPTH_MAX: usize = 64;
/// Maximum aggregate UTF-8 bytes retained by strings and record keys in one value.
pub const EXECUTION_VALUE_TEXT_BYTES_MAX: usize = 8 * 1024 * 1024;
/// Maximum non-fatal diagnostics retained in one successful outcome.
pub const EXECUTION_DIAGNOSTICS_MAX: usize = 64;
/// Maximum wall-clock deadline accepted by the common request contract.
pub const EXECUTION_DEADLINE_MAX: Duration = Duration::from_secs(300);

/// Exact bounded UTF-8 source and its logical diagnostic origin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSource {
    name: String,
    text: String,
}

impl ExecutionSource {
    /// Validate and retain one source identity.
    ///
    /// The name is capped at [`EXECUTION_SOURCE_NAME_BYTES_MAX`] bytes and the
    /// text at [`EXECUTION_SOURCE_BYTES_MAX`] bytes. Rust strings already prove
    /// UTF-8 validity; the text is never normalized or re-quoted.
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Result<Self, ShellError> {
        let source = Self {
            name: name.into(),
            text: text.into(),
        };
        source.validate()?;
        Ok(source)
    }

    /// Return the exact logical source name used by diagnostics.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the exact accepted source text.
    pub fn text(&self) -> &str {
        &self.text
    }

    fn validate(&self) -> Result<(), ShellError> {
        validate_bytes(
            "execution source name",
            self.name.len(),
            EXECUTION_SOURCE_NAME_BYTES_MAX,
        )?;
        validate_bytes(
            "execution source",
            self.text.len(),
            EXECUTION_SOURCE_BYTES_MAX,
        )
    }
}

/// Validated half-open UTF-8 byte span in an [`ExecutionSource`].
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSpan {
    /// Inclusive byte offset from the start of the source.
    pub start: u32,
    /// Exclusive byte offset from the start of the source.
    pub end: u32,
}

impl ExecutionSpan {
    /// Validate ordering, range, and UTF-8 boundaries against `source`.
    pub fn new(source: &ExecutionSource, start: usize, end: usize) -> Result<Self, ShellError> {
        let valid = start <= end
            && end <= source.text.len()
            && source.text.is_char_boundary(start)
            && source.text.is_char_boundary(end);
        if !valid {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "execution source span is not a valid UTF-8 byte range",
            )
            .with_context(format!(
                "source bytes: {}; requested span: {start}..{end}",
                source.text.len()
            ))
            .with_help("Use an ordered half-open span on UTF-8 character boundaries"));
        }
        Ok(Self {
            start: u32::try_from(start).map_err(span_range_error)?,
            end: u32::try_from(end).map_err(span_range_error)?,
        })
    }
}

/// Engine selected by the CLI composition root.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Quirl's native command and process graph.
    NativeCommand,
    /// A native Quirl script containing explicit command and data statements.
    QuirlScript,
    /// Quirl's focused structured-data evaluator.
    Data,
    /// The restricted Lua 5.4 runtime.
    Lua,
    /// A typed Lua runner module invoked through `quirl run`.
    LuaScript,
    /// An explicit bounded Bash reference interpreter.
    Bash,
    /// An explicit bounded Zsh reference interpreter.
    Zsh,
    /// A future validated plugin command adapter; dispatch currently fails closed.
    Plugin,
    /// A future protocol-owned adapter; dispatch currently fails closed.
    Protocol,
}

/// Observable authority declared by an execution plan.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionEffect {
    /// Read filesystem data without intentionally changing it.
    ReadFilesystem,
    /// Create, replace, remove, or otherwise change filesystem data.
    WriteFilesystem,
    /// Start or delegate work to an operating-system process.
    SpawnProcess,
    /// Change the working directory used by subsequent work.
    ChangeDirectory,
}

/// Fixed-size effect set that cannot grow with untrusted input.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEffects {
    read_filesystem: bool,
    write_filesystem: bool,
    spawn_process: bool,
    change_directory: bool,
}

impl ExecutionEffects {
    /// Construct an empty effect set.
    pub const fn none() -> Self {
        Self {
            read_filesystem: false,
            write_filesystem: false,
            spawn_process: false,
            change_directory: false,
        }
    }

    /// Construct the complete currently-known effect set.
    pub const fn all() -> Self {
        Self {
            read_filesystem: true,
            write_filesystem: true,
            spawn_process: true,
            change_directory: true,
        }
    }

    /// Construct a set from a finite slice; repeated effects are idempotent.
    pub fn from_effects(effects: &[ExecutionEffect]) -> Self {
        let mut set = Self::none();
        for effect in effects {
            match effect {
                ExecutionEffect::ReadFilesystem => set.read_filesystem = true,
                ExecutionEffect::WriteFilesystem => set.write_filesystem = true,
                ExecutionEffect::SpawnProcess => set.spawn_process = true,
                ExecutionEffect::ChangeDirectory => set.change_directory = true,
            }
        }
        set
    }

    /// Return whether this set contains `effect`.
    pub const fn contains(self, effect: ExecutionEffect) -> bool {
        match effect {
            ExecutionEffect::ReadFilesystem => self.read_filesystem,
            ExecutionEffect::WriteFilesystem => self.write_filesystem,
            ExecutionEffect::SpawnProcess => self.spawn_process,
            ExecutionEffect::ChangeDirectory => self.change_directory,
        }
    }

    /// Return whether every effect in `required` is present.
    pub const fn allows(self, required: Self) -> bool {
        (!required.read_filesystem || self.read_filesystem)
            && (!required.write_filesystem || self.write_filesystem)
            && (!required.spawn_process || self.spawn_process)
            && (!required.change_directory || self.change_directory)
    }

    fn first_denied(self, required: Self) -> Option<ExecutionEffect> {
        [
            ExecutionEffect::ReadFilesystem,
            ExecutionEffect::WriteFilesystem,
            ExecutionEffect::SpawnProcess,
            ExecutionEffect::ChangeDirectory,
        ]
        .into_iter()
        .find(|effect| required.contains(*effect) && !self.contains(*effect))
    }
}

/// Cloneable cancellation flag shared by a request and its selected engine.
#[derive(Debug, Clone, Default)]
pub struct ExecutionCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ExecutionCancellation {
    /// Wrap an existing atomic flag so an established engine can share identity.
    pub fn from_atomic(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    /// Request cancellation of current and subsequent work using this handle.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Return whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Clone the atomic flag for an engine boundary that uses the passive flag directly.
    pub fn atomic(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    /// Reject a cancelled request with a consistent resource-limit diagnostic.
    pub fn ensure_active(&self, stage: &str) -> Result<(), ShellError> {
        if self.is_cancelled() {
            return Err(
                ShellError::new(ErrorCode::ResourceLimit, "execution was cancelled")
                    .with_context(format!("cancellation observed {stage}"))
                    .with_help(
                        "Retry only after creating or clearing the owning cancellation handle",
                    ),
            );
        }
        Ok(())
    }
}

/// Stable structured value shared by data, scripts, plugins, and outcomes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum StructuredValue {
    /// Absence of a value, equivalent to JSON `null`.
    Nothing,
    /// A Boolean value.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// An unsigned 64-bit integer.
    UInt(u64),
    /// Decimal source text retained without binary floating-point conversion.
    Decimal(String),
    /// UTF-8 text without domain-specific semantics.
    String(String),
    /// An ordered bounded sequence of values.
    List(Vec<StructuredValue>),
    /// A deterministically ordered bounded mapping.
    Record(BTreeMap<String, StructuredValue>),
    /// A filesystem path represented as UTF-8 text.
    Path(String),
    /// An elapsed duration in nanoseconds.
    Duration {
        /// Unsigned duration magnitude in nanoseconds.
        nanoseconds: u64,
    },
    /// A byte size.
    Size {
        /// Unsigned size magnitude in bytes.
        bytes: u64,
    },
    /// A date and time represented as UTF-8 text.
    DateTime(String),
    /// A pattern preserved in its source representation.
    Pattern(String),
}

impl StructuredValue {
    /// Convert a JSON-compatible value without losing integer sign or width.
    ///
    /// This compatibility bridge is iterative. Callers must validate the
    /// returned value before retaining it; execution request and outcome
    /// constructors perform that bounded validation automatically.
    pub fn from_json(value: serde_json::Value) -> Self {
        enum Work {
            Visit(serde_json::Value),
            FinishList(usize),
            FinishRecord(Vec<String>),
        }

        let mut work = vec![Work::Visit(value)];
        let mut output = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Work::Visit(value) => match value {
                    serde_json::Value::Null => output.push(Self::Nothing),
                    serde_json::Value::Bool(value) => output.push(Self::Bool(value)),
                    serde_json::Value::Number(value) => {
                        output.push(match (value.as_i64(), value.as_u64()) {
                            (Some(value), _) => Self::Int(value),
                            (_, Some(value)) => Self::UInt(value),
                            _ => Self::Decimal(value.to_string()),
                        })
                    }
                    serde_json::Value::String(value) => output.push(Self::String(value)),
                    serde_json::Value::Array(values) => {
                        let length = values.len();
                        work.push(Work::FinishList(length));
                        work.extend(values.into_iter().rev().map(Work::Visit));
                    }
                    serde_json::Value::Object(values) => {
                        let (keys, values): (Vec<_>, Vec<_>) = values.into_iter().unzip();
                        work.push(Work::FinishRecord(keys));
                        work.extend(values.into_iter().rev().map(Work::Visit));
                    }
                },
                Work::FinishList(length) => {
                    let start = output.len().saturating_sub(length);
                    let values = output.split_off(start);
                    output.push(Self::List(values));
                }
                Work::FinishRecord(keys) => {
                    let start = output.len().saturating_sub(keys.len());
                    let values = output.split_off(start);
                    output.push(Self::Record(keys.into_iter().zip(values).collect()));
                }
            }
        }
        debug_assert_eq!(output.len(), 1);
        output.pop().unwrap_or(Self::Nothing)
    }

    /// Convert to JSON-compatible data for existing renderers and Lua adapters.
    ///
    /// Domain strings lose their tag; durations and sizes become suffixed
    /// strings. Conversion is iterative, but callers should validate first so
    /// retained JSON remains within the common value bounds.
    pub fn json_value(&self) -> serde_json::Value {
        enum Work<'a> {
            Visit(&'a StructuredValue),
            FinishList(usize),
            FinishRecord(Vec<String>),
        }

        let mut work = vec![Work::Visit(self)];
        let mut output = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Work::Visit(value) => match value {
                    Self::Nothing => output.push(serde_json::Value::Null),
                    Self::Bool(value) => output.push(serde_json::Value::Bool(*value)),
                    Self::Int(value) => output.push(serde_json::Value::from(*value)),
                    Self::UInt(value) => output.push(serde_json::Value::from(*value)),
                    Self::Decimal(value) => output.push(
                        serde_json::from_str::<serde_json::Number>(value).map_or_else(
                            |_| serde_json::Value::String(value.clone()),
                            serde_json::Value::Number,
                        ),
                    ),
                    Self::String(value)
                    | Self::Path(value)
                    | Self::DateTime(value)
                    | Self::Pattern(value) => output.push(serde_json::Value::String(value.clone())),
                    Self::Duration { nanoseconds } => {
                        output.push(serde_json::Value::String(format!("{nanoseconds}ns")))
                    }
                    Self::Size { bytes } => {
                        output.push(serde_json::Value::String(format!("{bytes}B")))
                    }
                    Self::List(values) => {
                        work.push(Work::FinishList(values.len()));
                        work.extend(values.iter().rev().map(Work::Visit));
                    }
                    Self::Record(values) => {
                        work.push(Work::FinishRecord(values.keys().cloned().collect()));
                        work.extend(values.values().rev().map(Work::Visit));
                    }
                },
                Work::FinishList(length) => {
                    let start = output.len().saturating_sub(length);
                    let values = output.split_off(start);
                    output.push(serde_json::Value::Array(values));
                }
                Work::FinishRecord(keys) => {
                    let start = output.len().saturating_sub(keys.len());
                    let values = output.split_off(start);
                    output.push(serde_json::Value::Object(
                        keys.into_iter().zip(values).collect(),
                    ));
                }
            }
        }
        debug_assert_eq!(output.len(), 1);
        output.pop().unwrap_or(serde_json::Value::Null)
    }

    /// Render one value for a terminal-safe caller to escape before display.
    pub fn display_value(&self) -> String {
        match self {
            Self::Nothing => "null".to_owned(),
            Self::Bool(value) => value.to_string(),
            Self::Int(value) => value.to_string(),
            Self::UInt(value) => value.to_string(),
            Self::Decimal(value)
            | Self::String(value)
            | Self::Path(value)
            | Self::DateTime(value)
            | Self::Pattern(value) => value.clone(),
            Self::Duration { nanoseconds } => format!("{nanoseconds}ns"),
            Self::Size { bytes } => format!("{bytes}B"),
            Self::List(_) | Self::Record(_) => serde_json::to_string(&self.json_value())
                .unwrap_or_else(|_| "<unrenderable structured value>".to_owned()),
        }
    }

    /// Validate node, depth, and retained UTF-8 text bounds without recursion.
    pub fn validate(&self) -> Result<(), ShellError> {
        validate_value_stack(vec![(self, 0_usize)])
    }
}

fn validate_value_stack(mut stack: Vec<(&StructuredValue, usize)>) -> Result<(), ShellError> {
    let mut nodes = 0_usize;
    let mut text_bytes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > EXECUTION_VALUE_NODES_MAX {
            return Err(value_limit_error("nodes", EXECUTION_VALUE_NODES_MAX, nodes));
        }
        if depth > EXECUTION_VALUE_DEPTH_MAX {
            return Err(value_limit_error("depth", EXECUTION_VALUE_DEPTH_MAX, depth));
        }
        match value {
            StructuredValue::Decimal(value)
            | StructuredValue::String(value)
            | StructuredValue::Path(value)
            | StructuredValue::DateTime(value)
            | StructuredValue::Pattern(value) => {
                text_bytes = text_bytes.saturating_add(value.len())
            }
            StructuredValue::List(values) => {
                stack.extend(values.iter().rev().map(|value| (value, depth + 1)));
            }
            StructuredValue::Record(values) => {
                for (key, value) in values.iter().rev() {
                    text_bytes = text_bytes.saturating_add(key.len());
                    stack.push((value, depth + 1));
                }
            }
            StructuredValue::Nothing
            | StructuredValue::Bool(_)
            | StructuredValue::Int(_)
            | StructuredValue::UInt(_)
            | StructuredValue::Duration { .. }
            | StructuredValue::Size { .. } => {}
        }
        if text_bytes > EXECUTION_VALUE_TEXT_BYTES_MAX {
            return Err(value_limit_error(
                "text bytes",
                EXECUTION_VALUE_TEXT_BYTES_MAX,
                text_bytes,
            ));
        }
    }
    Ok(())
}

/// Data supplied to an execution engine before dispatch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "kind",
    content = "content",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExecutionInput {
    /// No piped or structured input.
    None,
    /// Bounded uninterpreted bytes.
    Bytes(Vec<u8>),
    /// One bounded structured value.
    Value(StructuredValue),
}

/// Representation requested from the selected engine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOutputTarget {
    /// Engine output is inherited by the caller's terminal or stream sink.
    Inherit,
    /// Retain each byte stream up to the explicit per-stream ceiling.
    Capture {
        /// Maximum bytes retained separately for stdout and stderr.
        max_bytes_per_stream: usize,
    },
    /// Preserve a structured value without stringification.
    Value,
}

/// Request awaiting cancellation, effect, and bound validation.
#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    source: ExecutionSource,
    mode: ExecutionMode,
    cancellation: ExecutionCancellation,
    input: ExecutionInput,
    output: ExecutionOutputTarget,
    arguments: Vec<String>,
    deadline: Duration,
    declared_effects: ExecutionEffects,
    allowed_effects: ExecutionEffects,
}

impl ExecutionRequest {
    /// Start a request with no input, value output, no effects, and a fresh cancellation handle.
    pub fn new(source: ExecutionSource, mode: ExecutionMode) -> Self {
        Self {
            source,
            mode,
            cancellation: ExecutionCancellation::default(),
            input: ExecutionInput::None,
            output: ExecutionOutputTarget::Value,
            arguments: Vec::new(),
            deadline: Duration::from_secs(30),
            declared_effects: ExecutionEffects::none(),
            allowed_effects: ExecutionEffects::none(),
        }
    }

    /// Replace the cancellation identity propagated to the engine.
    pub fn with_cancellation(mut self, cancellation: ExecutionCancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Set bounded byte or value input.
    pub fn with_input(mut self, input: ExecutionInput) -> Self {
        self.input = input;
        self
    }

    /// Set the exact bounded argument vector supplied to script-like engines.
    pub fn with_arguments(mut self, arguments: Vec<String>) -> Self {
        self.arguments = arguments;
        self
    }

    /// Set the positive wall-clock deadline shared with the selected engine.
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// Select inherited, bounded byte capture, or structured value output.
    pub fn with_output(mut self, output: ExecutionOutputTarget) -> Self {
        self.output = output;
        self
    }

    /// Declare required effects and the authority allowed by the caller.
    pub fn with_effects(mut self, declared: ExecutionEffects, allowed: ExecutionEffects) -> Self {
        self.declared_effects = declared;
        self.allowed_effects = allowed;
        self
    }

    /// Validate all retained data, cancellation, and authority before dispatch.
    pub fn plan(self) -> Result<ExecutionPlan, ShellError> {
        self.source.validate()?;
        self.cancellation.ensure_active("before dispatch")?;
        validate_input(&self.input)?;
        validate_arguments(&self.arguments)?;
        validate_deadline(self.deadline)?;
        validate_output_target(self.output)?;
        if let Some(effect) = self.allowed_effects.first_denied(self.declared_effects) {
            return Err(
                ShellError::new(ErrorCode::Validation, "execution effect was not allowed")
                    .with_command(self.source.text.clone())
                    .with_context(format!("denied effect: {effect:?}"))
                    .with_help(
                        "Grant the declared effect explicitly or choose an inert execution mode",
                    ),
            );
        }
        let deadline = ExecutionDeadline::after(self.deadline)?;
        Ok(ExecutionPlan {
            source: self.source,
            mode: self.mode,
            cancellation: self.cancellation,
            input: self.input,
            output: self.output,
            arguments: self.arguments,
            deadline,
            declared_effects: self.declared_effects,
            cleanup_owner: ExecutionCleanupOwner::Engine,
        })
    }
}

/// Fully validated immutable input to the CLI's single mode-selection point.
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    source: ExecutionSource,
    mode: ExecutionMode,
    cancellation: ExecutionCancellation,
    input: ExecutionInput,
    output: ExecutionOutputTarget,
    arguments: Vec<String>,
    deadline: ExecutionDeadline,
    declared_effects: ExecutionEffects,
    cleanup_owner: ExecutionCleanupOwner,
}

impl ExecutionPlan {
    /// Exact source identity preserved through the selected adapter.
    pub fn source(&self) -> &ExecutionSource {
        &self.source
    }
    /// Selected engine mode.
    pub const fn mode(&self) -> ExecutionMode {
        self.mode
    }
    /// Shared cancellation identity.
    pub fn cancellation(&self) -> &ExecutionCancellation {
        &self.cancellation
    }
    /// Bounded input representation.
    pub fn input(&self) -> &ExecutionInput {
        &self.input
    }
    /// Requested output representation.
    pub const fn output(&self) -> ExecutionOutputTarget {
        self.output
    }
    /// Bounded script arguments in source order.
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
    /// One monotonic absolute deadline for initialization, execution, output,
    /// cleanup, and outcome normalization.
    pub const fn deadline(&self) -> ExecutionDeadline {
        self.deadline
    }
    /// Reject cancellation or expiry at one adapter safe point.
    ///
    /// Explicit cancellation is checked first and therefore remains the
    /// originating error when it was already observable at the race boundary.
    pub fn ensure_active(&self, stage: &str) -> Result<(), ShellError> {
        self.cancellation.ensure_active(stage)?;
        self.deadline.ensure_remaining(stage).map(|_| ())
    }
    /// Effects validated before this plan was created.
    pub const fn declared_effects(&self) -> ExecutionEffects {
        self.declared_effects
    }
    /// Layer responsible for process, VM, stream, and job cleanup.
    pub const fn cleanup_owner(&self) -> ExecutionCleanupOwner {
        self.cleanup_owner
    }
}

/// Copyable monotonic deadline created exactly once while a request is planned.
///
/// Engines receive either this value or a remaining duration sampled from it.
/// Sampling never extends the absolute expiry, and local security policies may
/// only narrow the returned duration.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionDeadline {
    expires_at: Instant,
    budget: Duration,
}

impl ExecutionDeadline {
    fn after(budget: Duration) -> Result<Self, ShellError> {
        let expires_at = Instant::now().checked_add(budget).ok_or_else(|| {
            ShellError::new(
                ErrorCode::ResourceLimit,
                "execution deadline is outside the monotonic clock range",
            )
            .with_context(format!("requested duration: {budget:?}"))
            .with_help("Choose a shorter execution deadline")
        })?;
        Ok(Self { expires_at, budget })
    }

    /// Return the absolute monotonic expiry instant.
    pub const fn expires_at(self) -> Instant {
        self.expires_at
    }

    /// Return the originally validated positive wall-time budget.
    pub const fn budget(self) -> Duration {
        self.budget
    }

    /// Return time remaining at `stage`, or the common deadline error.
    pub fn ensure_remaining(self, stage: &str) -> Result<Duration, ShellError> {
        self.expires_at
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| execution_deadline_error(stage, self.budget))
    }
}

/// Owner responsible for releasing resources created by a plan.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCleanupOwner {
    /// The selected engine owns resources through RAII and returns no raw handles.
    Engine,
    /// The output consumer owns only an inherited sink, never engine internals.
    OutputConsumer,
}

/// Cleanup state reported by a successful outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCleanupState {
    /// All work owned solely for this call has been released or joined.
    Complete,
    /// Bounded job state remains owned by the engine that returned the outcome.
    RetainedByEngine,
}

/// Status attached only to a successful execution outcome.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "code",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExecutionStatus {
    /// Engine completion with the exact process-style status code.
    Exited(i32),
}

/// Output returned without erasing byte, inherited, and structured semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOutput {
    /// Output was inherited and therefore is not retained in this outcome.
    Inherited,
    /// Separately retained bounded stdout and stderr bytes.
    Bytes {
        /// Standard-output bytes in observed order.
        stdout: Vec<u8>,
        /// Standard-error bytes in observed order.
        stderr: Vec<u8>,
    },
    /// One bounded structured value.
    Value {
        /// Value returned without conversion through display text.
        value: StructuredValue,
    },
    /// A finite materialized value stream retained within the value-node bound.
    Values {
        /// Values in source order; live pull streams remain engine-owned.
        values: Vec<StructuredValue>,
    },
}

/// Successful bounded result shared by every execution adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOutcome {
    /// Engine status; operating failures are returned separately as [`ShellError`].
    pub status: ExecutionStatus,
    /// Representation requested by the validated plan.
    pub output: ExecutionOutput,
    /// Bounded non-fatal diagnostics retained in observation order.
    pub diagnostics: Vec<ShellError>,
    /// State of resources owned by the selected engine.
    pub cleanup: ExecutionCleanupState,
}

impl ExecutionOutcome {
    /// Validate and construct a successful outcome.
    pub fn new(
        status: ExecutionStatus,
        output: ExecutionOutput,
        diagnostics: Vec<ShellError>,
        cleanup: ExecutionCleanupState,
    ) -> Result<Self, ShellError> {
        validate_output(&output)?;
        if diagnostics.len() > EXECUTION_DIAGNOSTICS_MAX {
            return Err(limit_error(
                "execution diagnostics",
                EXECUTION_DIAGNOSTICS_MAX,
                diagnostics.len(),
            ));
        }
        Ok(Self {
            status,
            output,
            diagnostics,
            cleanup,
        })
    }

    /// Normalize a process-style result without erasing inherited versus
    /// captured output semantics.
    ///
    /// Captured streams are checked against the plan ceiling again at this
    /// reader boundary. Exactly the ceiling succeeds; a larger engine result
    /// fails rather than being truncated or silently reclassified.
    pub fn from_command(
        outcome: CommandOutcome,
        target: ExecutionOutputTarget,
    ) -> Result<Self, ShellError> {
        let status = outcome.status;
        let output = match target {
            ExecutionOutputTarget::Inherit => {
                if outcome.stdout.is_some() || outcome.stderr.is_some() {
                    return Err(ShellError::new(
                        ErrorCode::Validation,
                        "inherited process execution retained output",
                    )
                    .with_help(
                        "Use bounded capture when output must cross the execution boundary",
                    ));
                }
                ExecutionOutput::Inherited
            }
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream,
            } => {
                let stdout = outcome.stdout.unwrap_or_default().into_bytes();
                let stderr = outcome.stderr.unwrap_or_default().into_bytes();
                validate_bytes("execution stdout", stdout.len(), max_bytes_per_stream)?;
                validate_bytes("execution stderr", stderr.len(), max_bytes_per_stream)?;
                ExecutionOutput::Bytes { stdout, stderr }
            }
            ExecutionOutputTarget::Value => {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    "process outcome cannot satisfy structured value output",
                )
                .with_help("Request inherited output or bounded byte capture"));
            }
        };
        Self::new(
            ExecutionStatus::Exited(status),
            output,
            Vec::new(),
            ExecutionCleanupState::Complete,
        )
    }

    /// Return the process-style status code.
    pub const fn status_code(&self) -> i32 {
        match self.status {
            ExecutionStatus::Exited(code) => code,
        }
    }
}

fn validate_input(input: &ExecutionInput) -> Result<(), ShellError> {
    match input {
        ExecutionInput::None => Ok(()),
        ExecutionInput::Bytes(bytes) => {
            validate_bytes("execution input", bytes.len(), EXECUTION_BYTES_MAX)
        }
        ExecutionInput::Value(value) => value.validate(),
    }
}

fn validate_output_target(output: ExecutionOutputTarget) -> Result<(), ShellError> {
    match output {
        ExecutionOutputTarget::Capture {
            max_bytes_per_stream: 0,
        } => Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "execution capture limit must be greater than zero",
        )
        .with_help("Set a positive retained-byte limit for each captured stream")),
        ExecutionOutputTarget::Capture {
            max_bytes_per_stream,
        } => validate_bytes(
            "execution capture limit",
            max_bytes_per_stream,
            EXECUTION_CAPTURE_BYTES_MAX,
        ),
        ExecutionOutputTarget::Inherit | ExecutionOutputTarget::Value => Ok(()),
    }
}

fn execution_deadline_error(stage: &str, budget: Duration) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "execution exceeded its absolute deadline",
    )
    .with_context(format!("deadline observed {stage}; budget: {budget:?}"))
    .with_help("Use a shorter-running operation or increase the execution deadline")
}

fn validate_output(output: &ExecutionOutput) -> Result<(), ShellError> {
    match output {
        ExecutionOutput::Inherited => Ok(()),
        ExecutionOutput::Bytes { stdout, stderr } => {
            validate_bytes("execution stdout", stdout.len(), EXECUTION_BYTES_MAX)?;
            validate_bytes("execution stderr", stderr.len(), EXECUTION_BYTES_MAX)
        }
        ExecutionOutput::Value { value } => value.validate(),
        ExecutionOutput::Values { values } => {
            if values.len() > EXECUTION_VALUE_NODES_MAX {
                return Err(value_limit_error(
                    "nodes",
                    EXECUTION_VALUE_NODES_MAX,
                    values.len(),
                ));
            }
            validate_value_stack(values.iter().rev().map(|value| (value, 1_usize)).collect())
        }
    }
}

fn validate_arguments(arguments: &[String]) -> Result<(), ShellError> {
    if arguments.len() > EXECUTION_ARGUMENTS_MAX {
        return Err(limit_error(
            "execution arguments",
            EXECUTION_ARGUMENTS_MAX,
            arguments.len(),
        ));
    }
    let observed = arguments.iter().fold(0_usize, |total, argument| {
        total.saturating_add(argument.len())
    });
    validate_bytes(
        "execution argument bytes",
        observed,
        EXECUTION_ARGUMENT_BYTES_MAX,
    )
}

fn validate_deadline(deadline: Duration) -> Result<(), ShellError> {
    if !deadline.is_zero() && deadline <= EXECUTION_DEADLINE_MAX {
        return Ok(());
    }
    let maximum = EXECUTION_DEADLINE_MAX;
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        "execution deadline is outside the common contract bound",
    )
    .with_context(format!("maximum: {maximum:?}; requested: {deadline:?}"))
    .with_help("Choose a positive execution deadline no greater than five minutes"))
}

fn validate_bytes(name: &str, observed: usize, limit: usize) -> Result<(), ShellError> {
    if observed <= limit {
        Ok(())
    } else {
        Err(limit_error(name, limit, observed))
    }
}

fn limit_error(name: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{name} exceeds its retained-data limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Reduce the request or choose a streaming boundary with a tighter window")
}

fn value_limit_error(kind: &str, limit: usize, observed: usize) -> ShellError {
    limit_error(&format!("structured value {kind}"), limit, observed)
}

fn span_range_error(error: std::num::TryFromIntError) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "execution source span exceeds its fixed-width range",
    )
    .with_context(error.to_string())
    .with_help("Keep execution sources within the documented source byte limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_spans_preserve_exact_utf8_boundaries() {
        let source = ExecutionSource::new("argument", "printf '🌀'").unwrap();
        assert_eq!(source.text(), "printf '🌀'");
        assert!(ExecutionSpan::new(&source, 8, 12).is_ok());
        assert!(ExecutionSpan::new(&source, 9, 12).is_err());
    }

    #[test]
    fn cancellation_and_effect_denial_happen_before_planning() {
        let source = ExecutionSource::new("argument", "touch file").unwrap();
        let denied = ExecutionRequest::new(source.clone(), ExecutionMode::NativeCommand)
            .with_effects(
                ExecutionEffects::from_effects(&[ExecutionEffect::WriteFilesystem]),
                ExecutionEffects::none(),
            )
            .plan()
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::Validation);

        let cancellation = ExecutionCancellation::default();
        cancellation.cancel();
        let cancelled = ExecutionRequest::new(source, ExecutionMode::NativeCommand)
            .with_cancellation(cancellation)
            .plan()
            .unwrap_err();
        assert_eq!(cancelled.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn structured_values_reject_depth_without_recursion() {
        let mut value = StructuredValue::Nothing;
        for _ in 0..=EXECUTION_VALUE_DEPTH_MAX {
            value = StructuredValue::List(vec![value]);
        }
        assert_eq!(value.validate().unwrap_err().code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn outcome_keeps_status_and_representation_distinct() {
        let outcome = ExecutionOutcome::new(
            ExecutionStatus::Exited(7),
            ExecutionOutput::Bytes {
                stdout: b"value".to_vec(),
                stderr: Vec::new(),
            },
            Vec::new(),
            ExecutionCleanupState::Complete,
        )
        .unwrap();
        assert_eq!(outcome.status_code(), 7);
        assert!(matches!(outcome.output, ExecutionOutput::Bytes { .. }));
    }

    #[test]
    fn absolute_deadline_expires_after_planning() {
        let plan = ExecutionRequest::new(
            ExecutionSource::new("deadline", "true").unwrap(),
            ExecutionMode::NativeCommand,
        )
        .with_deadline(Duration::from_millis(1))
        .plan()
        .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        let error = plan.ensure_active("in the contract test").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("absolute deadline"));
    }

    #[test]
    fn command_normalization_accepts_exact_capture_and_rejects_limit_plus_one() {
        let exact = ExecutionOutcome::from_command(
            CommandOutcome {
                status: 0,
                stdout: Some("1234".to_owned()),
                stderr: Some(String::new()),
            },
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream: 4,
            },
        )
        .unwrap();
        assert!(matches!(
            exact.output,
            ExecutionOutput::Bytes { ref stdout, .. } if stdout == b"1234"
        ));

        let error = ExecutionOutcome::from_command(
            CommandOutcome {
                status: 0,
                stdout: Some("12345".to_owned()),
                stderr: None,
            },
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream: 4,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 4"));
    }
}
