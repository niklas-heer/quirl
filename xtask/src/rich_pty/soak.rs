//! Replayable real-keyboard journeys through bounded, isolated rich sessions.
//!
//! Failure model: a key may be dropped, an overlay may steal later input, a
//! repaint may show stale output, or a child may stop responding. Each journey
//! has a unique visible token; actions are journaled before delivery and screen
//! oracles inspect modeled cells. Every wait has an absolute deadline, each PTY
//! retains at most its existing 16 MiB cap, and sessions own temporary state and
//! child cleanup through RAII. A failed session is closed before the next starts.
//! The executable snapshot outlives all children. Reports are private, bounded,
//! created without replacement, and the summary is written after session logs.
//!
//! Resource model: at most 1,000 sessions × 200 journeys, one live PTY, 180 s per
//! session work deadline, 2 h per run, 20 failed sessions, 1 MiB per action journal, and 256 MiB
//! of run artifacts. Screens are 24–40 rows × 72–120 columns. Representative
//! SVGs show styled VT cells with a fixed palette, not terminal pixel fidelity.
//! Existing bounded PTY writes and cleanup may finish after a work deadline;
//! no new action starts once that deadline has expired.
//! Sixty completed journeys represent one modeled active-use hour; removed
//! think time does not constitute equivalent real-time endurance evidence.

use super::{
    Session, SessionOptions, VirtualScreen, create_private_directory, ensure_terminal_restored, key,
};
use crate::{TaskError, simulation::PinnedExecutable};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const SESSIONS_MAX: usize = 1_000;
const JOURNEYS_MAX: usize = 200;
const SESSION_TIME_MAX: Duration = Duration::from_secs(180);
const RUN_TIME_MAX: Duration = Duration::from_secs(2 * 60 * 60);
const ACTION_TIMEOUT: Duration = Duration::from_secs(8);
const POLL_INTERVAL: Duration = Duration::from_millis(16);
const TRACE_BYTES_MAX: usize = 1024 * 1024;
const ARTIFACT_BYTES_MAX: u64 = 256 * 1024 * 1024;
const SUMMARY_RESERVE_BYTES: u64 = 2 * 1024 * 1024;
const FAILURES_MAX: u64 = 20;
const INPUT_BYTES_MAX: usize = 1024;
const FEATURE_COUNT: usize = 12;
const READY_INPUT: &[u8] = b"\x1b[?1000h";

/// Validated bounds and deterministic replay selection for one soak run.
pub(crate) struct SoakOptions {
    pub seed: u64,
    pub sessions: usize,
    pub journeys_per_session: usize,
    pub only_session: Option<usize>,
    pub output: PathBuf,
}

/// Actual observed work, binary identity, and explicitly modeled usage time.
#[derive(Debug, Serialize)]
pub(crate) struct SoakSummary {
    pub schema_version: u32,
    pub seed: u64,
    pub sessions_requested: u64,
    pub sessions_attempted: u64,
    pub sessions_completed: u64,
    pub journeys_per_session: u64,
    pub journeys_attempted: u64,
    pub journeys_completed: u64,
    pub key_bytes: u64,
    pub actions: u64,
    pub screen_assertions: u64,
    pub resize_count: u64,
    pub failure_count: u64,
    pub failures: Vec<String>,
    pub wall_ms: u64,
    pub modeled_hours: f64,
    pub journeys_per_modeled_hour: u32,
    pub stopped_reason: Option<String>,
    pub binary_source: PathBuf,
    pub binary_sha256: String,
    pub binary_bytes: u64,
    pub report_directory: PathBuf,
    pub feature_counts: BTreeMap<String, u64>,
    pub limitations: &'static str,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Journey {
    Backspace,
    CursorDelete,
    UnicodeDelete,
    CancelRecovery,
    History,
    ContextHelp,
    DataMode,
    ErrorRecovery,
    Resize,
    Completion,
    FilePicker,
    Palette,
}

impl Journey {
    const ALL: [Self; FEATURE_COUNT] = [
        Self::Backspace,
        Self::CursorDelete,
        Self::UnicodeDelete,
        Self::CancelRecovery,
        Self::History,
        Self::ContextHelp,
        Self::DataMode,
        Self::ErrorRecovery,
        Self::Resize,
        Self::Completion,
        Self::FilePicker,
        Self::Palette,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Backspace => "backspace",
            Self::CursorDelete => "cursor_delete",
            Self::UnicodeDelete => "unicode_delete",
            Self::CancelRecovery => "cancel_recovery",
            Self::History => "history",
            Self::ContextHelp => "context_help",
            Self::DataMode => "data_mode",
            Self::ErrorRecovery => "error_recovery",
            Self::Resize => "resize",
            Self::Completion => "completion",
            Self::FilePicker => "file_picker",
            Self::Palette => "palette",
        }
    }
}

