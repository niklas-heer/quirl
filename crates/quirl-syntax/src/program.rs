//! Compound POSIX programs: control flow, groups, and function definitions.
//!
//! [`parse_program`] extends the simple command grammar of
//! [`crate::parse_command_list`] with the POSIX `sh` compound commands Quirl
//! executes natively: `if`, `for`, `while`, `until`, `case`, `{ ...; }`, and
//! function definitions. It reuses the same lexer, so quoting, expansion text,
//! redirections, and line rules are identical in both grammars.
//!
//! Parsing never guesses. Valid shell syntax outside the native subset, such as
//! `[[`, subshells, or a loop used as a pipeline stage, returns
//! [`ProgramError::Unsupported`] so the caller can hand the source to a
//! reference shell unchanged. Input that ends inside a construct returns
//! [`ProgramError::Incomplete`] so an editor can continue on a new line.
//!
//! The parser is recursive descent over the token vector. Every recursion step
//! enters a compound command, and the nesting depth is checked against
//! [`PROGRAM_NESTING_MAX`] before descending, so the stack is bounded by that
//! constant rather than by attacker-controlled input.

use crate::{
    CommandSyntaxError, DialectControlForm, ListConnector, Quoting, Redirect, RedirectKind,
    SimpleCommand, Token, TokenKind, Word, WordPart, contains_unquoted_brace_expansion,
    lex_command,
};
use serde::{Deserialize, Serialize};

/// Deepest nesting of compound commands one program may contain.
pub const PROGRAM_NESTING_MAX: usize = 64;
/// Most alias substitutions applied while parsing one program.
pub const ALIAS_EXPANSIONS_MAX: usize = 64;
/// Largest alias value, in bytes, that parsing will substitute.
pub const ALIAS_VALUE_BYTES_MAX: usize = 64 * 1024;

/// A complete, natively executable program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    /// Top-level commands in source order.
    pub body: CompoundList,
}

impl Program {
    /// Whether the program uses nothing beyond the simple command grammar:
    /// no compound command, function definition, negation, or mid-list `&`.
    pub fn is_simple(&self) -> bool {
        self.body.items.iter().enumerate().all(|(index, item)| {
            let last = index.saturating_add(1) == self.body.items.len();
            (last || !item.background)
                && item
                    .and_or
                    .pipelines()
                    .all(|pipeline| !pipeline.negated && pipeline.simple_commands().is_some())
        })
    }
}

/// A sequence of and-or lists separated by `;`, `&`, or line feeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompoundList {
    /// Items in execution order.
    pub items: Vec<ListItem>,
}

/// One and-or list and whether it runs in the background.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListItem {
    /// Pipelines joined by `&&` and `||`.
    pub and_or: AndOrList,
    /// Whether the item was terminated by `&`. The parser only allows this for
    /// an and-or list made of one pipeline of simple commands, which the
    /// executor can start as a job without forking the shell.
    pub background: bool,
}

/// Pipelines joined by `&&` and `||`, evaluated left to right.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndOrList {
    /// The first pipeline, which always runs.
    pub first: PipelineNode,
    /// Following pipelines and the connector that decides whether each runs.
    pub rest: Vec<ConnectedPipeline>,
}

impl AndOrList {
    /// Iterate every pipeline in source order.
    pub fn pipelines(&self) -> impl Iterator<Item = &PipelineNode> {
        std::iter::once(&self.first).chain(self.rest.iter().map(|next| &next.pipeline))
    }
}

/// A pipeline preceded by the `&&` or `||` that guards it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectedPipeline {
    /// `And` or `Or`; `Sequence` never appears here.
    pub connector: ListConnector,
    /// The guarded pipeline.
    pub pipeline: PipelineNode,
}

/// One pipeline, optionally negated with `!`.
///
/// A pipeline of more than one stage contains only simple commands; a compound
/// command as a pipeline stage needs a subshell and is left to a reference shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineNode {
    /// Whether `!` inverts the pipeline's exit status.
    pub negated: bool,
    /// Non-empty stages in left-to-right order.
    pub commands: Vec<CommandNode>,
}

impl PipelineNode {
    /// The stages as simple commands, or `None` when a stage is compound or a
    /// function definition.
    pub fn simple_commands(&self) -> Option<Vec<&SimpleCommand>> {
        self.commands
            .iter()
            .map(|command| match command {
                CommandNode::Simple(simple) => Some(simple),
                CommandNode::Compound(_) | CommandNode::FunctionDefinition(_) => None,
            })
            .collect()
    }
}

/// One pipeline stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandNode {
    /// A command name with arguments and redirections.
    Simple(SimpleCommand),
    /// A control-flow construct or brace group.
    Compound(CompoundCommand),
    /// A function definition, which records the body without running it.
    FunctionDefinition(FunctionDefinition),
}

/// Compound commands executed natively.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompoundCommand {
    /// `{ list; }`, run in the current shell.
    BraceGroup(CompoundList),
    /// `if ...; then ...; [elif ...; then ...;] [else ...;] fi`.
    If(IfClause),
    /// `for NAME [in WORD...]; do ...; done`.
    For(ForClause),
    /// `while ...; do ...; done`.
    While(LoopClause),
    /// `until ...; do ...; done`.
    Until(LoopClause),
    /// `case WORD in PATTERN) ...;; esac`.
    Case(CaseClause),
}

/// The branches of an `if` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IfClause {
    /// The `if` branch followed by every `elif` branch, tested in order.
    pub branches: Vec<ConditionalBranch>,
    /// The `else` body, if present.
    pub else_body: Option<CompoundList>,
}

/// A condition and the body that runs when it succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalBranch {
    /// Commands whose final status selects the branch.
    pub condition: CompoundList,
    /// Commands run when the condition's status is zero.
    pub body: CompoundList,
}

/// A `for` loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForClause {
    /// Loop variable; a valid shell name.
    pub variable: String,
    /// Words after `in`, expanded when the loop starts; `None` iterates `"$@"`.
    pub words: Option<Vec<Word>>,
    /// Loop body.
    pub body: CompoundList,
}

/// A `while` or `until` loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopClause {
    /// Commands evaluated before every iteration.
    pub condition: CompoundList,
    /// Loop body.
    pub body: CompoundList,
}

/// A `case` command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseClause {
    /// The word matched against each arm's patterns.
    pub subject: Word,
    /// Arms in source order; the first matching arm runs.
    pub arms: Vec<CaseArm>,
}

/// One `case` arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseArm {
    /// Alternative patterns separated by `|`.
    pub patterns: Vec<Word>,
    /// Commands run when a pattern matches; may be empty.
    pub body: CompoundList,
}

/// A function definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionDefinition {
    /// Function name.
    pub name: String,
    /// Body executed on each call.
    pub body: CompoundCommand,
    /// Exact source text of the definition, for listing and inspection.
    pub source: String,
}

