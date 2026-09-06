pub mod child_terminal;
pub(crate) mod completion;
mod degrade;
mod editor;
mod environment;
mod explorer;
mod frame;
pub(crate) mod highlight;
mod overlay;
mod project_clone;
mod runtime;
mod screen_selection;
mod statusbar;
mod transcript;

pub use degrade::{SurfaceKind, select_surface};
pub use project_clone::ProjectCloneChoice;
pub use runtime::{
    ACTIVITY_MESSAGE_BYTES_MAX, COMPLETION_HOME_BYTES_MAX, DATA_ITEMS_MAX, DATA_RETAINED_BYTES_MAX,
    ENVIRONMENT_ITEMS_MAX, ENVIRONMENT_RETAINED_BYTES_MAX, InteractiveActivityProvider,
    InteractiveActivitySnapshot, InteractiveDataSnapshot, InteractiveEnvironmentSnapshot,
    InteractiveHomeDirectory, InteractiveJobAction, InteractiveJobSnapshot, InteractiveJobStatus,
    InteractivePanelBatch, InteractivePanelProvider, InteractivePanelSnapshot,
    InteractiveRuntimeSnapshot, JOB_ACTION_ITEMS_MAX, JOB_RETAINED_BYTES_MAX, PANEL_COLUMNS_MAX,
    PANEL_COUNT_MAX, PANEL_FIELD_BYTES_MAX, PANEL_GENERATION_BYTES_MAX, PANEL_ROWS_MAX,
};

/// One-shot rich-session loader that returns the complete immutable catalog.
///
/// The rich surface starts this on one owned worker only after its first frame
/// has been flushed. Input remains responsive while the bounded loader runs;
/// the complete catalog is published atomically on the surface thread.
pub type CatalogLoader = Box<dyn FnOnce() -> Result<Arc<Catalog>, ShellError> + Send>;

/// One nonblocking update from an interactive natural-language planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveIntentPlannerUpdate {
    /// Planning is still active and the bounded status text should be rendered in place.
    Progress {
        /// Actual model selected by the local planner connection, when known.
        model: Option<String>,
        /// Short terminal-independent description of the current phase.
        message: String,
    },
    /// One conversational reply, optionally carrying a validated command proposal.
    Reply {
        /// Shell-safe command text available for explicit insertion with Tab.
        command: Option<String>,
        /// Short assistant reply explaining, clarifying, or declining the proposal.
        message: String,
        /// Actual model used to construct the reply.
        model: String,
        /// Actual reasoning effort used for the turn.
        effort: String,
        /// Token totals reported by the planner for this turn and open conversation.
        token_usage: Option<InteractiveIntentTokenUsage>,
        /// End-to-end planning latency in milliseconds.
        elapsed_ms: u64,
    },
}

/// Bounded token totals reported after one interactive planner turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractiveIntentTokenUsage {
    /// Tokens consumed by the most recently completed turn.
    pub turn_total: u64,
    /// Tokens consumed by the complete open planner conversation.
    pub session_total: u64,
}

/// Asynchronous planner used by the rich editor's natural-language mode.
///
/// Implementations may own a persistent local service, but these methods must
/// never perform blocking process, network, or filesystem work on the render
/// thread. At most one request is active at a time. [`Self::cancel`] must make
/// cancellation observable to the worker without waiting for it to finish.
pub trait InteractiveIntentPlanner: Send {
    /// Begin nonblocking connection preparation before the user submits an intent.
    fn prepare(&mut self) {}

    /// Start a new bounded conversation while preserving the warm local connection.
    fn begin_session(&mut self) {}

    /// Discard conversation history and cancel any active turn.
    fn end_session(&mut self) {
        self.cancel();
    }

    /// Schedule one bounded intent against the complete admitted catalog.
    fn start(&mut self, intent: &str, catalog: Arc<Catalog>) -> Result<(), ShellError>;

    /// Return the newest completed update without blocking.
    fn poll_cached(&mut self) -> Result<Option<InteractiveIntentPlannerUpdate>, ShellError>;

    /// Request cancellation of the active plan, if any.
    fn cancel(&mut self);
}

/// Maximum repository candidates retained by one rich-surface project snapshot.
pub const PROJECT_ITEMS_MAX: usize = 4_096;
/// Maximum encoded bytes retained for one repository path or display name.
pub const PROJECT_FIELD_BYTES_MAX: usize = 4 * 1_024;
/// Maximum encoded bytes retained across repository paths and display names.
pub const PROJECT_RETAINED_BYTES_MAX: usize = 2 * 1_024 * 1_024;
/// Maximum bytes retained for project-discovery status shown in the picker.
pub const PROJECT_STATUS_BYTES_MAX: usize = 256;

/// One repository candidate supplied by the product's project index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveProjectEntry {
    /// Exact repository directory returned when the candidate is accepted.
    pub path: PathBuf,
    /// Short repository name used as the primary fuzzy-search field.
    pub name: String,
}

/// Complete immutable generation supplied by the background project index.
///
/// The composition root may replace this snapshot between edit sessions. A
/// running scan can publish cached candidates with `scanning` set; `status`
/// then provides a bounded explanation in candidate preview details.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InteractiveProjectSnapshot {
    /// Monotonic database or scanner generation represented by this snapshot.
    pub generation: u64,
    /// Repository directories available to the project picker.
    pub projects: Vec<InteractiveProjectEntry>,
    /// Whether a newer discovery generation is currently being assembled.
    pub scanning: bool,
    /// Whether discovery stopped at a configured resource bound.
    pub truncated: bool,
    /// Optional short discovery state suitable for terminal presentation.
    pub status: Option<String>,
}

/// Nonblocking source of completed project-index snapshots.
///
/// Implementations may signal an owned background worker from
/// [`Self::picker_opened`], but neither method may perform filesystem or
/// database work, wait for a worker, or invoke user code on the render thread.
pub trait InteractiveProjectProvider: Send {
    /// Return a newer immutable snapshot already published in provider memory.
    fn poll_cached(&mut self) -> Result<Option<InteractiveProjectSnapshot>, ShellError>;

    /// Request a stale-index refresh immediately before the project picker opens.
    ///
    /// The default is a no-op for providers whose periodic refresh is sufficient.
    fn picker_opened(&mut self) -> Result<(), ShellError> {
        Ok(())
    }
}

/// Bounded history metadata installed by the product's durable history store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveHistoryEntry {
    /// Exact command line restored when the entry is accepted.
    pub command_line: String,
    /// Working directory in which the command was started, when known.
    pub directory: Option<String>,
    /// Exit status recorded after execution, when known.
    pub status: Option<i32>,
    /// Ranking preference applied before deterministic picker tie-breaking.
    pub rank_bias: i32,
}

use self::{
    completion::{CompletionState, IntentCompletionState},
    editor::{EditAction, EditorState},
    environment::{EnvironmentExplorer, ExplorerAction as EnvironmentExplorerAction},
    explorer::{DirectoryExplorer, ExplorerAction as DirectoryExplorerAction},
    frame::FrameModel,
    highlight::InputAnalyzer,
    overlay::{PickerLayout, PickerOverlay, contextual_help_query},
    runtime::RuntimeSurfaceState,
    screen_selection::{ScreenPosition, ScreenSelection, VisibleScreen, style_selection},
    transcript::{TextPosition, Transcript, TranscriptLimits},
};
use super::{
    ExtensionCompleter, MAX_HISTORY_ENCODED_ENTRY_BYTES, MAX_HISTORY_ENTRY_BYTES,
    MAX_HISTORY_RETAINED_BYTES, PickerRanker, QuirlPrompt, read_history,
};
use crate::SurfaceSymbols;
use crate::theme::Theme;
use crossterm::{
    cursor::{MoveTo, SetCursorStyle, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    style::Print,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use quirl_catalog::Catalog;
use quirl_core::{
    AtomicReplaceOptions, ErrorCode, OutputStream, ShellError, escape_terminal_line,
    replace_file_atomically,
};
use quirl_lua::QuirlConfig;
use quirl_syntax::{Mode, parse_command_list};
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::{Backend, CrosstermBackend},
    layout::Rect,
    style::Style,
};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::VecDeque,
    env, fs,
    fs::OpenOptions,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const EVENT_POLL: Duration = Duration::from_millis(16);
const TIMING_WINDOW: usize = 128;
// Keep the rich surface interoperable with Reedline's durable history format.
const HISTORY_NEWLINE_ESCAPE: &str = "<\\n>";
const MAX_HISTORY_FILE_BYTES: usize = MAX_HISTORY_RETAINED_BYTES * 4 + 50_000;
const HELP_DETAIL_SCROLL_MAX: u16 = 4_096;
const RICH_TERMINAL_COLUMNS_MAX: u16 = 512;
const RICH_TERMINAL_ROWS_MAX: u16 = 256;
const HISTORY_REPLACEMENT_BYTES_MAX: usize = MAX_HISTORY_FILE_BYTES * 2;
const TRANSCRIPT_LINES_MAX: usize = 50_000;
const TRANSCRIPT_BYTES_MAX: usize = 16 * 1024 * 1024;
const TRANSCRIPT_COPY_BYTES_MAX: usize = 1024 * 1024;
const TRANSCRIPT_MOUSE_SCROLL_LINES: usize = 3;
const STREAM_CHUNK_BYTES_MAX: usize = 8 * 1024;
const ANSI_CSI_BYTES_MAX: usize = 64;
const ANSI_OSC_BYTES_MAX: usize = 4 * 1024;
const PRODUCT_IDENTITY_BYTES_MAX: usize = 64;
const INTENT_CONVERSATION_MESSAGES_MAX: usize = 8;
const INTENT_CONVERSATION_RETAINED_BYTES_MAX: usize = 8 * 1024;
const INTENT_CONVERSATION_MESSAGE_BYTES_MAX: usize = 2 * 1024;
static PRODUCT_IDENTITY: OnceLock<String> = OnceLock::new();
#[cfg(test)]
static HISTORY_TEST_ID: AtomicU64 = AtomicU64::new(0);

/// Install the bounded build identity rendered by rich status bars in this process.
///
/// The identity is immutable after first use so concurrent surfaces cannot disagree about the
/// running executable. Calling this again with the same value is idempotent.
pub fn set_product_identity(identity: &str) -> Result<(), ShellError> {
    if identity.len() > PRODUCT_IDENTITY_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "status-bar build identity exceeds its byte limit",
        )
        .with_context(format!(
            "limit {PRODUCT_IDENTITY_BYTES_MAX} bytes; observed {} bytes",
            identity.len()
        ))
        .with_help("Use a shorter release version or development build identifier"));
    }
    let identity = quirl_core::escape_terminal_line(identity);
    if PRODUCT_IDENTITY
        .get()
        .is_some_and(|current| current == &identity)
    {
        return Ok(());
    }
    PRODUCT_IDENTITY.set(identity).map_err(|_| {
        ShellError::new(
            ErrorCode::Validation,
            "status-bar build identity is already initialized",
        )
        .with_help("Install build identity once before constructing the first rich surface")
    })
}

fn product_identity() -> &'static str {
    PRODUCT_IDENTITY
        .get_or_init(|| format!("v{}", env!("CARGO_PKG_VERSION")))
        .as_str()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseDrag {
    ScreenSelection {
        anchor: ScreenPosition,
        dragged: bool,
    },
    Scrollbar,
}

#[derive(Debug, Default)]
struct StreamingText {
    utf8_tail: Vec<u8>,
    line: String,
    /// A bare `\r` was seen and the next printable character (if any)
    /// should overwrite `line` from its start rather than append.
    ///
    /// A real terminal treats `\r` as "move the cursor to column 0", not as
    /// "erase the line": content already written stays visible until
    /// something actually overwrites it. Clearing `line` immediately on
    /// `\r` would both lose the in-flight content that [`Self::pending`]
    /// exists to show and mishandle a `\r\n` line ending, which must keep
    /// the line it terminates rather than blank it.
    overwrite_pending: bool,
    sequence: TerminalSequence,
}

#[derive(Debug, Default)]
enum TerminalSequence {
    #[default]
    Ground,
    Escape,
    Csi(usize),
    Osc(usize),
    OscEscape(usize),
}

impl StreamingText {
    fn reset(&mut self) {
        self.utf8_tail.clear();
        self.line.clear();
        self.overwrite_pending = false;
        self.sequence = TerminalSequence::Ground;
    }

    /// Clear `line` exactly once for the character that follows a `\r`.
    fn begin_overwrite_if_pending(&mut self) {
        if std::mem::take(&mut self.overwrite_pending) {
            self.line.clear();
        }
    }

    /// Return the in-progress line accumulated since the last completed line.
    ///
    /// A child that reports progress with bare `\r` overwrites (no trailing
    /// `\n`) never produces a completed line through [`Self::push`] until it
    /// either starts a new line or the process exits. Callers use this to
    /// show that in-flight content live instead of leaving the viewport
    /// static for the whole operation.
    fn pending(&self) -> &str {
        &self.line
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let text = decode_utf8_chunk(&mut self.utf8_tail, bytes);
        self.push_text(&text)
    }

    fn finish(&mut self) -> Vec<String> {
        let mut text = String::new();
        if !self.utf8_tail.is_empty() {
            text.push('\u{fffd}');
            self.utf8_tail.clear();
        }
        let mut lines = self.push_text(&text);
        if !self.line.is_empty() {
            lines.push(std::mem::take(&mut self.line));
        }
        self.overwrite_pending = false;
        self.sequence = TerminalSequence::Ground;
        lines
    }

    fn push_text(&mut self, text: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for character in text.chars() {
            match &mut self.sequence {
                TerminalSequence::Ground => match character {
                    '\u{1b}' => self.sequence = TerminalSequence::Escape,
                    '\n' => {
                        self.overwrite_pending = false;
                        lines.push(std::mem::take(&mut self.line));
                    }
                    '\r' => self.overwrite_pending = true,
                    '\t' => {
                        self.begin_overwrite_if_pending();
                        self.line.push_str("    ");
                    }
                    value if !value.is_control() => {
                        self.begin_overwrite_if_pending();
                        self.line.push(value);
                    }
                    _ => {}
                },
                TerminalSequence::Escape => {
                    self.sequence = match character {
                        '[' => TerminalSequence::Csi(0),
                        ']' => TerminalSequence::Osc(0),
                        _ => TerminalSequence::Ground,
                    };
                }
                TerminalSequence::Csi(count) => {
                    *count = count.saturating_add(character.len_utf8());
                    if ('@'..='~').contains(&character) || *count >= ANSI_CSI_BYTES_MAX {
                        self.sequence = TerminalSequence::Ground;
                    }
                }
                TerminalSequence::Osc(count) => {
                    *count = count.saturating_add(character.len_utf8());
                    if character == '\u{7}' || *count >= ANSI_OSC_BYTES_MAX {
                        self.sequence = TerminalSequence::Ground;
                    } else if character == '\u{1b}' {
                        self.sequence = TerminalSequence::OscEscape(*count);
                    }
                }
                TerminalSequence::OscEscape(count) => {
                    self.sequence = if character == '\\' || *count >= ANSI_OSC_BYTES_MAX {
                        TerminalSequence::Ground
                    } else {
                        TerminalSequence::Osc(count.saturating_add(character.len_utf8()))
                    };
                }
            }
        }
        lines
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "slice bounds are returned by from_utf8 as the valid prefix and bounded error length"
)]
fn decode_utf8_chunk(tail: &mut Vec<u8>, bytes: &[u8]) -> String {
    let mut combined = Vec::with_capacity(tail.len().saturating_add(bytes.len()));
    combined.append(tail);
    combined.extend_from_slice(bytes);
    let mut decoded = String::new();
    let mut remaining = combined.as_slice();
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                decoded.push_str(text);
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                if valid > 0 {
                    decoded.push_str(std::str::from_utf8(&remaining[..valid]).unwrap_or_default());
                }
                match error.error_len() {
                    Some(length) => {
                        decoded.push('\u{fffd}');
                        remaining = &remaining[valid.saturating_add(length)..];
                    }
                    None => {
                        tail.extend_from_slice(&remaining[valid..]);
                        break;
                    }
                }
            }
        }
    }
    decoded
}

fn legacy_history(path: &Path) -> Vec<InteractiveHistoryEntry> {
    read_history(path)
        .unwrap_or_default()
        .into_iter()
        .map(|command_line| InteractiveHistoryEntry {
            command_line,
            directory: None,
            status: None,
            rank_bias: 0,
        })
        .collect()
}

fn validate_project_snapshot(snapshot: &InteractiveProjectSnapshot) -> Result<(), ShellError> {
    if snapshot.projects.len() > PROJECT_ITEMS_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "project snapshot exceeds its repository limit",
        )
        .with_context(format!(
            "limit {PROJECT_ITEMS_MAX} repositories; observed {} repositories",
            snapshot.projects.len()
        ))
        .with_help("Reduce configured discovery roots or tighten project scan limits"));
    }
    if snapshot
        .status
        .as_ref()
        .is_some_and(|status| status.len() > PROJECT_STATUS_BYTES_MAX)
    {
        let observed = snapshot.status.as_ref().map_or(0, String::len);
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "project snapshot status exceeds its byte limit",
        )
        .with_context(format!(
            "limit {PROJECT_STATUS_BYTES_MAX} bytes; observed {observed} bytes"
        ))
        .with_help("Publish a shorter project discovery status"));
    }
    let mut retained_bytes = 0_usize;
    for project in &snapshot.projects {
        if !project.path.is_absolute() || project.name.is_empty() {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "project snapshot contains an invalid repository identity",
            )
            .with_help("Publish absolute repository paths with non-empty display names"));
        }
        let path_bytes = project.path.as_os_str().as_encoded_bytes().len();
        let name_bytes = project.name.len();
        let field_bytes = path_bytes.max(name_bytes);
        if field_bytes > PROJECT_FIELD_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "project snapshot field exceeds its byte limit",
            )
            .with_context(format!(
                "limit {PROJECT_FIELD_BYTES_MAX} bytes; observed {field_bytes} bytes"
            ))
            .with_help("Exclude the oversized repository path from project discovery"));
        }
        retained_bytes = retained_bytes
            .saturating_add(path_bytes)
            .saturating_add(name_bytes);
        if retained_bytes > PROJECT_RETAINED_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "project snapshot exceeds its retained-byte limit",
            )
            .with_context(format!(
                "limit {PROJECT_RETAINED_BYTES_MAX} bytes; observed at least {retained_bytes} bytes"
            ))
            .with_help("Reduce configured discovery roots or tighten project scan limits"));
        }
    }
    Ok(())
}

