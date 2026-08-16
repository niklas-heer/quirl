//! Quirl's bounded native structured-data runtime.
//!
//! This crate deliberately owns values, streams, result envelopes, and rendering
//! without depending on the UI or CLI layers. Adapters are explicit: JSON,
//! YAML, TOML, CSV, POSIX tar headers, and filesystem rows become structured
//! values at `open`/`files`.

pub mod syntax;

use quirl_core::{
    directory_entries, escape_terminal_controls, ErrorCode, ProcessHost, ProcessRequest,
    ShellError, StructuredValue,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering as AtomicOrdering},
        Arc,
    },
    time::Duration,
};
use syntax::{
    BooleanOperator as SyntaxBooleanOperator, ComparisonOperator as SyntaxComparisonOperator,
    DataPredicate as SyntaxPredicate, DataSource, DataSyntaxDiagnostic, DataSyntaxDiagnosticKind,
    DataSyntaxLimits, DataTransform, SortDirection, Spanned,
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
    /// Wrap a JSON-compatible value in a typed value envelope.
    pub fn value(value: Value) -> Self {
        Self::Value {
            value: DataValue::from_json(value),
        }
    }
    /// Wrap materialized JSON-compatible rows in a stream envelope.
    pub fn stream(items: Vec<Value>) -> Self {
        Self::Stream {
            items: items.into_iter().map(DataValue::from_json).collect(),
        }
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
    /// Render this envelope in the selected stable human or machine format.
    pub fn render(&self, format: DataRenderFormat) -> Result<String, ShellError> {
        render_envelope(self, format)
    }
}

/// Output representation selected at a data-rendering boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataRenderFormat {
    /// Stable typed JSON suitable for machine consumers.
    Json,
    /// Terminal-safe line-oriented text.
    Plain,
    /// A human-readable table that collects streams within their configured row bound.
    Table,
}

/// A pull-based stream. Calling `next` performs at most one row of work and
/// checks cancellation before consuming it. Sources and consumers share the
/// configured row budget.
type StreamPull = dyn FnMut(&AtomicBool) -> Result<Option<Value>, ShellError> + Send;

/// A bounded, pull-based sequence of JSON-compatible rows.
pub struct DataStream {
    pull: Box<StreamPull>,
    emitted: usize,
    limits: DataLimits,
}

