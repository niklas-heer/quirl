//! Terminal interaction that treats completion and diagnostics as core behavior.

use crossterm::event::{Event, KeyEvent};
use nu_ansi_term::{Color, Style};
use quirl_catalog::Catalog;
use quirl_core::ShellError;
use quirl_lua::QuirlConfig;
use quirl_syntax::Mode;
use reedline::{
    default_emacs_keybindings, default_vi_insert_keybindings, default_vi_normal_keybindings,
    Completer, DefaultHinter, DefaultValidator, DescriptionMode, EditMode, Emacs, Helix,
    Highlighter, IdeMenu, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, ReedlineRawEvent, Span, StyledText,
    Suggestion, Vi,
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    time::Duration,
};

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
    let completer = Box::new(CatalogCompleter::with_extensions(
        catalog.clone(),
        extension_completer,
    ));
    let completion_menu = Box::new(configured_completion_menu(&config));
    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(Style::new().italic().fg(Color::DarkGray)),
        ))
        .with_validator(Box::new(DefaultValidator))
        .with_edit_mode(configured_edit_mode(&config.editor.keymap))
        .with_quick_completions(false);
    if config.editor.semantic_hints {
        line_editor = line_editor.with_highlighter(Box::new(SemanticHighlighter::new(catalog)));
    }
    line_editor
}

fn completion_menu_event() -> ReedlineEvent {
    ReedlineEvent::UntilFound(vec![
        ReedlineEvent::Menu("completion_menu".to_owned()),
        ReedlineEvent::MenuNext,
    ])
}

fn configured_edit_mode(keymap: &str) -> Box<dyn EditMode> {
    match keymap {
        "vim" => {
            let mut insert = default_vi_insert_keybindings();
            let mut normal = default_vi_normal_keybindings();
            insert.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            normal.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            Box::new(Vi::new(insert, normal))
        }
        "helix" => Box::new(QuirlHelix::default()),
        // Config validation rejects other values. Keep this fallback for direct Rust callers.
        "emacs" => {
            let mut keybindings = default_emacs_keybindings();
            keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            Box::new(Emacs::new(keybindings))
        }
        _ => {
            let mut keybindings = default_emacs_keybindings();
            keybindings.add_binding(KeyModifiers::NONE, KeyCode::Tab, completion_menu_event());
            Box::new(Emacs::new(keybindings))
        }
    }
}

/// Reedline's native Helix mode intentionally owns its modal editing behavior,
/// but does not bind Tab to a completion menu. Keep that product-level binding
/// at Quirl's editor boundary and delegate every other event unchanged.
#[derive(Default)]
struct QuirlHelix {
    inner: Helix,
}

impl EditMode for QuirlHelix {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let event: Event = event.into();
        if matches!(
            event,
            Event::Key(KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            })
        ) {
            return completion_menu_event();
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

fn configured_completion_menu(config: &QuirlConfig) -> IdeMenu {
    let menu = IdeMenu::default()
        .with_name("completion_menu")
        .with_default_border();
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

#[derive(Clone)]
pub struct QuirlPrompt {
    mode: Mode,
    cwd: String,
    git_branch: Option<String>,
    status: Option<i32>,
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
            .as_ref()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "/".to_owned());
        Self {
            mode,
            cwd,
            git_branch: cwd_path.as_deref().and_then(read_git_branch),
            status: None,
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

    /// Set the exit status that can be rendered by the configured `status` segment.
    pub fn with_status(mut self, status: i32) -> Self {
        self.status = Some(status);
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
        requested
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
                // Job control and a cheap dirty-worktree signal land in Phase 1.
                "jobs" | "git_state" => None,
                _ => self.named_extension_segments.get(name).cloned(),
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
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

fn read_git_branch(cwd: &Path) -> Option<String> {
    let git_dir = cwd.ancestors().find_map(resolve_git_dir)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    Some(
        head.strip_prefix("ref: refs/heads/")
            .map(str::to_owned)
            .unwrap_or_else(|| head.chars().take(8).collect()),
    )
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
        Cow::Owned(format!("{} ", parts.join(" · ")))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(self.render_segments(&self.configured_right))
    }

    fn render_prompt_indicator(&self, _prompt_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Owned(format!("{} ", self.mode.prompt()))
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("  · ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        Cow::Owned(format!(
            "search `{}` {} ",
            history_search.term,
            self.mode.prompt()
        ))
    }
}

pub struct CatalogCompleter {
    catalog: Catalog,
    extensions: Option<Box<dyn ExtensionCompleter + Send>>,
}

impl CatalogCompleter {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            extensions: None,
        }
    }

    pub fn with_extensions(
        catalog: Catalog,
        extensions: Option<Box<dyn ExtensionCompleter + Send>>,
    ) -> Self {
        Self {
            catalog,
            extensions,
        }
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

impl Completer for CatalogCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
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
    let mut rendered = format!("{heading}: {}\n", error.message);
    for label in &error.details.labels {
        let source = label.source.as_deref().unwrap_or("input");
        rendered.push_str(&format!("  ╭─[{source}:{}..{}]\n", label.start, label.end));
        rendered.push_str(&format!("  ╰─ {}\n", label.message));
    }
    for context in &error.details.context {
        rendered.push_str(&format!("  caused by: {context}\n"));
    }
    for help in &error.details.help {
        let marker = if color {
            Color::Cyan.bold().paint("help").to_string()
        } else {
            "help".to_owned()
        };
        rendered.push_str(&format!("  {marker}: {help}\n"));
    }
    rendered.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_core::ErrorCode;
    use quirl_lua::{EditorConfig, PickerConfig, PromptConfig};

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
    fn lua_suggestions_merge_with_catalog_completion() {
        let mut completer =
            CatalogCompleter::with_extensions(Catalog::builtin(), Some(Box::new(ExampleExtension)));
        let result = completer.complete("deploy --environment prod", 25);
        assert!(result.iter().any(|suggestion| {
            suggestion.value == "production"
                && suggestion.description.as_deref() == Some("Deployment environment")
        }));
    }

    #[test]
    fn prompt_renders_extension_segments() {
        let prompt = QuirlPrompt::new(Mode::Command)
            .with_extension_segments(vec!["project".to_owned(), "git:main".to_owned()]);
        let rendered = prompt.render_prompt_left();
        assert!(rendered.contains("project · git:main"));
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
        assert!(left.starts_with("command · quirl · "));
        assert_eq!(prompt.render_prompt_right(), "status:7 · eu-central");
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
        let mut helix = QuirlHelix::default();

        assert_eq!(helix.parse_event(event), completion_menu_event());
    }
}