fn safe_project_text(value: &str, bytes_max: usize) -> String {
    let escaped = quirl_core::escape_terminal_line(value);
    crate::truncate_utf8_ref(&escaped, bytes_max).to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result of one rich-surface input session after terminal ownership is released.
pub enum InteractiveSignal {
    /// User accepted the complete input buffer.
    Success(String),
    /// Ctrl-C cancelled editing and discarded the current buffer.
    CtrlC,
    /// Ctrl-D was pressed while the input buffer was empty.
    CtrlD,
    /// Opaque command for the composition root rather than child-process execution.
    HostCommand(String),
    /// A Miller-column session accepted a new working directory while preserving input.
    ChangeDirectory {
        /// Exact filesystem path selected by the user.
        path: PathBuf,
        /// Editor buffer to restore after the composition root commits the transition.
        buffer: String,
        /// UTF-8 byte cursor within `buffer`.
        cursor: usize,
    },
    /// Open a cloned project after the host revalidates its directory and Git marker.
    OpenProject {
        /// Absolute directory offered after the successful clone.
        path: PathBuf,
        /// Editor text to retain even if the destination is no longer valid.
        buffer: String,
        /// UTF-8 byte cursor within `buffer`.
        cursor: usize,
    },
    /// The host should suspend the shell after performing platform job control.
    Suspend,
}

/// Latest picker intent awaiting catalog admission. Help retains at most one
/// editor-sized source and one bounded picker query; dismissal drops both.
enum DeferredCatalogPicker {
    Palette {
        replace_end: usize,
    },
    Help {
        line: String,
        cursor: usize,
        initial_query: String,
    },
}

/// Stateful full-screen terminal editor with completion, pickers, history, and diagnostics.
///
/// The surface owns raw mode, bracketed paste, cursor shape, and its ratatui
/// alternate screen only while [`Self::read_line`] is active. Normal returns restore all
/// terminal state; error unwinding and drop make a final best-effort restoration.
/// Completion and prompt-analysis queues retain at most one pending generation,
/// preventing stale work from repainting newer input.
pub struct RichSurface {
    catalog: Option<Arc<Catalog>>,
    catalog_loader: Option<CatalogLoader>,
    catalog_admission: Option<CatalogAdmission>,
    completion: CompletionState,
    picker: PickerOverlay,
    deferred_catalog_picker: Option<DeferredCatalogPicker>,
    explorer: Option<DirectoryExplorer>,
    pending_input: Option<(String, usize)>,
    pending_project_open: Option<PathBuf>,
    picker_layout: PickerLayout,
    picker_preview: bool,
    expand_completion_pending: bool,
    keymap: String,
    history_path: PathBuf,
    history: Vec<InteractiveHistoryEntry>,
    projects: InteractiveProjectSnapshot,
    project_picker_active: bool,
    project_provider: Option<Box<dyn InteractiveProjectProvider>>,
    terminal: SurfaceTerminal,
    draw_times: VecDeque<Duration>,
    input_analysis: InputAnalyzer,
    show_timings: bool,
    hints: bool,
    completion_auto: bool,
    completion_min_chars: usize,
    semantic_hints: bool,
    help_active: bool,
    help_detail_scroll: u16,
    leader_active: bool,
    environment: EnvironmentExplorer,
    theme: Theme,
    runtime: RuntimeSurfaceState,
    transcript: Transcript,
    transcript_truncated: bool,
    output_focus: bool,
    output_cursor_line: usize,
    output_anchor_line: Option<usize>,
    output_notice: Option<String>,
    /// Activity glyph for foreground progress and interactive AI planning.
    /// Execution frames never display the editor's prompt or input cursor.
    busy_glyph: Option<char>,
    transcript_area: Rect,
    visible_screen: VisibleScreen,
    screen_selection: Option<ScreenSelection>,
    screen_copy_pending: bool,
    mouse_drag: Option<MouseDrag>,
    intent_completion: IntentCompletionState,
    intent_planner: Option<Box<dyn InteractiveIntentPlanner>>,
    intent_planning_started: Option<Instant>,
    intent_conversation: VecDeque<IntentConversationMessage>,
    intent_proposal: Option<String>,
    intent_model: Option<(String, String)>,
    intent_token_usage: Option<InteractiveIntentTokenUsage>,
    intent_phase: Option<String>,
    pending_prefill: Option<String>,
    stream_stdout: StreamingText,
    stream_stderr: StreamingText,
    /// Stream currently occupying the transcript's uncommitted live line, if any.
    ///
    /// `\r`-driven progress updates overwrite one transcript line in place
    /// instead of appending; this tracks which stream owns that line so the
    /// other stream's next chunk starts a new one rather than clobbering it.
    live_output_owner: Option<OutputStream>,
}

struct CatalogAdmission {
    receiver: Receiver<Result<Arc<Catalog>, ShellError>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
enum IntentConversationRole {
    User,
    Assistant,
}

struct IntentConversationMessage {
    role: IntentConversationRole,
    text: String,
}

impl RichSurface {
    /// Construct a rich surface without entering raw terminal mode.
    ///
    /// Existing history is loaded through a bounded, best-effort tail read;
    /// missing, unreadable, invalid, and oversized entries are skipped. The
    /// configured picker ranker and optional extension completer are retained for
    /// subsequent input sessions. Terminal capability failures are reported only
    /// when [`Self::read_line`] tries to acquire the terminal.
    pub fn new(
        catalog: Arc<Catalog>,
        extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
        picker_ranker: Arc<dyn PickerRanker>,
        config: &QuirlConfig,
        history_path: PathBuf,
    ) -> Result<Self, ShellError> {
        let history = legacy_history(&history_path);
        let input_analysis = InputAnalyzer::new(Arc::clone(&catalog));
        let theme = Theme::from_config(config, true)?;
        Ok(Self {
            completion: CompletionState::new(Arc::clone(&catalog), extension_completer),
            picker: PickerOverlay::new(picker_ranker),
            deferred_catalog_picker: None,
            explorer: None,
            pending_input: None,
            pending_project_open: None,
            picker_layout: PickerLayout::from_config(&config.picker.layout),
            picker_preview: config.picker.preview,
            expand_completion_pending: false,
            catalog: Some(catalog),
            catalog_loader: None,
            catalog_admission: None,
            keymap: config.editor.keymap.clone(),
            history_path,
            history,
            projects: InteractiveProjectSnapshot::default(),
            project_picker_active: false,
            project_provider: None,
            terminal: SurfaceTerminal::default(),
            draw_times: VecDeque::with_capacity(TIMING_WINDOW),
            input_analysis,
            show_timings: env::var("QUIRL_UI_TIMINGS").is_ok_and(|value| value == "1"),
            hints: config.ui.statusline.hints,
            completion_auto: config.completion.auto,
            completion_min_chars: usize::from(config.completion.min_chars),
            semantic_hints: config.editor.semantic_hints,
            help_active: false,
            help_detail_scroll: 0,
            leader_active: false,
            environment: EnvironmentExplorer::new(),
            theme,
            runtime: RuntimeSurfaceState::new(),
            transcript: Transcript::new(TranscriptLimits {
                line_count_max: TRANSCRIPT_LINES_MAX,
                retained_bytes_max: TRANSCRIPT_BYTES_MAX,
            }),
            transcript_truncated: false,
            output_focus: false,
            output_cursor_line: 0,
            output_anchor_line: None,
            output_notice: None,
            busy_glyph: None,
            transcript_area: Rect::default(),
            visible_screen: VisibleScreen::default(),
            screen_selection: None,
            screen_copy_pending: false,
            mouse_drag: None,
            intent_completion: IntentCompletionState::default(),
            intent_planner: None,
            intent_planning_started: None,
            intent_conversation: VecDeque::new(),
            intent_proposal: None,
            intent_model: None,
            intent_token_usage: None,
            intent_phase: None,
            pending_prefill: None,
            stream_stdout: StreamingText::default(),
            stream_stderr: StreamingText::default(),
            live_output_owner: None,
        })
    }

    /// Construct a rich surface whose complete catalog starts loading after
    /// the first successful terminal flush without blocking the first input.
    ///
    /// Configuration, theme, keymap, history, picker policy, and terminal
    /// acquisition remain eager. The loader is consumed exactly once by one
    /// owned worker. Its returned [`Arc<Catalog>`] is published as one complete
    /// generation on the surface thread to analysis, completion, picker/help,
    /// and the composition root. Dropping the surface joins the bounded worker.
    pub fn new_deferred(
        catalog_loader: CatalogLoader,
        extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
        picker_ranker: Arc<dyn PickerRanker>,
        config: &QuirlConfig,
        history_path: PathBuf,
    ) -> Result<Self, ShellError> {
        let history = legacy_history(&history_path);
        let theme = Theme::from_config(config, true)?;
        Ok(Self {
            catalog: None,
            catalog_loader: Some(catalog_loader),
            catalog_admission: None,
            completion: CompletionState::unpublished(extension_completer),
            picker: PickerOverlay::new(picker_ranker),
            deferred_catalog_picker: None,
            explorer: None,
            pending_input: None,
            pending_project_open: None,
            picker_layout: PickerLayout::from_config(&config.picker.layout),
            picker_preview: config.picker.preview,
            expand_completion_pending: false,
            keymap: config.editor.keymap.clone(),
            history_path,
            history,
            projects: InteractiveProjectSnapshot::default(),
            project_picker_active: false,
            project_provider: None,
            terminal: SurfaceTerminal::default(),
            draw_times: VecDeque::with_capacity(TIMING_WINDOW),
            input_analysis: InputAnalyzer::unpublished(),
            show_timings: env::var("QUIRL_UI_TIMINGS").is_ok_and(|value| value == "1"),
            hints: config.ui.statusline.hints,
            completion_auto: config.completion.auto,
            completion_min_chars: usize::from(config.completion.min_chars),
            semantic_hints: config.editor.semantic_hints,
            help_active: false,
            help_detail_scroll: 0,
            leader_active: false,
            environment: EnvironmentExplorer::new(),
            theme,
            runtime: RuntimeSurfaceState::new(),
            transcript: Transcript::new(TranscriptLimits {
                line_count_max: TRANSCRIPT_LINES_MAX,
                retained_bytes_max: TRANSCRIPT_BYTES_MAX,
            }),
            transcript_truncated: false,
            output_focus: false,
            output_cursor_line: 0,
            output_anchor_line: None,
            output_notice: None,
            busy_glyph: None,
            transcript_area: Rect::default(),
            visible_screen: VisibleScreen::default(),
            screen_selection: None,
            screen_copy_pending: false,
            mouse_drag: None,
            intent_completion: IntentCompletionState::default(),
            intent_planner: None,
            intent_planning_started: None,
            intent_conversation: VecDeque::new(),
            intent_proposal: None,
            intent_model: None,
            intent_token_usage: None,
            intent_phase: None,
            pending_prefill: None,
            stream_stdout: StreamingText::default(),
            stream_stderr: StreamingText::default(),
            live_output_owner: None,
        })
    }

    /// Append one completed command and its bounded captured output to the session viewport.
    ///
    /// Invalid UTF-8 is replaced, terminal controls are rendered visibly, and retention is
    /// limited to 50,000 logical lines and 16 MiB. The viewport follows new output unless the
    /// user has explicitly scrolled away from the tail.
    pub fn append_transcript(
        &mut self,
        command: &str,
        stdout: &[u8],
        stderr: &[u8],
        status: i32,
        duration: Duration,
    ) -> Result<(), ShellError> {
        self.append_transcript_line(&format!("❯ {}", quirl_core::escape_terminal_line(command)));
        self.append_transcript_bytes(stdout);
        self.append_transcript_bytes(stderr);
        self.append_transcript_line(&format!("── exit {status} · {}ms ──", duration.as_millis()));
        self.output_cursor_line = self.transcript.line_count().saturating_sub(1);
        self.output_notice =
            Some("result kept in viewport · PageUp/PageDown scroll · Alt-Q O copy".to_owned());
        Ok(())
    }

    /// Start one foreground command record while the alternate-screen viewport remains owned.
    pub fn begin_command_stream(
        &mut self,
        command: &str,
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        self.dismiss_picker();
        self.stream_stdout.reset();
        self.stream_stderr.reset();
        self.live_output_owner = None;
        self.append_transcript_line(&format!("❯ {command}"));
        let symbols = prompt.surface_symbols();
        self.output_notice = Some(running_notice(Duration::ZERO, symbols));
        self.busy_glyph = Some(spinner_glyph(Duration::ZERO, symbols));
        self.draw_execution(prompt)
    }

    /// Refresh the running-command spinner and elapsed time, then repaint.
    ///
    /// Call this once per liveness tick delivered while a foreground command
    /// has produced no new output: without it, a silent long-running command
    /// leaves the viewport looking frozen even though Quirl is still
    /// waiting on it.
    pub fn tick_command_stream(
        &mut self,
        elapsed: Duration,
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        let symbols = prompt.surface_symbols();
        self.output_notice = Some(running_notice(elapsed, symbols));
        self.busy_glyph = Some(spinner_glyph(elapsed, symbols));
        self.draw_execution(prompt)
    }

    /// Append one bounded stdout or stderr chunk and repaint the active command viewport.
    pub fn append_command_stream(
        &mut self,
        stream: OutputStream,
        bytes: &[u8],
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        if bytes.len() > STREAM_CHUNK_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "process output chunk exceeds the rich viewport limit",
            )
            .with_context(format!(
                "limit {STREAM_CHUNK_BYTES_MAX} bytes; observed {} bytes",
                bytes.len()
            ))
            .with_help("Deliver process output in chunks no larger than 8 KiB"));
        }
        let lines = match stream {
            OutputStream::Stdout => self.stream_stdout.push(bytes),
            OutputStream::Stderr => self.stream_stderr.push(bytes),
        };
        let pending = match stream {
            OutputStream::Stdout => self.stream_stdout.pending().to_owned(),
            OutputStream::Stderr => self.stream_stderr.pending().to_owned(),
        };
        self.push_stream_output(stream, lines, &pending);
        self.draw_execution(prompt)
    }

    /// Finish the active command record after every captured reader has been drained.
    /// This repaints the result without a prompt; `read_line` restores the next
    /// editable prompt only when the composition root resumes input.
    pub fn finish_command_stream(
        &mut self,
        status: i32,
        duration: Duration,
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        let stdout = self.stream_stdout.finish();
        self.push_stream_output(OutputStream::Stdout, stdout, "");
        let stderr = self.stream_stderr.finish();
        self.push_stream_output(OutputStream::Stderr, stderr, "");
        self.live_output_owner = None;
        self.append_transcript_line(&format!("── exit {status} · {}ms ──", duration.as_millis()));
        self.output_cursor_line = self.transcript.line_count().saturating_sub(1);
        self.output_notice =
            Some("result kept in viewport · PageUp/PageDown scroll · Alt-Q O copy".to_owned());
        self.busy_glyph = None;
        self.draw_execution(prompt)
    }

    /// Begin a bounded embedded terminal while retaining the physical viewport.
    ///
    /// The caller must pair this with `finish_embedded_terminal`, including on
    /// process, protocol, or rendering errors. Physical input stays raw so
    /// control keys belong to the child terminal instead of the shell process.
    pub fn begin_embedded_terminal(
        &mut self,
    ) -> Result<child_terminal::ChildTerminalSize, ShellError> {
        let size = Self::embedded_terminal_size()?;
        self.terminal.resume_input()?;
        Ok(size)
    }

    /// Measure the child grid, bounded to 512 columns and 256 rows.
    pub fn embedded_terminal_size() -> Result<child_terminal::ChildTerminalSize, ShellError> {
        let (columns, rows) =
            crate::terminal_size().map_err(terminal_error("measure child terminal"))?;
        Ok(child_terminal::ChildTerminalSize {
            rows: rows.clamp(1, 256),
            columns: columns.clamp(2, 512),
        })
    }

    /// Draw only interpreted terminal cells; child escape sequences never reach the backend.
    /// An optional stopped-job notice occupies the last visible row.
    pub fn draw_embedded_terminal(
        &mut self,
        child: &child_terminal::ChildTerminal,
        notice: Option<&str>,
    ) -> Result<(), ShellError> {
        let size = crate::terminal_size().map_err(terminal_error("measure child viewport"))?;
        validate_rich_terminal_size(size)?;
        let terminal = self.terminal.terminal.as_mut().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "the child viewport is unavailable")
                .with_help("Restart Quirl using the simple surface")
        })?;
        let area = Rect::new(0, 0, size.0, size.1);
        if resize_fixed_terminal(terminal, self.terminal.last_size, area)
            .map_err(terminal_error("resize child viewport"))?
        {
            self.terminal.last_size = Some(size);
        }
        terminal
            .draw(|frame| {
                child.render(frame, frame.area());
                if let Some(notice) = notice {
                    let area = frame.area();
                    frame.render_widget(
                        ratatui::widgets::Paragraph::new(notice),
                        Rect::new(0, area.height.saturating_sub(1), area.width, 1),
                    );
                }
            })
            .map_err(terminal_error("draw child terminal"))?;
        Ok(())
    }

    /// Poll at most one physical input event, clamping `wait` to 20 ms.
    /// Use zero while output is flowing so keyboard polling does not throttle it.
    pub fn poll_embedded_terminal_event(
        wait: Duration,
    ) -> Result<Option<event::Event>, ShellError> {
        if event::poll(wait.min(Duration::from_millis(20)))
            .map_err(terminal_error("poll child input"))?
        {
            event::read()
                .map(Some)
                .map_err(terminal_error("read child input"))
        } else {
            Ok(None)
        }
    }

    /// Restore command-execution modes and append the bounded terminal snapshot.
    /// Existing transcript retention limits apply independently of child output volume.
    pub fn finish_embedded_terminal(
        &mut self,
        snapshot: Vec<String>,
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        self.terminal.pause_for_execution()?;
        for line in snapshot {
            self.append_transcript_line(&line);
        }
        self.terminal.force_repaint()?;
        self.draw_execution(prompt)
    }

    /// Commit one stream's newly completed lines, then show its still-open
    /// `\r`-driven line (if any) as one uncommitted, in-place-updated line.
    ///
    /// Real terminals let concurrent stdout and stderr writers share one
    /// cursor; the transcript instead gives each stream its own line so one
    /// stream's in-flight update can never overwrite the other's completed
    /// output. A stream's uncommitted line survives only until the other
    /// stream appends past it or this stream finishes it with a real line.
    fn push_stream_output(&mut self, stream: OutputStream, completed: Vec<String>, pending: &str) {
        for line in completed {
            if self.live_output_owner == Some(stream) {
                self.replace_transcript_line(&line);
            } else {
                self.live_output_owner = None;
                self.append_transcript_line(&line);
            }
            self.live_output_owner = None;
        }
        if pending.is_empty() {
            return;
        }
        if self.live_output_owner == Some(stream) {
            self.replace_transcript_line(pending);
        } else {
            self.live_output_owner = None;
            self.append_transcript_line(pending);
            self.live_output_owner = Some(stream);
        }
    }

    fn draw_execution(&mut self, prompt: &QuirlPrompt) -> Result<(), ShellError> {
        let editor = EditorState::new(&self.keymap, Vec::new());
        let symbols = prompt.surface_symbols();
        let unicode = symbols.uses_unicode();
        let color = std::io::stderr().is_terminal()
            && env::var_os("NO_COLOR").is_none()
            && !env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
        let theme = self.theme.with_color(color);
        let context_left = prompt.surface_context_left();
        let context_right = prompt.surface_context_right();
        let (terminal_width, terminal_height) = crate::terminal_size().unwrap_or((80, 24));
        let model = FrameModel {
            context_left: &context_left,
            context_right: &context_right,
            input_active: false,
            editor: &editor,
            completion: &self.completion,
            mode: prompt.mode,
            diagnostic: None,
            highlight_spans: &[],
            theme,
            unicode,
            symbols,
            semantic_hints: self.semantic_hints,
            hints: self.hints,
            timings: None,
            compact: terminal_height < 8,
            picker_query: None,
            picker_layout: self.picker_layout,
            picker_preview: self.picker_preview,
            detail_scroll: 0,
            environment: None,
            runtime: &self.runtime,
            transcript: Some(&self.transcript),
            transcript_truncated: self.transcript_truncated,
            output_focus: false,
            output_notice: self.output_notice.as_deref(),
            busy_glyph: self.busy_glyph,
        };
        self.transcript_area =
            model.transcript_area(Rect::new(0, 0, terminal_width, terminal_height));
        self.visible_screen = self.terminal.draw(
            &model,
            None,
            prompt.mode,
            symbols,
            self.screen_selection,
            theme.selected(prompt.mode),
        )?;
        Ok(())
    }

    fn append_transcript_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(bytes);
        let mut lines = text.split('\n').peekable();
        while let Some(line) = lines.next() {
            if line.is_empty() && lines.peek().is_none() && text.ends_with('\n') {
                break;
            }
            self.append_transcript_line(line.trim_end_matches('\r'));
        }
    }

    fn append_transcript_line(&mut self, line: &str) {
        let safe = quirl_core::escape_terminal_line(line);
        let outcome = self.transcript.append_line(&safe);
        self.transcript_truncated |= outcome.evicted_line_count > 0
            || outcome.evicted_bytes > 0
            || outcome.truncated_prefix_bytes > 0;
    }

    /// Replace the transcript's most recently appended line in place.
    ///
    /// Used only for the live, uncommitted line tracked by
    /// [`Self::push_stream_output`]; every other caller must keep using
    /// [`Self::append_transcript_line`] so committed history is never edited.
    fn replace_transcript_line(&mut self, line: &str) {
        let safe = quirl_core::escape_terminal_line(line);
        let outcome = self.transcript.replace_last_line(&safe);
        self.transcript_truncated |= outcome.evicted_line_count > 0
            || outcome.evicted_bytes > 0
            || outcome.truncated_prefix_bytes > 0;
    }

    /// Return the complete catalog after deferred admission has succeeded.
    pub fn published_catalog(&self) -> Option<Arc<Catalog>> {
        self.catalog.as_ref().map(Arc::clone)
    }

    /// Replace catalog-backed analysis and completion without restarting the rich session.
    pub fn replace_catalog(&mut self, catalog: Arc<Catalog>) {
        self.completion.replace_catalog(Arc::clone(&catalog));
        self.input_analysis.replace_catalog(Arc::clone(&catalog));
        self.catalog = Some(catalog);
        self.dismiss_picker();
    }

    fn begin_catalog_admission(&mut self) -> Result<(), ShellError> {
        if self.catalog.is_some() || self.catalog_admission.is_some() {
            return Ok(());
        }
        let loader = self.catalog_loader.take().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "interactive catalog loader is unavailable")
                .with_help("Restart Quirl to create a fresh interactive session")
        })?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("quirl-catalog-admission".to_owned())
            .spawn(move || {
                let _ = sender.send(loader());
            })
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "cannot start the catalog admission worker")
                    .with_context(error.to_string())
                    .with_help("Retry after freeing process or thread resources")
            })?;
        self.catalog_admission = Some(CatalogAdmission {
            receiver,
            worker: Some(worker),
        });
        Ok(())
    }

    fn poll_catalog_admission(&mut self) -> Result<bool, ShellError> {
        let Some(admission) = self.catalog_admission.as_ref() else {
            return Ok(false);
        };
        let outcome = match admission.receiver.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => Err(ShellError::new(
                ErrorCode::Io,
                "catalog admission worker ended without publishing a result",
            )
            .with_help("Restart Quirl to create a fresh interactive session")),
        };
        let Some(mut admission) = self.catalog_admission.take() else {
            return Err(ShellError::new(
                ErrorCode::Io,
                "catalog admission worker ownership was lost",
            )
            .with_help("Restart Quirl to create a fresh interactive session"));
        };
        Self::join_catalog_admission(&mut admission)?;
        let catalog = outcome?;

        // The worker owns construction only. Publish clones to every consumer
        // on the surface thread first, then expose the catalog slot last so no
        // reachable state can observe a partial generation.
        self.completion.publish_catalog(Arc::clone(&catalog));
        self.input_analysis.publish_catalog(Arc::clone(&catalog));
        self.catalog = Some(catalog);
        self.runtime.catalog_admitted();
        self.completion.resume_deferred()?;
        if let Some(deferred) = self.deferred_catalog_picker.take()
            && self.picker.active()
            && let Some(catalog) = self.catalog.as_deref()
        {
            let visible = match deferred {
                DeferredCatalogPicker::Palette { replace_end } => self
                    .picker
                    .replace_items(overlay::palette_items(catalog, replace_end)),
                DeferredCatalogPicker::Help {
                    line,
                    cursor,
                    initial_query,
                } => {
                    let items = overlay::palette_items(catalog, line.len());
                    // Upgrade the original cursor context when discovery adds
                    // a longer command contract. An edited query takes priority.
                    if self.picker.query() == Some(initial_query.as_str()) {
                        let query = contextual_help_query(catalog, &line, cursor);
                        self.picker
                            .open_with_query(items, "catalog help", false, &query)
                    } else {
                        self.picker.replace_items(items)
                    }
                }
            };
            self.completion
                .show_picker_results(visible, self.picker.label());
        }
        #[cfg(debug_assertions)]
        catalog_admission_published_test_hook()?;
        Ok(true)
    }

    fn join_catalog_admission(admission: &mut CatalogAdmission) -> Result<(), ShellError> {
        let Some(worker) = admission.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| {
            ShellError::new(ErrorCode::Io, "catalog admission worker panicked")
                .with_help("Restart Quirl to create a fresh interactive session")
        })
    }

    /// Replace the bounded immutable job and typed-result sources for the next frame.
    pub fn install_runtime_snapshot(&mut self, snapshot: InteractiveRuntimeSnapshot) {
        self.runtime.install_snapshot(snapshot);
    }

    /// Share the composition root's bounded raw HOME with filesystem completion.
    /// Updates apply on later completion requests without rereading process globals.
    pub fn set_home_directory(&mut self, home: InteractiveHomeDirectory) {
        self.completion.home_directory = home;
    }

    /// Replace the next editor and fuzzy picker history with a bounded snapshot.
    pub fn install_history_snapshot(&mut self, history: Vec<InteractiveHistoryEntry>) {
        self.history = history;
    }

    /// Replace the project picker with one complete, bounded index generation.
    ///
    /// This may be called between [`Self::read_line`] sessions; active sessions
    /// should use [`Self::set_project_provider`]. An open picker is rebuilt as
    /// one generation so ordinal identities cannot cross snapshots. Older
    /// generations are ignored. Paths remain structured [`PathBuf`] values and
    /// are never reconstructed from their terminal display representation.
    pub fn install_project_snapshot(
        &mut self,
        snapshot: InteractiveProjectSnapshot,
    ) -> Result<(), ShellError> {
        let _ = self.admit_project_snapshot(snapshot)?;
        Ok(())
    }

    fn admit_project_snapshot(
        &mut self,
        snapshot: InteractiveProjectSnapshot,
    ) -> Result<bool, ShellError> {
        if snapshot.generation < self.projects.generation || snapshot == self.projects {
            return Ok(false);
        }
        validate_project_snapshot(&snapshot)?;
        let active_query = self
            .project_picker_active
            .then(|| self.picker.query().unwrap_or_default().to_owned());
        self.projects = snapshot;
        if let Some(query) = active_query {
            self.show_project_picker(&query);
        }
        Ok(true)
    }

    /// Attach a cache-only provider of asynchronously discovered projects.
    ///
    /// The provider is polled once per bounded rich-surface loop turn. It must
    /// return immediately and leave filesystem and database work to an owned
    /// background worker.
    pub fn set_project_provider(&mut self, provider: Box<dyn InteractiveProjectProvider>) {
        self.project_provider = Some(provider);
    }

    /// Remove project discovery state without rebuilding the rich surface.
    ///
    /// An active project picker is dismissed before its generation-local item
    /// identities are discarded. Other pickers and editor state are unchanged.
    pub fn clear_project_provider(&mut self) {
        self.project_provider = None;
        self.projects = InteractiveProjectSnapshot::default();
        if self.project_picker_active {
            self.dismiss_picker();
        }
    }

    fn poll_project_provider(&mut self) -> bool {
        let outcome = self
            .project_provider
            .as_mut()
            .map(|provider| provider.poll_cached());
        match outcome {
            Some(Ok(Some(snapshot))) => match self.admit_project_snapshot(snapshot) {
                Ok(changed) => changed,
                Err(error) => self.set_project_provider_notice(&error.message),
            },
            Some(Err(error)) => self.set_project_provider_notice(&error.message),
            Some(Ok(None)) | None => false,
        }
    }

    fn request_project_refresh(&mut self) {
        let outcome = self
            .project_provider
            .as_mut()
            .map(|provider| provider.picker_opened());
        if let Some(Err(error)) = outcome {
            let _ = self.set_project_provider_notice(&error.message);
        }
    }

    fn set_project_provider_notice(&mut self, message: &str) -> bool {
        let notice = safe_project_text(message, PROJECT_STATUS_BYTES_MAX);
        if self.projects.status.as_deref() == Some(&notice) {
            return false;
        }
        self.projects.status = Some(notice);
        if self.project_picker_active {
            let query = self.picker.query().unwrap_or_default().to_owned();
            self.show_project_picker(&query);
        }
        true
    }

    /// Restore an editor buffer after a modal shell action completed between prompts.
    pub fn restore_input(&mut self, buffer: String, cursor: usize) -> Result<(), ShellError> {
        if buffer.len() > editor::MAX_EDITOR_BUFFER_BYTES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "restored editor input exceeds the rich-surface buffer limit",
            )
            .with_context(format!(
                "limit {} bytes; observed {} bytes",
                editor::MAX_EDITOR_BUFFER_BYTES,
                buffer.len()
            ))
            .with_help("Shorten the input before opening another modal action"));
        }
        if cursor > buffer.len() || !buffer.is_char_boundary(cursor) {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "restored editor cursor is not a UTF-8 boundary",
            )
            .with_help("Restart the interactive editor to discard the invalid saved cursor"));
        }
        self.pending_input = Some((buffer, cursor));
        Ok(())
    }

    /// Install the bounded local intent-search provider used by AI mode.
    pub fn set_intent_completer(&mut self, completer: Box<dyn ExtensionCompleter + Send>) {
        self.intent_completion.install(completer);
    }

    /// Seed the next editor session with one bounded command for human review.
    ///
    /// The pending value is consumed once by [`Self::read_line`]. This method
    /// never accepts or executes the command and rejects values larger than the
    /// editor's normal input ceiling.
    pub fn prefill_command(&mut self, command: &str) -> Result<(), ShellError> {
        if command.len() > editor::MAX_EDITOR_BUFFER_BYTES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "command prefill exceeded the editor byte limit",
            )
            .with_context(format!(
                "limit: {}; observed: {}",
                editor::MAX_EDITOR_BUFFER_BYTES,
                command.len()
            ))
            .with_help("Use a shorter generated command"));
        }
        self.pending_prefill = Some(command.to_owned());
        Ok(())
    }

    /// Append reviewed terminal text to the next editable prompt without submitting it.
    ///
    /// Sequential foreground pipelines may each return unread typing. Preserve
    /// their order and reject cumulative input beyond the editor's 64 KiB ceiling
    /// before changing an existing prefill. The caller must remove VT controls.
    pub fn append_recovered_input(&mut self, text: &str) -> Result<(), ShellError> {
        let observed = self
            .pending_prefill
            .as_ref()
            .map_or(0, String::len)
            .saturating_add(text.len());
        if observed > editor::MAX_EDITOR_BUFFER_BYTES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "recovered typing exceeded the editor byte limit",
            )
            .with_context(format!(
                "limit: {}; observed: {observed}",
                editor::MAX_EDITOR_BUFFER_BYTES
            ))
            .with_help("Enter a shorter command after the foreground programs finish"));
        }
        self.pending_prefill
            .get_or_insert_with(String::new)
            .push_str(text);
        Ok(())
    }

    /// Attach a nonblocking provider of completed asynchronous panel snapshots.
    pub fn set_panel_provider(&mut self, provider: Box<dyn InteractivePanelProvider>) {
        self.runtime.set_provider(provider);
    }

    /// Attach a nonblocking provider of cached bottom-bar activity.
    pub fn set_activity_provider(&mut self, provider: Box<dyn InteractiveActivityProvider>) {
        self.runtime.set_activity_provider(provider);
        if self.catalog.is_some() {
            self.runtime.catalog_admitted();
        }
    }

    /// Attach the nonblocking planner used when Enter is pressed in natural mode.
    pub fn set_intent_planner(&mut self, planner: Box<dyn InteractiveIntentPlanner>) {
        self.intent_planner = Some(planner);
    }

    /// Run one blocking interactive edit session and return after terminal release.
    ///
    /// Input is polled every 16 ms so completion and PATH-analysis results can be
    /// observed without blocking keyboard handling. Accepted non-empty input is
    /// appended and flushed to bounded history before returning. Ctrl-C, empty-buffer
    /// Ctrl-D and suspension return explicit signals only after cooked mode,
    /// cursor visibility, bracketed paste, and the alternate screen have been
    /// restored. Grammar-mode toggles redraw within this session and preserve
    /// the edit buffer. Terminal/history I/O and invalid completion requests
    /// return [`ShellError`]; the drop guard retries terminal cleanup on error.
    pub fn read_line(&mut self, prompt: &mut QuirlPrompt) -> Result<InteractiveSignal, ShellError> {
        self.dismiss_picker();
        self.environment.close();
        self.intent_completion.cancel();
        self.expand_completion_pending = false;
        let mut editor = EditorState::new(
            &self.keymap,
            self.history
                .iter()
                .map(|entry| entry.command_line.clone())
                .collect(),
        );
        if let Some((buffer, cursor)) = self.pending_input.take() {
            editor.restore(buffer, cursor);
        }
        if let Some(prefill) = self.pending_prefill.take() {
            editor.replace(0, 0, &prefill);
        }
        if prompt.mode == Mode::Natural {
            self.begin_intent_session();
        }
        let symbols = prompt.surface_symbols();
        self.terminal.enter()?;
        let unicode = symbols.uses_unicode();
        let color = std::io::stderr().is_terminal()
            && env::var_os("NO_COLOR").is_none()
            && !env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
        let theme = self.theme.with_color(color);
        let mut dirty = true;
        let mut prompt_prepared = false;

        loop {
            if dirty {
                let (terminal_width, terminal_height) = crate::terminal_size().unwrap_or((80, 24));
                if self.catalog.is_some() {
                    self.input_analysis
                        .ensure(editor.revision(), editor.buffer(), prompt.mode);
                }
                let timing_text = self.timing_text();
                let analysis = self.input_analysis.current();
                let context_left = prompt.surface_context_left();
                let context_right = prompt.surface_context_right();
                debug_assert!(
                    !self.picker.bottom_anchored() || self.picker.active(),
                    "bottom anchoring cannot outlive its picker overlay"
                );
                let model = FrameModel {
                    context_left: &context_left,
                    context_right: &context_right,
                    input_active: true,
                    editor: &editor,
                    completion: &self.completion,
                    mode: prompt.mode,
                    diagnostic: self
                        .semantic_hints
                        .then_some(analysis.diagnostic.as_ref())
                        .flatten(),
                    highlight_spans: &analysis.spans,
                    theme,
                    unicode,
                    symbols,
                    semantic_hints: self.semantic_hints,
                    hints: self.hints,
                    timings: timing_text.as_deref(),
                    compact: terminal_height < 8,
                    picker_query: self.picker.query(),
                    picker_layout: if self.picker.expanded() {
                        PickerLayout::Full
                    } else {
                        self.picker_layout
                    },
                    picker_preview: self.picker_preview,
                    detail_scroll: self.help_detail_scroll,
                    environment: self.environment.active().then_some(&self.environment),
                    runtime: &self.runtime,
                    transcript: Some(&self.transcript),
                    transcript_truncated: self.transcript_truncated,
                    output_focus: self.output_focus,
                    output_notice: self.output_notice.as_deref(),
                    busy_glyph: self.busy_glyph,
                };
                let transcript_area =
                    model.transcript_area(Rect::new(0, 0, terminal_width, terminal_height));
                let started = Instant::now();
                let mode = prompt.mode;
                let ((visible_screen, draw_elapsed), context_changed) =
                    draw_before_prompt_refresh(prompt, || {
                        let screen = self.terminal.draw(
                            &model,
                            self.explorer.as_ref(),
                            mode,
                            symbols,
                            self.screen_selection,
                            theme.selected(mode),
                        )?;
                        Ok((screen, started.elapsed()))
                    })?;
                self.visible_screen = visible_screen;
                self.transcript_area = transcript_area;
                // Ratatui flushes the backend before `draw` returns. Start the
                // bounded catalog worker only after that first visible frame;
                // subsequent input polling must not wait for discovery.
                self.begin_catalog_admission()?;
                self.record_draw(draw_elapsed);
                dirty = self.copy_pending_screen_selection()? || context_changed;
            } else if prompt.poll_native_context() {
                dirty = true;
            }

            if self.poll_catalog_admission()? {
                self.refresh_completion_after_catalog(&editor, prompt.mode)?;
                dirty = true;
                continue;
            }
            if self.semantic_hints && self.input_analysis.poll_path() {
                dirty = true;
                continue;
            }
            if self.completion.poll(editor.buffer(), editor.cursor()) {
                if self.expand_completion_pending {
                    self.expand_completion_pending = false;
                    let items = self.completion.items.clone();
                    self.open_picker_items(items, "completions", true);
                }
                dirty = true;
                continue;
            }
            if let Some(items) = self
                .intent_completion
                .poll(editor.buffer(), editor.cursor())
            {
                self.completion.show_picker_results(items, "AI intent");
                dirty = true;
                continue;
            }
            if self.runtime.poll_panels() {
                dirty = true;
                continue;
            }
            if self.runtime.poll_activity() {
                dirty = true;
                continue;
            }
            if self.poll_project_provider() {
                dirty = true;
                continue;
            }
            if self.environment.poll() {
                dirty = true;
                continue;
            }
            if self.poll_intent_planner(&mut editor, prompt)? {
                dirty = true;
                continue;
            }
            if !event::poll(EVENT_POLL).map_err(terminal_error("poll terminal input"))? {
                continue;
            }
            let input_event = event::read().map_err(terminal_error("read terminal input"))?;
            if self.intent_planning_started.is_some() {
                if let Event::Key(key) = input_event
                    && key.kind != KeyEventKind::Release
                    && (key.code == KeyCode::Esc
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)))
                {
                    if let Some(planner) = self.intent_planner.as_mut() {
                        planner.cancel();
                    }
                    self.intent_planning_started = None;
                    self.busy_glyph = None;
                    self.intent_phase = None;
                    self.push_intent_message(
                        IntentConversationRole::Assistant,
                        "Cancelled. You can edit the request and try again.",
                    );
                    self.refresh_intent_notice(Duration::ZERO);
                }
                dirty = true;
                continue;
            }
            if self.semantic_hints && !prompt_prepared {
                // The empty first frame cannot use PATH discovery. Start the
                // bounded worker when input first arrives, then publish only a
                // complete snapshot on a later loop turn.
                self.input_analysis.prepare_prompt();
                prompt_prepared = true;
            }
            dirty = true;
            match input_event {
                Event::Paste(text) => {
                    self.clear_transient_output_notice();
                    if self.environment.active() {
                        self.environment.insert_filter_text(&text);
                        continue;
                    }
                    self.return_to_tail_for_input();
                    if let Some(explorer) = self.explorer.as_mut() {
                        explorer.insert_query(&text);
                        continue;
                    }
                    if self.picker.active() {
                        self.update_picker_query(|picker| picker.insert_query(&text));
                        continue;
                    }
                    self.expand_completion_pending = false;
                    editor.insert_paste(&text);
                    self.refresh_completion_after_edit(&editor, prompt.mode)?;
                }
                Event::Resize(_, _) => continue,
                Event::Mouse(mouse) => {
                    if self.environment.active() {
                        continue;
                    }
                    self.handle_mouse(mouse);
                    continue;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if self.screen_selection.is_some()
                        && key.code == KeyCode::Char('c')
                        && key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
                    {
                        self.copy_output_selection()?;
                        continue;
                    }
                    if self.output_focus {
                        self.handle_output_key(key.code, key.modifiers)?;
                        continue;
                    }
                    self.clear_transient_output_notice();
                    if self.explorer.is_some() {
                        let action = self
                            .explorer
                            .as_mut()
                            .map_or(DirectoryExplorerAction::Dismiss, |explorer| {
                                explorer.handle_key(key)
                            });
                        match action {
                            DirectoryExplorerAction::Pending => continue,
                            DirectoryExplorerAction::Dismiss => {
                                self.explorer = None;
                                continue;
                            }
                            DirectoryExplorerAction::ChangeDirectory(path) => {
                                self.explorer = None;
                                return Ok(InteractiveSignal::ChangeDirectory {
                                    path,
                                    buffer: editor.buffer().to_owned(),
                                    cursor: editor.cursor(),
                                });
                            }
                        }
                    }
                    if self.leader_active {
                        if key.code == KeyCode::Char('u')
                            && let Some(signal) = self.take_project_open_signal(
                                editor.buffer().to_owned(),
                                editor.cursor(),
                            )
                        {
                            self.dismiss_picker();
                            self.terminal.pause_for_execution()?;
                            return Ok(signal);
                        }
                        self.return_to_tail_for_input();
                        self.handle_leader_key(key.code, prompt, editor.buffer(), editor.cursor())?;
                        continue;
                    }
                    if self.environment.active() {
                        self.handle_environment_key(key, &mut editor, prompt.mode)?;
                        continue;
                    }
                    if key.code == KeyCode::F(6) {
                        let _ = self.runtime.cycle_panel_focus();
                        continue;
                    }
                    if key.code == KeyCode::F(1) {
                        if self.help_active {
                            self.completion.next();
                            self.help_detail_scroll = 0;
                        } else {
                            self.open_context_help(editor.buffer(), editor.cursor());
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::PageUp if self.transcript.line_count() > 0 => {
                            self.transcript.page_up(self.transcript_visible_rows());
                            continue;
                        }
                        KeyCode::PageDown if self.transcript.line_count() > 0 => {
                            self.transcript.page_down(self.transcript_visible_rows());
                            continue;
                        }
                        KeyCode::End
                            if key.modifiers.contains(KeyModifiers::CONTROL)
                                && self.transcript.line_count() > 0 =>
                        {
                            self.transcript.scroll_to_end();
                            continue;
                        }
                        _ => {}
                    }
                    self.return_to_tail_for_input();
                    if self.picker.active() {
                        match key.code {
                            KeyCode::Up | KeyCode::BackTab => {
                                self.completion.previous();
                                self.help_detail_scroll = 0;
                                continue;
                            }
                            KeyCode::Down | KeyCode::Tab => {
                                self.completion.next();
                                self.help_detail_scroll = 0;
                                continue;
                            }
                            KeyCode::Enter => {
                                if self.help_active {
                                    self.dismiss_picker();
                                } else if let Some(item) = self.completion.selected_item().cloned()
                                {
                                    if self.project_picker_active {
                                        let buffer = editor.buffer().to_owned();
                                        let cursor = editor.cursor();
                                        let signal =
                                            self.project_change_signal(&item, buffer, cursor)?;
                                        self.dismiss_picker();
                                        return Ok(signal);
                                    }
                                    self.accept_selected_completion(&mut editor, prompt.mode)?;
                                }
                                continue;
                            }
                            KeyCode::Left if self.help_active => {
                                self.help_detail_scroll = self.help_detail_scroll.saturating_sub(1);
                                continue;
                            }
                            KeyCode::Right if self.help_active => {
                                self.help_detail_scroll = self
                                    .help_detail_scroll
                                    .saturating_add(1)
                                    .min(HELP_DETAIL_SCROLL_MAX);
                                continue;
                            }
                            KeyCode::Esc => {
                                self.dismiss_picker();
                                continue;
                            }
                            KeyCode::Backspace => {
                                self.update_picker_query(PickerOverlay::backspace_query);
                                continue;
                            }
                            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.update_picker_query(PickerOverlay::clear_query);
                                continue;
                            }
                            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                self.update_picker_query(PickerOverlay::kill_query_word);
                                continue;
                            }
                            KeyCode::Char(character)
                                if !key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                            {
                                let text = character.to_string();
                                self.update_picker_query(|picker| picker.insert_query(&text));
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if prompt.mode == Mode::Natural
                        && key.code == KeyCode::Tab
                        && self.accept_intent_proposal(&mut editor, prompt)?
                    {
                        continue;
                    }
                    if prompt.mode == Mode::Natural && key.code == KeyCode::Esc {
                        self.end_intent_session();
                        prompt.set_mode(Mode::Command);
                        editor.clear();
                        self.completion.dismiss();
                        continue;
                    }
                    if self.completion.open && !self.picker.active() {
                        match key.code {
                            KeyCode::Up => {
                                self.handle_completion_up(
                                    prompt.mode,
                                    editor.buffer(),
                                    editor.cursor(),
                                );
                                continue;
                            }
                            KeyCode::Down => {
                                self.completion.next();
                                continue;
                            }
                            KeyCode::Tab if prompt.mode == Mode::Natural => {
                                if let Some(item) = self.completion.selected_item().cloned() {
                                    editor.replace(0, editor.buffer().len(), &item.value);
                                    prompt.set_mode(Mode::Command);
                                    self.intent_completion.cancel();
                                    self.completion.dismiss();
                                    self.refresh_completion_after_edit(&editor, prompt.mode)?;
                                }
                                continue;
                            }
                            KeyCode::Enter
                                if prompt.mode != Mode::Natural
                                    && self.completion.accepts_enter()
                                    && self.completion.selected_item().is_some() =>
                            {
                                self.accept_selected_completion(&mut editor, prompt.mode)?;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if key.code == KeyCode::Right
                        && editor.cursor() == editor.buffer().len()
                        && editor.accept_suggestion()
                    {
                        continue;
                    }
                    let action = editor.apply_key(key, self.completion.open);
                    match action {
                        EditAction::Accept => {
                            if prompt.mode == Mode::Natural
                                && editor.buffer().trim().is_empty()
                                && self.accept_intent_proposal(&mut editor, prompt)?
                            {
                                continue;
                            }
                            if input_is_incomplete(editor.buffer(), prompt.mode) {
                                editor.apply(EditAction::ForceNewline);
                                continue;
                            }
                            if editor.buffer().trim().is_empty() {
                                // Nothing to run: skip the terminal pause/resume
                                // round trip a real command would need, but still
                                // advance the transcript by one bare prompt line —
                                // a plain shell's Enter on an empty line moves the
                                // cursor down even though nothing is added to
                                // command history.
                                self.append_transcript_line("❯");
                                editor.clear();
                                self.dismiss_picker();
                                continue;
                            }
                            if prompt.mode == Mode::Natural && self.intent_planner.is_some() {
                                let catalog = self.catalog.clone().ok_or_else(|| {
                                    ShellError::new(
                                        ErrorCode::Io,
                                        "Codex planning requires the admitted command catalog",
                                    )
                                    .with_help("Wait for catalog loading to finish and retry")
                                })?;
                                let intent = editor.buffer().to_owned();
                                if let Some(planner) = self.intent_planner.as_mut() {
                                    planner.start(&intent, catalog)?;
                                }
                                self.push_intent_message(IntentConversationRole::User, &intent);
                                editor.clear();
                                self.intent_proposal = None;
                                self.intent_token_usage = None;
                                self.intent_planning_started = Some(Instant::now());
                                self.busy_glyph =
                                    Some(spinner_glyph(Duration::ZERO, prompt.surface_symbols()));
                                self.intent_phase = Some("connecting to Codex".to_owned());
                                self.refresh_intent_notice(Duration::ZERO);
                                self.dismiss_picker();
                                continue;
                            }
                            let buffer = editor.buffer().to_owned();
                            self.pending_project_open = None;
                            self.append_history(&buffer)?;
                            self.terminal.pause_for_execution()?;
                            return Ok(InteractiveSignal::Success(buffer));
                        }
                        EditAction::Eof if editor.buffer().is_empty() => {
                            self.terminal.release()?;
                            return Ok(InteractiveSignal::CtrlD);
                        }
                        EditAction::Eof => {
                            editor.apply(EditAction::Delete);
                        }
                        EditAction::Cancel => {
                            editor.clear();
                            self.dismiss_picker();
                            return Ok(InteractiveSignal::CtrlC);
                        }
                        EditAction::ToggleGrammarMode => {
                            self.toggle_grammar_mode(prompt);
                        }
                        EditAction::OpenLeader => self.open_leader(editor.buffer().len()),
                        EditAction::Complete => {
                            if self.completion.open && !self.completion.automatic {
                                self.completion.next();
                            } else {
                                self.completion.request(
                                    editor.buffer(),
                                    editor.cursor(),
                                    prompt.mode,
                                )?;
                            }
                        }
                        EditAction::ExpandCompletionPicker => {
                            if self.completion.streaming {
                                self.expand_completion_pending = true;
                            } else if self.completion.open {
                                let items = self.completion.items.clone();
                                self.open_picker_items(items, "completions", true);
                            } else {
                                self.completion.request(
                                    editor.buffer(),
                                    editor.cursor(),
                                    prompt.mode,
                                )?;
                                self.expand_completion_pending = true;
                            }
                        }
                        EditAction::Dismiss => {
                            self.dismiss_picker();
                            if prompt.mode == Mode::Natural {
                                self.intent_completion.cancel();
                            }
                        }
                        EditAction::OpenPicker(kind) => {
                            self.open_picker(kind, editor.buffer(), editor.cursor(), "picker");
                        }
                        EditAction::ClearScreen => {
                            self.terminal.force_repaint()?;
                        }
                        EditAction::Suspend => {
                            self.terminal.release()?;
                            return Ok(InteractiveSignal::Suspend);
                        }
                        action => {
                            if editor.apply(action) {
                                self.expand_completion_pending = false;
                                self.refresh_completion_after_edit(&editor, prompt.mode)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn poll_intent_planner(
        &mut self,
        _editor: &mut EditorState,
        prompt: &mut QuirlPrompt,
    ) -> Result<bool, ShellError> {
        let Some(started) = self.intent_planning_started else {
            return Ok(false);
        };
        let elapsed = started.elapsed();
        let next_glyph = spinner_glyph(elapsed, prompt.surface_symbols());
        let mut changed = self.busy_glyph != Some(next_glyph);
        self.busy_glyph = Some(next_glyph);
        let update = match self.intent_planner.as_mut() {
            Some(planner) => planner.poll_cached(),
            None => return Ok(false),
        };
        match update {
            Ok(None) => {
                self.refresh_intent_notice(elapsed);
                Ok(changed)
            }
            Err(error) => {
                self.intent_planning_started = None;
                self.busy_glyph = None;
                self.intent_phase = None;
                self.push_intent_message(
                    IntentConversationRole::Assistant,
                    &format!("I couldn't finish that: {}", error.message),
                );
                self.refresh_intent_notice(elapsed);
                Ok(true)
            }
            Ok(Some(InteractiveIntentPlannerUpdate::Progress { model, message })) => {
                if let Some(label) = model {
                    let safe = escape_terminal_line(&label);
                    self.intent_model = safe
                        .rsplit_once(" · ")
                        .map(|(model, effort)| (model.to_owned(), effort.to_owned()))
                        .or_else(|| Some((safe, "default".to_owned())));
                }
                self.intent_phase = Some(escape_terminal_line(&message));
                self.refresh_intent_notice(elapsed);
                Ok(true)
            }
            Ok(Some(InteractiveIntentPlannerUpdate::Reply {
                command,
                message,
                model,
                effort,
                token_usage,
                elapsed_ms,
            })) => {
                if let Some(command) = command.as_ref()
                    && (command.is_empty() || command.len() > editor::MAX_EDITOR_BUFFER_BYTES)
                {
                    return Err(ShellError::new(
                        ErrorCode::ResourceLimit,
                        "Codex returned a command outside the editor byte limit",
                    )
                    .with_context(format!(
                        "limit: {}; observed: {}",
                        editor::MAX_EDITOR_BUFFER_BYTES,
                        command.len()
                    ))
                    .with_help("Retry with a request that produces a shorter command"));
                }
                self.intent_proposal = command;
                self.intent_model =
                    Some((escape_terminal_line(&model), escape_terminal_line(&effort)));
                self.intent_token_usage = token_usage;
                self.push_intent_message(IntentConversationRole::Assistant, &message);
                self.intent_planning_started = None;
                self.busy_glyph = None;
                self.intent_phase = None;
                self.intent_completion.cancel();
                self.completion.dismiss();
                self.refresh_intent_notice(Duration::from_millis(elapsed_ms));
                changed = true;
                Ok(changed)
            }
        }
    }

    fn toggle_grammar_mode(&mut self, prompt: &mut QuirlPrompt) {
        // Failure model: releasing the alternate screen here destroys the
        // editor state, while host feedback commits an unbounded line per
        // toggle to scrollback. A mode switch transfers no terminal or process
        // ownership, so it must remain an in-frame state transition. Preserve
        // the buffer and cursor, invalidate mode-sensitive transient UI, and
        // let the already-dirty loop repaint exactly one current frame.
        prompt.toggle_mode();
        if prompt.mode == Mode::Natural {
            self.begin_intent_session();
        } else {
            self.end_intent_session();
        }
        self.expand_completion_pending = false;
        self.completion.cancel_for_edit();
        self.deferred_catalog_picker = None;
        self.picker.dismiss();
        self.project_picker_active = false;
        self.help_active = false;
        self.help_detail_scroll = 0;
    }

    fn accept_intent_proposal(
        &mut self,
        editor: &mut EditorState,
        prompt: &mut QuirlPrompt,
    ) -> Result<bool, ShellError> {
        let Some(command) = self.intent_proposal.take() else {
            return Ok(false);
        };
        editor.replace(0, editor.buffer().len(), &command);
        self.end_intent_session();
        prompt.set_mode(Mode::Command);
        self.completion.dismiss();
        self.refresh_completion_after_edit(editor, prompt.mode)?;
        Ok(true)
    }

    fn begin_intent_session(&mut self) {
        self.intent_planning_started = None;
        self.intent_conversation.clear();
        self.intent_proposal = None;
        self.intent_model = None;
        self.intent_token_usage = None;
        self.intent_phase = None;
        self.output_notice = None;
        self.busy_glyph = None;
        if let Some(planner) = self.intent_planner.as_mut() {
            planner.begin_session();
        }
    }

    fn end_intent_session(&mut self) {
        if let Some(planner) = self.intent_planner.as_mut() {
            planner.end_session();
        }
        self.intent_planning_started = None;
        self.intent_conversation.clear();
        self.intent_proposal = None;
        self.intent_model = None;
        self.intent_token_usage = None;
        self.intent_phase = None;
        self.output_notice = None;
        self.busy_glyph = None;
    }

    fn push_intent_message(&mut self, role: IntentConversationRole, text: &str) {
        let safe = escape_terminal_line(text);
        let text = truncate_utf8(&safe, INTENT_CONVERSATION_MESSAGE_BYTES_MAX);
        self.intent_conversation
            .push_back(IntentConversationMessage { role, text });
        while self.intent_conversation.len() > INTENT_CONVERSATION_MESSAGES_MAX
            || self
                .intent_conversation
                .iter()
                .map(|message| message.text.len())
                .sum::<usize>()
                > INTENT_CONVERSATION_RETAINED_BYTES_MAX
        {
            self.intent_conversation.pop_front();
        }
    }

    fn refresh_intent_notice(&mut self, elapsed: Duration) {
        let (model, effort) = self
            .intent_model
            .as_ref()
            .map(|(model, effort)| (model.as_str(), effort.as_str()))
            .unwrap_or(("Codex", "model pending"));
        let mut notice = format!("MODEL\t{model}\t{effort}");
        if let Some(usage) = self.intent_token_usage {
            notice.push_str("\nTOKENS\t");
            notice.push_str(&usage.turn_total.to_string());
            notice.push('\t');
            notice.push_str(&usage.session_total.to_string());
        }
        for message in &self.intent_conversation {
            notice.push('\n');
            notice.push_str(match message.role {
                IntentConversationRole::User => "USER\t",
                IntentConversationRole::Assistant => "ASSISTANT\t",
            });
            notice.push_str(&message.text);
        }
        if let Some(command) = self.intent_proposal.as_deref() {
            notice.push_str("\nCOMMAND\t");
            let safe = escape_terminal_line(command);
            notice.push_str(&truncate_utf8(&safe, INTENT_CONVERSATION_MESSAGE_BYTES_MAX));
        }
        if let Some(phase) = self.intent_phase.as_deref() {
            notice.push_str("\nBUSY\t");
            notice.push_str(phase);
            notice.push('\t');
            notice.push_str(&format_elapsed(elapsed));
        }
        self.output_notice = Some(notice);
    }

    fn open_leader(&mut self, replace_end: usize) {
        self.leader_active = true;
        let mut entries = vec![
            ("n", "Normal mode", "Native commands and pipelines"),
            ("d", "Data mode", "Typed data expressions and pipelines"),
            ("i", "AI mode", "Describe your intent in everyday language"),
            ("h", "History", "Search commands from every directory"),
            ("p", "Command palette", "Browse Quirl commands and help"),
            ("f", "Files", "Find a file in this directory"),
            ("g", "Projects", "Jump to an indexed Git repository"),
            ("c", "Explorer", "Browse columns, preview, and jump"),
            ("j", "Jobs", "Inspect background jobs"),
            ("r", "Results", "Inspect recent typed data"),
            (
                "e",
                "Environment",
                "Inspect PATH and variables inherited by commands",
            ),
            ("o", "Output", "Scroll and copy retained command output"),
        ];
        if self.pending_project_open.is_some() {
            entries.push(("u", "Open project", "Enter the repository just cloned"));
        }
        let items = entries
            .into_iter()
            .map(|(key, name, detail)| completion::CompletionItem {
                value: key.to_owned(),
                display: format!("{key}  {name}"),
                summary: detail.to_owned(),
                detail: detail.to_owned(),
                replace_start: 0,
                replace_end,
                match_indices: Vec::new(),
                kind: completion::CompletionKind::Command,
                source: "quirl",
                trust: "built-in",
            })
            .collect();
        self.open_picker_items(items, "Alt-Q · Quirl", false);
        self.leader_active = true;
    }

    fn handle_leader_key(
        &mut self,
        key: KeyCode,
        prompt: &mut QuirlPrompt,
        line: &str,
        cursor: usize,
    ) -> Result<(), ShellError> {
        self.leader_active = false;
        self.dismiss_picker();
        match key {
            KeyCode::Char('n') => {
                prompt.set_mode(Mode::Command);
                self.refresh_completion_after_text(line, cursor, prompt.mode)?;
            }
            KeyCode::Char('d') => {
                prompt.set_mode(Mode::Data);
                self.refresh_completion_after_text(line, cursor, prompt.mode)?;
            }
            KeyCode::Char('i') => {
                prompt.set_mode(Mode::Natural);
                if let Some(planner) = self.intent_planner.as_mut() {
                    planner.prepare();
                }
                self.refresh_completion_after_text(line, cursor, prompt.mode)?;
            }
            KeyCode::Char('h') => {
                self.open_picker(editor::PickerKind::History, line, cursor, "history")
            }
            KeyCode::Char('p') => {
                self.open_picker(editor::PickerKind::Palette, line, cursor, "commands")
            }
            KeyCode::Char('f') => {
                self.open_picker(editor::PickerKind::Files, line, cursor, "files")
            }
            KeyCode::Char('g') => self.open_project_picker(line.len()),
            KeyCode::Char('c') => {
                self.open_directory_explorer()?;
            }
            KeyCode::Char('j') => self.open_picker(editor::PickerKind::Jobs, line, cursor, "jobs"),
            KeyCode::Char('r') => {
                self.open_picker(editor::PickerKind::Data, line, cursor, "results")
            }
            KeyCode::Char('e') => {
                self.environment.open(self.runtime.environment())?;
            }
            KeyCode::Char('o') if self.transcript.line_count() > 0 => self.open_output_focus(),
            _ => {}
        }
        Ok(())
    }

    fn open_directory_explorer(&mut self) -> Result<(), ShellError> {
        let current_dir = env::current_dir().map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read the current directory")
                .with_context(error.to_string())
                .with_help("Change to an accessible directory before opening the explorer")
        })?;
        self.dismiss_picker();
        self.completion.dismiss();
        self.help_active = false;
        self.help_detail_scroll = 0;
        self.explorer = Some(DirectoryExplorer::open(&current_dir)?);
        Ok(())
    }

    fn transcript_visible_rows(&self) -> usize {
        let rows = crate::terminal_size().map_or(24, |(_, rows)| rows);
        transcript_visible_rows_for_terminal(rows, self.transcript.follows_tail())
    }

    fn return_to_tail_for_input(&mut self) {
        self.transcript.scroll_to_end();
        self.dismiss_output_selection();
    }

    fn clear_transient_output_notice(&mut self) {
        if self.intent_conversation.is_empty() {
            self.output_notice = None;
        }
    }

    fn open_output_focus(&mut self) {
        self.screen_selection = None;
        self.screen_copy_pending = false;
        self.output_focus = true;
        self.output_notice = None;
        self.output_cursor_line = self.transcript.line_count().saturating_sub(1);
        self.output_anchor_line = Some(self.output_cursor_line);
        self.mouse_drag = None;
        self.select_output_range();
    }

    fn handle_output_key(
        &mut self,
        key: KeyCode,
        modifiers: KeyModifiers,
    ) -> Result<(), ShellError> {
        let visible_rows = self.transcript_visible_rows();
        match key {
            KeyCode::Esc => {
                self.dismiss_output_selection();
            }
            KeyCode::Up => {
                self.output_cursor_line = self.output_cursor_line.saturating_sub(1);
                self.select_output_range();
            }
            KeyCode::Down => {
                self.output_cursor_line = self
                    .output_cursor_line
                    .saturating_add(1)
                    .min(self.transcript.line_count().saturating_sub(1));
                self.select_output_range();
            }
            KeyCode::PageUp => {
                self.transcript.page_up(visible_rows);
                self.output_cursor_line = self.transcript.visible_range(visible_rows).start;
                self.select_output_range();
            }
            KeyCode::PageDown => {
                self.transcript.page_down(visible_rows);
                self.output_cursor_line = self
                    .transcript
                    .visible_range(visible_rows)
                    .end
                    .saturating_sub(1);
                self.select_output_range();
            }
            KeyCode::Home | KeyCode::Char('g') => {
                while self.transcript.page_up(visible_rows) {}
                self.output_cursor_line = 0;
                self.select_output_range();
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.transcript.scroll_to_end();
                self.output_cursor_line = self.transcript.line_count().saturating_sub(1);
                self.select_output_range();
            }
            KeyCode::Char('v') => {
                self.output_anchor_line = Some(self.output_cursor_line);
                self.select_output_range();
            }
            KeyCode::Char('y') => self.copy_output_selection()?,
            KeyCode::Char('c')
                if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                self.copy_output_selection()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let visible_rows = usize::from(self.transcript_area.height);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.transcript
                    .scroll_up(TRANSCRIPT_MOUSE_SCROLL_LINES, visible_rows);
            }
            MouseEventKind::ScrollDown => {
                self.transcript
                    .scroll_down(TRANSCRIPT_MOUSE_SCROLL_LINES, visible_rows);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.mouse_is_on_scrollbar(mouse.column, mouse.row) {
                    self.screen_selection = None;
                    self.screen_copy_pending = false;
                    self.mouse_drag = Some(MouseDrag::Scrollbar);
                    self.scrollbar_to_row(mouse.row);
                } else if let Some((before, after)) =
                    self.visible_screen.hit_test(mouse.column, mouse.row)
                {
                    self.transcript.clear_selection();
                    self.output_focus = true;
                    self.output_notice = None;
                    self.output_anchor_line = None;
                    self.screen_selection = Some(ScreenSelection::new(before, after));
                    self.screen_copy_pending = false;
                    self.mouse_drag = Some(MouseDrag::ScreenSelection {
                        anchor: before,
                        dragged: false,
                    });
                } else {
                    self.dismiss_output_selection();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.mouse_drag {
                Some(MouseDrag::ScreenSelection { anchor, .. }) => {
                    self.update_screen_selection(anchor, mouse.column, mouse.row);
                    self.mouse_drag = Some(MouseDrag::ScreenSelection {
                        anchor,
                        dragged: true,
                    });
                }
                Some(MouseDrag::Scrollbar) => self.scrollbar_to_row(mouse.row),
                None => {}
            },
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(MouseDrag::ScreenSelection { anchor, dragged }) = self.mouse_drag {
                    self.update_screen_selection(anchor, mouse.column, mouse.row);
                    self.mouse_drag = None;
                    // The final pointer coordinate must be rendered before copy so the
                    // bounded snapshot and the highlighted cells describe one frame.
                    self.screen_copy_pending = dragged && self.terminal.input_active;
                    self.output_focus = false;
                } else if self.mouse_drag == Some(MouseDrag::Scrollbar) {
                    self.scrollbar_to_row(mouse.row);
                    self.mouse_drag = None;
                }
            }
            _ => {}
        }
    }

    fn mouse_is_on_scrollbar(&self, column: u16, row: u16) -> bool {
        let area = self.transcript_area;
        area.height > 0
            && self.transcript.line_count() > usize::from(area.height)
            && column == area.right().saturating_sub(1)
            && row >= area.y
            && row < area.bottom()
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "the row is clamped to the rendered viewport before converting it to a scroll offset"
    )]
    fn scrollbar_to_row(&mut self, row: u16) {
        let area = self.transcript_area;
        let visible_rows = usize::from(area.height);
        if visible_rows == 0 || self.transcript.line_count() <= visible_rows {
            return;
        }
        let track_max = usize::from(area.height.saturating_sub(1));
        let track_position = usize::from(
            row.saturating_sub(area.y)
                .min(area.height.saturating_sub(1)),
        );
        let maximum_start = self.transcript.line_count().saturating_sub(visible_rows);
        let start = match track_max {
            0 => 0,
            track_length => {
                track_position
                    .saturating_mul(maximum_start)
                    .saturating_add(track_length / 2)
                    / track_length
            }
        };
        self.transcript.scroll_to(start, visible_rows);
    }

    fn update_screen_selection(&mut self, anchor: ScreenPosition, column: u16, row: u16) {
        let Some((before, after)) = self.visible_screen.hit_test(column, row) else {
            return;
        };
        let head = if before < anchor { before } else { after };
        if let Some(selection) = self.screen_selection.as_mut() {
            selection.update(head);
        }
    }

    fn dismiss_output_selection(&mut self) {
        self.output_focus = false;
        self.output_anchor_line = None;
        self.mouse_drag = None;
        self.screen_selection = None;
        self.screen_copy_pending = false;
        self.transcript.clear_selection();
        if self.intent_conversation.is_empty() {
            self.output_notice = None;
        } else {
            let elapsed = self
                .intent_planning_started
                .map_or(Duration::ZERO, |started| started.elapsed());
            self.refresh_intent_notice(elapsed);
        }
    }

    fn select_output_range(&mut self) {
        let Some(anchor_line) = self.output_anchor_line else {
            return;
        };
        let anchor_offset = self.transcript.line(anchor_line).map_or(0, str::len);
        let cursor_offset = self
            .transcript
            .line(self.output_cursor_line)
            .map_or(0, str::len);
        self.transcript.begin_selection(TextPosition {
            line_index: anchor_line,
            byte_offset: if anchor_line <= self.output_cursor_line {
                0
            } else {
                anchor_offset
            },
        });
        self.transcript.update_selection(TextPosition {
            line_index: self.output_cursor_line,
            byte_offset: if self.output_cursor_line >= anchor_line {
                cursor_offset
            } else {
                0
            },
        });
    }

    fn copy_output_selection(&mut self) -> Result<(), ShellError> {
        let selected = if self.screen_selection.is_some() {
            self.visible_screen
                .selected_text_bounded()
                .map(|text| text.map(str::to_owned))
        } else {
            self.transcript
                .selected_text_bounded(TRANSCRIPT_COPY_BYTES_MAX)
        };
        let text = match selected {
            Ok(Some(text)) if !text.is_empty() => text,
            Ok(Some(_)) | Ok(None) => {
                self.output_notice = Some("nothing selected to copy".to_owned());
                return Ok(());
            }
            Err(observed_bytes) => {
                self.output_notice = Some(format!(
                    "selection is {observed_bytes} bytes; copy limit is {TRANSCRIPT_COPY_BYTES_MAX}"
                ));
                return Ok(());
            }
        };
        if let Err(error) = self.terminal.copy_to_clipboard(&text) {
            self.output_notice = Some(format!("copy failed: {}", error.message));
            return Ok(());
        }
        self.output_notice = Some(format!(
            "copied {} bytes via terminal clipboard",
            text.len()
        ));
        Ok(())
    }

    fn copy_pending_screen_selection(&mut self) -> Result<bool, ShellError> {
        if !self.screen_copy_pending {
            return Ok(false);
        }
        self.screen_copy_pending = false;
        self.copy_output_selection()?;
        Ok(true)
    }

    fn refresh_completion_after_text(
        &mut self,
        line: &str,
        cursor: usize,
        mode: Mode,
    ) -> Result<(), ShellError> {
        self.completion.cancel_for_edit();
        if mode == Mode::Natural {
            self.intent_completion.request(line, cursor);
            return Ok(());
        }
        self.intent_completion.cancel();
        let Some(catalog) = self.catalog.as_deref() else {
            return Ok(());
        };
        if should_open_automatic_completion(
            catalog,
            line,
            cursor,
            mode,
            self.completion_auto,
            self.completion_min_chars,
        ) {
            self.completion.request_automatic(line, cursor, mode)?;
        }
        Ok(())
    }

    fn refresh_completion_after_edit(
        &mut self,
        editor: &EditorState,
        mode: Mode,
    ) -> Result<(), ShellError> {
        self.refresh_completion_after_text(editor.buffer(), editor.cursor(), mode)
    }

    fn refresh_completion_after_catalog(
        &mut self,
        editor: &EditorState,
        mode: Mode,
    ) -> Result<(), ShellError> {
        // Typing can precede publication without creating a request. Reapply
        // the normal automatic policy to the current buffer exactly once at
        // admission, while preserving explicit completion, prior dismissal,
        // and every overlay. Background publication is not a new user edit.
        if mode == Mode::Natural
            || self.completion.was_dismissed()
            || self.completion.open
            || self.picker.active()
            || self.leader_active
            || self.environment.active()
            || self.explorer.is_some()
        {
            return Ok(());
        }
        self.refresh_completion_after_edit(editor, mode)
    }

    fn handle_completion_up(&mut self, mode: Mode, line: &str, cursor: usize) {
        if mode == Mode::Natural || self.completion.accepts_enter() {
            self.completion.previous();
        } else {
            // Informational popups do not capture normal history recall. Once
            // the user chooses completion navigation, both arrows stay there.
            self.completion.dismiss();
            self.open_picker(editor::PickerKind::History, line, cursor, "history");
        }
    }

    /// Accepting a path is an edit, never command execution. Directory choices
    /// start one fresh bounded request so Enter can continue through children.
    /// Cancel old workers before publishing the new path; a rejected oversized
    /// replacement retains both the buffer and its actionable resource notice.
    fn accept_selected_completion(
        &mut self,
        editor: &mut EditorState,
        mode: Mode,
    ) -> Result<(), ShellError> {
        let Some(item) = self.completion.selected_item().cloned() else {
            return Ok(());
        };
        let revision = editor.revision();
        editor.replace(item.replace_start, item.replace_end, &item.value);
        if editor.revision() == revision {
            return Ok(());
        }
        let browse_directory =
            item.source == "filesystem" && item.kind == completion::CompletionKind::Directory;
        self.completion.cancel_for_edit();
        self.dismiss_picker();
        if browse_directory {
            self.completion
                .request(editor.buffer(), editor.cursor(), mode)?;
        }
        Ok(())
    }

    fn open_picker(
        &mut self,
        kind: editor::PickerKind,
        line: &str,
        cursor: usize,
        label: &'static str,
    ) {
        self.deferred_catalog_picker = None;
        self.project_picker_active = false;
        self.help_active = false;
        self.help_detail_scroll = 0;
        let items = match kind {
            editor::PickerKind::Projects => self.project_items(cursor),
            editor::PickerKind::Jobs => self.runtime.job_items(line.len()),
            editor::PickerKind::Data => self.runtime.data_items(line.len()),
            // Durable history is installed before the first prompt. Catalog
            // admission may still be pending or unavailable, but must not hide
            // that independent snapshot or make recall depend on discovery I/O.
            editor::PickerKind::History => overlay::history_items(&self.history, line.len()),
            // Files and directories depend on the current filesystem, not on
            // imported command metadata. A cold catalog must not hide paths.
            editor::PickerKind::Files | editor::PickerKind::Directories => {
                overlay::filesystem_items(kind, line, cursor)
            }
            editor::PickerKind::Palette => {
                self.catalog.as_deref().map_or_else(Vec::new, |catalog| {
                    overlay::items(kind, catalog, &self.history, line, cursor)
                })
            }
        };
        if kind == editor::PickerKind::History {
            let visible = self.picker.open_with_query(items, label, true, line);
            self.completion.show_picker_results(visible, label);
        } else if kind.bottom_anchored() {
            self.open_bottom_anchored_picker(items, label);
            self.project_picker_active = kind == editor::PickerKind::Projects;
        } else {
            self.open_picker_items(items, label, false);
        }
        if kind == editor::PickerKind::Palette && self.catalog.is_none() {
            self.deferred_catalog_picker = Some(DeferredCatalogPicker::Palette {
                replace_end: line.len(),
            });
        }
    }

    fn open_project_picker(&mut self, _replace_end: usize) {
        self.request_project_refresh();
        self.show_project_picker("");
    }

    fn show_project_picker(&mut self, query: &str) {
        // Catalog publication must not reinterpret this independent snapshot
        // as the help or palette request that previously occupied the overlay.
        self.deferred_catalog_picker = None;
        let label = if self.projects.scanning {
            "projects · scanning"
        } else if self.projects.truncated {
            "projects · partial"
        } else {
            "projects"
        };
        self.help_active = false;
        self.help_detail_scroll = 0;
        let items = self.project_items(0);
        let visible = self
            .picker
            .open_bottom_anchored_with_query(items, label, query);
        self.completion.show_picker_results(visible, label);
        self.project_picker_active = true;
    }

    fn project_items(&self, replace_end: usize) -> Vec<completion::CompletionItem> {
        self.projects
            .projects
            .iter()
            .enumerate()
            .map(|(index, project)| {
                let parent = project.path.parent().map_or_else(
                    || "filesystem root".to_owned(),
                    |parent| safe_project_text(&parent.to_string_lossy(), 2 * 1_024),
                );
                let mut detail = safe_project_text(&project.path.to_string_lossy(), 4 * 1_024);
                if let Some(status) = self.projects.status.as_deref() {
                    detail.push_str("\n\n");
                    detail.push_str(&safe_project_text(status, PROJECT_STATUS_BYTES_MAX));
                }
                completion::CompletionItem {
                    // This bounded ordinal is interpreted only while the
                    // corresponding immutable snapshot remains installed.
                    value: index.to_string(),
                    display: safe_project_text(&project.name, 2 * 1_024),
                    summary: parent,
                    detail,
                    replace_start: 0,
                    replace_end,
                    match_indices: Vec::new(),
                    kind: completion::CompletionKind::Directory,
                    source: "projects",
                    trust: "local-index",
                }
            })
            .collect()
    }

    fn project_path_for_item(
        &self,
        item: &completion::CompletionItem,
    ) -> Result<PathBuf, ShellError> {
        if item.source != "projects" {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "project picker accepted a candidate from another source",
            )
            .with_help("Close and reopen the project picker before selecting a repository"));
        }
        let index = item.value.parse::<usize>().ok();
        index
            .and_then(|index| self.projects.projects.get(index))
            .map(|project| project.path.clone())
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::Validation,
                    "project picker accepted a stale repository identity",
                )
                .with_help("Close and reopen the project picker to use the latest index")
            })
    }

    fn project_change_signal(
        &self,
        item: &completion::CompletionItem,
        buffer: String,
        cursor: usize,
    ) -> Result<InteractiveSignal, ShellError> {
        let path = self.project_path_for_item(item)?;
        Ok(InteractiveSignal::ChangeDirectory {
            path,
            buffer,
            cursor,
        })
    }

    fn handle_environment_key(
        &mut self,
        key: crossterm::event::KeyEvent,
        editor: &mut EditorState,
        mode: Mode,
    ) -> Result<(), ShellError> {
        match self.environment.handle_key(key)? {
            EnvironmentExplorerAction::Repaint => {}
            EnvironmentExplorerAction::Close => self.environment.close(),
            EnvironmentExplorerAction::Insert(text) => {
                self.environment.close();
                editor.insert_paste(&text);
                self.refresh_completion_after_edit(editor, mode)?;
            }
            EnvironmentExplorerAction::Copy(text) => {
                if let Err(error) = self.terminal.copy_to_clipboard(&text) {
                    self.environment
                        .set_notice(format!("copy failed: {}", error.message));
                } else {
                    self.environment.set_notice(format!(
                        "copied {} bytes via terminal clipboard",
                        text.len()
                    ));
                }
            }
        }
        Ok(())
    }

    fn open_context_help(&mut self, line: &str, cursor: usize) {
        self.deferred_catalog_picker = None;
        if line.len() > editor::MAX_EDITOR_BUFFER_BYTES {
            self.dismiss_picker();
            return;
        }
        // Builtin help is available at first paint without discovery I/O. Keep
        // one admitted context so imported contracts can upgrade it later; all
        // normal picker dismissal/replacement paths invalidate this intent.
        let fallback;
        let catalog = match self.catalog.as_deref() {
            Some(catalog) => catalog,
            None => {
                fallback = Catalog::builtin();
                &fallback
            }
        };
        let query = contextual_help_query(catalog, line, cursor);
        let items = overlay::items(
            editor::PickerKind::Palette,
            catalog,
            &self.history,
            line,
            cursor,
        );
        let visible = self
            .picker
            .open_with_query(items, "catalog help", false, &query);
        self.project_picker_active = false;
        self.completion.show_picker_results(visible, "catalog help");
        self.help_active = true;
        self.help_detail_scroll = 0;
        if self.catalog.is_none() {
            self.deferred_catalog_picker = Some(DeferredCatalogPicker::Help {
                line: line.to_owned(),
                cursor,
                initial_query: query,
            });
        }
    }

    fn open_picker_items(
        &mut self,
        items: Vec<completion::CompletionItem>,
        label: &'static str,
        expanded: bool,
    ) {
        self.deferred_catalog_picker = None;
        self.project_picker_active = false;
        self.help_active = false;
        self.help_detail_scroll = 0;
        let visible = self.picker.open(items, label, expanded);
        self.completion.show_picker_results(visible, label);
    }

    fn open_bottom_anchored_picker(
        &mut self,
        items: Vec<completion::CompletionItem>,
        label: &'static str,
    ) {
        self.deferred_catalog_picker = None;
        self.project_picker_active = false;
        self.help_active = false;
        self.help_detail_scroll = 0;
        let visible = self.picker.open_bottom_anchored(items, label);
        self.completion.show_picker_results(visible, label);
    }

    fn update_picker_query(
        &mut self,
        update: impl FnOnce(&mut PickerOverlay) -> Option<Vec<completion::CompletionItem>>,
    ) {
        if let Some(items) = update(&mut self.picker) {
            self.help_detail_scroll = 0;
            let label = self.picker.label();
            self.completion.show_picker_results(items, label);
        }
    }

    fn dismiss_picker(&mut self) {
        self.deferred_catalog_picker = None;
        self.picker.dismiss();
        self.completion.dismiss();
        self.project_picker_active = false;
        self.help_active = false;
        self.help_detail_scroll = 0;
    }

    /// Compact an oversized on-disk history file to the bounded in-memory tail.
    ///
    /// Files at or below the encoded limit are left untouched. Existing regular
    /// files are replaced through a bounded, identity-validated transaction;
    /// links and special files fail closed. Errors preserve the originating
    /// failure and may be I/O, validation, invalid-argument, or resource errors.
    pub fn sync_history(&mut self) -> Result<(), ShellError> {
        if history_file_needs_compaction(&self.history_path)? {
            self.compact_history()?;
        }
        Ok(())
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "the encoded record size is checked against the configured history byte limit before writing"
    )]
    fn append_history(&mut self, value: &str) -> Result<(), ShellError> {
        if self
            .history
            .last()
            .is_some_and(|last| last.command_line == value)
        {
            return Ok(());
        }
        if let Some(parent) = self.history_path.parent() {
            fs::create_dir_all(parent).map_err(history_error(&self.history_path))?;
        }
        let mut staged_history = self
            .history
            .iter()
            .map(|entry| entry.command_line.clone())
            .collect::<Vec<_>>();
        staged_history.push(value.to_owned());
        trim_history(&mut staged_history);
        let encoded = format!("{}\n", encode_history_entry(value));
        let existing = read_history_file_for_update(&self.history_path)?;
        let replacement = match existing.as_deref() {
            Some(bytes) if bytes.len().saturating_add(encoded.len()) <= MAX_HISTORY_FILE_BYTES => {
                let mut replacement = Vec::with_capacity(bytes.len() + encoded.len());
                replacement.extend_from_slice(bytes);
                replacement.extend_from_slice(encoded.as_bytes());
                replacement
            }
            Some(_) => encode_history(&staged_history),
            None => encoded.into_bytes(),
        };
        replace_or_create_history(&self.history_path, existing.as_deref(), &replacement)?;
        self.history = staged_history
            .into_iter()
            .map(|command_line| InteractiveHistoryEntry {
                command_line,
                directory: None,
                status: None,
                rank_bias: 0,
            })
            .collect();
        Ok(())
    }

    fn compact_history(&self) -> Result<(), ShellError> {
        let existing = read_history_file_for_update(&self.history_path)?;
        let Some(existing) = existing else {
            return Ok(());
        };
        replace_or_create_history(
            &self.history_path,
            Some(&existing),
            &encode_history(
                &self
                    .history
                    .iter()
                    .map(|entry| entry.command_line.clone())
                    .collect::<Vec<_>>(),
            ),
        )
    }

    fn record_draw(&mut self, elapsed: Duration) {
        if self.draw_times.len() == TIMING_WINDOW {
            self.draw_times.pop_front();
        }
        self.draw_times.push_back(elapsed);
    }

    fn timing_text(&self) -> Option<String> {
        if !self.show_timings {
            return None;
        }
        let draw = timing_p95(&self.draw_times)
            .map(|sample| format!("draw {:.2}ms", sample.as_secs_f64() * 1_000.0));
        let highlight = self
            .input_analysis
            .p95()
            .map(|sample| format!("highlight {:.2}ms", sample.as_secs_f64() * 1_000.0));
        let values = [draw, highlight].into_iter().flatten().collect::<Vec<_>>();
        (!values.is_empty()).then(|| format!("p95 {}", values.join(" · ")))
    }
}

