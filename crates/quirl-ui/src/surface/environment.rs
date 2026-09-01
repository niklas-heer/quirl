//! Full-frame, bounded exploration of the session environment and command lookup path.

use super::runtime::InteractiveEnvironmentSnapshot;
use crate::{SurfaceSymbols, theme::Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use quirl_core::{ErrorCode, ShellError, escape_terminal_line};
use quirl_syntax::Mode;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
};
use unicode_width::UnicodeWidthStr;

const FILTER_BYTES_MAX: usize = 1_024;
const PATH_DIRECTORIES_MAX: usize = 256;
const PATH_ENTRIES_PER_DIRECTORY_MAX: usize = 4_096;
const PATH_EXECUTABLES_MAX: usize = 65_536;
const PATH_NAME_BYTES_MAX: usize = 1024 * 1024;
const DETAIL_LINES_MAX: usize = 128;
const COPY_BYTES_MAX: usize = 64 * 1024;

/// Result of one explorer input event that must be applied by the owning surface.
pub(super) enum ExplorerAction {
    /// Keep the explorer open after an in-place state change.
    Repaint,
    /// Close the explorer without changing the edit buffer.
    Close,
    /// Insert bounded text into the preserved edit buffer and close the explorer.
    Insert(String),
    /// Copy bounded text while keeping the explorer open.
    Copy(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EnvironmentCategory {
    Health,
    All,
    CommandLookup,
    Project,
    Toolchains,
    TerminalSessions,
    ShellEditor,
    UserDirectories,
    Locale,
    Secrets,
    Other,
}

impl EnvironmentCategory {
    const ORDER: [Self; 11] = [
        Self::Health,
        Self::All,
        Self::CommandLookup,
        Self::Project,
        Self::Toolchains,
        Self::TerminalSessions,
        Self::ShellEditor,
        Self::UserDirectories,
        Self::Locale,
        Self::Secrets,
        Self::Other,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Health => "Health",
            Self::All => "All variables",
            Self::CommandLookup => "Command lookup",
            Self::Project => "Project",
            Self::Toolchains => "Toolchains",
            Self::TerminalSessions => "Terminal & sessions",
            Self::ShellEditor => "Shell & editor",
            Self::UserDirectories => "User directories",
            Self::Locale => "Locale",
            Self::Secrets => "Secrets",
            Self::Other => "Other",
        }
    }
}

#[derive(Debug, Clone)]
struct CategoryGroup {
    category: EnvironmentCategory,
    variables: Vec<usize>,
}

#[derive(Debug, Clone)]
enum HealthIssue {
    EmptyPathEntry {
        position: usize,
    },
    DuplicatePathEntry {
        position: usize,
        first: usize,
    },
    PathDirectoryUnavailable {
        position: usize,
        status: DirectoryStatus,
    },
    PathDirectoryListTruncated {
        observed_at_least: usize,
    },
    PathScanTruncated,
}

impl HealthIssue {
    const fn path_position(&self) -> Option<usize> {
        match self {
            Self::EmptyPathEntry { position }
            | Self::DuplicatePathEntry { position, .. }
            | Self::PathDirectoryUnavailable { position, .. } => Some(*position),
            Self::PathDirectoryListTruncated { .. } | Self::PathScanTruncated => None,
        }
    }

    fn label(&self) -> String {
        match self {
            Self::EmptyPathEntry { position } => {
                format!("PATH[{position}] searches the current directory")
            }
            Self::DuplicatePathEntry { position, first } => {
                format!("PATH[{position}] duplicates PATH[{first}]")
            }
            Self::PathDirectoryUnavailable { position, status } => {
                format!("PATH[{position}] is {}", status.label())
            }
            Self::PathDirectoryListTruncated { observed_at_least } => format!(
                "PATH has at least {observed_at_least} entries; explorer limit is {PATH_DIRECTORIES_MAX}"
            ),
            Self::PathScanTruncated => "PATH scan reached a resource limit".to_owned(),
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::EmptyPathEntry { .. } => concat!(
                "An empty PATH component resolves to the current directory. ",
                "That makes command resolution depend on where the shell is and can run an ",
                "unexpected local executable."
            )
            .to_owned(),
            Self::DuplicatePathEntry { .. } => concat!(
                "The same directory occurs more than once. The later entry cannot win command ",
                "resolution and adds redundant filesystem work."
            )
            .to_owned(),
            Self::PathDirectoryUnavailable { status, .. } => format!(
                "Quirl could not use this PATH directory during the bounded scan: {}.",
                status.label()
            ),
            Self::PathDirectoryListTruncated { .. } => concat!(
                "The explorer retained the leading PATH entries that affect command lookup ",
                "first and omitted the remaining entries from filesystem scanning."
            )
            .to_owned(),
            Self::PathScanTruncated => concat!(
                "The explorer stopped before retaining an incomplete unbounded result. ",
                "Absence and shadowing information may therefore be incomplete."
            )
            .to_owned(),
        }
    }

    fn recommendation(&self) -> &'static str {
        match self {
            Self::EmptyPathEntry { .. } => {
                "Replace the empty component with an explicit directory, or remove it."
            }
            Self::DuplicatePathEntry { .. } => {
                "Remove the later duplicate from the startup file that builds PATH."
            }
            Self::PathDirectoryUnavailable {
                status: DirectoryStatus::Missing,
                ..
            } => "Remove the stale entry or reinstall the tool that owns this directory.",
            Self::PathDirectoryUnavailable {
                status: DirectoryStatus::NotDirectory,
                ..
            } => "Point PATH at a directory rather than this filesystem entry.",
            Self::PathDirectoryUnavailable {
                status: DirectoryStatus::Unreadable,
                ..
            } => "Check directory permissions before relying on commands stored here.",
            Self::PathDirectoryUnavailable {
                status: DirectoryStatus::Ready | DirectoryStatus::Truncated,
                ..
            }
            | Self::PathScanTruncated => {
                "Narrow PATH or refresh after filesystem activity has settled."
            }
            Self::PathDirectoryListTruncated { .. } => {
                "Shorten PATH so its complete lookup order can be inspected."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ExplorerFocus {
    Categories,
    Variables,
    PathDirectories,
    Executables,
    Commands,
}

#[derive(Debug, Clone)]
struct PathEntry {
    position: usize,
    display: String,
    path: PathBuf,
    empty: bool,
    duplicate_of: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DirectoryStatus {
    Ready,
    Missing,
    NotDirectory,
    Unreadable,
    Truncated,
}

impl DirectoryStatus {
    const fn label(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::NotDirectory => "not a directory",
            Self::Unreadable => "unreadable",
            Self::Truncated => "partially scanned",
        }
    }

    const fn glyph(&self, symbols: SurfaceSymbols) -> &'static str {
        match (self, symbols) {
            (Self::Ready, SurfaceSymbols::Plain) => "ok",
            (Self::Ready, _) => "✓",
            (Self::Missing | Self::NotDirectory, SurfaceSymbols::Plain) => "!!",
            (Self::Missing | Self::NotDirectory, _) => "×",
            (Self::Unreadable | Self::Truncated, SurfaceSymbols::Plain) => "!",
            (Self::Unreadable | Self::Truncated, _) => "!",
        }
    }
}

#[derive(Debug, Clone)]
struct ScannedDirectory {
    status: DirectoryStatus,
    commands: Vec<String>,
}

#[derive(Debug, Default)]
struct PathScanSnapshot {
    directories: Vec<ScannedDirectory>,
    candidates: BTreeMap<String, Vec<usize>>,
    truncated: bool,
}

struct PathScanWorker {
    response: Receiver<PathScanSnapshot>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl PathScanWorker {
    fn start(entries: Vec<PathEntry>) -> Result<Self, ShellError> {
        let (sender, response) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let worker = thread::Builder::new()
            .name("quirl-environment-path-scan".to_owned())
            .spawn(move || {
                let snapshot = scan_path_entries(&entries, &worker_cancel);
                let _ = sender.send(snapshot);
            })
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "cannot start the environment PATH scanner")
                    .with_context(error.to_string())
                    .with_help("Retry after freeing process or thread resources")
            })?;
        Ok(Self {
            response,
            cancel,
            worker: Some(worker),
        })
    }

    fn poll(&mut self) -> Option<PathScanSnapshot> {
        match self.response.try_recv() {
            Ok(snapshot) => {
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| worker.is_finished())
                {
                    let _ = self.worker.take().map(JoinHandle::join);
                } else {
                    self.worker.take();
                }
                Some(snapshot)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                let _ = self.worker.take().map(JoinHandle::join);
                Some(PathScanSnapshot {
                    truncated: true,
                    ..PathScanSnapshot::default()
                })
            }
        }
    }
}

impl Drop for PathScanWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        // A filesystem call may be blocked inside the operating system. Detach
        // rather than delaying terminal restoration; the worker checks
        // cancellation between every bounded directory and entry operation.
        self.worker.take();
    }
}

/// Session-local state for the full-screen Environment Explorer.
pub(super) struct EnvironmentExplorer {
    active: bool,
    variables: Vec<InteractiveEnvironmentSnapshot>,
    categories: Vec<CategoryGroup>,
    health: Vec<HealthIssue>,
    focus: ExplorerFocus,
    category_selected: usize,
    variable_selected: usize,
    path_selected: usize,
    executable_selected: usize,
    command_selected: usize,
    filter: String,
    search_active: bool,
    actions_visible: bool,
    revealed_secrets: HashSet<usize>,
    path_entries: Vec<PathEntry>,
    path_entries_truncated: bool,
    path_snapshot: Option<PathScanSnapshot>,
    path_worker: Option<PathScanWorker>,
    notice: Option<String>,
}

