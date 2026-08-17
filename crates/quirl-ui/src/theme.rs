use crossterm::style::Color as CrosstermColor;
use nu_ansi_term::{Color as AnsiColor, Style as AnsiStyle};
use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{QuirlConfig, ThemeColors};
use quirl_syntax::{HighlightKind, Mode};
use ratatui::style::{Color as RatatuiColor, Modifier, Style as RatatuiStyle};

use crate::surface::highlight::DiagnosticSeverity;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb {
    red: u8,
    green: u8,
    blue: u8,
}

impl Rgb {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    fn parse(field: &str, value: &str) -> Result<Self, ShellError> {
        let Some(hex) = value.strip_prefix('#') else {
            return Err(invalid_theme_color(field, value));
        };
        if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid_theme_color(field, value));
        }
        let parse_component = |range: std::ops::Range<usize>| {
            u8::from_str_radix(&hex[range], 16).map_err(|_| invalid_theme_color(field, value))
        };
        Ok(Self::new(
            parse_component(0..2)?,
            parse_component(2..4)?,
            parse_component(4..6)?,
        ))
    }

    const fn ratatui(self) -> RatatuiColor {
        RatatuiColor::Rgb(self.red, self.green, self.blue)
    }

    const fn ansi(self) -> AnsiColor {
        AnsiColor::Rgb(self.red, self.green, self.blue)
    }

    const fn crossterm(self) -> CrosstermColor {
        CrosstermColor::Rgb {
            r: self.red,
            g: self.green,
            b: self.blue,
        }
    }
}

fn invalid_theme_color(field: &str, value: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("theme color `{field}` must be an RGB hex color, got `{value}`"),
    )
    .with_help("Use a color in #RRGGBB form, such as #7aa2f7")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Palette {
    accent_command: Rgb,
    accent_data: Rgb,
    context_primary: Rgb,
    context_secondary: Rgb,
    muted: Rgb,
    border: Rgb,
    status_background: Rgb,
    error: Rgb,
    warning: Rgb,
    hint: Rgb,
    string: Rgb,
    operator: Rgb,
    expansion: Rgb,
    number: Rgb,
}

impl Palette {
    const TOKYO_NIGHT: Self = Self {
        accent_command: Rgb::new(158, 206, 106),
        accent_data: Rgb::new(187, 154, 247),
        context_primary: Rgb::new(125, 207, 255),
        context_secondary: Rgb::new(187, 154, 247),
        muted: Rgb::new(86, 95, 137),
        border: Rgb::new(65, 72, 104),
        status_background: Rgb::new(36, 40, 59),
        error: Rgb::new(247, 118, 142),
        warning: Rgb::new(224, 175, 104),
        hint: Rgb::new(122, 162, 247),
        string: Rgb::new(158, 206, 106),
        operator: Rgb::new(137, 221, 255),
        expansion: Rgb::new(122, 162, 247),
        number: Rgb::new(255, 158, 100),
    };

    fn from_colors(colors: &ThemeColors) -> Result<Self, ShellError> {
        Ok(Self {
            accent_command: Rgb::parse("accent_command", &colors.accent_command)?,
            accent_data: Rgb::parse("accent_data", &colors.accent_data)?,
            context_primary: Rgb::parse("context_primary", &colors.context_primary)?,
            context_secondary: Rgb::parse("context_secondary", &colors.context_secondary)?,
            muted: Rgb::parse("muted", &colors.muted)?,
            border: Rgb::parse("border", &colors.border)?,
            status_background: Rgb::parse("status_background", &colors.status_background)?,
            error: Rgb::parse("error", &colors.error)?,
            warning: Rgb::parse("warning", &colors.warning)?,
            hint: Rgb::parse("hint", &colors.hint)?,
            string: Rgb::parse("string", &colors.string)?,
            operator: Rgb::parse("operator", &colors.operator)?,
            expansion: Rgb::parse("expansion", &colors.expansion)?,
            number: Rgb::parse("number", &colors.number)?,
        })
    }
}

/// Resolved semantic styles shared by Quirl's Ratatui and Reedline surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Theme {
    palette: Palette,
    color: bool,
}

impl Theme {
    pub(crate) const fn new(color: bool) -> Self {
        Self {
            palette: Palette::TOKYO_NIGHT,
            color,
        }
    }

