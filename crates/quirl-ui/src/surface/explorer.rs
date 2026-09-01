//! Bounded Miller-column directory navigation for the rich surface.

use crate::{SurfaceSymbols, theme::Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use image::{DynamicImage, GenericImageView, ImageReader, Limits, RgbaImage};
use quirl_core::{
    DirectoryOptions, DirectorySort, Entry, EntryKind, ErrorCode, ShellError,
    directory_entries_with_options, escape_terminal_line,
};
use quirl_syntax::{HighlightKind, Mode};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    fs::{self, File},
    io::{Cursor, Read, Take},
    path::{Path, PathBuf},
    sync::OnceLock,
};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};
use unicode_width::UnicodeWidthStr;

const EXPLORER_ENTRIES_MAX: usize = 4_096;
const EXPLORER_RETAINED_NAME_BYTES_MAX: usize = 2 * 1024 * 1024;
const EXPLORER_QUERY_BYTES_MAX: usize = 1_024;
const PREVIEW_BYTES_MAX: usize = 128 * 1024;
const PREVIEW_LINES_MAX: usize = 4_096;
const PREVIEW_SYNTAX_SPANS_MAX: usize = 32_768;
const IMAGE_ENCODED_BYTES_MAX: usize = 16 * 1024 * 1024;
const IMAGE_DIMENSION_MAX: u32 = 8_192;
const IMAGE_DECODE_ALLOC_BYTES_MAX: u64 = 64 * 1024 * 1024;
const IMAGE_THUMBNAIL_WIDTH_MAX: u32 = 160;
const IMAGE_THUMBNAIL_HEIGHT_MAX: u32 = 100;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static SOURCE_SCOPE_CLASSIFIER: OnceLock<SourceScopeClassifier> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ExplorerAction {
    Pending,
    Dismiss,
    ChangeDirectory(PathBuf),
}

#[derive(Debug)]
enum Preview {
    Empty,
    Directory(Vec<Entry>),
    Text {
        lines: Vec<HighlightedLine>,
        syntax_name: Option<String>,
        truncated: bool,
        highlighting_limited: bool,
    },
    Image(ImagePreview),
    Binary {
        lines: Vec<String>,
        size_bytes: u64,
    },
    Metadata(Vec<String>),
    Error(String),
}

#[derive(Debug)]
struct HighlightedLine {
    spans: Vec<HighlightedSpan>,
}

#[derive(Debug)]
struct HighlightedSpan {
    text: String,
    token_kind: SourceTokenKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceTokenKind {
    Plain,
    Comment,
    String,
    Number,
    Keyword,
    Type,
    Function,
    Property,
    Operator,
    Constant,
    Tag,
    Error,
}

#[derive(Debug)]
struct SourceScopeClassifier {
    prefixes: Vec<(Scope, SourceTokenKind)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TomlStringDelimiter {
    Basic,
    Literal,
}

#[derive(Debug)]
struct ImagePreview {
    pixels: RgbaImage,
    source_width: u32,
    source_height: u32,
}

/// One modal, transaction-like browser session.
///
/// Browsing never mutates the process working directory. The caller receives a
/// path only after an explicit accept action, so dismissing always preserves the
/// original shell state.
#[derive(Debug)]
pub(super) struct DirectoryExplorer {
    current_dir: PathBuf,
    parent_entries: Vec<Entry>,
    entries: Vec<Entry>,
    visible_indices: Vec<usize>,
    selected: usize,
    preview: Preview,
    show_hidden: bool,
    sort: DirectorySort,
    query: String,
    query_active: bool,
    notice: Option<String>,
}

impl DirectoryExplorer {
    pub fn open(current_dir: &Path) -> Result<Self, ShellError> {
        let entries = list_directory(current_dir, false, DirectorySort::Name)?;
        let parent_entries = list_parent(current_dir, false, DirectorySort::Name);
        let visible_indices = (0..entries.len()).collect();
        let mut explorer = Self {
            current_dir: current_dir.to_path_buf(),
            parent_entries,
            entries,
            visible_indices,
            selected: 0,
            preview: Preview::Empty,
            show_hidden: false,
            sort: DirectorySort::Name,
            query: String::new(),
            query_active: false,
            notice: None,
        };
        explorer.refresh_preview();
        Ok(explorer)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ExplorerAction {
        if self.query_active {
            return self.handle_query_key(key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ExplorerAction::Dismiss,
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                ExplorerAction::Pending
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                ExplorerAction::Pending
            }
            KeyCode::PageUp => {
                self.move_selection(-10);
                ExplorerAction::Pending
            }
            KeyCode::PageDown => {
                self.move_selection(10);
                ExplorerAction::Pending
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.select(0);
                ExplorerAction::Pending
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.select(self.visible_indices.len().saturating_sub(1));
                ExplorerAction::Pending
            }
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                self.ascend();
                ExplorerAction::Pending
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.descend();
                ExplorerAction::Pending
            }
            KeyCode::Enter => ExplorerAction::ChangeDirectory(self.accepted_directory()),
            KeyCode::Char('/') => {
                self.query_active = true;
                self.notice = None;
                ExplorerAction::Pending
            }
            KeyCode::Char('.') => {
                self.show_hidden = !self.show_hidden;
                self.reload_current();
                ExplorerAction::Pending
            }
            KeyCode::Char('s') => {
                self.sort = next_sort(self.sort);
                self.reload_current();
                ExplorerAction::Pending
            }
            KeyCode::Char('r') => {
                self.reload_current();
                ExplorerAction::Pending
            }
            KeyCode::Char('~') => {
                self.navigate_home();
                ExplorerAction::Pending
            }
            _ => ExplorerAction::Pending,
        }
    }

    pub fn insert_query(&mut self, text: &str) {
        if !self.query_active {
            return;
        }
        let sanitized = text.replace(['\r', '\n'], " ");
        if self.query.len().saturating_add(sanitized.len()) > EXPLORER_QUERY_BYTES_MAX {
            self.notice = Some(format!(
                "filter is limited to {EXPLORER_QUERY_BYTES_MAX} UTF-8 bytes"
            ));
            return;
        }
        self.query.push_str(&sanitized);
        self.rebuild_filter();
    }

    pub fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        frame.render_widget(Clear, area);
        let vertical = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
        let mut regions = vertical.iter().copied();
        let (Some(header), Some(columns), Some(footer)) =
            (regions.next(), regions.next(), regions.next())
        else {
            debug_assert!(false, "three explorer rows must produce three regions");
            return;
        };
        self.render_header(frame, header, theme, mode, symbols);
        self.render_columns(frame, columns, theme, mode, symbols);
        self.render_footer(frame, footer, theme, mode);
    }

    fn handle_query_key(&mut self, key: KeyEvent) -> ExplorerAction {
        match key.code {
            KeyCode::Esc => {
                self.query_active = false;
                self.query.clear();
                self.rebuild_filter();
            }
            KeyCode::Enter => self.query_active = false,
            KeyCode::Backspace => {
                self.query.pop();
                self.rebuild_filter();
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert_query(&character.to_string());
            }
            _ => {}
        }
        ExplorerAction::Pending
    }