#[cfg(debug_assertions)]
fn catalog_admission_published_test_hook() -> Result<(), ShellError> {
    let Some(gate) = env::var_os("QUIRL_TEST_CATALOG_GATE") else {
        return Ok(());
    };
    let published = PathBuf::from(format!("{}.published", PathBuf::from(gate).display()));
    fs::write(&published, b"catalog published").map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "could not publish catalog admission test marker",
        )
        .with_context(error.to_string())
        .with_help("Use a writable QUIRL_TEST_CATALOG_GATE path")
    })
}

impl Drop for RichSurface {
    fn drop(&mut self) {
        // Failure model: a read/history/completion error can unwind while raw
        // mode is active, and completion workers may still be inside plugin
        // code. Restore terminal ownership explicitly before Rust begins
        // dropping fields in declaration order.
        if self.terminal.active {
            self.terminal.reset_best_effort();
        }
        // Discovery is bounded by the owning CLI loader. Join only after raw
        // mode is gone so even a slow cleanup cannot strand terminal state.
        if let Some(mut admission) = self.catalog_admission.take() {
            let _ = Self::join_catalog_admission(&mut admission);
        }
    }
}

/// Frame length of one spinner glyph, matching the observer's tick cadence
/// so every delivered tick advances the spinner by exactly one frame.
const SPINNER_FRAME_MS: u128 = 70;
const SPINNER_FRAMES_UNICODE: [char; 12] = [
    '🕛', '🕐', '🕑', '🕒', '🕓', '🕔', '🕕', '🕖', '🕗', '🕘', '🕙', '🕚',
];
const SPINNER_FRAMES_PLAIN: [char; 4] = ['|', '/', '-', '\\'];