struct Generator(u64);
impl Generator {
    fn for_session(seed: u64, session: usize) -> Self {
        let mixed = seed
            ^ u64::try_from(session)
                .unwrap_or(u64::MAX)
                .wrapping_add(1)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15);
        Self(if mixed == 0 {
            0xa076_1d64_78bd_642f
        } else {
            mixed
        })
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Every complete block covers every workflow, with seeded ordering and replay
/// independent of earlier sessions. No wall-clock randomness affects journeys.
fn plan(seed: u64, session: usize, count: usize) -> Vec<Journey> {
    let mut generator = Generator::for_session(seed, session);
    let mut result = Vec::with_capacity(count);
    while result.len() < count {
        let mut block = Journey::ALL;
        for index in (1..FEATURE_COUNT).rev() {
            let modulus = u64::try_from(index.saturating_add(1)).unwrap_or(u64::MAX);
            let selected =
                usize::try_from(generator.next().checked_rem(modulus).unwrap_or(0)).unwrap_or(0);
            block.swap(index, selected);
        }
        result.extend(block.into_iter().take(count.saturating_sub(result.len())));
    }
    result
}

struct Artifacts {
    root: PathBuf,
    bytes: u64,
}

impl Artifacts {
    fn new(parent: &Path, seed: u64) -> io::Result<Self> {
        fs::create_dir_all(parent)?;
        for attempt in 0..64 {
            let root = parent.join(format!("seed-{seed}-{}-{attempt}", std::process::id()));
            match create_private_directory(&root) {
                Ok(()) => {
                    return Ok(Self {
                        root: fs::canonicalize(root)?,
                        bytes: 0,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::other(
            "cannot reserve a private soak report directory after 64 attempts",
        ))
    }

    fn admit(&mut self, bytes: usize) -> io::Result<()> {
        let observed = self
            .bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        if observed > ARTIFACT_BYTES_MAX.saturating_sub(SUMMARY_RESERVE_BYTES) {
            return Err(io::Error::other(format!(
                "soak artifact limit exceeded: {observed} > {ARTIFACT_BYTES_MAX}"
            )));
        }
        self.bytes = observed;
        Ok(())
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.admit(bytes.len())?;
        let mut file = private_file(path)?;
        file.write_all(bytes)?;
        file.flush()
    }

    fn summary(&mut self, path: &Path, value: &impl Serialize) -> Result<(), TaskError> {
        let bytes = serde_json::to_vec_pretty(value)?;
        let observed = self.bytes.saturating_add(u64::try_from(bytes.len())?);
        if bytes.len() > 64 * 1024 || observed > ARTIFACT_BYTES_MAX {
            return Err(io::Error::other("reserved soak summary budget exceeded").into());
        }
        self.bytes = observed;
        let mut file = private_file(path)?;
        file.write_all(&bytes)?;
        file.flush()?;
        Ok(())
    }

    fn json(&mut self, path: &Path, value: &impl Serialize) -> Result<(), TaskError> {
        self.write(path, &serde_json::to_vec_pretty(value)?)?;
        Ok(())
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[derive(Default, Serialize)]
struct SessionStats {
    session: usize,
    journeys_attempted: u64,
    journeys_completed: u64,
    key_bytes: u64,
    actions: u64,
    screen_assertions: u64,
    resize_count: u64,
    wall_ms: u64,
    failure: Option<String>,
    evidence_errors: Vec<String>,
}

struct Driver<'a> {
    session: Session,
    trace: File,
    trace_bytes: usize,
    started: Instant,
    deadline: Instant,
    stats: SessionStats,
    journey_index: usize,
    feature: &'static str,
    output: PathBuf,
    artifacts: &'a mut Artifacts,
    gallery: &'a mut BTreeSet<&'static str>,
}

impl Driver<'_> {
    fn record(&mut self, kind: &str, detail: serde_json::Value) -> Result<(), TaskError> {
        self.check_time()?;
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "session": self.stats.session, "journey": self.journey_index,
            "feature": self.feature, "action": self.stats.actions,
            "elapsed_ms": millis(self.started.elapsed()), "kind": kind, "detail": detail,
        }))?;
        bytes.push(b'\n');
        let observed = self.trace_bytes.saturating_add(bytes.len());
        if observed > TRACE_BYTES_MAX {
            return Err(io::Error::other("session action journal exceeded 1 MiB").into());
        }
        self.artifacts.admit(bytes.len())?;
        self.trace.write_all(&bytes)?;
        // Flushing before delivery retains the last attempted input on failure.
        self.trace.flush()?;
        self.trace_bytes = observed;
        self.stats.actions = self.stats.actions.saturating_add(1);
        Ok(())
    }

    fn check_time(&self) -> Result<(), TaskError> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::other("soak session/run wall deadline exceeded").into());
        }
        Ok(())
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), TaskError> {
        if bytes.len() > INPUT_BYTES_MAX {
            return Err(io::Error::other("soak input action exceeded 1024 bytes").into());
        }
        self.record("keys", serde_json::json!({"bytes": bytes}))?;
        self.session.pty.send(bytes)?;
        self.stats.key_bytes = self
            .stats
            .key_bytes
            .saturating_add(u64::try_from(bytes.len())?);
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<(), TaskError> {
        self.send(text.as_bytes())
    }

    fn wait(
        &mut self,
        description: &str,
        predicate: impl Fn(&VirtualScreen) -> bool,
    ) -> Result<(), TaskError> {
        self.record("screen_oracle", serde_json::json!({"expect": description}))?;
        let deadline = Instant::now()
            .checked_add(ACTION_TIMEOUT)
            .unwrap_or(self.deadline)
            .min(self.deadline);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(
                    io::Error::other(format!("screen oracle timed out: {description}")).into(),
                );
            }
            self.session.pty.drain_for(remaining.min(POLL_INTERVAL))?;
            if predicate(self.session.pty.screen()) {
                self.stats.screen_assertions = self.stats.screen_assertions.saturating_add(1);
                return Ok(());
            }
        }
    }

    fn wait_token(&mut self, token: &str) -> Result<(), TaskError> {
        self.wait(
            &format!("completed visible output line {token}"),
            |screen| {
                screen
                    .lines()
                    .iter()
                    .any(|line| is_output_line(line, token, screen.columns()))
                    && screen.bottom_line().contains("NORMAL")
            },
        )
    }

    fn execute(&mut self, command: &str, token: &str) -> Result<(), TaskError> {
        self.type_text(command)?;
        self.enter(token)
    }

    fn enter(&mut self, token: &str) -> Result<(), TaskError> {
        let output_start = self.session.pty.output().len();
        self.send(key::ENTER)?;
        self.wait_token(token)?;
        // Unique output alone could paint while execution still owns input.
        // Await a fresh input-mode restoration as well, never an extra Enter.
        self.wait_ready(output_start, token)
    }

    fn wait_ready(&mut self, output_start: usize, token: &str) -> Result<(), TaskError> {
        self.record("input_ready", serde_json::json!({"token": token}))?;
        let deadline = Instant::now()
            .checked_add(ACTION_TIMEOUT)
            .unwrap_or(self.deadline)
            .min(self.deadline);
        while !self
            .session
            .pty
            .output()
            .get(output_start..)
            .unwrap_or_default()
            .windows(READY_INPUT.len())
            .any(|window| window == READY_INPUT)
        {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::other(
                    "command output appeared without restoring editor input",
                )
                .into());
            }
            self.session.pty.drain_for(remaining.min(POLL_INTERVAL))?;
        }
        Ok(())
    }

    fn checkpoint(&mut self, label: &'static str) -> Result<(), TaskError> {
        if self.gallery.insert(label) {
            let result = (|| -> Result<(), TaskError> {
                // A content oracle can match halfway through a VT frame. Wait for
                // Ratatui's final cursor placement before publishing visual evidence.
                self.wait(
                    "completed render frame before visual checkpoint",
                    |screen| screen.has_completed_frame(),
                )?;
                let base = self.artifacts.root.join(format!("screen-{label}"));
                self.artifacts.write(
                    &base.with_extension("txt"),
                    self.session.pty.screen().text().as_bytes(),
                )?;
                self.artifacts.write(
                    &base.with_extension("svg"),
                    self.session.pty.screen().to_svg()?.as_bytes(),
                )?;
                Ok(())
            })();
            if result.is_err() {
                self.gallery.remove(label);
            }
            return result;
        }
        Ok(())
    }

    fn resize(&mut self, rows: usize, columns: usize) -> Result<(), TaskError> {
        self.record(
            "resize",
            serde_json::json!({"rows": rows, "columns": columns}),
        )?;
        self.stats.resize_count = self.stats.resize_count.saturating_add(1);
        self.session.pty.resize(rows, columns)
    }

    fn journey(&mut self, kind: Journey, token: &str) -> Result<(), TaskError> {
        self.feature = kind.name();
        self.stats.journeys_attempted = self.stats.journeys_attempted.saturating_add(1);
        self.record("journey_begin", serde_json::json!({"token": token}))?;
        match kind {
            Journey::Backspace => {
                self.type_text(&format!("/usr/bin/printf '{token}_BAD\\n'"))?;
                self.send(b"\x1b[D\x1b[D\x1b[D\x7f\x7f\x7f")?;
                self.type_text("OK")?;
                self.send(b"\x05")?;
                self.enter(&format!("{token}_OK"))?;
            }
            Journey::CursorDelete => {
                self.type_text(&format!("/usr/bin/printf {token}XOK"))?;
                self.send(b"\x1b[D\x1b[D\x1b[D\x1b[3~")?;
                self.send(b"\x05")?;
                self.enter(&format!("{token}OK"))?;
            }
            Journey::UnicodeDelete => {
                self.type_text(&format!("/usr/bin/printf {token}e\u{301}"))?;
                self.send(b"\x7f")?;
                self.enter(token)?;
            }
            Journey::CancelRecovery => {
                self.type_text(&format!("/usr/bin/printf NEVER_{token}"))?;
                self.send(key::CTRL_C)?;
                self.wait("cancel returns to normal editor", |screen| {
                    screen.bottom_line().contains("NORMAL") && screen.text().contains("^C")
                })?;
                self.execute(&print_command(token), token)?;
                if self.session.pty.screen().lines().iter().any(|line| {
                    is_output_line(
                        line,
                        &format!("NEVER_{token}"),
                        self.session.pty.screen().columns(),
                    )
                }) {
                    return Err(io::Error::other("cancelled input executed").into());
                }
            }
            Journey::History => self.history(token)?,
            Journey::ContextHelp => self.context_help(token)?,
            Journey::DataMode => self.data_mode(token)?,
            Journey::ErrorRecovery => {
                self.type_text(&format!("quirl_missing_{token}"))?;
                let output_start = self.session.pty.output().len();
                self.send(key::ENTER)?;
                self.wait("missing command error is visible", |screen| {
                    screen
                        .lines()
                        .iter()
                        .any(|line| line.contains(token) && line.contains("could not start"))
                })?;
                self.wait_ready(output_start, token)?;
                self.checkpoint("error-recovery")?;
                self.execute(&print_command(token), token)?;
            }
            Journey::Resize => {
                self.type_text(&print_command(token))?;
                self.resize(24, 72)?;
                self.wait("narrow resized editor retains unique input", |screen| {
                    screen.text().contains(token) && screen.bottom_line().contains("NORMAL")
                })?;
                self.checkpoint("narrow-editor")?;
                self.resize(40, 120)?;
                self.wait(
                    "wide frame acknowledges resize before execution",
                    |screen| {
                        screen.text().contains(token) && screen.bottom_line().contains("NORMAL")
                    },
                )?;
                self.enter(token)?;
            }
            Journey::Completion => self.complete_file(token)?,
            Journey::FilePicker => self.pick_file(token)?,
            Journey::Palette => {
                self.type_text(&print_command(token))?;
                self.send(key::ALT_Q)?;
                self.send(b"p")?;
                self.wait("palette is visible over unfinished command", |screen| {
                    screen.text().contains("picker") && screen.text().contains(token)
                })?;
                self.type_text("doctor")?;
                self.wait("palette search selects catalog command", |screen| {
                    selected_palette_command(screen, "quirl config doctor")
                })?;
                self.checkpoint("palette")?;
                self.send(key::ESCAPE)?;
                self.wait("palette dismissal preserves unfinished command", |screen| {
                    let text = screen.text();
                    text.contains(token)
                        && !text.contains("┌ picker")
                        && !screen.bottom_line().contains("Enter accept")
                })?;
                self.enter(token)?;
            }
        }
        self.checkpoint(kind.name())?;
        self.stats.journeys_completed = self.stats.journeys_completed.saturating_add(1);
        self.record("journey_complete", serde_json::json!({"token": token}))
    }

    fn history(&mut self, token: &str) -> Result<(), TaskError> {
        self.execute(&print_command(token), token)?;
        self.send(b"\x12")?;
        self.type_text(token)?;
        self.wait("history query recalls unique command", |screen| {
            screen.text().contains("history") && screen.text().contains(token)
        })?;
        self.checkpoint("history-open")?;
        self.send(key::ENTER)?;
        self.send(b"\x05")?;
        // Replace the whole recalled command's trailing unquoted token, giving
        // the second execution a fresh screen oracle rather than an old match.
        self.type_text("B")?;
        self.enter(&format!("{token}B"))
    }

    fn context_help(&mut self, token: &str) -> Result<(), TaskError> {
        self.type_text(&format!("git status | quirl data {token}"))?;
        self.send(b"\x1bOP")?;
        self.wait("F1 follows current pipeline stage", |screen| {
            let text = screen.text();
            text.contains(token) && text.contains("> quirl data") && text.contains("Sources are")
        })?;
        self.checkpoint("help-open")?;
        self.send(key::ESCAPE)?;
        self.wait("context help dismissed without changing input", |screen| {
            let text = screen.text();
            text.contains(token)
                && !text.contains("┌ picker")
                && !screen.bottom_line().contains("Enter accept")
        })?;
        self.send(key::CTRL_C)?;
        self.execute(&print_command(token), token)
    }

    fn data_mode(&mut self, token: &str) -> Result<(), TaskError> {
        self.send(key::ALT_Q)?;
        self.send(b"d")?;
        self.wait("Data mode visible", |screen| {
            screen.bottom_line().contains("DATA")
        })?;
        let excluded = format!("EXCLUDED_{token}");
        self.type_text(&format!(r#"[{{"name":"{token}","status":"failed"}},{{"name":"{excluded}","status":"healthy"}}] | where status == "failed" | select name"#))?;
        let output_start = self.session.pty.output().len();
        self.send(key::ENTER)?;
        self.wait("filtered typed result visible in Data mode", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| is_data_row(line, token, screen.columns()))
                && screen.bottom_line().contains("DATA")
        })?;
        self.wait_ready(output_start, token)?;
        self.record(
            "screen_oracle",
            serde_json::json!({"expect": "filtered-out row is absent after execution"}),
        )?;
        if self
            .session
            .pty
            .screen()
            .lines()
            .iter()
            .any(|line| is_data_row(line, &excluded, self.session.pty.screen().columns()))
        {
            return Err(io::Error::other("typed where returned its excluded row").into());
        }
        self.stats.screen_assertions = self.stats.screen_assertions.saturating_add(1);
        self.checkpoint("typed-data")?;
        self.send(key::ALT_Q)?;
        self.send(b"n")?;
        self.wait("Normal mode restored", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
        self.execute(&print_command(token), token)
    }

    fn complete_file(&mut self, token: &str) -> Result<(), TaskError> {
        let first = format!("{token}-a.txt");
        let second = format!("{token}-b.txt");
        fs::write(self.session.private.path.join(&first), format!("{token}\n"))?;
        fs::write(
            self.session.private.path.join(&second),
            b"WRONG_COMPLETION\n",
        )?;
        self.type_text(&format!("cat {token}"))?;
        self.send(key::TAB)?;
        self.wait("first completion selected", |screen| {
            selected_file(screen, &first)
        })?;
        self.send(b"\x1b[B")?;
        self.wait("Down selects second completion", |screen| {
            selected_file(screen, &second)
        })?;
        self.send(b"\x1b[A")?;
        self.wait("Up returns to first completion", |screen| {
            selected_file(screen, &first)
        })?;
        self.checkpoint("completion-open")?;
        self.send(key::ENTER)?;
        let editor_line = format!("cat {first}");
        self.wait("completion inserted before execution", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.trim_start_matches('>').trim() == editor_line)
        })?;
        self.enter(token)
    }

    fn pick_file(&mut self, token: &str) -> Result<(), TaskError> {
        let (name, query) = if self.journey_index.is_multiple_of(2) {
            ("-n", "-n")
        } else {
            ("quarterly report;$notes.txt", "quarterly")
        };
        fs::write(self.session.private.path.join(name), format!("{token}\n"))?;
        self.type_text("cat ")?;
        self.send(key::ALT_Q)?;
        self.send(b"f")?;
        self.wait("file picker opens", |screen| {
            screen.text().contains("picker")
        })?;
        self.type_text(query)?;
        self.wait("file picker selects intended filesystem entry", |screen| {
            // The query itself may equal the name (notably `-n`). Require the
            // selected item's filesystem provenance and actual path in the
            // documentation pane before accepting, rather than echo alone.
            let text = screen.text();
            text.contains("source: filesystem") && text.contains(&format!("./{name}"))
        })?;
        self.checkpoint("file-picker-open")?;
        self.send(key::ENTER)?;
        self.wait("file selection returned to command editor", |screen| {
            !screen.bottom_line().contains("Enter accept") && screen.text().contains("cat ")
        })?;
        self.enter(token)
    }
}

