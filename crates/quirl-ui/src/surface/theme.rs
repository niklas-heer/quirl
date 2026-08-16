use super::highlight::DiagnosticSeverity;
use quirl_syntax::{HighlightKind, Mode};
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
}

impl RgbColor {
    pub const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    pub fn css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
    }

    const fn terminal(self) -> Color {
        Color::Rgb(self.red, self.green, self.blue)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeDefinition {
    pub id: &'static str,
    pub name: &'static str,
    pub background: RgbColor,
    pub foreground: RgbColor,
    pub command: RgbColor,
    pub data: RgbColor,
    pub context: RgbColor,
    pub secondary: RgbColor,
    pub dim: RgbColor,
    pub error: RgbColor,
    pub warning: RgbColor,
    pub hint: RgbColor,
    pub string: RgbColor,
    pub status_background: RgbColor,
}

const fn rgb(red: u8, green: u8, blue: u8) -> RgbColor {
    RgbColor::new(red, green, blue)
}

pub const THEME_DEFINITIONS: [ThemeDefinition; 8] = [
    ThemeDefinition {
        id: "quirl",
        name: "Quirl",
        background: rgb(18, 18, 24),
        foreground: rgb(232, 232, 240),
        command: rgb(95, 215, 135),
        data: rgb(215, 135, 255),
        context: rgb(95, 215, 255),
        secondary: rgb(215, 135, 255),
        dim: rgb(108, 112, 134),
        error: rgb(255, 95, 95),
        warning: rgb(255, 215, 95),
        hint: rgb(95, 175, 255),
        string: rgb(249, 226, 175),
        status_background: rgb(24, 24, 32),
    },
    ThemeDefinition {
        id: "catppuccin_mocha",
        name: "Catppuccin Mocha",
        background: rgb(30, 30, 46),
        foreground: rgb(205, 214, 244),
        command: rgb(166, 227, 161),
        data: rgb(203, 166, 247),
        context: rgb(116, 199, 236),
        secondary: rgb(245, 194, 231),
        dim: rgb(127, 132, 156),
        error: rgb(243, 139, 168),
        warning: rgb(249, 226, 175),
        hint: rgb(137, 180, 250),
        string: rgb(166, 227, 161),
        status_background: rgb(24, 24, 37),
    },
    ThemeDefinition {
        id: "dracula",
        name: "Dracula",
        background: rgb(40, 42, 54),
        foreground: rgb(248, 248, 242),
        command: rgb(80, 250, 123),
        data: rgb(189, 147, 249),
        context: rgb(139, 233, 253),
        secondary: rgb(255, 121, 198),
        dim: rgb(98, 114, 164),
        error: rgb(255, 85, 85),
        warning: rgb(241, 250, 140),
        hint: rgb(139, 233, 253),
        string: rgb(241, 250, 140),
        status_background: rgb(33, 34, 44),
    },
    ThemeDefinition {
        id: "gruvbox_dark",
        name: "Gruvbox Dark",
        background: rgb(40, 40, 40),
        foreground: rgb(235, 219, 178),
        command: rgb(184, 187, 38),
        data: rgb(211, 134, 155),
        context: rgb(131, 165, 152),
        secondary: rgb(254, 128, 25),
        dim: rgb(146, 131, 116),
        error: rgb(251, 73, 52),
        warning: rgb(250, 189, 47),
        hint: rgb(131, 165, 152),
        string: rgb(184, 187, 38),
        status_background: rgb(50, 48, 47),
    },
    ThemeDefinition {
        id: "nord",
        name: "Nord",
        background: rgb(46, 52, 64),
        foreground: rgb(216, 222, 233),
        command: rgb(163, 190, 140),
        data: rgb(180, 142, 173),
        context: rgb(136, 192, 208),
        secondary: rgb(129, 161, 193),
        dim: rgb(76, 86, 106),
        error: rgb(191, 97, 106),
        warning: rgb(235, 203, 139),
        hint: rgb(94, 129, 172),
        string: rgb(163, 190, 140),
        status_background: rgb(59, 66, 82),
    },
    ThemeDefinition {
        id: "solarized_dark",
        name: "Solarized Dark",
        background: rgb(0, 43, 54),
        foreground: rgb(131, 148, 150),
        command: rgb(133, 153, 0),
        data: rgb(211, 54, 130),
        context: rgb(42, 161, 152),
        secondary: rgb(108, 113, 196),
        dim: rgb(88, 110, 117),
        error: rgb(220, 50, 47),
        warning: rgb(181, 137, 0),
        hint: rgb(38, 139, 210),
        string: rgb(42, 161, 152),
        status_background: rgb(7, 54, 66),
    },
    ThemeDefinition {
        id: "tokyo_night",
        name: "Tokyo Night",
        background: rgb(26, 27, 38),
        foreground: rgb(192, 202, 245),
        command: rgb(158, 206, 106),
        data: rgb(187, 154, 247),
        context: rgb(125, 207, 255),
        secondary: rgb(255, 117, 127),
        dim: rgb(86, 95, 137),
        error: rgb(247, 118, 142),
        warning: rgb(224, 175, 104),
        hint: rgb(122, 162, 247),
        string: rgb(158, 206, 106),
        status_background: rgb(36, 40, 59),
    },
    ThemeDefinition {
        id: "one_dark",
        name: "One Dark",
        background: rgb(40, 44, 52),
        foreground: rgb(171, 178, 191),
        command: rgb(152, 195, 121),
        data: rgb(198, 120, 221),
        context: rgb(86, 182, 194),
        secondary: rgb(97, 175, 239),
        dim: rgb(92, 99, 112),
        error: rgb(224, 108, 117),
        warning: rgb(229, 192, 123),
        hint: rgb(97, 175, 239),
        string: rgb(152, 195, 121),
        status_background: rgb(33, 37, 43),
    },
];

pub fn theme_definition(id: &str) -> ThemeDefinition {
    THEME_DEFINITIONS
        .iter()
        .copied()
        .find(|theme| theme.id == id)
        .unwrap_or(THEME_DEFINITIONS[0])
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    color: bool,
    definition: ThemeDefinition,
}

impl Theme {
    pub fn new(color: bool, id: &str) -> Self {
        Self {
            color,
            definition: theme_definition(id),
        }
    }

    pub fn accent(self, mode: Mode) -> Style {
        self.colored(match mode {
            Mode::Command => self.definition.command,
            Mode::Data => self.definition.data,
        })
        .add_modifier(Modifier::BOLD)
    }

    pub fn context(self) -> Style {
        self.colored(self.definition.context)
            .add_modifier(Modifier::BOLD)
    }
    pub fn context_secondary(self) -> Style {
        self.colored(self.definition.secondary)
            .add_modifier(Modifier::BOLD)
    }
    pub fn dim(self) -> Style {
        self.colored(self.definition.dim)
            .add_modifier(Modifier::DIM)
    }
    pub fn border(self) -> Style {
        self.colored(self.definition.dim)
    }
    pub fn selected(self, mode: Mode) -> Style {
        self.accent(mode).add_modifier(Modifier::REVERSED)
    }

    pub fn status(self) -> Style {
        if self.color {
            Style::default().bg(self.definition.status_background.terminal())
        } else {
            Style::default()
        }
    }

    pub fn diagnostic(self, severity: DiagnosticSeverity) -> Style {
        self.colored(match severity {
            DiagnosticSeverity::Error => self.definition.error,
            DiagnosticSeverity::Warning => self.definition.warning,
            DiagnosticSeverity::Hint => self.definition.hint,
        })
        .add_modifier(Modifier::BOLD)
    }

    pub fn highlight(self, kind: HighlightKind) -> Style {
        match kind {
            HighlightKind::Command => self
                .colored(self.definition.command)
                .add_modifier(Modifier::BOLD),
            HighlightKind::Flag => self.colored(self.definition.context),
            HighlightKind::StringSingle | HighlightKind::StringDouble => {
                self.colored(self.definition.string)
            }
            HighlightKind::Operator | HighlightKind::Redirect => self
                .colored(self.definition.foreground)
                .add_modifier(Modifier::BOLD),
            HighlightKind::Expansion => self.colored(self.definition.hint),
            HighlightKind::Error => self
                .colored(self.definition.error)
                .add_modifier(Modifier::UNDERLINED),
            HighlightKind::Number => self.colored(self.definition.data),
            HighlightKind::Argument | HighlightKind::PathLike | HighlightKind::Escaped => {
                self.colored(self.definition.foreground)
            }
        }
    }

    fn colored(self, color: RgbColor) -> Style {
        if self.color {
            Style::default().fg(color.terminal())
        } else {
            Style::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_configured_theme_has_a_unique_bounded_definition() {
        let mut ids = THEME_DEFINITIONS
            .iter()
            .map(|theme| theme.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), THEME_DEFINITIONS.len());
        assert_eq!(THEME_DEFINITIONS.len(), quirl_lua::UI_THEME_NAMES.len());
        for id in quirl_lua::UI_THEME_NAMES {
            assert_eq!(theme_definition(id).id, id);
        }
    }

    #[test]
    fn unknown_theme_falls_back_to_the_quirl_palette() {
        assert_eq!(theme_definition("unknown"), THEME_DEFINITIONS[0]);
    }
}