impl EnvironmentExplorer {
    pub(super) fn new() -> Self {
        Self {
            active: false,
            variables: Vec::new(),
            categories: Vec::new(),
            health: Vec::new(),
            focus: ExplorerFocus::Categories,
            category_selected: 0,
            variable_selected: 0,
            path_selected: 0,
            executable_selected: 0,
            command_selected: 0,
            filter: String::new(),
            search_active: false,
            actions_visible: false,
            revealed_secrets: HashSet::new(),
            path_entries: Vec::new(),
            path_entries_truncated: false,
            path_snapshot: None,
            path_worker: None,
            notice: None,
        }
    }

    pub(super) const fn active(&self) -> bool {
        self.active
    }

    pub(super) fn open(
        &mut self,
        variables: &[InteractiveEnvironmentSnapshot],
    ) -> Result<(), ShellError> {
        self.close();
        self.variables = variables.to_vec();
        self.path_entries = path_entries(&self.variables);
        self.path_entries_truncated = path_entry_limit_reached(&self.variables);
        self.health = health_issues(&self.path_entries, None, self.path_entries_truncated);
        self.categories = category_groups(&self.variables);
        self.category_selected = self
            .categories
            .iter()
            .position(|group| group.category == EnvironmentCategory::CommandLookup)
            .unwrap_or(0);
        self.active = true;
        self.request_path_scan()?;
        Ok(())
    }

    pub(super) fn close(&mut self) {
        self.active = false;
        self.variables.clear();
        self.categories.clear();
        self.health.clear();
        self.focus = ExplorerFocus::Categories;
        self.category_selected = 0;
        self.variable_selected = 0;
        self.path_selected = 0;
        self.executable_selected = 0;
        self.command_selected = 0;
        self.filter.clear();
        self.search_active = false;
        self.actions_visible = false;
        self.revealed_secrets.clear();
        self.path_entries.clear();
        self.path_entries_truncated = false;
        self.path_snapshot = None;
        self.path_worker = None;
        self.notice = None;
    }

    pub(super) fn poll(&mut self) -> bool {
        let Some(snapshot) = self.path_worker.as_mut().and_then(PathScanWorker::poll) else {
            return false;
        };
        self.path_worker = None;
        self.health = health_issues(
            &self.path_entries,
            Some(&snapshot),
            self.path_entries_truncated,
        );
        let selected_category = self.selected_category();
        self.categories = category_groups(&self.variables);
        self.category_selected = self
            .categories
            .iter()
            .position(|group| group.category == selected_category)
            .unwrap_or(0);
        if snapshot.truncated {
            self.notice = Some("PATH scan reached a configured resource limit".to_owned());
        } else {
            self.notice = Some("PATH scan complete".to_owned());
        }
        self.path_snapshot = Some(snapshot);
        self.clamp_selections();
        true
    }

    pub(super) fn insert_filter_text(&mut self, text: &str) {
        self.search_active = true;
        let text = text.replace(['\r', '\n'], " ");
        let available = FILTER_BYTES_MAX.saturating_sub(self.filter.len());
        let end = utf8_boundary_at_or_before(&text, available.min(text.len()));
        self.filter.push_str(text.get(..end).unwrap_or_default());
        self.reset_current_selection();
    }

