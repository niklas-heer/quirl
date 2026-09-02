//! Bounded Unix PTY ownership and the deterministic VT screen used by xtask.

use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    poll::{PollFd, PollFlags, PollTimeout, poll},
    pty::{ForkptyResult, Winsize, forkpty},
    sys::{
        signal::{Signal, kill},
        termios::{Termios, tcgetattr},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{Pid, getpgrp, tcgetpgrp},
};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::ffi::OsStrExt,
    },
    path::PathBuf,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthChar;

use crate::TaskError;

/// Default deadline for one bounded PTY wait.
///
/// A shared, CPU-constrained CI runner can be slow enough under load that a
/// correct interactive round trip (render a completion popup, stream a large
/// file, exit cleanly) misses a short deadline with nothing actually wrong;
/// reproduced locally by adding artificial CPU contention, and observed on
/// the actual CI runner even at 15s. `GITHUB_ACTIONS` (set by every GitHub
/// Actions job) selects a much more generous bound there, since wasting a
/// few extra seconds of CI wall time beats a false failure; a real hang is
/// still caught well within a job's multi-minute budget either way.
///
/// A check waiting on completed automatic command discovery can need more
/// than one background pass: `index::BACKGROUND_DISCOVERY_DEADLINE` (30s)
/// bounds a single attempt, but a fresh command not covered by the quick
/// first-run fallback may only be found after `index::
/// DISCOVERY_REFRESH_INTERVAL` (60s) brings the next one around -- 60s alone
/// was confirmed insufficient on real CI even after raising it once. 100s
/// clears one attempt plus one full refresh interval with real margin, and
/// stays under `validate_timeout`'s own 150s ceiling.
pub(super) fn default_timeout() -> Duration {
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        Duration::from_secs(100)
    } else {
        Duration::from_secs(15)
    }
}
const DEFAULT_OUTPUT_BYTES_MAX: usize = 16 * 1024 * 1024;
const READ_BYTES_MAX: usize = 64 * 1024;
const SCREEN_CELLS_MAX: usize = 512 * 512;
const CONTROL_SEQUENCE_CHARS_MAX: usize = 128;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) mod key {
    pub const ALT_Q: &[u8] = b"\x1bq";
    pub const CTRL_C: &[u8] = b"\x03";
    pub const CTRL_D: &[u8] = b"\x04";
    pub const CTRL_L: &[u8] = b"\x0c";
    pub const CTRL_U: &[u8] = b"\x15";
    pub const ESCAPE: &[u8] = b"\x1b";
    pub const ENTER: &[u8] = b"\r";
    pub const TAB: &[u8] = b"\t";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

/// Bounded terminal model for assertions about visible cells rather than stale bytes.
#[derive(Debug)]
pub(super) struct VirtualScreen {
    rows: usize,
    columns: usize,
    cells: Vec<Vec<String>>,
    cursor_row: usize,
    cursor_column: usize,
    saved_cursor: (usize, usize),
    scroll_top: usize,
    scroll_bottom: usize,
    wrap_pending: bool,
    state: ParserState,
    control: String,
    utf8_pending: Vec<u8>,
}

impl VirtualScreen {
    pub(super) fn new(rows: usize, columns: usize, initial_cursor_row: usize) -> io::Result<Self> {
        validate_screen_size(rows, columns)?;
        let cursor_row = initial_cursor_row.min(rows.saturating_sub(1));
        Ok(Self {
            rows,
            columns,
            cells: blank_cells(rows, columns),
            cursor_row,
            cursor_column: 0,
            saved_cursor: (cursor_row, 0),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            wrap_pending: false,
            state: ParserState::Ground,
            control: String::with_capacity(CONTROL_SEQUENCE_CHARS_MAX),
            utf8_pending: Vec::with_capacity(READ_BYTES_MAX.saturating_add(4)),
        })
    }

    pub(super) fn resize(&mut self, rows: usize, columns: usize) -> io::Result<()> {
        validate_screen_size(rows, columns)?;
        let mut resized = blank_cells(rows, columns);
        for (target, source) in resized.iter_mut().zip(&self.cells) {
            for (target_cell, source_cell) in target.iter_mut().zip(source) {
                target_cell.clone_from(source_cell);
            }
        }
        self.rows = rows;
        self.columns = columns;
        self.cells = resized;
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_column = self.cursor_column.min(columns.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.wrap_pending = false;
        Ok(())
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "the escape scanner validates the fixed CSI prefix length before slicing"
    )]
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.utf8_pending.extend_from_slice(bytes);
        let mut characters = Vec::new();
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(text) => {
                    characters.extend(text.chars());
                    self.utf8_pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        if let Ok(text) = std::str::from_utf8(&self.utf8_pending[..valid]) {
                            characters.extend(text.chars());
                        }
                        self.utf8_pending.drain(..valid);
                    }
                    let Some(invalid_bytes) = error.error_len() else {
                        break;
                    };
                    self.utf8_pending
                        .drain(..invalid_bytes.min(self.utf8_pending.len()));
                    characters.push('\u{fffd}');
                }
            }
        }

        let mut replies = Vec::new();
        for character in characters {
            if let Some(reply) = self.feed_character(character) {
                replies.push(reply);
            }
        }
        replies
    }

    pub(super) fn lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| row.concat().trim_end().to_owned())
            .collect()
    }

    pub(super) fn text(&self) -> String {
        self.lines().join("\n")
    }

    pub(super) fn bottom_line(&self) -> String {
        self.cells
            .last()
            .map(|row| row.concat().trim_end().to_owned())
            .unwrap_or_default()
    }

    fn feed_character(&mut self, character: char) -> Option<Vec<u8>> {
        match self.state {
            ParserState::Ground => self.ground(character),
            ParserState::Escape => self.escape(character),
            ParserState::Csi => self.csi(character),
            ParserState::Osc => {
                if character == '\u{7}' {
                    self.state = ParserState::Ground;
                } else if character == '\u{1b}' {
                    self.state = ParserState::OscEscape;
                }
                None
            }
            ParserState::OscEscape => {
                self.state = if character == '\\' {
                    ParserState::Ground
                } else {
                    ParserState::Osc
                };
                None
            }
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "terminal cursor coordinates are maintained within the fixed screen bounds"
    )]
    fn ground(&mut self, character: char) -> Option<Vec<u8>> {
        match character {
            '\u{1b}' => self.state = ParserState::Escape,
            '\r' => {
                self.cursor_column = 0;
                self.wrap_pending = false;
            }
            '\n' | '\u{b}' | '\u{c}' => self.line_feed(),
            '\u{8}' => {
                self.cursor_column = self.cursor_column.saturating_sub(1);
                self.wrap_pending = false;
            }
            '\t' => {
                self.cursor_column = (((self.cursor_column / 8).saturating_add(1)) * 8)
                    .min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            value if value >= ' ' && value != '\u{7f}' => self.put(value),
            _ => {}
        }
        None
    }

    fn escape(&mut self, character: char) -> Option<Vec<u8>> {
        self.state = ParserState::Ground;
        match character {
            '[' => {
                self.state = ParserState::Csi;
                self.control.clear();
            }
            ']' => self.state = ParserState::Osc,
            '7' => self.saved_cursor = (self.cursor_row, self.cursor_column),
            '8' => {
                (self.cursor_row, self.cursor_column) = self.saved_cursor;
                self.clamp_cursor();
            }
            'D' | 'E' => {
                if character == 'E' {
                    self.cursor_column = 0;
                }
                self.line_feed();
            }
            'M' => self.reverse_index(),
            'c' => self.reset(),
            _ => {}
        }
        None
    }

    fn csi(&mut self, character: char) -> Option<Vec<u8>> {
        if ('@'..='~').contains(&character) {
            let control = std::mem::take(&mut self.control);
            self.state = ParserState::Ground;
            return self.apply_csi(&control, character);
        }
        if self.control.len() >= CONTROL_SEQUENCE_CHARS_MAX {
            self.control.clear();
            self.state = ParserState::Ground;
        } else {
            self.control.push(character);
        }
        None
    }

    #[allow(
        clippy::string_slice,
        clippy::arithmetic_side_effects,
        reason = "the CSI introducer is validated as ASCII and coordinates remain within fixed screen bounds"
    )]
    fn apply_csi(&mut self, control: &str, final_character: char) -> Option<Vec<u8>> {
        let private = control.starts_with(['?', '>', '!']);
        let body = if private { &control[1..] } else { control };
        let parameters = csi_parameters(body);
        let first = parameters.first().copied().unwrap_or(0);
        let amount = first.max(1);
        match final_character {
            'H' | 'f' => {
                let row = parameters.first().copied().unwrap_or(1).max(1);
                let column = parameters.get(1).copied().unwrap_or(1).max(1);
                self.cursor_row = row.saturating_sub(1).min(self.rows.saturating_sub(1));
                self.cursor_column = column.saturating_sub(1).min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            'A' => {
                self.cursor_row = self.cursor_row.saturating_sub(amount).max(self.scroll_top);
                self.wrap_pending = false;
            }
            'B' => {
                self.cursor_row = self
                    .cursor_row
                    .saturating_add(amount)
                    .min(self.scroll_bottom);
                self.wrap_pending = false;
            }
            'C' => {
                self.cursor_column = self
                    .cursor_column
                    .saturating_add(amount)
                    .min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            'D' => {
                self.cursor_column = self.cursor_column.saturating_sub(amount);
                self.wrap_pending = false;
            }
            'E' | 'F' => {
                self.cursor_row = if final_character == 'E' {
                    self.cursor_row
                        .saturating_add(amount)
                        .min(self.scroll_bottom)
                } else {
                    self.cursor_row.saturating_sub(amount).max(self.scroll_top)
                };
                self.cursor_column = 0;
                self.wrap_pending = false;
            }
            'G' | '`' => {
                self.cursor_column = amount.saturating_sub(1).min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            'd' => {
                self.cursor_row = amount.saturating_sub(1).min(self.rows.saturating_sub(1));
                self.wrap_pending = false;
            }
            'J' => self.erase_display(first),
            'K' => self.erase_line(first),
            'S' => {
                let rows = self
                    .scroll_bottom
                    .saturating_sub(self.scroll_top)
                    .saturating_add(1);
                for _ in 0..amount.min(rows) {
                    self.scroll_up();
                }
            }
            'T' => {
                let rows = self
                    .scroll_bottom
                    .saturating_sub(self.scroll_top)
                    .saturating_add(1);
                for _ in 0..amount.min(rows) {
                    self.scroll_down();
                }
            }
            'r' if !private => {
                let top = parameters.first().copied().unwrap_or(1).max(1);
                let bottom = parameters.get(1).copied().unwrap_or(self.rows).max(1);
                if top < bottom && bottom <= self.rows {
                    self.scroll_top = top.saturating_sub(1);
                    self.scroll_bottom = bottom.saturating_sub(1);
                    self.cursor_row = 0;
                    self.cursor_column = 0;
                    self.wrap_pending = false;
                }
            }
            's' => self.saved_cursor = (self.cursor_row, self.cursor_column),
            'u' => {
                (self.cursor_row, self.cursor_column) = self.saved_cursor;
                self.clamp_cursor();
            }
            'n' if !private && first == 6 => {
                return Some(
                    format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_column + 1)
                        .into_bytes(),
                );
            }
            _ => {}
        }
        None
    }

    #[allow(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "row and column coordinates are clamped to the fixed terminal grid before access"
    )]
    fn put(&mut self, character: char) {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            let column = self.cursor_column.saturating_sub(1);
            self.cells[self.cursor_row][column].push(character);
            return;
        }
        if self.wrap_pending {
            self.cursor_column = 0;
            self.line_feed();
            self.wrap_pending = false;
        }
        if width == 2 && self.cursor_column == self.columns.saturating_sub(1) {
            self.cursor_column = 0;
            self.line_feed();
        }
        self.cells[self.cursor_row][self.cursor_column] = character.to_string();
        if width == 2 && self.cursor_column + 1 < self.columns {
            self.cells[self.cursor_row][self.cursor_column + 1].clear();
        }
        let final_column = self.cursor_column.saturating_add(width);
        if final_column >= self.columns {
            self.cursor_column = self.columns.saturating_sub(1);
            self.wrap_pending = true;
        } else {
            self.cursor_column = final_column;
        }
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up();
        } else {
            self.cursor_row = self
                .cursor_row
                .saturating_add(1)
                .min(self.rows.saturating_sub(1));
        }
    }

    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_top {
            self.scroll_down();
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    fn scroll_up(&mut self) {
        self.cells.remove(self.scroll_top);
        self.cells
            .insert(self.scroll_bottom, blank_row(self.columns));
    }

    fn scroll_down(&mut self) {
        self.cells.remove(self.scroll_bottom);
        self.cells.insert(self.scroll_top, blank_row(self.columns));
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "erase ranges are clamped to the fixed terminal grid"
    )]
    fn erase_display(&mut self, mode: usize) {
        match mode {
            2 | 3 => self.cells = blank_cells(self.rows, self.columns),
            1 => {
                for row in 0..self.cursor_row {
                    self.cells[row] = blank_row(self.columns);
                }
                for column in 0..=self.cursor_column {
                    self.cells[self.cursor_row][column] = " ".to_owned();
                }
            }
            _ => {
                for column in self.cursor_column..self.columns {
                    self.cells[self.cursor_row][column] = " ".to_owned();
                }
                for row in self.cursor_row.saturating_add(1)..self.rows {
                    self.cells[row] = blank_row(self.columns);
                }
            }
        }
        self.wrap_pending = false;
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "erase ranges are clamped to the fixed terminal row"
    )]
    fn erase_line(&mut self, mode: usize) {
        let (start, end) = match mode {
            1 => (0, self.cursor_column.saturating_add(1)),
            2 => (0, self.columns),
            _ => (self.cursor_column, self.columns),
        };
        for column in start..end {
            self.cells[self.cursor_row][column] = " ".to_owned();
        }
        self.wrap_pending = false;
    }

    fn reset(&mut self) {
        self.cells = blank_cells(self.rows, self.columns);
        self.cursor_row = 0;
        self.cursor_column = 0;
        self.saved_cursor = (0, 0);
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.wrap_pending = false;
    }

    fn clamp_cursor(&mut self) {
        self.cursor_row = self.cursor_row.min(self.rows.saturating_sub(1));
        self.cursor_column = self.cursor_column.min(self.columns.saturating_sub(1));
        self.wrap_pending = false;
    }
}

