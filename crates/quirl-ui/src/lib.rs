//! Terminal interaction that treats completion and diagnostics as core behavior.

mod panel;
mod surface;
mod theme;

pub use panel::{
    directory_panel, process_panel, LiveBuffer, LiveSample, LiveSnapshot, PanelModel,
    ProcessPanelRow,
};
pub use surface::{
    select_surface, CatalogLoader, InteractiveDataSnapshot, InteractiveJobAction,
    InteractiveJobSnapshot, InteractiveJobStatus, InteractivePanelBatch, InteractivePanelProvider,
    InteractivePanelSnapshot, InteractiveRuntimeSnapshot, InteractiveSignal, RichSurface,
    SurfaceKind, DATA_ITEMS_MAX, DATA_RETAINED_BYTES_MAX, JOB_ACTION_ITEMS_MAX,
    JOB_RETAINED_BYTES_MAX, PANEL_COLUMNS_MAX, PANEL_COUNT_MAX, PANEL_FIELD_BYTES_MAX,
    PANEL_GENERATION_BYTES_MAX, PANEL_ROWS_MAX,
};

use crossterm::{
    cursor::SetCursorStyle,
    event::{Event, KeyEvent},
};
use nu_ansi_term::{Color, Style};
use quirl_catalog::{
    Catalog, CommandSpec, CompletionCancellation, CompletionOutcome, CompletionRequest,
    CompletionResponse, COMPLETION_PROTOCOL_VERSION, MAX_COMPLETION_DEADLINE_MS,
    MAX_COMPLETION_QUERY_BYTES, MAX_COMPLETION_RESULTS,
};
use quirl_core::{
    escape_terminal_controls, escape_terminal_line, ErrorCode, ShellError, VersionPolicy,
};
use quirl_lua::QuirlConfig;
use quirl_syntax::{HighlightKind, Mode};
use reedline::{
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    Completer, CursorConfig, DefaultHinter, DefaultValidator, DescriptionMenu, DescriptionMode,
    EditCommand, EditMode, Emacs, FileBackedHistory, Helix, Highlighter, History, HistoryItem,
    HistoryItemId, HistorySessionId, IdeMenu, InputMode, KeyCode, KeyModifiers, MenuBuilder,
    OutputMode, Prompt, PromptEditMode, PromptHistorySearch, PromptViMode, Reedline, ReedlineEvent,
    ReedlineMenu, ReedlineRawEvent, SearchQuery, Span, StyledText, Suggestion, Vi,
};
#[cfg(test)]
use std::sync::mpsc;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet, VecDeque},
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use theme::Theme;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

/// Opaque Reedline host command used to switch Quirl's interactive grammar.
pub const MODE_TOGGLE_HOST_COMMAND: &str = "quirl:mode-toggle";

const HISTORY_CAPACITY: usize = 50_000;
const HISTORY_NEWLINE_ESCAPE: &str = "<\\n>";
const MAX_HISTORY_ENTRY_BYTES: usize = 64 * 1024;
const MAX_HISTORY_ENCODED_ENTRY_BYTES: usize = MAX_HISTORY_ENTRY_BYTES * 4;
const MAX_HISTORY_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const MAX_HISTORY_SCANNED_BYTES: usize = MAX_HISTORY_RETAINED_BYTES * 4 + HISTORY_CAPACITY;
const _: () = assert!(
    MAX_HISTORY_SCANNED_BYTES
        >= MAX_HISTORY_RETAINED_BYTES * (MAX_HISTORY_ENCODED_ENTRY_BYTES / MAX_HISTORY_ENTRY_BYTES)
            + HISTORY_CAPACITY
);
const COMPLETION_MENU: &str = "completion_menu";
const HISTORY_PICKER_MENU: &str = "history_picker_menu";
const FILE_PICKER_MENU: &str = "file_picker_menu";
const ACTION_PICKER_MENU: &str = "action_picker_menu";
const HELP_MENU: &str = "catalog_help_menu";
const PICKER_ITEMS_MAX: usize = 4_096;
const PICKER_RESULTS_MAX: usize = 256;
pub(crate) const PICKER_QUERY_BYTES_MAX: usize = 1_024;
pub(crate) const PICKER_RANKING_TEXT_BYTES_MAX: usize = 2 * 1_024;

#[derive(Debug)]
/// Reedline adapter that keeps all file ingestion behind Quirl's history limits.
///
/// The inner Reedline backend is memory-only because its file synchronization
/// scans and materializes the complete file before enforcing an entry count.
struct BoundedFileHistory {
    inner: FileBackedHistory,
    entries: VecDeque<String>,
    pending: VecDeque<String>,
    retained_bytes: usize,
    path: PathBuf,
}

impl BoundedFileHistory {
    fn with_file(path: PathBuf) -> Result<Self, ShellError> {
        ensure_history_parent(&path).map_err(|error| history_access_error(&path, error))?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .map_err(|error| history_access_error(&path, error))?;
        let entries = read_history(&path)?.into_iter().collect::<VecDeque<_>>();
        let retained_bytes = entries.iter().map(String::len).sum();
        let inner = in_memory_history(&entries).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not initialize bounded history")
                .with_context(error.to_string())
                .with_help("Set QUIRL_HISTORY to a readable and writable file path")
        })?;
        Ok(Self {
            inner,
            entries,
            pending: VecDeque::new(),
            retained_bytes,
            path,
        })
    }

    fn trim_and_rebuild(&mut self) -> reedline::Result<()> {
        let mut trimmed = false;
        while self.entries.len() > HISTORY_CAPACITY
            || self.retained_bytes > MAX_HISTORY_RETAINED_BYTES
        {
            let Some(entry) = self.entries.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.len());
            trimmed = true;
        }
        while self.pending.len() > self.entries.len() {
            self.pending.pop_front();
        }
        if trimmed {
            self.inner = in_memory_history(&self.entries)?;
        }
        Ok(())
    }

    fn refresh_from_disk(&mut self) -> io::Result<()> {
        let history = read_history(&self.path).map_err(io::Error::other)?;
        self.entries = history.into_iter().collect();
        self.retained_bytes = self.entries.iter().map(String::len).sum();
        self.inner = in_memory_history(&self.entries).map_err(io::Error::other)?;
        Ok(())
    }

    fn append_pending(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        ensure_history_parent(&self.path)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        while let Some(entry) = self.pending.front() {
            let mut encoded = entry.replace('\n', HISTORY_NEWLINE_ESCAPE);
            debug_assert!(encoded.len() <= MAX_HISTORY_ENCODED_ENTRY_BYTES);
            encoded.push('\n');
            if let Err(error) = file.write_all(encoded.as_bytes()) {
                // Terminate a possible partial record so a later retry cannot merge with it.
                let _ = file.write_all(b"\n");
                return Err(error);
            }
            self.pending.pop_front();
        }
        file.flush()
    }
}

impl History for BoundedFileHistory {
    fn save(&mut self, item: HistoryItem) -> reedline::Result<HistoryItem> {
        if item.command_line.len() > MAX_HISTORY_ENTRY_BYTES {
            return Ok(HistoryItem { id: None, ..item });
        }
        let saved = self.inner.save(item)?;
        if saved.id.is_some() {
            self.retained_bytes = self.retained_bytes.saturating_add(saved.command_line.len());
            self.entries.push_back(saved.command_line.clone());
            self.pending.push_back(saved.command_line.clone());
            self.trim_and_rebuild()?;
        }
        Ok(saved)
    }

    fn load(&self, id: HistoryItemId) -> reedline::Result<HistoryItem> {
        self.inner.load(id)
    }

    fn count(&self, query: SearchQuery) -> reedline::Result<i64> {
        self.inner.count(query)
    }

    fn search(&self, query: SearchQuery) -> reedline::Result<Vec<HistoryItem>> {
        self.inner.search(query)
    }

    fn update(
        &mut self,
        id: HistoryItemId,
        updater: &dyn Fn(HistoryItem) -> HistoryItem,
    ) -> reedline::Result<()> {
        self.inner.update(id, updater)
    }

    fn clear(&mut self) -> reedline::Result<()> {
        self.inner.clear()?;
        self.entries.clear();
        self.pending.clear();
        self.retained_bytes = 0;
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(reedline::ReedlineError(
                reedline::ReedlineErrorVariants::IOError(error),
            )),
        }
    }

    fn delete(&mut self, id: HistoryItemId) -> reedline::Result<()> {
        self.inner.delete(id)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.append_pending()?;
        self.refresh_from_disk()
    }

    fn session(&self) -> Option<HistorySessionId> {
        self.inner.session()
    }
}

impl Drop for BoundedFileHistory {
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

fn in_memory_history(entries: &VecDeque<String>) -> reedline::Result<FileBackedHistory> {
    let mut history = FileBackedHistory::new(HISTORY_CAPACITY)?;
    for entry in entries {
        history.save(HistoryItem::from_command_line(entry))?;
    }
    Ok(history)
}

fn ensure_history_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Semantic source category for an item passed to [`PickerRanker`].
pub enum PickerItemKind {
    /// Previously accepted command or expression.
    History,
    /// Filesystem regular-file candidate.
    File,
    /// Filesystem directory candidate.
    Directory,
    /// Built-in command or editor action.
    Action,
    /// Semantic command-completion candidate.
    Completion,
    /// Background-job candidate.
    Job,
    /// Structured-data value candidate.
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Terminal-independent value and presentation metadata supplied to a picker.
pub struct PickerItem {
    /// Stable identity used to distinguish equal display labels.
    pub id: String,
    /// Source category used for glyphs and filtering.
    pub kind: PickerItemKind,
    /// Primary single-line text searched and displayed by the picker.
    pub label: String,
    /// Secondary searchable explanation.
    pub description: String,
    /// Optional bounded detail text for a preview pane.
    pub preview: Option<String>,
    /// Exact value returned when the item is accepted.
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Ranked reference into the immutable input slice passed to [`PickerRanker::rank`].
pub struct PickerMatch {
    /// Zero-based index into the original item slice.
    pub index: usize,
    /// Zero-based character positions in the item's label selected for emphasis.
    pub match_indices: Vec<usize>,
}

/// Ranking stays behind this terminal-independent boundary so `quirl-cli` can
/// inject the shared `quirl-picker` engine without inverting the crate graph.
pub trait PickerRanker: Send + Sync {
    /// Return at most `limit` ranked matches for `query`.
    ///
    /// Every returned index must refer to `items`; match positions are character,
    /// not byte, offsets into the corresponding label. Implementations run on UI
    /// paths and must keep work bounded by the supplied slice and result limit.
    fn rank(&self, items: &[PickerItem], query: &str, limit: usize) -> Vec<PickerMatch>;
}

#[derive(Debug, Default)]
struct StablePickerRanker;

impl PickerRanker for StablePickerRanker {
    fn rank(&self, items: &[PickerItem], query: &str, limit: usize) -> Vec<PickerMatch> {
        let query = truncate_utf8_ref(query, PICKER_QUERY_BYTES_MAX).to_lowercase();
        items
            .iter()
            .take(PICKER_ITEMS_MAX)
            .enumerate()
            .filter_map(|(index, item)| {
                let label = truncate_utf8_ref(&item.label, PICKER_RANKING_TEXT_BYTES_MAX);
                let description =
                    truncate_utf8_ref(&item.description, PICKER_RANKING_TEXT_BYTES_MAX);
                let searchable = StableFoldedText::new(label, description);
                let mut search = searchable.value.char_indices();
                let mut matched = Vec::new();
                for wanted in query.chars() {
                    let (byte, _) = search.find(|(_, character)| *character == wanted)?;
                    if let Some(character_index) = searchable.label_index_at(byte) {
                        if matched.last().copied() != Some(character_index) {
                            matched.push(character_index);
                        }
                    }
                }
                Some(PickerMatch {
                    index,
                    match_indices: matched,
                })
            })
            .take(limit)
            .collect()
    }
}

struct StableFoldedText {
    value: String,
    label_index_by_byte: Vec<Option<usize>>,
}

impl StableFoldedText {
    fn new(label: &str, description: &str) -> Self {
        let mut folded = Self {
            value: String::with_capacity(label.len().saturating_add(description.len())),
            label_index_by_byte: Vec::with_capacity(label.len().saturating_add(description.len())),
        };
        for (index, character) in label.chars().enumerate() {
            folded.push(character, Some(index));
        }
        if !description.is_empty() {
            folded.push(' ', None);
            for character in description.chars() {
                folded.push(character, None);
            }
        }
        folded
    }

    fn push(&mut self, character: char, label_index: Option<usize>) {
        for lowercase in character.to_lowercase() {
            self.value.push(lowercase);
            self.label_index_by_byte
                .extend(std::iter::repeat_n(label_index, lowercase.len_utf8()));
        }
    }

    fn label_index_at(&self, byte_index: usize) -> Option<usize> {
        self.label_index_by_byte.get(byte_index).copied().flatten()
    }
}

fn truncate_utf8_ref(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum PickerInvocation {
    None,
    History,
    File,
    Action,
    Help,
}

impl PickerInvocation {
    fn from_state(state: &AtomicU8) -> Self {
        match state.load(Ordering::Relaxed) {
            value if value == Self::History as u8 => Self::History,
            value if value == Self::File as u8 => Self::File,
            value if value == Self::Action as u8 => Self::Action,
            value if value == Self::Help as u8 => Self::Help,
            _ => Self::None,
        }
    }

    fn item_kind(self) -> Option<PickerItemKind> {
        match self {
            Self::None => None,
            Self::History => Some(PickerItemKind::History),
            Self::File => Some(PickerItemKind::File),
            Self::Action => Some(PickerItemKind::Action),
            Self::Help => None,
        }
    }

    fn activate(self, state: &AtomicU8) {
        state.store(self as u8, Ordering::Relaxed);
    }
}

/// Maximum time native context work is allowed to consume on the editor thread.
///
/// Filesystem and Git inspection run on a persistent worker. This budget is still
/// reported with every sample so callers can detect scheduling or lock contention.
pub const PROMPT_FIRST_PAINT_BUDGET: Duration = Duration::from_millis(8);

/// Return the active terminal width without emitting a terminal query.
///
/// Crossterm uses the platform terminal API for this lookup. Callers should
/// retain a conservative fallback for redirected or unusually limited output.
pub fn terminal_width() -> Option<u16> {
    crossterm::terminal::size().ok().map(|(columns, _)| columns)
}

/// Whether conservative UI chrome may use broadly supported Unicode glyphs.
/// Private-use patched-font symbols remain a separate explicit opt-in.
pub fn terminal_supports_unicode() -> bool {
    unicode_is_safe(dumb_terminal(), locale_supports_unicode())
}

const fn unicode_is_safe(dumb: bool, unicode_locale: bool) -> bool {
    !dumb && unicode_locale
}

/// Create a Reedline editor with default configuration and in-memory history.
pub fn editor(catalog: Catalog) -> Reedline {
    editor_with_config(catalog, QuirlConfig::default())
}

/// Create a default Reedline editor with optional extension completions.
///
/// Extension callbacks execute through the completion boundary and must return
/// bounded, terminal-safe suggestions. History remains in memory.
pub fn editor_with_extensions(
    catalog: Catalog,
    extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
) -> Reedline {
    editor_with_extensions_and_config(catalog, extension_completer, QuirlConfig::default())
}

/// Create an editor using the configured keymap, completion menu, and semantic hints.
///
/// `QuirlConfig` is passed by value so a caller can apply a newly loaded configuration
/// atomically when it rebuilds its editor.
pub fn editor_with_config(catalog: Catalog, config: QuirlConfig) -> Reedline {
    editor_with_extensions_and_config(catalog, None, config)
}

/// Like [`editor_with_config`], with completions supplied by Lua extensions as well.
pub fn editor_with_extensions_and_config(
    catalog: Catalog,
    extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
    config: QuirlConfig,
) -> Reedline {
    configured_editor(
        catalog,
        extension_completer,
        config,
        None,
        Vec::new(),
        None,
        Arc::new(StablePickerRanker),
    )
}

/// Create an editor backed by a durable, newline-delimited history file.
///
/// Reopening an editor with the same path reloads the prior entries. Callers that
/// rebuild the editor while it is live should call [`Reedline::sync_history`] first
/// so the replacement observes the newest commands. Reads scan at most about
/// 32 MiB and retain at most 50,000 entries or 8 MiB; malformed and entries over
/// 64 KiB are ignored without making the terminal session unusable.
pub fn editor_with_extensions_config_and_history(
    catalog: Catalog,
    extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
    config: QuirlConfig,
    history_path: PathBuf,
) -> Result<Reedline, ShellError> {
    editor_with_extensions_config_history_and_picker(
        catalog,
        extension_completer,
        config,
        history_path,
        Arc::new(StablePickerRanker),
    )
}

/// Create a configured durable-history editor with an injected picker ranker.
///
/// Reedline retains at most 50,000 history entries or 8 MiB after scanning a
/// roughly 32 MiB tail; each decoded entry is capped at 64 KiB. Picker
/// materialization later caps candidates at 4,096 and results at 256. Returns
/// [`ErrorCode::Io`] when the history backend cannot open or read `history_path`.
/// Callers rebuilding a live editor should synchronize the old backend before
/// replacement.
pub fn editor_with_extensions_config_history_and_picker(
    catalog: Catalog,
    extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
    config: QuirlConfig,
    history_path: PathBuf,
    picker_ranker: Arc<dyn PickerRanker>,
) -> Result<Reedline, ShellError> {
    Theme::from_config(&config, true)?;
    let mut history = BoundedFileHistory::with_file(history_path.clone())?;
    let history_items = history_picker_items(history.entries.make_contiguous());
    Ok(configured_editor(
        catalog,
        extension_completer,
        config,
        Some(history),
        history_items,
        Some(history_path),
        picker_ranker,
    ))
}

fn configured_editor(
    catalog: Catalog,
    extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
    config: QuirlConfig,
    history: Option<BoundedFileHistory>,
    history_items: Vec<PickerItem>,
    history_path: Option<PathBuf>,
    picker_ranker: Arc<dyn PickerRanker>,
) -> Reedline {
    let terminal_styles = terminal_styling_enabled(
        std::io::stdout().is_terminal(),
        env::var_os("NO_COLOR").is_some(),
        dumb_terminal(),
    );
    let theme = Theme::from_config_or_default(&config, terminal_styles);
    let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::None as u8));
    let completer = Box::new(CatalogCompleter::with_extensions_and_picker(
        catalog.clone(),
        extension_completer,
        picker_sources(&catalog, history_items),
        history_path,
        Arc::clone(&picker_invocation),
        picker_ranker,
    ));
    let completion_menu = Box::new(configured_completion_menu(&config));
    let help_menu = Box::new(configured_help_menu());
    let history_picker_menu = Box::new(configured_picker_menu(
        &config,
        HISTORY_PICKER_MENU,
        OutputMode::FullBuffer,
        false,
    ));
    let file_picker_menu = Box::new(configured_picker_menu(
        &config,
        FILE_PICKER_MENU,
        OutputMode::SuggestedSpan,
        true,
    ));
    let action_picker_menu = Box::new(configured_picker_menu(
        &config,
        ACTION_PICKER_MENU,
        OutputMode::FullBuffer,
        false,
    ));
    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_menu(ReedlineMenu::EngineCompleter(help_menu))
        .with_menu(ReedlineMenu::EngineCompleter(history_picker_menu))
        .with_menu(ReedlineMenu::EngineCompleter(file_picker_menu))
        .with_menu(ReedlineMenu::EngineCompleter(action_picker_menu))
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(theme.ansi_hint()),
        ))
        .with_validator(Box::new(DefaultValidator))
        .with_edit_mode(configured_edit_mode_with_picker(
            &config.editor.keymap,
            picker_invocation,
        ))
        .with_ansi_colors(terminal_styles)
        .with_quick_completions(false);
    if let Some(history) = history {
        line_editor = line_editor.with_history(Box::new(history));
    }
    if std::io::stdout().is_terminal() && !dumb_terminal() {
        line_editor = line_editor.with_cursor_config(editor_cursor_config());
    }
    if config.editor.semantic_hints && terminal_styles {
        line_editor =
            line_editor.with_highlighter(Box::new(SemanticHighlighter::new(catalog, theme)));
    }
    line_editor
}