// The transcript's last cell can be a plain-symbol scrollbar after it fills.
// Accept that known UI chrome, but reject command headers, token prefixes, or
// additional output words; the unique token must be the complete output value.
fn is_output_line(line: &str, token: &str, columns: usize) -> bool {
    line.strip_prefix(token).is_some_and(|suffix| {
        suffix.trim().is_empty()
            || (unicode_width::UnicodeWidthStr::width(line) == columns
                && matches!(suffix.trim(), "#" | "|"))
    })
}

fn selected_file(screen: &VirtualScreen, filename: &str) -> bool {
    screen.lines().iter().any(|line| {
        line.split('│')
            .rev()
            .nth(1)
            .is_some_and(|detail| detail.contains(filename))
    })
}

// A result in the left candidate list does not prove it is selected. The
// right documentation pane's complete title must name the expected command.
fn selected_palette_command(screen: &VirtualScreen, command: &str) -> bool {
    screen.lines().iter().any(|line| {
        line.matches('│').count() >= 4
            && line
                .rsplit('│')
                .nth(1)
                .is_some_and(|title| title.trim() == command)
    })
}

// Match one selected field in either the JSON stream format or a materialized
// table, including its optional numeric row index. Source echoes, extra fields,
// and suffix text cannot prove a selected data result.
fn is_data_row(line: &str, token: &str, columns: usize) -> bool {
    if is_output_line(line, &format!(r#"{{"name":"{token}"}}"#), columns) {
        return true;
    }
    let Some((first, rest)) = line.strip_prefix('│').and_then(|line| line.split_once('│'))
    else {
        return false;
    };
    let valid_tail = |tail: &str| {
        tail.trim().is_empty()
            || (unicode_width::UnicodeWidthStr::width(line) == columns
                && matches!(tail.trim(), "#" | "|"))
    };
    if first.trim() == token {
        return valid_tail(rest);
    }
    let Some((second, tail)) = rest.split_once('│') else {
        return false;
    };
    first.trim().parse::<u64>().is_ok() && second.trim() == token && valid_tail(tail)
}

fn print_command(token: &str) -> String {
    format!("/usr/bin/printf '%s\\n' {token}")
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn validate(options: &SoakOptions) -> io::Result<()> {
    if options.sessions == 0
        || options.sessions > SESSIONS_MAX
        || options.journeys_per_session == 0
        || options.journeys_per_session > JOURNEYS_MAX
        || options
            .only_session
            .is_some_and(|session| session >= options.sessions)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "soak requires 1..=1000 sessions, 1..=200 journeys, and session index below sessions",
        ));
    }
    Ok(())
}

