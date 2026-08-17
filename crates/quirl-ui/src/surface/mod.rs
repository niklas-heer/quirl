mod completion;
mod degrade;
mod editor;
mod frame;
pub(crate) mod highlight;
mod overlay;
mod runtime;
mod statusbar;
mod transcript;

pub use degrade::{select_surface, SurfaceKind};
pub use runtime::{
    InteractiveActivityProvider, InteractiveActivitySnapshot, InteractiveDataSnapshot,
    InteractiveJobAction, InteractiveJobSnapshot, InteractiveJobStatus, InteractivePanelBatch,
    InteractivePanelProvider, InteractivePanelSnapshot, InteractiveRuntimeSnapshot,
    ACTIVITY_MESSAGE_BYTES_MAX, DATA_ITEMS_MAX, DATA_RETAINED_BYTES_MAX, JOB_ACTION_ITEMS_MAX,
    JOB_RETAINED_BYTES_MAX, PANEL_COLUMNS_MAX, PANEL_COUNT_MAX, PANEL_FIELD_BYTES_MAX,
    PANEL_GENERATION_BYTES_MAX, PANEL_ROWS_MAX,
};

/// One-shot rich-session loader that returns the complete immutable catalog.
///
/// The rich surface invokes this only after its first frame has been flushed
/// and before terminal input is polled. An error leaves terminal restoration to
/// the surface's existing RAII guard and is returned unchanged.
pub type CatalogLoader = Box<dyn FnOnce() -> Result<Arc<Catalog>, ShellError>>;

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
    frame::FrameModel,
    highlight::InputAnalyzer,
    overlay::{contextual_help_query, PickerLayout, PickerOverlay},
    runtime::RuntimeSurfaceState,
    transcript::{TextPosition, Transcript, TranscriptLimits},
};
use super::{
    read_history, ExtensionCompleter, PickerRanker, QuirlPrompt, MAX_HISTORY_ENCODED_ENTRY_BYTES,
    MAX_HISTORY_ENTRY_BYTES, MAX_HISTORY_RETAINED_BYTES,
};
use crate::theme::Theme;
use crossterm::{
    cursor::{SetCursorStyle, Show},
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    style::Print,
    terminal::{self, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use quirl_catalog::Catalog;
use quirl_core::{
    replace_file_atomically, AtomicReplaceOptions, ErrorCode, OutputStream, ShellError,
};
use quirl_lua::QuirlConfig;
use quirl_syntax::{parse_command_list, Mode};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal, TerminalOptions, Viewport};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    collections::VecDeque,
    env, fs,
    fs::OpenOptions,
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
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
    Selection { anchor: TextPosition, dragged: bool },
    Scrollbar,
}