fn editor_cursor_config() -> CursorConfig {
    CursorConfig {
        vi_insert: Some(SetCursorStyle::SteadyBar),
        vi_normal: Some(SetCursorStyle::SteadyBlock),
        emacs: Some(SetCursorStyle::SteadyBar),
    }
}

/// Resolve Quirl's history path from the process environment.
///
/// `QUIRL_HISTORY` wins, followed by `$XDG_STATE_HOME/quirl/history`, then
/// `$HOME/.local/state/quirl/history`.
pub fn history_path() -> Result<PathBuf, ShellError> {
    resolve_history_path(
        env::var_os("QUIRL_HISTORY"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )
    .ok_or_else(|| {
        ShellError::new(
            quirl_core::ErrorCode::Io,
            "could not determine a durable history path",
        )
        .with_help("Set QUIRL_HISTORY to a writable file path")
    })
}

fn resolve_history_path(
    quirl_history: Option<OsString>,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    non_empty_path(quirl_history)
        .or_else(|| non_empty_path(xdg_state_home).map(|path| path.join("quirl/history")))
        .or_else(|| non_empty_path(home).map(|path| path.join(".local/state/quirl/history")))
}

fn non_empty_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn completion_menu_event() -> ReedlineEvent {
    menu_event(COMPLETION_MENU, false)
}

fn menu_event(menu: &str, replace_active: bool) -> ReedlineEvent {
    if replace_active {
        return ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Menu(menu.to_owned()),
            ReedlineEvent::MenuNext,
        ]);
    }
    ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu(menu.to_owned()),
        ReedlineEvent::MenuNext,
    ])
}

fn picker_menu_event(menu: &str, replace_active: bool) -> ReedlineEvent {
    menu_event(menu, replace_active)
}

#[cfg(test)]
fn configured_edit_mode(keymap: &str) -> Box<dyn EditMode> {
    configured_edit_mode_with_picker(
        keymap,
        Arc::new(AtomicU8::new(PickerInvocation::None as u8)),
    )
}

fn configured_edit_mode_with_picker(
    keymap: &str,
    picker_invocation: Arc<AtomicU8>,
) -> Box<dyn EditMode> {
    let (inner, complete_tab, needs_basic_edit_fallback): (Box<dyn EditMode>, bool, bool) =
        match keymap {
            "vim" => {
                let mut insert = default_vi_insert_keybindings();
                let mut normal = default_vi_normal_keybindings();
                insert.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
                normal.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
                (Box::new(Vi::new(insert, normal)), false, false)
            }
            "helix" => (Box::<Helix>::default(), true, true),
            // Config validation rejects other values. Keep this fallback for direct Rust callers.
            "emacs" => {
                let mut keybindings = default_emacs_keybindings();
                keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
                (Box::new(Emacs::new(keybindings)), false, false)
            }
            _ => {
                let mut keybindings = default_emacs_keybindings();
                keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
                (Box::new(Emacs::new(keybindings)), false, false)
            }
        };
    Box::new(QuirlEditMode {
        inner,
        complete_tab,
        needs_basic_edit_fallback,
        picker_invocation,
    })
}

/// Add Quirl-wide shortcuts without replacing Reedline's keymap implementations.
struct QuirlEditMode {
    inner: Box<dyn EditMode>,
    complete_tab: bool,
    /// Reedline 0.49's minimal Helix mode handles character insertion and a
    /// small normal-mode subset, but omits the common EOF and erase keys.
    needs_basic_edit_fallback: bool,
    picker_invocation: Arc<AtomicU8>,
}

impl EditMode for QuirlEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let event: Event = event.into();
        if is_mode_toggle(&event) {
            return ReedlineEvent::ExecuteHostCommand(MODE_TOGGLE_HOST_COMMAND.to_owned());
        }
        if is_context_help(&event) {
            let replace_active =
                PickerInvocation::from_state(&self.picker_invocation) != PickerInvocation::Help;
            PickerInvocation::Help.activate(&self.picker_invocation);
            return menu_event(HELP_MENU, replace_active);
        }
        // Ctrl-D is Reedline's context-sensitive EOF action: it exits an empty
        // editor and deletes at the cursor otherwise. Handle it before Helix's
        // character fallback can turn Ctrl-D into a literal `d`.
        if is_ctrl_d(&event) {
            return ReedlineEvent::CtrlD;
        }
        if self.needs_basic_edit_fallback {
            if let Some(event) = basic_edit_fallback(&event) {
                return event;
            }
        }
        if is_history_search(&event) {
            let replace_active =
                PickerInvocation::from_state(&self.picker_invocation) != PickerInvocation::History;
            PickerInvocation::History.activate(&self.picker_invocation);
            return picker_menu_event(HISTORY_PICKER_MENU, replace_active);
        }
        if is_file_picker(&event) {
            let replace_active =
                PickerInvocation::from_state(&self.picker_invocation) != PickerInvocation::File;
            PickerInvocation::File.activate(&self.picker_invocation);
            return picker_menu_event(FILE_PICKER_MENU, replace_active);
        }
        if is_action_picker(&event) {
            let replace_active =
                PickerInvocation::from_state(&self.picker_invocation) != PickerInvocation::Action;
            PickerInvocation::Action.activate(&self.picker_invocation);
            return picker_menu_event(ACTION_PICKER_MENU, replace_active);
        }
        if matches!(
            event,
            Event::Key(KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            })
        ) {
            PickerInvocation::None.activate(&self.picker_invocation);
            return ReedlineEvent::Enter;
        }
        if PickerInvocation::from_state(&self.picker_invocation) != PickerInvocation::None
            && ends_picker_session(&event)
        {
            PickerInvocation::None.activate(&self.picker_invocation);
        }
        if matches!(
            event,
            Event::Key(KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            })
        ) {
            if PickerInvocation::from_state(&self.picker_invocation) == PickerInvocation::Help {
                return ReedlineEvent::MenuNext;
            }
            let replace_active =
                PickerInvocation::from_state(&self.picker_invocation) != PickerInvocation::None;
            PickerInvocation::None.activate(&self.picker_invocation);
            if self.complete_tab || replace_active {
                return menu_event(COMPLETION_MENU, replace_active);
            }
        }
        let Ok(event) = ReedlineRawEvent::try_from(event) else {
            // Reedline intentionally rejects key-release events; they have no editor action.
            return ReedlineEvent::None;
        };
        self.inner.parse_event(event)
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

fn is_ctrl_d(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

fn is_context_help(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::F(1),
            modifiers: KeyModifiers::NONE,
            ..
        })
    )
}

fn basic_edit_fallback(event: &Event) -> Option<ReedlineEvent> {
    let command = match event {
        Event::Key(KeyEvent {
            code: KeyCode::Backspace,
            modifiers: KeyModifiers::NONE,
            ..
        })
        | Event::Key(KeyEvent {
            code: KeyCode::Char('h'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => EditCommand::Backspace,
        Event::Key(KeyEvent {
            code: KeyCode::Delete,
            modifiers: KeyModifiers::NONE,
            ..
        }) => EditCommand::Delete,
        _ => return None,
    };
    Some(ReedlineEvent::Edit(vec![command]))
}

fn is_mode_toggle(event: &Event) -> bool {
    match event {
        Event::Key(KeyEvent {
            code: KeyCode::Char('m'),
            modifiers: KeyModifiers::ALT,
            ..
        }) => true,
        // Ctrl-Space emits NUL in legacy terminals. Keep both decoded forms as
        // compatibility aliases, but do not advertise a chord commonly owned
        // by terminal multiplexers.
        Event::Key(KeyEvent {
            code: KeyCode::Char(' '),
            modifiers: KeyModifiers::CONTROL,
            ..
        }) => true,
        Event::Key(KeyEvent {
            code: KeyCode::Char('\0'),
            modifiers,
            ..
        }) => matches!(*modifiers, KeyModifiers::NONE | KeyModifiers::CONTROL),
        _ => false,
    }
}

fn is_history_search(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('r'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

fn is_file_picker(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

fn is_action_picker(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::CONTROL,
            ..
        })
    )
}

fn ends_picker_session(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Esc | KeyCode::Enter,
            ..
        })
    ) || matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL)
    )
}

fn configured_completion_menu(config: &QuirlConfig) -> IdeMenu {
    configured_menu(config, COMPLETION_MENU)
}

fn configured_help_menu() -> DescriptionMenu {
    DescriptionMenu::default()
        .with_name(HELP_MENU)
        .with_input_mode(InputMode::FullBuffer)
        .with_output_mode(OutputMode::SuggestedSpan)
        .with_columns(2)
        .with_selection_rows(5)
        .with_description_rows(12)
}

fn configured_picker_menu(
    config: &QuirlConfig,
    name: &str,
    output_mode: OutputMode,
    full_buffer: bool,
) -> IdeMenu {
    let menu = configured_menu(config, name).with_output_mode(output_mode);
    if full_buffer {
        menu.with_input_mode(InputMode::FullBuffer)
    } else {
        menu
    }
}

fn configured_menu(config: &QuirlConfig, name: &str) -> IdeMenu {
    let menu = IdeMenu::default()
        .with_name(name)
        .with_default_border()
        .with_padding(1)
        .with_min_completion_width(22)
        .with_max_completion_width(48)
        .with_min_description_width(24)
        .with_max_description_width(72)
        .with_max_description_height(8)
        .with_description_offset(2)
        .with_correct_cursor_pos(true);
    let menu = match config.picker.layout.as_str() {
        // Reedline's IDE menu is always anchored below the input. A bounded height is
        // the closest supported equivalent to a bottom picker; the default adapts to
        // the remaining terminal space, and `full` removes that extra cap.
        "bottom" => menu.with_max_completion_height(10),
        "full" | "adaptive" => menu,
        _ => menu,
    };
    if config.picker.preview {
        menu.with_description_mode(DescriptionMode::PreferRight)
    } else {
        // IdeMenu has no preview on/off switch. Zero-sized description bounds suppress
        // its detail pane while retaining the IDE completion layout.
        menu.with_min_description_width(0)
            .with_max_description_width(0)
            .with_max_description_height(0)
    }
}

/// Native prompt values collected without running a shell command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NativePromptContext {
    /// Display form of the working directory captured for this sample.
    pub directory: String,
    /// Current Git branch, detached identifier, or `None` outside a discovered repository.
    pub git_branch: Option<String>,
    /// Bounded native state label such as `dirty`, `merging`, or `rebasing`.
    pub git_state: Option<String>,
}

/// Instrumentation for one non-blocking native prompt context lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptTimingSample {
    /// Time spent obtaining the immediate cached/fallback response.
    pub elapsed: Duration,
    /// Caller-configured first-paint latency budget used for instrumentation.
    pub budget: Duration,
    /// Whether the returned context came from a completed cache entry.
    pub cache_hit: bool,
    /// The returned value was usable but a newer value is being collected.
    pub stale: bool,
    /// Whether this call queued a background validation or refresh.
    pub refresh_started: bool,
}