/// Why [`parse_program`] did not produce a [`Program`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramError {
    /// The input ends inside a construct, for example before `done` or after
    /// `|`. An editor should insert a line feed and keep reading.
    Incomplete(CommandSyntaxError),
    /// Valid shell syntax that Quirl leaves to a reference shell, unchanged.
    Unsupported(DialectControlForm),
    /// Invalid input.
    Syntax(CommandSyntaxError),
}

/// Session aliases consulted while parsing.
pub trait AliasLookup {
    /// The replacement text for `name`, if it is an alias.
    fn alias(&self, name: &str) -> Option<&str>;
}

/// An alias table with no entries.
pub struct NoAliases;

impl AliasLookup for NoAliases {
    fn alias(&self, _name: &str) -> Option<&str> {
        None
    }
}

impl AliasLookup for std::collections::BTreeMap<String, String> {
    fn alias(&self, name: &str) -> Option<&str> {
        self.get(name).map(String::as_str)
    }
}

/// Parse `input` as a native POSIX program, applying `aliases` to command
/// names as POSIX specifies.
///
/// Retained memory is proportional to the input plus at most
/// [`ALIAS_EXPANSIONS_MAX`] alias values of at most [`ALIAS_VALUE_BYTES_MAX`]
/// bytes each. Callers bound the input size before parsing.
pub fn parse_program(input: &str, aliases: &dyn AliasLookup) -> Result<Program, ProgramError> {
    let tokens = lex_command(input).map_err(classify_lexer_error)?;
    let mut parser = Parser {
        input,
        tokens: tokens
            .into_iter()
            .map(|token| ParsedToken {
                token,
                alias_origin: None,
                alias_eligible: false,
            })
            .collect(),
        position: 0,
        depth: 0,
        expansions: Vec::new(),
        aliases,
    };
    let body = parser.compound_list(&[], false)?;
    if let Some(token) = parser.peek() {
        let text = parser.token_text(token);
        return Err(ProgramError::Syntax(syntax_error(
            token,
            format!("unexpected `{text}`"),
            "Remove it or add the construct it closes",
        )));
    }
    Ok(Program { body })
}

/// Whether `input` ends inside an unfinished construct, so Enter should
/// continue the line rather than submit it.
pub fn is_incomplete(input: &str) -> bool {
    matches!(
        parse_program(input, &NoAliases),
        Err(ProgramError::Incomplete(_))
    )
}

/// Words with this exact unquoted text are reserved at command position.
const RESERVED_WORDS: [&str; 16] = [
    "if", "then", "elif", "else", "fi", "do", "done", "case", "esac", "while", "until", "for", "{",
    "}", "!", "function",
];

/// Whether `text` is a reserved word of the native grammar.
pub fn is_reserved_word(text: &str) -> bool {
    RESERVED_WORDS.contains(&text) || text == "in"
}

/// Commands that read or change shell state Quirl's executor does not model.
/// Inside a compound command they would silently misbehave as external
/// programs, so such programs run in a reference shell instead.
const ISLAND_ONLY_COMMANDS: [&str; 12] = [
    "read", "set", "trap", "getopts", "readonly", "exec", "wait", "umask", "ulimit", "declare",
    "typeset", "let",
];

fn classify_lexer_error(error: CommandSyntaxError) -> ProgramError {
    let unfinished =
        error.message.starts_with("unclosed ") || error.message.contains("ends with an escape");
    if unfinished {
        ProgramError::Incomplete(error)
    } else {
        ProgramError::Syntax(error)
    }
}

fn syntax_error(token: &Token, message: impl Into<String>, help: &str) -> CommandSyntaxError {
    CommandSyntaxError {
        message: message.into(),
        start: token.start,
        end: token.end,
        help: help.to_owned(),
    }
}

struct ParsedToken {
    token: Token,
    /// Index into [`Parser::expansions`] of the alias that produced this token.
    alias_origin: Option<usize>,
    /// Whether this word follows an alias value that ends in a blank and is
    /// therefore itself checked for alias substitution.
    alias_eligible: bool,
}

struct AliasExpansion {
    name: String,
    parent: Option<usize>,
}

struct Parser<'a> {
    input: &'a str,
    tokens: Vec<ParsedToken>,
    position: usize,
    depth: usize,
    expansions: Vec<AliasExpansion>,
    aliases: &'a dyn AliasLookup,
}