/// Build the running-command status text: a spinner glyph that advances
/// with `elapsed`, plus a compact elapsed-time readout.
///
/// This is Quirl's own liveness indicator, distinct from a child process's
/// own carriage-return progress output (which the transcript already
/// replays live): it is what tells the user Quirl is still waiting on a
/// command that has produced no output of its own at all.
fn running_notice(elapsed: Duration, symbols: SurfaceSymbols) -> String {
    let glyph = spinner_glyph(elapsed, symbols);
    format!(
        "{glyph} running {} · output streams into this viewport",
        format_elapsed(elapsed)
    )
}

/// Select the spinner glyph for `elapsed`, advancing one frame every
/// [`SPINNER_FRAME_MS`] so every delivered liveness tick animates it.
///
/// Shared by the status-bar running notice and the prompt row's busy
/// indicator so both spin in lockstep.
#[allow(
    clippy::indexing_slicing,
    reason = "spinner_frame always reduces the index modulo the non-empty compile-time glyph array"
)]
fn spinner_glyph(elapsed: Duration, symbols: SurfaceSymbols) -> char {
    match symbols {
        SurfaceSymbols::Plain => {
            let frame = spinner_frame(elapsed, SPINNER_FRAMES_PLAIN.len());
            SPINNER_FRAMES_PLAIN[frame]
        }
        SurfaceSymbols::Unicode | SurfaceSymbols::NerdFont => {
            let frame = spinner_frame(elapsed, SPINNER_FRAMES_UNICODE.len());
            SPINNER_FRAMES_UNICODE[frame]
        }
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "frame counts are tiny compile-time constants and elapsed milliseconds are reduced before conversion"
)]
fn spinner_frame(elapsed: Duration, frame_count: usize) -> usize {
    let frame_count = u128::try_from(frame_count).unwrap_or(u128::MAX);
    let frame = (elapsed.as_millis() / SPINNER_FRAME_MS) % frame_count;
    usize::try_from(frame).unwrap_or(0)
}