    fn move_selection(&mut self, delta: isize) {
        let selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected
                .saturating_add(delta.unsigned_abs())
                .min(self.visible_indices.len().saturating_sub(1))
        };
        self.select(selected);
    }

    fn select(&mut self, selected: usize) {
        self.selected = selected.min(self.visible_indices.len().saturating_sub(1));
        self.notice = None;
        self.refresh_preview();
    }

    fn selected_entry(&self) -> Option<&Entry> {
        let index = *self.visible_indices.get(self.selected)?;
        self.entries.get(index)
    }

    fn accepted_directory(&self) -> PathBuf {
        self.selected_entry()
            .filter(|entry| entry_is_navigable(entry))
            .map_or_else(|| self.current_dir.clone(), |entry| entry.path.clone())
    }

    fn descend(&mut self) {
        let Some(path) = self
            .selected_entry()
            .filter(|entry| entry_is_navigable(entry))
            .map(|entry| entry.path.clone())
        else {
            self.notice = Some("select a directory to descend".to_owned());
            return;
        };
        self.navigate_to(path, None);
    }

    fn ascend(&mut self) {
        let Some(parent) = self.current_dir.parent().map(Path::to_path_buf) else {
            self.notice = Some("already at the filesystem root".to_owned());
            return;
        };
        let previous = self.current_dir.clone();
        self.navigate_to(parent, Some(&previous));
    }

    fn navigate_home(&mut self) {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            self.notice = Some("HOME is not configured".to_owned());
            return;
        };
        self.navigate_to(home, None);
    }

    fn navigate_to(&mut self, path: PathBuf, select_path: Option<&Path>) {
        let entries = match list_directory(&path, self.show_hidden, self.sort) {
            Ok(entries) => entries,
            Err(error) => {
                self.notice = Some(error.message);
                return;
            }
        };
        let parent_entries = list_parent(&path, self.show_hidden, self.sort);
        self.current_dir = path;
        self.entries = entries;
        self.parent_entries = parent_entries;
        self.query.clear();
        self.query_active = false;
        self.rebuild_filter();
        if let Some(select_path) = select_path
            && let Some(position) = self
                .visible_indices
                .iter()
                .filter_map(|index| self.entries.get(*index))
                .position(|entry| entry.path == select_path)
        {
            self.selected = position;
        }
        self.notice = None;
        self.refresh_preview();
    }

    fn reload_current(&mut self) {
        let selected_path = self.selected_entry().map(|entry| entry.path.clone());
        let entries = match list_directory(&self.current_dir, self.show_hidden, self.sort) {
            Ok(entries) => entries,
            Err(error) => {
                self.notice = Some(error.message);
                return;
            }
        };
        self.entries = entries;
        self.parent_entries = list_parent(&self.current_dir, self.show_hidden, self.sort);
        self.rebuild_filter();
        if let Some(selected_path) = selected_path
            && let Some(position) = self
                .visible_indices
                .iter()
                .filter_map(|index| self.entries.get(*index))
                .position(|entry| entry.path == selected_path)
        {
            self.selected = position;
        }
        self.notice = None;
        self.refresh_preview();
    }

    fn rebuild_filter(&mut self) {
        let query = self.query.to_lowercase();
        self.visible_indices = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                (query.is_empty() || entry.name.to_lowercase().contains(&query)).then_some(index)
            })
            .collect();
        self.selected = self
            .selected
            .min(self.visible_indices.len().saturating_sub(1));
        self.refresh_preview();
    }

    fn refresh_preview(&mut self) {
        self.preview = self.selected_entry().map_or(Preview::Empty, load_preview);
    }

    fn render_header(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        let title = match symbols {
            SurfaceSymbols::NerdFont => " 󰉋  Quirl Explorer",
            SurfaceSymbols::Unicode => " ◫  Quirl Explorer",
            SurfaceSymbols::Plain => "[] Quirl Explorer",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(title, theme.accent(mode)),
                Line::styled(
                    truncate_left(&self.current_dir.display().to_string(), area.width),
                    theme.context(),
                ),
            ]),
            area,
        );
    }

    fn render_columns(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        if area.width >= 96 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(24),
                    Constraint::Percentage(38),
                    Constraint::Percentage(38),
                ])
                .split(area);
            let mut columns = columns.iter().copied();
            let (Some(parent), Some(current), Some(preview)) =
                (columns.next(), columns.next(), columns.next())
            else {
                debug_assert!(false, "three explorer columns must produce three regions");
                return;
            };
            self.render_parent(frame, parent, theme, mode, symbols);
            self.render_current(frame, current, theme, mode, symbols);
            self.render_preview(frame, preview, theme, mode, symbols);
        } else if area.width >= 56 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
                .split(area);
            let mut columns = columns.iter().copied();
            let (Some(current), Some(preview)) = (columns.next(), columns.next()) else {
                debug_assert!(false, "two explorer columns must produce two regions");
                return;
            };
            self.render_current(frame, current, theme, mode, symbols);
            self.render_preview(frame, preview, theme, mode, symbols);
        } else {
            self.render_current(frame, area, theme, mode, symbols);
        }
    }

    fn render_parent(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        let selected = self
            .parent_entries
            .iter()
            .position(|entry| entry.path == self.current_dir);
        render_entry_list(
            frame,
            area,
            self.parent_entries.iter(),
            EntryListOptions {
                title: " parent ",
                selected,
                theme,
                mode,
                symbols,
                directories_only: true,
            },
        );
    }

    fn render_current(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        let title = self.current_dir.file_name().map_or_else(
            || " / ".to_owned(),
            |name| format!(" {} ", name.to_string_lossy()),
        );
        render_entry_list(
            frame,
            area,
            self.visible_indices
                .iter()
                .filter_map(|index| self.entries.get(*index)),
            EntryListOptions {
                title: &title,
                selected: (!self.visible_indices.is_empty()).then_some(self.selected),
                theme,
                mode,
                symbols,
                directories_only: false,
            },
        );
    }

    fn render_preview(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: Theme,
        mode: Mode,
        symbols: SurfaceSymbols,
    ) {
        let title = self.selected_entry().map_or_else(
            || " preview ".to_owned(),
            |entry| format!(" {} ", escape_terminal_line(&entry.name)),
        );
        match &self.preview {
            Preview::Directory(entries) => render_entry_list(
                frame,
                area,
                entries.iter(),
                EntryListOptions {
                    title: &title,
                    selected: None,
                    theme,
                    mode,
                    symbols,
                    directories_only: false,
                },
            ),
            Preview::Image(image) => {
                let block = Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(theme.border());
                let inner = block.inner(area);
                frame.render_widget(block, area);
                render_image_preview(frame, inner, image, theme, mode, symbols);
            }
            preview => {
                let block = Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(theme.border());
                let inner = block.inner(area);
                frame.render_widget(block, area);
                let lines = preview_lines(preview, theme, mode);
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
        }
    }

    fn render_footer(&self, frame: &mut Frame<'_>, area: Rect, theme: Theme, mode: Mode) {
        let line = if self.query_active {
            Line::from(vec![
                Span::styled(" filter ", theme.accent(mode)),
                Span::raw(escape_terminal_line(&self.query)),
                Span::styled("█", theme.accent(mode)),
                Span::styled("  Enter finish · Esc clear", theme.dim()),
            ])
        } else if let Some(notice) = &self.notice {
            Line::from(vec![
                Span::styled(" ! ", theme.accent(mode)),
                Span::raw(escape_terminal_line(notice)),
            ])
        } else {
            Line::styled(
                format!(
                    "↑↓/jk select · ←→/hl navigate · Enter jump · / filter · . hidden · s sort:{} · r refresh · Esc cancel",
                    sort_label(self.sort)
                ),
                theme.dim(),
            )
        };
        frame.render_widget(Paragraph::new(line).style(theme.status()), area);
    }
}

