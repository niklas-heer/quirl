//! Quirl's deliberately small interaction grammar.

use serde::{Deserialize, Serialize};
use std::{fmt, ops::Range, str::FromStr};

/// Versioned, machine-readable evidence for Quirl's current native compatibility subset and
/// the frozen 1.0 C1/C2 disposition.
pub const COMPATIBILITY_MATRIX_JSON: &str = include_str!("../compatibility-v0.1.json");
/// Current serialized version of the native command grammar.
pub const GRAMMAR_PROTOCOL_VERSION: u32 = 2;
/// Schema version of the JSON evidence embedded in [`COMPATIBILITY_MATRIX_JSON`].
pub const COMPATIBILITY_MATRIX_SCHEMA_VERSION: u32 = 3;
/// Canonical structural description used to fingerprint the command grammar.
///
/// Any change to serialized shapes, token semantics, supported expansions, or
/// the referenced compatibility matrix must update this descriptor and the
/// corresponding protocol version.
pub const GRAMMAR_SCHEMA_DESCRIPTOR: &str = "quirl.command-grammar@2{CommandList{deny_unknown;pipelines:array<Pipeline>;connectors:array<and|or|sequence>;invariant:connectors.len+1=pipelines.len};Pipeline{deny_unknown;commands:array<SimpleCommand>;background:bool};SimpleCommand{deny_unknown;words:nonempty-array<string>;word_ir:array<Word>;redirects:array<Redirect>};Word{deny_unknown;parts:nonempty-array<WordPart>};WordPart{deny_unknown;text:string;quoting:unquoted|single|double|escaped};Redirect{deny_unknown;fd:u8;kind:input|output|append|here_string|duplicate_input|duplicate_output;path:string;target:Word};tokens:word|pipe|and|or|semicolon|input|output|append|here_string|fd_duplicate|background;expansion:parameter|special|arithmetic|command|pathname;compatibility_matrix:quirl-syntax/compatibility-v0.1.json@schema3}";

