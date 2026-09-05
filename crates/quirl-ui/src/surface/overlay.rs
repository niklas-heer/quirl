use super::{
    completion::{
        CompletionItem, CompletionKind, FilePickerReplacement, clamped_utf8_cursor,
        shell_segment_range,
    },
    editor::{MAX_EDITOR_BUFFER_BYTES, PickerKind, after_last_whitespace},
};
use crate::{
    InteractiveHistoryEntry, PICKER_QUERY_BYTES_MAX, PICKER_RANKING_TEXT_BYTES_MAX, PickerItem,
    PickerItemKind, PickerRanker,
};
use quirl_catalog::Catalog;
use std::{fs, path::Path, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

const OVERLAY_ITEMS_MAX: usize = 4_096;
const OVERLAY_RESULTS_MAX: usize = 256;
const OVERLAY_ITEM_BYTES_MAX: usize = 16 * 1_024;
const OVERLAY_RETAINED_BYTES_MAX: usize = 2 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerLayout {
    Adaptive,
    Bottom,
    Full,
}

impl PickerLayout {
    pub fn from_config(value: &str) -> Self {
        match value {
            "bottom" => Self::Bottom,
            "full" => Self::Full,
            _ => Self::Adaptive,
        }
    }
}

pub struct PickerOverlay {
    source: Vec<CompletionItem>,
    ranking: Vec<PickerItem>,
    ranker: Arc<dyn PickerRanker>,
    query: String,
    label: &'static str,
    active: bool,
    expanded: bool,
    bottom_anchored: bool,
}

impl PickerOverlay {
    pub fn new(ranker: Arc<dyn PickerRanker>) -> Self {
        Self {
            source: Vec::new(),
            ranking: Vec::new(),
            ranker,
            query: String::new(),
            label: "picker",
            active: false,
            expanded: false,
            bottom_anchored: false,
        }
    }

    pub fn open(
        &mut self,
        items: Vec<CompletionItem>,
        label: &'static str,
        expanded: bool,
    ) -> Vec<CompletionItem> {
        self.open_with_query(items, label, expanded, "")
    }

    pub fn open_bottom_anchored(
        &mut self,
        items: Vec<CompletionItem>,
        label: &'static str,
    ) -> Vec<CompletionItem> {
        self.open_bottom_anchored_with_query(items, label, "")
    }

    pub(super) fn open_bottom_anchored_with_query(
        &mut self,
        items: Vec<CompletionItem>,
        label: &'static str,
        query: &str,
    ) -> Vec<CompletionItem> {
        let visible = self.open_with_query(items, label, false, query);
        self.bottom_anchored = true;
        visible
    }

    pub fn open_with_query(
        &mut self,
        items: Vec<CompletionItem>,
        label: &'static str,
        expanded: bool,
        query: &str,
    ) -> Vec<CompletionItem> {
        self.dismiss();
        self.label = label;
        self.expanded = expanded;
        self.active = true;
        self.query = truncate_utf8(query, PICKER_QUERY_BYTES_MAX);

        let mut retained_bytes = 0_usize;
        for item in items.into_iter().take(OVERLAY_ITEMS_MAX) {
            let item_bytes = completion_item_bytes(&item);
            if item_bytes > OVERLAY_ITEM_BYTES_MAX
                || retained_bytes.saturating_add(item_bytes) > OVERLAY_RETAINED_BYTES_MAX
            {
                continue;
            }
            retained_bytes = retained_bytes.saturating_add(item_bytes);
            let index = self.source.len();
            self.ranking.push(PickerItem {
                id: format!("{label}:{index}"),
                kind: picker_item_kind(item.kind),
                label: truncate_utf8(&item.display, PICKER_RANKING_TEXT_BYTES_MAX),
                description: truncate_utf8(&item.summary, PICKER_RANKING_TEXT_BYTES_MAX),
                preview: None,
                value: item.value.clone(),
                rank_bias: if item.source == "history-local" {
                    4_000
                } else {
                    0
                },
            });
            self.source.push(item);
        }
        self.ranked_items()
    }

    pub const fn active(&self) -> bool {
        self.active
    }

    pub const fn expanded(&self) -> bool {
        self.expanded
    }

    pub const fn bottom_anchored(&self) -> bool {
        self.bottom_anchored
    }

    pub fn query(&self) -> Option<&str> {
        self.active.then_some(self.query.as_str())
    }

    pub const fn label(&self) -> &'static str {
        self.label
    }

    pub fn insert_query(&mut self, text: &str) -> Option<Vec<CompletionItem>> {
        if !self.active {
            return None;
        }
        let text = text.replace(['\r', '\n'], " ");
        if self.query.len().saturating_add(text.len()) > PICKER_QUERY_BYTES_MAX {
            return None;
        }
        self.query.push_str(&text);
        Some(self.ranked_items())
    }

    pub fn backspace_query(&mut self) -> Option<Vec<CompletionItem>> {
        let (start, _) = self.query.grapheme_indices(true).next_back()?;
        self.query.truncate(start);
        Some(self.ranked_items())
    }

    pub fn clear_query(&mut self) -> Option<Vec<CompletionItem>> {
        if !self.active || self.query.is_empty() {
            return None;
        }
        self.query.clear();
        Some(self.ranked_items())
    }

    pub fn kill_query_word(&mut self) -> Option<Vec<CompletionItem>> {
        if !self.active || self.query.is_empty() {
            return None;
        }
        let trimmed = self.query.trim_end_matches(char::is_whitespace);
        let start = after_last_whitespace(trimmed);
        self.query.truncate(start);
        Some(self.ranked_items())
    }

    pub fn dismiss(&mut self) {
        self.source.clear();
        self.ranking.clear();
        self.query.clear();
        self.active = false;
        self.expanded = false;
        self.bottom_anchored = false;
    }

    /// Replace a pending palette's source while preserving its bounded query
    /// and presentation. Admission must not discard typing performed meanwhile.
    pub fn replace_items(&mut self, items: Vec<CompletionItem>) -> Vec<CompletionItem> {
        let query = self.query.clone();
        let bottom_anchored = self.bottom_anchored;
        let visible = self.open_with_query(items, self.label, self.expanded, &query);
        self.bottom_anchored = bottom_anchored;
        visible
    }

    fn ranked_items(&self) -> Vec<CompletionItem> {
        self.ranker
            .rank(&self.ranking, &self.query, OVERLAY_RESULTS_MAX)
            .into_iter()
            .filter_map(|matched| {
                let mut item = self.source.get(matched.index)?.clone();
                item.match_indices = matched.match_indices;
                Some(item)
            })
            .collect()
    }
}