pub(super) struct SpawnOptions {
    pub(super) argv: Vec<OsString>,
    pub(super) cwd: PathBuf,
    pub(super) environment: BTreeMap<OsString, OsString>,
    pub(super) rows: usize,
    pub(super) columns: usize,
    pub(super) timeout: Duration,
    pub(super) output_bytes_max: usize,
    pub(super) stderr_path: Option<PathBuf>,
}

impl SpawnOptions {
    pub(super) fn new(argv: Vec<OsString>, cwd: PathBuf) -> Self {
        Self {
            argv,
            cwd,
            environment: BTreeMap::new(),
            rows: 30,
            columns: 120,
            timeout: default_timeout(),
            output_bytes_max: DEFAULT_OUTPUT_BYTES_MAX,
            stderr_path: None,
        }
    }
}

/// PTY child owner with bounded I/O, terminal replies, and process-group cleanup.
pub(super) struct PtySession {
    master: Option<File>,
    child: Option<Pid>,
    timeout: Duration,
    output_bytes_max: usize,
    output: Vec<u8>,
    screen: VirtualScreen,
}

impl PtySession {
    #[allow(
        clippy::indexing_slicing,
        reason = "openpty populates both fixed descriptor slots before they are read"
    )]
    pub(super) fn spawn(options: SpawnOptions) -> Result<Self, TaskError> {
        validate_timeout(options.timeout)?;
        validate_screen_size(options.rows, options.columns)?;
        if options.argv.is_empty() {
            return Err(invalid("PTY argv must not be empty").into());
        }
        if options.output_bytes_max == 0 {
            return Err(invalid("PTY output byte limit must be positive").into());
        }

        let program = cstring(options.argv[0].as_os_str())?;
        let arguments = options
            .argv
            .iter()
            .map(|argument| cstring(argument.as_os_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut argument_pointers = arguments
            .iter()
            .map(|argument| argument.as_ptr())
            .collect::<Vec<_>>();
        argument_pointers.push(std::ptr::null());
        let environment = options
            .environment
            .iter()
            .map(|(key, value)| {
                let mut entry = key.as_os_str().as_bytes().to_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_os_str().as_bytes());
                CString::new(entry).map_err(|_| invalid("PTY environment contains a NUL byte"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut environment_pointers = environment
            .iter()
            .map(|entry| entry.as_ptr())
            .collect::<Vec<_>>();
        environment_pointers.push(std::ptr::null());
        let cwd = cstring(options.cwd.as_os_str())?;
        let stderr = options
            .stderr_path
            .as_ref()
            .map(|path| {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)
            })
            .transpose()?;
        let winsize = Winsize {
            ws_row: u16::try_from(options.rows)?,
            ws_col: u16::try_from(options.columns)?,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let screen = VirtualScreen::new(
            options.rows,
            options.columns,
            options.rows.saturating_sub(1),
        )?;
        let output = Vec::with_capacity(options.output_bytes_max.min(64 * 1024));

        // SAFETY: xtask prepares every allocation and pointer vector before forkpty.
        // The child branch calls only async-signal-safe libc operations and then
        // execve or _exit, so it cannot observe or mutate another thread's allocator.
        let fork = unsafe { forkpty(&winsize, None) }?;
        let (master, child) = match fork {
            ForkptyResult::Child => {
                if let Some(stderr) = stderr.as_ref() {
                    // SAFETY: stderr is a live descriptor and fd 2 is the process stderr slot.
                    if unsafe { nix::libc::dup2(stderr.as_raw_fd(), nix::libc::STDERR_FILENO) } < 0
                    {
                        // SAFETY: _exit is required after a post-fork failure.
                        unsafe { nix::libc::_exit(127) };
                    }
                }
                // SAFETY: all pointers reference pre-fork CString storage that remains live
                // through execve; both pointer arrays are explicitly null-terminated.
                unsafe {
                    if nix::libc::chdir(cwd.as_ptr()) == 0 {
                        nix::libc::execve(
                            program.as_ptr(),
                            argument_pointers.as_ptr(),
                            environment_pointers.as_ptr(),
                        );
                    }
                    nix::libc::_exit(127);
                }
            }
            ForkptyResult::Parent { master, child } => (master, child),
        };
        // Own both process and descriptor before another fallible operation so
        // Drop can contain and reap the child after partial parent setup.
        let session = Self {
            master: Some(File::from(master)),
            child: Some(child),
            timeout: options.timeout,
            output_bytes_max: options.output_bytes_max,
            output,
            screen,
        };
        let master = session
            .master
            .as_ref()
            .ok_or_else(|| invalid("new PTY session has no master descriptor"))?;
        let flags = OFlag::from_bits_truncate(fcntl(master, FcntlArg::F_GETFL)?);
        fcntl(master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(session)
    }

    pub(super) fn child_pid(&self) -> Option<Pid> {
        self.child
    }

    pub(super) fn output(&self) -> &[u8] {
        &self.output
    }

    pub(super) fn screen(&self) -> &VirtualScreen {
        &self.screen
    }

    pub(super) fn foreground_group(&self) -> Result<Pid, TaskError> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("PTY master is closed"))?;
        Ok(tcgetpgrp(master)?)
    }

    pub(super) fn terminal_modes(&self) -> Result<Termios, TaskError> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("PTY master is closed"))?;
        Ok(tcgetattr(master)?)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bytes written cannot exceed the bounded caller-provided slice length"
    )]
    pub(super) fn send(&mut self, bytes: &[u8]) -> Result<(), TaskError> {
        let deadline = Instant::now() + self.timeout;
        self.send_until(bytes, deadline)
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "the write offset is bounded by the source slice length in the loop condition"
    )]
    fn send_until(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), TaskError> {
        let mut offset = 0;
        while offset < bytes.len() {
            if Instant::now() >= deadline {
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "timed out writing PTY input").into(),
                );
            }
            let master = self
                .master
                .as_mut()
                .ok_or_else(|| invalid("PTY input is closed"))?;
            match master.write(&bytes[offset..]) {
                Ok(0) => {
                    return Err(
                        io::Error::new(io::ErrorKind::BrokenPipe, "PTY input closed").into(),
                    );
                }
                Ok(written) => offset = offset.saturating_add(written),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "timed out writing PTY input",
                        )
                        .into());
                    }
                    self.poll_master(PollFlags::POLLOUT, remaining.min(Duration::from_millis(50)))?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    pub(super) fn type_text(&mut self, text: &str) -> Result<(), TaskError> {
        self.send(text.as_bytes())
    }

    pub(super) fn resize(&mut self, rows: usize, columns: usize) -> Result<(), TaskError> {
        validate_screen_size(rows, columns)?;
        self.screen.resize(rows, columns)?;
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("cannot resize a closed PTY"))?;
        let winsize = Winsize {
            ws_row: u16::try_from(rows)?,
            ws_col: u16::try_from(columns)?,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: master is a live PTY descriptor and winsize points to an
        // initialized kernel-compatible value for the duration of ioctl.
        let winsize_pointer = std::ptr::from_ref(&winsize);
        let result =
            unsafe { nix::libc::ioctl(master.as_raw_fd(), nix::libc::TIOCSWINSZ, winsize_pointer) };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[allow(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "read lengths are returned by the kernel for the fixed buffer and retained bytes are explicitly bounded"
    )]
    pub(super) fn drain_for(&mut self, duration: Duration) -> Result<Vec<u8>, TaskError> {
        validate_timeout(duration)?;
        let deadline = Instant::now() + duration;
        let mut chunk = Vec::new();
        while self.master.is_some() && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !self.poll_master(PollFlags::POLLIN, remaining.min(Duration::from_millis(50)))? {
                continue;
            }
            let mut buffer = vec![0_u8; READ_BYTES_MAX];
            let master = self
                .master
                .as_mut()
                .ok_or_else(|| invalid("PTY master is closed"))?;
            let read = match master.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(nix::libc::EIO) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let observed = self.output.len().saturating_add(read);
            if observed > self.output_bytes_max {
                return Err(io::Error::other(format!(
                    "PTY output exceeded its byte limit; observed={observed} limit={}",
                    self.output_bytes_max
                ))
                .into());
            }
            chunk.extend_from_slice(&buffer[..read]);
            self.output.extend_from_slice(&buffer[..read]);
            let replies = self.screen.feed(&buffer[..read]);
            send_terminal_replies(replies, self.timeout, Instant::now, |reply, deadline| {
                self.send_until(reply, deadline)
            })?;
        }
        Ok(chunk)
    }

    pub(super) fn wait_for(&mut self, marker: &[u8]) -> Result<Vec<u8>, TaskError> {
        self.wait_for_since(marker, self.output.len(), self.timeout)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the wait deadline"
    )]
    pub(super) fn wait_for_since(
        &mut self,
        marker: &[u8],
        start: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, TaskError> {
        validate_timeout(timeout)?;
        let deadline = Instant::now() + timeout;
        while !contains_bytes(self.output.get(start..).unwrap_or_default(), marker)
            && Instant::now() < deadline
        {
            self.drain_for(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100))
                    .max(Duration::from_millis(1)),
            )?;
        }
        let observed = self.output.get(start..).unwrap_or_default().to_vec();
        if !contains_bytes(&observed, marker) {
            return Err(self.timeout_error(&format!("output marker {marker:?}"), &observed));
        }
        Ok(observed)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the wait deadline"
    )]
    pub(super) fn wait_for_screen(
        &mut self,
        description: &str,
        predicate: impl Fn(&VirtualScreen) -> bool,
    ) -> Result<String, TaskError> {
        let deadline = Instant::now() + self.timeout;
        while !predicate(&self.screen) && Instant::now() < deadline {
            self.drain_for(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100))
                    .max(Duration::from_millis(1)),
            )?;
        }
        let snapshot = self.screen.text();
        if !predicate(&self.screen) {
            return Err(self.timeout_error(&format!("screen state {description}"), &[]));
        }
        Ok(snapshot)
    }

    pub(super) fn wait_for_screen_text(&mut self, marker: &str) -> Result<String, TaskError> {
        self.wait_for_screen(&format!("{marker:?}"), |screen| {
            screen.text().contains(marker)
        })
    }

    pub(super) fn wait_exit(&mut self) -> Result<i32, TaskError> {
        self.wait_exit_within(self.timeout)
    }

    /// Wait for the child to exit within an explicit, shorter-than-usual
    /// deadline rather than the session's configured [`Self::timeout`].
    ///
    /// Meant for a bounded probe before a defensive retry (for example
    /// resending a keystroke that a still-alive child may have swallowed as
    /// something other than the intended action) that itself still has the
    /// full session timeout to work with.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the process deadline"
    )]
    pub(super) fn wait_exit_within(&mut self, timeout: Duration) -> Result<i32, TaskError> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.drain_for(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50))
                    .max(Duration::from_millis(1)),
            )?;
            if let Some(status) = self.try_reap()? {
                return Ok(wait_status_code(status));
            }
        }
        Err(self.timeout_error("child exit", &[]))
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the close deadline"
    )]
    pub(super) fn close(&mut self) -> Result<(), TaskError> {
        if let Some(master) = self.master.as_ref()
            && let Ok(group) = tcgetpgrp(master)
            && group.as_raw() > 0
            && group != getpgrp()
        {
            kill_group(group);
        }
        if let Some(child) = self.child {
            kill_group(child);
            let _ = kill(child, Signal::SIGKILL);
        }
        self.master.take();
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        while self.child.is_some() && Instant::now() < deadline {
            if self.try_reap()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(child) = self.child {
            return Err(io::Error::other(format!(
                "PTY child {} could not be reaped after SIGKILL",
                child.as_raw()
            ))
            .into());
        }
        Ok(())
    }

    fn poll_master(&self, flags: PollFlags, timeout: Duration) -> Result<bool, TaskError> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("PTY master is closed"))?;
        let mut descriptors = [PollFd::new(master.as_fd(), flags)];
        let timeout = PollTimeout::try_from(timeout)?;
        Ok(poll(&mut descriptors, timeout)? > 0)
    }

    fn try_reap(&mut self) -> Result<Option<WaitStatus>, TaskError> {
        let Some(child) = self.child else {
            return Ok(None);
        };
        match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => Ok(None),
            Ok(status) => {
                self.child = None;
                Ok(Some(status))
            }
            Err(Errno::ECHILD) => {
                self.child = None;
                Ok(Some(WaitStatus::Exited(child, 0)))
            }
            Err(error) => Err(error.into()),
        }
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "diagnostic output is sliced at a byte count clamped to the observed buffer length"
    )]
    fn timeout_error(&self, expected: &str, observed: &[u8]) -> TaskError {
        let source = if observed.is_empty() {
            &self.output
        } else {
            observed
        };
        let tail = &source[source.len().saturating_sub(1_000)..];
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for {expected}; raw_tail={tail:?}; screen=\n{}; child_state=\n{}; process_tree=\n{}",
                self.screen.text(),
                self.child.map_or_else(
                    || "(no tracked child)".to_owned(),
                    |pid| child_kernel_state(pid.as_raw()),
                ),
                process_tree_snapshot()
            ),
        )
        .into()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(target_os = "linux")]