const MAX_COMMAND_SUBSTITUTION_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Parsed native command list composed of pipelines and their control connectors.
///
/// Empty input is represented by two empty vectors. For a non-empty list,
/// `connectors.len() + 1 == pipelines.len()` and connector `i` controls whether
/// pipeline `i + 1` runs after pipeline `i`.
pub struct CommandList {
    /// Pipelines in source and execution order.
    pub pipelines: Vec<Pipeline>,
    /// Boolean or sequential relationships between adjacent pipelines.
    pub connectors: Vec<ListConnector>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One byte-stream pipeline of simple commands.
///
/// Parsed pipelines contain at least one command. A background pipeline can
/// only occur at the end of a command list because `&` terminates native input.
pub struct Pipeline {
    /// Non-empty command stages in left-to-right pipe order.
    pub commands: Vec<SimpleCommand>,
    /// Whether the host should start the pipeline without foreground waiting.
    pub background: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Command name, arguments, and redirections without executing expansions.
pub struct SimpleCommand {
    /// Quote-stripped text for the command name and arguments.
    ///
    /// Parsed commands contain at least one word. This compatibility view loses
    /// quoting origin; execution must use [`Self::word_ir`].
    pub words: Vec<String>,
    /// Quote-aware form of `words`. `words` remains a convenient lossless joined view for
    /// built-ins and protocol consumers from grammar v1; execution uses this field.
    pub word_ir: Vec<Word>,
    /// Redirections in source order, separate from argument words.
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One lexical shell word split whenever its quoting context changes.
///
/// Quote delimiters and escape backslashes are not retained; each part records
/// how its text appeared so the process layer can decide which expansions apply.
pub struct Word {
    /// Contiguous fragments in source order.
    pub parts: Vec<WordPart>,
}

impl Word {
    /// Concatenate fragment text into the quote-stripped compatibility view.
    ///
    /// This intentionally performs no parameter, command, arithmetic, or
    /// pathname expansion.
    pub fn text(&self) -> String {
        self.parts.iter().map(|part| part.text.as_str()).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Contiguous text within one source quoting context.
pub struct WordPart {
    /// Decoded fragment text without quote delimiters or an escape backslash.
    pub text: String,
    /// Source context that controls later expansion.
    pub quoting: Quoting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Source quoting context retained for expansion and display semantics.
pub enum Quoting {
    /// Ordinary shell text eligible for every supported native expansion.
    Unquoted,
    /// Single-quoted literal text; no expansion is permitted.
    Single,
    /// Double-quoted text; parameter, arithmetic, and command expansion may apply.
    Double,
    /// A character made literal by a preceding backslash.
    Escaped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Execution relationship between adjacent pipelines in a [`CommandList`].
pub enum ListConnector {
    /// Run the following pipeline only when the preceding status is zero (`&&`).
    And,
    /// Run the following pipeline only when the preceding status is non-zero (`||`).
    Or,
    /// Run the following pipeline regardless of the preceding status (`;`).
    Sequence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Redirection attached to a [`SimpleCommand`].
///
/// Native C1 accepts descriptors 0, 1, and 2 with kind-specific constraints;
/// descriptor duplication is limited to `2>&1` by [`parse_command_list`].
pub struct Redirect {
    /// Source or destination file descriptor.
    pub fd: u8,
    /// Operation applied to the descriptor.
    pub kind: RedirectKind,
    /// Quote-stripped compatibility view of [`Self::target`].
    pub path: String,
    /// Quote-aware target used by execution-time expansion.
    pub target: Word,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Supported redirection operators in the native command graph.
pub enum RedirectKind {
    /// Read descriptor 0 from a file (`<`).
    Input,
    /// Replace descriptor 1 or 2 with a newly truncated file (`>`).
    Output,
    /// Append descriptor 1 or 2 to a file (`>>`).
    Append,
    /// Feed one expanded word plus a newline to descriptor 0 (`<<<`).
    HereString,
    /// Duplicate an input descriptor (`<&`); reserved by the IR but rejected in native C1.
    DuplicateInput,
    /// Duplicate an output descriptor (`>&`); native C1 accepts only `2>&1`.
    DuplicateOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Recoverable command-grammar diagnostic over a half-open UTF-8 byte range.
///
/// Offsets refer to the exact input passed to the producing parser, except that
/// [`check_script`] rebases them to the complete script. Ranges may be empty at
/// end-of-input and are always suitable for slicing when `start < end`.
pub struct CommandSyntaxError {
    /// Concise explanation of the invalid or unsupported form.
    pub message: String,
    /// Inclusive diagnostic start byte offset.
    pub start: usize,
    /// Exclusive diagnostic end byte offset.
    pub end: usize,
    /// Actionable guidance for expressing the intended operation safely.
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
    Word(Word),
    Pipe,
    And,
    Or,
    Redirect { fd: u8, kind: RedirectKind },
    Semicolon,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

/// A lexer-derived presentation span into the original input.
///
/// Spans returned by [`highlight`] are sorted, non-overlapping, and exhaustive.
/// Keeping this type free of UI concepts lets every interactive surface use the
/// same syntax truth without inverting the workspace dependency graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    /// Half-open UTF-8 byte range into the exact line passed to [`highlight`].
    pub range: Range<usize>,
    /// Presentation classification for every byte in [`Self::range`].
    pub kind: HighlightKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Lexer-derived presentation role independent of any terminal color theme.
pub enum HighlightKind {
    /// First word of a command or pipeline stage.
    Command,
    /// Non-command word whose raw source starts with `-`.
    Flag,
    /// Ordinary argument text, including whitespace and neutral data-mode text.
    Argument,
    /// Redirection target or word resembling a filesystem/glob path.
    PathLike,
    /// Word consisting of a single-quoted fragment.
    StringSingle,
    /// Word consisting of a double-quoted fragment.
    StringDouble,
    /// Word consisting of a backslash-escaped fragment.
    Escaped,
    /// Pipe, boolean connector, semicolon, or background marker.
    Operator,
    /// Redirection operator, excluding its target word.
    Redirect,
    /// Word whose raw source contains a parameter or command-expansion marker.
    Expansion,
    /// Word accepted by Rust's floating-point parser.
    Number,
    /// Lexer diagnostic range for invalid or incomplete input.
    Error,
}

/// Return total, lossless highlighting information for an arbitrary edit buffer.
///
/// Command-mode classification is built on the parser's lexer tokens. Invalid or
/// incomplete input remains renderable: the valid prefix keeps its neutral style
/// and the lexer diagnostic range becomes an error span. Data mode intentionally
/// stays neutral until its expression grammar exposes tokens. Returned ranges
/// use UTF-8 byte offsets, cover `0..line.len()` without gaps for non-empty
/// input, and require memory proportional to the line length. This function does
/// not impose an input-size limit; interactive callers must bound edit buffers.
pub fn highlight(line: &str, mode: Mode) -> Vec<HighlightSpan> {
    if line.is_empty() {
        return Vec::new();
    }
    if mode != Mode::Command {
        return vec![HighlightSpan {
            range: 0..line.len(),
            kind: HighlightKind::Argument,
        }];
    }

    match lex_command(line) {
        Ok(tokens) => highlight_tokens(line, &tokens),
        Err(error) => {
            let start = error.start.min(line.len());
            let end = error.end.max(start.saturating_add(1)).min(line.len());
            let mut spans = Vec::new();
            if start > 0 {
                spans.push(HighlightSpan {
                    range: 0..start,
                    kind: HighlightKind::Argument,
                });
            }
            if end > start {
                spans.push(HighlightSpan {
                    range: start..end,
                    kind: HighlightKind::Error,
                });
            }
            if end < line.len() {
                spans.push(HighlightSpan {
                    range: end..line.len(),
                    kind: HighlightKind::Argument,
                });
            }
            spans
        }
    }
}

fn highlight_tokens(line: &str, tokens: &[Token]) -> Vec<HighlightSpan> {
    let mut classified = Vec::with_capacity(tokens.len());
    let mut command_position = true;
    let mut redirect_target = false;
    for token in tokens {
        let raw = line.get(token.start..token.end).unwrap_or_default();
        let kind = match &token.kind {
            TokenKind::Pipe
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Semicolon
            | TokenKind::Background => {
                command_position = true;
                HighlightKind::Operator
            }
            TokenKind::Redirect { .. } => {
                redirect_target = true;
                HighlightKind::Redirect
            }
            TokenKind::Word(word) => {
                if command_position {
                    command_position = false;
                    HighlightKind::Command
                } else if redirect_target {
                    redirect_target = false;
                    HighlightKind::PathLike
                } else {
                    classify_word(raw, word)
                }
            }
        };
        classified.push((token.start..token.end, kind));
    }

    let mut spans = Vec::with_capacity(classified.len().saturating_mul(2).saturating_add(1));
    let mut cursor = 0;
    for (range, kind) in classified {
        if cursor < range.start {
            spans.push(HighlightSpan {
                range: cursor..range.start,
                kind: HighlightKind::Argument,
            });
        }
        spans.push(HighlightSpan {
            range: range.clone(),
            kind,
        });
        cursor = range.end;
    }
    if cursor < line.len() {
        spans.push(HighlightSpan {
            range: cursor..line.len(),
            kind: HighlightKind::Argument,
        });
    }
    spans
}

fn classify_word(raw: &str, word: &Word) -> HighlightKind {
    if word.parts.len() == 1 {
        match word.parts[0].quoting {
            Quoting::Single => return HighlightKind::StringSingle,
            Quoting::Double => return HighlightKind::StringDouble,
            Quoting::Escaped => return HighlightKind::Escaped,
            Quoting::Unquoted => {}
        }
    }
    if raw.starts_with('-') {
        HighlightKind::Flag
    } else if raw.contains("$(") || raw.contains("${") || raw.contains('$') {
        HighlightKind::Expansion
    } else if raw.parse::<f64>().is_ok() {
        HighlightKind::Number
    } else if raw.contains('/')
        || raw.contains('~')
        || raw.contains('*')
        || raw.contains('?')
        || raw.contains('[')
    {
        HighlightKind::PathLike
    } else {
        HighlightKind::Argument
    }
}

/// Parse Quirl's quote-aware C1 command graph.
///
/// Parsing deliberately does not execute expansion.  The IR preserves the origin of every
/// fragment so the process boundary can expand unquoted/double quoted parameters while keeping
/// single quoted and escaped text literal.
///
/// Empty or whitespace-only input succeeds with an empty [`CommandList`].
/// Invalid quoting, unsupported C1 dialect forms, malformed control operators,
/// or unsupported descriptor graphs return [`CommandSyntaxError`] with UTF-8
/// byte offsets into `input`. Parsing is non-recursive and retains data
/// proportional to the input; the owning ingestion boundary must enforce its
/// configured source-byte and command-count limits.
pub fn parse_command_list(input: &str) -> Result<CommandList, CommandSyntaxError> {
    let tokens = lex_command(input)?;
    reject_reserved_dialect_forms(&tokens)?;
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
    let mut word_ir = Vec::new();
    let mut redirects = Vec::new();
    let mut index = 0;

    while index < tokens.len() {
        let token = &tokens[index];
        match &token.kind {
            TokenKind::Word(word) => {
                words.push(word.text());
                word_ir.push(word.clone());
            }
            TokenKind::Redirect { fd, kind } => {
                if *fd > 2 {
                    return Err(syntax_error(
                        token,
                        "native C1 supports only file descriptors 0, 1, and 2",
                        "Use `bash { ... }` or `zsh { ... }` for non-standard descriptor routing",
                    ));
                }
                let descriptor_matches_kind = match kind {
                    RedirectKind::Input | RedirectKind::HereString => *fd == 0,
                    RedirectKind::Output | RedirectKind::Append => matches!(*fd, 1 | 2),
                    RedirectKind::DuplicateInput | RedirectKind::DuplicateOutput => true,
                };
                if !descriptor_matches_kind {
                    return Err(syntax_error(
                        token,
                        "native C1 does not support this descriptor and redirect combination",
                        "Use descriptor 0 for input, 1 or 2 for output, or an explicit `bash { ... }`/`zsh { ... }` island",
                    ));
                }
                let Some(next) = tokens.get(index + 1) else {
                    return Err(syntax_error(
                        token,
                        "redirection needs a path",
                        "Add a file path after the redirection operator",
                    ));
                };
                let TokenKind::Word(target) = &next.kind else {
                    return Err(syntax_error(
                        next,
                        "redirection path must be a word",
                        "Quote the path if it contains shell operators",
                    ));
                };
                if *kind == RedirectKind::DuplicateInput
                    || (*kind == RedirectKind::DuplicateOutput
                        && (*fd != 2 || target.text() != "1"))
                {
                    return Err(syntax_error(
                        token,
                        "native C1 supports only `2>&1` descriptor duplication",
                        "Use `bash { ... }` or `zsh { ... }` for this descriptor graph",
                    ));
                }
                redirects.push(Redirect {
                    fd: *fd,
                    kind: *kind,
                    path: target.text(),
                    target: target.clone(),
                });
                index += 1;
            }
            TokenKind::Pipe => {
                commands.push(finish_command(
                    &mut words,
                    &mut word_ir,
                    &mut redirects,
                    token,
                )?);
            }
            TokenKind::And | TokenKind::Or | TokenKind::Semicolon => {
                commands.push(finish_command(
                    &mut words,
                    &mut word_ir,
                    &mut redirects,
                    token,
                )?);
                pipelines.push(Pipeline {
                    commands: std::mem::take(&mut commands),
                    background: false,
                });
                connectors.push(match token.kind {
                    TokenKind::And => ListConnector::And,
                    TokenKind::Or => ListConnector::Or,
                    TokenKind::Semicolon => ListConnector::Sequence,
                    _ => unreachable!(),
                });
            }
            TokenKind::Background => {
                commands.push(finish_command(
                    &mut words,
                    &mut word_ir,
                    &mut redirects,
                    token,
                )?);
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

    if tokens.last().is_some_and(|token| {
        matches!(
            token.kind,
            TokenKind::Pipe | TokenKind::And | TokenKind::Or | TokenKind::Semicolon
        )
    }) {
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
        commands.push(finish_command(
            &mut words,
            &mut word_ir,
            &mut redirects,
            &sentinel,
        )?);
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

/// Keep the native grammar deliberately unambiguous: compound dialect syntax cannot silently
/// become a process name. Those forms retain exact Bash/Zsh semantics only inside an explicit
/// bounded island.
fn reject_reserved_dialect_forms(tokens: &[Token]) -> Result<(), CommandSyntaxError> {
    let mut command_position = true;
    let mut redirect_target = false;
    for token in tokens {
        if let TokenKind::Word(word) = &token.kind {
            if contains_unquoted_brace_expansion(word) {
                return Err(CommandSyntaxError {
                    message: format!(
                        "unsupported C1 dialect control form `{}`",
                        word.text()
                    ),
                    start: token.start,
                    end: token.end,
                    help: "Run it as `bash { ... }` or `zsh { ... }`; the bounded reference island preserves the selected dialect's control semantics".to_owned(),
                });
            }
        }
        if redirect_target {
            redirect_target = false;
            continue;
        }
        match &token.kind {
            TokenKind::Redirect { .. } => redirect_target = true,
            TokenKind::Pipe | TokenKind::And | TokenKind::Or | TokenKind::Semicolon => {
                command_position = true;
            }
            TokenKind::Background => command_position = true,
            TokenKind::Word(word) => {
                let value = word.text();
                let reserved = command_position
                    && (matches!(
                        value.as_str(),
                        "for"
                            | "while"
                            | "until"
                            | "if"
                            | "case"
                            | "select"
                            | "function"
                            | "{"
                            | "}"
                    ) || value == "[["
                        || value == "(("
                        || value.ends_with("()"));
                if reserved {
                    return Err(CommandSyntaxError {
                        message: format!("unsupported C1 dialect control form `{value}`"),
                        start: token.start,
                        end: token.end,
                        help: "Run it as `bash { ... }` or `zsh { ... }`; the bounded reference island preserves the selected dialect's control semantics".to_owned(),
                    });
                }
                command_position = false;
            }
        }
    }
    Ok(())
}

/// Brace expansion is intentionally not part of native C1.  Treat only the
/// unquoted shell form as reserved; quoted braces and `${parameter}` remain
/// ordinary literals/parameter expansion inputs for the process layer.
fn contains_unquoted_brace_expansion(word: &Word) -> bool {
    let text = word
        .parts
        .iter()
        .filter(|part| part.quoting == Quoting::Unquoted)
        .map(|part| part.text.as_str())
        .collect::<String>();
    let mut offset = 0;
    while let Some(open) = text[offset..].find('{') {
        let open = offset + open;
        if text[..open].ends_with('$') {
            offset = open + 1;
            continue;
        }
        let content_start = open + 1;
        let Some(close_relative) = text[content_start..].find('}') else {
            return false;
        };
        let content = &text[content_start..content_start + close_relative];
        if !content.is_empty() && (content.contains(',') || content.contains("..")) {
            return true;
        }
        offset = content_start + close_relative + 1;
    }
    false
}

/// Return the expression from a `data` statement.
///
/// The keyword must be followed by whitespace or the end of the line. Callers
/// share this classifier so checking and execution cannot disagree. The returned
/// slice borrows `line`, excludes the keyword and following leading whitespace,
/// and may be empty.
pub fn data_statement_expression(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim_start())
}

/// Validate a line-oriented native Quirl script without executing it.
///
/// `.qrl` is the canonical extension; `.quirl` and `.🌀` are accepted aliases
/// at the CLI and language-service boundaries.
///
/// Command statements use the same compatibility parser as interactive execution. `data`
/// statements are recognized as a separate language island and require a non-empty expression.
/// Every returned span is absolute within `source` and diagnostics retain source order. The
/// checker scans linearly, does not execute source or recurse, and does not impose a byte limit;
/// file-reading callers must reject oversized source before invoking it.
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

fn finish_command(
    words: &mut Vec<String>,
    word_ir: &mut Vec<Word>,
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
        word_ir: std::mem::take(word_ir),
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

struct SubstitutionOpening {
    depth: usize,
    start: usize,
    suspended_quote: Option<(char, usize)>,
}

fn lex_command(input: &str) -> Result<Vec<Token>, CommandSyntaxError> {
    let mut tokens = Vec::new();
    let mut word_start = None;
    let mut quote = None;
    let mut quote_open = None;
    let mut parts = Vec::new();
    let mut fragment = String::new();
    let mut fragment_quoting = Quoting::Unquoted;
    let mut substitution_depth = 0;
    let mut substitution_openings = Vec::new();
    let mut substitution_quote = None;
    let mut substitution_quote_open = None;
    let mut substitution_escaped = false;
    let mut characters = input.char_indices().peekable();

    while let Some((index, character)) = characters.next() {
        if substitution_depth > 0 {
            let quoting = match quote {
                Some('\'') => Quoting::Single,
                Some('"') => Quoting::Double,
                None => Quoting::Unquoted,
                _ => unreachable!(),
            };
            if substitution_escaped {
                substitution_escaped = false;
            } else if character == '\\' && substitution_quote != Some('\'') {
                substitution_escaped = true;
            } else if character == '('
                && fragment.ends_with('$')
                && substitution_quote != Some('\'')
            {
                open_command_substitution(
                    &mut substitution_depth,
                    &mut substitution_openings,
                    &mut substitution_quote,
                    &mut substitution_quote_open,
                    index,
                )?;
            } else if let Some(active) = substitution_quote {
                if character == active {
                    substitution_quote = None;
                    substitution_quote_open = None;
                }
            } else if matches!(character, '\'' | '"') {
                substitution_quote = Some(character);
                substitution_quote_open = Some(index);
            } else {
                match character {
                    '(' => {
                        substitution_depth = substitution_depth.saturating_add(1);
                    }
                    ')' => {
                        if substitution_openings
                            .last()
                            .is_some_and(|opening| opening.depth == substitution_depth)
                        {
                            if let Some(opening) = substitution_openings.pop() {
                                if let Some((active, start)) = opening.suspended_quote {
                                    substitution_quote = Some(active);
                                    substitution_quote_open = Some(start);
                                }
                            }
                        }
                        substitution_depth = substitution_depth.saturating_sub(1);
                    }
                    _ => {}
                }
            }
            append_fragment(
                &mut parts,
                &mut fragment,
                &mut fragment_quoting,
                character,
                quoting,
            );
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            word_start.get_or_insert(index);
            let Some((_, escaped)) = characters.next() else {
                return Err(CommandSyntaxError {
                    message: "command ends with an escape".to_owned(),
                    start: index,
                    end: input.len(),
                    help: "Add the escaped character or remove the trailing backslash".to_owned(),
                });
            };
            append_fragment(
                &mut parts,
                &mut fragment,
                &mut fragment_quoting,
                escaped,
                Quoting::Escaped,
            );
            continue;
        }
        if let Some(active) = quote {
            let quoting = if active == '\'' {
                Quoting::Single
            } else {
                Quoting::Double
            };
            if active != '\'' && character == '(' && fragment.ends_with('$') {
                open_command_substitution(
                    &mut substitution_depth,
                    &mut substitution_openings,
                    &mut substitution_quote,
                    &mut substitution_quote_open,
                    index,
                )?;
                append_fragment(
                    &mut parts,
                    &mut fragment,
                    &mut fragment_quoting,
                    character,
                    quoting,
                );
                continue;
            }
            if character == active {
                push_fragment(&mut parts, &mut fragment, fragment_quoting);
                quote = None;
                quote_open = None;
            } else {
                append_fragment(
                    &mut parts,
                    &mut fragment,
                    &mut fragment_quoting,
                    character,
                    quoting,
                );
            }
            continue;
        }
        if character == '(' && fragment.ends_with('$') {
            open_command_substitution(
                &mut substitution_depth,
                &mut substitution_openings,
                &mut substitution_quote,
                &mut substitution_quote_open,
                index,
            )?;
            append_fragment(
                &mut parts,
                &mut fragment,
                &mut fragment_quoting,
                character,
                Quoting::Unquoted,
            );
            continue;
        }
        if matches!(character, '\'' | '"') {
            word_start.get_or_insert(index);
            push_fragment(&mut parts, &mut fragment, fragment_quoting);
            quote = Some(character);
            quote_open = Some(index);
            continue;
        }
        if character.is_whitespace() {
            push_word(
                &mut tokens,
                &mut parts,
                &mut fragment,
                &mut fragment_quoting,
                &mut word_start,
                index,
            );
            continue;
        }

        let mut redirect_fd = 0;
        if matches!(character, '<' | '>')
            && !fragment.is_empty()
            && parts.is_empty()
            && fragment.chars().all(|value| value.is_ascii_digit())
        {
            redirect_fd = fragment.parse::<u8>().map_err(|_| CommandSyntaxError {
                message: "file descriptor is outside the supported range".to_owned(),
                start: word_start.unwrap_or(index),
                end: index,
                help: "Use a descriptor from 0 through 255".to_owned(),
            })?;
            fragment.clear();
            word_start = None;
        } else if character == '<' {
            redirect_fd = 0;
        } else if character == '>' {
            redirect_fd = 1;
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
            ';' => Some((TokenKind::Semicolon, 1)),
            '<' if characters.peek().is_some_and(|(_, next)| *next == '(') => {
                return Err(CommandSyntaxError {
                    message: "process substitution requires an explicit dialect island".to_owned(),
                    start: index,
                    end: index + 2,
                    help: "Use `bash { ... }` or `zsh { ... }` so the selected dialect owns file-descriptor lifecycle semantics".to_owned(),
                });
            }
            '>' if characters.peek().is_some_and(|(_, next)| *next == '(') => {
                return Err(CommandSyntaxError {
                    message: "process substitution requires an explicit dialect island".to_owned(),
                    start: index,
                    end: index + 2,
                    help: "Use `bash { ... }` or `zsh { ... }` so the selected dialect owns file-descriptor lifecycle semantics".to_owned(),
                });
            }
            '<' if characters.peek().is_some_and(|(_, next)| *next == '<') => {
                characters.next();
                if characters.peek().is_some_and(|(_, next)| *next == '<') {
                    characters.next();
                    Some((
                        TokenKind::Redirect {
                            fd: redirect_fd,
                            kind: RedirectKind::HereString,
                        },
                        3,
                    ))
                } else {
                    return Err(CommandSyntaxError { message: "here-documents require a multiline script parser".to_owned(), start: index, end: index + 2, help: "Use a here-string (`<<< value`) or an explicit `bash { ... }`/`zsh { ... }` island".to_owned() });
                }
            }
            '<' if characters.peek().is_some_and(|(_, next)| *next == '&') => {
                characters.next();
                Some((
                    TokenKind::Redirect {
                        fd: redirect_fd,
                        kind: RedirectKind::DuplicateInput,
                    },
                    2,
                ))
            }
            '>' if characters.peek().is_some_and(|(_, next)| *next == '>') => {
                characters.next();
                Some((
                    TokenKind::Redirect {
                        fd: redirect_fd,
                        kind: RedirectKind::Append,
                    },
                    2,
                ))
            }
            '>' if characters.peek().is_some_and(|(_, next)| *next == '&') => {
                characters.next();
                Some((
                    TokenKind::Redirect {
                        fd: redirect_fd,
                        kind: RedirectKind::DuplicateOutput,
                    },
                    2,
                ))
            }
            '>' => Some((
                TokenKind::Redirect {
                    fd: redirect_fd,
                    kind: RedirectKind::Output,
                },
                1,
            )),
            '<' => Some((
                TokenKind::Redirect {
                    fd: redirect_fd,
                    kind: RedirectKind::Input,
                },
                1,
            )),
            _ => None,
        };
        if let Some((kind, width)) = operator {
            push_word(
                &mut tokens,
                &mut parts,
                &mut fragment,
                &mut fragment_quoting,
                &mut word_start,
                index,
            );
            tokens.push(Token {
                kind,
                start: index,
                end: index + width,
            });
        } else {
            word_start.get_or_insert(index);
            append_fragment(
                &mut parts,
                &mut fragment,
                &mut fragment_quoting,
                character,
                Quoting::Unquoted,
            );
        }
    }
    if let Some(active) = substitution_quote {
        let start = substitution_quote_open.unwrap_or(input.len());
        return Err(CommandSyntaxError {
            message: format!("unclosed {active} quote in command substitution"),
            start,
            end: start + active.len_utf8(),
            help: format!("Close the quote with {active} before closing the command substitution"),
        });
    }
    if substitution_depth > 0 {
        let start = substitution_openings
            .last()
            .map_or(input.len(), |opening| opening.start);
        return Err(CommandSyntaxError {
            message: "unclosed command substitution".to_owned(),
            start,
            end: (start + 2).min(input.len()),
            help: "Close the command substitution with `)`".to_owned(),
        });
    }
    if let Some(active) = quote {
        let start = quote_open.unwrap_or(input.len());
        return Err(CommandSyntaxError {
            message: format!("unclosed {active} quote"),
            start,
            end: start + active.len_utf8(),
            help: format!("Close the quote with {active}"),
        });
    }
    push_word(
        &mut tokens,
        &mut parts,
        &mut fragment,
        &mut fragment_quoting,
        &mut word_start,
        input.len(),
    );
    Ok(tokens)
}

fn open_command_substitution(
    depth: &mut usize,
    openings: &mut Vec<SubstitutionOpening>,
    active_quote: &mut Option<char>,
    active_quote_open: &mut Option<usize>,
    parenthesis_index: usize,
) -> Result<(), CommandSyntaxError> {
    let observed = depth.saturating_add(1);
    if observed > MAX_COMMAND_SUBSTITUTION_DEPTH {
        return Err(CommandSyntaxError {
            message: "command substitution nesting exceeds the supported limit".to_owned(),
            start: parenthesis_index - 1,
            end: parenthesis_index + 1,
            help: format!(
                "Reduce command substitution nesting to {MAX_COMMAND_SUBSTITUTION_DEPTH} levels or fewer"
            ),
        });
    }
    let suspended_quote = active_quote.take().zip(active_quote_open.take());
    *depth = observed;
    openings.push(SubstitutionOpening {
        depth: observed,
        start: parenthesis_index - 1,
        suspended_quote,
    });
    Ok(())
}

fn append_fragment(
    parts: &mut Vec<WordPart>,
    fragment: &mut String,
    quoting: &mut Quoting,
    character: char,
    next_quoting: Quoting,
) {
    if !fragment.is_empty() && *quoting != next_quoting {
        push_fragment(parts, fragment, *quoting);
    }
    *quoting = next_quoting;
    fragment.push(character);
}

fn push_fragment(parts: &mut Vec<WordPart>, fragment: &mut String, quoting: Quoting) {
    if !fragment.is_empty() {
        parts.push(WordPart {
            text: std::mem::take(fragment),
            quoting,
        });
    }
}

fn push_word(
    tokens: &mut Vec<Token>,
    parts: &mut Vec<WordPart>,
    fragment: &mut String,
    quoting: &mut Quoting,
    start: &mut Option<usize>,
    end: usize,
) {
    if let Some(start_index) = start.take() {
        push_fragment(parts, fragment, *quoting);
        tokens.push(Token {
            kind: TokenKind::Word(Word {
                parts: std::mem::take(parts),
            }),
            start: start_index,
            end,
        });
    }
}

/// The grammar and runtime contract currently active in an interactive session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Byte-oriented native commands, pipelines, redirects, and process status.
    #[default]
    Command,
    /// Native typed-data expressions and value pipelines.
    Data,
    /// AI-assisted command and option discovery. Suggestions are never executed directly.
    Natural,
}

impl Mode {
    /// Return the next interactive grammar without mutating session state.
    pub const fn toggled(self) -> Self {
        match self {
            Self::Command => Self::Data,
            Self::Data => Self::Natural,
            Self::Natural => Self::Command,
        }
    }

    /// Return the built-in visible prompt marker for this grammar.
    ///
    /// Hosts may replace this marker through configuration, but should keep mode
    /// identity visible by another accessible means.
    pub const fn prompt(self) -> &'static str {
        match self {
            Self::Command => "❯",
            Self::Data => "▦",
            Self::Natural => "✧",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Command => "normal",
            Self::Data => "data",
            Self::Natural => "ai",
        })
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "normal" | "command" | "cmd" => Ok(Self::Command),
            "data" => Ok(Self::Data),
            "ai" | "natural" | "nl" | "human" => Ok(Self::Natural),
            _ => Err(format!(
                "unknown mode `{value}`; expected `normal`, `data`, or `ai`"
            )),
        }
    }
}

/// A line classified without guessing between two full languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveLine<'a> {
    /// Empty or whitespace-only input.
    Empty,
    /// The mode-independent `exit` or `quit` command.
    Exit,
    /// A valid explicit `mode normal`, `mode data`, or `mode ai` request.
    ChangeMode(Mode),
    /// The explicit `mode toggle` request.
    ToggleMode,
    /// Help request with an optional trimmed topic borrowed from the input.
    Help(Option<&'a str>),
    /// Trimmed source to parse and execute with the native command grammar.
    Command(&'a str),
    /// Trimmed source to evaluate with the native typed-data grammar.
    Data(&'a str),
    /// AI-mode task description to search without direct execution.
    Natural(&'a str),
    /// Expression following the mode-independent `lua ` prefix.
    Lua(&'a str),
}

/// Classify one interactive line using explicit built-ins and the active mode.
///
/// Leading and trailing whitespace is removed before classification. Exit,
/// mode, help, and Lua bridge forms take precedence over the active grammar;
/// all other input is returned for normal, data, or AI-mode handling.
/// An invalid `mode <value>` is deliberately left to
/// the active grammar instead of being silently accepted. Returned string
/// slices borrow `input`, and this function performs no allocation on success.
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
        Mode::Natural => InteractiveLine::Natural(input),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlighting_is_sorted_non_overlapping_and_exhaustive_for_arbitrary_edits() {
        let long = "x".repeat(4096);
        let samples = [
            "",
            "git status",
            "printf '%s' \"$HOME\" | tr a-z A-Z",
            "unterminated 'quote",
            "echo 👨‍👩‍👧‍👦 > ./out",
            "a && b || c; d &",
            "\0\u{1b}\u{85}",
            long.as_str(),
        ];
        for sample in samples {
            let spans = highlight(sample, Mode::Command);
            let mut cursor = 0;
            for span in spans {
                assert_eq!(span.range.start, cursor, "gap or overlap for {sample:?}");
                assert!(span.range.end >= span.range.start);
                assert!(sample.is_char_boundary(span.range.start));
                assert!(sample.is_char_boundary(span.range.end));
                cursor = span.range.end;
            }
            assert_eq!(cursor, sample.len(), "incomplete coverage for {sample:?}");
        }
    }

    #[test]
    fn highlighting_reuses_lexer_operator_and_command_boundaries() {
        let source = "printf '%s' value | tr a-z A-Z";
        let spans = highlight(source, Mode::Command);
        assert!(spans.iter().any(|span| {
            span.kind == HighlightKind::Command && &source[span.range.clone()] == "printf"
        }));
        assert!(spans.iter().any(|span| {
            span.kind == HighlightKind::Operator && &source[span.range.clone()] == "|"
        }));
        assert!(spans.iter().any(|span| {
            span.kind == HighlightKind::Command && &source[span.range.clone()] == "tr"
        }));
    }
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
        fixtures: Vec<serde_json::Value>,
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
        assert_eq!(
            classify(Mode::Natural, "copy a directory preserving permissions"),
            InteractiveLine::Natural("copy a directory preserving permissions")
        );
        assert_eq!(Mode::Command.toggled(), Mode::Data);
        assert_eq!(Mode::Data.toggled(), Mode::Natural);
        assert_eq!(Mode::Natural.toggled(), Mode::Command);
        assert_eq!(Mode::Natural.to_string(), "ai");
        for alias in ["ai", "natural", "nl", "human"] {
            assert_eq!(alias.parse(), Ok(Mode::Natural));
        }
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
                fd: 1,
                kind: RedirectKind::Append,
                path: "out.txt".to_owned(),
                target: Word {
                    parts: vec![WordPart {
                        text: "out.txt".to_owned(),
                        quoting: Quoting::Unquoted
                    }]
                },
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
    fn script_checker_reuses_command_syntax_with_absolute_spans() {
        let source =
            "#!/usr/bin/env -S quirl run\ndata {\"ok\":true}\n  echo $HOME\nprintf ok |\ndata\n";
        let diagnostics = check_script(source);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(&source[diagnostics[0].start..diagnostics[0].end], "|");
        assert_eq!(&source[diagnostics[1].start..diagnostics[1].end], "data");
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
        assert_eq!(matrix.schema_version, 3);
        assert_eq!(matrix.matrix_version, "0.1");
        assert_eq!(matrix.target_contract_version, "1.0");
        assert_eq!(matrix.compatibility_level, "C1-core-unix+C2-runner");
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
                "redirects_and_here_strings",
                "background_marker",
                "lists_and_boolean_connectors",
                "export_assignment",
                "expansions",
                "conditionals_and_control",
                "interactive_dialect_islands",
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
        assert!(deferred.contains("here_documents_and_process_substitution"));
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
    fn quote_aware_ir_preserves_expansion_boundaries() {
        let matrix: CompatibilityMatrix = serde_json::from_str(COMPATIBILITY_MATRIX_JSON).unwrap();
        assert_eq!(matrix.deferred.len(), 1);
        let quoted = parse_command_list("printf '%s' '*.rs $HOME'").unwrap();
        assert_eq!(quoted.pipelines[0].commands[0].words[2], "*.rs $HOME");
        assert_eq!(
            quoted.pipelines[0].commands[0].word_ir[2].parts[0].quoting,
            Quoting::Single
        );

        let graph = parse_command_list("cat <<< \"$HOME\"; echo $((1 + 2)) 2>&1").unwrap();
        assert_eq!(graph.connectors, [ListConnector::Sequence]);
        assert_eq!(
            graph.pipelines[0].commands[0].redirects[0].kind,
            RedirectKind::HereString
        );
        assert_eq!(
            graph.pipelines[1].commands[0].redirects[0].kind,
            RedirectKind::DuplicateOutput
        );
        for fixture in matrix.deferred.iter().flat_map(|feature| &feature.fixtures) {
            let source = fixture
                .get("source")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            let diagnostic = fixture
                .get("diagnostic")
                .and_then(serde_json::Value::as_str)
                .unwrap();
            assert_eq!(parse_command_list(source).unwrap_err().message, diagnostic);
        }
    }

    #[test]
    fn unsupported_descriptor_directions_fail_closed() {
        for source in ["cat 1<input", "printf ok 0>output", "cat 3>output"] {
            let error = parse_command_list(source).unwrap_err();
            assert!(error.message.contains("descriptor"), "{source}: {error}");
            assert!(error.help.contains("bash { ... }") || error.help.contains("descriptor 0"));
        }
    }

    #[test]
    fn dialect_control_forms_fail_closed_with_an_explicit_island_remedy() {
        for source in [
            "for item in a; do echo $item; done",
            "while true; do break; done",
            "until false; do break; done",
            "if true; then echo yes; fi",
            "case value in value) echo yes;; esac",
            "function greeting { echo hello; }",
            "greeting() { echo hello; }",
            "[[ -n value ]]",
            "(( 1 + 1 ))",
            "printf {left,right}",
            "printf {1..3}",
            "2>&1 if true; then echo yes; fi",
        ] {
            let error = parse_command_list(source).unwrap_err();
            assert!(error
                .message
                .starts_with("unsupported C1 dialect control form"));
            assert!(error.help.contains("bash { ... }"));
            assert!(error.help.contains("zsh { ... }"));
        }
    }

    #[test]
    fn double_quoted_substitution_stays_double_quoted_and_invalid_descriptors_fail_closed() {
        let graph = parse_command_list("printf '%s' \"$(printf '*.qrl')\"").unwrap();
        assert!(graph.pipelines[0].commands[0].word_ir[2]
            .parts
            .iter()
            .all(|part| part.quoting == Quoting::Double));
        for source in ["echo nope 3> output", "echo nope 1>&2", "cat 0<&1"] {
            let error = parse_command_list(source).unwrap_err();
            assert!(error.help.contains("bash { ... }"));
        }
    }

    #[test]
    fn quotes_inside_unquoted_substitution_do_not_quote_the_outer_word() {
        let graph = parse_command_list(r#"printf $(printf "%s" "two words")"#).unwrap();
        let word = &graph.pipelines[0].commands[0].word_ir[1];
        assert_eq!(word.parts.len(), 1);
        assert_eq!(word.parts[0].quoting, Quoting::Unquoted);
        assert_eq!(word.parts[0].text, r#"$(printf "%s" "two words")"#);
    }

    #[test]
    fn quoted_and_escaped_parentheses_inside_substitution_do_not_close_the_outer_word() {
        for source in [
            r#"printf $(printf ')')"#,
            r#"printf $(printf \\))"#,
            r#"printf $(printf ")")"#,
        ] {
            let graph = parse_command_list(source).unwrap();
            let word = &graph.pipelines[0].commands[0].word_ir[1];
            assert_eq!(word.parts[0].quoting, Quoting::Unquoted);
            assert!(word.parts[0].text.starts_with("$("));
            assert!(word.parts[0].text.ends_with(')'));
        }
    }

    #[test]
    fn unterminated_substitutions_and_quotes_report_the_opening_utf8_span() {
        for (source, message, start, end) in [
            ("printf $(date", "unclosed command substitution", 7, 9),
            (
                "printf $(printf $(date",
                "unclosed command substitution",
                16,
                18,
            ),
            (
                "printf $(printf \"$(date",
                "unclosed command substitution",
                17,
                19,
            ),
            ("echo prefix'missing", "unclosed ' quote", 11, 12),
            (
                "echo $(printf 'missing",
                "unclosed ' quote in command substitution",
                14,
                15,
            ),
        ] {
            let error = parse_command_list(source).unwrap_err();
            assert_eq!(error.message, message, "{source}");
            assert_eq!((error.start, error.end), (start, end), "{source}");
            assert!(error.help.contains("Close"), "{source}: {}", error.help);
        }
    }

    #[test]
    fn closed_substitutions_return_to_the_surrounding_quote_and_word_states() {
        for source in [
            "printf $(date)",
            "printf $(printf $(date))",
            "printf $(printf \"$(date)\")",
            "printf \"$(date)\" suffix",
            "printf before$(date)after",
        ] {
            assert!(parse_command_list(source).is_ok(), "{source}");
        }
    }

    #[test]
    fn command_substitution_nesting_accepts_the_exact_bound_and_rejects_plus_one() {
        let exact = format!(
            "printf {}true{}",
            "$(".repeat(MAX_COMMAND_SUBSTITUTION_DEPTH),
            ")".repeat(MAX_COMMAND_SUBSTITUTION_DEPTH)
        );
        assert!(parse_command_list(&exact).is_ok());

        let plus_one = format!("printf $({}", &exact["printf ".len()..]);
        let error = parse_command_list(&plus_one).unwrap_err();
        assert!(error.message.contains("nesting"));
        assert!(error
            .help
            .contains(&MAX_COMMAND_SUBSTITUTION_DEPTH.to_string()));
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