    pub(crate) fn from_config(config: &QuirlConfig, color: bool) -> Result<Self, ShellError> {
        let colors = config.active_theme()?;
        Ok(Self {
            palette: Palette::from_colors(&colors)?,
            color,
        })
    }

    /// Infallible editor constructors predate fallible theme selection. Loaded
    /// Lua configuration is validated before reaching them; this fallback only
    /// protects callers that construct an invalid `QuirlConfig` directly.
    pub(crate) fn from_config_or_default(config: &QuirlConfig, color: bool) -> Self {
        Self::from_config(config, color).unwrap_or_else(|_| Self::new(color))
    }

    pub(crate) const fn with_color(self, color: bool) -> Self {
        Self { color, ..self }
    }

    pub(crate) fn accent(self, mode: Mode) -> RatatuiStyle {
        self.ratatui_foreground(self.accent_color(mode))
            .add_modifier(Modifier::BOLD)
    }

    pub(crate) fn context(self) -> RatatuiStyle {
        self.ratatui_foreground(self.palette.context_primary)
            .add_modifier(Modifier::BOLD)
    }

    pub(crate) fn context_secondary(self) -> RatatuiStyle {
        self.ratatui_foreground(self.palette.context_secondary)
            .add_modifier(Modifier::BOLD)
    }

    pub(crate) fn dim(self) -> RatatuiStyle {
        self.ratatui_foreground(self.palette.muted)
            .add_modifier(Modifier::DIM)
    }

    pub(crate) fn border(self) -> RatatuiStyle {
        self.ratatui_foreground(self.palette.border)
    }

    pub(crate) fn selected(self, mode: Mode) -> RatatuiStyle {
        self.accent(mode).add_modifier(Modifier::REVERSED)
    }

    pub(crate) fn status(self) -> RatatuiStyle {
        if self.color {
            RatatuiStyle::default().bg(self.palette.status_background.ratatui())
        } else {
            RatatuiStyle::default()
        }
    }

    pub(crate) fn diagnostic(self, severity: DiagnosticSeverity) -> RatatuiStyle {
        let color = match severity {
            DiagnosticSeverity::Error => self.palette.error,
            DiagnosticSeverity::Warning => self.palette.warning,
            DiagnosticSeverity::Hint => self.palette.hint,
        };
        self.ratatui_foreground(color).add_modifier(Modifier::BOLD)
    }

    pub(crate) fn highlight(self, kind: HighlightKind) -> RatatuiStyle {
        match kind {
            HighlightKind::Command => self
                .ratatui_foreground(self.palette.accent_command)
                .add_modifier(Modifier::BOLD),
            HighlightKind::Flag => self.ratatui_foreground(self.palette.context_primary),
            HighlightKind::StringSingle | HighlightKind::StringDouble => {
                self.ratatui_foreground(self.palette.string)
            }
            HighlightKind::Operator | HighlightKind::Redirect => self
                .ratatui_foreground(self.palette.operator)
                .add_modifier(Modifier::BOLD),
            HighlightKind::Expansion => self.ratatui_foreground(self.palette.expansion),
            HighlightKind::Error => self
                .ratatui_foreground(self.palette.error)
                .add_modifier(Modifier::UNDERLINED),
            HighlightKind::Number => self.ratatui_foreground(self.palette.number),
            HighlightKind::Argument | HighlightKind::PathLike | HighlightKind::Escaped => {
                RatatuiStyle::default()
            }
        }
    }

    pub(crate) fn ansi_hint(self) -> AnsiStyle {
        self.ansi_foreground(self.palette.muted).italic()
    }

    pub(crate) fn ansi_prompt_segment(self, name: &str) -> AnsiStyle {
        match name {
            "directory" => self.ansi_foreground(self.palette.context_primary).bold(),
            "git_branch" | "git_state" => {
                self.ansi_foreground(self.palette.context_secondary).bold()
            }
            "status" => self.ansi_foreground(self.palette.error).bold(),
            "duration" | "jobs" => self.ansi_foreground(self.palette.context_secondary),
            _ => AnsiStyle::new(),
        }
    }

    pub(crate) fn prompt_right_color(self) -> CrosstermColor {
        self.crossterm_color(self.palette.context_secondary)
    }

    pub(crate) fn prompt_accent_color(self, mode: Mode) -> CrosstermColor {
        self.crossterm_color(self.accent_color(mode))
    }

