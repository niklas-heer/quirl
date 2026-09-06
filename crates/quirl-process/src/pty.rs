//! Bounded Unix PTY I/O and ownership for one explicitly launched executable.
//!
//! This module owns no shell grammar, VT parser, input queue, or shell-state
//! protocol. The host supplies a validated worker executable and decides when
//! to cancel it. Each I/O call performs one nonblocking operation; a live
//! interactive session has no implicit wall deadline. The host must bound its
//! event turns and post-exit output drain. Closing or cancelling a session
//! terminates its owned process group and reaps its direct child.
//!
//! A process group does not contain descendants which deliberately create new
//! sessions or groups. A worker using `NativeExecutor` retains that executor's
//! separate group anchors: worker death closes their keepalives. This module
//! never signals a numeric process group after releasing the direct child's PID.

use filedescriptor::FileDescriptor;
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use quirl_core::{ErrorCode, ShellError};
use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::fd::{AsRawFd, RawFd},
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

/// Maximum bytes read from the PTY in one nonblocking event turn.
pub const PTY_OUTPUT_TURN_BYTES_MAX: usize = 8 * 1024;
/// Maximum bytes admitted to one nonblocking input write; no queue is retained.
pub const PTY_INPUT_TURN_BYTES_MAX: usize = 64 * 1024;
/// Maximum unread terminal input recovered after the host pauses forwarding.
pub const PTY_TYPEAHEAD_BYTES_MAX: usize = 64 * 1024;
const TYPEAHEAD_READS_MAX: usize = 128;
/// Maximum simultaneously owned sessions, including children awaiting reaping.
pub const PTY_SESSIONS_MAX: usize = 32;
/// Maximum argument count admitted before PTY creation or child spawn.
pub const PTY_ARGUMENTS_MAX: usize = 4096;
/// Maximum aggregate executable, argument, and cwd bytes admitted before spawn.
pub const PTY_ARGUMENT_BYTES_MAX: usize = crate::NATIVE_COMMAND_BYTES_MAX;
/// Maximum physical rows accepted by the process-owned PTY.
pub const PTY_ROWS_MAX: u16 = 500;
/// Maximum physical columns accepted by the process-owned PTY.
pub const PTY_COLUMNS_MAX: u16 = 1000;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_POLL: Duration = Duration::from_millis(1);
#[cfg(any(target_os = "macos", test))]
const GROUP_OBSERVATION_BYTES_MAX: usize = 1024 * 1024;

// Failure model: spawn can fail after PTY allocation; either endpoint can
// backpressure, close, or flood; the child can exit before its first observer,
// leave descendants, stop, or delay kernel reaping after SIGKILL. Admit all
// growth before allocation, own every descriptor immediately, and never use
// portable-pty's writer (its Drop performs a potentially blocking EOF write).
// Keep one direct child PID reserved with WNOWAIT through group signalling.
// Close both master handles before cleanup waits. At most 32 admission permits
// retain active/deferred owners. One prestarted reaper, with 64 slots, can hold
// each owner's main child plus its fixed macOS verification probe; it cannot
// become an unbounded thread-per-cancel fallback. No callbacks run in cleanup.
static ACTIVE_SESSIONS: AtomicUsize = AtomicUsize::new(0);
static REAPER: OnceLock<Result<SyncSender<DeferredChild>, String>> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Terminal dimensions in character cells, validated before allocation or resize.
pub struct PtyDimensions {
    /// Visible rows; must be between 1 and [`PTY_ROWS_MAX`].
    pub rows: u16,
    /// Visible columns; must be between 1 and [`PTY_COLUMNS_MAX`].
    pub columns: u16,
}

impl PtyDimensions {
    fn validate(self) -> Result<PtySize, ShellError> {
        if self.rows == 0 || self.columns == 0 {
            return Err(error(
                ErrorCode::InvalidArgument,
                "PTY dimensions must be nonzero",
            ));
        }
        if self.rows > PTY_ROWS_MAX || self.columns > PTY_COLUMNS_MAX {
            return Err(error(
                ErrorCode::ResourceLimit,
                "PTY dimensions exceed their limits",
            )
            .with_context(format!(
                "limits rows={PTY_ROWS_MAX} columns={PTY_COLUMNS_MAX}; observed rows={} columns={}",
                self.rows, self.columns
            )));
        }
        Ok(PtySize {
            rows: self.rows,
            cols: self.columns,
            pixel_width: 0,
            pixel_height: 0,
        })
    }
}

