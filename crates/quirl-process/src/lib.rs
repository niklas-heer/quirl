//! Native command graph execution and background-job lifecycle.

#![cfg_attr(
    test,
    allow(
        dead_code_pub_in_binary,
        reason = "the libtest harness is an executable, but these public items remain library API"
    )
)]

mod builtin;
mod developer_context;
pub mod local_completion;

pub use developer_context::{DeveloperContextProbe, DeveloperContextSnapshot};

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    process::Command,
};

/// Maximum variables retained by one native executor's environment snapshot.
pub const SESSION_ENVIRONMENT_VARIABLES_MAX: usize = 65_536;
/// Maximum key and value bytes retained by one native executor's environment snapshot.
pub const SESSION_ENVIRONMENT_BYTES_MAX: usize = 16 * 1024 * 1024;
/// One observation delivered to an [`OutputObserver`] during native execution.
#[derive(Debug, Clone, Copy)]
pub enum ObservedActivity<'a> {
    /// One bounded retained-output chunk read from the foreground child.
    Output {
        /// Which stream the chunk was read from.
        stream: quirl_core::OutputStream,
        /// The chunk itself, read in at most 8 KiB pieces.
        bytes: &'a [u8],
    },
    /// A liveness heartbeat delivered on a bounded cadence while a foreground
    /// child is running but has produced no new output to report.
    ///
    /// Carries no payload: elapsed time is the observer's own concern (it
    /// already knows when the command started), and a fixed cadence keeps
    /// this a plain "still running" signal rather than a timing source
    /// callers might otherwise be tempted to accumulate against.
    Tick,
}

