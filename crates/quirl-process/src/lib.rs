//! Native command graph execution and background-job lifecycle.

use nix::{
    sys::{
        signal::{kill, killpg, pthread_sigmask, SigSet, SigmaskHow, Signal},
        termios::{tcgetattr, tcsetattr, SetArg, Termios},
        wait::{waitpid, WaitPidFlag, WaitStatus},
    },
    unistd::{tcgetpgrp, tcsetpgrp, Pid},
};
use os_pipe::{pipe, PipeReader, PipeWriter};
use quirl_core::{CommandOutcome, CommandRunner, ErrorCode, ShellError};
use quirl_syntax::{parse_command_list, ListConnector, Pipeline, RedirectKind, SimpleCommand};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{File, OpenOptions},
    io::{IsTerminal, Read, Write},
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
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
    stderr_reader: Option<ReaderTask>,
    writers: Vec<WriterTask>,
}

struct JobChild {
    child: Child,
    status: JobStatus,
    exit_status: Option<i32>,
}

type ReaderTask = JoinHandle<std::io::Result<Vec<u8>>>;
type WriterTask = JoinHandle<std::io::Result<()>>;

pub struct NativeExecutor {
    jobs: Vec<Job>,
    next_job_id: u32,
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
                terminate_children(&mut job.children, job.state.process_group);
            }
            finish_job_tasks_silently(job);
        }
    }
}

impl NativeExecutor {
    pub fn execute(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
        self.execute_inner(input, false)
    }

    pub fn execute_capture(&mut self, input: &str) -> Result<CommandOutcome, ShellError> {
        self.execute_inner(input, true)
    }

    pub fn jobs(&mut self) -> Vec<JobState> {
        self.refresh_jobs();
        self.jobs.iter().map(|job| job.state.clone()).collect()
    }

    fn execute_inner(&mut self, input: &str, capture: bool) -> Result<CommandOutcome, ShellError> {
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
        if graph.pipelines.is_empty() {
            return Ok(outcome(0, None, None));
        }

        let mut last = outcome(0, None, None);
        for (index, pipeline) in graph.pipelines.iter().enumerate() {
            if index > 0 {
                let connector = graph.connectors[index - 1];
                if (matches!(connector, ListConnector::And) && last.status != 0)
                    || (matches!(connector, ListConnector::Or) && last.status == 0)
                {
                    continue;
                }
            }
            last = self.execute_pipeline(pipeline, input, capture)?;
        }
        Ok(last)
    }

    fn execute_pipeline(
        &mut self,
        pipeline: &Pipeline,
        source: &str,
        capture: bool,
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
            if let Some(result) = self.execute_control_builtin(&pipeline.commands[0], capture)? {
                return Ok(result);
            }
        }
        self.spawn_pipeline(pipeline, source, capture)
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
                CommandRunner::default().execute_capture(&join_command_words(&command.words))?,
            ),
            "ls" => Some(
                CommandRunner::default().execute_capture(&join_command_words(&command.words))?,
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
    ) -> Result<CommandOutcome, ShellError> {
        let mut spawned = SpawnGuard::default();
        let mut previous_reader: Option<PipeReader> = None;
        let mut capture_reader = None;
        let mut builtin_writers: Vec<(PipeWriter, Vec<u8>)> = Vec::new();
        let capture_streams = capture && !pipeline.background;

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
            let stdin = input_stdio(command, previous_reader.take(), index > 0)?;
            let (stdout, next_reader, writer) = output_stdio(command, last, capture_streams)?;
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
                    builtin_writers.push((writer, bytes));
                } else if !capture_streams {
                    io_write_all(std::io::stdout(), &bytes, "standard output")?;
                }
                drop(stdin);
                continue;
            }

            let executable = command
                .words
                .first()
                .map(|word| word.strip_prefix('^').unwrap_or(word))
                .ok_or_else(|| ShellError::new(ErrorCode::InvalidCommand, "empty command stage"))?;
            let mut process = Command::new(executable);
            process.args(command.words.iter().skip(1));
            process
                .stdin(stdin)
                .stdout(stdout)
                .stderr(if capture_streams && last {
                    Stdio::piped()
                } else {
                    Stdio::inherit()
                });
            #[cfg(unix)]
            process.process_group(spawned.process_group.unwrap_or(0));
            let child = process.spawn().map_err(|error| {
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
            spawned.push(child)?;
        }

        let writers = builtin_writers
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
                stderr_reader: None,
                writers,
            });
            return Ok(outcome(
                0,
                Some(format!("[{id}] {}", process_group.unwrap_or_default())),
                capture.then(String::new),
            ));
        }

        let process_group = spawned.process_group;
        let terminal = ForegroundTerminal::give_to(process_group)?;
        let mut children = spawned.release();
        let stdout_reader = capture_reader.map(spawn_reader);
        let child_count = children.len();
        let stderr_reader = if capture {
            children
                .last_mut()
                .and_then(|child| child.child.stderr.take())
                .map(spawn_reader)
        } else {
            None
        };
        let mut wait_error = None;
        for child in &mut children {
            match wait_for_child(&mut child.child) {
                Ok(exit) => child.record(exit),
                Err(error) => {
                    wait_error = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = wait_error {
            terminate_children(&mut children, process_group);
            return Err(error);
        }
        drop(terminal);
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
                stderr_reader,
                writers,
            });
            return Ok(outcome(
                status,
                Some(format!("[{id}] stopped {source}")),
                capture.then(String::new),
            ));
        }
        let stdout = join_reader(stdout_reader, "pipeline output")?;
        let stderr = join_reader(stderr_reader, "command error output")?;
        join_writers(writers)?;
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
            if job
                .children
                .iter()
                .all(|child| child.status == JobStatus::Done)
            {
                job.state.status = JobStatus::Done;
                job.state.exit_status = job.children.last().and_then(|child| child.exit_status);
                finish_job_tasks_silently(job);
            } else if job
                .children
                .iter()
                .filter(|child| child.status != JobStatus::Done)
                .all(|child| child.status == JobStatus::Stopped)
            {
                job.state.status = JobStatus::Stopped;
            } else {
                job.state.status = JobStatus::Running;
            }
        }
    }

    fn foreground(&mut self, id: Option<u32>) -> Result<CommandOutcome, ShellError> {
        self.refresh_jobs();
        let index = select_job(&self.jobs, id)?;
        let terminal = ForegroundTerminal::give_to(self.jobs[index].state.process_group)?;
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
        drop(terminal);
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
        let stdout = join_reader(job.stdout_reader.take(), "pipeline output")?;
        let stderr = join_reader(job.stderr_reader.take(), "command error output")?;
        join_writers(std::mem::take(&mut job.writers))?;
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

fn input_stdio(
    command: &SimpleCommand,
    previous: Option<PipeReader>,
    has_upstream: bool,
) -> Result<Stdio, ShellError> {
    let mut redirected = None;
    for redirect in command
        .redirects
        .iter()
        .filter(|redirect| redirect.kind == RedirectKind::Input)
    {
        redirected = Some(File::open(&redirect.path).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                format!("cannot read redirected input {}", redirect.path),
            )
            .with_context(error.to_string())
            .with_help("Check that the file exists and is readable")
        })?);
    }
    if let Some(file) = redirected {
        return Ok(Stdio::from(file));
    }
    Ok(previous.map_or_else(
        || {
            if has_upstream {
                Stdio::null()
            } else {
                Stdio::inherit()
            }
        },
        Stdio::from,
    ))
}