impl PromptTimingSample {
    /// Whether immediate lookup latency did not exceed [`Self::budget`].
    pub fn within_budget(self) -> bool {
        self.elapsed <= self.budget
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Prompt context paired with instrumentation for the non-blocking lookup.
pub struct PromptContextSample {
    /// Immediately usable cached or fallback prompt values.
    pub context: NativePromptContext,
    /// Lookup latency and cache/refresh disposition.
    pub timing: PromptTimingSample,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    modified_nanos: Option<u128>,
    len: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct WorktreeStamp {
    newest_modified_nanos: Option<u128>,
    files: u64,
    total_len: u64,
    truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PromptDependencies {
    git_dir: Option<PathBuf>,
    head: Option<String>,
    head_ref: Option<FileStamp>,
    packed_refs: Option<FileStamp>,
    index: Option<FileStamp>,
    merge_head: Option<FileStamp>,
    rebase_merge: bool,
    rebase_apply: bool,
    worktree: WorktreeStamp,
}

#[derive(Clone, Debug)]
struct PromptCacheEntry {
    context: NativePromptContext,
    dependencies: PromptDependencies,
}

#[derive(Default)]
struct PromptSchedulerState {
    entries: HashMap<PathBuf, PromptCacheEntry>,
    entry_recency: VecDeque<PathBuf>,
    active: Option<PathBuf>,
    pending: Option<RefreshRequest>,
    shutdown: bool,
    refresh_generation: u64,
}

struct PromptSchedulerShared {
    state: Mutex<PromptSchedulerState>,
    request_ready: Condvar,
    refreshed: Condvar,
}

struct RefreshRequest {
    cwd: PathBuf,
    previous: Option<PromptCacheEntry>,
}

type PromptContextLoader =
    dyn Fn(PathBuf, Option<PromptCacheEntry>) -> PromptCacheEntry + Send + Sync + 'static;

/// A stale-while-refresh cache for native prompt context.
///
/// `sample` never waits for filesystem or Git work. On a cold lookup it returns the
/// directory immediately; on later lookups it returns the last completed snapshot
/// while one persistent worker validates the cwd and `.git` dependencies.
pub struct PromptContextScheduler {
    shared: Arc<PromptSchedulerShared>,
    worker: Option<JoinHandle<()>>,
    first_paint_budget: Duration,
}

impl Default for PromptContextScheduler {
    fn default() -> Self {
        Self::new(PROMPT_FIRST_PAINT_BUDGET)
    }
}

impl PromptContextScheduler {
    /// Start a stale-while-refresh scheduler with a reporting budget.
    ///
    /// The budget measures only immediate lookup latency; background filesystem
    /// work does not run on the caller. Worker creation is best-effort: if the
    /// thread cannot start, samples still return directory fallbacks without
    /// scheduling refreshes. The cache retains at most 64 working directories,
    /// and each Git worktree scan inspects at most 4,096 entries.
    pub fn new(first_paint_budget: Duration) -> Self {
        Self::with_context_loader(first_paint_budget, Arc::new(load_prompt_context))
    }

    fn with_context_loader(first_paint_budget: Duration, loader: Arc<PromptContextLoader>) -> Self {
        // Failure model: cwd changes can outpace filesystem scans, and a read
        // error can drop the scheduler while the loader is still running. One
        // replaceable request bounds queued paths, the cache has a fixed LRU
        // cardinality, and shutdown never waits for loader completion.
        let shared = Arc::new(PromptSchedulerShared {
            state: Mutex::new(PromptSchedulerState::default()),
            request_ready: Condvar::new(),
            refreshed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("quirl-prompt-context".to_owned())
            .spawn(move || loop {
                let request = {
                    let mut state = lock_recover(&worker_shared.state);
                    while state.pending.is_none() && !state.shutdown {
                        state = match worker_shared.request_ready.wait(state) {
                            Ok(state) => state,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                    }
                    if state.shutdown {
                        return;
                    }
                    let request = state.pending.take();
                    state.active = request.as_ref().map(|request| request.cwd.clone());
                    request
                };
                let Some(request) = request else {
                    continue;
                };
                let entry = loader(request.cwd.clone(), request.previous);
                let mut state = lock_recover(&worker_shared.state);
                if !state.shutdown {
                    insert_prompt_cache_entry(&mut state, request.cwd, entry);
                }
                state.active = None;
                state.refresh_generation = state.refresh_generation.wrapping_add(1);
                worker_shared.refreshed.notify_all();
            })
            .ok();
        Self {
            shared,
            worker,
            first_paint_budget,
        }
    }

    /// Sample the process working directory without waiting for filesystem analysis.
    ///
    /// If the host cannot resolve the current directory, `/` is used as the
    /// conservative display fallback.
    pub fn sample_current_dir(&self) -> PromptContextSample {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        self.sample(&cwd)
    }

    /// Return cached/fallback context for `cwd` and schedule one bounded refresh.
    ///
    /// A cold call returns the directory immediately. A cache hit may be marked
    /// stale while a newer snapshot is collected. At most one pending path is
    /// retained; a newer request replaces older queued work, while active work
    /// completes in the persistent worker. This method never waits for Git or
    /// filesystem I/O.
    pub fn sample(&self, cwd: &Path) -> PromptContextSample {
        let started = Instant::now();
        let cwd = cwd.to_path_buf();
        let directory = display_directory(&cwd);
        let (context, cache_hit, refresh_started) = {
            let mut state = lock_recover(&self.shared.state);
            let cached = state.entries.get(&cwd).cloned();
            let cache_hit = cached.is_some();
            if cache_hit {
                touch_prompt_cache_entry(&mut state, &cwd);
            }
            let context = cached
                .as_ref()
                .map(|entry| entry.context.clone())
                .unwrap_or(NativePromptContext {
                    directory,
                    ..NativePromptContext::default()
                });
            let already_scheduled = state.active.as_ref() == Some(&cwd)
                || state.pending.as_ref().map(|request| &request.cwd) == Some(&cwd);
            let refresh_started = self.worker.is_some() && !already_scheduled && !state.shutdown;
            if refresh_started {
                state.pending = Some(RefreshRequest {
                    cwd: cwd.clone(),
                    previous: cached,
                });
                self.shared.request_ready.notify_one();
            }
            (context, cache_hit, refresh_started)
        };

        PromptContextSample {
            context,
            timing: PromptTimingSample {
                elapsed: started.elapsed(),
                budget: self.first_paint_budget,
                cache_hit,
                stale: cache_hit && refresh_started,
                refresh_started,
            },
        }
    }

    #[cfg(test)]
    fn wait_until_idle(&self, timeout: Duration) -> bool {
        let state = lock_recover(&self.shared.state);
        if state.active.is_none() && state.pending.is_none() {
            return true;
        }
        let waited = self
            .shared
            .refreshed
            .wait_timeout_while(state, timeout, |state| {
                state.active.is_some() || state.pending.is_some()
            });
        match waited {
            Ok((state, _)) => state.active.is_none() && state.pending.is_none(),
            Err(poisoned) => {
                let state = poisoned.into_inner().0;
                state.active.is_none() && state.pending.is_none()
            }
        }
    }
}

impl Drop for PromptContextScheduler {
    fn drop(&mut self) {
        let mut state = lock_recover(&self.shared.state);
        state.shutdown = true;
        state.pending = None;
        self.shared.request_ready.notify_one();
        self.shared.refreshed.notify_all();
        drop(state);
        // Native loading is bounded by the ancestor walk and the worktree
        // entry limit below. Arc-owned state makes detaching safe, while not
        // joining guarantees terminal restoration cannot wait on filesystem I/O.
        self.worker.take();
    }
}

const MAX_PROMPT_CACHE_ENTRIES: usize = 64;

fn touch_prompt_cache_entry(state: &mut PromptSchedulerState, cwd: &Path) {
    state.entry_recency.retain(|entry| entry != cwd);
    state.entry_recency.push_back(cwd.to_path_buf());
}

fn insert_prompt_cache_entry(
    state: &mut PromptSchedulerState,
    cwd: PathBuf,
    entry: PromptCacheEntry,
) {
    touch_prompt_cache_entry(state, &cwd);
    state.entries.insert(cwd, entry);
    while state.entries.len() > MAX_PROMPT_CACHE_ENTRIES {
        let Some(expired) = state.entry_recency.pop_front() else {
            debug_assert!(false, "prompt cache recency must track every entry");
            state.entries.clear();
            return;
        };
        state.entries.remove(&expired);
    }
    debug_assert_eq!(state.entries.len(), state.entry_recency.len());
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

const MAX_PROMPT_WORKTREE_ENTRIES: usize = 4_096;

fn load_prompt_context(cwd: PathBuf, previous: Option<PromptCacheEntry>) -> PromptCacheEntry {
    let git_dir = cwd.ancestors().find_map(resolve_git_dir);
    let head = git_dir
        .as_ref()
        .and_then(|git_dir| fs::read_to_string(git_dir.join("HEAD")).ok())
        .map(|head| head.trim().to_owned());
    let worktree = scan_worktree(&cwd);
    let dependencies = PromptDependencies {
        head_ref: git_dir.as_ref().and_then(|git_dir| {
            head.as_deref()
                .and_then(|head| head.strip_prefix("ref: "))
                .and_then(|head_ref| file_stamp(&git_dir.join(head_ref)))
        }),
        packed_refs: git_dir
            .as_ref()
            .and_then(|git_dir| file_stamp(&git_dir.join("packed-refs"))),
        index: git_dir
            .as_ref()
            .and_then(|git_dir| file_stamp(&git_dir.join("index"))),
        merge_head: git_dir
            .as_ref()
            .and_then(|git_dir| file_stamp(&git_dir.join("MERGE_HEAD"))),
        rebase_merge: git_dir
            .as_ref()
            .is_some_and(|git_dir| git_dir.join("rebase-merge").is_dir()),
        rebase_apply: git_dir
            .as_ref()
            .is_some_and(|git_dir| git_dir.join("rebase-apply").is_dir()),
        git_dir: git_dir.clone(),
        head: head.clone(),
        worktree,
    };

    if let Some(mut previous) = previous.filter(|entry| entry.dependencies == dependencies) {
        previous.context.directory = display_directory(&cwd);
        return previous;
    }

    let git_branch = head.as_deref().map(|head| {
        head.strip_prefix("ref: refs/heads/")
            .map(str::to_owned)
            .unwrap_or_else(|| head.chars().take(8).collect())
    });
    let git_state = git_dir
        .as_ref()
        .and_then(|_| render_native_git_state(&dependencies));
    PromptCacheEntry {
        context: NativePromptContext {
            directory: display_directory(&cwd),
            git_branch,
            git_state,
        },
        dependencies,
    }
}

fn display_directory(cwd: &Path) -> String {
    let home = env::var_os("HOME").map(PathBuf::from);
    display_directory_with_home(cwd, home.as_deref())
}

fn display_directory_with_home(cwd: &Path, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return directory_leaf(cwd);
    };
    let Ok(relative) = cwd.strip_prefix(home) else {
        return directory_leaf(cwd);
    };
    let components = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if components.is_empty() {
        return "~".to_owned();
    }
    let last_index = components.len().saturating_sub(1);
    let compact = components
        .into_iter()
        .enumerate()
        .map(|(index, component)| {
            if index == last_index {
                component
            } else {
                component
                    .chars()
                    .next()
                    .map_or_else(String::new, |character| character.to_string())
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    format!("~/{compact}")
}

fn directory_leaf(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "/".to_owned())
}

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    Some(FileStamp {
        modified_nanos: metadata.modified().ok().and_then(system_time_nanos),
        len: metadata.len(),
    })
}

fn system_time_nanos(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|time| time.as_nanos())
}

fn scan_worktree(root: &Path) -> WorktreeStamp {
    let mut stamp = WorktreeStamp::default();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if stamp.files as usize >= MAX_PROMPT_WORKTREE_ENTRIES {
                stamp.truncated = true;
                return stamp;
            }
            if entry.file_name() == ".git" {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                pending.push(entry.path());
            } else {
                stamp.files = stamp.files.saturating_add(1);
                stamp.total_len = stamp.total_len.saturating_add(metadata.len());
                if let Some(modified) = metadata.modified().ok().and_then(system_time_nanos) {
                    stamp.newest_modified_nanos = Some(
                        stamp
                            .newest_modified_nanos
                            .map_or(modified, |newest| newest.max(modified)),
                    );
                }
            }
        }
    }
    stamp
}

fn render_native_git_state(dependencies: &PromptDependencies) -> Option<String> {
    if dependencies.rebase_merge || dependencies.rebase_apply {
        return Some("rebasing".to_owned());
    }
    if dependencies.merge_head.is_some() {
        return Some("merging".to_owned());
    }
    let index_modified = dependencies.index.as_ref()?.modified_nanos?;
    dependencies
        .worktree
        .newest_modified_nanos
        .filter(|modified| *modified > index_modified)
        .map(|_| "dirty".to_owned())
}

#[derive(Clone)]
/// Reedline and rich-surface prompt assembled from sanitized native and extension data.
///
/// Terminal-derived text is escaped before storage. Symbol selection degrades
/// to plain ASCII for dumb terminals or non-Unicode locales unless an explicit
/// supported profile is configured, and styling respects `NO_COLOR`.
pub struct QuirlPrompt {
    mode: Mode,
    cwd: String,
    git_branch: Option<String>,
    git_state: Option<String>,
    status: Option<i32>,
    jobs: usize,
    duration: Option<Duration>,
    extension_segments: Vec<String>,
    configured_left: Option<Vec<String>>,
    configured_right: Vec<String>,
    configured_symbols: String,
    named_extension_segments: HashMap<String, String>,
    styled: bool,
    theme: Theme,
}

/// Prompt glyphs are intentionally separate from color. Terminal-derived text
/// is escaped before Quirl adds its own fixed styles, and `NO_COLOR` sessions
/// remain control-sequence-free without losing useful context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PromptSymbols {
    Plain,
    Unicode,
    NerdFont,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceSymbols {
    Plain,
    Unicode,
    NerdFont,
}

impl SurfaceSymbols {
    pub(crate) const fn uses_unicode(self) -> bool {
        !matches!(self, Self::Plain)
    }

    pub(crate) const fn input_indicator(self, mode: Mode) -> &'static str {
        match (self, mode) {
            (Self::Plain, Mode::Command) => "> ",
            (Self::Plain, Mode::Data) => "D ",
            (Self::Plain, Mode::Natural) => "AI ",
            (Self::Unicode, Mode::Command) => "❯ ",
            (Self::Unicode, Mode::Data) => "◆ ",
            (Self::Unicode, Mode::Natural) => "✦ ",
            // These private-use glyphs are restricted to the explicit patched-font profile.
            (Self::NerdFont, Mode::Command) => "\u{f105} ",
            (Self::NerdFont, Mode::Data) => "\u{f1b2} ",
            (Self::NerdFont, Mode::Natural) => "\u{f544} ",
        }
    }

    pub(crate) const fn multiline_indicator(self) -> &'static str {
        match self {
            Self::Plain => ". ",
            Self::Unicode => "∙ ",
            Self::NerdFont => "\u{f105} ",
        }
    }
}

impl PromptSymbols {
    fn resolve(requested: &str, dumb: bool, unicode_locale: bool) -> Self {
        if dumb {
            return Self::Plain;
        }
        match requested {
            "plain" => Self::Plain,
            "unicode" => Self::Unicode,
            "nerd_font" => Self::NerdFont,
            _ if unicode_locale => Self::Unicode,
            _ => Self::Plain,
        }
    }

    const fn separator(self) -> &'static str {
        match self {
            Self::Plain => " | ",
            Self::Unicode => " · ",
            // U+E0B1 is the slim Powerline separator. It is restricted to the
            // explicit Nerd Font profile so auto mode never displays tofu.
            Self::NerdFont => " \u{e0b1} ",
        }
    }

    fn directory(self, value: &str) -> String {
        match self {
            Self::NerdFont => format!("\u{f07c} {value}"),
            Self::Plain | Self::Unicode => value.to_owned(),
        }
    }

    fn git_branch(self, value: &str) -> String {
        match self {
            Self::NerdFont => format!("on \u{e0a0} {value}"),
            Self::Plain | Self::Unicode => format!("on {value}"),
        }
    }

    fn git_state(self, value: &str) -> String {
        match self {
            Self::NerdFont => format!("\u{f044} {value}"),
            Self::Unicode => format!("\u{2261} {value}"),
            Self::Plain => value.to_owned(),
        }
    }

    fn status(self, value: i32) -> String {
        match self {
            Self::NerdFont => format!("\u{f057} {value}"),
            Self::Unicode => format!("\u{2717} {value}"),
            Self::Plain => format!("status:{value}"),
        }
    }

    fn jobs(self, value: usize) -> String {
        match self {
            Self::NerdFont => format!("\u{f085} {value}"),
            Self::Plain | Self::Unicode => format!("jobs:{value}"),
        }
    }

    fn duration(self, value: Duration) -> String {
        let value = format_duration(value);
        match self {
            Self::NerdFont => format!("\u{f017} {value}"),
            Self::Plain | Self::Unicode => value,
        }
    }
}

impl QuirlPrompt {
    /// Construct a prompt for `mode` using the current directory and safe defaults.
    ///
    /// Failure to read the working directory falls back to `/`. Git, status,
    /// duration, jobs, and extension segments remain absent until supplied.
    pub fn new(mode: Mode) -> Self {
        let cwd_path = env::current_dir().ok();
        let cwd = cwd_path
            .as_deref()
            .map(display_directory)
            .map(|directory| safe_prompt_text(&directory))
            .unwrap_or_else(|| "/".to_owned());
        let styled = terminal_styling_enabled(
            std::io::stdout().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
            dumb_terminal(),
        );
        Self {
            mode,
            cwd,
            git_branch: None,
            git_state: None,
            status: None,
            jobs: 0,
            duration: None,
            extension_segments: Vec::new(),
            configured_left: None,
            configured_right: Vec::new(),
            configured_symbols: "auto".to_owned(),
            named_extension_segments: HashMap::new(),
            styled,
            theme: Theme::new(styled),
        }
    }

    /// Create a prompt whose visible segments and order are selected by Lua config.
    ///
    /// Known native segments are `directory`, `git_branch`, and `mode`; `status` and
    /// `duration` are available after their builder methods receive session values.
    /// `jobs` and `git_state` are skipped until the interactive host provides them.
    pub fn with_config(mode: Mode, config: &QuirlConfig) -> Self {
        let mut prompt = Self::new(mode);
        prompt.configured_left = Some(config.prompt.left.clone());
        prompt.configured_right = config.prompt.right.clone();
        prompt.configured_symbols.clone_from(&config.prompt.symbols);
        prompt.theme = Theme::from_config_or_default(config, prompt.styled);
        prompt
    }

    /// Return the grammar mode currently rendered by this prompt.
    ///
    /// Rich editing may change the mode without releasing terminal ownership;
    /// the composition root reads this value when the edit session eventually
    /// returns so execution uses the same mode the user last saw.
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) fn toggle_mode(&mut self) {
        self.mode = self.mode.toggled();
    }

    /// Append legacy positional extension segments after neutralizing terminal controls.
    ///
    /// Prefer [`Self::with_named_extension_segments`] when configuration must
    /// place individual contributions deterministically.
    pub fn with_extension_segments(mut self, segments: Vec<String>) -> Self {
        self.extension_segments = segments
            .into_iter()
            .map(|value| safe_prompt_text(&value))
            .collect();
        self
    }

    /// Apply a snapshot returned by [`PromptContextScheduler`].
    pub fn with_native_context(mut self, context: NativePromptContext) -> Self {
        self.cwd = safe_prompt_text(&context.directory);
        self.git_branch = context.git_branch.map(|branch| safe_prompt_text(&branch));
        self.git_state = context.git_state.map(|state| safe_prompt_text(&state));
        self
    }

    /// Set the exit status that can be rendered by the configured `status` segment.
    pub fn with_status(mut self, status: i32) -> Self {
        self.status = Some(status);
        self
    }

    /// Set the number of active background jobs for the configured `jobs` segment.
    pub fn with_jobs(mut self, jobs: usize) -> Self {
        self.jobs = jobs;
        self
    }

    /// Set the duration of the most recently evaluated command or expression.
    pub fn with_duration(mut self, duration: Duration) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Supply rendered plugin segments by registration name so the prompt config can
    /// position them on either side of the input.
    pub fn with_named_extension_segments(
        mut self,
        segments: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.named_extension_segments = segments
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(name, value)| (name, safe_prompt_text(&value)))
            .collect();
        self
    }

    fn render_segments(&self, requested: &[String]) -> String {
        let symbols = self.symbols();
        let parts = requested
            .iter()
            .filter_map(|name| self.render_segment(name, symbols))
            .collect::<Vec<_>>();
        join_prompt_parts(&parts, symbols)
    }

    fn render_left_segments(&self, requested: &[String]) -> String {
        let symbols = self.symbols();
        let parts = requested
            .iter()
            .filter_map(|name| {
                self.render_segment(name, symbols)
                    .map(|value| self.style_left_segment(name, value))
            })
            .collect::<Vec<_>>();
        join_prompt_parts(&parts, symbols)
    }

    fn render_segment(&self, name: &str, symbols: PromptSymbols) -> Option<String> {
        match name {
            "directory" => Some(symbols.directory(&self.cwd)),
            "mode" => Some(self.mode.to_string()),
            "git_branch" => self
                .git_branch
                .as_ref()
                .map(|branch| symbols.git_branch(branch)),
            "status" => self
                .status
                .filter(|status| *status != 0)
                .map(|status| symbols.status(status)),
            "duration" => self.duration.map(|duration| symbols.duration(duration)),
            "jobs" => (self.jobs > 0).then(|| symbols.jobs(self.jobs)),
            "git_state" => self
                .git_state
                .as_ref()
                .map(|state| symbols.git_state(state)),
            _ => self.named_extension_segments.get(name).cloned(),
        }
    }

    fn style_left_segment(&self, name: &str, value: String) -> String {
        if !self.styled {
            return value;
        }
        self.theme
            .ansi_prompt_segment(name)
            .paint(value)
            .to_string()
    }

    fn symbols(&self) -> PromptSymbols {
        PromptSymbols::resolve(
            &self.configured_symbols,
            dumb_terminal(),
            locale_supports_unicode(),
        )
    }

    pub(crate) fn surface_symbols(&self) -> SurfaceSymbols {
        match self.symbols() {
            PromptSymbols::Plain => SurfaceSymbols::Plain,
            PromptSymbols::Unicode => SurfaceSymbols::Unicode,
            PromptSymbols::NerdFont => SurfaceSymbols::NerdFont,
        }
    }

    fn render_right_for(&self, width: u16, interactive: bool, minimal: bool) -> String {
        let configured = self.render_segments(&self.configured_right);
        if !interactive || minimal {
            return configured;
        }
        truncate_prompt_width(&configured, usize::from(width))
    }