#[derive(Debug, Default)]
struct StreamingText {
    utf8_tail: Vec<u8>,
    line: String,
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
        self.sequence = TerminalSequence::Ground;
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
        self.sequence = TerminalSequence::Ground;
        lines
    }

    fn push_text(&mut self, text: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for character in text.chars() {
            match &mut self.sequence {
                TerminalSequence::Ground => match character {
                    '\u{1b}' => self.sequence = TerminalSequence::Escape,
                    '\n' => lines.push(std::mem::take(&mut self.line)),
                    '\r' => self.line.clear(),
                    '\t' => self.line.push_str("    "),
                    value if !value.is_control() => self.line.push(value),
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
    /// The host should suspend the shell after performing platform job control.
    Suspend,
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
    completion: CompletionState,
    picker: PickerOverlay,
    picker_layout: PickerLayout,
    picker_preview: bool,
    expand_completion_pending: bool,
    keymap: String,
    history_path: PathBuf,
    history: Vec<InteractiveHistoryEntry>,
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
    theme: Theme,
    runtime: RuntimeSurfaceState,
    transcript: Transcript,
    transcript_truncated: bool,
    output_focus: bool,
    output_cursor_line: usize,
    output_anchor_line: Option<usize>,
    output_notice: Option<String>,
    transcript_area: Rect,
    mouse_drag: Option<MouseDrag>,
    intent_completion: IntentCompletionState,
    stream_stdout: StreamingText,
    stream_stderr: StreamingText,
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
            picker_layout: PickerLayout::from_config(&config.picker.layout),
            picker_preview: config.picker.preview,
            expand_completion_pending: false,
            catalog: Some(catalog),
            catalog_loader: None,
            keymap: config.editor.keymap.clone(),
            history_path,
            history,
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
            transcript_area: Rect::default(),
            mouse_drag: None,
            intent_completion: IntentCompletionState::default(),
            stream_stdout: StreamingText::default(),
            stream_stderr: StreamingText::default(),
        })
    }

    /// Construct a rich surface whose complete catalog is admitted after the
    /// first successful terminal flush and before the first input poll.
    ///
    /// Configuration, theme, keymap, history, picker policy, and terminal
    /// acquisition remain eager. The loader is consumed exactly once. Its
    /// returned [`Arc<Catalog>`] is published as one complete generation to
    /// analysis, completion, picker/help, and the composition root.
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
            completion: CompletionState::unpublished(extension_completer),
            picker: PickerOverlay::new(picker_ranker),
            picker_layout: PickerLayout::from_config(&config.picker.layout),
            picker_preview: config.picker.preview,
            expand_completion_pending: false,
            keymap: config.editor.keymap.clone(),
            history_path,
            history,
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
            transcript_area: Rect::default(),
            mouse_drag: None,
            intent_completion: IntentCompletionState::default(),
            stream_stdout: StreamingText::default(),
            stream_stderr: StreamingText::default(),
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
        self.append_transcript_line(&format!("❯ {command}"));
        self.output_notice = Some("running · output streams into this viewport".to_owned());
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
        self.append_stream_lines(lines);
        self.draw_execution(prompt)
    }

    /// Finish the active command record after every captured reader has been drained.
    pub fn finish_command_stream(
        &mut self,
        status: i32,
        duration: Duration,
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        let stdout = self.stream_stdout.finish();
        self.append_stream_lines(stdout);
        let stderr = self.stream_stderr.finish();
        self.append_stream_lines(stderr);
        self.append_transcript_line(&format!("── exit {status} · {}ms ──", duration.as_millis()));
        self.output_cursor_line = self.transcript.line_count().saturating_sub(1);
        self.output_notice =
            Some("result kept in viewport · PageUp/PageDown scroll · Alt-Q O copy".to_owned());
        self.draw_execution(prompt)
    }

    fn append_stream_lines(&mut self, lines: Vec<String>) {
        for line in lines {
            self.append_transcript_line(&line);
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
        let (terminal_width, terminal_height) = terminal::size().unwrap_or((80, 24));
        let model = FrameModel {
            context_left: &context_left,
            context_right: &context_right,
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
            runtime: &self.runtime,
            transcript: Some(&self.transcript),
            transcript_truncated: self.transcript_truncated,
            output_focus: false,
            output_notice: self.output_notice.as_deref(),
        };
        self.transcript_area =
            model.transcript_area(Rect::new(0, 0, terminal_width, terminal_height));
        self.terminal.draw(&model)
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

    fn admit_catalog(&mut self) -> Result<(), ShellError> {
        if self.catalog.is_some() {
            return Ok(());
        }
        let loader = self.catalog_loader.take().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "interactive catalog loader is unavailable")
                .with_help("Restart Quirl to create a fresh interactive session")
        })?;
        let catalog = loader()?;

        // Failure model: the terminal is already raw and owns the alternate
        // viewport. No input observer runs during this transaction. Publish
        // clones to every consumer first, then expose the catalog slot last so
        // no reachable state can observe a partial generation.
        self.completion.publish_catalog(Arc::clone(&catalog));
        self.input_analysis.publish_catalog(Arc::clone(&catalog));
        self.catalog = Some(catalog);
        self.runtime.catalog_admitted();
        Ok(())
    }

    /// Replace the bounded immutable job and typed-result sources for the next frame.
    pub fn install_runtime_snapshot(&mut self, snapshot: InteractiveRuntimeSnapshot) {
        self.runtime.install_snapshot(snapshot);
    }

    /// Replace the next editor and fuzzy picker history with a bounded snapshot.
    pub fn install_history_snapshot(&mut self, history: Vec<InteractiveHistoryEntry>) {
        self.history = history;
    }

    /// Install the bounded local intent-search provider used by AI mode.
    pub fn set_intent_completer(&mut self, completer: Box<dyn ExtensionCompleter + Send>) {
        self.intent_completion.install(completer);
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
        self.intent_completion.cancel();
        self.expand_completion_pending = false;
        let mut editor = EditorState::new(
            &self.keymap,
            self.history
                .iter()
                .map(|entry| entry.command_line.clone())
                .collect(),
        );
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
                let (terminal_width, terminal_height) = terminal::size().unwrap_or((80, 24));
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
                    runtime: &self.runtime,
                    transcript: Some(&self.transcript),
                    transcript_truncated: self.transcript_truncated,
                    output_focus: self.output_focus,
                    output_notice: self.output_notice.as_deref(),
                };
                let transcript_area =
                    model.transcript_area(Rect::new(0, 0, terminal_width, terminal_height));
                let started = Instant::now();
                self.terminal.draw(&model)?;
                self.transcript_area = transcript_area;
                let draw_elapsed = started.elapsed();
                // Ratatui flushes the backend before `draw` returns. Catalog
                // admission is deliberately the next effect: terminal input
                // remains queued until this complete generation is published.
                self.admit_catalog()?;
                self.record_draw(draw_elapsed);
                dirty = false;
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
            if !event::poll(EVENT_POLL).map_err(terminal_error("poll terminal input"))? {
                continue;
            }
            let input_event = event::read().map_err(terminal_error("read terminal input"))?;
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
                    self.output_notice = None;
                    self.return_to_tail_for_input();
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
                    self.handle_mouse(mouse);
                    continue;
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if self.output_focus {
                        self.handle_output_key(key.code, key.modifiers)?;
                        continue;
                    }
                    self.output_notice = None;
                    if self.leader_active {
                        self.return_to_tail_for_input();
                        self.handle_leader_key(key.code, prompt, editor.buffer(), editor.cursor())?;
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
                                    editor.replace(
                                        item.replace_start,
                                        item.replace_end,
                                        &item.value,
                                    );
                                    self.dismiss_picker();
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
                    if self.completion.open && !self.picker.active() {
                        match key.code {
                            KeyCode::Up if prompt.mode == Mode::Natural => {
                                self.completion.previous();
                                continue;
                            }
                            KeyCode::Up => {
                                self.completion.dismiss();
                                self.open_picker(
                                    editor::PickerKind::History,
                                    editor.buffer(),
                                    editor.cursor(),
                                    "history",
                                );
                                continue;
                            }
                            KeyCode::Down => {
                                self.completion.next();
                                continue;
                            }
                            KeyCode::Enter if prompt.mode == Mode::Natural => {
                                if let Some(item) = self.completion.selected_item().cloned() {
                                    editor.replace(0, editor.buffer().len(), &item.value);
                                    prompt.set_mode(Mode::Command);
                                    self.intent_completion.cancel();
                                    self.completion.dismiss();
                                    self.refresh_completion_after_edit(&editor, prompt.mode)?;
                                }
                                continue;
                            }
                            KeyCode::Enter if self.completion.accepts_enter() => {
                                if let Some(item) = self.completion.selected_item().cloned() {
                                    editor.replace(
                                        item.replace_start,
                                        item.replace_end,
                                        &item.value,
                                    );
                                    self.completion.dismiss();
                                    continue;
                                }
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
                            if prompt.mode == Mode::Natural {
                                // AI mode is an intent-to-command editor, not an
                                // execution grammar. Search happens as the user
                                // types; Enter either accepts the highlighted
                                // safe suggestion above or keeps the query live.
                                continue;
                            }
                            if input_is_incomplete(editor.buffer(), prompt.mode) {
                                editor.apply(EditAction::ForceNewline);
                                continue;
                            }
                            let buffer = editor.buffer().to_owned();
                            if !buffer.trim().is_empty() {
                                self.append_history(&buffer)?;
                            }
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
                            execute!(io::stderr(), terminal::Clear(ClearType::All))
                                .map_err(terminal_error("clear the terminal"))?;
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

    fn toggle_grammar_mode(&mut self, prompt: &mut QuirlPrompt) {
        // Failure model: releasing the alternate screen here destroys the
        // editor state, while host feedback commits an unbounded line per
        // toggle to scrollback. A mode switch transfers no terminal or process
        // ownership, so it must remain an in-frame state transition. Preserve
        // the buffer and cursor, invalidate mode-sensitive transient UI, and
        // let the already-dirty loop repaint exactly one current frame.
        prompt.toggle_mode();
        self.expand_completion_pending = false;
        self.completion.cancel_for_edit();
        self.picker.dismiss();
        self.help_active = false;
        self.help_detail_scroll = 0;
    }

    fn open_leader(&mut self, replace_end: usize) {
        self.leader_active = true;
        let entries = [
            ("n", "Normal mode", "Native commands and pipelines"),
            ("d", "Data mode", "Typed data expressions and pipelines"),
            ("i", "AI mode", "Describe your intent in everyday language"),
            ("h", "History", "Search commands from every directory"),
            ("p", "Command palette", "Browse Quirl commands and help"),
            ("f", "Files", "Find a file in this directory"),
            ("c", "Directories", "Find a directory"),
            ("j", "Jobs", "Inspect background jobs"),
            ("r", "Results", "Inspect recent typed data"),
            ("o", "Output", "Scroll and copy retained command output"),
        ];
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
            KeyCode::Char('c') => {
                self.open_picker(editor::PickerKind::Directories, line, cursor, "directories")
            }
            KeyCode::Char('j') => self.open_picker(editor::PickerKind::Jobs, line, cursor, "jobs"),
            KeyCode::Char('r') => {
                self.open_picker(editor::PickerKind::Data, line, cursor, "results")
            }
            KeyCode::Char('o') if self.transcript.line_count() > 0 => self.open_output_focus(),
            _ => {}
        }
        Ok(())
    }

    fn transcript_visible_rows(&self) -> usize {
        let rows = terminal::size().map_or(24, |(_, rows)| rows);
        transcript_visible_rows_for_terminal(rows, self.transcript.follows_tail())
    }

    fn return_to_tail_for_input(&mut self) {
        self.transcript.scroll_to_end();
        self.dismiss_output_selection();
    }

    fn open_output_focus(&mut self) {
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
                    self.mouse_drag = Some(MouseDrag::Scrollbar);
                    self.scrollbar_to_row(mouse.row);
                } else if let Some((before, after)) = self.transcript_hit(mouse.column, mouse.row) {
                    self.output_focus = true;
                    self.output_notice = None;
                    self.output_anchor_line = None;
                    self.output_cursor_line = before.line_index;
                    self.transcript.begin_selection(before);
                    self.transcript.update_selection(after);
                    self.mouse_drag = Some(MouseDrag::Selection {
                        anchor: before,
                        dragged: false,
                    });
                } else {
                    self.dismiss_output_selection();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => match self.mouse_drag {
                Some(MouseDrag::Selection { anchor, .. }) => {
                    self.autoscroll_mouse_selection(mouse.row);
                    self.update_mouse_selection(anchor, mouse.column, mouse.row);
                    self.mouse_drag = Some(MouseDrag::Selection {
                        anchor,
                        dragged: true,
                    });
                }
                Some(MouseDrag::Scrollbar) => self.scrollbar_to_row(mouse.row),
                None => {}
            },
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(MouseDrag::Selection { anchor, dragged }) = self.mouse_drag {
                    self.update_mouse_selection(anchor, mouse.column, mouse.row);
                    self.mouse_drag = None;
                    if dragged && self.terminal.input_active {
                        let _ = self.copy_output_selection();
                    }
                } else if self.mouse_drag == Some(MouseDrag::Scrollbar) {
                    self.scrollbar_to_row(mouse.row);
                    self.mouse_drag = None;
                }
            }
            _ => {}
        }
    }

    fn transcript_hit(&self, column: u16, row: u16) -> Option<(TextPosition, TextPosition)> {
        let area = self.transcript_area;
        if column < area.x || column >= area.right() || row < area.y || row >= area.bottom() {
            return None;
        }
        let visible = self.transcript.visible_range(usize::from(area.height));
        let line_index = visible
            .start
            .saturating_add(usize::from(row.saturating_sub(area.y)));
        self.transcript
            .hit_test(line_index, usize::from(column.saturating_sub(area.x)))
    }

    fn mouse_is_on_scrollbar(&self, column: u16, row: u16) -> bool {
        let area = self.transcript_area;
        area.height > 0
            && self.transcript.line_count() > usize::from(area.height)
            && column == area.right().saturating_sub(1)
            && row >= area.y
            && row < area.bottom()
    }

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
        let start = if track_max == 0 {
            0
        } else {
            track_position
                .saturating_mul(maximum_start)
                .saturating_add(track_max / 2)
                / track_max
        };
        self.transcript.scroll_to(start, visible_rows);
    }

    fn autoscroll_mouse_selection(&mut self, row: u16) {
        let area = self.transcript_area;
        let visible_rows = usize::from(area.height);
        if row <= area.y {
            self.transcript.scroll_up(1, visible_rows);
        } else if row >= area.bottom().saturating_sub(1) {
            self.transcript.scroll_down(1, visible_rows);
        }
    }

    fn update_mouse_selection(&mut self, anchor: TextPosition, column: u16, row: u16) {
        let Some((before, after)) = self.transcript_hit(column, row) else {
            return;
        };
        let head = if before < anchor { before } else { after };
        self.output_cursor_line = head.line_index;
        self.transcript.update_selection(head);
    }

    fn dismiss_output_selection(&mut self) {
        self.output_focus = false;
        self.output_anchor_line = None;
        self.output_notice = None;
        self.mouse_drag = None;
        self.transcript.clear_selection();
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
        let text = match self
            .transcript
            .selected_text_bounded(TRANSCRIPT_COPY_BYTES_MAX)
        {
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

    fn open_picker(
        &mut self,
        kind: editor::PickerKind,
        line: &str,
        cursor: usize,
        label: &'static str,
    ) {
        self.help_active = false;
        self.help_detail_scroll = 0;
        let items = match kind {
            editor::PickerKind::Jobs => self.runtime.job_items(line.len()),
            editor::PickerKind::Data => self.runtime.data_items(line.len()),
            _ => self.catalog.as_deref().map_or_else(Vec::new, |catalog| {
                overlay::items(kind, catalog, &self.history, line, cursor)
            }),
        };
        if kind == editor::PickerKind::History {
            let visible = self.picker.open_with_query(items, label, true, line);
            self.completion.show_picker_results(visible, label);
        } else if kind.bottom_anchored() {
            self.open_bottom_anchored_picker(items, label);
        } else {
            self.open_picker_items(items, label, false);
        }
    }

    fn open_context_help(&mut self, line: &str, cursor: usize) {
        let Some(catalog) = self.catalog.as_deref() else {
            return;
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
        self.completion.show_picker_results(visible, "catalog help");
        self.help_active = true;
        self.help_detail_scroll = 0;
    }

    fn open_picker_items(
        &mut self,
        items: Vec<completion::CompletionItem>,
        label: &'static str,
        expanded: bool,
    ) {
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
        self.picker.dismiss();
        self.completion.dismiss();
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

impl Drop for RichSurface {
    fn drop(&mut self) {
        // Failure model: a read/history/completion error can unwind while raw
        // mode is active, and completion workers may still be inside plugin
        // code. Restore terminal ownership explicitly before Rust begins
        // dropping fields in declaration order.
        if self.terminal.active {
            self.terminal.reset_best_effort();
        }
    }
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
        let size = terminal::size().map_err(terminal_error("measure the interactive terminal"))?;
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

    fn draw(&mut self, model: &FrameModel<'_>) -> Result<(), ShellError> {
        let size = match terminal::size() {
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
        if self.last_size != Some(size) {
            let backend = CrosstermBackend::new(io::stderr());
            let replacement = Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Fixed(area),
                },
            )
            .map_err(terminal_error("recreate the resized interactive frame"))?;
            *terminal = replacement;
            self.last_size = Some(size);
        }
        let result = terminal.draw(|frame| model.render(frame)).map(|_| ());
        if let Err(error) = result {
            self.reset_best_effort();
            return Err(terminal_error("draw the interactive frame")(error));
        }
        Ok(())
    }

    fn copy_to_clipboard(&mut self, text: &str) -> Result<(), ShellError> {
        debug_assert!(text.len() <= TRANSCRIPT_COPY_BYTES_MAX);
        let payload = encode_base64(text.as_bytes());
        execute!(io::stderr(), Print(format!("\u{1b}]52;c;{payload}\u{7}")))
            .map_err(terminal_error("copy the output selection through OSC 52"))
    }

    fn release(&mut self) -> Result<(), ShellError> {
        let mut failure = None;
        if let Some(mut terminal) = self.terminal.take() {
            if let Err(error) = terminal.show_cursor() {
                retain_error(
                    &mut failure,
                    terminal_error("restore the terminal cursor")(error),
                );
            }
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
                Ok(()) => self.alternate_screen = false,
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
            let _ = terminal.clear();
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

fn retain_error(slot: &mut Option<ShellError>, error: ShellError) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

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
            Ok(metadata.len() > MAX_HISTORY_FILE_BYTES as u64)
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
        .take(HISTORY_REPLACEMENT_BYTES_MAX.saturating_add(1) as u64)
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

fn current_token_len(buffer: &str, cursor: usize) -> usize {
    buffer[..cursor.min(buffer.len())]
        .rsplit_once(char::is_whitespace)
        .map_or(
            buffer[..cursor.min(buffer.len())].chars().count(),
            |(_, token)| token.chars().count(),
        )
}

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

fn timing_p95(samples: &VecDeque<Duration>) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    let index = (sorted.len().saturating_sub(1) * 95) / 100;
    sorted.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn incomplete_quotes_continue_instead_of_executing() {
        assert!(input_is_incomplete("printf 'hello", Mode::Command));
        assert!(!input_is_incomplete("printf hello", Mode::Command));
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
    fn data_ls_information_opens_automatically_without_enabling_global_auto_completion() {
        let config = QuirlConfig::default();
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
        assert!(surface
            .completion
            .items
            .iter()
            .any(|item| item.value == "--all" && item.kind == completion::CompletionKind::Flag));
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

            surface.toggle_grammar_mode(&mut prompt);

            assert_eq!(prompt.mode(), expected_mode);
            assert_eq!(prompt.surface_context_left(), expected_mode.to_string());
            assert_eq!(editor.buffer(), buffer);
            assert_eq!(editor.cursor(), cursor);
            assert!(!surface.expand_completion_pending);
            assert!(!surface.completion.open);
            assert!(!surface.help_active);
        }
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
    fn streaming_text_preserves_utf8_split_across_chunks() {
        let mut stream = StreamingText::default();
        let bytes = "repo → target\n".as_bytes();
        let split = bytes.iter().position(|byte| *byte == 0xe2).unwrap() + 1;
        assert!(stream.push(&bytes[..split]).is_empty());
        assert_eq!(stream.push(&bytes[split..]), ["repo → target"]);
        assert!(stream.finish().is_empty());
    }

    #[test]
    fn mouse_drag_selects_exact_visible_utf8_text() {
        let mut surface = RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();
        surface.append_transcript_line("alpha βeta omega");
        surface.transcript_area = Rect::new(0, 0, 40, 1);

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

        assert!(surface.output_focus);
        assert_eq!(
            surface
                .transcript
                .selected_text_bounded(TRANSCRIPT_COPY_BYTES_MAX),
            Ok(Some("βeta".to_owned()))
        );
        assert_eq!(surface.mouse_drag, None);
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
    fn deferred_catalog_publishes_one_complete_arc_exactly_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let loader_calls = Arc::clone(&calls);
        let expected = Arc::new(Catalog::builtin());
        let loader_catalog = Arc::clone(&expected);
        let mut surface = RichSurface::new_deferred(
            Box::new(move || {
                loader_calls.fetch_add(1, Ordering::Relaxed);
                Ok(loader_catalog)
            }),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap();

        assert!(surface.published_catalog().is_none());
        surface.admit_catalog().unwrap();
        surface.admit_catalog().unwrap();

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

        let error = surface.admit_catalog().unwrap_err();
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
