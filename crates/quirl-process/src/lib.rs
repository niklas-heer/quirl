//! Native command graph execution and background-job lifecycle.

pub const RUNNER_PROTOCOL_VERSION: u32 = 1;
/// Maximum bytes retained per captured output stream when a caller does not
/// provide the tighter sandboxed-process budget.
pub const DEFAULT_CAPTURE_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes accepted by one native command-list execution.
pub const NATIVE_COMMAND_BYTES_MAX: usize = 1024 * 1024;
/// Maximum pipelines in one native command list.
pub const NATIVE_PIPELINES_MAX: usize = 256;
/// Maximum command stages in one native pipeline.
pub const NATIVE_PIPELINE_STAGES_MAX: usize = 64;
/// Maximum bytes written for one here-string, including its trailing newline.
pub const HERE_STRING_BYTES_MAX: usize = 256 * 1024;
/// Maximum bytes parsed by one arithmetic expansion.
pub const ARITHMETIC_SOURCE_BYTES_MAX: usize = 16 * 1024;
/// Maximum nested unary/parenthesized arithmetic expressions.
pub const ARITHMETIC_DEPTH_MAX: usize = 64;
pub const RUNNER_SCHEMA_DESCRIPTOR: &str = "quirl.runner@1{input:quirl.command-grammar@1,native-source-bytes<=1048576,native-pipelines<=256,native-stages-per-pipeline<=64,here-string-bytes-including-newline<=262144,arithmetic-source-bytes<=16384,arithmetic-depth<=64;ProcessBackend{execute_capture(source)->CommandOutcome;execute_interactive(source)->CommandOutcome;jobs()->array<JobState>;foreground_job(id)->JobState;cancel_job(id)->JobState;suspend_job(id)->JobState};JobState{deny_unknown;id:u32;command:string;status:running|stopped|done;process_group:null|i32;exit_status:null|i32};CommandOutcome{status:i32;stdout:null|string;stderr:null|string};capture:default-retained-per-stream=1048576|caller-tighter,drain-excess-then-ResourceLimit-with-retained-and-discarded-byte-context;interactive:inherit-streams-without-retention-limit;byte-pipeline:ordered;redirection:input|output|append|here-string;background:terminal-ampersand;cancel-status:130;errors:ShellError;platform:suspend-unavailable-on-windows}";

pub fn runner_schema_hash() -> String {
    quirl_core::schema_fingerprint(RUNNER_SCHEMA_DESCRIPTOR)
}

fn validate_native_source(input: &str) -> Result<(), quirl_core::ShellError> {
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
    }
    Ok(())
}

#[cfg(test)]
mod simulation_support {
    pub const DEFAULT_SIMULATION_CASES: usize = 128;
    pub const DEFAULT_SIMULATION_SEED: u64 = 7_640_891_576_956_012_809;
    pub const SIMULATION_CASES_MAX: usize = 10_000;

    pub struct DeterministicRng(u64);