/// Which keywords end the compound list being parsed.
type Terminators<'t> = &'t [&'t str];

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position).map(|parsed| &parsed.token)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self
            .tokens
            .get(self.position)
            .map(|parsed| parsed.token.clone());
        if token.is_some() {
            self.position = self.position.saturating_add(1);
        }
        token
    }

    fn token_text(&self, token: &Token) -> String {
        match &token.kind {
            TokenKind::Word(word) => word.text(),
            _ => self
                .input
                .get(token.start..token.end)
                .unwrap_or_default()
                .to_owned(),
        }
    }

    /// The reserved word at the cursor, if the next token is one.
    fn keyword(&self) -> Option<&'static str> {
        let token = self.peek()?;
        let TokenKind::Word(word) = &token.kind else {
            return None;
        };
        let [part] = word.parts.as_slice() else {
            return None;
        };
        if part.quoting != Quoting::Unquoted {
            return None;
        }
        RESERVED_WORDS
            .iter()
            .copied()
            .find(|reserved| *reserved == part.text)
    }

    fn end_error(&self, expected: &str) -> ProgramError {
        let end = self.input.len();
        ProgramError::Incomplete(CommandSyntaxError {
            message: format!("expected `{expected}` before the end of input"),
            start: end,
            end,
            help: format!("Continue the command and close it with `{expected}`"),
        })
    }

    fn skip_newlines(&mut self) {
        while self
            .peek()
            .is_some_and(|token| token.kind == TokenKind::Newline)
        {
            self.position = self.position.saturating_add(1);
        }
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<(), ProgramError> {
        self.skip_newlines();
        if self.keyword() == Some(expected) {
            self.position = self.position.saturating_add(1);
            return Ok(());
        }
        match self.peek() {
            None => Err(self.end_error(expected)),
            Some(token) => {
                let text = self.token_text(token);
                Err(ProgramError::Syntax(syntax_error(
                    token,
                    format!("expected `{expected}`, found `{text}`"),
                    "Check that every compound command is closed in order",
                )))
            }
        }
    }

    fn enter(&mut self, token: &Token) -> Result<(), ProgramError> {
        let observed = self.depth.saturating_add(1);
        if observed > PROGRAM_NESTING_MAX {
            return Err(ProgramError::Syntax(syntax_error(
                token,
                format!("compound commands nest deeper than {PROGRAM_NESTING_MAX} levels"),
                "Flatten the program or move inner parts into functions",
            )));
        }
        self.depth = observed;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Parse items until end of input, one of `terminators` at command
    /// position, or (inside a `case` arm) `;;`. The terminator is not consumed.
    fn compound_list(
        &mut self,
        terminators: Terminators<'_>,
        case_arm: bool,
    ) -> Result<CompoundList, ProgramError> {
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            let Some(token) = self.peek().cloned() else {
                break;
            };
            if self
                .keyword()
                .is_some_and(|keyword| terminators.contains(&keyword))
            {
                break;
            }
            if token.kind == TokenKind::DoubleSemicolon {
                if case_arm {
                    break;
                }
                return Err(ProgramError::Syntax(syntax_error(
                    &token,
                    "unexpected `;;` outside a `case` arm",
                    "Use a single `;` to separate commands",
                )));
            }
            let and_or = self.and_or()?;
            let background = match self.peek().map(|token| token.kind.clone()) {
                Some(TokenKind::Semicolon | TokenKind::Newline) => {
                    self.position = self.position.saturating_add(1);
                    false
                }
                Some(TokenKind::Background) => {
                    let marker = self.advance().unwrap_or(token);
                    if !and_or.rest.is_empty() || and_or.first.simple_commands().is_none() {
                        return Err(ProgramError::Unsupported(unsupported(
                            &marker,
                            "&",
                            "only a pipeline of simple commands can run in the background natively",
                        )));
                    }
                    true
                }
                None => false,
                Some(TokenKind::DoubleSemicolon) if case_arm => false,
                Some(_) => {
                    if self
                        .keyword()
                        .is_some_and(|keyword| terminators.contains(&keyword))
                    {
                        false
                    } else {
                        let next = self.peek().cloned().unwrap_or(token);
                        let text = self.token_text(&next);
                        return Err(ProgramError::Syntax(syntax_error(
                            &next,
                            format!("unexpected `{text}`"),
                            "Separate commands with `;` or a new line",
                        )));
                    }
                }
            };
            items.push(ListItem { and_or, background });
        }
        Ok(CompoundList { items })
    }

    /// A compound list that must contain at least one command.
    fn required_list(
        &mut self,
        terminators: Terminators<'_>,
        construct: &str,
    ) -> Result<CompoundList, ProgramError> {
        let list = self.compound_list(terminators, false)?;
        if list.items.is_empty() {
            let closing = terminators.first().copied().unwrap_or("command");
            return match self.peek() {
                None => Err(self.end_error(closing)),
                Some(token) => Err(ProgramError::Syntax(syntax_error(
                    token,
                    format!("`{construct}` needs at least one command here"),
                    "Add a command, or `:` to do nothing",
                ))),
            };
        }
        Ok(list)
    }

    fn and_or(&mut self) -> Result<AndOrList, ProgramError> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let connector = match self.peek().map(|token| &token.kind) {
                Some(TokenKind::And) => ListConnector::And,
                Some(TokenKind::Or) => ListConnector::Or,
                _ => break,
            };
            self.position = self.position.saturating_add(1);
            if self.peek().is_none() {
                return Err(self.end_error("command"));
            }
            let pipeline = self.pipeline()?;
            rest.push(ConnectedPipeline {
                connector,
                pipeline,
            });
        }
        Ok(AndOrList { first, rest })
    }

    fn pipeline(&mut self) -> Result<PipelineNode, ProgramError> {
        let negated = self.keyword() == Some("!");
        if negated {
            self.position = self.position.saturating_add(1);
        }
        let mut commands = vec![self.command()?];
        while let Some(token) = self.peek().cloned() {
            if token.kind != TokenKind::Pipe {
                break;
            }
            self.position = self.position.saturating_add(1);
            if self.peek().is_none() {
                return Err(self.end_error("command"));
            }
            commands.push(self.command()?);
            if commands
                .iter()
                .any(|command| !matches!(command, CommandNode::Simple(_)))
            {
                return Err(ProgramError::Unsupported(unsupported(
                    &token,
                    "|",
                    "a compound command in a pipeline needs a subshell",
                )));
            }
        }
        Ok(PipelineNode { negated, commands })
    }

    fn command(&mut self) -> Result<CommandNode, ProgramError> {
        self.expand_alias_at_cursor()?;
        let Some(token) = self.peek().cloned() else {
            return Err(self.end_error("command"));
        };
        if let Some(keyword) = self.keyword() {
            return match keyword {
                "if" | "for" | "while" | "until" | "case" | "{" => {
                    let compound = self.compound_command()?;
                    self.reject_compound_redirect()?;
                    Ok(CommandNode::Compound(compound))
                }
                "function" => self.function_keyword_definition(),
                other => Err(ProgramError::Syntax(syntax_error(
                    &token,
                    format!("unexpected `{other}`"),
                    "Remove it or add the construct it closes",
                ))),
            };
        }
        if let TokenKind::Word(word) = &token.kind {
            reject_unsupported_command_word(&token, word)?;
            if let Some(name) = function_name_with_parentheses(word) {
                return self.function_definition(token.start, name, 1);
            }
            if let Some(next) = self.tokens.get(self.position.saturating_add(1))
                && let TokenKind::Word(next_word) = &next.token.kind
                && next_word.parts.len() == 1
                && next_word
                    .parts
                    .first()
                    .is_some_and(|part| part.quoting == Quoting::Unquoted && part.text == "()")
                && let Some(name) = function_name(word)
            {
                return self.function_definition(token.start, name, 2);
            }
        }
        self.simple_command().map(CommandNode::Simple)
    }

    fn reject_compound_redirect(&self) -> Result<(), ProgramError> {
        if let Some(token) = self.peek()
            && matches!(token.kind, TokenKind::Redirect { .. })
        {
            return Err(ProgramError::Unsupported(unsupported(
                token,
                self.input.get(token.start..token.end).unwrap_or_default(),
                "redirecting a compound command needs a subshell",
            )));
        }
        Ok(())
    }

    fn compound_command(&mut self) -> Result<CompoundCommand, ProgramError> {
        let Some(token) = self.advance() else {
            return Err(self.end_error("command"));
        };
        self.enter(&token)?;
        let keyword = match &token.kind {
            TokenKind::Word(word) => word.text(),
            _ => String::new(),
        };
        let result = match keyword.as_str() {
            "{" => self.brace_group(),
            "if" => self.if_clause(),
            "for" => self.for_clause(),
            "while" => self.loop_clause().map(CompoundCommand::While),
            "until" => self.loop_clause().map(CompoundCommand::Until),
            "case" => self.case_clause(),
            _ => Err(ProgramError::Syntax(syntax_error(
                &token,
                "expected a compound command",
                "Use `{`, `if`, `for`, `while`, `until`, or `case`",
            ))),
        };
        self.leave();
        result
    }

    fn brace_group(&mut self) -> Result<CompoundCommand, ProgramError> {
        let body = self.required_list(&["}"], "{")?;
        self.expect_keyword("}")?;
        Ok(CompoundCommand::BraceGroup(body))
    }

    fn if_clause(&mut self) -> Result<CompoundCommand, ProgramError> {
        let mut branches = Vec::new();
        let mut else_body = None;
        loop {
            let condition = self.required_list(&["then"], "if")?;
            self.expect_keyword("then")?;
            let body = self.required_list(&["elif", "else", "fi"], "then")?;
            branches.push(ConditionalBranch { condition, body });
            match self.keyword() {
                Some("elif") => {
                    self.position = self.position.saturating_add(1);
                }
                Some("else") => {
                    self.position = self.position.saturating_add(1);
                    else_body = Some(self.required_list(&["fi"], "else")?);
                    self.expect_keyword("fi")?;
                    break;
                }
                _ => {
                    self.expect_keyword("fi")?;
                    break;
                }
            }
        }
        Ok(CompoundCommand::If(IfClause {
            branches,
            else_body,
        }))
    }

    fn for_clause(&mut self) -> Result<CompoundCommand, ProgramError> {
        let Some(name_token) = self.advance() else {
            return Err(self.end_error("name"));
        };
        let variable = match &name_token.kind {
            TokenKind::Word(word) if is_shell_name(&word.text()) => word.text(),
            TokenKind::Word(word) if word.text().starts_with("((") => {
                return Err(ProgramError::Unsupported(unsupported(
                    &name_token,
                    "for ((",
                    "arithmetic `for` loops are Bash syntax",
                )));
            }
            _ => {
                return Err(ProgramError::Syntax(syntax_error(
                    &name_token,
                    "`for` needs a variable name",
                    "Write `for name in words; do ...; done`",
                )));
            }
        };
        self.skip_newlines();
        let mut words = None;
        if self.keyword_text() == Some("in") {
            self.position = self.position.saturating_add(1);
            let mut list = Vec::new();
            loop {
                let Some(token) = self.peek().cloned() else {
                    return Err(self.end_error("do"));
                };
                match token.kind.clone() {
                    TokenKind::Word(word) => {
                        reject_brace_expansion(&token, &word)?;
                        list.push(word);
                        self.position = self.position.saturating_add(1);
                    }
                    TokenKind::Semicolon | TokenKind::Newline => {
                        self.position = self.position.saturating_add(1);
                        break;
                    }
                    _ => {
                        let text = self.token_text(&token);
                        return Err(ProgramError::Syntax(syntax_error(
                            &token,
                            format!("unexpected `{text}` in a `for` word list"),
                            "End the word list with `;` or a new line before `do`",
                        )));
                    }
                }
            }
            words = Some(list);
        } else if self
            .peek()
            .is_some_and(|token| token.kind == TokenKind::Semicolon)
        {
            self.position = self.position.saturating_add(1);
        }
        self.expect_keyword("do")?;
        let body = self.required_list(&["done"], "do")?;
        self.expect_keyword("done")?;
        Ok(CompoundCommand::For(ForClause {
            variable,
            words,
            body,
        }))
    }

    /// `in` is reserved only after `for NAME` and `case WORD`, so it is not in
    /// [`RESERVED_WORDS`]; this checks for it at those two positions.
    fn keyword_text(&self) -> Option<&'static str> {
        let token = self.peek()?;
        let TokenKind::Word(word) = &token.kind else {
            return None;
        };
        let [part] = word.parts.as_slice() else {
            return None;
        };
        (part.quoting == Quoting::Unquoted && part.text == "in").then_some("in")
    }

    fn loop_clause(&mut self) -> Result<LoopClause, ProgramError> {
        let condition = self.required_list(&["do"], "while")?;
        self.expect_keyword("do")?;
        let body = self.required_list(&["done"], "do")?;
        self.expect_keyword("done")?;
        Ok(LoopClause { condition, body })
    }

    fn case_clause(&mut self) -> Result<CompoundCommand, ProgramError> {
        let Some(subject_token) = self.advance() else {
            return Err(self.end_error("word"));
        };
        let TokenKind::Word(subject) = subject_token.kind.clone() else {
            return Err(ProgramError::Syntax(syntax_error(
                &subject_token,
                "`case` needs a word to match",
                "Write `case $value in pattern) ...;; esac`",
            )));
        };
        self.skip_newlines();
        if self.keyword_text() != Some("in") {
            return match self.peek() {
                None => Err(self.end_error("in")),
                Some(token) => Err(ProgramError::Syntax(syntax_error(
                    token,
                    "expected `in` after the `case` word",
                    "Write `case $value in pattern) ...;; esac`",
                ))),
            };
        }
        self.position = self.position.saturating_add(1);
        let mut arms = Vec::new();
        loop {
            self.skip_newlines();
            if self.keyword() == Some("esac") {
                self.position = self.position.saturating_add(1);
                break;
            }
            if self.peek().is_none() {
                return Err(self.end_error("esac"));
            }
            let patterns = self.case_patterns()?;
            let body = self.compound_list(&["esac"], true)?;
            arms.push(CaseArm { patterns, body });
            match self.peek().map(|token| token.kind.clone()) {
                Some(TokenKind::DoubleSemicolon) => {
                    let marker = self.advance();
                    if self
                        .peek()
                        .is_some_and(|token| token.kind == TokenKind::Background)
                        && let Some(marker) = marker
                    {
                        return Err(ProgramError::Unsupported(unsupported(
                            &marker,
                            ";;&",
                            "`;;&` is Bash syntax",
                        )));
                    }
                }
                None => return Err(self.end_error("esac")),
                Some(_) => {
                    if self.keyword() != Some("esac") {
                        let next = self.peek().cloned();
                        if let Some(next) = next {
                            let text = self.token_text(&next);
                            return Err(ProgramError::Syntax(syntax_error(
                                &next,
                                format!("unexpected `{text}` in a `case` arm"),
                                "End each arm with `;;`",
                            )));
                        }
                    }
                }
            }
        }
        Ok(CompoundCommand::Case(CaseClause { subject, arms }))
    }

    /// Parse `[(] pattern [| pattern]... )`. The lexer keeps `(` and `)`
    /// inside words, so the closing parenthesis is found inside the last
    /// pattern word, and anything after it is pushed back as the next word.
    fn case_patterns(&mut self) -> Result<Vec<Word>, ProgramError> {
        let mut patterns = Vec::new();
        let mut current: Option<Word> = None;
        let mut first = true;
        loop {
            let Some(token) = self.advance() else {
                return Err(self.end_error(")"));
            };
            match token.kind.clone() {
                TokenKind::Word(mut word) => {
                    if first {
                        strip_leading_open_paren(&mut word);
                        first = false;
                        if word.parts.is_empty() {
                            continue;
                        }
                    }
                    if let Some((pattern, remainder)) = split_at_unquoted_close_paren(&word) {
                        if let Some(pattern) = pattern {
                            if current.is_some() {
                                return Err(ProgramError::Syntax(syntax_error(
                                    &token,
                                    "expected `|` or `)` between case patterns",
                                    "Separate alternative patterns with `|`",
                                )));
                            }
                            current = Some(pattern);
                        }
                        let Some(pattern) = current.take() else {
                            return Err(ProgramError::Syntax(syntax_error(
                                &token,
                                "empty case pattern",
                                "Write a pattern before `)`, or `*` to match anything",
                            )));
                        };
                        patterns.push(pattern);
                        if let Some(remainder) = remainder {
                            let consumed = self.position.saturating_sub(1);
                            let origin = self.tokens.get(consumed).and_then(|t| t.alias_origin);
                            self.tokens.insert(
                                self.position,
                                ParsedToken {
                                    token: Token {
                                        kind: TokenKind::Word(remainder),
                                        start: token.start,
                                        end: token.end,
                                    },
                                    alias_origin: origin,
                                    alias_eligible: false,
                                },
                            );
                        }
                        return Ok(patterns);
                    }
                    if current.is_some() {
                        return Err(ProgramError::Syntax(syntax_error(
                            &token,
                            "expected `|` or `)` between case patterns",
                            "Separate alternative patterns with `|`",
                        )));
                    }
                    current = Some(word);
                }
                TokenKind::Pipe => {
                    let Some(pattern) = current.take() else {
                        return Err(ProgramError::Syntax(syntax_error(
                            &token,
                            "empty case pattern",
                            "Write a pattern on both sides of `|`",
                        )));
                    };
                    patterns.push(pattern);
                }
                _ => {
                    let text = self.token_text(&token);
                    return Err(ProgramError::Syntax(syntax_error(
                        &token,
                        format!("unexpected `{text}` in a case pattern"),
                        "End the pattern list with `)`",
                    )));
                }
            }
        }
    }

    fn function_keyword_definition(&mut self) -> Result<CommandNode, ProgramError> {
        let Some(keyword) = self.advance() else {
            return Err(self.end_error("name"));
        };
        let Some(name_token) = self.peek().cloned() else {
            return Err(self.end_error("name"));
        };
        let TokenKind::Word(word) = &name_token.kind else {
            return Err(ProgramError::Syntax(syntax_error(
                &name_token,
                "`function` needs a name",
                "Write `function name { ...; }`",
            )));
        };
        let (name, consumed) = if let Some(name) = function_name_with_parentheses(word) {
            (name, 1)
        } else if let Some(name) = function_name(word) {
            let parentheses = self
                .tokens
                .get(self.position.saturating_add(1))
                .is_some_and(|next| match &next.token.kind {
                    TokenKind::Word(next) => next.text() == "()",
                    _ => false,
                });
            (name, if parentheses { 2 } else { 1 })
        } else {
            return Err(ProgramError::Syntax(syntax_error(
                &name_token,
                "invalid function name",
                "Use letters, digits, `_`, `-`, `.`, or `:` and start with a letter or `_`",
            )));
        };
        self.function_definition(keyword.start, name, consumed)
    }

    fn function_definition(
        &mut self,
        start: usize,
        name: String,
        name_tokens: usize,
    ) -> Result<CommandNode, ProgramError> {
        self.position = self.position.saturating_add(name_tokens);
        self.skip_newlines();
        if is_reserved_word(&name) {
            return Err(ProgramError::Syntax(CommandSyntaxError {
                message: format!("`{name}` is reserved and cannot name a function"),
                start,
                end: start.saturating_add(name.len()),
                help: "Choose another function name".to_owned(),
            }));
        }
        match self.keyword() {
            Some("{" | "if" | "for" | "while" | "until" | "case") => {}
            _ => {
                return match self.peek() {
                    None => Err(self.end_error("{")),
                    Some(token) => {
                        if let TokenKind::Word(word) = &token.kind
                            && word.text().starts_with('(')
                        {
                            return Err(ProgramError::Unsupported(DialectControlForm {
                                text: "(".to_owned(),
                                start: token.start,
                                end: token.end,
                                defines_function: true,
                                reason: "a subshell function body",
                            }));
                        }
                        Err(ProgramError::Syntax(syntax_error(
                            token,
                            "a function body must be a compound command",
                            "Wrap the body in braces: `name() { ...; }`",
                        )))
                    }
                };
            }
        }
        let body = self.compound_command()?;
        self.reject_compound_redirect()?;
        let end = self
            .position
            .checked_sub(1)
            .and_then(|last| self.tokens.get(last))
            .map_or(self.input.len(), |last| last.token.end);
        let source = self.input.get(start..end).unwrap_or_default().to_owned();
        Ok(CommandNode::FunctionDefinition(FunctionDefinition {
            name,
            body,
            source,
        }))
    }

    fn simple_command(&mut self) -> Result<SimpleCommand, ProgramError> {
        let mut words = Vec::new();
        let mut word_ir: Vec<Word> = Vec::new();
        let mut redirects = Vec::new();
        let mut opener = None;
        while let Some(token) = self.peek().cloned() {
            opener.get_or_insert_with(|| token.clone());
            match &token.kind {
                TokenKind::Word(word) => {
                    if word_ir.is_empty() {
                        reject_unsupported_command_word(&token, word)?;
                    } else if self
                        .tokens
                        .get(self.position)
                        .is_some_and(|parsed| parsed.alias_eligible)
                    {
                        self.expand_alias_at_cursor()?;
                        continue;
                    }
                    reject_brace_expansion(&token, word)?;
                    words.push(word.text());
                    word_ir.push(word.clone());
                    self.position = self.position.saturating_add(1);
                }
                TokenKind::Redirect { fd, kind } => {
                    self.position = self.position.saturating_add(1);
                    let target = self.redirect_target(&token, *fd, *kind)?;
                    redirects.push(target);
                }
                _ => break,
            }
        }
        if word_ir.is_empty() {
            let token = opener.or_else(|| self.peek().cloned());
            return match token {
                None => Err(self.end_error("command")),
                Some(token) => Err(ProgramError::Syntax(syntax_error(
                    &token,
                    "expected a command",
                    "Add a command name",
                ))),
            };
        }
        if let Some(name) = words.first()
            && ISLAND_ONLY_COMMANDS.contains(&name.as_str())
            && self.depth > 0
            && let Some(opener) = &opener
        {
            return Err(ProgramError::Unsupported(unsupported(
                opener,
                name,
                "this built-in is not native yet",
            )));
        }
        Ok(SimpleCommand {
            words,
            word_ir,
            redirects,
        })
    }

    fn redirect_target(
        &mut self,
        token: &Token,
        fd: u8,
        kind: RedirectKind,
    ) -> Result<Redirect, ProgramError> {
        if fd > 2 {
            return Err(ProgramError::Syntax(syntax_error(
                token,
                "native C1 supports only file descriptors 0, 1, and 2",
                "Use `bash { ... }` or `zsh { ... }` for non-standard descriptor routing",
            )));
        }
        let descriptor_matches_kind = match kind {
            RedirectKind::Input | RedirectKind::HereString => fd == 0,
            RedirectKind::Output | RedirectKind::Append => matches!(fd, 1 | 2),
            RedirectKind::DuplicateInput | RedirectKind::DuplicateOutput => true,
        };
        if !descriptor_matches_kind {
            return Err(ProgramError::Syntax(syntax_error(
                token,
                "native C1 does not support this descriptor and redirect combination",
                "Use descriptor 0 for input, 1 or 2 for output, or an explicit `bash { ... }`/`zsh { ... }` island",
            )));
        }
        let Some(next) = self.advance() else {
            return Err(ProgramError::Syntax(syntax_error(
                token,
                "redirection needs a path",
                "Add a file path after the redirection operator",
            )));
        };
        let TokenKind::Word(target) = &next.kind else {
            return Err(ProgramError::Syntax(syntax_error(
                &next,
                "redirection path must be a word",
                "Quote the path if it contains shell operators",
            )));
        };
        if kind == RedirectKind::DuplicateInput
            || (kind == RedirectKind::DuplicateOutput && (fd != 2 || target.text() != "1"))
        {
            return Err(ProgramError::Syntax(syntax_error(
                token,
                "native C1 supports only `2>&1` descriptor duplication",
                "Use `bash { ... }` or `zsh { ... }` for this descriptor graph",
            )));
        }
        Ok(Redirect {
            fd,
            kind,
            path: target.text(),
            target: target.clone(),
        })
    }

    /// Replace an alias name at the cursor with the tokens of its value.
    ///
    /// The cursor must be at a command name, or at a word that follows an
    /// alias value ending in a blank. A name is never expanded again inside
    /// its own expansion, which ends recursion such as `alias ls='ls -F'`.
    fn expand_alias_at_cursor(&mut self) -> Result<(), ProgramError> {
        loop {
            let Some(parsed) = self.tokens.get_mut(self.position) else {
                return Ok(());
            };
            parsed.alias_eligible = false;
            let origin = parsed.alias_origin;
            let token = parsed.token.clone();
            let TokenKind::Word(word) = &token.kind else {
                return Ok(());
            };
            let [part] = word.parts.as_slice() else {
                return Ok(());
            };
            if part.quoting != Quoting::Unquoted || is_reserved_word(&part.text) {
                return Ok(());
            }
            let name = part.text.as_str();
            if self.alias_is_active(origin, name) {
                return Ok(());
            }
            let Some(value) = self.aliases.alias(name) else {
                return Ok(());
            };
            if self.expansions.len() >= ALIAS_EXPANSIONS_MAX {
                return Err(ProgramError::Syntax(syntax_error(
                    &token,
                    format!("alias expansion exceeded {ALIAS_EXPANSIONS_MAX} substitutions"),
                    "Check for aliases that expand into each other",
                )));
            }
            if value.len() > ALIAS_VALUE_BYTES_MAX {
                return Err(ProgramError::Syntax(syntax_error(
                    &token,
                    format!("alias `{name}` exceeds {ALIAS_VALUE_BYTES_MAX} bytes"),
                    "Shorten the alias or define a function instead",
                )));
            }
            let trailing_blank = value.ends_with([' ', '\t']);
            let replacement = lex_command(value).map_err(|error| {
                ProgramError::Syntax(CommandSyntaxError {
                    message: format!("alias `{name}` does not parse: {}", error.message),
                    start: token.start,
                    end: token.end,
                    help: "Fix the alias with `alias name='...'`".to_owned(),
                })
            })?;
            let expansion = self.expansions.len();
            self.expansions.push(AliasExpansion {
                name: name.to_owned(),
                parent: origin,
            });
            let inserted = replacement.len();
            let spliced = replacement.into_iter().map(|replacement| ParsedToken {
                token: Token {
                    kind: replacement.kind,
                    start: token.start,
                    end: token.end,
                },
                alias_origin: Some(expansion),
                alias_eligible: false,
            });
            let after = self.position.saturating_add(1);
            self.tokens.splice(self.position..after, spliced);
            if trailing_blank
                && let Some(next) = self.tokens.get_mut(self.position.saturating_add(inserted))
            {
                next.alias_eligible = true;
            }
            if inserted == 0 {
                // An empty alias removes the word; a following word is now
                // the command name and is itself eligible for expansion.
                continue;
            }
        }
    }

    fn alias_is_active(&self, mut origin: Option<usize>, name: &str) -> bool {
        let mut steps = 0_usize;
        while let Some(index) = origin {
            let Some(expansion) = self.expansions.get(index) else {
                return false;
            };
            if expansion.name == name {
                return true;
            }
            origin = expansion.parent;
            steps = steps.saturating_add(1);
            if steps > ALIAS_EXPANSIONS_MAX {
                return true;
            }
        }
        false
    }
}