/// Callback invoked with one bounded retained-output chunk, or a liveness
/// tick, during native execution.
pub type OutputObserver<'a> =
    dyn FnMut(ObservedActivity<'_>) -> Result<(), quirl_core::ShellError> + 'a;

#[derive(Clone)]
pub(crate) struct SessionEnvironment {
    variables: BTreeMap<OsString, OsString>,
    initialization_error: Option<quirl_core::ShellError>,
    generation: u64,
}

impl Default for SessionEnvironment {
    fn default() -> Self {
        Self::capture(std::env::vars_os())
    }
}

impl SessionEnvironment {
    fn capture(variables: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        Self::capture_with_limits(
            variables,
            SESSION_ENVIRONMENT_VARIABLES_MAX,
            SESSION_ENVIRONMENT_BYTES_MAX,
        )
    }

    fn capture_with_limits(
        variables: impl IntoIterator<Item = (OsString, OsString)>,
        variables_max: usize,
        bytes_max: usize,
    ) -> Self {
        match Self::from_iter_with_limits(variables, variables_max, bytes_max) {
            Ok(variables) => Self {
                variables,
                initialization_error: None,
                generation: 0,
            },
            Err(error) => Self {
                variables: BTreeMap::new(),
                initialization_error: Some(error),
                generation: 0,
            },
        }
    }

    fn from_iter_with_limits(
        variables: impl IntoIterator<Item = (OsString, OsString)>,
        variables_max: usize,
        bytes_max: usize,
    ) -> Result<BTreeMap<OsString, OsString>, quirl_core::ShellError> {
        let mut captured = BTreeMap::new();
        let mut retained_bytes = 0_usize;
        for (name, value) in variables {
            retained_bytes = retained_bytes
                .saturating_add(name.len())
                .saturating_add(value.len());
            let observed_variables = captured.len().saturating_add(1);
            if observed_variables > variables_max {
                return Err(environment_variable_limit_error(
                    variables_max,
                    observed_variables,
                ));
            }
            if retained_bytes > bytes_max {
                return Err(environment_byte_limit_error(bytes_max, retained_bytes));
            }
            captured.insert(name, value);
        }
        Ok(captured)
    }

    fn ensure_valid(&self) -> Result<(), quirl_core::ShellError> {
        self.initialization_error.clone().map_or(Ok(()), Err)
    }

    fn configure(&self, command: &mut Command) -> Result<(), quirl_core::ShellError> {
        self.ensure_valid()?;
        command.env_clear().envs(&self.variables);
        Ok(())
    }

    fn value(&self, name: &str) -> String {
        self.variables
            .get(OsStr::new(name))
            .and_then(|value| value.to_str().map(str::to_owned))
            .unwrap_or_default()
    }

    fn resolve_executable(&self, program: &str) -> Option<std::path::PathBuf> {
        let path = self.variables.get(OsStr::new("PATH"))?;
        std::env::split_paths(path).find_map(|directory| {
            let candidate = directory.join(program);
            if candidate.is_file() {
                return Some(candidate);
            }
            #[cfg(windows)]
            {
                let candidate = directory.join(format!("{program}.exe"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
            None
        })
    }

    fn set_variables(
        &mut self,
        assignments: &[(String, String)],
    ) -> Result<(), quirl_core::ShellError> {
        self.ensure_valid()?;
        let mut staged = self.variables.clone();
        for (name, value) in assignments {
            validate_environment_assignment(name, value)?;
            staged.insert(OsString::from(name), OsString::from(value));
        }
        let retained_bytes = staged.iter().fold(0_usize, |retained, (name, value)| {
            retained
                .saturating_add(name.len())
                .saturating_add(value.len())
        });
        if staged.len() > SESSION_ENVIRONMENT_VARIABLES_MAX {
            return Err(environment_variable_limit_error(
                SESSION_ENVIRONMENT_VARIABLES_MAX,
                staged.len(),
            ));
        }
        if retained_bytes > SESSION_ENVIRONMENT_BYTES_MAX {
            return Err(environment_byte_limit_error(
                SESSION_ENVIRONMENT_BYTES_MAX,
                retained_bytes,
            ));
        }
        if staged != self.variables {
            let generation = self.generation.checked_add(1).ok_or_else(|| {
                quirl_core::ShellError::new(
                    quirl_core::ErrorCode::ResourceLimit,
                    "session environment has been updated too many times to track safely",
                )
                .with_help("Restart Quirl before applying another environment update")
            })?;
            self.variables = staged;
            self.generation = generation;
        }
        Ok(())
    }
}

fn environment_variable_limit_error(limit: usize, observed: usize) -> quirl_core::ShellError {
    quirl_core::ShellError::new(
        quirl_core::ErrorCode::ResourceLimit,
        "session environment exceeds its variable limit",
    )
    .with_context(format!("limit {limit} variables; observed {observed}"))
    .with_help("Remove unused variables or restart the session with a smaller environment")
}

fn environment_byte_limit_error(limit: usize, observed: usize) -> quirl_core::ShellError {
    quirl_core::ShellError::new(
        quirl_core::ErrorCode::ResourceLimit,
        "session environment exceeds its retained-byte limit",
    )
    .with_context(format!("limit {limit} bytes; observed {observed}"))
    .with_help("Shorten exported values or restart the session with a smaller environment")
}

fn validate_environment_assignment(name: &str, value: &str) -> Result<(), quirl_core::ShellError> {
    let mut characters = name.chars();
    let valid_name = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if !valid_name {
        return Err(quirl_core::ShellError::new(
            quirl_core::ErrorCode::InvalidArgument,
            format!("invalid environment name `{name}`"),
        )
        .with_help("Environment names use ASCII letters, digits, and underscores"));
    }
    if value.contains('\0') {
        return Err(interior_nul_error("environment value"));
    }
    Ok(())
}

fn noninteractive_process_error(source: &str, message: &str) -> quirl_core::ShellError {
    quirl_core::ShellError::new(quirl_core::ErrorCode::InvalidArgument, message)
        .with_command(source)
        .with_help("Run one foreground command without job-control or session-state built-ins")
}

/// Version of the serialized native runner contract.
pub const RUNNER_PROTOCOL_VERSION: u32 = 2;
/// Historical runner version retained as fail-closed compatibility evidence.
pub const RUNNER_PROTOCOL_VERSION_V1: u32 = 1;
/// Default bytes retained for final stdout and, separately, aggregate stderr
/// when a caller does not provide a tighter sandboxed-process budget.
pub const DEFAULT_CAPTURE_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes accepted by one native command-list execution.
pub const NATIVE_COMMAND_BYTES_MAX: usize = 1024 * 1024;
/// Maximum pipelines in one native command list.
pub const NATIVE_PIPELINES_MAX: usize = 256;
/// Maximum command stages in one native pipeline.
pub const NATIVE_PIPELINE_STAGES_MAX: usize = 64;
/// Maximum bytes written for one here-string, including its trailing newline.
pub const HERE_STRING_BYTES_MAX: usize = 256 * 1024;
/// Maximum bytes retained by expansion for one native pipeline.
pub const EXPANSION_BYTES_MAX: usize = NATIVE_COMMAND_BYTES_MAX;
/// Maximum bytes parsed by one arithmetic expansion.
pub const ARITHMETIC_SOURCE_BYTES_MAX: usize = 16 * 1024;
/// Maximum nested unary/parenthesized arithmetic expressions.
pub const ARITHMETIC_DEPTH_MAX: usize = 64;
/// Maximum active and completed job records retained by one executor.
pub const RETAINED_JOBS_MAX: usize = 1024;
/// Historical runner-v1 descriptor retained verbatim for compatibility review.
pub const RUNNER_SCHEMA_DESCRIPTOR_V1: &str = "quirl.runner@1{input:quirl.command-grammar@1,native-source-bytes<=1048576,native-pipelines<=256,native-stages-per-pipeline<=64,here-string-bytes-including-newline<=262144,arithmetic-source-bytes<=16384,arithmetic-depth<=64;ProcessBackend{execute_capture(source)->CommandOutcome;execute_interactive(source)->CommandOutcome;jobs()->array<JobState>;foreground_job(id)->JobState;cancel_job(id)->JobState;suspend_job(id)->JobState};JobState{deny_unknown;id:u32;command:string;status:running|stopped|done;process_group:null|i32;exit_status:null|i32};CommandOutcome{status:i32;stdout:null|string;stderr:null|string};capture:default-retained-per-stream=1048576|caller-tighter,drain-excess-then-ResourceLimit-with-retained-and-discarded-byte-context;interactive:inherit-streams-without-retention-limit;byte-pipeline:ordered;redirection:input|output|append|here-string;background:terminal-ampersand;cancel-status:130;errors:ShellError;platform:suspend-unavailable-on-windows}";
/// Canonical descriptor hashed to identify the native runner contract.
pub const RUNNER_SCHEMA_DESCRIPTOR: &str = "quirl.runner@2{input:quirl.command-grammar@2,native-source-bytes<=1048576,native-pipelines<=256,native-stages-per-pipeline<=64,retained-jobs<=1024,job-id:u32-nonzero-wrap-skip-visible,here-string-bytes-including-newline<=262144,arithmetic-source-bytes<=16384,arithmetic-depth<=64;ProcessBackend{execute(source)->CommandOutcome;execute_capture(source)->CommandOutcome;jobs()->array<JobState>;cancel_job(id)->JobState;suspend_job(id)->JobState};JobState{deny_unknown;id:u32-nonzero;command:string;status:running|stopped|done;process_group:null|i32;exit_status:null|i32};CommandOutcome{status:i32;stdout:null|string;stderr:null|string};capture:default-retained-per-stream=1048576|caller-tighter,drain-excess-then-ResourceLimit-with-retained-and-discarded-byte-context;interactive:inherit-streams-without-retention-limit;byte-pipeline:ordered;redirection:input-source-order-last-wins|output|append|here-string;background:terminal-ampersand;cancel-status:130;errors:ShellError;platform:suspend-unavailable-on-windows;compatibility:frozen-major-v1-fails-closed}";

/// Historical encoded runner-v1 job fixture retained for compatibility evidence.
pub const RUNNER_JOB_STATE_FIXTURE_V1: &str =
    r#"{"id":1,"command":"sleep 1 &","status":"running","process_group":123,"exit_status":null}"#;
/// Current encoded runner-v2 job fixture used to verify the authoritative shape.
pub const RUNNER_JOB_STATE_FIXTURE: &str =
    r#"{"id":1,"command":"sleep 1 &","status":"running","process_group":123,"exit_status":null}"#;

/// Return the deterministic fingerprint of [`RUNNER_SCHEMA_DESCRIPTOR`].
pub fn runner_schema_hash() -> String {
    quirl_core::schema_fingerprint(RUNNER_SCHEMA_DESCRIPTOR)
}

/// Validate a runner protocol identity at an authoritative reader boundary.
///
/// Runner v1 cannot be migrated because its descriptor names a stale grammar
/// and methods that never matched the public backend trait. It therefore fails
/// closed with an actionable version error, as do unknown future versions.
pub fn validate_runner_protocol_version(version: u32) -> Result<(), quirl_core::ShellError> {
    if version == RUNNER_PROTOCOL_VERSION {
        return Ok(());
    }
    let relation = if version < RUNNER_PROTOCOL_VERSION {
        "expired"
    } else {
        "future"
    };
    Err(quirl_core::ShellError::new(
        quirl_core::ErrorCode::Validation,
        format!("{relation} runner protocol version {version}"),
    )
    .with_context(format!(
        "supported runner protocol version: {RUNNER_PROTOCOL_VERSION}"
    ))
    .with_help("Use a client and Quirl build that both implement runner protocol v2"))
}

fn allocate_job_id(next_job_id: &mut u32, visible_ids: &[u32]) -> u32 {
    let mut candidate = (*next_job_id).max(1);
    for _ in 0..=visible_ids.len() {
        if !visible_ids.contains(&candidate) {
            *next_job_id = candidate.checked_add(1).unwrap_or(1);
            return candidate;
        }
        candidate = candidate.checked_add(1).unwrap_or(1);
    }
    // The retained table is much smaller than the nonzero u32 ID space, so a
    // free candidate must exist after at most visible_ids.len() + 1 probes.
    unreachable!("bounded job id search exhausted the nonzero u32 space")
}

fn validate_native_source(input: &str) -> Result<(), quirl_core::ShellError> {
    if input.contains('\0') {
        return Err(interior_nul_error("native command source"));
    }
    if input.len() <= NATIVE_COMMAND_BYTES_MAX {
        return Ok(());
    }
    Err(quirl_core::ShellError::new(
        quirl_core::ErrorCode::ResourceLimit,
        "native command source exceeds its byte limit",
    )
    .with_context(format!(
        "limit {NATIVE_COMMAND_BYTES_MAX} bytes; observed {} bytes",
        input.len()
    ))
    .with_help("Split the command list into smaller commands or move large input to a file"))
}

fn interior_nul_error(context: &str) -> quirl_core::ShellError {
    quirl_core::ShellError::new(
        quirl_core::ErrorCode::InvalidCommand,
        "native command expansion contains an interior NUL byte",
    )
    .with_context(context)
    .with_help("Remove the NUL byte; operating-system arguments and paths cannot contain NUL")
}

fn validate_native_plan(graph: &quirl_syntax::CommandList) -> Result<(), quirl_core::ShellError> {
    if graph.pipelines.len() > NATIVE_PIPELINES_MAX {
        return Err(quirl_core::ShellError::new(
            quirl_core::ErrorCode::ResourceLimit,
            "native command list exceeds its pipeline limit",
        )
        .with_context(format!(
            "limit {NATIVE_PIPELINES_MAX} pipelines; observed {} pipelines",
            graph.pipelines.len()
        ))
        .with_help("Split the command list into smaller commands"));
    }
    for (index, pipeline) in graph.pipelines.iter().enumerate() {
        if pipeline.commands.len() > NATIVE_PIPELINE_STAGES_MAX {
            return Err(quirl_core::ShellError::new(
                quirl_core::ErrorCode::ResourceLimit,
                "native pipeline exceeds its stage limit",
            )
            .with_context(format!(
                "limit {NATIVE_PIPELINE_STAGES_MAX} stages; pipeline {} has {} stages",
                index + 1,
                pipeline.commands.len()
            ))
            .with_help("Split the pipeline or use intermediate files"));
        }
        for command in &pipeline.commands {
            if command
                .redirects
                .iter()
                .any(|redirect| redirect.kind == quirl_syntax::RedirectKind::DuplicateInput)
            {
                return Err(quirl_core::ShellError::new(
                    quirl_core::ErrorCode::InvalidCommand,
                    "native C1 does not support input-descriptor duplication",
                )
                .with_help("Use an input file, a here-string, or an explicit Bash/Zsh island"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod simulation_support {
    /// Default generated lifecycle cases used by the deterministic test suite.
    pub const DEFAULT_SIMULATION_CASES: usize = 128;
    /// Default reproducible seed used by lifecycle simulations.
    pub const DEFAULT_SIMULATION_SEED: u64 = 7_640_891_576_956_012_809;
    /// Maximum generated lifecycle cases accepted from the environment.
    pub const SIMULATION_CASES_MAX: usize = 10_000;

    /// Small deterministic generator used only for reproducible test schedules.
    pub struct DeterministicRng(u64);

    impl DeterministicRng {
        /// Initialize the generator, mapping zero to a fixed nonzero state.
        pub fn new(seed: u64) -> Self {
            // Xorshift cannot advance from zero, so map that valid CLI seed to
            // a fixed non-zero state while preserving deterministic replay.
            Self(if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            })
        }

        /// Return an index in `0..upper` and advance the deterministic state.
        ///
        /// # Panics
        ///
        /// Panics when `upper` is zero.
        pub fn index(&mut self, upper: usize) -> usize {
            assert!(upper > 0);
            let mut value = self.0;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.0 = value;
            let upper = u64::try_from(upper).unwrap();
            usize::try_from(value % upper).unwrap()
        }
    }

    /// Read the bounded seed and case count used by the test harness.
    pub fn configuration() -> (u64, usize) {
        let seed = std::env::var("QUIRL_TEST_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(DEFAULT_SIMULATION_SEED);
        let cases = std::env::var("QUIRL_TEST_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|cases| (1..=SIMULATION_CASES_MAX).contains(cases))
            .unwrap_or(DEFAULT_SIMULATION_CASES);
        (seed, cases)
    }
}

#[cfg(unix)]
mod platform {
    use super::{
        ARITHMETIC_DEPTH_MAX, ARITHMETIC_SOURCE_BYTES_MAX, DEFAULT_CAPTURE_BYTES,
        EXPANSION_BYTES_MAX, HERE_STRING_BYTES_MAX, ObservedActivity, OutputObserver,
        RETAINED_JOBS_MAX, SessionEnvironment, allocate_job_id, builtin, validate_native_plan,
        validate_native_source,
    };

    use nix::{
        errno::Errno,
        sys::{
            signal::{SigSet, SigmaskHow, Signal, kill, killpg, pthread_sigmask},
            termios::{SetArg, Termios, tcgetattr, tcsetattr},
            wait::{WaitPidFlag, WaitStatus, waitpid},
        },
        unistd::{Pid, getpgid, setpgid, tcgetpgrp, tcsetpgrp},
    };
    use os_pipe::{PipeReader, PipeWriter, pipe};
    use quirl_core::{CommandOutcome, ErrorCode, OutputStream, ProcessRequest, ShellError};
    use quirl_syntax::{
        ListConnector, Pipeline, Quoting, RedirectKind, SimpleCommand, Word, parse_command_list,
    };
    use serde::{Deserialize, Serialize};
    #[cfg(test)]
    use std::env;
    use std::{
        cell::{Cell, RefCell},
        fs::{File, OpenOptions},
        io::{ErrorKind, IsTerminal, Read, Write},
        path::{Path, PathBuf},
        process::{Child, ChildStdin, ChildStdout, Command, Stdio},
        sync::{
            Arc, Mutex, MutexGuard, OnceLock, TryLockError,
            atomic::{AtomicUsize, Ordering},
            mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel, sync_channel},
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    use std::os::unix::{
        ffi::OsStrExt,
        process::{CommandExt, ExitStatusExt},
    };

    /// Aggregate lifecycle state for a native job.
    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum JobStatus {
        /// At least one child is executing.
        Running,
        /// Every live child is stopped.
        Stopped,
        /// Every child has exited or been reaped.
        Done,
    }

    /// Serializable snapshot of a native job and its process group.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct JobState {
        /// Stable session-local job identifier.
        pub id: u32,
        /// Original command source associated with the job.
        pub command: String,
        /// Aggregate lifecycle state of the job's children.
        pub status: JobStatus,
        /// Operating-system process-group identifier, when one exists.
        pub process_group: Option<i32>,
        /// Final shell status after the job reaches [`JobStatus::Done`].
        pub exit_status: Option<i32>,
    }

    struct Job {
        state: JobState,
        children: Vec<JobChild>,
        process_group_anchor: Option<ProcessGroupAnchor>,
        terminal_modes: Option<Termios>,
        capture: bool,
        stdout_reader: Option<ReaderTask>,
        stderr_readers: Vec<ReaderTask>,
        writers: Vec<WriterTask>,
    }

    struct JobChild {
        child: Child,
        status: JobStatus,
        exit_status: Option<i32>,
    }

    struct ReaderCapture {
        bytes: Vec<u8>,
        discarded_bytes: u64,
    }

    struct OutputEvent {
        stream: OutputStream,
        bytes: Vec<u8>,
    }

    /// Minimum spacing between two [`ObservedActivity::Tick`] deliveries to
    /// one observer, so a spinner or elapsed-time display animates smoothly
    /// without redrawing far faster than a terminal or a human eye needs.
    const OBSERVER_TICK_INTERVAL: Duration = Duration::from_millis(100);

    struct OutputObserverHandle<'a> {
        callback: RefCell<&'a mut OutputObserver<'a>>,
        last_tick: Cell<Instant>,
    }

    impl OutputObserverHandle<'_> {
        /// Deliver a [`ObservedActivity::Tick`] if at least
        /// [`OBSERVER_TICK_INTERVAL`] has passed since the last one.
        ///
        /// Called every turn of the bounded foreground-wait loop, which is
        /// itself already cancellation-checked, so this stays a plain
        /// elapsed-time comparison rather than a timer needing its own
        /// cleanup or cancellation path.
        fn maybe_tick(&self) -> Result<(), ShellError> {
            let now = Instant::now();
            if now.duration_since(self.last_tick.get()) < OBSERVER_TICK_INTERVAL {
                return Ok(());
            }
            self.last_tick.set(now);
            (self.callback.borrow_mut())(ObservedActivity::Tick)
        }
    }

    struct CaptureBudget {
        limit: usize,
        retained: AtomicUsize,
    }

    impl CaptureBudget {
        fn new(limit: usize) -> Self {
            Self {
                limit,
                retained: AtomicUsize::new(0),
            }
        }

        fn claim(&self, requested: usize) -> usize {
            let mut retained = self.retained.load(Ordering::Relaxed);
            loop {
                let claimed = requested.min(self.limit.saturating_sub(retained));
                match self.retained.compare_exchange_weak(
                    retained,
                    retained + claimed,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return claimed,
                    Err(observed) => retained = observed,
                }
            }
        }
    }

    struct ExpansionBudget {
        retained_bytes: usize,
    }

    impl ExpansionBudget {
        fn new() -> Self {
            Self { retained_bytes: 0 }
        }

        fn claim(&mut self, next: &str) -> Result<(), ShellError> {
            if next.contains('\0') {
                return Err(super::interior_nul_error(
                    "expanded command word or redirect target",
                ));
            }
            let observed_bytes = self.retained_bytes.saturating_add(next.len());
            if observed_bytes > EXPANSION_BYTES_MAX {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "native pipeline expansion exceeds its retained-byte limit",
                )
                .with_context(format!(
                    "limit {EXPANSION_BYTES_MAX} bytes; retained {} bytes; observed {observed_bytes} bytes",
                    self.retained_bytes
                ))
                .with_help("Use fewer expansions, shorten environment values, or move data to a file"));
            }
            self.retained_bytes = observed_bytes;
            Ok(())
        }

        fn append(&mut self, output: &mut String, next: &str) -> Result<(), ShellError> {
            self.claim(next)?;
            output.push_str(next);
            Ok(())
        }
    }

    type ReaderTask = JoinHandle<std::io::Result<ReaderCapture>>;
    type WriterTask = JoinHandle<std::io::Result<()>>;
    type PendingWriter = (PipeWriter, Vec<u8>);
    type OutputStdio = (Stdio, Option<PipeReader>, Option<PipeWriter>, Option<File>);
    const FOREGROUND_TERMINAL_LEASE_WAIT_MAX: Duration = Duration::from_secs(30);
    const PROCESS_GROUP_ANCHOR_STARTUP_WAIT_MAX: Duration = Duration::from_secs(2);
    const PROCESS_GROUP_ANCHOR_STATUS_EVENTS_PER_TURN_MAX: usize = 16;
    const PROCESS_GROUP_ANCHOR_PATH: &str = "/bin/sh";
    const PROCESS_GROUP_ANCHOR_READY: u8 = b'R';
    const PROCESS_GROUP_ANCHOR_SCRIPT: &str =
        "trap '' HUP INT QUIT TERM TTIN TTOU; trap - TSTP; printf R; IFS= read -r _";
    const PROCESS_GROUP_LEADER_STAGE_SCRIPT: &str =
        "command -v \"$1\" >/dev/null 2>&1 || exit 127; kill -STOP $$; exec \"$@\"";

    struct PreparedInput {
        stdio: Stdio,
        writer: Option<PendingWriter>,
    }

    #[derive(Clone, Copy)]
    struct RequestContext<'a> {
        request: &'a ProcessRequest,
        deadline: Instant,
    }

    impl<'a> RequestContext<'a> {
        fn new(request: &'a ProcessRequest) -> Result<Self, ShellError> {
            let deadline = Instant::now()
                .checked_add(request.deadline)
                .ok_or_else(|| {
                    ShellError::new(
                        ErrorCode::ResourceLimit,
                        "process execution deadline is outside the platform range",
                    )
                    .with_context(format!("requested duration: {:?}", request.deadline))
                    .with_help("Use a finite process deadline supported by this platform")
                })?;
            Ok(Self { request, deadline })
        }

        fn ensure_active(self) -> Result<(), ShellError> {
            let cancelled = self.request.cancelled.load(Ordering::Relaxed);
            if !cancelled && Instant::now() < self.deadline {
                return Ok(());
            }
            let message = if cancelled {
                "process execution was cancelled"
            } else {
                "process execution exceeded its deadline"
            };
            Err(ShellError::new(ErrorCode::ResourceLimit, message)
                .with_help("Use a shorter-running command or increase the Lua policy deadline"))
        }
    }

    /// Unix native process executor with owned foreground and background jobs.
    pub struct NativeExecutor {
        jobs: Vec<Job>,
        next_job_id: u32,
        substitution_depth: u8,
        noninteractive_host: bool,
        environment: SessionEnvironment,
        #[cfg(test)]
        fail_stopped_terminal_mode_read: bool,
    }

    /// Cross-platform containment hook for a directly spawned child process.
    pub struct ChildProcessTree {
        process_group: i32,
        state: Mutex<ContainedProcessGroup>,
    }

    enum ContainedProcessGroup {
        Unassigned(ProcessGroupAnchor),
        Assigned(ProcessGroupAnchor),
        Released,
    }

    impl ChildProcessTree {
        /// Create a verified Unix process-group anchor.
        ///
        /// The anchor adds one direct child and fails if absolute `/bin/sh`
        /// cannot complete its bounded readiness handshake.
        pub fn new() -> Result<Self, ShellError> {
            let anchor = ProcessGroupAnchor::spawn()?;
            Ok(Self {
                process_group: anchor.process_group(),
                state: Mutex::new(ContainedProcessGroup::Unassigned(anchor)),
            })
        }

        /// Configure `command` to join the already-owned process group before
        /// any guest code can run.
        pub fn configure(&self, command: &mut Command) {
            command.process_group(self.process_group);
        }

        /// Verify that `child` joined this object's anchored process group.
        ///
        /// Callers must apply [`Self::configure`] before spawning. Assignment
        /// fails closed and does not attempt to move already-running guest code.
        pub fn assign(&self, child: &mut Child) -> Result<(), ShellError> {
            let process_id = i32::try_from(child.id()).map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "child process id is outside the platform range",
                )
                .with_context(error.to_string())
                .with_help("Report this platform-specific process error")
            })?;
            let verification = verify_process_group(child, process_id, self.process_group)?;
            let mut state = self.state.lock().map_err(|_| {
                ShellError::new(ErrorCode::Io, "process containment state is unavailable")
                    .with_help("Restart Quirl before launching another contained process")
            })?;
            let _ = verification;
            *state = match std::mem::replace(&mut *state, ContainedProcessGroup::Released) {
                ContainedProcessGroup::Unassigned(anchor) => {
                    ContainedProcessGroup::Assigned(anchor)
                }
                previous => {
                    *state = previous;
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        "process containment object already owns a child",
                    )
                    .with_help("Create one containment object for each direct child tree"));
                }
            };
            Ok(())
        }

        /// Terminate the anchored group and directly spawned child if live.
        ///
        /// The anchor is reaped after the only group signal; this object never
        /// addresses the released numeric process-group identifier again.
        pub fn terminate(&self, child: &mut Child) -> Result<(), ShellError> {
            let mut anchor = self.state.lock().ok().and_then(|mut state| {
                match std::mem::replace(&mut *state, ContainedProcessGroup::Released) {
                    ContainedProcessGroup::Unassigned(anchor)
                    | ContainedProcessGroup::Assigned(anchor) => Some(anchor),
                    ContainedProcessGroup::Released => None,
                }
            });
            let group_signal = anchor.as_mut().map(ProcessGroupAnchor::begin_termination);
            let child_result = child.kill();
            let group_result = anchor.map(|mut anchor| {
                let group_signal = group_signal.unwrap_or(Ok(()));
                anchor.finish_termination(group_signal)
            });
            let group_ok = matches!(group_result, None | Some(Ok(())));
            let child_ok = match &child_result {
                Ok(()) => true,
                Err(error) => error.kind() == ErrorKind::InvalidInput,
            };
            if group_ok && child_ok {
                Ok(())
            } else {
                let context = group_result.and_then(Result::err).map_or_else(
                    || {
                        child_result.err().map_or_else(
                            || "unknown termination failure".to_owned(),
                            |error| error.to_string(),
                        )
                    },
                    |error| error.to_string(),
                );
                Err(ShellError::new(
                    ErrorCode::ProcessSpawn,
                    "could not terminate contained child process tree",
                )
                .with_context(context)
                .with_help("Retry the command; report repeated process termination failures"))
            }
        }
    }

    #[derive(Debug)]
    struct ProcessGroupAnchor {
        child: Child,
        keepalive: Option<ChildStdin>,
        process_group: i32,
        termination_signaled: bool,
        stopped: bool,
        released: bool,
    }

    impl ProcessGroupAnchor {
        fn spawn() -> Result<Self, ShellError> {
            Self::spawn_with_group(
                PROCESS_GROUP_ANCHOR_PATH,
                PROCESS_GROUP_ANCHOR_SCRIPT,
                None,
                None,
            )
        }

        #[cfg(test)]
        fn spawn_with(path: &str, script: &str) -> Result<Self, ShellError> {
            Self::spawn_with_group(path, script, None, None)
        }

        fn join(
            process_group: i32,
            request: Option<RequestContext<'_>>,
        ) -> Result<Self, ShellError> {
            Self::spawn_with_group(
                PROCESS_GROUP_ANCHOR_PATH,
                PROCESS_GROUP_ANCHOR_SCRIPT,
                Some(process_group),
                request,
            )
        }

        fn spawn_with_group(
            path: &str,
            script: &str,
            process_group: Option<i32>,
            request: Option<RequestContext<'_>>,
        ) -> Result<Self, ShellError> {
            let (child, keepalive, readiness, process_group) =
                spawn_anchor_process(path, script, process_group)?;
            let mut anchor = Self {
                child,
                keepalive: Some(keepalive),
                process_group,
                termination_signaled: false,
                stopped: false,
                released: false,
            };
            anchor.await_readiness(readiness, request)?;
            Ok(anchor)
        }

        fn await_readiness(
            &mut self,
            mut readiness: ChildStdout,
            request: Option<RequestContext<'_>>,
        ) -> Result<(), ShellError> {
            let (sender, receiver) = sync_channel(1);
            let reader = thread::spawn(move || {
                let mut byte = [0_u8; 1];
                let result = readiness.read_exact(&mut byte).map(|()| byte[0]);
                let _ = sender.send(result);
            });
            let readiness_result = receive_anchor_readiness(&receiver, request);
            if !matches!(readiness_result, Ok(Ok(PROCESS_GROUP_ANCHOR_READY))) {
                let cleanup = self.terminate();
                let _ = reader.join();
                let mut error = anchor_readiness_error(readiness_result);
                if let Err(cleanup) = cleanup {
                    error = error.with_context(format!("anchor cleanup: {}", cleanup.message));
                }
                return Err(error);
            }
            if reader.join().is_err() {
                let _ = self.terminate();
                return Err(ShellError::new(
                    ErrorCode::Io,
                    "process-group anchor readiness task failed",
                )
                .with_help("Retry the command; report repeated anchor startup failures"));
            }
            Ok(())
        }

        fn process_group(&self) -> i32 {
            self.process_group
        }

        fn signal(&self, signal: Signal) -> Result<(), Errno> {
            debug_assert!(!self.released, "released process-group anchor was signaled");
            killpg(Pid::from_raw(self.process_group), signal)
        }

        fn poll_stopped(&mut self) -> Result<bool, ShellError> {
            if self.released {
                return Err(ShellError::new(
                    ErrorCode::Io,
                    "process-group anchor exited before foreground cleanup",
                )
                .with_help("Retry the command; report repeated anchor lifecycle failures"));
            }
            let process_id = Pid::from_raw(i32::try_from(self.child.id()).map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "process-group anchor id is outside the platform range",
                )
                .with_context(error.to_string())
                .with_help("Report this platform-specific process error")
            })?);
            for _ in 0..PROCESS_GROUP_ANCHOR_STATUS_EVENTS_PER_TURN_MAX {
                let status = waitpid(
                    process_id,
                    Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED | WaitPidFlag::WNOHANG),
                )
                .map_err(|error| {
                    ShellError::new(
                        ErrorCode::Io,
                        "could not observe process-group anchor state",
                    )
                    .with_context(error.to_string())
                    .with_help("Retry the command; report repeated anchor lifecycle failures")
                })?;
                match status {
                    WaitStatus::Stopped(_, _) => self.stopped = true,
                    WaitStatus::Continued(_) => self.stopped = false,
                    WaitStatus::Exited(_, code) => {
                        self.released = true;
                        self.keepalive.take();
                        return Err(ShellError::new(
                            ErrorCode::Io,
                            "process-group anchor exited before foreground cleanup",
                        )
                        .with_context(format!("exit status {code}"))
                        .with_help(
                            "Retry the command; report repeated anchor lifecycle failures",
                        ));
                    }
                    WaitStatus::Signaled(_, signal, _) => {
                        self.released = true;
                        self.keepalive.take();
                        return Err(ShellError::new(
                            ErrorCode::Io,
                            "process-group anchor exited before foreground cleanup",
                        )
                        .with_context(format!("signal {signal}"))
                        .with_help(
                            "Retry the command; report repeated anchor lifecycle failures",
                        ));
                    }
                    WaitStatus::StillAlive => return Ok(self.stopped),
                    #[cfg(any(target_os = "linux", target_os = "android"))]
                    WaitStatus::PtraceEvent(_, _, _) | WaitStatus::PtraceSyscall(_) => {
                        self.stopped = true;
                    }
                }
            }
            Ok(self.stopped)
        }

        fn terminate_owned_group(mut self) -> Result<(), ShellError> {
            let group_result = self.begin_termination();
            self.finish_termination(group_result)
        }

        fn terminate(&mut self) -> Result<(), ShellError> {
            if self.released {
                return Ok(());
            }
            let group_result = self.begin_termination();
            self.finish_termination(group_result)
        }

        fn begin_termination(&mut self) -> Result<(), Errno> {
            if self.released {
                return Err(Errno::ESRCH);
            }
            debug_assert!(
                !self.termination_signaled,
                "process group was signaled twice"
            );
            self.termination_signaled = true;
            self.signal(Signal::SIGKILL)
        }

        fn finish_termination(
            &mut self,
            group_result: Result<(), Errno>,
        ) -> Result<(), ShellError> {
            fn render_outcome<T, E: std::fmt::Display>(result: &Result<T, E>) -> String {
                match result {
                    Ok(_) => "ok".to_owned(),
                    Err(error) => error.to_string(),
                }
            }

            let child_result = self.child.kill();
            self.keepalive.take();
            let wait_result = self.child.wait();
            self.released = true;

            let group_ok = matches!(group_result, Ok(()) | Err(Errno::ESRCH));
            if group_ok && wait_result.is_ok() {
                return Ok(());
            }
            Err(
                ShellError::new(ErrorCode::Io, "could not terminate owned process group")
                    .with_context(format!(
                        "group={}; anchor_kill={}; anchor_wait={}",
                        render_outcome(&group_result),
                        render_outcome(&child_result),
                        render_outcome(&wait_result)
                    ))
                    .with_help(
                        "Retry the command; report repeated anchored-group cleanup failures",
                    ),
            )
        }
    }

    fn spawn_anchor_process(
        path: &str,
        script: &str,
        requested_group: Option<i32>,
    ) -> Result<(Child, ChildStdin, ChildStdout, i32), ShellError> {
        let mut command = Command::new(path);
        command
            .arg("-c")
            .arg(script)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(requested_group.unwrap_or(0));
        let mut child = command.spawn().map_err(|error| {
            ShellError::new(
                ErrorCode::ProcessSpawn,
                "could not start process-group anchor",
            )
            .with_context(format!("anchor path {path}: {error}"))
            .with_help("Restore /bin/sh and retry after reducing process pressure")
        })?;
        let child_id = i32::try_from(child.id()).map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            ShellError::new(
                ErrorCode::Io,
                "process-group anchor id is outside the platform range",
            )
            .with_context(error.to_string())
            .with_help("Report this platform-specific process error")
        })?;
        let process_group = requested_group.unwrap_or(child_id);
        let observed_group = getpgid(Some(Pid::from_raw(child_id)));
        if observed_group != Ok(Pid::from_raw(process_group)) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ShellError::new(
                ErrorCode::ProcessSpawn,
                "could not establish the owned process-group anchor",
            )
            .with_context(format!(
                "pid {child_id}; expected group {process_group}; observed group {}",
                observed_group.map_or_else(|e| format!("unavailable ({e})"), |g| g.to_string())
            ))
            .with_help("Retry the command; report repeated anchor construction failures"));
        }
        let keepalive = child
            .stdin
            .take()
            .ok_or_else(|| anchor_pipe_error(&mut child, "keepalive"))?;
        let readiness = child
            .stdout
            .take()
            .ok_or_else(|| anchor_pipe_error(&mut child, "readiness"))?;
        Ok((child, keepalive, readiness, process_group))
    }

    fn anchor_pipe_error(child: &mut Child, pipe: &str) -> ShellError {
        let _ = child.kill();
        let _ = child.wait();
        ShellError::new(
            ErrorCode::Io,
            format!("process-group anchor {pipe} pipe is unavailable"),
        )
        .with_help("Retry the command; report repeated anchor pipe failures")
    }

    type AnchorReadinessResult = Result<std::io::Result<u8>, ShellError>;

    fn receive_anchor_readiness(
        receiver: &Receiver<std::io::Result<u8>>,
        request: Option<RequestContext<'_>>,
    ) -> AnchorReadinessResult {
        let started = Instant::now();
        loop {
            if let Some(request) = request {
                request.ensure_active()?;
            }
            let remaining = PROCESS_GROUP_ANCHOR_STARTUP_WAIT_MAX.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "process-group anchor startup exceeded its limit",
                )
                .with_context(format!(
                    "limit {} ms",
                    PROCESS_GROUP_ANCHOR_STARTUP_WAIT_MAX.as_millis()
                ))
                .with_help("Retry after reducing system process pressure"));
            }
            match receiver.recv_timeout(remaining.min(Duration::from_millis(1))) {
                Ok(result) => return Ok(result),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(ShellError::new(
                        ErrorCode::Io,
                        "process-group anchor readiness task disconnected",
                    )
                    .with_help("Retry the command; report repeated anchor startup failures"));
                }
            }
        }
    }

    fn anchor_readiness_error(result: AnchorReadinessResult) -> ShellError {
        match result {
            Ok(Ok(byte)) => ShellError::new(
                ErrorCode::ProcessSpawn,
                "process-group anchor returned malformed readiness",
            )
            .with_context(format!("unexpected readiness byte {byte}"))
            .with_help("Retry the command; report repeated anchor startup failures"),
            Ok(Err(error)) => ShellError::new(
                ErrorCode::ProcessSpawn,
                "process-group anchor did not become ready",
            )
            .with_context(error.to_string())
            .with_help("Retry the command; report repeated anchor startup failures"),
            Err(error) => error,
        }
    }

    impl Drop for ProcessGroupAnchor {
        fn drop(&mut self) {
            let _ = self.terminate();
        }
    }

    impl Default for NativeExecutor {
        fn default() -> Self {
            Self {
                jobs: Vec::new(),
                next_job_id: 1,
                substitution_depth: 0,
                noninteractive_host: false,
                environment: SessionEnvironment::default(),
                #[cfg(test)]
                fail_stopped_terminal_mode_read: false,
            }
        }
    }

    impl Drop for NativeExecutor {
        fn drop(&mut self) {
            for job in &mut self.jobs {
                if job.state.status != JobStatus::Done {
                    terminate_children(&mut job.children, &mut job.process_group_anchor);
                }
                finish_job_tasks_silently(job);
            }
        }
    }

    impl NativeExecutor {
        pub(crate) fn noninteractive_host() -> Self {
            let mut executor = Self::default();
            executor.noninteractive_host = true;
            executor
        }

        /// Replace one variable in this executor's private environment snapshot.
        ///
        /// Future parameter expansions and child processes observe the update;
        /// the host process and independent executors remain unchanged.
        pub fn set_environment_variable(
            &mut self,
            name: String,
            value: String,
        ) -> Result<(), ShellError> {
            self.set_environment_variables(&[(name, value)])
        }

        /// Validate and atomically apply several private environment updates.
        pub fn set_environment_variables(
            &mut self,
            assignments: &[(String, String)],
        ) -> Result<(), ShellError> {
            self.environment.set_variables(assignments)
        }

        /// Apply this executor's complete environment snapshot to a child command.
        pub fn configure_child(&self, command: &mut Command) -> Result<(), ShellError> {
            self.environment.configure(command)
        }

        /// Snapshot this executor's private environment for background prompt probes.
        pub fn developer_context_probe(&self) -> crate::DeveloperContextProbe {
            crate::DeveloperContextProbe::new(self.environment.clone())
        }

        /// Generation of the private environment observed by future children.
        pub const fn environment_generation(&self) -> u64 {
            self.environment.generation
        }

        #[cfg(test)]
        pub(crate) fn replace_environment_for_test(&mut self, environment: SessionEnvironment) {
            self.environment = environment;
        }

        /// Execute an ordinary foreground command with terminal streams
        /// inherited. Unlike capture APIs, interactive output is not retained
        /// or rejected at the programmatic capture ceiling. This trusted-local
        /// convenience path has no host cancellation flag or deadline; hosted
        /// callers use [`Self::execute_interactive_request`].
        pub fn execute_interactive(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute(input)
        }

        /// Execute `input` in the foreground with inherited terminal streams.
        ///
        /// This trusted-local convenience path has no host cancellation flag
        /// or deadline; hosted callers use [`Self::execute_interactive_request`].
        pub fn execute(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, false)
        }

        /// Execute `input` while retaining bounded stdout and stderr.
        ///
        /// This trusted-local convenience path has no host cancellation flag
        /// or deadline; hosted callers use [`Self::execute_capture_request`].
        pub fn execute_capture(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, true)
        }

        /// Execute a foreground command under a host-provided cancellation,
        /// deadline, and retained-output budget.
        pub fn execute_capture_request(
            &mut self,
            request: ProcessRequest,
        ) -> Result<CommandOutcome, ShellError> {
            let context = RequestContext::new(&request)?;
            self.execute_inner_with_request(&request.command, true, Some(context))
        }

        /// Execute a captured foreground command while reporting bounded output chunks.
        ///
        /// The observer runs on the executor's owning thread after the complete process graph
        /// has been committed. Each chunk is at most 8 KiB and is also charged to the request's
        /// retained-output limit. Observer failure terminates and reaps the foreground graph
        /// before the error is returned.
        pub fn execute_capture_request_streaming(
            &mut self,
            request: ProcessRequest,
            observer: &mut OutputObserver<'_>,
        ) -> Result<CommandOutcome, ShellError> {
            let context = RequestContext::new(&request)?;
            let observer = OutputObserverHandle {
                callback: RefCell::new(observer),
                last_tick: Cell::new(Instant::now()),
            };
            self.execute_inner_with_observer(&request.command, true, Some(context), Some(&observer))
        }

        /// Execute a foreground command with inherited streams under the
        /// caller's cancellation and deadline. No stdout or stderr is retained.
        pub fn execute_interactive_request(
            &mut self,
            request: ProcessRequest,
        ) -> Result<CommandOutcome, ShellError> {
            let context = RequestContext::new(&request)?;
            self.execute_inner_with_request(&request.command, false, Some(context))
        }

        /// Refresh and return snapshots for every job owned by this executor.
        pub fn jobs(&mut self) -> Vec<JobState> {
            self.refresh_jobs();
            self.jobs.iter().map(|job| job.state.clone()).collect()
        }

        fn reserve_job_id(&mut self) -> Result<u32, ShellError> {
            self.refresh_jobs();
            self.reserve_refreshed_job_id()
        }

        fn reserve_refreshed_job_id(&mut self) -> Result<u32, ShellError> {
            if self.jobs.len() >= RETAINED_JOBS_MAX {
                self.jobs.retain(|job| job.state.status != JobStatus::Done);
            }
            if self.jobs.len() >= RETAINED_JOBS_MAX {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "native job table reached its retention limit",
                )
                .with_context(format!(
                    "limit {RETAINED_JOBS_MAX} jobs; observed {} live jobs",
                    self.jobs.len()
                ))
                .with_help(
                    "Finish or cancel an active job before starting another background job",
                ));
            }
            let visible_ids = self.jobs.iter().map(|job| job.state.id).collect::<Vec<_>>();
            Ok(allocate_job_id(&mut self.next_job_id, &visible_ids))
        }

        /// Terminate job `id`, reap its children, and return the final snapshot.
        pub fn cancel_job(&mut self, id: u32) -> Result<JobState, ShellError> {
            let job = self
                .jobs
                .iter_mut()
                .find(|job| job.state.id == id)
                .ok_or_else(|| {
                    ShellError::new(
                        ErrorCode::InvalidArgument,
                        format!("job %{id} does not exist"),
                    )
                    .with_help("Run `jobs` to list known jobs")
                })?;
            if job.state.status != JobStatus::Done {
                terminate_children(&mut job.children, &mut job.process_group_anchor);
                finish_job_tasks_silently(job);
                job.state.status = JobStatus::Done;
                job.state.exit_status = Some(130);
            }
            Ok(job.state.clone())
        }

        /// Stop every live process in job `id` and return its snapshot.
        pub fn suspend_job(&mut self, id: u32) -> Result<JobState, ShellError> {
            self.refresh_jobs();
            let job = self
                .jobs
                .iter_mut()
                .find(|job| job.state.id == id)
                .ok_or_else(|| {
                    ShellError::new(
                        ErrorCode::InvalidArgument,
                        format!("job %{id} does not exist"),
                    )
                    .with_help("Run `jobs` to list known jobs")
                })?;
            if job.state.status == JobStatus::Done {
                return Err(ShellError::new(
                    ErrorCode::InvalidArgument,
                    format!("job %{id} has already completed"),
                )
                .with_help("Start the command again to create a new job"));
            }
            suspend_running_children(job, id)?;
            Ok(job.state.clone())
        }

        fn execute_inner(
            &mut self,
            input: &str,
            capture: bool,
        ) -> Result<CommandOutcome, ShellError> {
            self.execute_inner_with_request(input, capture, None)
        }

        fn execute_inner_with_request(
            &mut self,
            input: &str,
            capture: bool,
            request: Option<RequestContext<'_>>,
        ) -> Result<CommandOutcome, ShellError> {
            self.execute_inner_with_observer(input, capture, request, None)
        }

        fn execute_inner_with_observer(
            &mut self,
            input: &str,
            capture: bool,
            request: Option<RequestContext<'_>>,
            observer: Option<&OutputObserverHandle<'_>>,
        ) -> Result<CommandOutcome, ShellError> {
            self.environment.ensure_valid()?;
            if let Some(request) = request {
                request.ensure_active()?;
            }
            validate_native_source(input)?;
            let graph = parse_command_list(input).map_err(|error| {
                ShellError::new(ErrorCode::InvalidCommand, error.message)
                    .with_label(
                        Some("command".to_owned()),
                        error.start,
                        error.end,
                        "syntax error",
                    )
                    .with_help(error.help)
                    .with_command(input)
            })?;
            validate_native_plan(&graph)?;
            if let Some(request) = request {
                request.ensure_active()?;
            }
            if graph.pipelines.is_empty() {
                return Ok(outcome(0, None, None));
            }

            let mut last = outcome(0, None, None);
            let mut captured_stdout = String::new();
            let mut captured_stderr = String::new();
            for (index, pipeline) in graph.pipelines.iter().enumerate() {
                if index > 0 {
                    let connector = graph.connectors[index - 1];
                    if (matches!(connector, ListConnector::And) && last.status != 0)
                        || (matches!(connector, ListConnector::Or) && last.status == 0)
                    {
                        continue;
                    }
                }
                last = self.execute_pipeline(
                    pipeline,
                    input,
                    capture,
                    request,
                    observer,
                    last.status,
                )?;
                if capture {
                    append_captured_output(
                        &mut captured_stdout,
                        last.stdout.as_deref().unwrap_or_default(),
                        retained_output_limit(request.map(|request| request.request)),
                    )?;
                    append_captured_output(
                        &mut captured_stderr,
                        last.stderr.as_deref().unwrap_or_default(),
                        retained_output_limit(request.map(|request| request.request)),
                    )?;
                }
            }
            if capture {
                last.stdout = Some(captured_stdout);
                last.stderr = Some(captured_stderr);
            }
            Ok(last)
        }

        fn execute_pipeline(
            &mut self,
            pipeline: &Pipeline,
            source: &str,
            capture: bool,
            request: Option<RequestContext<'_>>,
            observer: Option<&OutputObserverHandle<'_>>,
            previous_status: i32,
        ) -> Result<CommandOutcome, ShellError> {
            if let Some(request) = request {
                request.ensure_active()?;
            }
            let pipeline = self.expand_pipeline(pipeline, request, previous_status)?;
            let pipeline = &pipeline;
            if self.noninteractive_host && pipeline.background {
                return Err(super::noninteractive_process_error(
                    source,
                    "background execution is unavailable to isolated Lua",
                ));
            }
            if pipeline.commands.len() == 1 {
                if self.noninteractive_host
                    && pipeline.commands[0].words.first().is_some_and(|name| {
                        matches!(name.as_str(), "cd" | "export" | "jobs" | "fg" | "bg")
                    })
                {
                    return Err(super::noninteractive_process_error(
                        source,
                        "stateful and job-control built-ins are unavailable to isolated Lua",
                    ));
                }
                if pipeline.background
                    && pipeline.commands[0].words.first().is_some_and(|name| {
                        matches!(name.as_str(), "cd" | "export" | "jobs" | "fg" | "bg")
                    })
                {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        "stateful built-ins cannot run in the background",
                    )
                    .with_command(source)
                    .with_help("Run the built-in without `&`"));
                }
                if let Some(result) =
                    self.execute_control_builtin(&pipeline.commands[0], capture)?
                {
                    return Ok(result);
                }
            }
            self.spawn_pipeline(pipeline, source, capture, request, observer)
        }

        fn expand_pipeline(
            &mut self,
            pipeline: &Pipeline,
            request: Option<RequestContext<'_>>,
            previous_status: i32,
        ) -> Result<Pipeline, ShellError> {
            const MAX_SUBSTITUTION_BYTES: usize = 16 * 1024;
            if let Some(request) = request {
                request.ensure_active()?;
            }
            let mut expanded = pipeline.clone();
            let mut budget = ExpansionBudget::new();
            for command in &mut expanded.commands {
                let forms = command.word_ir.clone();
                if forms.is_empty() {
                    for word in &command.words {
                        budget.claim(word)?;
                    }
                    for redirect in &command.redirects {
                        budget.claim(&redirect.path)?;
                    }
                    continue;
                }
                let mut words = Vec::new();
                for word in &forms {
                    let (value, glob) = self.expand_word(
                        word,
                        MAX_SUBSTITUTION_BYTES,
                        request,
                        previous_status,
                        &mut budget,
                    )?;
                    let matches = if glob {
                        pathname_expand(&value, budget.retained_bytes)?
                    } else {
                        Vec::new()
                    };
                    if matches.is_empty() {
                        words.push(value);
                    } else {
                        for value in &matches {
                            budget.claim(value)?;
                        }
                        words.extend(matches);
                    }
                }
                command.words = words;
                for redirect in &mut command.redirects {
                    let (path, _) = self.expand_word(
                        &redirect.target,
                        MAX_SUBSTITUTION_BYTES,
                        request,
                        previous_status,
                        &mut budget,
                    )?;
                    redirect.path = path;
                }
            }
            Ok(expanded)
        }

        fn expand_word(
            &mut self,
            word: &Word,
            limit: usize,
            request: Option<RequestContext<'_>>,
            previous_status: i32,
            budget: &mut ExpansionBudget,
        ) -> Result<(String, bool), ShellError> {
            let mut value = String::new();
            let mut pathname = false;
            for part in &word.parts {
                if matches!(part.quoting, Quoting::Single | Quoting::Escaped) {
                    budget.append(&mut value, &part.text)?;
                    continue;
                }
                pathname |= part.quoting == Quoting::Unquoted
                    && part
                        .text
                        .chars()
                        .any(|character| matches!(character, '*' | '?' | '['));
                self.expand_fragment(
                    &part.text,
                    limit,
                    request,
                    previous_status,
                    &mut value,
                    budget,
                )?;
            }
            Ok((value, pathname))
        }

        fn expand_fragment(
            &mut self,
            text: &str,
            limit: usize,
            request: Option<RequestContext<'_>>,
            previous_status: i32,
            output: &mut String,
            budget: &mut ExpansionBudget,
        ) -> Result<(), ShellError> {
            let mut index = 0;
            while index < text.len() {
                let rest = &text[index..];
                if let Some(arithmetic) = rest.strip_prefix("$((") {
                    let Some(close) = matching_double_paren(arithmetic) else {
                        return Err(expansion_error(
                            "unclosed arithmetic expansion",
                            "Close the `))` in `$((...))`",
                        ));
                    };
                    let value = evaluate_arithmetic(&arithmetic[..close])?.to_string();
                    budget.append(output, &value)?;
                    index += 3 + close + 2;
                    continue;
                }
                if let Some(after) = rest.strip_prefix("$(") {
                    let Some(close) = matching_paren(after) else {
                        return Err(expansion_error(
                            "unclosed command substitution",
                            "Close the `)` in `$(...)`",
                        ));
                    };
                    let source = &after[..close];
                    if source.len() > limit {
                        return Err(expansion_error(
                            "command substitution exceeds its source budget",
                            "Keep `$(...)` below 16 KiB or use an explicit pipeline",
                        ));
                    }
                    const MAX_COMMAND_SUBSTITUTION_DEPTH: u8 = 8;
                    if self.substitution_depth >= MAX_COMMAND_SUBSTITUTION_DEPTH {
                        return Err(expansion_error(
                            "command substitution nesting exceeds the depth limit",
                            "Flatten nested substitutions or use an explicit pipeline",
                        ));
                    }
                    self.substitution_depth += 1;
                    let nested = self.execute_inner_with_request(source, true, request);
                    self.substitution_depth = self.substitution_depth.saturating_sub(1);
                    let nested = nested?;
                    let stdout = nested.stdout.unwrap_or_default();
                    if stdout.len() > limit {
                        return Err(expansion_error(
                            "command substitution exceeded its output budget",
                            "Write large output to a file before substituting it",
                        ));
                    }
                    budget.append(output, stdout.trim_end_matches('\n'))?;
                    index += 2 + close + 1;
                    continue;
                }
                if let Some(after) = rest.strip_prefix("${") {
                    let Some(close) = after.find('}') else {
                        return Err(expansion_error(
                            "unclosed parameter expansion",
                            "Close the `}` in `${...}`",
                        ));
                    };
                    let value = self.environment.value(&after[..close]);
                    budget.append(output, &value)?;
                    index += 3 + close;
                    continue;
                }
                if let Some(after) = rest.strip_prefix('$') {
                    let Some(character) = after.chars().next() else {
                        budget.append(output, "$")?;
                        break;
                    };
                    if character == '?' {
                        let value = previous_status.to_string();
                        budget.append(output, &value)?;
                        index += 2;
                        continue;
                    }
                    if character == '$' {
                        let value = std::process::id().to_string();
                        budget.append(output, &value)?;
                        index += 2;
                        continue;
                    }
                    if character == '_' || character.is_ascii_alphabetic() {
                        let length = after
                            .chars()
                            .take_while(|value| *value == '_' || value.is_ascii_alphanumeric())
                            .map(char::len_utf8)
                            .sum();
                        let value = self.environment.value(&after[..length]);
                        budget.append(output, &value)?;
                        index += 1 + length;
                        continue;
                    }
                }
                let character = rest.chars().next().unwrap_or_default();
                budget.append(output, &rest[..character.len_utf8()])?;
                index += character.len_utf8();
            }
            Ok(())
        }

        fn execute_control_builtin(
            &mut self,
            command: &SimpleCommand,
            capture: bool,
        ) -> Result<Option<CommandOutcome>, ShellError> {
            let Some(name) = command.words.first().map(String::as_str) else {
                return Ok(None);
            };
            if !matches!(name, "cd" | "export" | "jobs" | "fg" | "bg") {
                return Ok(None);
            }
            validate_control_redirects(command)?;
            let result = match name {
                "cd" => Some(builtin::execute_cd(&command.words)?),
                "export" => {
                    if command.words.len() == 1 {
                        return Err(ShellError::new(
                            ErrorCode::InvalidArgument,
                            "export needs at least one NAME=value assignment",
                        )
                        .with_help("Use `export NAME=value`"));
                    }
                    let mut assignments = Vec::with_capacity(command.words.len() - 1);
                    for assignment in command.words.iter().skip(1) {
                        let Some((name, value)) = assignment.split_once('=') else {
                            return Err(ShellError::new(
                                ErrorCode::InvalidArgument,
                                format!("invalid export assignment `{assignment}`"),
                            )
                            .with_help("Use `export NAME=value`"));
                        };
                        assignments.push((name.to_owned(), value.to_owned()));
                    }
                    self.environment.set_variables(&assignments)?;
                    Some(outcome(0, Some(String::new()), Some(String::new())))
                }
                "jobs" => {
                    let states = self.jobs();
                    let rendered = states
                        .iter()
                        .map(|job| {
                            format!(
                                "[{}] {:<7} {}",
                                job.id,
                                format!("{:?}", job.status).to_lowercase(),
                                job.command
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Some(outcome(0, Some(rendered), Some(String::new())))
                }
                "fg" => Some(self.foreground(parse_job_id(command)?)?),
                "bg" => Some(self.background(parse_job_id(command)?)?),
                _ => return Ok(None),
            };
            result
                .map(|result| finish_control_builtin(command, result, capture))
                .transpose()
        }

        fn spawn_pipeline(
            &mut self,
            pipeline: &Pipeline,
            source: &str,
            capture: bool,
            request: Option<RequestContext<'_>>,
            observer: Option<&OutputObserverHandle<'_>>,
        ) -> Result<CommandOutcome, ShellError> {
            // Foreground ownership is process-global. Acquire the lease before
            // the first child can inherit the controlling terminal, and keep it
            // until both the foreground process group and saved terminal modes
            // have been restored. Per-syscall locking would still allow another
            // executor to steal the terminal between handoff and restoration.
            let terminal_lease = if pipeline.background || self.noninteractive_host {
                ForegroundTerminalLease::none()
            } else {
                ForegroundTerminalLease::acquire(request)?
            };
            // Pipeline construction is a transaction. Local descriptors and
            // the guard own every partial resource until the complete process
            // group is either registered as a job or handed to the waiter.
            // Extension callbacks and other user code must stay outside this
            // window so an early child notification cannot observe half a job.
            let mut spawned = PipelineConstructionGuard::new();
            let mut previous_reader: Option<PipeReader> = None;
            let mut capture_reader = None;
            let mut stderr_readers = Vec::new();
            let mut pending_writers: Vec<PendingWriter> = Vec::new();
            let capture_streams = capture && !pipeline.background;
            let output_limit = retained_output_limit(request.map(|request| request.request));
            let stderr_budget = Arc::new(CaptureBudget::new(output_limit));
            let (output_sender, output_receiver) = if observer.is_some() {
                let (sender, receiver) = channel();
                (Some(sender), Some(receiver))
            } else {
                (None, None)
            };

            for (index, command) in pipeline.commands.iter().enumerate() {
                let last = index + 1 == pipeline.commands.len();
                if stderr_duplication_precedes_stdout_redirect(command) {
                    return Err(ShellError::new(
                        ErrorCode::InvalidCommand,
                        "native C1 cannot preserve `2>&1` before a later stdout file redirect",
                    )
                    .with_command(source)
                    .with_help(
                        "Use `> file 2>&1` to merge both streams into the file, or use an explicit Bash/Zsh island for ordered descriptor routing",
                    ));
                }
                let input = input_stdio(
                    command,
                    previous_reader.take(),
                    index > 0,
                    self.noninteractive_host,
                )?;
                let (stdout, next_reader, writer, redirected_stdout) =
                    output_stdio(command, last, capture_streams)?;
                if last && capture_streams {
                    capture_reader = next_reader;
                } else {
                    previous_reader = next_reader;
                }

                let executable = command
                    .words
                    .first()
                    .map(|word| word.strip_prefix('^').unwrap_or(word))
                    .ok_or_else(|| {
                        ShellError::new(
                            ErrorCode::InvalidCommand,
                            "a pipeline stage has no command name",
                        )
                        .with_command(source)
                        .with_help(
                            "Remove the empty stage, e.g. a stray `|` or `^` with nothing after it",
                        )
                    })?;
                let staged_group_leader = spawned.process_group.is_none();
                let mut process = if staged_group_leader {
                    let mut process = Command::new(PROCESS_GROUP_ANCHOR_PATH);
                    process
                        .arg("-c")
                        .arg(PROCESS_GROUP_LEADER_STAGE_SCRIPT)
                        .arg("quirl-process-group-stage")
                        .arg(executable);
                    process
                } else {
                    Command::new(executable)
                };
                self.configure_child(&mut process)?;
                process.args(command.words.iter().skip(1));
                let stderr = if command.redirects.iter().any(duplicates_stderr_to_stdout) {
                    if let Some(writer) = writer.as_ref() {
                        Stdio::from(writer.try_clone().map_err(|error| {
                            ShellError::new(
                                ErrorCode::Io,
                                "could not duplicate the pipeline output descriptor",
                            )
                            .with_context(error.to_string())
                            .with_help("Retry the command or use an explicit dialect island")
                        })?)
                    } else if let Some(file) = redirected_stdout {
                        Stdio::from(file)
                    } else if !capture_streams {
                        Stdio::inherit()
                    } else {
                        Stdio::piped()
                    }
                } else {
                    stderr_stdio(command, capture_streams)?
                };
                process.stdin(input.stdio).stdout(stdout).stderr(stderr);
                process.process_group(spawned.process_group.unwrap_or(0));
                let mut child = process.spawn().map_err(|error| {
                    ShellError::new(
                        ErrorCode::ProcessSpawn,
                        format!("could not start `{executable}`"),
                    )
                    .with_command(source)
                    .with_context(error.to_string())
                    .with_help(
                        "Check that the command exists on PATH, or use `help` to inspect built-ins",
                    )
                })?;
                let child_stderr = capture_streams.then(|| child.stderr.take()).flatten();
                if staged_group_leader {
                    spawned.push_staged_group_leader(child, executable, source, request)?;
                } else {
                    spawned.push(child)?;
                }
                if let Some(stderr) = child_stderr {
                    stderr_readers.push(spawn_reader_with_budget(
                        stderr,
                        Arc::clone(&stderr_budget),
                        output_sender
                            .clone()
                            .map(|sender| (sender, OutputStream::Stderr)),
                    ));
                }
                if let Some(writer) = input.writer {
                    pending_writers.push(writer);
                }
            }

            // Reserve background retention before writer threads start. A
            // full job table therefore unwinds only transaction-owned children
            // and descriptors, without detaching a writer task.
            let background_job_id = if pipeline.background {
                Some(self.reserve_job_id()?)
            } else {
                None
            };
            // Start writers only after every child exists. A pending writer is
            // owned by this construction transaction until then, so any spawn
            // failure closes the descriptor without leaving a detached task.
            let writers = pending_writers
                .into_iter()
                .map(|(mut writer, bytes)| thread::spawn(move || writer.write_all(&bytes)))
                .collect::<Vec<_>>();

            if pipeline.background {
                let Some(id) = background_job_id else {
                    unreachable!("background pipeline reserved no job id");
                };
                let process_group = spawned.process_group;
                let (children, process_group_anchor) = spawned.release();
                self.jobs.push(Job {
                    state: JobState {
                        id,
                        command: source.to_owned(),
                        status: JobStatus::Running,
                        process_group,
                        exit_status: None,
                    },
                    children,
                    process_group_anchor,
                    terminal_modes: None,
                    capture: false,
                    stdout_reader: None,
                    stderr_readers: Vec::new(),
                    writers,
                });
                return notification_outcome(
                    0,
                    format!("[{id}] {}", process_group.unwrap_or_default()),
                    capture,
                );
            }

            let process_group = spawned.process_group;
            let mut terminal = ForegroundTerminal::give_to(process_group, terminal_lease)?;
            let (mut children, mut process_group_anchor) = spawned.release();
            let stdout_reader = capture_reader.map(|reader| {
                spawn_reader_observed(
                    reader,
                    output_limit,
                    output_sender.map(|sender| (sender, OutputStream::Stdout)),
                )
            });
            let child_count = children.len();
            let wait_error = wait_for_foreground_children(
                &mut children,
                &mut process_group_anchor,
                request,
                observer,
                output_receiver.as_ref(),
            )
            .err();
            if let Some(error) = wait_error {
                terminate_children(&mut children, &mut process_group_anchor);
                let _ = terminal.restore();
                let _ = join_reader(stdout_reader, "pipeline output");
                let _ = join_readers(stderr_readers, "command error output");
                let _ = join_writers(writers);
                return Err(error);
            }
            let stopped = children
                .iter()
                .any(|child| child.status == JobStatus::Stopped);
            if !stopped {
                terminate_group_descendants(&mut process_group_anchor)?;
            }
            let stopped_terminal_modes = if stopped {
                match terminal.current_modes() {
                    Ok(modes) => modes,
                    Err(error) => {
                        terminate_children(&mut children, &mut process_group_anchor);
                        let _ = terminal.restore();
                        let _ = join_reader(stdout_reader, "pipeline output");
                        let _ = join_readers(stderr_readers, "command error output");
                        let _ = join_writers(writers);
                        return Err(error);
                    }
                }
            } else {
                None
            };
            terminal.restore()?;
            let status = children
                .get(child_count.saturating_sub(1))
                .and_then(|child| child.exit_status)
                .unwrap_or(0);
            if stopped {
                let id = match self.reserve_job_id() {
                    Ok(id) => id,
                    Err(error) => {
                        terminate_children(&mut children, &mut process_group_anchor);
                        let _ = join_reader(stdout_reader, "pipeline output");
                        let _ = join_readers(stderr_readers, "command error output");
                        let _ = join_writers(writers);
                        return Err(error);
                    }
                };
                self.jobs.push(Job {
                    state: JobState {
                        id,
                        command: source.to_owned(),
                        status: JobStatus::Stopped,
                        process_group,
                        exit_status: None,
                    },
                    children,
                    process_group_anchor,
                    terminal_modes: stopped_terminal_modes,
                    capture: capture_streams,
                    stdout_reader,
                    stderr_readers,
                    writers,
                });
                return notification_outcome(status, format!("[{id}] stopped {source}"), capture);
            }
            let stdout = join_reader(stdout_reader, "pipeline output");
            let stderr = join_readers(stderr_readers, "command error output");
            let writers = join_writers(writers);
            let stream = drain_output_events(observer, output_receiver.as_ref(), usize::MAX);
            let stdout = stdout?;
            let stderr = stderr?;
            writers?;
            stream?;
            Ok(outcome(
                status,
                capture.then_some(stdout),
                capture.then_some(stderr),
            ))
        }

        fn refresh_jobs(&mut self) {
            for job in &mut self.jobs {
                if job.state.status == JobStatus::Done {
                    continue;
                }
                for child in &mut job.children {
                    if child.status == JobStatus::Done {
                        continue;
                    }
                    poll_child(child);
                }
                let (status, exit_status) = super::summarize_job_lifecycle(
                    job.children
                        .iter()
                        .map(|child| (child.status, child.exit_status)),
                );
                job.state.status = status;
                job.state.exit_status = exit_status;
                if status == JobStatus::Done {
                    // Direct children can exit before descendants close capture
                    // and input pipes. Contain the group before joining tasks so
                    // refresh remains bounded even when cleanup itself fails.
                    let _ = terminate_group_descendants(&mut job.process_group_anchor);
                    finish_job_tasks_silently(job);
                }
            }
        }

        fn foreground(&mut self, id: Option<u32>) -> Result<CommandOutcome, ShellError> {
            self.refresh_jobs();
            let index = select_job(&self.jobs, id)?;
            let terminal_lease = ForegroundTerminalLease::acquire(None)?;
            let mut terminal =
                ForegroundTerminal::give_to(self.jobs[index].state.process_group, terminal_lease)?;
            terminal.apply_modes(self.jobs[index].terminal_modes.as_ref())?;
            if let Err(error) = resume_job(&self.jobs[index]) {
                let job = &mut self.jobs[index];
                terminate_children(&mut job.children, &mut job.process_group_anchor);
                finish_job_tasks_silently(job);
                job.state.status = JobStatus::Done;
                job.state.exit_status = Some(130);
                let _ = terminal.restore();
                return Err(error);
            }
            let mut job = self.jobs.remove(index);
            for child in &mut job.children {
                if child.status == JobStatus::Done {
                    continue;
                }
                child.status = JobStatus::Running;
                child.exit_status = None;
            }
            let result = (|| {
                wait_for_foreground_children(
                    &mut job.children,
                    &mut job.process_group_anchor,
                    None,
                    None,
                    None,
                )?;
                let status = job
                    .children
                    .last()
                    .and_then(|child| child.exit_status)
                    .unwrap_or(0);
                if job
                    .children
                    .iter()
                    .any(|child| child.status == JobStatus::Stopped)
                {
                    job.terminal_modes = self.stopped_terminal_modes(&terminal)?;
                    terminal.restore()?;
                    job.state.status = JobStatus::Stopped;
                    return Ok(None);
                }
                terminate_group_descendants(&mut job.process_group_anchor)?;
                terminal.restore()?;
                job.state.status = JobStatus::Done;
                job.state.exit_status = Some(status);
                let stdout = join_reader(job.stdout_reader.take(), "pipeline output")?;
                let stderr = join_readers(
                    std::mem::take(&mut job.stderr_readers),
                    "command error output",
                )?;
                join_writers(std::mem::take(&mut job.writers))?;
                Ok(Some(outcome(
                    status,
                    job.capture.then_some(stdout),
                    job.capture.then_some(stderr),
                )))
            })();
            match result {
                Ok(Some(result)) => Ok(result),
                Ok(None) => {
                    let status = job
                        .children
                        .last()
                        .and_then(|child| child.exit_status)
                        .unwrap_or(0);
                    self.jobs.push(job);
                    Ok(outcome(status, None, None))
                }
                Err(error) => {
                    terminate_children(&mut job.children, &mut job.process_group_anchor);
                    let _ = terminal.restore();
                    finish_job_tasks_silently(&mut job);
                    Err(error)
                }
            }
        }

        fn stopped_terminal_modes(
            &self,
            terminal: &ForegroundTerminal,
        ) -> Result<Option<Termios>, ShellError> {
            #[cfg(test)]
            if self.fail_stopped_terminal_mode_read {
                return Err(ShellError::new(
                    ErrorCode::Io,
                    "injected stopped-job terminal mode failure",
                )
                .with_help("Test-only foreground cleanup fault"));
            }
            terminal.current_modes()
        }

        fn background(&mut self, id: Option<u32>) -> Result<CommandOutcome, ShellError> {
            self.refresh_jobs();
            let index = select_job(&self.jobs, id)?;
            if self.jobs[index].state.status != JobStatus::Stopped {
                return Err(ShellError::new(
                    ErrorCode::InvalidArgument,
                    format!("job {} is already running", self.jobs[index].state.id),
                )
                .with_help("Use `fg` to wait for a running job, or `jobs` to inspect its state"));
            }
            resume_job(&self.jobs[index])?;
            for child in &mut self.jobs[index].children {
                if child.status == JobStatus::Stopped {
                    child.status = JobStatus::Running;
                }
            }
            self.jobs[index].state.status = JobStatus::Running;
            Ok(outcome(0, None, None))
        }
    }

    fn expansion_error(message: &str, help: &str) -> ShellError {
        ShellError::new(ErrorCode::InvalidCommand, message).with_help(help)
    }

    fn matching_paren(source: &str) -> Option<usize> {
        let mut depth = 1_u32;
        let mut quote = None;
        let mut escaped = false;
        for (index, character) in source.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' && quote != Some('\'') {
                escaped = true;
                continue;
            }
            if let Some(active) = quote {
                if character == active {
                    quote = None;
                }
                continue;
            }
            if matches!(character, '\'' | '"') {
                quote = Some(character);
                continue;
            }
            match character {
                '(' => depth = depth.saturating_add(1),
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn matching_double_paren(source: &str) -> Option<usize> {
        let mut depth = 0_usize;
        for (index, character) in source.char_indices() {
            match character {
                '(' => depth = depth.saturating_add(1),
                ')' if depth > 0 => depth = depth.saturating_sub(1),
                ')' if source[index..].starts_with("))") => return Some(index),
                ')' => return None,
                _ => {}
            }
        }
        None
    }

    fn evaluate_arithmetic(source: &str) -> Result<i64, ShellError> {
        if source.len() > ARITHMETIC_SOURCE_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "arithmetic expansion exceeds its source byte limit",
            )
            .with_context(format!(
                "limit {ARITHMETIC_SOURCE_BYTES_MAX} bytes; observed {} bytes",
                source.len()
            ))
            .with_help("Split the calculation into smaller expressions"));
        }
        #[derive(Clone, Copy)]
        struct Parser<'a> {
            input: &'a [u8],
            index: usize,
        }
        impl<'a> Parser<'a> {
            fn skip(&mut self) {
                while self
                    .input
                    .get(self.index)
                    .is_some_and(u8::is_ascii_whitespace)
                {
                    self.index += 1;
                }
            }
            fn expression(&mut self, depth: usize) -> Result<i64, ShellError> {
                let mut value = self.term(depth)?;
                loop {
                    self.skip();
                    match self.input.get(self.index).copied() {
                        Some(b'+') => {
                            self.index += 1;
                            value = value.checked_add(self.term(depth)?).ok_or_else(|| {
                                expansion_error(
                                    "arithmetic expansion overflowed",
                                    "Use a smaller integer expression",
                                )
                            })?;
                        }
                        Some(b'-') => {
                            self.index += 1;
                            value = value.checked_sub(self.term(depth)?).ok_or_else(|| {
                                expansion_error(
                                    "arithmetic expansion overflowed",
                                    "Use a smaller integer expression",
                                )
                            })?;
                        }
                        _ => return Ok(value),
                    }
                }
            }
            fn term(&mut self, depth: usize) -> Result<i64, ShellError> {
                let mut value = self.factor(depth)?;
                loop {
                    self.skip();
                    match self.input.get(self.index).copied() {
                        Some(b'*') => {
                            self.index += 1;
                            value = value.checked_mul(self.factor(depth)?).ok_or_else(|| {
                                expansion_error(
                                    "arithmetic expansion overflowed",
                                    "Use a smaller integer expression",
                                )
                            })?;
                        }
                        Some(b'/') => {
                            self.index += 1;
                            let divisor = self.factor(depth)?;
                            if divisor == 0 {
                                return Err(expansion_error(
                                    "arithmetic expansion divides by zero",
                                    "Use a non-zero divisor",
                                ));
                            }
                            value = value.checked_div(divisor).ok_or_else(|| {
                                expansion_error(
                                    "arithmetic expansion overflowed",
                                    "Use a smaller integer expression",
                                )
                            })?;
                        }
                        _ => return Ok(value),
                    }
                }
            }
            fn nested_depth(depth: usize) -> Result<usize, ShellError> {
                let observed_depth = depth.saturating_add(1);
                if observed_depth > ARITHMETIC_DEPTH_MAX {
                    return Err(ShellError::new(
                        ErrorCode::ResourceLimit,
                        "arithmetic expansion exceeds its nesting depth limit",
                    )
                    .with_context(format!(
                        "limit {ARITHMETIC_DEPTH_MAX} levels; observed at least {observed_depth} levels"
                    ))
                    .with_help("Flatten the arithmetic expression"));
                }
                Ok(observed_depth)
            }

            fn factor(&mut self, depth: usize) -> Result<i64, ShellError> {
                self.skip();
                if self.input.get(self.index) == Some(&b'(') {
                    self.index += 1;
                    let value = self.expression(Self::nested_depth(depth)?)?;
                    self.skip();
                    if self.input.get(self.index) != Some(&b')') {
                        return Err(expansion_error(
                            "unbalanced parentheses in arithmetic expansion",
                            "Balance parentheses in `$((...))`",
                        ));
                    }
                    self.index += 1;
                    return Ok(value);
                }
                if self.input.get(self.index) == Some(&b'-') {
                    self.index += 1;
                    return self
                        .factor(Self::nested_depth(depth)?)?
                        .checked_neg()
                        .ok_or_else(|| {
                            expansion_error(
                                "arithmetic expansion overflowed",
                                "Use a smaller integer expression",
                            )
                        });
                }
                let start = self.index;
                while self.input.get(self.index).is_some_and(u8::is_ascii_digit) {
                    self.index += 1;
                }
                if start == self.index {
                    return Err(expansion_error(
                        "missing operand in arithmetic expansion",
                        "Use integer literals and +, -, *, /, or parentheses",
                    ));
                }
                let text = std::str::from_utf8(&self.input[start..self.index]).map_err(|_| {
                    expansion_error(
                        "non-numeric digits in arithmetic expansion",
                        "Use ASCII integer literals",
                    )
                })?;
                let value = text.parse::<i64>().map_err(|_| {
                    expansion_error(
                        "arithmetic expansion overflowed",
                        "Use a smaller integer expression",
                    )
                })?;
                Ok(value)
            }
        }
        let mut parser = Parser {
            input: source.as_bytes(),
            index: 0,
        };
        let value = parser.expression(0)?;
        parser.skip();
        if parser.index != parser.input.len() {
            return Err(expansion_error(
                "unexpected trailing text in arithmetic expansion",
                "Use integer literals and +, -, *, /, or parentheses",
            )
            .with_context(format!("in `{source}`")));
        }
        Ok(value)
    }

    fn pathname_expand(
        pattern: &str,
        retained_before_paths: usize,
    ) -> Result<Vec<String>, ShellError> {
        const MAX_GLOB_MATCHES: usize = 10_000;
        let absolute = pattern.starts_with('/');
        let mut paths = vec![if absolute {
            PathBuf::from("/")
        } else {
            PathBuf::new()
        }];
        for component in pattern.split('/').filter(|component| !component.is_empty()) {
            let has_pattern = component
                .chars()
                .any(|character| matches!(character, '*' | '?' | '['));
            let mut next = Vec::new();
            let previous_path_bytes = paths
                .iter()
                .map(|path| path.as_os_str().as_bytes().len())
                .sum::<usize>();
            let mut next_path_bytes = 0_usize;
            for prefix in &paths {
                if !has_pattern {
                    let path = prefix.join(component);
                    claim_path_expansion_bytes(
                        retained_before_paths,
                        previous_path_bytes,
                        next_path_bytes,
                        path.as_os_str().as_bytes().len(),
                    )?;
                    next_path_bytes =
                        next_path_bytes.saturating_add(path.as_os_str().as_bytes().len());
                    next.push(path);
                    continue;
                }
                let directory = if prefix.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    prefix.as_path()
                };
                let Ok(entries) = std::fs::read_dir(directory) else {
                    continue;
                };
                for entry in entries.filter_map(Result::ok) {
                    let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if glob_matches(component, &name) {
                        let path = prefix.join(name);
                        claim_path_expansion_bytes(
                            retained_before_paths,
                            previous_path_bytes,
                            next_path_bytes,
                            path.as_os_str().as_bytes().len(),
                        )?;
                        next_path_bytes =
                            next_path_bytes.saturating_add(path.as_os_str().as_bytes().len());
                        next.push(path);
                        if next.len() > MAX_GLOB_MATCHES {
                            return Err(ShellError::new(
                                ErrorCode::ResourceLimit,
                                "pathname expansion exceeded its match budget",
                            )
                            .with_context(format!(
                                "limit {MAX_GLOB_MATCHES} matches; observed {} matches",
                                next.len()
                            ))
                            .with_help(format!(
                                "Narrow the pattern below {MAX_GLOB_MATCHES} matches or use an explicit data pipeline",
                            )));
                        }
                    }
                }
            }
            paths = next;
            if paths.is_empty() {
                return Ok(Vec::new());
            }
        }
        let mut matches = paths
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        matches.sort();
        Ok(matches)
    }

    fn claim_path_expansion_bytes(
        retained_before_paths: usize,
        previous_path_bytes: usize,
        next_path_bytes: usize,
        additional_path_bytes: usize,
    ) -> Result<(), ShellError> {
        let observed_bytes = retained_before_paths
            .saturating_add(previous_path_bytes)
            .saturating_add(next_path_bytes)
            .saturating_add(additional_path_bytes);
        if observed_bytes <= EXPANSION_BYTES_MAX {
            return Ok(());
        }
        Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "pathname expansion exceeds the retained-byte limit",
        )
        .with_context(format!(
            "limit {EXPANSION_BYTES_MAX} bytes; retained {retained_before_paths} bytes before pathname expansion; observed at least {observed_bytes} bytes"
        ))
        .with_help("Narrow the pathname pattern or move the file list into a data pipeline"))
    }

    fn glob_matches(pattern: &str, candidate: &str) -> bool {
        enum GlobAtom {
            Star,
            Any,
            Literal(char),
            Class {
                negated: bool,
                ranges: Vec<(char, char)>,
            },
        }

        let characters = pattern.chars().collect::<Vec<_>>();
        let mut atoms = Vec::new();
        let mut index = 0;
        while index < characters.len() {
            match characters[index] {
                '*' => atoms.push(GlobAtom::Star),
                '?' => atoms.push(GlobAtom::Any),
                '[' => {
                    let Some(relative_end) = characters[index + 1..]
                        .iter()
                        .position(|character| *character == ']')
                    else {
                        atoms.push(GlobAtom::Literal('['));
                        index += 1;
                        continue;
                    };
                    let end = index + 1 + relative_end;
                    let mut class = &characters[index + 1..end];
                    let negated = class
                        .first()
                        .is_some_and(|character| matches!(character, '!' | '^'));
                    if negated {
                        class = &class[1..];
                    }
                    let mut ranges = Vec::new();
                    let mut class_index = 0;
                    while class_index < class.len() {
                        if class_index + 2 < class.len() && class[class_index + 1] == '-' {
                            ranges.push((class[class_index], class[class_index + 2]));
                            class_index += 3;
                        } else {
                            ranges.push((class[class_index], class[class_index]));
                            class_index += 1;
                        }
                    }
                    atoms.push(GlobAtom::Class { negated, ranges });
                    index = end;
                }
                character => atoms.push(GlobAtom::Literal(character)),
            }
            index += 1;
        }
        if candidate.starts_with('.') && !pattern.starts_with('.') {
            return false;
        }
        let candidate = candidate.chars().collect::<Vec<_>>();
        let mut previous = vec![false; candidate.len() + 1];
        previous[0] = true;
        for atom in atoms {
            let mut current = vec![false; candidate.len() + 1];
            match atom {
                GlobAtom::Star => {
                    current[0] = previous[0];
                    for candidate_index in 1..=candidate.len() {
                        current[candidate_index] =
                            previous[candidate_index] || current[candidate_index - 1];
                    }
                }
                GlobAtom::Any => {
                    current[1..].copy_from_slice(&previous[..candidate.len()]);
                }
                GlobAtom::Literal(expected) => {
                    for candidate_index in 1..=candidate.len() {
                        current[candidate_index] = previous[candidate_index - 1]
                            && candidate[candidate_index - 1] == expected;
                    }
                }
                GlobAtom::Class { negated, ranges } => {
                    for candidate_index in 1..=candidate.len() {
                        let character = candidate[candidate_index - 1];
                        let contained = ranges
                            .iter()
                            .any(|(start, end)| *start <= character && character <= *end);
                        current[candidate_index] =
                            previous[candidate_index - 1] && (contained != negated);
                    }
                }
            }
            previous = current;
        }
        previous[candidate.len()]
    }

    fn input_stdio(
        command: &SimpleCommand,
        previous: Option<PipeReader>,
        has_upstream: bool,
        stdin_is_null: bool,
    ) -> Result<PreparedInput, ShellError> {
        enum InputSource {
            File(File),
            HereString(String),
        }

        let mut selected = None;
        for redirect in command.redirects.iter().filter(|redirect| {
            matches!(
                redirect.kind,
                RedirectKind::Input | RedirectKind::HereString
            )
        }) {
            if redirect.kind == RedirectKind::HereString {
                selected = Some(InputSource::HereString(redirect.path.clone()));
            } else {
                let file = File::open(&redirect.path).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Io,
                        format!("cannot read redirected input {}", redirect.path),
                    )
                    .with_context(error.to_string())
                    .with_help("Check that the file exists and is readable")
                })?;
                selected = Some(InputSource::File(file));
            }
        }
        match selected {
            Some(InputSource::HereString(value)) => {
                let observed_bytes = value.len().saturating_add(1);
                if observed_bytes > HERE_STRING_BYTES_MAX {
                    return Err(ShellError::new(
                        ErrorCode::ResourceLimit,
                        "here-string input exceeds its byte limit",
                    )
                    .with_context(format!(
                        "limit {HERE_STRING_BYTES_MAX} bytes; observed {observed_bytes} bytes including the trailing newline"
                    ))
                    .with_help("Use an input file or pipeline for larger input"));
                }
                let mut bytes = Vec::with_capacity(observed_bytes);
                bytes.extend_from_slice(value.as_bytes());
                bytes.push(b'\n');
                let (reader, writer) = pipe().map_err(|error| {
                    ShellError::new(ErrorCode::Io, "could not create here-string input")
                        .with_context(error.to_string())
                        .with_help("Retry the command or use an input file")
                })?;
                Ok(PreparedInput {
                    stdio: Stdio::from(reader),
                    writer: Some((writer, bytes)),
                })
            }
            Some(InputSource::File(file)) => Ok(PreparedInput {
                stdio: Stdio::from(file),
                writer: None,
            }),
            None => Ok(PreparedInput {
                stdio: previous.map_or_else(
                    || {
                        if has_upstream || stdin_is_null {
                            Stdio::null()
                        } else {
                            Stdio::inherit()
                        }
                    },
                    Stdio::from,
                ),
                writer: None,
            }),
        }
    }

    fn output_stdio(
        command: &SimpleCommand,
        last: bool,
        capture: bool,
    ) -> Result<OutputStdio, ShellError> {
        let mut redirected = None;
        for redirect in command.redirects.iter().filter(|redirect| {
            redirect.fd == 1 && matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append)
        }) {
            redirected = Some(open_redirected_output(redirect)?);
        }
        if let Some(file) = redirected {
            let duplicate = file.try_clone().map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not duplicate the redirected output descriptor",
                )
                .with_context(error.to_string())
                .with_help("Retry the command or use an explicit dialect island")
            })?;
            return Ok((Stdio::from(file), None, None, Some(duplicate)));
        }
        if !last || capture {
            let (reader, writer) = pipe().map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not create a byte pipeline")
                    .with_context(error.to_string())
                    .with_help("Retry after closing unused processes or file descriptors")
            })?;
            let stdout = Stdio::from(writer.try_clone().map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not clone a pipeline writer")
                    .with_context(error.to_string())
                    .with_help("Retry after closing unused processes or file descriptors")
            })?);
            return Ok((stdout, Some(reader), Some(writer), None));
        }
        Ok((Stdio::inherit(), None, None, None))
    }

    fn stderr_stdio(command: &SimpleCommand, capture: bool) -> Result<Stdio, ShellError> {
        if let Some(redirect) = command.redirects.iter().rev().find(|redirect| {
            redirect.fd == 2 && matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append)
        }) {
            return Ok(Stdio::from(open_redirected_output(redirect)?));
        }
        Ok(if capture {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
    }

    fn duplicates_stderr_to_stdout(redirect: &quirl_syntax::Redirect) -> bool {
        redirect.fd == 2 && redirect.kind == RedirectKind::DuplicateOutput && redirect.path == "1"
    }

    fn stderr_duplication_precedes_stdout_redirect(command: &SimpleCommand) -> bool {
        let mut duplicated = false;
        for redirect in &command.redirects {
            if duplicates_stderr_to_stdout(redirect) {
                duplicated = true;
            } else if duplicated
                && redirect.fd == 1
                && matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append)
            {
                return true;
            }
        }
        false
    }

    fn finish_control_builtin(
        command: &SimpleCommand,
        mut result: CommandOutcome,
        capture: bool,
    ) -> Result<CommandOutcome, ShellError> {
        if command
            .redirects
            .iter()
            .any(|redirect| matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append))
        {
            write_redirected_output(
                command,
                result.stdout.as_deref().unwrap_or_default().as_bytes(),
            )?;
            result.stdout = capture.then(String::new);
        } else if !capture && let Some(stdout) = result.stdout.take() {
            io_write_all(std::io::stdout(), stdout.as_bytes(), "standard output")?;
        }
        if !capture && let Some(stderr) = result.stderr.take() {
            io_write_all(std::io::stderr(), stderr.as_bytes(), "standard error")?;
        }
        Ok(result)
    }

    fn validate_control_redirects(command: &SimpleCommand) -> Result<(), ShellError> {
        for redirect in command
            .redirects
            .iter()
            .filter(|redirect| redirect.kind == RedirectKind::Input)
        {
            File::open(&redirect.path).map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    format!("cannot read redirected input {}", redirect.path),
                )
                .with_context(error.to_string())
                .with_help("Check that the file exists and is readable")
            })?;
        }
        for redirect in command
            .redirects
            .iter()
            .filter(|redirect| matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append))
        {
            open_redirected_output(redirect)?;
        }
        Ok(())
    }

    fn write_redirected_output(command: &SimpleCommand, bytes: &[u8]) -> Result<(), ShellError> {
        let redirect = command
            .redirects
            .iter()
            .rev()
            .find(|redirect| matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append))
            .ok_or_else(|| {
                ShellError::new(ErrorCode::Io, "missing output redirection")
                    .with_help(
                        "Add a > or >> redirect to the command, or report this if one was already given",
                    )
            })?;
        let file = open_redirected_output(redirect)?;
        io_write_all(file, bytes, &redirect.path)
    }

    fn open_redirected_output(redirect: &quirl_syntax::Redirect) -> Result<File, ShellError> {
        let mut options = OpenOptions::new();
        options.create(true).write(true);
        if redirect.kind == RedirectKind::Append {
            options.append(true);
        } else {
            options.truncate(true);
        }
        options.open(&redirect.path).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                format!("cannot write redirected output {}", redirect.path),
            )
            .with_context(error.to_string())
            .with_help("Check the parent directory and file permissions")
        })
    }

    fn parse_job_id(command: &SimpleCommand) -> Result<Option<u32>, ShellError> {
        if command.words.len() > 2 {
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("{} accepts at most one job id", command.words[0]),
            )
            .with_help(format!("Usage: {} [%job]", command.words[0])));
        }
        command
            .words
            .get(1)
            .map(|value| {
                value.trim_start_matches('%').parse::<u32>().map_err(|_| {
                    ShellError::new(
                        ErrorCode::InvalidArgument,
                        format!("invalid job id `{value}`"),
                    )
                    .with_help("Use `jobs` to list valid numeric ids")
                })
            })
            .transpose()
    }

    fn select_job(jobs: &[Job], id: Option<u32>) -> Result<usize, ShellError> {
        jobs.iter()
            .rposition(|job| {
                job.state.status != JobStatus::Done && id.is_none_or(|id| job.state.id == id)
            })
            .ok_or_else(|| {
                ShellError::new(ErrorCode::InvalidArgument, "no matching active job")
                    .with_help("Run `jobs` to list active jobs")
            })
    }

    fn suspend_running_children(job: &mut Job, id: u32) -> Result<(), ShellError> {
        let group_stopped = match job.process_group_anchor.as_ref() {
            Some(anchor) => match anchor.signal(Signal::SIGSTOP) {
                Ok(()) => true,
                Err(Errno::ESRCH) => false,
                Err(error) => return Err(suspend_error(id, error)),
            },
            None => false,
        };
        for child in &mut job.children {
            if child.status != JobStatus::Running {
                continue;
            }
            if group_stopped {
                child.status = JobStatus::Stopped;
                continue;
            }
            let process_id = i32::try_from(child.child.id()).map_err(|error| {
                ShellError::new(ErrorCode::Io, "child process id exceeds platform limits")
                    .with_context(error.to_string())
                    .with_help("Cancel the job and start it again")
            })?;
            match kill(Pid::from_raw(process_id), Signal::SIGSTOP) {
                Ok(()) => child.status = JobStatus::Stopped,
                Err(Errno::ESRCH) => {
                    poll_child_checked(child)?;
                }
                Err(error) => return Err(suspend_error(id, error)),
            }
        }
        let (status, exit_status) = super::summarize_job_lifecycle(
            job.children
                .iter()
                .map(|child| (child.status, child.exit_status)),
        );
        job.state.status = status;
        job.state.exit_status = exit_status;
        Ok(())
    }

    fn suspend_error(id: u32, error: Errno) -> ShellError {
        ShellError::new(ErrorCode::Io, format!("could not suspend job %{id}"))
            .with_context(error.to_string())
            .with_help("Run `jobs` to refresh the job before retrying")
    }

    fn resume_job(job: &Job) -> Result<(), ShellError> {
        if let Some(anchor) = job.process_group_anchor.as_ref()
            && anchor.signal(Signal::SIGCONT).is_ok()
        {
            return Ok(());
        }
        let mut resumed = false;
        let mut failure = None;
        for child in &job.children {
            if child.status == JobStatus::Done {
                continue;
            }
            let Ok(process_id) = i32::try_from(child.child.id()) else {
                continue;
            };
            match kill(Pid::from_raw(process_id), Signal::SIGCONT) {
                Ok(()) => resumed = true,
                Err(error) => failure = Some(error),
            }
        }
        if !resumed {
            return Err(ShellError::new(
                ErrorCode::Io,
                format!("could not resume job {}", job.state.id),
            )
            .with_context(failure.map_or_else(
                || "no live child process".to_owned(),
                |error| error.to_string(),
            ))
            .with_help("Run `jobs`; the process may have already exited"));
        }
        Ok(())
    }

    #[derive(Debug)]
    enum ProcessGroupVerification {
        Live,
        Exited(i32),
    }

    fn observe_fast_child_exit(
        child: &mut Child,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        const STATUS_OBSERVATIONS_MAX: usize = 1_024;
        for _ in 0..STATUS_OBSERVATIONS_MAX {
            match child.try_wait() {
                Ok(None) => thread::yield_now(),
                result => return result,
            }
        }
        Ok(None)
    }

    fn verify_process_group(
        child: &mut Child,
        process_id: i32,
        process_group: i32,
    ) -> Result<ProcessGroupVerification, ShellError> {
        let process_id = Pid::from_raw(process_id);
        let expected_group = Pid::from_raw(process_group);
        let set_result = setpgid(process_id, expected_group);
        let observed_group = getpgid(Some(process_id));
        let mut exited_child_context = None;
        if observed_group == Ok(expected_group) {
            return Ok(ProcessGroupVerification::Live);
        }
        if set_result == Err(Errno::ESRCH) && observed_group == Err(Errno::ESRCH) {
            let child_status = observe_fast_child_exit(child);
            exited_child_context = Some(format!("; child_status={child_status:?}"));
            if let Ok(Some(status)) = child_status {
                let exit_status = status
                    .code()
                    .or_else(|| status.signal().map(|signal| 128 + signal))
                    .unwrap_or(1);
                return Ok(ProcessGroupVerification::Exited(exit_status));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        Err(ShellError::new(
            ErrorCode::ProcessSpawn,
            "could not establish the native pipeline process group",
        )
        .with_context(format!(
            "pid {process_id}; expected group {expected_group}; setpgid={set_result:?}; observed group {}{}",
            observed_group.map_or_else(|e| format!("unavailable ({e})"), |g| g.to_string()),
            exited_child_context.unwrap_or_default()
        ))
        .with_help("Retry the command; report repeated process-group construction failures"))
    }

    struct PipelineConstructionGuard {
        children: Vec<JobChild>,
        process_group: Option<i32>,
        process_group_anchor: Option<ProcessGroupAnchor>,
    }

    impl PipelineConstructionGuard {
        fn new() -> Self {
            Self {
                children: Vec::new(),
                process_group: None,
                process_group_anchor: None,
            }
        }

        fn push_staged_group_leader(
            &mut self,
            mut child: Child,
            executable: &str,
            source: &str,
            request: Option<RequestContext<'_>>,
        ) -> Result<(), ShellError> {
            let process_group = i32::try_from(child.id()).map_err(|error| {
                let _ = child.kill();
                let _ = child.wait();
                ShellError::new(
                    ErrorCode::Io,
                    "staged process-group leader id is outside the platform range",
                )
                .with_context(error.to_string())
                .with_help("Report this platform-specific process error")
            })?;
            wait_for_staged_group_leader(&mut child, process_group, executable, source, request)?;
            let observed_group = getpgid(Some(Pid::from_raw(process_group)));
            if observed_group != Ok(Pid::from_raw(process_group)) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ShellError::new(
                    ErrorCode::ProcessSpawn,
                    "could not verify the staged native process-group leader",
                )
                .with_command(source)
                .with_context(format!(
                    "pid {process_group}; expected group {process_group}; observed group {}",
                    observed_group.map_or_else(|e| format!("unavailable ({e})"), |g| g.to_string())
                ))
                .with_help("Retry the command; report repeated process-group staging failures"));
            }
            let mut anchor = match ProcessGroupAnchor::join(process_group, request) {
                Ok(anchor) => anchor,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.with_command(source));
                }
            };
            if let Err(error) = anchor.signal(Signal::SIGCONT) {
                let _ = anchor.terminate();
                let _ = child.kill();
                let _ = child.wait();
                return Err(ShellError::new(
                    ErrorCode::ProcessSpawn,
                    "could not release the staged native process-group leader",
                )
                .with_command(source)
                .with_context(error.to_string())
                .with_help("Retry the command; report repeated process-group staging failures"));
            }
            self.process_group = Some(process_group);
            self.process_group_anchor = Some(anchor);
            self.children.push(JobChild {
                child,
                status: JobStatus::Running,
                exit_status: None,
            });
            Ok(())
        }

        fn push(&mut self, mut child: Child) -> Result<(), ShellError> {
            let process_id = i32::try_from(child.id()).map_err(|error| {
                let _ = child.kill();
                let _ = child.wait();
                ShellError::new(
                    ErrorCode::Io,
                    "child process id is outside the platform range",
                )
                .with_context(error.to_string())
                .with_help("Report this platform-specific process error")
            })?;
            let process_group = self.process_group.ok_or_else(|| {
                let _ = child.kill();
                let _ = child.wait();
                ShellError::new(
                    ErrorCode::ProcessSpawn,
                    "native pipeline child has no owned process-group anchor",
                )
                .with_help("Report this process construction invariant failure")
            })?;
            let verification = verify_process_group(&mut child, process_id, process_group)?;
            let (status, exit_status) = match verification {
                ProcessGroupVerification::Live => (JobStatus::Running, None),
                ProcessGroupVerification::Exited(exit_status) => {
                    (JobStatus::Done, Some(exit_status))
                }
            };
            self.children.push(JobChild {
                child,
                status,
                exit_status,
            });
            Ok(())
        }

        fn release(&mut self) -> (Vec<JobChild>, Option<ProcessGroupAnchor>) {
            (
                std::mem::take(&mut self.children),
                self.process_group_anchor.take(),
            )
        }
    }

    fn wait_for_staged_group_leader(
        child: &mut Child,
        process_group: i32,
        executable: &str,
        source: &str,
        request: Option<RequestContext<'_>>,
    ) -> Result<(), ShellError> {
        let started = Instant::now();
        let process_id = Pid::from_raw(process_group);
        loop {
            if let Some(request) = request
                && let Err(error) = request.ensure_active()
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            match waitpid(
                process_id,
                Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG),
            ) {
                Ok(WaitStatus::Stopped(_, Signal::SIGSTOP)) => return Ok(()),
                Ok(WaitStatus::StillAlive) => {
                    if started.elapsed() >= PROCESS_GROUP_ANCHOR_STARTUP_WAIT_MAX {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(ShellError::new(
                            ErrorCode::ResourceLimit,
                            "native process-group staging exceeded its startup limit",
                        )
                        .with_command(source)
                        .with_context(format!(
                            "limit {} ms; executable {executable}",
                            PROCESS_GROUP_ANCHOR_STARTUP_WAIT_MAX.as_millis()
                        ))
                        .with_help("Retry after reducing system process pressure"));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(WaitStatus::Exited(_, 127)) => {
                    return Err(ShellError::new(
                        ErrorCode::ProcessSpawn,
                        format!("could not start `{executable}`"),
                    )
                    .with_command(source)
                    .with_context("executable was not found by the trusted process-group stage")
                    .with_help(
                        "Check that the command exists on PATH, or use `help` to inspect built-ins",
                    ));
                }
                Ok(status) => {
                    return Err(ShellError::new(
                        ErrorCode::ProcessSpawn,
                        "trusted process-group staging exited before guest release",
                    )
                    .with_command(source)
                    .with_context(format!("pid {process_group}; status {status:?}"))
                    .with_help(
                        "Retry the command; report repeated process-group staging failures",
                    ));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ShellError::new(
                        ErrorCode::Io,
                        "could not observe the staged native process-group leader",
                    )
                    .with_command(source)
                    .with_context(error.to_string())
                    .with_help("Retry the command; report repeated process staging failures"));
                }
            }
        }
    }

    impl Drop for PipelineConstructionGuard {
        fn drop(&mut self) {
            terminate_children(&mut self.children, &mut self.process_group_anchor);
        }
    }

    fn terminate_children(
        children: &mut [JobChild],
        process_group_anchor: &mut Option<ProcessGroupAnchor>,
    ) {
        let group_result = process_group_anchor
            .as_mut()
            .map(ProcessGroupAnchor::begin_termination);
        for child in children {
            if child.status != JobStatus::Done {
                // Group cleanup contains descendants; the direct fallback is
                // still required when a leader exited before the group became
                // observable or the kernel rejects a group operation.
                let _ = child.child.kill();
                let _ = child.child.wait();
                child.status = JobStatus::Done;
            }
        }
        if let Some(mut anchor) = process_group_anchor.take() {
            let _ = anchor.finish_termination(group_result.unwrap_or(Ok(())));
        }
    }

    fn terminate_group_descendants(
        process_group_anchor: &mut Option<ProcessGroupAnchor>,
    ) -> Result<(), ShellError> {
        let Some(anchor) = process_group_anchor.take() else {
            return Ok(());
        };
        anchor.terminate_owned_group()
    }

    fn spawn_reader_observed(
        reader: impl Read + Send + 'static,
        limit: usize,
        output: Option<(Sender<OutputEvent>, OutputStream)>,
    ) -> ReaderTask {
        spawn_reader_with_budget(reader, Arc::new(CaptureBudget::new(limit)), output)
    }

    fn spawn_reader_with_budget(
        mut reader: impl Read + Send + 'static,
        budget: Arc<CaptureBudget>,
        output: Option<(Sender<OutputEvent>, OutputStream)>,
    ) -> ReaderTask {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 8 * 1024];
            let mut discarded_bytes = 0_u64;
            loop {
                let count = reader.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                let retained = budget.claim(count);
                bytes.extend_from_slice(&chunk[..retained]);
                if retained > 0
                    && let Some((sender, stream)) = &output
                {
                    let _ = sender.send(OutputEvent {
                        stream: *stream,
                        bytes: chunk[..retained].to_vec(),
                    });
                }
                discarded_bytes = discarded_bytes.saturating_add((count - retained) as u64);
            }
            Ok(ReaderCapture {
                bytes,
                discarded_bytes,
            })
        })
    }

    fn join_reader(reader: Option<ReaderTask>, description: &str) -> Result<String, ShellError> {
        let Some(reader) = reader else {
            return Ok(String::new());
        };
        let capture = join_reader_task(reader, description)?;
        if capture.discarded_bytes > 0 {
            return Err(capture_limit_error(description, &capture));
        }
        Ok(String::from_utf8_lossy(&capture.bytes).into_owned())
    }

    fn join_reader_task(
        reader: ReaderTask,
        description: &str,
    ) -> Result<ReaderCapture, ShellError> {
        reader
            .join()
            .map_err(|_| {
                ShellError::new(ErrorCode::Io, format!("{description} reader panicked"))
                    .with_help("Retry the command; report this if the pipeline is reproducible")
            })?
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, format!("could not read {description}"))
                    .with_context(error.to_string())
                    .with_help("Retry the command; report this if the pipeline is reproducible")
            })
    }

    fn capture_limit_error(description: &str, capture: &ReaderCapture) -> ShellError {
        ShellError::new(
            ErrorCode::ResourceLimit,
            format!("{description} exceeded the retained output limit"),
        )
        .with_context(format!(
            "retained {} bytes; discarded {} bytes",
            capture.bytes.len(),
            capture.discarded_bytes
        ))
        .with_help("Reduce process output, redirect it to a file, or consume it in a pipeline")
    }

    fn join_readers(readers: Vec<ReaderTask>, description: &str) -> Result<String, ShellError> {
        let mut capture = ReaderCapture {
            bytes: Vec::new(),
            discarded_bytes: 0,
        };
        let mut failure = None;
        for reader in readers {
            match join_reader_task(reader, description) {
                Ok(next) => {
                    capture.bytes.extend(next.bytes);
                    capture.discarded_bytes =
                        capture.discarded_bytes.saturating_add(next.discarded_bytes);
                }
                Err(error) if failure.is_none() => failure = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if capture.discarded_bytes > 0 {
            return Err(capture_limit_error(description, &capture));
        }
        Ok(String::from_utf8_lossy(&capture.bytes).into_owned())
    }

    fn join_writers(writers: Vec<WriterTask>) -> Result<(), ShellError> {
        for writer in writers {
            let result = writer.join().map_err(|_| {
                ShellError::new(ErrorCode::Io, "pipeline writer panicked")
                    .with_help("Retry the command; report this if the pipeline is reproducible")
            })?;
            if let Err(error) = result {
                if error.kind() == std::io::ErrorKind::BrokenPipe {
                    continue;
                }
                return Err(
                    ShellError::new(ErrorCode::Io, "could not write pipeline input")
                        .with_context(error.to_string())
                        .with_help(
                            "Retry the command; report this if the pipeline is reproducible",
                        ),
                );
            }
        }
        Ok(())
    }

    fn finish_job_tasks_silently(job: &mut Job) {
        let _ = join_reader(job.stdout_reader.take(), "pipeline output");
        let _ = join_readers(
            std::mem::take(&mut job.stderr_readers),
            "command error output",
        );
        let _ = join_writers(std::mem::take(&mut job.writers));
    }

    fn poll_child(child: &mut JobChild) {
        let _ = poll_child_checked(child);
    }

    fn poll_child_checked(child: &mut JobChild) -> Result<bool, ShellError> {
        let process_id = i32::try_from(child.child.id()).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                "child process id is outside the platform range",
            )
            .with_context(error.to_string())
            .with_help("Report this platform-specific process error")
        })?;
        let status = waitpid(
            Pid::from_raw(process_id),
            Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED),
        )
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not observe command state")
                .with_context(error.to_string())
                .with_help("Inspect the job with `jobs` and retry")
        })?;
        Ok(record_wait_status(
            status,
            &mut child.status,
            &mut child.exit_status,
        ))
    }

    fn record_wait_status(
        status: WaitStatus,
        child_status: &mut JobStatus,
        exit_status: &mut Option<i32>,
    ) -> bool {
        match status {
            WaitStatus::Exited(_, code) => {
                *child_status = JobStatus::Done;
                *exit_status = Some(code);
            }
            WaitStatus::Signaled(_, signal, _) => {
                *child_status = JobStatus::Done;
                *exit_status = Some(128 + signal as i32);
            }
            WaitStatus::Stopped(_, signal) => {
                *child_status = JobStatus::Stopped;
                *exit_status = Some(128 + signal as i32);
            }
            WaitStatus::Continued(_) => {
                *child_status = JobStatus::Running;
                *exit_status = None;
            }
            WaitStatus::StillAlive => return false,
            // Linux reports ptrace events as stops distinct from WIFSTOPPED.
            // They remain live children, so retaining them as stopped keeps
            // process ownership intact and prevents a false reap.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            WaitStatus::PtraceEvent(_, signal, _) => {
                *child_status = JobStatus::Stopped;
                *exit_status = Some(128 + signal as i32);
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            WaitStatus::PtraceSyscall(_) => {
                *child_status = JobStatus::Stopped;
                *exit_status = Some(128 + Signal::SIGTRAP as i32);
            }
        }
        true
    }

    static FOREGROUND_TERMINAL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct ForegroundTerminalLease {
        guard: Option<MutexGuard<'static, ()>>,
    }

    impl ForegroundTerminalLease {
        fn none() -> Self {
            Self { guard: None }
        }

        fn acquire(request: Option<RequestContext<'_>>) -> Result<Self, ShellError> {
            if !std::io::stdin().is_terminal() {
                return Ok(Self::none());
            }
            Self::acquire_from(
                FOREGROUND_TERMINAL_LOCK.get_or_init(|| Mutex::new(())),
                request,
                FOREGROUND_TERMINAL_LEASE_WAIT_MAX,
            )
        }

        fn acquire_from(
            lock: &'static Mutex<()>,
            request: Option<RequestContext<'_>>,
            wait_max: Duration,
        ) -> Result<Self, ShellError> {
            let started = Instant::now();
            loop {
                match lock.try_lock() {
                    Ok(guard) => return Ok(Self { guard: Some(guard) }),
                    Err(TryLockError::Poisoned(_)) => {
                        return Err(ShellError::new(
                            ErrorCode::Io,
                            "foreground terminal ownership state is unavailable",
                        )
                        .with_help(
                            "Restart Quirl so terminal ownership can be initialized safely",
                        ));
                    }
                    Err(TryLockError::WouldBlock) => {
                        if let Some(request) = request {
                            request.ensure_active()?;
                        } else if started.elapsed() >= wait_max {
                            return Err(ShellError::new(
                                ErrorCode::ResourceLimit,
                                "foreground terminal lease wait exceeded its limit",
                            )
                            .with_context(format!(
                                "limit {} ms; observed at least {} ms",
                                wait_max.as_millis(),
                                started.elapsed().as_millis()
                            ))
                            .with_help(
                                "Wait for the active foreground command to finish, then retry",
                            ));
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }

        fn release(&mut self) {
            self.guard.take();
        }

        fn permits_handoff(&self) -> bool {
            self.guard.is_some()
        }
    }

    struct ForegroundTerminal {
        restore_group: Option<Pid>,
        restore_modes: Option<Termios>,
        lease: ForegroundTerminalLease,
    }

    struct BlockedTerminalSignals {
        previous: SigSet,
    }

    impl BlockedTerminalSignals {
        fn new() -> Result<Self, ShellError> {
            let mut blocked = SigSet::empty();
            blocked.add(Signal::SIGTTOU);
            blocked.add(Signal::SIGTTIN);
            let mut previous = SigSet::empty();
            pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&blocked), Some(&mut previous)).map_err(
                |error| {
                    ShellError::new(ErrorCode::Io, "could not block terminal-control signals")
                        .with_context(error.to_string())
                        .with_help("Run the command from a terminal with native job control")
                },
            )?;
            Ok(Self { previous })
        }
    }

    impl Drop for BlockedTerminalSignals {
        fn drop(&mut self) {
            let _ = pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&self.previous), None);
        }
    }

    impl ForegroundTerminal {
        fn give_to(
            process_group: Option<i32>,
            lease: ForegroundTerminalLease,
        ) -> Result<Self, ShellError> {
            let mut restore_modes = None;
            let restore_group = if lease.permits_handoff() && std::io::stdin().is_terminal() {
                if let Some(group) = process_group {
                    let _blocked = BlockedTerminalSignals::new()?;
                    restore_modes = Some(tcgetattr(std::io::stdin()).map_err(|error| {
                        ShellError::new(ErrorCode::Io, "could not save terminal modes")
                            .with_context(error.to_string())
                            .with_help("Run the command from a terminal with native job control")
                    })?);
                    let restore_group = tcgetpgrp(std::io::stdin()).map_err(|error| {
                        ShellError::new(
                            ErrorCode::Io,
                            "could not identify the terminal foreground process group",
                        )
                        .with_context(error.to_string())
                        .with_help("Run the command from a terminal with native job control")
                    })?;
                    tcsetpgrp(std::io::stdin(), Pid::from_raw(group)).map_err(|error| {
                        ShellError::new(
                            ErrorCode::Io,
                            "could not give the terminal to the foreground job",
                        )
                        .with_context(error.to_string())
                        .with_help("Run the command from a terminal with native job control")
                    })?;
                    Some(restore_group)
                } else {
                    None
                }
            } else {
                None
            };
            Ok(Self {
                restore_group,
                restore_modes,
                lease,
            })
        }

        fn current_modes(&self) -> Result<Option<Termios>, ShellError> {
            if self.restore_group.is_none() {
                return Ok(None);
            }
            tcgetattr(std::io::stdin()).map(Some).map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not save stopped job terminal modes")
                    .with_context(error.to_string())
                    .with_help("Run `reset`, then cancel or foreground the stopped job")
            })
        }

        fn apply_modes(&self, modes: Option<&Termios>) -> Result<(), ShellError> {
            let Some(modes) = modes else {
                return Ok(());
            };
            let _blocked = BlockedTerminalSignals::new()?;
            tcsetattr(std::io::stdin(), SetArg::TCSADRAIN, modes).map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not restore stopped job terminal modes",
                )
                .with_context(error.to_string())
                .with_help("Run `reset`, then cancel the stopped job and retry")
            })
        }

        fn restore(&mut self) -> Result<(), ShellError> {
            let Some(group) = self.restore_group else {
                self.lease.release();
                return Ok(());
            };
            let _blocked = BlockedTerminalSignals::new()?;
            tcsetpgrp(std::io::stdin(), group).map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not return the terminal to Quirl after the foreground job",
                )
                .with_context(error.to_string())
                .with_help("Run `jobs`, then retry the command from a controlling terminal")
            })?;
            if let Some(modes) = &self.restore_modes {
                tcsetattr(std::io::stdin(), SetArg::TCSADRAIN, modes).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Io,
                        "could not restore terminal modes after the foreground job",
                    )
                    .with_context(error.to_string())
                    .with_help("Run `reset` in the controlling terminal, then restart Quirl")
                })?;
            }
            self.restore_group = None;
            self.restore_modes = None;
            self.lease.release();
            Ok(())
        }
    }

    impl Drop for ForegroundTerminal {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    fn wait_for_foreground_children(
        children: &mut [JobChild],
        process_group_anchor: &mut Option<ProcessGroupAnchor>,
        request: Option<RequestContext<'_>>,
        observer: Option<&OutputObserverHandle<'_>>,
        output: Option<&Receiver<OutputEvent>>,
    ) -> Result<(), ShellError> {
        // A pipeline is one job: if any member stops, stop every remaining
        // live member before returning it to the job table. Waiting children in
        // source order can deadlock when an upstream process stops while a
        // downstream reader waits forever for its still-open pipe. Polling all
        // children every bounded turn observes exits and stops independently,
        // and every observed exit is reaped exactly once into `JobChild`.
        let mut stop_propagated = false;
        loop {
            drain_output_events(observer, output, 64)?;
            if let Some(observer) = observer {
                observer.maybe_tick()?;
            }
            for child in children
                .iter_mut()
                .filter(|child| child.status != JobStatus::Done)
            {
                poll_child_checked(child)?;
            }

            let anchor_stopped = process_group_anchor
                .as_mut()
                .map(ProcessGroupAnchor::poll_stopped)
                .transpose()?
                .unwrap_or(false);
            let any_stopped = anchor_stopped
                || children
                    .iter()
                    .any(|child| child.status == JobStatus::Stopped);
            if any_stopped && !stop_propagated {
                stop_live_children(children, process_group_anchor.as_ref())?;
                stop_propagated = true;
            }

            let all_done = children.iter().all(|child| child.status == JobStatus::Done);
            let all_live_stopped = children
                .iter()
                .filter(|child| child.status != JobStatus::Done)
                .all(|child| child.status == JobStatus::Stopped);
            if all_done || (any_stopped && all_live_stopped) {
                return Ok(());
            }

            if let Some(request) = request {
                request.ensure_active()?;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn drain_output_events(
        observer: Option<&OutputObserverHandle<'_>>,
        output: Option<&Receiver<OutputEvent>>,
        event_count_max: usize,
    ) -> Result<(), ShellError> {
        let (Some(observer), Some(output)) = (observer, output) else {
            return Ok(());
        };
        for _ in 0..event_count_max {
            let event = match output.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            };
            (observer.callback.borrow_mut())(ObservedActivity::Output {
                stream: event.stream,
                bytes: &event.bytes,
            })?;
        }
        Ok(())
    }

    fn stop_live_children(
        children: &[JobChild],
        process_group_anchor: Option<&ProcessGroupAnchor>,
    ) -> Result<(), ShellError> {
        if let Some(anchor) = process_group_anchor {
            return match anchor.signal(Signal::SIGSTOP) {
                Ok(()) | Err(Errno::ESRCH) => Ok(()),
                Err(error) => Err(ShellError::new(
                    ErrorCode::Io,
                    "could not stop the complete foreground pipeline",
                )
                .with_context(error.to_string())
                .with_help("Inspect the job with `jobs`, then cancel or resume it")),
            };
        }
        for child in children
            .iter()
            .filter(|child| child.status == JobStatus::Running)
        {
            let process_id = i32::try_from(child.child.id()).map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "child process id is outside the platform range",
                )
                .with_context(error.to_string())
                .with_help("Report this platform-specific process error")
            })?;
            match kill(Pid::from_raw(process_id), Signal::SIGSTOP) {
                Ok(()) | Err(Errno::ESRCH) => {}
                Err(error) => {
                    return Err(ShellError::new(
                        ErrorCode::Io,
                        "could not stop the complete foreground pipeline",
                    )
                    .with_context(error.to_string())
                    .with_help("Inspect the job with `jobs`, then cancel or resume it"));
                }
            }
        }
        Ok(())
    }

    fn outcome(status: i32, stdout: Option<String>, stderr: Option<String>) -> CommandOutcome {
        CommandOutcome {
            status,
            stdout,
            stderr,
        }
    }

    fn notification_outcome(
        status: i32,
        message: String,
        capture: bool,
    ) -> Result<CommandOutcome, ShellError> {
        if capture {
            return Ok(outcome(status, Some(message), Some(String::new())));
        }
        io_write_all(std::io::stdout(), message.as_bytes(), "job notification")?;
        Ok(outcome(status, None, None))
    }

    fn append_captured_output(
        retained: &mut String,
        next: &str,
        limit: usize,
    ) -> Result<(), ShellError> {
        if next.len() > limit.saturating_sub(retained.len()) {
            let available = limit.saturating_sub(retained.len());
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "captured process output exceeded the retained output limit",
            )
            .with_context(format!(
                "retained {} bytes; discarded at least {} bytes; limit {limit} bytes",
                retained.len(),
                next.len().saturating_sub(available)
            ))
            .with_help(
                "Reduce process output, redirect it to a file, or consume it in a pipeline",
            ));
        }
        retained.push_str(next);
        Ok(())
    }

    fn retained_output_limit(request: Option<&ProcessRequest>) -> usize {
        request.map_or(DEFAULT_CAPTURE_BYTES, |request| {
            request.max_output_bytes.min(DEFAULT_CAPTURE_BYTES)
        })
    }

    fn io_write_all(mut writer: impl Write, bytes: &[u8], target: &str) -> Result<(), ShellError> {
        writer.write_all(bytes).map_err(|error| {
            ShellError::new(ErrorCode::Io, format!("could not write {target}"))
                .with_context(error.to_string())
                .with_help("Check the destination and retry")
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::simulation_support::{DeterministicRng, configuration};
        use std::{
            fs,
            sync::atomic::{AtomicUsize, Ordering},
            time::Duration,
        };

        static NEXT_TEMP_PATH: AtomicUsize = AtomicUsize::new(0);
        static TERMINAL_LEASE_TEST_LOCK: Mutex<()> = Mutex::new(());

        fn temporary_path(label: &str) -> std::path::PathBuf {
            env::temp_dir().join(format!(
                "quirl-process-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed)
            ))
        }

        fn spawn_test_pipeline_stage(guard: &mut PipelineConstructionGuard, script: &str) -> Pid {
            let first_stage = guard.process_group.is_none();
            let mut command = if first_stage {
                let mut command = Command::new(PROCESS_GROUP_ANCHOR_PATH);
                command.args([
                    "-c",
                    PROCESS_GROUP_LEADER_STAGE_SCRIPT,
                    "quirl-process-group-stage-test",
                    "sh",
                    "-c",
                    script,
                ]);
                command.process_group(0);
                command
            } else {
                let mut command = Command::new("sh");
                command
                    .arg("-c")
                    .arg(script)
                    .process_group(guard.process_group.unwrap_or(0));
                command
            };
            let child = command.spawn().unwrap();
            let process_id = Pid::from_raw(i32::try_from(child.id()).unwrap());
            if first_stage {
                guard
                    .push_staged_group_leader(child, "sh", script, None)
                    .unwrap();
            } else {
                guard.push(child).unwrap();
            }
            process_id
        }

        fn wait_for_status(executor: &mut NativeExecutor, status: JobStatus) -> Vec<JobState> {
            for _ in 0..100 {
                let jobs = executor.jobs();
                if jobs.iter().any(|job| job.status == status) {
                    return jobs;
                }
                thread::sleep(Duration::from_millis(5));
            }
            executor.jobs()
        }

        #[test]
        fn pathname_matching_is_unicode_aware_and_non_exponential() {
            assert!(glob_matches("über-?.[rR][s-t]", "über-🌀.rs"));
            assert!(glob_matches(".quirl*", ".quirl-history"));
            assert!(!glob_matches("*", ".quirl-history"));
            assert!(!glob_matches(
                "********************************x",
                "a-very-long-candidate-without-the-final-letter",
            ));
        }

        #[test]
        fn path_ls_and_external_commands_share_a_byte_pipeline() {
            let mut executor = NativeExecutor::default();
            let result = executor.execute_capture("ls | grep Cargo.toml").unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some("Cargo.toml\n"));
        }

        #[test]
        fn ls_resolves_from_the_session_path_without_builtin_interception() {
            use std::os::unix::fs::PermissionsExt;

            let directory = temporary_path("path-ls");
            let executable = directory.join("ls");
            fs::create_dir_all(&directory).unwrap();
            fs::write(&executable, "#!/bin/sh\nprintf 'system-ls:%s\\n' \"$*\"\n").unwrap();
            let mut permissions = fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&executable, permissions).unwrap();

            let mut executor = NativeExecutor::default();
            let result = executor
                .execute_capture(&format!("export PATH={}; ls -al", directory.display()))
                .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some("system-ls:-al\n"));
            assert_eq!(result.stderr.as_deref(), Some(""));

            fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn here_string_larger_than_pipe_capacity_starts_the_reader_before_writing() {
            let payload = "x".repeat(128 * 1024);
            let result = NativeExecutor::default()
                .execute_capture(&format!("cat <<< {payload}"))
                .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(
                result.stdout.as_deref().map(str::len),
                Some(payload.len() + 1)
            );
            assert_eq!(
                result.stdout.as_deref(),
                Some(format!("{payload}\n").as_str())
            );
        }

        #[test]
        fn here_string_input_is_bounded_before_pipe_creation() {
            let payload = "x".repeat(HERE_STRING_BYTES_MAX);
            let error = NativeExecutor::default()
                .execute_capture(&format!("cat <<< {payload}"))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("here-string"));
            assert!(error.details.context.iter().any(|context| {
                context.contains(&format!("limit {HERE_STRING_BYTES_MAX} bytes"))
                    && context.contains(&format!("observed {} bytes", payload.len() + 1))
            }));
        }

        #[test]
        fn cancelling_a_blocked_here_string_writer_reaps_the_reader() {
            let payload = "x".repeat(128 * 1024);
            let request = ProcessRequest {
                command: format!("sh -c 'sleep 5' <<< {payload}"),
                deadline: Duration::from_millis(20),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let started = Instant::now();
            let error = NativeExecutor::default()
                .execute_capture_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn arithmetic_input_and_nesting_are_bounded() {
            let source = "1".repeat(ARITHMETIC_SOURCE_BYTES_MAX + 1);
            let error = evaluate_arithmetic(&source).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.details.context.iter().any(|context| {
                context.contains(&format!("limit {ARITHMETIC_SOURCE_BYTES_MAX} bytes"))
            }));

            let nested = format!(
                "{}1{}",
                "(".repeat(ARITHMETIC_DEPTH_MAX + 1),
                ")".repeat(ARITHMETIC_DEPTH_MAX + 1)
            );
            let error = evaluate_arithmetic(&nested).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("nesting depth"));
        }

        #[test]
        fn arithmetic_minimum_division_and_negation_return_overflow_errors() {
            for source in [
                "(-9223372036854775807 - 1) / -1",
                "-(-9223372036854775807 - 1)",
            ] {
                let error = evaluate_arithmetic(source).unwrap_err();
                assert_eq!(error.code, ErrorCode::InvalidCommand);
                assert!(error.message.contains("overflowed"));
            }
        }

        #[test]
        fn native_source_and_process_graph_counts_are_bounded_before_spawning() {
            let oversized_source = "x".repeat(crate::NATIVE_COMMAND_BYTES_MAX + 1);
            let error = NativeExecutor::default()
                .execute_capture(&oversized_source)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("source"));

            let command_list = std::iter::repeat_n("true", crate::NATIVE_PIPELINES_MAX + 1)
                .collect::<Vec<_>>()
                .join(";");
            let error = NativeExecutor::default()
                .execute_capture(&command_list)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("pipeline limit"));

            let pipeline = std::iter::repeat_n("cat", crate::NATIVE_PIPELINE_STAGES_MAX + 1)
                .collect::<Vec<_>>()
                .join("|");
            let error = NativeExecutor::default()
                .execute_capture(&pipeline)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("stage limit"));
        }

        #[test]
        fn expansion_budget_accepts_the_exact_boundary_and_rejects_the_next_byte() {
            let variable = format!(
                "QUIRL_EXPANSION_BOUNDARY_{}",
                NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed)
            );
            let command_bytes = "printf".len();
            let value = "x".repeat((EXPANSION_BYTES_MAX - command_bytes) / 2);
            let mut executor = NativeExecutor::default();
            executor
                .set_environment_variable(variable.clone(), value)
                .unwrap();
            let exact_source = format!("printf ${{{variable}}}${{{variable}}}");
            let exact = parse_command_list(&exact_source).unwrap();
            let expanded = executor
                .expand_pipeline(&exact.pipelines[0], None, 0)
                .unwrap();
            assert_eq!(
                expanded.commands[0]
                    .words
                    .iter()
                    .map(String::len)
                    .sum::<usize>(),
                EXPANSION_BYTES_MAX
            );

            let oversized_source = format!("printf ${{{variable}}}${{{variable}}}x");
            let oversized = parse_command_list(&oversized_source).unwrap();
            let error = executor
                .expand_pipeline(&oversized.pipelines[0], None, 0)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.details.context.iter().any(|context| {
                context.contains(&format!("limit {EXPANSION_BYTES_MAX} bytes"))
                    && context.contains(&format!("retained {EXPANSION_BYTES_MAX} bytes"))
                    && context.contains(&format!("observed {} bytes", EXPANSION_BYTES_MAX + 1))
            }));
        }

        #[test]
        fn repeated_status_expansion_cannot_amplify_past_the_pipeline_budget() {
            let repetitions = EXPANSION_BYTES_MAX / i32::MIN.to_string().len() + 1;
            let source = format!("printf {}", "$?".repeat(repetitions));
            let graph = parse_command_list(&source).unwrap();
            let error = NativeExecutor::default()
                .expand_pipeline(&graph.pipelines[0], None, i32::MIN)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("expansion"));
        }

        #[test]
        fn pathname_expansion_counts_bytes_while_building_candidate_paths() {
            let directory = temporary_path("pathname-byte-budget");
            fs::create_dir_all(&directory).unwrap();
            for index in 0..300 {
                fs::write(directory.join(format!("entry-{index}")), "x").unwrap();
            }
            let suffix = "x".repeat(4_000);
            let source = format!("printf {}/*/{suffix}", directory.display());
            let graph = parse_command_list(&source).unwrap();
            let error = NativeExecutor::default()
                .expand_pipeline(&graph.pipelines[0], None, 0)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("pathname expansion"));
            fs::remove_dir_all(directory).unwrap();
        }

        #[test]
        fn redirects_and_boolean_connectors_use_the_native_graph() {
            let path = temporary_path("redirect");
            let command = format!(
                "printf first > {} && printf second >> {}",
                path.display(),
                path.display()
            );
            let mut executor = NativeExecutor::default();
            let result = executor.execute_capture(&command).unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(fs::read_to_string(&path).unwrap(), "firstsecond");
            fs::remove_file(path).unwrap();
        }

        #[test]
        fn captured_boolean_lists_preserve_output_from_every_executed_pipeline() {
            let mut executor = NativeExecutor::default();
            let result = executor
                .execute_capture(
                    "sh -c 'printf left; printf left-error >&2; exit 7' && printf no || sh -c 'printf recovered; printf recovered-error >&2'",
                )
                .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some("leftrecovered"));
            assert_eq!(result.stderr.as_deref(), Some("left-errorrecovered-error"));
        }

        #[test]
        fn streaming_capture_reports_output_before_process_exit() {
            let finished = temporary_path("streaming-finished");
            let request = ProcessRequest {
                command: format!(
                    "sh -c 'printf first; sleep 0.2; printf second; : > {}'",
                    finished.display()
                ),
                deadline: Duration::from_secs(2),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let mut first_arrived_while_running = false;
            let outcome = NativeExecutor::default()
                .execute_capture_request_streaming(request, &mut |activity| {
                    if let ObservedActivity::Output { stream, bytes } = activity
                        && stream == OutputStream::Stdout
                        && bytes == b"first"
                    {
                        first_arrived_while_running = !finished.exists();
                    }
                    Ok(())
                })
                .unwrap();
            assert!(first_arrived_while_running);
            assert_eq!(outcome.stdout.as_deref(), Some("firstsecond"));
            fs::remove_file(finished).unwrap();
        }

        #[test]
        fn background_jobs_are_structured_and_can_be_foregrounded() {
            let mut executor = NativeExecutor::default();
            executor.execute_capture("sh -c 'sleep 0.02' &").unwrap();
            let jobs = executor.jobs();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].status, JobStatus::Running);
            let mut jobs = executor.jobs();
            for _ in 0..20 {
                if jobs[0].status == JobStatus::Done {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
                jobs = executor.jobs();
            }
            assert_eq!(jobs[0].status, JobStatus::Done);
        }

        #[test]
        fn inherited_background_notification_is_not_retained() {
            let mut executor = NativeExecutor::default();
            let outcome = executor.execute("sleep 30 &").unwrap();
            assert_eq!(outcome.stdout, None);
            assert_eq!(outcome.stderr, None);
            executor.cancel_job(1).unwrap();
        }

        #[test]
        fn invalid_native_syntax_becomes_a_labeled_shell_error() {
            let error = NativeExecutor::default()
                .execute_capture("printf hello |")
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidCommand);
            assert_eq!(error.details.labels[0].start, 13);
            assert!(!error.details.help.is_empty());
        }

        #[test]
        fn capture_drains_large_stdout_and_stderr_without_deadlocking() {
            let mut executor = NativeExecutor::default();
            let result = executor
            .execute_capture(
                r#"sh -c 'i=0; while [ "$i" -lt 20000 ]; do printf eeeeeeee >&2; i=$((i+1)); done; printf done'"#,
            )
            .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some("done"));
            assert_eq!(result.stderr.as_deref().map(str::len), Some(160_000));
        }

        #[test]
        fn capture_collects_stderr_from_every_pipeline_stage() {
            let result = NativeExecutor::default()
                .execute_capture(
                    "sh -c 'printf first >&2' | sh -c 'cat >/dev/null; printf second >&2'",
                )
                .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stderr.as_deref(), Some("firstsecond"));
        }

        #[test]
        fn bounded_capture_cancels_a_process_tree_at_its_deadline() {
            let request = ProcessRequest {
                command: "sh -c 'sleep 5'".to_owned(),
                deadline: Duration::from_millis(20),
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let started = Instant::now();
            let error = NativeExecutor::default()
                .execute_capture_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn inherited_execution_observes_deadline_without_retaining_output() {
            let request = ProcessRequest {
                command: "sh -c 'sleep 5'".to_owned(),
                deadline: Duration::from_millis(20),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1,
            };
            let started = Instant::now();
            let error = NativeExecutor::default()
                .execute_interactive_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("deadline"));
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn inherited_execution_observes_shared_cancellation() {
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_cancelled = Arc::clone(&cancelled);
            let worker = std::thread::spawn(move || {
                NativeExecutor::default().execute_interactive_request(ProcessRequest {
                    command: "sh -c 'sleep 5'".to_owned(),
                    deadline: Duration::from_secs(1),
                    cancelled: worker_cancelled,
                    max_output_bytes: 1,
                })
            });
            std::thread::sleep(Duration::from_millis(20));
            cancelled.store(true, Ordering::Relaxed);
            let error = worker.join().unwrap().unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("cancelled"));
        }

        #[test]
        fn exited_leader_cannot_leave_a_descendant_holding_capture_open() {
            let started = Instant::now();
            let outcome = NativeExecutor::default()
                .execute_capture("sh -c 'sleep 10 & exec true'")
                .unwrap();
            assert_eq!(outcome.status, 0);
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn one_absolute_deadline_bounds_the_complete_command_list() {
            let request = ProcessRequest {
                command: "sh -c 'sleep 0.07'; sh -c 'sleep 0.07'".to_owned(),
                deadline: Duration::from_millis(110),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let started = Instant::now();
            let error = NativeExecutor::default()
                .execute_capture_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("deadline"));
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn nested_substitution_consumes_the_parent_absolute_deadline() {
            let request = ProcessRequest {
                command: "printf '%s' $(sh -c 'sleep 0.07; printf nested'); sh -c 'sleep 0.07'"
                    .to_owned(),
                deadline: Duration::from_millis(110),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let started = Instant::now();
            let error = NativeExecutor::default()
                .execute_capture_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("deadline"));
            assert!(started.elapsed() < Duration::from_secs(1));
        }

        #[test]
        fn terminal_lease_waits_are_bounded_with_and_without_a_request() {
            let held = ForegroundTerminalLease::acquire_from(
                &TERMINAL_LEASE_TEST_LOCK,
                None,
                Duration::from_secs(1),
            )
            .unwrap();
            let request = ProcessRequest {
                command: "true".to_owned(),
                deadline: Duration::from_millis(20),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let context = RequestContext::new(&request).unwrap();
            let started = Instant::now();
            let error = ForegroundTerminalLease::acquire_from(
                &TERMINAL_LEASE_TEST_LOCK,
                Some(context),
                Duration::from_secs(1),
            )
            .err()
            .unwrap();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(started.elapsed() < Duration::from_secs(1));

            let started = Instant::now();
            let error = ForegroundTerminalLease::acquire_from(
                &TERMINAL_LEASE_TEST_LOCK,
                None,
                Duration::from_millis(20),
            )
            .err()
            .unwrap();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.details.context.iter().any(|context| {
                context.contains("limit 20 ms") && context.contains("observed at least")
            }));
            assert!(started.elapsed() < Duration::from_secs(1));
            drop(held);
            assert!(
                ForegroundTerminalLease::acquire_from(
                    &TERMINAL_LEASE_TEST_LOCK,
                    None,
                    Duration::from_secs(1)
                )
                .is_ok()
            );
        }

        #[test]
        fn unleased_terminal_wrapper_never_changes_the_foreground_group() {
            if !std::io::stdin().is_terminal() {
                return;
            }
            let original_group = tcgetpgrp(std::io::stdin()).unwrap();
            let anchor = ProcessGroupAnchor::spawn().unwrap();
            let terminal = ForegroundTerminal::give_to(
                Some(anchor.process_group()),
                ForegroundTerminalLease::none(),
            )
            .unwrap();

            assert_eq!(tcgetpgrp(std::io::stdin()).unwrap(), original_group);

            drop(terminal);
            anchor.terminate_owned_group().unwrap();
        }

        #[test]
        fn bounded_capture_drains_but_does_not_retain_unbounded_output() {
            let request = ProcessRequest {
                command: "sh -c 'yes x | head -c 65536'".to_owned(),
                deadline: Duration::from_secs(1),
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let error = NativeExecutor::default()
                .execute_capture_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("output limit"));
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|context| context.contains("discarded") && context.contains("retained"))
            );
        }

        #[test]
        fn ordinary_capture_has_a_mandatory_retention_limit() {
            let error = NativeExecutor::default()
                .execute_capture(&format!(
                    "sh -c 'yes x | head -c {}'",
                    DEFAULT_CAPTURE_BYTES + 32 * 1024
                ))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("output limit"));
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|context| context.contains("discarded 32768 bytes"))
            );
        }

        #[test]
        fn interactive_helper_emits_above_the_capture_limit() {
            if env::var_os("QUIRL_INTERACTIVE_LIMIT_HELPER").is_none() {
                return;
            }
            let outcome = NativeExecutor::default()
                .execute_interactive(&format!(
                    "sh -c 'yes x | head -c {}'",
                    DEFAULT_CAPTURE_BYTES + 32 * 1024
                ))
                .unwrap();
            assert_eq!(outcome.status, 0);
            assert_eq!(outcome.stdout, None);
            assert_eq!(outcome.stderr, None);
        }

        #[test]
        fn interactive_output_above_the_capture_limit_streams_without_failure() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "platform::tests::interactive_helper_emits_above_the_capture_limit",
                    "--nocapture",
                ])
                .env("QUIRL_INTERACTIVE_LIMIT_HELPER", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success());
        }

        #[test]
        fn pipeline_stages_share_one_stderr_retention_budget() {
            let request = ProcessRequest {
                command: "sh -c 'yes a | head -c 800 >&2' | sh -c 'cat >/dev/null; yes b | head -c 800 >&2'"
                    .to_owned(),
                deadline: Duration::from_secs(1),
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: 1024,
            };
            let error = NativeExecutor::default()
                .execute_capture_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.details.context.iter().any(|context| {
                context.contains("retained 1024 bytes") && context.contains("discarded 576 bytes")
            }));
        }

        #[test]
        fn sandbox_request_can_tighten_but_not_expand_the_default_capture_ceiling() {
            let request = ProcessRequest {
                command: format!("sh -c 'yes x | head -c {}'", DEFAULT_CAPTURE_BYTES + 1024),
                deadline: Duration::from_secs(1),
                cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                max_output_bytes: DEFAULT_CAPTURE_BYTES * 2,
            };
            let error = NativeExecutor::default()
                .execute_capture_request(request)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|context| context.contains("discarded 1024 bytes"))
            );
        }

        #[test]
        fn redirects_override_pipe_ends_without_falling_back_to_shell_stdin() {
            let output = temporary_path("pipe-output");
            let input = temporary_path("pipe-input");
            fs::write(&input, "from-file").unwrap();
            let mut executor = NativeExecutor::default();

            let result = executor
                .execute_capture(&format!("printf hidden > {} | cat", output.display()))
                .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some(""));
            assert_eq!(fs::read_to_string(&output).unwrap(), "hidden");

            let result = executor
                .execute_capture(&format!("printf pipe | cat < {}", input.display()))
                .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some("from-file"));

            fs::remove_file(output).unwrap();
            fs::remove_file(input).unwrap();
        }

        #[test]
        fn input_redirects_apply_in_source_order_and_last_redirect_wins() {
            let input = temporary_path("ordered-input");
            let missing = temporary_path("ordered-missing");
            fs::write(&input, "from-file").unwrap();

            let from_file = NativeExecutor::default()
                .execute_capture(&format!("cat <<< inline < {}", input.display()))
                .unwrap();
            assert_eq!(from_file.stdout.as_deref(), Some("from-file"));

            let from_here_string = NativeExecutor::default()
                .execute_capture(&format!("cat < {} <<< inline", input.display()))
                .unwrap();
            assert_eq!(from_here_string.stdout.as_deref(), Some("inline\n"));

            let error = NativeExecutor::default()
                .execute_capture(&format!("cat < {} <<< inline", missing.display()))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Io);
            fs::remove_file(input).unwrap();
        }

        #[test]
        fn builtin_redirects_are_opened_before_state_mutation() {
            let variable = format!(
                "QUIRL_PROCESS_REDIRECT_{}_{}",
                std::process::id(),
                NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed)
            );
            assert!(env::var_os(&variable).is_none());
            let missing = temporary_path("missing-parent").join("output");
            let mut executor = NativeExecutor::default();
            let error = executor
                .execute_capture(&format!(
                    "export {variable}=changed > {}",
                    missing.display()
                ))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Io);
            assert!(env::var_os(&variable).is_none());

            let error = executor
                .execute_capture(&format!("export {variable}=changed &"))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidArgument);
            assert!(env::var_os(&variable).is_none());
        }

        #[test]
        fn command_redirection_and_quoted_paths_preserve_words() {
            let directory = temporary_path("directory with spaces");
            let output = temporary_path("builtin-output");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("visible.txt"), "contents").unwrap();
            let mut executor = NativeExecutor::default();
            let result = executor
                .execute_capture(&format!(
                    "ls '{}' > {}",
                    directory.display(),
                    output.display()
                ))
                .unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some(""));
            assert!(fs::read_to_string(&output).unwrap().contains("visible.txt"));
            fs::remove_dir_all(directory).unwrap();
            fs::remove_file(output).unwrap();
        }

        #[test]
        fn foreground_stops_are_retained_and_fg_preserves_the_job_exit_status() {
            let mut executor = NativeExecutor::default();
            let stopped = executor.execute("sh -c 'kill -STOP $$; exit 7'").unwrap();
            assert_ne!(stopped.status, 0);
            assert_eq!(stopped.stdout, None);
            assert_eq!(stopped.stderr, None);
            let jobs = executor.jobs();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].status, JobStatus::Stopped);

            let finished = executor.execute_capture("fg %1").unwrap();
            assert_eq!(finished.status, 7);
            assert!(executor.jobs().is_empty());
        }

        #[test]
        fn refresh_controls_contain_orphans_before_joining_capture_readers() {
            for control in ["jobs", "fg %1", "bg %1"] {
                let process_id_path = temporary_path("refresh-orphan");
                let command = format!(
                    "sh -c 'kill -STOP $$; sleep 5 & printf %s $! > {}; exit 0'",
                    process_id_path.display()
                );
                let mut executor = NativeExecutor::default();
                executor.execute_capture(&command).unwrap();
                executor.execute_capture("bg %1").unwrap();
                for _ in 0..100 {
                    if process_id_path.exists() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                assert!(process_id_path.exists());
                thread::sleep(Duration::from_millis(20));

                let started = Instant::now();
                if control == "jobs" {
                    executor.execute_capture(control).unwrap();
                } else {
                    let error = executor.execute_capture(control).unwrap_err();
                    assert_eq!(error.code, ErrorCode::InvalidArgument);
                }
                assert!(started.elapsed() < Duration::from_secs(1));

                let process_id = fs::read_to_string(&process_id_path)
                    .unwrap()
                    .trim()
                    .parse::<i32>()
                    .unwrap();
                for _ in 0..100 {
                    if kill(Pid::from_raw(process_id), None).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                assert!(kill(Pid::from_raw(process_id), None).is_err());
                fs::remove_file(process_id_path).unwrap();
            }
        }

        #[test]
        fn refresh_contains_orphans_before_joining_blocked_here_string_writers() {
            let process_id_path = temporary_path("refresh-writer-orphan");
            let payload = "x".repeat(128 * 1024);
            let command = format!(
                "sh -c 'kill -STOP $$; sleep 5 & printf %s $! > {}; exit 0' <<< {payload}",
                process_id_path.display()
            );
            let mut executor = NativeExecutor::default();
            executor.execute(&command).unwrap();
            executor.execute("bg %1").unwrap();
            for _ in 0..100 {
                if process_id_path.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(process_id_path.exists());
            thread::sleep(Duration::from_millis(20));

            let started = Instant::now();
            assert_eq!(executor.jobs()[0].status, JobStatus::Done);
            assert!(started.elapsed() < Duration::from_secs(1));
            let process_id = fs::read_to_string(&process_id_path)
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap();
            for _ in 0..100 {
                if kill(Pid::from_raw(process_id), None).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(kill(Pid::from_raw(process_id), None).is_err());
            fs::remove_file(process_id_path).unwrap();
        }

        #[test]
        fn foreground_terminal_mode_failure_reaps_children_and_finishes_tasks() {
            let payload = "x".repeat(128 * 1024);
            let mut executor = NativeExecutor::default();
            executor
                .execute_capture(&format!(
                    "sh -c 'kill -STOP $$; kill -STOP $$; sleep 5' <<< {payload}"
                ))
                .unwrap();
            let process_id =
                Pid::from_raw(i32::try_from(executor.jobs[0].children[0].child.id()).unwrap());
            executor.fail_stopped_terminal_mode_read = true;

            let started = Instant::now();
            let error = executor.foreground(Some(1)).unwrap_err();
            assert_eq!(error.code, ErrorCode::Io);
            assert!(error.message.contains("injected"));
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(executor.jobs.is_empty());
            assert!(kill(process_id, None).is_err());
        }

        #[test]
        fn suspend_refreshes_mixed_stages_and_stops_only_running_children() {
            let mut executor = NativeExecutor::default();
            executor.execute_capture("true | sleep 5 &").unwrap();
            for _ in 0..100 {
                executor.refresh_jobs();
                if executor.jobs[0].children[0].status == JobStatus::Done {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(executor.jobs[0].children[0].status, JobStatus::Done);

            let state = executor.suspend_job(1).unwrap();
            assert_eq!(state.status, JobStatus::Stopped);
            assert_eq!(executor.jobs[0].children[0].status, JobStatus::Done);
            assert_eq!(executor.jobs[0].children[1].status, JobStatus::Stopped);
            executor.cancel_job(1).unwrap();
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        #[test]
        fn ptrace_wait_states_remain_live_and_stopped() {
            let process_id = Pid::from_raw(7);
            let mut child_status = JobStatus::Running;
            let mut exit_status = None;

            assert!(record_wait_status(
                WaitStatus::PtraceEvent(process_id, Signal::SIGTRAP, 1),
                &mut child_status,
                &mut exit_status,
            ));
            assert_eq!(child_status, JobStatus::Stopped);
            assert_eq!(exit_status, Some(128 + Signal::SIGTRAP as i32));

            child_status = JobStatus::Running;
            exit_status = None;
            assert!(record_wait_status(
                WaitStatus::PtraceSyscall(process_id),
                &mut child_status,
                &mut exit_status,
            ));
            assert_eq!(child_status, JobStatus::Stopped);
            assert_eq!(exit_status, Some(128 + Signal::SIGTRAP as i32));
        }

        #[test]
        fn downstream_stop_is_observed_without_waiting_for_the_upstream_stage() {
            let mut executor = NativeExecutor::default();
            let started = Instant::now();
            let stopped = executor
                .execute_capture("sh -c 'sleep 5' | sh -c 'kill -STOP $$'")
                .unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_ne!(stopped.status, 0);

            let jobs = executor.jobs();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].status, JobStatus::Stopped);
            executor.cancel_job(jobs[0].id).unwrap();
            assert_eq!(executor.jobs()[0].status, JobStatus::Done);
        }

        #[test]
        fn stopped_upstream_cannot_hang_a_downstream_pipe_reader() {
            let mut executor = NativeExecutor::default();
            let started = Instant::now();
            executor
                .execute_capture("sh -c 'kill -STOP $$' | cat")
                .unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));

            let jobs = executor.jobs();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].status, JobStatus::Stopped);
            executor.cancel_job(jobs[0].id).unwrap();
        }

        #[test]
        fn stopped_background_jobs_transition_through_bg_to_done() {
            let mut executor = NativeExecutor::default();
            executor
                .execute_capture("sh -c 'sleep 0.05; exit 3' &")
                .unwrap();
            let group = executor.jobs()[0].process_group.unwrap();
            killpg(Pid::from_raw(group), Signal::SIGSTOP).unwrap();
            let jobs = wait_for_status(&mut executor, JobStatus::Stopped);
            assert_eq!(jobs[0].status, JobStatus::Stopped);

            executor.execute_capture("bg %1").unwrap();
            assert_eq!(executor.jobs()[0].status, JobStatus::Running);
            let jobs = wait_for_status(&mut executor, JobStatus::Done);
            assert_eq!(jobs[0].status, JobStatus::Done);
            assert_eq!(jobs[0].exit_status, Some(3));
        }

        #[test]
        fn pipeline_construction_guard_kills_and_reaps_children_on_early_errors() {
            let mut guard = PipelineConstructionGuard::new();
            let pid = spawn_test_pipeline_stage(&mut guard, "sleep 10");
            let anchor_pid = Pid::from_raw(
                i32::try_from(guard.process_group_anchor.as_ref().unwrap().child.id()).unwrap(),
            );
            drop(guard);
            assert!(kill(pid, None).is_err());
            assert!(kill(anchor_pid, None).is_err());
        }

        #[test]
        fn anchor_retains_group_identity_after_every_guest_is_reaped() {
            let mut guard = PipelineConstructionGuard::new();
            let group_leader = spawn_test_pipeline_stage(&mut guard, "exit 23");
            let process_group = guard.process_group.unwrap();
            let anchor_pid = Pid::from_raw(
                i32::try_from(guard.process_group_anchor.as_ref().unwrap().child.id()).unwrap(),
            );
            assert_eq!(group_leader.as_raw(), process_group);
            assert_ne!(anchor_pid, group_leader);

            for _ in 0..1_000 {
                poll_child(&mut guard.children[0]);
                if guard.children[0].status == JobStatus::Done {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(guard.children[0].status, JobStatus::Done);
            assert_eq!(guard.children[0].exit_status, Some(23));
            assert_eq!(getpgid(Some(anchor_pid)), Ok(group_leader));
            assert_eq!(killpg(group_leader, None), Ok(()));

            drop(guard);
        }

        #[test]
        fn anchor_spawn_and_readiness_failures_return_without_partial_ownership() {
            let missing = ProcessGroupAnchor::spawn_with(
                "/definitely/missing/quirl-process-group-anchor",
                PROCESS_GROUP_ANCHOR_SCRIPT,
            )
            .unwrap_err();
            assert_eq!(missing.code, ErrorCode::ProcessSpawn);
            assert!(missing.message.contains("anchor"));

            let early_exit = ProcessGroupAnchor::spawn_with("/bin/sh", "exit 0").unwrap_err();
            assert_eq!(early_exit.code, ErrorCode::ProcessSpawn);
            assert!(early_exit.message.contains("ready"));

            let missing_group = ProcessGroupAnchor::join(i32::MAX, None).unwrap_err();
            assert_eq!(missing_group.code, ErrorCode::ProcessSpawn);
            assert!(missing_group.message.contains("anchor"));

            let request = ProcessRequest {
                command: "anchor cancellation fixture".to_owned(),
                deadline: Duration::from_secs(30),
                cancelled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                max_output_bytes: 0,
            };
            let request = RequestContext::new(&request).unwrap();
            let cancelled =
                ProcessGroupAnchor::spawn_with_group("/bin/sh", "sleep 30", None, Some(request))
                    .unwrap_err();
            assert_eq!(cancelled.code, ErrorCode::ResourceLimit);
            assert!(cancelled.message.contains("cancelled"));
        }

        #[test]
        fn anchor_ignores_interrupt_and_reports_terminal_stop() {
            let mut anchor = ProcessGroupAnchor::spawn().unwrap();
            let process_group = Pid::from_raw(anchor.process_group());
            anchor.signal(Signal::SIGINT).unwrap();
            assert!(!anchor.poll_stopped().unwrap());
            anchor.signal(Signal::SIGTSTP).unwrap();
            for _ in 0..1_000 {
                if anchor.poll_stopped().unwrap() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(anchor.stopped);
            assert_eq!(getpgid(Some(process_group)), Ok(process_group));
            anchor.signal(Signal::SIGCONT).unwrap();
            for _ in 0..1_000 {
                if !anchor.poll_stopped().unwrap() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(!anchor.stopped);
            anchor.terminate_owned_group().unwrap();
        }

        #[test]
        fn anchor_stop_sentinel_stops_guest_that_misses_ctrl_z() {
            let ready_path = temporary_path("anchor-stop-sentinel");
            let mut guard = PipelineConstructionGuard::new();
            spawn_test_pipeline_stage(
                &mut guard,
                &format!(
                    "trap '' TSTP; printf ready > {}; sleep 30",
                    ready_path.display()
                ),
            );
            for _ in 0..1_000 {
                if ready_path.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(ready_path.exists());

            guard
                .process_group_anchor
                .as_ref()
                .unwrap()
                .signal(Signal::SIGTSTP)
                .unwrap();
            wait_for_foreground_children(
                &mut guard.children,
                &mut guard.process_group_anchor,
                None,
                None,
                None,
            )
            .unwrap();
            assert_eq!(guard.children[0].status, JobStatus::Stopped);
            assert!(guard.process_group_anchor.as_ref().unwrap().stopped);

            drop(guard);
            fs::remove_file(ready_path).unwrap();
        }

        #[test]
        fn eperm_cleanup_fails_closed_and_reaps_the_anchor_without_group_retry() {
            let mut anchor = ProcessGroupAnchor::spawn().unwrap();
            let process_group = Pid::from_raw(anchor.process_group());
            anchor.termination_signaled = true;
            let error = anchor.finish_termination(Err(Errno::EPERM)).unwrap_err();
            assert_eq!(error.code, ErrorCode::Io);
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|value| value.contains("EPERM"))
            );
            assert!(anchor.released);
            assert_eq!(anchor.process_group(), process_group.as_raw());
        }

        #[test]
        fn contained_child_cleanup_preserves_a_reaped_exit_status() {
            let containment = ChildProcessTree::new().unwrap();
            let mut command = Command::new("sh");
            command.args(["-c", "read _; exit 9"]).stdin(Stdio::piped());
            containment.configure(&mut command);
            let mut child = command.spawn().unwrap();
            containment.assign(&mut child).unwrap();

            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(b"exit\n").unwrap();
            drop(stdin);
            let status = child.wait().unwrap();
            assert_eq!(status.code(), Some(9));

            containment.terminate(&mut child).unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(9));
        }

        #[test]
        fn containment_drop_kills_descendants_after_the_direct_child_is_reaped() {
            let process_id_path = temporary_path("contained-descendant");
            let containment = ChildProcessTree::new().unwrap();
            let mut command = Command::new("sh");
            command.arg("-c").arg(format!(
                "sleep 30 & printf %s $! > {}; exit 0",
                process_id_path.display()
            ));
            containment.configure(&mut command);
            let mut child = command.spawn().unwrap();
            containment.assign(&mut child).unwrap();
            assert!(child.wait().unwrap().success());
            let descendant = fs::read_to_string(&process_id_path)
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap();

            drop(containment);
            for _ in 0..1_000 {
                if kill(Pid::from_raw(descendant), None).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert!(kill(Pid::from_raw(descendant), None).is_err());
            fs::remove_file(process_id_path).unwrap();
        }

        #[test]
        fn live_child_in_the_wrong_group_fails_closed_and_is_reaped() {
            let mut command = Command::new("sh");
            command.arg("-c").arg("sleep 10");
            let mut child = command.spawn().unwrap();
            let process_id = i32::try_from(child.id()).unwrap();

            let error = verify_process_group(&mut child, process_id, process_id).unwrap_err();

            assert_eq!(error.code, ErrorCode::ProcessSpawn);
            assert!(error.message.contains("process group"));
            assert!(child.try_wait().unwrap().is_some());
            assert!(kill(Pid::from_raw(process_id), None).is_err());
        }

        #[test]
        fn immediate_guest_exit_does_not_release_the_owned_group_early() {
            for _ in 0..512 {
                let outcome = NativeExecutor::default()
                    .execute_capture("true | cat")
                    .unwrap();
                assert_eq!(outcome.status, 0);

                let outcome = NativeExecutor::default()
                    .execute_capture("printf value | sh -c 'exit 23'")
                    .unwrap();
                assert_eq!(outcome.status, 23);
            }
        }

        #[test]
        fn later_stage_spawn_failure_reaps_started_stage_and_preserves_spawn_error() {
            let process_id_path = temporary_path("construction-child");
            let command = format!(
                "sh -c 'printf %s $$ > {}; sleep 10' | /definitely/missing/quirl-stage",
                process_id_path.display()
            );
            let error = NativeExecutor::default()
                .execute_capture(&command)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ProcessSpawn);
            assert!(error.message.contains("/definitely/missing/quirl-stage"));

            for _ in 0..100 {
                if process_id_path.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            if let Ok(value) = fs::read_to_string(&process_id_path) {
                let process_id = value.trim().parse::<i32>().unwrap();
                assert!(kill(Pid::from_raw(process_id), None).is_err());
                fs::remove_file(process_id_path).unwrap();
            }
        }

        #[test]
        fn job_table_prunes_done_records_and_rejects_a_full_live_table() {
            fn empty_job(id: u32, status: JobStatus) -> Job {
                Job {
                    state: JobState {
                        id,
                        command: "synthetic".to_owned(),
                        status,
                        process_group: None,
                        exit_status: (status == JobStatus::Done).then_some(0),
                    },
                    children: Vec::new(),
                    process_group_anchor: None,
                    terminal_modes: None,
                    capture: false,
                    stdout_reader: None,
                    stderr_readers: Vec::new(),
                    writers: Vec::new(),
                }
            }

            let mut executor = NativeExecutor::default();
            executor.jobs = (1..=u32::try_from(RETAINED_JOBS_MAX).unwrap())
                .map(|id| empty_job(id, JobStatus::Done))
                .collect();
            executor.next_job_id = u32::MAX;
            assert_eq!(executor.reserve_refreshed_job_id().unwrap(), u32::MAX);
            assert!(executor.jobs.is_empty());

            executor.jobs = (1..=u32::try_from(RETAINED_JOBS_MAX).unwrap())
                .map(|id| empty_job(id, JobStatus::Running))
                .collect();
            let error = executor.reserve_refreshed_job_id().unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.details.context.iter().any(|context| {
                context.contains(&format!("limit {RETAINED_JOBS_MAX} jobs"))
                    && context.contains(&format!("observed {RETAINED_JOBS_MAX} live jobs"))
            }));
        }

        #[test]
        fn seeded_construction_fault_schedule_reaps_every_started_child() {
            const PROCESS_CASES_MAX: usize = 32;
            const PIPELINE_STAGES_MAX: usize = 4;

            let (seed, requested_cases) = configuration();
            let cases = requested_cases.min(PROCESS_CASES_MAX);
            let mut rng = DeterministicRng::new(seed);
            for case_index in 0..cases {
                // Rotate through every spawn checkpoint. The seed varies the
                // planned suffix, which must never be reached after the fault.
                let fault_after = case_index % PIPELINE_STAGES_MAX + 1;
                let planned_stages = fault_after + rng.index(PIPELINE_STAGES_MAX + 1 - fault_after);
                let mut guard = PipelineConstructionGuard::new();
                let mut process_ids = Vec::with_capacity(fault_after);

                for stage_index in 0..planned_stages {
                    let process_id = spawn_test_pipeline_stage(&mut guard, "sleep 10");
                    process_ids.push(process_id);
                    if stage_index + 1 == fault_after {
                        break;
                    }
                }

                drop(guard); // Inject the construction failure at this checkpoint.
                for process_id in process_ids {
                    assert!(
                        kill(process_id, None).is_err(),
                        "seed={seed} case={case_index} fault_after={fault_after} pid={process_id} survived cleanup"
                    );
                }
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{
        DEFAULT_CAPTURE_BYTES, HERE_STRING_BYTES_MAX, ObservedActivity, OutputObserver,
        RETAINED_JOBS_MAX, SessionEnvironment, allocate_job_id, builtin, validate_native_plan,
        validate_native_source,
    };
    use quirl_core::{CommandOutcome, ErrorCode, OutputStream, ProcessRequest, ShellError};
    use quirl_syntax::{ListConnector, Pipeline, RedirectKind, SimpleCommand, parse_command_list};
    use serde::{Deserialize, Serialize};
    use std::{
        fs::{File, OpenOptions},
        io::{self, Read, Write},
        os::windows::io::AsRawHandle,
        process::{Child, ChildStdout, Command, Stdio},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread::{self, JoinHandle},
        time::Instant,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject,
        },
    };

    /// Aggregate lifecycle state for a native job.
    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum JobStatus {
        /// At least one child is executing.
        Running,
        /// Every live child is stopped.
        Stopped,
        /// Every child has exited or been reaped.
        Done,
    }

    /// Serializable snapshot of a Windows native job.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct JobState {
        /// Stable session-local job identifier.
        pub id: u32,
        /// Original command source associated with the job.
        pub command: String,
        /// Aggregate lifecycle state of the job's children.
        pub status: JobStatus,
        /// Reserved cross-platform process-group field; always `None` on Windows.
        pub process_group: Option<i32>,
        /// Final shell status after the job reaches [`JobStatus::Done`].
        pub exit_status: Option<i32>,
    }

    struct Job {
        state: JobState,
        children: Vec<Child>,
        exit_statuses: Vec<Option<i32>>,
        object: JobObject,
        writers: Vec<WriterTask>,
    }

    struct ReaderCapture {
        bytes: Vec<u8>,
        discarded_bytes: u64,
    }

    struct CaptureBudget {
        limit: usize,
        retained: AtomicUsize,
    }

    impl CaptureBudget {
        fn new(limit: usize) -> Self {
            Self {
                limit,
                retained: AtomicUsize::new(0),
            }
        }

        fn claim(&self, requested: usize) -> usize {
            let mut retained = self.retained.load(Ordering::Relaxed);
            loop {
                let claimed = requested.min(self.limit.saturating_sub(retained));
                match self.retained.compare_exchange_weak(
                    retained,
                    retained + claimed,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return claimed,
                    Err(observed) => retained = observed,
                }
            }
        }
    }

    type ReaderTask = JoinHandle<io::Result<ReaderCapture>>;
    type WriterTask = JoinHandle<io::Result<()>>;

    #[derive(Clone, Copy)]
    struct RequestContext<'a> {
        request: &'a ProcessRequest,
        deadline: Instant,
    }

    impl<'a> RequestContext<'a> {
        fn new(request: &'a ProcessRequest) -> Result<Self, ShellError> {
            let deadline = Instant::now()
                .checked_add(request.deadline)
                .ok_or_else(|| {
                    ShellError::new(
                        ErrorCode::ResourceLimit,
                        "process execution deadline is outside the platform range",
                    )
                    .with_context(format!("requested duration: {:?}", request.deadline))
                    .with_help("Use a finite process deadline supported by this platform")
                })?;
            Ok(Self { request, deadline })
        }

        fn ensure_active(self) -> Result<(), ShellError> {
            let cancelled = self.request.cancelled.load(Ordering::Relaxed);
            if !cancelled && Instant::now() < self.deadline {
                return Ok(());
            }
            let message = if cancelled {
                "process execution was cancelled"
            } else {
                "process execution exceeded its deadline"
            };
            Err(ShellError::new(ErrorCode::ResourceLimit, message)
                .with_help("Use a shorter-running command or increase the Lua policy deadline"))
        }
    }

    /// Windows native process executor with kill-on-close Job Object ownership.
    pub struct NativeExecutor {
        jobs: Vec<Job>,
        next_job_id: u32,
        noninteractive_host: bool,
        environment: SessionEnvironment,
    }

    /// A kill-on-close Job Object used by non-shell process adapters.
    pub struct ChildProcessTree(JobObject);

    impl ChildProcessTree {
        /// Create an empty kill-on-close Job Object.
        pub fn new() -> Result<Self, ShellError> {
            JobObject::new().map(Self)
        }

        /// Configure `command` before spawning a child for this containment object.
        /// Windows Job Object assignment is completed after spawn.
        pub fn configure(&self, _command: &mut Command) {}

        /// Assign `child` to this containment object.
        pub fn assign(&self, child: &mut Child) -> Result<(), ShellError> {
            self.0.assign(child)
        }

        /// Terminate every process assigned to this containment object.
        pub fn terminate(&self, _child: &mut Child) -> Result<(), ShellError> {
            self.0.terminate(130)
        }
    }

    impl Default for NativeExecutor {
        fn default() -> Self {
            Self {
                jobs: Vec::new(),
                next_job_id: 1,
                noninteractive_host: false,
                environment: SessionEnvironment::default(),
            }
        }
    }

    impl Drop for NativeExecutor {
        fn drop(&mut self) {
            for job in &mut self.jobs {
                if job.state.status != JobStatus::Done {
                    let _ = job.object.terminate(130);
                    wait_children(&mut job.children, &mut job.exit_statuses);
                }
                finish_writers_silently(&mut job.writers);
            }
        }
    }

    impl NativeExecutor {
        pub(crate) fn noninteractive_host() -> Self {
            let mut executor = Self::default();
            executor.noninteractive_host = true;
            executor
        }

        /// Replace one variable in this executor's private environment snapshot.
        ///
        /// Future child processes observe the update; the host process and
        /// independent executors remain unchanged.
        pub fn set_environment_variable(
            &mut self,
            name: String,
            value: String,
        ) -> Result<(), ShellError> {
            self.set_environment_variables(&[(name, value)])
        }

        /// Validate and atomically apply several private environment updates.
        pub fn set_environment_variables(
            &mut self,
            assignments: &[(String, String)],
        ) -> Result<(), ShellError> {
            self.environment.set_variables(assignments)
        }

        /// Apply this executor's complete environment snapshot to a child command.
        pub fn configure_child(&self, command: &mut Command) -> Result<(), ShellError> {
            self.environment.configure(command)
        }

        /// Snapshot this executor's private environment for background prompt probes.
        pub fn developer_context_probe(&self) -> crate::DeveloperContextProbe {
            crate::DeveloperContextProbe::new(self.environment.clone())
        }

        /// Generation of the private environment observed by future children.
        pub const fn environment_generation(&self) -> u64 {
            self.environment.generation
        }

        #[cfg(test)]
        pub(crate) fn replace_environment_for_test(&mut self, environment: SessionEnvironment) {
            self.environment = environment;
        }

        /// Execute an ordinary foreground command with terminal streams
        /// inherited. Unlike capture APIs, interactive output is not retained
        /// or rejected at the programmatic capture ceiling. This trusted-local
        /// convenience path has no host cancellation flag or deadline; hosted
        /// callers use [`Self::execute_interactive_request`].
        pub fn execute_interactive(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute(input)
        }

        /// Execute `input` in the foreground with inherited terminal streams.
        ///
        /// This trusted-local convenience path has no host cancellation flag
        /// or deadline; hosted callers use [`Self::execute_interactive_request`].
        pub fn execute(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, false)
        }

        /// Execute `input` while retaining bounded stdout and stderr.
        ///
        /// This trusted-local convenience path has no host cancellation flag
        /// or deadline; hosted callers use [`Self::execute_capture_request`].
        pub fn execute_capture(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, true)
        }

        /// Execute a capture request with explicit cancellation, deadline, and output bounds.
        pub fn execute_capture_request(
            &mut self,
            request: ProcessRequest,
        ) -> Result<CommandOutcome, ShellError> {
            let context = RequestContext::new(&request)?;
            self.execute_inner_with_request(&request.command, true, Some(context))
        }

        /// Execute a captured foreground command and report its bounded output.
        ///
        /// Windows currently delivers the retained stream chunks immediately after the
        /// contained process graph exits; Unix backends additionally deliver them while the
        /// graph is running. Observer failure is returned after process cleanup is complete.
        pub fn execute_capture_request_streaming(
            &mut self,
            request: ProcessRequest,
            observer: &mut OutputObserver<'_>,
        ) -> Result<CommandOutcome, ShellError> {
            let outcome = self.execute_capture_request(request)?;
            if let Some(stdout) = &outcome.stdout {
                observer(ObservedActivity::Output {
                    stream: OutputStream::Stdout,
                    bytes: stdout.as_bytes(),
                })?;
            }
            if let Some(stderr) = &outcome.stderr {
                observer(ObservedActivity::Output {
                    stream: OutputStream::Stderr,
                    bytes: stderr.as_bytes(),
                })?;
            }
            Ok(outcome)
        }

        /// Execute a foreground command with inherited streams under the
        /// caller's cancellation and deadline. No stdout or stderr is retained.
        pub fn execute_interactive_request(
            &mut self,
            request: ProcessRequest,
        ) -> Result<CommandOutcome, ShellError> {
            let context = RequestContext::new(&request)?;
            self.execute_inner_with_request(&request.command, false, Some(context))
        }

        /// Refresh and return snapshots for every job owned by this executor.
        pub fn jobs(&mut self) -> Vec<JobState> {
            for job in &mut self.jobs {
                if job.state.status == JobStatus::Running {
                    refresh_children(&mut job.children, &mut job.exit_statuses);
                    if job.exit_statuses.iter().all(Option::is_some) {
                        job.state.status = JobStatus::Done;
                        job.state.exit_status = job.exit_statuses.last().copied().flatten();
                        finish_writers_silently(&mut job.writers);
                    }
                }
            }
            self.jobs.iter().map(|job| job.state.clone()).collect()
        }

        fn reserve_job_id(&mut self) -> Result<u32, ShellError> {
            let _ = self.jobs();
            self.reserve_refreshed_job_id()
        }

        fn reserve_refreshed_job_id(&mut self) -> Result<u32, ShellError> {
            if self.jobs.len() >= RETAINED_JOBS_MAX {
                self.jobs.retain(|job| job.state.status != JobStatus::Done);
            }
            if self.jobs.len() >= RETAINED_JOBS_MAX {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "native job table reached its retention limit",
                )
                .with_context(format!(
                    "limit {RETAINED_JOBS_MAX} jobs; observed {} live jobs",
                    self.jobs.len()
                ))
                .with_help(
                    "Finish or cancel an active job before starting another background job",
                ));
            }
            let visible_ids = self.jobs.iter().map(|job| job.state.id).collect::<Vec<_>>();
            Ok(allocate_job_id(&mut self.next_job_id, &visible_ids))
        }

        /// Terminate job `id`, reap its children, and return the final snapshot.
        pub fn cancel_job(&mut self, id: u32) -> Result<JobState, ShellError> {
            let job = self
                .jobs
                .iter_mut()
                .find(|job| job.state.id == id)
                .ok_or_else(|| missing_job_error(id))?;
            if job.state.status != JobStatus::Done {
                job.object.terminate(130)?;
                wait_children(&mut job.children, &mut job.exit_statuses);
                finish_writers_silently(&mut job.writers);
                job.state.status = JobStatus::Done;
                job.state.exit_status = Some(130);
            }
            Ok(job.state.clone())
        }

        /// Report that job suspension is unavailable on Windows.
        pub fn suspend_job(&mut self, id: u32) -> Result<JobState, ShellError> {
            if !self.jobs.iter().any(|job| job.state.id == id) {
                return Err(missing_job_error(id));
            }
            Err(ShellError::new(
                ErrorCode::InvalidArgument,
                "the native Windows backend does not support job suspension",
            )
            .with_help("Use `fg %<id>` to wait for the job or cancel it explicitly"))
        }

        fn execute_inner(
            &mut self,
            input: &str,
            capture: bool,
        ) -> Result<CommandOutcome, ShellError> {
            self.execute_inner_with_request(input, capture, None)
        }

        fn execute_inner_with_request(
            &mut self,
            input: &str,
            capture: bool,
            request: Option<RequestContext<'_>>,
        ) -> Result<CommandOutcome, ShellError> {
            self.environment.ensure_valid()?;
            if let Some(request) = request {
                request.ensure_active()?;
            }
            validate_native_source(input)?;
            let graph = parse_command_list(input).map_err(|error| {
                ShellError::new(ErrorCode::InvalidCommand, error.message)
                    .with_label(
                        Some("command".to_owned()),
                        error.start,
                        error.end,
                        "syntax error",
                    )
                    .with_help(error.help)
                    .with_command(input)
            })?;
            validate_native_plan(&graph)?;
            if let Some(request) = request {
                request.ensure_active()?;
            }
            let mut last = CommandOutcome {
                status: 0,
                stdout: None,
                stderr: None,
            };
            let mut captured_stdout = String::new();
            let mut captured_stderr = String::new();
            for (index, pipeline) in graph.pipelines.iter().enumerate() {
                if index > 0 {
                    let connector = graph.connectors[index - 1];
                    if (connector == ListConnector::And && last.status != 0)
                        || (connector == ListConnector::Or && last.status == 0)
                    {
                        continue;
                    }
                }
                last = self.execute_pipeline(pipeline, input, capture, request)?;
                if capture {
                    append_captured_output(
                        &mut captured_stdout,
                        last.stdout.as_deref().unwrap_or_default(),
                        retained_output_limit(request.map(|request| request.request)),
                    )?;
                    append_captured_output(
                        &mut captured_stderr,
                        last.stderr.as_deref().unwrap_or_default(),
                        retained_output_limit(request.map(|request| request.request)),
                    )?;
                }
            }
            if capture {
                last.stdout = Some(captured_stdout);
                last.stderr = Some(captured_stderr);
            }
            Ok(last)
        }

        fn execute_pipeline(
            &mut self,
            pipeline: &Pipeline,
            source: &str,
            capture: bool,
            request: Option<RequestContext<'_>>,
        ) -> Result<CommandOutcome, ShellError> {
            if let Some(request) = request {
                request.ensure_active()?;
            }
            if self.noninteractive_host && pipeline.background {
                return Err(super::noninteractive_process_error(
                    source,
                    "background execution is unavailable to isolated Lua",
                ));
            }
            if pipeline.commands.len() == 1 {
                if self.noninteractive_host
                    && pipeline.commands[0].words.first().is_some_and(|name| {
                        matches!(name.as_str(), "cd" | "export" | "jobs" | "fg" | "bg")
                    })
                {
                    return Err(super::noninteractive_process_error(
                        source,
                        "stateful and job-control built-ins are unavailable to isolated Lua",
                    ));
                }
                if pipeline.background
                    && pipeline.commands[0].words.first().is_some_and(|name| {
                        matches!(name.as_str(), "cd" | "export" | "jobs" | "fg" | "bg")
                    })
                {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        "stateful built-ins cannot run in the background",
                    )
                    .with_command(source)
                    .with_help("Run the built-in without `&`"));
                }
                validate_builtin_redirects(&pipeline.commands[0], source)?;
                if let Some(outcome) = self.execute_builtin(&pipeline.commands[0], capture)? {
                    return apply_builtin_redirects(
                        &pipeline.commands[0],
                        outcome,
                        capture,
                        source,
                    );
                }
            }
            self.spawn_pipeline(pipeline, source, capture, request)
        }

        fn execute_builtin(
            &mut self,
            command: &SimpleCommand,
            capture: bool,
        ) -> Result<Option<CommandOutcome>, ShellError> {
            let Some(name) = command.words.first().map(String::as_str) else {
                return Ok(Some(CommandOutcome {
                    status: 0,
                    stdout: None,
                    stderr: None,
                }));
            };
            match name {
                "cd" => Ok(Some(builtin::execute_cd(&command.words)?)),
                "export" => {
                    let mut assignments = Vec::with_capacity(command.words.len().saturating_sub(1));
                    for assignment in command.words.iter().skip(1) {
                        let Some((name, value)) = assignment.split_once('=') else {
                            return Err(ShellError::new(
                                ErrorCode::InvalidArgument,
                                format!("invalid export assignment `{assignment}`"),
                            )
                            .with_help("Use `export NAME=value`"));
                        };
                        assignments.push((name.to_owned(), value.to_owned()));
                    }
                    self.environment.set_variables(&assignments)?;
                    Ok(Some(CommandOutcome {
                        status: 0,
                        stdout: None,
                        stderr: None,
                    }))
                }
                "jobs" => {
                    let text = self
                        .jobs()
                        .into_iter()
                        .map(|job| format!("[{}] {:?} {}", job.id, job.status, job.command))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !capture && !text.is_empty() {
                        println!("{text}");
                    }
                    Ok(Some(CommandOutcome {
                        status: 0,
                        stdout: capture.then_some(text),
                        stderr: None,
                    }))
                }
                "fg" => {
                    let id = parse_job_id(command.words.get(1))?;
                    let index = self
                        .jobs
                        .iter()
                        .position(|job| job.state.id == id)
                        .ok_or_else(|| {
                            ShellError::new(
                                ErrorCode::InvalidArgument,
                                format!("job %{id} does not exist"),
                            )
                            .with_help("Run `jobs` to list known jobs")
                        })?;
                    let mut job = self.jobs.remove(index);
                    wait_children(&mut job.children, &mut job.exit_statuses);
                    join_writers(std::mem::take(&mut job.writers))?;
                    let status = job.exit_statuses.last().copied().flatten().unwrap_or(1);
                    Ok(Some(CommandOutcome {
                        status,
                        stdout: None,
                        stderr: None,
                    }))
                }
                "bg" => Err(ShellError::new(
                    ErrorCode::InvalidArgument,
                    "Windows jobs cannot be resumed because this backend does not suspend them",
                )
                .with_help("Start the command with `&` to run it in the background")),
                _ => Ok(None),
            }
        }

        fn spawn_pipeline(
            &mut self,
            pipeline: &Pipeline,
            source: &str,
            capture: bool,
            request: Option<RequestContext<'_>>,
        ) -> Result<CommandOutcome, ShellError> {
            let background_job_id = if pipeline.background {
                Some(self.reserve_job_id()?)
            } else {
                None
            };
            let object = JobObject::new()?;
            let mut children = Vec::with_capacity(pipeline.commands.len());
            let mut exit_statuses = Vec::with_capacity(pipeline.commands.len());
            let mut previous_stdout: Option<ChildStdout> = None;
            let mut stdout_reader = None;
            let mut stderr_readers = Vec::new();
            let mut writers = Vec::new();
            let output_limit = retained_output_limit(request.map(|request| request.request));
            let stderr_budget = Arc::new(CaptureBudget::new(output_limit));

            for (index, command) in pipeline.commands.iter().enumerate() {
                let Some(program) = command.words.first() else {
                    continue;
                };
                let last = index + 1 == pipeline.commands.len();
                let input = prepare_windows_input(command, source)?;
                let output = command.redirects.iter().rev().find(|redirect| {
                    matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append)
                });
                let mut process = Command::new(program);
                self.configure_child(&mut process)?;
                process.args(command.words.iter().skip(1));
                let here_string = match input {
                    Some(WindowsInput::File(file)) => {
                        drop(previous_stdout.take());
                        process.stdin(Stdio::from(file));
                        None
                    }
                    Some(WindowsInput::HereString(bytes)) => {
                        drop(previous_stdout.take());
                        process.stdin(Stdio::piped());
                        Some(bytes)
                    }
                    None => {
                        if let Some(stdout) = previous_stdout.take() {
                            process.stdin(Stdio::from(stdout));
                        } else if index > 0 {
                            process.stdin(Stdio::null());
                        } else if self.noninteractive_host {
                            process.stdin(Stdio::null());
                        } else {
                            process.stdin(Stdio::inherit());
                        }
                        None
                    }
                };
                if let Some(redirect) = output {
                    process.stdout(Stdio::from(open_output(
                        &redirect.path,
                        redirect.kind == RedirectKind::Append,
                        source,
                    )?));
                } else if !last || (capture && !pipeline.background) {
                    process.stdout(Stdio::piped());
                } else {
                    process.stdout(Stdio::inherit());
                }
                if capture && !pipeline.background {
                    process.stderr(Stdio::piped());
                } else {
                    process.stderr(Stdio::inherit());
                }
                let mut child = process
                    .spawn()
                    .map_err(|error| spawn_error(program, source, error))?;
                object.assign(&mut child).map_err(|error| {
                    let _ = child.kill();
                    let _ = child.wait();
                    error.with_command(source)
                })?;
                if let Some(bytes) = here_string {
                    let mut stdin = child.stdin.take().ok_or_else(|| {
                        ShellError::new(ErrorCode::Io, "here-string stdin pipe is unavailable")
                            .with_command(source)
                            .with_help("Retry the command or use an input file")
                    })?;
                    writers.push(thread::spawn(move || stdin.write_all(&bytes)));
                }
                if capture && !pipeline.background {
                    if let Some(stderr) = child.stderr.take() {
                        stderr_readers
                            .push(spawn_reader_with_budget(stderr, Arc::clone(&stderr_budget)));
                    }
                }
                if output.is_none() && !last {
                    previous_stdout = child.stdout.take();
                } else if output.is_none() && last && capture && !pipeline.background {
                    stdout_reader = child
                        .stdout
                        .take()
                        .map(|stdout| spawn_reader(stdout, output_limit));
                }
                children.push(child);
                exit_statuses.push(None);
            }

            if pipeline.background {
                let Some(id) = background_job_id else {
                    unreachable!("background pipeline reserved no job id");
                };
                self.jobs.push(Job {
                    state: JobState {
                        id,
                        command: source.to_owned(),
                        status: JobStatus::Running,
                        process_group: None,
                        exit_status: None,
                    },
                    children,
                    exit_statuses,
                    object,
                    writers,
                });
                return Ok(CommandOutcome {
                    status: 0,
                    stdout: capture.then(|| format!("[{id}]\n")),
                    stderr: None,
                });
            }
            if let Some(request) = request {
                if let Err(error) =
                    wait_children_with_request(&object, &mut children, &mut exit_statuses, request)
                {
                    let _ = join_reader(stdout_reader, "pipeline stdout");
                    for reader in stderr_readers {
                        let _ = join_reader(Some(reader), "pipeline stderr");
                    }
                    return Err(error);
                }
            } else {
                wait_children(&mut children, &mut exit_statuses);
            }
            let _ = object.terminate(0);
            join_writers(writers)?;
            let status = exit_statuses.last().copied().flatten().unwrap_or(0);
            let stdout = if capture {
                Some(join_reader(stdout_reader, "pipeline stdout"))
            } else {
                None
            };
            let stderr = if capture {
                let mut bytes = Vec::new();
                let mut failure = None;
                for reader in stderr_readers {
                    match join_reader(Some(reader), "pipeline stderr") {
                        Ok(output) => bytes.extend(output),
                        Err(error) if failure.is_none() => failure = Some(error),
                        Err(_) => {}
                    }
                }
                failure.map_or(Ok(String::from_utf8_lossy(&bytes).into_owned()), Err)
            } else {
                Ok(String::new())
            };
            let stdout = stdout.transpose()?;
            let stderr = stderr?;
            Ok(CommandOutcome {
                status,
                stdout: stdout.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
                stderr: capture.then_some(stderr),
            })
        }
    }

    struct JobObject(HANDLE);

    impl JobObject {
        fn new() -> Result<Self, ShellError> {
            // SAFETY: both pointers are null as permitted by CreateJobObjectW, and the returned
            // owned HANDLE is closed exactly once by Drop.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(windows_job_error("create", io::Error::last_os_error()));
            }
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `limits` has the exact structure and byte length required by the selected
            // information class, and remains alive for the duration of the call.
            let configured = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&limits).cast(),
                    u32::try_from(std::mem::size_of_val(&limits)).unwrap_or(u32::MAX),
                )
            };
            if configured == 0 {
                let error = io::Error::last_os_error();
                // SAFETY: `handle` is a live owned handle and will not be wrapped after this path.
                unsafe { CloseHandle(handle) };
                return Err(windows_job_error("configure", error));
            }
            Ok(Self(handle))
        }

        fn assign(&self, child: &mut Child) -> Result<(), ShellError> {
            let process = child.as_raw_handle() as HANDLE;
            // SAFETY: both handles are live for the call. The Child retains ownership of its
            // process handle; AssignProcessToJobObject does not consume either handle.
            if unsafe { AssignProcessToJobObject(self.0, process) } == 0 {
                return Err(windows_job_error(
                    "contain process tree",
                    io::Error::last_os_error(),
                ));
            }
            Ok(())
        }

        fn terminate(&self, status: u32) -> Result<(), ShellError> {
            // SAFETY: the job handle is live and owned by self for the duration of the call.
            if unsafe { TerminateJobObject(self.0, status) } == 0 {
                return Err(windows_job_error("terminate", io::Error::last_os_error()));
            }
            Ok(())
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            // SAFETY: JobObject uniquely owns this non-null handle and Drop runs exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    enum WindowsInput {
        File(File),
        HereString(Vec<u8>),
    }

    fn prepare_windows_input(
        command: &SimpleCommand,
        source: &str,
    ) -> Result<Option<WindowsInput>, ShellError> {
        let mut selected = None;
        for redirect in &command.redirects {
            match redirect.kind {
                RedirectKind::Input => {
                    selected = Some(WindowsInput::File(open_input(&redirect.path, source)?));
                }
                RedirectKind::HereString => {
                    let observed_bytes = redirect.path.len().saturating_add(1);
                    if observed_bytes > HERE_STRING_BYTES_MAX {
                        return Err(ShellError::new(
                            ErrorCode::ResourceLimit,
                            "here-string input exceeds its byte limit",
                        )
                        .with_context(format!(
                            "limit {HERE_STRING_BYTES_MAX} bytes; observed {observed_bytes} bytes including the trailing newline"
                        ))
                        .with_help("Use an input file or pipeline for larger input"));
                    }
                    let mut bytes = redirect.path.as_bytes().to_vec();
                    bytes.push(b'\n');
                    selected = Some(WindowsInput::HereString(bytes));
                }
                RedirectKind::Output
                | RedirectKind::Append
                | RedirectKind::DuplicateInput
                | RedirectKind::DuplicateOutput => {}
            }
        }
        Ok(selected)
    }

    fn apply_builtin_redirects(
        command: &SimpleCommand,
        mut outcome: CommandOutcome,
        capture: bool,
        source: &str,
    ) -> Result<CommandOutcome, ShellError> {
        if command
            .redirects
            .iter()
            .any(|redirect| redirect.kind == RedirectKind::Input)
        {
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                "input redirection is not supported for stateful built-ins",
            )
            .with_command(source)
            .with_help("Redirect input to an external command instead"));
        }
        if let Some(redirect) =
            command.redirects.iter().rev().find(|redirect| {
                matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append)
            })
        {
            let mut output = open_output(
                &redirect.path,
                redirect.kind == RedirectKind::Append,
                source,
            )?;
            if let Some(stdout) = outcome.stdout.as_deref() {
                output
                    .write_all(stdout.as_bytes())
                    .map_err(|error| redirect_error("write", &redirect.path, source, error))?;
            }
            outcome.stdout = capture.then(String::new);
        } else if !capture {
            if let Some(stdout) = outcome.stdout.as_deref() {
                io::stdout().write_all(stdout.as_bytes()).map_err(|error| {
                    ShellError::new(ErrorCode::Io, "could not write built-in output")
                        .with_context(error.to_string())
                        .with_help("Check the terminal output stream")
                })?;
            }
            outcome.stdout = None;
        }
        if !capture {
            if let Some(stderr) = outcome.stderr.as_deref() {
                io::stderr().write_all(stderr.as_bytes()).map_err(|error| {
                    ShellError::new(ErrorCode::Io, "could not write built-in error output")
                        .with_context(error.to_string())
                        .with_help("Check the terminal error stream")
                })?;
            }
            outcome.stderr = None;
        }
        Ok(outcome)
    }

    fn validate_builtin_redirects(command: &SimpleCommand, source: &str) -> Result<(), ShellError> {
        for redirect in &command.redirects {
            match redirect.kind {
                RedirectKind::Input | RedirectKind::HereString | RedirectKind::DuplicateInput => {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        "input redirection is not supported for stateful built-ins",
                    )
                    .with_command(source)
                    .with_help("Redirect input to an external command instead"));
                }
                RedirectKind::Output | RedirectKind::Append => {
                    drop(open_output(
                        &redirect.path,
                        redirect.kind == RedirectKind::Append,
                        source,
                    )?);
                }
                RedirectKind::DuplicateOutput => {}
            }
        }
        Ok(())
    }

    fn open_input(path: &str, source: &str) -> Result<File, ShellError> {
        File::open(path).map_err(|error| redirect_error("open", path, source, error))
    }

    fn open_output(path: &str, append: bool, source: &str) -> Result<File, ShellError> {
        OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(path)
            .map_err(|error| redirect_error("open", path, source, error))
    }

    fn redirect_error(action: &str, path: &str, source: &str, error: io::Error) -> ShellError {
        ShellError::new(
            ErrorCode::Io,
            format!("could not {action} redirect target `{path}`"),
        )
        .with_command(source)
        .with_context(error.to_string())
        .with_help("Check the redirect path and file permissions")
    }

    fn spawn_reader(reader: impl Read + Send + 'static, limit: usize) -> ReaderTask {
        spawn_reader_with_budget(reader, Arc::new(CaptureBudget::new(limit)))
    }

    fn spawn_reader_with_budget(
        mut reader: impl Read + Send + 'static,
        budget: Arc<CaptureBudget>,
    ) -> ReaderTask {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 8 * 1024];
            let mut discarded_bytes = 0_u64;
            loop {
                let count = reader.read(&mut chunk)?;
                if count == 0 {
                    break;
                }
                let retained = budget.claim(count);
                bytes.extend_from_slice(&chunk[..retained]);
                discarded_bytes = discarded_bytes.saturating_add((count - retained) as u64);
            }
            Ok(ReaderCapture {
                bytes,
                discarded_bytes,
            })
        })
    }

    fn join_reader(reader: Option<ReaderTask>, description: &str) -> Result<Vec<u8>, ShellError> {
        let Some(reader) = reader else {
            return Ok(Vec::new());
        };
        match reader.join() {
            Ok(Ok(capture)) if capture.discarded_bytes > 0 => Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!("{description} exceeded the retained output limit"),
            )
            .with_context(format!(
                "retained {} bytes; discarded {} bytes",
                capture.bytes.len(),
                capture.discarded_bytes
            ))
            .with_help(
                "Reduce process output, redirect it to a file, or consume it in a pipeline",
            )),
            Ok(Ok(capture)) => Ok(capture.bytes),
            Ok(Err(error)) => Err(ShellError::new(
                ErrorCode::Io,
                format!("could not read {description}"),
            )
            .with_context(error.to_string())
            .with_help("Retry the command; report repeated pipeline capture failures")),
            Err(_) => Err(
                ShellError::new(ErrorCode::Io, format!("{description} reader failed"))
                    .with_help("Retry the command; report repeated pipeline capture failures"),
            ),
        }
    }

    fn join_writers(writers: Vec<WriterTask>) -> Result<(), ShellError> {
        for writer in writers {
            let result = writer.join().map_err(|_| {
                ShellError::new(ErrorCode::Io, "pipeline writer task failed")
                    .with_help("Retry the command; report repeated pipeline failures")
            })?;
            if let Err(error) = result {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    continue;
                }
                return Err(
                    ShellError::new(ErrorCode::Io, "could not write pipeline input")
                        .with_context(error.to_string())
                        .with_help("Retry the command; report repeated pipeline failures"),
                );
            }
        }
        Ok(())
    }

    fn finish_writers_silently(writers: &mut Vec<WriterTask>) {
        let _ = join_writers(std::mem::take(writers));
    }

    fn refresh_children(children: &mut [Child], exit_statuses: &mut [Option<i32>]) {
        for (child, exit_status) in children.iter_mut().zip(exit_statuses) {
            if exit_status.is_none() {
                if let Ok(Some(status)) = child.try_wait() {
                    *exit_status = Some(status.code().unwrap_or(1));
                }
            }
        }
    }

    fn wait_children(children: &mut [Child], exit_statuses: &mut [Option<i32>]) {
        for (child, exit_status) in children.iter_mut().zip(exit_statuses) {
            if exit_status.is_none() {
                *exit_status = Some(
                    child
                        .wait()
                        .ok()
                        .and_then(|status| status.code())
                        .unwrap_or(1),
                );
            }
        }
    }

    fn wait_children_with_request(
        object: &JobObject,
        children: &mut [Child],
        exit_statuses: &mut [Option<i32>],
        request: RequestContext<'_>,
    ) -> Result<(), ShellError> {
        loop {
            refresh_children(children, exit_statuses);
            if exit_statuses.iter().all(Option::is_some) {
                return Ok(());
            }
            if let Err(error) = request.ensure_active() {
                object.terminate(130)?;
                wait_children(children, exit_statuses);
                return Err(error);
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn append_captured_output(
        retained: &mut String,
        next: &str,
        limit: usize,
    ) -> Result<(), ShellError> {
        if next.len() > limit.saturating_sub(retained.len()) {
            let available = limit.saturating_sub(retained.len());
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "captured process output exceeded the retained output limit",
            )
            .with_context(format!(
                "retained {} bytes; discarded at least {} bytes; limit {limit} bytes",
                retained.len(),
                next.len().saturating_sub(available)
            ))
            .with_help(
                "Reduce process output, redirect it to a file, or consume it in a pipeline",
            ));
        }
        retained.push_str(next);
        Ok(())
    }

    fn retained_output_limit(request: Option<&ProcessRequest>) -> usize {
        request.map_or(DEFAULT_CAPTURE_BYTES, |request| {
            request.max_output_bytes.min(DEFAULT_CAPTURE_BYTES)
        })
    }

    fn missing_job_error(id: u32) -> ShellError {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("job %{id} does not exist"),
        )
        .with_help("Run `jobs` to list known jobs")
    }

    fn windows_job_error(action: &str, error: io::Error) -> ShellError {
        ShellError::new(
            ErrorCode::ProcessSpawn,
            format!("could not {action} Windows job object"),
        )
        .with_context(error.to_string())
        .with_help("Run outside a restrictive parent job or grant process lifecycle access")
    }

    fn parse_job_id(word: Option<&String>) -> Result<u32, ShellError> {
        word.and_then(|word| word.strip_prefix('%'))
            .and_then(|id| id.parse().ok())
            .ok_or_else(|| {
                ShellError::new(ErrorCode::InvalidArgument, "fg needs a job id like %1")
                    .with_help("Run `jobs`, then use `fg %<id>`")
            })
    }

    fn spawn_error(program: &str, source: &str, error: std::io::Error) -> ShellError {
        ShellError::new(
            ErrorCode::ProcessSpawn,
            format!("could not start `{program}`"),
        )
        .with_command(source)
        .with_context(error.to_string())
        .with_help("Check that the executable exists and is available on PATH")
    }
}

