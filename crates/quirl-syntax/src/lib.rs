//! Quirl's deliberately small interaction grammar.

#![cfg_attr(
    test,
    allow(
        dead_code_pub_in_binary,
        reason = "the libtest harness is an executable, but these public items remain library API"
    )
)]

use serde::{Deserialize, Serialize};
use std::{fmt, ops::Range, str::FromStr};

mod program;

pub use program::{
    ALIAS_EXPANSIONS_MAX, ALIAS_VALUE_BYTES_MAX, AliasLookup, AndOrList, CaseArm, CaseClause,
    CommandNode, CompoundCommand, CompoundList, ConditionalBranch, ConnectedPipeline, ForClause,
    FunctionDefinition, IfClause, ListItem, LoopClause, NoAliases, PROGRAM_NESTING_MAX,
    PipelineNode, Program, ProgramError, RedirectedCompound, is_function_name, is_incomplete,
    is_reserved_word, is_shell_name, native_set_allexport, parse_program,
};

/// Versioned, machine-readable evidence for Quirl's current native compatibility subset and
/// the frozen 1.0 C1/C2 disposition.
pub const COMPATIBILITY_MATRIX_JSON: &str = include_str!("../compatibility-v0.1.json");
/// Current serialized version of the native command grammar.
pub const GRAMMAR_PROTOCOL_VERSION: u32 = 4;
/// Schema version of the JSON evidence embedded in [`COMPATIBILITY_MATRIX_JSON`].
pub const COMPATIBILITY_MATRIX_SCHEMA_VERSION: u32 = 3;
/// Canonical structural description used to fingerprint the command grammar.
///
/// Any change to serialized shapes, token semantics, supported expansions, or
/// the referenced compatibility matrix must update this descriptor and the
/// corresponding protocol version.
pub const GRAMMAR_SCHEMA_DESCRIPTOR: &str = "quirl.command-grammar@4{CommandList{deny_unknown;pipelines:array<Pipeline>;connectors:array<and|or|sequence>;invariant:connectors.len+1=pipelines.len};Pipeline{deny_unknown;commands:array<SimpleCommand>;background:bool};SimpleCommand{deny_unknown;words:nonempty-array<string>;word_ir:array<Word>;redirects:array<Redirect>};Word{deny_unknown;parts:nonempty-array<WordPart>};WordPart{deny_unknown;text:string;quoting:unquoted|single|double|escaped};Redirect{deny_unknown;fd:u8;kind:input|output|append|here_string|duplicate_input|duplicate_output;path:string;target:Word};Program{deny_unknown;body:CompoundList};CompoundList{deny_unknown;items:array<ListItem>};ListItem{deny_unknown;and_or:AndOrList;background:bool};AndOrList{deny_unknown;first:PipelineNode;rest:array<ConnectedPipeline>};ConnectedPipeline{deny_unknown;connector:and|or;pipeline:PipelineNode};PipelineNode{deny_unknown;negated:bool;commands:nonempty-array<CommandNode>};CommandNode:simple(SimpleCommand)|compound(CompoundCommand)|redirected_compound(RedirectedCompound)|function_definition(FunctionDefinition);RedirectedCompound{deny_unknown;command:CompoundCommand;redirects:nonempty-array<Redirect>};CompoundCommand:brace_group(CompoundList)|if(IfClause)|for(ForClause)|while(LoopClause)|until(LoopClause)|case(CaseClause);IfClause{deny_unknown;branches:array<ConditionalBranch>;else_body:null|CompoundList};ConditionalBranch{deny_unknown;condition:CompoundList;body:CompoundList};ForClause{deny_unknown;variable:string;words:null|array<Word>;body:CompoundList};LoopClause{deny_unknown;condition:CompoundList;body:CompoundList};CaseClause{deny_unknown;subject:Word;arms:array<CaseArm>};CaseArm{deny_unknown;patterns:nonempty-array<Word>;body:CompoundList};FunctionDefinition{deny_unknown;name:string;body:CompoundCommand;source:string};tokens:word|pipe|and|or|semicolon|double_semicolon|newline|input|output|append|here_string|fd_duplicate|background;comments:hash-at-word-start-to-LF;lines:unquoted-LF-sequences|leading-blank-trailing-LF-ignored|LF-after-pipe-and-or-continues|backslash-LF-removed|quoted-LF-literal|unquoted-CR-whitespace|backslash-CRLF-keeps-escaped-CR-then-separates;expansion:parameter|positional|special|arithmetic|command|pathname|field-splitting;aliases:posix-command-position;compatibility_matrix:quirl-syntax/compatibility-v0.1.json@schema3}";

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
/// Empty quoted fragments are retained because their origin can suppress a
/// leading tilde expansion even when they add no bytes to the word.
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
    /// Duplicate an output descriptor (`>&`); native C1 accepts `2>&1` and `>&2`.
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
    Redirect {
        fd: u8,
        kind: RedirectKind,
    },
    Semicolon,
    /// `;;`, which ends a `case` arm in a compound program.
    DoubleSemicolon,
    Newline,
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
    /// Unquoted reserved word at command position, such as `if`, `done`, `{`, or
    /// `function`. The `in` of a `for` or `case` clause is also a keyword.
    Keyword,
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

/// The word a compound-command keyword is still waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingWord {
    /// No reserved-word construct is waiting for its next word.
    None,
    /// The loop variable of `for` or the subject of `case` comes next; `in` may follow it.
    Subject,
    /// The word after a `for` variable or `case` subject: `in` is a keyword here only.
    AwaitIn,
    /// The name of a `function NAME` definition comes next.
    FunctionName,
}

/// Command-position state threaded through the lexer tokens of one line.
///
/// Keeping this state in one place makes the rules visible: an operator starts a
/// new command, leading keywords such as `if` keep the next word in command
/// position, and trailing keywords such as `done` end the command they close.
#[derive(Debug)]
struct HighlightState {
    command_position: bool,
    redirect_target: bool,
    pending: PendingWord,
}

impl HighlightState {
    const fn new() -> Self {
        Self {
            command_position: true,
            redirect_target: false,
            pending: PendingWord::None,
        }
    }

    fn operator(&mut self) -> HighlightKind {
        self.command_position = true;
        self.pending = PendingWord::None;
        HighlightKind::Operator
    }

    fn redirect(&mut self) -> HighlightKind {
        self.redirect_target = true;
        HighlightKind::Redirect
    }

    fn word(&mut self, raw: &str, word: &Word) -> HighlightKind {
        let plain = plain_unquoted(raw, word);
        match (self.pending, plain) {
            (PendingWord::FunctionName, _) => {
                // The body of `function NAME` starts at command position.
                self.pending = PendingWord::None;
                self.command_position = true;
                return HighlightKind::Command;
            }
            (PendingWord::Subject, _) => {
                self.pending = PendingWord::AwaitIn;
                self.command_position = false;
                return classify_word(raw, word);
            }
            (PendingWord::AwaitIn, Some("in")) => {
                self.pending = PendingWord::None;
                self.command_position = false;
                return HighlightKind::Keyword;
            }
            (PendingWord::AwaitIn, _) => self.pending = PendingWord::None,
            (PendingWord::None, _) => {}
        }
        if self.redirect_target {
            self.redirect_target = false;
            return HighlightKind::PathLike;
        }
        if self.command_position {
            return self.command_word(raw, plain);
        }
        classify_word(raw, word)
    }

