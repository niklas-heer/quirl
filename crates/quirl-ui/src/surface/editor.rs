use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;

pub(super) const MAX_EDITOR_BUFFER_BYTES: usize = 64 * 1024;
pub(super) const MAX_HISTORY_ENTRY_BYTES: usize = MAX_EDITOR_BUFFER_BYTES;
pub(super) const MAX_HISTORY_ENCODED_ENTRY_BYTES: usize = MAX_HISTORY_ENTRY_BYTES * 4;
pub(super) const MAX_HISTORY_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const MAX_UNDO_STATES: usize = 256;
const MAX_UNDO_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    Emacs,
    HelixInsert,
    HelixNormal,
    VimInsert,
    VimNormal,
    VimVisual,
}

impl EditorMode {
    pub fn from_keymap(keymap: &str) -> Self {
        match keymap {
            "helix" => Self::HelixInsert,
            "vim" => Self::VimInsert,
            _ => Self::Emacs,
        }
    }

    pub const fn label(self) -> Option<&'static str> {
        match self {
            Self::Emacs => None,
            Self::HelixInsert | Self::VimInsert => Some("INS"),
            Self::HelixNormal | Self::VimNormal => Some("NOR"),
            Self::VimVisual => Some("VIS"),
        }
    }

    pub const fn is_insert(self) -> bool {
        matches!(self, Self::Emacs | Self::HelixInsert | Self::VimInsert)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    History,
    Files,
    Directories,
    Palette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditAction {
    None,
    Insert(char),
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    MoveHome,
    MoveEnd,
    MoveWordLeft,
    MoveWordRight,
    KillToStart,
    KillWord,
    Yank,
    HistoryPrev,
    HistoryNext,
    Accept,
    ForceNewline,
    Complete,
    ExpandCompletionPicker,
    Dismiss,
    ToggleGrammarMode,
    OpenPicker(PickerKind),
    Cancel,
    ClearScreen,
    Suspend,
    Eof,
    Undo,
    Redo,
}

#[derive(Debug, Clone)]
pub struct EditorState {
    buffer: String,
    cursor: usize,
    revision: u64,
    mode: EditorMode,
    undo: VecDeque<(String, usize)>,
    redo: VecDeque<(String, usize)>,
    undo_bytes: usize,
    redo_bytes: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_prefix: String,
    pasted_lines: Option<usize>,
    kill_ring: Option<String>,
    resource_notice: Option<String>,
}

impl EditorState {
    pub fn new(keymap: &str, history: Vec<String>) -> Self {
        Self {
            buffer: String::new(),
            cursor: 0,
            revision: 0,
            mode: EditorMode::from_keymap(keymap),
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            undo_bytes: 0,
            redo_bytes: 0,
            history: bounded_history(history),
            history_index: None,
            history_prefix: String::new(),
            pasted_lines: None,
            kill_ring: None,
            resource_notice: None,
        }
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Monotonically identifies the buffer contents for single-line analysis caches.
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn mode(&self) -> EditorMode {
        self.mode
    }

    pub const fn pasted_lines(&self) -> Option<usize> {
        self.pasted_lines
    }

    pub fn resource_notice(&self) -> Option<&str> {
        self.resource_notice.as_deref()
    }

    pub fn autosuggestion(&self) -> Option<&str> {
        if self.buffer.is_empty() || self.cursor != self.buffer.len() {
            return None;
        }
        self.history
            .iter()
            .rev()
            .find(|entry| entry.starts_with(&self.buffer) && entry.as_str() != self.buffer)
            .and_then(|entry| entry.get(self.buffer.len()..))
    }

    pub fn apply_key(&mut self, key: KeyEvent, popup_open: bool) -> EditAction {
        let modifiers = key.modifiers;
        if modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('d') => EditAction::Eof,
                KeyCode::Char('c') => EditAction::Cancel,
                KeyCode::Char(' ') => EditAction::ToggleGrammarMode,
                KeyCode::Char('r') => EditAction::OpenPicker(PickerKind::History),
                KeyCode::Char('t') => EditAction::OpenPicker(PickerKind::Files),
                KeyCode::Char('k') => EditAction::OpenPicker(PickerKind::Palette),
                KeyCode::Char('l') => EditAction::ClearScreen,
                KeyCode::Char('z') => EditAction::Suspend,
                KeyCode::Char('a') => EditAction::MoveHome,
                KeyCode::Char('e') => EditAction::MoveEnd,
                KeyCode::Char('u') => EditAction::KillToStart,
                KeyCode::Char('w') => EditAction::KillWord,
                KeyCode::Char('y') => EditAction::Yank,
                KeyCode::Char('_') => EditAction::Undo,
                _ => EditAction::None,
            };
        }
        if modifiers.contains(KeyModifiers::ALT) {
            return match key.code {
                KeyCode::Char('m') => EditAction::ToggleGrammarMode,
                KeyCode::Enter => EditAction::ForceNewline,
                KeyCode::Char('c') => EditAction::OpenPicker(PickerKind::Directories),
                KeyCode::Char('b') => EditAction::MoveWordLeft,
                KeyCode::Char('f') => EditAction::MoveWordRight,
                KeyCode::Char('u') => EditAction::Undo,
                KeyCode::Char('r') => EditAction::Redo,
                _ => EditAction::None,
            };
        }
        if key.code == KeyCode::Esc {
            if popup_open {
                return EditAction::Dismiss;
            }
            self.mode = match self.mode {
                EditorMode::HelixInsert => EditorMode::HelixNormal,
                EditorMode::VimInsert | EditorMode::VimVisual => EditorMode::VimNormal,
                mode => mode,
            };
            return EditAction::None;
        }
        if !self.mode.is_insert() {
            return match key.code {
                KeyCode::Char('i') => {
                    self.mode = match self.mode {
                        EditorMode::HelixNormal => EditorMode::HelixInsert,
                        _ => EditorMode::VimInsert,
                    };
                    EditAction::None
                }
                KeyCode::Char('v') if self.mode == EditorMode::VimNormal => {
                    self.mode = EditorMode::VimVisual;
                    EditAction::None
                }
                KeyCode::Char('h') | KeyCode::Left => EditAction::MoveLeft,
                KeyCode::Char('l') | KeyCode::Right => EditAction::MoveRight,
                KeyCode::Char('b') => EditAction::MoveWordLeft,
                KeyCode::Char('w') => EditAction::MoveWordRight,
                KeyCode::Char('x') | KeyCode::Delete => EditAction::Delete,
                KeyCode::Char('u') => EditAction::Undo,
                KeyCode::Enter => EditAction::Accept,
                _ => EditAction::None,
            };
        }
        match key.code {
            KeyCode::Char(character) => EditAction::Insert(character),
            KeyCode::Backspace => EditAction::Backspace,
            KeyCode::Delete => EditAction::Delete,
            KeyCode::Left => EditAction::MoveLeft,
            KeyCode::Right => EditAction::MoveRight,
            KeyCode::Home => EditAction::MoveHome,
            KeyCode::End => EditAction::MoveEnd,
            KeyCode::Up => EditAction::HistoryPrev,
            KeyCode::Down => EditAction::HistoryNext,
            KeyCode::Enter => EditAction::Accept,
            KeyCode::Tab if modifiers.contains(KeyModifiers::SHIFT) => {
                EditAction::ExpandCompletionPicker
            }
            KeyCode::BackTab => EditAction::ExpandCompletionPicker,
            KeyCode::Tab => EditAction::Complete,
            _ => EditAction::None,
        }
    }

    pub fn apply(&mut self, action: EditAction) -> bool {
        self.pasted_lines = None;
        self.resource_notice = None;
        let buffer_changed = match action {
            EditAction::Insert(character) => {
                if self.buffer.len().saturating_add(character.len_utf8()) > MAX_EDITOR_BUFFER_BYTES
                {
                    self.set_buffer_limit_notice("input");
                    return false;
                }
                self.record_edit();
                self.buffer.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                true
            }
            EditAction::Backspace => {
                let previous = previous_boundary(&self.buffer, self.cursor);
                if previous == self.cursor {
                    return false;
                }
                self.record_edit();
                self.buffer.drain(previous..self.cursor);
                self.cursor = previous;
                true
            }
            EditAction::Delete => {
                let next = next_boundary(&self.buffer, self.cursor);
                if next == self.cursor {
                    return false;
                }
                self.record_edit();
                self.buffer.drain(self.cursor..next);
                true
            }
            EditAction::MoveLeft => {
                self.cursor = previous_boundary(&self.buffer, self.cursor);
                false
            }
            EditAction::MoveRight => {
                self.cursor = next_boundary(&self.buffer, self.cursor);
                false
            }
            EditAction::MoveHome => {
                self.cursor = line_start(&self.buffer, self.cursor);
                false
            }
            EditAction::MoveEnd => {
                self.cursor = line_end(&self.buffer, self.cursor);
                false
            }
            EditAction::MoveWordLeft => {
                self.cursor = word_left(&self.buffer, self.cursor);
                false
            }
            EditAction::MoveWordRight => {
                self.cursor = word_right(&self.buffer, self.cursor);
                false
            }
            EditAction::KillToStart => {
                let start = line_start(&self.buffer, self.cursor);
                if start == self.cursor {
                    return false;
                }
                self.record_edit();
                self.kill_ring = Some(self.buffer[start..self.cursor].to_owned());
                self.buffer.drain(start..self.cursor);
                self.cursor = start;
                true
            }
            EditAction::KillWord => {
                let start = word_left(&self.buffer, self.cursor);
                if start == self.cursor {
                    return false;
                }
                self.record_edit();
                self.kill_ring = Some(self.buffer[start..self.cursor].to_owned());
                self.buffer.drain(start..self.cursor);
                self.cursor = start;
                true
            }
            EditAction::Yank => {
                let Some(value) = self.kill_ring.clone() else {
                    return false;
                };
                if self.buffer.len().saturating_add(value.len()) > MAX_EDITOR_BUFFER_BYTES {
                    self.set_buffer_limit_notice("yank");
                    return false;
                }
                self.record_edit();
                self.buffer.insert_str(self.cursor, &value);
                self.cursor = self.cursor.saturating_add(value.len());
                true
            }
            EditAction::ForceNewline => {
                if self.buffer.len() == MAX_EDITOR_BUFFER_BYTES {
                    self.set_buffer_limit_notice("newline");
                    return false;
                }
                self.record_edit();
                self.buffer.insert(self.cursor, '\n');
                self.cursor += 1;
                true
            }
            EditAction::HistoryPrev => {
                self.history_previous();
                true
            }
            EditAction::HistoryNext => {
                self.history_next();
                true
            }
            EditAction::Undo => {
                self.undo();
                true
            }
            EditAction::Redo => {
                self.redo();
                true
            }
            _ => return false,
        };
        if buffer_changed {
            self.revision = self.revision.saturating_add(1);
        }
        true
    }

    pub fn insert_paste(&mut self, text: &str) {
        self.pasted_lines = None;
        self.resource_notice = None;
        let available = MAX_EDITOR_BUFFER_BYTES.saturating_sub(self.buffer.len());
        let insert_bytes = char_boundary_at_or_before(text, available.min(text.len()));
        if insert_bytes == 0 {
            if !text.is_empty() {
                self.set_buffer_limit_notice("paste");
            }
            return;
        }
        self.record_edit();
        self.buffer.insert_str(self.cursor, &text[..insert_bytes]);
        self.cursor = self.cursor.saturating_add(insert_bytes);
        self.revision = self.revision.saturating_add(1);
        if insert_bytes < text.len() {
            self.set_buffer_limit_notice("paste");
        } else {
            self.pasted_lines = Some(text.lines().count().max(1));
        }
    }

    pub fn accept_suggestion(&mut self) -> bool {
        let Some(suffix) = self.autosuggestion().map(str::to_owned) else {
            return false;
        };
        self.pasted_lines = None;
        self.resource_notice = None;
        if self.buffer.len().saturating_add(suffix.len()) > MAX_EDITOR_BUFFER_BYTES {
            self.set_buffer_limit_notice("suggestion");
            return false;
        }
        self.record_edit();
        self.buffer.push_str(&suffix);
        self.cursor = self.buffer.len();
        self.revision = self.revision.saturating_add(1);
        true
    }

    pub fn replace(&mut self, start: usize, end: usize, value: &str) {
        let start = start.min(self.buffer.len());
        let end = end.max(start).min(self.buffer.len());
        if !self.buffer.is_char_boundary(start) || !self.buffer.is_char_boundary(end) {
            return;
        }
        self.pasted_lines = None;
        self.resource_notice = None;
        let replaced_bytes = end.saturating_sub(start);
        let final_bytes = self
            .buffer
            .len()
            .saturating_sub(replaced_bytes)
            .saturating_add(value.len());
        if final_bytes > MAX_EDITOR_BUFFER_BYTES {
            self.set_buffer_limit_notice("completion");
            return;
        }
        self.record_edit();
        self.buffer.replace_range(start..end, value);
        self.cursor = start.saturating_add(value.len());
        self.revision = self.revision.saturating_add(1);
    }

    pub fn clear(&mut self) {
        if !self.buffer.is_empty() {
            self.record_edit();
            self.buffer.clear();
            self.cursor = 0;
            self.revision = self.revision.saturating_add(1);
        }
    }

    fn record_edit(&mut self) {
        push_undo_state(
            &mut self.undo,
            &mut self.undo_bytes,
            (self.buffer.clone(), self.cursor),
        );
        self.redo.clear();
        self.redo_bytes = 0;
        self.history_index = None;
    }

    fn undo(&mut self) {
        if let Some((buffer, cursor)) = self.undo.pop_back() {
            self.undo_bytes = self.undo_bytes.saturating_sub(buffer.len());
            push_undo_state(
                &mut self.redo,
                &mut self.redo_bytes,
                (self.buffer.clone(), self.cursor),
            );
            self.buffer = buffer;
            self.cursor = cursor;
        }
    }

    fn redo(&mut self) {
        if let Some((buffer, cursor)) = self.redo.pop_back() {
            self.redo_bytes = self.redo_bytes.saturating_sub(buffer.len());
            push_undo_state(
                &mut self.undo,
                &mut self.undo_bytes,
                (self.buffer.clone(), self.cursor),
            );
            self.buffer = buffer;
            self.cursor = cursor;
        }
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.history_prefix.clone_from(&self.buffer);
        }
        let start = self.history_index.unwrap_or(self.history.len());
        if let Some(index) = (0..start)
            .rev()
            .find(|index| self.history[*index].starts_with(&self.history_prefix))
        {
            self.history_index = Some(index);
            self.buffer.clone_from(&self.history[index]);
            self.cursor = self.buffer.len();
        }
    }

    fn history_next(&mut self) {
        let Some(current) = self.history_index else {
            return;
        };
        if let Some(index) = ((current + 1)..self.history.len())
            .find(|index| self.history[*index].starts_with(&self.history_prefix))
        {
            self.history_index = Some(index);
            self.buffer.clone_from(&self.history[index]);
        } else {
            self.history_index = None;
            self.buffer.clone_from(&self.history_prefix);
        }
        self.cursor = self.buffer.len();
    }

    fn set_buffer_limit_notice(&mut self, operation: &str) {
        self.resource_notice = Some(format!(
            "{operation} limited to {MAX_EDITOR_BUFFER_BYTES} editor bytes"
        ));
    }
}

