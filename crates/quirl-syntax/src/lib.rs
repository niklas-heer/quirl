//! Quirl's deliberately small interaction grammar.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Versioned, machine-readable evidence for Quirl's current native compatibility subset and
/// the frozen 1.0 C1/C2 disposition.
pub const COMPATIBILITY_MATRIX_JSON: &str = include_str!("../compatibility-v0.1.json");
pub const GRAMMAR_PROTOCOL_VERSION: u32 = 1;
pub const COMPATIBILITY_MATRIX_SCHEMA_VERSION: u32 = 2;
pub const GRAMMAR_SCHEMA_DESCRIPTOR: &str = "quirl.command-grammar@1{CommandList{deny_unknown;pipelines:array<Pipeline>;connectors:array<and|or>;invariant:connectors.len+1=pipelines.len};Pipeline{deny_unknown;commands:array<SimpleCommand>;background:bool};SimpleCommand{deny_unknown;words:nonempty-array<string>;redirects:array<Redirect>};Redirect{deny_unknown;kind:input|output|append;path:string};tokens:word|pipe|and|or|input|output|append|background;quoting:single|double|backslash;expansion:none;compatibility_matrix:quirl-syntax/compatibility-v0.1.json@schema2}";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandList {
    pub pipelines: Vec<Pipeline>,
    pub connectors: Vec<ListConnector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    pub commands: Vec<SimpleCommand>,
    pub background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimpleCommand {
    pub words: Vec<String>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListConnector {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Redirect {
    pub kind: RedirectKind,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedirectKind {
    Input,
    Output,
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSyntaxError {
    pub message: String,
    pub start: usize,
    pub end: usize,
    pub help: String,
}

impl fmt::Display for CommandSyntaxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CommandSyntaxError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Word(String),
    Pipe,
    And,
    Or,
    Input,
    Output,
    Append,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

/// Parse Quirl's native command-mode C0 graph and the Preview C1 `&&`/`||` subset.
///
/// Expansion is intentionally not performed here. The executor receives exact words after
/// quoting and escaping have been resolved, while redirects and control operators remain typed.
pub fn parse_command_list(input: &str) -> Result<CommandList, CommandSyntaxError> {
    reject_unsupported_constructs(input)?;
    let tokens = lex_command(input)?;
    if tokens.is_empty() {
        return Ok(CommandList {
            pipelines: Vec::new(),
            connectors: Vec::new(),
        });
    }
    let mut pipelines = Vec::new();
    let mut connectors = Vec::new();
    let mut commands = Vec::new();
    let mut words = Vec::new();
    let mut redirects = Vec::new();
    let mut index = 0;

    while index < tokens.len() {
        let token = &tokens[index];
        match &token.kind {
            TokenKind::Word(word) => words.push(word.clone()),
            TokenKind::Input | TokenKind::Output | TokenKind::Append => {
                let Some(next) = tokens.get(index + 1) else {
                    return Err(syntax_error(
                        token,
                        "redirection needs a path",
                        "Add a file path after the redirection operator",
                    ));
                };
                let TokenKind::Word(path) = &next.kind else {
                    return Err(syntax_error(
                        next,
                        "redirection path must be a word",
                        "Quote the path if it contains shell operators",
                    ));
                };
                redirects.push(Redirect {
                    kind: match token.kind {
                        TokenKind::Input => RedirectKind::Input,
                        TokenKind::Output => RedirectKind::Output,
                        TokenKind::Append => RedirectKind::Append,
                        _ => unreachable!(),
                    },
                    path: path.clone(),
                });
                index += 1;
            }
            TokenKind::Pipe => {
                commands.push(finish_command(&mut words, &mut redirects, token)?);
            }
            TokenKind::And | TokenKind::Or => {
                commands.push(finish_command(&mut words, &mut redirects, token)?);
                pipelines.push(Pipeline {
                    commands: std::mem::take(&mut commands),
                    background: false,
                });
                connectors.push(if matches!(token.kind, TokenKind::And) {
                    ListConnector::And
                } else {
                    ListConnector::Or
                });
            }
            TokenKind::Background => {
                commands.push(finish_command(&mut words, &mut redirects, token)?);
                pipelines.push(Pipeline {
                    commands: std::mem::take(&mut commands),
                    background: true,
                });
                if index + 1 < tokens.len() {
                    return Err(syntax_error(
                        &tokens[index + 1],
                        "background marker must end a command list",
                        "Run the following command on a new line",
                    ));
                }
            }
        }
        index += 1;
    }

    if tokens
        .last()
        .is_some_and(|token| matches!(token.kind, TokenKind::Pipe | TokenKind::And | TokenKind::Or))
    {
        let token = tokens.last().ok_or_else(|| CommandSyntaxError {
            message: "command list is empty".to_owned(),
            start: 0,
            end: 0,
            help: "Enter a command".to_owned(),
        })?;
        return Err(syntax_error(
            token,
            "command list ends with a control operator",
            "Add a command after the operator",
        ));
    }
    if !words.is_empty() || !redirects.is_empty() {
        let end = input.len();
        let sentinel = Token {
            kind: TokenKind::Background,
            start: end,
            end,
        };
        commands.push(finish_command(&mut words, &mut redirects, &sentinel)?);
    }
    if !commands.is_empty() {
        pipelines.push(Pipeline {
            commands,
            background: false,
        });
    }
    debug_assert_eq!(pipelines.len(), connectors.len() + 1);
    Ok(CommandList {
        pipelines,
        connectors,
    })
}

/// Return the expression from a `data` statement.
///
/// The keyword must be followed by whitespace or the end of the line. Callers
/// share this classifier so checking and execution cannot disagree.
pub fn data_statement_expression(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

/// Validate a line-oriented `.quirl` script without executing it.
///
/// Command statements use the same compatibility parser as interactive execution. `data`
/// statements are recognized as a separate language island and require a non-empty expression.
/// Every returned span is absolute within `source` and diagnostics retain source order.
pub fn check_script(source: &str) -> Vec<CommandSyntaxError> {
    let mut diagnostics = Vec::new();
    let mut offset = 0;
    for (line_index, raw_line) in source.split_inclusive('\n').enumerate() {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let trimmed = line.trim();
        let leading = line.len().saturating_sub(line.trim_start().len());
        if (line_index == 0 && trimmed.starts_with("#!"))
            || trimmed.is_empty()
            || trimmed.starts_with('#')
        {
            offset += raw_line.len();
            continue;
        }
        if let Some(expression) = data_statement_expression(trimmed) {
            if expression.is_empty() {
                diagnostics.push(CommandSyntaxError {
                    message: "data statement requires an expression".to_owned(),
                    start: offset + leading,
                    end: offset + leading + trimmed.len(),
                    help: "Add a structured-data expression after `data`".to_owned(),
                });
            }
            offset += raw_line.len();
            continue;
        }
        if let Err(mut error) = parse_command_list(trimmed) {
            error.start += offset + leading;
            error.end += offset + leading;
            diagnostics.push(error);
        }
        offset += raw_line.len();
    }
    diagnostics
}

fn reject_unsupported_constructs(input: &str) -> Result<(), CommandSyntaxError> {
    let trimmed = input.trim_start();
    let leading_whitespace = input.len() - trimmed.len();
    for keyword in ["for", "while", "until", "if", "case", "select", "function"] {
        if trimmed == keyword
            || trimmed
                .strip_prefix(keyword)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
        {
            return Err(dialect_mismatch(
                leading_whitespace,
                leading_whitespace + keyword.len(),
                &format!("compound command `{keyword}`"),
            ));
        }
    }
    if trimmed.starts_with("[[") || trimmed.starts_with("((") {
        let form = if trimmed.starts_with("[[") {
            "conditional command `[[ ... ]]`"
        } else {
            "arithmetic command `(( ... ))`"
        };
        return Err(dialect_mismatch(
            leading_whitespace,
            leading_whitespace + 2,
            form,
        ));
    }

    let mut quote = None;
    let mut escaped = false;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if quote == Some('\'') {
            if character == '\'' {
                quote = None;
            }
            continue;
        }
        if quote.is_none() && character == '\'' {
            quote = Some('\'');
            continue;
        }
        if character == '"' {
            quote = if quote == Some('"') { None } else { Some('"') };
            continue;
        }

        let rest = &input[index..];
        if quote.is_none()
            && (rest.starts_with("<<")
                || rest.starts_with(">&")
                || rest.starts_with("<&")
                || (character.is_ascii_digit()
                    && rest
                        .chars()
                        .nth(1)
                        .is_some_and(|next| matches!(next, '>' | '<'))))
        {
            let form = if rest.starts_with("<<<") {
                "here-string redirection `<<<`".to_owned()
            } else if rest.starts_with("<<") {
                "here-document redirection `<<`".to_owned()
            } else if rest.starts_with(">&") {
                "file-descriptor duplication `>&`".to_owned()
            } else if rest.starts_with("<&") {
                "file-descriptor duplication `<&`".to_owned()
            } else {
                format!("file-descriptor redirection `{}`", &rest[..2])
            };
            return Err(dialect_mismatch(index, index + 2, &form));
        }
        if rest.starts_with("$(") {
            let form = if rest.starts_with("$((") {
                "arithmetic expansion `$((...))`"
            } else {
                "command substitution `$(...)`"
            };
            return Err(dialect_mismatch(index, index + 2, form));
        }
        if character == '$' {
            let form = parameter_expansion_form(rest);
            return Err(dialect_mismatch(index, index + character.len_utf8(), form));
        }
        if character == '`' {
            return Err(dialect_mismatch(
                index,
                index + character.len_utf8(),
                "backtick command substitution `` `...` ``",
            ));
        }
        if quote.is_none() && (rest.starts_with("<(") || rest.starts_with(">(")) {
            let form = if rest.starts_with("<(") {
                "process substitution `<(...)`"
            } else {
                "process substitution `>(...)`"
            };
            return Err(dialect_mismatch(index, index + 2, form));
        }
        if quote.is_none() && matches!(character, '*' | '?') {
            return Err(dialect_mismatch(
                index,
                index + character.len_utf8(),
                &format!("pathname expansion `{character}`"),
            ));
        }
        if quote.is_none() && character == '[' && rest.contains(']') {
            return Err(dialect_mismatch(
                index,
                index + character.len_utf8(),
                "pathname expansion `[...]`",
            ));
        }
        if quote.is_none() && character == ';' {
            return Err(dialect_mismatch(
                index,
                index + character.len_utf8(),
                "semicolon-separated command list `;`",
            ));
        }
        if quote.is_none() && matches!(character, '(' | ')' | '{' | '}') {
            return Err(dialect_mismatch(
                index,
                index + character.len_utf8(),
                &format!("compound-command token `{character}`"),
            ));
        }
    }
    Ok(())
}

fn parameter_expansion_form(rest: &str) -> &'static str {
    if rest.starts_with("${") {
        "parameter expansion `${...}`"
    } else if rest
        .chars()
        .nth(1)
        .is_some_and(|character| character.is_ascii_digit() || "?@*#$!-".contains(character))
    {
        "special parameter expansion `$?`/`$1`"
    } else {
        "parameter expansion `$NAME`"
    }
}

fn dialect_mismatch(start: usize, end: usize, form: &str) -> CommandSyntaxError {
    CommandSyntaxError {
        message: format!("unsupported Bash/Zsh construct: {form}"),
        start,
        end,
        help: "Run this form explicitly with `bash -c '...'` or `zsh -c '...'`; its native C1 disposition is recorded in the versioned compatibility matrix"
            .to_owned(),
    }
}

fn finish_command(
    words: &mut Vec<String>,
    redirects: &mut Vec<Redirect>,
    operator: &Token,
) -> Result<SimpleCommand, CommandSyntaxError> {
    if words.is_empty() {
        return Err(syntax_error(
            operator,
            "expected a command before this operator",
            "Add a command name or remove the operator",
        ));
    }
    Ok(SimpleCommand {
        words: std::mem::take(words),
        redirects: std::mem::take(redirects),
    })
}

fn syntax_error(token: &Token, message: &str, help: &str) -> CommandSyntaxError {
    CommandSyntaxError {
        message: message.to_owned(),
        start: token.start,
        end: token.end,
        help: help.to_owned(),
    }
}

fn lex_command(input: &str) -> Result<Vec<Token>, CommandSyntaxError> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut word_start = None;
    let mut quote = None;
    let mut escaped = false;
    let mut characters = input.char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            word_start.get_or_insert(index);
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                word.push(character);
            }
            continue;
        }
        if matches!(character, '\'' | '"') {
            word_start.get_or_insert(index);
            quote = Some(character);
            continue;
        }
        if character.is_whitespace() {
            push_word(&mut tokens, &mut word, &mut word_start, index);
            continue;
        }

        let operator = match character {
            '|' if characters.peek().is_some_and(|(_, next)| *next == '|') => {
                characters.next();
                Some((TokenKind::Or, 2))
            }
            '|' => Some((TokenKind::Pipe, 1)),
            '&' if characters.peek().is_some_and(|(_, next)| *next == '&') => {
                characters.next();
                Some((TokenKind::And, 2))
            }
            '&' => Some((TokenKind::Background, 1)),
            '>' if characters.peek().is_some_and(|(_, next)| *next == '>') => {
                characters.next();
                Some((TokenKind::Append, 2))
            }
            '>' => Some((TokenKind::Output, 1)),
            '<' => Some((TokenKind::Input, 1)),
            _ => None,
        };
        if let Some((kind, width)) = operator {
            push_word(&mut tokens, &mut word, &mut word_start, index);
            tokens.push(Token {
                kind,
                start: index,
                end: index + width,
            });
        } else {
            word_start.get_or_insert(index);
            word.push(character);
        }
    }

    if escaped {
        return Err(CommandSyntaxError {
            message: "command ends with an escape".to_owned(),
            start: input.len().saturating_sub(1),
            end: input.len(),
            help: "Add the escaped character or remove the trailing backslash".to_owned(),
        });
    }
    if let Some(active) = quote {
        return Err(CommandSyntaxError {
            message: format!("unclosed {active} quote"),
            start: word_start.unwrap_or(input.len()),
            end: input.len(),
            help: format!("Close the quote with {active}"),
        });
    }
    push_word(&mut tokens, &mut word, &mut word_start, input.len());
    Ok(tokens)
}