fn list_directory(
    path: &Path,
    show_hidden: bool,
    sort: DirectorySort,
) -> Result<Vec<Entry>, ShellError> {
    list_directory_with_limits(
        path,
        show_hidden,
        sort,
        EXPLORER_ENTRIES_MAX,
        EXPLORER_RETAINED_NAME_BYTES_MAX,
    )
}

fn list_directory_with_limits(
    path: &Path,
    show_hidden: bool,
    sort: DirectorySort,
    entries_max: usize,
    retained_name_bytes_max: usize,
) -> Result<Vec<Entry>, ShellError> {
    let entries = directory_entries_with_options(
        path,
        &DirectoryOptions {
            show_all: show_hidden,
            sort,
            directories_first: true,
            max_entries: entries_max,
            ..DirectoryOptions::default()
        },
    )?;
    let retained_name_bytes = entries.iter().fold(0_usize, |bytes, entry| {
        bytes.saturating_add(entry.name.len())
    });
    if retained_name_bytes > retained_name_bytes_max {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!(
                "directory listing retained {retained_name_bytes} filename bytes, limit is {retained_name_bytes_max}"
            ),
        )
        .with_help("Choose a narrower directory or hide generated entries"));
    }
    Ok(entries)
}

fn list_parent(path: &Path, show_hidden: bool, sort: DirectorySort) -> Vec<Entry> {
    path.parent()
        .and_then(|parent| list_directory(parent, show_hidden, sort).ok())
        .map(|entries| entries.into_iter().filter(entry_is_navigable).collect())
        .unwrap_or_default()
}

fn entry_is_navigable(entry: &Entry) -> bool {
    entry.kind == EntryKind::Directory
        || (entry.kind == EntryKind::Symlink && fs::metadata(&entry.path).is_ok_and(|m| m.is_dir()))
}

fn load_preview(entry: &Entry) -> Preview {
    match entry.kind {
        EntryKind::Directory => list_directory(&entry.path, false, DirectorySort::Name)
            .map(Preview::Directory)
            .unwrap_or_else(|error| Preview::Error(error.message)),
        EntryKind::File => load_file_preview(entry),
        EntryKind::Symlink => Preview::Metadata(vec![
            "symbolic link".to_owned(),
            fs::read_link(&entry.path).map_or_else(
                |error| format!("target unavailable: {error}"),
                |target| format!("target: {}", target.display()),
            ),
            format!("size: {}", format_bytes(entry.size)),
        ]),
        EntryKind::Other => Preview::Metadata(vec![
            "special filesystem entry".to_owned(),
            format!("size: {}", format_bytes(entry.size)),
        ]),
    }
}

fn load_file_preview(entry: &Entry) -> Preview {
    if is_supported_image_path(&entry.path) {
        return load_image_preview(entry);
    }
    let file = match File::open(&entry.path) {
        Ok(file) => file,
        Err(error) => return Preview::Error(format!("could not open preview: {error}")),
    };
    let limit = u64::try_from(PREVIEW_BYTES_MAX.saturating_add(1)).unwrap_or(u64::MAX);
    let mut reader: Take<File> = file.take(limit);
    let mut bytes = Vec::with_capacity(PREVIEW_BYTES_MAX.saturating_add(1));
    if let Err(error) = reader.read_to_end(&mut bytes) {
        return Preview::Error(format!("could not read preview: {error}"));
    }
    let truncated = bytes.len() > PREVIEW_BYTES_MAX;
    bytes.truncate(PREVIEW_BYTES_MAX);
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Preview::Binary {
            lines: hex_preview(&bytes),
            size_bytes: entry.size,
        };
    }
    let text = String::from_utf8(bytes).unwrap_or_default();
    let (mut lines, syntax_name, highlighting_limited) = highlight_source(&entry.path, &text);
    let line_truncated = text.lines().nth(PREVIEW_LINES_MAX).is_some();
    if lines.is_empty() {
        lines.push(HighlightedLine::plain("empty file"));
    }
    Preview::Text {
        lines,
        syntax_name,
        truncated: truncated || line_truncated,
        highlighting_limited,
    }
}

fn highlight_source(path: &Path, text: &str) -> (Vec<HighlightedLine>, Option<String>, bool) {
    highlight_source_with_limit(path, text, PREVIEW_SYNTAX_SPANS_MAX)
}

fn highlight_source_with_limit(
    path: &Path,
    text: &str,
    syntax_spans_max: usize,
) -> (Vec<HighlightedLine>, Option<String>, bool) {
    if is_toml_path(path) {
        let (lines, highlighting_limited) = highlight_toml(text, syntax_spans_max);
        return (lines, Some("TOML".to_owned()), highlighting_limited);
    }
    let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_nonewlines);
    let syntax = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| syntax_set.find_syntax_by_extension(name))
        .or_else(|| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        });
    let Some(syntax) = syntax else {
        return (
            text.lines()
                .take(PREVIEW_LINES_MAX)
                .map(HighlightedLine::plain)
                .collect(),
            None,
            false,
        );
    };
    let mut parse_state = ParseState::new(syntax);
    let mut scope_stack = ScopeStack::new();
    let mut span_count = 0_usize;
    let mut highlighting_limited = false;
    let mut parser_failed = false;
    let lines = text
        .lines()
        .take(PREVIEW_LINES_MAX)
        .map(|line| {
            if parser_failed {
                return HighlightedLine::plain(line);
            }
            let Ok(scope_operations) = parse_state.parse_line(line, syntax_set) else {
                parser_failed = true;
                return HighlightedLine::plain(line);
            };
            let mut spans = Vec::new();
            let mut region_start = 0_usize;
            for (region_end, operation) in scope_operations {
                if region_end > line.len() || !line.is_char_boundary(region_end) {
                    parser_failed = true;
                    return HighlightedLine::plain(line);
                }
                if region_end > region_start {
                    let Some(region) = line.get(region_start..region_end) else {
                        parser_failed = true;
                        return HighlightedLine::plain(line);
                    };
                    push_source_span(
                        &mut spans,
                        region,
                        source_scope_classifier().classify(&scope_stack),
                        &mut span_count,
                        syntax_spans_max,
                        &mut highlighting_limited,
                    );
                    region_start = region_end;
                }
                if scope_stack.apply(&operation).is_err() {
                    parser_failed = true;
                    return HighlightedLine::plain(line);
                }
            }
            let Some(region) = line.get(region_start..) else {
                parser_failed = true;
                return HighlightedLine::plain(line);
            };
            push_source_span(
                &mut spans,
                region,
                source_scope_classifier().classify(&scope_stack),
                &mut span_count,
                syntax_spans_max,
                &mut highlighting_limited,
            );
            HighlightedLine { spans }
        })
        .collect();
    (lines, Some(syntax.name.clone()), highlighting_limited)
}

