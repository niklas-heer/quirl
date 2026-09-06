//! Private foreground terminal worker and bounded UI/process composition.
//!
//! Prepared arguments use a private socket, never terminal data or shell source.
//! The parent owns expansion and stateful built-ins; the worker owns the native
//! child graph. Every failure unwinds the PTY owner before restoring the editor.

use crate::{QuirlPrompt, SessionEditor};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use quirl_core::{CommandOutcome, ErrorCode, ShellError};
use quirl_process::{
    JobStatus, NativeExecutor, TerminalPipelineRequest,
    pty::{PtyDimensions, PtyRead, PtySession, PtySpawnRequest},
};
use quirl_syntax::Pipeline;
use quirl_ui::{RichSurface, child_terminal::ChildTerminal};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, DirBuilder},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

const ARGUMENT: &str = "--internal-terminal-worker-v1";
const REQUEST_BYTES_MAX: usize = 8 * 1024 * 1024;
const RESPONSE_BYTES_MAX: usize = 384 * 1024;
const INPUT_BYTES_MAX: usize = quirl_ui::child_terminal::CHILD_TERMINAL_INPUT_BYTES_MAX;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    pipeline: Pipeline,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Response {
    Stopped {},
    InputHandoff {},
    Typeahead {
        bytes: Vec<u8>,
    },
    Finished {
        status: i32,
        error: Option<WorkerError>,
    },
}

/// Error text crossing the worker boundary has a separate strict schema and
/// byte budget. Source labels stay in the parent that parsed the command.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerError {
    code: ErrorCode,
    message: String,
    context: Vec<String>,
    help: Vec<String>,
}
impl WorkerError {
    fn from_error(error: ShellError) -> Self {
        fn bounded(text: &str) -> String {
            let end = text.floor_char_boundary(text.len().min(512));
            text.get(..end).unwrap_or_default().to_owned()
        }
        Self {
            code: error.code,
            message: bounded(&error.message),
            context: error
                .details
                .context
                .iter()
                .take(4)
                .map(|text| bounded(text))
                .collect(),
            help: error
                .details
                .help
                .iter()
                .take(4)
                .map(|text| bounded(text))
                .collect(),
        }
    }
    fn into_error(self) -> ShellError {
        if self.message.len() > 512
            || self.context.len() > 4
            || self.help.len() > 4
            || self
                .context
                .iter()
                .chain(&self.help)
                .any(|text| text.len() > 512)
        {
            return protocol_error("terminal worker diagnostic exceeds its field limits");
        }
        let mut error = ShellError::new(self.code, self.message);
        error.details.context = self.context;
        error.details.help = self.help;
        error
    }
}

pub(crate) fn worker_requested() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == ARGUMENT)
}

pub(crate) fn run_worker() -> Result<(), ShellError> {
    let mut args = std::env::args_os().skip(2);
    let path = args
        .next()
        .ok_or_else(|| protocol_error("missing worker socket"))?;
    if args.next().is_some() {
        return Err(protocol_error("unexpected worker argument"));
    }
    let mut socket = UnixStream::connect(path).map_err(io_error)?;
    socket
        .set_read_timeout(Some(STARTUP_TIMEOUT))
        .map_err(io_error)?;
    socket
        .set_write_timeout(Some(STARTUP_TIMEOUT))
        .map_err(io_error)?;
    let request: Request = read_frame(&mut socket, REQUEST_BYTES_MAX)?;
    if request.version != 1 {
        return Err(protocol_error("unsupported worker protocol"));
    }
    let mut executor = NativeExecutor::default();
    let mut result = executor.execute_prepared_terminal_pipeline(request.pipeline);
    // A stopped child remains owned here. The parent presents an explicit
    // resume/cancel state rather than silently dropping an editor's live job.
    while let Some(job) = executor
        .jobs()
        .into_iter()
        .find(|job| job.status == JobStatus::Stopped)
    {
        write_frame(&mut socket, &Response::Stopped {}, RESPONSE_BYTES_MAX)?;
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(io_error)?;
        let mut action = [0u8];
        loop {
            match socket.read_exact(&mut action) {
                Ok(()) => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        result = match action[0] {
            b'r' => executor.execute_interactive(&format!("fg {}", job.id)),
            b'c' => executor.cancel_job(job.id).map(|_| CommandOutcome {
                status: 130,
                stdout: None,
                stderr: None,
            }),
            _ => return Err(protocol_error("invalid stopped-job action")),
        };
    }
    // Quiesce parent writes before reading the slave's unconsumed input. The
    // child graph has completed; recovered bytes are editable text, never code
    // accepted on the worker's authority.
    write_frame(&mut socket, &Response::InputHandoff {}, RESPONSE_BYTES_MAX)?;
    socket.set_nonblocking(true).map_err(io_error)?;
    let mut acknowledgement = [0u8];
    read_bytes_deadline(&mut socket, &mut acknowledgement, Instant::now())?;
    socket.set_nonblocking(false).map_err(io_error)?;
    if acknowledgement != *b"d" {
        return Err(protocol_error("invalid input handoff acknowledgement"));
    }
    let bytes = quirl_process::pty::read_terminal_typeahead()?;
    write_frame(
        &mut socket,
        &Response::Typeahead { bytes },
        RESPONSE_BYTES_MAX,
    )?;
    let response = match result {
        Ok(outcome) => Response::Finished {
            status: outcome.status,
            error: None,
        },
        Err(error) => Response::Finished {
            status: 1,
            error: Some(WorkerError::from_error(error)),
        },
    };
    write_frame(&mut socket, &response, RESPONSE_BYTES_MAX)
}

struct ControlDirectory(PathBuf);
impl Drop for ControlDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("control"));
        let _ = fs::remove_dir(&self.0);
    }
}