pub fn contextual_help_query(catalog: &Catalog, line: &str, cursor: usize) -> String {
    if line.len() > MAX_EDITOR_BUFFER_BYTES {
        return String::new();
    }
    let cursor = clamped_utf8_cursor(line, cursor);
    let segment = shell_segment_range(line, cursor);
    let prefix = line.get(segment.start..cursor).unwrap_or_default().trim();
    if prefix.is_empty() {
        return String::new();
    }
    let context = catalog
        .commands
        .iter()
        .filter_map(|command| {
            std::iter::once(command.path.as_str())
                .chain(command.aliases.iter().map(String::as_str))
                .filter(|path| {
                    prefix == *path
                        || prefix
                            .strip_prefix(*path)
                            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
                })
                .max_by_key(|path| path.len())
        })
        .max_by_key(|path| path.len());
    truncate_utf8(context.unwrap_or(prefix), PICKER_QUERY_BYTES_MAX)
}

#[allow(
    clippy::string_slice,
    reason = "highlight spans come from the ranker's character-boundary-aware matcher"
)]
pub fn items(
    kind: PickerKind,
    catalog: &Catalog,
    history: &[InteractiveHistoryEntry],
    line: &str,
    cursor: usize,
) -> Vec<CompletionItem> {
    match kind {
        PickerKind::History => history_items(history, line.len()),
        PickerKind::Palette => palette_items(catalog, line.len()),
        PickerKind::Files | PickerKind::Directories => filesystem_items(kind, line, cursor),
        PickerKind::Projects | PickerKind::Jobs | PickerKind::Data => Vec::new(),
    }
}