    fn command_word(&mut self, raw: &str, plain: Option<&str>) -> HighlightKind {
        match plain {
            Some("if" | "then" | "elif" | "else" | "do" | "while" | "until" | "!" | "{") => {
                // The next word is still a command, as in `if git status`.
                HighlightKind::Keyword
            }
            Some("fi" | "done" | "esac" | "}") => {
                self.command_position = false;
                HighlightKind::Keyword
            }
            Some("for" | "case") => {
                self.command_position = false;
                self.pending = PendingWord::Subject;
                HighlightKind::Keyword
            }
            Some("function") => {
                self.command_position = false;
                self.pending = PendingWord::FunctionName;
                HighlightKind::Keyword
            }
            _ => {
                self.command_position = false;
                // `name()` opens a function body, which starts at command position.
                if raw.strip_suffix("()").is_some_and(|name| !name.is_empty()) {
                    self.command_position = true;
                }
                HighlightKind::Command
            }
        }
    }
}

/// Returns the raw text when `word` is one unquoted fragment, which is the only form
/// that can be a reserved word.
fn plain_unquoted<'a>(raw: &'a str, word: &Word) -> Option<&'a str> {
    match word.parts.as_slice() {
        [part] if part.quoting == Quoting::Unquoted => Some(raw),
        _ => None,
    }
}

fn highlight_tokens(line: &str, tokens: &[Token]) -> Vec<HighlightSpan> {
    let mut classified = Vec::with_capacity(tokens.len());
    let mut state = HighlightState::new();
    for token in tokens {
        let raw = line.get(token.start..token.end).unwrap_or_default();
        let kind = match &token.kind {
            TokenKind::Pipe
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Semicolon
            | TokenKind::DoubleSemicolon
            | TokenKind::Newline
            | TokenKind::Background => state.operator(),
            TokenKind::Redirect { .. } => state.redirect(),
            TokenKind::Word(word) => state.word(raw, word),
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
    if let [part] = word.parts.as_slice() {
        match part.quoting {
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
/// Unquoted line feeds separate commands. Leading, blank, and trailing lines
/// are ignored; line feeds after `|`, `&&`, or `||` continue that operation.
/// Quoted line feeds remain literal and backslash-line-feed pairs disappear.
/// A carriage return outside quotes is whitespace, so CRLF input also separates
/// commands; a backslash before CRLF escapes its CR rather than continuing LF.
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
        let Some(token) = tokens.get(index) else {
            break;
        };
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
                let Some(next) = tokens.get(index.saturating_add(1)) else {
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
                        && !is_standard_duplication(*fd, &target.text()))
                {
                    return Err(syntax_error(
                        token,
                        "native C1 supports only `2>&1` and `>&2` descriptor duplication",
                        "Use `bash { ... }` or `zsh { ... }` for this descriptor graph",
                    ));
                }
                redirects.push(Redirect {
                    fd: *fd,
                    kind: *kind,
                    path: target.text(),
                    target: target.clone(),
                });
                index = index.saturating_add(1);
            }
            TokenKind::Pipe => {
                commands.push(finish_command(
                    &mut words,
                    &mut word_ir,
                    &mut redirects,
                    token,
                )?);
            }
            TokenKind::And | TokenKind::Or | TokenKind::Semicolon | TokenKind::Newline => {
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
                    TokenKind::Semicolon | TokenKind::Newline => ListConnector::Sequence,
                    _ => {
                        return Err(syntax_error(
                            token,
                            "invalid command-list connector",
                            "Report this parser state as a Quirl defect",
                        ));
                    }
                });
            }
            TokenKind::DoubleSemicolon => {
                return Err(syntax_error(
                    token,
                    "`;;` only ends a `case` arm",
                    "Use a single `;` to separate commands",
                ));
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
                if let Some(next) = tokens.get(index.saturating_add(1)) {
                    return Err(syntax_error(
                        next,
                        "background marker must end a command list",
                        "Submit the background command separately, then enter the next command",
                    ));
                }
            }
        }
        index = index.saturating_add(1);
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
    debug_assert_eq!(pipelines.len(), connectors.len().saturating_add(1));
    Ok(CommandList {
        pipelines,
        connectors,
    })
}

/// A Bash/Zsh form outside the native grammar, found by [`dialect_control_form`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialectControlForm {
    /// Source text of the reserved word or brace-expansion word.
    pub text: String,
    /// Inclusive start byte offset in the input.
    pub start: usize,
    /// Exclusive end byte offset in the input.
    pub end: usize,
    /// Whether the form defines a shell function (`name()` or `function`),
    /// which cannot outlive the shell that runs it.
    pub defines_function: bool,
    /// Why the native grammar leaves this form to a reference shell.
    pub reason: &'static str,
}

/// Return the first form in `input` that Quirl's native program grammar
/// leaves to a reference shell, such as `[[`, a subshell, brace expansion, or a
/// loop used as a pipeline stage. Natively executable control flow such as
/// `for`, `if`, `case`, and function definitions returns `None`, as does input
/// that is incomplete or invalid. Aliases are not applied.
///
/// [`DialectControlForm::defines_function`] is set when the input also
/// defines a function anywhere, since that function could not outlive the
/// reference shell.
pub fn dialect_control_form(input: &str) -> Option<DialectControlForm> {
    let Err(ProgramError::Unsupported(mut form)) = parse_program(input, &NoAliases) else {
        return None;
    };
    form.defines_function |= lex_command(input)
        .ok()
        .and_then(|tokens| first_dialect_form(&tokens))
        .is_some_and(|first| first.defines_function)
        || defines_function_anywhere(input);
    Some(form)
}

/// Whether any command-position word in `input` starts a function definition.
fn defines_function_anywhere(input: &str) -> bool {
    let Ok(tokens) = lex_command(input) else {
        return false;
    };
    let mut command_position = true;
    let mut previous_function_name = false;
    for token in &tokens {
        match &token.kind {
            TokenKind::Word(word) => {
                let text = word.text();
                if previous_function_name && text == "()" {
                    return true;
                }
                if command_position
                    && (text == "function"
                        || text
                            .strip_suffix("()")
                            .is_some_and(program::is_function_name))
                {
                    return true;
                }
                previous_function_name = command_position && program::is_function_name(&text);
                command_position = is_reserved_word(&text) && text != "in";
            }
            TokenKind::Redirect { .. } => {
                previous_function_name = false;
                command_position = false;
            }
            _ => {
                previous_function_name = false;
                command_position = true;
            }
        }
    }
    false
}