    pub(super) fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> Result<ExplorerAction, ShellError> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('g'))
        {
            return Ok(ExplorerAction::Close);
        }
        if self.search_active {
            return Ok(self.handle_search_key(key));
        }
        match key.code {
            KeyCode::Char('q') => Ok(ExplorerAction::Close),
            KeyCode::Esc => Ok(self.back_or_close()),
            KeyCode::Left | KeyCode::Backspace | KeyCode::BackTab => Ok(self.move_back()),
            KeyCode::Right | KeyCode::Enter | KeyCode::Tab => self.drill_down(),
            KeyCode::Up => {
                self.move_selection(-1);
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Down => {
                self.move_selection(1);
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Home => {
                self.set_current_selection(0);
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::End => {
                self.set_current_selection(self.current_row_count().saturating_sub(1));
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Char('/') => {
                self.search_active = true;
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Char('g') => {
                self.select_category(EnvironmentCategory::All);
                self.focus = ExplorerFocus::Variables;
                self.search_active = true;
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Char('w') => {
                self.open_command_resolution()?;
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Char('a') | KeyCode::Char('?') => {
                self.actions_visible = !self.actions_visible;
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Char('v') => {
                self.toggle_secret_reveal();
                Ok(ExplorerAction::Repaint)
            }
            KeyCode::Char('y') => Ok(self
                .copy_selected()
                .map_or(ExplorerAction::Repaint, ExplorerAction::Copy)),
            KeyCode::Char('i') => Ok(self
                .selected_insertion()
                .map_or(ExplorerAction::Repaint, ExplorerAction::Insert)),
            KeyCode::Char('r') => {
                self.refresh_path_scan()?;
                Ok(ExplorerAction::Repaint)
            }
            _ => Ok(ExplorerAction::Repaint),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> ExplorerAction {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.search_active = false,
            KeyCode::Backspace => {
                self.filter.pop();
                self.reset_current_selection();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.clear();
                self.reset_current_selection();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let text = character.to_string();
                if self.filter.len().saturating_add(text.len()) <= FILTER_BYTES_MAX {
                    self.filter.push(character);
                    self.reset_current_selection();
                }
            }
            _ => {}
        }
        ExplorerAction::Repaint
    }

    fn drill_down(&mut self) -> Result<ExplorerAction, ShellError> {
        self.clear_filter_preserving_selection();
        match self.focus {
            ExplorerFocus::Categories => {
                self.focus = ExplorerFocus::Variables;
                self.variable_selected = 0;
            }
            ExplorerFocus::Variables if self.selected_variable_name() == Some("PATH") => {
                self.focus = ExplorerFocus::PathDirectories;
                self.path_selected = 0;
                self.request_path_scan()?;
            }
            ExplorerFocus::Variables if self.selected_category() == EnvironmentCategory::Health => {
                self.open_selected_health_path()?;
            }
            ExplorerFocus::PathDirectories => {
                self.focus = ExplorerFocus::Executables;
                self.executable_selected = 0;
                self.request_path_scan()?;
            }
            ExplorerFocus::Variables | ExplorerFocus::Executables | ExplorerFocus::Commands => {}
        }
        Ok(ExplorerAction::Repaint)
    }

    fn back_or_close(&mut self) -> ExplorerAction {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.reset_current_selection();
            return ExplorerAction::Repaint;
        }
        if self.focus == ExplorerFocus::Categories {
            ExplorerAction::Close
        } else {
            self.move_back()
        }
    }

    fn move_back(&mut self) -> ExplorerAction {
        self.clear_filter_preserving_selection();
        self.focus = match self.focus {
            ExplorerFocus::Categories => return ExplorerAction::Close,
            ExplorerFocus::Variables => ExplorerFocus::Categories,
            ExplorerFocus::PathDirectories => ExplorerFocus::Variables,
            ExplorerFocus::Executables => ExplorerFocus::PathDirectories,
            ExplorerFocus::Commands => ExplorerFocus::Variables,
        };
        ExplorerAction::Repaint
    }

    fn clear_filter_preserving_selection(&mut self) {
        if self.filter.is_empty() {
            return;
        }
        let category = self.selected_category();
        let variable_index = self.selected_variable_index();
        let health_index = self
            .filtered_health_indices()
            .get(self.variable_selected)
            .copied();
        let path_index = self.selected_directory_index();
        let executable = self.selected_executable().map(str::to_owned);
        let command = self.selected_command().map(str::to_owned);
        self.filter.clear();
        match self.focus {
            ExplorerFocus::Categories => {
                self.category_selected = self
                    .categories
                    .iter()
                    .position(|group| group.category == category)
                    .unwrap_or(0);
            }
            ExplorerFocus::Variables if category == EnvironmentCategory::Health => {
                self.variable_selected = health_index.unwrap_or(0);
            }
            ExplorerFocus::Variables => {
                self.variable_selected = variable_index
                    .and_then(|index| {
                        self.categories
                            .iter()
                            .find(|group| group.category == category)?
                            .variables
                            .iter()
                            .position(|candidate| *candidate == index)
                    })
                    .unwrap_or(0);
            }
            ExplorerFocus::PathDirectories => {
                self.path_selected = path_index.unwrap_or(0);
            }
            ExplorerFocus::Executables => {
                self.executable_selected = executable
                    .as_deref()
                    .and_then(|selected| {
                        self.selected_directory_commands()
                            .iter()
                            .position(|command| command == selected)
                    })
                    .unwrap_or(0);
            }
            ExplorerFocus::Commands => {
                self.command_selected = command
                    .as_deref()
                    .and_then(|selected| {
                        self.resolved_command_names()
                            .iter()
                            .position(|command| command.as_str() == selected)
                    })
                    .unwrap_or(0);
            }
        }
    }

    fn open_selected_health_path(&mut self) -> Result<(), ShellError> {
        let path_index = self
            .selected_health_issue()
            .and_then(HealthIssue::path_position)
            .and_then(|position| position.checked_sub(1));
        self.select_path_variable();
        self.focus = ExplorerFocus::PathDirectories;
        self.path_selected = path_index
            .unwrap_or(0)
            .min(self.path_entries.len().saturating_sub(1));
        self.request_path_scan()
    }

    fn open_command_resolution(&mut self) -> Result<(), ShellError> {
        self.clear_filter_preserving_selection();
        self.select_path_variable();
        self.focus = ExplorerFocus::Commands;
        self.command_selected = 0;
        self.search_active = true;
        self.request_path_scan()
    }

    fn select_path_variable(&mut self) {
        self.select_category(EnvironmentCategory::CommandLookup);
        self.variable_selected = self
            .categories
            .iter()
            .find(|group| group.category == EnvironmentCategory::CommandLookup)
            .and_then(|group| {
                group.variables.iter().position(|index| {
                    self.variables
                        .get(*index)
                        .is_some_and(|variable| variable.name == "PATH")
                })
            })
            .unwrap_or(0);
    }

    fn request_path_scan(&mut self) -> Result<(), ShellError> {
        if self.path_entries.is_empty() {
            self.notice = Some("PATH is unset or has no retained directories".to_owned());
            return Ok(());
        }
        if self.path_snapshot.is_some() {
            return Ok(());
        }
        if self.path_worker.is_some() {
            self.notice = Some("PATH scan is already running".to_owned());
            return Ok(());
        }
        self.path_worker = Some(PathScanWorker::start(self.path_entries.clone())?);
        self.notice = Some("scanning PATH with bounded filesystem work…".to_owned());
        Ok(())
    }

    fn refresh_path_scan(&mut self) -> Result<(), ShellError> {
        self.path_worker = None;
        self.path_snapshot = None;
        self.health = health_issues(&self.path_entries, None, self.path_entries_truncated);
        self.request_path_scan()
    }

    fn selected_category(&self) -> EnvironmentCategory {
        self.filtered_category_indices()
            .get(self.category_selected)
            .and_then(|index| self.categories.get(*index))
            .map_or(EnvironmentCategory::All, |group| group.category)
    }

    fn select_category(&mut self, category: EnvironmentCategory) {
        self.filter.clear();
        self.category_selected = self
            .categories
            .iter()
            .position(|group| group.category == category)
            .unwrap_or(0);
        self.variable_selected = 0;
    }

    fn selected_variable_index(&self) -> Option<usize> {
        self.filtered_variable_indices()
            .get(self.variable_selected)
            .copied()
    }

    fn selected_variable_name(&self) -> Option<&str> {
        let index = self.selected_variable_index()?;
        self.variables
            .get(index)
            .map(|variable| variable.name.as_str())
    }

    fn filtered_category_indices(&self) -> Vec<usize> {
        let filter = self.filter_for(ExplorerFocus::Categories);
        self.categories
            .iter()
            .enumerate()
            .filter(|(_, group)| matches_filter(group.category.label(), filter))
            .map(|(index, _)| index)
            .collect()
    }

    fn filtered_variable_indices(&self) -> Vec<usize> {
        let filter = self.filter_for(ExplorerFocus::Variables);
        let category = self.selected_category();
        let Some(group) = self
            .categories
            .iter()
            .find(|group| group.category == category)
        else {
            return Vec::new();
        };
        group
            .variables
            .iter()
            .copied()
            .filter(|index| {
                self.variables.get(*index).is_some_and(|variable| {
                    matches_filter(&variable.name, filter)
                        || (!is_sensitive_name(&variable.name)
                            && matches_filter(&variable.value, filter))
                })
            })
            .collect()
    }

    fn filtered_health_indices(&self) -> Vec<usize> {
        let filter = self.filter_for(ExplorerFocus::Variables);
        self.health
            .iter()
            .enumerate()
            .filter(|(_, issue)| matches_filter(&issue.label(), filter))
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_health_issue(&self) -> Option<&HealthIssue> {
        let index = self
            .filtered_health_indices()
            .get(self.variable_selected)
            .copied()?;
        self.health.get(index)
    }

    fn filtered_path_indices(&self) -> Vec<usize> {
        let filter = self.filter_for(ExplorerFocus::PathDirectories);
        self.path_entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| matches_filter(&entry.display, filter))
            .map(|(index, _)| index)
            .collect()
    }

    fn filtered_executable_indices(&self) -> Vec<usize> {
        let filter = self.filter_for(ExplorerFocus::Executables);
        self.selected_directory_commands()
            .iter()
            .enumerate()
            .filter(|(_, command)| matches_filter(command, filter))
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_directory_index(&self) -> Option<usize> {
        self.filtered_path_indices()
            .get(self.path_selected)
            .copied()
    }

    fn filter_for(&self, focus: ExplorerFocus) -> &str {
        if self.focus == focus {
            &self.filter
        } else {
            ""
        }
    }

    fn selected_directory_commands(&self) -> &[String] {
        let Some(directory_index) = self.selected_directory_index() else {
            return &[];
        };
        self.path_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.directories.get(directory_index))
            .map_or(&[], |directory| directory.commands.as_slice())
    }

    fn selected_executable(&self) -> Option<&str> {
        let command_index = self
            .filtered_executable_indices()
            .get(self.executable_selected)
            .copied()?;
        self.selected_directory_commands()
            .get(command_index)
            .map(String::as_str)
    }

    fn resolved_command_names(&self) -> Vec<&String> {
        let filter = self.filter_for(ExplorerFocus::Commands);
        self.path_snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .candidates
                    .keys()
                    .filter(|command| matches_filter(command, filter))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn selected_command(&self) -> Option<&str> {
        self.resolved_command_names()
            .get(self.command_selected)
            .map(|command| command.as_str())
    }

    fn command_candidates(&self, command: &str) -> &[usize] {
        self.path_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.candidates.get(command))
            .map_or(&[], Vec::as_slice)
    }

    fn command_winner_index(&self, command: &str) -> Option<usize> {
        self.command_candidates(command).first().copied()
    }

    fn command_path(&self, command: &str, directory_index: usize) -> Option<PathBuf> {
        self.path_entries
            .get(directory_index)
            .map(|entry| entry.path.join(command))
    }

    fn resolution_counts(&self) -> (usize, usize) {
        self.path_snapshot.as_ref().map_or((0, 0), |snapshot| {
            let shadowed = snapshot
                .candidates
                .values()
                .filter(|candidates| candidates.len() > 1)
                .count();
            (snapshot.candidates.len(), shadowed)
        })
    }

    fn health_scan_pending(&self) -> bool {
        self.path_worker.is_some()
    }

    fn health_scan_complete(&self) -> bool {
        self.path_snapshot.is_some() || self.path_entries.is_empty()
    }

    fn current_row_count(&self) -> usize {
        match self.focus {
            ExplorerFocus::Categories => self.filtered_category_indices().len(),
            ExplorerFocus::Variables if self.selected_category() == EnvironmentCategory::Health => {
                self.filtered_health_indices().len()
            }
            ExplorerFocus::Variables => self.filtered_variable_indices().len(),
            ExplorerFocus::PathDirectories => self.filtered_path_indices().len(),
            ExplorerFocus::Executables => self.filtered_executable_indices().len(),
            ExplorerFocus::Commands => self.resolved_command_names().len(),
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.current_row_count();
        if count == 0 {
            self.set_current_selection(0);
            return;
        }
        let current = self.current_selection();
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta.unsigned_abs())
        };
        self.set_current_selection(next.min(count.saturating_sub(1)));
        if self.focus == ExplorerFocus::Categories {
            self.variable_selected = 0;
        } else if self.focus == ExplorerFocus::PathDirectories {
            self.executable_selected = 0;
        }
    }

    const fn current_selection(&self) -> usize {
        match self.focus {
            ExplorerFocus::Categories => self.category_selected,
            ExplorerFocus::Variables => self.variable_selected,
            ExplorerFocus::PathDirectories => self.path_selected,
            ExplorerFocus::Executables => self.executable_selected,
            ExplorerFocus::Commands => self.command_selected,
        }
    }

    fn set_current_selection(&mut self, value: usize) {
        match self.focus {
            ExplorerFocus::Categories => self.category_selected = value,
            ExplorerFocus::Variables => self.variable_selected = value,
            ExplorerFocus::PathDirectories => self.path_selected = value,
            ExplorerFocus::Executables => self.executable_selected = value,
            ExplorerFocus::Commands => self.command_selected = value,
        }
    }

    fn reset_current_selection(&mut self) {
        self.set_current_selection(0);
        self.clamp_selections();
    }

    fn clamp_selections(&mut self) {
        let count = self.current_row_count();
        self.set_current_selection(self.current_selection().min(count.saturating_sub(1)));
    }

    fn toggle_secret_reveal(&mut self) {
        let Some(index) = self.selected_variable_index() else {
            return;
        };
        let Some(variable) = self.variables.get(index) else {
            return;
        };
        if !is_sensitive_name(&variable.name) {
            self.notice = Some("the selected value is already visible".to_owned());
            return;
        }
        if !self.revealed_secrets.insert(index) {
            self.revealed_secrets.remove(&index);
        }
    }

    fn copy_selected(&mut self) -> Option<String> {
        if self.focus == ExplorerFocus::Variables
            && self.selected_category() != EnvironmentCategory::Health
        {
            let index = self.selected_variable_index()?;
            let variable = self.variables.get(index)?;
            if is_sensitive_name(&variable.name) && !self.revealed_secrets.contains(&index) {
                self.notice =
                    Some("reveal this sensitive value with v before copying it".to_owned());
                return None;
            }
        }
        let value = match self.focus {
            ExplorerFocus::Variables if self.selected_category() == EnvironmentCategory::Health => {
                let position = self.selected_health_issue()?.path_position()?;
                self.path_entries
                    .get(position.checked_sub(1)?)?
                    .path
                    .display()
                    .to_string()
            }
            ExplorerFocus::Variables => {
                let index = self.selected_variable_index()?;
                self.variables.get(index)?.value.clone()
            }
            ExplorerFocus::PathDirectories => {
                let index = self.selected_directory_index()?;
                self.path_entries.get(index)?.path.display().to_string()
            }
            ExplorerFocus::Executables => {
                let directory_index = self.selected_directory_index()?;
                let command = self.selected_executable()?;
                self.path_entries
                    .get(directory_index)?
                    .path
                    .join(command)
                    .display()
                    .to_string()
            }
            ExplorerFocus::Commands => {
                let command = self.selected_command()?;
                let winner = self.command_winner_index(command)?;
                self.command_path(command, winner)?.display().to_string()
            }
            ExplorerFocus::Categories => return None,
        };
        (value.len() <= COPY_BYTES_MAX).then_some(value)
    }

    fn selected_insertion(&self) -> Option<String> {
        match self.focus {
            ExplorerFocus::Variables if self.selected_category() == EnvironmentCategory::Health => {
                let position = self.selected_health_issue()?.path_position()?;
                self.path_entries
                    .get(position.checked_sub(1)?)
                    .map(|entry| shell_quote(&entry.path.display().to_string()))
            }
            ExplorerFocus::Variables => {
                let name = self.selected_variable_name()?;
                valid_environment_name(name).then(|| format!("${{{name}}}"))
            }
            ExplorerFocus::PathDirectories => {
                let index = self.selected_directory_index()?;
                self.path_entries
                    .get(index)
                    .map(|entry| shell_quote(&entry.path.display().to_string()))
            }
            ExplorerFocus::Executables => {
                let directory_index = self.selected_directory_index()?;
                let command = self.selected_executable()?;
                self.path_entries
                    .get(directory_index)
                    .map(|entry| shell_quote(&entry.path.join(command).display().to_string()))
            }
            ExplorerFocus::Commands => {
                let command = self.selected_command()?;
                let winner = self.command_winner_index(command)?;
                self.command_path(command, winner)
                    .map(|path| shell_quote(&path.display().to_string()))
            }
            ExplorerFocus::Categories => None,
        }
    }

    pub(super) fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        frame.render_widget(Clear, area);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let header_height = area.height.min(2);
        let footer_height = area.height.saturating_sub(header_height).min(2);
        let content_height = area
            .height
            .saturating_sub(header_height.saturating_add(footer_height));
        let header = Rect::new(area.x, area.y, area.width, header_height);
        let content = Rect::new(
            area.x,
            area.y.saturating_add(header_height),
            area.width,
            content_height,
        );
        let footer = Rect::new(area.x, content.bottom(), area.width, footer_height);
        self.render_header(frame, header, theme, mode, symbols);
        self.render_columns(frame, content, theme, mode, symbols);
        self.render_footer(frame, footer, theme);
    }

    fn render_header(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        let breadcrumb = self.breadcrumb();
        let title = Line::from(vec![
            Span::styled(
                match symbols {
                    SurfaceSymbols::NerdFont => "\u{f0ac} Environment Explorer  ",
                    SurfaceSymbols::Unicode => "◎ Environment Explorer  ",
                    SurfaceSymbols::Plain => "Environment Explorer  ",
                },
                theme.accent(mode),
            ),
            Span::styled(breadcrumb, theme.context_secondary()),
        ]);
        let query = Line::from(vec![
            Span::styled(
                if self.search_active {
                    "filter › "
                } else {
                    "/ filter  "
                },
                theme.dim(),
            ),
            Span::styled(escape_terminal_line(&self.filter), theme.context()),
        ]);
        frame.render_widget(Paragraph::new(vec![title, query]), area);
    }

    fn render_columns(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        if area.height < 3 {
            return;
        }
        if area.width < 70 {
            let list_height = area.height.saturating_mul(3) / 5;
            let list_area = Rect::new(area.x, area.y, area.width, list_height);
            let detail_area = Rect::new(
                area.x,
                area.y.saturating_add(list_height),
                area.width,
                area.height.saturating_sub(list_height),
            );
            let current = self.current_column(symbols);
            render_column(frame, list_area, &current, true, theme, mode, symbols);
            render_detail(frame, detail_area, self.detail_lines(), theme, mode);
            return;
        }
        if area.width < 110 {
            let left_width = area.width.saturating_mul(2) / 5;
            let list_area = Rect::new(area.x, area.y, left_width, area.height);
            let detail_area = Rect::new(
                area.x.saturating_add(left_width),
                area.y,
                area.width.saturating_sub(left_width),
                area.height,
            );
            let current = self.current_column(symbols);
            render_column(frame, list_area, &current, true, theme, mode, symbols);
            render_detail(frame, detail_area, self.detail_lines(), theme, mode);
            return;
        }
        let first_width = area.width / 4;
        let second_width = area.width.saturating_mul(3) / 10;
        let third_width = area
            .width
            .saturating_sub(first_width.saturating_add(second_width));
        let first_area = Rect::new(area.x, area.y, first_width, area.height);
        let second_area = Rect::new(first_area.right(), area.y, second_width, area.height);
        let third_area = Rect::new(second_area.right(), area.y, third_width, area.height);
        let (first, second) = self.context_columns(symbols);
        render_column(
            frame,
            first_area,
            &first,
            self.focus == first.focus,
            theme,
            mode,
            symbols,
        );
        render_column(
            frame,
            second_area,
            &second,
            self.focus == second.focus,
            theme,
            mode,
            symbols,
        );
        render_detail(frame, third_area, self.detail_lines(), theme, mode);
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, theme: Theme) {
        if area.height == 0 {
            return;
        }
        let keys = if self.search_active {
            "Filtering focused column · type to narrow · Enter finish · Ctrl-U clear · Esc finish"
        } else if self.actions_visible {
            "Enter/→ drill · ← back · / filter · w which · g variables · y copy · i insert · v reveal · r refresh · q close"
        } else {
            match self.focus {
                ExplorerFocus::Categories => {
                    "Enter/→ open · / filter · w resolve command · g find variable · a all actions · q close"
                }
                ExplorerFocus::Variables
                    if self.selected_category() == EnvironmentCategory::Health =>
                {
                    "Enter/→ jump to PATH entry · y copy path · i insert path · r refresh · ← categories"
                }
                ExplorerFocus::Variables if self.selected_variable_name() == Some("PATH") => {
                    "Enter/→ browse directories · w resolve command · y copy PATH · i insert ${PATH} · ← categories"
                }
                ExplorerFocus::Variables => {
                    "y copy value · i insert ${NAME} · v reveal sensitive value · / filter · ← categories"
                }
                ExplorerFocus::PathDirectories => {
                    "Enter/→ list directory commands · w resolve globally · y copy path · i insert path · ← PATH"
                }
                ExplorerFocus::Executables => {
                    "y copy candidate · i insert exact candidate · / filter · ← directories"
                }
                ExplorerFocus::Commands => {
                    "/ filter commands · y copy winner · i insert exact winner · r refresh · ← PATH"
                }
            }
        };
        let mut lines = vec![Line::styled(keys, theme.status())];
        if area.height > 1 {
            lines.push(Line::styled(
                self.notice.as_deref().unwrap_or(
                    "Read-only explorer · changes are inserted into the command buffer for review",
                ),
                theme.dim(),
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn breadcrumb(&self) -> String {
        let mut parts = vec![self.selected_category().label().to_owned()];
        if self.focus != ExplorerFocus::Categories
            && let Some(name) = self.selected_variable_name()
        {
            parts.push(name.to_owned());
        }
        if matches!(
            self.focus,
            ExplorerFocus::PathDirectories | ExplorerFocus::Executables
        ) && let Some(index) = self.selected_directory_index()
            && let Some(entry) = self.path_entries.get(index)
        {
            parts.push(format!("{:02} {}", entry.position, entry.display));
        }
        if self.focus == ExplorerFocus::Executables
            && let Some(command) = self.selected_executable()
        {
            parts.push(command.to_owned());
        }
        if self.focus == ExplorerFocus::Commands {
            parts.push("Resolved commands".to_owned());
            if let Some(command) = self.selected_command() {
                parts.push(command.to_owned());
            }
        }
        parts.join(" / ")
    }

    fn context_columns(&self, symbols: SurfaceSymbols) -> (ExplorerColumn, ExplorerColumn) {
        match self.focus {
            ExplorerFocus::Categories | ExplorerFocus::Variables => {
                (self.category_column(), self.variable_column())
            }
            ExplorerFocus::PathDirectories => (self.variable_column(), self.path_column(symbols)),
            ExplorerFocus::Executables => (self.path_column(symbols), self.executable_column()),
            ExplorerFocus::Commands => (self.variable_column(), self.command_column()),
        }
    }

    fn current_column(&self, symbols: SurfaceSymbols) -> ExplorerColumn {
        match self.focus {
            ExplorerFocus::Categories => self.category_column(),
            ExplorerFocus::Variables => self.variable_column(),
            ExplorerFocus::PathDirectories => self.path_column(symbols),
            ExplorerFocus::Executables => self.executable_column(),
            ExplorerFocus::Commands => self.command_column(),
        }
    }

    fn category_column(&self) -> ExplorerColumn {
        let indices = self.filtered_category_indices();
        let rows = indices
            .iter()
            .filter_map(|index| self.categories.get(*index))
            .map(|group| ExplorerRow {
                label: group.category.label().to_owned(),
                summary: if group.category == EnvironmentCategory::Health {
                    if self.health_scan_pending() {
                        "scanning…".to_owned()
                    } else if self.health_scan_complete() && self.health.is_empty() {
                        "clean".to_owned()
                    } else if self.health_scan_complete() {
                        self.health.len().to_string()
                    } else {
                        "not checked".to_owned()
                    }
                } else {
                    group.variables.len().to_string()
                },
                warning: group.category == EnvironmentCategory::Health && !self.health.is_empty(),
            })
            .collect();
        ExplorerColumn {
            title: "Categories".to_owned(),
            rows,
            selected: self.category_selected,
            focus: ExplorerFocus::Categories,
        }
    }

    fn variable_column(&self) -> ExplorerColumn {
        if self.selected_category() == EnvironmentCategory::Health {
            let mut rows = self
                .filtered_health_indices()
                .iter()
                .filter_map(|index| self.health.get(*index))
                .map(|issue| ExplorerRow {
                    label: issue.label(),
                    summary: "warning".to_owned(),
                    warning: true,
                })
                .collect::<Vec<_>>();
            if self.health_scan_pending() {
                rows.push(ExplorerRow {
                    label: "Scanning PATH directories…".to_owned(),
                    summary: format!("{} total", self.path_entries.len()),
                    warning: false,
                });
            } else if self.health_scan_complete() && rows.is_empty() {
                rows.push(ExplorerRow {
                    label: "No environment findings".to_owned(),
                    summary: "clean".to_owned(),
                    warning: false,
                });
            }
            return ExplorerColumn {
                title: "Health".to_owned(),
                rows,
                selected: self.variable_selected,
                focus: ExplorerFocus::Variables,
            };
        }
        let rows = self
            .filtered_variable_indices()
            .iter()
            .filter_map(|index| {
                self.variables
                    .get(*index)
                    .map(|variable| (*index, variable))
            })
            .map(|(index, variable)| ExplorerRow {
                label: variable.name.clone(),
                summary: variable_summary(variable, self.revealed_secrets.contains(&index)),
                warning: false,
            })
            .collect();
        ExplorerColumn {
            title: self.selected_category().label().to_owned(),
            rows,
            selected: self.variable_selected,
            focus: ExplorerFocus::Variables,
        }
    }

    fn path_column(&self, symbols: SurfaceSymbols) -> ExplorerColumn {
        let rows = self
            .filtered_path_indices()
            .iter()
            .filter_map(|index| self.path_entries.get(*index).map(|entry| (*index, entry)))
            .map(|(index, entry)| {
                let status = self
                    .path_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.directories.get(index))
                    .map(|directory| directory.status.clone());
                let summary = if let Some(first) = entry.duplicate_of {
                    format!("dup {first}")
                } else if entry.empty {
                    "cwd".to_owned()
                } else if let Some(status) = status.as_ref() {
                    format!("{} {}", status.glyph(symbols), status.label())
                } else if self.path_worker.is_some() {
                    "scanning…".to_owned()
                } else {
                    "not scanned".to_owned()
                };
                ExplorerRow {
                    label: format!("{:02} {}", entry.position, entry.display),
                    summary,
                    warning: entry.empty
                        || entry.duplicate_of.is_some()
                        || status.is_some_and(|status| status != DirectoryStatus::Ready),
                }
            })
            .collect();
        ExplorerColumn {
            title: format!("PATH · {} directories", self.path_entries.len()),
            rows,
            selected: self.path_selected,
            focus: ExplorerFocus::PathDirectories,
        }
    }

    fn executable_column(&self) -> ExplorerColumn {
        let commands = self.selected_directory_commands();
        let rows = self
            .filtered_executable_indices()
            .iter()
            .filter_map(|index| commands.get(*index))
            .map(|command| {
                let candidates = self
                    .path_snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.candidates.get(command));
                let selected_directory = self.selected_directory_index();
                let winner = candidates
                    .and_then(|candidates| candidates.first())
                    .copied();
                ExplorerRow {
                    label: command.clone(),
                    summary: if winner == selected_directory {
                        "winner".to_owned()
                    } else {
                        winner.map_or_else(
                            || "local".to_owned(),
                            |index| format!("shadowed by {:02}", index.saturating_add(1)),
                        )
                    },
                    warning: winner.is_some() && winner != selected_directory,
                }
            })
            .collect();
        ExplorerColumn {
            title: format!("Executables · {}", commands.len()),
            rows,
            selected: self.executable_selected,
            focus: ExplorerFocus::Executables,
        }
    }

    fn command_column(&self) -> ExplorerColumn {
        let rows = self
            .resolved_command_names()
            .into_iter()
            .map(|command| {
                let candidates = self.command_candidates(command);
                let winner = candidates.first().copied();
                ExplorerRow {
                    label: command.clone(),
                    summary: winner.map_or_else(
                        || "unresolved".to_owned(),
                        |index| {
                            let position = self
                                .path_entries
                                .get(index)
                                .map_or(index.saturating_add(1), |entry| entry.position);
                            if candidates.len() > 1 {
                                format!(
                                    "PATH[{position}] · +{}",
                                    candidates.len().saturating_sub(1)
                                )
                            } else {
                                format!("PATH[{position}]")
                            }
                        },
                    ),
                    warning: candidates.len() > 1,
                }
            })
            .collect::<Vec<_>>();
        ExplorerColumn {
            title: format!("Resolved commands · {}", rows.len()),
            rows,
            selected: self.command_selected,
            focus: ExplorerFocus::Commands,
        }
    }

    fn detail_lines(&self) -> Vec<Line<'static>> {
        let lines = match self.focus {
            ExplorerFocus::Categories => self.category_detail(),
            ExplorerFocus::Variables => self.variable_detail(),
            ExplorerFocus::PathDirectories => self.path_detail(),
            ExplorerFocus::Executables => self.executable_detail(),
            ExplorerFocus::Commands => self.command_detail(),
        };
        lines.into_iter().take(DETAIL_LINES_MAX).collect()
    }

    fn category_detail(&self) -> Vec<Line<'static>> {
        let category = self.selected_category();
        let count = self
            .categories
            .iter()
            .find(|group| group.category == category)
            .map_or(0, |group| group.variables.len());
        let mut lines = vec![
            Line::raw(category.label()),
            Line::raw(category_description(category)),
            Line::raw(""),
            Line::raw(if category == EnvironmentCategory::Health {
                if self.health_scan_pending() {
                    format!(
                        "Scanning {} PATH directories for health findings…",
                        self.path_entries.len()
                    )
                } else if self.health_scan_complete() && self.health.is_empty() {
                    "Health scan complete · no findings".to_owned()
                } else if self.health_scan_complete() {
                    format!("{} actionable environment findings", self.health.len())
                } else {
                    "PATH health has not been checked".to_owned()
                }
            } else {
                format!("{count} retained variables")
            }),
        ];
        if category == EnvironmentCategory::Health && self.health_scan_complete() {
            let missing = self
                .health
                .iter()
                .filter(|issue| {
                    matches!(
                        issue,
                        HealthIssue::PathDirectoryUnavailable {
                            status: DirectoryStatus::Missing,
                            ..
                        }
                    )
                })
                .count();
            let duplicates = self
                .health
                .iter()
                .filter(|issue| matches!(issue, HealthIssue::DuplicatePathEntry { .. }))
                .count();
            lines.push(Line::raw(format!(
                "{missing} missing · {duplicates} duplicate · {} other",
                self.health
                    .len()
                    .saturating_sub(missing.saturating_add(duplicates))
            )));
        }
        if category == EnvironmentCategory::CommandLookup {
            let (commands, shadowed) = self.resolution_counts();
            lines.push(Line::raw(format!(
                "{} PATH directories · {commands} commands · {shadowed} shadowed names",
                self.path_entries.len()
            )));
            lines.push(Line::raw(
                "w searches the effective command-resolution table.",
            ));
        }
        let examples = self
            .categories
            .iter()
            .find(|group| group.category == category)
            .into_iter()
            .flat_map(|group| group.variables.iter())
            .filter_map(|index| self.variables.get(*index))
            .take(8)
            .map(|variable| variable.name.as_str())
            .collect::<Vec<_>>();
        if !examples.is_empty() && category != EnvironmentCategory::Health {
            lines.push(Line::raw(""));
            lines.push(Line::raw(format!("Includes: {}", examples.join(", "))));
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw("Enter or → opens this category."));
        lines
    }

    fn variable_detail(&self) -> Vec<Line<'static>> {
        if self.selected_category() == EnvironmentCategory::Health {
            let Some(issue) = self.selected_health_issue() else {
                return vec![
                    Line::raw("Environment health"),
                    Line::raw(""),
                    Line::raw(if self.health_scan_pending() {
                        "Scanning PATH in the background… findings will appear when the complete bounded snapshot is ready."
                    } else if self.health_scan_complete() {
                        "Health scan complete · no findings."
                    } else {
                        "PATH health has not been checked. Press r to scan."
                    }),
                ];
            };
            let mut lines = vec![
                Line::raw(issue.label()),
                Line::raw(""),
                Line::raw(issue.detail()),
                Line::raw(""),
                Line::raw(format!("Suggested next step: {}", issue.recommendation())),
            ];
            if let Some(position) = issue.path_position()
                && let Some(entry) = position
                    .checked_sub(1)
                    .and_then(|index| self.path_entries.get(index))
            {
                lines.insert(1, Line::raw(entry.display.clone()));
                lines.push(Line::raw(""));
                lines.push(Line::raw(
                    "Enter/→ jump to PATH entry · y copy path · i insert quoted path",
                ));
            }
            return lines;
        }
        let Some(index) = self.selected_variable_index() else {
            return vec![Line::raw("No variables in this category.")];
        };
        let Some(variable) = self.variables.get(index) else {
            return Vec::new();
        };
        let sensitive = is_sensitive_name(&variable.name);
        let revealed = self.revealed_secrets.contains(&index);
        let value = if sensitive && !revealed {
            "••••••  (press v to reveal)".to_owned()
        } else if variable.value.is_empty() {
            "<set flag with empty value>".to_owned()
        } else {
            variable.value.clone()
        };
        let mut lines = vec![
            Line::raw(variable.name.clone()),
            Line::raw(format!(
                "category: {}",
                classify_variable(&variable.name).label()
            )),
            Line::raw("source: session environment · process snapshot"),
            Line::raw(""),
            Line::raw(value),
            Line::raw(""),
        ];
        if variable.name == "PATH" {
            let (commands, shadowed) = self.resolution_counts();
            lines.push(Line::raw(format!(
                "{} lookup directories · Enter or → to drill down",
                self.path_entries.len()
            )));
            lines.push(Line::raw(format!(
                "{commands} resolved command names · {shadowed} with shadowed candidates"
            )));
            lines.push(Line::raw(
                "w searches commands by effective PATH resolution",
            ));
        } else if variable.value.contains(',') && !sensitive {
            lines.push(Line::raw("comma-separated members:"));
            lines.extend(
                variable
                    .value
                    .split(',')
                    .take(32)
                    .map(|member| Line::raw(format!("  · {}", member.trim()))),
            );
        }
        lines.push(Line::raw("y copy value · i insert ${NAME}"));
        lines
    }

    fn path_detail(&self) -> Vec<Line<'static>> {
        let Some(index) = self.selected_directory_index() else {
            return vec![Line::raw("PATH has no retained directories.")];
        };
        let Some(entry) = self.path_entries.get(index) else {
            return Vec::new();
        };
        let scanned = self
            .path_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.directories.get(index));
        let mut lines = vec![
            Line::raw(format!("PATH[{}]", entry.position)),
            Line::raw(entry.display.clone()),
            Line::raw(""),
        ];
        if entry.empty {
            lines.push(Line::raw(
                "warning: empty entry searches the current directory",
            ));
        }
        if let Some(first) = entry.duplicate_of {
            lines.push(Line::raw(format!("warning: duplicate of PATH[{first}]")));
        }
        if let Some(scanned) = scanned {
            lines.push(Line::raw(format!("status: {}", scanned.status.label())));
            lines.push(Line::raw(format!(
                "executables: {}",
                scanned.commands.len()
            )));
            lines.push(Line::raw(""));
            lines.push(Line::raw("Enter or → lists commands from this directory."));
        } else {
            lines.push(Line::raw(if self.path_worker.is_some() {
                "bounded scan in progress…"
            } else {
                "not scanned · press r to scan"
            }));
        }
        lines.push(Line::raw("y copy path · i insert quoted path"));
        lines
    }

    fn executable_detail(&self) -> Vec<Line<'static>> {
        let Some(command) = self.selected_executable() else {
            return vec![Line::raw(if self.path_worker.is_some() {
                "Scanning executables…"
            } else {
                "No executable entries retained for this directory."
            })];
        };
        let Some(directory_index) = self.selected_directory_index() else {
            return Vec::new();
        };
        let Some(directory) = self.path_entries.get(directory_index) else {
            return Vec::new();
        };
        let resolved = directory.path.join(command);
        let candidates = self
            .path_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.candidates.get(command))
            .cloned()
            .unwrap_or_default();
        let mut lines = vec![
            Line::raw(command.to_owned()),
            Line::raw(resolved.display().to_string()),
            Line::raw(""),
        ];
        if candidates.first().copied() == Some(directory_index) {
            lines.push(Line::raw("resolution: effective PATH winner"));
        } else if let Some(winner) = candidates.first()
            && let Some(entry) = self.path_entries.get(*winner)
        {
            lines.push(Line::raw(format!(
                "resolution: shadowed by PATH[{}] {}",
                entry.position, entry.display
            )));
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw("all retained candidates:"));
        lines.extend(candidates.iter().filter_map(|index| {
            let entry = self.path_entries.get(*index)?;
            Some(Line::raw(format!(
                "  {:02} {}",
                entry.position,
                entry.path.join(command).display()
            )))
        }));
        lines.push(Line::raw(""));
        lines.push(Line::raw(
            "y copy candidate path · i insert exact candidate path",
        ));
        lines.push(Line::raw("Executable code is never run by inspection."));
        lines
    }

    fn command_detail(&self) -> Vec<Line<'static>> {
        let Some(command) = self.selected_command() else {
            return vec![
                Line::raw("Resolved commands"),
                Line::raw(""),
                Line::raw(if self.path_worker.is_some() {
                    "Scanning PATH… type a command name while the bounded scan completes."
                } else if self.path_snapshot.is_some() {
                    "No command matches the focused filter."
                } else {
                    "Press r to scan PATH, or w to start command resolution."
                }),
            ];
        };
        let candidates = self.command_candidates(command);
        let Some(winner) = candidates.first().copied() else {
            return vec![Line::raw(command.to_owned()), Line::raw("unresolved")];
        };
        let Some(winner_path) = self.command_path(command, winner) else {
            return Vec::new();
        };
        let mut lines = vec![
            Line::raw(command.to_owned()),
            Line::raw(winner_path.display().to_string()),
            Line::raw(""),
            Line::raw(format!(
                "effective winner: PATH[{}]",
                self.path_entries
                    .get(winner)
                    .map_or(winner.saturating_add(1), |entry| entry.position)
            )),
            Line::raw(format!("{} retained candidate(s)", candidates.len())),
            Line::raw(""),
            Line::raw("Resolution order:"),
        ];
        lines.extend(candidates.iter().enumerate().filter_map(|(rank, index)| {
            let entry = self.path_entries.get(*index)?;
            let role = if rank == 0 { "winner" } else { "shadowed" };
            Some(Line::raw(format!(
                "  {}  PATH[{}]  {}  ({role})",
                rank.saturating_add(1),
                entry.position,
                entry.path.join(command).display()
            )))
        }));
        lines.push(Line::raw(""));
        lines.push(Line::raw(
            "y copy winner path · i insert exact winner path · / filter commands",
        ));
        lines.push(Line::raw("Inspection never executes the selected command."));
        lines
    }
}

