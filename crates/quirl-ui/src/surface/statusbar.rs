use super::{completion::CompletionState, editor::EditorState};
use crate::{SurfaceSymbols, theme::Theme};
use quirl_syntax::Mode;
use ratatui::text::{Line, Span};
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
}

impl StatusBarModel<'_> {
    pub fn line(&self, theme: Theme) -> Line<'static> {
        let unicode = self.symbols.uses_unicode();
        let separator = match self.symbols {
            SurfaceSymbols::Plain => " | ",
            SurfaceSymbols::Unicode => " │ ",
            SurfaceSymbols::NerdFont => " \u{e0b1} ",
        };
        let mut left = Vec::new();
        if let Some(label) = self.editor.mode().label() {
            left.push(label.to_owned());
        }
        let mode_icon = self.symbols.status_mode_icon(self.mode);
        left.push(format!(
            " {mode_icon} {} ",
            self.mode.to_string().to_uppercase()
        ));

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

        let right = if self.completion.open && self.completion.automatic {
            if unicode {
                "↑↓ select · Tab choose · Enter run · Esc close".to_owned()
            } else {
                "up/down select | Tab choose | Enter run | Esc close".to_owned()
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
        let left_text = left.join(separator);
        let fixed = UnicodeWidthStr::width(left_text.as_str())
            .saturating_add(UnicodeWidthStr::width(right.as_str()))
            .saturating_add(UnicodeWidthStr::width(separator).saturating_mul(2));
        let available = usize::from(self.width).saturating_sub(fixed);
        let center = if self.width < 60 {
            String::new()
        } else {
            center.chars().take(available).collect()
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