    fn surface_context_left(&self) -> String {
        self.configured_left.as_deref().map_or_else(
            || self.cwd.clone(),
            |segments| self.render_segments(segments),
        )
    }

    fn surface_context_right(&self) -> String {
        self.render_segments(&self.configured_right)
    }
}

fn truncate_prompt_width(value: &str, width: usize) -> String {
    value
        .chars()
        .scan(0_usize, |used, character| {
            let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            if used.saturating_add(character_width) > width {
                return None;
            }
            *used = used.saturating_add(character_width);
            Some(character)
        })
        .collect()
}

fn safe_prompt_text(value: &str) -> String {
    escape_terminal_line(value)
}

fn join_prompt_parts(parts: &[String], symbols: PromptSymbols) -> String {
    parts.join(symbols.separator())
}

fn dumb_terminal() -> bool {
    env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"))
}

fn locale_supports_unicode() -> bool {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| env::var_os(name).filter(|value| !value.is_empty()));
    locale_value_supports_unicode(locale.as_deref())
}

fn locale_value_supports_unicode(locale: Option<&std::ffi::OsStr>) -> bool {
    locale.is_some_and(locale_name_supports_unicode)
}

fn locale_name_supports_unicode(locale: &std::ffi::OsStr) -> bool {
    let locale = locale.to_string_lossy().to_ascii_lowercase();
    locale.contains("utf-8") || locale.contains("utf8")
}

fn terminal_styling_enabled(terminal: bool, no_color_is_set: bool, dumb: bool) -> bool {
    terminal && !no_color_is_set && !dumb
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() > 0 {
        format!("{:.1}s", duration.as_secs_f64())
    } else if duration.as_millis() > 0 {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{}µs", duration.as_micros())
    }
}

fn resolve_git_dir(directory: &Path) -> Option<PathBuf> {
    let marker = directory.join(".git");
    if marker.is_dir() {
        return Some(marker);
    }
    let contents = fs::read_to_string(marker).ok()?;
    let path = contents.trim().strip_prefix("gitdir:")?.trim();
    let path = PathBuf::from(path);
    Some(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

impl Prompt for QuirlPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        if let Some(segments) = &self.configured_left {
            let rendered = self.render_left_segments(segments);
            return Cow::Owned(if rendered.is_empty() {
                String::new()
            } else {
                format!("{rendered}\n")
            });
        }
        let mut parts = vec![self.cwd.clone(), self.mode.to_string()];
        parts.extend(self.extension_segments.iter().cloned());
        Cow::Owned(format!("{}\n", join_prompt_parts(&parts, self.symbols())))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(self.render_right_for(
            terminal_width().unwrap_or(80),
            std::io::stdout().is_terminal(),
            dumb_terminal(),
        ))
    }

    fn render_prompt_indicator(&self, prompt_mode: PromptEditMode) -> Cow<'_, str> {
        let edit_mode = match prompt_mode {
            PromptEditMode::Vi(PromptViMode::Normal) => Some("normal"),
            PromptEditMode::Vi(PromptViMode::Insert) => Some("insert"),
            PromptEditMode::Vi(PromptViMode::Visual) => Some("visual"),
            PromptEditMode::Custom(_) => Some("edit"),
            PromptEditMode::Emacs | PromptEditMode::Default => None,
        };
        let indicator = if self.symbols() == PromptSymbols::Plain {
            match self.mode {
                Mode::Command => "command >",
                Mode::Data => "data >",
                Mode::Natural => "natural >",
            }
        } else {
            match self.mode {
                Mode::Command => "command ❯",
                Mode::Data => "data ◆",
                Mode::Natural => "natural ✦",
            }
        };
        Cow::Owned(match edit_mode {
            Some(edit_mode) => format!("{edit_mode} {indicator} "),
            None => format!("{indicator} "),
        })
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(if self.symbols() == PromptSymbols::Plain {
            "... "
        } else {
            "  · "
        })
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let indicator = if self.symbols() == PromptSymbols::Plain {
            match self.mode {
                Mode::Command => ">",
                Mode::Data => "data>",
                Mode::Natural => "natural>",
            }
        } else {
            self.mode.prompt()
        };
        Cow::Owned(format!(
            "search `{}` {indicator} ",
            bounded_terminal_line(&history_search.term)
        ))
    }

    fn get_prompt_right_color(&self) -> reedline::Color {
        self.theme.prompt_right_color()
    }

    fn get_indicator_color(&self) -> reedline::Color {
        self.theme.prompt_accent_color(self.mode)
    }

    fn right_prompt_on_last_line(&self) -> bool {
        true
    }
}

struct HistoryPickerCache {
    len: u64,
    modified: Option<SystemTime>,
    items: Vec<PickerItem>,
}

/// Reedline adapter for the semantic catalog, optional extensions, and picker sources.
///
/// Catalog facts remain authoritative for replacement spans and descriptions.
/// Picker candidate sets are capped before ranking; extension providers are
/// responsible for returning bounded suggestions with valid UTF-8 byte spans.
pub struct CatalogCompleter {
    catalog: Catalog,
    extensions: Option<Box<dyn ExtensionCompleter + Send>>,
    picker_items: Vec<PickerItem>,
    history_path: Option<PathBuf>,
    history_cache: Option<HistoryPickerCache>,
    picker_invocation: Arc<AtomicU8>,
    picker_ranker: Arc<dyn PickerRanker>,
}

impl CatalogCompleter {
    /// Construct a catalog-only completer with the built-in stable picker ranker.
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            extensions: None,
            picker_items: Vec::new(),
            history_path: None,
            history_cache: None,
            picker_invocation: Arc::new(AtomicU8::new(PickerInvocation::None as u8)),
            picker_ranker: Arc::new(StablePickerRanker),
        }
    }

    /// Construct a completer that appends optional extension suggestions.
    ///
    /// Catalog and extension values are deduplicated by replacement value, with
    /// the catalog result retaining precedence.
    pub fn with_extensions(
        catalog: Catalog,
        extensions: Option<Box<dyn ExtensionCompleter + Send>>,
    ) -> Self {
        Self {
            catalog,
            extensions,
            picker_items: Vec::new(),
            history_path: None,
            history_cache: None,
            picker_invocation: Arc::new(AtomicU8::new(PickerInvocation::None as u8)),
            picker_ranker: Arc::new(StablePickerRanker),
        }
    }

    fn with_extensions_and_picker(
        catalog: Catalog,
        extensions: Option<Box<dyn ExtensionCompleter + Send>>,
        picker_items: Vec<PickerItem>,
        history_path: Option<PathBuf>,
        picker_invocation: Arc<AtomicU8>,
        picker_ranker: Arc<dyn PickerRanker>,
    ) -> Self {
        Self {
            catalog,
            extensions,
            picker_items,
            history_path,
            history_cache: None,
            picker_invocation,
            picker_ranker,
        }
    }

    fn refreshed_history_items(&mut self) -> Option<&[PickerItem]> {
        let path = self.history_path.as_deref()?;
        let metadata = fs::metadata(path).ok()?;
        let len = metadata.len();
        let modified = metadata.modified().ok();
        let hit = self
            .history_cache
            .as_ref()
            .is_some_and(|cache| cache.len == len && cache.modified == modified);
        if !hit {
            let items = read_history_picker_items(path).ok()?;
            self.history_cache = Some(HistoryPickerCache {
                len,
                modified,
                items,
            });
        }
        Some(&self.history_cache.as_ref()?.items)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Completion candidate returned by an [`ExtensionCompleter`].
pub struct ExtensionSuggestion {
    /// Exact text inserted into the input buffer when accepted.
    pub value: String,
    /// Terminal-facing label; the UI escapes controls before rendering.
    pub display: String,
    /// Concise terminal-facing explanation.
    pub summary: String,
    /// Optional longer terminal-facing context.
    pub detail: String,
    /// Inclusive UTF-8 byte offset of the replaced input range.
    pub replace_start: usize,
    /// Exclusive UTF-8 byte offset of the replaced input range.
    pub replace_end: usize,
}

/// Stateful extension completion boundary used by Reedline and the rich surface.
pub trait ExtensionCompleter {
    /// Produce suggestions for the input prefix ending at byte offset `pos`.
    ///
    /// Implementations must return promptly with bounded output. Replacement
    /// ranges must satisfy `start <= end <= line.len()` and fall on UTF-8
    /// boundaries. Display text is treated as untrusted and escaped by the UI.
    fn complete(&mut self, line: &str, pos: usize) -> Vec<ExtensionSuggestion>;
}

const COMPLETION_VERSION_POLICY: VersionPolicy = VersionPolicy::frozen(COMPLETION_PROTOCOL_VERSION);

/// Worker-backed catalog completion. The editor can submit on every keystroke
/// and consume only the newest response; old queries and explicit cancellation
/// are never allowed to repaint a newer input buffer.
#[derive(Default)]
struct CompletionQueue {
    pending: Option<CompletionRequest>,
    shutdown: bool,
}

/// Single-threaded, latest-generation-only catalog completion worker.
///
/// The queue holds at most one pending request and one response. Submitting a
/// newer generation replaces queued work and makes older in-flight results
/// unobservable. Drop records shutdown and detaches rather than waiting on
/// catalog work, keeping terminal teardown bounded.
pub struct CompletionWorker {
    queue: Arc<(Mutex<CompletionQueue>, Condvar)>,
    response: Arc<Mutex<Option<CompletionResponse>>>,
    latest_request_id: Arc<AtomicU64>,
    submitted_request_id: u64,
    worker: Option<JoinHandle<()>>,
}

impl CompletionWorker {
    /// Start a persistent worker owning the supplied immutable catalog.
    pub fn new(catalog: Catalog) -> Self {
        // Failure model: input can arrive faster than catalog work completes,
        // and a terminal error can drop the editor while work is in flight.
        // One replaceable request and one replaceable response bound retained
        // query memory. Shutdown only records state; it never waits for a
        // worker that might still be inside a dependency call.
        let queue = Arc::new((Mutex::new(CompletionQueue::default()), Condvar::new()));
        let worker_queue = Arc::clone(&queue);
        let response = Arc::new(Mutex::new(None));
        let worker_response = Arc::clone(&response);
        let latest_request_id = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_request_id);
        let worker = thread::spawn(move || loop {
            let request = {
                let (lock, ready) = &*worker_queue;
                let mut state = lock_recover(lock);
                while state.pending.is_none() && !state.shutdown {
                    state = match ready.wait(state) {
                        Ok(state) => state,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                }
                if state.shutdown {
                    return;
                }
                state.pending.take()
            };
            let Some(request) = request else {
                continue;
            };
            let request_id = request.request_id;
            if worker_latest.load(Ordering::Acquire) != request_id {
                continue;
            }
            let started = Instant::now();
            let mut outcome = if worker_latest.load(Ordering::Acquire) != request_id {
                CompletionOutcome::Cancelled
            } else {
                CompletionOutcome::Ready {
                    items: catalog
                        .complete(&request.line, request.cursor)
                        .into_iter()
                        .take(request.limit)
                        .collect(),
                }
            };
            if worker_latest.load(Ordering::Acquire) != request_id {
                outcome = CompletionOutcome::Cancelled;
            } else if started.elapsed() >= Duration::from_millis(request.deadline_ms) {
                outcome = CompletionOutcome::DeadlineExceeded;
            }
            if worker_latest.load(Ordering::Acquire) == request_id {
                let mut slot = lock_recover(&worker_response);
                if worker_latest.load(Ordering::Acquire) != request_id {
                    continue;
                }
                *slot = Some(CompletionResponse {
                    protocol_version: COMPLETION_PROTOCOL_VERSION,
                    request_id,
                    outcome,
                });
            }
        });
        Self {
            queue,
            response,
            latest_request_id,
            submitted_request_id: 0,
            worker: Some(worker),
        }
    }

    /// Validate and enqueue a strictly newer completion request.
    ///
    /// Query text is limited to the catalog protocol byte bound, cursor offsets
    /// must be valid UTF-8 boundaries, result count is bounded, and deadlines
    /// are milliseconds in `1..=250`. Returns [`ErrorCode::Validation`] for
    /// invalid protocol/order/offsets and [`ErrorCode::ResourceLimit`] for size,
    /// count, or unavailable-worker failures.
    pub fn submit(&mut self, request: CompletionRequest) -> Result<(), ShellError> {
        validate_completion_request(&request)?;
        if request.request_id <= self.submitted_request_id {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "completion request IDs must be strictly increasing",
            )
            .with_help("Allocate a new request ID for every input change"));
        }
        self.submitted_request_id = request.request_id;
        self.latest_request_id
            .store(request.request_id, Ordering::Release);
        let (lock, ready) = &*self.queue;
        let mut state = lock_recover(lock);
        if state.shutdown {
            return Err(unavailable_completion_worker());
        }
        state.pending = Some(request);
        ready.notify_one();
        Ok(())
    }

    /// Make a matching pending, in-flight, or completed generation unobservable.
    ///
    /// Cancellation is idempotent and does not synchronously interrupt catalog
    /// code already running. An unsupported protocol version returns
    /// [`ErrorCode::Validation`].
    pub fn cancel(&self, cancellation: CompletionCancellation) -> Result<(), ShellError> {
        COMPLETION_VERSION_POLICY
            .validate("completion cancellation", cancellation.protocol_version)?;
        let _ = self.latest_request_id.compare_exchange(
            cancellation.request_id,
            cancellation.request_id.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let (lock, _) = &*self.queue;
        let mut state = lock_recover(lock);
        if state
            .pending
            .as_ref()
            .is_some_and(|request| request.request_id == cancellation.request_id)
        {
            state.pending = None;
        }
        let mut response = lock_recover(&self.response);
        if response
            .as_ref()
            .is_some_and(|response| response.request_id == cancellation.request_id)
        {
            response.take();
        }
        Ok(())
    }

    /// Consume the response only if it belongs to the newest submitted generation.
    ///
    /// Stale responses are discarded and never returned. `None` means work is
    /// pending, cancelled, stale, or already consumed; this method never blocks.
    pub fn try_recv_latest(&self) -> Option<CompletionResponse> {
        let expected = self.submitted_request_id;
        let mut response = lock_recover(&self.response);
        if self.latest_request_id.load(Ordering::Acquire) != expected {
            response.take();
            return None;
        }
        response
            .take()
            .filter(|response| response.request_id == expected)
    }
}

impl Drop for CompletionWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.queue;
        let mut state = lock_recover(lock);
        state.shutdown = true;
        state.pending = None;
        ready.notify_one();
        drop(state);
        // A worker already inside catalog code cannot be synchronously
        // cancelled. Its state is Arc-owned, so detaching is memory-safe and
        // keeps terminal teardown bounded.
        self.worker.take();
    }
}

fn validate_completion_request(request: &CompletionRequest) -> Result<(), ShellError> {
    COMPLETION_VERSION_POLICY.validate("completion request", request.protocol_version)?;
    if request.line.len() > MAX_COMPLETION_QUERY_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("completion query exceeds its limit of {MAX_COMPLETION_QUERY_BYTES} bytes"),
        )
        .with_help("Shorten the input before requesting completion"));
    }
    if request.cursor > request.line.len() || !request.line.is_char_boundary(request.cursor) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "completion cursor must be a UTF-8 character boundary within the input",
        )
        .with_help("Use the editor cursor offset from the same input string"));
    }
    if request.limit > MAX_COMPLETION_RESULTS {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("completion result limit exceeds {MAX_COMPLETION_RESULTS}"),
        )
        .with_help("Request at most the documented completion result limit"));
    }
    if !(1..=MAX_COMPLETION_DEADLINE_MS).contains(&request.deadline_ms) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("completion deadline must be between 1 and {MAX_COMPLETION_DEADLINE_MS} milliseconds"),
        )
        .with_help("Use a small positive deadline and issue a new request after expiry"));
    }
    Ok(())
}

fn unavailable_completion_worker() -> ShellError {
    ShellError::new(ErrorCode::ResourceLimit, "completion worker is unavailable")
        .with_help("Create a new editor completion worker for the next interactive session")
}

impl Completer for CatalogCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let invocation = PickerInvocation::from_state(&self.picker_invocation);
        if invocation == PickerInvocation::Help {
            return catalog_help_suggestions(&self.catalog, line, pos);
        }
        if let Some(kind) = invocation.item_kind() {
            let picker_ranker = Arc::clone(&self.picker_ranker);
            let (query, replace_start, replace_end) = picker_query_and_span(kind, line, pos);
            return if kind == PickerItemKind::History {
                if let Some(items) = self.refreshed_history_items() {
                    rank_picker_suggestions(
                        picker_ranker.as_ref(),
                        items,
                        query,
                        replace_start,
                        replace_end,
                    )
                } else {
                    rank_picker_suggestions_of_kind(
                        picker_ranker.as_ref(),
                        &self.picker_items,
                        kind,
                        query,
                        replace_start,
                        replace_end,
                    )
                }
            } else {
                rank_picker_suggestions_of_kind(
                    picker_ranker.as_ref(),
                    &self.picker_items,
                    kind,
                    query,
                    replace_start,
                    replace_end,
                )
            };
        }
        let mut suggestions = self
            .catalog
            .complete(line, pos)
            .into_iter()
            .map(catalog_suggestion)
            .collect::<Vec<_>>();
        if let Some(extensions) = &mut self.extensions {
            suggestions.extend(
                extensions
                    .complete(line, pos)
                    .into_iter()
                    .filter_map(|suggestion| extension_suggestion(line, suggestion)),
            );
        }
        let mut seen = HashSet::new();
        suggestions.retain(|suggestion| seen.insert(suggestion.value.clone()));
        suggestions
    }
}