impl Default for EnvironmentExplorer {
    fn default() -> Self {
        Self::new()
    }
}

struct ExplorerColumn {
    title: String,
    rows: Vec<ExplorerRow>,
    selected: usize,
    focus: ExplorerFocus,
}

struct ExplorerRow {
    label: String,
    summary: String,
    warning: bool,
}

fn render_column(
    frame: &mut Frame<'_>,
    area: Rect,
    column: &ExplorerColumn,
    focused: bool,
    theme: Theme,
    mode: Mode,
    symbols: SurfaceSymbols,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let title = format!(" {} · {} ", column.title, column.rows.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if focused {
            theme.accent(mode)
        } else {
            theme.border()
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let visible = usize::from(inner.height);
    let start = column
        .selected
        .saturating_sub(visible.saturating_sub(1))
        .min(column.rows.len().saturating_sub(visible));
    let overflow = column.rows.len() > visible;
    let content = Rect::new(
        inner.x,
        inner.y,
        inner.width.saturating_sub(u16::from(overflow)),
        inner.height,
    );
    let lines = column
        .rows
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, row)| {
            let selected = index == column.selected;
            let base = if selected {
                theme.selected(mode)
            } else if row.warning {
                theme.diagnostic(super::highlight::DiagnosticSeverity::Warning)
            } else {
                Style::default()
            };
            let glyph = if row.warning {
                match symbols {
                    SurfaceSymbols::Plain => "! ",
                    _ => "⚠ ",
                }
            } else if selected {
                match symbols {
                    SurfaceSymbols::Plain => "> ",
                    _ => "› ",
                }
            } else {
                "  "
            };
            row_line(row, glyph, content.width, base)
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), content);
    if overflow {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some(if symbols.uses_unicode() { "│" } else { "|" }))
            .thumb_symbol(if symbols.uses_unicode() { "█" } else { "#" })
            .thumb_style(theme.accent(mode));
        let mut state = ScrollbarState::new(column.rows.len())
            .position(start)
            .viewport_content_length(visible);
        frame.render_stateful_widget(scrollbar, inner, &mut state);
    }
}

