//! Quirl's bounded native structured-data runtime.
//!
//! This crate owns streams, data envelopes, adapters, evaluation, and rendering
//! over the shared [`DataValue`] contract without depending on UI or CLI layers.
//! JSON, YAML, TOML, and bytes are explicit conversion boundaries; CSV, POSIX
//! tar headers, and filesystem rows enter the evaluator as typed values.

#![cfg_attr(
    test,
    allow(
        dead_code_pub_in_binary,
        reason = "the libtest harness is an executable, but these public items remain library API"
    )
)]

pub mod syntax;
mod value_boundary;

use indexmap::{IndexMap, IndexSet};
use quirl_core::{
    DirectoryOptions, Entry, EntryKind, ErrorCode, ProcessHost, ProcessRequest, ShellError,
    StructuredValue, directory_entries_with_options,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use syntax::{
    BooleanOperator as SyntaxBooleanOperator, ComparisonOperator as SyntaxComparisonOperator,
    DataPredicate as SyntaxPredicate, DataSource, DataSyntaxDiagnostic, DataSyntaxDiagnosticKind,
    DataSyntaxLimits, DataTransform, SortDirection, Spanned,
};
use unicode_width::UnicodeWidthStr;
use value_boundary::{
    ValueUsage, data_value_from_json, data_value_from_syntax, data_value_from_toml,
    data_value_from_yaml, json_from_data_value, validate_data_value,
};

/// Resource limits applied before data enters the evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataLimits {
    /// Maximum UTF-8 bytes accepted in one data expression.
    pub max_source_bytes: usize,
    /// Maximum bytes read from one input file.
    pub max_file_bytes: u64,
    /// Maximum rows emitted by a source or retained by a collecting operation.
    pub max_rows: usize,
    /// Maximum fields accepted in a record or tabular row.
    pub max_fields: usize,
    /// Maximum nesting depth accepted in structured input.
    pub max_depth: usize,
    /// Maximum scalar and container nodes accepted in one value or materialization.
    pub max_nodes: usize,
    /// Maximum aggregate UTF-8 bytes retained by strings and record keys.
    pub max_retained_text_bytes: usize,
    /// Maximum approximate value bytes or exact rendered bytes retained at a collection boundary.
    pub max_materialized_bytes: usize,
    /// Wall-clock deadline applied to an explicitly injected external process.
    pub external_deadline: Duration,
    /// Maximum bytes retained for each captured output stream of an external process.
    pub max_external_output_bytes: usize,
}

impl DataLimits {
    /// Conservative limits used by a default [`DataRuntime`].
    pub const DEFAULT: Self = Self {
        max_source_bytes: 256 * 1024,
        max_file_bytes: 8 * 1024 * 1024,
        max_rows: 100_000,
        max_fields: 256,
        max_depth: 64,
        max_nodes: 100_000,
        max_retained_text_bytes: 8 * 1024 * 1024,
        max_materialized_bytes: 16 * 1024 * 1024,
        external_deadline: Duration::from_secs(2),
        max_external_output_bytes: 1024 * 1024,
    };
}

impl Default for DataLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Compatibility name for the shared structured value owned by `quirl-core`.
///
/// Data parsing, streams, and rendering remain in this crate; the value itself
/// is shared with other execution front doors without a second ABI type.
pub type DataValue = StructuredValue;

/// Stable machine-facing data shapes. `Option`, `Result`, and `Task` are
/// envelopes rather than hidden control flow. Runtime failures remain
/// `ShellError` until a caller deliberately serializes one into a result/task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DataEnvelope {
    /// One scalar or structured value.
    Value {
        /// The contained typed value.
        value: DataValue,
    },
    /// A finite, already-materialized sequence of values.
    Stream {
        /// Values in source order.
        items: Vec<DataValue>,
    },
    /// Explicit optional control flow.
    Option {
        /// The present envelope, or `None` for an absent value.
        value: Option<Box<DataEnvelope>>,
    },
    /// Explicit success or failure control flow.
    Result {
        /// Whether the operation succeeded or failed.
        state: ResultState,
        /// Successful payload when `state` is [`ResultState::Ok`].
        value: Option<Box<DataEnvelope>>,
        /// Failure payload when `state` is [`ResultState::Error`].
        error: Option<ShellError>,
    },
    /// Explicit asynchronous task state.
    Task {
        /// Current task lifecycle state.
        state: TaskState,
        /// Completed task payload, when available.
        value: Option<Box<DataEnvelope>>,
        /// Failed task diagnostic, when available.
        error: Option<ShellError>,
    },
}

/// Completion state for an explicit data result envelope.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultState {
    /// The result contains a successful value.
    Ok,
    /// The result contains a [`ShellError`].
    Error,
}

/// Lifecycle state for an explicit task envelope.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// The task has not completed yet.
    Pending,
    /// The task completed with a value.
    Complete,
    /// Cancellation stopped the task before completion.
    Cancelled,
    /// The task completed with an error.
    Failed,
}

impl DataEnvelope {
    /// Wrap one already-typed value without a serialization round trip.
    pub const fn value(value: DataValue) -> Self {
        Self::Value { value }
    }
    /// Wrap already-typed materialized rows in a stream envelope.
    pub const fn stream(items: Vec<DataValue>) -> Self {
        Self::Stream { items }
    }
    /// Construct a present optional envelope.
    pub fn some(value: Self) -> Self {
        Self::Option {
            value: Some(Box::new(value)),
        }
    }
    /// Construct an absent optional envelope.
    pub fn none() -> Self {
        Self::Option { value: None }
    }
    /// Construct a successful result envelope.
    pub fn result(value: Self) -> Self {
        Self::Result {
            state: ResultState::Ok,
            value: Some(Box::new(value)),
            error: None,
        }
    }
    /// Construct a failed result envelope carrying `error`.
    pub fn result_error(error: ShellError) -> Self {
        Self::Result {
            state: ResultState::Error,
            value: None,
            error: Some(error),
        }
    }
    /// Construct a completed task envelope.
    pub fn task(value: Self) -> Self {
        Self::Task {
            state: TaskState::Complete,
            value: Some(Box::new(value)),
            error: None,
        }
    }
    /// Construct a task that has not completed.
    pub fn pending_task() -> Self {
        Self::Task {
            state: TaskState::Pending,
            value: None,
            error: None,
        }
    }
    /// Construct a task stopped by cancellation.
    pub fn cancelled_task() -> Self {
        Self::Task {
            state: TaskState::Cancelled,
            value: None,
            error: None,
        }
    }
    /// Construct a failed task carrying `error`.
    pub fn failed_task(error: ShellError) -> Self {
        Self::Task {
            state: TaskState::Failed,
            value: None,
            error: Some(error),
        }
    }
    /// Render this envelope within the default 16 MiB emitted-byte ceiling.
    pub fn render(&self, format: DataRenderFormat) -> Result<String, ShellError> {
        self.render_with_limits(format, DataLimits::DEFAULT)
    }

    /// Render this envelope while enforcing `limits.max_materialized_bytes`
    /// against every emitted byte before retaining it in the returned string.
    pub fn render_with_limits(
        &self,
        format: DataRenderFormat,
        limits: DataLimits,
    ) -> Result<String, ShellError> {
        self.validate(limits)?;
        collect_rendered(limits.max_materialized_bytes, |writer| {
            render_envelope_to(self, format, limits, writer)
        })
    }

    /// Validate control-state coherence and retained typed values iteratively.
    ///
    /// Task values are declarative states only; validation never polls, waits,
    /// schedules work, or executes callbacks.
    pub fn validate(&self, limits: DataLimits) -> Result<(), ShellError> {
        let mut stack = vec![(self, 0_usize)];
        let mut usage = ValueUsage::default();
        while let Some((envelope, depth)) = stack.pop() {
            if depth > limits.max_depth {
                return Err(resource_limit_error(
                    "data envelope nesting depth",
                    limits.max_depth,
                    depth,
                    "Flatten nested control envelopes or raise the configured data depth limit",
                ));
            }
            usage.nodes = usage.nodes.saturating_add(1);
            if usage.nodes > limits.max_nodes {
                return Err(resource_limit_error(
                    "data envelope and value nodes",
                    limits.max_nodes,
                    usage.nodes,
                    "Reduce the materialized envelope or raise the configured data node limit",
                ));
            }
            match envelope {
                Self::Value { value } => {
                    merge_usage(&mut usage, validate_data_value(value, limits)?)
                }
                Self::Stream { items } => {
                    if items.len() > limits.max_rows {
                        return Err(resource_limit_error(
                            "data envelope rows",
                            limits.max_rows,
                            items.len(),
                            "Retain fewer stream rows or raise the configured row limit",
                        ));
                    }
                    for item in items {
                        merge_usage(&mut usage, validate_data_value(item, limits)?);
                    }
                }
                Self::Option { value } => stack.extend(
                    value
                        .as_deref()
                        .map(|value| (value, depth.saturating_add(1))),
                ),
                Self::Result {
                    state,
                    value,
                    error,
                } => {
                    let coherent = matches!(
                        (state, value.is_some(), error.is_some()),
                        (ResultState::Ok, true, false) | (ResultState::Error, false, true)
                    );
                    if !coherent {
                        return Err(control_state_error("result"));
                    }
                    if let Some(error) = error {
                        merge_usage(&mut usage, control_error_usage(error, limits)?);
                    }
                    stack.extend(
                        value
                            .as_deref()
                            .map(|value| (value, depth.saturating_add(1))),
                    );
                }
                Self::Task {
                    state,
                    value,
                    error,
                } => {
                    let coherent = matches!(
                        (state, value.is_some(), error.is_some()),
                        (TaskState::Pending | TaskState::Cancelled, false, false)
                            | (TaskState::Complete, true, false)
                            | (TaskState::Failed, false, true)
                    );
                    if !coherent {
                        return Err(control_state_error("task"));
                    }
                    if let Some(error) = error {
                        merge_usage(&mut usage, control_error_usage(error, limits)?);
                    }
                    stack.extend(
                        value
                            .as_deref()
                            .map(|value| (value, depth.saturating_add(1))),
                    );
                }
            }
            validate_materialization_usage(usage, limits)?;
        }
        Ok(())
    }
}

/// Output representation selected at a data-rendering boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRenderFormat {
    /// Stable typed JSON suitable for machine consumers.
    Json,
    /// Terminal-safe line-oriented text.
    Plain,
    /// A framed, Unicode-width-aligned table that collects streams within their configured row bound.
    Table,
}

/// A pull-based stream. Calling `next` performs at most one row of work and
/// checks cancellation before consuming it. Sources and consumers share the
/// configured row, node, text, and materialization budgets.
type StreamPull = dyn FnMut(&AtomicBool) -> Result<Option<DataValue>, ShellError> + Send;

/// A bounded, pull-based sequence of typed rows.
pub struct DataStream {
    pull: Box<StreamPull>,
    emitted: usize,
    limits: DataLimits,
}

impl DataStream {
    fn from_values(values: Vec<DataValue>, limits: DataLimits) -> Self {
        Self {
            pull: Box::new({
                let mut values = values.into_iter();
                move |_| Ok(values.next())
            }),
            emitted: 0,
            limits,
        }
    }

    fn from_iterator(
        iterator: impl Iterator<Item = Result<DataValue, ShellError>> + Send + 'static,
        limits: DataLimits,
    ) -> Self {
        Self {
            pull: Box::new({
                let mut iterator = iterator;
                move |_| iterator.next().transpose()
            }),
            emitted: 0,
            limits,
        }
    }

    fn from_lines(bytes: String, limits: DataLimits) -> Self {
        let mut offset = 0;
        Self::from_pull(
            move |_| {
                if offset >= bytes.len() {
                    return Ok(None);
                }
                let remaining = bytes.get(offset..).unwrap_or_default();
                let (line, consumed) = match remaining.find('\n') {
                    Some(index) => (
                        remaining.get(..index).unwrap_or_default(),
                        index.saturating_add(1),
                    ),
                    None => (remaining, remaining.len()),
                };
                offset = offset.saturating_add(consumed);
                Ok(Some(DataValue::String(
                    line.strip_suffix('\r').unwrap_or(line).to_owned(),
                )))
            },
            limits,
        )
    }

    fn from_pull(
        pull: impl FnMut(&AtomicBool) -> Result<Option<DataValue>, ShellError> + Send + 'static,
        limits: DataLimits,
    ) -> Self {
        Self {
            pull: Box::new(pull),
            emitted: 0,
            limits,
        }
    }

    fn map(
        self,
        transform: impl Fn(DataValue) -> Result<DataValue, ShellError> + Send + 'static,
    ) -> Self {
        let limits = self.limits();
        let mut source = self;
        Self::from_pull(
            move |cancelled| source.next(cancelled)?.map(&transform).transpose(),
            limits,
        )
    }

    fn filter(
        self,
        predicate: impl Fn(&DataValue) -> Result<bool, ShellError> + Send + 'static,
    ) -> Self {
        let limits = self.limits();
        let mut source = self;
        Self::from_pull(
            move |cancelled| loop {
                check_cancelled(cancelled)?;
                let Some(value) = source.next(cancelled)? else {
                    return Ok(None);
                };
                if predicate(&value)? {
                    return Ok(Some(value));
                }
            },
            limits,
        )
    }

    fn take(self, count: usize) -> Self {
        let limits = self.limits();
        let mut source = self;
        let mut remaining = count;
        Self::from_pull(
            move |cancelled| {
                if remaining == 0 {
                    return Ok(None);
                }
                let value = source.next(cancelled)?;
                if value.is_some() {
                    remaining = remaining.saturating_sub(1);
                }
                Ok(value)
            },
            limits,
        )
    }

    const fn limits(&self) -> DataLimits {
        self.limits
    }

    /// Pull at most one row, observing cancellation and the configured row limit.
    pub fn next(&mut self, cancelled: &AtomicBool) -> Result<Option<DataValue>, ShellError> {
        check_cancelled(cancelled)?;
        if self.emitted == self.limits.max_rows {
            return match (self.pull)(cancelled)? {
                None => Ok(None),
                Some(_) => Err(resource_limit_error(
                    "stream rows",
                    self.limits.max_rows,
                    self.limits.max_rows.saturating_add(1),
                    "Use `take <count>` or raise the configured data row limit",
                )),
            };
        }
        let value = (self.pull)(cancelled)?;
        if let Some(value) = &value {
            validate_data_value(value, self.limits)?;
            self.emitted = self.emitted.saturating_add(1);
        }
        Ok(value)
    }

    /// Consume the stream at an explicit materialization boundary.
    ///
    /// Rows, aggregate nodes, retained text, and approximate retained bytes are
    /// checked before each row is appended.
    pub fn collect(mut self, cancelled: &AtomicBool) -> Result<Vec<DataValue>, ShellError> {
        let mut values = Vec::new();
        let mut usage = ValueUsage::default();
        while let Some(value) = self.next(cancelled)? {
            let row_usage = validate_data_value(&value, self.limits)?;
            usage.nodes = usage.nodes.saturating_add(row_usage.nodes);
            usage.text_bytes = usage.text_bytes.saturating_add(row_usage.text_bytes);
            usage.retained_bytes = usage
                .retained_bytes
                .saturating_add(row_usage.retained_bytes);
            validate_materialization_usage(usage, self.limits)?;
            values.push(value);
        }
        Ok(values)
    }
}

#[derive(Clone)]
/// Evaluates bounded native data expressions and explicitly granted processes.
pub struct DataRuntime {
    limits: DataLimits,
    process_host: Option<ProcessHost>,
}

/// The result of evaluating a pipeline before a renderer chooses to consume it.
/// A stream deliberately has no `Clone` implementation: duplicating a live
/// reader would hide an I/O or memory boundary from callers.
pub enum DataOutput {
    /// An already-materialized scalar or structured value.
    Value(DataValue),
    /// A pull-based stream whose rows have not necessarily been read yet.
    Stream(DataStream),
    /// Optional output produced by focused operations such as `first`.
    Option(Option<Box<DataOutput>>),
}

impl DataOutput {
    fn into_value(self, cancelled: &AtomicBool) -> Result<DataValue, ShellError> {
        match self {
            Self::Value(value) => Ok(value),
            Self::Stream(stream) => stream.collect(cancelled).map(DataValue::List),
            Self::Option(_) => Err(ShellError::new(
                ErrorCode::Data,
                "collected value API cannot erase an optional result",
            )
            .with_help("Use `eval_envelope` to preserve the explicit Option state")),
        }
    }

    /// Materialize this output into the stable bounded data envelope.
    ///
    /// Live streams are collected only at this explicit boundary and remain
    /// subject to their configured row, field, depth, and retained-value limits.
    pub fn into_envelope(self, cancelled: &AtomicBool) -> Result<DataEnvelope, ShellError> {
        match self {
            Self::Value(value) => Ok(DataEnvelope::value(value)),
            Self::Stream(stream) => Ok(DataEnvelope::stream(stream.collect(cancelled)?)),
            Self::Option(value) => match value {
                Some(value) => Ok(DataEnvelope::some(value.into_envelope(cancelled)?)),
                None => Ok(DataEnvelope::none()),
            },
        }
    }

    /// Render this output into an owned UTF-8 string under `limits`.
    ///
    /// This convenience boundary checks every emitted byte before buffering it
    /// and rejects output above `max_materialized_bytes`. Stream rows are still
    /// pulled individually, while table rendering materializes rows within the
    /// supplied limit. Callers must pass the limits that originated this output;
    /// values and absent options intentionally carry no hidden fallback policy.
    pub fn render(
        self,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
        limits: DataLimits,
    ) -> Result<String, ShellError> {
        self.render_with_limits(format, cancelled, limits)
    }

    /// Render to an output sink without applying a hidden aggregate output
    /// ceiling. Plain and JSON streams write each row as it is pulled; callers
    /// that need one bounded `String` can use [`Self::render`]. `limits` must be
    /// the policy that originated this output so value validation cannot fall
    /// back to a different runtime configuration.
    pub fn render_to(
        self,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
        writer: &mut impl Write,
        limits: DataLimits,
    ) -> Result<(), ShellError> {
        self.render_to_with_limits(format, cancelled, writer, limits)
    }

    fn render_with_limits(
        self,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
        limits: DataLimits,
    ) -> Result<String, ShellError> {
        collect_rendered(limits.max_materialized_bytes, |writer| {
            self.render_to_with_limits(format, cancelled, writer, limits)
        })
    }

    fn render_to_with_limits(
        self,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
        writer: &mut impl Write,
        limits: DataLimits,
    ) -> Result<(), ShellError> {
        check_cancelled(cancelled)?;
        match self {
            Self::Value(value) => {
                let envelope = DataEnvelope::value(value);
                envelope.validate(limits)?;
                render_envelope_to(&envelope, format, limits, writer)
            }
            Self::Stream(stream) => render_stream_to(stream, format, cancelled, writer),
            Self::Option(value) => {
                let envelope = match value {
                    Some(value) => DataEnvelope::some(value.into_envelope(cancelled)?),
                    None => DataEnvelope::none(),
                };
                envelope.validate(limits)?;
                render_envelope_to(&envelope, format, limits, writer)
            }
        }
    }
}

impl Default for DataRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl DataRuntime {
    /// Create a runtime with [`DataLimits::DEFAULT`] and no process capability.
    pub const fn new() -> Self {
        Self {
            limits: DataLimits::DEFAULT,
            process_host: None,
        }
    }

    /// Create a runtime with caller-supplied limits and no process capability.
    pub const fn with_limits(limits: DataLimits) -> Self {
        Self {
            limits,
            process_host: None,
        }
    }

    /// Attach the composition-root process host required by `^external`.
    /// Standalone runtimes deliberately have no ambient process capability.
    pub fn with_process_host(process_host: ProcessHost) -> Self {
        Self {
            limits: DataLimits::DEFAULT,
            process_host: Some(process_host),
        }
    }

