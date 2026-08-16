mod completion;
mod degrade;
mod editor;
mod frame;
mod highlight;
mod overlay;
mod statusbar;
mod theme;

pub use degrade::{select_surface, SurfaceKind};

use self::{
    completion::CompletionState,
    editor::{
        EditAction, EditorState, MAX_HISTORY_ENCODED_ENTRY_BYTES, MAX_HISTORY_ENTRY_BYTES,
        MAX_HISTORY_RETAINED_BYTES,
    },
    frame::FrameModel,
    highlight::InputAnalyzer,
    overlay::{contextual_help_query, PickerLayout, PickerOverlay},
    theme::Theme,
};
use super::{
    ExtensionCompleter, PickerRanker, QuirlPrompt, SurfaceSymbols, MODE_TOGGLE_HOST_COMMAND,
};
use crossterm::{
    cursor::{SetCursorStyle, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{self, ClearType},
};
use quirl_catalog::Catalog;
use quirl_core::{ErrorCode, ShellError};
use quirl_lua::QuirlConfig;
use quirl_syntax::{parse_command_list, Mode};
use ratatui::{
    backend::CrosstermBackend, layout::Position, text::Line, widgets::Paragraph, Terminal,
    TerminalOptions, Viewport,
};
use std::{
    collections::VecDeque,
    env, fs,
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
};

const EVENT_POLL: Duration = Duration::from_millis(16);
const TIMING_WINDOW: usize = 128;
// Keep the rich surface interoperable with Reedline's durable history format.
const HISTORY_NEWLINE_ESCAPE: &str = "<\\n>";
const MAX_HISTORY_FILE_BYTES: usize = MAX_HISTORY_RETAINED_BYTES * 4 + 50_000;
const HELP_DETAIL_SCROLL_MAX: u16 = 4_096;
static HISTORY_COMPACTION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveSignal {
    Success(String),
    CtrlC,
    CtrlD,
    HostCommand(String),
    Suspend,
}

pub struct RichSurface {
    catalog: Arc<Catalog>,
    completion: CompletionState,
    picker: PickerOverlay,
    picker_layout: PickerLayout,
    picker_preview: bool,
    expand_completion_pending: bool,
    keymap: String,
    history_path: PathBuf,
    history: Vec<String>,
    terminal: SurfaceTerminal,
    draw_times: VecDeque<Duration>,
    input_analysis: InputAnalyzer,
    show_timings: bool,
    hints: bool,
    transient: bool,
    completion_auto: bool,
    completion_min_chars: usize,
    semantic_hints: bool,
    help_active: bool,
    help_detail_scroll: u16,
}

impl RichSurface {
    pub fn new(
        catalog: Arc<Catalog>,
        extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
        picker_ranker: Arc<dyn PickerRanker>,
        config: &QuirlConfig,
        history_path: PathBuf,
    ) -> Result<Self, ShellError> {
        let history = read_history(&history_path);
        let input_analysis = InputAnalyzer::new(Arc::clone(&catalog));
        Ok(Self {
            completion: CompletionState::new(Arc::clone(&catalog), extension_completer),
            picker: PickerOverlay::new(picker_ranker),
            picker_layout: PickerLayout::from_config(&config.picker.layout),
            picker_preview: config.picker.preview,
            expand_completion_pending: false,
            catalog,
            keymap: config.editor.keymap.clone(),
            history_path,
            history,
            terminal: SurfaceTerminal::default(),
            draw_times: VecDeque::with_capacity(TIMING_WINDOW),
            input_analysis,
            show_timings: env::var("QUIRL_UI_TIMINGS").is_ok_and(|value| value == "1"),
            hints: config.ui.statusline.hints,
            transient: config.prompt.transient,
            completion_auto: config.completion.auto,
            completion_min_chars: usize::from(config.completion.min_chars),
            semantic_hints: config.editor.semantic_hints,
            help_active: false,
            help_detail_scroll: 0,
        })
    }

    pub fn read_line(&mut self, prompt: &QuirlPrompt) -> Result<InteractiveSignal, ShellError> {
        self.dismiss_picker();
        self.expand_completion_pending = false;
        let mut editor = EditorState::new(&self.keymap, self.history.clone());
        self.terminal.enter()?;
        let symbols = prompt.surface_symbols();
        let unicode = symbols.uses_unicode();
        let color = std::io::stderr().is_terminal()
            && env::var_os("NO_COLOR").is_none()
            && !env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
        let theme = Theme::new(color);
        let mut dirty = true;
        let mut prompt_prepared = false;

        loop {
            if dirty {
                let (_, terminal_height) = terminal::size().unwrap_or((80, 24));
                if self.semantic_hints {
                    self.input_analysis
                        .ensure(editor.revision(), editor.buffer(), prompt.mode);
                }
                let timing_text = self.timing_text();
                let analysis = self.input_analysis.current();
                let context_left = prompt.surface_context_left();
                let context_right = prompt.surface_context_right();
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
                    highlight_spans: if self.semantic_hints {
                        &analysis.spans
                    } else {
                        &[]
                    },
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
                };
                let height = model.height(terminal_height);
                self.terminal.ensure_height(height)?;
                let started = Instant::now();
                self.terminal.draw(&model)?;
                self.record_draw(started.elapsed());
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
                    if self.picker.active() {
                        self.update_picker_query(|picker| picker.insert_query(&text));
                        continue;
                    }
                    self.expand_completion_pending = false;
                    editor.insert_paste(&text);
                    self.completion.cancel_for_edit();
                }
                Event::Resize(_, _) => continue,
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if key.code == KeyCode::F(1) {
                        if self.help_active {
                            self.completion.next();
                            self.help_detail_scroll = 0;
                        } else {
                            self.open_context_help(editor.buffer(), editor.cursor());
                        }
                        continue;
                    }
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
                            KeyCode::Up => {
                                self.completion.previous();
                                continue;
                            }
                            KeyCode::Down => {
                                self.completion.next();
                                continue;
                            }
                            KeyCode::Enter => {
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
                            if input_is_incomplete(editor.buffer(), prompt.mode) {
                                editor.apply(EditAction::ForceNewline);
                                continue;
                            }
                            let buffer = editor.buffer().to_owned();
                            if !buffer.trim().is_empty() {
                                self.append_history(&buffer)?;
                            }
                            let transient = self
                                .transient
                                .then(|| transient_line(prompt.mode, &buffer, symbols));
                            self.terminal.release(transient)?;
                            return Ok(InteractiveSignal::Success(buffer));
                        }
                        EditAction::Eof if editor.buffer().is_empty() => {
                            self.terminal.release(None)?;
                            return Ok(InteractiveSignal::CtrlD);
                        }
                        EditAction::Eof => {
                            editor.apply(EditAction::Delete);
                        }
                        EditAction::Cancel => {
                            editor.clear();
                            self.dismiss_picker();
                            self.terminal.release(None)?;
                            return Ok(InteractiveSignal::CtrlC);
                        }
                        EditAction::ToggleGrammarMode => {
                            self.terminal.release(None)?;
                            return Ok(InteractiveSignal::HostCommand(
                                MODE_TOGGLE_HOST_COMMAND.to_owned(),
                            ));
                        }
                        EditAction::Complete => {
                            if self.completion.open {
                                self.completion.next();
                            } else {
                                self.completion.request(editor.buffer(), editor.cursor())?;
                            }
                        }
                        EditAction::ExpandCompletionPicker => {
                            if self.completion.streaming {
                                self.expand_completion_pending = true;
                            } else if self.completion.open {
                                let items = self.completion.items.clone();
                                self.open_picker_items(items, "completions", true);
                            } else {
                                self.completion.request(editor.buffer(), editor.cursor())?;
                                self.expand_completion_pending = true;
                            }
                        }
                        EditAction::Dismiss => self.dismiss_picker(),
                        EditAction::OpenPicker(kind) => {
                            self.open_picker(
                                kind,
                                editor.buffer(),
                                editor.cursor(),
                                "picker",
                                false,
                            );
                        }
                        EditAction::ClearScreen => {
                            execute!(io::stderr(), terminal::Clear(ClearType::All))
                                .map_err(terminal_error("clear the terminal"))?;
                        }
                        EditAction::Suspend => {
                            self.terminal.release(None)?;
                            return Ok(InteractiveSignal::Suspend);
                        }
                        action => {
                            if editor.apply(action) {
                                self.expand_completion_pending = false;
                                self.completion.cancel_for_edit();
                                if self.completion_auto
                                    && current_token_len(editor.buffer(), editor.cursor())
                                        >= self.completion_min_chars
                                {
                                    self.completion.request(editor.buffer(), editor.cursor())?;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn open_picker(
        &mut self,
        kind: editor::PickerKind,
        line: &str,
        cursor: usize,
        label: &'static str,
        expanded: bool,
    ) {
        self.help_active = false;
        self.help_detail_scroll = 0;
        let items = overlay::items(kind, &self.catalog, &self.history, line, cursor);
        self.open_picker_items(items, label, expanded);
    }

    fn open_context_help(&mut self, line: &str, cursor: usize) {
        let query = contextual_help_query(&self.catalog, line, cursor);
        let items = overlay::items(
            editor::PickerKind::Palette,
            &self.catalog,
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

    pub fn sync_history(&mut self) -> Result<(), ShellError> {
        if fs::metadata(&self.history_path)
            .is_ok_and(|metadata| metadata.len() > MAX_HISTORY_FILE_BYTES as u64)
        {
            self.compact_history()?;
        }
        Ok(())
    }

    fn append_history(&mut self, value: &str) -> Result<(), ShellError> {
        if self.history.last().is_some_and(|last| last == value) {
            return Ok(());
        }
        if let Some(parent) = self.history_path.parent() {
            fs::create_dir_all(parent).map_err(history_error(&self.history_path))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_path)
            .map_err(history_error(&self.history_path))?;
        writeln!(file, "{}", encode_history_entry(value))
            .map_err(history_error(&self.history_path))?;
        file.flush().map_err(history_error(&self.history_path))?;
        drop(file);
        self.history.push(value.to_owned());
        trim_history(&mut self.history);
        if fs::metadata(&self.history_path)
            .is_ok_and(|metadata| metadata.len() > MAX_HISTORY_FILE_BYTES as u64)
        {
            self.compact_history()?;
        }
        Ok(())
    }

    fn compact_history(&self) -> Result<(), ShellError> {
        let file_name = self
            .history_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("history");
        let temporary = self.history_path.with_file_name(format!(
            ".{file_name}.compact-{}-{}.tmp",
            std::process::id(),
            HISTORY_COMPACTION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(history_error(&temporary))?;
            for entry in &self.history {
                let encoded = encode_history_entry(entry);
                if encoded.len() > MAX_HISTORY_ENCODED_ENTRY_BYTES {
                    continue;
                }
                writeln!(file, "{encoded}").map_err(history_error(&temporary))?;
            }
            file.flush().map_err(history_error(&temporary))?;
            file.sync_all().map_err(history_error(&temporary))?;
            fs::rename(&temporary, &self.history_path).map_err(history_error(&self.history_path))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
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
    height: u16,
    active: bool,
}

impl SurfaceTerminal {
    fn enter(&mut self) -> Result<(), ShellError> {
        terminal::enable_raw_mode().map_err(terminal_error("enable terminal raw mode"))?;
        self.active = true;
        let result = execute!(
            io::stderr(),
            EnableBracketedPaste,
            SetCursorStyle::SteadyBar
        );
        if let Err(error) = result {
            self.reset_best_effort();
            return Err(terminal_error("enable terminal input features")(error));
        }
        Ok(())
    }

    fn ensure_height(&mut self, height: u16) -> Result<(), ShellError> {
        let height = height.max(1);
        if self.terminal.is_some() && self.height == height {
            return Ok(());
        }
        if let Some(mut terminal) = self.terminal.take() {
            if let Err(error) = terminal.clear() {
                let _ = terminal.show_cursor();
                self.reset_best_effort();
                return Err(terminal_error("clear the prior interactive frame")(error));
            }
            if let Err(error) = terminal.show_cursor() {
                self.reset_best_effort();
                return Err(terminal_error("restore the terminal cursor")(error));
            }
        }
        let backend = CrosstermBackend::new(io::stderr());
        let terminal_result = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        );
        let mut terminal = match terminal_result {
            Ok(terminal) => terminal,
            Err(error) => {
                self.reset_best_effort();
                return Err(terminal_error("create the inline terminal viewport")(error));
            }
        };
        if let Err(error) = terminal.hide_cursor() {
            self.reset_best_effort();
            return Err(terminal_error("hide the software cursor")(error));
        }
        self.terminal = Some(terminal);
        self.height = height;
        Ok(())
    }

    fn draw(&mut self, model: &FrameModel<'_>) -> Result<(), ShellError> {
        let terminal = self.terminal.as_mut().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "the inline terminal is unavailable")
                .with_help("Retry with ui.surface = \"simple\"")
        })?;
        let result = terminal.draw(|frame| model.render(frame)).map(|_| ());
        if let Err(error) = result {
            self.reset_best_effort();
            return Err(terminal_error("draw the interactive frame")(error));
        }
        Ok(())
    }

    fn release(&mut self, transient: Option<String>) -> Result<(), ShellError> {
        let has_transient = transient.is_some();
        let mut failure = None;
        if let Some(line) = transient {
            if let Err(error) = self.ensure_height(1) {
                failure = Some(error);
            } else if let Some(terminal) = self.terminal.as_mut() {
                if let Err(error) = terminal.draw(|frame| {
                    frame.render_widget(Paragraph::new(Line::raw(line)), frame.area());
                    frame.set_cursor_position(Position::new(
                        0,
                        frame.area().bottom().saturating_sub(1),
                    ));
                }) {
                    retain_error(
                        &mut failure,
                        terminal_error("draw the transient prompt")(error),
                    );
                }
            }
        }
        if let Some(mut terminal) = self.terminal.take() {
            if !has_transient {
                if let Err(error) = terminal.clear() {
                    retain_error(
                        &mut failure,
                        terminal_error("clear the interactive frame")(error),
                    );
                }
            }
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
            SetCursorStyle::DefaultUserShape
        ) {
            retain_error(
                &mut failure,
                terminal_error("restore terminal input features")(error),
            );
        }
        if let Err(error) = terminal::disable_raw_mode() {
            retain_error(
                &mut failure,
                terminal_error("restore cooked terminal mode")(error),
            );
        }
        if has_transient {
            let mut stderr = io::stderr();
            if let Err(error) = stderr.write_all(b"\r\n").and_then(|()| stderr.flush()) {
                retain_error(
                    &mut failure,
                    terminal_error("commit the transient prompt to scrollback")(error),
                );
            }
        }
        self.finish_release(failure)
    }

    fn finish_release(&mut self, failure: Option<ShellError>) -> Result<(), ShellError> {
        self.height = 0;
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
            SetCursorStyle::DefaultUserShape
        );
        let _ = terminal::disable_raw_mode();
        self.height = 0;
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

fn read_history(path: &Path) -> Vec<String> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let file_len = file.metadata().map_or(0, |metadata| metadata.len());
    let start = file_len.saturating_sub(MAX_HISTORY_FILE_BYTES as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(file_len.saturating_sub(start))
            .unwrap_or(MAX_HISTORY_FILE_BYTES)
            .min(MAX_HISTORY_FILE_BYTES),
    );
    if file
        .take(MAX_HISTORY_FILE_BYTES as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Vec::new();
    }
    let bytes = if start == 0 {
        bytes.as_slice()
    } else if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
        &bytes[newline.saturating_add(1)..]
    } else {
        return Vec::new();
    };
    let mut retained_bytes = 0_usize;
    let mut history = Vec::new();
    for line in bytes.rsplit(|byte| *byte == b'\n') {
        if line.is_empty() || line.len() > MAX_HISTORY_ENCODED_ENTRY_BYTES {
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let entry = decode_history_entry(line);
        if entry.len() > MAX_HISTORY_ENTRY_BYTES {
            continue;
        }
        let next_bytes = retained_bytes.saturating_add(entry.len());
        if history.len() == 50_000 || next_bytes > MAX_HISTORY_RETAINED_BYTES {
            break;
        }
        retained_bytes = next_bytes;
        history.push(entry);
    }
    history.reverse();
    history
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

fn encode_history_entry(value: &str) -> String {
    value.replace('\n', HISTORY_NEWLINE_ESCAPE)
}

fn decode_history_entry(value: &str) -> String {
    value.replace(HISTORY_NEWLINE_ESCAPE, "\n")
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
            .with_help("Retry with ui.surface = \"simple\" if the terminal lacks inline UI support")
    }
}

fn transient_line(mode: Mode, buffer: &str, symbols: SurfaceSymbols) -> String {
    format!("{}{buffer}", symbols.input_indicator(mode))
}

fn input_is_incomplete(buffer: &str, mode: Mode) -> bool {
    if mode == Mode::Data {
        return false;
    }
    parse_command_list(buffer).is_err_and(|error| {
        error.message.starts_with("unclosed ")
            || error.message.contains("ends with an escape")
            || (error.message.contains("expected a command")
                && buffer.trim_end().ends_with(['|', '&']))
    })
}

fn current_token_len(buffer: &str, cursor: usize) -> usize {
    buffer[..cursor.min(buffer.len())]
        .rsplit_once(char::is_whitespace)
        .map_or(
            buffer[..cursor.min(buffer.len())].chars().count(),
            |(_, token)| token.chars().count(),
        )
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

    #[test]
    fn incomplete_quotes_continue_instead_of_executing() {
        assert!(input_is_incomplete("printf 'hello", Mode::Command));
        assert!(!input_is_incomplete("printf hello", Mode::Command));
    }

    #[test]
    fn multiline_history_uses_the_reedline_compatible_encoding() {
        let entry = "printf 'one\ntwo'\necho done";
        let encoded = encode_history_entry(entry);
        assert!(!encoded.contains('\n'));
        assert!(encoded.contains(HISTORY_NEWLINE_ESCAPE));
        assert_eq!(decode_history_entry(&encoded), entry);
    }

    #[test]
    fn history_reader_skips_oversized_entries_and_keeps_the_valid_tail() {
        let path = std::env::temp_dir().join(format!(
            "quirl-history-bound-{}-{}",
            std::process::id(),
            HISTORY_COMPACTION_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = File::create(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_HISTORY_ENCODED_ENTRY_BYTES + 1])
            .unwrap();
        file.write_all(b"\nsafe tail\n").unwrap();
        drop(file);
        assert_eq!(read_history(&path), vec!["safe tail"]);
        fs::remove_file(path).unwrap();
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
            height: 4,
            active: true,
        };
        let failure = ShellError::new(ErrorCode::Io, "injected terminal cleanup failure")
            .with_help("Retry terminal cleanup from Drop");

        assert!(terminal.finish_release(Some(failure)).is_err());
        assert!(terminal.active);
        assert_eq!(terminal.height, 0);

        assert!(terminal.finish_release(None).is_ok());
        assert!(!terminal.active);
    }
}