fn catalog_help_suggestions(catalog: &Catalog, line: &str, pos: usize) -> Vec<Suggestion> {
    let prefix = line.get(..pos.min(line.len())).unwrap_or(line);
    let query = prefix.trim();
    let mut commands = if query.is_empty() {
        catalog.commands.iter().collect::<Vec<_>>()
    } else {
        let exact = catalog
            .commands
            .iter()
            .filter(|command| command_matches_exactly(command, query))
            .collect::<Vec<_>>();
        if !exact.is_empty() {
            exact
        } else {
            let path_prefix = catalog
                .commands
                .iter()
                .filter(|command| command_matches_prefix(command, query))
                .collect::<Vec<_>>();
            if !path_prefix.is_empty() {
                path_prefix
            } else if let Some(context) = catalog
                .commands
                .iter()
                .filter(|command| command_is_context_for(command, query))
                .max_by_key(|command| command.path.len())
            {
                vec![context]
            } else {
                let query = query.to_ascii_lowercase();
                catalog
                    .commands
                    .iter()
                    .filter(|command| {
                        command.path.to_ascii_lowercase().contains(&query)
                            || command.summary.to_ascii_lowercase().contains(&query)
                    })
                    .collect::<Vec<_>>()
            }
        }
    };
    commands.sort_by(|left, right| left.path.cmp(&right.path));
    commands
        .into_iter()
        .map(|command| command_help_suggestion(command, pos))
        .collect()
}

fn command_matches_exactly(command: &CommandSpec, query: &str) -> bool {
    command.path == query || command.aliases.iter().any(|alias| alias == query)
}

fn command_matches_prefix(command: &CommandSpec, query: &str) -> bool {
    command.path.starts_with(query) || command.aliases.iter().any(|alias| alias.starts_with(query))
}

fn command_is_context_for(command: &CommandSpec, query: &str) -> bool {
    query
        .strip_prefix(&command.path)
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        || command.aliases.iter().any(|alias| {
            query
                .strip_prefix(alias)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        })
}