pub use platform::{ChildProcessTree, JobState, JobStatus, NativeExecutor};

/// RAII ownership for one directly spawned process tree.
///
/// Construction establishes containment before returning. Every drop path
/// terminates the contained tree and reaps the direct child, including callers
/// that fail while taking pipes or starting protocol readers.
pub struct ContainedChild {
    containment: ChildProcessTree,
    child: std::process::Child,
    reaped: bool,
}

impl ContainedChild {
    /// Spawn `command` without allowing a partially initialized child to escape.
    pub fn spawn(command: &mut std::process::Command) -> Result<Self, quirl_core::ShellError> {
        let containment = ChildProcessTree::new()?;
        containment.configure(command);
        let mut child = command.spawn().map_err(|error| {
            quirl_core::ShellError::new(
                quirl_core::ErrorCode::ProcessSpawn,
                "could not start contained child process",
            )
            .with_context(error.to_string())
            .with_help("Check the executable and retry after reducing process pressure")
        })?;
        if let Err(error) = containment.assign(&mut child) {
            let _ = containment.terminate(&mut child);
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(Self {
            containment,
            child,
            reaped: false,
        })
    }

    /// Borrow the direct child to take its configured pipes.
    pub fn child_mut(&mut self) -> &mut std::process::Child {
        &mut self.child
    }

    /// Observe direct-child exit without relinquishing cleanup ownership.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, quirl_core::ShellError> {
        self.child.try_wait().map_err(|error| {
            quirl_core::ShellError::new(
                quirl_core::ErrorCode::Io,
                "could not observe contained child status",
            )
            .with_context(error.to_string())
            .with_help("Retry; report repeated process observation failures")
        })
    }