    impl DeterministicRng {
        pub fn new(seed: u64) -> Self {
            // Xorshift cannot advance from zero, so map that valid CLI seed to
            // a fixed non-zero state while preserving deterministic replay.
            Self(if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            })
        }

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
        validate_native_plan, validate_native_source, ARITHMETIC_DEPTH_MAX,
        ARITHMETIC_SOURCE_BYTES_MAX, DEFAULT_CAPTURE_BYTES, HERE_STRING_BYTES_MAX,
    };

    use nix::{
        sys::{
            signal::{kill, killpg, pthread_sigmask, SigSet, SigmaskHow, Signal},
            termios::{tcgetattr, tcsetattr, SetArg, Termios},
            wait::{waitpid, WaitPidFlag, WaitStatus},
        },
        unistd::{tcgetpgrp, tcsetpgrp, Pid},
    };
    use os_pipe::{pipe, PipeReader, PipeWriter};
    use quirl_core::{CommandOutcome, CommandRunner, ErrorCode, ProcessRequest, ShellError};
    use quirl_syntax::{
        parse_command_list, ListConnector, Pipeline, Quoting, RedirectKind, SimpleCommand, Word,
    };
    use serde::{Deserialize, Serialize};
    use std::{
        env,
        fs::{File, OpenOptions},
        io::{ErrorKind, IsTerminal, Read, Write},
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        thread::{self, JoinHandle},
        time::Instant,
    };

    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum JobStatus {
        Running,
        Stopped,
        Done,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct JobState {
        pub id: u32,
        pub command: String,
        pub status: JobStatus,
        pub process_group: Option<i32>,
        pub exit_status: Option<i32>,
    }

    struct Job {
        state: JobState,
        children: Vec<JobChild>,
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

    type ReaderTask = JoinHandle<std::io::Result<ReaderCapture>>;
    type WriterTask = JoinHandle<std::io::Result<()>>;
    type PendingWriter = (PipeWriter, Vec<u8>);
    type OutputStdio = (Stdio, Option<PipeReader>, Option<PipeWriter>, Option<File>);

    struct PreparedInput {
        stdio: Stdio,
        writer: Option<PendingWriter>,
    }

    pub struct NativeExecutor {
        jobs: Vec<Job>,
        next_job_id: u32,
        substitution_depth: u8,
    }

    /// Cross-platform containment hook for a directly spawned child process.
    pub struct ChildProcessTree;

    impl ChildProcessTree {
        pub fn new() -> Result<Self, ShellError> {
            Ok(Self)
        }

        pub fn assign(&self, _child: &mut Child) -> Result<(), ShellError> {
            Ok(())
        }

        pub fn terminate(&self, child: &mut Child) -> Result<(), ShellError> {
            match child.kill() {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == ErrorKind::InvalidInput => Ok(()),
                Err(error) => Err(ShellError::new(
                    ErrorCode::ProcessSpawn,
                    "could not terminate contained child process",
                )
                .with_context(error.to_string())
                .with_help("Retry the command; report repeated process termination failures")),
            }
        }
    }

    impl Default for NativeExecutor {
        fn default() -> Self {
            Self {
                jobs: Vec::new(),
                next_job_id: 1,
                substitution_depth: 0,
            }
        }
    }

    impl Drop for NativeExecutor {
        fn drop(&mut self) {
            for job in &mut self.jobs {
                if job.state.status != JobStatus::Done {
                    terminate_children(&mut job.children, job.state.process_group);
                }
                finish_job_tasks_silently(job);
            }
        }
    }

    impl NativeExecutor {
        /// Execute an ordinary foreground command with terminal streams
        /// inherited. Unlike capture APIs, interactive output is not retained
        /// or rejected at the programmatic capture ceiling.
        pub fn execute_interactive(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute(input)
        }

        pub fn execute(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, false)
        }

        pub fn execute_capture(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, true)
        }

        /// Execute a foreground command under a host-provided cancellation,
        /// deadline, and retained-output budget.
        pub fn execute_capture_request(
            &mut self,
            request: ProcessRequest,
        ) -> Result<CommandOutcome, ShellError> {
            self.execute_inner_with_request(&request.command, true, Some(&request))
        }

        pub fn jobs(&mut self) -> Vec<JobState> {
            self.refresh_jobs();
            self.jobs.iter().map(|job| job.state.clone()).collect()
        }

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
                terminate_children(&mut job.children, job.state.process_group);
                finish_job_tasks_silently(job);
                job.state.status = JobStatus::Done;
                job.state.exit_status = Some(130);
            }
            Ok(job.state.clone())
        }

        pub fn suspend_job(&mut self, id: u32) -> Result<JobState, ShellError> {
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
            if let Some(group) = job.state.process_group {
                killpg(Pid::from_raw(group), Signal::SIGSTOP).map_err(|error| {
                    ShellError::new(ErrorCode::Io, format!("could not suspend job %{id}"))
                        .with_context(error.to_string())
                        .with_help("Run `jobs` to refresh the job before retrying")
                })?;
            } else {
                for child in &job.children {
                    let pid = i32::try_from(child.child.id()).map_err(|_| {
                        ShellError::new(ErrorCode::Io, "child process id exceeds platform limits")
                            .with_help("Cancel the job and start it again")
                    })?;
                    kill(Pid::from_raw(pid), Signal::SIGSTOP).map_err(|error| {
                        ShellError::new(ErrorCode::Io, format!("could not suspend job %{id}"))
                            .with_context(error.to_string())
                            .with_help("Run `jobs` to refresh the job before retrying")
                    })?;
                }
            }
            for child in &mut job.children {
                child.status = JobStatus::Stopped;
            }
            job.state.status = JobStatus::Stopped;
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
            request: Option<&ProcessRequest>,
        ) -> Result<CommandOutcome, ShellError> {
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
                last = self.execute_pipeline(pipeline, input, capture, request, last.status)?;
                if capture {
                    append_captured_output(
                        &mut captured_stdout,
                        last.stdout.as_deref().unwrap_or_default(),
                        retained_output_limit(request),
                    )?;
                    append_captured_output(
                        &mut captured_stderr,
                        last.stderr.as_deref().unwrap_or_default(),
                        retained_output_limit(request),
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
            request: Option<&ProcessRequest>,
            previous_status: i32,
        ) -> Result<CommandOutcome, ShellError> {
            let pipeline = self.expand_pipeline(pipeline, request, previous_status)?;
            let pipeline = &pipeline;
            if pipeline.commands.len() == 1 {
                if pipeline.background
                    && pipeline.commands[0].words.first().is_some_and(|name| {
                        matches!(name.as_str(), "cd" | "ls" | "export" | "jobs" | "fg" | "bg")
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
            self.spawn_pipeline(pipeline, source, capture, request)
        }

        fn expand_pipeline(
            &mut self,
            pipeline: &Pipeline,
            request: Option<&ProcessRequest>,
            previous_status: i32,
        ) -> Result<Pipeline, ShellError> {
            const MAX_SUBSTITUTION_BYTES: usize = 16 * 1024;
            if request.is_some_and(|request| request.cancelled.load(Ordering::Relaxed)) {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "command expansion was cancelled before execution",
                )
                .with_help("Run the command again when cancellation is no longer requested"));
            }
            let mut expanded = pipeline.clone();
            for command in &mut expanded.commands {
                let forms = command.word_ir.clone();
                if forms.is_empty() {
                    continue;
                }
                let mut words = Vec::new();
                for word in &forms {
                    let (value, glob) =
                        self.expand_word(word, MAX_SUBSTITUTION_BYTES, request, previous_status)?;
                    let matches = if glob {
                        pathname_expand(&value)?
                    } else {
                        Vec::new()
                    };
                    if matches.is_empty() {
                        words.push(value);
                    } else {
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
            request: Option<&ProcessRequest>,
            previous_status: i32,
        ) -> Result<(String, bool), ShellError> {
            let mut value = String::new();
            let mut pathname = false;
            for part in &word.parts {
                if matches!(part.quoting, Quoting::Single | Quoting::Escaped) {
                    value.push_str(&part.text);
                    continue;
                }
                pathname |= part.quoting == Quoting::Unquoted
                    && part
                        .text
                        .chars()
                        .any(|character| matches!(character, '*' | '?' | '['));
                value.push_str(&self.expand_fragment(
                    &part.text,
                    limit,
                    request,
                    previous_status,
                )?);
            }
            Ok((value, pathname))
        }

        fn expand_fragment(
            &mut self,
            text: &str,
            limit: usize,
            request: Option<&ProcessRequest>,
            previous_status: i32,
        ) -> Result<String, ShellError> {
            let mut output = String::new();
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
                    output.push_str(&evaluate_arithmetic(&arithmetic[..close])?.to_string());
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
                    output.push_str(stdout.trim_end_matches('\n'));
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
                    output.push_str(&parameter_value(&after[..close]));
                    index += 3 + close;
                    continue;
                }
                if let Some(after) = rest.strip_prefix('$') {
                    let Some(character) = after.chars().next() else {
                        output.push('$');
                        break;
                    };
                    if character == '?' {
                        output.push_str(&previous_status.to_string());
                        index += 2;
                        continue;
                    }
                    if character == '$' {
                        output.push_str(&std::process::id().to_string());
                        index += 2;
                        continue;
                    }
                    if character == '_' || character.is_ascii_alphabetic() {
                        let length = after
                            .chars()
                            .take_while(|value| *value == '_' || value.is_ascii_alphanumeric())
                            .map(char::len_utf8)
                            .sum();
                        output.push_str(&parameter_value(&after[..length]));
                        index += 1 + length;
                        continue;
                    }
                }
                let character = rest.chars().next().unwrap_or_default();
                output.push(character);
                index += character.len_utf8();
            }
            Ok(output)
        }

        fn execute_control_builtin(
            &mut self,
            command: &SimpleCommand,
            capture: bool,
        ) -> Result<Option<CommandOutcome>, ShellError> {
            let Some(name) = command.words.first().map(String::as_str) else {
                return Ok(None);
            };
            if !matches!(name, "cd" | "ls" | "export" | "jobs" | "fg" | "bg") {
                return Ok(None);
            }
            validate_control_redirects(command)?;
            let result = match name {
                "cd" => Some(
                    CommandRunner::default()
                        .execute_capture(&join_command_words(&command.words))?,
                ),
                "ls" => Some(
                    CommandRunner::default()
                        .execute_capture(&join_command_words(&command.words))?,
                ),
                "export" => {
                    if command.words.len() == 1 {
                        return Err(ShellError::new(
                            ErrorCode::InvalidArgument,
                            "export needs at least one NAME=value assignment",
                        )
                        .with_help("Use `export NAME=value`"));
                    }
                    for assignment in command.words.iter().skip(1) {
                        let Some((name, value)) = assignment.split_once('=') else {
                            return Err(ShellError::new(
                                ErrorCode::InvalidArgument,
                                format!("invalid export assignment `{assignment}`"),
                            )
                            .with_help("Use `export NAME=value`"));
                        };
                        let mut characters = name.chars();
                        if !characters.next().is_some_and(|character| {
                            character == '_' || character.is_ascii_alphabetic()
                        }) || !characters
                            .all(|character| character == '_' || character.is_ascii_alphanumeric())
                        {
                            return Err(ShellError::new(
                                ErrorCode::InvalidArgument,
                                format!("invalid environment name `{name}`"),
                            )
                            .with_help(
                                "Environment names use ASCII letters, digits, and underscores",
                            ));
                        }
                        env::set_var(name, value);
                    }
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
            request: Option<&ProcessRequest>,
        ) -> Result<CommandOutcome, ShellError> {
            // Pipeline construction is a transaction. Local descriptors and
            // the guard own every partial resource until the complete process
            // group is either registered as a job or handed to the waiter.
            // Extension callbacks and other user code must stay outside this
            // window so an early child notification cannot observe half a job.
            let mut spawned = PipelineConstructionGuard::default();
            let mut previous_reader: Option<PipeReader> = None;
            let mut capture_reader = None;
            let mut stderr_readers = Vec::new();
            let mut pending_writers: Vec<PendingWriter> = Vec::new();
            let capture_streams = capture && !pipeline.background;
            let output_limit = retained_output_limit(request);
            let stderr_budget = Arc::new(CaptureBudget::new(output_limit));

            for (index, command) in pipeline.commands.iter().enumerate() {
                let last = index + 1 == pipeline.commands.len();
                if command.words.first().is_some_and(|word| word == "ls") && index != 0 {
                    return Err(ShellError::new(
                        ErrorCode::InvalidCommand,
                        "native `ls` can only be the first stage of a Preview pipeline",
                    )
                    .with_command(source)
                    .with_help("Move `ls` to the start of the pipeline or use `^ls`"));
                }
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
                let input = input_stdio(command, previous_reader.take(), index > 0)?;
                let (stdout, next_reader, writer, redirected_stdout) =
                    output_stdio(command, last, capture_streams)?;
                if last && capture_streams {
                    capture_reader = next_reader;
                } else {
                    previous_reader = next_reader;
                }

                if command.words.first().is_some_and(|word| word == "ls") {
                    let result = CommandRunner::default()
                        .execute_capture(&join_command_words(&command.words))?;
                    let bytes = result.stdout.unwrap_or_default().into_bytes();
                    if command.redirects.iter().any(|redirect| {
                        matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append)
                    }) {
                        write_redirected_output(command, &bytes)?;
                    } else if let Some(writer) = writer {
                        pending_writers.push((writer, bytes));
                    } else if !capture_streams {
                        io_write_all(std::io::stdout(), &bytes, "standard output")?;
                    }
                    drop(input);
                    continue;
                }

                let executable = command
                    .words
                    .first()
                    .map(|word| word.strip_prefix('^').unwrap_or(word))
                    .ok_or_else(|| {
                        ShellError::new(ErrorCode::InvalidCommand, "empty command stage")
                    })?;
                let mut process = Command::new(executable);
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
                #[cfg(unix)]
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
                if capture_streams {
                    if let Some(stderr) = child.stderr.take() {
                        stderr_readers
                            .push(spawn_reader_with_budget(stderr, Arc::clone(&stderr_budget)));
                    }
                }
                spawned.push(child)?;
                if let Some(writer) = input.writer {
                    pending_writers.push(writer);
                }
            }

            // Start writers only after every child exists. A pending writer is
            // owned by this construction transaction until then, so any spawn
            // failure closes the descriptor without leaving a detached task.
            let writers = pending_writers
                .into_iter()
                .map(|(mut writer, bytes)| thread::spawn(move || writer.write_all(&bytes)))
                .collect::<Vec<_>>();

            if pipeline.background {
                let id = self.next_job_id;
                self.next_job_id = self.next_job_id.saturating_add(1);
                let process_group = spawned.process_group;
                self.jobs.push(Job {
                    state: JobState {
                        id,
                        command: source.to_owned(),
                        status: JobStatus::Running,
                        process_group,
                        exit_status: None,
                    },
                    children: spawned.release(),
                    capture: false,
                    stdout_reader: None,
                    stderr_readers: Vec::new(),
                    writers,
                });
                return Ok(outcome(
                    0,
                    Some(format!("[{id}] {}", process_group.unwrap_or_default())),
                    capture.then(String::new),
                ));
            }

            let process_group = spawned.process_group;
            let mut terminal = ForegroundTerminal::give_to(process_group)?;
            let mut children = spawned.release();
            let stdout_reader = capture_reader.map(|reader| spawn_reader(reader, output_limit));
            let child_count = children.len();
            let mut wait_error = None;
            if let Some(request) = request {
                wait_error =
                    wait_for_children_with_request(&mut children, process_group, request).err();
            } else {
                for child in &mut children {
                    match wait_for_child(&mut child.child) {
                        Ok(exit) => child.record(exit),
                        Err(error) => {
                            wait_error = Some(error);
                            break;
                        }
                    }
                }
            }
            if let Some(error) = wait_error {
                terminate_children(&mut children, process_group);
                let _ = terminal.restore();
                let _ = join_reader(stdout_reader, "pipeline output");
                let _ = join_readers(stderr_readers, "command error output");
                let _ = join_writers(writers);
                return Err(error);
            }
            terminal.restore()?;
            let status = children
                .get(child_count.saturating_sub(1))
                .and_then(|child| child.exit_status)
                .unwrap_or(0);
            if children
                .iter()
                .any(|child| child.status == JobStatus::Stopped)
            {
                let id = self.next_job_id;
                self.next_job_id = self.next_job_id.saturating_add(1);
                self.jobs.push(Job {
                    state: JobState {
                        id,
                        command: source.to_owned(),
                        status: JobStatus::Stopped,
                        process_group,
                        exit_status: None,
                    },
                    children,
                    capture: capture_streams,
                    stdout_reader,
                    stderr_readers,
                    writers,
                });
                return Ok(outcome(
                    status,
                    Some(format!("[{id}] stopped {source}")),
                    capture.then(String::new),
                ));
            }
            let stdout = join_reader(stdout_reader, "pipeline output");
            let stderr = join_readers(stderr_readers, "command error output");
            let writers = join_writers(writers);
            let stdout = stdout?;
            let stderr = stderr?;
            writers?;
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
                    finish_job_tasks_silently(job);
                }
            }
        }

        fn foreground(&mut self, id: Option<u32>) -> Result<CommandOutcome, ShellError> {
            self.refresh_jobs();
            let index = select_job(&self.jobs, id)?;
            let mut terminal = ForegroundTerminal::give_to(self.jobs[index].state.process_group)?;
            resume_job(&self.jobs[index])?;
            let mut job = self.jobs.remove(index);
            let mut wait_error = None;
            for child in &mut job.children {
                if child.status == JobStatus::Done {
                    continue;
                }
                child.status = JobStatus::Running;
                match wait_for_child(&mut child.child) {
                    Ok(exit) => child.record(exit),
                    Err(error) => {
                        wait_error = Some(error);
                        break;
                    }
                }
            }
            if let Some(error) = wait_error {
                terminate_children(&mut job.children, job.state.process_group);
                return Err(error);
            }
            terminal.restore()?;
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
                job.state.status = JobStatus::Stopped;
                self.jobs.push(job);
                return Ok(outcome(status, None, None));
            }
            job.state.status = JobStatus::Done;
            job.state.exit_status = Some(status);
            let stdout = join_reader(job.stdout_reader.take(), "pipeline output");
            let stderr = join_readers(
                std::mem::take(&mut job.stderr_readers),
                "command error output",
            );
            let writers = join_writers(std::mem::take(&mut job.writers));
            let stdout = stdout?;
            let stderr = stderr?;
            writers?;
            Ok(outcome(
                status,
                job.capture.then_some(stdout),
                job.capture.then_some(stderr),
            ))
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

    fn parameter_value(name: &str) -> String {
        std::env::var(name).unwrap_or_default()
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
                            "invalid arithmetic expansion",
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
                        "invalid arithmetic expansion",
                        "Use integer literals and +, -, *, /, or parentheses",
                    ));
                }
                let text = std::str::from_utf8(&self.input[start..self.index]).map_err(|_| {
                    expansion_error("invalid arithmetic expansion", "Use ASCII integer literals")
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
                "invalid arithmetic expansion",
                "Use integer literals and +, -, *, /, or parentheses",
            ));
        }
        Ok(value)
    }

    fn pathname_expand(pattern: &str) -> Result<Vec<String>, ShellError> {
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
            for prefix in &paths {
                if !has_pattern {
                    next.push(prefix.join(component));
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
                        next.push(prefix.join(name));
                        if next.len() > MAX_GLOB_MATCHES {
                            return Err(ShellError::new(
                                ErrorCode::ResourceLimit,
                                "pathname expansion exceeded its match budget",
                            )
                            .with_help(
                                "Narrow the pattern below 10,000 matches or use an explicit data pipeline",
                            ));
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
    ) -> Result<PreparedInput, ShellError> {
        let mut redirected = None;
        let mut here_string = None;
        for redirect in command.redirects.iter().filter(|redirect| {
            matches!(
                redirect.kind,
                RedirectKind::Input | RedirectKind::HereString
            )
        }) {
            if redirect.kind == RedirectKind::HereString {
                here_string = Some(redirect.path.clone());
            } else {
                redirected = Some(File::open(&redirect.path).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Io,
                        format!("cannot read redirected input {}", redirect.path),
                    )
                    .with_context(error.to_string())
                    .with_help("Check that the file exists and is readable")
                })?);
            }
        }
        if let Some(value) = here_string {
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
            return Ok(PreparedInput {
                stdio: Stdio::from(reader),
                writer: Some((writer, bytes)),
            });
        }
        if let Some(file) = redirected {
            return Ok(PreparedInput {
                stdio: Stdio::from(file),
                writer: None,
            });
        }
        Ok(PreparedInput {
            stdio: previous.map_or_else(
                || {
                    if has_upstream {
                        Stdio::null()
                    } else {
                        Stdio::inherit()
                    }
                },
                Stdio::from,
            ),
            writer: None,
        })
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
        } else if !capture {
            if let Some(stdout) = result.stdout.take() {
                io_write_all(std::io::stdout(), stdout.as_bytes(), "standard output")?;
            }
        }
        if !capture {
            if let Some(stderr) = result.stderr.take() {
                io_write_all(std::io::stderr(), stderr.as_bytes(), "standard error")?;
            }
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

    fn join_command_words(words: &[String]) -> String {
        words
            .iter()
            .map(|word| {
                if word.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "_./-".contains(character)
                }) {
                    word.clone()
                } else {
                    format!("'{}'", word.replace('\'', "'\\''"))
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn write_redirected_output(command: &SimpleCommand, bytes: &[u8]) -> Result<(), ShellError> {
        let redirect = command
            .redirects
            .iter()
            .rev()
            .find(|redirect| matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append))
            .ok_or_else(|| {
                ShellError::new(ErrorCode::Io, "missing output redirection")
                    .with_help("Remove the redirect and retry")
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

    fn resume_job(job: &Job) -> Result<(), ShellError> {
        if let Some(group) = job.state.process_group {
            if killpg(Pid::from_raw(group), Signal::SIGCONT).is_ok() {
                return Ok(());
            }
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

    #[derive(Default)]
    struct PipelineConstructionGuard {
        children: Vec<JobChild>,
        process_group: Option<i32>,
    }

    impl PipelineConstructionGuard {
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
            self.process_group.get_or_insert(process_id);
            self.children.push(JobChild {
                child,
                status: JobStatus::Running,
                exit_status: None,
            });
            Ok(())
        }

        fn release(&mut self) -> Vec<JobChild> {
            std::mem::take(&mut self.children)
        }
    }

    impl Drop for PipelineConstructionGuard {
        fn drop(&mut self) {
            if !self.children.is_empty() {
                terminate_children(&mut self.children, self.process_group);
            }
        }
    }

    impl JobChild {
        fn record(&mut self, result: ChildWait) {
            self.status = if result.stopped {
                JobStatus::Stopped
            } else {
                JobStatus::Done
            };
            self.exit_status = Some(result.status);
        }
    }

    fn terminate_children(children: &mut [JobChild], process_group: Option<i32>) {
        if children.is_empty() {
            return;
        }
        if let Some(group) = process_group {
            let _ = killpg(Pid::from_raw(group), Signal::SIGKILL);
        }
        for child in children {
            if child.status != JobStatus::Done {
                if process_group.is_none() {
                    let _ = child.child.kill();
                }
                let _ = child.child.wait();
                child.status = JobStatus::Done;
            }
        }
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
        let Ok(process_id) = i32::try_from(child.child.id()) else {
            return;
        };
        let Ok(status) = waitpid(
            Pid::from_raw(process_id),
            Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED),
        ) else {
            return;
        };
        match status {
            WaitStatus::Exited(_, code) => {
                child.status = JobStatus::Done;
                child.exit_status = Some(code);
            }
            WaitStatus::Signaled(_, signal, _) => {
                child.status = JobStatus::Done;
                child.exit_status = Some(128 + signal as i32);
            }
            WaitStatus::Stopped(_, signal) => {
                child.status = JobStatus::Stopped;
                child.exit_status = Some(128 + signal as i32);
            }
            WaitStatus::Continued(_) => {
                child.status = JobStatus::Running;
                child.exit_status = None;
            }
            WaitStatus::StillAlive => {}
        }
    }

    struct ForegroundTerminal {
        restore_group: Option<Pid>,
        restore_modes: Option<Termios>,
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
        fn give_to(process_group: Option<i32>) -> Result<Self, ShellError> {
            let mut restore_modes = None;
            let restore_group = if std::io::stdin().is_terminal() {
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
            })
        }

        fn restore(&mut self) -> Result<(), ShellError> {
            let Some(group) = self.restore_group else {
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
            Ok(())
        }
    }

    impl Drop for ForegroundTerminal {
        fn drop(&mut self) {
            let _ = self.restore();
        }
    }

    struct ChildWait {
        status: i32,
        stopped: bool,
    }

    fn wait_for_children_with_request(
        children: &mut [JobChild],
        process_group: Option<i32>,
        request: &ProcessRequest,
    ) -> Result<(), ShellError> {
        let deadline = Instant::now() + request.deadline;
        loop {
            for child in children
                .iter_mut()
                .filter(|child| child.status != JobStatus::Done)
            {
                match child.child.try_wait() {
                    Ok(Some(status)) => child.record(ChildWait {
                        status: status.code().unwrap_or(1),
                        stopped: false,
                    }),
                    Ok(None) => {}
                    Err(error) => {
                        terminate_children(children, process_group);
                        return Err(ShellError::new(ErrorCode::Io, "could not poll command")
                            .with_context(error.to_string())
                            .with_help("Retry the command; report this if the failure repeats"));
                    }
                }
            }
            if children.iter().all(|child| child.status == JobStatus::Done) {
                return Ok(());
            }
            let cancelled = request.cancelled.load(Ordering::Relaxed);
            if cancelled || Instant::now() >= deadline {
                terminate_children(children, process_group);
                let message = if cancelled {
                    "process execution was cancelled"
                } else {
                    "process execution exceeded its deadline"
                };
                return Err(
                    ShellError::new(ErrorCode::ResourceLimit, message).with_help(
                        "Use a shorter-running command or increase the Lua policy deadline",
                    ),
                );
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn wait_for_child(child: &mut Child) -> Result<ChildWait, ShellError> {
        let pid = i32::try_from(child.id())
            .map(Pid::from_raw)
            .map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "child process id is outside the platform range",
                )
                .with_context(error.to_string())
                .with_help("Report this platform-specific process error")
            })?;
        loop {
            match waitpid(pid, Some(WaitPidFlag::WUNTRACED)).map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not wait for command")
                    .with_context(error.to_string())
                    .with_help("Inspect the job with `jobs` and retry")
            })? {
                WaitStatus::Exited(_, code) => {
                    return Ok(ChildWait {
                        status: code,
                        stopped: false,
                    });
                }
                WaitStatus::Signaled(_, signal, _) => {
                    return Ok(ChildWait {
                        status: 128 + signal as i32,
                        stopped: false,
                    });
                }
                WaitStatus::Stopped(_, signal) => {
                    return Ok(ChildWait {
                        status: 128 + signal as i32,
                        stopped: true,
                    });
                }
                WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
            }
        }
    }

    fn outcome(status: i32, stdout: Option<String>, stderr: Option<String>) -> CommandOutcome {
        CommandOutcome {
            status,
            stdout,
            stderr,
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
        use crate::simulation_support::{configuration, DeterministicRng};
        use std::{
            fs,
            sync::atomic::{AtomicUsize, Ordering},
            time::Duration,
        };

        static NEXT_TEMP_PATH: AtomicUsize = AtomicUsize::new(0);

        fn temporary_path(label: &str) -> std::path::PathBuf {
            env::temp_dir().join(format!(
                "quirl-process-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed)
            ))
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
        fn native_ls_and_external_commands_share_a_byte_pipeline() {
            let mut executor = NativeExecutor::default();
            let result = executor.execute_capture("ls | grep Cargo.toml").unwrap();
            assert_eq!(result.status, 0);
            assert_eq!(result.stdout.as_deref(), Some("Cargo.toml\n"));
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
            assert!(error
                .details
                .context
                .iter()
                .any(|context| context.contains("discarded") && context.contains("retained")));
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
            assert!(error
                .details
                .context
                .iter()
                .any(|context| context.contains("discarded 32768 bytes")));
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
            assert!(error
                .details
                .context
                .iter()
                .any(|context| context.contains("discarded 1024 bytes")));
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
        fn builtin_redirects_are_opened_before_state_mutation() {
            let variable = format!(
                "QUIRL_PROCESS_REDIRECT_{}",
                NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed)
            );
            env::remove_var(&variable);
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
        fn builtin_redirection_and_quoted_paths_preserve_words() {
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
            let jobs = executor.jobs();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].status, JobStatus::Stopped);

            let finished = executor.execute_capture("fg %1").unwrap();
            assert_eq!(finished.status, 7);
            assert!(executor.jobs().is_empty());
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
            let mut command = Command::new("sh");
            command.arg("-c").arg("sleep 10");
            #[cfg(unix)]
            command.process_group(0);
            let child = command.spawn().unwrap();
            let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
            let mut guard = PipelineConstructionGuard::default();
            guard.push(child).unwrap();
            drop(guard);
            assert!(kill(pid, None).is_err());
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
                let mut guard = PipelineConstructionGuard::default();
                let mut process_ids = Vec::with_capacity(fault_after);

                for stage in 0..planned_stages {
                    let mut command = Command::new("sh");
                    command.arg("-c").arg("sleep 10");
                    command.process_group(guard.process_group.unwrap_or(0));
                    let child = command.spawn().unwrap_or_else(|error| {
                        panic!("seed={seed} case={case_index} stage={stage} spawn failed: {error}")
                    });
                    process_ids.push(Pid::from_raw(i32::try_from(child.id()).unwrap()));
                    guard.push(child).unwrap();
                    if stage + 1 == fault_after {
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
    use super::{validate_native_plan, validate_native_source, DEFAULT_CAPTURE_BYTES};
    use quirl_core::{CommandOutcome, CommandRunner, ErrorCode, ProcessRequest, ShellError};
    use quirl_syntax::{parse_command_list, ListConnector, Pipeline, RedirectKind, SimpleCommand};
    use serde::{Deserialize, Serialize};
    use std::{
        env,
        fs::{File, OpenOptions},
        io::{self, Read, Write},
        os::windows::io::AsRawHandle,
        process::{Child, ChildStdout, Command, Stdio},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        thread::{self, JoinHandle},
        time::Instant,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    };

    #[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(rename_all = "snake_case")]
    pub enum JobStatus {
        Running,
        Stopped,
        Done,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    pub struct JobState {
        pub id: u32,
        pub command: String,
        pub status: JobStatus,
        pub process_group: Option<i32>,
        pub exit_status: Option<i32>,
    }

    struct Job {
        state: JobState,
        children: Vec<Child>,
        exit_statuses: Vec<Option<i32>>,
        object: JobObject,
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

    pub struct NativeExecutor {
        jobs: Vec<Job>,
        next_job_id: u32,
    }

    /// A kill-on-close Job Object used by non-shell process adapters.
    pub struct ChildProcessTree(JobObject);

    impl ChildProcessTree {
        pub fn new() -> Result<Self, ShellError> {
            JobObject::new().map(Self)
        }

        pub fn assign(&self, child: &mut Child) -> Result<(), ShellError> {
            self.0.assign(child)
        }

        pub fn terminate(&self, _child: &mut Child) -> Result<(), ShellError> {
            self.0.terminate(130)
        }
    }

    impl Default for NativeExecutor {
        fn default() -> Self {
            Self {
                jobs: Vec::new(),
                next_job_id: 1,
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
            }
        }
    }

    impl NativeExecutor {
        /// Execute an ordinary foreground command with terminal streams
        /// inherited. Unlike capture APIs, interactive output is not retained
        /// or rejected at the programmatic capture ceiling.
        pub fn execute_interactive(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute(input)
        }

        pub fn execute(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, false)
        }

        pub fn execute_capture(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
            self.execute_inner(input, true)
        }

        pub fn execute_capture_request(
            &mut self,
            request: ProcessRequest,
        ) -> Result<CommandOutcome, ShellError> {
            self.execute_inner_with_request(&request.command, true, Some(&request))
        }

        pub fn jobs(&mut self) -> Vec<JobState> {
            for job in &mut self.jobs {
                if job.state.status == JobStatus::Running {
                    refresh_children(&mut job.children, &mut job.exit_statuses);
                    if job.exit_statuses.iter().all(Option::is_some) {
                        job.state.status = JobStatus::Done;
                        job.state.exit_status = job.exit_statuses.last().copied().flatten();
                    }
                }
            }
            self.jobs.iter().map(|job| job.state.clone()).collect()
        }

        pub fn cancel_job(&mut self, id: u32) -> Result<JobState, ShellError> {
            let job = self
                .jobs
                .iter_mut()
                .find(|job| job.state.id == id)
                .ok_or_else(|| missing_job_error(id))?;
            if job.state.status != JobStatus::Done {
                job.object.terminate(130)?;
                wait_children(&mut job.children, &mut job.exit_statuses);
                job.state.status = JobStatus::Done;
                job.state.exit_status = Some(130);
            }
            Ok(job.state.clone())
        }

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
            request: Option<&ProcessRequest>,
        ) -> Result<CommandOutcome, ShellError> {
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
                        retained_output_limit(request),
                    )?;
                    append_captured_output(
                        &mut captured_stderr,
                        last.stderr.as_deref().unwrap_or_default(),
                        retained_output_limit(request),
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
            request: Option<&ProcessRequest>,
        ) -> Result<CommandOutcome, ShellError> {
            if pipeline.commands.len() == 1 {
                if pipeline.background
                    && pipeline.commands[0].words.first().is_some_and(|name| {
                        matches!(name.as_str(), "cd" | "ls" | "export" | "jobs" | "fg" | "bg")
                    })
                {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        "stateful built-ins cannot run in the background",
                    )
                    .with_command(source)
                    .with_help("Run the built-in without `&`"));
                }
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
                "cd" | "ls" => {
                    let runner = CommandRunner::default();
                    let line = command.words.join(" ");
                    Ok(Some(runner.execute_capture(&line)?))
                }
                "export" => {
                    for assignment in command.words.iter().skip(1) {
                        let Some((name, value)) = assignment.split_once('=') else {
                            return Err(ShellError::new(
                                ErrorCode::InvalidArgument,
                                format!("invalid export assignment `{assignment}`"),
                            )
                            .with_help("Use `export NAME=value`"));
                        };
                        env::set_var(name, value);
                    }
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
            request: Option<&ProcessRequest>,
        ) -> Result<CommandOutcome, ShellError> {
            let object = JobObject::new()?;
            let mut children = Vec::with_capacity(pipeline.commands.len());
            let mut exit_statuses = Vec::with_capacity(pipeline.commands.len());
            let mut previous_stdout: Option<ChildStdout> = None;
            let mut stdout_reader = None;
            let mut stderr_readers = Vec::new();
            let output_limit = retained_output_limit(request);
            let stderr_budget = Arc::new(CaptureBudget::new(output_limit));

            for (index, command) in pipeline.commands.iter().enumerate() {
                let Some(program) = command.words.first() else {
                    continue;
                };
                let last = index + 1 == pipeline.commands.len();
                let input = command
                    .redirects
                    .iter()
                    .rev()
                    .find(|redirect| redirect.kind == RedirectKind::Input);
                let output = command.redirects.iter().rev().find(|redirect| {
                    matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append)
                });
                let mut process = Command::new(program);
                process.args(command.words.iter().skip(1));
                if let Some(redirect) = input {
                    drop(previous_stdout.take());
                    process.stdin(Stdio::from(open_input(&redirect.path, source)?));
                } else if let Some(stdout) = previous_stdout.take() {
                    process.stdin(Stdio::from(stdout));
                } else if index > 0 {
                    process.stdin(Stdio::null());
                } else {
                    process.stdin(Stdio::inherit());
                }
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
                let id = self.next_job_id;
                self.next_job_id = self.next_job_id.saturating_add(1);
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
        request: &ProcessRequest,
    ) -> Result<(), ShellError> {
        let deadline = Instant::now() + request.deadline;
        loop {
            refresh_children(children, exit_statuses);
            if exit_statuses.iter().all(Option::is_some) {
                return Ok(());
            }
            if request.cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
                object.terminate(130)?;
                wait_children(children, exit_statuses);
                let message = if request.cancelled.load(Ordering::Relaxed) {
                    "process execution was cancelled"
                } else {
                    "process execution exceeded its deadline"
                };
                return Err(
                    ShellError::new(ErrorCode::ResourceLimit, message).with_help(
                        "Use a shorter-running command or increase the Lua policy deadline",
                    ),
                );
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobLifecycleEvent {
    Stop,
    Continue,
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
        (_, event) => Err(quirl_core::ShellError::new(
            quirl_core::ErrorCode::InvalidArgument,
            format!("invalid job lifecycle transition from {current:?} through {event:?}"),
        )
        .with_help("Refresh the job list before requesting another lifecycle transition")),
    }
}

/// Stable process backend contract used by the CLI independently of the host platform.
pub trait ProcessBackend {
    fn execute(
        &mut self,
        input: &str,
    ) -> Result<quirl_core::CommandOutcome, quirl_core::ShellError>;
    fn execute_capture(
        &mut self,
        input: &str,
    ) -> Result<quirl_core::CommandOutcome, quirl_core::ShellError>;
    fn jobs(&mut self) -> Vec<JobState>;
    fn cancel_job(&mut self, id: u32) -> Result<JobState, quirl_core::ShellError>;
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
    use crate::simulation_support::{configuration, DeterministicRng};
    use quirl_core::ErrorCode;
    use std::{
        fs,
        path::PathBuf,
        sync::{atomic::AtomicBool, Arc},
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
        assert!(transition_job_state(JobStatus::Done, JobLifecycleEvent::Continue).is_err());
        assert!(transition_job_state(JobStatus::Stopped, JobLifecycleEvent::Stop).is_err());
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
        std::env::set_var("QUIRL_C1_WORD", "expanded");
        let mut backend = NativeExecutor::default();
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
        std::env::remove_var("QUIRL_C1_WORD");
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
        assert!(ProcessBackend::suspend_job(&mut backend, jobs[0].id)
            .unwrap_err()
            .message
            .contains("does not support job suspension"));
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
}