    /// Create a bounded runtime with an explicitly injected process capability.
    pub fn with_limits_and_process_host(limits: DataLimits, process_host: ProcessHost) -> Self {
        Self {
            limits,
            process_host: Some(process_host),
        }
    }
    /// Return the immutable limits applied by this runtime.
    pub const fn limits(&self) -> DataLimits {
        self.limits
    }

    /// Evaluate `source` and return a stable typed envelope.
    pub fn eval_envelope(&self, source: &str) -> Result<DataEnvelope, ShellError> {
        self.eval_output(source)?
            .into_envelope(&AtomicBool::new(false))
    }

    /// Evaluate without choosing a terminal or machine serialization boundary.
    /// Callers that render a stream pull rows as they render them; callers that
    /// need a single `Value` opt into collection through `eval`.
    pub fn eval_output(&self, source: &str) -> Result<DataOutput, ShellError> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.eval_output_with_token(source, &cancelled, Some(Arc::clone(&cancelled)))
    }

    /// Evaluate while observing a borrowed cancellation flag.
    ///
    /// A borrowed flag cannot be forwarded to a spawned external process; use
    /// [`Self::eval_output_with_cancellation_handle`] for that boundary.
    pub fn eval_output_with_cancellation(
        &self,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<DataOutput, ShellError> {
        self.eval_output_with_token(source, cancelled, None)
    }

    /// Evaluate with a shareable cancellation token. Use this entry point for
    /// pipelines that may cross the `^external` byte boundary so cancellation
    /// reaches the injected process host.
    pub fn eval_output_with_cancellation_handle(
        &self,
        source: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<DataOutput, ShellError> {
        self.eval_output_with_token(source, &cancelled, Some(Arc::clone(&cancelled)))
    }

    fn eval_output_with_token(
        &self,
        source: &str,
        cancelled: &AtomicBool,
        cancellation_handle: Option<Arc<AtomicBool>>,
    ) -> Result<DataOutput, ShellError> {
        validate_limits(self.limits)?;
        let syntax_limits = syntax_limits(self.limits);
        let expression =
            syntax::parse_data_expression(source, syntax_limits).map_err(syntax_shell_error)?;
        check_cancelled(cancelled)?;
        let mut output = evaluate_source_output(
            &expression.source,
            self.limits,
            self.process_host.as_ref(),
            cancellation_handle,
        )?;
        validate_data_output(&output, self.limits)?;
        for transform in &expression.transforms {
            check_cancelled(cancelled)?;
            output = apply_output_transform(output, transform, self.limits, cancelled)?;
            validate_data_output(&output, self.limits)?;
        }
        Ok(output)
    }

    /// Evaluate and render `source` into an owned UTF-8 string.
    pub fn render(
        &self,
        source: &str,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
    ) -> Result<String, ShellError> {
        self.eval_output_with_cancellation(source, cancelled)?
            .render_with_limits(format, cancelled, self.limits)
    }

    /// Evaluate and write a result without buffering plain/JSON streams in the
    /// CLI. Table output remains a documented bounded collection boundary.
    pub fn render_to(
        &self,
        source: &str,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
        writer: &mut impl Write,
    ) -> Result<(), ShellError> {
        self.eval_output_with_cancellation(source, cancelled)?
            .render_to_with_limits(format, cancelled, writer, self.limits)
    }

    /// Stream a pipeline with a cancellation token that can also stop a
    /// bounded injected external process.
    pub fn render_to_with_cancellation_handle(
        &self,
        source: &str,
        format: DataRenderFormat,
        cancelled: Arc<AtomicBool>,
        writer: &mut impl Write,
    ) -> Result<(), ShellError> {
        self.eval_output_with_cancellation_handle(source, Arc::clone(&cancelled))?
            .render_to_with_limits(format, &cancelled, writer, self.limits)
    }

    /// Open a stream without collecting it. CSV rows are parsed only as the
    /// caller pulls them. Other bounded adapters validate before exposing rows.
    pub fn open_stream(&self, path: impl AsRef<Path>) -> Result<DataStream, ShellError> {
        validate_limits(self.limits)?;
        let path = path.as_ref();
        match extension(path).as_deref() {
            Some("csv") => open_csv_stream(path, self.limits),
            Some("tar") => open_tar_stream(path, self.limits),
            _ => match self.open_value(path)? {
                DataValue::List(values) => Ok(DataStream::from_values(values, self.limits)),
                value => Ok(DataStream::from_values(vec![value], self.limits)),
            },
        }
    }

    /// Evaluate `source` and collect its result into one typed value.
    ///
    /// Live streams materialize only at this named convenience boundary.
    pub fn eval_typed(&self, source: &str) -> Result<DataValue, ShellError> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.eval_output_with_token(source, &cancelled, Some(Arc::clone(&cancelled)))?
            .into_value(&cancelled)
    }

    /// Evaluate and collect a typed result while observing cancellation.
    pub fn eval_typed_with_cancellation(
        &self,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<DataValue, ShellError> {
        self.eval_output_with_cancellation(source, cancelled)?
            .into_value(cancelled)
    }

    /// Evaluate and explicitly convert the collected typed result to ordinary JSON.
    ///
    /// This compatibility API is a named lossy boundary for existing script and
    /// watch consumers. Prefer [`Self::eval_typed`] or [`Self::eval_output`]
    /// when domain tags must survive.
    pub fn eval(&self, source: &str) -> Result<serde_json::Value, ShellError> {
        self.eval_typed(source)
            .and_then(|value| json_from_data_value(&value, self.limits))
    }

    /// Evaluate with cancellation and explicitly convert the result to ordinary JSON.
    pub fn eval_with_cancellation(
        &self,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<serde_json::Value, ShellError> {
        self.eval_typed_with_cancellation(source, cancelled)
            .and_then(|value| json_from_data_value(&value, self.limits))
    }

    /// Evaluate and deliberately capture success or failure in an explicit result envelope.
    ///
    /// Normal evaluator entry points continue to return operating failures as
    /// `ShellError`; this method is the focused runtime's intentional Result boundary.
    pub fn eval_result_envelope(&self, source: &str) -> DataEnvelope {
        match self.eval_envelope(source) {
            Ok(value) => DataEnvelope::result(value),
            Err(error) => DataEnvelope::result_error(error),
        }
    }

    fn open_value(&self, path: &Path) -> Result<DataValue, ShellError> {
        validate_limits(self.limits)?;
        match extension(path).as_deref() {
            Some("csv") => return Ok(DataValue::List(read_csv(path, self.limits)?)),
            Some("tar") => return Ok(DataValue::List(read_tar(path, self.limits)?)),
            _ => {}
        }
        let contents = read_bounded_utf8(path, self.limits.max_file_bytes)?;
        let value = match extension(path).as_deref() {
            Some("json") => {
                let parsed = serde_json::from_str(&contents).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Data,
                        format!("cannot parse JSON in {}", path.display()),
                    )
                    .with_context(error.to_string())
                    .with_help("Correct the JSON syntax or use a .toml/.csv adapter")
                })?;
                data_value_from_json(parsed, self.limits)?
            }
            Some("toml") => {
                let parsed: toml::Value = toml::from_str(&contents).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Data,
                        format!("cannot parse TOML in {}", path.display()),
                    )
                    .with_context(error.to_string())
                    .with_help("Correct the TOML syntax before opening the file")
                })?;
                data_value_from_toml(parsed, self.limits)?
            }
            Some("yaml") | Some("yml") => {
                reject_yaml_references(&contents, path)?;
                let parsed = serde_yaml_ng::from_str(&contents).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Data,
                        format!("cannot parse YAML in {}", path.display()),
                    )
                    .with_context(error.to_string())
                    .with_help("Correct the YAML syntax before opening the file")
                })?;
                data_value_from_yaml(parsed, self.limits)?
            }
            _ => DataValue::String(contents),
        };
        validate_data_value(&value, self.limits)?;
        Ok(value)
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), ShellError> {
    if cancelled.load(AtomicOrdering::Relaxed) {
        Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "typed data evaluation was cancelled",
        )
        .with_help("Retry the pipeline when cancellation is no longer requested"))
    } else {
        Ok(())
    }
}

fn validate_limits(limits: DataLimits) -> Result<(), ShellError> {
    let zero_limit = [
        ("source bytes", limits.max_source_bytes == 0),
        ("file bytes", limits.max_file_bytes == 0),
        ("rows", limits.max_rows == 0),
        ("fields", limits.max_fields == 0),
        ("nodes", limits.max_nodes == 0),
        ("retained text bytes", limits.max_retained_text_bytes == 0),
        ("materialized bytes", limits.max_materialized_bytes == 0),
        (
            "external output bytes",
            limits.max_external_output_bytes == 0,
        ),
    ]
    .into_iter()
    .find_map(|(name, zero)| zero.then_some(name));
    if let Some(name) = zero_limit {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("data {name} limit must be greater than zero"),
        )
        .with_help("Configure a positive bound for every retained data resource"));
    }
    if limits.external_deadline.is_zero() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "data external process deadline must be greater than zero",
        )
        .with_help("Configure a positive external process deadline"));
    }
    Ok(())
}

fn validate_data_output(output: &DataOutput, limits: DataLimits) -> Result<(), ShellError> {
    let mut current = Some(output);
    while let Some(output) = current.take() {
        match output {
            DataOutput::Value(value) => {
                validate_data_value(value, limits)?;
            }
            DataOutput::Stream(_) | DataOutput::Option(None) => {}
            DataOutput::Option(Some(value)) => current = Some(value),
        }
    }
    Ok(())
}

fn evaluate_source(
    source: &Spanned<DataSource>,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    match &source.value {
        DataSource::Pwd => std::env::current_dir()
            .map(|path| DataValue::Path(path.display().to_string()))
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "cannot read the current directory")
                    .with_context(error.to_string())
                    .with_help("Check that the current directory still exists and is accessible")
            }),
        DataSource::Files { path } => {
            let path = path
                .as_ref()
                .map_or_else(|| PathBuf::from("."), |path| PathBuf::from(&path.value));
            let entries = directory_entries_with_options(
                &path,
                &DirectoryOptions {
                    max_entries: limits.max_rows,
                    ..DirectoryOptions::default()
                },
            )?;
            let value = DataValue::List(entries.into_iter().map(directory_entry_value).collect());
            validate_data_value(&value, limits)?;
            Ok(value)
        }
        DataSource::Open { path } => {
            let path = PathBuf::from(&path.value);
            DataRuntime::with_limits(limits).open_value(&path)
        }
        DataSource::Literal(literal) => data_value_from_syntax(literal, limits),
        DataSource::External { .. } => Err(data_error(
            "^external",
            "external sources require the explicit process-host output boundary",
        )),
    }
}

fn directory_entry_value(entry: Entry) -> DataValue {
    let kind = match entry.kind {
        EntryKind::Directory => "directory",
        EntryKind::File => "file",
        EntryKind::Symlink => "symlink",
        EntryKind::Other => "other",
    };
    let modified = entry
        .modified_unix_seconds
        .map_or(DataValue::Nothing, |seconds| {
            // Fixed-width Unix seconds preserve chronological lexical ordering
            // without adding a datetime dependency to the data runtime.
            DataValue::DateTime(format!("unix:{seconds:020}"))
        });
    let target = entry.symlink_target.map_or(DataValue::Nothing, |path| {
        DataValue::Path(path.display().to_string())
    });
    DataValue::Record(IndexMap::from([
        ("hidden".to_owned(), DataValue::Bool(entry.hidden)),
        ("kind".to_owned(), DataValue::String(kind.to_owned())),
        ("modified".to_owned(), modified),
        ("name".to_owned(), DataValue::String(entry.name)),
        (
            "path".to_owned(),
            DataValue::Path(entry.path.display().to_string()),
        ),
        ("readonly".to_owned(), DataValue::Bool(entry.readonly)),
        ("size".to_owned(), DataValue::Size { bytes: entry.size }),
        ("target".to_owned(), target),
    ]))
}

fn evaluate_source_output(
    source: &Spanned<DataSource>,
    limits: DataLimits,
    process_host: Option<&ProcessHost>,
    cancellation_handle: Option<Arc<AtomicBool>>,
) -> Result<DataOutput, ShellError> {
    if let DataSource::External { command } = &source.value {
        let process_host = process_host.ok_or_else(|| {
            ShellError::new(
                ErrorCode::Validation,
                "external data source is unavailable without an injected process host",
            )
            .with_help("Run through the Quirl CLI or inject a bounded ProcessHost explicitly")
        })?;
        let cancelled = cancellation_handle.ok_or_else(|| {
            ShellError::new(
                ErrorCode::Validation,
                "external data source requires a shareable cancellation token",
            )
            .with_help("Use `eval_output_with_cancellation_handle` for cancellable external data")
        })?;
        let outcome = process_host(ProcessRequest {
            command: command.value.clone(),
            deadline: limits.external_deadline,
            cancelled,
            max_output_bytes: limits.max_external_output_bytes,
        })?;
        if outcome.status != 0 {
            let mut error = ShellError::new(
                ErrorCode::Data,
                format!("external data source exited with status {}", outcome.status),
            )
            .with_command(&command.value)
            .with_help(
                "Inspect the command's stderr, fix the input, then run the data pipeline again",
            );
            if let Some(stderr) = outcome.stderr.filter(|stderr| !stderr.is_empty()) {
                error = error.with_context(stderr);
            }
            error.details.exit_status = Some(outcome.status);
            return Err(error);
        }
        return Ok(DataOutput::Value(DataValue::String(
            outcome.stdout.unwrap_or_default(),
        )));
    }
    match &source.value {
        DataSource::Open { path } => {
            let path = PathBuf::from(&path.value);
            let runtime = DataRuntime::with_limits(limits);
            match extension(&path).as_deref() {
                Some("csv") | Some("tar") => Ok(DataOutput::Stream(runtime.open_stream(path)?)),
                _ => match runtime.open_value(&path)? {
                    DataValue::List(rows) => {
                        Ok(DataOutput::Stream(DataStream::from_values(rows, limits)))
                    }
                    value => Ok(DataOutput::Value(value)),
                },
            }
        }
        DataSource::Files { .. } => {
            let DataValue::List(rows) = evaluate_source(source, limits)? else {
                return Err(ShellError::new(
                    ErrorCode::Data,
                    "filesystem source did not produce directory rows",
                )
                .with_help("Use `files [path]` to produce directory-entry records"));
            };
            Ok(DataOutput::Stream(DataStream::from_values(rows, limits)))
        }
        DataSource::Pwd | DataSource::Literal(_) => match evaluate_source(source, limits)? {
            DataValue::List(rows) => Ok(DataOutput::Stream(DataStream::from_values(rows, limits))),
            value => Ok(DataOutput::Value(value)),
        },
        DataSource::External { .. } => Err(ShellError::new(
            ErrorCode::Data,
            "external data source escaped its explicit evaluation boundary",
        )
        .with_help("Report this internal data runtime invariant failure")),
    }
}

fn apply_output_transform(
    output: DataOutput,
    transform: &Spanned<DataTransform>,
    limits: DataLimits,
    cancelled: &AtomicBool,
) -> Result<DataOutput, ShellError> {
    if let DataOutput::Option(value) = output {
        return match value {
            Some(value) => apply_output_transform(*value, transform, limits, cancelled)
                .map(|value| DataOutput::Option(Some(Box::new(value)))),
            None => Ok(DataOutput::Option(None)),
        };
    }
    if let DataOutput::Value(value) = output {
        match &transform.value {
            DataTransform::Lines => {
                let DataValue::String(bytes) = value else {
                    return Err(data_error("lines", "lines expects a string byte value"));
                };
                return Ok(DataOutput::Stream(DataStream::from_lines(bytes, limits)));
            }
            DataTransform::FromJson => {
                let DataValue::String(bytes) = value else {
                    return Err(data_error(
                        "from json",
                        "from json expects a string byte value",
                    ));
                };
                return match parse_json_boundary(&bytes, "from json", limits)? {
                    DataValue::List(rows) => {
                        Ok(DataOutput::Stream(DataStream::from_values(rows, limits)))
                    }
                    value => Ok(DataOutput::Value(value)),
                };
            }
            DataTransform::ToJson => {
                return Ok(DataOutput::Value(DataValue::String(to_json_boundary(
                    &value, "to json", limits,
                )?)));
            }
            DataTransform::First => {
                let DataValue::List(values) = value else {
                    return Err(data_error("first", "first expects a list or stream"));
                };
                return Ok(DataOutput::Option(
                    values
                        .into_iter()
                        .next()
                        .map(DataOutput::Value)
                        .map(Box::new),
                ));
            }
            _ => {
                let value = apply_transform(value, &transform.value, limits)?;
                validate_data_value(&value, limits)?;
                return Ok(DataOutput::Value(value));
            }
        }
    }
    let DataOutput::Stream(mut stream) = output else {
        return Err(ShellError::new(
            ErrorCode::Data,
            "typed data output escaped exhaustive transform dispatch",
        )
        .with_help("Report this internal typed-data state invariant failure"));
    };
    match &transform.value {
        DataTransform::Lines => Err(data_error(
            "lines",
            "lines expects one string value; apply it immediately after ^external or from json",
        )),
        DataTransform::FromJson => Ok(DataOutput::Stream(stream.map(move |value| {
            let DataValue::String(bytes) = value else {
                return Err(data_error(
                    "from json",
                    "from json expects string stream items",
                ));
            };
            parse_json_boundary(&bytes, "from json", limits)
        }))),
        DataTransform::ToJson => Ok(DataOutput::Stream(stream.map(move |value| {
            to_json_boundary(&value, "to json", limits).map(DataValue::String)
        }))),
        DataTransform::Where(predicate) => {
            let predicate = RuntimePredicate::from_syntax(predicate, limits)?;
            Ok(DataOutput::Stream(stream.filter(move |row| {
                if !matches!(row, DataValue::Record(_)) {
                    return Err(data_error("where", "where expects object rows"));
                }
                predicate_matches(&predicate, row, "where")
            })))
        }
        DataTransform::Select { fields } => {
            let fields = fields
                .iter()
                .map(|field| field.value.clone())
                .collect::<Vec<_>>();
            Ok(DataOutput::Stream(
                stream.map(move |row| select_fields(row, &fields, "select")),
            ))
        }
        DataTransform::Get { path } => {
            let field = path.value.clone();
            Ok(DataOutput::Stream(
                stream.map(move |row| get_field(row, &field, "get")),
            ))
        }
        DataTransform::Take { count } => {
            let count = usize::try_from(count.value).map_err(|_| {
                limit_error(
                    "take count exceeds the platform index range",
                    "Use a smaller non-negative count",
                )
            })?;
            Ok(DataOutput::Stream(stream.take(count)))
        }
        DataTransform::First => Ok(DataOutput::Option(
            stream.next(cancelled)?.map(DataOutput::Value).map(Box::new),
        )),
        DataTransform::Length => {
            let mut length = 0_u64;
            while stream.next(cancelled)?.is_some() {
                length = length.checked_add(1).ok_or_else(|| {
                    limit_error(
                        "stream length exceeds the supported integer range",
                        "Use `take <count>` before `length`",
                    )
                })?;
            }
            Ok(DataOutput::Value(DataValue::UInt(length)))
        }
        DataTransform::Sort { field, direction } => {
            // Sorting is intentionally a collection boundary. The row limit is
            // still enforced by `collect`, and callers can make it explicit
            // with `take` before sorting.
            let rows = stream.collect(cancelled)?;
            let sorted = sort_rows(DataValue::List(rows), &field.value, *direction, "sort")?;
            let DataValue::List(rows) = sorted else {
                return Err(ShellError::new(
                    ErrorCode::Data,
                    "sort did not produce structured rows",
                )
                .with_help("Sort only a stream of object records"));
            };
            Ok(DataOutput::Stream(DataStream::from_values(rows, limits)))
        }
    }
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, format!("cannot {action} {}", path.display()))
        .with_context(error.to_string())
        .with_help("Check that the path exists and that Quirl has permission to read it")
}

const FILE_SIZE_LIMIT_ERROR: &str = "Quirl bounded reader reached its input limit";