    /// Terminate the complete tree and reap the direct child exactly once.
    pub fn terminate_and_reap(
        &mut self,
    ) -> Result<std::process::ExitStatus, quirl_core::ShellError> {
        let terminate = self.containment.terminate(&mut self.child);
        let waited = self.child.wait().map_err(|error| {
            quirl_core::ShellError::new(
                quirl_core::ErrorCode::Io,
                "could not reap contained child process",
            )
            .with_context(error.to_string())
            .with_help("Report the unreaped process and restart Quirl")
        });
        if waited.is_ok() {
            self.reaped = true;
        }
        match (terminate, waited) {
            (Ok(()), Ok(status)) => Ok(status),
            (Err(error), Ok(_)) | (Err(error), Err(_)) => Err(error),
            (Ok(()), Err(error)) => Err(error),
        }
    }
}

impl Drop for ContainedChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.containment.terminate(&mut self.child);
            let _ = self.child.wait();
            self.reaped = true;
        }
    }
}

#[cfg(any(unix, test))]
fn summarize_job_lifecycle(
    children: impl IntoIterator<Item = (JobStatus, Option<i32>)>,
) -> (JobStatus, Option<i32>) {
    let mut live_count = 0_usize;
    let mut stopped_count = 0_usize;
    let mut last_exit_status = None;
    for (status, exit_status) in children {
        last_exit_status = exit_status;
        if status != JobStatus::Done {
            live_count += 1;
            if status == JobStatus::Stopped {
                stopped_count += 1;
            }
        }
    }
    if live_count == 0 {
        (JobStatus::Done, last_exit_status)
    } else if stopped_count == live_count {
        (JobStatus::Stopped, None)
    } else {
        (JobStatus::Running, None)
    }
}