#[derive(Debug)]
/// Explicit executable invocation; inherited host environment is always cleared.
///
/// Arguments, executable and cwd share [`PTY_ARGUMENT_BYTES_MAX`]. Environment
/// admission uses the native executor's existing variable and byte limits.
/// Unix paths and environment bytes are preserved without lossy conversion.
pub struct PtySpawnRequest {
    /// Executable path; this boundary requires an absolute path, not PATH search.
    pub executable: PathBuf,
    /// Exact argv after argv0; at most [`PTY_ARGUMENTS_MAX`] entries.
    pub arguments: Vec<OsString>,
    /// Complete private environment; no host variables are implicitly retained.
    pub environment: Vec<(OsString, OsString)>,
    /// Absolute initial working directory of the child.
    pub cwd: PathBuf,
    /// Initial terminal size, independent of the host's physical terminal.
    pub size: PtyDimensions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Result of one bounded, nonblocking master read.
pub enum PtyRead {
    /// Number of initialized bytes in the caller's buffer.
    Bytes(usize),
    /// No bytes are available now; return control to the host event loop.
    Pending,
    /// The slave side closed. This alone does not prove the child exited.
    Closed,
}

/// Recover unread input from this worker's controlling terminal without flushing it.
///
/// The host must first stop forwarding input and finish the native foreground
/// graph. This function briefly disables canonical input and echo with
/// `TCSANOW`, reads at most [`PTY_TYPEAHEAD_BYTES_MAX`] bytes, and restores the
/// original configured modes before returning (kernel-managed pending-input
/// status may change). It performs at most 128 nonblocking
/// reads of at most 8 KiB; there is no input wait, output, decoding, or execution.
/// Partial UTF-8 and partial canonical lines are returned as original bytes.
/// Limit, I/O, and restoration failures return an actionable [`ShellError`].
pub fn read_terminal_typeahead() -> Result<Vec<u8>, ShellError> {
    read_terminal_typeahead_with_limit(PTY_TYPEAHEAD_BYTES_MAX)
}

fn read_terminal_typeahead_with_limit(limit: usize) -> Result<Vec<u8>, ShellError> {
    use nix::{
        fcntl::OFlag,
        sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr},
    };
    use std::os::unix::fs::OpenOptionsExt;
    // Failure model: foreground programs may leave a partial canonical line;
    // reading it canonically would block or omit it. The pause handshake keeps
    // this drain finite and prevents a producer from racing the final empty
    // read. TCSANOW preserves queued bytes; TCSAFLUSH would silently lose them.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NONBLOCK | OFlag::O_NOCTTY | OFlag::O_CLOEXEC).bits())
        .open("/dev/tty")
        .map_err(|cause| io_error("could not open terminal typeahead", cause))?;
    let original = tcgetattr(&file)
        .map_err(|cause| io_error("could not save terminal modes for typeahead", cause))?;
    let mut terminal = TypeaheadTerminal {
        file,
        original,
        restored: false,
    };
    let mut readable = terminal.original.clone();
    readable
        .local_flags
        .remove(LocalFlags::ICANON | LocalFlags::ECHO);
    for index in [nix::libc::VMIN, nix::libc::VTIME] {
        *readable
            .control_chars
            .get_mut(index)
            .ok_or_else(|| error(ErrorCode::Io, "terminal read-control index is unavailable"))? = 0;
    }
    tcsetattr(&terminal.file, SetArg::TCSANOW, &readable)
        .map_err(|cause| io_error("could not expose partial terminal typeahead", cause))?;
    let recovered = collect_typeahead(&mut terminal.file, limit);
    let restored = terminal.restore();
    match (recovered, restored) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(restore)) => Err(error.with_context(format!(
            "terminal restoration also failed: {}",
            restore.message
        ))),
    }
}

struct TypeaheadTerminal {
    file: File,
    original: nix::sys::termios::Termios,
    restored: bool,
}

impl TypeaheadTerminal {
    fn restore(&mut self) -> Result<(), ShellError> {
        nix::sys::termios::tcsetattr(
            &self.file,
            nix::sys::termios::SetArg::TCSANOW,
            &self.original,
        )
        .map_err(|cause| io_error("could not restore terminal modes after typeahead", cause))?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for TypeaheadTerminal {
    fn drop(&mut self) {
        if !self.restored {
            let _ = self.restore();
        }
    }
}

fn collect_typeahead(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>, ShellError> {
    let mut bytes = Vec::new();
    for _ in 0..TYPEAHEAD_READS_MAX {
        let mut chunk = [0_u8; PTY_OUTPUT_TURN_BYTES_MAX];
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(bytes),
            Ok(length) => {
                let observed = bytes.len().saturating_add(length);
                if observed > limit {
                    return Err(limit_error("terminal typeahead bytes", limit, observed));
                }
                bytes.extend_from_slice(chunk.get(..length).ok_or_else(|| {
                    error(ErrorCode::Io, "invalid terminal typeahead read length")
                })?);
            }
            Err(cause) if cause.kind() == io::ErrorKind::WouldBlock => return Ok(bytes),
            Err(cause) if cause.kind() == io::ErrorKind::Interrupted => {}
            Err(cause) => return Err(io_error("could not recover terminal typeahead", cause)),
        }
    }
    Err(limit_error(
        "terminal typeahead read attempts",
        TYPEAHEAD_READS_MAX,
        TYPEAHEAD_READS_MAX.saturating_add(1),
    ))
}

/// RAII ownership for a Unix controlling terminal and its session leader.
///
/// Drop closes terminal handles, signals the still-owned group once, and waits
/// at most two seconds before transferring an unreaped child to the bounded
/// reaper. Explicit [`Self::finish`] reports cleanup errors to the caller.
pub struct PtySession {
    io: Option<FileDescriptor>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: ChildOwner,
}

impl PtySession {
    /// Validate and launch an executable in a new session with a controlling PTY.
    ///
    /// Invalid requests fail before spawning. Partial construction closes both
    /// PTY ends and retains any spawned child's cleanup owner. OS and dependency
    /// errors are mapped into actionable [`ShellError`] values.
    pub fn spawn(request: PtySpawnRequest) -> Result<Self, ShellError> {
        let size = request.size.validate()?;
        validate_request(&request)?;
        let environment = crate::SessionEnvironment::capture(request.environment);
        environment.ensure_valid()?;
        let reaper = reaper()?;
        let permit = Permit::acquire()?;
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|cause| io_error("could not allocate a controlling PTY", cause))?;
        let fd = pair.master.as_raw_fd().ok_or_else(|| {
            error(
                ErrorCode::Io,
                "Unix PTY did not expose an owned master descriptor",
            )
        })?;
        let mut io = FileDescriptor::dup(&Descriptor(fd))
            .map_err(|cause| io_error("could not duplicate the PTY master", cause))?;
        io.set_non_blocking(true)
            .map_err(|cause| io_error("could not make PTY I/O nonblocking", cause))?;
        let mut command = CommandBuilder::new(&request.executable);
        command.args(&request.arguments);
        command.cwd(&request.cwd);
        command.env_clear();
        for (name, value) in environment.variables {
            command.env(name, value);
        }
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|cause| io_error("could not spawn the PTY executable", cause))?;
        let child = ChildOwner::new(child, permit, reaper, Scope::Group)?;
        drop(pair.slave);
        Ok(Self {
            io: Some(io),
            master: Some(pair.master),
            child,
        })
    }