impl SourceScopeClassifier {
    fn new() -> Self {
        let definitions = [
            ("invalid", SourceTokenKind::Error),
            ("comment", SourceTokenKind::Comment),
            ("string", SourceTokenKind::String),
            ("constant.numeric", SourceTokenKind::Number),
            ("keyword", SourceTokenKind::Keyword),
            ("storage", SourceTokenKind::Keyword),
            ("entity.name.type", SourceTokenKind::Type),
            ("entity.name.class", SourceTokenKind::Type),
            ("support.type", SourceTokenKind::Type),
            ("entity.name.function", SourceTokenKind::Function),
            ("support.function", SourceTokenKind::Function),
            ("entity.name.tag", SourceTokenKind::Tag),
            ("variable.other.member", SourceTokenKind::Property),
            ("variable.other.property", SourceTokenKind::Property),
            ("constant", SourceTokenKind::Constant),
            ("punctuation", SourceTokenKind::Operator),
        ];
        let prefixes = definitions
            .into_iter()
            .filter_map(|(scope, kind)| Scope::new(scope).ok().map(|scope| (scope, kind)))
            .collect();
        Self { prefixes }
    }

    fn classify(&self, stack: &ScopeStack) -> SourceTokenKind {
        self.prefixes
            .iter()
            .find_map(|(prefix, kind)| {
                stack
                    .scopes
                    .iter()
                    .any(|scope| prefix.is_prefix_of(*scope))
                    .then_some(*kind)
            })
            .unwrap_or(SourceTokenKind::Plain)
    }
}

fn source_scope_classifier() -> &'static SourceScopeClassifier {
    SOURCE_SCOPE_CLASSIFIER.get_or_init(SourceScopeClassifier::new)
}

fn push_source_span(
    spans: &mut Vec<HighlightedSpan>,
    text: &str,
    token_kind: SourceTokenKind,
    span_count: &mut usize,
    syntax_spans_max: usize,
    highlighting_limited: &mut bool,
) {
    if text.is_empty() {
        return;
    }
    let retained_kind = if *span_count < syntax_spans_max {
        *span_count = span_count.saturating_add(1);
        token_kind
    } else {
        *highlighting_limited = true;
        SourceTokenKind::Plain
    };
    if let Some(previous) = spans.last_mut()
        && previous.token_kind == retained_kind
    {
        previous.text.push_str(text);
        return;
    }
    spans.push(HighlightedSpan {
        text: text.to_owned(),
        token_kind: retained_kind,
    });
}

fn is_toml_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("toml"))
}

fn highlight_toml(text: &str, syntax_spans_max: usize) -> (Vec<HighlightedLine>, bool) {
    let mut multiline_string = None;
    let mut span_count = 0_usize;
    let mut highlighting_limited = false;
    let lines = text
        .lines()
        .take(PREVIEW_LINES_MAX)
        .map(|line| {
            highlight_toml_line(
                line,
                &mut multiline_string,
                &mut span_count,
                syntax_spans_max,
                &mut highlighting_limited,
            )
        })
        .collect();
    (lines, highlighting_limited)
}

fn highlight_toml_line(
    line: &str,
    multiline_string: &mut Option<TomlStringDelimiter>,
    span_count: &mut usize,
    syntax_spans_max: usize,
    highlighting_limited: &mut bool,
) -> HighlightedLine {
    let mut spans = Vec::new();
    let mut cursor = highlight_toml_multiline_prefix(
        line,
        multiline_string,
        &mut spans,
        span_count,
        syntax_spans_max,
        highlighting_limited,
    );
    if multiline_string.is_some() {
        return HighlightedLine { spans };
    }
    let mut expects_key = line.get(cursor..).is_some_and(|rest| rest.contains('='));
    let mut inline_table_depth = 0_usize;
    while cursor < line.len() {
        let token_end = toml_token_end(
            line,
            cursor,
            &mut expects_key,
            &mut inline_table_depth,
            multiline_string,
        );
        let (end, kind) = token_end;
        let Some(token) = line.get(cursor..end) else {
            return HighlightedLine::plain(line);
        };
        push_source_span(
            &mut spans,
            token,
            kind,
            span_count,
            syntax_spans_max,
            highlighting_limited,
        );
        cursor = end;
        if multiline_string.is_some() || kind == SourceTokenKind::Comment {
            break;
        }
    }
    HighlightedLine { spans }
}

fn highlight_toml_multiline_prefix(
    line: &str,
    multiline_string: &mut Option<TomlStringDelimiter>,
    spans: &mut Vec<HighlightedSpan>,
    span_count: &mut usize,
    syntax_spans_max: usize,
    highlighting_limited: &mut bool,
) -> usize {
    let Some(delimiter) = *multiline_string else {
        return 0;
    };
    let marker = match delimiter {
        TomlStringDelimiter::Basic => "\"\"\"",
        TomlStringDelimiter::Literal => "'''",
    };
    let closing_index = line.find(marker);
    let end = closing_index.map_or(line.len(), |index| index.saturating_add(marker.len()));
    let Some(prefix) = line.get(..end) else {
        return 0;
    };
    push_source_span(
        spans,
        prefix,
        SourceTokenKind::String,
        span_count,
        syntax_spans_max,
        highlighting_limited,
    );
    if closing_index.is_some() {
        *multiline_string = None;
    }
    end
}

fn toml_token_end(
    line: &str,
    start: usize,
    expects_key: &mut bool,
    inline_table_depth: &mut usize,
    multiline_string: &mut Option<TomlStringDelimiter>,
) -> (usize, SourceTokenKind) {
    let bytes = line.as_bytes();
    let Some(&byte) = bytes.get(start) else {
        return (line.len(), SourceTokenKind::Plain);
    };
    if byte.is_ascii_whitespace() {
        return (
            scan_ascii_while(bytes, start, u8::is_ascii_whitespace),
            SourceTokenKind::Plain,
        );
    }
    if byte == b'#' {
        return (line.len(), SourceTokenKind::Comment);
    }
    if matches!(byte, b'\'' | b'"') {
        let (end, is_multiline, closed) = scan_toml_string(line, start, byte);
        if is_multiline && !closed {
            *multiline_string = Some(if byte == b'"' {
                TomlStringDelimiter::Basic
            } else {
                TomlStringDelimiter::Literal
            });
        }
        let kind = if *expects_key {
            SourceTokenKind::Property
        } else {
            SourceTokenKind::String
        };
        return (end, kind);
    }
    if byte == b'=' {
        *expects_key = false;
        return (start.saturating_add(1), SourceTokenKind::Operator);
    }
    if matches!(byte, b'[' | b']' | b'{' | b'}' | b',' | b'.') {
        update_toml_container_state(byte, expects_key, inline_table_depth);
        return (start.saturating_add(1), SourceTokenKind::Operator);
    }
    let end = scan_ascii_while(bytes, start, |candidate| {
        !candidate.is_ascii_whitespace()
            && !matches!(
                candidate,
                b'#' | b'=' | b'[' | b']' | b'{' | b'}' | b',' | b'.' | b'\'' | b'"'
            )
    });
    let Some(token) = line.get(start..end) else {
        return (line.len(), SourceTokenKind::Plain);
    };
    let kind = if *expects_key {
        SourceTokenKind::Property
    } else {
        classify_toml_value(token)
    };
    (end, kind)
}