/// Process host adapter for sandboxed callers.  A fresh executor keeps Lua
/// process work isolated from the interactive job table while still using the
/// platform backend's process-tree containment.
pub fn sandboxed_process_host() -> quirl_core::ProcessHost {
    std::sync::Arc::new(|request| {
        let mut executor = NativeExecutor::default();
        executor.execute_capture_request(request)
    })
}

/// Noninteractive process host for requests proxied from an isolated runtime.
///
/// Standard input is closed, terminal ownership is never transferred, and the
/// ordinary platform process-tree container remains responsible for cleanup.
pub fn isolated_process_host() -> quirl_core::ProcessHost {
    std::sync::Arc::new(|request| {
        let mut executor = NativeExecutor::noninteractive_host();
        executor.execute_capture_request(request)
    })
}

/// Portable event applied to a native job's aggregate lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobLifecycleEvent {
    /// Stop a running job.
    Stop,
    /// Resume a stopped job.
    Continue,
    /// Record process completion with the supplied shell status.
    Exit(i32),
}

/// Validate a portable job-state transition before a backend mutates its native handle.
pub fn transition_job_state(
    current: JobStatus,
    event: JobLifecycleEvent,
) -> Result<(JobStatus, Option<i32>), quirl_core::ShellError> {
    match (current, event) {
        (JobStatus::Running, JobLifecycleEvent::Stop) => Ok((JobStatus::Stopped, None)),
        (JobStatus::Stopped, JobLifecycleEvent::Continue) => Ok((JobStatus::Running, None)),
        (JobStatus::Running | JobStatus::Stopped, JobLifecycleEvent::Exit(status)) => {
            Ok((JobStatus::Done, Some(status)))
        }
        (_, event) => {
            fn render_status(status: JobStatus) -> &'static str {
                match status {
                    JobStatus::Running => "running",
                    JobStatus::Stopped => "stopped",
                    JobStatus::Done => "done",
                }
            }
            fn render_event(event: JobLifecycleEvent) -> String {
                match event {
                    JobLifecycleEvent::Stop => "stop".to_owned(),
                    JobLifecycleEvent::Continue => "continue".to_owned(),
                    JobLifecycleEvent::Exit(status) => format!("exit({status})"),
                }
            }
            Err(quirl_core::ShellError::new(
                quirl_core::ErrorCode::InvalidArgument,
                format!(
                    "invalid job lifecycle transition from {} through {}",
                    render_status(current),
                    render_event(event)
                ),
            )
            .with_help("Refresh the job list before requesting another lifecycle transition")
            .with_help(
                "Report this internal job-table inconsistency, including the job's recent history",
            ))
        }
    }
}

