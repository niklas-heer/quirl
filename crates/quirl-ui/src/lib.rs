//! Terminal interaction that treats completion and diagnostics as core behavior.

mod panel;

pub use panel::{
    directory_panel, process_panel, LiveBuffer, LiveSample, LiveSnapshot, PanelModel,
    ProcessPanelRow,
};

use crossterm::event::{Event, KeyEvent};
use nu_ansi_term::{Color, Style};
use quirl_catalog::{
    Catalog, CompletionCancellation, CompletionOutcome, CompletionRequest, CompletionResponse,
    COMPLETION_PROTOCOL_VERSION, MAX_COMPLETION_DEADLINE_MS, MAX_COMPLETION_QUERY_BYTES,
    MAX_COMPLETION_RESULTS,
};
use quirl_core::{ErrorCode, ShellError, VersionPolicy};
use quirl_lua::QuirlConfig;
use quirl_picker::{ItemKind, PickItem, Picker};
use quirl_syntax::Mode;
use reedline::{
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    Completer, DefaultHinter, DefaultValidator, DescriptionMode, EditMode, Emacs,
    FileBackedHistory, Helix, Highlighter, IdeMenu, InputMode, KeyCode, KeyModifiers, MenuBuilder,
    OutputMode, Prompt, PromptEditMode, PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu,
    ReedlineRawEvent, Span, StyledText, Suggestion, Vi,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Opaque Reedline host command used to switch Quirl's interactive grammar.
pub const MODE_TOGGLE_HOST_COMMAND: &str = "quirl:mode-toggle";

const HISTORY_CAPACITY: usize = 50_000;
const COMPLETION_MENU: &str = "completion_menu";
const HISTORY_PICKER_MENU: &str = "history_picker_menu";
const FILE_PICKER_MENU: &str = "file_picker_menu";
const ACTION_PICKER_MENU: &str = "action_picker_menu";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum PickerInvocation {
    None,
    History,
    File,
    Action,
}

impl PickerInvocation {
    fn from_state(state: &AtomicU8) -> Self {
        match state.load(Ordering::Relaxed) {
            value if value == Self::History as u8 => Self::History,
            value if value == Self::File as u8 => Self::File,
            value if value == Self::Action as u8 => Self::Action,
            _ => Self::None,
        }
    }

    fn item_kind(self) -> Option<ItemKind> {
        match self {
            Self::None => None,
            Self::History => Some(ItemKind::History),
            Self::File => Some(ItemKind::File),
            Self::Action => Some(ItemKind::Action),
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

pub fn editor(catalog: Catalog) -> Reedline {
    editor_with_config(catalog, QuirlConfig::default())
}

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
    configured_editor(catalog, extension_completer, config, None, Vec::new(), None)
}

/// Create an editor backed by a durable, newline-delimited history file.
///
/// Reopening an editor with the same path reloads the prior entries. Callers that
/// rebuild the editor while it is live should call [`Reedline::sync_history`] first
/// so the replacement observes the newest commands.
pub fn editor_with_extensions_config_and_history(
    catalog: Catalog,
    extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
    config: QuirlConfig,
    history_path: PathBuf,
) -> Result<Reedline, ShellError> {
    let history =
        FileBackedHistory::with_file(HISTORY_CAPACITY, history_path.clone()).map_err(|error| {
            ShellError::new(
                quirl_core::ErrorCode::Io,
                format!("could not open history at {}", history_path.display()),
            )
            .with_context(error.to_string())
            .with_help("Set QUIRL_HISTORY to a writable file path")
        })?;
    let history_items =
        history_picker_items(&fs::read_to_string(&history_path).unwrap_or_default());
    Ok(configured_editor(
        catalog,
        extension_completer,
        config,
        Some(history),
        history_items,
        Some(history_path),
    ))
}

fn configured_editor(
    catalog: Catalog,
    extension_completer: Option<Box<dyn ExtensionCompleter + Send>>,
    config: QuirlConfig,
    history: Option<FileBackedHistory>,
    history_items: Vec<PickItem>,
    history_path: Option<PathBuf>,
) -> Reedline {
    let terminal_styles = terminal_styling_enabled(
        std::io::stdout().is_terminal(),
        env::var_os("NO_COLOR").is_some(),
        dumb_terminal(),
    );
    let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::None as u8));
    let completer = Box::new(CatalogCompleter::with_extensions_and_picker(
        catalog.clone(),
        extension_completer,
        picker_sources(&catalog, history_items),
        history_path,
        Arc::clone(&picker_invocation),
    ));
    let completion_menu = Box::new(configured_completion_menu(&config));
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
        .with_menu(ReedlineMenu::EngineCompleter(history_picker_menu))
        .with_menu(ReedlineMenu::EngineCompleter(file_picker_menu))
        .with_menu(ReedlineMenu::EngineCompleter(action_picker_menu))
        .with_hinter(Box::new(DefaultHinter::default().with_style(
            if terminal_styles {
                Style::new().italic().fg(Color::DarkGray)
            } else {
                Style::new()
            },
        )))
        .with_validator(Box::new(DefaultValidator))
        .with_edit_mode(configured_edit_mode_with_picker(
            &config.editor.keymap,
            picker_invocation,
        ))
        .with_quick_completions(false);
    if let Some(history) = history {
        line_editor = line_editor.with_history(Box::new(history));
    }
    if config.editor.semantic_hints && terminal_styles {
        line_editor = line_editor.with_highlighter(Box::new(SemanticHighlighter::new(catalog)));
    }
    line_editor
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
    let (inner, complete_tab): (Box<dyn EditMode>, bool) = match keymap {
        "vim" => {
            let mut insert = default_vi_insert_keybindings();
            let mut normal = default_vi_normal_keybindings();
            insert.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            normal.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            (Box::new(Vi::new(insert, normal)), false)
        }
        "helix" => (Box::<Helix>::default(), true),
        // Config validation rejects other values. Keep this fallback for direct Rust callers.
        "emacs" => {
            let mut keybindings = default_emacs_keybindings();
            keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            (Box::new(Emacs::new(keybindings)), false)
        }
        _ => {
            let mut keybindings = default_emacs_keybindings();
            keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            (Box::new(Emacs::new(keybindings)), false)
        }
    };
    Box::new(QuirlEditMode {
        inner,
        complete_tab,
        picker_invocation,
    })
}

/// Add Quirl-wide shortcuts without replacing Reedline's keymap implementations.
struct QuirlEditMode {
    inner: Box<dyn EditMode>,
    complete_tab: bool,
    picker_invocation: Arc<AtomicU8>,
}

impl EditMode for QuirlEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let event: Event = event.into();
        if is_mode_toggle(&event) {
            return ReedlineEvent::ExecuteHostCommand(MODE_TOGGLE_HOST_COMMAND.to_owned());
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

fn is_mode_toggle(event: &Event) -> bool {
    match event {
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
    let menu = IdeMenu::default().with_name(name).with_default_border();
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
    pub directory: String,
    pub git_branch: Option<String>,
    pub git_state: Option<String>,
}

/// Instrumentation for one non-blocking native prompt context lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptTimingSample {
    pub elapsed: Duration,
    pub budget: Duration,
    pub cache_hit: bool,
    /// The returned value was usable but a newer value is being collected.
    pub stale: bool,
    pub refresh_started: bool,
}

impl PromptTimingSample {
    pub fn within_budget(self) -> bool {
        self.elapsed <= self.budget
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptContextSample {
    pub context: NativePromptContext,
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
    in_flight: HashSet<PathBuf>,
    refresh_generation: u64,
}

struct PromptSchedulerShared {
    state: Mutex<PromptSchedulerState>,
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
    requests: Option<mpsc::Sender<RefreshRequest>>,
    worker: Option<JoinHandle<()>>,
    first_paint_budget: Duration,
}

impl Default for PromptContextScheduler {
    fn default() -> Self {
        Self::new(PROMPT_FIRST_PAINT_BUDGET)
    }
}

impl PromptContextScheduler {
    pub fn new(first_paint_budget: Duration) -> Self {
        Self::with_context_loader(first_paint_budget, Arc::new(load_prompt_context))
    }

    fn with_context_loader(first_paint_budget: Duration, loader: Arc<PromptContextLoader>) -> Self {
        let shared = Arc::new(PromptSchedulerShared {
            state: Mutex::new(PromptSchedulerState::default()),
            refreshed: Condvar::new(),
        });
        let (requests, receiver) = mpsc::channel::<RefreshRequest>();
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("quirl-prompt-context".to_owned())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let entry = loader(request.cwd.clone(), request.previous);
                    let mut state = lock_recover(&worker_shared.state);
                    state.entries.insert(request.cwd.clone(), entry);
                    state.in_flight.remove(&request.cwd);
                    state.refresh_generation = state.refresh_generation.wrapping_add(1);
                    worker_shared.refreshed.notify_all();
                }
            })
            .ok();
        Self {
            shared,
            requests: Some(requests),
            worker,
            first_paint_budget,
        }
    }

    pub fn sample_current_dir(&self) -> PromptContextSample {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        self.sample(&cwd)
    }

    pub fn sample(&self, cwd: &Path) -> PromptContextSample {
        let started = Instant::now();
        let cwd = cwd.to_path_buf();
        let directory = display_directory(&cwd);
        let (context, cache_hit, refresh_started, request) = {
            let mut state = lock_recover(&self.shared.state);
            let cached = state.entries.get(&cwd).cloned();
            let cache_hit = cached.is_some();
            let context = cached
                .as_ref()
                .map(|entry| entry.context.clone())
                .unwrap_or(NativePromptContext {
                    directory,
                    ..NativePromptContext::default()
                });
            let refresh_started = state.in_flight.insert(cwd.clone());
            let request = refresh_started.then_some(RefreshRequest {
                cwd: cwd.clone(),
                previous: cached,
            });
            (context, cache_hit, refresh_started, request)
        };

        if let Some(request) = request {
            let sent = self
                .requests
                .as_ref()
                .is_some_and(|requests| requests.send(request).is_ok());
            if !sent {
                lock_recover(&self.shared.state).in_flight.remove(&cwd);
            }
        }

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
        if state.in_flight.is_empty() {
            return true;
        }
        let waited = self
            .shared
            .refreshed
            .wait_timeout_while(state, timeout, |state| !state.in_flight.is_empty());
        match waited {
            Ok((state, _)) => state.in_flight.is_empty(),
            Err(poisoned) => poisoned.into_inner().0.in_flight.is_empty(),
        }
    }
}

impl Drop for PromptContextScheduler {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
    named_extension_segments: HashMap<String, String>,
}

impl QuirlPrompt {
    pub fn new(mode: Mode) -> Self {
        let cwd_path = env::current_dir().ok();
        let cwd = cwd_path
            .as_deref()
            .map(display_directory)
            .unwrap_or_else(|| "/".to_owned());
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
            named_extension_segments: HashMap::new(),
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
        prompt
    }

    pub fn with_extension_segments(mut self, segments: Vec<String>) -> Self {
        self.extension_segments = segments;
        self
    }

    /// Apply a snapshot returned by [`PromptContextScheduler`].
    pub fn with_native_context(mut self, context: NativePromptContext) -> Self {
        self.cwd = context.directory;
        self.git_branch = context.git_branch;
        self.git_state = context.git_state;
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
            .collect();
        self
    }

    fn render_segments(&self, requested: &[String]) -> String {
        let parts = requested
            .iter()
            .filter_map(|name| match name.as_str() {
                "directory" => Some(self.cwd.clone()),
                "mode" => Some(self.mode.to_string()),
                "git_branch" => self
                    .git_branch
                    .as_ref()
                    .map(|branch| format!("git:{branch}")),
                "status" => self
                    .status
                    .filter(|status| *status != 0)
                    .map(|status| format!("status:{status}")),
                "duration" => self.duration.map(format_duration),
                "jobs" => (self.jobs > 0).then(|| format!("jobs:{}", self.jobs)),
                "git_state" => self.git_state.as_ref().map(|state| format!("git:{state}")),
                _ => self.named_extension_segments.get(name).cloned(),
            })
            .collect::<Vec<_>>();
        join_prompt_parts(&parts, dumb_terminal())
    }
}

fn join_prompt_parts(parts: &[String], dumb: bool) -> String {
    parts.join(if dumb { " | " } else { " · " })
}

fn dumb_terminal() -> bool {
    env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"))
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
            let rendered = self.render_segments(segments);
            return Cow::Owned(if rendered.is_empty() {
                String::new()
            } else {
                format!("{rendered} ")
            });
        }
        let mut parts = vec![self.cwd.clone(), self.mode.to_string()];
        parts.extend(self.extension_segments.iter().cloned());
        Cow::Owned(format!("{} ", join_prompt_parts(&parts, dumb_terminal())))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(self.render_segments(&self.configured_right))
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        if dumb_terminal() {
            Cow::Owned(match self.mode {
                Mode::Command => "> ".to_owned(),
                Mode::Data => "data> ".to_owned(),
            })
        } else {
            Cow::Owned(format!("{} ", self.mode.prompt()))
        }
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(if dumb_terminal() { "... " } else { "  · " })
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let indicator = if dumb_terminal() {
            match self.mode {
                Mode::Command => ">",
                Mode::Data => "data>",
            }
        } else {
            self.mode.prompt()
        };
        Cow::Owned(format!("search `{}` {indicator} ", history_search.term))
    }
}