/// Scan at most 4,096 entries for a file or directory picker without command
/// discovery. Oversized editor input is rejected before building replacements;
/// paths retain the same literal-name quoting and option-prefix protection.
pub(super) fn filesystem_items(kind: PickerKind, line: &str, cursor: usize) -> Vec<CompletionItem> {
    if line.len() > MAX_EDITOR_BUFFER_BYTES {
        return Vec::new();
    }
    let replacement = FilePickerReplacement::at_cursor(line, cursor);
    directory_items(kind, Path::new("."), &replacement)
}

/// Build at most 4,096 palette choices for the admitted catalog generation.
pub(super) fn palette_items(catalog: &Catalog, replace_end: usize) -> Vec<CompletionItem> {
    catalog
        .commands
        .iter()
        .take(OVERLAY_ITEMS_MAX)
        .map(|command| CompletionItem {
            value: command.path.clone(),
            display: command.path.clone(),
            summary: command.summary.clone(),
            detail: format!("{}\n\n{}", command.signature, command.details),
            replace_start: 0,
            replace_end,
            match_indices: Vec::new(),
            kind: CompletionKind::Command,
            source: "catalog",
            trust: "validated",
        })
        .collect()
}

/// Build at most 4,096 history choices without requiring command discovery.
/// The caller supplies the bounded history snapshot and editor replacement size.
pub(super) fn history_items(
    history: &[InteractiveHistoryEntry],
    replace_end: usize,
) -> Vec<CompletionItem> {
    history
        .iter()
        .rev()
        .take(OVERLAY_ITEMS_MAX)
        .map(|entry| CompletionItem {
            value: entry.command_line.clone(),
            display: entry.command_line.clone(),
            summary: entry.directory.as_ref().map_or_else(
                || "history".to_owned(),
                |directory| format!("history · {directory}"),
            ),
            detail: entry.status.map_or_else(
                || "Previously accepted command".to_owned(),
                |status| format!("Previously accepted command · status {status}"),
            ),
            replace_start: 0,
            replace_end,
            match_indices: Vec::new(),
            kind: CompletionKind::History,
            source: if entry.rank_bias > 0 {
                "history-local"
            } else {
                "history"
            },
            trust: "session",
        })
        .collect()
}