/// Stable process backend contract used by the CLI independently of the host platform.
pub trait ProcessBackend {
    /// Execute a trusted foreground command with inherited terminal streams.
    ///
    /// This convenience method has no host cancellation or deadline input;
    /// interactive callers rely on terminal signals. Use
    /// [`NativeExecutor::execute_interactive_request`] whenever a host
    /// cancellation flag or deadline must remain observable.
    fn execute(
        &mut self,
        input: &str,
    ) -> Result<quirl_core::CommandOutcome, quirl_core::ShellError>;
    /// Execute a trusted command and retain output within [`DEFAULT_CAPTURE_BYTES`].
    ///
    /// This convenience method has no host cancellation or deadline input. Use
    /// [`NativeExecutor::execute_capture_request`] for untrusted or hosted work.
    fn execute_capture(
        &mut self,
        input: &str,
    ) -> Result<quirl_core::CommandOutcome, quirl_core::ShellError>;
    /// Refresh and return all jobs owned by the backend.
    fn jobs(&mut self) -> Vec<JobState>;
    /// Cancel job `id` and return its final state.
    fn cancel_job(&mut self, id: u32) -> Result<JobState, quirl_core::ShellError>;
    /// Suspend job `id` when supported by the host platform.
    fn suspend_job(&mut self, id: u32) -> Result<JobState, quirl_core::ShellError>;
}