/// Keep the native grammar deliberately unambiguous: compound dialect syntax cannot silently
/// become a process name. Those forms retain exact shell semantics in a reference-shell island.
fn reject_reserved_dialect_forms(tokens: &[Token]) -> Result<(), CommandSyntaxError> {
    match first_dialect_form(tokens) {
        Some(form) => Err(CommandSyntaxError {
            message: format!("unsupported C1 dialect control form `{}`", form.text),
            start: form.start,
            end: form.end,
            help: "Run it as `bash { ... }` or `zsh { ... }`; the bounded reference island preserves the selected dialect's control semantics".to_owned(),
        }),
        None => Ok(()),
    }
}

fn first_dialect_form(tokens: &[Token]) -> Option<DialectControlForm> {
    let mut command_position = true;
    let mut redirect_target = false;
    for token in tokens {
        if let TokenKind::Word(word) = &token.kind
            && contains_unquoted_brace_expansion(word)
        {
            return Some(DialectControlForm {
                text: word.text(),
                start: token.start,
                end: token.end,
                defines_function: false,
                reason: "brace expansion is Bash and Zsh syntax",
            });
        }
        if redirect_target {
            redirect_target = false;
            continue;
        }
        match &token.kind {
            TokenKind::Redirect { .. } => redirect_target = true,
            TokenKind::Pipe
            | TokenKind::And
            | TokenKind::Or
            | TokenKind::Semicolon
            | TokenKind::DoubleSemicolon
            | TokenKind::Newline
            | TokenKind::Background => command_position = true,
            TokenKind::Word(word) => {
                let value = word.text();
                let defines_function = value == "function" || value.ends_with("()");
                let reserved = command_position
                    && (defines_function
                        || matches!(
                            value.as_str(),
                            "for"
                                | "while"
                                | "until"
                                | "if"
                                | "case"
                                | "select"
                                | "{"
                                | "}"
                                | "[["
                                | "(("
                        ));
                if reserved {
                    return Some(DialectControlForm {
                        text: value,
                        start: token.start,
                        end: token.end,
                        defines_function,
                        reason: "compound commands need the compound program grammar",
                    });
                }
                command_position = false;
            }
        }
    }
    None
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
    let mut offset = 0_usize;
    while let Some(open_relative) = text.get(offset..).and_then(|tail| tail.find('{')) {
        let Some(open) = offset.checked_add(open_relative) else {
            return false;
        };
        if text.get(..open).is_some_and(|prefix| prefix.ends_with('$')) {
            offset = open.saturating_add(1);
            continue;
        }
        let content_start = open.saturating_add(1);
        let Some(close_relative) = text.get(content_start..).and_then(|tail| tail.find('}')) else {
            return false;
        };
        let content_end = content_start.saturating_add(close_relative);
        let Some(content) = text.get(content_start..content_end) else {
            return false;
        };
        if !content.is_empty() && (content.contains(',') || content.contains("..")) {
            return true;
        }
        offset = content_end.saturating_add(1);
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

/// Largest statement, in bytes of source text, that [`split_script_statements`] accepts.
///
/// Joining re-checks the growing statement after each added line, so the
/// work for one statement grows with lines × bytes. These bounds keep the
/// worst case near 64 MiB of lexing, about a tenth of a second.
pub const SCRIPT_STATEMENT_BYTES_MAX: usize = 64 * 1024;
/// Most physical lines one script statement may span.
pub const SCRIPT_STATEMENT_LINES_MAX: usize = 2_000;

/// Bounds applied while splitting one script. Tests lower them so oversize cases stay cheap.
#[derive(Debug, Clone, Copy)]
struct ScriptStatementLimits {
    bytes_max: usize,
    lines_max: usize,
}

impl ScriptStatementLimits {
    const DEFAULT: Self = Self {
        bytes_max: SCRIPT_STATEMENT_BYTES_MAX,
        lines_max: SCRIPT_STATEMENT_LINES_MAX,
    };
}

/// One complete statement of a native `.qrl` script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptStatement<'a> {
    /// Source text from the first non-blank byte through the last non-blank byte, including
    /// interior line feeds and indentation.
    pub text: &'a str,
    /// Absolute half-open byte range of `text` within the script.
    pub span: Range<usize>,
}

/// The complete statements of a script and the diagnostics found while splitting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptSplit<'a> {
    /// Complete statements in source order.
    pub statements: Vec<ScriptStatement<'a>>,
    /// Unterminated or oversized statements in source order. Their statements are omitted.
    pub diagnostics: Vec<CommandSyntaxError>,
    /// Whether splitting stopped at a statement bound. The last diagnostic then reports that bound
    /// and no later statement is returned, because the oversized statement's extent, and so the
    /// start of the next statement, is unknown.
    pub limit_exceeded: bool,
}

/// Split a native `.qrl` script into complete statements without validating command syntax.
///
/// The shebang, blank lines, and `#` comment lines are skipped. A `data <expr>` line is one
/// statement. Any other line starts a command statement, and following lines are joined while the
/// text still ends inside a construct, so `for ...; do` through `done` is one statement. A `data`
/// line never joins a pending command: the pending command is then reported as unterminated if it is
/// still incomplete. An unterminated statement is a diagnostic derived from the parser's
/// end-of-input error, pointing at that error's span or, when it has none, at the statement's last
/// token.
///
/// A statement longer than [`SCRIPT_STATEMENT_BYTES_MAX`] bytes or
/// [`SCRIPT_STATEMENT_LINES_MAX`] lines is a diagnostic, and splitting stops there. Joining reparses
/// the growing statement, so one statement costs work quadratic in its line count; the bounds keep
/// that finite. Every span is absolute within `source`, and nothing is executed.
pub fn split_script_statements(source: &str) -> ScriptSplit<'_> {
    split_script_statements_with_limits(source, ScriptStatementLimits::DEFAULT)
}

fn split_script_statements_with_limits(
    source: &str,
    limits: ScriptStatementLimits,
) -> ScriptSplit<'_> {
    let lines = script_lines(source);
    let mut split = ScriptSplit {
        statements: Vec::new(),
        diagnostics: Vec::new(),
        limit_exceeded: false,
    };
    let mut index = 0_usize;
    while let Some(line) = lines.get(index) {
        // The shebang is a `#` line, so it is skipped with the other comments.
        let trimmed = line.text.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index = index.saturating_add(1);
            continue;
        }
        let Some(joined) = join_statement(source, &lines, index, limits) else {
            break;
        };
        match joined {
            Joined::Complete { span, next } => {
                split.statements.push(ScriptStatement {
                    text: source.get(span.clone()).unwrap_or_default(),
                    span,
                });
                index = next;
            }
            Joined::Unterminated { diagnostic, next } => {
                split.diagnostics.push(diagnostic);
                index = next;
            }
            Joined::Oversized(diagnostic) => {
                // Later lines cannot be attributed to statements once a boundary is unknown.
                split.diagnostics.push(diagnostic);
                split.limit_exceeded = true;
                break;
            }
        }
    }
    split
}