/// Executes safe UI workflows against one pinned binary and publishes replay
/// evidence. Journey failures stop that session, are reported, and allow later
/// independent sessions until the failure/run budget is exhausted.
pub(crate) fn run(binary: &Path, options: SoakOptions) -> Result<SoakSummary, TaskError> {
    validate(&options)?;
    let pinned = PinnedExecutable::create(binary, options.seed)?;
    let started = Instant::now();
    let run_deadline = started
        .checked_add(RUN_TIME_MAX)
        .ok_or_else(|| io::Error::other("invalid run deadline"))?;
    let mut artifacts = Artifacts::new(&options.output, options.seed)?;
    let mut gallery = BTreeSet::new();
    let mut summary = SoakSummary {
        schema_version: 1,
        seed: options.seed,
        sessions_requested: u64::try_from(options.only_session.map_or(options.sessions, |_| 1))?,
        sessions_attempted: 0,
        sessions_completed: 0,
        journeys_per_session: u64::try_from(options.journeys_per_session)?,
        journeys_attempted: 0,
        journeys_completed: 0,
        key_bytes: 0,
        actions: 0,
        screen_assertions: 0,
        resize_count: 0,
        failure_count: 0,
        failures: Vec::new(),
        wall_ms: 0,
        modeled_hours: 0.0,
        journeys_per_modeled_hour: 60,
        stopped_reason: None,
        binary_source: pinned.source().to_path_buf(),
        binary_sha256: pinned.sha256().to_owned(),
        binary_bytes: pinned.byte_size(),
        report_directory: artifacts.root.clone(),
        feature_counts: BTreeMap::new(),
        limitations: "Modeled active use at 60 journeys/hour with think time removed; not real 100-hour endurance. Emacs keymap, rich surface, manual/always-on contextual completion; no AI/network providers or interactive child applications. SVGs show styled VT cells and cursor with an explicit fixed palette, not exact terminal fonts, shaping, theme, or pixel rendering.",
    };
    let manifest = serde_json::json!({
        "schema_version": 1, "seed": options.seed, "sessions": options.sessions,
        "journeys_per_session": options.journeys_per_session, "only_session": options.only_session,
        "binary_source": pinned.source(), "binary_sha256": pinned.sha256(), "binary_bytes": pinned.byte_size(),
        "replay": "cargo xtask session-soak --seed SEED --sessions ORIGINAL_SESSIONS --journeys ORIGINAL_JOURNEYS --session SESSION --binary ORIGINAL_BINARY --output FRESH_OUTPUT; verify SHA-256 equals binary_sha256 first",
    });
    let manifest_path = artifacts.root.join("manifest.json");
    artifacts.json(&manifest_path, &manifest)?;
    for index in 0..options.sessions {
        if options
            .only_session
            .is_some_and(|selected| selected != index)
        {
            continue;
        }
        if Instant::now() >= run_deadline || summary.failure_count >= FAILURES_MAX {
            summary.stopped_reason =
                Some("run wall-time or failed-session limit reached".to_owned());
            break;
        }
        summary.sessions_attempted = summary.sessions_attempted.saturating_add(1);
        let output = artifacts.root.join(format!("session-{index:04}"));
        let session_started = Instant::now();
        let setup = (|| -> Result<_, TaskError> {
            create_private_directory(&output)?;
            let trace = private_file(&output.join("actions.jsonl"))?;
            let session = Session::new(
                pinned.path(),
                SessionOptions {
                    rows: Some(40),
                    ..SessionOptions::default()
                },
            )?;
            Ok((session, trace))
        })();
        let (session, trace) = match setup {
            Ok(resources) => resources,
            Err(error) => {
                let failure = format!("session {index} setup: {error}")
                    .chars()
                    .take(1024)
                    .collect::<String>();
                summary.failures.push(failure.clone());
                summary.failure_count = summary.failure_count.saturating_add(1);
                let stats = SessionStats {
                    session: index,
                    wall_ms: millis(session_started.elapsed()),
                    failure: Some(failure),
                    ..SessionStats::default()
                };
                if let Err(error) = artifacts.summary(&output.join("summary.json"), &stats) {
                    summary.stopped_reason =
                        Some(format!("cannot retain session setup failure: {error}"));
                    break;
                }
                continue;
            }
        };
        let mut driver = Driver {
            session,
            trace,
            trace_bytes: 0,
            started: session_started,
            deadline: session_started
                .checked_add(SESSION_TIME_MAX)
                .unwrap_or(run_deadline)
                .min(run_deadline),
            stats: SessionStats {
                session: index,
                ..SessionStats::default()
            },
            journey_index: 0,
            feature: "startup",
            output,
            artifacts: &mut artifacts,
            gallery: &mut gallery,
        };
        let outcome = run_session(
            &mut driver,
            options.seed,
            index,
            options.journeys_per_session,
            &mut summary.feature_counts,
            options.only_session.is_some() || index.saturating_add(1) == options.sessions,
        );
        if let Err(error) = outcome {
            driver.stats.failure = Some(error.to_string().chars().take(8192).collect());
        }
        // Reap before committing success. Secondary cleanup/evidence failures
        // must never replace the original failed action and its diagnostic.
        if let Err(error) = driver.session.pty.close() {
            driver
                .stats
                .evidence_errors
                .push(format!("PTY cleanup: {error}"));
        }
        if driver.stats.failure.is_some() {
            let result = driver.session.pty.screen().to_svg().and_then(|svg| {
                driver
                    .artifacts
                    .write(&driver.output.join("failure.svg"), svg.as_bytes())
            });
            if let Err(error) = result {
                driver
                    .stats
                    .evidence_errors
                    .push(format!("failure SVG: {error}"));
            }
        }
        if let Err(error) = driver.artifacts.write(
            &driver.output.join("last-screen.txt"),
            driver.session.pty.screen().text().as_bytes(),
        ) {
            driver
                .stats
                .evidence_errors
                .push(format!("last screen: {error}"));
        }
        let tail = driver.session.pty.output();
        let tail = tail
            .get(tail.len().saturating_sub(8192)..)
            .unwrap_or_default();
        if let Err(error) = driver
            .artifacts
            .json(&driver.output.join("pty-tail.json"), &tail)
        {
            driver
                .stats
                .evidence_errors
                .push(format!("PTY tail: {error}"));
        }
        if let Err(error) = driver.trace.flush() {
            driver
                .stats
                .evidence_errors
                .push(format!("trace flush: {error}"));
        }
        for message in &mut driver.stats.evidence_errors {
            *message = message.chars().take(1024).collect();
        }
        if driver.stats.failure.is_none() && !driver.stats.evidence_errors.is_empty() {
            driver.stats.failure = Some("cleanup or evidence persistence failed".to_owned());
        }
        driver.stats.wall_ms = millis(session_started.elapsed());
        if let Err(error) = driver
            .artifacts
            .summary(&driver.output.join("summary.json"), &driver.stats)
        {
            summary.stopped_reason = Some(format!("cannot retain session summary: {error}"));
            if driver.stats.failure.is_none() {
                driver.stats.failure = Some("session summary persistence failed".to_owned());
            }
        }
        if summary.stopped_reason.is_none() && !driver.stats.evidence_errors.is_empty() {
            summary.stopped_reason = Some(
                "cleanup/evidence failure; original action preserved in session summary".to_owned(),
            );
        }
        if let Some(error) = &driver.stats.failure {
            summary.failure_count = summary.failure_count.saturating_add(1);
            summary.failures.push(
                format!("session {index}: {error}")
                    .chars()
                    .take(1024)
                    .collect(),
            );
        } else {
            summary.sessions_completed = summary.sessions_completed.saturating_add(1);
        }
        summary.journeys_attempted = summary
            .journeys_attempted
            .saturating_add(driver.stats.journeys_attempted);
        summary.journeys_completed = summary
            .journeys_completed
            .saturating_add(driver.stats.journeys_completed);
        summary.key_bytes = summary.key_bytes.saturating_add(driver.stats.key_bytes);
        summary.actions = summary.actions.saturating_add(driver.stats.actions);
        summary.screen_assertions = summary
            .screen_assertions
            .saturating_add(driver.stats.screen_assertions);
        summary.resize_count = summary
            .resize_count
            .saturating_add(driver.stats.resize_count);
        println!(
            "soak session {index}: {}/{} journeys, failure={}, artifacts={}",
            driver.stats.journeys_completed,
            options.journeys_per_session,
            driver.stats.failure.is_some(),
            driver.output.display()
        );
        if summary.stopped_reason.is_some() {
            break;
        }
        // Driver drops its already closed PTY before the next session or snapshot.
    }
    summary.wall_ms = millis(started.elapsed());
    summary.modeled_hours = f64::from(u32::try_from(summary.journeys_completed)?) / 60.0;
    let summary_path = artifacts.root.join("summary.json");
    let gallery_html = super::soak_gallery::render(&summary, &gallery);
    if let Err(error) = artifacts.write(&artifacts.root.join("index.html"), gallery_html.as_bytes())
    {
        summary.stopped_reason = Some(format!("gallery persistence failed: {error}"));
    }
    artifacts.summary(&summary_path, &summary).map_err(|error| io::Error::other(format!(
        "could not persist final soak summary at {}; first failure: {}; stopped: {}; persistence error: {error}",
        summary_path.display(), summary.failures.first().map_or("none", String::as_str),
        summary.stopped_reason.as_deref().unwrap_or("none")
    )))?;
    println!("soak summary: {}", summary_path.display());
    Ok(summary)
}