fn create_control() -> Result<(ControlDirectory, UnixListener), ShellError> {
    for _ in 0..8 {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(io_error)?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        // /tmp keeps the Unix socket address within macOS's short sun_path.
        let directory = PathBuf::from(format!("/tmp/quirl-pty-{suffix}"));
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error(error)),
        }
        let owner = ControlDirectory(directory);
        let path = owner.0.join("control");
        let listener = UnixListener::bind(&path).map_err(io_error)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(io_error)?;
        listener.set_nonblocking(true).map_err(io_error)?;
        return Ok((owner, listener));
    }
    Err(protocol_error("could not allocate a private worker socket"))
}

fn connect_worker(
    listener: &UnixListener,
    session: &mut PtySession,
    request: &TerminalPipelineRequest<'_>,
    started: Instant,
) -> Result<UnixStream, ShellError> {
    loop {
        ensure_active(request)?;
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_write_timeout(Some(STARTUP_TIMEOUT))
                    .map_err(io_error)?;
                return Ok(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(io_error(error)),
        }
        if started.elapsed() >= STARTUP_TIMEOUT || session.exit_status()?.is_some() {
            return Err(protocol_error(
                "terminal worker did not connect within 2000 ms",
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

pub(crate) fn execute(
    editor: &mut SessionEditor,
    request: TerminalPipelineRequest<'_>,
    prompt: &QuirlPrompt,
) -> Result<CommandOutcome, ShellError> {
    let SessionEditor::Rich(editor) = editor else {
        return Err(protocol_error(
            "terminal execution requires the rich surface",
        ));
    };
    let size = editor.begin_embedded_terminal()?;
    let mut terminal = match ChildTerminal::new(size) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = editor.finish_embedded_terminal(Vec::new(), prompt);
            return Err(error);
        }
    };
    let result =
        execute_owned(editor, &request, &mut terminal, size, prompt).and_then(|(status, bytes)| {
            quirl_ui::child_terminal::recover_terminal_input(&bytes)
                .map(|recovered| (status, recovered))
        });
    let (mut snapshot, snapshot_error) = match terminal.finish_snapshot() {
        Ok(snapshot) => (snapshot, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    if result
        .as_ref()
        .is_ok_and(|(_, recovered)| recovered.omitted_controls)
    {
        snapshot.push("Quirl omitted terminal controls from recovered typing; review the next command before pressing Enter.".to_owned());
    }
    let restored = editor.finish_embedded_terminal(snapshot, prompt);
    match (result, snapshot_error, restored) {
        (Ok((status, recovered)), None, Ok(())) => {
            if !recovered.text.is_empty() {
                // A recovered line is offered for review, never auto-submitted.
                // Controls remain inert inside the editor's text model.
                editor.append_recovered_input(&recovered.text)?;
            }
            Ok(CommandOutcome {
                status,
                stdout: None,
                stderr: None,
            })
        }
        (Err(error), _, _) | (_, Some(error), _) | (_, _, Err(error)) => Err(error),
    }
}

fn execute_owned(
    editor: &mut RichSurface,
    request: &TerminalPipelineRequest<'_>,
    terminal: &mut ChildTerminal,
    size: quirl_ui::child_terminal::ChildTerminalSize,
    prompt: &QuirlPrompt,
) -> Result<(i32, Vec<u8>), ShellError> {
    ensure_active(request)?;
    editor.tick_command_stream(Duration::ZERO, prompt)?;
    let startup_started = Instant::now();
    let (directory, listener) = create_control()?;
    let mut environment = request.environment.to_vec();
    environment.retain(|(name, _)| name != "TERM" && name != "TERM_PROGRAM");
    environment.push(("TERM".into(), "xterm-256color".into()));
    environment.push(("TERM_PROGRAM".into(), "quirl".into()));
    let mut session = PtySession::spawn(PtySpawnRequest {
        executable: std::env::current_exe().map_err(io_error)?,
        arguments: vec![
            ARGUMENT.into(),
            directory.0.join("control").into_os_string(),
        ],
        environment,
        cwd: std::env::current_dir().map_err(io_error)?,
        size: PtyDimensions {
            rows: size.rows,
            columns: size.columns,
        },
    })?;
    let mut socket = connect_worker(&listener, &mut session, request, startup_started)?;
    let mut pipeline = request.pipeline.clone();
    for command in &mut pipeline.commands {
        command.word_ir.clear();
        for redirect in &mut command.redirects {
            redirect.target.parts.clear();
        }
    }
    write_frame_deadline(
        &mut socket,
        &Request {
            version: 1,
            pipeline,
        },
        REQUEST_BYTES_MAX,
        startup_started,
        Some(request),
    )?;
    socket.set_nonblocking(true).map_err(io_error)?;
    let result = terminal_loop(editor, request, terminal, &mut session, &mut socket, prompt);
    let cleanup = session.finish();
    match (result, cleanup) {
        (Ok(status), Ok(_)) => Ok(status),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn terminal_loop(
    editor: &mut RichSurface,
    request: &TerminalPipelineRequest<'_>,
    terminal: &mut ChildTerminal,
    session: &mut PtySession,
    socket: &mut UnixStream,
    prompt: &QuirlPrompt,
) -> Result<(i32, Vec<u8>), ShellError> {
    let mut input = VecDeque::new();
    let mut replies = VecDeque::new();
    let mut response = FrameReader::default();
    let mut finished = None;
    let mut stopped = false;
    let mut input_handoff = false;
    let mut handoff_started = None;
    let mut recovered_input = None;
    let mut exit_seen = None;
    let mut completion_seen = None;
    let mut bytes = [0u8; 8192];
    let mut size = terminal.size();
    let mut dirty = true;
    let mut has_output = false;
    let started = Instant::now();
    let mut last_tick = started;
    let mut last_draw = started;
    // Keep ownership transitions in this one bounded loop: output, control,
    // exit observation, rendering, input, and partial writes share one state.
    loop {
        ensure_active(request)?;
        // SIGWINCH notifications may coalesce or be consumed by another event
        // observer. The kernel dimensions are the authoritative resize state.
        let measured = RichSurface::embedded_terminal_size()?;
        if measured != size {
            terminal.resize(measured)?;
            session.resize(PtyDimensions {
                rows: measured.rows,
                columns: measured.columns,
            })?;
            size = measured;
            dirty = true;
        }
        let mut closed = false;
        let mut read_output = false;
        for _ in 0..8 {
            match session.read_output(&mut bytes)? {
                PtyRead::Bytes(count) => {
                    read_output |= count > 0;
                    has_output |= count > 0;
                    let generated = terminal.process(
                        bytes
                            .get(..count)
                            .ok_or_else(|| protocol_error("invalid PTY read count"))?,
                    )?;
                    if !input_handoff {
                        enqueue_with_other(&mut replies, input.len(), &generated)?;
                    }
                    dirty = true;
                }
                PtyRead::Pending => break,
                PtyRead::Closed => {
                    closed = true;
                    break;
                }
            }
        }
        for _ in 0..16 {
            if let Some(message) = response.poll(socket)? {
                if finished.is_some() {
                    return Err(protocol_error(
                        "unexpected response after terminal completion",
                    ));
                }
                match message {
                    Response::Stopped {} => {
                        if input_handoff || stopped {
                            return Err(protocol_error("unexpected suspended terminal response"));
                        }
                        stopped = true;
                        dirty = true;
                    }
                    Response::InputHandoff {} => {
                        if input_handoff {
                            return Err(protocol_error("duplicate input handoff"));
                        }
                        input_handoff = true;
                        handoff_started = Some(Instant::now());
                        replies.clear();
                        socket.write_all(b"d").map_err(io_error)?;
                    }
                    Response::Typeahead { mut bytes } => {
                        if !input_handoff || recovered_input.is_some() {
                            return Err(protocol_error("unexpected recovered input"));
                        }
                        let observed = bytes.len().saturating_add(input.len());
                        if observed > 64 * 1024 {
                            return Err(frame_limit(64 * 1024, observed));
                        }
                        bytes.extend(std::mem::take(&mut input));
                        recovered_input = Some(bytes);
                    }
                    Response::Finished { status, error } => {
                        let typeahead = recovered_input
                            .take()
                            .ok_or_else(|| protocol_error("missing input handoff result"))?;
                        finished = Some(
                            error.map_or(Ok((status, typeahead)), |error| Err(error.into_error())),
                        );
                        completion_seen = Some(Instant::now());
                    }
                }
            }
        }
        if response.closed && finished.is_none() {
            return Err(protocol_error(
                "terminal control channel closed without a result",
            ));
        }
        let exited = session.exit_status()?.is_some();
        if finished.is_none()
            && handoff_started.is_some_and(|time| time.elapsed() >= STARTUP_TIMEOUT)
        {
            return Err(protocol_error(
                "terminal worker did not return unread input within 2000 ms",
            ));
        }
        if !exited && completion_seen.is_some_and(|time| time.elapsed() >= STARTUP_TIMEOUT) {
            return Err(protocol_error(
                "terminal worker did not finish cleanup within 2000 ms",
            ));
        }
        if exited && exit_seen.is_none() {
            exit_seen = Some(Instant::now());
        }
        if exited && closed && finished.is_some() {
            return finished.unwrap_or_else(|| {
                Err(protocol_error(
                    "terminal worker exited without a complete result",
                ))
            });
        }
        if exited && exit_seen.is_some_and(|time| time.elapsed() >= DRAIN_TIMEOUT) {
            return Err(protocol_error(
                "terminal output or control result did not close within 2000 ms after worker exit",
            ));
        }
        let notice = stopped.then_some("Program suspended · Enter resumes · Ctrl-C cancels");
        if has_output || stopped {
            if dirty && (stopped || closed || last_draw.elapsed() >= Duration::from_millis(16)) {
                editor.draw_embedded_terminal(terminal, notice)?;
                last_draw = Instant::now();
                dirty = false;
            }
        } else if dirty || last_tick.elapsed() >= Duration::from_millis(100) {
            editor.tick_command_stream(started.elapsed(), prompt)?;
            last_tick = Instant::now();
            dirty = false;
        }
        let input_wait = if read_output {
            Duration::ZERO
        } else {
            Duration::from_millis(20)
        };
        if !input_handoff
            && finished.is_none()
            && let Some(event) = RichSurface::poll_embedded_terminal_event(input_wait)?
        {
            match event {
                Event::Resize(_, _) => {
                    dirty = true;
                }
                Event::Key(key) if stopped && key.kind != KeyEventKind::Release => {
                    let action = match (key.code, key.modifiers) {
                        (KeyCode::Enter, _) => Some(b'r'),
                        (KeyCode::Char('c'), modifiers)
                            if modifiers.contains(KeyModifiers::CONTROL) =>
                        {
                            Some(b'c')
                        }
                        _ => None,
                    };
                    if let Some(action) = action {
                        socket.write_all(&[action]).map_err(io_error)?;
                        stopped = false;
                        dirty = true;
                    }
                }
                event if !stopped => {
                    enqueue_with_other(&mut input, replies.len(), &terminal.encode_input(&event)?)?;
                }
                _ => {}
            }
        }
        if input_handoff || finished.is_some() {
            std::thread::sleep(Duration::from_millis(2));
        }
        if !input_handoff && finished.is_none() {
            // Local replies are never recovered as user text. Prioritize them
            // so a terminal query cannot deadlock behind a full pasted line.
            let pending = if replies.is_empty() {
                &mut input
            } else {
                &mut replies
            };
            if pending.is_empty() {
                continue;
            }
            let contiguous = pending.make_contiguous();
            let count = contiguous
                .len()
                .min(quirl_process::pty::PTY_INPUT_TURN_BYTES_MAX);
            let written = session.write_input(
                contiguous
                    .get(..count)
                    .ok_or_else(|| protocol_error("invalid input queue bound"))?,
            )?;
            pending.drain(..written);
        }
    }
}

fn enqueue_with_other(
    queue: &mut VecDeque<u8>,
    other_bytes: usize,
    bytes: &[u8],
) -> Result<(), ShellError> {
    let observed = queue
        .len()
        .saturating_add(other_bytes)
        .saturating_add(bytes.len());
    if observed > INPUT_BYTES_MAX {
        return Err(frame_limit(INPUT_BYTES_MAX, observed));
    }
    enqueue(queue, bytes)
}

fn enqueue(queue: &mut VecDeque<u8>, bytes: &[u8]) -> Result<(), ShellError> {
    let observed = queue.len().saturating_add(bytes.len());
    if observed > INPUT_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "child terminal input queue is full",
        )
        .with_context(format!(
            "limit {INPUT_BYTES_MAX} bytes; observed {observed} bytes"
        ))
        .with_help("Paste smaller chunks or wait until the program reads input"));
    }
    queue.extend(bytes);
    Ok(())
}

fn ensure_active(request: &TerminalPipelineRequest<'_>) -> Result<(), ShellError> {
    if request
        .cancelled
        .is_some_and(|flag| flag.load(Ordering::Relaxed))
        || request
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "terminal execution was cancelled or exceeded its deadline",
        )
        .with_help("Retry the command if it is still needed"));
    }
    Ok(())
}

struct BoundedFrame {
    bytes: Vec<u8>,
    limit: usize,
    observed: usize,
}
impl Write for BoundedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.observed = self.bytes.len().saturating_add(bytes.len());
        if self.observed > self.limit {
            return Err(std::io::Error::other("frame byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn frame_limit(limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "terminal worker frame exceeds its byte limit",
    )
    .with_context(format!("limit {limit} bytes; observed {observed} bytes"))
    .with_help("Split the command into smaller pipelines")
}

fn write_frame(
    socket: &mut UnixStream,
    value: &impl Serialize,
    limit: usize,
) -> Result<(), ShellError> {
    write_frame_deadline(socket, value, limit, Instant::now(), None)
}

fn write_frame_deadline(
    socket: &mut UnixStream,
    value: &impl Serialize,
    limit: usize,
    started: Instant,
    request: Option<&TerminalPipelineRequest<'_>>,
) -> Result<(), ShellError> {
    let mut buffer = BoundedFrame {
        bytes: Vec::new(),
        limit,
        observed: 0,
    };
    let serialized = serde_json::to_writer(&mut buffer, value);
    if buffer.observed > limit {
        return Err(frame_limit(limit, buffer.observed));
    }
    serialized.map_err(io_error)?;
    let length = u32::try_from(buffer.bytes.len()).map_err(io_error)?;
    socket.set_nonblocking(true).map_err(io_error)?;
    let result = write_bytes_deadline(socket, &length.to_be_bytes(), started, request)
        .and_then(|()| write_bytes_deadline(socket, &buffer.bytes, started, request));
    let restored = socket.set_nonblocking(false).map_err(io_error);
    result.and(restored)
}

fn check_transfer_deadline(
    started: Instant,
    request: Option<&TerminalPipelineRequest<'_>>,
) -> Result<(), ShellError> {
    if let Some(request) = request {
        ensure_active(request)?;
    }
    if started.elapsed() >= STARTUP_TIMEOUT {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "terminal worker transfer exceeded 2000 ms",
        )
        .with_help("Retry the command after reducing system load"));
    }
    Ok(())
}