fn scan_ascii_while(bytes: &[u8], start: usize, predicate: impl Fn(&u8) -> bool) -> usize {
    let mut cursor = start;
    while let Some(byte) = bytes.get(cursor) {
        if !predicate(byte) {
            break;
        }
        cursor = cursor.saturating_add(1);
    }
    if cursor == start {
        line_char_end(bytes, start)
    } else {
        cursor
    }
}

fn line_char_end(bytes: &[u8], start: usize) -> usize {
    let Some(&byte) = bytes.get(start) else {
        return bytes.len();
    };
    let width = if byte < 0x80 {
        1
    } else if byte < 0xe0 {
        2
    } else if byte < 0xf0 {
        3
    } else {
        4
    };
    start.saturating_add(width).min(bytes.len())
}

fn scan_toml_string(line: &str, start: usize, quote: u8) -> (usize, bool, bool) {
    let bytes = line.as_bytes();
    let triple = bytes.get(start..start.saturating_add(3)) == Some(&[quote, quote, quote]);
    let marker_width = if triple { 3 } else { 1 };
    let mut cursor = start.saturating_add(marker_width);
    while cursor < bytes.len() {
        if triple && bytes.get(cursor..cursor.saturating_add(3)) == Some(&[quote, quote, quote]) {
            return (cursor.saturating_add(3), true, true);
        }
        if !triple && bytes.get(cursor) == Some(&quote) {
            return (cursor.saturating_add(1), false, true);
        }
        if quote == b'"' && bytes.get(cursor) == Some(&b'\\') {
            cursor = cursor.saturating_add(1);
        }
        cursor = cursor.saturating_add(1);
    }
    (line.len(), triple, false)
}

fn update_toml_container_state(byte: u8, expects_key: &mut bool, inline_table_depth: &mut usize) {
    match byte {
        b'{' => {
            *inline_table_depth = inline_table_depth.saturating_add(1);
            *expects_key = true;
        }
        b'}' => *inline_table_depth = inline_table_depth.saturating_sub(1),
        b',' if *inline_table_depth > 0 => *expects_key = true,
        _ => {}
    }
}

fn classify_toml_value(token: &str) -> SourceTokenKind {
    match token {
        "true" | "false" | "inf" | "+inf" | "-inf" | "nan" | "+nan" | "-nan" => {
            SourceTokenKind::Constant
        }
        _ if token
            .as_bytes()
            .first()
            .is_some_and(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-')) =>
        {
            SourceTokenKind::Number
        }
        _ => SourceTokenKind::Plain,
    }
}

impl HighlightedLine {
    fn plain(line: &str) -> Self {
        Self {
            spans: vec![HighlightedSpan {
                text: line.to_owned(),
                token_kind: SourceTokenKind::Plain,
            }],
        }
    }
}

fn is_supported_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "gif" | "jpeg" | "jpg" | "png" | "webp"
            )
        })
}

fn load_image_preview(entry: &Entry) -> Preview {
    if entry.size > u64::try_from(IMAGE_ENCODED_BYTES_MAX).unwrap_or(u64::MAX) {
        return Preview::Error(format!(
            "image is {}, encoded preview limit is {}",
            format_bytes(entry.size),
            format_bytes(u64::try_from(IMAGE_ENCODED_BYTES_MAX).unwrap_or(u64::MAX))
        ));
    }
    let file = match File::open(&entry.path) {
        Ok(file) => file,
        Err(error) => return Preview::Error(format!("could not open image preview: {error}")),
    };
    let read_limit = u64::try_from(IMAGE_ENCODED_BYTES_MAX.saturating_add(1)).unwrap_or(u64::MAX);
    let mut bytes = Vec::with_capacity(
        usize::try_from(entry.size)
            .unwrap_or(IMAGE_ENCODED_BYTES_MAX)
            .min(IMAGE_ENCODED_BYTES_MAX),
    );
    if let Err(error) = file.take(read_limit).read_to_end(&mut bytes) {
        return Preview::Error(format!("could not read image preview: {error}"));
    }
    if bytes.len() > IMAGE_ENCODED_BYTES_MAX {
        return Preview::Error(format!(
            "image grew beyond the {} encoded preview limit",
            format_bytes(u64::try_from(IMAGE_ENCODED_BYTES_MAX).unwrap_or(u64::MAX))
        ));
    }
    let mut reader = match ImageReader::new(Cursor::new(bytes)).with_guessed_format() {
        Ok(reader) => reader,
        Err(error) => return Preview::Error(format!("could not identify image preview: {error}")),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(IMAGE_DIMENSION_MAX);
    limits.max_image_height = Some(IMAGE_DIMENSION_MAX);
    limits.max_alloc = Some(IMAGE_DECODE_ALLOC_BYTES_MAX);
    reader.limits(limits);
    let image = match reader.decode() {
        Ok(image) => image,
        Err(error) => return Preview::Error(format!("could not decode bounded image: {error}")),
    };
    image_preview_from_decoded(image)
}

fn image_preview_from_decoded(image: DynamicImage) -> Preview {
    let (source_width, source_height) = image.dimensions();
    let thumbnail = image
        .thumbnail(IMAGE_THUMBNAIL_WIDTH_MAX, IMAGE_THUMBNAIL_HEIGHT_MAX)
        .into_rgba8();
    Preview::Image(ImagePreview {
        pixels: thumbnail,
        source_width,
        source_height,
    })
}

fn hex_preview(bytes: &[u8]) -> Vec<String> {
    bytes
        .iter()
        .take(4 * 1024)
        .copied()
        .collect::<Vec<_>>()
        .chunks(16)
        .enumerate()
        .map(|(row, chunk)| {
            let hex = chunk
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            let ascii = chunk
                .iter()
                .map(|byte| {
                    if byte.is_ascii_graphic() {
                        char::from(*byte)
                    } else {
                        '.'
                    }
                })
                .collect::<String>();
            format!("{:08x}  {hex:<47}  {ascii}", row.saturating_mul(16))
        })
        .collect()
}

struct EntryListOptions<'a> {
    title: &'a str,
    selected: Option<usize>,
    theme: Theme,
    mode: Mode,
    symbols: SurfaceSymbols,
    directories_only: bool,
}

fn render_entry_list<'a>(
    frame: &mut Frame<'_>,
    area: Rect,
    entries: impl IntoIterator<Item = &'a Entry>,
    options: EntryListOptions<'_>,
) {
    let block = Block::default()
        .title(options.title.to_owned())
        .borders(Borders::ALL)
        .border_style(options.theme.border());
    let items = entries
        .into_iter()
        .filter(|entry| !options.directories_only || entry_is_navigable(entry))
        .map(|entry| {
            let glyph = entry_glyph(entry, options.symbols);
            let name = escape_terminal_line(&entry.name);
            let style = if entry_is_navigable(entry) {
                options.theme.context()
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{glyph} "), style),
                Span::styled(name, style),
            ]))
        });
    let list = List::new(items)
        .block(block)
        .highlight_style(options.theme.selected(options.mode))
        .highlight_symbol(if options.symbols.uses_unicode() {
            "▸ "
        } else {
            "> "
        });
    let mut state = ListState::default().with_selected(options.selected);
    frame.render_stateful_widget(list, area, &mut state);
}