/// A reader that proves the bytes consumed after opening a file stay within a
/// limit. Filesystem metadata is advisory: a path can be replaced or enlarged
/// between `metadata` and `open`, so every adapter enforces its bound on the
/// opened handle itself.
struct BoundedReader<R> {
    inner: R,
    limit: u64,
    consumed: u64,
}

impl<R> BoundedReader<R> {
    const fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            limit,
            consumed: 0,
        }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.consumed < self.limit {
            let remaining = self.limit.saturating_sub(self.consumed);
            let allowed = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            let target = buffer.get_mut(..allowed).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "bounded read size exceeds the destination buffer",
                )
            })?;
            let read = self.inner.read(target)?;
            self.consumed = self
                .consumed
                .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            return Ok(read);
        }

        // Probe once at the boundary. Returning EOF here accepts an input of
        // exactly `limit` bytes; seeing one more byte fails before it reaches a
        // parser or accumulates in memory.
        let mut probe = [0_u8; 1];
        if self.inner.read(&mut probe)? == 0 {
            Ok(0)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                FILE_SIZE_LIMIT_ERROR,
            ))
        }
    }
}

fn is_file_size_limit(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::InvalidData && error.to_string() == FILE_SIZE_LIMIT_ERROR
}

fn bounded_io_error(action: &str, path: &Path, limit: u64, error: std::io::Error) -> ShellError {
    if is_file_size_limit(&error) {
        resource_limit_error_u64(
            &format!("{} file bytes", path.display()),
            limit,
            limit.saturating_add(1),
            "Increase the data file limit or select a smaller input file",
        )
    } else {
        io_error(action, path, error)
    }
}

fn read_bounded_utf8(path: &Path, limit: u64) -> Result<String, ShellError> {
    let mut reader = open_bounded_file(path, limit)?;
    let mut contents = Vec::new();
    reader
        .read_to_end(&mut contents)
        .map_err(|error| bounded_io_error("read", path, limit, error))?;
    let mut contents = String::from_utf8(contents).map_err(|error| {
        ShellError::new(
            ErrorCode::Data,
            format!("data file {} is not valid UTF-8", path.display()),
        )
        .with_context(format!(
            "invalid UTF-8 begins at byte {}",
            error.utf8_error().valid_up_to()
        ))
        .with_help("Save the file as UTF-8 or use an explicit binary-data adapter")
    })?;
    if contents.starts_with('\u{feff}') {
        contents.drain(..'\u{feff}'.len_utf8());
    }
    Ok(contents)
}

fn bounded_text_io_error(
    action: &str,
    path: &Path,
    limit: u64,
    error: std::io::Error,
) -> ShellError {
    if is_file_size_limit(&error) {
        return bounded_io_error(action, path, limit, error);
    }
    if error.kind() == std::io::ErrorKind::InvalidData {
        return ShellError::new(
            ErrorCode::Data,
            format!("data file {} is not valid UTF-8", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Save the file as UTF-8 or use an explicit binary-data adapter");
    }
    io_error(action, path, error)
}

/// Open first, then inspect that exact handle before exposing it to a lazy
/// adapter. The follow-up [`BoundedReader`] still enforces the same limit for
/// regular files that grow after this metadata snapshot.
fn open_bounded_file(path: &Path, limit: u64) -> Result<BoundedReader<File>, ShellError> {
    let file = open_data_file(path).map_err(|error| io_error("open", path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect", path, error))?;
    if !metadata.is_file() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("data source {} is not a regular file", path.display()),
        )
        .with_context("directories, FIFOs, sockets, and device nodes are rejected")
        .with_help("Copy the input into a bounded regular file before opening it as data"));
    }
    if metadata.len() > limit {
        return Err(resource_limit_error_u64(
            &format!("{} file bytes", path.display()),
            limit,
            metadata.len(),
            "Increase the data file limit or select a smaller input file",
        ));
    }
    Ok(BoundedReader::new(file, limit))
}

fn open_data_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // Opening a read-only FIFO normally waits forever for a writer. Apply
        // O_NONBLOCK to the open operation itself, then reject every
        // non-regular handle from its exact metadata above. Regular files keep
        // their usual blocking read semantics on supported Unix kernels.
        options.custom_flags(nix::libc::O_NONBLOCK);
    }
    options.open(path)
}

fn limit_error(message: impl Into<String>, help: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::ResourceLimit, message).with_help(help)
}

fn resource_limit_error(name: &str, limit: usize, observed: usize, help: &str) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{name} exceeds its configured limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help(help)
}

fn resource_limit_error_u64(name: &str, limit: u64, observed: u64, help: &str) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{name} exceeds its configured limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help(help)
}

fn validate_materialization_usage(usage: ValueUsage, limits: DataLimits) -> Result<(), ShellError> {
    if usage.nodes > limits.max_nodes {
        return Err(resource_limit_error(
            "materialized value nodes",
            limits.max_nodes,
            usage.nodes,
            "Use a streaming transform or raise the configured data node limit",
        ));
    }
    if usage.text_bytes > limits.max_retained_text_bytes {
        return Err(resource_limit_error(
            "materialized retained text bytes",
            limits.max_retained_text_bytes,
            usage.text_bytes,
            "Use a streaming transform or raise the configured retained-text limit",
        ));
    }
    if usage.retained_bytes > limits.max_materialized_bytes {
        return Err(resource_limit_error(
            "materialized retained bytes",
            limits.max_materialized_bytes,
            usage.retained_bytes,
            "Use a streaming transform or raise the configured materialization limit",
        ));
    }
    Ok(())
}

fn merge_usage(total: &mut ValueUsage, value: ValueUsage) {
    total.nodes = total.nodes.saturating_add(value.nodes);
    total.text_bytes = total.text_bytes.saturating_add(value.text_bytes);
    total.retained_bytes = total.retained_bytes.saturating_add(value.retained_bytes);
}

fn control_state_error(kind: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("data {kind} envelope has an incoherent state"),
    )
    .with_help("Construct control envelopes with the typed DataEnvelope constructors")
}

fn control_error_usage(error: &ShellError, limits: DataLimits) -> Result<ValueUsage, ShellError> {
    let mut usage = ValueUsage {
        nodes: 1,
        text_bytes: error.message.len(),
        retained_bytes: std::mem::size_of::<ShellError>().saturating_add(error.message.len()),
    };
    for label in &error.details.labels {
        usage.nodes = usage.nodes.saturating_add(1);
        if let Some(source) = &label.source {
            observe_control_text(&mut usage, source);
        }
        observe_control_text(&mut usage, &label.message);
    }
    for context in &error.details.context {
        observe_control_text(&mut usage, context);
    }
    for help in &error.details.help {
        observe_control_text(&mut usage, help);
    }
    if let Some(command) = &error.details.command {
        observe_control_text(&mut usage, command);
    }
    validate_materialization_usage(usage, limits)?;
    Ok(usage)
}

/// Reject YAML graph references before `serde_yaml_ng` can replay an anchored
/// event subtree into an amplified materialized value. The scan recognizes
/// reference indicators only in YAML syntax, excluding quoted scalars,
/// comments, and indented block-scalar contents.
fn reject_yaml_references(contents: &str, path: &Path) -> Result<(), ShellError> {
    let mut block_parent_indent = None;
    let mut single_quoted = false;
    let mut double_quoted = false;
    let mut offset = 0_usize;
    for line in contents.split_inclusive('\n') {
        let line_without_newline = line.strip_suffix('\n').unwrap_or(line);
        let indentation = line_without_newline
            .as_bytes()
            .iter()
            .take_while(|byte| **byte == b' ')
            .count();
        if let Some(parent_indent) = block_parent_indent {
            if line_without_newline.trim().is_empty() || indentation > parent_indent {
                offset = offset.saturating_add(line.len());
                continue;
            }
            block_parent_indent = None;
        }

        let bytes = line_without_newline.as_bytes();
        let mut escaped = false;
        let mut index = 0_usize;
        while index < bytes.len() {
            let Some(byte) = bytes.get(index).copied() else {
                break;
            };
            if double_quoted {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    double_quoted = false;
                }
                index = index.saturating_add(1);
                continue;
            }
            if single_quoted {
                if byte == b'\'' {
                    if bytes.get(index.saturating_add(1)) == Some(&b'\'') {
                        index = index.saturating_add(2);
                        continue;
                    }
                    single_quoted = false;
                }
                index = index.saturating_add(1);
                continue;
            }
            match byte {
                b'"' => double_quoted = true,
                b'\'' => single_quoted = true,
                b'#' if yaml_indicator_boundary(bytes.get(index.wrapping_sub(1)).copied()) => break,
                b'|' | b'>'
                    if yaml_indicator_boundary(bytes.get(index.wrapping_sub(1)).copied()) =>
                {
                    block_parent_indent = Some(indentation);
                    break;
                }
                b'&' | b'*'
                    if yaml_indicator_boundary(bytes.get(index.wrapping_sub(1)).copied())
                        && bytes
                            .get(index.saturating_add(1))
                            .is_some_and(|next| !next.is_ascii_whitespace()) =>
                {
                    let kind = if byte == b'&' { "anchor" } else { "alias" };
                    return Err(ShellError::new(
                        ErrorCode::Data,
                        format!("YAML {kind}s are not supported by the bounded data adapter"),
                    )
                    .with_context(format!(
                        "{} reference indicator at byte {}",
                        path.display(),
                        offset.saturating_add(index)
                    ))
                    .with_help(
                        "Expand the referenced value explicitly before opening the YAML document",
                    ));
                }
                _ => {}
            }
            index = index.saturating_add(1);
        }
        offset = offset.saturating_add(line.len());
    }
    Ok(())
}

fn yaml_indicator_boundary(previous: Option<u8>) -> bool {
    previous.is_none_or(|byte| {
        byte.is_ascii_whitespace()
            || matches!(byte, b'-' | b'?' | b':' | b',' | b'[' | b']' | b'{' | b'}')
    })
}

fn observe_control_text(usage: &mut ValueUsage, text: &str) {
    usage.nodes = usage.nodes.saturating_add(1);
    usage.text_bytes = usage.text_bytes.saturating_add(text.len());
    usage.retained_bytes = usage
        .retained_bytes
        .saturating_add(std::mem::size_of::<String>())
        .saturating_add(text.len());
}

fn read_csv(path: &Path, limits: DataLimits) -> Result<Vec<DataValue>, ShellError> {
    let stream = open_csv_stream(path, limits)?;
    stream.collect(&AtomicBool::new(false))
}

fn open_csv_stream(path: &Path, limits: DataLimits) -> Result<DataStream, ShellError> {
    let mut lines = BufReader::new(open_bounded_file(path, limits.max_file_bytes)?).lines();
    let mut header = lines
        .next()
        .transpose()
        .map_err(|error| bounded_text_io_error("read", path, limits.max_file_bytes, error))?
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::Data,
                format!("CSV file {} has no header row", path.display()),
            )
            .with_help("Add a header row with one unique name per column")
        })?;
    if header.starts_with('\u{feff}') {
        header.drain(..'\u{feff}'.len_utf8());
    }
    let headers = parse_csv_record(&header, limits.max_fields, path, 1)?;
    validate_csv_headers(&headers, path)?;
    let display = path.display().to_string();
    let source_path = path.to_path_buf();
    let iterator = lines.enumerate().map(move |(index, line)| {
        let line_number = index.saturating_add(2);
        let line = line.map_err(|error| {
            bounded_text_io_error("read", &source_path, limits.max_file_bytes, error)
        })?;
        let fields = parse_csv_record_display(&line, limits.max_fields, &display, line_number)?;
        if fields.len() != headers.len() {
            return Err(csv_error_display(
                &display,
                line_number,
                format!(
                    "expected {} fields but found {}",
                    headers.len(),
                    fields.len()
                ),
            ));
        }
        let row = headers
            .iter()
            .cloned()
            .zip(fields)
            .map(|(key, value)| (key, DataValue::String(value)))
            .collect();
        Ok(DataValue::Record(row))
    });
    Ok(DataStream::from_iterator(iterator, limits))
}

fn validate_csv_headers(headers: &[String], path: &Path) -> Result<(), ShellError> {
    if headers.is_empty() {
        return Err(csv_error(path, 1, "header must contain at least one field"));
    }
    let mut seen = BTreeSet::new();
    for header in headers {
        if header.is_empty() || !seen.insert(header) {
            return Err(csv_error(
                path,
                1,
                "header names must be non-empty and unique",
            ));
        }
    }
    Ok(())
}

fn parse_csv_record(
    line: &str,
    fields_max: usize,
    path: &Path,
    line_number: usize,
) -> Result<Vec<String>, ShellError> {
    parse_csv_record_display(line, fields_max, &path.display().to_string(), line_number)
}

fn parse_csv_record_display(
    line: &str,
    fields_max: usize,
    path: &str,
    line_number: usize,
) -> Result<Vec<String>, ShellError> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut after_quote = false;
    for character in line.chars() {
        if quoted {
            if character == '"' {
                quoted = false;
                after_quote = true;
            } else {
                current.push(character);
            }
        } else if after_quote {
            match character {
                '"' => {
                    current.push('"');
                    quoted = true;
                    after_quote = false;
                }
                ',' => {
                    push_csv_field(&mut fields, &mut current, fields_max, path, line_number)?;
                    after_quote = false;
                }
                _ => {
                    return Err(csv_error_display(
                        path,
                        line_number,
                        "characters after a closing quote must be a comma",
                    ));
                }
            }
        } else {
            match character {
                ',' => push_csv_field(&mut fields, &mut current, fields_max, path, line_number)?,
                '"' if current.is_empty() => quoted = true,
                '"' => {
                    return Err(csv_error_display(
                        path,
                        line_number,
                        "quote must begin a field",
                    ));
                }
                _ => current.push(character),
            }
        }
    }
    if quoted {
        return Err(csv_error_display(
            path,
            line_number,
            "unclosed quoted field (multiline CSV fields are unsupported)",
        ));
    }
    push_csv_field(&mut fields, &mut current, fields_max, path, line_number)?;
    Ok(fields)
}

fn push_csv_field(
    fields: &mut Vec<String>,
    current: &mut String,
    fields_max: usize,
    path: &str,
    line_number: usize,
) -> Result<(), ShellError> {
    if fields.len() == fields_max {
        return Err(resource_limit_error(
            &format!("CSV fields at {path}:{line_number}"),
            fields_max,
            fields_max.saturating_add(1),
            "Select a narrower CSV file or raise the configured data field limit",
        ));
    }
    fields.push(std::mem::take(current));
    Ok(())
}

fn csv_error(path: &Path, line: usize, message: impl Into<String>) -> ShellError {
    csv_error_display(&path.display().to_string(), line, message)
}

fn csv_error_display(path: &str, line: usize, message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Data, format!("invalid CSV at {path}:{line}"))
        .with_context(message.into())
        .with_help("Use a header row, balanced quotes, and the same field count on every row")
}

fn read_tar(path: &Path, limits: DataLimits) -> Result<Vec<DataValue>, ShellError> {
    open_tar_stream(path, limits)?.collect(&AtomicBool::new(false))
}

/// Inspect uncompressed POSIX tar archives without extracting an entry. The
/// stream owns a single file reader and advances one header per pull, so a
/// caller that takes a few entries does not walk the rest of the archive.
fn open_tar_stream(path: &Path, limits: DataLimits) -> Result<DataStream, ShellError> {
    let display = path.display().to_string();
    let source_path = path.to_path_buf();
    let mut reader = BufReader::new(open_bounded_file(path, limits.max_file_bytes)?);
    let mut finished = false;
    Ok(DataStream::from_pull(
        move |cancelled| {
            check_cancelled(cancelled)?;
            if finished {
                return Ok(None);
            }
            let mut header = [0_u8; 512];
            let mut read = 0;
            while read < header.len() {
                let target = header.get_mut(read..).ok_or_else(|| {
                    tar_error_display(&display, "header read offset exceeds header bounds")
                })?;
                let count = reader.read(target).map_err(|error| {
                    bounded_io_error("read", &source_path, limits.max_file_bytes, error)
                })?;
                if count == 0 {
                    if read == 0 {
                        finished = true;
                        return Ok(None);
                    }
                    return Err(tar_error_display(&display, "truncated header"));
                }
                read = read.saturating_add(count);
            }
            if header.iter().all(|byte| *byte == 0) {
                finished = true;
                return Ok(None);
            }
            validate_tar_checksum(&header, &display)?;
            let size = tar_octal(&header[124..136], &display, "entry size")?;
            let name = tar_text(&header[0..100], &display, "entry name")?;
            let prefix = tar_text(&header[345..500], &display, "entry prefix")?;
            let path = if prefix.is_empty() {
                name
            } else if name.is_empty() {
                prefix
            } else {
                format!("{prefix}/{name}")
            };
            if path.is_empty() {
                return Err(tar_error_display(&display, "entry has an empty path"));
            }
            let kind = match header[156] {
                0 | b'0' => "file",
                b'1' => "hard_link",
                b'2' => "symlink",
                b'3' => "character_device",
                b'4' => "block_device",
                b'5' => "directory",
                b'6' => "fifo",
                b'x' | b'g' => {
                    return Err(tar_error_display(
                        &display,
                        "PAX extended headers are unsupported for 0.1.0 tar inspection",
                    ));
                }
                other => {
                    return Err(tar_error_display(
                        &display,
                        format!("unsupported entry type byte {other}"),
                    ));
                }
            };
            let payload_blocks = size.checked_add(511).ok_or_else(|| {
                tar_error_display(&display, "entry size overflows archive bounds")
            })? / 512;
            let payload_bytes = payload_blocks.checked_mul(512).ok_or_else(|| {
                tar_error_display(&display, "entry size overflows archive bounds")
            })?;
            let mut remaining = payload_bytes;
            let mut discard = [0_u8; 8192];
            while remaining > 0 {
                check_cancelled(cancelled)?;
                let discard_len = u64::try_from(discard.len()).unwrap_or(u64::MAX);
                let wanted = usize::try_from(remaining.min(discard_len)).map_err(|_| {
                    tar_error_display(&display, "entry size cannot be represented on this host")
                })?;
                let target = discard.get_mut(..wanted).ok_or_else(|| {
                    tar_error_display(&display, "payload read size exceeds discard buffer")
                })?;
                let count = reader.read(target).map_err(|error| {
                    bounded_io_error("read", &source_path, limits.max_file_bytes, error)
                })?;
                if count == 0 {
                    return Err(tar_error_display(&display, "entry payload is truncated"));
                }
                remaining = remaining.saturating_sub(u64::try_from(count).map_err(|_| {
                    tar_error_display(&display, "read size cannot be represented on this host")
                })?);
            }
            Ok(Some(DataValue::Record(IndexMap::from([
                ("kind".to_owned(), DataValue::String(kind.to_owned())),
                ("path".to_owned(), DataValue::Path(path)),
                ("size".to_owned(), DataValue::Size { bytes: size }),
            ]))))
        },
        limits,
    ))
}

fn validate_tar_checksum(header: &[u8; 512], display: &str) -> Result<(), ShellError> {
    let expected = tar_octal(&header[148..156], display, "header checksum")?;
    let actual = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    if expected == actual {
        Ok(())
    } else {
        Err(tar_error_display(
            display,
            format!("header checksum is {expected}, expected {actual}"),
        ))
    }
}

fn tar_text(bytes: &[u8], display: &str, field: &str) -> Result<String, ShellError> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(bytes.get(..end).unwrap_or_default())
        .map(str::to_owned)
        .map_err(|_| tar_error_display(display, format!("{field} is not valid UTF-8")))
}

fn tar_octal(bytes: &[u8], display: &str, field: &str) -> Result<u64, ShellError> {
    let text = tar_text(bytes, display, field)?;
    let text = text.trim_matches([' ', '\0']);
    if text.is_empty() {
        return Ok(0);
    }
    if !text.bytes().all(|byte| matches!(byte, b'0'..=b'7')) {
        return Err(tar_error_display(
            display,
            format!("{field} is not a POSIX octal integer"),
        ));
    }
    u64::from_str_radix(text, 8)
        .map_err(|_| tar_error_display(display, format!("{field} is too large")))
}