fn directory_items(
    kind: PickerKind,
    directory: &Path,
    replacement: &FilePickerReplacement,
) -> Vec<CompletionItem> {
    let mut entries = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flatten()
        .take(OVERLAY_ITEMS_MAX)
        .filter_map(Result::ok)
        .filter(|entry| kind != PickerKind::Directories || entry.path().is_dir())
        .take(OVERLAY_ITEMS_MAX)
        .filter_map(|entry| {
            let path = entry.path();
            // Display repair must never select a different filesystem object.
            let mut display = entry.file_name().to_str()?.to_owned();
            if path.is_dir() {
                display.push('/');
            }
            Some(CompletionItem {
                value: replacement.encode(&display),
                display,
                summary: if path.is_dir() { "directory" } else { "file" }.to_owned(),
                detail: path.display().to_string(),
                replace_start: replacement.range.start,
                replace_end: replacement.range.end,
                match_indices: Vec::new(),
                kind: CompletionKind::Path,
                source: "filesystem",
                trust: "local",
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.display.cmp(&right.display));
    entries
}

const fn picker_item_kind(kind: CompletionKind) -> PickerItemKind {
    match kind {
        CompletionKind::History => PickerItemKind::History,
        CompletionKind::Path => PickerItemKind::File,
        CompletionKind::Directory => PickerItemKind::Directory,
        CompletionKind::Job => PickerItemKind::Job,
        CompletionKind::Data => PickerItemKind::Data,
        CompletionKind::Command | CompletionKind::Flag | CompletionKind::Value => {
            PickerItemKind::Action
        }
    }
}

fn completion_item_bytes(item: &CompletionItem) -> usize {
    item.value
        .len()
        .saturating_add(item.display.len())
        .saturating_add(item.summary.len())
        .saturating_add(item.detail.len())
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the decrement is guarded by a nonzero offset and stops at a UTF-8 boundary"
)]
fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.get(..end).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StablePickerRanker;

    fn item(value: &str) -> CompletionItem {
        CompletionItem {
            value: value.to_owned(),
            display: value.to_owned(),
            summary: "command".to_owned(),
            detail: "details".to_owned(),
            replace_start: 0,
            replace_end: 0,
            match_indices: Vec::new(),
            kind: CompletionKind::Command,
            source: "test",
            trust: "validated",
        }
    }

    #[test]
    fn deleting_picker_words_preserves_multibyte_whitespace() {
        for separator in [" ", "\u{00a0}", "\u{2003}", "\u{3000}"] {
            let mut overlay = PickerOverlay::new(Arc::new(StablePickerRanker));
            overlay.open_with_query(
                Vec::new(),
                "history",
                false,
                &format!("one{separator}two{separator}"),
            );
            assert!(overlay.kill_query_word().is_some());
            assert_eq!(overlay.query(), Some(format!("one{separator}").as_str()));
            assert!(overlay.kill_query_word().is_some());
            assert_eq!(overlay.query(), Some(""));
        }
    }

    #[test]
    fn file_picker_replacement_starts_after_the_complete_unicode_separator() {
        let line = "cat\u{3000}fi";
        let results = items(
            PickerKind::Files,
            &Catalog::builtin(),
            &[],
            line,
            line.len(),
        );
        assert!(
            !results.is_empty(),
            "the crate fixture directory contains Cargo.toml"
        );
        for result in results {
            assert_eq!(result.replace_start, "cat\u{3000}".len());
            assert!(line.is_char_boundary(result.replace_start));
            assert_eq!(result.replace_end, line.len());
        }
    }

    #[test]
    fn overlay_queries_use_shared_picker_ranking_and_keep_typed_values() {
        let mut overlay = PickerOverlay::new(Arc::new(StablePickerRanker));
        let initial = overlay.open(
            vec![item("git status"), item("cargo test"), item("git stash")],
            "actions",
            false,
        );
        assert_eq!(initial.len(), 3);

        let filtered = overlay.insert_query("stash").unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].value, "git stash");
        assert!(!filtered[0].match_indices.is_empty());
        assert_eq!(overlay.query(), Some("stash"));
    }

    #[test]
    fn overlay_query_and_results_are_bounded() {
        let mut overlay = PickerOverlay::new(Arc::new(StablePickerRanker));
        let items = (0..(OVERLAY_ITEMS_MAX + 10))
            .map(|index| item(&format!("item-{index}")))
            .collect();
        let visible = overlay.open(items, "bounded", false);
        assert_eq!(visible.len(), OVERLAY_RESULTS_MAX);
        assert!(
            overlay
                .insert_query(&"x".repeat(PICKER_QUERY_BYTES_MAX + 1))
                .is_none()
        );
        assert_eq!(overlay.query(), Some(""));
    }

    #[test]
    fn query_backspace_is_grapheme_aware() {
        let mut overlay = PickerOverlay::new(Arc::new(StablePickerRanker));
        overlay.open(vec![item("echo")], "history", false);
        overlay.insert_query("e\u{301}").unwrap();
        overlay.backspace_query().unwrap();
        assert_eq!(overlay.query(), Some(""));
    }

    #[test]
    fn bottom_anchored_overlay_state_is_bounded_to_its_active_lifetime() {
        let mut overlay = PickerOverlay::new(Arc::new(StablePickerRanker));
        overlay.open_bottom_anchored(vec![item("git status")], "actions");

        assert!(overlay.active());
        assert!(overlay.bottom_anchored());
        assert!(!overlay.expanded());

        overlay.dismiss();
        assert!(!overlay.active());
        assert!(!overlay.bottom_anchored());
    }

    #[test]
    fn contextual_help_uses_the_typed_prefix_and_longest_command_context() {
        let catalog = Catalog::builtin();
        assert_eq!(contextual_help_query(&catalog, "git st", 6), "git st");
        assert_eq!(
            contextual_help_query(&catalog, "git status --short", 18),
            "git status"
        );
    }

    #[test]
    fn contextual_help_follows_the_cursor_across_command_boundaries() {
        let catalog = Catalog::builtin();
        for separator in [" | ", " && ", " || ", "; ", "\n"] {
            let line = format!("git status{separator}quirl doctor");
            assert_eq!(
                contextual_help_query(&catalog, &line, line.len()),
                "quirl doctor"
            );
            assert_eq!(
                contextual_help_query(&catalog, &line, "git status".len()),
                "git status"
            );
            let empty_segment = format!("git status{separator}");
            assert_eq!(
                contextual_help_query(&catalog, &empty_segment, empty_segment.len()),
                ""
            );
        }
        for argument in ["'x | y'", "\"x && y\"", "x\\;y", "'x\ny'"] {
            let line = format!("git status {argument}");
            assert_eq!(
                contextual_help_query(&catalog, &line, line.len()),
                "git status"
            );
        }
    }

    #[test]
    fn file_picker_preserves_selected_paths_and_surrounding_command_text() {
        use super::super::editor::EditorState;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "quirl-file-picker-{}-{}",
            std::process::id(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir(&fixture.0).unwrap();
        let names = [
            "quarterly report.txt",
            "it's here.txt",
            "$notes",
            "star*file",
            "semi;colon",
            "#notes",
            "~notes",
            "line\nbreak",
            "carriage\rreturn",
            "日本語.txt",
            "-n",
            "-two words",
            "-$notes",
        ];
        for name in names {
            fs::write(fixture.0.join(name), b"selected contents").unwrap();
        }
        // Linux permits byte filenames that macOS filesystems reject at creation.
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStringExt;
            fs::write(
                fixture
                    .0
                    .join(std::ffi::OsString::from_vec(vec![b'x', 0xff])),
                b"unrepresentable",
            )
            .unwrap();
        }
        // A file choice replaces the whole current word, even when the cursor
        // is inside a quoted word. Arguments and operators outside it survive.
        for marked in [
            "cat ▌ keep | cat",
            "cat ol▌d.txt keep | cat",
            "cat 'old na▌me' keep | cat",
            "cat \"old na▌me\" keep | cat",
            "cat 'unfinished ▌",
            "cat \"unfinished ▌",
        ] {
            let cursor = marked.find('▌').unwrap();
            let line = marked.replace('▌', "");
            let replacement = FilePickerReplacement::at_cursor(&line, cursor);
            let results = directory_items(PickerKind::Files, &fixture.0, &replacement);
            assert_eq!(
                results.len(),
                names.len(),
                "non-UTF-8 names must not acquire a lossy identity"
            );
            for item in results {
                let mut editor = EditorState::new("emacs", Vec::new());
                editor.restore(line.clone(), cursor);
                editor.replace(item.replace_start, item.replace_end, &item.value);
                let parsed = quirl_syntax::parse_command_list(editor.buffer()).unwrap();
                assert_eq!(parsed.pipelines.len(), 1);
                let command = &parsed.pipelines[0].commands[0];
                assert_eq!(command.words[0], "cat");
                let expected_argument = if item.display.starts_with('-') {
                    format!("./{}", item.display)
                } else {
                    item.display.clone()
                };
                assert_eq!(
                    command.words[1], expected_argument,
                    "{marked}: {}",
                    item.value
                );
                assert!(!command.words[1].starts_with('-'));
                assert_eq!(
                    fs::canonicalize(fixture.0.join(&command.words[1])).unwrap(),
                    fs::canonicalize(fixture.0.join(&item.display)).unwrap(),
                    "the inserted argument must still name the selected file"
                );
                let expected_words = if marked.ends_with("keep | cat") { 3 } else { 2 };
                assert_eq!(command.words.len(), expected_words);
                if expected_words == 3 {
                    assert_eq!(command.words[2], "keep");
                    assert_eq!(parsed.pipelines[0].commands[1].words, ["cat"]);
                }
                for part in &command.word_ir[1].parts {
                    if part.text.contains(['$', '`']) {
                        assert!(matches!(
                            part.quoting,
                            quirl_syntax::Quoting::Single | quirl_syntax::Quoting::Escaped
                        ));
                    }
                    if part.text.contains(['*', '~']) {
                        assert_ne!(part.quoting, quirl_syntax::Quoting::Unquoted);
                    }
                }
            }
        }
    }
}
