use crate::{hinter::get_first_token, history::SearchQuery, Hinter, History};
use nu_ansi_term::{Color, Style};

/// A hinter that uses the completions or the history to show a hint to the user
pub struct DefaultHinter {
    style: Style,
    current_hint: String,
    min_chars: usize,
}

impl Hinter for DefaultHinter {
    fn handle(
        &mut self,
        line: &str,
        #[allow(unused_variables)] pos: usize,
        history: &dyn History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        self.current_hint = if line.chars().count() >= self.min_chars {
            history
                .search(SearchQuery::last_with_prefix(
                    line.to_string(),
                    history.session(),
                ))
                .expect("todo: error handling")
                .first()
                .map_or_else(String::new, |entry| {
                    entry
                        .command_line
                        .get(line.len()..)
                        .unwrap_or_default()
                        .to_string()
                })
        } else {
            String::new()
        };

        // Keep the completion bytes verbatim, but never render history controls
        // as terminal commands. Escape before adding the hinter's own style.
        let display_hint = crate::painting::escape_display_controls(&self.current_hint);
        if use_ansi_coloring && !display_hint.is_empty() {
            self.style.paint(display_hint).to_string()
        } else {
            display_hint.into_owned()
        }
    }

    fn complete_hint(&self) -> String {
        self.current_hint.clone()
    }

    fn next_hint_token(&self) -> String {
        get_first_token(&self.current_hint)
    }
}

impl Default for DefaultHinter {
    fn default() -> Self {
        DefaultHinter {
            style: Style::new().fg(Color::LightGray),
            current_hint: String::new(),
            min_chars: 1,
        }
    }
}

impl DefaultHinter {
    /// A builder that sets the style applied to the hint as part of the buffer
    #[must_use]
    pub fn with_style(mut self, style: Style) -> Self {
        self.style = style;
        self
    }

    /// A builder that sets the number of characters that have to be present to enable history hints
    #[must_use]
    pub fn with_min_chars(mut self, min_chars: usize) -> Self {
        self.min_chars = min_chars;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileBackedHistory, HistoryItem};

    #[test]
    fn hostile_history_hint_is_escaped_for_display_but_completion_keeps_source_bytes() {
        let suffix = "\x1b]52;c;payload\x07";
        let mut history = FileBackedHistory::new(2).unwrap();
        history
            .save(HistoryItem::from_command_line(format!("echo {suffix}")))
            .unwrap();
        for color in [false, true] {
            let mut hinter = DefaultHinter::default();
            let display = hinter.handle("echo ", 5, &history, color, "");
            assert!(!display.contains("\x1b]52;"));
            assert!(display.contains("\\u{1b}]52;c;payload\\u{7}"));
            assert_eq!(hinter.complete_hint(), suffix);
        }
    }
}