fn tar_error_display(path: &str, message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Data, format!("invalid tar archive {path}"))
        .with_context(message.into())
        .with_help("Use a readable uncompressed POSIX .tar archive with bounded entry names")
}

fn render_envelope_to(
    envelope: &DataEnvelope,
    format: DataRenderFormat,
    limits: DataLimits,
    writer: &mut impl Write,
) -> Result<(), ShellError> {
    match format {
        DataRenderFormat::Json => {
            write_typed_json_envelope(writer, envelope)?;
            write_output(writer, b"\n")
        }
        DataRenderFormat::Plain => {
            write_plain_envelope(writer, envelope, limits)?;
            write_output(writer, b"\n")
        }
        DataRenderFormat::Table => render_table_to(writer, envelope, limits),
    }
}

/// Render a live stream at the explicitly chosen output boundary. Plain and
/// JSON forms write each row as it is pulled. Tables need their complete column
/// set before printing a header, so table rendering intentionally materializes
/// the bounded stream.
fn render_stream_to(
    mut stream: DataStream,
    format: DataRenderFormat,
    cancelled: &AtomicBool,
    writer: &mut impl Write,
) -> Result<(), ShellError> {
    check_cancelled(cancelled)?;
    match format {
        DataRenderFormat::Table => {
            let limits = stream.limits();
            let rows = stream.collect(cancelled)?;
            check_cancelled(cancelled)?;
            render_table_rows_to(writer, &rows, limits, Some(cancelled))
        }
        DataRenderFormat::Plain => {
            let limits = stream.limits();
            while let Some(value) = stream.next(cancelled)? {
                write_plain_data_value(writer, &value, limits)?;
                write_output(writer, b"\n")?;
            }
            Ok(())
        }
        DataRenderFormat::Json => {
            write_output(writer, b"{\n  \"kind\": \"stream\",\n  \"items\": [")?;
            let mut first = true;
            while let Some(value) = stream.next(cancelled)? {
                if first {
                    write_output(writer, b"\n")?;
                    first = false;
                } else {
                    write_output(writer, b",\n")?;
                }
                write_output(writer, b"    ")?;
                write_typed_json_value(writer, &value)?;
            }
            if !first {
                write_output(writer, b"\n")?;
            }
            write_output(writer, b"  ]\n}\n")
        }
    }
}

fn write_output(writer: &mut impl Write, bytes: &[u8]) -> Result<(), ShellError> {
    writer.write_all(bytes).map_err(|error| {
        ShellError::new(ErrorCode::Io, "cannot write data output")
            .with_context(error.to_string())
            .with_help(
                "Check that the output destination is writable or consume the complete output",
            )
    })
}

fn write_json(
    writer: &mut impl Write,
    value: &impl Serialize,
    message: &str,
) -> Result<(), ShellError> {
    serde_json::to_writer(writer, value).map_err(|error| json_render_error(error, message))
}

fn json_render_error(error: serde_json::Error, message: &str) -> ShellError {
    let code = if error.is_io() {
        ErrorCode::Io
    } else {
        ErrorCode::Data
    };
    ShellError::new(code, message)
        .with_context(error.to_string())
        .with_help("Use `--format plain` or check that the output destination is writable")
}

enum EnvelopeJsonFrame<'a> {
    Visit(&'a DataEnvelope),
    Stream {
        items: &'a [DataValue],
        index: usize,
    },
    Bytes(&'static [u8]),
    ResultTail(Option<&'a ShellError>),
    TaskTail(Option<&'a ShellError>),
}

fn write_typed_json_envelope(
    writer: &mut impl Write,
    envelope: &DataEnvelope,
) -> Result<(), ShellError> {
    let mut stack = vec![EnvelopeJsonFrame::Visit(envelope)];
    while let Some(frame) = stack.pop() {
        match frame {
            EnvelopeJsonFrame::Visit(envelope) => match envelope {
                DataEnvelope::Value { value } => {
                    write_output(writer, b"{\"kind\": \"value\", \"value\": ")?;
                    write_typed_json_value(writer, value)?;
                    write_output(writer, b"}")?;
                }
                DataEnvelope::Stream { items } => {
                    write_output(writer, b"{\"kind\": \"stream\", \"items\": [")?;
                    stack.push(EnvelopeJsonFrame::Stream { items, index: 0 });
                }
                DataEnvelope::Option { value } => {
                    write_output(writer, b"{\"kind\": \"option\", \"value\": ")?;
                    if let Some(value) = value {
                        stack.push(EnvelopeJsonFrame::Bytes(b"}"));
                        stack.push(EnvelopeJsonFrame::Visit(value));
                    } else {
                        write_output(writer, b"null}")?;
                    }
                }
                DataEnvelope::Result {
                    state,
                    value,
                    error,
                } => {
                    write_output(writer, b"{\"kind\": \"result\", \"state\": ")?;
                    write_json(writer, state, "cannot serialize data result state")?;
                    write_output(writer, b", \"value\": ")?;
                    if let Some(value) = value {
                        stack.push(EnvelopeJsonFrame::ResultTail(error.as_ref()));
                        stack.push(EnvelopeJsonFrame::Visit(value));
                    } else {
                        write_output(writer, b"null")?;
                        write_envelope_error_tail(writer, error.as_ref())?;
                    }
                }
                DataEnvelope::Task {
                    state,
                    value,
                    error,
                } => {
                    write_output(writer, b"{\"kind\": \"task\", \"state\": ")?;
                    write_json(writer, state, "cannot serialize data task state")?;
                    write_output(writer, b", \"value\": ")?;
                    if let Some(value) = value {
                        stack.push(EnvelopeJsonFrame::TaskTail(error.as_ref()));
                        stack.push(EnvelopeJsonFrame::Visit(value));
                    } else {
                        write_output(writer, b"null")?;
                        write_envelope_error_tail(writer, error.as_ref())?;
                    }
                }
            },
            EnvelopeJsonFrame::Stream { items, index } => {
                let Some(value) = items.get(index) else {
                    write_output(writer, b"]}")?;
                    continue;
                };
                if index > 0 {
                    write_output(writer, b", ")?;
                }
                stack.push(EnvelopeJsonFrame::Stream {
                    items,
                    index: index.saturating_add(1),
                });
                write_typed_json_value(writer, value)?;
            }
            EnvelopeJsonFrame::Bytes(bytes) => write_output(writer, bytes)?,
            EnvelopeJsonFrame::ResultTail(error) | EnvelopeJsonFrame::TaskTail(error) => {
                write_envelope_error_tail(writer, error)?
            }
        }
    }
    Ok(())
}

fn write_envelope_error_tail(
    writer: &mut impl Write,
    error: Option<&ShellError>,
) -> Result<(), ShellError> {
    write_output(writer, b", \"error\": ")?;
    if let Some(error) = error {
        serde_json::to_writer_pretty(&mut *writer, error)
            .map_err(|error| json_render_error(error, "cannot serialize data envelope error"))?;
    } else {
        write_output(writer, b"null")?;
    }
    write_output(writer, b"}")
}

enum TypedJsonFrame<'a> {
    Value(&'a DataValue),
    List {
        values: &'a [DataValue],
        index: usize,
    },
    Record {
        entries: indexmap::map::Iter<'a, String, DataValue>,
        first: bool,
    },
}

fn write_typed_json_value(writer: &mut impl Write, value: &DataValue) -> Result<(), ShellError> {
    let mut stack = vec![TypedJsonFrame::Value(value)];
    while let Some(frame) = stack.pop() {
        match frame {
            TypedJsonFrame::Value(value) => match value {
                DataValue::Nothing => write_output(writer, b"{\"type\": \"nothing\"}")?,
                DataValue::Bool(value) => {
                    write_typed_scalar(writer, "bool", value, "cannot serialize Boolean")?
                }
                DataValue::Int(value) => {
                    write_typed_scalar(writer, "int", value, "cannot serialize integer")?
                }
                DataValue::UInt(value) => {
                    write_typed_scalar(writer, "u_int", value, "cannot serialize integer")?
                }
                DataValue::Decimal(value) => {
                    write_typed_scalar(writer, "decimal", value, "cannot serialize decimal")?
                }
                DataValue::String(value) => {
                    write_typed_scalar(writer, "string", value, "cannot serialize string")?
                }
                DataValue::Path(value) => {
                    write_typed_scalar(writer, "path", value, "cannot serialize path")?
                }
                DataValue::DateTime(value) => {
                    write_typed_scalar(writer, "date_time", value, "cannot serialize date-time")?
                }
                DataValue::Pattern(value) => {
                    write_typed_scalar(writer, "pattern", value, "cannot serialize pattern")?
                }
                DataValue::Duration { nanoseconds } => {
                    write_output(
                        writer,
                        b"{\"type\": \"duration\", \"value\": {\"nanoseconds\": ",
                    )?;
                    write_json(writer, nanoseconds, "cannot serialize duration")?;
                    write_output(writer, b"}}")?;
                }
                DataValue::Size { bytes } => {
                    write_output(writer, b"{\"type\": \"size\", \"value\": {\"bytes\": ")?;
                    write_json(writer, bytes, "cannot serialize size")?;
                    write_output(writer, b"}}")?;
                }
                DataValue::List(values) => {
                    write_output(writer, b"{\"type\": \"list\", \"value\": [")?;
                    stack.push(TypedJsonFrame::List { values, index: 0 });
                }
                DataValue::Record(values) => {
                    write_output(writer, b"{\"type\": \"record\", \"value\": {")?;
                    stack.push(TypedJsonFrame::Record {
                        entries: values.iter(),
                        first: true,
                    });
                }
            },
            TypedJsonFrame::List { values, index } => {
                let Some(value) = values.get(index) else {
                    write_output(writer, b"]}")?;
                    continue;
                };
                if index > 0 {
                    write_output(writer, b", ")?;
                }
                stack.push(TypedJsonFrame::List {
                    values,
                    index: index.saturating_add(1),
                });
                stack.push(TypedJsonFrame::Value(value));
            }
            TypedJsonFrame::Record { mut entries, first } => {
                let Some((key, value)) = entries.next() else {
                    write_output(writer, b"}}")?;
                    continue;
                };
                if !first {
                    write_output(writer, b", ")?;
                }
                write_json_string(writer, key)?;
                write_output(writer, b": ")?;
                stack.push(TypedJsonFrame::Record {
                    entries,
                    first: false,
                });
                stack.push(TypedJsonFrame::Value(value));
            }
        }
    }
    Ok(())
}

fn write_typed_scalar(
    writer: &mut impl Write,
    kind: &str,
    value: &impl Serialize,
    message: &str,
) -> Result<(), ShellError> {
    write_output(writer, b"{\"type\": ")?;
    write_json_string(writer, kind)?;
    write_output(writer, b", \"value\": ")?;
    write_json(writer, value, message)?;
    write_output(writer, b"}")
}

fn write_terminal_safe(writer: &mut impl Write, value: &str) -> Result<(), ShellError> {
    let mut safe_start = 0_usize;
    for (index, character) in value.char_indices() {
        let escaped = (character.is_control() && !matches!(character, '\n' | '\t'))
            || character == '\u{009b}';
        if !escaped {
            continue;
        }
        write_output(
            writer,
            value.as_bytes().get(safe_start..index).unwrap_or_default(),
        )?;
        for escaped_character in character.escape_default() {
            let mut encoded = [0_u8; 4];
            write_output(
                writer,
                escaped_character.encode_utf8(&mut encoded).as_bytes(),
            )?;
        }
        safe_start = index.saturating_add(character.len_utf8());
    }
    write_output(
        writer,
        value.as_bytes().get(safe_start..).unwrap_or_default(),
    )
}

fn write_plain_data_value(
    writer: &mut impl Write,
    value: &DataValue,
    limits: DataLimits,
) -> Result<(), ShellError> {
    validate_data_value(value, limits)?;
    match value {
        DataValue::String(value)
        | DataValue::Path(value)
        | DataValue::DateTime(value)
        | DataValue::Pattern(value) => write_terminal_safe(writer, value),
        DataValue::Nothing => write_output(writer, b"null"),
        DataValue::Bool(value) => write_output(writer, value.to_string().as_bytes()),
        DataValue::Int(value) => write_output(writer, value.to_string().as_bytes()),
        DataValue::UInt(value) => write_output(writer, value.to_string().as_bytes()),
        DataValue::Decimal(value) => write_output(writer, value.as_bytes()),
        DataValue::Duration { nanoseconds } => {
            write_output(writer, format!("{nanoseconds}ns").as_bytes())
        }
        DataValue::Size { bytes } => write_output(writer, format!("{bytes}B").as_bytes()),
        DataValue::List(_) | DataValue::Record(_) => write_json_compatible_value(writer, value),
    }
}

enum JsonCompatibleFrame<'a> {
    Value(&'a DataValue),
    List {
        values: &'a [DataValue],
        index: usize,
    },
    Record {
        entries: indexmap::map::Iter<'a, String, DataValue>,
        first: bool,
    },
}

fn write_json_compatible_value(
    writer: &mut impl Write,
    value: &DataValue,
) -> Result<(), ShellError> {
    let mut stack = vec![JsonCompatibleFrame::Value(value)];
    while let Some(frame) = stack.pop() {
        match frame {
            JsonCompatibleFrame::Value(value) => match value {
                DataValue::Nothing => write_output(writer, b"null")?,
                DataValue::Bool(value) => write_json(writer, value, "cannot render Boolean")?,
                DataValue::Int(value) => write_json(writer, value, "cannot render integer")?,
                DataValue::UInt(value) => write_json(writer, value, "cannot render integer")?,
                DataValue::Decimal(value) => {
                    let number = value.parse::<serde_json::Number>().map_err(|_| {
                        ShellError::new(
                            ErrorCode::Data,
                            "decimal cannot be represented as a JSON number",
                        )
                        .with_context(format!("decimal source: {value}"))
                        .with_help("Select or convert non-finite decimals before rendering")
                    })?;
                    write_json(writer, &number, "cannot render decimal")?;
                }
                DataValue::String(value)
                | DataValue::Path(value)
                | DataValue::DateTime(value)
                | DataValue::Pattern(value) => write_json_string(writer, value)?,
                DataValue::Duration { nanoseconds } => {
                    write_json_string(writer, &format!("{nanoseconds}ns"))?
                }
                DataValue::Size { bytes } => write_json_string(writer, &format!("{bytes}B"))?,
                DataValue::List(values) => {
                    write_output(writer, b"[")?;
                    stack.push(JsonCompatibleFrame::List { values, index: 0 });
                }
                DataValue::Record(values) => {
                    write_output(writer, b"{")?;
                    stack.push(JsonCompatibleFrame::Record {
                        entries: values.iter(),
                        first: true,
                    });
                }
            },
            JsonCompatibleFrame::List { values, index } => {
                let Some(value) = values.get(index) else {
                    write_output(writer, b"]")?;
                    continue;
                };
                if index > 0 {
                    write_output(writer, b",")?;
                }
                stack.push(JsonCompatibleFrame::List {
                    values,
                    index: index.saturating_add(1),
                });
                stack.push(JsonCompatibleFrame::Value(value));
            }
            JsonCompatibleFrame::Record { mut entries, first } => {
                let Some((key, value)) = entries.next() else {
                    write_output(writer, b"}")?;
                    continue;
                };
                if !first {
                    write_output(writer, b",")?;
                }
                write_json_string(writer, key)?;
                write_output(writer, b":")?;
                stack.push(JsonCompatibleFrame::Record {
                    entries,
                    first: false,
                });
                stack.push(JsonCompatibleFrame::Value(value));
            }
        }
    }
    Ok(())
}

fn write_json_string(writer: &mut impl Write, value: &str) -> Result<(), ShellError> {
    write_output(writer, b"\"")?;
    let mut safe_start = 0_usize;
    for (index, character) in value.char_indices() {
        let escape = match character {
            '\"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\u{0008}' => Some("\\b"),
            '\u{000c}' => Some("\\f"),
            '\n' => Some("\\n"),
            '\r' => Some("\\r"),
            '\t' => Some("\\t"),
            character if character.is_control() => None,
            _ => continue,
        };
        write_output(
            writer,
            value.as_bytes().get(safe_start..index).unwrap_or_default(),
        )?;
        if let Some(escape) = escape {
            write_output(writer, escape.as_bytes())?;
        } else {
            let code = u32::from(character);
            write_output(writer, format!("\\u{code:04x}").as_bytes())?;
        }
        safe_start = index.saturating_add(character.len_utf8());
    }
    write_output(
        writer,
        value.as_bytes().get(safe_start..).unwrap_or_default(),
    )?;
    write_output(writer, b"\"")
}

fn write_plain_envelope(
    writer: &mut impl Write,
    envelope: &DataEnvelope,
    limits: DataLimits,
) -> Result<(), ShellError> {
    let mut current = envelope;
    loop {
        match current {
            DataEnvelope::Value { value } => return write_plain_data_value(writer, value, limits),
            DataEnvelope::Stream { items } => {
                for (index, value) in items.iter().enumerate() {
                    if index > 0 {
                        write_output(writer, b"\n")?;
                    }
                    write_plain_data_value(writer, value, limits)?;
                }
                return Ok(());
            }
            DataEnvelope::Option { value: Some(value) }
            | DataEnvelope::Result {
                value: Some(value), ..
            } => current = value,
            DataEnvelope::Option { value: None } => return write_output(writer, b"none"),
            DataEnvelope::Result {
                state,
                value: None,
                error,
            } => {
                if let Some(error) = error {
                    write_output(writer, b"error: ")?;
                    return write_terminal_safe(writer, &error.message);
                }
                return write_output(writer, format!("result {state:?}").as_bytes());
            }
            DataEnvelope::Task {
                state,
                value: Some(value),
                ..
            } => {
                write_output(writer, format!("task {state:?}: ").as_bytes())?;
                current = value;
            }
            DataEnvelope::Task {
                state,
                value: None,
                error,
            } => {
                write_output(writer, format!("task {state:?}").as_bytes())?;
                if let Some(error) = error {
                    write_output(writer, b": ")?;
                    write_terminal_safe(writer, &error.message)?;
                }
                return Ok(());
            }
        }
    }
}

fn render_table_to(
    writer: &mut impl Write,
    envelope: &DataEnvelope,
    limits: DataLimits,
) -> Result<(), ShellError> {
    let original = envelope;
    let mut current = envelope;
    let value = loop {
        match current {
            DataEnvelope::Value { value } => break value,
            DataEnvelope::Stream { items } => {
                return render_table_rows_to(writer, items, limits, None);
            }
            DataEnvelope::Option { value }
            | DataEnvelope::Result { value, .. }
            | DataEnvelope::Task { value, .. } => match value {
                Some(value) => current = value,
                None => {
                    write_plain_envelope(writer, original, limits)?;
                    return write_output(writer, b"\n");
                }
            },
        }
    };
    match value {
        DataValue::List(rows) => render_table_rows_to(writer, rows, limits, None),
        DataValue::Record(_) => {
            render_table_rows_to(writer, std::slice::from_ref(value), limits, None)
        }
        _ => {
            write_plain_data_value(writer, value, limits)?;
            write_output(writer, b"\n")
        }
    }
}

fn render_table_rows_to(
    writer: &mut impl Write,
    rows: &[DataValue],
    limits: DataLimits,
    cancelled: Option<&AtomicBool>,
) -> Result<(), ShellError> {
    let mut columns = IndexSet::new();
    for row in rows {
        if let DataValue::Record(row) = row {
            columns.extend(row.keys().map(String::as_str));
        }
    }
    if columns.is_empty() {
        for value in rows {
            if let Some(cancelled) = cancelled {
                check_cancelled(cancelled)?;
            }
            write_plain_data_value(writer, value, limits)?;
            write_output(writer, b"\n")?;
        }
        return Ok(());
    }
    let layout = table_layout(columns, rows, limits)?;
    write_table_rule(writer, &layout.widths, TableRule::Top)?;
    write_table_header(writer, &layout)?;
    write_table_rule(writer, &layout.widths, TableRule::Header)?;
    for (row_index, row) in rows.iter().enumerate() {
        if let Some(cancelled) = cancelled {
            check_cancelled(cancelled)?;
        }
        write_table_value_row(writer, row_index, row, &layout, limits)?;
    }
    if rows.len() >= TABLE_REPEAT_HEADER_ROW_MIN {
        write_table_rule(writer, &layout.widths, TableRule::Header)?;
        write_table_header(writer, &layout)?;
    }
    write_table_rule(writer, &layout.widths, TableRule::Bottom)
}

const TABLE_PADDING_CHUNK_BYTES: usize = 64;
const TABLE_REPEAT_HEADER_ROW_MIN: usize = 16;
const TABLE_HORIZONTAL_RULE_CHUNK: &str = "────────────────";
const TABLE_HORIZONTAL_RULE_CHUNK_CELLS: usize = 16;
const _: () =
    assert!(TABLE_HORIZONTAL_RULE_CHUNK.len() == TABLE_HORIZONTAL_RULE_CHUNK_CELLS * "─".len());

#[derive(Clone, Copy)]
enum TableAlignment {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy)]
enum TableRule {
    Top,
    Header,
    Bottom,
}

struct TableLayout<'a> {
    columns: Vec<TableColumn<'a>>,
    rendered_columns: Vec<String>,
    widths: Vec<usize>,
    now_unix_seconds: Option<u64>,
}

#[derive(Clone, Copy)]
struct TableColumn<'a> {
    key: &'a str,
    heading: &'a str,
}