    /// Read at most 8 KiB with one nonblocking syscall, retaining no output here.
    /// Empty buffers are invalid; larger buffers are restricted to the turn cap.
    pub fn read_output(&mut self, output: &mut [u8]) -> Result<PtyRead, ShellError> {
        let amount = output.len().min(PTY_OUTPUT_TURN_BYTES_MAX);
        if amount == 0 {
            return Err(error(
                ErrorCode::InvalidArgument,
                "PTY output buffer is empty",
            ));
        }
        let output = output.get_mut(..amount).ok_or_else(|| {
            error(
                ErrorCode::InvalidArgument,
                "PTY output buffer boundary is invalid",
            )
        })?;
        let io = self.io.as_mut().ok_or_else(closed_error)?;
        match io.read(output) {
            Ok(0) => Ok(PtyRead::Closed),
            Ok(length) => Ok(PtyRead::Bytes(length)),
            Err(cause)
                if matches!(
                    cause.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(PtyRead::Pending)
            }
            Err(cause) if cause.raw_os_error() == Some(nix::libc::EIO) => Ok(PtyRead::Closed),
            Err(cause) => Err(io_error("could not read PTY output", cause)),
        }
    }

    /// Write one bounded input slice without waiting or retaining unsent bytes.
    /// Returns the consumed prefix length, or zero for temporary backpressure.
    /// The caller must preserve ordering and bound its own pending-input queue.
    pub fn write_input(&mut self, input: &[u8]) -> Result<usize, ShellError> {
        if input.len() > PTY_INPUT_TURN_BYTES_MAX {
            return Err(limit_error(
                "PTY input turn",
                PTY_INPUT_TURN_BYTES_MAX,
                input.len(),
            ));
        }
        let io = self.io.as_mut().ok_or_else(closed_error)?;
        match io.write(input) {
            Ok(length) => Ok(length),
            Err(cause)
                if matches!(
                    cause.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(0)
            }
            Err(cause) => Err(io_error("could not write PTY input", cause)),
        }
    }

    /// Set the child terminal size; the kernel delivers SIGWINCH to its foreground group.
    pub fn resize(&self, size: PtyDimensions) -> Result<(), ShellError> {
        self.master
            .as_ref()
            .ok_or_else(closed_error)?
            .resize(size.validate()?)
            .map_err(|cause| io_error("could not resize the PTY", cause))
    }

    /// Observe an exit without reaping; signals use conventional 128+signal status.
    /// This reserves the numeric process-group identity until cleanup finishes.
    pub fn exit_status(&self) -> Result<Option<i32>, ShellError> {
        self.child.exit_status()
    }

    /// Direct session-leader PID, for bounded diagnostics; not signal authority.
    pub fn process_id(&self) -> u32 {
        self.child.pid.as_raw().unsigned_abs()
    }

    /// Close I/O, terminate retained group members, and reap the direct child.
    /// Call only after the host has finished its explicitly bounded output drain.
    pub fn finish(mut self) -> Result<i32, ShellError> {
        self.close_terminal();
        self.child.finish()
    }

    /// Cancel the active executable and perform the same bounded cleanup as finish.
    pub fn cancel(self) -> Result<i32, ShellError> {
        self.finish()
    }

    fn close_terminal(&mut self) {
        // Release every master reference before waiting so cleanup never holds
        // an undrained terminal open. No writer Drop may inject terminal bytes.
        self.io.take();
        self.master.take();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.close_terminal();
    }
}

fn validate_request(request: &PtySpawnRequest) -> Result<(), ShellError> {
    use std::os::unix::ffi::OsStrExt;
    if !request.executable.is_absolute() || !request.cwd.is_absolute() {
        return Err(error(
            ErrorCode::InvalidArgument,
            "PTY executable and cwd must be absolute paths",
        ));
    }
    if request.arguments.len() > PTY_ARGUMENTS_MAX {
        return Err(limit_error(
            "PTY argument count",
            PTY_ARGUMENTS_MAX,
            request.arguments.len(),
        ));
    }
    if request.environment.len() > crate::SESSION_ENVIRONMENT_VARIABLES_MAX {
        return Err(limit_error(
            "PTY environment entries",
            crate::SESSION_ENVIRONMENT_VARIABLES_MAX,
            request.environment.len(),
        ));
    }
    let mut environment_bytes = 0_usize;
    for (name, value) in &request.environment {
        environment_bytes = environment_bytes
            .saturating_add(name.len())
            .saturating_add(value.len());
        if environment_bytes > crate::SESSION_ENVIRONMENT_BYTES_MAX {
            return Err(limit_error(
                "PTY environment bytes",
                crate::SESSION_ENVIRONMENT_BYTES_MAX,
                environment_bytes,
            ));
        }
        if name.is_empty()
            || name.as_bytes().contains(&b'=')
            || name.as_bytes().contains(&0)
            || value.as_bytes().contains(&0)
        {
            return Err(error(
                ErrorCode::InvalidArgument,
                "PTY environment has an invalid name or NUL byte",
            ));
        }
    }
    let mut retained = 0_usize;
    for value in std::iter::once(request.executable.as_os_str())
        .chain(std::iter::once(request.cwd.as_os_str()))
        .chain(request.arguments.iter().map(OsString::as_os_str))
    {
        if value.as_bytes().contains(&0) {
            return Err(error(
                ErrorCode::InvalidArgument,
                "PTY arguments or paths contain a NUL byte",
            ));
        }
        retained = retained.saturating_add(value.len());
        if retained > PTY_ARGUMENT_BYTES_MAX {
            return Err(limit_error(
                "PTY argument bytes",
                PTY_ARGUMENT_BYTES_MAX,
                retained,
            ));
        }
    }
    Ok(())
}