/// One physical line of a script.
struct ScriptLine<'a> {
    /// Absolute byte offset of the line's first byte.
    start: usize,
    /// Line text without its line feed.
    text: &'a str,
}

fn script_lines(source: &str) -> Vec<ScriptLine<'_>> {
    let mut start = 0_usize;
    source
        .split_inclusive('\n')
        .map(|raw| {
            let line = ScriptLine {
                start,
                text: raw.strip_suffix('\n').unwrap_or(raw),
            };
            start = start.saturating_add(raw.len());
            line
        })
        .collect()
}

/// The outcome of joining physical lines into one statement.
enum Joined {
    /// A complete statement spanning `span`; splitting resumes at line `next`.
    Complete { span: Range<usize>, next: usize },
    /// An incomplete statement. Its statement is omitted and splitting resumes at line `next`.
    Unterminated {
        diagnostic: CommandSyntaxError,
        next: usize,
    },
    /// A statement over a bound. The diagnostic points at its first line.
    Oversized(CommandSyntaxError),
}

/// Join the physical lines of the statement that begins at line `first`.
///
/// Returns `None` only when `first` is out of range. A `data` line is complete on its own and is
/// never extended. A command line is extended one line at a time while it is incomplete, and
/// extension stops before a `data` line.
fn join_statement(
    source: &str,
    lines: &[ScriptLine<'_>],
    first: usize,
    limits: ScriptStatementLimits,
) -> Option<Joined> {
    let first_line = lines.get(first)?;
    let head = statement_span(first_line, first_line);
    let joinable = data_statement_expression(first_line.text.trim()).is_none();
    let mut last = first;
    loop {
        let span = statement_span(first_line, lines.get(last)?);
        if span.len() > limits.bytes_max {
            return Some(Joined::Oversized(bytes_limit_diagnostic(
                &head,
                span.len(),
                limits,
            )));
        }
        let next = last.saturating_add(1);
        if !joinable {
            return Some(Joined::Complete { span, next });
        }
        let text = source.get(span.clone()).unwrap_or_default();
        let Err(ProgramError::Incomplete(error)) = parse_program(text, &NoAliases) else {
            return Some(Joined::Complete { span, next });
        };
        let Some(following) = lines.get(next) else {
            return Some(Joined::Unterminated {
                diagnostic: unterminated_diagnostic(error, &span, text),
                next,
            });
        };
        if data_statement_expression(following.text.trim()).is_some() {
            return Some(Joined::Unterminated {
                diagnostic: unterminated_diagnostic(error, &span, text),
                next,
            });
        }
        // Adding `following` would make the statement `next - first + 1` lines long.
        if next.saturating_sub(first) >= limits.lines_max {
            return Some(Joined::Oversized(lines_limit_diagnostic(&head, limits)));
        }
        last = next;
    }
}

/// The span from the first non-blank byte of `first` through the last non-blank byte of `last`.
fn statement_span(first: &ScriptLine<'_>, last: &ScriptLine<'_>) -> Range<usize> {
    let indentation = first
        .text
        .len()
        .saturating_sub(first.text.trim_start().len());
    let start = first.start.saturating_add(indentation);
    let end = last
        .start
        .saturating_add(last.text.trim_end().len())
        .max(start);
    start..end
}

fn bytes_limit_diagnostic(
    head: &Range<usize>,
    observed: usize,
    limits: ScriptStatementLimits,
) -> CommandSyntaxError {
    CommandSyntaxError {
        message: format!(
            "script statement is {observed} bytes, over the {}-byte limit",
            limits.bytes_max
        ),
        start: head.start,
        end: head.end,
        help: STATEMENT_SIZE_HELP.to_owned(),
    }
}

fn lines_limit_diagnostic(
    head: &Range<usize>,
    limits: ScriptStatementLimits,
) -> CommandSyntaxError {
    CommandSyntaxError {
        message: format!(
            "script statement spans more than {} lines",
            limits.lines_max
        ),
        start: head.start,
        end: head.end,
        help: STATEMENT_SIZE_HELP.to_owned(),
    }
}

const STATEMENT_SIZE_HELP: &str =
    "Split the statement, for example by moving a long loop body into a function";

/// Derive the diagnostic for a statement that ends inside a construct.
///
/// `text` is the statement without trailing whitespace, and `span` is its absolute range. An
/// end-of-input error has no width, so the diagnostic then points at the statement's last token.
fn unterminated_diagnostic(
    error: CommandSyntaxError,
    span: &Range<usize>,
    text: &str,
) -> CommandSyntaxError {
    let trimmed = text.trim_end();
    let end = span.start.saturating_add(trimmed.len());
    let (start, end) = if error.start < error.end {
        (
            span.start.saturating_add(error.start),
            span.start.saturating_add(error.end),
        )
    } else {
        let width = trimmed.chars().next_back().map_or(0, char::len_utf8);
        (end.saturating_sub(width), end)
    };
    CommandSyntaxError {
        message: error.message,
        start,
        end,
        help: error.help,
    }
}

/// Validate a line-oriented native Quirl script without executing it.
///
/// `.qrl` is the canonical extension; `.quirl` and `.🌀` are accepted aliases
/// at the CLI and language-service boundaries.
///
/// Statements come from [`split_script_statements`]. Each command statement is checked with the
/// native program grammar, and a `data` statement must have an expression. Every returned span is
/// absolute within `source`, and diagnostics are in source order. The checker does not execute
/// source or recurse. It does not bound the source size, so file-reading callers must reject
/// oversized source before invoking it; each statement is bounded by the splitter.
pub fn check_script(source: &str) -> Vec<CommandSyntaxError> {
    let split = split_script_statements(source);
    let mut diagnostics = split.diagnostics;
    for statement in &split.statements {
        match data_statement_expression(statement.text) {
            Some("") => diagnostics.push(CommandSyntaxError {
                message: "data statement requires an expression".to_owned(),
                start: statement.span.start,
                end: statement.span.end,
                help: "Add a structured-data expression after `data`".to_owned(),
            }),
            Some(_) => {}
            None => diagnostics.extend(
                check_command_statement(statement.text)
                    .map(|error| rebase_diagnostic(error, statement.span.start)),
            ),
        }
    }
    diagnostics.sort_by_key(|diagnostic| diagnostic.start);
    diagnostics
}

/// Validate one complete command statement with the native program grammar.
///
/// Returns the statement's diagnostic with offsets relative to `statement`; callers that know the
/// statement's position in a larger script rebase them. An unsupported form is a diagnostic here,
/// because `.qrl` scripts have no reference-shell fallback.
pub fn check_command_statement(statement: &str) -> Option<CommandSyntaxError> {
    match parse_program(statement, &NoAliases) {
        Ok(_) => None,
        // A complete statement cannot end inside a construct, so an incomplete error only occurs
        // for partial text, and it is reported like any other syntax error.
        Err(ProgramError::Incomplete(error) | ProgramError::Syntax(error)) => Some(error),
        Err(ProgramError::Unsupported(form)) => Some(CommandSyntaxError {
            message: format!(
                "`{}` is not supported in .qrl scripts: {}",
                form.text, form.reason
            ),
            start: form.start,
            end: form.end,
            help: "Run it with a reference shell: wrap it in `bash { ... }` or run the file with `quirl run --lang bash`".to_owned(),
        }),
    }
}

fn rebase_diagnostic(mut diagnostic: CommandSyntaxError, base: usize) -> CommandSyntaxError {
    diagnostic.start = diagnostic.start.saturating_add(base);
    diagnostic.end = diagnostic.end.saturating_add(base);
    diagnostic
}

/// Whether `fd>&target` is one of the two native duplications: `2>&1`
/// (standard error to standard output) or `1>&2` (the reverse).
fn is_standard_duplication(fd: u8, target: &str) -> bool {
    matches!((fd, target), (2, "1") | (1, "2"))
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
    let mut quote_parts_start = 0;
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
                _ => {
                    return Err(CommandSyntaxError {
                        message: "invalid substitution quote state".to_owned(),
                        start: index,
                        end: index.saturating_add(character.len_utf8()),
                        help: "Report this parser state as a Quirl defect".to_owned(),
                    });
                }
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
                            && let Some(opening) = substitution_openings.pop()
                            && let Some((active, start)) = opening.suspended_quote
                        {
                            substitution_quote = Some(active);
                            substitution_quote_open = Some(start);
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
        // Inside double quotes POSIX gives a backslash meaning only before
        // `$`, a backtick, `"`, `\`, or a line feed. Before anything else it
        // is literal text, and the next character is lexed normally.
        if character == '\\'
            && quote == Some('"')
            && !characters
                .peek()
                .is_some_and(|&(_, next)| matches!(next, '$' | '`' | '"' | '\\' | '\n'))
        {
            append_fragment(
                &mut parts,
                &mut fragment,
                &mut fragment_quoting,
                character,
                Quoting::Double,
            );
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            let Some((_, escaped)) = characters.next() else {
                return Err(CommandSyntaxError {
                    message: "command ends with an escape".to_owned(),
                    start: index,
                    end: input.len(),
                    help: "Add the escaped character or remove the trailing backslash".to_owned(),
                });
            };
            // A continued physical line contributes no argument bytes. Do
            // not start an empty word when the continuation stands alone.
            if escaped == '\n' {
                continue;
            }
            word_start.get_or_insert(index);
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
                // Empty quotes still suppress leading tilde expansion. Keep
                // their origin even though they contribute no argument bytes.
                if fragment.is_empty() && parts.len() == quote_parts_start {
                    parts.push(WordPart {
                        text: String::new(),
                        quoting,
                    });
                }
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
            quote_parts_start = parts.len();
            quote = Some(character);
            quote_open = Some(index);
            continue;
        }
        // A `#` that begins a word starts a comment that runs to the end of
        // the physical line, as in POSIX `sh` and interactive Bash. The line
        // feed itself still separates commands.
        if character == '#' && word_start.is_none() && fragment.is_empty() && parts.is_empty() {
            while characters.peek().is_some_and(|(_, next)| *next != '\n') {
                characters.next();
            }
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
            if character == '\n' {
                push_line_separator(&mut tokens, index);
            }
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
            ';' if characters.peek().is_some_and(|(_, next)| *next == ';') => {
                characters.next();
                Some((TokenKind::DoubleSemicolon, 2))
            }
            ';' => Some((TokenKind::Semicolon, 1)),
            '<' if characters.peek().is_some_and(|(_, next)| *next == '(') => {
                return Err(CommandSyntaxError {
                    message: "process substitution requires an explicit dialect island".to_owned(),
                    start: index,
                    end: index.saturating_add(2),
                    help: "Use `bash { ... }` or `zsh { ... }` so the selected dialect owns file-descriptor lifecycle semantics".to_owned(),
                });
            }
            '>' if characters.peek().is_some_and(|(_, next)| *next == '(') => {
                return Err(CommandSyntaxError {
                    message: "process substitution requires an explicit dialect island".to_owned(),
                    start: index,
                    end: index.saturating_add(2),
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
                    return Err(CommandSyntaxError { message: "here-documents require a multiline script parser".to_owned(), start: index, end: index.saturating_add(2), help: "Use a here-string (`<<< value`) or an explicit `bash { ... }`/`zsh { ... }` island".to_owned() });
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
                end: index.saturating_add(width),
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
            end: start.saturating_add(active.len_utf8()),
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
            end: start.saturating_add(2).min(input.len()),
            help: "Close the command substitution with `)`".to_owned(),
        });
    }
    if let Some(active) = quote {
        let start = quote_open.unwrap_or(input.len());
        return Err(CommandSyntaxError {
            message: format!("unclosed {active} quote"),
            start,
            end: start.saturating_add(active.len_utf8()),
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
    // A final physical line terminator completes the preceding command rather
    // than asking for another one. Explicit trailing semicolons stay invalid.
    if tokens
        .last()
        .is_some_and(|token| token.kind == TokenKind::Newline)
    {
        tokens.pop();
    }
    Ok(tokens)
}

fn push_line_separator(tokens: &mut Vec<Token>, index: usize) {
    // Blank lines and newlines following a continuation operator add no graph
    // node. After a redirect, retain the delimiter so its missing operand is
    // rejected before any command can execute.
    if tokens
        .last()
        .is_some_and(|token| matches!(token.kind, TokenKind::Word(_) | TokenKind::Redirect { .. }))
    {
        tokens.push(Token {
            kind: TokenKind::Newline,
            start: index,
            end: index.saturating_add(1),
        });
    }
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
            start: parenthesis_index.saturating_sub(1),
            end: parenthesis_index.saturating_add(1),
            help: format!(
                "Reduce command substitution nesting to {MAX_COMMAND_SUBSTITUTION_DEPTH} levels or fewer"
            ),
        });
    }
    let suspended_quote = active_quote.take().zip(active_quote_open.take());
    *depth = observed;
    openings.push(SubstitutionOpening {
        depth: observed,
        start: parenthesis_index.saturating_sub(1),
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
    /// The mode-independent `clear` command, which empties the screen.
    Clear,
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
    if input == "clear" {
        return InteractiveLine::Clear;
    }
    if input == "mode toggle" {
        return InteractiveLine::ToggleMode;
    }
    if let Some(value) = input.strip_prefix("mode ")
        && let Ok(mode) = value.trim().parse()
    {
        return InteractiveLine::ChangeMode(mode);
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
            "for x in a b; do echo $x; done",
            "if git status; then\necho ok\nelif false; then echo no\nelse echo else; fi",
            "case $x in a) echo a;; *) echo b;; esac",
            "function greet { echo hi; }",
            "greet() {\n  echo hi\n}",
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
            span.kind == HighlightKind::Command && source.get(span.range.clone()) == Some("printf")
        }));
        assert!(spans.iter().any(|span| {
            span.kind == HighlightKind::Operator && source.get(span.range.clone()) == Some("|")
        }));
        assert!(spans.iter().any(|span| {
            span.kind == HighlightKind::Command && source.get(span.range.clone()) == Some("tr")
        }));
    }

    /// Returns each non-blank span's text with its kind, in source order.
    fn word_kinds(source: &str) -> Vec<(&str, HighlightKind)> {
        highlight(source, Mode::Command)
            .into_iter()
            .filter_map(|span| {
                let text = source.get(span.range.clone())?;
                (!text.trim().is_empty()).then_some((text, span.kind))
            })
            .collect()
    }

    #[test]
    fn reserved_words_at_command_position_are_keywords() {
        use HighlightKind::{Argument, Command, Keyword, Operator};
        assert_eq!(
            word_kinds("if git status; then echo ok; fi"),
            vec![
                ("if", Keyword),
                ("git", Command),
                ("status", Argument),
                (";", Operator),
                ("then", Keyword),
                ("echo", Command),
                ("ok", Argument),
                (";", Operator),
                ("fi", Keyword),
            ]
        );
    }

    #[test]
    fn loop_variable_and_in_are_not_commands() {
        use HighlightKind::{Argument, Command, Keyword, Operator};
        assert_eq!(
            word_kinds("for x in a b; do echo $x; done"),
            vec![
                ("for", Keyword),
                ("x", Argument),
                ("in", Keyword),
                ("a", Argument),
                ("b", Argument),
                (";", Operator),
                ("do", Keyword),
                ("echo", Command),
                ("$x", HighlightKind::Expansion),
                (";", Operator),
                ("done", Keyword),
            ]
        );
    }

    #[test]
    fn case_subject_is_followed_by_an_in_keyword() {
        let kinds = word_kinds("case $x in");
        assert_eq!(kinds[0], ("case", HighlightKind::Keyword));
        assert_eq!(kinds[2], ("in", HighlightKind::Keyword));
    }

    #[test]
    fn function_names_and_name_parens_are_commands_before_a_body_keyword() {
        use HighlightKind::{Argument, Command, Keyword, Operator};
        assert_eq!(
            word_kinds("function greet { echo hi; }"),
            vec![
                ("function", Keyword),
                ("greet", Command),
                ("{", Keyword),
                ("echo", Command),
                ("hi", Argument),
                (";", Operator),
                ("}", Keyword),
            ]
        );
        let kinds = word_kinds("greet() { echo hi; }");
        assert_eq!(kinds[0], ("greet()", Command));
        assert_eq!(kinds[1], ("{", Keyword));
    }

    #[test]
    fn reserved_words_outside_command_position_or_quoted_are_plain_text() {
        use HighlightKind::{Argument, Command, StringSingle};
        assert_eq!(
            word_kinds("echo if 'done'"),
            vec![
                ("echo", Command),
                ("if", Argument),
                ("'done'", StringSingle),
            ]
        );
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
    fn physical_lines_form_sequences_without_empty_commands() {
        let graph = parse_command_list("\n \n printf one\n\n printf two\r\n").unwrap();
        assert_eq!(graph.pipelines.len(), 2);
        assert_eq!(graph.connectors, [ListConnector::Sequence]);
        assert_eq!(graph.pipelines[0].commands[0].words, ["printf", "one"]);
        assert_eq!(graph.pipelines[1].commands[0].words, ["printf", "two"]);
        assert!(parse_command_list("\n\r\n\t").unwrap().pipelines.is_empty());
    }

    #[test]
    fn physical_lines_after_boolean_and_pipe_operators_continue_commands() {
        let graph = parse_command_list("printf a |\n\n cat &&\n true ||\n printf b\n").unwrap();
        assert_eq!(graph.pipelines.len(), 3);
        assert_eq!(graph.pipelines[0].commands.len(), 2);
        assert_eq!(graph.connectors, [ListConnector::And, ListConnector::Or]);
        for source in [
            "printf a |\n",
            "printf a &&\n",
            "printf a ||\n",
            "printf a;\n",
        ] {
            assert_eq!(
                parse_command_list(source).unwrap_err().message,
                "command list ends with a control operator"
            );
        }
    }

    #[test]
    fn quoted_newlines_stay_literal_and_escaped_newlines_add_no_argument() {
        let graph = parse_command_list("printf 'a\nb' \"c\nd\" e\\\nf \\\n tail").unwrap();
        assert_eq!(graph.pipelines.len(), 1);
        assert_eq!(
            graph.pipelines[0].commands[0].words,
            ["printf", "a\nb", "c\nd", "ef", "tail"]
        );
        assert!(parse_command_list("\\\n").unwrap().pipelines.is_empty());
        let graph = parse_command_list("printf \"a\\\nb\" \"\\\n\"").unwrap();
        assert_eq!(graph.pipelines[0].commands[0].words, ["printf", "ab", ""]);
    }

    #[test]
    fn newlines_do_not_supply_redirect_operands_or_extend_background_lists() {
        let source = "printf a >\n out";
        let error = parse_command_list(source).unwrap_err();
        assert_eq!(error.message, "redirection path must be a word");
        assert_eq!(source.get(error.start..error.end), Some("\n"));
        let error = parse_command_list("printf a &\nprintf b").unwrap_err();
        assert_eq!(error.message, "background marker must end a command list");
        assert!(parse_command_list("printf a &\n").unwrap().pipelines[0].background);
    }

    #[test]
    fn carriage_returns_and_hash_words_keep_quote_semantics_and_comments_end_lines() {
        let graph =
            parse_command_list("printf a\\\r\nprintf '#quoted' word#suffix #comment\n").unwrap();
        assert_eq!(graph.pipelines.len(), 2);
        assert_eq!(graph.pipelines[0].commands[0].words, ["printf", "a\r"]);
        assert_eq!(
            graph.pipelines[1].commands[0].words,
            ["printf", "#quoted", "word#suffix"]
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
    fn fixed_width_token_transitions_report_the_following_token() {
        let redirect = parse_command_list("printf ok > | next").unwrap_err();
        assert_eq!(redirect.message, "redirection path must be a word");
        assert_eq!(
            "printf ok > | next".get(redirect.start..redirect.end),
            Some("|")
        );

        let background = parse_command_list("sleep 1 & echo after").unwrap_err();
        assert_eq!(
            background.message,
            "background marker must end a command list"
        );
        assert_eq!(
            "sleep 1 & echo after".get(background.start..background.end),
            Some("echo")
        );
    }

    #[test]
    fn script_checker_reuses_command_syntax_with_absolute_spans() {
        let source =
            "#!/usr/bin/env -S quirl run\ndata {\"ok\":true}\n  echo $HOME\nprintf ok |\ndata\n";
        let diagnostics = check_script(source);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(
            source.get(diagnostics[0].start..diagnostics[0].end),
            Some("|")
        );
        assert_eq!(
            source.get(diagnostics[1].start..diagnostics[1].end),
            Some("data")
        );
    }

    #[test]
    fn script_splitter_joins_a_multi_line_loop_into_one_statement() {
        let source = "echo before\nfor name in a b; do\n  echo \"$name\"\ndone\necho after\n";
        let split = split_script_statements(source);
        assert!(split.diagnostics.is_empty());
        assert!(!split.limit_exceeded);
        let texts = split
            .statements
            .iter()
            .map(|statement| statement.text)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "echo before",
                "for name in a b; do\n  echo \"$name\"\ndone",
                "echo after"
            ]
        );
        for statement in &split.statements {
            assert_eq!(source.get(statement.span.clone()), Some(statement.text));
        }
    }

    #[test]
    fn script_splitter_joins_a_function_definition_across_lines() {
        let source = "greet() {\n  printf '%s\\n' \"$1\"\n}\ngreet hi\n";
        let split = split_script_statements(source);
        assert!(split.diagnostics.is_empty());
        let texts = split
            .statements
            .iter()
            .map(|statement| statement.text)
            .collect::<Vec<_>>();
        assert_eq!(texts, ["greet() {\n  printf '%s\\n' \"$1\"\n}", "greet hi"]);
    }

    #[test]
    fn script_splitter_keeps_nested_constructs_in_one_statement() {
        let source = "for x in 1 2; do\n  if test \"$x\" = 1; then\n    echo one\n  else\n    echo other\n  fi\ndone\necho end\n";
        let split = split_script_statements(source);
        assert!(split.diagnostics.is_empty());
        assert_eq!(split.statements.len(), 2);
        assert!(split.statements[0].text.ends_with("\ndone"));
        assert_eq!(split.statements[1].text, "echo end");
    }

    #[test]
    fn script_splitter_keeps_comment_lines_inside_a_loop_body_in_the_statement() {
        let source = "while true; do\n  # wait for the next turn\n  break\ndone\n# trailing comment\necho after\n";
        let split = split_script_statements(source);
        assert!(split.diagnostics.is_empty());
        let texts = split
            .statements
            .iter()
            .map(|statement| statement.text)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "while true; do\n  # wait for the next turn\n  break\ndone",
                "echo after"
            ]
        );
    }

    #[test]
    fn script_splitter_ends_pending_commands_before_a_data_line() {
        let source = "echo a\ndata [1, 2]\necho b\n";
        let split = split_script_statements(source);
        assert!(split.diagnostics.is_empty());
        let texts = split
            .statements
            .iter()
            .map(|statement| statement.text)
            .collect::<Vec<_>>();
        assert_eq!(texts, ["echo a", "data [1, 2]", "echo b"]);
    }

    #[test]
    fn script_splitter_reports_a_construct_interrupted_by_a_data_line_as_unterminated() {
        let source = "for x in a; do\ndata [1]\ndone\n";
        let split = split_script_statements(source);
        assert_eq!(split.diagnostics.len(), 1);
        assert!(split.diagnostics[0].message.contains("`done`"));
        let texts = split
            .statements
            .iter()
            .map(|statement| statement.text)
            .collect::<Vec<_>>();
        assert_eq!(texts, ["data [1]", "done"]);
    }

    #[test]
    fn script_splitter_reports_an_unterminated_loop_at_end_of_input_on_its_last_token() {
        let source = "echo ok\nfor x in a; do\n  echo $x\n";
        let split = split_script_statements(source);
        assert_eq!(
            split
                .statements
                .iter()
                .map(|statement| statement.text)
                .collect::<Vec<_>>(),
            ["echo ok"]
        );
        let [diagnostic] = split.diagnostics.as_slice() else {
            panic!("expected one unterminated diagnostic");
        };
        assert_eq!(
            diagnostic.message,
            "expected `done` before the end of input"
        );
        let last_token_end = source.trim_end().len();
        assert_eq!(diagnostic.end, last_token_end);
        assert_eq!(diagnostic.start, last_token_end - 1);
    }

    #[test]
    fn script_splitter_stops_at_a_statement_over_the_line_limit() {
        let source = "echo a\nfor x in a; do\n:\n:\ndone\necho z\n";
        let split = split_script_statements_with_limits(
            source,
            ScriptStatementLimits {
                bytes_max: 1024,
                lines_max: 3,
            },
        );
        assert!(split.limit_exceeded);
        assert_eq!(
            split
                .statements
                .iter()
                .map(|statement| statement.text)
                .collect::<Vec<_>>(),
            ["echo a"]
        );
        let [diagnostic] = split.diagnostics.as_slice() else {
            panic!("expected one limit diagnostic");
        };
        assert!(diagnostic.message.contains("more than 3 lines"));
        assert_eq!(diagnostic.start, source.find("for").unwrap());
    }

    #[test]
    fn script_splitter_rejects_a_statement_over_the_byte_limit() {
        let source = "echo short\necho this statement is far too long\n";
        let split = split_script_statements_with_limits(
            source,
            ScriptStatementLimits {
                bytes_max: 16,
                lines_max: 10,
            },
        );
        assert!(split.limit_exceeded);
        assert_eq!(split.statements.len(), 1);
        let [diagnostic] = split.diagnostics.as_slice() else {
            panic!("expected one limit diagnostic");
        };
        assert!(diagnostic.message.contains("16-byte limit"));
        assert_eq!(diagnostic.start, source.find("echo this").unwrap());
    }

    #[test]
    fn script_checker_accepts_multi_line_loops_and_functions() {
        let source = "for x in a; do\n  echo $x\ndone\nf() {\n  return 0\n}\n";
        assert!(check_script(source).is_empty());
    }

    #[test]
    fn script_checker_reports_unsupported_forms_with_a_reference_shell_suggestion() {
        let source = "echo ok\n[[ -f x ]]\n";
        let diagnostics = check_script(source);
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("expected one unsupported diagnostic");
        };
        assert_eq!(source.get(diagnostic.start..diagnostic.end), Some("[["));
        assert!(diagnostic.message.contains("not supported in .qrl scripts"));
        assert!(diagnostic.message.contains("[["));
        assert!(diagnostic.help.contains("bash { ... }"));
        assert!(diagnostic.help.contains("--lang bash"));
    }

    #[test]
    fn script_checker_reports_a_syntax_error_inside_a_loop_with_an_absolute_span() {
        let source = "echo ok\nfor x in a; do\n  echo ok |\ndone\n";
        let diagnostics = check_script(source);
        assert!(!diagnostics.is_empty());
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.start >= source.find("for").unwrap()
                && source.get(diagnostic.start..diagnostic.end).is_some()
        }));
    }

    #[test]
    fn script_checker_reports_an_unterminated_construct_at_end_of_input() {
        let source = "echo ok\nf() {\n  echo body\n";
        let diagnostics = check_script(source);
        let [diagnostic] = diagnostics.as_slice() else {
            panic!("expected one unterminated diagnostic");
        };
        assert!(diagnostic.message.contains("before the end of input"));
        assert_eq!(diagnostic.end, source.trim_end().len());
    }

    #[test]
    fn script_checker_validates_a_statement_after_a_data_line_with_absolute_spans() {
        let source = "data [1]\nprintf ok |\ndata\necho last\n";
        let diagnostics = check_script(source);
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(
            source.get(diagnostics[0].start..diagnostics[0].end),
            Some("|")
        );
        assert_eq!(
            source.get(diagnostics[1].start..diagnostics[1].end),
            Some("data")
        );
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
        assert_eq!(matrix.matrix_version, "0.2");
        assert_eq!(matrix.target_contract_version, "1.0");
        assert_eq!(matrix.compatibility_level, "C1-posix-native-unix+C2-runner");
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
                "functions",
                "aliases",
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
    fn empty_quotes_preserve_the_origin_that_suppresses_home_expansion() {
        for (source, quoting) in [
            ("printf ''~/file", Quoting::Single),
            ("printf \"\"~/file", Quoting::Double),
        ] {
            let graph = parse_command_list(source).unwrap();
            let word = &graph.pipelines[0].commands[0].word_ir[1];
            assert_eq!(word.text(), "~/file");
            assert_eq!(word.parts[0].text, "");
            assert_eq!(word.parts[0].quoting, quoting);
            assert_eq!(word.parts[1].quoting, Quoting::Unquoted);
        }
        let graph = parse_command_list("printf ~\"\" \"\\\n\"~/file").unwrap();
        assert_eq!(graph.pipelines[0].commands[0].word_ir[1].parts.len(), 2);
        assert_eq!(
            graph.pipelines[0].commands[0].word_ir[2].parts[0].quoting,
            Quoting::Double
        );
    }

    #[test]
    fn double_quoted_backslash_escapes_only_posix_special_characters() {
        // POSIX keeps a backslash inside double quotes unless it precedes
        // `$`, a backtick, `"`, `\`, or a line feed; printf then sees `\n`.
        for (source, expected) in [
            (r#"printf "a\n""#, r"a\n"),
            (r#"printf "a\tb""#, r"a\tb"),
            (r#"printf "a\qb""#, r"a\qb"),
            (r#"printf "a\\b""#, r"a\b"),
            (r#"printf "a\"b""#, r#"a"b"#),
            (r#"printf "a\$b""#, "a$b"),
            (r#"printf "a\`b""#, "a`b"),
            ("printf \"a\\\nb\"", "ab"),
            (r#"printf "a\\\nb""#, r"a\\nb"),
        ] {
            let graph = parse_command_list(source).unwrap();
            let word = &graph.pipelines[0].commands[0].word_ir[1];
            assert_eq!(word.text(), expected, "{source}");
        }

        let graph = parse_command_list(r#"printf "\n$HOME""#).unwrap();
        let parts = &graph.pipelines[0].commands[0].word_ir[1].parts;
        assert!(
            parts.iter().all(|part| part.quoting == Quoting::Double),
            "a kept backslash stays double-quoted text: {parts:?}"
        );
        let graph = parse_command_list(r#"printf "\$HOME""#).unwrap();
        let parts = &graph.pipelines[0].commands[0].word_ir[1].parts;
        assert_eq!(parts[0].text, "$");
        assert_eq!(parts[0].quoting, Quoting::Escaped);
    }

    #[test]
    fn unquoted_and_single_quoted_backslashes_keep_their_rules() {
        for (source, expected) in [
            (r"printf a\nb", "anb"),
            (r"printf 'a\nb'", r"a\nb"),
            ("printf a\\\nb", "ab"),
        ] {
            let graph = parse_command_list(source).unwrap();
            let word = &graph.pipelines[0].commands[0].word_ir[1];
            assert_eq!(word.text(), expected, "{source}");
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
            assert!(
                error
                    .message
                    .starts_with("unsupported C1 dialect control form")
            );
            assert!(error.help.contains("bash { ... }"));
            assert!(error.help.contains("zsh { ... }"));
        }
    }

    #[test]
    fn clear_is_a_mode_independent_line_but_not_a_prefix() {
        for mode in [Mode::Command, Mode::Data, Mode::Natural] {
            assert_eq!(classify(mode, "  clear "), InteractiveLine::Clear);
        }
        assert_eq!(
            classify(Mode::Command, "clear -x"),
            InteractiveLine::Command("clear -x")
        );
    }

    #[test]
    fn only_forms_outside_the_native_program_grammar_are_dialect_forms() {
        for native in [
            "cd /tmp && for f in *; do echo $f; done",
            "greet() { echo hi; }",
            "function greet { echo hi; }",
            "if true; then echo yes; fi",
            "echo for '{a,b}' ${HOME}",
            "echo 'unclosed",
        ] {
            assert_eq!(dialect_control_form(native), None, "{native}");
        }
        assert_eq!(dialect_control_form("{ echo a; } > out"), None);
        let form = dialect_control_form("{ echo a; } < in").unwrap();
        assert_eq!(form.text, "<");
        assert!(!form.defines_function);
        assert!(form.reason.contains("subshell"));
        assert_eq!(dialect_control_form("echo {a,b}").unwrap().text, "{a,b}");
        assert_eq!(dialect_control_form("[[ -n x ]]").unwrap().text, "[[");
        assert!(
            dialect_control_form("greet() { echo hi; }; [[ -n x ]]")
                .unwrap()
                .defines_function
        );
        assert!(
            dialect_control_form("[[ -n x ]] && function greet { echo hi; }")
                .unwrap()
                .defines_function
        );
        assert!(
            !dialect_control_form("echo greet; [[ -n x ]]")
                .unwrap()
                .defines_function
        );
    }

    #[test]
    fn double_quoted_substitution_stays_double_quoted_and_invalid_descriptors_fail_closed() {
        let graph = parse_command_list("printf '%s' \"$(printf '*.qrl')\"").unwrap();
        assert!(
            graph.pipelines[0].commands[0].word_ir[2]
                .parts
                .iter()
                .all(|part| part.quoting == Quoting::Double)
        );
        for source in ["echo nope 3> output", "echo nope 1>&3", "cat 0<&1"] {
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

        let suffix = exact.get("printf ".len()..).unwrap();
        let plus_one = format!("printf $({suffix}");
        let error = parse_command_list(&plus_one).unwrap_err();
        assert!(error.message.contains("nesting"));
        assert!(
            error
                .help
                .contains(&MAX_COMMAND_SUBSTITUTION_DEPTH.to_string())
        );
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
                parse_program(&fixture.source, &NoAliases).unwrap();
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