fn unsupported(token: &Token, text: &str, reason: &'static str) -> DialectControlForm {
    DialectControlForm {
        text: text.to_owned(),
        start: token.start,
        end: token.end,
        defines_function: false,
        reason,
    }
}

fn reject_brace_expansion(token: &Token, word: &Word) -> Result<(), ProgramError> {
    if contains_unquoted_brace_expansion(word) {
        return Err(ProgramError::Unsupported(unsupported(
            token,
            &word.text(),
            "brace expansion is Bash and Zsh syntax",
        )));
    }
    Ok(())
}

/// Reject command words that begin forms the native grammar leaves to a
/// reference shell.
fn reject_unsupported_command_word(token: &Token, word: &Word) -> Result<(), ProgramError> {
    let unquoted_start = word
        .parts
        .first()
        .filter(|part| part.quoting == Quoting::Unquoted)
        .map_or("", |part| part.text.as_str());
    let reason = match word.text().as_str() {
        _ if word.parts.len() > 1 && unquoted_start.starts_with('(') => {
            Some("a subshell runs in a separate shell process")
        }
        "[[" => Some("`[[` is Bash and Zsh syntax"),
        "select" => Some("`select` is Bash and Zsh syntax"),
        "coproc" => Some("`coproc` is Bash syntax"),
        _ if unquoted_start.starts_with("((") => Some("`((` arithmetic commands are Bash syntax"),
        _ if unquoted_start.starts_with('(') => Some("a subshell runs in a separate shell process"),
        _ => None,
    };
    match reason {
        Some(reason) => {
            let text = if unquoted_start.starts_with("((") {
                "((".to_owned()
            } else if unquoted_start.starts_with('(') {
                "(".to_owned()
            } else {
                word.text()
            };
            Err(ProgramError::Unsupported(unsupported(token, &text, reason)))
        }
        None => Ok(()),
    }
}