/// Render `elapsed` as a compact `12.3s` or `4m05s` readout.
fn format_elapsed(elapsed: Duration) -> String {
    let total_seconds = elapsed.as_secs();
    if total_seconds < 60 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else {
        format!("{}m{:02}s", total_seconds / 60, total_seconds % 60)
    }
}

fn truncate_utf8(value: &str, byte_limit: usize) -> String {
    if value.len() <= byte_limit {
        return value.to_owned();
    }
    let mut end = byte_limit;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut truncated = value.get(..end).unwrap_or_default().to_owned();
    truncated.push('…');
    truncated
}

#[derive(Default)]
struct SurfaceTerminal {
    terminal: Option<Terminal<CrosstermBackend<io::Stderr>>>,
    last_size: Option<(u16, u16)>,
    alternate_screen: bool,
    input_active: bool,
    active: bool,
}

impl SurfaceTerminal {
    fn enter(&mut self) -> Result<(), ShellError> {
        if self.active {
            if !self.input_active {
                self.resume_input()?;
            }
            return Ok(());
        }
        let size =
            crate::terminal_size().map_err(terminal_error("measure the interactive terminal"))?;
        validate_rich_terminal_size(size)?;
        terminal::enable_raw_mode().map_err(terminal_error("enable terminal raw mode"))?;
        self.active = true;
        self.input_active = true;
        if let Err(error) = execute!(io::stderr(), EnterAlternateScreen) {
            self.reset_best_effort();
            return Err(terminal_error("enter the alternate terminal screen")(error));
        }
        self.alternate_screen = true;
        if let Err(error) = execute!(
            io::stderr(),
            EnableBracketedPaste,
            EnableMouseCapture,
            SetCursorStyle::SteadyBar
        ) {
            self.reset_best_effort();
            return Err(terminal_error("enable terminal input features")(error));
        }
        let backend = CrosstermBackend::new(io::stderr());
        let terminal_result = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, size.0, size.1)),
            },
        );
        let mut terminal = match terminal_result {
            Ok(terminal) => terminal,
            Err(error) => {
                self.reset_best_effort();
                return Err(terminal_error("create the full-screen terminal viewport")(
                    error,
                ));
            }
        };
        if let Err(error) = terminal.hide_cursor() {
            self.reset_best_effort();
            return Err(terminal_error("hide the software cursor")(error));
        }
        self.terminal = Some(terminal);
        self.last_size = Some(size);
        Ok(())
    }

    fn resume_input(&mut self) -> Result<(), ShellError> {
        debug_assert!(self.active, "only an owned rich terminal can resume input");
        debug_assert!(
            self.alternate_screen,
            "rich input requires the alternate screen"
        );
        terminal::enable_raw_mode().map_err(terminal_error("enable terminal raw mode"))?;
        // Cooked execution permits the kernel to echo type-ahead directly into
        // our viewport, outside ratatui's cached frame. Once raw mode owns input
        // again, erase that external output and invalidate the cache before
        // announcing readiness. This costs one bounded viewport repaint per
        // execution handoff; ordinary keystrokes still use incremental diffs.
        if let Err(error) = self.force_repaint() {
            self.reset_best_effort();
            return Err(error);
        }
        if let Err(error) = execute!(
            io::stderr(),
            EnableBracketedPaste,
            EnableMouseCapture,
            SetCursorStyle::SteadyBar
        ) {
            self.reset_best_effort();
            return Err(terminal_error("restore terminal input features")(error));
        }
        self.input_active = true;
        Ok(())
    }

    fn pause_for_execution(&mut self) -> Result<(), ShellError> {
        debug_assert!(
            self.active,
            "accepted input requires an owned rich terminal"
        );
        let mut failure = None;
        if let Err(error) = execute!(
            io::stderr(),
            Show,
            DisableBracketedPaste,
            DisableMouseCapture,
            SetCursorStyle::DefaultUserShape
        ) {
            retain_error(
                &mut failure,
                terminal_error("pause terminal input features")(error),
            );
        }
        if let Err(error) = terminal::disable_raw_mode() {
            retain_error(
                &mut failure,
                terminal_error("restore cooked terminal mode for command execution")(error),
            );
        }
        if failure.is_some() {
            self.reset_best_effort();
            return failure.map_or(Ok(()), Err);
        }
        self.input_active = false;
        Ok(())
    }

    /// Wipe the real screen and force ratatui to repaint every cell on its
    /// next draw, without `Terminal::clear`'s blocking cursor-position query.
    ///
    /// `Terminal::clear` snapshots the cursor position first by writing an
    /// `ESC[6n` device-status-report and synchronously reading the
    /// terminal's reply off stdin. That read races the real reply against
    /// whatever the terminal, a just-exited child, or the user's very next
    /// keystroke writes next, and can stall the whole session if the
    /// terminal is slow, busy, or never answers — exactly the kind of
    /// terminal-corrupting hang this call exists to recover from.
    /// `Terminal::resize` clears the same fixed viewport using only the
    /// already-known terminal size and local cursor-set/erase commands, with
    /// no read from the terminal at all, so use it here when restoring the editor after foreground execution.
    fn force_repaint(&mut self) -> Result<(), ShellError> {
        let size =
            crate::terminal_size().map_err(terminal_error("measure the terminal for repaint"))?;
        validate_rich_terminal_size(size)?;
        if let Some(terminal) = self.terminal.as_mut() {
            terminal
                .resize(Rect::new(0, 0, size.0, size.1))
                .map_err(terminal_error("repaint the terminal"))?;
        }
        Ok(())
    }

    fn draw(
        &mut self,
        model: &FrameModel<'_>,
        explorer: Option<&DirectoryExplorer>,
        mode: Mode,
        symbols: SurfaceSymbols,
        selection: Option<ScreenSelection>,
        selection_style: Style,
    ) -> Result<VisibleScreen, ShellError> {
        let size = match crate::terminal_size() {
            Ok(size) => size,
            Err(error) => {
                self.reset_best_effort();
                return Err(terminal_error("measure the resized interactive terminal")(
                    error,
                ));
            }
        };
        if let Err(error) = validate_rich_terminal_size(size) {
            self.reset_best_effort();
            return Err(error);
        }
        let terminal = self.terminal.as_mut().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "the full-screen terminal is unavailable")
                .with_help("Retry with ui.surface = \"simple\"")
        })?;
        let area = Rect::new(0, 0, size.0, size.1);
        match resize_fixed_terminal(terminal, self.last_size, area) {
            Ok(true) => self.last_size = Some(size),
            Ok(false) => {}
            Err(error) => {
                let error = terminal_error("resize the interactive frame")(error);
                self.reset_best_effort();
                return Err(error);
            }
        }
        let mut visible_screen = VisibleScreen::default();
        let result = terminal
            .draw(|frame| {
                model.render(frame);
                if let Some(explorer) = explorer {
                    explorer.render(frame, frame.area(), model.theme, mode, symbols);
                }
                let buffer = frame.buffer_mut();
                visible_screen =
                    VisibleScreen::capture(buffer, selection, TRANSCRIPT_COPY_BYTES_MAX);
                style_selection(buffer, selection, selection_style);
            })
            .map(|_| ());
        if let Err(error) = result {
            self.reset_best_effort();
            return Err(terminal_error("draw the interactive frame")(error));
        }
        Ok(visible_screen)
    }

    fn copy_to_clipboard(&mut self, text: &str) -> Result<(), ShellError> {
        debug_assert!(text.len() <= TRANSCRIPT_COPY_BYTES_MAX);
        let payload = encode_base64(text.as_bytes());
        execute!(io::stderr(), Print(format!("\u{1b}]52;c;{payload}\u{7}")))
            .map_err(terminal_error("copy the output selection through OSC 52"))
    }

    fn release(&mut self) -> Result<(), ShellError> {
        let mut failure = None;
        if let Some(mut terminal) = self.terminal.take()
            && let Err(error) = terminal.show_cursor()
        {
            retain_error(
                &mut failure,
                terminal_error("restore the terminal cursor")(error),
            );
        }
        if let Err(error) = execute!(
            io::stderr(),
            Show,
            DisableBracketedPaste,
            DisableMouseCapture,
            SetCursorStyle::DefaultUserShape
        ) {
            retain_error(
                &mut failure,
                terminal_error("restore terminal input features")(error),
            );
        }
        if self.alternate_screen {
            match execute!(io::stderr(), LeaveAlternateScreen) {
                Ok(()) => {
                    self.alternate_screen = false;
                    // The primary screen still holds whatever was on it before
                    // Quirl entered the alternate screen (its own startup
                    // banner, most often), and LeaveAlternateScreen restores
                    // that content verbatim. Clear it so exiting or
                    // suspending Quirl hands back a clean terminal instead of
                    // resurrecting stale pre-launch output.
                    let _ = execute!(io::stderr(), Clear(ClearType::All), MoveTo(0, 0));
                }
                Err(error) => retain_error(
                    &mut failure,
                    terminal_error("leave the alternate terminal screen")(error),
                ),
            }
        }
        if let Err(error) = terminal::disable_raw_mode() {
            retain_error(
                &mut failure,
                terminal_error("restore cooked terminal mode")(error),
            );
        }
        self.input_active = false;
        self.finish_release(failure)
    }

    fn finish_release(&mut self, failure: Option<ShellError>) -> Result<(), ShellError> {
        // Keep the guard armed after any cleanup failure. The caller may
        // unwind immediately, and Drop must make one more best-effort attempt
        // to restore cooked mode, cursor visibility, and input features.
        self.active = failure.is_some();
        failure.map_or(Ok(()), Err)
    }

    fn reset_best_effort(&mut self) {
        if let Some(mut terminal) = self.terminal.take() {
            // Not `terminal.clear()`: it snapshots the cursor position with
            // a blocking `ESC[6n` read off stdin first, and this is the
            // last-resort path Drop and fault handlers call when the
            // terminal may already be unresponsive — the one place a hang
            // here would strand the session with no way back to a prompt.
            // `resize` clears the same viewport through already-known size
            // and local cursor-set/erase calls only, with no read at all.
            if let Ok(size) = crate::terminal_size() {
                let _ = terminal.resize(Rect::new(0, 0, size.0, size.1));
            }
            let _ = terminal.show_cursor();
        }
        let _ = execute!(
            io::stderr(),
            Show,
            DisableBracketedPaste,
            DisableMouseCapture,
            SetCursorStyle::DefaultUserShape
        );
        if self.alternate_screen {
            let _ = execute!(io::stderr(), LeaveAlternateScreen);
        }
        let _ = terminal::disable_raw_mode();
        self.alternate_screen = false;
        self.last_size = None;
        self.input_active = false;
        self.active = false;
    }
}