const CHILD_THREADS_MAX: usize = 64;

/// Best-effort per-thread kernel sleep state for the harness's own tracked
/// child, read straight from Linux's `/proc` (a no-op elsewhere: macOS has
/// no equivalent, and this is diagnostic only).
///
/// `wchan` names the kernel function a thread is blocked in, which turns "a
/// wait timed out" into either "genuinely still computing" (short or empty
/// wchan, high recent CPU) or "asleep waiting on something that never
/// arrived" (a wchan like `futex_wait_queue` or `pipe_read`) without needing
/// a live debugger attached to the CI runner.
#[cfg(target_os = "linux")]
fn child_kernel_state(pid: i32) -> String {
    let task_directory = PathBuf::from(format!("/proc/{pid}/task"));
    let Ok(entries) = std::fs::read_dir(&task_directory) else {
        return format!("(could not read {})", task_directory.display());
    };
    let mut lines = Vec::new();
    for entry in entries.filter_map(Result::ok).take(CHILD_THREADS_MAX) {
        let tid = entry.file_name();
        let wchan = std::fs::read_to_string(entry.path().join("wchan"))
            .unwrap_or_else(|error| format!("(unreadable: {error})"));
        let comm = std::fs::read_to_string(entry.path().join("comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let state = std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|stat| {
                stat.split(')')
                    .nth(1)?
                    .split_whitespace()
                    .next()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "?".to_owned());
        lines.push(format!(
            "tid={} comm={comm:?} state={state} wchan={wchan}",
            tid.to_string_lossy()
        ));
    }
    if lines.is_empty() {
        return format!("(no threads found under {})", task_directory.display());
    }
    lines.join("\n")
}

#[cfg(not(target_os = "linux"))]
fn child_kernel_state(_pid: i32) -> String {
    "(child kernel state is only available on Linux)".to_owned()
}

const PROCESS_TREE_LINES_MAX: usize = 200;

/// Best-effort `ps -ef` snapshot attached to a timeout diagnostic, filtered
/// to userspace processes.
///
/// A PTY wait timing out this test harness could mean the harness itself is
/// just slow under CI load, or that a child (or something it spawned) is
/// genuinely still alive and blocking cleanup; this distinguishes the two
/// without needing to reproduce the hang interactively. On a real VM-backed
/// CI runner (unlike a container) `ps -ef` lists the whole host, hundreds of
/// bracketed kernel threads (`[kworker/...]`) and all; drop those so the
/// bound below is spent on the processes that could actually matter here.
/// Never fails the check itself: a `ps` failure just yields a short
/// diagnostic string.
fn process_tree_snapshot() -> String {
    match std::process::Command::new("ps").arg("-ef").output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut lines = text.lines();
            let header = lines.next().unwrap_or_default();
            let mut kept = vec![header.to_owned()];
            kept.extend(
                lines
                    .filter(|line| {
                        line.split_whitespace()
                            .last()
                            .is_none_or(|command| !command.starts_with('['))
                    })
                    .take(PROCESS_TREE_LINES_MAX)
                    .map(str::to_owned),
            );
            kept.join("\n")
        }
        Err(error) => format!("(ps -ef unavailable: {error})"),
    }
}