struct Descriptor(RawFd);
impl AsRawFd for Descriptor {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

struct Permit;
impl Permit {
    fn acquire() -> Result<Arc<Self>, ShellError> {
        ACTIVE_SESSIONS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < PTY_SESSIONS_MAX).then(|| active.saturating_add(1))
            })
            .map_err(|active| {
                limit_error(
                    "PTY sessions including deferred cleanup",
                    PTY_SESSIONS_MAX,
                    active.saturating_add(1),
                )
            })?;
        Ok(Arc::new(Self))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE_SESSIONS.fetch_sub(1, Ordering::AcqRel);
    }
}

type Child = Box<dyn portable_pty::Child + Send + Sync>;
struct DeferredChild {
    child: Child,
    _permit: Arc<Permit>,
}

fn reaper() -> Result<SyncSender<DeferredChild>, ShellError> {
    REAPER
        .get_or_init(|| {
            let (sender, receiver) =
                sync_channel::<DeferredChild>(PTY_SESSIONS_MAX.saturating_mul(2));
            thread::Builder::new()
                .name("quirl-pty-reaper".into())
                .spawn(move || {
                    while let Ok(mut owned) = receiver.recv() {
                        let _ = owned.child.wait();
                    }
                })
                .map_err(|cause| cause.to_string())?;
            Ok(sender)
        })
        .as_ref()
        .cloned()
        .map_err(|cause| io_error("could not initialize the bounded PTY reaper", cause))
}

#[derive(Clone, Copy)]
enum Scope {
    Group,
    #[cfg(target_os = "macos")]
    Probe,
}

struct ChildOwner {
    child: Option<Child>,
    pid: Pid,
    permit: Arc<Permit>,
    reaper: SyncSender<DeferredChild>,
    signalled: bool,
    cleanup_deadline: Option<Instant>,
    scope: Scope,
}

impl ChildOwner {
    fn new(
        mut child: Child,
        permit: Arc<Permit>,
        reaper: SyncSender<DeferredChild>,
        scope: Scope,
    ) -> Result<Self, ShellError> {
        let pid = child
            .process_id()
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|pid| *pid > 0);
        let Some(pid) = pid else {
            let _ = child.kill();
            let _ = reaper.send(DeferredChild {
                child,
                _permit: permit,
            });
            return Err(error(
                ErrorCode::Io,
                "spawned Unix PTY child has no valid process identity",
            ));
        };
        Ok(Self {
            child: Some(child),
            pid: Pid::from_raw(pid),
            permit,
            reaper,
            signalled: false,
            cleanup_deadline: None,
            scope,
        })
    }

    fn exit_status(&self) -> Result<Option<i32>, ShellError> {
        use rustix::process::{Pid as WaitPid, WaitId, WaitIdOptions, waitid};
        let pid = WaitPid::from_raw(self.pid.as_raw())
            .ok_or_else(|| error(ErrorCode::Io, "invalid owned PTY process identity"))?;
        let status = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map_err(|cause| io_error("could not observe owned PTY child", cause))?;
        Ok(status.map(|status| {
            status
                .exit_status()
                .unwrap_or_else(|| 128_i32.saturating_add(status.terminating_signal().unwrap_or(0)))
        }))
    }

    fn finish(&mut self) -> Result<i32, ShellError> {
        if self.child.is_none() {
            return Err(closed_error());
        }
        let deadline = match self.cleanup_deadline {
            Some(deadline) => deadline,
            None => Instant::now()
                .checked_add(CLEANUP_TIMEOUT)
                .ok_or_else(|| error(ErrorCode::ResourceLimit, "PTY cleanup deadline overflow"))?,
        };
        // Explicit failure, Drop, and the optional observation probe all share
        // this deadline. Cleanup never acquires a fresh retry budget.
        self.cleanup_deadline = Some(deadline);
        let signal = if self.signalled {
            Ok(())
        } else {
            self.signalled = true;
            let group = match self.scope {
                Scope::Group => killpg(self.pid, Signal::SIGKILL),
                #[cfg(target_os = "macos")]
                Scope::Probe => Ok(()),
            };
            let direct = kill(self.pid, Signal::SIGKILL);
            match group.and(direct) {
                Ok(()) | Err(Errno::ESRCH) => Ok(()),
                Err(cause) => Err(cause),
            }
        };
        #[cfg(target_os = "macos")]
        let signal = if matches!(self.scope, Scope::Group) && signal == Err(Errno::EPERM) {
            while self.exit_status()?.is_none() {
                ensure_deadline(deadline)?;
                thread::sleep(CLEANUP_POLL);
            }
            if self.group_has_no_live_members(deadline)? {
                Ok(())
            } else {
                signal
            }
        } else {
            signal
        };
        loop {
            if let Some(status) = self.exit_status()? {
                if let Some(child) = self.child.as_mut() {
                    child
                        .try_wait()
                        .map_err(|cause| io_error("could not reap owned PTY child", cause))?
                        .ok_or_else(|| {
                            error(ErrorCode::Io, "observed PTY exit was not reapable")
                        })?;
                }
                self.child.take();
                signal.map_err(|cause| {
                    io_error("could not terminate owned PTY process group", cause)
                })?;
                return Ok(status);
            }
            ensure_deadline(deadline)?;
            thread::sleep(CLEANUP_POLL);
        }
    }

    #[cfg(target_os = "macos")]
    fn group_has_no_live_members(&self, deadline: Instant) -> Result<bool, ShellError> {
        use std::process::{Command, Stdio};
        // Darwin can report EPERM for a zombie-only group. Retain its unreaped
        // leader while a fixed, bounded OS query distinguishes that from a real
        // permission failure. The probe owns only its direct child, avoiding
        // recursive group verification during probe cleanup.
        ensure_deadline(deadline)?;
        let mut child = Command::new("/bin/ps")
            .args(["-axo", "pgid=,stat="])
            .env_clear()
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|cause| io_error("could not verify PTY group cleanup", cause))?;
        let stdout = child.stdout.take();
        let mut owner = Self::new(
            Box::new(child),
            Arc::clone(&self.permit),
            self.reaper.clone(),
            Scope::Probe,
        )?;
        owner.cleanup_deadline = Some(deadline);
        let stdout =
            stdout.ok_or_else(|| error(ErrorCode::Io, "PTY cleanup probe has no stdout"))?;
        let mut pipe = FileDescriptor::dup(&stdout)
            .map_err(|cause| io_error("could not own PTY cleanup probe output", cause))?;
        drop(stdout);
        pipe.set_non_blocking(true)
            .map_err(|cause| io_error("could not bound PTY cleanup probe output", cause))?;
        let mut bytes = Vec::new();
        let mut closed = false;
        loop {
            ensure_deadline(deadline)?;
            let mut chunk = [0; PTY_OUTPUT_TURN_BYTES_MAX];
            match pipe.read(&mut chunk) {
                Ok(0) => closed = true,
                Ok(length) => {
                    let observed = bytes.len().saturating_add(length);
                    if observed > GROUP_OBSERVATION_BYTES_MAX {
                        return Err(limit_error(
                            "PTY cleanup probe output",
                            GROUP_OBSERVATION_BYTES_MAX,
                            observed,
                        ));
                    }
                    bytes.extend_from_slice(
                        chunk
                            .get(..length)
                            .ok_or_else(|| error(ErrorCode::Io, "invalid PTY probe read length"))?,
                    );
                }
                Err(cause)
                    if matches!(
                        cause.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(cause) => return Err(io_error("could not read PTY cleanup probe", cause)),
            }
            if closed && let Some(status) = owner.exit_status()? {
                owner.finish()?;
                if status != 0 {
                    return Err(error(ErrorCode::Io, "PTY cleanup probe failed"));
                }
                break;
            }
            thread::sleep(CLEANUP_POLL);
        }
        no_live_group_rows(&bytes, self.pid.as_raw())
    }
}