fn table_layout<'a>(
    columns: IndexSet<&'a str>,
    rows: &[DataValue],
    limits: DataLimits,
) -> Result<TableLayout<'a>, ShellError> {
    let now_unix_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs());
    let columns = table_columns(columns, rows);
    let rendered_columns = columns
        .iter()
        .map(|column| render_table_text(column.heading, limits))
        .collect::<Result<Vec<_>, _>>()?;
    let mut widths = rendered_columns
        .iter()
        .map(|column| UnicodeWidthStr::width(column.as_str()))
        .collect::<Vec<_>>();
    for row in rows {
        match row {
            DataValue::Record(row) => {
                for (index, column) in columns.iter().enumerate() {
                    let Some(value) = row.get(column.key) else {
                        continue;
                    };
                    let width = UnicodeWidthStr::width(
                        render_table_value(Some(column.key), value, limits, now_unix_seconds)?
                            .as_str(),
                    );
                    let Some(column_width) = widths.get_mut(index) else {
                        return Err(table_shape_error());
                    };
                    *column_width = (*column_width).max(width);
                }
            }
            value => {
                let rendered = render_table_value(None, value, limits, now_unix_seconds)?;
                let Some(first_width) = widths.first_mut() else {
                    return Err(table_shape_error());
                };
                *first_width = (*first_width).max(UnicodeWidthStr::width(rendered.as_str()));
            }
        }
    }
    let last_row_index = rows.len().saturating_sub(1);
    let row_index_width = last_row_index.to_string().len().max(1);
    widths.insert(0, row_index_width);
    Ok(TableLayout {
        columns,
        rendered_columns,
        widths,
        now_unix_seconds,
    })
}

fn table_columns<'a>(columns: IndexSet<&'a str>, rows: &[DataValue]) -> Vec<TableColumn<'a>> {
    const DIRECTORY_FIELDS: [&str; 8] = [
        "hidden", "kind", "modified", "name", "path", "readonly", "size", "target",
    ];
    let is_directory_table = columns.len() == DIRECTORY_FIELDS.len()
        && DIRECTORY_FIELDS.iter().all(|field| columns.contains(field));
    if !is_directory_table {
        return columns
            .into_iter()
            .map(|column| TableColumn {
                key: column,
                heading: column,
            })
            .collect();
    }

    let mut displayed = vec![
        TableColumn {
            key: "name",
            heading: "name",
        },
        TableColumn {
            key: "kind",
            heading: "type",
        },
        TableColumn {
            key: "size",
            heading: "size",
        },
        TableColumn {
            key: "modified",
            heading: "modified",
        },
    ];
    for field in ["target", "readonly", "hidden"] {
        if rows
            .iter()
            .any(|row| directory_field_is_not_default(row, field))
        {
            displayed.push(TableColumn {
                key: field,
                heading: field,
            });
        }
    }
    displayed
}

fn directory_field_is_not_default(row: &DataValue, field: &str) -> bool {
    let DataValue::Record(row) = row else {
        return false;
    };
    match row.get(field) {
        Some(DataValue::Nothing | DataValue::Bool(false)) | None => false,
        Some(_) => true,
    }
}

fn write_table_header(writer: &mut impl Write, layout: &TableLayout<'_>) -> Result<(), ShellError> {
    write_table_row(
        writer,
        std::iter::once(("#", TableAlignment::Center)).chain(
            layout
                .rendered_columns
                .iter()
                .map(|column| (column.as_str(), TableAlignment::Center)),
        ),
        &layout.widths,
    )
}

fn write_table_value_row(
    writer: &mut impl Write,
    row_index: usize,
    value: &DataValue,
    layout: &TableLayout<'_>,
    limits: DataLimits,
) -> Result<(), ShellError> {
    let rendered_row_index = row_index.to_string();
    let DataValue::Record(row) = value else {
        let rendered = render_table_value(None, value, limits, layout.now_unix_seconds)?;
        let trailing_cells = std::iter::repeat_n(
            ("", TableAlignment::Left),
            layout.widths.len().saturating_sub(2),
        );
        return write_table_row(
            writer,
            std::iter::once((rendered_row_index.as_str(), TableAlignment::Right))
                .chain(std::iter::once((rendered.as_str(), table_alignment(value))))
                .chain(trailing_cells),
            &layout.widths,
        );
    };
    write_table_record(writer, &rendered_row_index, row, layout, limits)
}

fn write_table_record(
    writer: &mut impl Write,
    rendered_row_index: &str,
    row: &IndexMap<String, DataValue>,
    layout: &TableLayout<'_>,
    limits: DataLimits,
) -> Result<(), ShellError> {
    let rendered = layout
        .columns
        .iter()
        .map(|column| {
            row.get(column.key)
                .map(|value| {
                    render_table_value(Some(column.key), value, limits, layout.now_unix_seconds)
                        .map(|rendered| (rendered, table_alignment(value)))
                })
                .transpose()
                .map(|value| value.unwrap_or_else(|| (String::new(), TableAlignment::Left)))
        })
        .collect::<Result<Vec<_>, ShellError>>()?;
    write_table_row(
        writer,
        std::iter::once((rendered_row_index, TableAlignment::Right)).chain(
            rendered
                .iter()
                .map(|(value, alignment)| (value.as_str(), *alignment)),
        ),
        &layout.widths,
    )
}

fn table_alignment(value: &DataValue) -> TableAlignment {
    match value {
        DataValue::Int(_)
        | DataValue::UInt(_)
        | DataValue::Decimal(_)
        | DataValue::Duration { .. }
        | DataValue::Size { .. } => TableAlignment::Right,
        _ => TableAlignment::Left,
    }
}

fn render_table_text(value: &str, limits: DataLimits) -> Result<String, ShellError> {
    collect_rendered(limits.max_materialized_bytes, |writer| {
        write_terminal_safe(writer, value)
    })
}

fn render_table_value(
    column: Option<&str>,
    value: &DataValue,
    limits: DataLimits,
    now_unix_seconds: Option<u64>,
) -> Result<String, ShellError> {
    if let (Some("kind"), DataValue::String(kind)) = (column, value) {
        return Ok(match kind.as_str() {
            "directory" => "dir".to_owned(),
            "symlink" => "link".to_owned(),
            _ => kind.clone(),
        });
    }
    match value {
        DataValue::Nothing => return Ok("—".to_owned()),
        DataValue::Bool(true) => return Ok("✓".to_owned()),
        DataValue::Bool(false) => return Ok("·".to_owned()),
        _ => {}
    }
    if let DataValue::Size { bytes } = value {
        return Ok(format_table_size(*bytes));
    }
    if let (DataValue::DateTime(value), Some(now_unix_seconds)) = (value, now_unix_seconds)
        && let Some(modified_unix_seconds) = value
            .strip_prefix("unix:")
            .and_then(|seconds| seconds.parse::<u64>().ok())
    {
        return Ok(format_relative_time(
            modified_unix_seconds,
            now_unix_seconds,
        ));
    }
    collect_rendered(limits.max_materialized_bytes, |writer| {
        write_plain_data_value(writer, value, limits)
    })
}

fn format_table_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1_000 {
        return format!("{bytes} B");
    }
    let bytes = u128::from(bytes);
    let mut unit_index = 0_usize;
    let mut divisor = 1_u128;
    while unit_index.saturating_add(1) < UNITS.len() && bytes >= divisor.saturating_mul(1_000) {
        divisor = divisor.saturating_mul(1_000);
        unit_index = unit_index.saturating_add(1);
    }
    let tenths = bytes
        .saturating_mul(10)
        .checked_div(divisor)
        .unwrap_or_default();
    format!(
        "{}.{} {}",
        tenths / 10,
        tenths % 10,
        UNITS.get(unit_index).copied().unwrap_or("B")
    )
}

fn format_relative_time(timestamp: u64, now: u64) -> String {
    let future = timestamp > now;
    let elapsed = timestamp.abs_diff(now);
    if elapsed == 0 {
        return "now".to_owned();
    }
    let (quantity, unit) = if elapsed < 60 {
        (elapsed, "second")
    } else if elapsed < 60 * 60 {
        (elapsed / 60, "minute")
    } else if elapsed < 24 * 60 * 60 {
        (elapsed / (60 * 60), "hour")
    } else if elapsed < 7 * 24 * 60 * 60 {
        (elapsed / (24 * 60 * 60), "day")
    } else if elapsed < 30 * 24 * 60 * 60 {
        (elapsed / (7 * 24 * 60 * 60), "week")
    } else if elapsed < 365 * 24 * 60 * 60 {
        (elapsed / (30 * 24 * 60 * 60), "month")
    } else {
        (elapsed / (365 * 24 * 60 * 60), "year")
    };
    let duration = relative_duration(quantity, unit);
    if future {
        format!("in {duration}")
    } else {
        format!("{duration} ago")
    }
}

fn relative_duration(quantity: u64, unit: &str) -> String {
    if quantity == 1 {
        let article = if unit == "hour" { "an" } else { "a" };
        format!("{article} {unit}")
    } else {
        format!("{quantity} {unit}s")
    }
}

fn write_table_row<'a>(
    writer: &mut impl Write,
    cells: impl Iterator<Item = (&'a str, TableAlignment)>,
    widths: &[usize],
) -> Result<(), ShellError> {
    write_output(writer, "│".as_bytes())?;
    for (index, (cell, alignment)) in cells.enumerate() {
        let width = widths.get(index).copied().ok_or_else(table_shape_error)?;
        let cell_width = UnicodeWidthStr::width(cell);
        let padding = width.saturating_sub(cell_width);
        let (left_padding, right_padding) = match alignment {
            TableAlignment::Left => (0, padding),
            TableAlignment::Center => (padding / 2, padding.saturating_sub(padding / 2)),
            TableAlignment::Right => (padding, 0),
        };
        write_output(writer, b" ")?;
        write_table_padding(writer, left_padding)?;
        write_output(writer, cell.as_bytes())?;
        write_table_padding(writer, right_padding)?;
        write_output(writer, b" ")?;
        write_output(writer, "│".as_bytes())?;
    }
    write_output(writer, b"\n")
}

fn write_table_rule(
    writer: &mut impl Write,
    widths: &[usize],
    rule: TableRule,
) -> Result<(), ShellError> {
    let (left, junction, right) = match rule {
        TableRule::Top => ("╭", "┬", "╮"),
        TableRule::Header => ("├", "┼", "┤"),
        TableRule::Bottom => ("╰", "┴", "╯"),
    };
    write_output(writer, left.as_bytes())?;
    for (index, width) in widths.iter().copied().enumerate() {
        write_table_horizontal_rule(writer, width.saturating_add(2))?;
        let boundary = if index.saturating_add(1) < widths.len() {
            junction
        } else {
            right
        };
        write_output(writer, boundary.as_bytes())?;
    }
    write_output(writer, b"\n")
}

fn write_table_horizontal_rule(
    writer: &mut impl Write,
    mut count: usize,
) -> Result<(), ShellError> {
    while count > 0 {
        let cells = count.min(TABLE_HORIZONTAL_RULE_CHUNK_CELLS);
        let bytes = cells.saturating_mul("─".len());
        let chunk = TABLE_HORIZONTAL_RULE_CHUNK
            .as_bytes()
            .get(..bytes)
            .ok_or_else(table_shape_error)?;
        write_output(writer, chunk)?;
        count = count.saturating_sub(cells);
    }
    Ok(())
}

fn write_table_padding(writer: &mut impl Write, mut count: usize) -> Result<(), ShellError> {
    const PADDING: [u8; TABLE_PADDING_CHUNK_BYTES] = [b' '; TABLE_PADDING_CHUNK_BYTES];
    while count > 0 {
        let chunk = count.min(PADDING.len());
        write_output(writer, PADDING.get(..chunk).unwrap_or_default())?;
        count = count.saturating_sub(chunk);
    }
    Ok(())
}

fn table_shape_error() -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "table row and column widths have inconsistent shapes",
    )
    .with_help("Report this bounded table-renderer invariant failure")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollectionFailure {
    Limit { observed: usize },
    Allocation { observed: usize },
}

/// The collected-render failure model is deliberately localized here:
/// many individually valid rows, one oversized rendered row, escaping
/// expansion, headers, separators, and JSON syntax all pass through `write`.
/// Checked addition maps an exact-size overflow to a limit failure; a limit or
/// allocation failure retains none of the rejected chunk. Renderer, reader,
/// and cancellation errors after partial output remain the originating
/// `ShellError`, and dropping this collector releases every partial byte.
struct BoundedRenderCollector {
    bytes: Vec<u8>,
    bytes_max: usize,
    failure: Option<CollectionFailure>,
}

impl BoundedRenderCollector {
    const fn new(bytes_max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            bytes_max,
            failure: None,
        }
    }

    fn reserve_for(&mut self, attempted: usize) -> Result<(), std::io::Error> {
        if attempted <= self.bytes.capacity() {
            return Ok(());
        }
        let doubled = self
            .bytes
            .capacity()
            .checked_mul(2)
            .unwrap_or(self.bytes_max);
        let capacity = attempted.max(doubled).min(self.bytes_max);
        let additional = capacity.saturating_sub(self.bytes.len());
        self.bytes.try_reserve_exact(additional).map_err(|_| {
            self.failure = Some(CollectionFailure::Allocation {
                observed: attempted,
            });
            std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                "cannot allocate bounded data render output",
            )
        })
    }
}

impl Write for BoundedRenderCollector {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let attempted =
            checked_rendered_usage(self.bytes.len(), buffer.len()).map_err(|observed| {
                self.failure = Some(CollectionFailure::Limit { observed });
                std::io::Error::new(
                    std::io::ErrorKind::FileTooLarge,
                    "collected data render size overflow",
                )
            })?;
        if attempted > self.bytes_max {
            self.failure = Some(CollectionFailure::Limit {
                observed: attempted,
            });
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "collected data render limit exceeded",
            ));
        }
        self.reserve_for(attempted)?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn checked_rendered_usage(retained: usize, incoming: usize) -> Result<usize, usize> {
    retained.checked_add(incoming).ok_or(usize::MAX)
}

fn collect_rendered(
    bytes_max: usize,
    render: impl FnOnce(&mut BoundedRenderCollector) -> Result<(), ShellError>,
) -> Result<String, ShellError> {
    let mut collector = BoundedRenderCollector::new(bytes_max);
    let result = render(&mut collector);
    if let Some(failure) = collector.failure {
        return Err(collected_render_error(bytes_max, failure));
    }
    result?;
    String::from_utf8(collector.bytes).map_err(|error| {
        ShellError::new(ErrorCode::Data, "data renderer produced invalid UTF-8")
            .with_context(error.to_string())
            .with_help("Report this internal data renderer invariant failure")
    })
}

fn collected_render_error(bytes_max: usize, failure: CollectionFailure) -> ShellError {
    let (message, observed) = match failure {
        CollectionFailure::Limit { observed } => (
            "collected data render bytes exceed the configured materialization limit",
            observed,
        ),
        CollectionFailure::Allocation { observed } => {
            ("cannot allocate the collected data render buffer", observed)
        }
    };
    ShellError::new(ErrorCode::ResourceLimit, message)
        .with_context(format!("limit: {bytes_max}; observed: {observed}"))
        .with_help(
            "Use `render_to` for streaming output, reduce the rendered data, or raise `max_materialized_bytes`",
        )
}

fn apply_transform(
    value: DataValue,
    transform: &DataTransform,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    match transform {
        DataTransform::Length => match value {
            DataValue::List(values) => usize_value(values.len()),
            DataValue::Record(values) => usize_value(values.len()),
            DataValue::String(value) => usize_value(value.chars().count()),
            _ => Err(data_error(
                "length",
                "length expects a list, record, or string",
            )),
        },
        DataTransform::First => Err(ShellError::new(
            ErrorCode::Data,
            "first escaped its explicit Option output boundary",
        )
        .with_help("Report this internal focused-evaluator invariant failure")),
        DataTransform::Get { path } => get_field(value, &path.value, "get"),
        DataTransform::Where(predicate) => filter_where(value, predicate, limits, "where"),
        DataTransform::Select { fields } => {
            let fields = fields
                .iter()
                .map(|field| field.value.clone())
                .collect::<Vec<_>>();
            select_fields(value, &fields, "select")
        }
        DataTransform::Sort { field, direction } => {
            sort_rows(value, &field.value, *direction, "sort")
        }
        DataTransform::Take { count } => take_values(value, count.value, "take"),
        DataTransform::Lines | DataTransform::FromJson | DataTransform::ToJson => {
            Err(ShellError::new(
                ErrorCode::Data,
                "byte/value bridge escaped its explicit output boundary",
            )
            .with_help("Report this internal data runtime invariant failure"))
        }
    }
}

/// Split a trailing `?` optional-access marker off a dotted/indexed cell path.
///
/// A path of just `?` is left untouched, since it is not a meaningful field
/// or index name to strip down to an empty path.
fn split_optional_path(path: &str) -> (&str, bool) {
    match path.strip_suffix('?') {
        Some(stripped) if !stripped.is_empty() => (stripped, true),
        _ => (path, false),
    }
}

fn get_field(value: DataValue, field: &str, stage: &str) -> Result<DataValue, ShellError> {
    let (field, optional) = split_optional_path(field);
    match value {
        DataValue::Record(object) => match get_path(&DataValue::Record(object), field) {
            Some(found) => Ok(found.clone()),
            None if optional => Ok(DataValue::Nothing),
            None => Err(data_error(stage, format!("record has no field `{field}`"))),
        },
        DataValue::List(values) => values
            .into_iter()
            .map(|value| {
                if !matches!(value, DataValue::Record(_)) {
                    return Err(data_error(stage, "get over a list expects record rows"));
                }
                match get_path(&value, field) {
                    Some(found) => Ok(found.clone()),
                    None if optional => Ok(DataValue::Nothing),
                    None => Err(data_error(stage, format!("row has no field `{field}`"))),
                }
            })
            .collect::<Result<Vec<_>, _>>()
            .map(DataValue::List),
        _ => Err(data_error(stage, "get expects a record or list of records")),
    }
}

fn filter_where(
    value: DataValue,
    predicate: &SyntaxPredicate,
    limits: DataLimits,
    stage: &str,
) -> Result<DataValue, ShellError> {
    let DataValue::List(values) = value else {
        return Err(data_error(stage, "where expects a list of records"));
    };

    let predicate = RuntimePredicate::from_syntax(predicate, limits)?;
    let mut filtered = Vec::new();
    for value in values {
        if !matches!(value, DataValue::Record(_)) {
            return Err(data_error(stage, "where expects record rows"));
        }
        if predicate_matches(&predicate, &value, stage)? {
            filtered.push(value);
        }
    }
    Ok(DataValue::List(filtered))
}