fn preview_lines(preview: &Preview, theme: Theme, mode: Mode) -> Vec<Line<'static>> {
    match preview {
        Preview::Empty => vec![Line::styled("No selection", theme.dim())],
        Preview::Text {
            lines,
            syntax_name,
            truncated,
            highlighting_limited,
        } => {
            let mut rendered = Vec::with_capacity(lines.len().saturating_add(2));
            if let Some(syntax_name) = syntax_name {
                rendered.push(Line::styled(
                    format!("source · {syntax_name}"),
                    theme.accent(mode),
                ));
            }
            rendered.extend(lines.iter().map(|line| {
                Line::from(
                    line.spans
                        .iter()
                        .map(|span| {
                            Span::styled(
                                escape_terminal_line(&span.text),
                                source_style(span.token_kind, theme, mode),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            }));
            if *highlighting_limited {
                rendered.push(Line::styled(
                    format!("… syntax styling limited to {PREVIEW_SYNTAX_SPANS_MAX} spans"),
                    theme.dim(),
                ));
            }
            if *truncated {
                rendered.push(Line::styled("… preview truncated", theme.dim()));
            }
            rendered
        }
        Preview::Binary { lines, size_bytes } => {
            let mut rendered = vec![Line::styled(
                format!("binary · {}", format_bytes(*size_bytes)),
                theme.accent(mode),
            )];
            rendered.extend(lines.iter().map(|line| Line::raw(line.clone())));
            rendered
        }
        Preview::Metadata(lines) => lines
            .iter()
            .map(|line| Line::raw(escape_terminal_line(line)))
            .collect(),
        Preview::Error(message) => vec![Line::styled(
            escape_terminal_line(message),
            theme.accent(mode),
        )],
        Preview::Directory(_) | Preview::Image(_) => Vec::new(),
    }
}

fn source_style(token_kind: SourceTokenKind, theme: Theme, mode: Mode) -> Style {
    match token_kind {
        SourceTokenKind::Plain => Style::default(),
        SourceTokenKind::Comment => theme.dim().add_modifier(Modifier::ITALIC),
        SourceTokenKind::String => theme.highlight(HighlightKind::StringDouble),
        SourceTokenKind::Number => theme.highlight(HighlightKind::Number),
        SourceTokenKind::Keyword => theme.accent(mode),
        SourceTokenKind::Type => theme.context_secondary(),
        SourceTokenKind::Function | SourceTokenKind::Tag => theme.context(),
        SourceTokenKind::Property => theme.highlight(HighlightKind::PathLike),
        SourceTokenKind::Operator => theme.highlight(HighlightKind::Operator),
        SourceTokenKind::Constant => theme.highlight(HighlightKind::Expansion),
        SourceTokenKind::Error => theme.highlight(HighlightKind::Error),
    }
}

fn render_image_preview(
    frame: &mut Frame<'_>,
    area: Rect,
    image: &ImagePreview,
    theme: Theme,
    mode: Mode,
    symbols: SurfaceSymbols,
) {
    if area.is_empty() {
        return;
    }
    let metadata = format!(
        "image · {}×{} · {}×{} retained",
        image.source_width,
        image.source_height,
        image.pixels.width(),
        image.pixels.height()
    );
    let Some(image_area) = area_rows_after_header(area) else {
        frame.render_widget(Paragraph::new(metadata).style(theme.accent(mode)), area);
        return;
    };
    frame.render_widget(
        Paragraph::new(metadata).style(theme.accent(mode)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if !theme.color_enabled() {
        frame.render_widget(
            Paragraph::new("color disabled; image pixels are not rendered").style(theme.dim()),
            image_area,
        );
        return;
    }
    let unicode = symbols.uses_unicode();
    let vertical_pixels_per_cell = if unicode { 2 } else { 1 };
    let Some((render_width, render_height_pixels)) = fitted_image_dimensions(
        image.pixels.width(),
        image.pixels.height(),
        image_area.width,
        image_area.height.saturating_mul(vertical_pixels_per_cell),
    ) else {
        return;
    };
    let render_rows = render_height_pixels
        .saturating_add(vertical_pixels_per_cell.saturating_sub(1))
        .checked_div(vertical_pixels_per_cell)
        .unwrap_or(0);
    let x_offset = image_area
        .width
        .checked_sub(render_width)
        .and_then(|remaining| remaining.checked_div(2))
        .unwrap_or(0);
    for row in 0..render_rows.min(image_area.height) {
        for column in 0..render_width.min(image_area.width) {
            let source_x = sample_coordinate(column, image.pixels.width(), render_width);
            let top_position = row.saturating_mul(vertical_pixels_per_cell);
            let source_y_top =
                sample_coordinate(top_position, image.pixels.height(), render_height_pixels);
            let top = image.pixels.get_pixel(source_x, source_y_top).0;
            let position = (
                image_area.x.saturating_add(x_offset).saturating_add(column),
                image_area.y.saturating_add(row),
            );
            let Some(cell) = frame.buffer_mut().cell_mut(position) else {
                continue;
            };
            if unicode {
                let bottom_position = top_position
                    .saturating_add(1)
                    .min(render_height_pixels.saturating_sub(1));
                let source_y_bottom =
                    sample_coordinate(bottom_position, image.pixels.height(), render_height_pixels);
                let bottom = image.pixels.get_pixel(source_x, source_y_bottom).0;
                cell.set_symbol("▀");
                cell.set_fg(rgba_color(top));
                cell.set_bg(rgba_color(bottom));
            } else {
                cell.set_symbol(" ");
                cell.set_bg(rgba_color(top));
            }
        }
    }
}

fn area_rows_after_header(area: Rect) -> Option<Rect> {
    (area.height > 1).then(|| {
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        )
    })
}

fn fitted_image_dimensions(
    source_width: u32,
    source_height: u32,
    width_max: u16,
    height_max: u16,
) -> Option<(u16, u16)> {
    if source_width == 0 || source_height == 0 || width_max == 0 || height_max == 0 {
        return None;
    }
    let width_max_u64 = u64::from(width_max);
    let height_max_u64 = u64::from(height_max);
    let source_width_u64 = u64::from(source_width);
    let source_height_u64 = u64::from(source_height);
    let width_limited = width_max_u64.saturating_mul(source_height_u64)
        <= height_max_u64.saturating_mul(source_width_u64);
    let (width, height) = if width_limited {
        let height = source_height_u64
            .saturating_mul(width_max_u64)
            .checked_div(source_width_u64)
            .unwrap_or(1)
            .max(1);
        (width_max_u64, height)
    } else {
        let width = source_width_u64
            .saturating_mul(height_max_u64)
            .checked_div(source_height_u64)
            .unwrap_or(1)
            .max(1);
        (width, height_max_u64)
    };
    Some((
        u16::try_from(width).unwrap_or(width_max).min(width_max),
        u16::try_from(height).unwrap_or(height_max).min(height_max),
    ))
}

fn sample_coordinate(position: u16, source_extent: u32, target_extent: u16) -> u32 {
    u64::from(position)
        .saturating_mul(u64::from(source_extent))
        .checked_div(u64::from(target_extent.max(1)))
        .and_then(|coordinate| u32::try_from(coordinate).ok())
        .unwrap_or(0)
        .min(source_extent.saturating_sub(1))
}

fn rgba_color(pixel: [u8; 4]) -> Color {
    if pixel[3] == 0 {
        Color::Rgb(0, 0, 0)
    } else {
        Color::Rgb(pixel[0], pixel[1], pixel[2])
    }
}

fn entry_glyph(entry: &Entry, symbols: SurfaceSymbols) -> &'static str {
    match (entry.kind, symbols) {
        (EntryKind::Directory, SurfaceSymbols::NerdFont) => "󰉋",
        (EntryKind::File, SurfaceSymbols::NerdFont) => "󰈔",
        (EntryKind::Symlink, SurfaceSymbols::NerdFont) => "󰌷",
        (EntryKind::Other, SurfaceSymbols::NerdFont) => "󰡯",
        (EntryKind::Directory, SurfaceSymbols::Unicode) => "▣",
        (EntryKind::File, SurfaceSymbols::Unicode) => "▫",
        (EntryKind::Symlink, SurfaceSymbols::Unicode) => "↗",
        (EntryKind::Other, SurfaceSymbols::Unicode) => "?",
        (EntryKind::Directory, SurfaceSymbols::Plain) => "d",
        (EntryKind::File, SurfaceSymbols::Plain) => "f",
        (EntryKind::Symlink, SurfaceSymbols::Plain) => "l",
        (EntryKind::Other, SurfaceSymbols::Plain) => "?",
    }
}

const fn next_sort(sort: DirectorySort) -> DirectorySort {
    match sort {
        DirectorySort::Name => DirectorySort::Size,
        DirectorySort::Size => DirectorySort::Modified,
        DirectorySort::Modified => DirectorySort::Kind,
        DirectorySort::Kind => DirectorySort::Name,
    }
}

const fn sort_label(sort: DirectorySort) -> &'static str {
    match sort {
        DirectorySort::Name => "name",
        DirectorySort::Size => "size",
        DirectorySort::Modified => "modified",
        DirectorySort::Kind => "kind",
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_unit(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_unit(bytes: u64, unit_bytes: u64, suffix: &str) -> String {
    let whole = bytes.checked_div(unit_bytes).unwrap_or(bytes);
    let decimal = bytes
        .checked_rem(unit_bytes)
        .and_then(|remainder| remainder.checked_mul(10))
        .and_then(|tenths| tenths.checked_div(unit_bytes))
        .unwrap_or(0);
    format!("{whole}.{decimal} {suffix}")
}

fn truncate_left(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let suffix_width = width.saturating_sub(1);
    let mut suffix = text
        .chars()
        .rev()
        .scan(0_usize, |used, character| {
            let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
            *used = used.saturating_add(character_width);
            (*used <= suffix_width).then_some(character)
        })
        .collect::<Vec<_>>();
    suffix.reverse();
    format!("…{}", suffix.into_iter().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::{
        env,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "quirl-explorer-{label}-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn rendered_text_with_color(
        explorer: &DirectoryExplorer,
        width: u16,
        height: u16,
        color: bool,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                explorer.render(
                    frame,
                    frame.area(),
                    Theme::new(color),
                    Mode::Command,
                    SurfaceSymbols::Unicode,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| {
                (0..buffer.area.width)
                    .filter_map(move |x| buffer.cell((x, y)))
                    .map(|cell| cell.symbol())
                    .chain(std::iter::once("\n"))
            })
            .collect()
    }

    fn rendered_text(explorer: &DirectoryExplorer, width: u16, height: u16) -> String {
        rendered_text_with_color(explorer, width, height, true)
    }

    #[test]
    fn navigation_is_transactional_until_acceptance() {
        let directory = TestDirectory::new("transaction");
        let child = directory.0.join("child");
        fs::create_dir(&child).unwrap();
        let mut explorer = DirectoryExplorer::open(&directory.0).unwrap();

        assert_eq!(explorer.current_dir, directory.0);
        assert_eq!(
            explorer.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            ExplorerAction::Pending
        );
        assert_eq!(explorer.current_dir, child);
        assert_eq!(
            explorer.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            ExplorerAction::Dismiss
        );
    }

    #[test]
    fn wide_and_narrow_layouts_prioritize_the_expected_columns() {
        let directory = TestDirectory::new("layout");
        fs::create_dir(directory.0.join("child")).unwrap();
        fs::write(directory.0.join("preview.txt"), "preview body").unwrap();
        let explorer = DirectoryExplorer::open(&directory.0).unwrap();

        let wide = rendered_text(&explorer, 120, 20);
        assert!(wide.contains("Quirl Explorer"));
        assert!(wide.contains("parent"));
        assert!(wide.contains("preview"));
        assert!(wide.contains("child"));

        let narrow = rendered_text(&explorer, 48, 16);
        assert!(narrow.contains("Quirl Explorer"));
        assert!(narrow.contains("child"));
        assert!(!narrow.contains(" parent "));
    }

    #[test]
    fn enter_accepts_a_selected_directory_and_a_files_parent() {
        let directory = TestDirectory::new("accept");
        let child = directory.0.join("child");
        fs::create_dir(&child).unwrap();
        fs::write(directory.0.join("file.txt"), "hello").unwrap();
        let mut explorer = DirectoryExplorer::open(&directory.0).unwrap();

        assert_eq!(
            explorer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ExplorerAction::ChangeDirectory(child)
        );
        explorer.move_selection(1);
        assert_eq!(
            explorer.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            ExplorerAction::ChangeDirectory(directory.0.clone())
        );
    }

    #[test]
    fn filter_is_bounded_and_cancel_restores_all_entries() {
        let directory = TestDirectory::new("filter");
        fs::create_dir(directory.0.join("alpha")).unwrap();
        fs::create_dir(directory.0.join("beta")).unwrap();
        let mut explorer = DirectoryExplorer::open(&directory.0).unwrap();

        explorer.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        explorer.insert_query("alp");
        assert_eq!(explorer.visible_indices.len(), 1);
        explorer.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(explorer.visible_indices.len(), 2);

        explorer.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        explorer.insert_query(&"x".repeat(EXPLORER_QUERY_BYTES_MAX + 1));
        assert!(explorer.notice.is_some());
        assert!(explorer.query.is_empty());
    }

    #[test]
    fn text_and_binary_previews_obey_byte_bounds() {
        let directory = TestDirectory::new("preview");
        let text = directory.0.join("a.txt");
        let binary = directory.0.join("b.bin");
        fs::write(&text, "hello\nworld\n").unwrap();
        fs::write(&binary, [0, 1, 2, 3]).unwrap();
        let text_entry = list_directory(&directory.0, false, DirectorySort::Name)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == text)
            .unwrap();
        let binary_entry = list_directory(&directory.0, false, DirectorySort::Name)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == binary)
            .unwrap();

        assert!(matches!(load_preview(&text_entry), Preview::Text { .. }));
        assert!(matches!(
            load_preview(&binary_entry),
            Preview::Binary { .. }
        ));

        let oversized = directory.0.join("oversized.txt");
        fs::write(&oversized, "x".repeat(PREVIEW_BYTES_MAX + 1)).unwrap();
        let oversized_entry = list_directory(&directory.0, false, DirectorySort::Name)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == oversized)
            .unwrap();
        assert!(matches!(
            load_preview(&oversized_entry),
            Preview::Text {
                truncated: true,
                ..
            }
        ));

        let many_lines = directory.0.join("many-lines.txt");
        fs::write(&many_lines, "x\n".repeat(PREVIEW_LINES_MAX + 1)).unwrap();
        let many_lines_entry = list_directory(&directory.0, false, DirectorySort::Name)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == many_lines)
            .unwrap();
        let Preview::Text {
            lines, truncated, ..
        } = load_preview(&many_lines_entry)
        else {
            panic!("many-line UTF-8 input must remain a text preview");
        };
        assert_eq!(lines.len(), PREVIEW_LINES_MAX);
        assert!(truncated);
    }

    #[test]
    fn source_preview_uses_bundled_syntax_and_bounds_styled_spans() {
        let source = "fn main() { let message = \"hello\"; println!(\"{message}\"); }";
        let (lines, syntax_name, highlighting_limited) =
            highlight_source(Path::new("main.rs"), source);
        assert_eq!(syntax_name.as_deref(), Some("Rust"));
        assert!(!highlighting_limited);
        assert!(
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.token_kind != SourceTokenKind::Plain)
        );
        assert!(
            source_style(SourceTokenKind::String, Theme::new(false), Mode::Command,)
                .fg
                .is_none()
        );

        let (_, _, highlighting_limited) =
            highlight_source_with_limit(Path::new("main.rs"), source, 1);
        assert!(highlighting_limited);
    }

    #[test]
    fn bundled_syntax_inventory_has_seventy_five_definitions() {
        let syntax_set = SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_nonewlines);
        assert_eq!(syntax_set.syntaxes().len(), 75);
        assert!(syntax_set.find_syntax_by_extension("toml").is_none());
    }

    #[test]
    fn toml_preview_highlights_semantic_tokens_with_quirl_roles() {
        let source = concat!(
            "[package]\n",
            "name = \"quirl\"\n",
            "version = 1.2\n",
            "publish = false # private\n",
            "description = \"\"\"a\nmultiline value\"\"\"\n",
        );
        let (lines, syntax_name, highlighting_limited) =
            highlight_source(Path::new("Cargo.TOML"), source);
        let kinds = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.token_kind))
            .collect::<Vec<_>>();

        assert_eq!(syntax_name.as_deref(), Some("TOML"));
        assert!(!highlighting_limited);
        assert!(kinds.contains(&SourceTokenKind::Property));
        assert!(kinds.contains(&SourceTokenKind::String));
        assert!(kinds.contains(&SourceTokenKind::Number));
        assert!(kinds.contains(&SourceTokenKind::Constant));
        assert!(kinds.contains(&SourceTokenKind::Comment));
        assert_eq!(
            source_style(SourceTokenKind::String, Theme::new(true), Mode::Command,),
            Theme::new(true).highlight(HighlightKind::StringDouble)
        );
    }

    #[test]
    fn image_preview_decodes_to_a_bounded_colored_thumbnail() {
        let directory = TestDirectory::new("image");
        let path = directory.0.join("sample.png");
        let pixels = RgbaImage::from_fn(12, 8, |x, y| {
            let red = u8::try_from(x.saturating_mul(20)).unwrap_or(u8::MAX);
            let green = u8::try_from(y.saturating_mul(30)).unwrap_or(u8::MAX);
            image::Rgba([red, green, 180, 255])
        });
        pixels.save(&path).unwrap();
        let entry = list_directory(&directory.0, false, DirectorySort::Name)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == path)
            .unwrap();

        let Preview::Image(image) = load_preview(&entry) else {
            panic!("bounded PNG must produce an image preview");
        };
        assert_eq!((image.source_width, image.source_height), (12, 8));
        assert!(image.pixels.width() <= IMAGE_THUMBNAIL_WIDTH_MAX);
        assert!(image.pixels.height() <= IMAGE_THUMBNAIL_HEIGHT_MAX);

        let explorer = DirectoryExplorer::open(&directory.0).unwrap();
        let rendered = rendered_text(&explorer, 100, 24);
        assert!(rendered.contains("image · 12×8"));
        assert!(rendered.contains('▀'));

        let no_color = rendered_text_with_color(&explorer, 100, 24, false);
        assert!(no_color.contains("color disabled"));
        assert!(!no_color.contains('▀'));
    }

    #[test]
    fn image_preview_rejects_encoded_and_dimension_limit_overflow() {
        let directory = TestDirectory::new("image-bounds");
        let oversized_bytes_path = directory.0.join("oversized.png");
        let oversized_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&oversized_bytes_path)
            .unwrap();
        oversized_file
            .set_len(
                u64::try_from(IMAGE_ENCODED_BYTES_MAX)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            )
            .unwrap();
        let oversized_entry = list_directory(&directory.0, false, DirectorySort::Name)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == oversized_bytes_path)
            .unwrap();
        assert!(matches!(load_preview(&oversized_entry), Preview::Error(_)));

        let oversized_dimensions_path = directory.0.join("wide.png");
        RgbaImage::new(IMAGE_DIMENSION_MAX.saturating_add(1), 1)
            .save(&oversized_dimensions_path)
            .unwrap();
        let oversized_dimensions_entry = list_directory(&directory.0, false, DirectorySort::Name)
            .unwrap()
            .into_iter()
            .find(|entry| entry.path == oversized_dimensions_path)
            .unwrap();
        assert!(matches!(
            load_preview(&oversized_dimensions_entry),
            Preview::Error(_)
        ));
    }

    #[test]
    fn listing_rejects_entry_and_retained_name_byte_overflow() {
        let directory = TestDirectory::new("listing-bounds");
        fs::write(directory.0.join("one"), "1").unwrap();
        fs::write(directory.0.join("two"), "2").unwrap();

        let entry_error = list_directory_with_limits(
            &directory.0,
            false,
            DirectorySort::Name,
            1,
            EXPLORER_RETAINED_NAME_BYTES_MAX,
        )
        .unwrap_err();
        assert_eq!(entry_error.code, ErrorCode::ResourceLimit);

        let byte_error = list_directory_with_limits(
            &directory.0,
            false,
            DirectorySort::Name,
            EXPLORER_ENTRIES_MAX,
            5,
        )
        .unwrap_err();
        assert_eq!(byte_error.code, ErrorCode::ResourceLimit);
        assert!(byte_error.message.contains("limit is 5"));
    }

    #[test]
    fn left_truncation_preserves_utf8_boundaries() {
        assert_eq!(truncate_left("路径/資料/quirl", 7), "…/quirl");
    }
}