impl ProcessBackend for NativeExecutor {
    fn execute(
        &mut self,
        input: &str,
    ) -> Result<quirl_core::CommandOutcome, quirl_core::ShellError> {
        NativeExecutor::execute(self, input)
    }

    fn execute_capture(
        &mut self,
        input: &str,
    ) -> Result<quirl_core::CommandOutcome, quirl_core::ShellError> {
        NativeExecutor::execute_capture(self, input)
    }

    fn jobs(&mut self) -> Vec<JobState> {
        NativeExecutor::jobs(self)
    }

    fn cancel_job(&mut self, id: u32) -> Result<JobState, quirl_core::ShellError> {
        NativeExecutor::cancel_job(self, id)
    }

    fn suspend_job(&mut self, id: u32) -> Result<JobState, quirl_core::ShellError> {
        NativeExecutor::suspend_job(self, id)
    }
}

#[cfg(test)]
mod backend_contract_tests {
    use super::*;
    use crate::simulation_support::{DeterministicRng, configuration};
    use quirl_core::ErrorCode;
    use std::{
        fs,
        path::PathBuf,
        sync::{Arc, atomic::AtomicBool},
        thread,
        time::{Duration, Instant},
    };

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("quirl-backend-{name}-{}", std::process::id()))
    }

    #[test]
    fn portable_job_lifecycle_accepts_only_valid_state_transitions() {
        assert_eq!(
            transition_job_state(JobStatus::Running, JobLifecycleEvent::Stop).unwrap(),
            (JobStatus::Stopped, None)
        );
        assert_eq!(
            transition_job_state(JobStatus::Stopped, JobLifecycleEvent::Continue).unwrap(),
            (JobStatus::Running, None)
        );
        assert_eq!(
            transition_job_state(JobStatus::Running, JobLifecycleEvent::Exit(7)).unwrap(),
            (JobStatus::Done, Some(7))
        );
        std::assert_matches!(
            transition_job_state(JobStatus::Done, JobLifecycleEvent::Continue),
            Err(error) if error.code == ErrorCode::InvalidArgument
                && error.message.contains("done")
                && !error.details.help.is_empty()
        );
        std::assert_matches!(
            transition_job_state(JobStatus::Stopped, JobLifecycleEvent::Stop),
            Err(error) if error.code == ErrorCode::InvalidArgument
                && error.message.contains("stopped")
                && !error.details.help.is_empty()
        );
    }

    #[test]
    fn runner_v1_evidence_is_frozen_and_v2_fails_closed_across_versions() {
        assert_eq!(
            quirl_core::schema_fingerprint(RUNNER_SCHEMA_DESCRIPTOR_V1),
            "fnv1a64:131ea5b3e770b424"
        );
        assert!(RUNNER_SCHEMA_DESCRIPTOR.contains("quirl.command-grammar@2"));
        assert!(RUNNER_SCHEMA_DESCRIPTOR.contains("execute(source)"));
        assert!(!RUNNER_SCHEMA_DESCRIPTOR.contains("foreground_job"));
        assert!(!RUNNER_SCHEMA_DESCRIPTOR.contains("execute_interactive"));
        assert!(validate_runner_protocol_version(RUNNER_PROTOCOL_VERSION).is_ok());
        for version in [RUNNER_PROTOCOL_VERSION_V1, RUNNER_PROTOCOL_VERSION + 1] {
            let error = validate_runner_protocol_version(version).unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation);
            assert!(!error.details.help.is_empty());
        }
    }

    #[test]
    fn runner_job_fixtures_round_trip_and_reject_malformed_or_unknown_fields() {
        for fixture in [RUNNER_JOB_STATE_FIXTURE_V1, RUNNER_JOB_STATE_FIXTURE] {
            let state: JobState = serde_json::from_str(fixture).unwrap();
            assert_eq!(state.id, 1);
            assert_eq!(state.status, JobStatus::Running);
        }
        for fixture in [
            r#"{"id":1,"command":"x","status":"unknown","process_group":null,"exit_status":null}"#,
            r#"{"id":1,"command":"x","status":"done","process_group":null,"exit_status":0,"extra":true}"#,
            r#"{"id":"bad"}"#,
        ] {
            assert!(serde_json::from_str::<JobState>(fixture).is_err());
        }
    }

    #[test]
    fn reserved_input_descriptor_duplication_fails_closed() {
        let error = NativeExecutor::default()
            .execute_capture("cat 0<&1")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert!(error.message.contains("descriptor duplication"));
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn interior_nul_is_rejected_before_spawn_with_actionable_help() {
        let error = NativeExecutor::default()
            .execute_capture("prin\0tf hi")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert!(error.message.contains("NUL"));
        assert!(error.details.help.iter().any(|help| help.contains("NUL")));
        assert!(!error.details.help.iter().any(|help| help.contains("PATH")));
    }

    #[test]
    fn public_request_execution_paths_observe_cancellation_before_spawn() {
        for capture in [false, true] {
            let cancelled = Arc::new(AtomicBool::new(true));
            let request = quirl_core::ProcessRequest {
                command: "this-command-must-not-run".to_owned(),
                deadline: Duration::from_secs(1),
                cancelled,
                max_output_bytes: 1024,
            };
            let mut backend = NativeExecutor::default();
            let error = if capture {
                backend.execute_capture_request(request).unwrap_err()
            } else {
                backend.execute_interactive_request(request).unwrap_err()
            };
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.message.contains("cancelled"));
        }
    }

    #[cfg(unix)]
    fn read_test_environment(executor: &mut NativeExecutor, name: &str) -> String {
        executor
            .execute_capture(&format!("sh -c 'printf %s \"${name}\"'"))
            .unwrap()
            .stdout
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn export_is_inherited_without_mutating_the_host_environment() {
        let name = "QUIRL_SESSION_EXPORT_ISOLATION";
        assert!(std::env::var_os(name).is_none());
        let mut executor = NativeExecutor::default();

        executor
            .execute_capture(&format!("export {name}=owned"))
            .unwrap();

        assert_eq!(read_test_environment(&mut executor, name), "owned");
        assert!(std::env::var_os(name).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_executors_keep_environment_updates_isolated() {
        let name = "QUIRL_CONCURRENT_SESSION_ENVIRONMENT";
        assert!(std::env::var_os(name).is_none());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers = ["first", "second"].map(|expected| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut executor = NativeExecutor::default();
                executor
                    .set_environment_variable(name.to_owned(), expected.to_owned())
                    .unwrap();
                barrier.wait();
                (expected, read_test_environment(&mut executor, name))
            })
        });

        for worker in workers {
            let (expected, observed) = worker.join().unwrap();
            assert_eq!(observed, expected);
        }
        assert!(std::env::var_os(name).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn explicit_process_request_uses_the_executor_environment_snapshot() {
        let name = "QUIRL_EXPLICIT_REQUEST_ENVIRONMENT";
        let mut executor = NativeExecutor::default();
        executor
            .set_environment_variable(name.to_owned(), "request-owned".to_owned())
            .unwrap();
        let request = quirl_core::ProcessRequest {
            command: format!("sh -c 'printf %s \"${name}\"'"),
            deadline: Duration::from_secs(1),
            cancelled: Arc::new(AtomicBool::new(false)),
            max_output_bytes: 1024,
        };

        let outcome = executor.execute_capture_request(request).unwrap();

        assert_eq!(outcome.stdout.as_deref(), Some("request-owned"));
        assert!(std::env::var_os(name).is_none());
    }

    #[test]
    fn environment_updates_validate_all_assignments_before_commit() {
        let mut environment = SessionEnvironment::default();
        let valid_name = "QUIRL_TRANSACTIONAL_ENVIRONMENT";
        assert!(std::env::var_os(valid_name).is_none());

        let error = environment
            .set_variables(&[
                (valid_name.to_owned(), "must-not-commit".to_owned()),
                ("INVALID-NAME".to_owned(), "value".to_owned()),
            ])
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(environment.value(valid_name), "");
    }

    #[test]
    fn initial_environment_capture_accepts_exact_limits_and_rejects_limit_plus_one() {
        let exact = SessionEnvironment::capture_with_limits(
            [
                (OsString::from("A"), OsString::from("1")),
                (OsString::from("B"), OsString::from("2")),
            ],
            2,
            4,
        );
        assert!(exact.ensure_valid().is_ok());

        let variable_error = SessionEnvironment::capture_with_limits(
            [
                (OsString::from("A"), OsString::new()),
                (OsString::from("B"), OsString::new()),
                (OsString::from("C"), OsString::new()),
            ],
            2,
            3,
        )
        .ensure_valid()
        .unwrap_err();
        assert_eq!(variable_error.code, ErrorCode::ResourceLimit);
        assert!(variable_error.details.context[0].contains("observed 3"));

        let byte_error = SessionEnvironment::capture_with_limits(
            [(OsString::from("A"), OsString::from("1234"))],
            1,
            4,
        )
        .ensure_valid()
        .unwrap_err();
        assert_eq!(byte_error.code, ErrorCode::ResourceLimit);
        assert!(byte_error.details.context[0].contains("observed 5"));
    }

    #[test]
    fn invalid_initial_environment_fails_before_executor_spawn() {
        let environment = SessionEnvironment::capture_with_limits(
            [(OsString::from("A"), OsString::from("12"))],
            1,
            2,
        );
        let mut executor = NativeExecutor::default();
        executor.replace_environment_for_test(environment);

        let error = executor.execute_capture("must-not-spawn").unwrap_err();

        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("environment"));
    }

    #[test]
    fn job_id_allocation_wraps_and_skips_every_visible_id() {
        let mut next = u32::MAX;
        assert_eq!(allocate_job_id(&mut next, &[1, 2]), u32::MAX);
        assert_eq!(next, 1);
        assert_eq!(allocate_job_id(&mut next, &[1, 2, u32::MAX]), 3);
        assert_eq!(next, 4);
    }

    #[test]
    fn platform_backend_contract_runs_byte_pipelines_and_file_redirects() {
        let output = temporary_path("pipeline-output");
        #[cfg(unix)]
        let command = format!("printf hello | cat > '{}'", output.display());
        #[cfg(windows)]
        let command = format!(
            "cmd.exe /D /C echo hello | findstr hello > '{}'",
            output.display()
        );
        let mut backend = NativeExecutor::default();
        let outcome = ProcessBackend::execute_capture(&mut backend, &command).unwrap();
        assert_eq!(outcome.status, 0);
        assert!(fs::read_to_string(&output).unwrap().contains("hello"));
        fs::remove_file(output).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn native_c1_expands_parameters_arithmetic_substitutions_and_here_strings() {
        let mut backend = NativeExecutor::default();
        backend
            .set_environment_variable("QUIRL_C1_WORD".to_owned(), "expanded".to_owned())
            .unwrap();
        let output = backend
            .execute_capture(
                "printf '%s|%s|%s|%s\\n' $QUIRL_C1_WORD $((1 + 2)) $((1 + ((2)))) $(printf nested); cat <<< value",
            )
            .unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(
            output.stdout.as_deref(),
            Some("expanded|3|3|nested\nvalue\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_c1_preserves_single_quotes_and_descriptor_redirects() {
        let output = temporary_path("c1-stderr");
        let mut backend = NativeExecutor::default();
        let command = format!(
            "printf '%s' '$HOME'; sh -c 'printf err >&2' 2> {}",
            output.display()
        );
        let result = backend.execute_capture(&command).unwrap();
        assert_eq!(result.stdout.as_deref(), Some("$HOME"));
        assert_eq!(fs::read_to_string(&output).unwrap(), "err");
        fs::remove_file(output).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stdout_file_redirect_and_stderr_duplication_share_one_output_handle() {
        let output = temporary_path("c1-merged-output");
        let command = format!(
            "sh -c 'printf stdout; printf stderr >&2' > {} 2>&1",
            output.display()
        );
        let result = NativeExecutor::default().execute_capture(&command).unwrap();
        assert_eq!(result.status, 0);
        assert_eq!(result.stdout.as_deref(), Some(""));
        assert_eq!(fs::read_to_string(&output).unwrap(), "stdoutstderr");
        fs::remove_file(output).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stderr_duplication_before_stdout_redirect_fails_closed() {
        let output = temporary_path("c1-ordered-redirect");
        let command = format!(
            "sh -c 'printf stdout; printf stderr >&2' 2>&1 > {}",
            output.display()
        );
        let error = NativeExecutor::default()
            .execute_capture(&command)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert!(error.message.contains("before a later stdout"));
        assert!(error.details.help[0].contains("> file 2>&1"));
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn native_c1_expands_the_actual_previous_status() {
        let output = NativeExecutor::default()
            .execute_capture("false; printf '%s' $?")
            .unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.as_deref(), Some("1"));
    }

    #[cfg(unix)]
    #[test]
    fn command_substitution_honors_outer_cancellation_and_deadline() {
        let mut backend = NativeExecutor::default();
        let request = quirl_core::ProcessRequest {
            command: "printf '%s' $(sleep 1)".to_owned(),
            deadline: Duration::from_millis(20),
            cancelled: Arc::new(AtomicBool::new(false)),
            max_output_bytes: 1024,
        };
        let started = Instant::now();
        let error = backend.execute_capture_request(request).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[cfg(unix)]
    #[test]
    fn command_substitution_depth_is_bounded_before_stack_exhaustion() {
        let mut source = "printf leaf".to_owned();
        for _ in 0..9 {
            source = format!("printf $({source})");
        }
        let error = NativeExecutor::default()
            .execute_capture(&source)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert!(error.message.contains("depth limit"));
        assert!(error.details.help[0].contains("Flatten nested"));
    }

    #[test]
    fn platform_backend_contract_lists_and_cancels_background_process_trees() {
        #[cfg(unix)]
        let command = "sleep 10 &";
        #[cfg(windows)]
        let command = "cmd.exe /D /C ping -n 30 127.0.0.1 &";
        let mut backend = NativeExecutor::default();
        ProcessBackend::execute_capture(&mut backend, command).unwrap();
        let jobs = ProcessBackend::jobs(&mut backend);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, JobStatus::Running);
        #[cfg(unix)]
        assert_eq!(
            ProcessBackend::suspend_job(&mut backend, jobs[0].id)
                .unwrap()
                .status,
            JobStatus::Stopped
        );
        #[cfg(windows)]
        assert!(
            ProcessBackend::suspend_job(&mut backend, jobs[0].id)
                .unwrap_err()
                .message
                .contains("does not support job suspension")
        );
        let started = Instant::now();
        let cancelled = ProcessBackend::cancel_job(&mut backend, jobs[0].id).unwrap();
        assert_eq!(cancelled.status, JobStatus::Done);
        assert_eq!(cancelled.exit_status, Some(130));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[derive(Clone, Copy)]
    struct SimulatedChild {
        status: JobStatus,
        exit_status: Option<i32>,
    }

    fn model_transition(
        current: JobStatus,
        event: JobLifecycleEvent,
    ) -> Option<(JobStatus, Option<i32>)> {
        match (current, event) {
            (JobStatus::Running, JobLifecycleEvent::Stop) => Some((JobStatus::Stopped, None)),
            (JobStatus::Stopped, JobLifecycleEvent::Continue) => Some((JobStatus::Running, None)),
            (JobStatus::Running | JobStatus::Stopped, JobLifecycleEvent::Exit(status)) => {
                Some((JobStatus::Done, Some(status)))
            }
            _ => None,
        }
    }

    fn assert_simulated_job_invariants(
        seed: u64,
        case_index: usize,
        step: usize,
        children: &[SimulatedChild],
    ) {
        let actual = summarize_job_lifecycle(
            children
                .iter()
                .map(|child| (child.status, child.exit_status)),
        );
        let live = children
            .iter()
            .filter(|child| child.status != JobStatus::Done)
            .collect::<Vec<_>>();
        let expected = if live.is_empty() {
            (
                JobStatus::Done,
                children.last().and_then(|child| child.exit_status),
            )
        } else if live.iter().all(|child| child.status == JobStatus::Stopped) {
            (JobStatus::Stopped, None)
        } else {
            (JobStatus::Running, None)
        };
        assert_eq!(
            actual, expected,
            "seed={seed} case={case_index} step={step}"
        );
    }

    #[test]
    fn seeded_job_simulation_converges_after_faults_freeze() {
        const CHILDREN_MAX: usize = 8;
        const SAFETY_STEPS_MAX: usize = 32;

        let (seed, cases) = configuration();
        let mut rng = DeterministicRng::new(seed);
        for case_index in 0..cases {
            let child_count = rng.index(CHILDREN_MAX) + 1;
            let mut children = vec![
                SimulatedChild {
                    status: JobStatus::Running,
                    exit_status: None,
                };
                child_count
            ];
            let safety_steps = rng.index(SAFETY_STEPS_MAX + 1);

            // Safety mode explores arbitrary notification orders, including
            // stale and duplicate events. Invalid transitions must fail
            // without mutating committed state.
            for step in 0..safety_steps {
                let child_index = rng.index(child_count);
                let event = match rng.index(3) {
                    0 => JobLifecycleEvent::Stop,
                    1 => JobLifecycleEvent::Continue,
                    _ => JobLifecycleEvent::Exit(i32::try_from(rng.index(256)).unwrap()),
                };
                let before = children[child_index];
                let expected = model_transition(before.status, event);
                match (expected, transition_job_state(before.status, event)) {
                    (Some(expected), Ok(actual)) => {
                        assert_eq!(
                            actual, expected,
                            "seed={seed} case={case_index} step={step} child={child_index}"
                        );
                        children[child_index] = SimulatedChild {
                            status: actual.0,
                            exit_status: actual.1,
                        };
                    }
                    (None, Err(error)) => assert_eq!(
                        error.code,
                        ErrorCode::InvalidArgument,
                        "seed={seed} case={case_index} step={step} child={child_index}"
                    ),
                    (expected, actual) => panic!(
                        "seed={seed} case={case_index} step={step} child={child_index} expected={expected:?} actual={actual:?}"
                    ),
                }
                assert_simulated_job_invariants(seed, case_index, step, &children);
            }

            // Liveness mode freezes completed children permanently and makes
            // the remaining process-group core healthy: stopped children are
            // continued, then every live child receives an exit notification.
            // No randomized fault is allowed to rescue a stuck transition.
            let mut order = (0..child_count).collect::<Vec<_>>();
            for index in (1..child_count).rev() {
                let other = rng.index(index + 1);
                order.swap(index, other);
            }
            let mut liveness_steps = 0_usize;
            for &child_index in &order {
                if children[child_index].status == JobStatus::Stopped {
                    let (status, exit_status) = transition_job_state(
                        children[child_index].status,
                        JobLifecycleEvent::Continue,
                    )
                    .unwrap();
                    children[child_index] = SimulatedChild {
                        status,
                        exit_status,
                    };
                    liveness_steps += 1;
                }
            }
            for &child_index in &order {
                if children[child_index].status != JobStatus::Done {
                    let (status, exit_status) = transition_job_state(
                        children[child_index].status,
                        JobLifecycleEvent::Exit(0),
                    )
                    .unwrap();
                    children[child_index] = SimulatedChild {
                        status,
                        exit_status,
                    };
                    liveness_steps += 1;
                }
            }
            assert!(
                liveness_steps <= child_count * 2,
                "seed={seed} case={case_index} liveness_steps={liveness_steps}"
            );
            assert_simulated_job_invariants(
                seed,
                case_index,
                safety_steps + liveness_steps,
                &children,
            );
            assert_eq!(
                summarize_job_lifecycle(
                    children
                        .iter()
                        .map(|child| (child.status, child.exit_status))
                )
                .0,
                JobStatus::Done,
                "seed={seed} case={case_index} did not converge"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn contained_child_termination_reaps_the_direct_child() {
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = ContainedChild::spawn(&mut command).unwrap();
        let started = Instant::now();
        let status = child.terminate_and_reap().unwrap();
        assert!(!status.success());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn contained_child_spawn_failure_returns_without_partial_ownership() {
        let mut command =
            std::process::Command::new("quirl-test-executable-that-must-not-exist-3f830f7d");
        let error = match ContainedChild::spawn(&mut command) {
            Ok(_) => panic!("missing executable unexpectedly spawned"),
            Err(error) => error,
        };
        assert_eq!(error.code, ErrorCode::ProcessSpawn);
    }

    #[test]
    fn isolated_process_host_closes_stdin_and_rejects_background_work() {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let host = isolated_process_host();
        let outcome = host(quirl_core::ProcessRequest {
            command: if cfg!(windows) { "cmd /c more" } else { "cat" }.to_owned(),
            deadline: std::time::Duration::from_secs(1),
            cancelled: std::sync::Arc::clone(&cancelled),
            max_output_bytes: 1024,
        })
        .unwrap();
        assert_eq!(outcome.stdout.as_deref(), Some(""));

        let error = host(quirl_core::ProcessRequest {
            command: if cfg!(windows) {
                "cmd /c timeout /t 1 &"
            } else {
                "sleep 1 &"
            }
            .to_owned(),
            deadline: std::time::Duration::from_secs(1),
            cancelled,
            max_output_bytes: 1024,
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains("background"));
    }
}