struct HistoryPickerCache {
    len: u64,
    modified: Option<SystemTime>,
    items: Vec<PickItem>,
}

pub struct CatalogCompleter {
    catalog: Catalog,
    extensions: Option<Box<dyn ExtensionCompleter + Send>>,
    picker_items: Vec<PickItem>,
    history_path: Option<PathBuf>,
    history_cache: Option<HistoryPickerCache>,
    picker_invocation: Arc<AtomicU8>,
}

impl CatalogCompleter {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            extensions: None,
            picker_items: Vec::new(),
            history_path: None,
            history_cache: None,
            picker_invocation: Arc::new(AtomicU8::new(PickerInvocation::None as u8)),
        }
    }

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
        }
    }

    fn with_extensions_and_picker(
        catalog: Catalog,
        extensions: Option<Box<dyn ExtensionCompleter + Send>>,
        picker_items: Vec<PickItem>,
        history_path: Option<PathBuf>,
        picker_invocation: Arc<AtomicU8>,
    ) -> Self {
        Self {
            catalog,
            extensions,
            picker_items,
            history_path,
            history_cache: None,
            picker_invocation,
        }
    }

    fn refreshed_history_items(&mut self) -> Option<&[PickItem]> {
        let path = self.history_path.as_deref()?;
        let metadata = fs::metadata(path).ok()?;
        let len = metadata.len();
        let modified = metadata.modified().ok();
        let hit = self
            .history_cache
            .as_ref()
            .is_some_and(|cache| cache.len == len && cache.modified == modified);
        if !hit {
            let items = read_history_picker_items(path)?;
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
pub struct ExtensionSuggestion {
    pub value: String,
    pub display: String,
    pub summary: String,
    pub detail: String,
    pub replace_start: usize,
    pub replace_end: usize,
}

pub trait ExtensionCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<ExtensionSuggestion>;
}

const COMPLETION_VERSION_POLICY: VersionPolicy = VersionPolicy::frozen(COMPLETION_PROTOCOL_VERSION);

/// Worker-backed catalog completion. The editor can submit on every keystroke
/// and consume only the newest response; old queries and explicit cancellation
/// are never allowed to repaint a newer input buffer.
pub struct CompletionWorker {
    requests: Option<mpsc::Sender<CompletionRequest>>,
    responses: mpsc::Receiver<CompletionResponse>,
    latest_request_id: Arc<AtomicU64>,
    submitted_request_id: u64,
    worker: Option<JoinHandle<()>>,
}

impl CompletionWorker {
    pub fn new(catalog: Catalog) -> Self {
        let (requests, request_receiver) = mpsc::channel::<CompletionRequest>();
        let (response_sender, responses) = mpsc::channel::<CompletionResponse>();
        let latest_request_id = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_request_id);
        let worker = thread::spawn(move || {
            while let Ok(request) = request_receiver.recv() {
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
                if worker_latest.load(Ordering::Acquire) == request_id
                    && response_sender
                        .send(CompletionResponse {
                            protocol_version: COMPLETION_PROTOCOL_VERSION,
                            request_id,
                            outcome,
                        })
                        .is_err()
                {
                    return;
                }
            }
        });
        Self {
            requests: Some(requests),
            responses,
            latest_request_id,
            submitted_request_id: 0,
            worker: Some(worker),
        }
    }

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
        self.requests
            .as_ref()
            .ok_or_else(unavailable_completion_worker)?
            .send(request)
            .map_err(|_| unavailable_completion_worker())
    }

    pub fn cancel(&self, cancellation: CompletionCancellation) -> Result<(), ShellError> {
        COMPLETION_VERSION_POLICY
            .validate("completion cancellation", cancellation.protocol_version)?;
        let _ = self.latest_request_id.compare_exchange(
            cancellation.request_id,
            cancellation.request_id.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Ok(())
    }

    pub fn try_recv_latest(&self) -> Option<CompletionResponse> {
        let expected = self.submitted_request_id;
        if self.latest_request_id.load(Ordering::Acquire) != expected {
            while self.responses.try_recv().is_ok() {}
            return None;
        }
        let mut newest = None;
        loop {
            match self.responses.try_recv() {
                Ok(response) if response.request_id == expected => newest = Some(response),
                Ok(_) => {}
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => return newest,
            }
        }
    }
}