impl DataStream {
    fn from_values(values: Vec<Value>, limits: DataLimits) -> Self {
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
        iterator: impl Iterator<Item = Result<Value, ShellError>> + Send + 'static,
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
                let remaining = &bytes[offset..];
                let (line, consumed) = match remaining.find('\n') {
                    Some(index) => (&remaining[..index], index + 1),
                    None => (remaining, remaining.len()),
                };
                offset += consumed;
                Ok(Some(Value::String(
                    line.strip_suffix('\r').unwrap_or(line).to_owned(),
                )))
            },
            limits,
        )
    }

    fn from_pull(
        pull: impl FnMut(&AtomicBool) -> Result<Option<Value>, ShellError> + Send + 'static,
        limits: DataLimits,
    ) -> Self {
        Self {
            pull: Box::new(pull),
            emitted: 0,
            limits,
        }
    }

    fn map(self, transform: impl Fn(Value) -> Result<Value, ShellError> + Send + 'static) -> Self {
        let limits = self.limits();
        let mut source = self;
        Self::from_pull(
            move |cancelled| source.next(cancelled)?.map(&transform).transpose(),
            limits,
        )
    }

    fn filter(
        self,
        predicate: impl Fn(&Value) -> Result<bool, ShellError> + Send + 'static,
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
                    remaining -= 1;
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
    pub fn next(&mut self, cancelled: &AtomicBool) -> Result<Option<Value>, ShellError> {
        check_cancelled(cancelled)?;
        if self.emitted == self.limits.max_rows {
            return match (self.pull)(cancelled)? {
                None => Ok(None),
                Some(_) => Err(limit_error(
                    "stream row limit exceeded",
                    "Use `take <count>` or raise the configured data row limit",
                )),
            };
        }
        let value = (self.pull)(cancelled)?;
        if value.is_some() {
            self.emitted += 1;
        }
        Ok(value)
    }

    /// Consume the stream into memory within its configured row bound.
    pub fn collect(mut self, cancelled: &AtomicBool) -> Result<Vec<Value>, ShellError> {
        let mut values = Vec::new();
        while let Some(value) = self.next(cancelled)? {
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
    Value(Value),
    /// A pull-based stream whose rows have not necessarily been read yet.
    Stream(DataStream),
}

impl DataOutput {
    fn into_value(self, cancelled: &AtomicBool) -> Result<Value, ShellError> {
        match self {
            Self::Value(value) => Ok(value),
            Self::Stream(stream) => stream.collect(cancelled).map(Value::Array),
        }
    }

    /// Materialize this output into the stable bounded data envelope.
    ///
    /// Live streams are collected only at this explicit boundary and remain
    /// subject to their configured row, field, depth, and retained-value limits.
    pub fn into_envelope(self, cancelled: &AtomicBool) -> Result<DataEnvelope, ShellError> {
        match self {
            Self::Value(value) => Ok(DataEnvelope::value(value)),
            // The JSON envelope is an explicit machine boundary, so it owns the
            // materialization rather than making a producer silently collect.
            Self::Stream(stream) => Ok(DataEnvelope::stream(stream.collect(cancelled)?)),
        }
    }

    /// Render this output into an owned UTF-8 string.
    ///
    /// This convenience boundary buffers the rendered bytes. Stream rows are
    /// still pulled individually, while table rendering materializes rows
    /// within the stream's configured limit.
    pub fn render(
        self,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
    ) -> Result<String, ShellError> {
        let mut output = Vec::new();
        self.render_to(format, cancelled, &mut output)?;
        String::from_utf8(output).map_err(|error| {
            ShellError::new(ErrorCode::Data, "data renderer produced invalid UTF-8")
                .with_context(error.to_string())
                .with_help(
                    "Use a UTF-8 terminal renderer or report this internal data renderer error",
                )
        })
    }

    /// Render to an output sink. Plain and JSON streams write each row as it is
    /// pulled; callers that need a single `String` can use `render`, which is
    /// the intentionally collecting convenience boundary.
    pub fn render_to(
        self,
        format: DataRenderFormat,
        cancelled: &AtomicBool,
        writer: &mut impl Write,
    ) -> Result<(), ShellError> {
        match self {
            Self::Value(value) => {
                write_rendered(writer, &DataEnvelope::value(value).render(format)?)
            }
            Self::Stream(stream) => render_stream_to(stream, format, cancelled, writer),
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
        let syntax_limits = syntax_limits(self.limits);
        let expression = syntax::parse_data_expression(source, syntax_limits)
            .map_err(|diagnostic| syntax_shell_error(source, diagnostic))?;
        check_cancelled(cancelled)?;
        let mut output = evaluate_source_output(
            &expression.source,
            self.limits,
            self.process_host.as_ref(),
            cancellation_handle,
        )?;
        for transform in &expression.transforms {
            check_cancelled(cancelled)?;
            output = apply_output_transform(output, transform, self.limits, cancelled)?;
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
            .render(format, cancelled)
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
            .render_to(format, cancelled, writer)
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
            .render_to(format, &cancelled, writer)
    }

    /// Open a stream without collecting it. CSV rows are parsed only as the
    /// caller pulls them. Other bounded adapters validate before exposing rows.
    pub fn open_stream(&self, path: impl AsRef<Path>) -> Result<DataStream, ShellError> {
        let path = path.as_ref();
        match extension(path).as_deref() {
            Some("csv") => open_csv_stream(path, self.limits),
            Some("tar") => open_tar_stream(path, self.limits),
            _ => match self.open_value(path)? {
                Value::Array(values) => Ok(DataStream::from_values(values, self.limits)),
                value => Ok(DataStream::from_values(vec![value], self.limits)),
            },
        }
    }

    /// Evaluate `source` and collect its result into a JSON-compatible value.
    pub fn eval(&self, source: &str) -> Result<Value, ShellError> {
        let cancelled = Arc::new(AtomicBool::new(false));
        self.eval_output_with_token(source, &cancelled, Some(Arc::clone(&cancelled)))?
            .into_value(&cancelled)
    }

    /// Evaluate and collect a result while observing cancellation.
    pub fn eval_with_cancellation(
        &self,
        source: &str,
        cancelled: &AtomicBool,
    ) -> Result<Value, ShellError> {
        self.eval_output_with_cancellation(source, cancelled)?
            .into_value(cancelled)
    }

    fn open_value(&self, path: &Path) -> Result<Value, ShellError> {
        match extension(path).as_deref() {
            Some("csv") => return Ok(Value::Array(read_csv(path, self.limits)?)),
            Some("tar") => return Ok(Value::Array(read_tar(path, self.limits)?)),
            _ => {}
        }
        let contents = read_bounded_utf8(path, self.limits.max_file_bytes)?;
        let value = match extension(path).as_deref() {
            Some("json") => serde_json::from_str(&contents).map_err(|error| {
                ShellError::new(
                    ErrorCode::Data,
                    format!("cannot parse JSON in {}", path.display()),
                )
                .with_context(error.to_string())
                .with_help("Correct the JSON syntax or use a .toml/.csv adapter")
            })?,
            Some("toml") => {
                let parsed: toml::Value = toml::from_str(&contents).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Data,
                        format!("cannot parse TOML in {}", path.display()),
                    )
                    .with_context(error.to_string())
                    .with_help("Correct the TOML syntax before opening the file")
                })?;
                serde_json::to_value(parsed).map_err(|error| {
                    ShellError::new(ErrorCode::Data, "cannot convert TOML into a typed value")
                        .with_context(error.to_string())
                        .with_help("Use TOML values supported by the Quirl data adapter")
                })?
            }
            Some("yaml") | Some("yml") => {
                serde_yaml_ng::from_str::<Value>(&contents).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Data,
                        format!("cannot parse YAML in {}", path.display()),
                    )
                    .with_context(error.to_string())
                    .with_help("Correct the YAML syntax before opening the file")
                })?
            }
            _ => Value::String(contents),
        };
        validate_value(&value, self.limits)?;
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