fn char_boundary_at_or_before(value: &str, mut index: usize) -> usize {
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn push_undo_state(
    stack: &mut VecDeque<(String, usize)>,
    retained_bytes: &mut usize,
    state: (String, usize),
) {
    *retained_bytes = retained_bytes.saturating_add(state.0.len());
    stack.push_back(state);
    while stack.len() > MAX_UNDO_STATES || *retained_bytes > MAX_UNDO_BYTES {
        let Some((buffer, _)) = stack.pop_front() else {
            break;
        };
        *retained_bytes = retained_bytes.saturating_sub(buffer.len());
    }
}

fn bounded_history(history: Vec<String>) -> Vec<String> {
    let mut retained_bytes = 0_usize;
    let mut retained = history
        .into_iter()
        .rev()
        .take(50_000)
        .take_while(|entry| {
            if entry.len() > MAX_HISTORY_ENTRY_BYTES {
                return true;
            }
            let next = retained_bytes.saturating_add(entry.len());
            if next > MAX_HISTORY_RETAINED_BYTES {
                return false;
            }
            retained_bytes = next;
            true
        })
        .filter(|entry| entry.len() <= MAX_HISTORY_ENTRY_BYTES)
        .collect::<Vec<_>>();
    retained.reverse();
    retained
}

fn previous_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor.min(value.len())]
        .grapheme_indices(true)
        .next_back()
        .map_or(cursor, |(index, _)| index)
}

