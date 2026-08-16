use super::highlight::DiagnosticSeverity;
use quirl_syntax::{HighlightKind, Mode};
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    color: bool,
}

impl Theme {
    pub const fn new(color: bool) -> Self {
        Self { color }
    }

    pub fn accent(self, mode: Mode) -> Style {
        self.colored(match mode {
            Mode::Command => Color::Green,
            Mode::Data => Color::Magenta,
        })
        .add_modifier(Modifier::BOLD)
    }

    pub fn context(self) -> Style {
        self.colored(Color::Cyan).add_modifier(Modifier::BOLD)
    }

    pub fn context_secondary(self) -> Style {
        self.colored(Color::Magenta).add_modifier(Modifier::BOLD)
    }

    pub fn dim(self) -> Style {
        self.colored(Color::DarkGray).add_modifier(Modifier::DIM)
    }

    pub fn border(self) -> Style {
        self.colored(Color::DarkGray)
    }

    pub fn selected(self, mode: Mode) -> Style {
        self.accent(mode).add_modifier(Modifier::REVERSED)
    }

    pub fn status(self) -> Style {
        if self.color {
            Style::default().bg(Color::Rgb(24, 24, 32))
        } else {
            Style::default()
        }
    }

    pub fn diagnostic(self, severity: DiagnosticSeverity) -> Style {
        self.colored(match severity {
            DiagnosticSeverity::Error => Color::Red,
            DiagnosticSeverity::Warning => Color::Yellow,
            DiagnosticSeverity::Hint => Color::Blue,
        })
        .add_modifier(Modifier::BOLD)
    }

    pub fn highlight(self, kind: HighlightKind) -> Style {
        match kind {
            HighlightKind::Command => self.colored(Color::Green).add_modifier(Modifier::BOLD),
            HighlightKind::Flag => self.colored(Color::Cyan),
            HighlightKind::StringSingle | HighlightKind::StringDouble => {
                self.colored(Color::Yellow)
            }
            HighlightKind::Operator | HighlightKind::Redirect => {
                self.colored(Color::White).add_modifier(Modifier::BOLD)
            }
            HighlightKind::Expansion => self.colored(Color::Blue),
            HighlightKind::Error => self.colored(Color::Red).add_modifier(Modifier::UNDERLINED),
            HighlightKind::Number => self.colored(Color::Magenta),
            HighlightKind::Argument | HighlightKind::PathLike | HighlightKind::Escaped => {
                Style::default()
            }
        }
    }

    fn colored(self, color: Color) -> Style {
        if self.color {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }
}