/// A valid POSIX shell name: a letter or `_` followed by letters, digits, or `_`.
pub fn is_shell_name(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// Whether `text` can name a function. Bash and Zsh accept more than POSIX
/// names; Quirl accepts the commonly used extras `-`, `.`, and `:`.
pub fn is_function_name(text: &str) -> bool {
    let mut characters = text.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| {
            character == '_'
                || character == '-'
                || character == '.'
                || character == ':'
                || character.is_ascii_alphanumeric()
        })
}

fn function_name(word: &Word) -> Option<String> {
    let [part] = word.parts.as_slice() else {
        return None;
    };
    (part.quoting == Quoting::Unquoted && is_function_name(&part.text)).then(|| part.text.clone())
}

fn function_name_with_parentheses(word: &Word) -> Option<String> {
    let [part] = word.parts.as_slice() else {
        return None;
    };
    if part.quoting != Quoting::Unquoted {
        return None;
    }
    let name = part.text.strip_suffix("()")?;
    is_function_name(name).then(|| name.to_owned())
}

fn strip_leading_open_paren(word: &mut Word) {
    let Some(first) = word.parts.first_mut() else {
        return;
    };
    if first.quoting != Quoting::Unquoted {
        return;
    }
    if let Some(rest) = first.text.strip_prefix('(') {
        first.text = rest.to_owned();
        if first.text.is_empty() {
            word.parts.remove(0);
        }
    }
}