fn evaluate_source(source: &Spanned<DataSource>, limits: DataLimits) -> Result<Value, ShellError> {
    match &source.value {
        DataSource::Pwd => std::env::current_dir()
            .map(|path| Value::String(path.display().to_string()))
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "cannot read the current directory")
                    .with_context(error.to_string())
                    .with_help("Check that the current directory still exists and is accessible")
            }),
        DataSource::Files { path } => {
            let path = path
                .as_ref()
                .map_or_else(|| PathBuf::from("."), |path| PathBuf::from(&path.value));
            let entries = directory_entries(&path, false)?;
            if entries.len() > limits.max_rows {
                return Err(limit_error(
                    "filesystem row limit exceeded",
                    "Select a narrower directory or raise the configured data row limit",
                ));
            }
            serde_json::to_value(entries).map_err(|error| {
                ShellError::new(ErrorCode::Data, "cannot represent directory entries")
                    .with_context(error.to_string())
                    .with_help("Try a directory whose entries can be represented as typed values")
            })
        }
        DataSource::Open { path } => {
            let path = PathBuf::from(&path.value);
            DataRuntime::with_limits(limits).open_value(&path)
        }
        DataSource::Literal(literal) => {
            let value = literal.to_json();
            validate_value(&value, limits)?;
            Ok(value)
        }
        DataSource::External { .. } => Err(data_error(
            "^external",
            "external sources require the explicit process-host output boundary",
        )),
    }
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
        return Ok(DataOutput::Value(Value::String(
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
                    Value::Array(rows) => {
                        Ok(DataOutput::Stream(DataStream::from_values(rows, limits)))
                    }
                    value => Ok(DataOutput::Value(value)),
                },
            }
        }
        DataSource::Files { .. } => {
            let Value::Array(rows) = evaluate_source(source, limits)? else {
                return Err(ShellError::new(
                    ErrorCode::Data,
                    "filesystem source did not produce directory rows",
                )
                .with_help("Use `files [path]` to produce directory-entry records"));
            };
            Ok(DataOutput::Stream(DataStream::from_values(rows, limits)))
        }
        DataSource::Pwd | DataSource::Literal(_) => match evaluate_source(source, limits)? {
            Value::Array(rows) => Ok(DataOutput::Stream(DataStream::from_values(rows, limits))),
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
    if let DataOutput::Value(value) = output {
        match &transform.value {
            DataTransform::Lines => {
                let Value::String(bytes) = value else {
                    return Err(data_error("lines", "lines expects a string byte value"));
                };
                return Ok(DataOutput::Stream(DataStream::from_lines(bytes, limits)));
            }
            DataTransform::FromJson => {
                let Value::String(bytes) = value else {
                    return Err(data_error(
                        "from json",
                        "from json expects a string byte value",
                    ));
                };
                return match parse_json_boundary(&bytes, "from json", limits)? {
                    Value::Array(rows) => {
                        Ok(DataOutput::Stream(DataStream::from_values(rows, limits)))
                    }
                    value => Ok(DataOutput::Value(value)),
                };
            }
            DataTransform::ToJson => {
                return Ok(DataOutput::Value(Value::String(to_json_boundary(
                    &value, "to json",
                )?)));
            }
            _ => {
                let value = apply_transform(value, &transform.value)?;
                validate_value(&value, limits)?;
                return Ok(DataOutput::Value(value));
            }
        }
    }
    let DataOutput::Stream(mut stream) = output else {
        unreachable!();
    };
    match &transform.value {
        DataTransform::Lines => Err(data_error(
            "lines",
            "lines expects one string value; apply it immediately after ^external or from json",
        )),
        DataTransform::FromJson => Ok(DataOutput::Stream(stream.map(move |value| {
            let Value::String(bytes) = value else {
                return Err(data_error(
                    "from json",
                    "from json expects string stream items",
                ));
            };
            parse_json_boundary(&bytes, "from json", limits)
        }))),
        DataTransform::ToJson => {
            Ok(DataOutput::Stream(stream.map(move |value| {
                to_json_boundary(&value, "to json").map(Value::String)
            })))
        }
        DataTransform::Where(predicate) => {
            let predicate = predicate.clone();
            Ok(DataOutput::Stream(stream.filter(move |row| {
                if !row.is_object() {
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
        DataTransform::First => Ok(DataOutput::Value(
            stream.next(cancelled)?.unwrap_or(Value::Null),
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
            Ok(DataOutput::Value(Value::from(length)))
        }
        DataTransform::Sort { field, direction } => {
            // Sorting is intentionally a collection boundary. The row limit is
            // still enforced by `collect`, and callers can make it explicit
            // with `take` before sorting.
            let rows = stream.collect(cancelled)?;
            let sorted = sort_rows(Value::Array(rows), &field.value, *direction, "sort")?;
            let Value::Array(rows) = sorted else {
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
            let remaining = self.limit - self.consumed;
            let allowed = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(buffer.len());
            let read = self.inner.read(&mut buffer[..allowed])?;
            self.consumed += u64::try_from(read).unwrap_or(u64::MAX);
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

fn bounded_io_error(action: &str, path: &Path, error: std::io::Error) -> ShellError {
    if is_file_size_limit(&error) {
        limit_error(
            format!("{} exceeds the file-size limit", path.display()),
            "Increase the data file limit or select a smaller input file",
        )
    } else {
        io_error(action, path, error)
    }
}

fn read_bounded_utf8(path: &Path, limit: u64) -> Result<String, ShellError> {
    let mut reader = open_bounded_file(path, limit)?;
    let mut contents = String::new();
    reader
        .read_to_string(&mut contents)
        .map_err(|error| bounded_io_error("read", path, error))?;
    Ok(contents)
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
        return Err(limit_error(
            format!("{} exceeds the file-size limit", path.display()),
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

fn validate_value(value: &Value, limits: DataLimits) -> Result<(), ShellError> {
    fn visit(value: &Value, depth: usize, limits: DataLimits) -> Result<(), ShellError> {
        if depth > limits.max_depth {
            return Err(limit_error(
                "structured value exceeds the nesting-depth limit",
                "Flatten the input or raise the configured data depth limit",
            ));
        }
        match value {
            Value::Array(values) => {
                if values.len() > limits.max_rows {
                    return Err(limit_error(
                        "structured value exceeds the row limit",
                        "Use `take <count>` or raise the configured data row limit",
                    ));
                }
                for value in values {
                    visit(value, depth + 1, limits)?;
                }
            }
            Value::Object(values) => {
                if values.len() > limits.max_fields {
                    return Err(limit_error(
                        "record exceeds the field limit",
                        "Select fewer fields or raise the configured data field limit",
                    ));
                }
                for value in values.values() {
                    visit(value, depth + 1, limits)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(value, 0, limits)
}

fn read_csv(path: &Path, limits: DataLimits) -> Result<Vec<Value>, ShellError> {
    let stream = open_csv_stream(path, limits)?;
    stream.collect(&AtomicBool::new(false))
}

fn open_csv_stream(path: &Path, limits: DataLimits) -> Result<DataStream, ShellError> {
    let mut lines = BufReader::new(open_bounded_file(path, limits.max_file_bytes)?).lines();
    let header = lines
        .next()
        .transpose()
        .map_err(|error| bounded_io_error("read", path, error))?
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::Data,
                format!("CSV file {} has no header row", path.display()),
            )
            .with_help("Add a header row with one unique name per column")
        })?;
    let headers = parse_csv_record(&header).map_err(|message| csv_error(path, 1, message))?;
    validate_csv_headers(&headers, limits, path)?;
    let display = path.display().to_string();
    let source_path = path.to_path_buf();
    let iterator = lines.enumerate().map(move |(index, line)| {
        let line_number = index + 2;
        let line = line.map_err(|error| bounded_io_error("read", &source_path, error))?;
        let fields = parse_csv_record(&line)
            .map_err(|message| csv_error_display(&display, line_number, message))?;
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
            .map(|(key, value)| (key, Value::String(value)))
            .collect();
        Ok(Value::Object(row))
    });
    Ok(DataStream::from_iterator(iterator, limits))
}

fn validate_csv_headers(
    headers: &[String],
    limits: DataLimits,
    path: &Path,
) -> Result<(), ShellError> {
    if headers.is_empty() || headers.len() > limits.max_fields {
        return Err(csv_error(
            path,
            1,
            format!("header must contain 1..={} fields", limits.max_fields),
        ));
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

fn parse_csv_record(line: &str) -> Result<Vec<String>, String> {
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
                    fields.push(std::mem::take(&mut current));
                    after_quote = false;
                }
                _ => return Err("characters after a closing quote must be a comma".to_owned()),
            }
        } else {
            match character {
                ',' => fields.push(std::mem::take(&mut current)),
                '"' if current.is_empty() => quoted = true,
                '"' => return Err("quote must begin a field".to_owned()),
                _ => current.push(character),
            }
        }
    }
    if quoted {
        return Err("unclosed quoted field (multiline CSV fields are unsupported)".to_owned());
    }
    fields.push(current);
    Ok(fields)
}

fn csv_error(path: &Path, line: usize, message: impl Into<String>) -> ShellError {
    csv_error_display(&path.display().to_string(), line, message)
}

fn csv_error_display(path: &str, line: usize, message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Data, format!("invalid CSV at {path}:{line}"))
        .with_context(message.into())
        .with_help("Use a header row, balanced quotes, and the same field count on every row")
}

fn read_tar(path: &Path, limits: DataLimits) -> Result<Vec<Value>, ShellError> {
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
                let count = reader
                    .read(&mut header[read..])
                    .map_err(|error| bounded_io_error("read", &source_path, error))?;
                if count == 0 {
                    if read == 0 {
                        finished = true;
                        return Ok(None);
                    }
                    return Err(tar_error_display(&display, "truncated header"));
                }
                read += count;
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
                    ))
                }
                other => {
                    return Err(tar_error_display(
                        &display,
                        format!("unsupported entry type byte {other}"),
                    ))
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
                let wanted =
                    usize::try_from(remaining.min(discard.len() as u64)).map_err(|_| {
                        tar_error_display(&display, "entry size cannot be represented on this host")
                    })?;
                let count = reader
                    .read(&mut discard[..wanted])
                    .map_err(|error| bounded_io_error("read", &source_path, error))?;
                if count == 0 {
                    return Err(tar_error_display(&display, "entry payload is truncated"));
                }
                remaining -= u64::try_from(count).map_err(|_| {
                    tar_error_display(&display, "read size cannot be represented on this host")
                })?;
            }
            Ok(Some(serde_json::json!({
                "path": path,
                "kind": kind,
                "size": size,
            })))
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
    std::str::from_utf8(&bytes[..end])
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

fn render_envelope(
    envelope: &DataEnvelope,
    format: DataRenderFormat,
) -> Result<String, ShellError> {
    match format {
        DataRenderFormat::Json => serde_json::to_string_pretty(envelope)
            .map(|value| format!("{value}\n"))
            .map_err(|error| {
                ShellError::new(ErrorCode::Data, "cannot render typed data as JSON")
                    .with_context(error.to_string())
                    .with_help("Use `--format plain` for terminal-only output")
            }),
        DataRenderFormat::Plain => Ok(format!(
            "{}\n",
            escape_terminal_controls(&plain_value(envelope))
        )),
        DataRenderFormat::Table => Ok(render_table(envelope)),
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
    match format {
        DataRenderFormat::Table => {
            write_rendered(writer, &render_table_rows(&stream.collect(cancelled)?))
        }
        DataRenderFormat::Plain => {
            while let Some(value) = stream.next(cancelled)? {
                write_rendered(
                    writer,
                    &format!("{}\n", escape_terminal_controls(&plain_json_value(&value))),
                )?;
            }
            Ok(())
        }
        DataRenderFormat::Json => {
            write_rendered(writer, "{\n  \"kind\": \"stream\",\n  \"items\": [")?;
            let mut first = true;
            while let Some(value) = stream.next(cancelled)? {
                let rendered =
                    serde_json::to_string(&DataValue::from_json(value)).map_err(|error| {
                        ShellError::new(ErrorCode::Data, "cannot serialize stream value as JSON")
                            .with_context(error.to_string())
                            .with_help("Use `--format plain` for terminal-only output")
                    })?;
                if first {
                    write_rendered(writer, "\n")?;
                    first = false;
                } else {
                    write_rendered(writer, ",\n")?;
                }
                write_rendered(writer, "    ")?;
                write_rendered(writer, &rendered)?;
            }
            if !first {
                write_rendered(writer, "\n")?;
            }
            write_rendered(writer, "  ]\n}\n")
        }
    }
}

fn write_rendered(writer: &mut impl Write, text: &str) -> Result<(), ShellError> {
    writer.write_all(text.as_bytes()).map_err(|error| {
        ShellError::new(ErrorCode::Io, "cannot write data output")
            .with_context(error.to_string())
            .with_help(
                "Check that the output destination is writable or consume the complete output",
            )
    })
}

fn plain_json_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        value => value.to_string(),
    }
}

fn plain_value(envelope: &DataEnvelope) -> String {
    match envelope {
        DataEnvelope::Value { value } => value.display_value(),
        DataEnvelope::Stream { items } => items
            .iter()
            .map(DataValue::display_value)
            .collect::<Vec<_>>()
            .join("\n"),
        DataEnvelope::Option { value } => value
            .as_deref()
            .map_or_else(|| "none".to_owned(), plain_value),
        DataEnvelope::Result {
            state,
            value,
            error,
        } => value.as_deref().map_or_else(
            || {
                error.as_ref().map_or_else(
                    || format!("result {state:?}"),
                    |error| format!("error: {}", error.message),
                )
            },
            plain_value,
        ),
        DataEnvelope::Task {
            state,
            value,
            error,
        } => value.as_deref().map_or_else(
            || {
                error.as_ref().map_or_else(
                    || format!("task {state:?}"),
                    |error| format!("task {state:?}: {}", error.message),
                )
            },
            |value| format!("task {state:?}: {}", plain_value(value)),
        ),
    }
}

fn render_table(envelope: &DataEnvelope) -> String {
    let value = match envelope {
        DataEnvelope::Value { value } => value,
        DataEnvelope::Stream { items } => {
            let rows = items.iter().map(DataValue::json_value).collect::<Vec<_>>();
            return render_table_rows(&rows);
        }
        DataEnvelope::Option { value }
        | DataEnvelope::Result { value, .. }
        | DataEnvelope::Task { value, .. } => {
            return value
                .as_deref()
                .map_or_else(|| format!("{}\n", plain_value(envelope)), render_table)
        }
    };
    match value.json_value() {
        Value::Array(rows) => render_table_rows(&rows),
        Value::Object(row) => render_table_rows(&[Value::Object(row)]),
        _ => format!("{}\n", escape_terminal_controls(&value.display_value())),
    }
}

fn render_table_rows(rows: &[Value]) -> String {
    let mut columns = BTreeSet::new();
    for row in rows {
        if let Value::Object(row) = row {
            columns.extend(row.keys().cloned());
        }
    }
    if columns.is_empty() {
        return rows
            .iter()
            .map(|value| format!("{}\n", escape_terminal_controls(&value.to_string())))
            .collect();
    }
    let columns: Vec<_> = columns.into_iter().collect();
    let mut output = format!("{}\n", columns.join("\t"));
    for row in rows {
        let Value::Object(row) = row else {
            output.push_str(&format!("{}\n", escape_terminal_controls(&row.to_string())));
            continue;
        };
        let cells = columns
            .iter()
            .map(|column| {
                row.get(column).map_or_else(String::new, |value| {
                    escape_terminal_controls(&value.to_string())
                })
            })
            .collect::<Vec<_>>();
        output.push_str(&format!("{}\n", cells.join("\t")));
    }
    output
}

fn apply_transform(value: Value, transform: &DataTransform) -> Result<Value, ShellError> {
    match transform {
        DataTransform::Length => match value {
            Value::Array(values) => Ok(Value::from(values.len())),
            Value::Object(values) => Ok(Value::from(values.len())),
            Value::String(value) => Ok(Value::from(value.chars().count())),
            _ => Err(data_error(
                "length",
                "length expects an array, object, or string",
            )),
        },
        DataTransform::First => match value {
            Value::Array(values) => Ok(values.into_iter().next().unwrap_or(Value::Null)),
            _ => Err(data_error("first", "first expects an array")),
        },
        DataTransform::Get { path } => get_field(value, &path.value, "get"),
        DataTransform::Where(predicate) => filter_where(value, predicate, "where"),
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

fn get_field(value: Value, field: &str, stage: &str) -> Result<Value, ShellError> {
    match value {
        Value::Object(object) => get_path(&Value::Object(object), field)
            .cloned()
            .ok_or_else(|| data_error(stage, format!("object has no field `{field}`"))),
        Value::Array(values) => values
            .into_iter()
            .map(|value| {
                if !value.is_object() {
                    return Err(data_error(stage, "get over an array expects object rows"));
                }
                get_path(&value, field)
                    .cloned()
                    .ok_or_else(|| data_error(stage, format!("row has no field `{field}`")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Err(data_error(
            stage,
            "get expects an object or array of objects",
        )),
    }
}

fn filter_where(
    value: Value,
    predicate: &SyntaxPredicate,
    stage: &str,
) -> Result<Value, ShellError> {
    let Value::Array(values) = value else {
        return Err(data_error(stage, "where expects an array of objects"));
    };

    let mut filtered = Vec::new();
    for value in values {
        if !value.is_object() {
            return Err(data_error(stage, "where expects object rows"));
        }
        if predicate_matches(predicate, &value, stage)? {
            filtered.push(value);
        }
    }
    Ok(Value::Array(filtered))
}

fn predicate_matches(
    predicate: &SyntaxPredicate,
    row: &Value,
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
        match operator.value {
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
    condition: &syntax::DataCondition,
    row: &Value,
    stage: &str,
) -> Result<bool, ShellError> {
    let Some(actual) = get_path(row, &condition.field.value) else {
        return Ok(false);
    };
    let expected = condition.expected.to_json();
    match condition.comparison.value {
        SyntaxComparisonOperator::Equal => Ok(actual == &expected),
        SyntaxComparisonOperator::NotEqual => Ok(actual != &expected),
        SyntaxComparisonOperator::Less => {
            Ok(compare_values(actual, &expected, stage)? == Ordering::Less)
        }
        SyntaxComparisonOperator::LessOrEqual => Ok(matches!(
            compare_values(actual, &expected, stage)?,
            Ordering::Less | Ordering::Equal
        )),
        SyntaxComparisonOperator::Greater => {
            Ok(compare_values(actual, &expected, stage)? == Ordering::Greater)
        }
        SyntaxComparisonOperator::GreaterOrEqual => Ok(matches!(
            compare_values(actual, &expected, stage)?,
            Ordering::Greater | Ordering::Equal
        )),
    }
}

fn compare_values(left: &Value, right: &Value, stage: &str) -> Result<Ordering, ShellError> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
                return Ok(left.cmp(&right));
            }
            if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
                return Ok(left.cmp(&right));
            }
            if let (Some(left), Some(right)) = (left.as_i64(), right.as_u64()) {
                return Ok(if left < 0 {
                    Ordering::Less
                } else {
                    (left as u64).cmp(&right)
                });
            }
            if let (Some(left), Some(right)) = (left.as_u64(), right.as_i64()) {
                return Ok(if right < 0 {
                    Ordering::Greater
                } else {
                    left.cmp(&(right as u64))
                });
            }
            left.as_f64()
                .and_then(|left| right.as_f64().and_then(|right| left.partial_cmp(&right)))
                .ok_or_else(|| data_error(stage, "numbers cannot be ordered"))
        }
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
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

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |value, field| value.as_object()?.get(field))
}

fn sort_rows(
    value: Value,
    field: &str,
    direction: SortDirection,
    stage: &str,
) -> Result<Value, ShellError> {
    let descending = direction == SortDirection::Descending;
    let Value::Array(mut values) = value else {
        return Err(data_error(stage, "sort expects an array of objects"));
    };
    for value in &values {
        if !value.is_object() {
            return Err(data_error(stage, "sort expects object rows"));
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
    Ok(Value::Array(values))
}

fn take_values(value: Value, count: u64, stage: &str) -> Result<Value, ShellError> {
    let count = usize::try_from(count).map_err(|_| {
        limit_error(
            "take count exceeds the platform index range",
            "Use a smaller non-negative count",
        )
    })?;
    let Value::Array(mut values) = value else {
        return Err(data_error(stage, "take expects an array"));
    };
    values.truncate(count);
    Ok(Value::Array(values))
}

fn select_fields(value: Value, fields: &[String], stage: &str) -> Result<Value, ShellError> {
    fn select(object: Map<String, Value>, fields: &[String]) -> Map<String, Value> {
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
        Value::Object(object) => Ok(Value::Object(select(object, fields))),
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::Object(object) => Ok(Value::Object(select(object, fields))),
                _ => Err(data_error(stage, "select expects object rows")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Err(data_error(
            stage,
            "select expects an object or array of objects",
        )),
    }
}

fn parse_json_boundary(bytes: &str, stage: &str, limits: DataLimits) -> Result<Value, ShellError> {
    if bytes.len() > limits.max_source_bytes {
        return Err(limit_error(
            "JSON byte value exceeds the data source-size limit",
            "Use a bounded external command or raise the configured data source limit",
        ));
    }
    let value = serde_json::from_str(bytes).map_err(|error| {
        data_error(stage, format!("from json received invalid JSON: {error}"))
            .with_help("Ensure the byte producer emits one valid JSON document")
    })?;
    validate_value(&value, limits)?;
    Ok(value)
}

fn to_json_boundary(value: &Value, stage: &str) -> Result<String, ShellError> {
    serde_json::to_string(value).map_err(|error| {
        data_error(stage, "to json cannot encode this value")
            .with_context(error.to_string())
            .with_help("Select JSON-compatible fields before crossing the byte boundary")
    })
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

fn syntax_shell_error(source: &str, diagnostic: DataSyntaxDiagnostic) -> ShellError {
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
        .with_context(format!("expression bytes: {}", source.len()))
        .with_help(diagnostic.help)
}

fn data_error(source: &str, message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Data, "invalid data expression")
        .with_context(message)
        .with_label(None, 0, source.len(), "could not evaluate this stage")
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
        header[124..124 + size.len()].copy_from_slice(size.as_bytes());
        header[156] = b'0';
        header[148..156].fill(b' ');
        let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
        let checksum = format!("{checksum:06o}\0 ");
        header[148..156].copy_from_slice(checksum.as_bytes());
        let mut archive = header.to_vec();
        archive.extend_from_slice(contents);
        archive.resize(512 + contents.len().div_ceil(512) * 512, 0);
        archive
    }

    struct CancelAfterFirstWrite<'a> {
        cancelled: &'a AtomicBool,
        writes: usize,
        output: Vec<u8>,
    }

    impl Write for CancelAfterFirstWrite<'_> {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes += 1;
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

    #[test]
    fn transforms_structured_rows_without_stringification() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval(
                r#"[{"name":"api","status":"up"},{"name":"db","status":"down"}]
                   | where status == "down" | select name"#,
            )
            .unwrap();
        assert_eq!(value, serde_json::json!([{"name": "db"}]));
    }

    #[test]
    fn pipes_inside_json_strings_do_not_split_the_pipeline() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime.eval(r#"{"value":"a|b"} | get value"#).unwrap(),
            "a|b"
        );
    }

    #[test]
    fn length_preserves_a_numeric_value() {
        let runtime = DataRuntime::new();
        assert_eq!(runtime.eval("[1,2,3] | length").unwrap(), 3);
    }

    #[test]
    fn filters_sorts_and_limits_rows_with_the_documented_grammar() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval(
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
            serde_json::json!([
                {"name": "old-largest", "size": 1100},
                {"name": "old-large", "size": 700}
            ])
        );
    }

    #[test]
    fn where_supports_all_comparisons_and_and_before_or_precedence() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval(
                r#"[
                    {"name":"a","score":1,"enabled":true},
                    {"name":"b","score":2,"enabled":false},
                    {"name":"c","score":3,"enabled":true},
                    {"name":"d","score":4,"enabled":true}
                ] | where score >= 2 and score < 4 or name != "d" and enabled == true
                  | get name"#,
            )
            .unwrap();
        assert_eq!(value, serde_json::json!(["a", "b", "c"]));

        assert_eq!(
            runtime
                .eval(r#"[{"n":1},{"n":2},{"n":3}] | where n <= 2 and n > 1"#)
                .unwrap(),
            serde_json::json!([{"n": 2}])
        );
    }

    #[test]
    fn quoted_predicate_values_remain_strings() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval(r#"[{"value":"42"},{"value":42}] | where value == "42""#)
                .unwrap(),
            serde_json::json!([{"value": "42"}])
        );
        assert_eq!(
            runtime
                .eval(r#"[{"value":"a and b"},{"value":"a"}] | where value == 'a and b'"#)
                .unwrap(),
            serde_json::json!([{"value": "a and b"}])
        );
    }

    #[test]
    fn nested_fields_work_for_get_where_and_sort() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval(
                    r#"[{"user":{"name":"Ada","rank":2}},{"user":{"name":"Lin","rank":1}}]
                       | where user.rank != 3 | sort user.rank | get user.name"#,
                )
                .unwrap(),
            serde_json::json!(["Lin", "Ada"])
        );
    }

    #[test]
    fn malformed_predicates_and_incomparable_sorts_are_errors() {
        let runtime = DataRuntime::new();
        assert!(runtime.eval(r#"[{"value":1}] | where value = 1"#).is_err());
        assert!(runtime
            .eval(r#"[{"value":1},{"value":"one"}] | sort value"#)
            .is_err());
        assert!(runtime.eval("[1,2 | length").is_err());
    }

    #[test]
    fn cancellation_stops_between_typed_pipeline_stages() {
        let cancelled = AtomicBool::new(true);
        let error = DataRuntime::new()
            .eval_with_cancellation("[1,2,3] | length", &cancelled)
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
                .eval(&format!(
                    "open {} | get service.name",
                    toml.path().display()
                ))
                .unwrap(),
            "api"
        );
        let mut stream = runtime.open_stream(&csv).unwrap();
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(serde_json::json!({"name": "api", "enabled": "true"}))
        );
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(serde_json::json!({"name": "worker", "enabled": "false"}))
        );
        assert_eq!(stream.next(&AtomicBool::new(false)).unwrap(), None);
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
                .eval(&format!(
                    "open {} | get service.name",
                    yaml.path().display()
                ))
                .unwrap(),
            "api"
        );
        let mut stream = runtime.open_stream(&tar).unwrap();
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(serde_json::json!({"path":"alpha.txt", "kind":"file", "size":5}))
        );
        assert_eq!(
            stream.next(&AtomicBool::new(false)).unwrap(),
            Some(serde_json::json!({"path":"nested/bravo.txt", "kind":"file", "size":5}))
        );
        assert_eq!(stream.next(&AtomicBool::new(false)).unwrap(), None);
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
            .render(DataRenderFormat::Plain, &AtomicBool::new(false))
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
    fn render_to_writes_rows_incrementally_and_honors_cancellation() {
        let cancelled = AtomicBool::new(false);
        let output = DataOutput::Stream(DataStream::from_values(
            vec![serde_json::json!("one"), serde_json::json!("two")],
            DataLimits::DEFAULT,
        ));
        let mut writer = CancelAfterFirstWrite {
            cancelled: &cancelled,
            writes: 0,
            output: Vec::new(),
        };
        let error = output
            .render_to(DataRenderFormat::Plain, &cancelled, &mut writer)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(writer.writes, 1);
        assert_eq!(writer.output, b"one\n");
    }

    #[test]
    fn render_to_reports_output_failures_as_shell_errors() {
        let mut writer = FailingWriter;
        let error = DataOutput::Value(serde_json::json!("value"))
            .render_to(
                DataRenderFormat::Plain,
                &AtomicBool::new(false),
                &mut writer,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert!(!error.details.help.is_empty());
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
            ..DataLimits::DEFAULT
        };
        let runtime = DataRuntime::with_limits(limits);
        assert_eq!(
            runtime.eval("[[1]]").unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            runtime.eval("[1,2]").unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn envelope_and_renderers_keep_machine_and_terminal_contracts_distinct() {
        let envelope = DataEnvelope::task(DataEnvelope::result(DataEnvelope::stream(vec![
            serde_json::json!({"name": "api", "port": 8080}),
        ])));
        let json = envelope.render(DataRenderFormat::Json).unwrap();
        assert!(json.contains("\"kind\": \"task\""));
        assert!(json.contains("\"state\": \"complete\""));
        assert!(json.contains("\"type\": \"string\""));
        let table = envelope.render(DataRenderFormat::Table).unwrap();
        assert_eq!(table, "name\tport\n\"api\"\t8080\n");
    }

    #[test]
    fn plain_rendering_of_nested_values_does_not_leak_abi_tags() {
        let output = DataEnvelope::value(serde_json::json!({
            "service": {"name": "api"},
            "ports": [8080, 8443],
        }))
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
            .eval(&format!("open {}", json.path().display()))
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
        assert!(none
            .render(DataRenderFormat::Json)
            .unwrap()
            .contains("\"option\""));
        let failure = DataEnvelope::result_error(ShellError::new(ErrorCode::Data, "bad input"));
        let json = failure.render(DataRenderFormat::Json).unwrap();
        assert!(json.contains("\"state\": \"error\""));
        assert!(json.contains("\"code\": \"data\""));
        assert!(DataEnvelope::pending_task()
            .render(DataRenderFormat::Plain)
            .unwrap()
            .contains("Pending"));
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
            runtime.eval("^external fixture | lines | take 1").unwrap(),
            serde_json::json!(["one"])
        );
        assert_eq!(
            DataRuntime::new()
                .eval(r#""{\"name\":\"api\"}" | from json | get name | to json"#)
                .unwrap(),
            serde_json::json!("\"api\"")
        );
    }

    #[test]
    fn external_sources_fail_closed_and_invalid_json_is_diagnostic() {
        let error = DataRuntime::new().eval("^external fixture").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!error.details.help.is_empty());

        let error = DataRuntime::new()
            .eval(r#""not json" | from json"#)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(error.details.context[0].contains("invalid JSON"));
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