struct RuntimeCondition {
    field: String,
    comparison: SyntaxComparisonOperator,
    expected: DataValue,
}

struct RuntimePredicate {
    conditions: Vec<RuntimeCondition>,
    operators: Vec<SyntaxBooleanOperator>,
}

impl RuntimePredicate {
    fn from_syntax(predicate: &SyntaxPredicate, limits: DataLimits) -> Result<Self, ShellError> {
        let conditions = predicate
            .conditions
            .iter()
            .map(|condition| {
                Ok(RuntimeCondition {
                    field: condition.field.value.clone(),
                    comparison: condition.comparison.value,
                    expected: data_value_from_syntax(&condition.expected, limits)?,
                })
            })
            .collect::<Result<Vec<_>, ShellError>>()?;
        let operators = predicate
            .operators
            .iter()
            .map(|operator| operator.value)
            .collect();
        Ok(Self {
            conditions,
            operators,
        })
    }
}

fn predicate_matches(
    predicate: &RuntimePredicate,
    row: &DataValue,
    stage: &str,
) -> Result<bool, ShellError> {
    let Some(first) = predicate.conditions.first() else {
        return Err(ShellError::new(
            ErrorCode::Data,
            "parsed data predicate contains no conditions",
        )
        .with_help("Report this internal parser invariant failure"));
    };
    let mut group = evaluate_condition(first, row, stage)?;
    let mut result = false;
    for (operator, condition) in predicate
        .operators
        .iter()
        .zip(predicate.conditions.iter().skip(1))
    {
        match operator {
            SyntaxBooleanOperator::And => {
                group = group && evaluate_condition(condition, row, stage)?;
            }
            SyntaxBooleanOperator::Or => {
                result = result || group;
                group = evaluate_condition(condition, row, stage)?;
            }
        }
    }
    Ok(result || group)
}

fn evaluate_condition(
    condition: &RuntimeCondition,
    row: &DataValue,
    stage: &str,
) -> Result<bool, ShellError> {
    let (field, optional) = split_optional_path(&condition.field);
    let Some(actual) = get_path(row, field) else {
        return if optional {
            Ok(false)
        } else {
            Err(data_error(stage, format!("row has no field `{field}`")))
        };
    };
    match condition.comparison {
        SyntaxComparisonOperator::Equal => values_equal(actual, &condition.expected, stage),
        SyntaxComparisonOperator::NotEqual => {
            values_equal(actual, &condition.expected, stage).map(|v| !v)
        }
        SyntaxComparisonOperator::Less => {
            Ok(compare_values(actual, &condition.expected, stage)? == Ordering::Less)
        }
        SyntaxComparisonOperator::LessOrEqual => Ok(matches!(
            compare_values(actual, &condition.expected, stage)?,
            Ordering::Less | Ordering::Equal
        )),
        SyntaxComparisonOperator::Greater => {
            Ok(compare_values(actual, &condition.expected, stage)? == Ordering::Greater)
        }
        SyntaxComparisonOperator::GreaterOrEqual => Ok(matches!(
            compare_values(actual, &condition.expected, stage)?,
            Ordering::Greater | Ordering::Equal
        )),
    }
}

fn compare_values(
    left: &DataValue,
    right: &DataValue,
    stage: &str,
) -> Result<Ordering, ShellError> {
    match (left, right) {
        (DataValue::Int(left), DataValue::Int(right)) => Ok(left.cmp(right)),
        (DataValue::UInt(left), DataValue::UInt(right)) => Ok(left.cmp(right)),
        (DataValue::Int(left), DataValue::UInt(right)) => Ok(i64_u64_order(*left, *right)),
        (DataValue::UInt(left), DataValue::Int(right)) => {
            Ok(i64_u64_order(*right, *left).reverse())
        }
        (DataValue::Decimal(left), DataValue::Decimal(right)) => decimal_order(left, right, stage),
        (DataValue::Decimal(left), DataValue::Int(right)) => {
            decimal_integer_order(left, *right, stage)
        }
        (DataValue::Int(left), DataValue::Decimal(right)) => {
            decimal_integer_order(right, *left, stage).map(Ordering::reverse)
        }
        (DataValue::Decimal(left), DataValue::UInt(right)) => {
            decimal_unsigned_order(left, *right, stage)
        }
        (DataValue::UInt(left), DataValue::Decimal(right)) => {
            decimal_unsigned_order(right, *left, stage).map(Ordering::reverse)
        }
        (DataValue::String(left), DataValue::String(right))
        | (DataValue::Path(left), DataValue::Path(right))
        | (DataValue::DateTime(left), DataValue::DateTime(right))
        | (DataValue::Pattern(left), DataValue::Pattern(right)) => Ok(left.cmp(right)),
        (DataValue::Bool(left), DataValue::Bool(right)) => Ok(left.cmp(right)),
        (DataValue::Duration { nanoseconds: left }, DataValue::Duration { nanoseconds: right })
        | (DataValue::Size { bytes: left }, DataValue::Size { bytes: right }) => {
            Ok(left.cmp(right))
        }
        (DataValue::Size { bytes: left }, right)
        | (DataValue::Duration { nanoseconds: left }, right) => {
            compare_unsigned_to_numeric(*left, right, stage)
        }
        (left, DataValue::Size { bytes: right })
        | (left, DataValue::Duration { nanoseconds: right }) => {
            compare_unsigned_to_numeric(*right, left, stage).map(Ordering::reverse)
        }
        _ => Err(data_error(
            stage,
            format!(
                "cannot order {} and {} values",
                value_kind(left),
                value_kind(right)
            ),
        )),
    }
}

fn values_equal(left: &DataValue, right: &DataValue, stage: &str) -> Result<bool, ShellError> {
    if is_numeric_value(left) && is_numeric_value(right) {
        return compare_values(left, right, stage).map(|ordering| ordering == Ordering::Equal);
    }
    Ok(left == right)
}

fn is_numeric_value(value: &DataValue) -> bool {
    matches!(
        value,
        DataValue::Int(_)
            | DataValue::UInt(_)
            | DataValue::Decimal(_)
            | DataValue::Duration { .. }
            | DataValue::Size { .. }
    )
}

fn compare_unsigned_to_numeric(
    left: u64,
    right: &DataValue,
    stage: &str,
) -> Result<Ordering, ShellError> {
    match right {
        DataValue::UInt(right) => Ok(left.cmp(right)),
        DataValue::Int(right) => Ok(i64_u64_order(*right, left).reverse()),
        DataValue::Decimal(right) => {
            decimal_unsigned_order(right, left, stage).map(Ordering::reverse)
        }
        _ => Err(data_error(
            stage,
            format!(
                "cannot order unsigned domain magnitude and {} values",
                value_kind(right)
            ),
        )),
    }
}

fn i64_u64_order(left: i64, right: u64) -> Ordering {
    u64::try_from(left).map_or(Ordering::Less, |left| left.cmp(&right))
}

fn decimal_order(left: &str, right: &str, stage: &str) -> Result<Ordering, ShellError> {
    let left = left
        .parse::<f64>()
        .map_err(|_| data_error(stage, "left decimal cannot be ordered"))?;
    let right = right
        .parse::<f64>()
        .map_err(|_| data_error(stage, "right decimal cannot be ordered"))?;
    left.partial_cmp(&right)
        .ok_or_else(|| data_error(stage, "non-finite decimals cannot be ordered"))
}

fn decimal_integer_order(left: &str, right: i64, stage: &str) -> Result<Ordering, ShellError> {
    decimal_order(left, &right.to_string(), stage)
}

fn decimal_unsigned_order(left: &str, right: u64, stage: &str) -> Result<Ordering, ShellError> {
    decimal_order(left, &right.to_string(), stage)
}

fn usize_value(value: usize) -> Result<DataValue, ShellError> {
    u64::try_from(value).map(DataValue::UInt).map_err(|_| {
        ShellError::new(
            ErrorCode::ResourceLimit,
            "data length exceeds the supported unsigned integer range",
        )
        .with_context(format!("observed platform length: {value}"))
        .with_help("Use `take <count>` before requesting the length")
    })
}

fn value_kind(value: &DataValue) -> &'static str {
    match value {
        DataValue::Nothing => "nothing",
        DataValue::Bool(_) => "boolean",
        DataValue::Int(_) => "integer",
        DataValue::UInt(_) => "unsigned integer",
        DataValue::Decimal(_) => "decimal",
        DataValue::String(_) => "string",
        DataValue::List(_) => "list",
        DataValue::Record(_) => "record",
        DataValue::Path(_) => "path",
        DataValue::Duration { .. } => "duration",
        DataValue::Size { .. } => "size",
        DataValue::DateTime(_) => "datetime",
        DataValue::Pattern(_) => "pattern",
    }
}

fn get_path<'a>(value: &'a DataValue, path: &str) -> Option<&'a DataValue> {
    path.split('.')
        .try_fold(value, |value, segment| match value {
            DataValue::Record(values) => values.get(segment),
            DataValue::List(values) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| values.get(index)),
            _ => None,
        })
}

fn sort_rows(
    value: DataValue,
    field: &str,
    direction: SortDirection,
    stage: &str,
) -> Result<DataValue, ShellError> {
    let descending = direction == SortDirection::Descending;
    let DataValue::List(mut values) = value else {
        return Err(data_error(stage, "sort expects a list of records"));
    };
    for value in &values {
        if !matches!(value, DataValue::Record(_)) {
            return Err(data_error(stage, "sort expects record rows"));
        }
        if get_path(value, field).is_none() {
            return Err(data_error(stage, format!("row has no field `{field}`")));
        }
    }

    let mut comparison_error = None;
    values.sort_by(|left, right| {
        if comparison_error.is_some() {
            return Ordering::Equal;
        }
        let (Some(left), Some(right)) = (get_path(left, field), get_path(right, field)) else {
            comparison_error = Some(data_error(stage, format!("row has no field `{field}`")));
            return Ordering::Equal;
        };
        match compare_values(left, right, stage) {
            Ok(ordering) if descending => ordering.reverse(),
            Ok(ordering) => ordering,
            Err(error) => {
                comparison_error = Some(error);
                Ordering::Equal
            }
        }
    });
    if let Some(error) = comparison_error {
        return Err(error);
    }
    Ok(DataValue::List(values))
}

fn take_values(value: DataValue, count: u64, stage: &str) -> Result<DataValue, ShellError> {
    let count = usize::try_from(count).map_err(|_| {
        limit_error(
            "take count exceeds the platform index range",
            "Use a smaller non-negative count",
        )
    })?;
    let DataValue::List(mut values) = value else {
        return Err(data_error(stage, "take expects a list"));
    };
    values.truncate(count);
    Ok(DataValue::List(values))
}