impl Drop for SurfaceTerminal {
    fn drop(&mut self) {
        if self.active {
            self.reset_best_effort();
        }
    }
}

fn resize_fixed_terminal<B: Backend>(
    terminal: &mut Terminal<B>,
    previous_size: Option<(u16, u16)>,
    area: Rect,
) -> Result<bool, B::Error> {
    let size = (area.width, area.height);
    if previous_size == Some(size) {
        return Ok(false);
    }
    // Retain Ratatui's physical-screen history. Its resize operation clears the
    // viewport and resets the back buffer so newly exposed cells are repainted.
    terminal.resize(area)?;
    Ok(true)
}

fn retain_error(slot: &mut Option<ShellError>, error: ShellError) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "indices are masked to six bits for the fixed 64-byte table and chunk access follows explicit length checks"
)]
fn encode_base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let output_len = bytes.len().saturating_add(2) / 3 * 4;
    let mut encoded = String::with_capacity(output_len);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(char::from(ALPHABET[usize::from(first >> 2)]));
        encoded.push(char::from(
            ALPHABET[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        encoded.push(if chunk.len() >= 2 {
            char::from(ALPHABET[usize::from(((second & 0x0f) << 2) | (third >> 6))])
        } else {
            '='
        });
        encoded.push(if chunk.len() == 3 {
            char::from(ALPHABET[usize::from(third & 0x3f)])
        } else {
            '='
        });
    }
    encoded
}

fn validate_rich_terminal_size((columns, rows): (u16, u16)) -> Result<(), ShellError> {
    if columns > 0
        && rows > 0
        && columns <= RICH_TERMINAL_COLUMNS_MAX
        && rows <= RICH_TERMINAL_ROWS_MAX
    {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        "interactive terminal dimensions exceed the rich-surface bounds",
    )
    .with_context(format!(
        "limit {RICH_TERMINAL_COLUMNS_MAX} columns by {RICH_TERMINAL_ROWS_MAX} rows; observed {columns} columns by {rows} rows"
    ))
    .with_help("Resize the terminal or retry with ui.surface = \"simple\""))
}

fn trim_history(history: &mut Vec<String>) {
    let mut retained_bytes = 0_usize;
    let mut keep_from = history.len();
    for (index, entry) in history.iter().enumerate().rev() {
        let next_bytes = retained_bytes.saturating_add(entry.len());
        if history.len().saturating_sub(index) > 50_000
            || entry.len() > MAX_HISTORY_ENTRY_BYTES
            || next_bytes > MAX_HISTORY_RETAINED_BYTES
        {
            break;
        }
        retained_bytes = next_bytes;
        keep_from = index;
    }
    if keep_from > 0 {
        history.drain(..keep_from);
    }
}

fn history_file_needs_compaction(path: &Path) -> Result<bool, ShellError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_history_file_type(path, &metadata)?;
            Ok(metadata.len() > u64::try_from(MAX_HISTORY_FILE_BYTES).unwrap_or(u64::MAX))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(history_error(path)(error)),
    }
}

fn read_history_file_for_update(path: &Path) -> Result<Option<Vec<u8>>, ShellError> {
    read_history_file_for_update_with_hook(path, || {})
}

fn read_history_file_for_update_with_hook(
    path: &Path,
    after_metadata: impl FnOnce(),
) -> Result<Option<Vec<u8>>, ShellError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(history_error(path)(error)),
    };
    validate_history_file_type(path, &metadata)?;
    let expected_identity = history_file_identity(&metadata);
    after_metadata();
    let mut file = open_history_file_no_follow(path).map_err(history_error(path))?;
    let opened_metadata = file.metadata().map_err(history_error(path))?;
    validate_history_file_type(path, &opened_metadata)?;
    let current_metadata = fs::symlink_metadata(path).map_err(history_error(path))?;
    validate_history_file_type(path, &current_metadata)?;
    if history_file_identity(&opened_metadata) != expected_identity
        || history_file_identity(&current_metadata) != expected_identity
    {
        return Err(concurrent_history_change_error(path));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(HISTORY_REPLACEMENT_BYTES_MAX)
            .min(HISTORY_REPLACEMENT_BYTES_MAX),
    );
    Read::by_ref(&mut file)
        .take(u64::try_from(HISTORY_REPLACEMENT_BYTES_MAX.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(history_error(path))?;
    if bytes.len() > HISTORY_REPLACEMENT_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "rich history file exceeds its synchronization limit",
        )
        .with_context(format!(
            "limit {HISTORY_REPLACEMENT_BYTES_MAX} bytes; observed at least {} bytes",
            bytes.len()
        ))
        .with_help("Move the oversized history file aside before restarting the rich shell"));
    }
    Ok(Some(bytes))
}

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
// These BSD-family platforms define `O_NOFOLLOW` as 0x100. Keeping the
// platform value local avoids adding a dependency for one guarded open.
const HISTORY_OPEN_NOFOLLOW: i32 = 0x100;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "solaris",
    target_os = "illumos"
))]
// Linux-family and Solaris-family platforms define `O_NOFOLLOW` as 0x20000.
const HISTORY_OPEN_NOFOLLOW: i32 = 0x20_000;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "linux",
    target_os = "android",
    target_os = "solaris",
    target_os = "illumos"
))]
fn open_history_file_no_follow(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(HISTORY_OPEN_NOFOLLOW)
        .open(path)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly",
    target_os = "linux",
    target_os = "android",
    target_os = "solaris",
    target_os = "illumos"
)))]
fn open_history_file_no_follow(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

#[cfg(unix)]
fn history_file_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn history_file_identity(metadata: &fs::Metadata) -> (u64, Option<std::time::SystemTime>, bool) {
    (
        metadata.len(),
        metadata.modified().ok(),
        metadata.permissions().readonly(),
    )
}

fn validate_history_file_type(path: &Path, metadata: &fs::Metadata) -> Result<(), ShellError> {
    if metadata.file_type().is_file() {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::InvalidArgument,
        format!("history path {} is not a regular file", path.display()),
    )
    .with_help("Replace the history link or special file with a private regular file"))
}

fn concurrent_history_change_error(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "rich history file changed during synchronization",
    )
    .with_context(format!("history path: {}", path.display()))
    .with_help("Retry after stopping concurrent history-file replacement")
}

fn encode_history(history: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in history {
        let encoded = encode_history_entry(entry);
        if encoded.len() > MAX_HISTORY_ENCODED_ENTRY_BYTES {
            continue;
        }
        bytes.extend_from_slice(encoded.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

fn replace_or_create_history(
    path: &Path,
    existing: Option<&[u8]>,
    replacement: &[u8],
) -> Result<(), ShellError> {
    if replacement.len() > MAX_HISTORY_FILE_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "rich history replacement exceeds its file limit",
        )
        .with_context(format!(
            "limit {MAX_HISTORY_FILE_BYTES} bytes; observed {} bytes",
            replacement.len()
        ))
        .with_help("Remove oversized history entries before retrying synchronization"));
    }
    if let Some(existing) = existing {
        return replace_file_atomically(
            path,
            existing,
            replacement,
            AtomicReplaceOptions {
                bytes_max: HISTORY_REPLACEMENT_BYTES_MAX,
            },
        )
        .map_err(|error| {
            error.with_context(format!(
                "while synchronizing rich history at {}",
                path.display()
            ))
        });
    }

    let mut created = false;
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(history_error(path))?;
        created = true;
        file.write_all(replacement).map_err(history_error(path))?;
        file.flush().map_err(history_error(path))?;
        file.sync_all().map_err(history_error(path))?;
        let opened_metadata = file.metadata().map_err(history_error(path))?;
        let current_metadata = fs::symlink_metadata(path).map_err(history_error(path))?;
        validate_history_file_type(path, &current_metadata)?;
        if history_file_identity(&opened_metadata) != history_file_identity(&current_metadata) {
            return Err(concurrent_history_change_error(path));
        }
        sync_history_parent(path)
    })();
    if let Err(mut error) = result {
        if created {
            error = error.with_context(format!(
                "a newly created partial history file may remain at {}; it was preserved because path-based cleanup cannot prove stable file identity",
                path.display()
            ));
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn sync_history_parent(path: &Path) -> Result<(), ShellError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(history_error(parent))
}

#[cfg(not(unix))]
fn sync_history_parent(_path: &Path) -> Result<(), ShellError> {
    Ok(())
}

fn encode_history_entry(value: &str) -> String {
    value.replace('\n', HISTORY_NEWLINE_ESCAPE)
}

fn history_error(path: &Path) -> impl Fn(io::Error) -> ShellError + '_ {
    move |error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not update history at {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Set QUIRL_HISTORY to a writable file path")
    }
}

fn terminal_error(action: &'static str) -> impl Fn(io::Error) -> ShellError {
    move |error| {
        if event::is_input_limit_error(&error) {
            return ShellError::new(ErrorCode::ResourceLimit, "terminal input exceeded its resource limit")
                .with_context(error.to_string())
                .with_help("Restart the session and paste at most 64 KiB at a time; ensure the terminal finishes bracketed paste sequences");
        }
        ShellError::new(ErrorCode::Io, format!("could not {action}"))
            .with_context(error.to_string())
            .with_help(
                "Retry with ui.surface = \"simple\" if the terminal lacks full-screen UI support",
            )
    }
}

fn input_is_incomplete(buffer: &str, mode: Mode) -> bool {
    if mode != Mode::Command {
        return false;
    }
    parse_command_list(buffer).is_err_and(|error| {
        error.message.starts_with("unclosed ")
            || error.message.contains("ends with an escape")
            || (error.message.contains("expected a command")
                && buffer.trim_end().ends_with(['|', '&']))
    })
}

fn transcript_visible_rows_for_terminal(rows: u16, follows_tail: bool) -> usize {
    // At the tail, context and the live input consume two additional rows
    // above the fixed status. Once scrolled, the transcript owns that space.
    let reserved_rows = if follows_tail { 3 } else { 1 };
    usize::from(rows.saturating_sub(reserved_rows).max(1))
}

#[allow(
    clippy::string_slice,
    reason = "the editor maintains cursor positions as UTF-8 boundaries"
)]
fn current_token_len(buffer: &str, cursor: usize) -> usize {
    buffer[..cursor.min(buffer.len())]
        .rsplit_once(char::is_whitespace)
        .map_or(
            buffer[..cursor.min(buffer.len())].chars().count(),
            |(_, token)| token.chars().count(),
        )
}

#[allow(
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "the editor maintains cursor positions as UTF-8 boundaries and token offsets are derived from char_indices"
)]
fn should_open_automatic_completion(
    catalog: &Catalog,
    buffer: &str,
    cursor: usize,
    mode: Mode,
    configured_auto: bool,
    configured_min_chars: usize,
) -> bool {
    let cursor = cursor.min(buffer.len());
    let before = &buffer[..cursor];
    let token_len = current_token_len(buffer, cursor);
    if mode == Mode::Natural {
        return false;
    }
    if completion::should_open_automatic_filesystem_completion(Some(catalog), buffer, cursor, mode)
    {
        return true;
    }
    if mode == Mode::Data && before.trim_end().len() == before.len() && before.trim_start() == "ls"
    {
        return true;
    }
    if configured_auto && token_len >= configured_min_chars {
        return true;
    }
    if mode != Mode::Command || before.trim().is_empty() {
        return false;
    }

    let trimmed = before.trim_start();
    let exact_command = before.trim_end().len() == before.len()
        && catalog.commands.iter().any(|command| {
            trimmed == command.path || command.aliases.iter().any(|alias| alias == trimmed)
        });
    if exact_command {
        return true;
    }

    let token_start = trimmed.rfind(char::is_whitespace).map_or(0, |index| {
        index + trimmed[index..].chars().next().map_or(0, char::len_utf8)
    });
    let token = &trimmed[token_start..];
    if !token.starts_with('-') {
        return false;
    }
    let command_text = trimmed[..token_start].trim_end();
    catalog.commands.iter().any(|command| {
        command_text == command.path || command_text.starts_with(&format!("{} ", command.path))
    })
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the sample collection is bounded and non-empty before the percentile rank calculation"
)]
fn timing_p95(samples: &VecDeque<Duration>) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    let index = (sorted.len().saturating_sub(1) * 95) / 100;
    sorted.get(index).copied()
}