fn row_line(row: &ExplorerRow, glyph: &str, width: u16, style: Style) -> Line<'static> {
    let label = escape_terminal_line(&row.label);
    let summary = escape_terminal_line(&row.summary);
    let used = UnicodeWidthStr::width(glyph)
        .saturating_add(UnicodeWidthStr::width(label.as_str()))
        .saturating_add(UnicodeWidthStr::width(summary.as_str()));
    let gap = usize::from(width).saturating_sub(used).max(1);
    Line::from(vec![
        Span::styled(glyph.to_owned(), style),
        Span::styled(label, style),
        Span::styled(" ".repeat(gap), style),
        Span::styled(summary, style),
    ])
    .style(style)
}

fn render_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    lines: Vec<Line<'static>>,
    theme: Theme,
    mode: Mode,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::default()
        .title(" Details & actions ")
        .borders(Borders::ALL)
        .border_style(theme.border());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if let Some((first, rest)) = lines.split_first() {
        let mut styled = vec![first.clone().style(theme.accent(mode))];
        styled.extend(rest.iter().cloned());
        frame.render_widget(
            Paragraph::new(styled).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }
}

fn category_groups(variables: &[InteractiveEnvironmentSnapshot]) -> Vec<CategoryGroup> {
    let mut groups = BTreeMap::<EnvironmentCategory, Vec<usize>>::new();
    groups.insert(EnvironmentCategory::Health, Vec::new());
    groups.insert(EnvironmentCategory::All, (0..variables.len()).collect());
    for (index, variable) in variables.iter().enumerate() {
        groups
            .entry(classify_variable(&variable.name))
            .or_default()
            .push(index);
    }
    for indices in groups.values_mut() {
        indices.sort_by(
            |left, right| match (variables.get(*left), variables.get(*right)) {
                (Some(left), Some(right)) => {
                    (left.name != "PATH", &left.name).cmp(&(right.name != "PATH", &right.name))
                }
                _ => left.cmp(right),
            },
        );
    }
    EnvironmentCategory::ORDER
        .into_iter()
        .filter_map(|category| {
            let variables = groups.remove(&category)?;
            let keep = category == EnvironmentCategory::Health
                || category == EnvironmentCategory::All
                || !variables.is_empty();
            keep.then_some(CategoryGroup {
                category,
                variables,
            })
        })
        .collect()
}