fn push_word(tokens: &mut Vec<Token>, word: &mut String, start: &mut Option<usize>, end: usize) {
    if let Some(start_index) = start.take() {
        tokens.push(Token {
            kind: TokenKind::Word(std::mem::take(word)),
            start: start_index,
            end,
        });
    }
}

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
    use serde::Deserialize;
    use std::{collections::HashSet, process::Command};

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CompatibilityMatrix {
        schema_version: u64,
        matrix_version: String,
        target_contract_version: String,
        compatibility_level: String,
        scope: String,
        contract_status: String,
        differential_fixtures: Vec<DifferentialFixture>,
        supported: Vec<SupportedFeature>,
        deferred: Vec<DeferredFeature>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DifferentialFixture {
        id: String,
        source: String,
        stdout: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SupportedFeature {
        id: String,
        level: String,
        implementation: String,
        forms: Vec<String>,
        limitations: String,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct DeferredFeature {
        id: String,
        level: String,
        dialects: Vec<String>,
        reason: String,
        fixtures: Vec<MismatchFixture>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct MismatchFixture {
        source: String,
        diagnostic: String,
    }

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

    #[test]
    fn command_graph_preserves_quotes_pipes_redirects_and_backgrounding() {
        let graph =
            parse_command_list("printf '%s\\n' 'hello world' | grep hello >> out.txt &").unwrap();
        assert_eq!(graph.pipelines.len(), 1);
        assert!(graph.pipelines[0].background);
        assert_eq!(graph.pipelines[0].commands.len(), 2);
        assert_eq!(
            graph.pipelines[0].commands[0].words,
            ["printf", "%s\\n", "hello world"]
        );
        assert_eq!(
            graph.pipelines[0].commands[1].redirects,
            [Redirect {
                kind: RedirectKind::Append,
                path: "out.txt".to_owned(),
            }]
        );
    }

    #[test]
    fn command_graph_parses_c1_boolean_connectors() {
        let graph = parse_command_list("false || echo recovered && echo done").unwrap();
        assert_eq!(graph.pipelines.len(), 3);
        assert_eq!(graph.connectors, [ListConnector::Or, ListConnector::And]);
    }

    #[test]
    fn incomplete_command_graph_reports_a_precise_recoverable_span() {
        let error = parse_command_list("echo ok | ").unwrap_err();
        assert_eq!(error.message, "command list ends with a control operator");
        assert_eq!((error.start, error.end), (8, 9));

        let error = parse_command_list("echo 'unfinished").unwrap_err();
        assert_eq!(error.start, 5);
        assert!(error.help.contains("Close"));
    }

    #[test]
    fn script_checker_reuses_command_dialect_rules_with_absolute_spans() {
        let source =
            "#!/usr/bin/env -S quirl run\ndata {\"ok\":true}\n  echo $HOME\nprintf ok |\ndata\n";
        let diagnostics = check_script(source);
        assert_eq!(diagnostics.len(), 3);
        assert_eq!(&source[diagnostics[0].start..diagnostics[0].end], "$");
        assert_eq!(&source[diagnostics[1].start..diagnostics[1].end], "|");
        assert_eq!(&source[diagnostics[2].start..diagnostics[2].end], "data");
    }

    #[test]
    fn data_statement_classifier_accepts_whitespace_without_matching_command_names() {
        assert_eq!(data_statement_expression("data\t[1, 2]"), Some("[1, 2]"));
        assert_eq!(
            data_statement_expression("data   { ok: true }"),
            Some("{ ok: true }")
        );
        assert_eq!(data_statement_expression("data"), Some(""));
        assert_eq!(data_statement_expression("database status"), None);
    }

    #[test]
    fn compatibility_matrix_is_versioned_complete_and_schema_checked() {
        let matrix: CompatibilityMatrix = serde_json::from_str(COMPATIBILITY_MATRIX_JSON).unwrap();
        assert_eq!(matrix.schema_version, 2);
        assert_eq!(matrix.matrix_version, "0.1");
        assert_eq!(matrix.target_contract_version, "1.0");
        assert_eq!(matrix.compatibility_level, "C1-partial+C2-runner");
        assert_eq!(matrix.contract_status, "frozen_disposition");
        assert!(!matrix.scope.is_empty());
        assert!(!matrix.differential_fixtures.is_empty());
        let fixture_ids = matrix
            .differential_fixtures
            .iter()
            .map(|fixture| fixture.id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(fixture_ids.len(), matrix.differential_fixtures.len());
        for fixture in &matrix.differential_fixtures {
            assert!(!fixture.source.is_empty(), "{} needs a source", fixture.id);
        }

        let supported = matrix
            .supported
            .iter()
            .map(|feature| feature.id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(supported.len(), matrix.supported.len());
        assert_eq!(
            supported,
            HashSet::from([
                "quoting",
                "byte_pipes",
                "redirects",
                "background_marker",
                "boolean_lists",
                "export_assignment",
                "bash_script_runner",
                "zsh_script_runner",
                "script_shebang_dispatch",
            ])
        );
        for feature in &matrix.supported {
            assert!(matches!(feature.level.as_str(), "C0" | "C1" | "C2"));
            assert!(matches!(
                feature.implementation.as_str(),
                "native" | "reference_runner"
            ));
            assert!(!feature.forms.is_empty(), "{} needs an example", feature.id);
            assert!(
                !feature.limitations.is_empty(),
                "{} needs explicit limitations",
                feature.id
            );
        }

        let deferred = matrix
            .deferred
            .iter()
            .map(|feature| feature.id.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(deferred.len(), matrix.deferred.len());
        assert!(deferred.contains("pathname_expansion"));
        assert!(deferred.contains("command_and_process_substitution"));
        assert!(deferred.contains("compound_commands"));
        assert!(deferred.contains("parameter_and_arithmetic_expansion"));
        assert!(deferred.contains("interactive_dialect_islands"));
        for feature in &matrix.deferred {
            assert!(matches!(feature.level.as_str(), "C1" | "C2"));
            assert_eq!(feature.dialects, ["bash", "zsh"]);
            assert!(!feature.reason.is_empty(), "{} needs a reason", feature.id);
            assert!(
                !feature.fixtures.is_empty(),
                "{} needs a fixture",
                feature.id
            );
        }
    }

    #[test]
    fn every_deferred_form_produces_its_frozen_exact_mismatch_diagnostic() {
        let matrix: CompatibilityMatrix = serde_json::from_str(COMPATIBILITY_MATRIX_JSON).unwrap();
        for fixture in matrix.deferred.iter().flat_map(|feature| &feature.fixtures) {
            let error = parse_command_list(&fixture.source).unwrap_err();
            assert_eq!(
                error.message, fixture.diagnostic,
                "source: {}",
                fixture.source
            );
            assert!(error.help.contains("bash -c"));
            assert!(error.help.contains("zsh -c"));
        }

        let quoted = parse_command_list("printf '%s' '*.rs $HOME'").unwrap();
        assert_eq!(quoted.pipelines[0].commands[0].words[2], "*.rs $HOME");
    }

    #[test]
    fn bash_and_zsh_differential_fixtures_match_frozen_output_when_available() {
        let matrix: CompatibilityMatrix = serde_json::from_str(COMPATIBILITY_MATRIX_JSON).unwrap();

        for shell in ["bash", "zsh"] {
            if !shell_is_available(shell) {
                eprintln!("skipping {shell} differential checks: executable is unavailable");
                continue;
            }
            for fixture in &matrix.differential_fixtures {
                parse_command_list(&fixture.source).unwrap();
                let mut command = Command::new(shell);
                if shell == "bash" {
                    command.args(["--noprofile", "--norc", "-c", &fixture.source]);
                    command.env_remove("BASH_ENV").env_remove("ENV");
                } else {
                    command.args(["-f", "-c", &fixture.source]);
                    command.env_remove("ZDOTDIR").env_remove("ENV");
                }
                let output = command.env("LC_ALL", "C").output().unwrap();
                assert!(
                    output.status.success(),
                    "{shell} rejected differential fixture `{}` ({})",
                    fixture.id,
                    fixture.source
                );
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap(),
                    fixture.stdout,
                    "{shell} disagreed for differential fixture `{}` ({})",
                    fixture.id,
                    fixture.source
                );
            }
        }
    }

    fn shell_is_available(shell: &str) -> bool {
        Command::new(shell)
            .arg("-c")
            .arg(":")
            .output()
            .is_ok_and(|output| output.status.success())
    }
}
