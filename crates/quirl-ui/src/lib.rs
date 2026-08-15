//! Terminal interaction that treats completion and diagnostics as core behavior.

use nu_ansi_term::{Color, Style};
use quirl_catalog::Catalog;
use quirl_core::ShellError;
use quirl_syntax::Mode;
use reedline::{
    default_emacs_keybindings, Completer, DefaultHinter, DefaultValidator, Emacs, Highlighter,
    IdeMenu, KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch,
    Reedline, ReedlineEvent, ReedlineMenu, Span, StyledText, Suggestion,
};
use std::{borrow::Cow, env};

pub fn editor(catalog: Catalog) -> Reedline {
    let completer = Box::new(CatalogCompleter::new(catalog.clone()));
    let highlighter = Box::new(SemanticHighlighter::new(catalog));
    let completion_menu = Box::new(
        IdeMenu::default()
            .with_name("completion_menu")
            .with_default_border(),
    );
    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_owned()),
            ReedlineEvent::MenuNext,
        ]),
    );

    Reedline::create()
        .with_completer(completer)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_highlighter(highlighter)
        .with_hinter(Box::new(
            DefaultHinter::default().with_style(Style::new().italic().fg(Color::DarkGray)),
        ))
        .with_validator(Box::new(DefaultValidator))
        .with_edit_mode(Box::new(Emacs::new(keybindings)))
        .with_quick_completions(false)
}

#[derive(Clone)]
pub struct QuirlPrompt {
    mode: Mode,
    cwd: String,
}

impl QuirlPrompt {
    pub fn new(mode: Mode) -> Self {
        let cwd = env::current_dir()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "/".to_owned());
        Self { mode, cwd }
    }
}

impl Prompt for QuirlPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Owned(format!("{} · {} ", self.cwd, self.mode))
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
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
}

impl CatalogCompleter {
    pub fn new(catalog: Catalog) -> Self {
        Self { catalog }
    }
}

impl Completer for CatalogCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        self.catalog
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
            .collect()
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

    #[test]
    fn completion_contains_explanatory_metadata() {
        let mut completer = CatalogCompleter::new(Catalog::builtin());
        let result = completer.complete("git c", 5);
        assert_eq!(result[0].value, "git commit");
        assert!(result[0].description.as_deref().unwrap().contains("Record"));
    }

    #[test]
    fn diagnostics_have_stable_codes_and_help() {
        let error = ShellError::new(ErrorCode::Steel, "program failed").with_help("fix it");
        let rendered = render_error(&error, false);
        assert!(rendered.starts_with("error[steel]"));
        assert!(rendered.contains("help: fix it"));
    }
}