impl Drop for CompletionWorker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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
        if let Some(kind) = PickerInvocation::from_state(&self.picker_invocation).item_kind() {
            let (query, replace_start, replace_end) = picker_query_and_span(kind, line, pos);
            return if kind == ItemKind::History {
                if let Some(items) = self.refreshed_history_items() {
                    rank_picker_suggestions(items, query, replace_start, replace_end)
                } else {
                    rank_picker_suggestions_of_kind(
                        &self.picker_items,
                        kind,
                        query,
                        replace_start,
                        replace_end,
                    )
                }
            } else {
                rank_picker_suggestions_of_kind(
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
            .map(|completion| Suggestion {
                value: completion.value,
                display_override: Some(completion.display),
                description: Some(completion.summary),
                extra: Some(vec![completion.detail]),
                span: Span::new(completion.replace_start, completion.replace_end),
                append_whitespace: true,
                match_indices: Some(completion.match_indices),
                ..Suggestion::default()
            })
            .collect::<Vec<_>>();
        if let Some(extensions) = &mut self.extensions {
            suggestions.extend(
                extensions
                    .complete(line, pos)
                    .into_iter()
                    .map(|completion| Suggestion {
                        value: completion.value,
                        display_override: Some(completion.display),
                        description: Some(completion.summary),
                        extra: Some(vec![completion.detail]),
                        span: Span::new(completion.replace_start, completion.replace_end),
                        append_whitespace: true,
                        ..Suggestion::default()
                    }),
            );
        }
        let mut seen = HashSet::new();
        suggestions.retain(|suggestion| seen.insert(suggestion.value.clone()));
        suggestions
    }
}

fn picker_query_and_span(kind: ItemKind, line: &str, pos: usize) -> (&str, usize, usize) {
    let end = pos.min(line.len());
    let before_cursor = line.get(..end).unwrap_or(line);
    if kind != ItemKind::File {
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
    items: &[PickItem],
    query: &str,
    replace_start: usize,
    replace_end: usize,
) -> Vec<Suggestion> {
    Picker
        .rank(items, query)
        .into_iter()
        .map(|matched| {
            let item = &items[matched.index];
            Suggestion {
                value: item.value.as_str().unwrap_or(&item.label).to_owned(),
                display_override: Some(item.label.clone()),
                description: Some(item.description.clone()),
                extra: item.preview.clone().map(|preview| vec![preview]),
                span: Span::new(replace_start, replace_end),
                append_whitespace: false,
                match_indices: Some(matched.match_indices),
                ..Suggestion::default()
            }
        })
        .collect()
}

fn rank_picker_suggestions_of_kind(
    items: &[PickItem],
    kind: ItemKind,
    query: &str,
    replace_start: usize,
    replace_end: usize,
) -> Vec<Suggestion> {
    let mut seen_history_values = HashSet::new();
    let scoped = items
        .iter()
        .filter(|item| {
            item.kind == kind
                && (kind != ItemKind::History || seen_history_values.insert(item.value.as_str()))
        })
        .cloned()
        .collect::<Vec<_>>();
    rank_picker_suggestions(&scoped, query, replace_start, replace_end)
}

fn read_history_picker_items(path: &Path) -> Option<Vec<PickItem>> {
    let source = fs::read_to_string(path).ok()?;
    Some(history_picker_items(&source))
}

fn history_picker_items(source: &str) -> Vec<PickItem> {
    let mut seen = HashSet::new();
    source
        .lines()
        .rev()
        .filter(|line| seen.insert((*line).to_owned()))
        .enumerate()
        .map(|(index, line)| picker_item(index, ItemKind::History, line, "history"))
        .collect()
}

fn picker_sources(catalog: &Catalog, mut items: Vec<PickItem>) -> Vec<PickItem> {
    let next_id = items.len();
    items.extend(
        catalog
            .commands
            .iter()
            .enumerate()
            .map(|(index, command)| PickItem {
                id: format!("action-{}", next_id + index),
                kind: ItemKind::Action,
                label: command.path.clone(),
                description: command.summary.clone(),
                preview: Some(command.details.clone()),
                value: serde_json::Value::String(command.path.clone()),
            }),
    );
    let next_id = items.len();
    if let Ok(entries) = fs::read_dir(".") {
        items.extend(
            entries
                .filter_map(Result::ok)
                .enumerate()
                .map(|(index, entry)| {
                    let path = entry.path();
                    let is_directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
                    PickItem {
                        id: format!("file-{}", next_id + index),
                        kind: ItemKind::File,
                        label: path.to_string_lossy().into_owned(),
                        description: if is_directory {
                            "directory".to_owned()
                        } else {
                            "file".to_owned()
                        },
                        preview: None,
                        value: serde_json::Value::String(path.to_string_lossy().into_owned()),
                    }
                }),
        );
    }
    items
}

fn picker_item(index: usize, kind: ItemKind, value: &str, description: &str) -> PickItem {
    PickItem {
        id: format!("{kind:?}-{index}"),
        kind,
        label: value.to_owned(),
        description: description.to_owned(),
        preview: None,
        value: serde_json::Value::String(value.to_owned()),
    }
}

struct SemanticHighlighter {
    catalog: Catalog,
}

impl SemanticHighlighter {
    fn new(catalog: Catalog) -> Self {
        Self { catalog }
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
            let style = if segment.trim().is_empty() {
                Style::new()
            } else if first_word {
                first_word = false;
                if known {
                    Style::new().bold().fg(Color::Green)
                } else {
                    Style::new().bold().fg(Color::Red)
                }
            } else if segment.starts_with('-') {
                Style::new().fg(Color::Cyan)
            } else if segment.starts_with('"') || segment.starts_with('\'') {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::White)
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

    #[test]
    fn completion_contains_explanatory_metadata() {
        let mut completer = CatalogCompleter::new(Catalog::builtin());
        let result = completer.complete("git c", 5);
        assert_eq!(result[0].value, "git commit");
        assert!(result[0].description.as_deref().unwrap().contains("Record"));
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
    fn dumb_terminal_prompt_join_uses_ascii_only() {
        let parts = vec![
            "project".to_owned(),
            "command".to_owned(),
            "git:main".to_owned(),
        ];
        assert_eq!(
            join_prompt_parts(&parts, true),
            "project | command | git:main"
        );
        assert_eq!(
            join_prompt_parts(&parts, false),
            "project · command · git:main"
        );
    }

    #[test]
    fn terminal_styles_require_an_interactive_color_capable_terminal() {
        assert!(terminal_styling_enabled(true, false, false));
        assert!(!terminal_styling_enabled(false, false, false));
        assert!(!terminal_styling_enabled(true, true, false));
        assert!(!terminal_styling_enabled(true, false, true));
    }

    #[test]
    fn lua_suggestions_merge_with_catalog_completion() {
        let mut completer =
            CatalogCompleter::with_extensions(Catalog::builtin(), Some(Box::new(ExampleExtension)));
        let result = completer.complete("deploy --environment prod", 25);
        assert!(result.iter().any(|suggestion| {
            suggestion.value == "production"
                && suggestion.description.as_deref() == Some("Deployment environment")
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
            dumb_terminal(),
        )));
    }

    #[test]
    fn configured_prompt_orders_native_and_named_segments() {
        let config = QuirlConfig {
            prompt: PromptConfig {
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
        let separator = if dumb_terminal() { " | " } else { " · " };
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
    fn git_state_renders_in_configured_order_with_extension_segments() {
        let config = QuirlConfig {
            prompt: PromptConfig {
                left: vec![
                    "directory".to_owned(),
                    "git_branch".to_owned(),
                    "git_state".to_owned(),
                    "project".to_owned(),
                ],
                right: Vec::new(),
            },
            ..QuirlConfig::default()
        };
        let prompt = QuirlPrompt::with_config(Mode::Command, &config)
            .with_native_context(NativePromptContext {
                directory: "quirl".to_owned(),
                git_branch: Some("main".to_owned()),
                git_state: Some("dirty".to_owned()),
            })
            .with_named_extension_segments(vec![("project".to_owned(), "nsh".to_owned())]);

        assert_eq!(
            prompt.render_prompt_left(),
            format!(
                "{} ",
                join_prompt_parts(
                    &[
                        "quirl".to_owned(),
                        "git:main".to_owned(),
                        "git:dirty".to_owned(),
                        "nsh".to_owned(),
                    ],
                    dumb_terminal(),
                )
            )
        );
    }

    #[test]
    fn editor_accepts_all_configured_keymaps_and_picker_options() {
        for keymap in ["emacs", "vim", "helix"] {
            let config = QuirlConfig {
                editor: EditorConfig {
                    keymap: keymap.to_owned(),
                    semantic_hints: keymap != "vim",
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
                KeyCode::Char(' '),
                KeyModifiers::CONTROL,
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
            picker_item(0, ItemKind::History, "cargo test --workspace", "history"),
            picker_item(1, ItemKind::File, "crates/quirl-ui/src/lib.rs", "file"),
            picker_item(2, ItemKind::Action, "mode data", "action"),
        ];
        let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
        let mut completer = CatalogCompleter::with_extensions_and_picker(
            Catalog::builtin(),
            None,
            items,
            None,
            Arc::clone(&picker_invocation),
        );
        let suggestions = completer.complete("cts", 3);
        assert_eq!(suggestions[0].value, "cargo test --workspace");
        assert_eq!(suggestions[0].span, Span::new(0, 3));

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
        let items = history_picker_items("cargo test\ncargo clippy\ncargo test\n");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].value, "cargo test");
        assert_eq!(items[1].value, "cargo clippy");

        let picker_invocation = Arc::new(AtomicU8::new(PickerInvocation::History as u8));
        let fallback_items = vec![
            picker_item(0, ItemKind::History, "cargo test", "history"),
            picker_item(1, ItemKind::History, "cargo test", "history"),
        ];
        let mut completer = CatalogCompleter::with_extensions_and_picker(
            Catalog::builtin(),
            None,
            fallback_items,
            None,
            picker_invocation,
        );
        let suggestions = completer.complete("cargo", 5);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "cargo test");
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