fn next_boundary(value: &str, cursor: usize) -> usize {
    value[cursor.min(value.len())..]
        .grapheme_indices(true)
        .nth(1)
        .map_or(value.len(), |(offset, _)| cursor.saturating_add(offset))
}

fn line_start(value: &str, cursor: usize) -> usize {
    value[..cursor.min(value.len())]
        .rfind('\n')
        .map_or(0, |index| index.saturating_add(1))
}

fn line_end(value: &str, cursor: usize) -> usize {
    value[cursor.min(value.len())..]
        .find('\n')
        .map_or(value.len(), |offset| cursor.saturating_add(offset))
}

fn word_left(value: &str, cursor: usize) -> usize {
    let prefix = &value[..cursor.min(value.len())];
    let trimmed = prefix.trim_end_matches(char::is_whitespace);
    trimmed
        .rfind(char::is_whitespace)
        .map_or(0, |index| index.saturating_add(1))
}

fn word_right(value: &str, cursor: usize) -> usize {
    let suffix = &value[cursor.min(value.len())..];
    let leading = suffix
        .len()
        .saturating_sub(suffix.trim_start_matches(char::is_whitespace).len());
    let word = &suffix[leading..];
    let word_len = word.find(char::is_whitespace).unwrap_or(word.len());
    cursor.saturating_add(leading).saturating_add(word_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn deletion_and_motion_follow_grapheme_boundaries() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("a👨‍👩‍👧‍👦b");
        assert!(editor.apply(EditAction::Backspace));
        assert!(editor.apply(EditAction::Backspace));
        assert_eq!(editor.buffer(), "a");
        assert_eq!(editor.cursor(), 1);
    }

    #[test]
    fn bracketed_paste_never_becomes_an_accept_action() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("printf one\nprintf two");
        assert_eq!(editor.pasted_lines(), Some(2));
        assert_eq!(editor.buffer(), "printf one\nprintf two");
    }

    #[test]
    fn history_recall_is_prefix_aware() {
        let mut editor = EditorState::new(
            "emacs",
            vec![
                "git status".to_owned(),
                "cargo test".to_owned(),
                "git log".to_owned(),
            ],
        );
        assert!(editor.apply(EditAction::Insert('g')));
        editor.apply(EditAction::HistoryPrev);
        assert_eq!(editor.buffer(), "git log");
        editor.apply(EditAction::HistoryPrev);
        assert_eq!(editor.buffer(), "git status");
    }

    #[test]
    fn all_keymaps_share_insert_delete_navigation_and_eof_conformance() {
        for keymap in ["emacs", "helix", "vim"] {
            let mut editor = EditorState::new(keymap, Vec::new());
            for character in ['a', 'b', 'c', 'd'] {
                let action = editor.apply_key(
                    KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
                    false,
                );
                assert!(editor.apply(action), "insert failed for {keymap}");
            }
            let action =
                editor.apply_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), false);
            assert!(editor.apply(action));
            let action = editor.apply_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), false);
            editor.apply(action);
            let action =
                editor.apply_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), false);
            assert!(editor.apply(action));
            assert_eq!(editor.buffer(), "ab", "edit mismatch for {keymap}");
            assert_eq!(
                editor.apply_key(
                    KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                    false,
                ),
                EditAction::Eof
            );
        }
    }

    #[test]
    fn alt_m_toggles_grammar_mode_in_every_keymap() {
        for keymap in ["emacs", "helix", "vim"] {
            let mut editor = EditorState::new(keymap, Vec::new());
            assert_eq!(
                editor.apply_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT), false,),
                EditAction::ToggleGrammarMode,
                "Alt-M mode toggle in {keymap} keymap"
            );
        }
    }

    #[test]
    fn ctrl_space_remains_a_compatibility_mode_toggle() {
        let mut editor = EditorState::new("emacs", Vec::new());
        assert_eq!(
            editor.apply_key(
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
                false,
            ),
            EditAction::ToggleGrammarMode
        );
    }

    #[test]
    fn shift_tab_expands_completion_in_every_keymap() {
        for keymap in ["emacs", "helix", "vim"] {
            let mut editor = EditorState::new(keymap, Vec::new());
            for key in [
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            ] {
                assert_eq!(
                    editor.apply_key(key, false),
                    EditAction::ExpandCompletionPicker,
                    "Shift-Tab expansion in {keymap} keymap"
                );
            }
        }
    }

    #[test]
    fn emacs_kill_ring_and_word_motions_are_grapheme_safe() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("echo hello world");
        assert!(editor.apply(EditAction::KillWord));
        assert_eq!(editor.buffer(), "echo hello ");
        assert!(editor.apply(EditAction::Yank));
        assert_eq!(editor.buffer(), "echo hello world");
        assert!(editor.apply(EditAction::MoveWordLeft));
        assert_eq!(&editor.buffer()[editor.cursor()..], "world");
    }

    #[test]
    fn oversized_paste_is_utf8_safe_and_reports_the_editor_limit() {
        let mut editor = EditorState::new("emacs", Vec::new());
        let input = format!("{}💥", "a".repeat(MAX_EDITOR_BUFFER_BYTES));
        editor.insert_paste(&input);
        assert_eq!(editor.buffer().len(), MAX_EDITOR_BUFFER_BYTES);
        assert!(editor.buffer().is_char_boundary(editor.buffer().len()));
        assert!(editor.resource_notice().unwrap().contains("paste limited"));
    }

    #[test]
    fn cursor_motion_reuses_the_same_buffer_revision() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("abc");
        let revision = editor.revision();
        assert!(editor.apply(EditAction::MoveLeft));
        assert_eq!(editor.revision(), revision);
    }

    #[test]
    fn history_and_undo_storage_have_explicit_byte_bounds() {
        let oversized = "x".repeat(MAX_HISTORY_ENTRY_BYTES + 1);
        let mut editor = EditorState::new("emacs", vec![oversized, "safe".to_owned()]);
        editor.apply(EditAction::HistoryPrev);
        assert_eq!(editor.buffer(), "safe");
        for _ in 0..MAX_UNDO_STATES.saturating_add(32) {
            editor.apply(EditAction::Insert('x'));
        }
        assert!(editor.undo.len() <= MAX_UNDO_STATES);
        assert!(editor.undo_bytes <= MAX_UNDO_BYTES);
    }
}