fn blank_cells(rows: usize, columns: usize) -> Vec<Vec<String>> {
    (0..rows).map(|_| blank_row(columns)).collect()
}

fn blank_row(columns: usize) -> Vec<String> {
    vec![" ".to_owned(); columns]
}

fn validate_timeout(timeout: Duration) -> io::Result<()> {
    if timeout.is_zero() || timeout > Duration::from_secs(150) {
        return Err(invalid(
            "PTY timeout must be greater than zero and at most 150 seconds",
        ));
    }
    Ok(())
}

fn validate_screen_size(rows: usize, columns: usize) -> io::Result<()> {
    if rows == 0
        || columns == 0
        || rows
            .checked_mul(columns)
            .is_none_or(|cells| cells > SCREEN_CELLS_MAX)
    {
        return Err(invalid(format!(
            "PTY screen dimensions must be positive and retain at most {SCREEN_CELLS_MAX} cells"
        )));
    }
    Ok(())
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| invalid("PTY value contains a NUL byte"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn csi_parameters(control: &str) -> Vec<usize> {
    let parameters = control
        .split_once(' ')
        .map_or(control, |(values, _)| values);
    if parameters.is_empty() {
        return Vec::new();
    }
    parameters
        .split(';')
        .map(|value| value.parse().unwrap_or(0))
        .collect()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "reply byte counts are bounded by the captured PTY byte limit"
)]
fn send_terminal_replies<E>(
    replies: impl IntoIterator<Item = Vec<u8>>,
    send_timeout: Duration,
    mut now: impl FnMut() -> Instant,
    mut send_until: impl FnMut(&[u8], Instant) -> Result<(), E>,
) -> Result<(), E> {
    for reply in replies {
        let deadline = now() + send_timeout;
        send_until(&reply, deadline)?;
    }
    Ok(())
}

#[allow(
    clippy::as_conversions,
    reason = "nix Signal is a repr(i32) wrapper over the platform signal number"
)]
fn wait_status_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, signal, _) => 128_i32.saturating_add(signal as i32),
        _ => 1,
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "negating a valid positive child process group identifier is required by POSIX kill"
)]
fn kill_group(group: Pid) {
    let raw = group.as_raw();
    if raw > 0 {
        let _ = kill(Pid::from_raw(-raw), Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "quirl-{label}-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn default_timeout_is_more_generous_under_github_actions() {
        let expected = if std::env::var_os("GITHUB_ACTIONS").is_some() {
            Duration::from_secs(100)
        } else {
            Duration::from_secs(15)
        };
        assert_eq!(default_timeout(), expected);
    }

    #[test]
    fn cursor_query_reports_modeled_bottom_row() {
        let mut screen = VirtualScreen::new(6, 20, 5).unwrap();
        assert_eq!(screen.feed(b"\x1b[6n"), [b"\x1b[6;1R".to_vec()]);
    }

    #[test]
    fn chunked_utf8_and_cursor_sequences_update_visible_cells() {
        let mut screen = VirtualScreen::new(4, 12, 0).unwrap();
        screen.feed(b"old\x1b[2");
        screen.feed(b"J\x1b[2;3Hdata \xe2");
        screen.feed(b"\x97\x86");
        assert_eq!(screen.lines()[0], "");
        assert_eq!(screen.lines()[1], "  data ◆");
    }

    #[test]
    fn clear_to_end_removes_expanded_picker_rows() {
        let mut screen = VirtualScreen::new(6, 20, 0).unwrap();
        screen.feed(b"\x1b[1;1Hpicker\x1b[6;1Hdata | results");
        screen.feed(b"\x1b[1;1H\x1b[Jcommand\x1b[6;1Hcommand | ready");
        assert!(!screen.text().contains("picker"));
        assert_eq!(screen.bottom_line(), "command | ready");
    }

    #[test]
    fn huge_scroll_count_is_bounded_by_the_screen_height() {
        let mut screen = VirtualScreen::new(4, 20, 0).unwrap();
        screen.feed(b"marker\x1b[999999999S");
        assert!(screen.lines().iter().all(String::is_empty));
    }

    #[test]
    fn close_kills_and_reaps_the_session_leader() {
        let directory = test_directory("pty-close");
        let mut options = SpawnOptions::new(
            ["/bin/sh", "-c", "printf READY; sleep 30"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options
            .environment
            .insert("TERM".into(), "xterm-256color".into());
        options.rows = 8;
        options.columns = 40;
        options.timeout = Duration::from_secs(2);
        options.output_bytes_max = 4_096;
        let mut session = PtySession::spawn(options).unwrap();
        let child = session.child_pid().unwrap();
        session.wait_for(b"READY").unwrap();
        session.close().unwrap();
        assert!(matches!(
            waitpid(child, Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn output_limit_fails_before_unbounded_retention() {
        let directory = test_directory("pty-limit");
        let mut options = SpawnOptions::new(
            ["/bin/sh", "-c", "while :; do printf 1234567890; done"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options
            .environment
            .insert("TERM".into(), "xterm-256color".into());
        options.rows = 8;
        options.columns = 40;
        options.timeout = Duration::from_secs(2);
        options.output_bytes_max = 1_024;
        let mut session = PtySession::spawn(options).unwrap();
        let error = session.drain_for(Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("output exceeded its byte limit"));
        assert!(session.output().len() <= 1_024);
        session.close().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cursor_reply_discovered_at_drain_deadline_gets_an_independent_send_deadline() {
        let mut screen = VirtualScreen::new(8, 40, 7).unwrap();
        let replies = screen.feed(b"\x1b[6n");
        let drain_deadline = Instant::now();
        let send_timeout = Duration::from_millis(250);
        let mut observed = Vec::new();

        send_terminal_replies(
            replies,
            send_timeout,
            || drain_deadline,
            |reply, deadline| {
                observed.push((reply.to_vec(), deadline));
                Ok::<(), io::Error>(())
            },
        )
        .unwrap();

        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].0, b"\x1b[8;1R");
        assert_eq!(observed[0].1.duration_since(drain_deadline), send_timeout);
    }

    #[test]
    fn blocked_terminal_reply_times_out_at_the_configured_send_bound() {
        let directory = test_directory("pty-reply-deadline");
        let send_timeout = Duration::from_millis(100);
        let mut options = SpawnOptions::new(
            ["/bin/sh", "-c", "stty raw -echo; printf READY; sleep 30"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options.timeout = send_timeout;
        let mut session = PtySession::spawn(options).unwrap();
        session.wait_for(b"READY").unwrap();
        let input = [b'x'; 4_096];
        let mut written = 0_usize;
        loop {
            assert!(
                written < 16 * 1024 * 1024,
                "PTY input did not become blocked"
            );
            let master = session.master.as_mut().unwrap();
            match master.write(&input) {
                Ok(count) => written = written.saturating_add(count),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("failed while filling PTY input: {error}"),
            }
        }

        // A `WouldBlock` on a 4 KiB write does not guarantee a much smaller
        // write stays blocked an instant later: on Linux the tty output
        // queue's low-water mark can free enough room, between the fill
        // loop above and the timed `send` below, for a 5-byte payload to
        // fit even though the queue is still effectively full. Measure the
        // timeout with a payload the same size as what actually established
        // `WouldBlock`, so the same fullness that blocked the fill loop
        // still blocks this write.
        let started = Instant::now();
        let error = session.send(&input).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::TimedOut)
        );
        assert!(elapsed >= send_timeout);
        assert!(elapsed < send_timeout + Duration::from_millis(250));
        session.close().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