const fn category_description(category: EnvironmentCategory) -> &'static str {
    match category {
        EnvironmentCategory::Health => "Stale, ambiguous, or incomplete command lookup state.",
        EnvironmentCategory::All => "The complete bounded process snapshot inherited by commands.",
        EnvironmentCategory::CommandLookup => {
            "Search paths that determine commands, libraries, manuals, and completions."
        }
        EnvironmentCategory::Project => "Working-directory and project activation context.",
        EnvironmentCategory::Toolchains => "Language runtimes, package managers, and build tools.",
        EnvironmentCategory::TerminalSessions => {
            "Terminal emulators, multiplexers, remote sessions, and shell integrations."
        }
        EnvironmentCategory::ShellEditor => {
            "Interactive shell, editor, pager, and history behavior."
        }
        EnvironmentCategory::UserDirectories => "Home, identity, temporary, and XDG directories.",
        EnvironmentCategory::Locale => "Language, region, timezone, and character handling.",
        EnvironmentCategory::Secrets => "Credential-like values masked until explicitly revealed.",
        EnvironmentCategory::Other => "Variables without a developer-specific classification.",
    }
}

fn classify_variable(name: &str) -> EnvironmentCategory {
    let upper = name.to_ascii_uppercase();
    if is_sensitive_name(&upper) {
        return EnvironmentCategory::Secrets;
    }
    if has_prefix(&upper, &["QUIRL", "DIRENV", "DEVBOX", "PWD", "OLDPWD"]) {
        return EnvironmentCategory::Project;
    }
    if has_prefix(
        &upper,
        &[
            "CARGO",
            "RUST",
            "RUSTUP",
            "BUN",
            "NPM",
            "NODE",
            "PNPM",
            "PYTHON",
            "PYENV",
            "VIRTUAL_ENV",
            "GO",
            "GOPATH",
            "JAVA",
            "JDK",
            "GEM",
            "RBENV",
            "DENO",
            "LLVM",
            "HOMEBREW",
        ],
    ) {
        return EnvironmentCategory::Toolchains;
    }
    if has_prefix(
        &upper,
        &[
            "TERM",
            "COLORTERM",
            "TMUX",
            "CMUX",
            "GHOSTTY",
            "KITTY",
            "WEZTERM",
            "ATUIN",
            "SSH",
            "STARSHIP",
            "ZELLIJ",
        ],
    ) {
        return EnvironmentCategory::TerminalSessions;
    }
    if has_prefix(
        &upper,
        &[
            "SHELL", "ZSH", "BASH", "FISH", "EDITOR", "VISUAL", "PAGER", "LESS", "HIST", "PS1",
        ],
    ) {
        return EnvironmentCategory::ShellEditor;
    }
    if has_prefix(
        &upper,
        &["XDG", "HOME", "USER", "LOGNAME", "TMPDIR", "TMP", "TEMP"],
    ) {
        return EnvironmentCategory::UserDirectories;
    }
    if upper == "LANG" || upper == "TZ" || upper.starts_with("LC_") {
        return EnvironmentCategory::Locale;
    }
    if matches!(
        upper.as_str(),
        "PATH"
            | "MANPATH"
            | "INFOPATH"
            | "FPATH"
            | "CPATH"
            | "LIBRARY_PATH"
            | "LD_LIBRARY_PATH"
            | "DYLD_LIBRARY_PATH"
            | "PKG_CONFIG_PATH"
    ) {
        return EnvironmentCategory::CommandLookup;
    }
    EnvironmentCategory::Other
}

