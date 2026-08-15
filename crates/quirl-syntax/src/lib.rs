//! Quirl's deliberately small interaction grammar.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// The grammar and runtime contract currently active in an interactive session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Command,
    Data,
}

impl Mode {
    pub const fn toggled(self) -> Self {
        match self {
            Self::Command => Self::Data,
            Self::Data => Self::Command,
        }
    }

    pub const fn prompt(self) -> &'static str {
        match self {
            Self::Command => "❯",
            Self::Data => "◆",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Command => "command",
            Self::Data => "data",
        })
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "command" | "cmd" => Ok(Self::Command),
            "data" => Ok(Self::Data),
            _ => Err(format!(
                "unknown mode `{value}`; expected `command` or `data`"
            )),
        }
    }
}

/// A line classified without guessing between two full languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveLine<'a> {
    Empty,
    Exit,
    ChangeMode(Mode),
    ToggleMode,
    Help(Option<&'a str>),
    Command(&'a str),
    Data(&'a str),
    Lua(&'a str),
}

pub fn classify(mode: Mode, input: &str) -> InteractiveLine<'_> {
    let input = input.trim();
    if input.is_empty() {
        return InteractiveLine::Empty;
    }
    if matches!(input, "exit" | "quit") {
        return InteractiveLine::Exit;
    }
    if input == "mode toggle" {
        return InteractiveLine::ToggleMode;
    }
    if let Some(value) = input.strip_prefix("mode ") {
        if let Ok(mode) = value.trim().parse() {
            return InteractiveLine::ChangeMode(mode);
        }
    }
    if input == "help" {
        return InteractiveLine::Help(None);
    }
    if let Some(topic) = input.strip_prefix("help ") {
        return InteractiveLine::Help(Some(topic.trim()));
    }
    if let Some(expression) = input.strip_prefix("lua ") {
        return InteractiveLine::Lua(expression.trim());
    }

    match mode {
        Mode::Command => InteractiveLine::Command(input),
        Mode::Data => InteractiveLine::Data(input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_explicit() {
        assert_eq!(
            classify(Mode::Command, "mode data"),
            InteractiveLine::ChangeMode(Mode::Data)
        );
        assert_eq!(
            classify(Mode::Data, "[1,2,3] | length"),
            InteractiveLine::Data("[1,2,3] | length")
        );
        assert_eq!(
            classify(Mode::Command, "echo hello"),
            InteractiveLine::Command("echo hello")
        );
    }

    #[test]
    fn lua_can_be_bridged_from_command_mode() {
        assert_eq!(
            classify(Mode::Command, "lua return 20 + 22"),
            InteractiveLine::Lua("return 20 + 22")
        );
    }
}