fn run_session(
    driver: &mut Driver<'_>,
    seed: u64,
    index: usize,
    count: usize,
    features: &mut BTreeMap<String, u64>,
    last_session: bool,
) -> Result<(), TaskError> {
    driver.wait("first editable prompt", |screen| {
        screen.bottom_line().contains("Tab complete")
    })?;
    driver.checkpoint("first-prompt")?;
    for (journey_index, kind) in plan(seed, index, count).into_iter().enumerate() {
        driver.journey_index = journey_index;
        let token = format!("SOAK_S{index:04}_J{journey_index:03}");
        driver.journey(kind, &token)?;
        let count = features.entry(kind.name().to_owned()).or_default();
        *count = count.saturating_add(1);
    }
    driver.checkpoint("completed-session")?;
    if last_session {
        driver.checkpoint("last-prompt")?;
    }
    driver.artifacts.write(
        &driver.output.join("settled-screen.txt"),
        driver.session.pty.screen().text().as_bytes(),
    )?;
    driver.feature = "shutdown";
    let cleanup_start = driver.session.pty.output().len();
    driver.send(key::CTRL_D)?;
    let status = driver.session.pty.wait_exit_within(ACTION_TIMEOUT)?;
    if status != 0 {
        return Err(
            io::Error::other(format!("session exit status was {status}, expected 0")).into(),
        );
    }
    ensure_terminal_restored(&driver.session, cleanup_start, "soak session shutdown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_oracle_requires_the_selected_documentation_title() {
        let mut screen = VirtualScreen::new(4, 80, 0).unwrap();
        screen.feed("│quirl config doctor││cd│".as_bytes());
        assert!(!selected_palette_command(&screen, "quirl config doctor"));
        screen.feed("\r\x1b[2K│cd││quirl config doctor│".as_bytes());
        assert!(selected_palette_command(&screen, "quirl config doctor"));
        screen.feed("\r\x1b[2K│quirl config doctor│".as_bytes());
        assert!(!selected_palette_command(&screen, "quirl config doctor"));
    }

    #[test]
    fn typed_row_oracle_distinguishes_selected_and_excluded_results_from_source_echo() {
        for line in [r#"{"name":"TOKEN"}"#, "│ TOKEN │", "│ 0 │ TOKEN │"] {
            assert!(is_data_row(line, "TOKEN", 80));
            assert!(!is_data_row(line, "EXCLUDED_TOKEN", 80));
        }
        for line in [r#"{"name":"EXCLUDED_TOKEN"}"#, "│ 1 │ EXCLUDED_TOKEN │"] {
            assert!(is_data_row(line, "EXCLUDED_TOKEN", 80));
            assert!(!is_data_row(line, "TOKEN", 80));
        }
        for line in [
            r#"> [{"name":"TOKEN"},{"name":"EXCLUDED_TOKEN"}] | select name"#,
            "│ 0 │ TOKEN │ failed │",
            "│ TOKEN │ EXTRA",
            "#TOKEN#",
            "│ TOKEN │#",
        ] {
            assert!(!is_data_row(line, "TOKEN", 80));
            assert!(!is_data_row(line, "EXCLUDED_TOKEN", 80));
        }
        let decorated = "│ 0 │ TOKEN │   #";
        assert!(is_data_row(
            decorated,
            "TOKEN",
            unicode_width::UnicodeWidthStr::width(decorated)
        ));
    }

    #[test]
    fn every_twelve_journeys_cover_the_whole_workflow_matrix() {
        for seed in [0, 1, 2026090501, u64::MAX] {
            let planned = plan(seed, 3, FEATURE_COUNT * 2);
            for block in planned.chunks(FEATURE_COUNT) {
                for kind in Journey::ALL {
                    assert!(block.contains(&kind));
                }
            }
            assert_eq!(planned, plan(seed, 3, FEATURE_COUNT * 2));
            assert_ne!(planned, plan(seed, 4, FEATURE_COUNT * 2));
        }
    }

    #[test]
    fn output_oracles_ignore_only_known_scrollbar_chrome() {
        for line in ["TOKEN", "TOKEN    ", "TOKEN     #", "TOKEN     |"] {
            assert!(is_output_line(line, "TOKEN", 11));
        }
        for line in [
            "> TOKEN",
            "❯ TOKEN",
            "TOKENB",
            "TOKEN#",
            "TOKEN #",
            "TOKEN  |",
            "TOKEN extra",
            "TOKEN # extra",
        ] {
            assert!(!is_output_line(line, "TOKEN", 11));
        }
    }

    #[test]
    fn replay_and_resource_limits_are_validated_before_spawning() {
        let mut options = SoakOptions {
            seed: 0,
            sessions: 1,
            journeys_per_session: 1,
            only_session: Some(0),
            output: PathBuf::new(),
        };
        assert!(validate(&options).is_ok());
        options.only_session = Some(1);
        assert!(validate(&options).is_err());
        options.only_session = None;
        options.sessions = SESSIONS_MAX;
        options.journeys_per_session = JOURNEYS_MAX;
        assert!(validate(&options).is_ok());
        options.sessions += 1;
        assert!(validate(&options).is_err());
        options.sessions = 1;
        options.journeys_per_session += 1;
        assert!(validate(&options).is_err());
    }

    #[test]
    fn exhausted_artifact_budget_retains_private_failure_summary_without_overwriting() {
        use std::os::unix::fs::PermissionsExt;
        let temporary = crate::rich_pty::TempDirectory::new("quirl-soak-evidence-test").unwrap();
        let mut artifacts = Artifacts {
            root: temporary.path.clone(),
            bytes: ARTIFACT_BYTES_MAX.saturating_sub(SUMMARY_RESERVE_BYTES),
        };
        assert!(
            artifacts
                .write(&temporary.path.join("overflow.svg"), b"x")
                .is_err()
        );
        let summary = temporary.path.join("summary.json");
        let primary = serde_json::json!({"failure": "original screen oracle failed"});
        artifacts.summary(&summary, &primary).unwrap();
        assert_eq!(
            fs::metadata(&summary).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            artifacts
                .summary(&summary, &serde_json::json!({"failure": "replacement"}))
                .is_err()
        );
        let retained: serde_json::Value =
            serde_json::from_slice(&fs::read(&summary).unwrap()).unwrap();
        assert_eq!(retained, primary);
        assert!(!temporary.path.join("overflow.svg").exists());
    }
}