fn has_prefix(value: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| {
        value == *prefix
            || value
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('_'))
    })
}

fn is_sensitive_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if upper.ends_with("_SOCK") || upper == "SSH_AUTH_SOCK" {
        return false;
    }
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "ACCESS_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "AUTHORIZATION",
        "COOKIE",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

fn variable_summary(variable: &InteractiveEnvironmentSnapshot, revealed: bool) -> String {
    if is_sensitive_name(&variable.name) && !revealed {
        return "masked".to_owned();
    }
    if variable.value.is_empty() {
        return "set flag".to_owned();
    }
    if variable.name == "PATH" {
        return format!(
            "{} directories",
            path_entries(std::slice::from_ref(variable)).len()
        );
    }
    truncate_chars(&variable.value, 48)
}

fn path_entries(variables: &[InteractiveEnvironmentSnapshot]) -> Vec<PathEntry> {
    let Some(path) = variables.iter().find(|variable| variable.name == "PATH") else {
        return Vec::new();
    };
    let mut seen = BTreeMap::<PathBuf, usize>::new();
    std::env::split_paths(&path.value)
        .take(PATH_DIRECTORIES_MAX)
        .enumerate()
        .map(|(index, raw)| {
            let position = index.saturating_add(1);
            let empty = raw.as_os_str().is_empty();
            let path = if empty { PathBuf::from(".") } else { raw };
            let display = if empty {
                "<current directory>".to_owned()
            } else {
                path.display().to_string()
            };
            let duplicate_of = seen.get(&path).copied();
            seen.entry(path.clone()).or_insert(position);
            PathEntry {
                position,
                display,
                path,
                empty,
                duplicate_of,
            }
        })
        .collect()
}

fn path_entry_limit_reached(variables: &[InteractiveEnvironmentSnapshot]) -> bool {
    variables
        .iter()
        .find(|variable| variable.name == "PATH")
        .is_some_and(|path| {
            std::env::split_paths(&path.value)
                .take(PATH_DIRECTORIES_MAX.saturating_add(1))
                .count()
                > PATH_DIRECTORIES_MAX
        })
}

fn health_issues(
    path_entries: &[PathEntry],
    scan: Option<&PathScanSnapshot>,
    path_entries_truncated: bool,
) -> Vec<HealthIssue> {
    let mut issues = Vec::new();
    for (index, entry) in path_entries.iter().enumerate() {
        if entry.empty {
            issues.push(HealthIssue::EmptyPathEntry {
                position: entry.position,
            });
        }
        if let Some(first) = entry.duplicate_of {
            issues.push(HealthIssue::DuplicatePathEntry {
                position: entry.position,
                first,
            });
        }
        if let Some(status) = scan
            .and_then(|snapshot| snapshot.directories.get(index))
            .map(|directory| directory.status.clone())
            && status != DirectoryStatus::Ready
        {
            issues.push(HealthIssue::PathDirectoryUnavailable {
                position: entry.position,
                status,
            });
        }
    }
    if path_entries_truncated {
        issues.push(HealthIssue::PathDirectoryListTruncated {
            observed_at_least: PATH_DIRECTORIES_MAX.saturating_add(1),
        });
    }
    if scan.is_some_and(|snapshot| snapshot.truncated) {
        issues.push(HealthIssue::PathScanTruncated);
    }
    issues
}

fn scan_path_entries(entries: &[PathEntry], cancel: &AtomicBool) -> PathScanSnapshot {
    let mut snapshot = PathScanSnapshot::default();
    let mut total_entries = 0_usize;
    let mut retained_name_bytes = 0_usize;
    for entry in entries.iter().take(PATH_DIRECTORIES_MAX) {
        if cancel.load(Ordering::Acquire) {
            snapshot.truncated = true;
            break;
        }
        let directory = scan_directory(
            &entry.path,
            cancel,
            &mut total_entries,
            &mut retained_name_bytes,
        );
        snapshot.truncated |= directory.status == DirectoryStatus::Truncated;
        snapshot.directories.push(directory);
        if total_entries == PATH_EXECUTABLES_MAX || retained_name_bytes == PATH_NAME_BYTES_MAX {
            snapshot.truncated = true;
            break;
        }
    }
    while snapshot.directories.len() < entries.len() {
        snapshot.directories.push(ScannedDirectory {
            status: DirectoryStatus::Truncated,
            commands: Vec::new(),
        });
    }
    for (directory_index, directory) in snapshot.directories.iter().enumerate() {
        for command in &directory.commands {
            snapshot
                .candidates
                .entry(command.clone())
                .or_default()
                .push(directory_index);
        }
    }
    snapshot
}

