use super::{completion::CompletionState, editor::EditorState};
use crate::{SurfaceSymbols, theme::Theme};
use quirl_syntax::Mode;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub struct StatusBarModel<'a> {
    pub editor: &'a EditorState,
    pub completion: &'a CompletionState,
    pub mode: Mode,
    pub width: u16,
    pub hints: bool,
    pub notice: Option<&'a str>,
    pub timings: Option<&'a str>,
    pub symbols: SurfaceSymbols,
    pub assistant_busy: bool,
    pub assistant_has_proposal: bool,
}

impl StatusBarModel<'_> {
    pub fn line(&self, theme: Theme) -> Line<'static> {
        let unicode = self.symbols.uses_unicode();
        let separator = match self.symbols {
            SurfaceSymbols::Plain => " | ",
            SurfaceSymbols::Unicode | SurfaceSymbols::NerdFont => " │ ",
        };
        let mut left = Vec::new();
        if let Some(label) = self.editor.mode().label() {
            left.push(label.to_owned());
        }
        let mode_icon = self.symbols.status_mode_icon(self.mode);
        let mode_label = self.mode.to_string().to_uppercase();
        left.push(if mode_icon.is_empty() {
            format!(" {mode_label} ")
        } else {
            format!(" {mode_icon} {mode_label} ")
        });

        let center = if let Some(notice) = self.notice {
            notice.to_owned()
        } else if let Some(notice) = self.editor.resource_notice() {
            notice.to_owned()
        } else if let Some(notice) = self.completion.resource_notice() {
            notice.to_owned()
        } else if let Some(lines) = self.editor.pasted_lines() {
            match self.symbols {
                SurfaceSymbols::NerdFont => format!("\u{f0ea} pasted {lines} lines"),
                SurfaceSymbols::Unicode => format!("⇪ pasted {lines} lines"),
                SurfaceSymbols::Plain => format!("pasted {lines} lines"),
            }
        } else if self.completion.open || self.completion.streaming {
            let streaming = if self.completion.streaming {
                format!("{separator}streaming...")
            } else {
                String::new()
            };
            format!(
                "{} results ({}){streaming}",
                self.completion.items.len(),
                self.completion.source_label
            )
        } else if self.hints {
            if self.width >= 96 {
                format!(
                    "Alt-Q Quirl{separator}Tab complete{separator}↑ / Ctrl-R history{separator}F1 help"
                )
            } else {
                format!("Alt-Q Quirl{separator}Tab complete{separator}Ctrl-R history")
            }
        } else {
            String::new()
        };

        let right = if self.mode == Mode::Natural && self.assistant_busy {
            "Esc cancel".to_owned()
        } else if self.mode == Mode::Natural && self.assistant_has_proposal {
            "Enter/Tab use · type to refine · Esc close".to_owned()
        } else if self.mode == Mode::Natural {
            "Enter send · Esc close".to_owned()
        } else if self.completion.open && self.completion.automatic {
            if unicode {
                "↑ history · ↓/Tab choose · Enter run · Esc close".to_owned()
            } else {
                "up history | down/Tab choose | Enter run | Esc close".to_owned()
            }
        } else if self.completion.open {
            if unicode {
                "↑↓ move · Enter accept · Esc close".to_owned()
            } else {
                "up/down move | Enter accept | Esc close".to_owned()
            }
        } else if let Some(timings) = self.timings {
            format!("{timings} · {}", super::product_identity())
        } else {
            match self.symbols {
                SurfaceSymbols::NerdFont => {
                    format!("\u{f120} {}", super::product_identity())
                }
                SurfaceSymbols::Unicode => format!("🌀 {}", super::product_identity()),
                SurfaceSymbols::Plain => format!("quirl {}", super::product_identity()),
            }
        };
        let left_text = fit_columns(&left.join(separator), usize::from(self.width));
        let right_columns = usize::from(self.width)
            .saturating_sub(UnicodeWidthStr::width(left_text.as_str()))
            .saturating_sub(1);
        // Keep the dismissal key discoverable when a narrow window cannot fit
        // the full interaction legend. Fit every region by terminal columns,
        // preserving whole graphemes so wide notices cannot push hints offscreen.
        let right = if UnicodeWidthStr::width(right.as_str()) > right_columns {
            let compact = if self.mode == Mode::Natural && self.assistant_busy {
                "Esc cancel"
            } else if self.completion.open || self.mode == Mode::Natural {
                "Esc close"
            } else {
                &right
            };
            fit_columns(compact, right_columns)
        } else {
            right
        };
        let fixed = UnicodeWidthStr::width(left_text.as_str())
            .saturating_add(UnicodeWidthStr::width(right.as_str()))
            .saturating_add(UnicodeWidthStr::width(separator).saturating_mul(2));
        let available = usize::from(self.width).saturating_sub(fixed);
        let center = if self.width < 60 {
            String::new()
        } else {
            fit_columns(&center, available)
        };
        let mut spans = vec![Span::styled(left_text, theme.accent(self.mode))];
        if !center.is_empty() {
            spans.push(Span::styled(separator.to_owned(), theme.dim()));
            spans.push(Span::styled(center, theme.dim()));
        }
        if self.width >= 32 {
            let occupied = spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            let padding = usize::from(self.width)
                .saturating_sub(occupied)
                .saturating_sub(UnicodeWidthStr::width(right.as_str()));
            spans.push(Span::styled(" ".repeat(padding), theme.status()));
            spans.push(Span::styled(right, theme.dim()));
        }
        Line::from(spans).style(theme.status())
    }
}

fn fit_columns(text: &str, columns: usize) -> String {
    let mut occupied = 0_usize;
    text.graphemes(true)
        .take_while(|grapheme| {
            occupied = occupied.saturating_add(UnicodeWidthStr::width(*grapheme));
            occupied <= columns
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_catalog::Catalog;

    #[test]
    fn column_clipping_preserves_combining_and_joined_graphemes() {
        assert_eq!(fit_columns("e\u{301}界👨‍👩‍👧‍👦z", 5), "e\u{301}界👨‍👩‍👧‍👦");
        assert_eq!(fit_columns("界z", 1), "");
        assert_eq!(fit_columns("abc", 0), "");
    }

    #[test]
    fn status_content_fits_narrow_windows_and_wide_character_notices() {
        let editor = EditorState::new("emacs", Vec::new());
        let mut completion = CompletionState::new(Catalog::builtin(), None);
        for open in [false, true] {
            completion.open = open;
            for width in [16, 32, 40, 60, 80, 120] {
                let line = StatusBarModel {
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Command,
                    width,
                    hints: true,
                    notice: Some("界面提示界面提示界面提示界面提示界面提示界面提示界面提示"),
                    timings: None,
                    symbols: SurfaceSymbols::Unicode,
                    assistant_busy: false,
                    assistant_has_proposal: false,
                }
                .line(Theme::new(false));
                assert!(
                    line.width() <= usize::from(width),
                    "width={width}; open={open}; line={line}"
                );
                if open && width >= 32 {
                    assert!(
                        line.to_string().contains("Esc close"),
                        "width={width}; line={line}"
                    );
                }
            }
        }
    }
}