impl Drop for ChildOwner {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        let _ = self.finish();
        if let Some(child) = self.child.take() {
            // 32 permits, at most 2 children each, and this child is not yet
            // queued: the 64-slot send has a free slot and cannot backpressure.
            let _ = self.reaper.send(DeferredChild {
                child,
                _permit: Arc::clone(&self.permit),
            });
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn no_live_group_rows(bytes: &[u8], group: i32) -> Result<bool, ShellError> {
    if bytes.len() > GROUP_OBSERVATION_BYTES_MAX {
        return Err(limit_error(
            "PTY process observation bytes",
            GROUP_OBSERVATION_BYTES_MAX,
            bytes.len(),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|cause| io_error("PTY process observation is not UTF-8", cause))?;
    if text.trim().is_empty() {
        return Err(error(ErrorCode::Io, "PTY process observation is empty"));
    }
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let pid = fields
            .next()
            .and_then(|value| value.parse::<i32>().ok())
            .ok_or_else(|| error(ErrorCode::Io, "invalid PTY process observation group"))?;
        let state = fields
            .next()
            .ok_or_else(|| error(ErrorCode::Io, "missing PTY process observation state"))?;
        if fields.next().is_some() {
            return Err(error(ErrorCode::Io, "extra PTY process observation fields"));
        }
        if pid == group && !state.starts_with('Z') {
            return Ok(false);
        }
    }
    Ok(true)
}

fn ensure_deadline(deadline: Instant) -> Result<(), ShellError> {
    if Instant::now() >= deadline {
        Err(error(
            ErrorCode::ResourceLimit,
            "PTY cleanup exceeded its two-second deadline",
        )
        .with_context(
            "cleanup limit 2000ms; direct-child reaping remains owned by the bounded reaper",
        ))
    } else {
        Ok(())
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> ShellError {
    ShellError::new(code, message).with_help("Retry with a smaller terminal/input or use the simple surface; report repeated PTY lifecycle failures")
}
fn io_error(message: &str, cause: impl std::fmt::Display) -> ShellError {
    error(ErrorCode::Io, message).with_context(cause.to_string())
}
fn limit_error(subject: &str, limit: usize, observed: usize) -> ShellError {
    error(
        ErrorCode::ResourceLimit,
        format!("{subject} exceeds its limit"),
    )
    .with_context(format!("limit {limit}; observed {observed}"))
}
fn closed_error() -> ShellError {
    error(ErrorCode::Io, "PTY session has already closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn request(script: &str) -> PtySpawnRequest {
        PtySpawnRequest {
            executable: PathBuf::from("/bin/sh"),
            arguments: vec!["-c".into(), script.into(), "pty-test".into()],
            environment: vec![("PATH".into(), "/usr/bin:/bin".into())],
            cwd: PathBuf::from("/"),
            size: PtyDimensions {
                rows: 24,
                columns: 80,
            },
        }
    }

    fn deadline() -> Instant {
        Instant::now().checked_add(Duration::from_secs(5)).unwrap()
    }

    fn output_until(session: &mut PtySession, needle: &[u8]) -> Vec<u8> {
        let end = deadline();
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; PTY_OUTPUT_TURN_BYTES_MAX];
            match session.read_output(&mut chunk).unwrap() {
                PtyRead::Bytes(length) => bytes.extend_from_slice(&chunk[..length]),
                PtyRead::Pending | PtyRead::Closed => thread::sleep(CLEANUP_POLL),
            }
            assert!(
                bytes.len() <= 128 * 1024,
                "fixture output exceeded its bound"
            );
            if bytes.windows(needle.len()).any(|part| part == needle) {
                return bytes;
            }
            assert!(Instant::now() < end, "missing {needle:?}; output {bytes:?}");
        }
    }

    fn assert_reaped(pid: u32) {
        use rustix::process::{Pid as WaitPid, WaitId, WaitIdOptions, waitid};
        assert_eq!(
            waitid(
                WaitId::Pid(WaitPid::from_raw(i32::try_from(pid).unwrap()).unwrap()),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT
            )
            .unwrap_err(),
            rustix::io::Errno::CHILD,
        );
    }

    #[test]
    fn terminal_typeahead_worker_fixture() {
        use nix::sys::termios::{LocalFlags, tcgetattr};
        use std::os::unix::net::UnixStream;
        let Some(socket) = std::env::var_os("QUIRL_TEST_TYPEAHEAD_SOCKET") else {
            return;
        };
        let mut control = UnixStream::connect(socket).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        control
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let terminal = File::open("/dev/tty").unwrap();
        let original = tcgetattr(&terminal).unwrap();
        assert!(original.local_flags.contains(LocalFlags::ICANON));
        control.write_all(b"R").unwrap();
        let mut acknowledgement = [0];
        control.read_exact(&mut acknowledgement).unwrap();
        assert_eq!(acknowledgement, *b"D");
        let limited = std::env::var_os("QUIRL_TEST_TYPEAHEAD_LIMIT").is_some();
        let result = if limited {
            read_terminal_typeahead_with_limit(2)
        } else {
            read_terminal_typeahead()
        };
        let restored = tcgetattr(&terminal).unwrap();
        assert_typeahead_modes_restored(&restored, &original);
        let (status, recovered) = match result {
            Ok(bytes) => (0, bytes),
            Err(error) => {
                assert!(limited);
                assert_eq!(error.code, ErrorCode::ResourceLimit);
                (1, Vec::new())
            }
        };
        // The parent has paused writes: a second read is a valid empty
        // transition, including after the intentionally failed limit case.
        assert!(read_terminal_typeahead().unwrap().is_empty());
        assert_typeahead_modes_restored(&tcgetattr(&terminal).unwrap(), &original);
        control.write_all(&[status]).unwrap();
        control
            .write_all(&u32::try_from(recovered.len()).unwrap().to_be_bytes())
            .unwrap();
        control.write_all(&recovered).unwrap();
    }

    fn assert_typeahead_modes_restored(
        current: &nix::sys::termios::Termios,
        original: &nix::sys::termios::Termios,
    ) {
        use nix::sys::termios::{LocalFlags, cfgetispeed, cfgetospeed};
        // Darwin sets PENDIN on the canonical-mode transition. It describes
        // kernel queue processing, not a changed user terminal configuration.
        assert_eq!(
            current.local_flags & !LocalFlags::PENDIN,
            original.local_flags & !LocalFlags::PENDIN
        );
        assert_eq!(current.input_flags, original.input_flags);
        assert_eq!(current.output_flags, original.output_flags);
        assert_eq!(current.control_flags, original.control_flags);
        assert_eq!(current.control_chars, original.control_chars);
        assert_eq!(cfgetispeed(current), cfgetispeed(original));
        assert_eq!(cfgetospeed(current), cfgetospeed(original));
    }

    fn recover_typeahead_fixture(input: &[u8], limited: bool) -> (u8, Vec<u8>) {
        use std::os::unix::{fs::DirBuilderExt, net::UnixListener};
        static NEXT_SOCKET: AtomicUsize = AtomicUsize::new(0);
        struct Directory(PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path = PathBuf::from(format!(
            "/tmp/quirl-typeahead-{}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let directory = Directory(path);
        let socket = directory.0.join("control");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut invocation = request("");
        invocation.executable = std::env::current_exe().unwrap();
        invocation.arguments = vec![
            "--exact".into(),
            "pty::tests::terminal_typeahead_worker_fixture".into(),
            "--nocapture".into(),
        ];
        invocation.environment.push((
            "QUIRL_TEST_TYPEAHEAD_SOCKET".into(),
            socket.into_os_string(),
        ));
        if limited {
            invocation
                .environment
                .push(("QUIRL_TEST_TYPEAHEAD_LIMIT".into(), "1".into()));
        }
        let mut session = PtySession::spawn(invocation).unwrap();
        let pid = session.process_id();
        let end = deadline();
        let mut control = loop {
            match listener.accept() {
                Ok((control, _)) => break control,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("could not accept fixture: {error}"),
            }
            assert!(Instant::now() < end);
            thread::sleep(CLEANUP_POLL);
        };
        control.set_nonblocking(false).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        control
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut ready = [0];
        control.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"R");
        assert_eq!(session.write_input(input).unwrap(), input.len());
        // This independent socket gates the drain without consuming any of the
        // test's queued canonical bytes or adding a newline to release them.
        control.write_all(b"D").unwrap();
        let mut status = [0];
        let mut length = [0; 4];
        if let Err(error) = control.read_exact(&mut status) {
            let mut diagnostic = Vec::new();
            for _ in 0..16 {
                let mut chunk = [0; PTY_OUTPUT_TURN_BYTES_MAX];
                match session.read_output(&mut chunk).unwrap() {
                    PtyRead::Bytes(length) => diagnostic.extend_from_slice(&chunk[..length]),
                    PtyRead::Pending | PtyRead::Closed => break,
                }
            }
            panic!(
                "typeahead worker: {error}; output {:?}",
                String::from_utf8_lossy(&diagnostic)
            );
        }
        control.read_exact(&mut length).unwrap();
        let length = usize::try_from(u32::from_be_bytes(length)).unwrap();
        assert!(length <= PTY_TYPEAHEAD_BYTES_MAX);
        let mut recovered = vec![0; length];
        control.read_exact(&mut recovered).unwrap();
        let end = deadline();
        let mut worker_output = Vec::new();
        while session.exit_status().unwrap().is_none() {
            let mut chunk = [0; PTY_OUTPUT_TURN_BYTES_MAX];
            if let PtyRead::Bytes(length) = session.read_output(&mut chunk).unwrap() {
                worker_output.extend_from_slice(&chunk[..length]);
            }
            assert!(worker_output.len() <= 128 * 1024);
            assert!(
                Instant::now() < end,
                "worker output {:?}",
                String::from_utf8_lossy(&worker_output)
            );
            thread::sleep(CLEANUP_POLL);
        }
        assert_eq!(session.finish().unwrap(), 0);
        assert_reaped(pid);
        (status[0], recovered)
    }

    #[test]
    fn typeahead_recovers_partial_canonical_utf8_without_a_newline() {
        let input = b"next \xce\xbb\xe2\x82";
        assert_eq!(recover_typeahead_fixture(input, false), (0, input.to_vec()));
        let input = b"complete\npartial";
        assert_eq!(recover_typeahead_fixture(input, false), (0, input.to_vec()));
    }

    #[test]
    fn typeahead_restores_modes_after_an_admission_error() {
        assert_eq!(
            recover_typeahead_fixture(b"too much", true),
            (1, Vec::new())
        );
    }

    #[test]
    fn typeahead_collection_enforces_exact_byte_and_read_attempt_bounds() {
        let exact = vec![b'x'; PTY_TYPEAHEAD_BYTES_MAX];
        assert_eq!(
            collect_typeahead(&mut io::Cursor::new(&exact), PTY_TYPEAHEAD_BYTES_MAX).unwrap(),
            exact
        );
        let overflow = vec![b'x'; PTY_TYPEAHEAD_BYTES_MAX.saturating_add(1)];
        assert_eq!(
            collect_typeahead(&mut io::Cursor::new(overflow), PTY_TYPEAHEAD_BYTES_MAX)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        struct Interrupted(usize);
        impl Read for Interrupted {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                self.0 = self.0.saturating_add(1);
                Err(io::ErrorKind::Interrupted.into())
            }
        }
        let mut interrupted = Interrupted(0);
        assert_eq!(
            collect_typeahead(&mut interrupted, PTY_TYPEAHEAD_BYTES_MAX)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(interrupted.0, TYPEAHEAD_READS_MAX);
    }

    #[test]
    fn controlling_terminal_preserves_private_environment_input_and_resize() {
        let mut invocation = request(
            "test -t 0 && test -t 1 && test -t 2 || exit 91; test \"$PTY_PRIVATE\" = 'private value' || exit 92; test \"$1\" = 'argument value' || exit 93; test \"$PWD\" = / || exit 94; stty -echo; printf READY; IFS= read -r line; printf '\\nRECEIVED:%s\\n' \"$line\"; stty size; exit 23",
        );
        invocation.arguments.push("argument value".into());
        invocation
            .environment
            .push(("PTY_PRIVATE".into(), "private value".into()));
        let mut session = PtySession::spawn(invocation).unwrap();
        let pid = session.process_id();
        output_until(&mut session, b"READY");
        session
            .resize(PtyDimensions {
                rows: 37,
                columns: 91,
            })
            .unwrap();
        assert_eq!(session.write_input(b"exact input\n").unwrap(), 12);
        let bytes = output_until(&mut session, b"37 91");
        assert!(
            bytes
                .windows(b"RECEIVED:exact input".len())
                .any(|part| part == b"RECEIVED:exact input")
        );
        let end = deadline();
        while session.exit_status().unwrap().is_none() {
            assert!(Instant::now() < end);
            thread::sleep(CLEANUP_POLL);
        }
        assert_eq!(session.finish().unwrap(), 23);
        assert_reaped(pid);
    }

    #[test]
    fn resize_delivers_winch_to_the_terminal_foreground_group_before_input() {
        // Bash 5 restarts an untimed read on WINCH. Its timed read waits via
        // select, dispatching the trap before input; our owner still enforces
        // the independent five-second fixture deadline.
        let mut invocation = request(
            "trap 'printf RESIZED:; stty size' WINCH; printf READY; IFS= read -r -t 30 line; exit 0",
        );
        invocation.executable = PathBuf::from("/bin/bash");
        let mut session = PtySession::spawn(invocation).unwrap();
        output_until(&mut session, b"READY");
        session
            .resize(PtyDimensions {
                rows: 33,
                columns: 111,
            })
            .unwrap();
        output_until(&mut session, b"RESIZED:33 111");
        session.cancel().unwrap();
    }

    #[test]
    fn nested_native_terminal_worker_fixture() {
        // The outer test owns this exact worker invocation and its terminal;
        // ordinary test-suite execution performs no child or terminal effects.
        if std::env::var_os("QUIRL_TEST_NESTED_PTY").as_deref() != Some(std::ffi::OsStr::new("1")) {
            return;
        }
        let pipeline = quirl_syntax::Pipeline {
            background: false,
            commands: vec![quirl_syntax::SimpleCommand {
                words: vec!["/bin/bash".into(), "-c".into(), "trap 'printf RESIZED:; stty size' WINCH; printf NESTED_READY; IFS= read -r -t 30 line".into()],
                word_ir: vec![],
                redirects: vec![],
            }],
        };
        crate::NativeExecutor::default()
            .execute_prepared_terminal_pipeline(pipeline)
            .unwrap();
    }

    #[test]
    fn nested_native_foreground_group_receives_resize_before_input() {
        let mut invocation = request("");
        invocation.executable = std::env::current_exe().unwrap();
        invocation.arguments = vec![
            "--exact".into(),
            "pty::tests::nested_native_terminal_worker_fixture".into(),
            "--nocapture".into(),
        ];
        invocation
            .environment
            .push(("QUIRL_TEST_NESTED_PTY".into(), "1".into()));
        let mut session = PtySession::spawn(invocation).unwrap();
        output_until(&mut session, b"NESTED_READY");
        session
            .resize(PtyDimensions {
                rows: 33,
                columns: 111,
            })
            .unwrap();
        output_until(&mut session, b"RESIZED:33 111");
        session.cancel().unwrap();
    }

    #[test]
    fn an_exit_before_observation_preserves_its_exact_status_and_reaps() {
        let session = PtySession::spawn(request("exit 42")).unwrap();
        let pid = session.process_id();
        let end = deadline();
        while session.exit_status().unwrap().is_none() {
            assert!(Instant::now() < end);
            thread::sleep(CLEANUP_POLL);
        }
        // WNOWAIT leaves the already exited child observable repeatedly, even
        // when no output ever arrived and no observer existed at child exit.
        assert_eq!(session.exit_status().unwrap(), Some(42));
        assert_eq!(session.finish().unwrap(), 42);
        assert_reaped(pid);
    }

    #[test]
    fn nonreading_terminal_returns_backpressure_and_cancellation_reaps() {
        let mut session = PtySession::spawn(request(
            "stty -echo -icanon; sleep 30 & printf 'READY:%s:END' \"$!\"; wait",
        ))
        .unwrap();
        let pid = session.process_id();
        let ready = output_until(&mut session, b":END");
        let descendant = String::from_utf8(ready)
            .unwrap()
            .split(':')
            .nth(1)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let input = vec![b'x'; PTY_INPUT_TURN_BYTES_MAX];
        let mut backpressured = false;
        // At most 8 MiB is attempted, and each call is one nonblocking write.
        for _ in 0..128 {
            if session.write_input(&input).unwrap() == 0 {
                backpressured = true;
                break;
            }
        }
        assert!(backpressured);
        session.cancel().unwrap();
        assert_reaped(pid);
        let end = deadline();
        loop {
            let result = kill(Pid::from_raw(descendant), None);
            if result == Err(Errno::ESRCH) {
                break;
            }
            // These fixed same-user fixtures never change credentials. Darwin
            // reports EPERM for an exited zombie; Linux exposes its inert state
            // until the system's init reaps this non-direct descendant.
            #[cfg(target_os = "macos")]
            if result == Err(Errno::EPERM) {
                break;
            }
            #[cfg(target_os = "linux")]
            if std::fs::read_to_string(format!("/proc/{descendant}/stat")).is_ok_and(|stat| {
                stat.split_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('Z'))
            }) {
                break;
            }
            assert!(Instant::now() < end, "owned descendant remains alive");
            thread::sleep(CLEANUP_POLL);
        }
    }

    #[test]
    fn raw_environment_and_terminal_output_do_not_require_utf8() {
        let mut invocation = request("printf 'RAW:%s:END' \"$RAW\"");
        invocation
            .environment
            .push(("RAW".into(), OsString::from_vec(vec![0x80, 0xff])));
        let mut session = PtySession::spawn(invocation).unwrap();
        assert_eq!(output_until(&mut session, b":END"), b"RAW:\x80\xff:END");
        session.finish().unwrap();
    }

    #[test]
    fn rejected_input_turn_preserves_a_usable_session() {
        let mut session = PtySession::spawn(request(
            "stty -echo; printf READY; IFS= read -r line; printf 'ACCEPTED:%s' \"$line\"",
        ))
        .unwrap();
        output_until(&mut session, b"READY");
        let error = session
            .write_input(&vec![b'x'; PTY_INPUT_TURN_BYTES_MAX.saturating_add(1)])
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(session.write_input(b"valid\n").unwrap(), 6);
        output_until(&mut session, b"ACCEPTED:valid");
        session.finish().unwrap();
    }

    #[test]
    fn argument_environment_and_dimensions_are_validated_before_spawn() {
        let mut invocation = request("exit 0");
        validate_request(&invocation).unwrap();
        invocation
            .environment
            .push(("BAD=KEY".into(), "value".into()));
        assert_eq!(
            validate_request(&invocation).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        invocation.environment.pop();
        invocation.arguments.push(OsString::from_vec(vec![0]));
        assert_eq!(
            validate_request(&invocation).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        invocation.arguments.pop();
        invocation
            .arguments
            .push("x".repeat(PTY_ARGUMENT_BYTES_MAX).into());
        assert_eq!(
            validate_request(&invocation).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        invocation.arguments.pop();
        validate_request(&invocation).unwrap();
        PtyDimensions {
            rows: PTY_ROWS_MAX,
            columns: PTY_COLUMNS_MAX,
        }
        .validate()
        .unwrap();
        assert_eq!(
            PtyDimensions {
                rows: 0,
                columns: 80
            }
            .validate()
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            PtyDimensions {
                rows: PTY_ROWS_MAX.saturating_add(1),
                columns: 80
            }
            .validate()
            .unwrap_err()
            .code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn drop_after_a_spawn_failure_does_not_poison_subsequent_ownership() {
        let mut invocation = request("exit 0");
        invocation.executable = PathBuf::from("/quirl-missing-pty-fixture/executable");
        assert!(PtySession::spawn(invocation).is_err());
        let session = PtySession::spawn(request("sleep 30")).unwrap();
        let pid = session.process_id();
        drop(session);
        assert_reaped(pid);
    }

    #[test]
    fn cleanup_reuses_its_original_deadline_after_failure() {
        let mut session = PtySession::spawn(request("sleep 30")).unwrap();
        let expired = Instant::now();
        session.child.cleanup_deadline = Some(expired);
        session.close_terminal();
        let _ = session.child.finish();
        assert_eq!(session.child.cleanup_deadline, Some(expired));
        let pid = session.process_id();
        drop(session);
        let end = deadline();
        use rustix::process::{Pid as WaitPid, WaitId, WaitIdOptions, waitid};
        loop {
            if waitid(
                WaitId::Pid(WaitPid::from_raw(i32::try_from(pid).unwrap()).unwrap()),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )
            .err()
                == Some(rustix::io::Errno::CHILD)
            {
                break;
            }
            assert!(Instant::now() < end, "deferred child was not reaped");
            thread::sleep(CLEANUP_POLL);
        }
    }

    #[test]
    fn group_observation_rejects_live_members_and_malformed_evidence() {
        assert!(no_live_group_rows(b"10 Z\n11 S+\n", 10).unwrap());
        assert!(!no_live_group_rows(b"10 Z\n10 S+\n", 10).unwrap());
        assert!(no_live_group_rows(b"", 10).is_err());
        assert!(no_live_group_rows(b"10\n", 10).is_err());
        assert!(no_live_group_rows(b"10 Z extra\n", 10).is_err());
        assert!(
            no_live_group_rows(
                &vec![b'x'; GROUP_OBSERVATION_BYTES_MAX.saturating_add(1)],
                10
            )
            .is_err()
        );
    }
}