fn command_help_suggestion(command: &CommandSpec, pos: usize) -> Suggestion {
    let mut description = vec![
        format!("Usage: {}", escape_terminal_controls(&command.signature)),
        escape_terminal_controls(&command.summary),
    ];
    if !command.details.is_empty() && command.details != command.summary {
        description.push(escape_terminal_controls(&command.details));
    }
    if !command.options.is_empty() {
        let options = command
            .options
            .iter()
            .map(|option| {
                format!(
                    "  {}: {}",
                    escape_terminal_controls(&option.names.join(", ")),
                    escape_terminal_controls(&option.documentation)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        description.push(format!("Arguments and options:\n{options}"));
    }
    if !command.examples.is_empty() {
        description.push(format!(
            "Examples:\n{}",
            command
                .examples
                .iter()
                .map(|example| format!("  {}", escape_terminal_controls(example)))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    Suggestion {
        // F1 is inspect-only: accepting or dismissing help must not rewrite input.
        value: String::new(),
        display_override: Some(escape_terminal_line(&command.signature)),
        description: Some(description.join("\n\n")),
        extra: None,
        span: Span::new(pos, pos),
        append_whitespace: false,
        ..Suggestion::default()
    }
}

fn catalog_suggestion(completion: quirl_catalog::Completion) -> Suggestion {
    let kind = if completion.value.starts_with('-') {
        "option"
    } else if completion.replace_start == 0 {
        "command"
    } else {
        "value"
    };
    let display = bounded_terminal_line(&completion.display);
    let summary = bounded_terminal_text(&completion.summary);
    let detail = bounded_terminal_text(&completion.detail);
    let match_indices = safe_match_indices(&completion.display, &display, completion.match_indices);
    Suggestion {
        value: completion.value,
        display_override: Some(format!("{display}  [{kind}]")),
        description: Some(format!(
            "{summary}\nkind: {kind} | source: catalog\n{detail}"
        )),
        extra: Some(vec![format!("kind: {kind} | source: catalog"), detail]),
        span: Span::new(completion.replace_start, completion.replace_end),
        append_whitespace: true,
        match_indices: Some(match_indices),
        ..Suggestion::default()
    }
}

fn extension_suggestion(line: &str, completion: ExtensionSuggestion) -> Option<Suggestion> {
    if !extension_replacement_is_valid(line, &completion) {
        return None;
    }
    let display = bounded_terminal_line(&completion.display);
    let summary = bounded_terminal_text(&completion.summary);
    let detail = bounded_terminal_text(&completion.detail);
    Some(Suggestion {
        value: completion.value,
        display_override: Some(format!("{display}  [plugin]")),
        description: Some(format!("{summary}\nkind: value | source: plugin\n{detail}")),
        extra: Some(vec!["kind: value | source: plugin".to_owned(), detail]),
        span: Span::new(completion.replace_start, completion.replace_end),
        append_whitespace: true,
        ..Suggestion::default()
    })
}

pub(crate) fn extension_replacement_is_valid(line: &str, completion: &ExtensionSuggestion) -> bool {
    completion.replace_start <= completion.replace_end
        && completion.replace_end <= line.len()
        && line.is_char_boundary(completion.replace_start)
        && line.is_char_boundary(completion.replace_end)
}

fn bounded_terminal_line(value: &str) -> String {
    let escaped = escape_terminal_line(value);
    truncate_utf8_ref(&escaped, PICKER_RANKING_TEXT_BYTES_MAX).to_owned()
}

fn bounded_terminal_text(value: &str) -> String {
    let escaped = escape_terminal_controls(value);
    truncate_utf8_ref(&escaped, PICKER_RANKING_TEXT_BYTES_MAX).to_owned()
}

fn safe_match_indices(original: &str, rendered: &str, indices: Vec<usize>) -> Vec<usize> {
    if original != rendered {
        return Vec::new();
    }
    let characters = rendered.chars().count();
    indices
        .into_iter()
        .filter(|index| *index < characters)
        .collect()
}

fn picker_query_and_span(kind: PickerItemKind, line: &str, pos: usize) -> (&str, usize, usize) {
    let end = pos.min(line.len());
    let before_cursor = line.get(..end).unwrap_or(line);
    if kind != PickerItemKind::File {
        return (before_cursor, 0, end);
    }
    let start = before_cursor
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    let suffix = line.get(end..).unwrap_or_default();
    let suffix_len = suffix
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map_or(suffix.len(), |(index, _)| index);
    (&before_cursor[start..], start, end + suffix_len)
}

fn rank_picker_suggestions(
    ranker: &dyn PickerRanker,
    items: &[PickerItem],
    query: &str,
    replace_start: usize,
    replace_end: usize,
) -> Vec<Suggestion> {
    let query = truncate_utf8_ref(query, PICKER_QUERY_BYTES_MAX);
    ranker
        .rank(items, query, PICKER_RESULTS_MAX)
        .into_iter()
        .filter_map(|matched| {
            let item = items.get(matched.index)?;
            let kind = picker_kind_label(item.kind);
            let label = bounded_terminal_line(&item.label);
            let description = bounded_terminal_text(&item.description);
            let match_indices = safe_match_indices(&item.label, &label, matched.match_indices);
            let detail = format!("{description}\nkind: {kind} | source: picker");
            let extra = item
                .preview
                .iter()
                .map(|preview| bounded_terminal_text(preview))
                .chain([format!("kind: {kind} | source: picker")])
                .collect::<Vec<_>>();
            Some(Suggestion {
                value: item.value.clone(),
                display_override: Some(format!("{label}  [{kind}]")),
                description: Some(detail),
                extra: Some(extra),
                span: Span::new(replace_start, replace_end),
                append_whitespace: false,
                match_indices: Some(match_indices),
                ..Suggestion::default()
            })
        })
        .collect()
}

const fn picker_kind_label(kind: PickerItemKind) -> &'static str {
    match kind {
        PickerItemKind::History => "history",
        PickerItemKind::File => "file",
        PickerItemKind::Directory => "directory",
        PickerItemKind::Action => "action",
        PickerItemKind::Completion => "completion",
        PickerItemKind::Job => "job",
        PickerItemKind::Data => "data",
    }
}

fn rank_picker_suggestions_of_kind(
    ranker: &dyn PickerRanker,
    items: &[PickerItem],
    kind: PickerItemKind,
    query: &str,
    replace_start: usize,
    replace_end: usize,
) -> Vec<Suggestion> {
    let mut seen_history_values = HashSet::new();
    let scoped = items
        .iter()
        .filter(|item| {
            item.kind == kind
                && (kind != PickerItemKind::History
                    || seen_history_values.insert(item.value.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    rank_picker_suggestions(ranker, &scoped, query, replace_start, replace_end)
}

fn read_history_picker_items(path: &Path) -> Result<Vec<PickerItem>, ShellError> {
    read_history(path).map(|history| history_picker_items(&history))
}

fn history_picker_items(history: &[String]) -> Vec<PickerItem> {
    let mut seen = HashSet::new();
    history
        .iter()
        .rev()
        .take(PICKER_ITEMS_MAX)
        .filter(|entry| seen.insert((*entry).clone()))
        .enumerate()
        .map(|(index, entry)| PickerItem {
            id: format!("History-{index}"),
            kind: PickerItemKind::History,
            label: bounded_terminal_line(entry),
            description: "history".to_owned(),
            preview: None,
            value: entry.clone(),
        })
        .collect()
}

#[derive(Clone, Copy)]
struct HistoryReadLimits {
    scanned_bytes_max: usize,
    retained_bytes_max: usize,
    entry_count_max: usize,
    encoded_entry_bytes_max: usize,
    entry_bytes_max: usize,
}

const HISTORY_READ_LIMITS: HistoryReadLimits = HistoryReadLimits {
    scanned_bytes_max: MAX_HISTORY_SCANNED_BYTES,
    retained_bytes_max: MAX_HISTORY_RETAINED_BYTES,
    entry_count_max: HISTORY_CAPACITY,
    encoded_entry_bytes_max: MAX_HISTORY_ENCODED_ENTRY_BYTES,
    entry_bytes_max: MAX_HISTORY_ENTRY_BYTES,
};

/// Read the newest valid entries from Quirl's durable Reedline history format.
///
/// The reader scans at most about 32 MiB from the file tail, retains at most
/// 50,000 entries or 8 MiB, skips invalid UTF-8 and oversized records, and
/// decodes Reedline-compatible multiline escapes. A missing file is an empty
/// history; other filesystem failures return [`ErrorCode::Io`].
pub fn read_history(path: &Path) -> Result<Vec<String>, ShellError> {
    read_history_with_limits(path, HISTORY_READ_LIMITS)
}

fn read_history_with_limits(
    path: &Path,
    limits: HistoryReadLimits,
) -> Result<Vec<String>, ShellError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(history_read_error(path, error)),
    };
    let file_len = file
        .metadata()
        .map_err(|error| history_read_error(path, error))?
        .len();
    read_history_file(&mut file, file_len, path, limits)
}

fn read_history_file(
    file: &mut File,
    file_len: u64,
    path: &Path,
    limits: HistoryReadLimits,
) -> Result<Vec<String>, ShellError> {
    let scanned_bytes_max = u64::try_from(limits.scanned_bytes_max).unwrap_or(u64::MAX);
    let start = file_len.saturating_sub(scanned_bytes_max);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| history_read_error(path, error))?;

    // Use the length observed before reading so concurrent appends cannot extend this scan.
    let scanned_bytes = file_len.saturating_sub(start);
    let capacity = usize::try_from(scanned_bytes)
        .unwrap_or(limits.scanned_bytes_max)
        .min(limits.scanned_bytes_max);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(scanned_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| history_read_error(path, error))?;

    let bytes = if start == 0 {
        bytes.as_slice()
    } else if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
        &bytes[newline.saturating_add(1)..]
    } else {
        return Ok(Vec::new());
    };
    Ok(parse_history_tail(bytes, limits))
}

fn parse_history_tail(bytes: &[u8], limits: HistoryReadLimits) -> Vec<String> {
    let mut retained_bytes = 0_usize;
    let mut history = Vec::new();
    for line in bytes.rsplit(|byte| *byte == b'\n') {
        if line.is_empty() || line.len() > limits.encoded_entry_bytes_max {
            continue;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        let entry = line.replace(HISTORY_NEWLINE_ESCAPE, "\n");
        if entry.len() > limits.entry_bytes_max {
            continue;
        }
        let next_bytes = retained_bytes.saturating_add(entry.len());
        if history.len() == limits.entry_count_max || next_bytes > limits.retained_bytes_max {
            break;
        }
        retained_bytes = next_bytes;
        history.push(entry);
    }
    history.reverse();
    history
}

fn history_read_error(path: &Path, error: io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not read history at {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Set QUIRL_HISTORY to a readable file path")
}

fn history_access_error(path: &Path, error: io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not access history at {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Set QUIRL_HISTORY to a readable and writable file path")
}

fn picker_sources(catalog: &Catalog, mut items: Vec<PickerItem>) -> Vec<PickerItem> {
    let next_id = items.len();
    let catalog_remaining = PICKER_ITEMS_MAX.saturating_sub(items.len());
    items.extend(
        catalog
            .commands
            .iter()
            .take(catalog_remaining)
            .enumerate()
            .map(|(index, command)| PickerItem {
                id: format!("action-{}", next_id + index),
                kind: PickerItemKind::Action,
                label: truncate_utf8_ref(&command.path, PICKER_RANKING_TEXT_BYTES_MAX).to_owned(),
                description: truncate_utf8_ref(&command.summary, PICKER_RANKING_TEXT_BYTES_MAX)
                    .to_owned(),
                preview: Some(
                    truncate_utf8_ref(&command.details, PICKER_RANKING_TEXT_BYTES_MAX).to_owned(),
                ),
                value: command.path.clone(),
            }),
    );
    let next_id = items.len();
    let file_remaining = PICKER_ITEMS_MAX.saturating_sub(items.len());
    if let Ok(entries) = fs::read_dir(".") {
        items.extend(
            entries
                .filter_map(Result::ok)
                .take(file_remaining)
                .enumerate()
                .map(|(index, entry)| {
                    let path = entry.path();
                    let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    PickerItem {
                        id: format!("file-{}", next_id + index),
                        kind: PickerItemKind::File,
                        label: truncate_utf8_ref(
                            path.to_string_lossy().as_ref(),
                            PICKER_RANKING_TEXT_BYTES_MAX,
                        )
                        .to_owned(),
                        description: if is_directory {
                            "directory".to_owned()
                        } else {
                            "file".to_owned()
                        },
                        preview: None,
                        value: path.to_string_lossy().into_owned(),
                    }
                }),
        );
    }
    items
}

#[cfg(test)]
fn picker_item(index: usize, kind: PickerItemKind, value: &str, description: &str) -> PickerItem {
    PickerItem {
        id: format!("{kind:?}-{index}"),
        kind,
        label: value.to_owned(),
        description: description.to_owned(),
        preview: None,
        value: value.to_owned(),
    }
}

struct SemanticHighlighter {
    catalog: Catalog,
    theme: Theme,
}

impl SemanticHighlighter {
    fn new(catalog: Catalog, theme: Theme) -> Self {
        Self { catalog, theme }
    }
}

impl Highlighter for SemanticHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut highlighted = StyledText::new();
        let known = self.catalog.commands.iter().any(|command| {
            line == command.path
                || line.starts_with(&format!("{} ", command.path))
                || command
                    .path
                    .starts_with(line.split_whitespace().next().unwrap_or_default())
        });
        let mut first_word = true;
        for segment in split_preserving_whitespace(line) {
            let kind = if segment.trim().is_empty() {
                None
            } else if first_word {
                first_word = false;
                Some(if known {
                    HighlightKind::Command
                } else {
                    HighlightKind::Error
                })
            } else if segment.starts_with('-') {
                Some(HighlightKind::Flag)
            } else if segment.starts_with('"') {
                Some(HighlightKind::StringDouble)
            } else if segment.starts_with('\'') {
                Some(HighlightKind::StringSingle)
            } else {
                Some(HighlightKind::Argument)
            };
            let style = if let Some(kind) = kind {
                self.theme.ansi_highlight(kind)
            } else {
                Style::new()
            };
            highlighted.push((style, segment.to_owned()));
        }
        highlighted
    }
}

fn split_preserving_whitespace(input: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut whitespace = input.chars().next().is_some_and(char::is_whitespace);
    for (index, character) in input.char_indices() {
        if character.is_whitespace() != whitespace {
            segments.push(&input[start..index]);
            start = index;
            whitespace = !whitespace;
        }
    }
    if start < input.len() {
        segments.push(&input[start..]);
    }
    segments
}

/// Render structured shell diagnostics as terminal-safe text.
///
/// All untrusted messages, source labels, context, and help are escaped before
/// output. When `color` is true, only Quirl-owned ANSI styles and Unicode label
/// chrome are added; otherwise the result contains no styling escapes. The
/// returned string has no trailing newline so the caller controls framing.
pub fn render_error(error: &ShellError, color: bool) -> String {
    let code = format!("{:?}", error.code).to_lowercase();
    let heading = format!("error[{code}]");
    let heading = if color {
        Color::Red.bold().paint(heading).to_string()
    } else {
        heading
    };
    let mut rendered = format!(
        "{heading}: {}\n",
        quirl_core::escape_terminal_controls(&error.message)
    );
    for label in &error.details.labels {
        let source =
            quirl_core::escape_terminal_controls(label.source.as_deref().unwrap_or("input"));
        let message = quirl_core::escape_terminal_controls(&label.message);
        if color {
            rendered.push_str(&format!("  ╭─[{source}:{}..{}]\n", label.start, label.end));
            rendered.push_str(&format!("  ╰─ {message}\n"));
        } else {
            rendered.push_str(&format!(
                "  at {source}:{}..{}: {message}\n",
                label.start, label.end
            ));
        }
    }
    for context in &error.details.context {
        rendered.push_str(&format!(
            "  caused by: {}\n",
            quirl_core::escape_terminal_controls(context)
        ));
    }
    for help in &error.details.help {
        let marker = if color {
            Color::Cyan.bold().paint("help").to_string()
        } else {
            "help".to_owned()
        };
        rendered.push_str(&format!(
            "  {marker}: {}\n",
            quirl_core::escape_terminal_controls(help)
        ));
    }
    rendered.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_core::ErrorCode;
    use quirl_lua::{EditorConfig, PickerConfig, PromptConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ExampleExtension;

    impl ExtensionCompleter for ExampleExtension {
        fn complete(&mut self, _line: &str, pos: usize) -> Vec<ExtensionSuggestion> {
            vec![ExtensionSuggestion {
                value: "production".to_owned(),
                display: "production".to_owned(),
                summary: "Deployment environment".to_owned(),
                detail: "Lua plugin".to_owned(),
                replace_start: pos.saturating_sub(4),
                replace_end: pos,
            }]
        }
    }

    fn assert_terminal_fields_are_safe(suggestion: &Suggestion) {
        let rendered = suggestion
            .display_override
            .iter()
            .chain(suggestion.description.iter())
            .chain(suggestion.extra.iter().flatten())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(!rendered.contains('\u{0007}'));
        assert!(rendered.contains("\\u{1b}"));
        assert!(rendered.contains("\\u{9b}"));
        assert!(rendered.contains("\\u{7}"));
    }

    #[test]
    fn simple_surface_escapes_completion_extension_picker_and_filename_fields() {
        let hostile = "safe\u{1b}[31m\u{1b}]8;;url\u{7}\u{009b}2J";
        let completion = catalog_suggestion(quirl_catalog::Completion {
            value: hostile.to_owned(),
            display: hostile.to_owned(),
            summary: hostile.to_owned(),
            detail: hostile.to_owned(),
            replace_start: 0,
            replace_end: 0,
            match_indices: vec![0],
        });
        assert_eq!(completion.value, hostile);
        assert_terminal_fields_are_safe(&completion);

        let extension = extension_suggestion(
            "",
            ExtensionSuggestion {
                value: hostile.to_owned(),
                display: hostile.to_owned(),
                summary: hostile.to_owned(),
                detail: hostile.to_owned(),
                replace_start: 0,
                replace_end: 0,
            },
        )
        .unwrap();
        assert_eq!(extension.value, hostile);
        assert_terminal_fields_are_safe(&extension);

        for kind in [PickerItemKind::Completion, PickerItemKind::File] {
            let item = PickerItem {
                id: "hostile".to_owned(),
                kind,
                label: hostile.to_owned(),
                description: hostile.to_owned(),
                preview: Some(hostile.to_owned()),
                value: hostile.to_owned(),
            };
            let suggestion = rank_picker_suggestions(&StablePickerRanker, &[item], "", 0, 0)
                .pop()
                .unwrap();
            assert_eq!(suggestion.value, hostile);
            assert_terminal_fields_are_safe(&suggestion);
        }
    }

    #[test]
    fn invalid_extension_replacement_offsets_are_discarded_at_ui_adapters() {
        let line = "écho";
        for (start, end) in [(3, 2), (0, line.len() + 1), (1, 2)] {
            let item = ExtensionSuggestion {
                value: "safe".to_owned(),
                display: "safe".to_owned(),
                summary: "safe".to_owned(),
                detail: "safe".to_owned(),
                replace_start: start,
                replace_end: end,
            };
            assert!(!extension_replacement_is_valid(line, &item));
            assert!(extension_suggestion(line, item).is_none());
        }
    }

    #[test]
    fn simple_picker_ranking_caps_items_labels_and_queries_without_shifting_indices() {
        let label = "a".repeat(PICKER_RANKING_TEXT_BYTES_MAX);
        let item = PickerItem {
            id: "bounded".to_owned(),
            kind: PickerItemKind::Completion,
            label,
            description: "z".repeat(PICKER_RANKING_TEXT_BYTES_MAX + 1),
            preview: None,
            value: "original".to_owned(),
        };
        let maximum_query = "a".repeat(PICKER_QUERY_BYTES_MAX);
        let matches = StablePickerRanker.rank(
            std::slice::from_ref(&item),
            &format!("{maximum_query}ignored"),
            PICKER_RESULTS_MAX,
        );
        assert_eq!(
            matches[0].match_indices,
            (0..PICKER_QUERY_BYTES_MAX).collect::<Vec<_>>()
        );

        let mut items = vec![item; PICKER_ITEMS_MAX];
        items.push(PickerItem {
            id: "outside".to_owned(),
            kind: PickerItemKind::Completion,
            label: "unique".to_owned(),
            description: String::new(),
            preview: None,
            value: "outside".to_owned(),
        });
        assert!(StablePickerRanker
            .rank(&items, "unique", PICKER_RESULTS_MAX)
            .is_empty());
    }

    #[test]
    fn history_search_prompt_escapes_transient_terminal_controls() {
        let prompt = QuirlPrompt::new(Mode::Command);
        let rendered = prompt.render_prompt_history_search_indicator(PromptHistorySearch::new(
            reedline::PromptHistorySearchStatus::Passing,
            "find\u{1b}]8;;url\u{7}\u{009b}2J".to_owned(),
        ));
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(!rendered.contains('\u{0007}'));
    }

    #[test]
    fn completion_contains_explanatory_metadata() {
        let mut completer = CatalogCompleter::new(Catalog::builtin());
        let result = completer.complete("git c", 5);
        assert_eq!(result[0].value, "git commit");
        assert!(result[0].description.as_deref().unwrap().contains("Record"));
        assert!(result[0]
            .display_override
            .as_deref()
            .unwrap()
            .contains("[command]"));
        assert!(result[0]
            .description
            .as_deref()
            .unwrap()
            .contains("source: catalog"));
    }

    #[test]
    fn contextual_help_uses_catalog_metadata_without_rewriting_input() {
        let catalog = Catalog::builtin();
        let command = catalog
            .commands
            .iter()
            .find(|command| command.path == "git commit")
            .unwrap();
        let line = "git commit --message";
        let suggestions = catalog_help_suggestions(&catalog, line, line.len());

        assert_eq!(suggestions.len(), 1);
        let suggestion = &suggestions[0];
        assert_eq!(suggestion.value, "");
        assert_eq!(suggestion.span, Span::new(line.len(), line.len()));
        assert!(!suggestion.append_whitespace);
        assert!(suggestion
            .display_override
            .as_deref()
            .unwrap()
            .contains(&command.signature));
        let description = suggestion.description.as_deref().unwrap();
        assert!(description.contains(&command.summary));
        assert!(description.contains(&command.details));
        for option in &command.options {
            assert!(description.contains(&option.documentation));
        }
        for example in &command.examples {
            assert!(description.contains(example));
        }
        assert!(suggestion.extra.is_none());
    }

    #[test]
    fn contextual_help_searches_prefixes_and_escapes_catalog_controls() {
        let mut catalog = Catalog::builtin();
        let command = catalog
            .commands
            .iter_mut()
            .find(|command| command.path == "git commit")
            .unwrap();
        command.summary.push_str("\u{1b}[31m");
        command.examples.push("echo safe\u{009b}2J".to_owned());

        let suggestions = catalog_help_suggestions(&catalog, "git c", 5);
        assert!(suggestions.iter().any(|suggestion| {
            suggestion
                .display_override
                .as_deref()
                .is_some_and(|display| display.starts_with("git commit"))
        }));
        let rendered = format!("{suggestions:?}");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(rendered.contains("\\u{1b}"));
        assert!(rendered.contains("\\u{9b}"));
    }

    #[test]
    fn diagnostics_have_stable_codes_and_help() {
        let error = ShellError::new(ErrorCode::Lua, "program failed").with_help("fix it");
        let rendered = render_error(&error, false);
        assert!(rendered.starts_with("error[lua]"));
        assert!(rendered.contains("help: fix it"));
    }

    #[test]
    fn plain_diagnostics_neutralize_terminal_controls_from_every_error_field() {
        let error = ShellError::new(ErrorCode::Lua, "bad\u{1b}[31mmessage")
            .with_label(
                Some("plugin\u{1b}]0;owned\u{7}".to_owned()),
                0,
                1,
                "label\u{9b}2J",
            )
            .with_context("context\rrewritten")
            .with_help("help\u{1b}[?25l");
        let rendered = render_error(&error, false);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.contains("\\u{1b}[31m"));
        assert!(rendered.contains("\\u{9b}2J"));
        assert!(rendered.contains("\\rrewritten"));
    }

    #[test]
    fn prompt_symbol_profiles_keep_font_requirements_explicit() {
        let parts = vec![
            "project".to_owned(),
            "command".to_owned(),
            "git:main".to_owned(),
        ];
        assert_eq!(
            join_prompt_parts(&parts, PromptSymbols::Plain),
            "project | command | git:main"
        );
        assert_eq!(
            join_prompt_parts(&parts, PromptSymbols::Unicode),
            "project · command · git:main"
        );
        assert_eq!(
            join_prompt_parts(&parts, PromptSymbols::NerdFont),
            "project \u{e0b1} command \u{e0b1} git:main"
        );
    }

    #[test]
    fn auto_symbols_only_use_unicode_for_a_unicode_locale() {
        assert!(unicode_is_safe(false, true));
        assert!(!unicode_is_safe(false, false));
        assert!(!unicode_is_safe(true, true));
        assert_eq!(
            PromptSymbols::resolve("auto", false, true),
            PromptSymbols::Unicode
        );
        assert_eq!(
            PromptSymbols::resolve("auto", false, false),
            PromptSymbols::Plain
        );
        assert_eq!(
            PromptSymbols::resolve("nerd_font", false, true),
            PromptSymbols::NerdFont
        );
        assert_eq!(
            PromptSymbols::resolve("nerd_font", false, false),
            PromptSymbols::NerdFont
        );
        assert_eq!(
            PromptSymbols::resolve("unicode", false, false),
            PromptSymbols::Unicode
        );
        assert_eq!(
            PromptSymbols::resolve("nerd_font", true, true),
            PromptSymbols::Plain
        );
        assert!(locale_name_supports_unicode(std::ffi::OsStr::new(
            "en_US.UTF-8"
        )));
        assert!(!locale_name_supports_unicode(std::ffi::OsStr::new("C")));
        assert!(!locale_value_supports_unicode(None));
        assert!(locale_value_supports_unicode(Some(std::ffi::OsStr::new(
            "C.UTF-8"
        ))));
    }

    #[test]
    fn filesystem_and_native_prompt_context_cannot_inject_terminal_controls() {
        let hostile = "cwd\u{1b}]0;owned\u{7}\u{009b}2J\rrewritten\nnext";
        let displayed = display_directory(Path::new(&format!("/tmp/{hostile}")));
        assert_eq!(displayed, hostile);
        let safe_display = safe_prompt_text(&displayed);
        assert!(safe_display.contains("\\u{1b}]0;owned\\u{7}"));
        assert!(safe_display.contains("\\u{9b}2J\\rrewritten\\nnext"));

        let context = NativePromptContext {
            directory: hostile.to_owned(),
            git_branch: Some(hostile.to_owned()),
            git_state: Some(hostile.to_owned()),
        };
        let original = context.clone();
        let config = QuirlConfig {
            prompt: PromptConfig {
                symbols: "plain".to_owned(),
                left: vec![
                    "directory".to_owned(),
                    "git_branch".to_owned(),
                    "git_state".to_owned(),
                ],
                right: Vec::new(),
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let rendered = QuirlPrompt::with_config(Mode::Command, &config)
            .with_native_context(context)
            .render_prompt_left()
            .into_owned();

        assert_eq!(original.directory, hostile);
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.ends_with('\n'));
        assert_eq!(rendered.matches('\n').count(), 1);
        assert!(rendered.contains("\\u{1b}]0;owned\\u{7}"));
        assert!(rendered.contains("\\u{9b}2J\\rrewritten\\nnext"));
    }

    #[test]
    fn plugin_prompt_segments_are_escaped_without_changing_the_source_value() {
        let hostile = "plugin\u{1b}]8;;https://example.invalid\u{7}link\u{1b}]8;;\u{7}\u{009b}2J\r";
        let original = hostile.to_owned();
        let config = QuirlConfig {
            prompt: PromptConfig {
                symbols: "plain".to_owned(),
                left: vec!["project".to_owned()],
                right: vec!["region".to_owned()],
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config)
            .with_named_extension_segments([
                ("project".to_owned(), hostile.to_owned()),
                ("region".to_owned(), hostile.to_owned()),
            ]);
        let named = format!(
            "{}{}",
            prompt.render_prompt_left(),
            prompt.render_prompt_right()
        );
        let legacy = QuirlPrompt::new(Mode::Command)
            .with_extension_segments(vec![hostile.to_owned()])
            .render_prompt_left()
            .into_owned();

        assert_eq!(original, hostile);
        for rendered in [named, legacy] {
            assert!(!rendered.contains('\u{1b}'));
            assert!(!rendered.contains('\u{7}'));
            assert!(!rendered.contains('\u{009b}'));
            assert!(!rendered.contains('\r'));
            assert!(rendered.contains("\\u{1b}]8;;https://example.invalid\\u{7}"));
            assert!(rendered.contains("\\u{9b}2J\\r"));
        }
    }

    #[test]
    fn prompt_profiles_are_escape_free_and_plain_is_ascii() {
        for profile in ["plain", "unicode", "nerd_font"] {
            let config = QuirlConfig {
                prompt: PromptConfig {
                    symbols: profile.to_owned(),
                    ..PromptConfig::default()
                },
                ..QuirlConfig::default()
            };
            let prompt = QuirlPrompt::with_config(Mode::Command, &config)
                .with_native_context(NativePromptContext {
                    directory: "quirl".to_owned(),
                    git_branch: Some("main".to_owned()),
                    git_state: Some("dirty".to_owned()),
                })
                .with_jobs(2)
                .with_duration(Duration::from_millis(42))
                .with_status(7);
            let rendered = format!(
                "{}{}{}",
                prompt.render_prompt_left(),
                prompt.render_prompt_right(),
                prompt.render_prompt_indicator(PromptEditMode::Default)
            );
            assert!(!rendered.contains('\u{1b}'));
            assert!(!rendered.contains('\u{009b}'));
            if profile == "plain" {
                assert!(rendered.is_ascii());
            }
        }
        let nerd = PromptSymbols::NerdFont;
        assert!(nerd.git_branch("main").contains('\u{e0a0}'));
        assert!(nerd.separator().contains('\u{e0b1}'));
        assert!(nerd.directory("quirl").contains('\u{f07c}'));
    }

    #[test]
    fn terminal_styles_require_an_interactive_color_capable_terminal() {
        assert!(terminal_styling_enabled(true, false, false));
        assert!(!terminal_styling_enabled(false, false, false));
        assert!(!terminal_styling_enabled(true, true, false));
        assert!(!terminal_styling_enabled(true, false, true));
    }

    #[test]
    fn right_prompt_stays_quiet_and_respects_configured_context() {
        let config = QuirlConfig {
            editor: EditorConfig {
                keymap: "vim".to_owned(),
                ..EditorConfig::default()
            },
            prompt: PromptConfig {
                symbols: "plain".to_owned(),
                right: vec!["jobs".to_owned()],
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config)
            .with_status(7)
            .with_jobs(2);

        let rendered = prompt.render_right_for(140, true, false);
        assert_eq!(rendered, "jobs:2");
        assert!(!rendered.contains("Tab"));
        assert!(!rendered.contains("COMMAND"));
        assert!(rendered.width() <= 140);
    }

    #[test]
    fn editor_chrome_has_ascii_tiny_and_dumb_terminal_fallbacks() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                symbols: "plain".to_owned(),
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Data, &config).with_status(12);

        let tiny = prompt.render_right_for(9, true, false);
        assert_eq!(tiny, "status:12");
        assert!(tiny.is_ascii());
        assert!(tiny.width() <= 9);
        let dumb = prompt.render_right_for(80, true, true);
        assert_eq!(dumb, "status:12");
    }

    #[test]
    fn modal_edit_indicators_and_cursor_shapes_match_editor_state() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                symbols: "plain".to_owned(),
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config);
        assert_eq!(
            prompt.render_prompt_indicator(PromptEditMode::Default),
            "command > "
        );
        assert_eq!(
            prompt.render_prompt_indicator(PromptEditMode::Emacs),
            "command > "
        );
        assert_eq!(
            prompt.render_prompt_indicator(PromptEditMode::Vi(PromptViMode::Normal)),
            "normal command > "
        );
        assert_eq!(
            prompt.render_prompt_indicator(PromptEditMode::Vi(PromptViMode::Insert)),
            "insert command > "
        );
        assert_eq!(
            prompt.render_prompt_indicator(PromptEditMode::Vi(PromptViMode::Visual)),
            "visual command > "
        );
        let cursors = editor_cursor_config();
        assert_eq!(cursors.vi_insert, Some(SetCursorStyle::SteadyBar));
        assert_eq!(cursors.vi_normal, Some(SetCursorStyle::SteadyBlock));
        assert_eq!(cursors.emacs, Some(SetCursorStyle::SteadyBar));
        assert!(prompt.right_prompt_on_last_line());
    }

    #[test]
    fn lua_suggestions_merge_with_catalog_completion() {
        let mut completer =
            CatalogCompleter::with_extensions(Catalog::builtin(), Some(Box::new(ExampleExtension)));
        let result = completer.complete("deploy --environment prod", 25);
        assert!(result.iter().any(|suggestion| {
            suggestion.value == "production"
                && suggestion
                    .description
                    .as_deref()
                    .is_some_and(|description| description.contains("Deployment environment"))
                && suggestion
                    .display_override
                    .as_deref()
                    .is_some_and(|display| display.contains("[plugin]"))
        }));
    }

    fn completion_request(request_id: u64, line: &str) -> CompletionRequest {
        CompletionRequest {
            protocol_version: COMPLETION_PROTOCOL_VERSION,
            request_id,
            line: line.to_owned(),
            cursor: line.len(),
            limit: 10,
            deadline_ms: 100,
        }
    }

    #[test]
    fn completion_worker_rejects_invalid_versions_bounds_and_cursor_offsets() {
        let mut worker = CompletionWorker::new(Catalog::builtin());
        let mut future = completion_request(1, "git");
        future.protocol_version += 1;
        assert_eq!(
            worker.submit(future).unwrap_err().code,
            ErrorCode::Validation
        );

        let mut offset = completion_request(1, "é");
        offset.cursor = 1;
        assert_eq!(
            worker.submit(offset).unwrap_err().code,
            ErrorCode::Validation
        );

        let mut excessive = completion_request(1, "git");
        excessive.limit = MAX_COMPLETION_RESULTS + 1;
        assert_eq!(
            worker.submit(excessive).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn completion_worker_never_returns_a_stale_query_result() {
        let mut worker = CompletionWorker::new(Catalog::builtin());
        worker.submit(completion_request(1, "git")).unwrap();
        worker.submit(completion_request(2, "git c")).unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        let mut response = None;
        while Instant::now() < until {
            response = worker.try_recv_latest();
            if response.is_some() {
                break;
            }
            thread::yield_now();
        }
        let response = response.expect("newest completion response should arrive");
        assert_eq!(response.request_id, 2);
        assert!(matches!(response.outcome, CompletionOutcome::Ready { .. }));
    }

    #[test]
    fn completion_worker_flood_retains_only_latest_request_and_response() {
        let mut worker = CompletionWorker::new(Catalog::builtin());
        const REQUESTS: u64 = 10_000;
        for request_id in 1..=REQUESTS {
            worker
                .submit(completion_request(request_id, "git c"))
                .unwrap();
            let (lock, _) = &*worker.queue;
            let pending_count = usize::from(lock_recover(lock).pending.is_some());
            assert!(pending_count <= 1);
        }

        let until = Instant::now() + Duration::from_secs(1);
        let response = loop {
            if let Some(response) = worker.try_recv_latest() {
                break response;
            }
            assert!(
                Instant::now() < until,
                "newest flooded request did not finish"
            );
            thread::yield_now();
        };
        assert_eq!(response.request_id, REQUESTS);
        assert!(lock_recover(&worker.response).is_none());
    }

    #[test]
    fn completion_cancellation_prevents_a_result_for_that_request() {
        let mut worker = CompletionWorker::new(Catalog::builtin());
        worker.submit(completion_request(1, "git c")).unwrap();
        worker
            .cancel(CompletionCancellation {
                protocol_version: COMPLETION_PROTOCOL_VERSION,
                request_id: 1,
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            assert!(worker.try_recv_latest().is_none());
            thread::yield_now();
        }
    }

    #[test]
    fn prompt_renders_extension_segments() {
        let prompt = QuirlPrompt::new(Mode::Command)
            .with_extension_segments(vec!["project".to_owned(), "git:main".to_owned()]);
        let rendered = prompt.render_prompt_left();
        assert!(rendered.contains(&join_prompt_parts(
            &["project".to_owned(), "git:main".to_owned()],
            prompt.symbols(),
        )));
    }

    #[test]
    fn configured_prompt_orders_native_and_named_segments() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                symbols: "plain".to_owned(),
                left: vec![
                    "mode".to_owned(),
                    "project".to_owned(),
                    "directory".to_owned(),
                ],
                right: vec![
                    "duration".to_owned(),
                    "status".to_owned(),
                    "region".to_owned(),
                ],
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config)
            .with_status(7)
            .with_named_extension_segments(vec![
                ("region".to_owned(), "eu-central".to_owned()),
                ("project".to_owned(), "quirl".to_owned()),
            ]);

        let left = prompt.render_prompt_left();
        let separator = " | ";
        assert!(left.starts_with(&format!("command{separator}quirl{separator}")));
        assert_eq!(
            prompt.render_prompt_right(),
            format!("status:7{separator}eu-central")
        );
    }

    #[test]
    fn unavailable_configured_prompt_segments_are_omitted() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                left: vec![
                    "jobs".to_owned(),
                    "duration".to_owned(),
                    "git_state".to_owned(),
                ],
                right: vec!["status".to_owned()],
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config);

        assert_eq!(prompt.render_prompt_left(), "");
        assert_eq!(prompt.render_prompt_right(), "");
    }

    #[test]
    fn prompt_duration_uses_a_compact_unit() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                left: Vec::new(),
                right: vec!["duration".to_owned()],
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config)
            .with_duration(Duration::from_millis(42));

        assert_eq!(prompt.render_prompt_right(), "42ms");
    }

    #[test]
    fn prompt_jobs_segment_only_renders_active_jobs() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                left: Vec::new(),
                right: vec!["jobs".to_owned()],
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        assert_eq!(
            QuirlPrompt::with_config(Mode::Command, &config).render_prompt_right(),
            ""
        );
        assert_eq!(
            QuirlPrompt::with_config(Mode::Command, &config)
                .with_jobs(2)
                .render_prompt_right(),
            "jobs:2"
        );
    }

    fn test_prompt_entry(cwd: &Path, branch: &str, state: Option<&str>) -> PromptCacheEntry {
        PromptCacheEntry {
            context: NativePromptContext {
                directory: display_directory(cwd),
                git_branch: Some(branch.to_owned()),
                git_state: state.map(str::to_owned),
            },
            dependencies: PromptDependencies {
                git_dir: None,
                head: Some(branch.to_owned()),
                head_ref: None,
                packed_refs: None,
                index: None,
                merge_head: None,
                rebase_merge: false,
                rebase_apply: false,
                worktree: WorktreeStamp::default(),
            },
        }
    }

    #[test]
    fn cold_prompt_sample_does_not_wait_for_slow_git_refresh() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = mpsc::channel();
        let loader = Arc::new(move |cwd: PathBuf, _previous: Option<PromptCacheEntry>| {
            let _ = started_tx.send(());
            let (lock, ready) = &*worker_gate;
            let mut released = lock_recover(lock);
            while !*released {
                released = match ready.wait(released) {
                    Ok(released) => released,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            test_prompt_entry(&cwd, "main", None)
        });
        let scheduler =
            PromptContextScheduler::with_context_loader(Duration::from_millis(250), loader);

        let sample = scheduler.sample(Path::new("/tmp/quirl-first-paint"));
        assert_eq!(sample.context.directory, "quirl-first-paint");
        assert!(!sample.timing.cache_hit);
        assert!(sample.timing.refresh_started);
        assert!(sample.timing.within_budget());
        assert!(started_rx.recv_timeout(Duration::from_secs(1)).is_ok());

        let (lock, ready) = &*gate;
        *lock_recover(lock) = true;
        ready.notify_all();
        assert!(scheduler.wait_until_idle(Duration::from_secs(1)));
    }

    #[test]
    fn cached_prompt_is_returned_stale_while_dependencies_refresh() {
        let calls = Arc::new(AtomicUsize::new(0));
        let refresh_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let loader_calls = Arc::clone(&calls);
        let loader_gate = Arc::clone(&refresh_gate);
        let loader = Arc::new(move |cwd: PathBuf, _previous: Option<PromptCacheEntry>| {
            let call = loader_calls.fetch_add(1, Ordering::SeqCst);
            if call == 1 {
                let (lock, ready) = &*loader_gate;
                let mut released = lock_recover(lock);
                while !*released {
                    released = match ready.wait(released) {
                        Ok(released) => released,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                }
            }
            test_prompt_entry(&cwd, if call == 0 { "main" } else { "topic" }, None)
        });
        let scheduler =
            PromptContextScheduler::with_context_loader(Duration::from_millis(250), loader);
        let cwd = Path::new("/tmp/quirl-stale-cache");

        let _cold = scheduler.sample(cwd);
        assert!(scheduler.wait_until_idle(Duration::from_secs(1)));
        let stale = scheduler.sample(cwd);
        assert_eq!(stale.context.git_branch.as_deref(), Some("main"));
        assert!(stale.timing.cache_hit);
        assert!(stale.timing.stale);

        let (lock, ready) = &*refresh_gate;
        *lock_recover(lock) = true;
        ready.notify_all();
        assert!(scheduler.wait_until_idle(Duration::from_secs(1)));
        let refreshed = scheduler.sample(cwd);
        assert_eq!(refreshed.context.git_branch.as_deref(), Some("topic"));
    }

    #[test]
    fn prompt_refresh_flood_retains_only_active_and_latest_paths() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = mpsc::channel();
        let loader = Arc::new(move |cwd: PathBuf, _previous: Option<PromptCacheEntry>| {
            let _ = started_tx.send(cwd.clone());
            let (lock, ready) = &*worker_gate;
            let mut released = lock_recover(lock);
            while !*released {
                released = ready
                    .wait(released)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            test_prompt_entry(&cwd, "main", None)
        });
        let scheduler =
            PromptContextScheduler::with_context_loader(Duration::from_millis(250), loader);
        let active = PathBuf::from("/tmp/quirl-prompt-active");
        let _ = scheduler.sample(&active);
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            active
        );

        const REQUESTS: usize = 10_000;
        let mut latest = PathBuf::new();
        for index in 0..REQUESTS {
            latest = PathBuf::from(format!("/tmp/quirl-prompt-flood-{index}"));
            let _ = scheduler.sample(&latest);
        }
        {
            let state = lock_recover(&scheduler.shared.state);
            assert_eq!(state.active.as_ref(), Some(&active));
            assert_eq!(
                state.pending.as_ref().map(|request| &request.cwd),
                Some(&latest)
            );
        }

        let (lock, ready) = &*gate;
        *lock_recover(lock) = true;
        ready.notify_all();
        assert!(scheduler.wait_until_idle(Duration::from_secs(1)));
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            latest
        );
        assert!(started_rx.try_recv().is_err());
    }

    #[test]
    fn prompt_cache_evicts_old_paths_at_its_cardinality_limit() {
        let loader = Arc::new(|cwd: PathBuf, _previous: Option<PromptCacheEntry>| {
            test_prompt_entry(&cwd, "main", None)
        });
        let scheduler =
            PromptContextScheduler::with_context_loader(Duration::from_millis(250), loader);

        for index in 0..MAX_PROMPT_CACHE_ENTRIES.saturating_mul(2) {
            let cwd = PathBuf::from(format!("/tmp/quirl-prompt-cache-{index}"));
            let _ = scheduler.sample(&cwd);
            assert!(scheduler.wait_until_idle(Duration::from_secs(1)));
            let state = lock_recover(&scheduler.shared.state);
            assert!(state.entries.len() <= MAX_PROMPT_CACHE_ENTRIES);
            assert_eq!(state.entries.len(), state.entry_recency.len());
        }

        let state = lock_recover(&scheduler.shared.state);
        assert!(!state
            .entries
            .contains_key(Path::new("/tmp/quirl-prompt-cache-0")));
        let newest = PathBuf::from(format!(
            "/tmp/quirl-prompt-cache-{}",
            MAX_PROMPT_CACHE_ENTRIES.saturating_mul(2).saturating_sub(1)
        ));
        assert!(state.entries.contains_key(&newest));
    }

    #[test]
    fn blocked_prompt_loader_does_not_block_scheduler_drop() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let loader = Arc::new(move |cwd: PathBuf, _previous: Option<PromptCacheEntry>| {
            let _ = started_tx.send(());
            let (lock, ready) = &*worker_gate;
            let mut released = lock_recover(lock);
            while !*released {
                released = ready
                    .wait(released)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            let _ = finished_tx.send(());
            test_prompt_entry(&cwd, "main", None)
        });
        let scheduler =
            PromptContextScheduler::with_context_loader(Duration::from_millis(250), loader);
        let _ = scheduler.sample(Path::new("/tmp/quirl-prompt-drop"));
        assert!(started_rx.recv_timeout(Duration::from_secs(1)).is_ok());

        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(scheduler);
            let _ = dropped_tx.send(());
        });
        let drop_result = dropped_rx.recv_timeout(Duration::from_millis(250));
        let (lock, ready) = &*gate;
        *lock_recover(lock) = true;
        ready.notify_all();
        assert!(finished_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        dropper.join().unwrap();
        assert!(
            drop_result.is_ok(),
            "scheduler shutdown waited for a blocked prompt loader"
        );
    }

    #[test]
    fn git_state_renders_in_configured_order_with_extension_segments() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                symbols: "plain".to_owned(),
                left: vec![
                    "directory".to_owned(),
                    "git_branch".to_owned(),
                    "git_state".to_owned(),
                    "project".to_owned(),
                ],
                right: Vec::new(),
                ..PromptConfig::default()
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config)
            .with_native_context(NativePromptContext {
                directory: "quirl".to_owned(),
                git_branch: Some("main".to_owned()),
                git_state: Some("dirty".to_owned()),
            })
            .with_named_extension_segments(vec![("project".to_owned(), "quirl".to_owned())]);

        assert_eq!(
            prompt.render_prompt_left(),
            format!(
                "{}\n",
                join_prompt_parts(
                    &[
                        "quirl".to_owned(),
                        "on main".to_owned(),
                        "dirty".to_owned(),
                        "quirl".to_owned(),
                    ],
                    PromptSymbols::Plain,
                )
            )
        );
    }

    #[test]
    fn home_directory_is_compacted_like_a_modern_shell_prompt() {
        let home = Path::new("/Users/alex");

        assert_eq!(display_directory_with_home(home, Some(home)), "~");
        assert_eq!(
            display_directory_with_home(
                Path::new("/Users/alex/Projects/github.com/example/quirl"),
                Some(home),
            ),
            "~/P/g/e/quirl"
        );
        assert_eq!(
            display_directory_with_home(Path::new("/var/tmp/quirl"), Some(home)),
            "quirl"
        );
    }

    #[test]
    fn styled_default_prompt_uses_only_fixed_quirl_color_sequences() {
        let config = QuirlConfig::default();
        let mut prompt = QuirlPrompt::with_config(Mode::Command, &config).with_native_context(
            NativePromptContext {
                directory: "~/P/g/n/quirl".to_owned(),
                git_branch: Some("main".to_owned()),
                git_state: Some("dirty".to_owned()),
            },
        );
        prompt.styled = true;
        prompt.theme = Theme::from_config(&config, true).unwrap();

        let rendered = prompt.render_prompt_left();

        assert!(rendered.starts_with("\u{1b}[1;38;2;125;207;255m~/P/g/n/quirl"));
        assert!(rendered.contains("\u{1b}[1;38;2;187;154;247mon main"));
        assert!(rendered.contains("dirty"));
        assert!(rendered.ends_with('\n'));
        assert!(!rendered.contains("\u{1b}]"));
    }

    #[test]
    fn simple_highlighter_uses_the_selected_custom_theme() {
        let mut config = QuirlConfig::default();
        let mut colors = config.active_theme().unwrap();
        colors.accent_command = "#010203".to_owned();
        colors.context_primary = "#040506".to_owned();
        config.ui.theme = "custom".to_owned();
        config.ui.themes.insert("custom".to_owned(), colors);
        let highlighter = SemanticHighlighter::new(
            Catalog::builtin(),
            Theme::from_config(&config, true).unwrap(),
        );

        let highlighted = highlighter.highlight("cd --help", 0);

        assert_eq!(
            highlighted.buffer[0].0.foreground,
            Some(Color::Rgb(1, 2, 3))
        );
        assert_eq!(
            highlighted.buffer[2].0.foreground,
            Some(Color::Rgb(4, 5, 6))
        );
    }

    #[test]
    fn editor_accepts_all_configured_keymaps_and_picker_options() {
        for keymap in ["emacs", "vim", "helix"] {
            let config = QuirlConfig {
                editor: EditorConfig {
                    keymap: keymap.to_owned(),
                    semantic_hints: keymap != "vim",
                    ..EditorConfig::default()
                },
                picker: PickerConfig {
                    layout: if keymap == "emacs" {
                        "bottom".to_owned()
                    } else {
                        "full".to_owned()
                    },
                    preview: keymap != "helix",
                },
                ..QuirlConfig::default()
            };
            let _editor = editor_with_config(Catalog::builtin(), config);
        }
    }

    #[test]
    fn helix_keeps_tab_bound_to_semantic_completion() {
        let event =
            ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)))
                .unwrap();
        let mut helix = configured_edit_mode("helix");

        assert_eq!(helix.parse_event(event), completion_menu_event());
    }

    fn parsed_key(
        edit_mode: &mut dyn EditMode,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> ReedlineEvent {
        let event = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap();
        edit_mode.parse_event(event)
    }

    fn apply_key(
        edit_mode: &mut dyn EditMode,
        editor: &mut Reedline,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> ReedlineEvent {
        let event = parsed_key(edit_mode, code, modifiers);
        if let ReedlineEvent::Edit(commands) = &event {
            editor.run_edit_commands(commands);
        }
        event
    }

    #[test]
    fn every_keymap_maps_ctrl_d_to_reedline_eof() {
        for keymap in ["emacs", "vim", "helix"] {
            let mut edit_mode = configured_edit_mode(keymap);

            assert_eq!(
                parsed_key(
                    edit_mode.as_mut(),
                    KeyCode::Char('d'),
                    KeyModifiers::CONTROL,
                ),
                ReedlineEvent::CtrlD,
                "{keymap}"
            );
        }
    }

    #[test]
    fn every_keymap_edits_again_after_backspace_delete_and_ctrl_h() {
        for keymap in ["emacs", "vim", "helix"] {
            let mut edit_mode = configured_edit_mode(keymap);
            let mut editor = Reedline::create().with_edit_mode(configured_edit_mode(keymap));

            for character in ['a', 'b', 'c'] {
                apply_key(
                    edit_mode.as_mut(),
                    &mut editor,
                    KeyCode::Char(character),
                    KeyModifiers::NONE,
                );
            }
            apply_key(
                edit_mode.as_mut(),
                &mut editor,
                KeyCode::Backspace,
                KeyModifiers::NONE,
            );
            apply_key(
                edit_mode.as_mut(),
                &mut editor,
                KeyCode::Char('x'),
                KeyModifiers::NONE,
            );
            assert_eq!(editor.current_buffer_contents(), "abx", "{keymap}");

            editor.run_edit_commands(&[EditCommand::MoveToStart { select: false }]);
            apply_key(
                edit_mode.as_mut(),
                &mut editor,
                KeyCode::Delete,
                KeyModifiers::NONE,
            );
            apply_key(
                edit_mode.as_mut(),
                &mut editor,
                KeyCode::Char('z'),
                KeyModifiers::NONE,
            );
            assert_eq!(editor.current_buffer_contents(), "zbx", "{keymap}");

            apply_key(
                edit_mode.as_mut(),
                &mut editor,
                KeyCode::Char('h'),
                KeyModifiers::CONTROL,
            );
            apply_key(
                edit_mode.as_mut(),
                &mut editor,
                KeyCode::Char('y'),
                KeyModifiers::NONE,
            );
            assert_eq!(editor.current_buffer_contents(), "ybx", "{keymap}");
        }
    }

    #[test]
    fn picker_query_edits_remain_buffer_mutations_in_every_keymap() {
        for keymap in ["emacs", "vim", "helix"] {
            for (code, modifiers) in [
                (KeyCode::Char('r'), KeyModifiers::CONTROL),
                (KeyCode::Char('t'), KeyModifiers::CONTROL),
                (KeyCode::Char('k'), KeyModifiers::CONTROL),
                (KeyCode::Tab, KeyModifiers::NONE),
            ] {
                let mut edit_mode = configured_edit_mode(keymap);
                let mut editor = Reedline::create().with_edit_mode(configured_edit_mode(keymap));
                editor.run_edit_commands(&[EditCommand::InsertString("abc".to_owned())]);

                let menu = parsed_key(edit_mode.as_mut(), code, modifiers);
                assert!(
                    matches!(
                        menu,
                        ReedlineEvent::Multiple(_) | ReedlineEvent::UntilFound(_)
                    ),
                    "menu opener in {keymap}"
                );
                apply_key(
                    edit_mode.as_mut(),
                    &mut editor,
                    KeyCode::Backspace,
                    KeyModifiers::NONE,
                );
                apply_key(
                    edit_mode.as_mut(),
                    &mut editor,
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                );
                editor.run_edit_commands(&[EditCommand::MoveToStart { select: false }]);
                apply_key(
                    edit_mode.as_mut(),
                    &mut editor,
                    KeyCode::Delete,
                    KeyModifiers::NONE,
                );
                apply_key(
                    edit_mode.as_mut(),
                    &mut editor,
                    KeyCode::Char('y'),
                    KeyModifiers::NONE,
                );

                assert_eq!(editor.current_buffer_contents(), "ybx", "{keymap}");
            }
        }
    }

    #[test]
    fn vim_normal_backspace_keeps_its_navigation_semantics() {
        let mut vim = configured_edit_mode("vim");
        let _ = parsed_key(vim.as_mut(), KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(
            parsed_key(vim.as_mut(), KeyCode::Backspace, KeyModifiers::NONE),
            ReedlineEvent::Edit(vec![EditCommand::MoveLeft { select: false }])
        );
    }

    #[test]
    fn every_keymap_submits_enter_and_closes_picker_state() {
        for keymap in ["emacs", "vim", "helix"] {
            let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
            let mut edit_mode =
                configured_edit_mode_with_picker(keymap, Arc::clone(&picker_invocation));
            let enter = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE,
            )))
            .unwrap();

            assert_eq!(
                edit_mode.parse_event(enter),
                ReedlineEvent::Enter,
                "{keymap}"
            );
            assert_eq!(
                PickerInvocation::from_state(&picker_invocation),
                PickerInvocation::None,
                "picker state in {keymap}"
            );
        }
    }

    #[test]
    fn every_keymap_exposes_mode_toggle_and_history_search() {
        for keymap in ["emacs", "vim", "helix"] {
            let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::None as u8));
            let mut edit_mode =
                configured_edit_mode_with_picker(keymap, Arc::clone(&picker_invocation));
            let toggle = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::ALT,
            )))
            .unwrap();
            assert_eq!(
                edit_mode.parse_event(toggle),
                ReedlineEvent::ExecuteHostCommand(MODE_TOGGLE_HOST_COMMAND.to_owned()),
                "mode toggle in {keymap} keymap"
            );

            let search = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
            assert_eq!(
                edit_mode.parse_event(search),
                picker_menu_event(HISTORY_PICKER_MENU, true),
                "history search in {keymap} keymap"
            );
            assert_eq!(
                PickerInvocation::from_state(&picker_invocation),
                PickerInvocation::History
            );
            let search_again = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
            )))
            .unwrap();
            assert_eq!(
                edit_mode.parse_event(search_again),
                picker_menu_event(HISTORY_PICKER_MENU, false)
            );

            for (key, menu, invocation) in [
                ('t', FILE_PICKER_MENU, PickerInvocation::File),
                ('k', ACTION_PICKER_MENU, PickerInvocation::Action),
            ] {
                let picker = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                    KeyCode::Char(key),
                    KeyModifiers::CONTROL,
                )))
                .unwrap();
                assert_eq!(edit_mode.parse_event(picker), picker_menu_event(menu, true));
                assert_eq!(PickerInvocation::from_state(&picker_invocation), invocation);
            }

            let help = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::F(1),
                KeyModifiers::NONE,
            )))
            .unwrap();
            assert_eq!(edit_mode.parse_event(help), menu_event(HELP_MENU, true));
            assert_eq!(
                PickerInvocation::from_state(&picker_invocation),
                PickerInvocation::Help
            );
            let help_again = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::F(1),
                KeyModifiers::NONE,
            )))
            .unwrap();
            assert_eq!(
                edit_mode.parse_event(help_again),
                menu_event(HELP_MENU, false)
            );
            let tab = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Tab,
                KeyModifiers::NONE,
            )))
            .unwrap();
            assert_eq!(edit_mode.parse_event(tab), ReedlineEvent::MenuNext);
            assert_eq!(
                PickerInvocation::from_state(&picker_invocation),
                PickerInvocation::Help
            );
        }
    }

    #[test]
    fn ctrl_space_encodings_remain_compatibility_mode_toggles() {
        for event in [
            Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL)),
            Event::Key(KeyEvent::new(KeyCode::Char('\0'), KeyModifiers::NONE)),
        ] {
            assert!(is_mode_toggle(&event));
        }
    }

    #[test]
    fn semantic_completion_clears_the_prior_picker_kind() {
        for keymap in ["emacs", "vim", "helix"] {
            let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
            let mut edit_mode =
                configured_edit_mode_with_picker(keymap, Arc::clone(&picker_invocation));
            let tab = ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(
                KeyCode::Tab,
                KeyModifiers::NONE,
            )))
            .unwrap();

            assert_eq!(
                edit_mode.parse_event(tab),
                menu_event(COMPLETION_MENU, true)
            );
            assert_eq!(
                PickerInvocation::from_state(&picker_invocation),
                PickerInvocation::None,
                "semantic completion in {keymap} keymap"
            );
        }
    }

    #[test]
    fn cancelling_or_accepting_clears_the_picker_kind() {
        for code in [KeyCode::Esc, KeyCode::Enter] {
            let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
            let mut edit_mode =
                configured_edit_mode_with_picker("emacs", Arc::clone(&picker_invocation));
            let event =
                ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
                    .unwrap();

            let _ = edit_mode.parse_event(event);
            assert_eq!(
                PickerInvocation::from_state(&picker_invocation),
                PickerInvocation::None
            );
        }
    }

    #[test]
    fn typed_picker_completer_returns_original_values_with_kind_specific_spans() {
        let items = vec![
            picker_item(
                0,
                PickerItemKind::History,
                "cargo test --workspace",
                "history",
            ),
            picker_item(
                1,
                PickerItemKind::File,
                "crates/quirl-ui/src/lib.rs",
                "file",
            ),
            picker_item(2, PickerItemKind::Action, "mode data", "action"),
        ];
        let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
        let mut completer = CatalogCompleter::with_extensions_and_picker(
            Catalog::builtin(),
            None,
            items,
            None,
            Arc::clone(&picker_invocation),
            Arc::new(StablePickerRanker),
        );
        let suggestions = completer.complete("cts", 3);
        assert_eq!(suggestions[0].value, "cargo test --workspace");
        assert_eq!(suggestions[0].span, Span::new(0, 3));
        assert!(suggestions[0]
            .display_override
            .as_deref()
            .is_some_and(|display| display.contains("[history]")));
        assert!(suggestions[0]
            .description
            .as_deref()
            .is_some_and(|description| description.contains("source: picker")));

        PickerInvocation::File.activate(&picker_invocation);
        let line = "cat crates/quirlXYZ trailing";
        let cursor = "cat crates/quirl".len();
        let suggestions = completer.complete(line, cursor);
        assert_eq!(suggestions[0].value, "crates/quirl-ui/src/lib.rs");
        assert_eq!(
            suggestions[0].span,
            Span::new(4, "cat crates/quirlXYZ".len())
        );
    }

    #[test]
    fn history_picker_keeps_only_the_most_recent_copy_of_each_command() {
        let history = [
            "cargo test".to_owned(),
            "cargo clippy".to_owned(),
            "cargo test".to_owned(),
        ];
        let items = history_picker_items(&history);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].value, "cargo test");
        assert_eq!(items[1].value, "cargo clippy");

        let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
        let fallback_items = vec![
            picker_item(0, PickerItemKind::History, "cargo test", "history"),
            picker_item(1, PickerItemKind::History, "cargo test", "history"),
        ];
        let mut completer = CatalogCompleter::with_extensions_and_picker(
            Catalog::builtin(),
            None,
            fallback_items,
            None,
            picker_invocation,
            Arc::new(StablePickerRanker),
        );
        let suggestions = completer.complete("cargo", 5);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "cargo test");
    }

    fn test_history_limits() -> HistoryReadLimits {
        HistoryReadLimits {
            scanned_bytes_max: 16,
            retained_bytes_max: 16,
            entry_count_max: 4,
            encoded_entry_bytes_max: 8,
            entry_bytes_max: 8,
        }
    }

    fn test_history_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("quirl-ui-{label}-{}-{unique}", std::process::id()))
    }

    #[test]
    fn bounded_history_reader_preserves_order_and_multiline_encoding() {
        let limits = HistoryReadLimits {
            scanned_bytes_max: 128,
            retained_bytes_max: 128,
            entry_count_max: 8,
            encoded_entry_bytes_max: 64,
            entry_bytes_max: 64,
        };
        let history = parse_history_tail(b"first\nprintf one<\\n>printf two\nlast\n", limits);

        assert_eq!(history, vec!["first", "printf one\nprintf two", "last"]);
        let items = history_picker_items(&history);
        assert_eq!(items[0].value, "last");
        assert_eq!(items[1].value, "printf one\nprintf two");
        assert_eq!(items[1].label, "printf one\\nprintf two");
        assert_eq!(items[2].value, "first");
    }

    #[test]
    fn bounded_history_reader_drops_truncated_first_entry() {
        let path = test_history_path("truncated-history");
        fs::write(&path, b"oldest\nmiddle\nnewest\n").unwrap();

        let history = read_history_with_limits(&path, test_history_limits()).unwrap();

        assert_eq!(history, vec!["middle", "newest"]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn bounded_history_reader_skips_malformed_and_oversized_entries() {
        let limits = HistoryReadLimits {
            scanned_bytes_max: 64,
            retained_bytes_max: 64,
            entry_count_max: 8,
            encoded_entry_bytes_max: 5,
            entry_bytes_max: 5,
        };
        let history = parse_history_tail(b"good\n\xff\xfe\n123456\nlast\n", limits);

        assert_eq!(history, vec!["good", "last"]);
    }

    #[test]
    fn bounded_history_reader_enforces_count_and_retained_byte_limits() {
        let count_limits = HistoryReadLimits {
            entry_count_max: 2,
            ..test_history_limits()
        };
        assert_eq!(
            parse_history_tail(b"aa\nbb\ncc\ndd\n", count_limits),
            vec!["cc", "dd"]
        );

        let byte_limits = HistoryReadLimits {
            retained_bytes_max: 3,
            ..test_history_limits()
        };
        assert_eq!(
            parse_history_tail(b"aa\nbb\ncc\ndd\n", byte_limits),
            vec!["dd"]
        );
    }

    #[test]
    fn bounded_history_reader_treats_a_missing_file_as_empty() {
        let path = test_history_path("missing-history");
        assert_eq!(
            read_history_with_limits(&path, test_history_limits()).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn bounded_history_reader_excludes_growth_after_metadata() {
        let path = test_history_path("growing-history");
        fs::write(&path, b"first\n").unwrap();
        let mut reader = File::open(&path).unwrap();
        let observed_len = reader.metadata().unwrap().len();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"second\n")
            .unwrap();

        let history =
            read_history_file(&mut reader, observed_len, &path, test_history_limits()).unwrap();

        assert_eq!(history, vec!["first"]);
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_history_reader_reports_permission_errors() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_history_path("permission-history");
        fs::write(&path, b"private\n").unwrap();
        let original = fs::metadata(&path).unwrap().permissions();
        let mut denied = original.clone();
        denied.set_mode(0o0);
        fs::set_permissions(&path, denied).unwrap();
        let result = read_history_with_limits(&path, test_history_limits());
        fs::set_permissions(&path, original).unwrap();
        fs::remove_file(path).unwrap();

        let error = result.unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert!(error.message.contains("could not read history"));
        assert!(!error.details.context.is_empty());
    }

    #[test]
    fn reedline_history_uses_the_bounded_reader_and_does_not_retain_oversized_saves() {
        let path = test_history_path("reedline-bounded-history");
        let mut file = File::create(&path).unwrap();
        file.write_all(&vec![b'x'; MAX_HISTORY_ENCODED_ENTRY_BYTES + 1])
            .unwrap();
        file.write_all(b"\nsafe tail\n").unwrap();
        drop(file);

        let mut history = BoundedFileHistory::with_file(path.clone()).unwrap();
        assert_eq!(history.count_all().unwrap(), 1);
        let oversized = history
            .save(HistoryItem::from_command_line(
                "x".repeat(MAX_HISTORY_ENTRY_BYTES + 1),
            ))
            .unwrap();
        assert!(oversized.id.is_none());
        assert_eq!(history.count_all().unwrap(), 1);
        history
            .save(HistoryItem::from_command_line("echo durable"))
            .unwrap();
        history.sync().unwrap();
        drop(history);

        let reopened = BoundedFileHistory::with_file(path.clone()).unwrap();
        assert_eq!(reopened.count_all().unwrap(), 2);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn reedline_history_sync_merges_concurrent_appends_without_an_unbounded_scan() {
        let path = test_history_path("reedline-concurrent-history");
        fs::write(&path, b"first\n").unwrap();
        let mut history = BoundedFileHistory::with_file(path.clone()).unwrap();
        history
            .save(HistoryItem::from_command_line("local"))
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"external\n")
            .unwrap();

        history.sync().unwrap();

        let entries = history
            .search(SearchQuery::everything(
                reedline::SearchDirection::Forward,
                None,
            ))
            .unwrap()
            .into_iter()
            .map(|item| item.command_line)
            .collect::<Vec<_>>();
        assert_eq!(entries, vec!["first", "external", "local"]);
        drop(history);
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn reedline_history_sync_failure_keeps_pending_data_for_retry() {
        use std::os::unix::fs::PermissionsExt;

        let path = test_history_path("reedline-retry-history");
        fs::write(&path, b"first\n").unwrap();
        let mut history = BoundedFileHistory::with_file(path.clone()).unwrap();
        history
            .save(HistoryItem::from_command_line("retry me"))
            .unwrap();
        let original = fs::metadata(&path).unwrap().permissions();
        let mut read_only = original.clone();
        read_only.set_mode(0o400);
        fs::set_permissions(&path, read_only).unwrap();

        let first_sync = history.sync();

        fs::set_permissions(&path, original).unwrap();
        assert!(first_sync.is_err());
        assert_eq!(history.pending, VecDeque::from(["retry me".to_owned()]));
        history.sync().unwrap();
        assert!(history.pending.is_empty());
        drop(history);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn history_picker_reloads_only_when_the_file_changes() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "quirl-ui-history-cache-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let history_path = directory.join("history");
        fs::write(&history_path, "cargo test\n").unwrap();
        let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
        let mut completer = CatalogCompleter::with_extensions_and_picker(
            Catalog::builtin(),
            None,
            Vec::new(),
            Some(history_path.clone()),
            picker_invocation,
            Arc::new(StablePickerRanker),
        );
        let line = "cargo";
        assert_eq!(completer.complete(line, line.len())[0].value, "cargo test");
        fs::write(&history_path, "cargo test\n").unwrap();
        assert_eq!(completer.complete(line, line.len())[0].value, "cargo test");
        fs::write(&history_path, "cargo clippy\n").unwrap();
        assert_eq!(
            completer.complete(line, line.len())[0].value,
            "cargo clippy"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn history_path_prefers_explicit_then_xdg_then_home() {
        assert_eq!(
            resolve_history_path(
                Some(OsString::from("/tmp/explicit-history")),
                Some(OsString::from("/tmp/state")),
                Some(OsString::from("/tmp/home")),
            ),
            Some(PathBuf::from("/tmp/explicit-history"))
        );
        assert_eq!(
            resolve_history_path(
                None,
                Some(OsString::from("/tmp/state")),
                Some(OsString::from("/tmp/home")),
            ),
            Some(PathBuf::from("/tmp/state/quirl/history"))
        );
        assert_eq!(
            resolve_history_path(None, None, Some(OsString::from("/tmp/home"))),
            Some(PathBuf::from("/tmp/home/.local/state/quirl/history"))
        );
    }

    #[test]
    fn durable_history_survives_editor_rebuilds() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            env::temp_dir().join(format!("quirl-ui-history-{}-{unique}", std::process::id()));
        let history_path = directory.join("history");
        let mut first = editor_with_extensions_config_and_history(
            Catalog::builtin(),
            None,
            QuirlConfig::default(),
            history_path.clone(),
        )
        .unwrap();
        first
            .history_mut()
            .save(reedline::HistoryItem::from_command_line("echo durable"))
            .unwrap();
        first.sync_history().unwrap();

        let second = editor_with_extensions_config_and_history(
            Catalog::builtin(),
            None,
            QuirlConfig::default(),
            history_path,
        )
        .unwrap();
        assert_eq!(second.history().count_all().unwrap(), 1);

        drop(first);
        drop(second);
        fs::remove_dir_all(directory).unwrap();
    }
}