/// Split a pattern word at its first unquoted `)`. Returns the pattern before
/// it (`None` when empty) and the remaining word after it (`None` when empty),
/// or `None` when the word has no unquoted `)`.
fn split_at_unquoted_close_paren(word: &Word) -> Option<(Option<Word>, Option<Word>)> {
    let mut before = Vec::new();
    let mut parts = word.parts.iter();
    for part in parts.by_ref() {
        if part.quoting == Quoting::Unquoted
            && let Some((head, tail)) = part.text.split_once(')')
        {
            if !head.is_empty() {
                before.push(WordPart {
                    text: head.to_owned(),
                    quoting: Quoting::Unquoted,
                });
            }
            let mut after = Vec::new();
            if !tail.is_empty() {
                after.push(WordPart {
                    text: tail.to_owned(),
                    quoting: Quoting::Unquoted,
                });
            }
            after.extend(parts.cloned());
            let pattern = (!before.is_empty()).then_some(Word { parts: before });
            let remainder = (!after.is_empty()).then_some(Word { parts: after });
            return Some((pattern, remainder));
        }
        before.push(part.clone());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn parse(source: &str) -> Program {
        parse_program(source, &NoAliases)
            .unwrap_or_else(|error| panic!("{source:?} failed: {error:?}"))
    }

    fn only_command(program: &Program) -> &CommandNode {
        let [item] = program.body.items.as_slice() else {
            panic!("expected one item: {program:?}");
        };
        let [command] = item.and_or.first.commands.as_slice() else {
            panic!("expected one command: {program:?}");
        };
        command
    }

    fn words(list: &CompoundList) -> Vec<Vec<String>> {
        list.items
            .iter()
            .flat_map(|item| item.and_or.pipelines())
            .flat_map(|pipeline| pipeline.commands.iter())
            .map(|command| match command {
                CommandNode::Simple(simple) => simple.words.clone(),
                other => vec![format!("{other:?}")],
            })
            .collect()
    }

    #[test]
    fn simple_lines_parse_into_simple_programs() {
        for source in [
            "git status",
            "printf A | tr A B && echo ok || echo no; date",
            "sleep 1 &",
            "printf '%s' 'a # b' # trailing comment",
        ] {
            assert!(parse(source).is_simple(), "{source}");
        }
        assert_eq!(
            words(&parse("echo a # comment\necho b").body),
            [vec!["echo", "a"], vec!["echo", "b"]]
        );
    }

    #[test]
    fn if_elif_else_parses_every_branch() {
        let program =
            parse("if test -f a; then echo a; elif test -f b\nthen echo b\nelse echo c; fi");
        let CommandNode::Compound(CompoundCommand::If(clause)) = only_command(&program) else {
            panic!("not an if");
        };
        assert_eq!(clause.branches.len(), 2);
        assert_eq!(words(&clause.branches[1].body), [vec!["echo", "b"]]);
        assert_eq!(
            words(clause.else_body.as_ref().unwrap()),
            [vec!["echo", "c"]]
        );
        assert!(!program.is_simple());
    }

    #[test]
    fn for_loops_accept_every_posix_word_list_form() {
        for (source, expected) in [
            ("for x in a 'b c' $y; do echo $x; done", Some(3)),
            ("for x in; do echo $x; done", Some(0)),
            ("for x\ndo echo $x\ndone", None),
            ("for x; do echo $x; done", None),
            ("for x do echo $x; done", None),
            ("for x\nin a b\ndo\n  echo $x\ndone", Some(2)),
        ] {
            let program = parse(source);
            let CommandNode::Compound(CompoundCommand::For(clause)) = only_command(&program) else {
                panic!("not a for loop: {source}");
            };
            assert_eq!(clause.variable, "x");
            assert_eq!(clause.words.as_ref().map(Vec::len), expected, "{source}");
        }
    }

    #[test]
    fn while_and_until_loops_keep_condition_and_body_apart() {
        let program = parse("while test -n \"$x\"; do x=; done; until false; do break; done");
        assert_eq!(program.body.items.len(), 2);
        let first = Program {
            body: CompoundList {
                items: vec![program.body.items[0].clone()],
            },
        };
        let CommandNode::Compound(CompoundCommand::While(clause)) = only_command(&first) else {
            panic!("not a while loop");
        };
        assert_eq!(words(&clause.condition), [vec!["test", "-n", "$x"]]);
    }

    #[test]
    fn case_arms_split_patterns_at_the_closing_parenthesis() {
        let program = parse(
            "case $1 in\n  a|b) echo ab;;\n  (c) echo c ;;\n  \"d)\"|e)echo de;;\n  *) ;;\nesac",
        );
        let CommandNode::Compound(CompoundCommand::Case(clause)) = only_command(&program) else {
            panic!("not a case");
        };
        let patterns: Vec<Vec<String>> = clause
            .arms
            .iter()
            .map(|arm| arm.patterns.iter().map(Word::text).collect())
            .collect();
        assert_eq!(
            patterns,
            [vec!["a", "b"], vec!["c"], vec!["d)", "e"], vec!["*"]]
        );
        assert_eq!(words(&clause.arms[2].body), [vec!["echo", "de"]]);
        assert!(clause.arms[3].body.items.is_empty());
        assert_eq!(clause.arms[2].patterns[0].parts[0].quoting, Quoting::Double);
    }

    #[test]
    fn last_case_arm_may_omit_its_terminator() {
        let program = parse("case x in x) echo yes\nesac");
        let CommandNode::Compound(CompoundCommand::Case(clause)) = only_command(&program) else {
            panic!("not a case");
        };
        assert_eq!(words(&clause.arms[0].body), [vec!["echo", "yes"]]);
    }

    #[test]
    fn function_definitions_record_name_body_and_source() {
        for (source, name) in [
            ("greet() { echo \"hi $1\"; }", "greet"),
            ("greet () { echo hi; }", "greet"),
            ("function greet { echo hi; }", "greet"),
            ("function greet() { echo hi; }", "greet"),
            ("git-sync()\n{\n  git pull\n}", "git-sync"),
            ("check() if true; then echo y; fi", "check"),
        ] {
            let program = parse(source);
            let CommandNode::FunctionDefinition(definition) = only_command(&program) else {
                panic!("not a function: {source}");
            };
            assert_eq!(definition.name, name);
            assert_eq!(definition.source, source);
        }
    }

    #[test]
    fn negation_and_mid_list_background_are_native() {
        let program = parse("! grep -q x file && echo absent; sleep 1 & echo started");
        assert!(program.body.items[0].and_or.first.negated);
        assert!(program.body.items[1].background);
        assert!(!program.is_simple());
    }

    #[test]
    fn reserved_words_are_ordinary_arguments_after_the_command_name() {
        let program = parse("echo if then fi done { }");
        assert_eq!(
            words(&program.body),
            [vec!["echo", "if", "then", "fi", "done", "{", "}"]]
        );
        let quoted = parse("'if' x");
        assert!(matches!(only_command(&quoted), CommandNode::Simple(_)));
    }

    #[test]
    fn unfinished_constructs_are_incomplete_rather_than_invalid() {
        for source in [
            "for x in a b; do",
            "for x in a b; do echo $x",
            "if true",
            "if true; then echo",
            "if true; then echo; else",
            "while true; do :",
            "case $x in",
            "case $x in a) echo",
            "{ echo a",
            "{ echo a }",
            "greet() {",
            "greet()",
            "echo a |",
            "echo a &&",
            "echo 'open",
            "echo \"open",
            "echo $(date",
        ] {
            assert!(is_incomplete(source), "{source} should be incomplete");
        }
        for source in [
            "echo done",
            "for x in a; do echo; done",
            "",
            "# only a comment",
        ] {
            assert!(!is_incomplete(source), "{source} should be complete");
        }
    }

    #[test]
    fn misplaced_keywords_are_syntax_errors_with_positions() {
        for (source, fragment) in [
            ("fi", "unexpected `fi`"),
            ("then echo", "unexpected `then`"),
            ("if true; then fi", "needs at least one command"),
            ("for 1x in a; do echo; done", "variable name"),
            ("echo a;; echo b", "unexpected `;;`"),
            ("case a in ) echo;; esac", "empty case pattern"),
            ("greet() echo hi", "compound command"),
            ("if() { :; }", "reserved"),
        ] {
            let Err(ProgramError::Syntax(error)) = parse_program(source, &NoAliases) else {
                panic!("{source} should be a syntax error");
            };
            assert!(
                error.message.contains(fragment),
                "{source}: {}",
                error.message
            );
            assert!(error.end <= source.len());
            assert!(!error.help.is_empty() || fragment == "reserved");
        }
    }

    #[test]
    fn forms_outside_the_native_subset_are_unsupported_not_errors() {
        for (source, text) in [
            ("[[ -n x ]]", "[["),
            ("(( 1 + 1 ))", "(("),
            ("(cd /tmp && ls)", "("),
            ("echo {a,b}", "{a,b}"),
            ("for f in {1..3}; do echo $f; done", "{1..3}"),
            ("printf a | while true; do break; done", "|"),
            ("for x in a; do echo $x; done > out", ">"),
            ("while read line; do echo $line; done", "read"),
            ("true && false &", "&"),
            ("select x in a; do break; done", "select"),
            ("for ((i=0; i<3; i++)); do echo; done", "for (("),
            ("case a in a) echo;;& esac", ";;&"),
            ("f() (echo sub)", "("),
        ] {
            match parse_program(source, &NoAliases) {
                Err(ProgramError::Unsupported(form)) => {
                    assert_eq!(form.text, text, "{source}");
                    assert!(!form.reason.is_empty());
                }
                other => panic!("{source} should be unsupported, got {other:?}"),
            }
        }
    }

    #[test]
    fn read_outside_a_compound_command_stays_a_plain_command() {
        assert!(parse("read -r line").is_simple());
    }

    #[test]
    fn nesting_beyond_the_limit_fails_closed() {
        let deep = |levels: usize| {
            let mut source = String::new();
            for _ in 0..levels {
                source.push_str("{ ");
            }
            source.push_str("true; ");
            for _ in 0..levels {
                source.push_str("} ");
            }
            source
        };
        assert!(parse_program(&deep(PROGRAM_NESTING_MAX), &NoAliases).is_ok());
        let Err(ProgramError::Syntax(error)) =
            parse_program(&deep(PROGRAM_NESTING_MAX + 1), &NoAliases)
        else {
            panic!("expected a nesting error");
        };
        assert!(error.message.contains("nest deeper"));
    }

    #[test]
    fn aliases_expand_at_command_position_only() {
        let aliases = BTreeMap::from([
            ("ll".to_owned(), "ls -l".to_owned()),
            ("g".to_owned(), "git".to_owned()),
        ]);
        let program =
            parse_program("ll src; echo ll; if true; then g status; fi", &aliases).unwrap();
        let first = words(&CompoundList {
            items: vec![program.body.items[0].clone(), program.body.items[1].clone()],
        });
        assert_eq!(first, [vec!["ls", "-l", "src"], vec!["echo", "ll"]]);
        let CommandNode::Compound(CompoundCommand::If(clause)) =
            &program.body.items[2].and_or.first.commands[0]
        else {
            panic!("not an if");
        };
        assert_eq!(words(&clause.branches[0].body), [vec!["git", "status"]]);
        let quoted = parse_program("'ll' x", &aliases).unwrap();
        assert_eq!(words(&quoted.body), [vec!["ll", "x"]]);
    }

    #[test]
    fn self_referencing_and_mutually_recursive_aliases_terminate() {
        let aliases = BTreeMap::from([
            ("ls".to_owned(), "ls -F".to_owned()),
            ("a".to_owned(), "b x".to_owned()),
            ("b".to_owned(), "a y".to_owned()),
        ]);
        assert_eq!(
            words(&parse_program("ls", &aliases).unwrap().body),
            [vec!["ls", "-F"]]
        );
        assert_eq!(
            words(&parse_program("a", &aliases).unwrap().body),
            [vec!["a", "y", "x"]]
        );
    }

    #[test]
    fn alias_values_ending_in_a_blank_expand_the_next_word() {
        let aliases = BTreeMap::from([
            ("sudo".to_owned(), "sudo ".to_owned()),
            ("ll".to_owned(), "ls -l".to_owned()),
        ]);
        assert_eq!(
            words(&parse_program("sudo ll /", &aliases).unwrap().body),
            [vec!["sudo", "ls", "-l", "/"]]
        );
    }

    #[test]
    fn aliases_may_contain_operators_and_keywords() {
        let aliases = BTreeMap::from([
            ("both".to_owned(), "echo a; echo b".to_owned()),
            ("empty".to_owned(), String::new()),
        ]);
        assert_eq!(
            words(&parse_program("both", &aliases).unwrap().body),
            [vec!["echo", "a"], vec!["echo", "b"]]
        );
        assert_eq!(
            words(&parse_program("empty echo c", &aliases).unwrap().body),
            [vec!["echo", "c"]]
        );
    }

    #[test]
    fn unparsable_alias_values_report_the_alias() {
        let aliases = BTreeMap::from([("bad".to_owned(), "echo 'open".to_owned())]);
        let Err(ProgramError::Syntax(error)) = parse_program("bad", &aliases) else {
            panic!("expected an alias error");
        };
        assert!(error.message.contains("alias `bad`"));
    }

    #[test]
    fn programs_round_trip_through_json() {
        let program = parse("f() { for x in a; do case $x in a) echo;; esac; done; }");
        let json = serde_json::to_string(&program).unwrap();
        let decoded: Program = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, program);
    }

    #[test]
    fn a_hash_inside_a_word_is_not_a_comment() {
        assert_eq!(
            words(&parse("echo a#b $# '#c'").body),
            [vec!["echo", "a#b", "$#", "#c"]]
        );
    }
}