fn scan_directory(
    path: &Path,
    cancel: &AtomicBool,
    total_entries: &mut usize,
    retained_name_bytes: &mut usize,
) -> ScannedDirectory {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return scanned_error(DirectoryStatus::Missing);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
            return scanned_error(DirectoryStatus::NotDirectory);
        }
        Err(_) => return scanned_error(DirectoryStatus::Unreadable),
    };
    let mut commands = Vec::new();
    let mut status = DirectoryStatus::Ready;
    for (entry_index, entry) in entries.enumerate() {
        if cancel.load(Ordering::Acquire)
            || entry_index == PATH_ENTRIES_PER_DIRECTORY_MAX
            || *total_entries == PATH_EXECUTABLES_MAX
        {
            status = DirectoryStatus::Truncated;
            break;
        }
        let Ok(entry) = entry else {
            status = DirectoryStatus::Truncated;
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            status = DirectoryStatus::Truncated;
            continue;
        };
        if !is_executable(&entry.path(), &metadata) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if retained_name_bytes.saturating_add(name.len()) > PATH_NAME_BYTES_MAX {
            *retained_name_bytes = PATH_NAME_BYTES_MAX;
            status = DirectoryStatus::Truncated;
            break;
        }
        *retained_name_bytes = retained_name_bytes.saturating_add(name.len());
        *total_entries = total_entries.saturating_add(1);
        commands.push(name);
    }
    commands.sort();
    commands.dedup();
    ScannedDirectory { status, commands }
}

fn scanned_error(status: DirectoryStatus) -> ScannedDirectory {
    ScannedDirectory {
        status,
        commands: Vec::new(),
    }
}

#[cfg(unix)]
fn is_executable(_path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.is_file()
        && path.extension().is_some_and(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "exe" | "cmd" | "bat" | "com"
            )
        })
}

fn matches_filter(value: &str, filter: &str) -> bool {
    filter.is_empty()
        || value
            .to_ascii_lowercase()
            .contains(&filter.to_ascii_lowercase())
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/' | ':')
        })
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn truncate_chars(value: &str, maximum: usize) -> String {
    let mut truncated = value.chars().take(maximum).collect::<String>();
    if value.chars().count() > maximum {
        truncated.push('…');
    }
    truncated
}

fn utf8_boundary_at_or_before(value: &str, maximum: usize) -> usize {
    let mut end = maximum.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory(label: &str) -> PathBuf {
        let unique = NEXT_TEST_DIRECTORY.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "quirl-environment-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn variable(name: &str, value: &str) -> InteractiveEnvironmentSnapshot {
        InteractiveEnvironmentSnapshot {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn categories_prioritize_developer_context_and_mask_secrets() {
        let mut explorer = EnvironmentExplorer::new();
        explorer
            .open(&[
                variable("CMUX_SOCKET_PATH", "/tmp/cmux.sock"),
                variable("CARGO_HOME", "/cargo"),
                variable("API_TOKEN", "secret"),
                variable("PATH", "/bin"),
            ])
            .unwrap();

        assert_eq!(
            explorer.selected_category(),
            EnvironmentCategory::CommandLookup
        );
        explorer.select_category(EnvironmentCategory::Secrets);
        explorer.focus = ExplorerFocus::Variables;
        assert_eq!(explorer.variable_column().rows[0].summary, "masked");
        assert_eq!(explorer.copy_selected(), None);
        assert!(
            explorer
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("reveal"))
        );
        explorer.toggle_secret_reveal();
        assert_eq!(explorer.variable_column().rows[0].summary, "secret");
        assert_eq!(explorer.copy_selected(), Some("secret".to_owned()));
    }

    #[test]
    fn health_is_scanning_until_the_complete_initial_path_audit_arrives() {
        let directory = test_directory("initial-health-scan");
        let mut explorer = EnvironmentExplorer::new();
        explorer
            .open(&[variable("PATH", &directory.display().to_string())])
            .unwrap();
        explorer.select_category(EnvironmentCategory::Health);

        assert_eq!(explorer.category_column().rows[0].summary, "scanning…");
        assert!(
            explorer
                .category_detail()
                .into_iter()
                .map(|line| line.to_string())
                .collect::<String>()
                .contains("Scanning 1 PATH directories")
        );

        let mut completed = false;
        for _ in 0..10_000 {
            if explorer.poll() {
                completed = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(completed, "bounded local PATH scan must complete");
        assert_eq!(explorer.category_column().rows[0].summary, "clean");

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn path_drilldown_reports_winners_shadowing_duplicates_and_empty_entries() {
        let first = test_directory("first");
        let second = test_directory("second");
        let first_tool = first.join("tool");
        let second_tool = second.join("tool");
        fs::write(&first_tool, "#!/bin/sh\n").unwrap();
        fs::write(&second_tool, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&first_tool, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&second_tool, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([first.as_path(), second.as_path(), first.as_path()])
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let entries = path_entries(&[variable("PATH", &path)]);
        let snapshot = scan_path_entries(&entries, &AtomicBool::new(false));

        assert_eq!(entries[2].duplicate_of, Some(1));
        assert_eq!(snapshot.candidates["tool"], [0, 1, 2]);
        assert_eq!(snapshot.directories[0].status, DirectoryStatus::Ready);

        fs::remove_dir_all(first).unwrap();
        fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn health_findings_show_the_path_and_jump_to_the_exact_directory() {
        let missing_root = test_directory("missing-parent");
        let first = missing_root.join("first-missing");
        let second = missing_root.join("second-missing");
        let path = std::env::join_paths([first.as_path(), second.as_path()])
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut explorer = EnvironmentExplorer::new();
        explorer.open(&[variable("PATH", &path)]).unwrap();
        let snapshot = scan_path_entries(&explorer.path_entries, &AtomicBool::new(false));
        explorer.health = health_issues(&explorer.path_entries, Some(&snapshot), false);
        explorer.path_snapshot = Some(snapshot);
        explorer.select_category(EnvironmentCategory::Health);
        explorer.focus = ExplorerFocus::Variables;
        explorer.variable_selected = 1;

        let detail = explorer
            .variable_detail()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<String>();
        assert!(detail.contains("second-missing"));
        assert!(detail.contains("Suggested next step"));

        explorer.drill_down().unwrap();
        assert_eq!(explorer.focus, ExplorerFocus::PathDirectories);
        assert_eq!(explorer.selected_directory_index(), Some(1));

        fs::remove_dir_all(missing_root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn command_resolution_search_uses_the_path_winner_and_exposes_shadowing() {
        use std::os::unix::fs::PermissionsExt;

        let first = test_directory("resolve-first");
        let second = test_directory("resolve-second");
        for executable in [first.join("cargo"), second.join("cargo")] {
            fs::write(&executable, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([first.as_path(), second.as_path()])
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut explorer = EnvironmentExplorer::new();
        explorer.open(&[variable("PATH", &path)]).unwrap();
        explorer.path_snapshot = Some(scan_path_entries(
            &explorer.path_entries,
            &AtomicBool::new(false),
        ));

        explorer.open_command_resolution().unwrap();
        explorer.insert_filter_text("cargo");
        let _ = explorer.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(explorer.focus, ExplorerFocus::Commands);
        assert_eq!(explorer.selected_command(), Some("cargo"));
        assert_eq!(
            explorer.copy_selected(),
            Some(first.join("cargo").display().to_string())
        );
        assert!(explorer.command_column().rows[0].summary.contains("+1"));
        assert!(
            explorer
                .command_detail()
                .into_iter()
                .map(|line| line.to_string())
                .collect::<String>()
                .contains("shadowed")
        );

        fs::remove_dir_all(first).unwrap();
        fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn filter_and_navigation_never_change_the_preserved_environment_values() {
        let variables = [
            variable("PATH", "/bin:/usr/bin"),
            variable("CMUX_NO_PR_WATCH", ""),
        ];
        let mut explorer = EnvironmentExplorer::new();
        explorer.open(&variables).unwrap();
        explorer.drill_down().unwrap();
        explorer.insert_filter_text("path");
        let _ = explorer.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(explorer.selected_variable_name(), Some("PATH"));
        assert_eq!(explorer.selected_insertion(), Some("${PATH}".to_owned()));
        assert_eq!(explorer.variables, variables);
    }

    #[test]
    fn filtering_a_category_preserves_it_when_drilling_down() {
        let mut explorer = EnvironmentExplorer::new();
        explorer
            .open(&[variable("PATH", "/bin"), variable("CARGO_HOME", "/cargo")])
            .unwrap();
        explorer.focus = ExplorerFocus::Categories;
        explorer.insert_filter_text("toolchains");
        let _ = explorer.handle_search_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        explorer.drill_down().unwrap();

        assert_eq!(
            explorer.selected_category(),
            EnvironmentCategory::Toolchains
        );
        assert_eq!(explorer.selected_variable_name(), Some("CARGO_HOME"));
    }

    #[test]
    fn full_screen_render_is_responsive_and_never_paints_a_masked_secret() {
        let mut explorer = EnvironmentExplorer::new();
        explorer
            .open(&[
                variable("PATH", "/bin:/usr/bin"),
                variable("CMUX_SOCKET_PATH", "/tmp/cmux.sock"),
                variable("API_TOKEN", "never-render-this-secret"),
            ])
            .unwrap();
        for width in [50, 90, 130] {
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    explorer.render(
                        frame,
                        area,
                        Theme::new(false),
                        Mode::Command,
                        SurfaceSymbols::Unicode,
                    );
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rendered = (0..buffer.area.height)
                .flat_map(|y| {
                    (0..buffer.area.width)
                        .filter_map(move |x| buffer.cell((x, y)))
                        .map(|cell| cell.symbol())
                })
                .collect::<String>();
            assert!(rendered.contains("Environment Explorer"));
            assert!(!rendered.contains("never-render-this-secret"));
        }
    }
}