fn select_fields(
    value: DataValue,
    fields: &[String],
    stage: &str,
) -> Result<DataValue, ShellError> {
    fn select(
        object: IndexMap<String, DataValue>,
        fields: &[String],
    ) -> IndexMap<String, DataValue> {
        fields
            .iter()
            .filter_map(|field| {
                object
                    .get(field)
                    .cloned()
                    .map(|value| (field.clone(), value))
            })
            .collect()
    }

    match value {
        DataValue::Record(object) => Ok(DataValue::Record(select(object, fields))),
        DataValue::List(values) => values
            .into_iter()
            .map(|value| match value {
                DataValue::Record(object) => Ok(DataValue::Record(select(object, fields))),
                _ => Err(data_error(stage, "select expects record rows")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(DataValue::List),
        _ => Err(data_error(
            stage,
            "select expects a record or list of records",
        )),
    }
}

fn parse_json_boundary(
    bytes: &str,
    stage: &str,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    if bytes.len() > limits.max_source_bytes {
        return Err(resource_limit_error(
            "JSON bridge input bytes",
            limits.max_source_bytes,
            bytes.len(),
            "Use a bounded external command or raise the configured data source limit",
        ));
    }
    let value = serde_json::from_str(bytes).map_err(|error| {
        data_error(stage, format!("from json received invalid JSON: {error}"))
            .with_help("Ensure the byte producer emits one valid JSON document")
    })?;
    data_value_from_json(value, limits)
}

fn to_json_boundary(
    value: &DataValue,
    stage: &str,
    limits: DataLimits,
) -> Result<String, ShellError> {
    let json = json_from_data_value(value, limits)?;
    let rendered = serde_json::to_string(&json).map_err(|error| {
        data_error(stage, "to json cannot encode this value")
            .with_context(error.to_string())
            .with_help("Select JSON-compatible fields before crossing the byte boundary")
    })?;
    let observed = u64::try_from(rendered.len()).unwrap_or(u64::MAX);
    if observed > limits.max_file_bytes {
        return Err(resource_limit_error_u64(
            "JSON bridge output bytes",
            limits.max_file_bytes,
            observed,
            "Select fewer fields or raise the configured conversion byte limit",
        ));
    }
    Ok(rendered)
}

fn syntax_limits(limits: DataLimits) -> DataSyntaxLimits {
    DataSyntaxLimits {
        input_bytes_max: limits.max_source_bytes,
        nesting_depth_max: limits.max_depth,
        fields_max: limits.max_fields,
        literal_bytes_max: DataSyntaxLimits::DEFAULT
            .literal_bytes_max
            .min(limits.max_source_bytes),
        ..DataSyntaxLimits::DEFAULT
    }
}

fn syntax_shell_error(diagnostic: DataSyntaxDiagnostic) -> ShellError {
    let code = match diagnostic.kind {
        DataSyntaxDiagnosticKind::ResourceLimit => ErrorCode::ResourceLimit,
        DataSyntaxDiagnosticKind::Encoding | DataSyntaxDiagnosticKind::Syntax => ErrorCode::Data,
    };
    ShellError::new(code, diagnostic.message)
        .with_label(
            None,
            diagnostic.start,
            diagnostic.end,
            "invalid data syntax",
        )
        .with_help(diagnostic.help)
}

fn data_error(stage: &str, message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Data, message)
        .with_context(format!("in `{stage}`"))
        .with_help("Try `help data` for source and transform syntax")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::OpenOptions,
        io::{self, Write},
        sync::atomic::AtomicU64,
    };

    static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TemporaryFile {
        path: PathBuf,
    }

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryFile {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl AsRef<Path> for TemporaryFile {
        fn as_ref(&self) -> &Path {
            self.path()
        }
    }

    impl Drop for TemporaryFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn temporary_bytes(extension: &str, contents: &[u8]) -> TemporaryFile {
        for _ in 0..128 {
            let nonce = TEMPORARY_FILE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quirl-data-{}-{nonce}.{extension}",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(contents) {
                        drop(file);
                        let _ = std::fs::remove_file(&path);
                        panic!("could not write temporary test file: {error}");
                    }
                    return TemporaryFile { path };
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create temporary test file: {error}"),
            }
        }
        panic!("could not allocate a unique temporary test file after 128 attempts");
    }

    fn temporary_file(extension: &str, contents: &str) -> TemporaryFile {
        temporary_bytes(extension, contents.as_bytes())
    }

    fn temporary_directory() -> TemporaryDirectory {
        for _ in 0..128 {
            let nonce = TEMPORARY_FILE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quirl-data-directory-{}-{nonce}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return TemporaryDirectory { path },
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("could not create temporary test directory: {error}"),
            }
        }
        panic!("could not allocate a unique temporary test directory after 128 attempts");
    }

    fn typed(value: serde_json::Value) -> DataValue {
        data_value_from_json(value, DataLimits::DEFAULT).unwrap()
    }

    #[cfg(unix)]
    fn temporary_fifo(extension: &str) -> TemporaryFile {
        use nix::{errno::Errno, sys::stat::Mode, unistd::mkfifo};

        for _ in 0..128 {
            let nonce = TEMPORARY_FILE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quirl-data-{}-{nonce}.{extension}",
                std::process::id()
            ));
            match mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR) {
                Ok(()) => return TemporaryFile { path },
                Err(Errno::EEXIST) => continue,
                Err(error) => panic!("could not create temporary test FIFO: {error}"),
            }
        }
        panic!("could not allocate a unique temporary test FIFO after 128 attempts");
    }

    fn tar_entry(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        let size = format!("{:011o}\0", contents.len());
        header[124..124_usize.saturating_add(size.len())].copy_from_slice(size.as_bytes());
        header[156] = b'0';
        header[148..156].fill(b' ');
        let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        let mut archive = header.to_vec();
        archive.extend_from_slice(contents);
        archive.resize(
            512_usize.saturating_add(contents.len().div_ceil(512).saturating_mul(512)),
            0,
        );
        archive
    }

    fn update_tar_checksum(header: &mut [u8]) {
        header[148..156].fill(b' ');
        let checksum = header[..512]
            .iter()
            .map(|byte| u64::from(*byte))
            .sum::<u64>();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
    }

    struct CancelAfterFirstWrite<'a> {
        cancelled: &'a AtomicBool,
        writes: usize,
        output: Vec<u8>,
    }

    impl Write for CancelAfterFirstWrite<'_> {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes = self.writes.saturating_add(1);
            self.output.extend_from_slice(buffer);
            if self.writes == 1 {
                self.cancelled.store(true, AtomicOrdering::Relaxed);
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed output"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, AtomicOrdering::Relaxed);
        }
    }

    fn render_stream_with_limit(
        rows: Vec<DataValue>,
        format: DataRenderFormat,
        max_materialized_bytes: usize,
    ) -> Result<String, ShellError> {
        let limits = DataLimits {
            max_materialized_bytes,
            ..DataLimits::DEFAULT
        };
        DataOutput::Stream(DataStream::from_values(rows, limits)).render(
            format,
            &AtomicBool::new(false),
            limits,
        )
    }

    fn expansion_row() -> DataValue {
        DataValue::Record(IndexMap::from([(
            "header".to_owned(),
            DataValue::String("\u{001b}".repeat(128)),
        )]))
    }

    #[test]
    fn transforms_structured_rows_without_stringification() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval_typed(
                r#"[{"name":"api","status":"up"},{"name":"db","status":"down"}]
                   | where status == "down" | select name"#,
            )
            .unwrap();
        assert_eq!(value, typed(serde_json::json!([{"name": "db"}])));
    }

    #[test]
    fn pipes_inside_json_strings_do_not_split_the_pipeline() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval_typed(r#"{"value":"a|b"} | get value"#)
                .unwrap(),
            DataValue::String("a|b".to_owned())
        );
    }

    #[test]
    fn length_preserves_a_numeric_value() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime.eval_typed("[1,2,3] | length").unwrap(),
            DataValue::UInt(3)
        );
    }

    #[test]
    fn filters_sorts_and_limits_rows_with_the_documented_grammar() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval_typed(
                r#"[
                    {"name":"old-small","kind":"file","size":100,"meta":{"age":40}},
                    {"name":"new-large","kind":"file","size":900,"meta":{"age":2}},
                    {"name":"old-large","kind":"file","size":700,"meta":{"age":35}},
                    {"name":"directory","kind":"dir","size":1200,"meta":{"age":90}},
                    {"name":"old-largest","kind":"file","size":1100,"meta":{"age":60}}
                ]
                | where kind == file and meta.age > 30
                | select name size
                | sort size desc
                | take 2"#,
            )
            .unwrap();
        assert_eq!(
            value,
            typed(serde_json::json!([
                {"name": "old-largest", "size": 1100},
                {"name": "old-large", "size": 700}
            ]))
        );
    }

    #[test]
    fn where_supports_all_comparisons_and_and_before_or_precedence() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval_typed(
                r#"[
                    {"name":"a","score":1,"enabled":true},
                    {"name":"b","score":2,"enabled":false},
                    {"name":"c","score":3,"enabled":true},
                    {"name":"d","score":4,"enabled":true}
                ] | where score >= 2 and score < 4 or name != "d" and enabled == true
                  | get name"#,
            )
            .unwrap();
        assert_eq!(value, typed(serde_json::json!(["a", "b", "c"])));

        assert_eq!(
            runtime
                .eval_typed(r#"[{"n":1},{"n":2},{"n":3}] | where n <= 2 and n > 1"#)
                .unwrap(),
            typed(serde_json::json!([{"n": 2}]))
        );
    }

    #[test]
    fn where_on_a_field_missing_from_a_row_is_an_error_not_a_silent_non_match() {
        let runtime = DataRuntime::new();
        let error = runtime
            .eval_typed(r#"[{"a":1}] | where b == 1"#)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(error.message.contains("row has no field `b`"));
    }

    #[test]
    fn quoted_predicate_values_remain_strings() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval_typed(r#"[{"value":"42"},{"value":42}] | where value == "42""#)
                .unwrap(),
            typed(serde_json::json!([{"value": "42"}]))
        );
        assert_eq!(
            runtime
                .eval_typed(r#"[{"value":"a and b"},{"value":"a"}] | where value == 'a and b'"#)
                .unwrap(),
            typed(serde_json::json!([{"value": "a and b"}]))
        );
    }

    #[test]
    fn predicate_literals_are_lowered_once_under_runtime_limits_before_row_pulls() {
        let runtime = DataRuntime::with_limits(DataLimits {
            max_retained_text_bytes: 1,
            ..DataLimits::DEFAULT
        });
        assert!(runtime.eval_output(r#"[] | where field == "a""#).is_ok());
        let error = match runtime.eval_output(r#"[] | where field == "ab""#) {
            Ok(_) => panic!("predicate literal must be validated even when the stream is empty"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 1; observed: 2"));

        assert_eq!(
            DataRuntime::new()
                .eval_typed(r#"[{"field":"ab"}] | where field == "ab""#)
                .unwrap(),
            typed(serde_json::json!([{"field": "ab"}]))
        );
    }

    #[test]
    fn nested_fields_work_for_get_where_and_sort() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval_typed(
                    r#"[{"user":{"name":"Ada","rank":2}},{"user":{"name":"Lin","rank":1}}]
                       | where user.rank != 3 | sort user.rank | get user.name"#,
                )
                .unwrap(),
            typed(serde_json::json!(["Lin", "Ada"]))
        );
    }

    #[test]
    fn cell_paths_index_into_lists_and_optional_paths_tolerate_a_missing_segment() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval_typed(r#"[{"items":[{"name":"a"},{"name":"b"}]}] | get items.1.name"#)
                .unwrap(),
            typed(serde_json::json!(["b"]))
        );

        assert_eq!(
            runtime.eval_typed(r#"[{"a":1}] | get b?"#).unwrap(),
            typed(serde_json::json!([null]))
        );
        assert!(runtime.eval_typed(r#"[{"a":1}] | get b"#).is_err());

        assert_eq!(
            runtime
                .eval_typed(r#"[{"a":1},{"a":2,"b":9}] | where b? == 9 | get a"#)
                .unwrap(),
            typed(serde_json::json!([2]))
        );
    }

    #[test]
    fn malformed_predicates_and_incomparable_sorts_are_errors() {
        let runtime = DataRuntime::new();
        assert!(
            runtime
                .eval_typed(r#"[{"value":1}] | where value = 1"#)
                .is_err()
        );
        assert!(
            runtime
                .eval_typed(r#"[{"value":1},{"value":"one"}] | sort value"#)
                .is_err()
        );
        assert!(runtime.eval_typed("[1,2 | length").is_err());
    }

    #[test]
    fn cancellation_stops_between_typed_pipeline_stages() {
        let cancelled = AtomicBool::new(true);
        let error = DataRuntime::new()
            .eval_typed_with_cancellation("[1,2,3] | length", &cancelled)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn adapters_preserve_typed_json_toml_and_csv_rows() {
        let toml = temporary_file("toml", "[service]\nname = 'api'\nport = 8080\n");
        let csv = temporary_file("csv", "name,enabled\napi,true\nworker,false\n");
        let runtime = DataRuntime::new();

        assert_eq!(
            runtime
                .eval_typed(&format!(
                    "open {} | get service.name",
                    toml.path().display()
                ))
                .unwrap(),
            DataValue::String("api".to_owned())
        );
        let mut stream = runtime.open_stream(&csv).unwrap();
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(typed(serde_json::json!({"name": "api", "enabled": "true"})))
        );
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(typed(
                serde_json::json!({"name": "worker", "enabled": "false"})
            ))
        );
        assert_eq!(stream.next(&AtomicBool::new(false)).unwrap(), None);
    }

    #[test]
    fn csv_field_limit_accepts_exact_width_and_rejects_header_and_row_plus_one() {
        let limits = DataLimits {
            max_fields: 2,
            ..DataLimits::DEFAULT
        };
        let runtime = DataRuntime::with_limits(limits);
        let exact = temporary_file("csv", "left,right\none,two\n");
        assert_eq!(
            runtime
                .open_stream(&exact)
                .unwrap()
                .next(&AtomicBool::new(false))
                .unwrap(),
            Some(typed(serde_json::json!({"left": "one", "right": "two"})))
        );

        let wide_header = temporary_file("csv", "one,two,three\n");
        let error = match runtime.open_stream(&wide_header) {
            Ok(_) => panic!("header field limit + 1 must fail during stream construction"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 2; observed: 3"));

        let wide_row = temporary_file("csv", "one,two\na,b,c\n");
        let error = runtime
            .open_stream(&wide_row)
            .unwrap()
            .next(&AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 2; observed: 3"));
    }

    #[test]
    fn text_adapters_strip_one_leading_bom_and_preserve_normal_utf8() {
        let json = temporary_file("json", "\u{feff}{\"name\":\"café\"}");
        let csv = temporary_file("csv", "\u{feff}name\ncafé\n");
        let yaml = temporary_file("yaml", "\u{feff}name: café\n");
        let runtime = DataRuntime::new();

        assert_eq!(
            runtime
                .eval_typed(&format!("open {} | get name", json.path().display()))
                .unwrap(),
            DataValue::String("café".to_owned())
        );
        assert_eq!(
            runtime
                .eval_typed(&format!("open {} | get name", csv.path().display()))
                .unwrap(),
            DataValue::List(vec![DataValue::String("café".to_owned())])
        );
        assert_eq!(
            runtime
                .eval_typed(&format!("open {} | get name", yaml.path().display()))
                .unwrap(),
            DataValue::String("café".to_owned())
        );
    }

    #[test]
    fn invalid_utf8_is_a_data_encoding_diagnostic_for_value_and_csv_adapters() {
        for extension in ["json", "csv"] {
            let file = temporary_bytes(extension, b"name\n\xff\n");
            let error = if extension == "csv" {
                DataRuntime::new()
                    .open_stream(&file)
                    .unwrap()
                    .next(&AtomicBool::new(false))
                    .unwrap_err()
            } else {
                DataRuntime::new()
                    .eval_typed(&format!("open {}", file.path().display()))
                    .unwrap_err()
            };
            assert_eq!(error.code, ErrorCode::Data, "{extension}");
            assert!(error.message.contains("not valid UTF-8"), "{extension}");
            assert!(!error.details.help.is_empty(), "{extension}");
        }
    }

    #[test]
    fn typed_sources_survive_transforms_bridges_and_rendering() {
        let directory = temporary_directory();
        std::fs::write(directory.path.join("entry.txt"), b"five!").unwrap();
        let expression = format!(
            "files {} | where size == 5 | select path size modified | first",
            directory.path.display()
        );
        let envelope = DataRuntime::new().eval_envelope(&expression).unwrap();
        let DataEnvelope::Option { value: Some(value) } = envelope else {
            panic!("first over one typed filesystem row must return some");
        };
        let DataEnvelope::Value {
            value: DataValue::Record(row),
        } = *value
        else {
            panic!("selected filesystem row must remain a typed record");
        };
        assert!(matches!(row.get("path"), Some(DataValue::Path(_))));
        assert_eq!(row.get("size"), Some(&DataValue::Size { bytes: 5 }));
        assert!(matches!(row.get("modified"), Some(DataValue::DateTime(_))));

        let rendered = DataRuntime::new()
            .render(
                &format!("{expression} | to json"),
                DataRenderFormat::Json,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(rendered.contains("\"kind\": \"option\""));
        assert!(rendered.contains("\\\"size\\\":\\\"5B\\\""));
    }

    #[test]
    fn toml_datetime_survives_without_a_json_round_trip() {
        let toml = temporary_file("toml", "when = 1979-05-27T07:32:00Z\n");
        assert_eq!(
            DataRuntime::new()
                .eval_typed(&format!("open {} | get when", toml.path().display()))
                .unwrap(),
            DataValue::DateTime("1979-05-27T07:32:00Z".to_owned())
        );
        assert_eq!(
            DataRuntime::new()
                .eval(&format!("open {} | get when", toml.path().display()))
                .unwrap(),
            serde_json::Value::String("1979-05-27T07:32:00Z".to_owned())
        );
    }

    #[test]
    fn yaml_and_tar_adapters_are_bounded_and_preserve_typed_rows() {
        let yaml = temporary_file("yaml", "service:\n  name: api\n  ports: [8080, 8443]\n");
        let mut archive = tar_entry("alpha.txt", b"alpha");
        archive.extend(tar_entry("nested/bravo.txt", b"bravo"));
        archive.extend([0_u8; 1024]);
        let tar = temporary_bytes("tar", &archive);
        let runtime = DataRuntime::new();

        assert_eq!(
            runtime
                .eval_typed(&format!(
                    "open {} | get service.name",
                    yaml.path().display()
                ))
                .unwrap(),
            DataValue::String("api".to_owned())
        );
        let mut stream = runtime.open_stream(&tar).unwrap();
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(DataValue::Record(IndexMap::from([
                ("kind".to_owned(), DataValue::String("file".to_owned())),
                ("path".to_owned(), DataValue::Path("alpha.txt".to_owned())),
                ("size".to_owned(), DataValue::Size { bytes: 5 }),
            ])))
        );
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(DataValue::Record(IndexMap::from([
                ("kind".to_owned(), DataValue::String("file".to_owned())),
                (
                    "path".to_owned(),
                    DataValue::Path("nested/bravo.txt".to_owned()),
                ),
                ("size".to_owned(), DataValue::Size { bytes: 5 }),
            ])))
        );
        assert_eq!(stream.next(&AtomicBool::new(false)).unwrap(), None);
    }

    #[test]
    fn yaml_references_fail_before_expansion_while_reference_text_remains_valid() {
        let amplified = temporary_file(
            "yaml",
            "base: &base [one, two]\ncopy: [*base, *base, *base, *base]\n",
        );
        let error = DataRuntime::new()
            .eval_typed(&format!("open {}", amplified.path().display()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(error.message.contains("anchor"));
        assert!(error.details.context[0].contains("reference indicator at byte"));

        let anchor_free = temporary_file(
            "yaml",
            "quoted: \"&anchor *alias\"\nplain: AT&T\nblock: |\n  *not-an-alias\n",
        );
        let value = DataRuntime::new()
            .eval_typed(&format!("open {}", anchor_free.path().display()))
            .unwrap();
        let DataValue::Record(value) = value else {
            panic!("anchor-free YAML must remain a record");
        };
        assert_eq!(
            value.get("quoted"),
            Some(&DataValue::String("&anchor *alias".to_owned()))
        );
        assert_eq!(
            value.get("block"),
            Some(&DataValue::String("*not-an-alias\n".to_owned()))
        );

        let exact_nodes = temporary_file("yaml", "[one, two]\n");
        let excess_nodes = temporary_file("yaml", "[one, two, three]\n");
        let runtime = DataRuntime::with_limits(DataLimits {
            max_nodes: 3,
            ..DataLimits::DEFAULT
        });
        assert!(
            runtime
                .eval_typed(&format!("open {}", exact_nodes.path().display()))
                .is_ok()
        );
        let error = runtime
            .eval_typed(&format!("open {}", excess_nodes.path().display()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 3; observed: 4"));
    }

    #[test]
    fn pull_pipeline_stops_before_later_invalid_csv_rows() {
        let csv = temporary_file("csv", "name,kind\napi,service\nbroken\n");
        let output = DataRuntime::new()
            .eval_output(&format!(
                "open {} | take 1 | select name",
                csv.path().display()
            ))
            .unwrap()
            .render(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                DataLimits::DEFAULT,
            )
            .unwrap();
        assert_eq!(output, "{\"name\":\"api\"}\n");
    }

    #[test]
    fn tar_stream_checks_cancellation_and_row_bounds_before_extracting() {
        let mut archive = tar_entry("one", b"1");
        archive.extend(tar_entry("two", b"2"));
        archive.extend([0_u8; 1024]);
        let tar = temporary_bytes("tar", &archive);
        let cancelled = AtomicBool::new(true);
        let mut stream = DataRuntime::new().open_stream(&tar).unwrap();
        assert_eq!(
            stream.next(&cancelled).unwrap_err().code,
            ErrorCode::ResourceLimit
        );

        let runtime = DataRuntime::with_limits(DataLimits {
            max_rows: 1,
            ..DataLimits::DEFAULT
        });
        let error = runtime
            .open_stream(&tar)
            .unwrap()
            .collect(&AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn tar_rejects_a_bad_header_checksum_before_reporting_entries() {
        let mut archive = tar_entry("one", b"1");
        archive.extend([0_u8; 1024]);
        archive[0] = b'x';
        let tar = temporary_bytes("tar", &archive);
        let error = DataRuntime::new()
            .open_stream(&tar)
            .unwrap()
            .next(&AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(error.details.context[0].contains("checksum"));
    }

    #[test]
    fn tar_rejects_non_utf8_entry_names_with_actionable_data_error() {
        let mut archive = tar_entry("valid", b"");
        archive[0] = 0xff;
        update_tar_checksum(&mut archive[..512]);
        archive.extend([0_u8; 1024]);
        let tar = temporary_bytes("tar", &archive);
        let error = DataRuntime::new()
            .open_stream(&tar)
            .unwrap()
            .next(&AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(error.details.context[0].contains("entry name is not valid UTF-8"));
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn render_to_writes_rows_incrementally_and_honors_cancellation() {
        let cancelled = AtomicBool::new(false);
        let output = DataOutput::Stream(DataStream::from_values(
            vec![
                DataValue::String("one".to_owned()),
                DataValue::String("two".to_owned()),
            ],
            DataLimits::DEFAULT,
        ));
        let mut writer = CancelAfterFirstWrite {
            cancelled: &cancelled,
            writes: 0,
            output: Vec::new(),
        };
        let error = output
            .render_to(
                DataRenderFormat::Plain,
                &cancelled,
                &mut writer,
                DataLimits::DEFAULT,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(writer.writes, 2);
        assert_eq!(writer.output, b"one\n");
    }

    #[test]
    fn every_collected_format_accepts_exact_limit_and_rejects_limit_plus_one() {
        for format in [
            DataRenderFormat::Json,
            DataRenderFormat::Plain,
            DataRenderFormat::Table,
        ] {
            let expected = render_stream_with_limit(
                vec![expansion_row()],
                format,
                DataLimits::DEFAULT.max_materialized_bytes,
            )
            .unwrap();
            let exact =
                render_stream_with_limit(vec![expansion_row()], format, expected.len()).unwrap();
            assert_eq!(exact, expected, "exact limit failed for {format:?}");

            let limit = expected.len() - 1;
            let error = render_stream_with_limit(vec![expansion_row()], format, limit).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit, "{format:?}");
            assert!(error.message.contains("collected data render bytes"));
            assert!(error.details.context[0].contains(&format!("limit: {limit}")));
            assert!(
                error.details.context[0].contains(&format!("observed: {}", expected.len())),
                "{}",
                error.details.context[0]
            );
        }
    }

    #[test]
    fn many_small_rows_cannot_exceed_the_aggregate_render_limit() {
        let rows = (0..8)
            .map(|_| DataValue::String("abcdefghij".to_owned()))
            .collect();
        let error = render_stream_with_limit(rows, DataRenderFormat::Plain, 64).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 64"));
        assert!(error.details.context[0].contains("observed: 72"));
    }

    #[test]
    fn render_to_keeps_many_small_rows_streaming_past_the_aggregate_limit() {
        let rows = (0..8)
            .map(|_| DataValue::String("abcdefghij".to_owned()))
            .collect();
        let output = DataOutput::Stream(DataStream::from_values(
            rows,
            DataLimits {
                max_materialized_bytes: 128,
                ..DataLimits::DEFAULT
            },
        ));
        let mut writer = Vec::new();
        output
            .render_to(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                &mut writer,
                DataLimits {
                    max_materialized_bytes: 128,
                    ..DataLimits::DEFAULT
                },
            )
            .unwrap();
        assert_eq!(writer.len(), 88);
    }

    #[test]
    fn one_expanding_row_and_nested_value_are_bounded_before_retention() {
        let row_error = render_stream_with_limit(
            vec![DataValue::String("\u{001b}".repeat(64))],
            DataRenderFormat::Plain,
            128,
        )
        .unwrap_err();
        assert_eq!(row_error.code, ErrorCode::ResourceLimit);

        let nested = DataValue::List(vec![DataValue::String("\u{001b}".repeat(128))]);
        let nested_error =
            render_stream_with_limit(vec![nested], DataRenderFormat::Plain, 256).unwrap_err();
        assert_eq!(nested_error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn table_headers_separators_and_escaping_are_part_of_the_bound() {
        let row = DataValue::Record(IndexMap::from([(
            "na\u{001b}me".to_owned(),
            DataValue::String("value".to_owned()),
        )]));
        let rendered = render_stream_with_limit(
            vec![row.clone(), row.clone()],
            DataRenderFormat::Table,
            DataLimits::DEFAULT.max_materialized_bytes,
        )
        .unwrap();
        assert_eq!(
            rendered,
            "╭───┬────────────╮\n\
             │ # │ na\\u{1b}me │\n\
             ├───┼────────────┤\n\
             │ 0 │ value      │\n\
             │ 1 │ value      │\n\
             ╰───┴────────────╯\n"
        );
        let single_row = render_stream_with_limit(
            vec![row.clone()],
            DataRenderFormat::Table,
            DataLimits::DEFAULT.max_materialized_bytes,
        )
        .unwrap();
        let error =
            render_stream_with_limit(vec![row], DataRenderFormat::Table, single_row.len() - 1)
                .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn select_and_table_rendering_preserve_requested_and_first_seen_column_order() {
        let runtime = DataRuntime::new();
        let DataValue::List(rows) = runtime
            .eval_typed(
                r#"[{"service":"api","region":"us","status":"failed"}]
                   | select service region"#,
            )
            .unwrap()
        else {
            panic!("expected a list of selected rows");
        };
        let [DataValue::Record(row)] = rows.as_slice() else {
            panic!("expected exactly one selected record");
        };
        assert_eq!(
            row.keys().map(String::as_str).collect::<Vec<_>>(),
            ["service", "region"]
        );

        let rendered = render_stream_with_limit(
            vec![
                DataValue::Record(IndexMap::from([
                    ("service".to_owned(), DataValue::String("api".to_owned())),
                    ("region".to_owned(), DataValue::String("us".to_owned())),
                ])),
                DataValue::Record(IndexMap::from([(
                    "status".to_owned(),
                    DataValue::String("degraded".to_owned()),
                )])),
            ],
            DataRenderFormat::Table,
            DataLimits::DEFAULT.max_materialized_bytes,
        )
        .unwrap();
        assert_eq!(
            rendered,
            "╭───┬─────────┬────────┬──────────╮\n\
             │ # │ service │ region │  status  │\n\
             ├───┼─────────┼────────┼──────────┤\n\
             │ 0 │ api     │ us     │          │\n\
             │ 1 │         │        │ degraded │\n\
             ╰───┴─────────┴────────┴──────────╯\n"
        );
    }

    #[test]
    fn table_rendering_uses_display_width_and_right_aligns_typed_numbers() {
        let rendered = render_stream_with_limit(
            vec![
                DataValue::Record(IndexMap::from([
                    ("name".to_owned(), DataValue::String("東京".to_owned())),
                    ("size".to_owned(), DataValue::Size { bytes: 2 }),
                ])),
                DataValue::Record(IndexMap::from([
                    ("name".to_owned(), DataValue::String("é".to_owned())),
                    ("size".to_owned(), DataValue::Size { bytes: 100 }),
                ])),
            ],
            DataRenderFormat::Table,
            DataLimits::DEFAULT.max_materialized_bytes,
        )
        .unwrap();

        assert_eq!(
            rendered,
            "╭───┬──────┬───────╮\n\
             │ # │ name │ size  │\n\
             ├───┼──────┼───────┤\n\
             │ 0 │ 東京 │   2 B │\n\
             │ 1 │ é    │ 100 B │\n\
             ╰───┴──────┴───────╯\n"
        );
        for line in rendered.lines() {
            assert_eq!(UnicodeWidthStr::width(line), 20);
        }
    }

    #[test]
    fn table_rendering_keeps_mixed_rows_inside_the_frame() {
        let rendered = render_stream_with_limit(
            vec![
                DataValue::Record(IndexMap::from([
                    ("name".to_owned(), DataValue::String("api".to_owned())),
                    ("count".to_owned(), DataValue::UInt(2)),
                ])),
                DataValue::String("fallback".to_owned()),
            ],
            DataRenderFormat::Table,
            DataLimits::DEFAULT.max_materialized_bytes,
        )
        .unwrap();

        assert_eq!(
            rendered,
            "╭───┬──────────┬───────╮\n\
             │ # │   name   │ count │\n\
             ├───┼──────────┼───────┤\n\
             │ 0 │ api      │     2 │\n\
             │ 1 │ fallback │       │\n\
             ╰───┴──────────┴───────╯\n"
        );
    }

    #[test]
    fn filesystem_table_projects_display_fields_without_losing_typed_metadata() {
        let row = DataValue::Record(IndexMap::from([
            ("hidden".to_owned(), DataValue::Bool(false)),
            ("kind".to_owned(), DataValue::String("directory".to_owned())),
            (
                "modified".to_owned(),
                DataValue::DateTime("2026-08-17T00:00:00Z".to_owned()),
            ),
            ("name".to_owned(), DataValue::String("assets".to_owned())),
            ("path".to_owned(), DataValue::Path("./assets".to_owned())),
            ("readonly".to_owned(), DataValue::Bool(false)),
            ("size".to_owned(), DataValue::Size { bytes: 1_500 }),
            ("target".to_owned(), DataValue::Nothing),
        ]));
        let rendered = render_stream_with_limit(
            vec![row],
            DataRenderFormat::Table,
            DataLimits::DEFAULT.max_materialized_bytes,
        )
        .unwrap();

        assert!(rendered.contains("│ # │  name  │ type │  size  │       modified       │"));
        assert!(rendered.contains("│ 0 │ assets │ dir  │ 1.5 kB │ 2026-08-17T00:00:00Z │"));
        assert!(!rendered.contains("path"));
        assert!(!rendered.contains("readonly"));
        assert!(!rendered.contains("hidden"));
        assert!(!rendered.contains("target"));
    }

    #[test]
    fn table_sizes_and_relative_times_are_compact_and_deterministic() {
        assert_eq!(format_table_size(0), "0 B");
        assert_eq!(format_table_size(999), "999 B");
        assert_eq!(format_table_size(1_000), "1.0 kB");
        assert_eq!(format_table_size(95_167), "95.1 kB");
        assert_eq!(format_table_size(u64::MAX), "18.4 EB");

        let now = 2_000_000;
        assert_eq!(format_relative_time(now, now), "now");
        assert_eq!(format_relative_time(now - 59, now), "59 seconds ago");
        assert_eq!(format_relative_time(now - 60, now), "a minute ago");
        assert_eq!(format_relative_time(now - 7_200, now), "2 hours ago");
        assert_eq!(format_relative_time(now - 604_800, now), "a week ago");
        assert_eq!(format_relative_time(now + 3_600, now), "in an hour");
    }

    #[test]
    fn long_tables_repeat_the_centered_heading_at_the_bottom() {
        let rows = (0..TABLE_REPEAT_HEADER_ROW_MIN)
            .map(|index| {
                DataValue::Record(IndexMap::from([(
                    "value".to_owned(),
                    DataValue::UInt(u64::try_from(index).unwrap()),
                )]))
            })
            .collect();
        let rendered = render_stream_with_limit(
            rows,
            DataRenderFormat::Table,
            DataLimits::DEFAULT.max_materialized_bytes,
        )
        .unwrap();

        assert_eq!(rendered.matches("│ #  │ value │").count(), 2);
        assert!(rendered.ends_with("│ #  │ value │\n╰────┴───────╯\n"));
    }

    #[test]
    fn collected_render_preserves_renderer_failure_after_partial_output() {
        let output = DataOutput::Stream(DataStream::from_values(
            vec![
                DataValue::String("valid".to_owned()),
                DataValue::List(vec![DataValue::Decimal("not-a-number".to_owned())]),
            ],
            DataLimits::DEFAULT,
        ));
        let error = output
            .render(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                DataLimits::DEFAULT,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(error.message.contains("decimal cannot be represented"));
    }

    #[test]
    fn collected_render_preserves_cancellation_after_partial_output() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = Arc::clone(&cancelled);
        let mut first = true;
        let stream = DataStream::from_pull(
            move |_| {
                if first {
                    first = false;
                    cancellation.store(true, AtomicOrdering::Relaxed);
                    Ok(Some(DataValue::String("first".to_owned())))
                } else {
                    Ok(Some(DataValue::String("second".to_owned())))
                }
            },
            DataLimits::DEFAULT,
        );
        let error = DataOutput::Stream(stream)
            .render(DataRenderFormat::Plain, &cancelled, DataLimits::DEFAULT)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("cancelled"));
    }

    #[test]
    fn envelope_and_runtime_collected_rendering_use_the_explicit_limit() {
        let envelope = DataEnvelope::value(expansion_row());
        let error = envelope
            .render_with_limits(
                DataRenderFormat::Json,
                DataLimits {
                    max_materialized_bytes: 512,
                    ..DataLimits::DEFAULT
                },
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);

        let escaped = "\\u001b".repeat(64);
        let runtime = DataRuntime::with_limits(DataLimits {
            max_materialized_bytes: 256,
            ..DataLimits::DEFAULT
        });
        let error = runtime
            .render(
                &format!("\"{escaped}\""),
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn value_and_option_rendering_require_and_enforce_explicit_custom_limits() {
        let exact_limits = DataLimits {
            max_materialized_bytes: std::mem::size_of::<DataValue>() + 2,
            ..DataLimits::DEFAULT
        };
        assert_eq!(
            DataOutput::Value(DataValue::String("ok".to_owned()))
                .render(
                    DataRenderFormat::Plain,
                    &AtomicBool::new(false),
                    exact_limits,
                )
                .unwrap(),
            "ok\n"
        );

        let value_error = DataOutput::Value(DataValue::String("\u{001b}".repeat(10)))
            .render(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                DataLimits {
                    max_materialized_bytes: 42,
                    ..DataLimits::DEFAULT
                },
            )
            .unwrap_err();
        assert_eq!(value_error.code, ErrorCode::ResourceLimit);
        assert!(value_error.details.context[0].contains("limit: 42; observed:"));

        let option_error = DataOutput::Option(None)
            .render(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                DataLimits {
                    max_materialized_bytes: 4,
                    ..DataLimits::DEFAULT
                },
            )
            .unwrap_err();
        assert_eq!(option_error.code, ErrorCode::ResourceLimit);
        assert!(option_error.details.context[0].contains("limit: 4; observed: 5"));
    }

    #[test]
    fn rendered_byte_arithmetic_overflow_is_a_limit_failure() {
        assert_eq!(checked_rendered_usage(usize::MAX, 1), Err(usize::MAX));
        assert_eq!(checked_rendered_usage(usize::MAX - 1, 1), Ok(usize::MAX));
    }

    #[test]
    fn collected_render_allocation_failure_is_a_resource_error() {
        let mut collector = BoundedRenderCollector::new(usize::MAX);
        collector.reserve_for(usize::MAX).unwrap_err();
        let failure = collector.failure.unwrap();
        assert_eq!(
            failure,
            CollectionFailure::Allocation {
                observed: usize::MAX
            }
        );
        let error = collected_render_error(usize::MAX, failure);
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("observed:"));
    }

    #[test]
    fn render_to_reports_output_failures_as_shell_errors() {
        let mut writer = FailingWriter;
        let error = DataOutput::Value(DataValue::String("value".to_owned()))
            .render_to(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                &mut writer,
                DataLimits::DEFAULT,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn stream_reader_state_is_released_after_a_pull_error() {
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = DropProbe(Arc::clone(&dropped));
        let stream = DataStream::from_pull(
            move |_| {
                let _keep_reader_alive = &probe;
                Err(ShellError::new(ErrorCode::Data, "injected reader failure")
                    .with_help("This is a deterministic cleanup-path test"))
            },
            DataLimits::DEFAULT,
        );
        let error = DataOutput::Stream(stream)
            .render(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                DataLimits::DEFAULT,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(dropped.load(AtomicOrdering::Relaxed));
    }

    #[test]
    fn conversion_and_materialization_overflow_report_limit_and_observed_usage() {
        let bridge_runtime = DataRuntime::with_limits(DataLimits {
            max_source_bytes: 128,
            max_file_bytes: 5,
            ..DataLimits::DEFAULT
        });
        let bridge_error = bridge_runtime
            .eval_typed(r#""abcdefgh" | to json"#)
            .unwrap_err();
        assert_eq!(bridge_error.code, ErrorCode::ResourceLimit);
        assert!(bridge_error.details.context[0].contains("limit: 5"));
        assert!(bridge_error.details.context[0].contains("observed:"));

        let limits = DataLimits {
            max_materialized_bytes: std::mem::size_of::<DataValue>() + 8,
            ..DataLimits::DEFAULT
        };
        let error = DataStream::from_values(
            vec![
                DataValue::String("one".to_owned()),
                DataValue::String("two".to_owned()),
            ],
            limits,
        )
        .collect(&AtomicBool::new(false))
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("materialized retained bytes"));
        assert!(error.details.context[0].contains("observed:"));
    }

    #[test]
    fn csv_stream_defers_bad_rows_and_honors_cancellation() {
        let csv = temporary_file("csv", "name,kind\napi,service\nbroken\n");
        let mut stream = DataRuntime::new().open_stream(&csv).unwrap();
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            stream.next(&cancelled).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        let active = AtomicBool::new(false);
        assert!(stream.next(&active).unwrap().is_some());
        assert_eq!(stream.next(&active).unwrap_err().code, ErrorCode::Data);
    }

    #[test]
    fn limits_reject_deep_and_oversized_values_before_transforms() {
        let limits = DataLimits {
            max_depth: 1,
            max_rows: 1,
            max_fields: 1,
            ..DataLimits::DEFAULT
        };
        let runtime = DataRuntime::with_limits(limits);
        assert_eq!(
            runtime.eval_typed("[[1]]").unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            runtime.eval_typed("[1,2]").unwrap_err().code,
            ErrorCode::ResourceLimit
        );

        let json = temporary_file("json", r#"{"one":1,"two":2}"#);
        let error = runtime
            .eval_typed(&format!("open {}", json.path().display()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 1"));
        assert!(error.details.context[0].contains("observed: 2"));
    }

    #[test]
    fn envelope_and_renderers_keep_machine_and_terminal_contracts_distinct() {
        let envelope = DataEnvelope::task(DataEnvelope::result(DataEnvelope::stream(vec![typed(
            serde_json::json!({"name": "api", "port": 8080}),
        )])));
        let json = envelope.render(DataRenderFormat::Json).unwrap();
        assert_eq!(
            serde_json::from_str::<DataEnvelope>(&json).unwrap(),
            envelope
        );
        assert!(json.contains("\"kind\": \"task\""));
        assert!(json.contains("\"state\": \"complete\""));
        assert!(json.contains("\"type\": \"string\""));
        let table = envelope.render(DataRenderFormat::Table).unwrap();
        assert_eq!(
            table,
            "╭───┬──────┬──────╮\n\
             │ # │ name │ port │\n\
             ├───┼──────┼──────┤\n\
             │ 0 │ api  │ 8080 │\n\
             ╰───┴──────┴──────╯\n"
        );
    }

    #[test]
    fn plain_rendering_of_nested_values_does_not_leak_abi_tags() {
        let output = DataEnvelope::value(typed(serde_json::json!({
            "service": {"name": "api"},
            "ports": [8080, 8443],
        })))
        .render(DataRenderFormat::Plain)
        .unwrap();
        assert_eq!(
            output,
            "{\"ports\":[8080,8443],\"service\":{\"name\":\"api\"}}\n"
        );
        assert!(!output.contains("\"type\""));
    }

    #[test]
    fn bounded_reader_rejects_data_past_the_open_handle_limit() {
        let mut reader = BoundedReader::new(std::io::Cursor::new(b"abc"), 2);
        let mut bytes = Vec::new();
        let error = reader.read_to_end(&mut bytes).unwrap_err();
        assert!(is_file_size_limit(&error));
        assert_eq!(bytes, b"ab");
    }

    #[test]
    fn file_adapters_enforce_the_bound_while_reading_not_from_metadata() {
        let json = temporary_file("json", "\"abc\"");
        let runtime = DataRuntime::with_limits(DataLimits {
            max_file_bytes: 4,
            ..DataLimits::DEFAULT
        });
        let error = runtime
            .eval_typed(&format!("open {}", json.path().display()))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(!error.details.help.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn fifo_without_a_writer_is_rejected_without_blocking_open() {
        let fifo = temporary_fifo("json");
        let path = fifo.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = open_bounded_file(&path, DataLimits::DEFAULT.max_file_bytes).map(|_| ());
            let _ = sender.send(result);
        });

        let result = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("opening a FIFO without a writer must not block");
        let error = match result {
            Ok(()) => panic!("non-regular data source must fail closed"),
            Err(error) => error,
        };
        worker.join().unwrap();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("not a regular file"));
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn option_result_and_task_states_remain_explicit_at_the_abi_boundary() {
        let none = DataEnvelope::none();
        assert!(
            none.render(DataRenderFormat::Json)
                .unwrap()
                .contains("\"option\"")
        );
        let failure = DataEnvelope::result_error(ShellError::new(ErrorCode::Data, "bad input"));
        let json = failure.render(DataRenderFormat::Json).unwrap();
        assert!(json.contains("\"state\": \"error\""));
        assert!(json.contains("\"code\": \"data\""));
        assert!(
            DataEnvelope::pending_task()
                .render(DataRenderFormat::Plain)
                .unwrap()
                .contains("Pending")
        );

        let first = DataRuntime::new().eval_envelope("[] | first").unwrap();
        assert_eq!(first, DataEnvelope::none());
        let captured = DataRuntime::new().eval_result_envelope("[1,2");
        assert!(matches!(
            captured,
            DataEnvelope::Result {
                state: ResultState::Error,
                error: Some(_),
                ..
            }
        ));

        let incoherent = DataEnvelope::Task {
            state: TaskState::Pending,
            value: Some(Box::new(DataEnvelope::value(DataValue::Int(1)))),
            error: None,
        };
        assert_eq!(
            incoherent.validate(DataLimits::DEFAULT).unwrap_err().code,
            ErrorCode::Validation
        );
    }

    #[test]
    fn envelope_depth_accepts_exact_limit_and_rejects_limit_plus_one_before_rendering() {
        let exact = DataEnvelope::some(DataEnvelope::some(DataEnvelope::value(DataValue::Int(1))));
        let limits = DataLimits {
            max_depth: 2,
            ..DataLimits::DEFAULT
        };
        exact.validate(limits).unwrap();
        let rendered = exact
            .render_with_limits(DataRenderFormat::Json, limits)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["kind"], "option");

        let too_deep = DataEnvelope::some(exact);
        let error = too_deep
            .render_with_limits(DataRenderFormat::Json, limits)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 2; observed: 3"));
    }

    #[test]
    fn envelope_and_value_nodes_share_one_materialization_budget() {
        let exact = DataEnvelope::some(DataEnvelope::value(DataValue::Int(1)));
        let limits = DataLimits {
            max_nodes: 3,
            ..DataLimits::DEFAULT
        };
        exact.validate(limits).unwrap();

        let error = DataEnvelope::some(exact).validate(limits).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 3; observed: 4"));
    }

    #[test]
    fn iterative_typed_json_writer_round_trips_every_value_shape() {
        let value = DataValue::List(vec![
            DataValue::Nothing,
            DataValue::Bool(true),
            DataValue::Int(-1),
            DataValue::UInt(1),
            DataValue::Decimal("1.25".to_owned()),
            DataValue::String("text".to_owned()),
            DataValue::Record(IndexMap::from([(
                "nested".to_owned(),
                DataValue::List(vec![DataValue::Path("path".to_owned())]),
            )])),
            DataValue::Duration { nanoseconds: 2 },
            DataValue::Size { bytes: 3 },
            DataValue::DateTime("2026-08-17T00:00:00Z".to_owned()),
            DataValue::Pattern("*.rs".to_owned()),
        ]);
        let envelope = DataEnvelope::value(value);
        let json = envelope.render(DataRenderFormat::Json).unwrap();
        assert_eq!(
            serde_json::from_str::<DataEnvelope>(&json).unwrap(),
            envelope
        );
    }

    #[test]
    fn envelope_deserialization_rejects_unknown_fields() {
        let error = serde_json::from_value::<DataEnvelope>(serde_json::json!({
            "kind": "value",
            "value": {"type": "int", "value": 1},
            "unexpected": true
        }))
        .unwrap_err();
        assert!(error.to_string().contains("unknown field `unexpected`"));
    }

    #[test]
    fn byte_value_boundaries_are_explicit_and_preserve_lazy_lines() {
        let host: ProcessHost = Arc::new(|request| {
            assert_eq!(request.deadline, Duration::from_secs(2));
            assert_eq!(request.max_output_bytes, 1024 * 1024);
            Ok(quirl_core::CommandOutcome {
                status: 0,
                stdout: Some("one\r\ntwo\n".to_owned()),
                stderr: None,
            })
        });
        let runtime = DataRuntime::with_process_host(host);
        assert_eq!(
            runtime
                .eval_typed("^external fixture | lines | take 1")
                .unwrap(),
            typed(serde_json::json!(["one"]))
        );
        assert_eq!(
            DataRuntime::new()
                .eval_typed(r#""{\"name\":\"api\"}" | from json | get name | to json"#)
                .unwrap(),
            DataValue::String("\"api\"".to_owned())
        );
    }

    #[test]
    fn external_sources_fail_closed_and_invalid_json_is_diagnostic() {
        let error = DataRuntime::new()
            .eval_typed("^external fixture")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!error.details.help.is_empty());

        let error = DataRuntime::new()
            .eval_typed(r#""not json" | from json"#)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(error.message.contains("invalid JSON"));
    }

    #[test]
    fn external_host_receives_bounds_and_cancellation_and_propagates_output_limit() {
        let observed_cancelled = Arc::new(AtomicBool::new(false));
        let expected_cancelled = Arc::clone(&observed_cancelled);
        let host: ProcessHost = Arc::new(move |request| {
            assert!(Arc::ptr_eq(&request.cancelled, &expected_cancelled));
            assert_eq!(request.max_output_bytes, 64);
            assert_eq!(request.deadline, Duration::from_millis(25));
            Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "external output exceeded the retained-output limit",
            )
            .with_help("Reduce external output before crossing into data mode"))
        });
        let runtime = DataRuntime::with_limits_and_process_host(
            DataLimits {
                external_deadline: Duration::from_millis(25),
                max_external_output_bytes: 64,
                ..DataLimits::DEFAULT
            },
            host,
        );
        let error = match runtime
            .eval_output_with_cancellation_handle("^external hostile", observed_cancelled)
        {
            Ok(_) => panic!("bounded host must reject hostile output"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }
}