// A provider may admit bounded background work on its first invocation. Never
// invoke it before a successful draw (including backend flush), or after a failed
// draw. This keeps process launch contention behind the first editable frame;
// subsequent idle-loop polling remains a nonblocking cache read.
fn draw_before_prompt_refresh<T>(
    prompt: &mut QuirlPrompt,
    draw: impl FnOnce() -> Result<T, ShellError>,
) -> Result<(T, bool), ShellError> {
    let frame = draw()?;
    Ok((frame, prompt.poll_native_context()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct PromptFlushWriter {
        flushed: Arc<std::sync::atomic::AtomicBool>,
        fail_flush: bool,
    }

    impl io::Write for PromptFlushWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                return Err(io::Error::other("injected first-frame flush failure"));
            }
            self.flushed.store(true, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn native_prompt_provider_waits_for_successful_terminal_frame_flush() {
        for fail_flush in [false, true] {
            let flushed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let calls = Arc::new(AtomicUsize::new(0));
            let observed_flush = Arc::clone(&flushed);
            let observed_calls = Arc::clone(&calls);
            let mut prompt =
                QuirlPrompt::new(Mode::Command).with_native_context_provider(move || {
                    assert!(observed_flush.load(AtomicOrdering::SeqCst));
                    observed_calls.fetch_add(1, AtomicOrdering::SeqCst);
                    None
                });
            let mut terminal = Terminal::with_options(
                CrosstermBackend::new(PromptFlushWriter {
                    flushed: Arc::clone(&flushed),
                    fail_flush,
                }),
                TerminalOptions {
                    viewport: Viewport::Fixed(Rect::new(0, 0, 12, 3)),
                },
            )
            .unwrap();
            assert!(!flushed.load(AtomicOrdering::SeqCst));
            assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
            let result = draw_before_prompt_refresh(&mut prompt, || {
                terminal
                    .draw(|frame| {
                        frame.render_widget(ratatui::widgets::Paragraph::new("> "), frame.area());
                    })
                    .map(|_| ())
                    .map_err(terminal_error("draw the test prompt"))
            });
            if fail_flush {
                assert!(
                    result
                        .unwrap_err()
                        .details
                        .context
                        .iter()
                        .any(|line| line.contains("injected first-frame flush failure"))
                );
                assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
            } else {
                assert!(!result.unwrap().1);
                assert!(flushed.load(AtomicOrdering::SeqCst));
                assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
            }
        }
    }

    struct CachedProjectProvider {
        snapshot: Option<InteractiveProjectSnapshot>,
        poll_count: Arc<AtomicUsize>,
        open_count: Arc<AtomicUsize>,
    }

    impl InteractiveProjectProvider for CachedProjectProvider {
        fn poll_cached(&mut self) -> Result<Option<InteractiveProjectSnapshot>, ShellError> {
            self.poll_count.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(self.snapshot.take())
        }

        fn picker_opened(&mut self) -> Result<(), ShellError> {
            self.open_count.fetch_add(1, AtomicOrdering::Relaxed);
            Ok(())
        }
    }

    struct ReadyIntentPlanner {
        update: Option<InteractiveIntentPlannerUpdate>,
    }

    impl InteractiveIntentPlanner for ReadyIntentPlanner {
        fn start(&mut self, _intent: &str, _catalog: Arc<Catalog>) -> Result<(), ShellError> {
            Ok(())
        }

        fn poll_cached(&mut self) -> Result<Option<InteractiveIntentPlannerUpdate>, ShellError> {
            Ok(self.update.take())
        }

        fn cancel(&mut self) {}
    }

    fn await_catalog_admission(surface: &mut RichSurface) -> Result<(), ShellError> {
        let started = Instant::now();
        let timeout = Duration::from_secs(1);
        loop {
            match surface.poll_catalog_admission() {
                Ok(true) => return Ok(()),
                Ok(false) if started.elapsed() < timeout => {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(false) => {
                    return Err(ShellError::new(
                        ErrorCode::ResourceLimit,
                        "catalog admission test exceeded its one-second bound",
                    )
                    .with_help("Inspect the blocked catalog admission worker"));
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[test]
    fn incomplete_quotes_continue_instead_of_executing() {
        assert!(input_is_incomplete("printf 'hello", Mode::Command));
        assert!(!input_is_incomplete("printf hello", Mode::Command));
    }

    #[test]
    fn unicode_spinner_uses_the_twelve_clock_seventy_millisecond_cycle() {
        let expected = [
            '🕛', '🕐', '🕑', '🕒', '🕓', '🕔', '🕕', '🕖', '🕗', '🕘', '🕙', '🕚', '🕛',
        ];
        for (index, glyph) in expected.into_iter().enumerate() {
            let elapsed = Duration::from_millis(u64::try_from(index).unwrap() * 70);
            assert_eq!(spinner_glyph(elapsed, SurfaceSymbols::Unicode), glyph);
        }
    }

    #[test]
    fn rejected_command_prefill_preserves_the_previous_bounded_value() {
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        surface.prefill_command("'quirl' 'status'").unwrap();
        assert_eq!(surface.pending_prefill.as_deref(), Some("'quirl' 'status'"));

        let oversized = "x".repeat(editor::MAX_EDITOR_BUFFER_BYTES.saturating_add(1));
        let error = surface.prefill_command(&oversized).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(surface.pending_prefill.as_deref(), Some("'quirl' 'status'"));
    }

    #[test]
    fn sequential_terminal_handoffs_preserve_order_and_reject_cumulative_overflow() {
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        surface.append_recovered_input("printf '").unwrap();
        surface.append_recovered_input("世界'\n").unwrap();
        let expected = "printf '世界'\n";
        assert_eq!(surface.pending_prefill.as_deref(), Some(expected));
        let remaining = editor::MAX_EDITOR_BUFFER_BYTES - expected.len();
        surface
            .append_recovered_input(&"x".repeat(remaining))
            .unwrap();
        assert_eq!(
            surface.pending_prefill.as_ref().unwrap().len(),
            editor::MAX_EDITOR_BUFFER_BYTES
        );
        assert_eq!(
            surface.append_recovered_input("x").unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        assert!(
            surface
                .pending_prefill
                .as_ref()
                .unwrap()
                .starts_with(expected)
        );
        assert_eq!(
            surface.pending_prefill.as_ref().unwrap().len(),
            editor::MAX_EDITOR_BUFFER_BYTES
        );
    }

    #[test]
    fn completed_intent_stays_conversational_until_the_proposal_is_accepted() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        surface.set_intent_planner(Box::new(ReadyIntentPlanner {
            update: Some(InteractiveIntentPlannerUpdate::Reply {
                command: Some("ls -a".to_owned()),
                message: "This lists hidden entries too.".to_owned(),
                model: "GPT-5.6 Luna".to_owned(),
                effort: "high".to_owned(),
                token_usage: Some(InteractiveIntentTokenUsage {
                    turn_total: 1_842,
                    session_total: 6_724,
                }),
                elapsed_ms: 1_250,
            }),
        }));
        surface.intent_planning_started = Some(Instant::now());
        let mut prompt = QuirlPrompt::with_config(Mode::Natural, &config);
        let mut editor = EditorState::new("emacs", Vec::new());
        surface.push_intent_message(IntentConversationRole::User, "list all files");

        assert!(
            surface
                .poll_intent_planner(&mut editor, &mut prompt)
                .unwrap()
        );
        assert_eq!(prompt.mode(), Mode::Natural);
        assert_eq!(editor.buffer(), "");
        assert_eq!(surface.intent_proposal.as_deref(), Some("ls -a"));
        assert_eq!(surface.transcript.line_count(), 0);
        assert_eq!(surface.busy_glyph, None);
        assert!(
            surface
                .output_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("MODEL\tGPT-5.6 Luna\thigh"))
        );
        assert!(surface.output_notice.as_deref().is_some_and(|notice| {
            notice.contains("ASSISTANT\tThis lists hidden entries too.")
                && notice.contains("COMMAND\tls -a")
                && notice.contains("TOKENS\t1842\t6724")
        }));
    }

    #[test]
    fn typing_a_follow_up_keeps_the_open_codex_conversation_visible() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        surface.intent_model = Some(("GPT-5.6 Luna".to_owned(), "high".to_owned()));
        surface.push_intent_message(IntentConversationRole::User, "list all files");
        surface.push_intent_message(
            IntentConversationRole::Assistant,
            "Which files should I include?",
        );
        surface.refresh_intent_notice(Duration::from_millis(850));
        let notice = surface.output_notice.clone();

        let mut editor = EditorState::new("emacs", Vec::new());
        assert!(editor.apply(EditAction::Insert('a')));
        surface.clear_transient_output_notice();
        surface.return_to_tail_for_input();

        assert_eq!(surface.output_notice, notice);
        assert_eq!(editor.buffer(), "a");
    }

    #[test]
    fn empty_enter_accepts_the_current_intent_proposal_for_review() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        surface.intent_proposal = Some("find . -type f | sort | head -n 1".to_owned());
        surface.output_notice = Some("MODEL\tGPT-5.6 Luna\thigh".to_owned());
        let mut prompt = QuirlPrompt::with_config(Mode::Natural, &config);
        let mut editor = EditorState::new("emacs", Vec::new());

        assert!(
            surface
                .accept_intent_proposal(&mut editor, &mut prompt)
                .unwrap()
        );
        assert_eq!(prompt.mode(), Mode::Command);
        assert_eq!(editor.buffer(), "find . -type f | sort | head -n 1");
        assert_eq!(surface.intent_proposal, None);
        assert_eq!(surface.output_notice, None);
    }

    #[test]
    fn tail_navigation_accounts_for_context_and_live_prompt_rows() {
        assert_eq!(transcript_visible_rows_for_terminal(24, true), 21);
        assert_eq!(transcript_visible_rows_for_terminal(24, false), 23);
        assert_eq!(transcript_visible_rows_for_terminal(2, true), 1);
    }

    #[test]
    fn rich_terminal_dimensions_accept_the_exact_bound_and_reject_overflow() {
        assert!(
            validate_rich_terminal_size((RICH_TERMINAL_COLUMNS_MAX, RICH_TERMINAL_ROWS_MAX))
                .is_ok()
        );
        for size in [
            (0, 24),
            (80, 0),
            (RICH_TERMINAL_COLUMNS_MAX + 1, 24),
            (80, RICH_TERMINAL_ROWS_MAX + 1),
        ] {
            let error = validate_rich_terminal_size(size).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(error.details.context[0].contains("observed"));
        }
    }

    #[test]
    fn fixed_terminal_resize_clears_cells_across_shrink_and_growth() {
        use ratatui::{backend::TestBackend, widgets::Paragraph};

        let large = Rect::new(0, 0, 8, 3);
        let small = Rect::new(0, 0, 4, 2);
        let backend = TestBackend::new(large.width, large.height);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(large),
            },
        )
        .unwrap();
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("XXXXXXXX\nXXXXXXXX"), large))
            .unwrap();

        terminal.backend_mut().resize(small.width, small.height);
        assert!(resize_fixed_terminal(&mut terminal, Some((8, 3)), small).unwrap());
        terminal.backend().assert_buffer_lines(["    ", "    "]);
        terminal
            .draw(|frame| frame.render_widget(Paragraph::new("old!"), small))
            .unwrap();

        terminal.backend_mut().resize(large.width, large.height);
        assert!(resize_fixed_terminal(&mut terminal, Some((4, 2)), large).unwrap());
        terminal
            .backend()
            .assert_buffer_lines(["        ", "        ", "        "]);
        assert!(!resize_fixed_terminal(&mut terminal, Some((8, 3)), large).unwrap());
    }

    #[test]
    fn data_ls_information_opens_automatically_without_enabling_global_auto_completion() {
        // Explicit, not the default: this test's whole point is that the
        // data-mode `ls` info panel opens on its own regardless of the
        // general auto-completion setting, so pin that setting off here
        // rather than relying on whatever QuirlConfig::default() happens
        // to be.
        let mut config = QuirlConfig::default();
        config.completion.auto = false;
        assert!(!config.completion.auto);
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("ls");

        surface
            .refresh_completion_after_edit(&editor, Mode::Data)
            .unwrap();
        assert!(surface.completion.open);
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline && surface.completion.streaming {
            surface.completion.poll(editor.buffer(), editor.cursor());
            std::thread::yield_now();
        }
        let item = surface
            .completion
            .items
            .iter()
            .find(|item| item.value == "ls")
            .unwrap();
        assert!(item.detail.contains("Capabilities:"));

        assert!(editor.apply(EditAction::Insert(' ')));
        surface
            .refresh_completion_after_edit(&editor, Mode::Data)
            .unwrap();
        assert!(!surface.completion.open);
        assert!(!surface.completion.streaming);
    }

    #[test]
    fn directory_and_explicit_path_contexts_open_automatically() {
        let catalog = Catalog::builtin();
        let auto = false;
        let min_chars = 2;

        for line in ["cd ", "cd sr", "cat ./sr"] {
            assert!(should_open_automatic_completion(
                &catalog,
                line,
                line.len(),
                Mode::Command,
                auto,
                min_chars,
            ));
        }
        assert!(!should_open_automatic_completion(
            &catalog,
            "echo ordinary",
            "echo ordinary".len(),
            Mode::Command,
            auto,
            min_chars,
        ));
        assert!(!should_open_automatic_completion(
            &catalog,
            "cd sr",
            "cd sr".len(),
            Mode::Natural,
            auto,
            min_chars,
        ));
    }

    #[test]
    fn normal_imported_flag_prefix_opens_catalog_options_without_tab() {
        let mut catalog = Catalog::builtin();
        let diagnostics = catalog.merge_report(quirl_catalog::import_fish(
            "complete -c ls -s a -l all -d 'Show all entries'",
            "ls.fish",
        ));
        assert!(diagnostics.is_empty());
        let mut surface = RichSurface::new(
            Arc::new(catalog),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("ls -");

        surface
            .refresh_completion_after_edit(&editor, Mode::Command)
            .unwrap();
        assert!(surface.completion.open);
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline && surface.completion.streaming {
            surface.completion.poll(editor.buffer(), editor.cursor());
            std::thread::yield_now();
        }
        assert!(
            surface
                .completion
                .items
                .iter()
                .any(|item| item.value == "--all" && item.kind == completion::CompletionKind::Flag)
        );
    }

    #[test]
    fn repeated_mode_transitions_redraw_without_discarding_the_edit_buffer() {
        let mut config = QuirlConfig::default();
        config.prompt.left = vec!["mode".to_owned()];
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        let mut prompt = QuirlPrompt::with_config(Mode::Command, &config);
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("printf preserved");
        editor.apply(EditAction::MoveLeft);
        let buffer = editor.buffer().to_owned();
        let cursor = editor.cursor();

        for expected_mode in [Mode::Data, Mode::Natural, Mode::Command] {
            surface.expand_completion_pending = true;
            surface.completion.open = true;
            surface.help_active = true;
            surface.deferred_catalog_picker = Some(DeferredCatalogPicker::Help {
                line: buffer.clone(),
                cursor,
                initial_query: "printf".to_owned(),
            });

            surface.toggle_grammar_mode(&mut prompt);

            assert_eq!(prompt.mode(), expected_mode);
            assert_eq!(prompt.surface_context_left(), expected_mode.to_string());
            assert_eq!(editor.buffer(), buffer);
            assert_eq!(editor.cursor(), cursor);
            assert!(!surface.expand_completion_pending);
            assert!(!surface.completion.open);
            assert!(!surface.help_active);
            assert!(surface.deferred_catalog_picker.is_none());
        }
    }

    #[test]
    fn alt_q_e_opens_the_full_screen_environment_explorer() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        surface.install_runtime_snapshot(InteractiveRuntimeSnapshot {
            generation: 1,
            environment: Some(vec![InteractiveEnvironmentSnapshot {
                name: "RUST_LOG".to_owned(),
                value: "debug".to_owned(),
            }]),
            ..InteractiveRuntimeSnapshot::default()
        });
        let mut prompt = QuirlPrompt::with_config(Mode::Command, &config);

        surface
            .handle_leader_key(KeyCode::Char('e'), &mut prompt, "git status", 3)
            .unwrap();

        assert!(surface.environment.active());
        assert!(!surface.picker.active());
        assert_eq!(prompt.mode(), Mode::Command);
    }

    #[test]
    fn alt_q_g_opens_projects_and_accepts_the_exact_path_without_losing_input() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        #[cfg(unix)]
        let exact_path = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from("/tmp/workspace").join(std::ffi::OsString::from_vec(vec![
                b'a', b'l', b'p', b'h', b'a', b'-', 0x80,
            ]))
        };
        #[cfg(not(unix))]
        let exact_path = PathBuf::from("C:/workspace/alpha repo");
        surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                generation: 7,
                projects: vec![InteractiveProjectEntry {
                    path: exact_path.clone(),
                    name: "alpha".to_owned(),
                }],
                scanning: true,
                truncated: false,
                status: Some("refreshing configured roots".to_owned()),
            })
            .unwrap();
        let mut prompt = QuirlPrompt::with_config(Mode::Command, &config);
        let unfinished = "git status --short";

        surface.open_leader(unfinished.len());
        assert!(
            surface
                .completion
                .items
                .iter()
                .any(|item| item.value == "g" && item.display.contains("Projects"))
        );
        surface
            .handle_leader_key(
                KeyCode::Char('g'),
                &mut prompt,
                unfinished,
                unfinished.len(),
            )
            .unwrap();

        assert!(surface.picker.active());
        assert!(surface.project_picker_active);
        assert!(surface.picker.bottom_anchored());
        assert_eq!(surface.completion.source_label, "projects · scanning");
        let item = surface.completion.selected_item().unwrap().clone();
        assert_eq!(item.kind, completion::CompletionKind::Directory);
        assert_eq!(item.display, "alpha");
        #[cfg(unix)]
        assert_eq!(item.summary, "/tmp/workspace");
        // The completion value is only a bounded generation-local identity;
        // accepting it returns the structured path from the snapshot.
        assert_ne!(item.value, exact_path.to_string_lossy());
        assert_eq!(
            surface
                .project_change_signal(&item, unfinished.to_owned(), 3)
                .unwrap(),
            InteractiveSignal::ChangeDirectory {
                path: exact_path,
                buffer: unfinished.to_owned(),
                cursor: 3,
            }
        );

        surface.dismiss_picker();
        assert!(!surface.project_picker_active);
        assert!(!surface.picker.active());
    }

    #[test]
    fn project_picker_searches_repository_name_and_parent_path() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                projects: vec![InteractiveProjectEntry {
                    path: PathBuf::from("/tmp/company-work/quirl"),
                    name: "quirl".to_owned(),
                }],
                ..InteractiveProjectSnapshot::default()
            })
            .unwrap();
        surface.open_project_picker(0);

        surface.update_picker_query(|picker| picker.insert_query("company"));
        assert_eq!(surface.completion.items.len(), 1);
        surface.update_picker_query(PickerOverlay::clear_query);
        surface.update_picker_query(|picker| picker.insert_query("quirl"));
        assert_eq!(surface.completion.items.len(), 1);
    }

    #[test]
    fn project_snapshots_enforce_count_and_retained_field_bounds() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        let entry = InteractiveProjectEntry {
            path: PathBuf::from("/tmp/project"),
            name: "project".to_owned(),
        };
        surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                projects: vec![entry.clone(); PROJECT_ITEMS_MAX],
                ..InteractiveProjectSnapshot::default()
            })
            .unwrap();

        let error = surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                projects: vec![entry; PROJECT_ITEMS_MAX + 1],
                ..InteractiveProjectSnapshot::default()
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);

        let error = surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                projects: vec![InteractiveProjectEntry {
                    path: PathBuf::from("/tmp/project"),
                    name: "x".repeat(PROJECT_FIELD_BYTES_MAX + 1),
                }],
                ..InteractiveProjectSnapshot::default()
            })
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn stale_project_generations_cannot_replace_newer_candidates() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                generation: 9,
                projects: vec![InteractiveProjectEntry {
                    path: PathBuf::from("/tmp/newer"),
                    name: "newer".to_owned(),
                }],
                ..InteractiveProjectSnapshot::default()
            })
            .unwrap();
        surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                generation: 8,
                projects: vec![InteractiveProjectEntry {
                    path: PathBuf::from("/tmp/stale"),
                    name: "stale".to_owned(),
                }],
                ..InteractiveProjectSnapshot::default()
            })
            .unwrap();

        surface.open_project_picker(0);
        let item = surface.completion.selected_item().unwrap();
        assert_eq!(item.display, "newer");
        assert_eq!(
            surface.project_path_for_item(item).unwrap(),
            Path::new("/tmp/newer")
        );
    }

    #[test]
    fn cached_provider_update_repaints_an_open_project_picker() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        let poll_count = Arc::new(AtomicUsize::new(0));
        let open_count = Arc::new(AtomicUsize::new(0));
        surface.set_project_provider(Box::new(CachedProjectProvider {
            snapshot: Some(InteractiveProjectSnapshot {
                generation: 1,
                projects: vec![InteractiveProjectEntry {
                    path: PathBuf::from("/tmp/background-project"),
                    name: "background-project".to_owned(),
                }],
                ..InteractiveProjectSnapshot::default()
            }),
            poll_count: Arc::clone(&poll_count),
            open_count: Arc::clone(&open_count),
        }));

        surface.open_project_picker(0);
        assert!(surface.project_picker_active);
        assert!(surface.completion.items.is_empty());
        assert!(surface.poll_project_provider());

        assert!(surface.project_picker_active);
        assert_eq!(surface.completion.items.len(), 1);
        assert_eq!(surface.completion.items[0].display, "background-project");
        assert_eq!(poll_count.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(open_count.load(AtomicOrdering::Relaxed), 1);
    }

    #[test]
    fn opening_projects_calls_only_the_nonblocking_refresh_hook() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        let poll_count = Arc::new(AtomicUsize::new(0));
        let open_count = Arc::new(AtomicUsize::new(0));
        surface.set_project_provider(Box::new(CachedProjectProvider {
            snapshot: None,
            poll_count: Arc::clone(&poll_count),
            open_count: Arc::clone(&open_count),
        }));

        surface.open_project_picker(0);

        assert_eq!(open_count.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(poll_count.load(AtomicOrdering::Relaxed), 0);
        assert!(surface.project_picker_active);
    }

    #[test]
    fn clearing_project_provider_discards_candidates_and_active_picker() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        surface
            .install_project_snapshot(InteractiveProjectSnapshot {
                generation: 4,
                projects: vec![InteractiveProjectEntry {
                    path: PathBuf::from("/tmp/project"),
                    name: "project".to_owned(),
                }],
                ..InteractiveProjectSnapshot::default()
            })
            .unwrap();
        surface.set_project_provider(Box::new(CachedProjectProvider {
            snapshot: None,
            poll_count: Arc::new(AtomicUsize::new(0)),
            open_count: Arc::new(AtomicUsize::new(0)),
        }));
        surface.open_project_picker(0);

        surface.clear_project_provider();

        assert!(surface.project_provider.is_none());
        assert_eq!(surface.projects, InteractiveProjectSnapshot::default());
        assert!(!surface.project_picker_active);
        assert!(!surface.picker.active());
        assert!(!surface.completion.open);
    }

    #[test]
    fn multiline_history_uses_the_reedline_compatible_encoding() {
        let entry = "printf 'one\ntwo'\necho done";
        let encoded = encode_history_entry(entry);
        assert!(!encoded.contains('\n'));
        assert!(encoded.contains(HISTORY_NEWLINE_ESCAPE));
        assert_eq!(
            crate::parse_history_tail(encoded.as_bytes(), crate::HISTORY_READ_LIMITS),
            vec![entry]
        );
    }

    #[test]
    fn history_reader_skips_oversized_entries_and_keeps_the_valid_tail() {
        let path = std::env::temp_dir().join(format!(
            "quirl-history-bound-{}-{}",
            std::process::id(),
            HISTORY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_HISTORY_ENCODED_ENTRY_BYTES + 1])
            .unwrap();
        file.write_all(b"\nsafe tail\n").unwrap();
        drop(file);
        assert_eq!(read_history(&path).unwrap(), vec!["safe tail"]);
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rich_history_sync_rejects_symlinks_without_touching_the_target() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "quirl-rich-history-link-{}-{}",
            std::process::id(),
            HISTORY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("target");
        let history_path = directory.join("history");
        fs::write(&target, "protected\n").unwrap();
        symlink(&target, &history_path).unwrap();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            history_path,
        )
        .unwrap();

        let error = surface.append_history("new entry").unwrap_err();

        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(fs::read_to_string(&target).unwrap(), "protected\n");
        let names = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
        fs::remove_file(directory.join("history")).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rich_history_reader_rejects_metadata_to_open_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let directory = std::env::temp_dir().join(format!(
            "quirl-rich-history-open-race-{}-{}",
            std::process::id(),
            HISTORY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("target");
        let history_path = directory.join("history");
        fs::write(&target, "foreign\n").unwrap();
        fs::write(&history_path, "original\n").unwrap();

        let error = read_history_file_for_update_with_hook(&history_path, || {
            fs::remove_file(&history_path).unwrap();
            symlink(&target, &history_path).unwrap();
        })
        .unwrap_err();

        assert!(matches!(
            error.code,
            ErrorCode::Io | ErrorCode::InvalidArgument | ErrorCode::Validation
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "foreign\n");
        fs::remove_file(&history_path).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rich_history_create_failure_preserves_the_original_error_and_foreign_entry() {
        let directory = std::env::temp_dir().join(format!(
            "quirl-rich-history-race-{}-{}",
            std::process::id(),
            HISTORY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let history_path = directory.join("history");
        fs::create_dir(&history_path).unwrap();

        let error = replace_or_create_history(&history_path, None, b"entry\n").unwrap_err();

        assert_eq!(error.code, ErrorCode::Io);
        assert!(history_path.is_dir());
        assert!(error.message.contains("history"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn in_memory_history_retention_is_byte_and_count_bounded() {
        let mut history = (0..9_000).map(|_| "x".repeat(1_024)).collect::<Vec<_>>();
        trim_history(&mut history);
        assert!(history.len() <= 50_000);
        assert!(history.iter().map(String::len).sum::<usize>() <= MAX_HISTORY_RETAINED_BYTES);
    }

    #[test]
    fn failed_explicit_release_keeps_drop_cleanup_armed() {
        let mut terminal = SurfaceTerminal {
            terminal: None,
            last_size: None,
            alternate_screen: true,
            input_active: false,
            active: true,
        };
        let failure = ShellError::new(ErrorCode::Io, "injected terminal cleanup failure")
            .with_help("Retry terminal cleanup from Drop");

        assert!(terminal.finish_release(Some(failure)).is_err());
        assert!(terminal.active);
        assert!(terminal.alternate_screen);

        terminal.alternate_screen = false;
        assert!(terminal.finish_release(None).is_ok());
        assert!(!terminal.active);
    }

    #[test]
    fn transcript_append_preserves_blank_lines_and_escapes_controls() {
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();

        surface
            .append_transcript(
                "printf test",
                b"first\n\nlast\x1b[2J\n",
                b"warning\n",
                7,
                Duration::from_millis(12),
            )
            .unwrap();

        let lines = (0..surface.transcript.line_count())
            .map(|index| surface.transcript.line(index).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines[0], "❯ printf test");
        assert_eq!(lines[1], "first");
        assert_eq!(lines[2], "");
        assert_eq!(lines[3], "last\\u{1b}[2J");
        assert_eq!(lines[4], "warning");
        assert_eq!(lines[5], "── exit 7 · 12ms ──");
    }

    #[test]
    fn silent_success_keeps_command_and_status_without_placeholder_output() {
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();

        surface
            .append_transcript("cd project", b"", b"", 0, Duration::from_millis(1))
            .unwrap();

        let lines = (0..surface.transcript.line_count())
            .map(|index| surface.transcript.line(index).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(lines, ["❯ cd project", "── exit 0 · 1ms ──"]);
    }

    #[test]
    fn editing_after_scrollback_returns_to_the_live_prompt_tail() {
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        for index in 0..30 {
            surface.append_transcript_line(&format!("output-{index}"));
        }
        assert!(surface.transcript.page_up(10));
        assert!(!surface.transcript.follows_tail());

        surface.return_to_tail_for_input();

        assert!(surface.transcript.follows_tail());
    }

    #[test]
    fn streaming_text_strips_split_ansi_and_keeps_progress_replacement() {
        let mut stream = StreamingText::default();
        assert!(stream.push(b"\x1b[0;3").is_empty());
        assert_eq!(
            stream.push(b"2mclone\x1b[0m ssh://example\n"),
            ["clone ssh://example"]
        );
        assert_eq!(stream.push(b"10%\r20%\n"), ["20%"]);
    }

    #[test]
    fn streaming_text_pending_exposes_carriage_return_updates_before_a_newline() {
        // A child reporting `\r`-driven progress (git push, curl, package
        // manager progress bars) may hold one logical line open across many
        // separate writes with no intervening `\n`. `push` alone cannot show
        // that: it only returns completed lines. `pending` is what a caller
        // uses to render the still-open line live instead of leaving the
        // viewport static until the line finally completes.
        let mut stream = StreamingText::default();
        assert!(stream.pending().is_empty());

        assert!(stream.push(b"Counting objects:  10%\r").is_empty());
        assert_eq!(stream.pending(), "Counting objects:  10%");

        // A later bare `\r` update overwrites the pending line in place
        // rather than accumulating after it, matching a real terminal.
        assert!(stream.push(b"Counting objects:  55%\r").is_empty());
        assert_eq!(stream.pending(), "Counting objects:  55%");

        // Finishing the line with a real newline clears the pending buffer
        // and surfaces it as one completed line.
        assert_eq!(
            stream.push(b"Counting objects: 100% (5/5), done.\n"),
            ["Counting objects: 100% (5/5), done."]
        );
        assert!(stream.pending().is_empty());
    }

    #[test]
    fn streaming_text_preserves_utf8_split_across_chunks() {
        let mut stream = StreamingText::default();
        let bytes = "repo → target\n".as_bytes();
        let split = bytes.iter().position(|byte| *byte == 0xe2).unwrap() + 1;
        assert!(stream.push(&bytes[..split]).is_empty());
        assert_eq!(stream.push(&bytes[split..]), ["repo → target"]);
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn mouse_drag_selects_utf8_text_then_returns_keyboard_focus_to_prompt() {
        use ratatui::buffer::Buffer;

        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 1));
        buffer.set_string(0, 0, "alpha βeta omega", Style::default());
        surface.visible_screen = VisibleScreen::capture(&buffer, None, 1_024);

        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 6,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 9,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 9,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        surface.visible_screen =
            VisibleScreen::capture(&buffer, surface.screen_selection, TRANSCRIPT_COPY_BYTES_MAX);

        assert!(!surface.output_focus);
        assert_eq!(
            surface.visible_screen.selected_text_bounded(),
            Ok(Some("βeta"))
        );
        assert_eq!(surface.mouse_drag, None);

        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });

        assert!(!surface.output_focus);
        assert!(surface.screen_selection.is_none());
    }

    #[test]
    fn mouse_drag_crosses_transcript_context_input_and_status_bar() {
        use ratatui::buffer::Buffer;

        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        surface.append_transcript_line("build output");
        surface.transcript_area = Rect::new(0, 0, 20, 1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 4));
        buffer.set_string(0, 0, "build output", Style::default());
        buffer.set_string(0, 1, "~/work   on main", Style::default());
        buffer.set_string(0, 2, "❯ git status", Style::default());
        buffer.set_string(0, 3, "NORMAL      v0.1", Style::default());
        surface.visible_screen = VisibleScreen::capture(&buffer, None, 1_024);

        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 15,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 15,
            row: 3,
            modifiers: KeyModifiers::NONE,
        });
        surface.visible_screen =
            VisibleScreen::capture(&buffer, surface.screen_selection, TRANSCRIPT_COPY_BYTES_MAX);

        assert!(!surface.output_focus);
        assert_eq!(
            surface.visible_screen.selected_text_bounded(),
            Ok(Some(
                "build output\n~/work   on main\n❯ git status\nNORMAL      v0.1"
            ))
        );
    }

    #[test]
    fn scrollbar_pointer_maps_track_endpoints_to_transcript_endpoints() {
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        for line in 0..100 {
            surface.append_transcript_line(&format!("line-{line}"));
        }
        surface.transcript_area = Rect::new(0, 0, 40, 20);

        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 39,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(surface.transcript.visible_range(20), 0..20);
        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 39,
            row: 19,
            modifiers: KeyModifiers::NONE,
        });
        surface.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 39,
            row: 19,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(surface.transcript.visible_range(20), 80..100);
        assert!(surface.transcript.follows_tail());
    }

    #[test]
    fn osc52_payload_encoder_matches_standard_base64_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64("💚".as_bytes()), "8J+Smg==");
    }

    #[test]
    fn default_adaptive_command_palette_requests_bottom_anchored_viewport() {
        let config = QuirlConfig::default();
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        assert_eq!(surface.picker_layout, PickerLayout::Adaptive);

        surface.open_picker(editor::PickerKind::Palette, "", 0, "picker");
        assert!(surface.picker.active());
        assert!(surface.picker.bottom_anchored());
        assert!(!surface.picker.expanded());
    }

    #[test]
    fn explicit_completion_up_returns_to_the_previous_choice_without_opening_history() {
        let catalog = Arc::new(Catalog::builtin());
        let mut surface = RichSurface::new(
            Arc::clone(&catalog),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        let line = "git st";
        let choices = overlay::items(editor::PickerKind::Palette, &catalog, &[], line, line.len())
            .into_iter()
            .take(2)
            .collect::<Vec<_>>();
        assert_eq!(choices.len(), 2);
        surface.completion.open_manual(choices.clone(), "catalog");
        surface.completion.next();
        assert_eq!(surface.completion.selected, 1);

        surface.handle_completion_up(Mode::Command, line, line.len());

        assert!(!surface.picker.active());
        assert_eq!(surface.completion.selected, 0);
        assert_eq!(surface.completion.items, choices);
        assert!(surface.completion.accepts_enter());
        surface.handle_completion_up(Mode::Command, line, line.len());
        assert_eq!(surface.completion.selected, 1);
    }

    #[test]
    fn automatic_completion_keeps_history_recall_until_the_user_navigates() {
        let catalog = Arc::new(Catalog::builtin());
        let mut surface = RichSurface::new(
            Arc::clone(&catalog),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        let line = "git st";
        surface.install_history_snapshot(vec![InteractiveHistoryEntry {
            command_line: "git status --short".to_owned(),
            directory: None,
            status: Some(0),
            rank_bias: 0,
        }]);
        let choices = overlay::items(editor::PickerKind::Palette, &catalog, &[], line, line.len())
            .into_iter()
            .take(2)
            .collect::<Vec<_>>();
        surface.completion.open_manual(choices.clone(), "catalog");
        surface.completion.automatic = true;
        assert!(!surface.completion.accepts_enter());

        surface.handle_completion_up(Mode::Command, line, line.len());

        assert!(surface.picker.active());
        assert_eq!(surface.picker.query(), Some(line));
        assert_eq!(
            surface.completion.selected_item().unwrap().value,
            "git status --short"
        );

        surface.dismiss_picker();
        surface.completion.open_manual(choices, "catalog");
        surface.completion.automatic = true;
        surface.completion.next();
        surface.handle_completion_up(Mode::Command, line, line.len());
        assert!(!surface.picker.active());
        assert_eq!(surface.completion.selected, 0);
        assert!(surface.completion.accepts_enter());
    }

    #[test]
    fn accepting_directories_browses_children_without_submitting_the_command() {
        let root = std::env::temp_dir().join(format!(
            "quirl-directory-accept-{}-{}",
            std::process::id(),
            HISTORY_TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("parent/child")).unwrap();
        let mut surface = cold_help_surface(Catalog::builtin());
        surface.begin_catalog_admission().unwrap();
        await_catalog_admission(&mut surface).unwrap();
        let mut editor = EditorState::new("emacs", Vec::new());
        let line = format!("cd {}/par", root.display());
        editor.replace(0, 0, &line);
        surface
            .completion
            .request_automatic(&line, line.len(), Mode::Command)
            .unwrap();
        assert!(surface.completion.accepts_enter());
        surface
            .accept_selected_completion(&mut editor, Mode::Command)
            .unwrap();
        assert!(editor.buffer().ends_with("/parent/"));
        assert!(surface.completion.accepts_enter());
        assert_eq!(
            surface.completion.selected_item().unwrap().display,
            "child/"
        );
        surface
            .accept_selected_completion(&mut editor, Mode::Command)
            .unwrap();
        assert!(editor.buffer().ends_with("/parent/child/"));
        surface.dismiss_picker();
        assert!(!surface.completion.accepts_enter());
        assert!(editor.buffer().ends_with("/parent/child/"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejected_completion_keeps_the_edit_and_resource_diagnostic() {
        let mut surface = cold_help_surface(Catalog::builtin());
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.replace(0, 0, &"x".repeat(editor::MAX_EDITOR_BUFFER_BYTES));
        let before = editor.buffer().to_owned();
        let mut item = overlay::palette_items(&Catalog::builtin(), 0).remove(0);
        item.replace_start = before.len();
        item.replace_end = before.len();
        item.value = "extra/".to_owned();
        item.kind = completion::CompletionKind::Directory;
        item.source = "filesystem";
        surface.completion.open_manual(vec![item], "filesystem");
        surface
            .accept_selected_completion(&mut editor, Mode::Command)
            .unwrap();
        assert_eq!(editor.buffer(), before);
        assert!(editor.resource_notice().is_some());
        assert!(surface.completion.open);
    }

    #[test]
    fn cold_context_help_is_immediate_and_keeps_the_latest_cursor_context() {
        let mut surface = cold_help_surface(Catalog::builtin());
        let line = "git status | quirl data pending";
        surface.open_context_help(line, "git status".len());
        assert_eq!(surface.picker.query(), Some("git status"));
        surface.open_context_help(line, line.len());
        assert!(surface.published_catalog().is_none());
        assert!(surface.help_active);
        assert_eq!(surface.picker.query(), Some("quirl data"));
        assert!(
            surface
                .completion
                .items
                .iter()
                .any(|item| item.value == "quirl data")
        );
        surface.begin_catalog_admission().unwrap();
        await_catalog_admission(&mut surface).unwrap();
        assert!(surface.help_active);
        assert_eq!(surface.picker.query(), Some("quirl data"));
        assert!(surface.deferred_catalog_picker.is_none());
    }

    #[test]
    fn cold_help_upgrades_imported_context_without_overwriting_an_edited_query() {
        let mut catalog = Catalog::builtin();
        let mut imported = catalog.commands[0].clone();
        imported.path = "fixture deploy".to_owned();
        catalog.merge([imported]);
        for edited in [false, true] {
            let mut surface = cold_help_surface(catalog.clone());
            surface.open_context_help("fixture deploy pending", 22);
            if edited {
                surface.update_picker_query(PickerOverlay::clear_query);
                surface.update_picker_query(|picker| picker.insert_query("doctor"));
            }
            surface.begin_catalog_admission().unwrap();
            await_catalog_admission(&mut surface).unwrap();
            let expected = if edited { "doctor" } else { "fixture deploy" };
            assert_eq!(surface.picker.query(), Some(expected));
            let expected_path = if edited {
                "quirl config doctor"
            } else {
                "fixture deploy"
            };
            assert!(
                surface
                    .completion
                    .items
                    .iter()
                    .any(|item| item.value == expected_path)
            );
        }
    }

    #[test]
    fn dismissed_or_replaced_cold_help_is_not_reopened_by_publication() {
        for replace in [false, true] {
            let mut surface = cold_help_surface(Catalog::builtin());
            surface.open_context_help("quirl data", 10);
            if replace {
                surface.open_picker(editor::PickerKind::History, "", 0, "history");
            } else {
                surface.dismiss_picker();
            }
            surface.begin_catalog_admission().unwrap();
            await_catalog_admission(&mut surface).unwrap();
            assert!(!surface.help_active);
            assert_eq!(surface.picker.active(), replace);
        }
    }

    #[test]
    fn history_replaces_project_selection_before_accepting_a_command() {
        let mut surface = cold_help_surface(Catalog::builtin());
        surface.install_history_snapshot(vec![InteractiveHistoryEntry {
            command_line: "printf history".to_owned(),
            directory: None,
            status: Some(0),
            rank_bias: 0,
        }]);
        surface.open_project_picker(0);
        assert!(surface.project_picker_active);

        surface.open_picker(editor::PickerKind::History, "", 0, "history");

        assert!(!surface.project_picker_active);
        assert!(surface.picker.active());
        assert_eq!(surface.picker.label(), "history");
        let item = surface.completion.selected_item().unwrap();
        assert_eq!(item.kind, completion::CompletionKind::History);
        assert_eq!(item.value, "printf history");
    }

    #[test]
    fn project_picker_replaces_deferred_help_and_palette_before_publication() {
        for help in [true, false] {
            let mut surface = cold_help_surface(Catalog::builtin());
            if help {
                surface.open_context_help("quirl data", 10);
            } else {
                surface.open_picker(editor::PickerKind::Palette, "", 0, "palette");
            }
            assert!(surface.deferred_catalog_picker.is_some());
            let path = PathBuf::from("/tmp/project-fixture");
            surface
                .install_project_snapshot(InteractiveProjectSnapshot {
                    generation: 1,
                    projects: vec![InteractiveProjectEntry {
                        path: path.clone(),
                        name: "project-fixture".to_owned(),
                    }],
                    ..InteractiveProjectSnapshot::default()
                })
                .unwrap();
            surface.open_project_picker(0);
            surface.update_picker_query(|picker| picker.insert_query("fixture"));
            assert!(surface.deferred_catalog_picker.is_none());

            surface.begin_catalog_admission().unwrap();
            await_catalog_admission(&mut surface).unwrap();

            assert!(surface.project_picker_active);
            assert!(!surface.help_active);
            assert_eq!(surface.picker.query(), Some("fixture"));
            assert_eq!(surface.picker.label(), "projects");
            let item = surface.completion.selected_item().unwrap();
            assert_eq!(surface.project_path_for_item(item).unwrap(), path);
        }
    }

    #[test]
    fn cold_help_context_copy_respects_the_editor_byte_limit() {
        let mut surface = cold_help_surface(Catalog::builtin());
        let exact = "x".repeat(editor::MAX_EDITOR_BUFFER_BYTES);
        surface.open_context_help(&exact, exact.len());
        let Some(DeferredCatalogPicker::Help {
            line,
            initial_query,
            ..
        }) = &surface.deferred_catalog_picker
        else {
            panic!("exact-size help context was not retained");
        };
        assert_eq!(line.len(), editor::MAX_EDITOR_BUFFER_BYTES);
        assert!(initial_query.len() <= crate::PICKER_QUERY_BYTES_MAX);
        let over = format!("{exact}x");
        surface.open_context_help(&over, over.len());
        assert!(surface.deferred_catalog_picker.is_none());
        assert!(!surface.picker.active());
    }

    fn cold_help_surface(catalog: Catalog) -> RichSurface {
        RichSurface::new_deferred(
            Box::new(move || Ok(Arc::new(catalog))),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap()
    }

    #[test]
    fn catalog_arrival_refreshes_current_automatic_information_without_retyping() {
        let mut config = QuirlConfig::default();
        config.completion.auto = false;
        let mut catalog = Catalog::builtin();
        catalog.merge_report(quirl_catalog::import_fish(
            "complete -c ls -s a -l all -d 'Show all entries'",
            "ls.fish",
        ));
        let mut surface = RichSurface::new_deferred(
            Box::new(move || Ok(Arc::new(catalog))),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("ls");
        surface
            .refresh_completion_after_edit(&editor, Mode::Command)
            .unwrap();
        assert!(!surface.completion.open);
        surface.begin_catalog_admission().unwrap();
        await_catalog_admission(&mut surface).unwrap();
        surface
            .refresh_completion_after_catalog(&editor, Mode::Command)
            .unwrap();
        assert!(surface.completion.open);
        assert!(surface.completion.automatic);

        surface.completion.cancel_for_edit();
        editor.clear();
        editor.insert_paste("unknown-prefix");
        surface
            .refresh_completion_after_catalog(&editor, Mode::Command)
            .unwrap();
        assert!(!surface.completion.open);
        surface
            .completion
            .request("git st", 6, Mode::Command)
            .unwrap();
        surface
            .refresh_completion_after_catalog(&editor, Mode::Command)
            .unwrap();
        assert!(surface.completion.open);
        assert!(!surface.completion.automatic);
    }

    #[test]
    fn catalog_publication_cannot_reopen_completion_dismissed_for_the_current_edit() {
        let mut surface = cold_help_surface(Catalog::builtin());
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("cd /tmp");
        surface
            .completion
            .request(editor.buffer(), editor.cursor(), Mode::Command)
            .unwrap();
        assert!(surface.completion.open);
        assert!(surface.published_catalog().is_none());

        // The loading popup is already visible when Escape is consumed. A
        // subsequent catalog publication must not turn the next Enter back
        // into completion acceptance for this unchanged command.
        surface.dismiss_picker();
        surface.begin_catalog_admission().unwrap();
        await_catalog_admission(&mut surface).unwrap();
        surface
            .refresh_completion_after_catalog(&editor, Mode::Command)
            .unwrap();
        assert!(!surface.completion.open);
        assert!(!surface.completion.accepts_enter());

        // Explicit Tab remains an independent request even without an edit.
        surface
            .completion
            .request(editor.buffer(), editor.cursor(), Mode::Command)
            .unwrap();
        assert!(surface.completion.open);
        surface.dismiss_picker();
        editor.insert_paste("/");
        surface
            .refresh_completion_after_edit(&editor, Mode::Command)
            .unwrap();
        assert!(surface.completion.open);
        assert!(surface.completion.automatic);
    }

    #[test]
    fn cold_file_and_directory_pickers_do_not_depend_on_catalog_admission() {
        let mut surface = RichSurface::new_deferred(
            Box::new(|| {
                Err(
                    ShellError::new(ErrorCode::Io, "catalog remains unavailable")
                        .with_help("Filesystem pickers do not need command discovery"),
                )
            }),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        surface.open_picker(editor::PickerKind::Files, "cat ", 4, "files");
        assert!(surface.published_catalog().is_none());
        assert!(surface.picker.active());
        // Cargo test runs in this crate, whose manifest is a stable local file.
        let manifest = surface
            .completion
            .items
            .iter()
            .find(|item| item.display == "Cargo.toml")
            .unwrap();
        assert_eq!(manifest.source, "filesystem");
        assert_eq!(manifest.replace_start, 4);
        assert_eq!(manifest.replace_end, 4);

        surface.open_picker(editor::PickerKind::Directories, "cd ", 3, "directories");
        assert!(surface.published_catalog().is_none());
        assert!(!surface.completion.items.is_empty());
        assert!(
            surface
                .completion
                .items
                .iter()
                .all(|item| item.summary == "directory")
        );
        let oversized = "x".repeat(editor::MAX_EDITOR_BUFFER_BYTES + 1);
        surface.open_picker(
            editor::PickerKind::Files,
            &oversized,
            oversized.len(),
            "files",
        );
        assert!(surface.completion.items.is_empty());
    }

    #[test]
    fn cold_palette_keeps_query_and_replacement_when_catalog_arrives() {
        let mut surface = RichSurface::new_deferred(
            Box::new(|| Ok(Arc::new(Catalog::builtin()))),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        surface.open_picker(editor::PickerKind::Palette, "replace me", 10, "picker");
        surface.update_picker_query(|picker| picker.insert_query("doctor"));
        assert!(surface.completion.items.is_empty());
        let bottom_anchored = surface.picker.bottom_anchored();
        surface.begin_catalog_admission().unwrap();
        await_catalog_admission(&mut surface).unwrap();
        assert_eq!(surface.picker.query(), Some("doctor"));
        assert_eq!(surface.picker.bottom_anchored(), bottom_anchored);
        let selected = surface
            .completion
            .items
            .iter()
            .find(|item| item.value == "quirl config doctor")
            .unwrap();
        assert_eq!(selected.replace_end, 10);
    }

    #[test]
    fn dismissed_cold_palette_is_not_reopened_by_catalog_publication() {
        let mut surface = RichSurface::new_deferred(
            Box::new(|| Ok(Arc::new(Catalog::builtin()))),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        surface.open_picker(editor::PickerKind::Palette, "", 0, "picker");
        surface.update_picker_query(|picker| picker.insert_query("doctor"));
        surface.dismiss_picker();
        surface.begin_catalog_admission().unwrap();
        await_catalog_admission(&mut surface).unwrap();
        assert!(!surface.picker.active());
        assert!(!surface.completion.open);
    }

    #[test]
    fn deferred_catalog_does_not_hide_installed_history() {
        let mut surface = RichSurface::new_deferred(
            Box::new(|| {
                Err(
                    ShellError::new(ErrorCode::Io, "catalog remains unavailable")
                        .with_help("History recall does not need this loader"),
                )
            }),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        assert!(surface.published_catalog().is_none());
        surface.open_picker(editor::PickerKind::History, "", 0, "history");
        assert!(surface.completion.items.is_empty());
        surface.install_history_snapshot(vec![InteractiveHistoryEntry {
            command_line: "printf REOPENED_HISTORY".to_owned(),
            directory: Some("/saved/project".to_owned()),
            status: Some(0),
            rank_bias: 4_000,
        }]);
        surface.open_picker(editor::PickerKind::History, "REOPENED", 8, "history");
        assert!(surface.published_catalog().is_none());
        assert!(surface.picker.active());
        let selected = surface.completion.selected_item().unwrap();
        assert_eq!(selected.value, "printf REOPENED_HISTORY");
        assert_eq!(selected.replace_end, 8);
        assert_eq!(selected.source, "history-local");

        surface.open_picker(editor::PickerKind::History, "missing", 7, "history");
        assert!(surface.completion.items.is_empty());
    }

    #[test]
    fn deferred_catalog_publishes_one_complete_arc_exactly_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let loader_calls = Arc::clone(&calls);
        let expected = Arc::new(Catalog::builtin());
        let loader_catalog = Arc::clone(&expected);
        let (release_sender, release_receiver) = mpsc::channel();
        let mut surface = RichSurface::new_deferred(
            Box::new(move || {
                loader_calls.fetch_add(1, Ordering::Relaxed);
                release_receiver
                    .recv_timeout(Duration::from_secs(1))
                    .map_err(|error| {
                        ShellError::new(ErrorCode::Io, "catalog test gate failed")
                            .with_context(error.to_string())
                            .with_help("Release the catalog test gate")
                    })?;
                Ok(loader_catalog)
            }),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();

        assert!(surface.published_catalog().is_none());
        surface.begin_catalog_admission().unwrap();
        surface.begin_catalog_admission().unwrap();
        assert!(!surface.poll_catalog_admission().unwrap());
        release_sender.send(()).unwrap();
        await_catalog_admission(&mut surface).unwrap();

        let published = surface.published_catalog().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(Arc::ptr_eq(&expected, &published));
        assert!(Arc::ptr_eq(
            &published,
            surface.completion.published_catalog().unwrap()
        ));
        assert!(Arc::ptr_eq(
            &published,
            surface.input_analysis.published_catalog().unwrap()
        ));
        assert!(published.find("quirl run").is_some());
    }

    #[test]
    fn deferred_catalog_failure_preserves_the_original_error() {
        let expected = ShellError::new(ErrorCode::Validation, "catalog fixture is corrupt")
            .with_context("observed fixture bytes: 9")
            .with_help("Rebuild the fixture");
        let mut surface = RichSurface::new_deferred(
            Box::new(move || Err(expected)),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();

        surface.begin_catalog_admission().unwrap();
        let error = await_catalog_admission(&mut surface).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(error.message, "catalog fixture is corrupt");
        assert_eq!(error.details.context, ["observed fixture bytes: 9"]);
        assert_eq!(error.details.help, ["Rebuild the fixture"]);
        assert!(surface.published_catalog().is_none());
    }

    #[test]
    fn deferred_surface_applies_theme_and_keymap_before_catalog_admission() {
        let mut config = QuirlConfig::default();
        config.editor.keymap = "vim".to_owned();
        config.ui.theme = "first-frame".to_owned();
        config.ui.themes.insert(
            "first-frame".to_owned(),
            quirl_lua::builtin_theme("dracula").unwrap(),
        );
        let expected_theme = Theme::from_config(&config, true).unwrap();
        let surface = RichSurface::new_deferred(
            Box::new(|| Ok(Arc::new(Catalog::builtin()))),
            None,
            Arc::new(crate::StablePickerRanker),
            &config,
            PathBuf::new(),
        )
        .unwrap();

        assert_eq!(surface.keymap, "vim");
        assert_eq!(
            EditorState::new(&surface.keymap, Vec::new()).mode(),
            editor::EditorMode::VimInsert
        );
        assert_eq!(surface.theme, expected_theme);
        assert!(surface.published_catalog().is_none());
    }
}