    pub(crate) fn ansi_highlight(self, kind: HighlightKind) -> AnsiStyle {
        match kind {
            HighlightKind::Command => self.ansi_foreground(self.palette.accent_command).bold(),
            HighlightKind::Flag => self.ansi_foreground(self.palette.context_primary),
            HighlightKind::StringSingle | HighlightKind::StringDouble => {
                self.ansi_foreground(self.palette.string)
            }
            HighlightKind::Operator | HighlightKind::Redirect => {
                self.ansi_foreground(self.palette.operator).bold()
            }
            HighlightKind::Expansion => self.ansi_foreground(self.palette.expansion),
            HighlightKind::Error => self.ansi_foreground(self.palette.error).underline(),
            HighlightKind::Number => self.ansi_foreground(self.palette.number),
            HighlightKind::Argument | HighlightKind::PathLike | HighlightKind::Escaped => {
                AnsiStyle::new()
            }
        }
    }

    fn accent_color(self, mode: Mode) -> Rgb {
        match mode {
            Mode::Command => self.palette.accent_command,
            Mode::Data => self.palette.accent_data,
            Mode::Natural => self.palette.accent_command,
        }
    }

    fn ratatui_foreground(self, color: Rgb) -> RatatuiStyle {
        if self.color {
            RatatuiStyle::default().fg(color.ratatui())
        } else {
            RatatuiStyle::default()
        }
    }

    fn ansi_foreground(self, color: Rgb) -> AnsiStyle {
        if self.color {
            AnsiStyle::new().fg(color.ansi())
        } else {
            AnsiStyle::new()
        }
    }

    fn crossterm_color(self, color: Rgb) -> CrosstermColor {
        if self.color {
            color.crossterm()
        } else {
            CrosstermColor::Reset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokyo_night_is_the_default_semantic_palette() {
        let theme = Theme::new(true);
        let configured = Theme::from_config(&QuirlConfig::default(), true).unwrap();

        assert_eq!(theme, configured);
        assert_eq!(
            theme.accent(Mode::Command).fg,
            Some(RatatuiColor::Rgb(158, 206, 106))
        );
        assert_eq!(
            theme.accent(Mode::Data).fg,
            Some(RatatuiColor::Rgb(187, 154, 247))
        );
        assert_eq!(theme.status().bg, Some(RatatuiColor::Rgb(36, 40, 59)));
        assert_eq!(
            theme.prompt_right_color(),
            CrosstermColor::Rgb {
                r: 187,
                g: 154,
                b: 247
            }
        );
    }

    #[test]
    fn custom_theme_roles_are_shared_by_ratatui_and_ansi_styles() {
        let mut config = QuirlConfig::default();
        let mut colors = config.active_theme().unwrap();
        colors.accent_command = "#010203".to_owned();
        colors.context_primary = "#040506".to_owned();
        colors.muted = "#070809".to_owned();
        config.ui.theme = "custom".to_owned();
        config.ui.themes.insert("custom".to_owned(), colors);

        let theme = Theme::from_config(&config, true).unwrap();

        assert_eq!(
            theme.accent(Mode::Command).fg,
            Some(RatatuiColor::Rgb(1, 2, 3))
        );
        assert_eq!(
            theme.prompt_accent_color(Mode::Command),
            CrosstermColor::Rgb { r: 1, g: 2, b: 3 }
        );
        assert_eq!(
            theme.ansi_prompt_segment("directory").foreground,
            Some(AnsiColor::Rgb(4, 5, 6))
        );
        assert_eq!(theme.ansi_hint().foreground, Some(AnsiColor::Rgb(7, 8, 9)));
    }

    #[test]
    fn no_color_removes_palette_colors_but_preserves_semantic_modifiers() {
        let theme = Theme::new(false);

        let command = theme.highlight(HighlightKind::Command);
        assert_eq!(command.fg, None);
        assert!(command.add_modifier.contains(Modifier::BOLD));
        assert_eq!(theme.status().bg, None);
        assert_eq!(
            theme.prompt_accent_color(Mode::Command),
            CrosstermColor::Reset
        );
    }

    #[test]
    fn malformed_rgb_values_fail_at_the_ui_boundary() {
        let error = Rgb::parse("accent_command", "green").unwrap_err();

        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("accent_command"));
        assert!(error.details.help[0].contains("#RRGGBB"));
    }
}