fn write_bytes_deadline(
    socket: &mut UnixStream,
    mut bytes: &[u8],
    started: Instant,
    request: Option<&TerminalPipelineRequest<'_>>,
) -> Result<(), ShellError> {
    while !bytes.is_empty() {
        check_transfer_deadline(started, request)?;
        let chunk = bytes
            .get(..bytes.len().min(8192))
            .ok_or_else(|| protocol_error("invalid transfer bound"))?;
        match socket.write(chunk) {
            Ok(0) => return Err(protocol_error("terminal control write closed")),
            Ok(count) => {
                bytes = bytes
                    .get(count..)
                    .ok_or_else(|| protocol_error("invalid transfer count"))?
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

fn read_frame<T: serde::de::DeserializeOwned>(
    socket: &mut UnixStream,
    limit: usize,
) -> Result<T, ShellError> {
    let started = Instant::now();
    socket.set_nonblocking(true).map_err(io_error)?;
    let result = (|| {
        let mut prefix = [0u8; 4];
        read_bytes_deadline(socket, &mut prefix, started)?;
        let length = usize::try_from(u32::from_be_bytes(prefix)).map_err(io_error)?;
        if length > limit {
            return Err(frame_limit(limit, length));
        }
        let mut bytes = vec![0u8; length];
        read_bytes_deadline(socket, &mut bytes, started)?;
        serde_json::from_slice(&bytes).map_err(io_error)
    })();
    let restored = socket.set_nonblocking(false).map_err(io_error);
    match (result, restored) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn read_bytes_deadline(
    socket: &mut UnixStream,
    bytes: &mut [u8],
    started: Instant,
) -> Result<(), ShellError> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        check_transfer_deadline(started, None)?;
        let end = offset.saturating_add(8192).min(bytes.len());
        let chunk = bytes
            .get_mut(offset..end)
            .ok_or_else(|| protocol_error("invalid receive bound"))?;
        match socket.read(chunk) {
            Ok(0) => return Err(protocol_error("truncated terminal control frame")),
            Ok(count) => offset = offset.saturating_add(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2))
            }
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

#[derive(Default)]
struct FrameReader {
    bytes: Vec<u8>,
    length: Option<usize>,
    closed: bool,
}
impl FrameReader {
    fn poll(&mut self, socket: &mut UnixStream) -> Result<Option<Response>, ShellError> {
        let needed = self.length.map_or(4, |length| length.saturating_add(4));
        let mut chunk = [0u8; 8192];
        let read_max = needed.saturating_sub(self.bytes.len()).min(chunk.len());
        match socket.read(
            chunk
                .get_mut(..read_max)
                .ok_or_else(|| protocol_error("invalid control read bound"))?,
        ) {
            Ok(0) => {
                self.closed = true;
                if !self.bytes.is_empty() {
                    return Err(protocol_error("truncated worker response"));
                }
                return Ok(None);
            }
            Ok(count) => self.bytes.extend_from_slice(
                chunk
                    .get(..count)
                    .ok_or_else(|| protocol_error("invalid control read count"))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(io_error(error)),
        }
        if self.length.is_none() && self.bytes.len() == 4 {
            let prefix: [u8; 4] = self
                .bytes
                .get(..4)
                .ok_or_else(|| protocol_error("missing frame prefix"))?
                .try_into()
                .map_err(io_error)?;
            let length = usize::try_from(u32::from_be_bytes(prefix)).map_err(io_error)?;
            if length == 0 {
                return Err(protocol_error("invalid worker response length"));
            }
            if length > RESPONSE_BYTES_MAX {
                return Err(frame_limit(RESPONSE_BYTES_MAX, length));
            }
            self.length = Some(length);
        }
        if self
            .length
            .is_some_and(|length| self.bytes.len() == length.saturating_add(4))
        {
            let response: Response = serde_json::from_slice(
                self.bytes
                    .get(4..)
                    .ok_or_else(|| protocol_error("missing frame body"))?,
            )
            .map_err(io_error)?;
            if matches!(&response, Response::Finished { status, .. } if !(0..=255).contains(status))
            {
                return Err(protocol_error(
                    "terminal worker returned an invalid exit status",
                ));
            }
            self.bytes.clear();
            self.length = None;
            return Ok(Some(response));
        }
        Ok(None)
    }
}

fn protocol_error(message: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help(
        "Retry using a matching Quirl executable; report repeated terminal-worker failures",
    )
}
fn io_error(error: impl std::fmt::Display) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not operate the terminal worker")
        .with_context(error.to_string())
        .with_help("Retry the command; report repeated terminal I/O failures")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_pair() -> (UnixStream, UnixStream) {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        (reader, writer)
    }

    #[test]
    fn response_reader_accepts_fragmented_header_and_payload_without_early_publication() {
        let (mut socket, mut writer) = response_pair();
        let body = serde_json::to_vec(&Response::Finished {
            status: 7,
            error: None,
        })
        .unwrap();
        let prefix = u32::try_from(body.len()).unwrap().to_be_bytes();
        let mut reader = FrameReader::default();
        assert!(reader.poll(&mut socket).unwrap().is_none());
        for byte in prefix {
            writer.write_all(&[byte]).unwrap();
            assert!(reader.poll(&mut socket).unwrap().is_none());
        }
        for (index, byte) in body.iter().enumerate() {
            writer.write_all(&[*byte]).unwrap();
            let result = reader.poll(&mut socket).unwrap();
            if index.saturating_add(1) == body.len() {
                assert!(matches!(
                    result,
                    Some(Response::Finished {
                        status: 7,
                        error: None
                    })
                ));
            } else {
                assert!(result.is_none());
            }
        }
        assert!(reader.bytes.is_empty());
        assert!(reader.length.is_none());
    }

    #[test]
    fn queued_worker_result_remains_readable_after_writer_exit_and_prefix_only_poll() {
        // The socket may already contain the complete result when the PTY
        // reports exit. A prefix-only poll must not be mistaken for no result.
        let (mut socket, mut writer) = response_pair();
        write_frame(
            &mut writer,
            &Response::Finished {
                status: 0,
                error: None,
            },
            RESPONSE_BYTES_MAX,
        )
        .unwrap();
        drop(writer);
        let mut reader = FrameReader::default();
        assert!(reader.poll(&mut socket).unwrap().is_none());
        assert!(matches!(
            reader.poll(&mut socket).unwrap(),
            Some(Response::Finished {
                status: 0,
                error: None
            })
        ));
        assert!(reader.poll(&mut socket).unwrap().is_none());
    }

    #[test]
    fn response_reader_reuses_framing_for_stopped_then_finished_messages() {
        let (mut socket, mut writer) = response_pair();
        write_frame(&mut writer, &Response::Stopped {}, RESPONSE_BYTES_MAX).unwrap();
        write_frame(
            &mut writer,
            &Response::Finished {
                status: 130,
                error: None,
            },
            RESPONSE_BYTES_MAX,
        )
        .unwrap();
        let mut reader = FrameReader::default();
        assert!(reader.poll(&mut socket).unwrap().is_none());
        assert!(matches!(
            reader.poll(&mut socket).unwrap(),
            Some(Response::Stopped {})
        ));
        assert!(reader.poll(&mut socket).unwrap().is_none());
        assert!(matches!(
            reader.poll(&mut socket).unwrap(),
            Some(Response::Finished {
                status: 130,
                error: None
            })
        ));
    }

    #[test]
    fn response_reader_rejects_truncation_and_oversized_prefix_without_waiting_for_body() {
        for bytes in [vec![0, 0], vec![0, 0, 0, 3, b'{']] {
            let (mut socket, mut writer) = response_pair();
            writer.write_all(&bytes).unwrap();
            drop(writer);
            let mut reader = FrameReader::default();
            let mut failed = false;
            for _ in 0..3 {
                if let Err(error) = reader.poll(&mut socket) {
                    assert!(error.message.contains("truncated"));
                    failed = true;
                    break;
                }
            }
            assert!(failed);
        }
        for length in [0, RESPONSE_BYTES_MAX.saturating_add(1)] {
            let (mut socket, mut writer) = response_pair();
            writer
                .write_all(&u32::try_from(length).unwrap().to_be_bytes())
                .unwrap();
            let mut reader = FrameReader::default();
            assert!(reader.poll(&mut socket).is_err());
            assert_eq!(reader.bytes.len(), 4);
            assert!(reader.length.is_none());
        }
    }

    #[test]
    fn response_reader_admits_exact_payload_limit_and_rejects_unknown_fields() {
        let (mut socket, mut writer) = response_pair();
        let mut body = serde_json::to_vec(&Response::Stopped {}).unwrap();
        body.resize(RESPONSE_BYTES_MAX, b' ');
        writer
            .write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
            .unwrap();
        let mut reader = FrameReader::default();
        assert!(reader.poll(&mut socket).unwrap().is_none());
        let mut admitted = false;
        for chunk in body.chunks(8_192) {
            writer.write_all(chunk).unwrap();
            if matches!(
                reader.poll(&mut socket).unwrap(),
                Some(Response::Stopped {})
            ) {
                admitted = true;
            }
        }
        assert!(admitted);
        write_frame(
            &mut writer,
            &serde_json::json!({"kind":"Stopped","unexpected":true}),
            RESPONSE_BYTES_MAX,
        )
        .unwrap();
        assert!(reader.poll(&mut socket).unwrap().is_none());
        assert!(reader.poll(&mut socket).is_err());
    }

    #[test]
    fn input_queue_rejects_the_first_excess_byte_without_changing_admitted_input() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, &vec![b'x'; INPUT_BYTES_MAX]).unwrap();
        assert_eq!(queue.len(), INPUT_BYTES_MAX);
        let error = enqueue(&mut queue, b"!").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|context| context.contains(&INPUT_BYTES_MAX.saturating_add(1).to_string()))
        );
        assert_eq!(queue.len(), INPUT_BYTES_MAX);
        assert!(queue.iter().all(|byte| *byte == b'x'));
        queue.pop_front();
        enqueue(&mut queue, b"!").unwrap();
        assert_eq!(queue.back(), Some(&b'!'));
    }

    #[test]
    fn oversized_worker_write_publishes_no_partial_frame() {
        let (mut socket, mut writer) = response_pair();
        assert!(write_frame(&mut writer, &Response::Stopped {}, 1).is_err());
        let mut byte = [0u8];
        assert_eq!(
            socket.read(&mut byte).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        write_frame(&mut writer, &Response::Stopped {}, RESPONSE_BYTES_MAX).unwrap();
        let mut reader = FrameReader::default();
        assert!(reader.poll(&mut socket).unwrap().is_none());
        assert!(matches!(
            reader.poll(&mut socket).unwrap(),
            Some(Response::Stopped {})
        ));
    }

    #[test]
    fn private_control_socket_permissions_and_owner_cleanup_are_exact() {
        let (owner, listener) = create_control().unwrap();
        let directory = owner.0.clone();
        let socket = directory.join("control");
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(listener);
        drop(owner);
        assert!(!socket.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn expired_or_cancelled_startup_publishes_no_frame_prefix() {
        use std::sync::atomic::AtomicBool;
        let parsed = quirl_syntax::parse_command_list("true").unwrap();
        let cancelled = AtomicBool::new(true);
        let request = TerminalPipelineRequest {
            pipeline: &parsed.pipelines[0],
            environment: &[],
            deadline: None,
            cancelled: Some(&cancelled),
        };
        for (started, request) in [
            (Instant::now().checked_sub(STARTUP_TIMEOUT).unwrap(), None),
            (Instant::now(), Some(&request)),
        ] {
            let (mut reader, mut writer) = response_pair();
            let error = write_frame_deadline(
                &mut writer,
                &Response::Stopped {},
                RESPONSE_BYTES_MAX,
                started,
                request,
            )
            .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            let mut prefix = [0_u8; 4];
            assert_eq!(
                reader.read(&mut prefix).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
    }

    // Shutdown wakes the nonblocking receive loop even if an assertion or the
    // broad watchdog fails. Join occurs only after the peer has been unblocked;
    // EOF cannot make the successful deadline assertion pass prematurely.
    struct TransferFixture {
        socket: UnixStream,
        worker: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for TransferFixture {
        fn drop(&mut self) {
            let _ = self.socket.shutdown(std::net::Shutdown::Both);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    #[test]
    fn partially_received_transfer_expires_while_sender_remains_open() {
        let (socket, mut receiver) = UnixStream::pair().unwrap();
        receiver.set_nonblocking(true).unwrap();
        let (progress, observed) = std::sync::mpsc::sync_channel(1);
        let (completed, result) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            let mut first = [0_u8; 1];
            let outcome = read_bytes_deadline(&mut receiver, &mut first, started).and_then(|()| {
                let _ = progress.send(first[0]);
                read_bytes_deadline(&mut receiver, &mut [0_u8; 1], started)
            });
            let _ = completed.send(outcome);
        });
        let mut fixture = TransferFixture {
            socket,
            worker: Some(worker),
        };
        fixture
            .socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        fixture.socket.write_all(b"a").unwrap();
        let progress = observed.recv_timeout(Duration::from_secs(5));
        // Keep the writer alive and send no second byte. This exercises a real
        // stalled transfer, with a broad watchdog rather than a latency oracle.
        let outcome = result.recv_timeout(Duration::from_secs(5));
        drop(fixture);
        assert_eq!(progress.unwrap(), b'a');
        let error = outcome.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("transfer exceeded 2000 ms"));
    }

    #[test]
    fn expired_receive_does_not_consume_already_queued_bytes() {
        let (mut reader, mut writer) = response_pair();
        writer.write_all(b"queued").unwrap();
        let mut buffer = [0_u8; 6];
        let started = Instant::now().checked_sub(STARTUP_TIMEOUT).unwrap();
        let error = read_bytes_deadline(&mut reader, &mut buffer, started).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(buffer, [0; 6]);
        reader.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"queued");
    }

    fn framed_response(value: &serde_json::Value) -> Result<Response, ShellError> {
        let (mut reader, mut writer) = response_pair();
        write_frame(&mut writer, value, RESPONSE_BYTES_MAX)?;
        let mut frames = FrameReader::default();
        for _ in 0..16 {
            if let Some(response) = frames.poll(&mut reader)? {
                return Ok(response);
            }
        }
        Err(protocol_error("fixture response did not complete"))
    }

    fn diagnostic_response(error: &WorkerError) -> serde_json::Value {
        serde_json::json!({ "kind": "Finished", "status": 1, "error": error })
    }

    fn diagnostic_fixture() -> WorkerError {
        WorkerError {
            code: ErrorCode::Io,
            message: "x".repeat(512),
            context: vec!["c".repeat(512); 4],
            help: vec!["h".repeat(512); 4],
        }
    }

    #[test]
    fn strict_nested_worker_error_accepts_exact_limits_and_rejects_each_excess() {
        let exact = diagnostic_response(&diagnostic_fixture());
        let Response::Finished {
            error: Some(error), ..
        } = framed_response(&exact).unwrap()
        else {
            panic!("expected the framed diagnostic");
        };
        let error = error.into_error();
        assert_eq!(error.code, ErrorCode::Io);
        assert_eq!(error.message.len(), 512);
        assert_eq!(error.details.context.len(), 4);
        assert_eq!(error.details.help.len(), 4);
        for field in [
            "message",
            "context_count",
            "help_count",
            "context_bytes",
            "help_bytes",
        ] {
            let mut invalid = diagnostic_fixture();
            match field {
                "message" => invalid.message.push('!'),
                "context_count" => invalid.context.push(String::new()),
                "help_count" => invalid.help.push(String::new()),
                "context_bytes" => invalid.context[0].push('!'),
                "help_bytes" => invalid.help[0].push('!'),
                _ => panic!("unrecognized diagnostic limit fixture: {field}"),
            }
            let response = framed_response(&diagnostic_response(&invalid)).unwrap();
            let Response::Finished {
                error: Some(error), ..
            } = response
            else {
                panic!("expected the framed diagnostic for {field}");
            };
            let error = error.into_error();
            assert_eq!(error.code, ErrorCode::Validation, "{field}");
            assert!(
                error
                    .message
                    .contains("diagnostic exceeds its field limits"),
                "{field}"
            );
        }
    }

    #[test]
    fn nested_worker_diagnostic_rejects_unknown_fields_at_the_frame_boundary() {
        let mut value = diagnostic_response(&diagnostic_fixture());
        value["error"]["labels"] = serde_json::json!([]);
        let error = framed_response(&value)
            .err()
            .expect("unknown nested fields must fail");
        assert!(
            error
                .details
                .context
                .iter()
                .any(|line| line.contains("unknown field"))
        );
        value["error"].as_object_mut().unwrap().remove("labels");
        value["error"]["code"] = serde_json::json!("not_a_real_error_code");
        assert!(framed_response(&value).is_err());
    }

    #[test]
    fn outbound_worker_diagnostic_truncates_utf8_and_collections_before_framing() {
        let mut original = ShellError::new(ErrorCode::Io, "é".repeat(257));
        original.details.context = vec!["界".repeat(180); 5];
        original.details.help = vec!["🦀".repeat(130); 5];
        let bounded = WorkerError::from_error(original);
        assert_eq!(bounded.message.len(), 512);
        assert_eq!(bounded.context.len(), 4);
        assert!(bounded.context.iter().all(|text| text.len() == 510));
        assert_eq!(bounded.help.len(), 4);
        assert!(bounded.help.iter().all(|text| text.len() == 512));
        let Response::Finished {
            error: Some(error), ..
        } = framed_response(&diagnostic_response(&bounded)).unwrap()
        else {
            panic!("expected bounded diagnostic round trip");
        };
        assert_eq!(error.into_error().code, ErrorCode::Io);
    }

    #[test]
    fn maximum_bracketed_paste_survives_bounded_partial_queue_writes_in_order() {
        use quirl_ui::child_terminal::{CHILD_TERMINAL_OUTPUT_BYTES_MAX, ChildTerminalSize};
        let mut terminal = ChildTerminal::new(ChildTerminalSize {
            rows: 2,
            columns: 10,
        })
        .unwrap();
        terminal.process(b"\x1b[?2004h").unwrap();
        let paste = terminal
            .encode_input(&Event::Paste("x".repeat(CHILD_TERMINAL_OUTPUT_BYTES_MAX)))
            .unwrap();
        assert_eq!(paste.len(), INPUT_BYTES_MAX);
        assert!(paste.starts_with(b"\x1b[200~"));
        assert!(paste.ends_with(b"\x1b[201~"));
        let mut queue = VecDeque::new();
        enqueue(&mut queue, &paste).unwrap();
        let mut written = Vec::new();
        let mut offers = Vec::new();
        // Simulate one temporarily blocked write, then a short write, followed
        // by full accepted prefixes. No byte may be dropped or reordered.
        for accepted_max in [
            0,
            3,
            quirl_process::pty::PTY_INPUT_TURN_BYTES_MAX,
            quirl_process::pty::PTY_INPUT_TURN_BYTES_MAX,
        ] {
            let contiguous = queue.make_contiguous();
            let offered = contiguous
                .len()
                .min(quirl_process::pty::PTY_INPUT_TURN_BYTES_MAX);
            offers.push(offered);
            let accepted = offered.min(accepted_max);
            written.extend_from_slice(&contiguous[..accepted]);
            queue.drain(..accepted);
        }
        assert!(offers.iter().all(|offered| *offered <= 64 * 1024));
        assert_eq!(offers.last(), Some(&9));
        assert!(queue.is_empty());
        assert_eq!(written, paste);
    }
}