fn output_stdio(
    command: &SimpleCommand,
    last: bool,
    capture: bool,
) -> Result<(Stdio, Option<PipeReader>, Option<PipeWriter>), ShellError> {
    let mut redirected = None;
    for redirect in command
        .redirects
        .iter()
        .filter(|redirect| matches!(redirect.kind, RedirectKind::Output | RedirectKind::Append))
    {
        redirected = Some(open_redirected_output(redirect)?);
    }
    if let Some(file) = redirected {
        return Ok((Stdio::from(file), None, None));
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
        return Ok((stdout, Some(reader), Some(writer)));
    }
    Ok((Stdio::inherit(), None, None))
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
            if word
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_./-".contains(character))
            {
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
struct SpawnGuard {
    children: Vec<JobChild>,
    process_group: Option<i32>,
}

impl SpawnGuard {
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

impl Drop for SpawnGuard {
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

fn spawn_reader(mut reader: impl Read + Send + 'static) -> ReaderTask {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_reader(reader: Option<ReaderTask>, description: &str) -> Result<String, ShellError> {
    let Some(reader) = reader else {
        return Ok(String::new());
    };
    let bytes = reader
        .join()
        .map_err(|_| {
            ShellError::new(ErrorCode::Io, format!("{description} reader panicked"))
                .with_help("Retry the command; report this if the pipeline is reproducible")
        })?
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, format!("could not read {description}"))
                .with_context(error.to_string())
                .with_help("Retry the command; report this if the pipeline is reproducible")
        })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
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
                    .with_help("Retry the command; report this if the pipeline is reproducible"),
            );
        }
    }
    Ok(())
}

fn finish_job_tasks_silently(job: &mut Job) {
    let _ = join_reader(job.stdout_reader.take(), "pipeline output");
    let _ = join_reader(job.stderr_reader.take(), "command error output");
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
}

impl Drop for ForegroundTerminal {
    fn drop(&mut self) {
        if let Some(group) = self.restore_group {
            if let Ok(_blocked) = BlockedTerminalSignals::new() {
                let _ = tcsetpgrp(std::io::stdin(), group);
                if let Some(modes) = &self.restore_modes {
                    let _ = tcsetattr(std::io::stdin(), SetArg::TCSADRAIN, modes);
                }
            }
        }
    }
}

struct ChildWait {
    status: i32,
    stopped: bool,
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
    fn native_ls_and_external_commands_share_a_byte_pipeline() {
        let mut executor = NativeExecutor::default();
        let result = executor.execute_capture("ls | grep Cargo.toml").unwrap();
        assert_eq!(result.status, 0);
        assert_eq!(result.stdout.as_deref(), Some("Cargo.toml\n"));
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
    fn spawn_guard_kills_and_reaps_children_on_early_errors() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 10");
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().unwrap();
        let pid = Pid::from_raw(i32::try_from(child.id()).unwrap());
        let mut guard = SpawnGuard::default();
        guard.push(child).unwrap();
        drop(guard);
        assert!(kill(pid, None).is_err());
    }
}
