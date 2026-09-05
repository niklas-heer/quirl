//! Supervised out-of-process execution for every CLI-owned Lua VM.
//!
//! The parent owns the absolute deadline and process-tree lifetime. The worker
//! owns `lua_State` exclusively and never receives terminal handles. Frames are
//! compact JSON behind a fixed-width length prefix; both peers reject unknown
//! fields and over-limit payloads before allocation.

use crate::bounded_file::{ReadFileOptions, read_regular_file};
use quirl_core::{
    CommandOutcome, ContributionKind, ErrorCode, ExecutionCancellation, ExecutionCleanupState,
    ExecutionEffects, ExecutionInput, ExecutionOutcome, ExecutionOutput, ExecutionOutputTarget,
    ExecutionStatus, ExtensionAction, ExtensionEvent, ExtensionEventData, OutputStream,
    ProcessHost, ProcessRequest, ShellError,
};
use quirl_lua::{
    EventHandlerReport, LuaPolicy, LuaRunnerContext, LuaRuntime, MAX_LUA_SOURCE_BYTES,
    PluginRegistrations, QuirlConfig,
};
use quirl_process::{
    ContainedChild, DEFAULT_CAPTURE_BYTES, NATIVE_COMMAND_BYTES_MAX, isolated_process_host,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
    process::{ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const WORKER_ARGUMENT: &str = "--internal-lua-worker-v1";
const TEST_WORKER_ENV: &str = "QUIRL_INTERNAL_LUA_WORKER_TEST";
const TEST_WORKER_FAULT_ENV: &str = "QUIRL_INTERNAL_LUA_WORKER_FAULT";
const PROTOCOL_VERSION: u32 = 1;
const FRAME_BYTES_MAX: usize = MAX_LUA_SOURCE_BYTES + 1024 * 1024;
const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(2);
const HOST_CALLS_PER_REQUEST_MAX: u32 = 16;
const POLICY_MEMORY_BYTES_MAX: u64 = 64 * 1024 * 1024;
const POLICY_INSTRUCTIONS_MAX: u64 = 100_000_000;
const POLICY_WALL_TIME_MS_MAX: u64 = 60_000;
const ERROR_ITEMS_MAX: usize = 64;
const ERROR_TEXT_BYTES_MAX: usize = 256 * 1024;

pub(crate) const LUA_WORKER_PROTOCOL_DESCRIPTOR: &str = "quirl.lua-worker@1{frame:u32be-length<=5242880+json;request:{deny_unknown,version=1,id:u64,operation};response:{deny_unknown,version=1,id:u64,ok:bool,result:any,error?:ShellError{labels.start/end:u32}};host-call:{deny_unknown,request_id:u64,call_id:u32,command<=1048576,deadline_ms:u64,max_output_bytes<=1048576};event.output.bytes:u64;context.capture.max_bytes_per_stream:u64;test-count:u64;policy:{memory_bytes:1..=67108864,instructions:1..=100000000,wall_time_ms:1..=60000};host-calls-per-request<=16;timeout|cancel|protocol-failure:kill-tree+reap+poison;stdio:stdin+stderr-protocol,stdout=null,terminal:none}";
#[cfg(test)]
const REQUEST_FIXTURE_V1: &str = r#"{"kind":"request","request":{"version":1,"id":7,"operation":{"operation":"eval","source":"return 42"}}}"#;
#[cfg(test)]
const RESPONSE_FIXTURE_V1: &str =
    r#"{"kind":"response","response":{"version":1,"id":7,"ok":true,"result":42}}"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyWire {
    allow_process: bool,
    memory_limit_bytes: u64,
    instruction_limit: u64,
    wall_time_ms: u64,
}

impl PolicyWire {
    fn from_policy(policy: LuaPolicy) -> Result<Self, ShellError> {
        let wire = Self {
            allow_process: policy.allow_process,
            memory_limit_bytes: u64::try_from(policy.memory_limit_bytes).map_err(|error| {
                protocol_error(
                    "Lua policy memory limit is outside the protocol range",
                    error,
                )
            })?,
            instruction_limit: policy.instruction_limit,
            wall_time_ms: u64::try_from(policy.wall_time.as_millis()).map_err(|error| {
                protocol_error(
                    "Lua policy wall deadline is outside the protocol range",
                    error,
                )
            })?,
        };
        wire.validate()?;
        Ok(wire)
    }

    fn validate(&self) -> Result<(), ShellError> {
        if !(1..=POLICY_MEMORY_BYTES_MAX).contains(&self.memory_limit_bytes) {
            return Err(policy_limit_error(
                "memory bytes",
                self.memory_limit_bytes,
                POLICY_MEMORY_BYTES_MAX,
            ));
        }
        if !(1..=POLICY_INSTRUCTIONS_MAX).contains(&self.instruction_limit) {
            return Err(policy_limit_error(
                "instructions",
                self.instruction_limit,
                POLICY_INSTRUCTIONS_MAX,
            ));
        }
        if !(1..=POLICY_WALL_TIME_MS_MAX).contains(&self.wall_time_ms) {
            return Err(policy_limit_error(
                "wall time milliseconds",
                self.wall_time_ms,
                POLICY_WALL_TIME_MS_MAX,
            ));
        }
        Ok(())
    }

    fn to_policy(&self) -> Result<LuaPolicy, ShellError> {
        self.validate()?;
        Ok(LuaPolicy {
            allow_process: self.allow_process,
            memory_limit_bytes: usize::try_from(self.memory_limit_bytes).map_err(|error| {
                protocol_error("Lua policy memory limit is outside usize", error)
            })?,
            instruction_limit: self.instruction_limit,
            wall_time: Duration::from_millis(self.wall_time_ms),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextWire {
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    working_directory: String,
    input: ExecutionInput,
    output: ExecutionOutputTargetWire,
    declared_effects: ExecutionEffects,
}

impl ContextWire {
    fn from_context(context: &LuaRunnerContext) -> Result<Self, ShellError> {
        Ok(Self {
            arguments: context.arguments().to_vec(),
            environment: context.environment().clone(),
            working_directory: context.working_directory().to_owned(),
            input: context.input().clone(),
            output: ExecutionOutputTargetWire::from_target(context.output())?,
            declared_effects: context.declared_effects(),
        })
    }

    fn to_context(&self, cancellation: Arc<AtomicBool>) -> Result<LuaRunnerContext, ShellError> {
        LuaRunnerContext::new(
            self.arguments.clone(),
            self.environment.clone(),
            self.working_directory.clone(),
            self.input.clone(),
            self.output.to_target()?,
            self.declared_effects,
            cancellation,
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExecutionOutputTargetWire {
    Inherit,
    Capture { max_bytes_per_stream: u64 },
    Value,
}

impl ExecutionOutputTargetWire {
    fn from_target(target: ExecutionOutputTarget) -> Result<Self, ShellError> {
        match target {
            ExecutionOutputTarget::Inherit => Ok(Self::Inherit),
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream,
            } => Ok(Self::Capture {
                max_bytes_per_stream: u64::try_from(max_bytes_per_stream).map_err(|error| {
                    protocol_error("execution output limit is outside u64", error)
                })?,
            }),
            ExecutionOutputTarget::Value => Ok(Self::Value),
        }
    }

    fn to_target(self) -> Result<ExecutionOutputTarget, ShellError> {
        match self {
            Self::Inherit => Ok(ExecutionOutputTarget::Inherit),
            Self::Capture {
                max_bytes_per_stream,
            } => Ok(ExecutionOutputTarget::Capture {
                max_bytes_per_stream: usize::try_from(max_bytes_per_stream).map_err(|error| {
                    protocol_error("execution output limit is outside usize", error)
                })?,
            }),
            Self::Value => Ok(ExecutionOutputTarget::Value),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequest {
    version: u32,
    id: u64,
    operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Initialize {
        protocol_descriptor: String,
        policy: PolicyWire,
        grants: Option<Vec<String>>,
        process_enabled: bool,
    },
    Eval {
        source: String,
    },
    Check {
        source: String,
        source_name: String,
    },
    LoadConfig {
        source: String,
        source_name: String,
    },
    LoadPlugin {
        source: String,
        source_name: String,
    },
    RenderPrompt {
        name: String,
        context: Value,
    },
    Complete {
        command: String,
        context: Value,
    },
    Contribution {
        kind: ContributionKind,
        name: String,
        context: Value,
    },
    Run {
        source: String,
        source_name: String,
        context: ContextWire,
    },
    PluginCommand {
        name: String,
        context: ContextWire,
        deadline_ms: u64,
    },
    Event {
        event: ExtensionEventWire,
    },
    Test {
        source: String,
        source_name: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerResponse {
    version: u32,
    id: u64,
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ShellErrorWire>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerFrame {
    Response { response: WorkerResponse },
    HostCall { call: HostCall },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ParentFrame {
    Request { request: WorkerRequest },
    HostResponse { response: HostResponse },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostCall {
    request_id: u64,
    call_id: u32,
    command: String,
    deadline_ms: u64,
    max_output_bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostResponse {
    request_id: u64,
    call_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<CommandOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<ShellErrorWire>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellErrorWire {
    code: ErrorCode,
    message: String,
    labels: Vec<ErrorLabelWire>,
    context: Vec<String>,
    help: Vec<String>,
    command: Option<String>,
    exit_status: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorLabelWire {
    source: Option<String>,
    start: u32,
    end: u32,
    message: String,
}

impl ShellErrorWire {
    fn from_shell(error: ShellError) -> Result<Self, ShellError> {
        let details = error.details;
        let wire = Self {
            code: error.code,
            message: error.message,
            labels: details
                .labels
                .into_iter()
                .map(|label| {
                    Ok(ErrorLabelWire {
                        source: label.source,
                        start: u32::try_from(label.start).map_err(|error| {
                            protocol_error("Lua worker label start is outside u32", error)
                        })?,
                        end: u32::try_from(label.end).map_err(|error| {
                            protocol_error("Lua worker label end is outside u32", error)
                        })?,
                        message: label.message,
                    })
                })
                .collect::<Result<Vec<_>, ShellError>>()?,
            context: details.context,
            help: details.help,
            command: details.command,
            exit_status: details.exit_status,
        };
        wire.validate()?;
        Ok(wire)
    }

    fn into_shell(self) -> Result<ShellError, ShellError> {
        self.validate()?;
        let mut error = ShellError::new(self.code, self.message);
        for label in self.labels {
            let start = usize::try_from(label.start).map_err(|conversion| {
                protocol_error("Lua worker label start is outside usize", conversion)
            })?;
            let end = usize::try_from(label.end).map_err(|conversion| {
                protocol_error("Lua worker label end is outside usize", conversion)
            })?;
            error = error.with_label(label.source, start, end, label.message);
        }
        for context in self.context {
            error = error.with_context(context);
        }
        for help in self.help {
            error = error.with_help(help);
        }
        if let Some(command) = self.command {
            error = error.with_command(command);
        }
        error.details.exit_status = self.exit_status;
        Ok(error)
    }

    fn validate(&self) -> Result<(), ShellError> {
        if self.labels.len() > ERROR_ITEMS_MAX
            || self.context.len() > ERROR_ITEMS_MAX
            || self.help.len() > ERROR_ITEMS_MAX
        {
            return Err(protocol_violation(
                "Lua worker error contains too many diagnostic items",
            ));
        }
        let mut bytes = self.message.len();
        for label in &self.labels {
            bytes = bytes.saturating_add(label.message.len());
            if let Some(source) = &label.source {
                bytes = bytes.saturating_add(source.len());
                if label.start > label.end {
                    return Err(protocol_violation(
                        "Lua worker error label has an invalid UTF-8 span",
                    ));
                }
                if usize::try_from(label.end).is_ok_and(|end| end <= source.len()) {
                    let start = usize::try_from(label.start).map_err(|error| {
                        protocol_error("Lua worker label start is outside usize", error)
                    })?;
                    let end = usize::try_from(label.end).map_err(|error| {
                        protocol_error("Lua worker label end is outside usize", error)
                    })?;
                    if !source.is_char_boundary(start) || !source.is_char_boundary(end) {
                        return Err(protocol_violation(
                            "Lua worker error label has an invalid UTF-8 span",
                        ));
                    }
                }
            } else if label.start > label.end {
                return Err(protocol_violation(
                    "Lua worker error label has a reversed span",
                ));
            }
        }
        for text in self.context.iter().chain(&self.help) {
            bytes = bytes.saturating_add(text.len());
        }
        if let Some(command) = &self.command {
            bytes = bytes.saturating_add(command.len());
        }
        if bytes > ERROR_TEXT_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "Lua worker error exceeds its text-byte limit",
            )
            .with_context(format!("observed: {bytes}; limit: {ERROR_TEXT_BYTES_MAX}"))
            .with_help("Reduce guest diagnostic size"));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionEventWire {
    protocol_version: u32,
    sequence: u64,
    data: ExtensionEventDataWire,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExtensionEventDataWire {
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
        bytes: u64,
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
        error: ShellErrorWire,
    },
}

impl ExtensionEventWire {
    fn from_event(event: &ExtensionEvent) -> Result<Self, ShellError> {
        let data = match &event.data {
            ExtensionEventData::SessionStart { restored } => ExtensionEventDataWire::SessionStart {
                restored: *restored,
            },
            ExtensionEventData::SessionRestore { session_id } => {
                ExtensionEventDataWire::SessionRestore {
                    session_id: session_id.clone(),
                }
            }
            ExtensionEventData::DirectoryChanged { previous, current } => {
                ExtensionEventDataWire::DirectoryChanged {
                    previous: previous.clone(),
                    current: current.clone(),
                }
            }
            ExtensionEventData::CommandPlan { source, effects } => {
                ExtensionEventDataWire::CommandPlan {
                    source: source.clone(),
                    effects: effects.clone(),
                }
            }
            ExtensionEventData::ExecutionProgress {
                completed,
                total,
                message,
            } => ExtensionEventDataWire::ExecutionProgress {
                completed: *completed,
                total: *total,
                message: message.clone(),
            },
            ExtensionEventData::Output {
                stream,
                bytes,
                text,
            } => ExtensionEventDataWire::Output {
                stream: *stream,
                bytes: u64::try_from(*bytes).map_err(|error| {
                    protocol_error("extension output byte count is outside u64", error)
                })?,
                text: text.clone(),
            },
            ExtensionEventData::Cancellation { reason } => ExtensionEventDataWire::Cancellation {
                reason: reason.clone(),
            },
            ExtensionEventData::Result {
                status,
                duration_ms,
            } => ExtensionEventDataWire::Result {
                status: *status,
                duration_ms: *duration_ms,
            },
            ExtensionEventData::Error { error } => ExtensionEventDataWire::Error {
                error: ShellErrorWire::from_shell(error.clone())?,
            },
        };
        Ok(Self {
            protocol_version: event.protocol_version,
            sequence: event.sequence,
            data,
        })
    }

    fn into_event(self) -> Result<ExtensionEvent, ShellError> {
        let data = match self.data {
            ExtensionEventDataWire::SessionStart { restored } => {
                ExtensionEventData::SessionStart { restored }
            }
            ExtensionEventDataWire::SessionRestore { session_id } => {
                ExtensionEventData::SessionRestore { session_id }
            }
            ExtensionEventDataWire::DirectoryChanged { previous, current } => {
                ExtensionEventData::DirectoryChanged { previous, current }
            }
            ExtensionEventDataWire::CommandPlan { source, effects } => {
                ExtensionEventData::CommandPlan { source, effects }
            }
            ExtensionEventDataWire::ExecutionProgress {
                completed,
                total,
                message,
            } => ExtensionEventData::ExecutionProgress {
                completed,
                total,
                message,
            },
            ExtensionEventDataWire::Output {
                stream,
                bytes,
                text,
            } => ExtensionEventData::Output {
                stream,
                bytes: usize::try_from(bytes).map_err(|error| {
                    protocol_error("extension output byte count is outside usize", error)
                })?,
                text,
            },
            ExtensionEventDataWire::Cancellation { reason } => {
                ExtensionEventData::Cancellation { reason }
            }
            ExtensionEventDataWire::Result {
                status,
                duration_ms,
            } => ExtensionEventData::Result {
                status,
                duration_ms,
            },
            ExtensionEventDataWire::Error { error } => ExtensionEventData::Error {
                error: error.into_shell()?,
            },
        };
        Ok(ExtensionEvent {
            protocol_version: self.protocol_version,
            sequence: self.sequence,
            data,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionOutcomeWire {
    status: ExecutionStatus,
    output: ExecutionOutput,
    diagnostics: Vec<ShellErrorWire>,
    cleanup: ExecutionCleanupState,
}

impl ExecutionOutcomeWire {
    fn from_outcome(outcome: ExecutionOutcome) -> Result<Self, ShellError> {
        Ok(Self {
            status: outcome.status,
            output: outcome.output,
            diagnostics: outcome
                .diagnostics
                .into_iter()
                .map(ShellErrorWire::from_shell)
                .collect::<Result<Vec<_>, _>>()?,
            cleanup: outcome.cleanup,
        })
    }

    fn into_outcome(self) -> Result<ExecutionOutcome, ShellError> {
        let diagnostics = self
            .diagnostics
            .into_iter()
            .map(ShellErrorWire::into_shell)
            .collect::<Result<Vec<_>, _>>()?;
        ExecutionOutcome::new(self.status, self.output, diagnostics, self.cleanup)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventHandlerReportWire {
    handler: String,
    actions: Vec<ExtensionAction>,
    error: Option<ShellErrorWire>,
}

impl EventHandlerReportWire {
    fn from_report(report: EventHandlerReport) -> Result<Self, ShellError> {
        Ok(Self {
            handler: report.handler,
            actions: report.actions,
            error: report.error.map(ShellErrorWire::from_shell).transpose()?,
        })
    }

    fn into_report(self) -> Result<EventHandlerReport, ShellError> {
        Ok(EventHandlerReport {
            handler: self.handler,
            actions: self.actions,
            error: self.error.map(ShellErrorWire::into_shell).transpose()?,
        })
    }
}

#[derive(Clone)]
pub(crate) struct LuaWorkerCancellation {
    cancelled: Arc<AtomicBool>,
}

impl LuaWorkerCancellation {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

pub(crate) struct LuaWorkerRuntime {
    process: Mutex<WorkerProcess>,
    policy: LuaPolicy,
    cancelled: Arc<AtomicBool>,
}

impl LuaWorkerRuntime {
    pub(crate) fn new(policy: LuaPolicy) -> Result<Self, ShellError> {
        Self::spawn(policy, None, false, Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn new_with_process_host(
        policy: LuaPolicy,
        _process_host: ProcessHost,
    ) -> Result<Self, ShellError> {
        Self::spawn(policy, None, true, Arc::new(AtomicBool::new(false)))
    }

    pub(crate) fn new_with_process_host_and_cancellation(
        policy: LuaPolicy,
        _process_host: ProcessHost,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        Self::spawn(policy, None, true, cancelled)
    }

    pub(crate) fn new_with_cancellation(
        policy: LuaPolicy,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        Self::spawn(policy, None, false, cancelled)
    }

    pub(crate) fn new_with_capabilities(
        policy: LuaPolicy,
        grants: &[String],
    ) -> Result<Self, ShellError> {
        Self::spawn(
            policy,
            Some(grants.to_vec()),
            false,
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub(crate) fn new_with_capabilities_and_process_host(
        policy: LuaPolicy,
        grants: &[String],
        process_host: Option<ProcessHost>,
    ) -> Result<Self, ShellError> {
        Self::spawn(
            policy,
            Some(grants.to_vec()),
            process_host.is_some(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn spawn(
        policy: LuaPolicy,
        grants: Option<Vec<String>>,
        process_enabled: bool,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        let mut process = WorkerProcess::spawn()?;
        process.call::<()>(
            Operation::Initialize {
                protocol_descriptor: LUA_WORKER_PROTOCOL_DESCRIPTOR.to_owned(),
                policy: PolicyWire::from_policy(policy)?,
                grants,
                process_enabled,
            },
            WORKER_STARTUP_TIMEOUT,
            &Arc::new(AtomicBool::new(false)),
        )?;
        Ok(Self {
            process: Mutex::new(process),
            policy,
            cancelled,
        })
    }

    fn call<T: DeserializeOwned>(&self, operation: Operation) -> Result<T, ShellError> {
        let mut process = self.process.lock().map_err(|_| unavailable_error())?;
        process.call(operation, self.policy.wall_time, &self.cancelled)
    }

    pub(crate) fn eval(&self, source: &str) -> Result<Value, ShellError> {
        self.call(Operation::Eval {
            source: source.to_owned(),
        })
    }

    pub(crate) fn load_config_file(&self, path: &Path) -> Result<QuirlConfig, ShellError> {
        let source = read_source(path)?;
        let config: QuirlConfig = self.call(Operation::LoadConfig {
            source,
            source_name: path.display().to_string(),
        })?;
        config.validate(&path.display().to_string())?;
        Ok(config)
    }

    pub(crate) fn load_plugin_file(&self, path: &Path) -> Result<PluginRegistrations, ShellError> {
        self.load_plugin_source(&read_source(path)?, &path.display().to_string())
    }

    pub(crate) fn load_plugin_source(
        &self,
        source: &str,
        source_name: &str,
    ) -> Result<PluginRegistrations, ShellError> {
        let registrations: PluginRegistrations = self.call(Operation::LoadPlugin {
            source: source.to_owned(),
            source_name: source_name.to_owned(),
        })?;
        registrations.validate()?;
        Ok(registrations)
    }

    pub(crate) fn render_prompt_segment(
        &self,
        name: &str,
        context: &Value,
    ) -> Result<Option<String>, ShellError> {
        self.call(Operation::RenderPrompt {
            name: name.to_owned(),
            context: context.clone(),
        })
    }

    pub(crate) fn complete_with_provider(
        &self,
        command: &str,
        context: &Value,
    ) -> Result<Value, ShellError> {
        self.call(Operation::Complete {
            command: command.to_owned(),
            context: context.clone(),
        })
    }

    pub(crate) fn invoke_contribution(
        &self,
        kind: ContributionKind,
        name: &str,
        context: &Value,
    ) -> Result<Value, ShellError> {
        self.call(Operation::Contribution {
            kind,
            name: name.to_owned(),
            context: context.clone(),
        })
    }

    pub(crate) fn run_source_with_context(
        &self,
        source: &str,
        source_name: &str,
        context: &LuaRunnerContext,
    ) -> Result<ExecutionOutcome, ShellError> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "isolated Lua execution was cancelled",
            )
            .with_context("before Lua module evaluation")
            .with_help("Retry only after clearing the owning cancellation handle"));
        }
        let outcome: ExecutionOutcomeWire = self.call(Operation::Run {
            source: source.to_owned(),
            source_name: source_name.to_owned(),
            context: ContextWire::from_context(context)?,
        })?;
        outcome.into_outcome()
    }

    pub(crate) fn run_plugin_command_with_context(
        &self,
        name: &str,
        context: &LuaRunnerContext,
        expires_at: Instant,
    ) -> Result<ExecutionOutcome, ShellError> {
        let remaining = expires_at.saturating_duration_since(Instant::now());
        let deadline_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let mut process = self.process.lock().map_err(|_| unavailable_error())?;
        let outcome: ExecutionOutcomeWire = process.call(
            Operation::PluginCommand {
                name: name.to_owned(),
                context: ContextWire::from_context(context)?,
                deadline_ms,
            },
            remaining,
            &self.cancelled,
        )?;
        outcome.into_outcome()
    }

    pub(crate) fn dispatch_extension_event(
        &self,
        event: &ExtensionEvent,
    ) -> Result<Vec<EventHandlerReport>, ShellError> {
        let reports: Vec<EventHandlerReportWire> = self.call(Operation::Event {
            event: ExtensionEventWire::from_event(event)?,
        })?;
        reports
            .into_iter()
            .map(EventHandlerReportWire::into_report)
            .collect()
    }

    pub(crate) fn test_file(&self, path: &Path) -> Result<usize, ShellError> {
        let count: u64 = self.call(Operation::Test {
            source: read_source(path)?,
            source_name: path.display().to_string(),
        })?;
        usize::try_from(count)
            .map_err(|error| protocol_error("Lua test count is outside usize", error))
    }

    pub(crate) fn check_file(path: &Path) -> Result<(), ShellError> {
        Self::check_source(&read_source(path)?, &path.display().to_string())
    }

    pub(crate) fn check_source(source: &str, source_name: &str) -> Result<(), ShellError> {
        Self::check_source_with_policy(source, source_name, LuaPolicy::config())
    }

    /// Check source syntax in an isolated worker under an explicit admitted policy.
    /// This preserves source diagnostics and applies the policy's wall deadline to
    /// the check RPC; ordinary callers retain [`Self::check_source`]'s config policy.
    pub(crate) fn check_source_with_policy(
        source: &str,
        source_name: &str,
        policy: LuaPolicy,
    ) -> Result<(), ShellError> {
        let runtime = Self::new(policy)?;
        runtime.call(Operation::Check {
            source: source.to_owned(),
            source_name: source_name.to_owned(),
        })
    }

    pub(crate) fn cancellation_token(&self) -> LuaWorkerCancellation {
        LuaWorkerCancellation {
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    pub(crate) fn execution_cancellation(&self) -> ExecutionCancellation {
        ExecutionCancellation::from_atomic(Arc::clone(&self.cancelled))
    }

    pub(crate) fn clear_cancellation(&self) {
        self.cancelled.store(false, Ordering::Relaxed);
    }
}

struct WorkerProcess {
    child: ContainedChild,
    input: Option<ChildStdin>,
    frames: Receiver<Result<WorkerFrame, ShellError>>,
    reader: Option<JoinHandle<()>>,
    next_request_id: u64,
    poisoned: bool,
}

impl WorkerProcess {
    fn spawn() -> Result<Self, ShellError> {
        Self::spawn_with_fault(None)
    }

    fn spawn_with_fault(fault: Option<&str>) -> Result<Self, ShellError> {
        let mut command = worker_command()?;
        if let Some(fault) = fault {
            command.env(TEST_WORKER_FAULT_ENV, fault);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = ContainedChild::spawn(&mut command)?;
        let input = child
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| worker_pipe_error("input"))?;
        let output = child
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| worker_pipe_error("response"))?;
        let (sender, frames) = mpsc::sync_channel(1);
        let reader = thread::Builder::new()
            .name("quirl-lua-worker-reader".to_owned())
            .spawn(move || {
                let mut output = output;
                loop {
                    let frame = read_frame::<_, WorkerFrame>(&mut output).map_err(|error| {
                        if error.code == ErrorCode::Io {
                            ShellError::new(
                                ErrorCode::Lua,
                                "isolated Lua worker response stream ended",
                            )
                            .with_context(error.message)
                            .with_help("Inspect the Lua source and restart Quirl")
                        } else {
                            error
                        }
                    });
                    let finished = frame.is_err();
                    if sender.try_send(frame).is_err() || finished {
                        break;
                    }
                }
            })
            .map_err(|error| {
                ShellError::new(
                    ErrorCode::ProcessSpawn,
                    "could not start Lua worker response reader",
                )
                .with_context(error.to_string())
                .with_help("Retry after reducing thread pressure")
            })?;
        Ok(Self {
            child,
            input: Some(input),
            frames,
            reader: Some(reader),
            next_request_id: 1,
            poisoned: false,
        })
    }

    fn call<T: DeserializeOwned>(
        &mut self,
        operation: Operation,
        timeout: Duration,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<T, ShellError> {
        if self.poisoned {
            return Err(unavailable_error());
        }
        if timeout.is_zero() {
            return Err(deadline_error(timeout));
        }
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            ShellError::new(
                ErrorCode::ResourceLimit,
                "Lua worker request counter was exhausted",
            )
            .with_help("Restart Quirl")
        })?;
        if let Err(error) = self.write_parent_frame(&ParentFrame::Request {
            request: WorkerRequest {
                version: PROTOCOL_VERSION,
                id: request_id,
                operation,
            },
        }) {
            return self.fail_and_cleanup(error);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| deadline_error(timeout))?;
        let mut host_calls = 0_u32;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.fail_and_cleanup(deadline_error(timeout));
            }
            match self.frames.recv_timeout(remaining.min(POLL_INTERVAL)) {
                Ok(Ok(WorkerFrame::Response { response })) => {
                    return match decode_response(response, request_id) {
                        Ok(result) => result,
                        Err(error) => self.fail_and_cleanup(error),
                    };
                }
                Ok(Ok(WorkerFrame::HostCall { call })) => {
                    host_calls = host_calls.saturating_add(1);
                    if host_calls > HOST_CALLS_PER_REQUEST_MAX || call.request_id != request_id {
                        return self.fail_and_cleanup(protocol_violation(
                            "invalid or excessive Lua worker host call",
                        ));
                    }
                    if let Err(error) = self.handle_host_call(call, remaining, cancelled) {
                        return self.fail_and_cleanup(error);
                    }
                }
                Ok(Err(error)) => return self.fail_and_cleanup(error),
                Err(RecvTimeoutError::Timeout) => {
                    if cancelled.load(Ordering::Relaxed) {
                        return self.fail_and_cleanup(
                            ShellError::new(
                                ErrorCode::ResourceLimit,
                                "isolated Lua execution was cancelled",
                            )
                            .with_help("Retry only after clearing the owning cancellation handle"),
                        );
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let error = worker_crash_error(&mut self.child);
                    return self.fail_and_cleanup(error);
                }
            }
        }
    }

    fn handle_host_call(
        &mut self,
        call: HostCall,
        remaining: Duration,
        cancelled: &Arc<AtomicBool>,
    ) -> Result<(), ShellError> {
        if call.command.len() > NATIVE_COMMAND_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "Lua worker host-call command exceeds its byte limit",
            )
            .with_context(format!(
                "observed: {}; limit: {NATIVE_COMMAND_BYTES_MAX}",
                call.command.len()
            ))
            .with_help("Reduce the guest process command"));
        }
        let requested = Duration::from_millis(call.deadline_ms).min(remaining);
        let max_output_bytes = usize::try_from(call.max_output_bytes)
            .map_err(|error| protocol_error("Lua worker output limit is outside usize", error))?;
        if max_output_bytes > DEFAULT_CAPTURE_BYTES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "Lua worker host-call output limit exceeds its protocol bound",
            )
            .with_context(format!(
                "observed: {max_output_bytes}; limit: {DEFAULT_CAPTURE_BYTES}"
            ))
            .with_help("Use the bounded output limit supplied by the Lua host API"));
        }
        let result = isolated_process_host()(ProcessRequest {
            command: call.command,
            deadline: requested,
            cancelled: Arc::clone(cancelled),
            max_output_bytes,
        });
        let response = match result {
            Ok(outcome) => HostResponse {
                request_id: call.request_id,
                call_id: call.call_id,
                outcome: Some(outcome),
                error: None,
            },
            Err(error) => HostResponse {
                request_id: call.request_id,
                call_id: call.call_id,
                outcome: None,
                error: Some(ShellErrorWire::from_shell(error)?),
            },
        };
        self.write_parent_frame(&ParentFrame::HostResponse { response })
    }

    fn write_parent_frame(&mut self, frame: &ParentFrame) -> Result<(), ShellError> {
        let Some(input) = self.input.as_mut() else {
            return Err(unavailable_error());
        };
        write_frame(input, frame)
            .map_err(|error| error.with_context("while writing a Lua worker request"))
    }

    fn fail_and_cleanup<T>(&mut self, primary: ShellError) -> Result<T, ShellError> {
        self.poisoned = true;
        self.input.take();
        let cleanup = self.child.terminate_and_reap();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        let mut error = primary;
        if let Err(cleanup) = cleanup {
            error = error.with_context(format!("cleanup: {}", cleanup.message));
        }
        Err(error)
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.input.take();
        // Containment must close before joining the reader. A worker leader may
        // already have exited while a descendant still retains the response pipe.
        let _ = self.child.terminate_and_reap();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub(crate) fn worker_requested() -> bool {
    std::env::args().nth(1).as_deref() == Some(WORKER_ARGUMENT)
}

pub(crate) fn run_worker() -> Result<(), ShellError> {
    let input = Arc::new(Mutex::new(std::io::stdin()));
    let output = Arc::new(Mutex::new(std::io::stderr()));
    let current_request_id = Arc::new(AtomicU64::new(0));
    let host_call_id = Arc::new(AtomicU32::new(0));
    let mut runtime: Option<LuaRuntime> = None;
    loop {
        let parent = {
            let mut input = input.lock().map_err(|_| unavailable_error())?;
            match read_frame::<_, ParentFrame>(&mut *input) {
                Ok(frame) => frame,
                Err(error) if error.code == ErrorCode::Io => return Ok(()),
                Err(error) => return Err(error),
            }
        };
        let ParentFrame::Request { request } = parent else {
            return Err(protocol_violation(
                "worker received a host response without a host call",
            ));
        };
        if request.version != PROTOCOL_VERSION {
            return Err(protocol_violation(
                "unsupported Lua worker protocol version",
            ));
        }
        current_request_id.store(request.id, Ordering::Relaxed);
        host_call_id.store(0, Ordering::Relaxed);
        let result = execute_operation(
            &mut runtime,
            request.operation,
            &input,
            &output,
            &current_request_id,
            &host_call_id,
        );
        let response = match result {
            Ok(value) => WorkerResponse {
                version: PROTOCOL_VERSION,
                id: request.id,
                ok: true,
                result: value,
                error: None,
            },
            Err(error) => WorkerResponse {
                version: PROTOCOL_VERSION,
                id: request.id,
                ok: false,
                result: Value::Null,
                error: Some(ShellErrorWire::from_shell(error)?),
            },
        };
        let mut output = output.lock().map_err(|_| unavailable_error())?;
        write_frame(&mut *output, &WorkerFrame::Response { response })?;
    }
}

fn execute_operation(
    runtime: &mut Option<LuaRuntime>,
    operation: Operation,
    input: &Arc<Mutex<std::io::Stdin>>,
    output: &Arc<Mutex<std::io::Stderr>>,
    request_id: &Arc<AtomicU64>,
    call_id: &Arc<AtomicU32>,
) -> Result<Value, ShellError> {
    if let Operation::Initialize {
        protocol_descriptor,
        policy,
        grants,
        process_enabled,
    } = operation
    {
        if runtime.is_some() {
            return Err(protocol_violation("Lua worker was initialized twice"));
        }
        if protocol_descriptor != LUA_WORKER_PROTOCOL_DESCRIPTOR {
            return Err(protocol_violation(
                "Lua worker protocol descriptor mismatch",
            ));
        }
        let policy = policy.to_policy()?;
        let process_host = process_enabled.then(|| {
            worker_process_host(
                Arc::clone(input),
                Arc::clone(output),
                Arc::clone(request_id),
                Arc::clone(call_id),
            )
        });
        let created = match grants {
            Some(grants) => {
                LuaRuntime::new_with_capabilities_and_process_host(policy, &grants, process_host)
            }
            None => match process_host {
                Some(host) => LuaRuntime::new_with_process_host(policy, host),
                None => LuaRuntime::new(policy),
            },
        }?;
        *runtime = Some(created);
        return Ok(Value::Null);
    }
    let runtime = runtime
        .as_ref()
        .ok_or_else(|| protocol_violation("Lua worker request preceded initialization"))?;
    let value = match operation {
        Operation::Initialize { .. } => {
            return Err(protocol_violation(
                "Lua worker received duplicate initialization",
            ));
        }
        Operation::Eval { source } => encode_value(runtime.eval(&source)?)?,
        Operation::Check {
            source,
            source_name,
        } => {
            LuaRuntime::check_source(&source, &source_name)?;
            Value::Null
        }
        Operation::LoadConfig {
            source,
            source_name,
        } => encode_value(runtime.load_config_source(&source, &source_name)?)?,
        Operation::LoadPlugin {
            source,
            source_name,
        } => encode_value(runtime.load_plugin_source(&source, &source_name)?)?,
        Operation::RenderPrompt { name, context } => {
            encode_value(runtime.render_prompt_segment(&name, &context)?)?
        }
        Operation::Complete { command, context } => {
            runtime.complete_with_provider(&command, &context)?
        }
        Operation::Contribution {
            kind,
            name,
            context,
        } => runtime.invoke_contribution(kind, &name, &context)?,
        Operation::Run {
            source,
            source_name,
            context,
        } => {
            let context = context.to_context(runtime.execution_cancellation().atomic())?;
            encode_value(ExecutionOutcomeWire::from_outcome(
                runtime.run_source_with_context(&source, &source_name, &context)?,
            )?)?
        }
        Operation::PluginCommand {
            name,
            context,
            deadline_ms,
        } => {
            let context = context.to_context(runtime.execution_cancellation().atomic())?;
            let expires = Instant::now()
                .checked_add(Duration::from_millis(deadline_ms))
                .ok_or_else(|| deadline_error(Duration::from_millis(deadline_ms)))?;
            encode_value(ExecutionOutcomeWire::from_outcome(
                runtime.run_plugin_command_with_context(&name, &context, expires)?,
            )?)?
        }
        Operation::Event { event } => {
            let reports = runtime
                .dispatch_extension_event(&event.into_event()?)?
                .into_iter()
                .map(EventHandlerReportWire::from_report)
                .collect::<Result<Vec<_>, _>>()?;
            encode_value(reports)?
        }
        Operation::Test {
            source,
            source_name,
        } => encode_value(
            u64::try_from(runtime.test_source(&source, &source_name)?)
                .map_err(|error| protocol_error("Lua test count is outside u64", error))?,
        )?,
    };
    Ok(value)
}

fn worker_process_host(
    input: Arc<Mutex<std::io::Stdin>>,
    output: Arc<Mutex<std::io::Stderr>>,
    request_id: Arc<AtomicU64>,
    call_id: Arc<AtomicU32>,
) -> ProcessHost {
    Arc::new(move |request| {
        let call_id = call_id
            .fetch_add(1, Ordering::Relaxed)
            .checked_add(1)
            .ok_or_else(|| protocol_violation("Lua worker host-call counter exhausted"))?;
        if call_id > HOST_CALLS_PER_REQUEST_MAX {
            return Err(protocol_violation("Lua worker host-call limit exceeded"));
        }
        let request_id = request_id.load(Ordering::Relaxed);
        let call = HostCall {
            request_id,
            call_id,
            command: request.command,
            deadline_ms: u64::try_from(request.deadline.as_millis()).unwrap_or(u64::MAX),
            max_output_bytes: u64::try_from(request.max_output_bytes).unwrap_or(u64::MAX),
        };
        {
            let mut output = output.lock().map_err(|_| unavailable_error())?;
            write_frame(&mut *output, &WorkerFrame::HostCall { call })?;
        }
        let parent = {
            let mut input = input.lock().map_err(|_| unavailable_error())?;
            read_frame::<_, ParentFrame>(&mut *input)?
        };
        let ParentFrame::HostResponse { response } = parent else {
            return Err(protocol_violation("Lua worker expected a host response"));
        };
        if response.request_id != request_id || response.call_id != call_id {
            return Err(protocol_violation(
                "Lua worker host response identity mismatch",
            ));
        }
        match (response.outcome, response.error) {
            (Some(outcome), None) => Ok(outcome),
            (None, Some(error)) => Err(error.into_shell()?),
            _ => Err(protocol_violation(
                "Lua worker host response must contain one outcome or error",
            )),
        }
    })
}

fn decode_response<T: DeserializeOwned>(
    response: WorkerResponse,
    request_id: u64,
) -> Result<Result<T, ShellError>, ShellError> {
    if response.version != PROTOCOL_VERSION || response.id != request_id {
        return Err(protocol_violation("Lua worker response identity mismatch"));
    }
    match (response.ok, response.error) {
        (true, None) => serde_json::from_value(response.result)
            .map(Ok)
            .map_err(|error| {
                protocol_error("Lua worker response has the wrong result shape", error)
            }),
        (false, Some(error)) if response.result.is_null() => Ok(Err(error.into_shell()?)),
        _ => Err(protocol_violation(
            "Lua worker response success/error fields are inconsistent",
        )),
    }
}

fn encode_value(value: impl Serialize) -> Result<Value, ShellError> {
    serde_json::to_value(value)
        .map_err(|error| protocol_error("could not encode Lua worker result", error))
}

fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> Result<(), ShellError> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| protocol_error("could not encode Lua worker frame", error))?;
    if payload.len() > FRAME_BYTES_MAX {
        return Err(frame_limit_error(payload.len()));
    }
    let length = u32::try_from(payload.len())
        .map_err(|error| protocol_error("Lua worker frame length is outside u32", error))?;
    writer
        .write_all(&length.to_be_bytes())
        .and_then(|()| writer.write_all(&payload))
        .and_then(|()| writer.flush())
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not write Lua worker frame")
                .with_context(error.to_string())
                .with_help("Restart Quirl and retry")
        })
}

fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, ShellError> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header).map_err(|error| {
        ShellError::new(ErrorCode::Io, "Lua worker protocol stream ended")
            .with_context(error.to_string())
            .with_help("Restart Quirl and retry")
    })?;
    let length = usize::try_from(u32::from_be_bytes(header)).unwrap_or(usize::MAX);
    if length > FRAME_BYTES_MAX {
        return Err(frame_limit_error(length));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).map_err(|error| {
        ShellError::new(ErrorCode::Io, "Lua worker frame was truncated")
            .with_context(error.to_string())
            .with_help("Restart Quirl and retry")
    })?;
    serde_json::from_slice(&payload)
        .map_err(|error| protocol_error("Lua worker frame is malformed", error))
}

fn read_source(path: &Path) -> Result<String, ShellError> {
    let bytes = read_regular_file(ReadFileOptions {
        path,
        bytes_max: MAX_LUA_SOURCE_BYTES,
        context: "Lua source",
        help: "Use a readable regular UTF-8 file within the Lua source limit",
        io_error_code: ErrorCode::ScriptRead,
    })?;
    String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("{} is not valid UTF-8", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Encode Lua source as UTF-8")
    })
}

fn worker_command() -> Result<Command, ShellError> {
    let executable = std::env::current_exe().map_err(|error| {
        ShellError::new(
            ErrorCode::ProcessSpawn,
            "could not locate the Quirl executable for Lua isolation",
        )
        .with_context(error.to_string())
        .with_help("Reinstall Quirl and retry")
    })?;
    let mut command = Command::new(executable);
    if cfg!(test) {
        command.args([
            "--exact",
            "lua_worker::tests::worker_entrypoint",
            "--nocapture",
        ]);
        command.env(TEST_WORKER_ENV, "1");
    } else {
        command.arg(WORKER_ARGUMENT);
    }
    Ok(command)
}

fn policy_limit_error(field: &str, observed: u64, limit: u64) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("Lua worker policy {field} is outside its limit"),
    )
    .with_context(format!("observed: {observed}; limit: {limit}"))
    .with_help("Use one of Quirl's bounded Lua policy profiles")
}

fn frame_limit_error(observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "Lua worker frame exceeds its byte limit",
    )
    .with_context(format!("observed: {observed}; limit: {FRAME_BYTES_MAX}"))
    .with_help("Reduce Lua source or callback payload size")
}

fn protocol_error(message: &str, error: impl std::fmt::Display) -> ShellError {
    ShellError::new(ErrorCode::Validation, message)
        .with_context(error.to_string())
        .with_help("Report this as an isolated Lua protocol defect")
}

fn protocol_violation(message: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message)
        .with_help("Report this as an isolated Lua protocol defect")
}

fn worker_pipe_error(name: &str) -> ShellError {
    ShellError::new(
        ErrorCode::ProcessSpawn,
        format!("isolated Lua worker has no {name} pipe"),
    )
    .with_help("Report this as a worker startup defect")
}

fn unavailable_error() -> ShellError {
    ShellError::new(ErrorCode::Lua, "isolated Lua worker is unavailable")
        .with_help("Reload the extension or restart Quirl")
}

fn deadline_error(timeout: Duration) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "isolated Lua execution exceeded its deadline",
    )
    .with_context(format!("limit: {} ms", timeout.as_millis()))
    .with_help("Reduce Lua work or raise the reviewed policy deadline")
}

fn worker_crash_error(child: &mut ContainedChild) -> ShellError {
    let status = child.try_wait().ok().flatten();
    ShellError::new(
        ErrorCode::Lua,
        "isolated Lua worker terminated without a response",
    )
    .with_context(status.map_or_else(
        || "exit status unavailable".to_owned(),
        |s| format!("worker exited with {s}"),
    ))
    .with_help("Inspect the Lua source and restart Quirl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(
        clippy::exit,
        reason = "the subprocess test entrypoint must emit exact worker exit statuses"
    )]
    fn worker_entrypoint() {
        if std::env::var_os(TEST_WORKER_ENV).is_none() {
            return;
        }
        match std::env::var(TEST_WORKER_FAULT_ENV).ok().as_deref() {
            Some("crash") => std::process::exit(42),
            Some("malformed") => {
                let mut output = std::io::stderr();
                output.write_all(&1_u32.to_be_bytes()).unwrap();
                output.write_all(b"{").unwrap();
                output.flush().unwrap();
                std::process::exit(0);
            }
            Some("oversized") => {
                let mut output = std::io::stderr();
                let length = u32::try_from(FRAME_BYTES_MAX + 1).unwrap();
                output.write_all(&length.to_be_bytes()).unwrap();
                output.flush().unwrap();
                std::process::exit(0);
            }
            _ => {}
        }
        let status = run_worker().map_or(1, |()| 0);
        std::process::exit(status);
    }

    #[test]
    fn native_c_call_is_killed_at_the_wall_deadline() {
        let runtime = LuaWorkerRuntime::new(LuaPolicy {
            memory_limit_bytes: 64 * 1024 * 1024,
            wall_time: Duration::from_millis(2),
            ..LuaPolicy::config()
        })
        .unwrap();
        let started = Instant::now();
        let error = runtime
            .eval("return string.rep('x', 50000000)")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("exceeded its deadline"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn worker_faults_are_bounded_and_reaped() {
        for (fault, code) in [
            ("crash", ErrorCode::Lua),
            ("malformed", ErrorCode::Validation),
            ("oversized", ErrorCode::ResourceLimit),
        ] {
            let mut worker = WorkerProcess::spawn_with_fault(Some(fault)).unwrap();
            let error = worker
                .call::<Value>(
                    Operation::Eval {
                        source: "return 1".to_owned(),
                    },
                    Duration::from_millis(100),
                    &Arc::new(AtomicBool::new(false)),
                )
                .unwrap_err();
            assert_eq!(error.code, code, "fault={fault}: {error:?}");
            assert!(worker.child.try_wait().unwrap().is_some());
        }
    }

    #[test]
    fn protected_call_runaway_and_cancellation_are_bounded() {
        let runtime = LuaWorkerRuntime::new(LuaPolicy {
            instruction_limit: 1_000,
            ..LuaPolicy::config()
        })
        .unwrap();
        let error = runtime
            .eval("return pcall(function() while true do end end)")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);

        let runtime = LuaWorkerRuntime::new(LuaPolicy::config()).unwrap();
        runtime.cancellation_token().cancel();
        let error = runtime
            .eval("return xpcall(function() while true do end end, function() return 'caught' end)")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn protocol_fixture_rejects_unknown_fields_and_future_versions() {
        let request = ParentFrame::Request {
            request: WorkerRequest {
                version: PROTOCOL_VERSION,
                id: 7,
                operation: Operation::Eval {
                    source: "return 42".to_owned(),
                },
            },
        };
        assert_eq!(serde_json::to_string(&request).unwrap(), REQUEST_FIXTURE_V1);
        let response = WorkerFrame::Response {
            response: WorkerResponse {
                version: PROTOCOL_VERSION,
                id: 7,
                ok: true,
                result: Value::from(42),
                error: None,
            },
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            RESPONSE_FIXTURE_V1
        );
        let unknown = br#"{\"kind\":\"request\",\"request\":{\"version\":1,\"id\":1,\"operation\":{\"operation\":\"eval\",\"source\":\"return 1\",\"extra\":true}}}"#;
        assert!(serde_json::from_slice::<ParentFrame>(unknown).is_err());
        let unknown_envelope = br#"{\"kind\":\"request\",\"request\":{\"version\":1,\"id\":1,\"operation\":{\"operation\":\"eval\",\"source\":\"return 1\"}},\"extra\":true}"#;
        assert!(serde_json::from_slice::<ParentFrame>(unknown_envelope).is_err());
        assert!(serde_json::from_slice::<ParentFrame>(b"{").is_err());
        for version in [0, PROTOCOL_VERSION + 1] {
            let response = WorkerResponse {
                version,
                id: 7,
                ok: true,
                result: Value::from(42),
                error: None,
            };
            assert!(decode_response::<Value>(response, 7).is_err());
        }
    }

    #[test]
    fn fixed_width_error_label_offsets_check_conversion_boundaries() {
        let exact = usize::try_from(u32::MAX).unwrap();
        let error =
            ShellError::new(ErrorCode::Lua, "boundary").with_label(None, exact, exact, "exact");
        let wire = ShellErrorWire::from_shell(error).unwrap();
        assert_eq!(wire.labels[0].start, u32::MAX);
        assert_eq!(wire.labels[0].end, u32::MAX);
        let round_trip = wire.into_shell().unwrap();
        assert_eq!(round_trip.details.labels[0].start, exact);
        assert_eq!(round_trip.details.labels[0].end, exact);

        #[cfg(target_pointer_width = "64")]
        {
            let over_limit = usize::try_from(u64::from(u32::MAX) + 1).unwrap();
            let error = ShellError::new(ErrorCode::Lua, "overflow")
                .with_label(None, over_limit, over_limit, "rejected");
            let rejected = ShellErrorWire::from_shell(error).unwrap_err();
            assert_eq!(rejected.code, ErrorCode::Validation);
            assert!(rejected.message.contains("outside u32"));
        }
    }
}
