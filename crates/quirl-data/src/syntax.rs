//! Bounded, side-effect-free syntax for Quirl's focused data language.
//!
//! This module owns data syntax because its AST names data-domain sources,
//! bridges, transforms, and types. `quirl-syntax` remains the independent
//! command-grammar foundation; protocol consumers receive these diagnostics
//! through composition-root adapters instead of adding a forbidden crate edge.
//!
//! Failure invariants: invalid input returns one inert diagnostic and no
//! partial AST; every offset is a UTF-8 byte boundary; limits are checked before
//! retaining the next token, node, field, or decoded literal; and parsing owns
//! no process, filesystem, adapter, environment, or rendering capability. The
//! lexer and literal state machine each scan input once and retain
//! `O(tokens + nodes)` bounded state. Attacker-controlled nesting uses explicit
//! stacks rather than the Rust call stack.

use serde_json::{Map, Number, Value};
use std::{fmt, ops::Range};

/// Conservative syntax budgets used by [`parse_data_expression`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataSyntaxLimits {
    /// Maximum UTF-8 bytes accepted for one expression.
    pub input_bytes_max: usize,
    /// Maximum lexical tokens retained for one expression.
    pub tokens_max: usize,
    /// Maximum delimiter or literal-container nesting depth.
    pub nesting_depth_max: usize,
    /// Maximum AST nodes retained for one expression.
    pub nodes_max: usize,
    /// Maximum fields retained by one record or field-list transform.
    pub fields_max: usize,
    /// Maximum UTF-8 bytes retained by one string, path, command, or numeric literal.
    pub literal_bytes_max: usize,
}

impl DataSyntaxLimits {
    /// Defaults aligned with the current bounded data runtime.
    pub const DEFAULT: Self = Self {
        input_bytes_max: 256 * 1024,
        tokens_max: 32 * 1024,
        nesting_depth_max: 64,
        nodes_max: 100_000,
        fields_max: 256,
        literal_bytes_max: 64 * 1024,
    };
}

impl Default for DataSyntaxLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Category of an inert data-syntax diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataSyntaxDiagnosticKind {
    /// The input is not valid UTF-8.
    Encoding,
    /// The input does not conform to the focused grammar.
    Syntax,
    /// A configured parser resource limit was exceeded.
    ResourceLimit,
}

/// Recoverable, side-effect-free data parser diagnostic over a UTF-8 byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataSyntaxDiagnostic {
    /// Stable diagnostic category used by effectful adapters.
    pub kind: DataSyntaxDiagnosticKind,
    /// Concise explanation of the rejected input.
    pub message: String,
    /// Inclusive byte offset in the exact parser input.
    pub start: usize,
    /// Exclusive byte offset in the exact parser input.
    pub end: usize,
    /// Actionable correction guidance.
    pub help: String,
}

impl fmt::Display for DataSyntaxDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DataSyntaxDiagnostic {}

/// One syntax value paired with its exact half-open UTF-8 byte span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spanned<T> {
    /// Parsed semantic value.
    pub value: T,
    /// Half-open byte range in the exact expression source.
    pub span: Range<usize>,
}

/// Parsed focused data expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataExpression {
    /// Producer at the left edge of the value pipeline.
    pub source: Spanned<DataSource>,
    /// Ordered transforms following the source.
    pub transforms: Vec<Spanned<DataTransform>>,
    /// Trimmed expression range in the exact input.
    pub span: Range<usize>,
}

/// Side-effect description of a focused data producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataSource {
    /// Produce the current directory path when evaluated.
    Pwd,
    /// Produce filesystem entry records, optionally rooted at a path.
    Files {
        /// Optional path argument; absence means the evaluator's current directory.
        path: Option<Spanned<String>>,
    },
    /// Open a path through an evaluator-owned typed adapter.
    Open {
        /// Path source text after quote decoding.
        path: Spanned<String>,
    },
    /// Explicit byte-producing external command bridge.
    External {
        /// Exact trimmed command text; parsing never invokes it.
        command: Spanned<String>,
    },
    /// Inert structured literal parsed without evaluating adapters.
    Literal(SyntaxLiteral),
}

/// Focused, evaluator-independent data transform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataTransform {
    /// Count a supported value or stream.
    Length,
    /// Select the first list or stream item.
    First,
    /// Select one dotted record path.
    Get {
        /// Dotted field path selected from a record or record stream.
        path: Spanned<String>,
    },
    /// Retain rows matching a bounded predicate.
    Where(DataPredicate),
    /// Project named record fields in source order.
    Select {
        /// Non-empty bounded field names in requested output order.
        fields: Vec<Spanned<String>>,
    },
    /// Sort records by one dotted field path.
    Sort {
        /// Field path used as the comparison key.
        field: Spanned<String>,
        /// Requested stable direction.
        direction: SortDirection,
    },
    /// Retain at most a non-negative item count.
    Take {
        /// Maximum number of items retained by the evaluator.
        count: Spanned<u64>,
    },
    /// Split one byte string into newline-delimited string values.
    Lines,
    /// Parse an explicit string-to-value JSON bridge.
    FromJson,
    /// Serialize an explicit value-to-string JSON bridge.
    ToJson,
}

/// Direction of a focused record sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    /// Lowest values first.
    Ascending,
    /// Highest values first.
    Descending,
}

/// Flat predicate representation preserving `and`-before-`or` evaluator semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPredicate {
    /// Comparisons in source order; this vector is never empty.
    pub conditions: Vec<DataCondition>,
    /// Boolean connectors between adjacent conditions.
    pub operators: Vec<Spanned<BooleanOperator>>,
}

/// One record-field comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataCondition {
    /// Dotted field path on the input record.
    pub field: Spanned<String>,
    /// Comparison operator and exact operator span.
    pub comparison: Spanned<ComparisonOperator>,
    /// Scalar expected value.
    pub expected: SyntaxLiteral,
}

/// Supported record comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOperator {
    /// Values are equal.
    Equal,
    /// Values are not equal.
    NotEqual,
    /// The row value is lower.
    Less,
    /// The row value is lower or equal.
    LessOrEqual,
    /// The row value is greater.
    Greater,
    /// The row value is greater or equal.
    GreaterOrEqual,
}

/// Boolean connector between adjacent conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BooleanOperator {
    /// Both adjacent comparisons belong to the same conjunction group.
    And,
    /// Start a new conjunction group and combine groups by disjunction.
    Or,
}

/// Structured literal with the exact aggregate source span retained at every node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxLiteral {
    /// Parsed literal representation.
    pub kind: SyntaxLiteralKind,
    /// Half-open byte range in the exact expression input.
    pub span: Range<usize>,
}

/// JSON-compatible literal kinds accepted by the current focused evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxLiteralKind {
    /// JSON `null`.
    Nothing,
    /// Boolean scalar.
    Bool(bool),
    /// Signed integer preserving the accepted 64-bit range.
    Int(i64),
    /// Unsigned integer preserving the accepted 64-bit range.
    UInt(u64),
    /// JSON decimal source text retained without binary conversion.
    Decimal(String),
    /// Decoded UTF-8 JSON string.
    String(String),
    /// Ordered structured sequence.
    List(Vec<SyntaxLiteral>),
    /// Ordered record fields; duplicate names are rejected by the parser.
    Record(Vec<SyntaxRecordField>),
}

/// One parsed record field retaining both key and value spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxRecordField {
    /// Decoded field name and its quoted source span.
    pub name: Spanned<String>,
    /// Parsed field value.
    pub value: SyntaxLiteral,
}

impl SyntaxLiteral {
    /// Convert this bounded syntax literal into JSON-compatible analysis data.
    ///
    /// The evaluator does not use this compatibility view. Conversion is
    /// iterative so deeply nested caller input cannot consume the Rust stack.
    pub fn to_json(&self) -> Value {
        enum Work<'a> {
            Visit(&'a SyntaxLiteral),
            FinishList(usize),
            FinishRecord(Vec<String>),
        }

        let mut work = vec![Work::Visit(self)];
        let mut output = Vec::new();
        while let Some(task) = work.pop() {
            match task {
                Work::Visit(literal) => match &literal.kind {
                    SyntaxLiteralKind::Nothing => output.push(Value::Null),
                    SyntaxLiteralKind::Bool(value) => output.push(Value::Bool(*value)),
                    SyntaxLiteralKind::Int(value) => output.push(Value::from(*value)),
                    SyntaxLiteralKind::UInt(value) => output.push(Value::from(*value)),
                    SyntaxLiteralKind::Decimal(value) => output.push(
                        serde_json::from_str::<Number>(value)
                            .map_or_else(|_| Value::String(value.clone()), Value::Number),
                    ),
                    SyntaxLiteralKind::String(value) => output.push(Value::String(value.clone())),
                    SyntaxLiteralKind::List(values) => {
                        work.push(Work::FinishList(values.len()));
                        work.extend(values.iter().rev().map(Work::Visit));
                    }
                    SyntaxLiteralKind::Record(fields) => {
                        work.push(Work::FinishRecord(
                            fields
                                .iter()
                                .map(|field| field.name.value.clone())
                                .collect(),
                        ));
                        work.extend(fields.iter().rev().map(|field| Work::Visit(&field.value)));
                    }
                },
                Work::FinishList(length) => {
                    let start = output.len().saturating_sub(length);
                    let values = output.split_off(start);
                    output.push(Value::Array(values));
                }
                Work::FinishRecord(keys) => {
                    let start = output.len().saturating_sub(keys.len());
                    let values = output.split_off(start);
                    output.push(Value::Object(
                        keys.into_iter().zip(values).collect::<Map<_, _>>(),
                    ));
                }
            }
        }
        output.pop().unwrap_or(Value::Null)
    }

    /// Infer the explicit literal type without resolving names or evaluator behavior.
    pub fn inferred_type(&self) -> DataType {
        match &self.kind {
            SyntaxLiteralKind::Nothing => DataType::Nothing,
            SyntaxLiteralKind::Bool(_) => DataType::Bool,
            SyntaxLiteralKind::Int(_) => DataType::Int,
            SyntaxLiteralKind::UInt(_) => DataType::UInt,
            SyntaxLiteralKind::Decimal(_) => DataType::Decimal,
            SyntaxLiteralKind::String(_) => DataType::String,
            SyntaxLiteralKind::List(values) => {
                let mut values = values.iter();
                let item_type = values
                    .next()
                    .map_or(DataType::Unknown, SyntaxLiteral::inferred_type);
                let homogeneous = values.all(|value| value.inferred_type() == item_type);
                DataType::List(Box::new(if homogeneous {
                    item_type
                } else {
                    DataType::Unknown
                }))
            }
            SyntaxLiteralKind::Record(fields) => DataType::Record(
                fields
                    .iter()
                    .map(|field| TypeField {
                        name: field.name.value.clone(),
                        value_type: field.value.inferred_type(),
                        optional: false,
                    })
                    .collect(),
            ),
        }
    }
}

/// Explicit semantic type surface for existing data values and control envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    /// Absence of a value.
    Nothing,
    /// Boolean scalar.
    Bool,
    /// Signed 64-bit integer.
    Int,
    /// Unsigned 64-bit integer.
    UInt,
    /// Decimal number retained as source text.
    Decimal,
    /// UTF-8 text.
    String,
    /// Explicit byte-oriented string at a bridge boundary.
    Bytes,
    /// Filesystem path.
    Path,
    /// Duration measured by its runtime representation.
    Duration,
    /// Byte size.
    Size,
    /// Date and time value.
    DateTime,
    /// Pattern value.
    Pattern,
    /// Homogeneous or unknown-element list.
    List(Box<DataType>),
    /// Named record fields.
    Record(Vec<TypeField>),
    /// Tabular records.
    Table(Box<DataType>),
    /// Optional value.
    Option(Box<DataType>),
    /// Explicit success and error types.
    Result {
        /// Successful payload type.
        ok: Box<DataType>,
        /// Error payload type.
        error: Box<DataType>,
    },
    /// Explicit task payload type without claiming scheduler behavior.
    Task(Box<DataType>),
    /// Lazy stream item type without claiming evaluator behavior.
    Stream(Box<DataType>),
    /// External command capability value.
    Command,
    /// Type not established by this focused syntax pass.
    Unknown,
}

/// One named field in a record type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeField {
    /// Field name.
    pub name: String,
    /// Declared field type.
    pub value_type: DataType,
    /// Whether records of this type may omit the field.
    pub optional: bool,
}

/// Lexer-derived role for a data syntax span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataHighlightKind {
    /// Whitespace between tokens.
    Whitespace,
    /// Producer, transform, or bridge keyword.
    Keyword,
    /// Quoted string or path.
    String,
    /// Integer or decimal token.
    Number,
    /// `true`, `false`, or `null`.
    Literal,
    /// Field, path, or external command text.
    Name,
    /// Pipeline, comparison, or Boolean operator.
    Operator,
    /// List/record punctuation.
    Punctuation,
    /// Lexically invalid input span.
    Error,
}

/// One total highlighting span into the exact input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataHighlightSpan {
    /// Half-open UTF-8 byte range.
    pub range: Range<usize>,
    /// Syntax-derived presentation role.
    pub kind: DataHighlightKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuoteStyle {
    Single,
    Double,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Bare,
    Quoted(QuoteStyle),
    LeftBracket,
    RightBracket,
    LeftBrace,
    RightBrace,
    LeftParenthesis,
    RightParenthesis,
    Colon,
    Comma,
    Pipe,
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    span: Range<usize>,
    depth: usize,
}

/// Parse possibly malformed bytes without panicking or guessing an encoding.
pub fn parse_data_bytes(
    input: &[u8],
    limits: DataSyntaxLimits,
) -> Result<DataExpression, DataSyntaxDiagnostic> {
    check_input_size(input.len(), limits)?;
    let source = std::str::from_utf8(input).map_err(|error| {
        let start = error.valid_up_to();
        let invalid_bytes = error.error_len().unwrap_or(1);
        diagnostic(
            DataSyntaxDiagnosticKind::Encoding,
            "data expression is not valid UTF-8",
            start..start.saturating_add(invalid_bytes).min(input.len()),
            "Save the expression as UTF-8 before parsing it",
        )
    })?;
    parse_data_expression(source, limits)
}

/// Parse one focused data expression without executing sources, bridges, or transforms.
pub fn parse_data_expression(
    source: &str,
    limits: DataSyntaxLimits,
) -> Result<DataExpression, DataSyntaxDiagnostic> {
    check_input_size(source.len(), limits)?;
    let tokens = lex(source, limits)?;
    if tokens.is_empty() {
        return Err(syntax_error(
            "data expression is empty",
            0..0,
            "Add a data source such as `pwd`, `open <path>`, or a JSON value",
        ));
    }
    let stage_ranges = stage_token_ranges(source, &tokens)?;
    let mut nodes = 0_usize;
    count_node(&mut nodes, limits, token_range(&tokens))?;
    let first = stage_ranges.first().ok_or_else(|| {
        syntax_error(
            "data expression is empty",
            0..0,
            "Add a data source before the first transform",
        )
    })?;
    let source_node = parse_source(source, &tokens[first.clone()], limits, &mut nodes)?;
    let mut transforms = Vec::with_capacity(stage_ranges.len().saturating_sub(1));
    for stage in stage_ranges.iter().skip(1) {
        count_node(&mut nodes, limits, token_range(&tokens[stage.clone()]))?;
        transforms.push(parse_transform(
            source,
            &tokens[stage.clone()],
            limits,
            &mut nodes,
        )?);
    }
    let expression_start = tokens.first().map_or(0, |token| token.span.start);
    let expression_end = tokens.last().map_or(0, |token| token.span.end);
    Ok(DataExpression {
        source: source_node,
        transforms,
        span: expression_start..expression_end,
    })
}

/// Format a valid expression deterministically without evaluating it.
pub fn format_data_expression(
    source: &str,
    limits: DataSyntaxLimits,
) -> Result<String, DataSyntaxDiagnostic> {
    let expression = parse_data_expression(source, limits)?;
    let mut output = format_source(&expression.source.value);
    for transform in &expression.transforms {
        output.push_str(" | ");
        output.push_str(&format_transform(&transform.value));
    }
    Ok(output)
}

/// Return total, sorted data highlighting spans without evaluating the input.
pub fn highlight_data_expression(source: &str, limits: DataSyntaxLimits) -> Vec<DataHighlightSpan> {
    if source.is_empty() {
        return Vec::new();
    }
    let tokens = match lex(source, limits) {
        Ok(tokens) => tokens,
        Err(error) => {
            let start = error.start.min(source.len());
            let end = error.end.max(start.saturating_add(1)).min(source.len());
            return total_highlight(source.len(), vec![(start..end, DataHighlightKind::Error)]);
        }
    };
    let classified = tokens
        .iter()
        .map(|token| {
            let text = &source[token.span.clone()];
            let kind = match token.kind {
                TokenKind::Quoted(_) => DataHighlightKind::String,
                TokenKind::Pipe
                | TokenKind::Equal
                | TokenKind::NotEqual
                | TokenKind::Less
                | TokenKind::LessOrEqual
                | TokenKind::Greater
                | TokenKind::GreaterOrEqual => DataHighlightKind::Operator,
                TokenKind::LeftBracket
                | TokenKind::RightBracket
                | TokenKind::LeftBrace
                | TokenKind::RightBrace
                | TokenKind::LeftParenthesis
                | TokenKind::RightParenthesis
                | TokenKind::Colon
                | TokenKind::Comma => DataHighlightKind::Punctuation,
                TokenKind::Bare if is_keyword(text) => DataHighlightKind::Keyword,
                TokenKind::Bare if matches!(text, "true" | "false" | "null") => {
                    DataHighlightKind::Literal
                }
                TokenKind::Bare if parse_number(text).is_some() => DataHighlightKind::Number,
                TokenKind::Bare => DataHighlightKind::Name,
            };
            (token.span.clone(), kind)
        })
        .collect();
    total_highlight(source.len(), classified)
}

fn check_input_size(observed: usize, limits: DataSyntaxLimits) -> Result<(), DataSyntaxDiagnostic> {
    if observed > limits.input_bytes_max {
        return Err(limit_error(
            "data expression exceeds the input byte limit",
            0..observed,
            "input bytes",
            limits.input_bytes_max,
            observed,
        ));
    }
    Ok(())
}

fn lex(source: &str, limits: DataSyntaxLimits) -> Result<Vec<Token>, DataSyntaxDiagnostic> {
    // Keeping delimiter and quote transitions together makes it possible to
    // verify that every input byte advances exactly once and every allocation
    // is preceded by its owning limit check.
    let mut tokens = Vec::new();
    let mut delimiters: Vec<(u8, usize)> = Vec::new();
    let bytes = source.as_bytes();
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        let start = index;
        let depth = delimiters.len();
        let kind = match bytes[index] {
            b'\'' | b'"' => {
                let quote = bytes[index];
                index += 1;
                let mut escaped = false;
                let mut closed = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    if escaped {
                        escaped = false;
                        index += 1;
                        continue;
                    }
                    if byte == b'\\' {
                        escaped = true;
                        index += 1;
                        continue;
                    }
                    if byte == quote {
                        index += 1;
                        closed = true;
                        break;
                    }
                    index += 1;
                }
                if !closed {
                    return Err(syntax_error(
                        "unclosed quoted value",
                        start..source.len(),
                        "Close the quoted value with the matching quote",
                    ));
                }
                TokenKind::Quoted(if quote == b'\'' {
                    QuoteStyle::Single
                } else {
                    QuoteStyle::Double
                })
            }
            b'[' | b'{' | b'(' => {
                let opener = bytes[index];
                delimiters.push((opener, index));
                if delimiters.len() > limits.nesting_depth_max {
                    return Err(limit_error(
                        "data expression exceeds the nesting depth limit",
                        index..index + 1,
                        "nesting depth",
                        limits.nesting_depth_max,
                        delimiters.len(),
                    ));
                }
                index += 1;
                match opener {
                    b'[' => TokenKind::LeftBracket,
                    b'{' => TokenKind::LeftBrace,
                    b'(' => TokenKind::LeftParenthesis,
                    _ => unreachable!(),
                }
            }
            b']' | b'}' | b')' => {
                let closer = bytes[index];
                let expected = match closer {
                    b']' => b'[',
                    b'}' => b'{',
                    b')' => b'(',
                    _ => unreachable!(),
                };
                let Some((opener, _)) = delimiters.pop() else {
                    return Err(syntax_error(
                        "closing delimiter has no opener",
                        index..index + 1,
                        "Remove the delimiter or add its matching opener",
                    ));
                };
                if opener != expected {
                    return Err(syntax_error(
                        "closing delimiter does not match its opener",
                        index..index + 1,
                        "Use `]` for lists and `}` for records",
                    ));
                }
                index += 1;
                match closer {
                    b']' => TokenKind::RightBracket,
                    b'}' => TokenKind::RightBrace,
                    b')' => TokenKind::RightParenthesis,
                    _ => unreachable!(),
                }
            }
            b':' => {
                index += 1;
                TokenKind::Colon
            }
            b',' => {
                index += 1;
                TokenKind::Comma
            }
            b'|' => {
                index += 1;
                TokenKind::Pipe
            }
            b'=' | b'!' | b'<' | b'>' => {
                let first = bytes[index];
                index += 1;
                let has_equal = bytes.get(index) == Some(&b'=');
                if has_equal {
                    index += 1;
                }
                match (first, has_equal) {
                    (b'=', true) => TokenKind::Equal,
                    (b'!', true) => TokenKind::NotEqual,
                    (b'<', false) => TokenKind::Less,
                    (b'<', true) => TokenKind::LessOrEqual,
                    (b'>', false) => TokenKind::Greater,
                    (b'>', true) => TokenKind::GreaterOrEqual,
                    _ => {
                        return Err(syntax_error(
                            "comparison operator is incomplete",
                            start..index,
                            "Use one of `==`, `!=`, `<`, `<=`, `>`, or `>=`",
                        ));
                    }
                }
            }
            _ => {
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && !matches!(
                        bytes[index],
                        b'\''
                            | b'"'
                            | b'['
                            | b']'
                            | b'{'
                            | b'}'
                            | b'('
                            | b')'
                            | b':'
                            | b','
                            | b'|'
                            | b'='
                            | b'!'
                            | b'<'
                            | b'>'
                    )
                {
                    index += 1;
                }
                if index == start {
                    let character_len = source[index..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(1);
                    index += character_len;
                }
                TokenKind::Bare
            }
        };
        let literal_bytes = index.saturating_sub(start);
        if literal_bytes > limits.literal_bytes_max {
            return Err(limit_error(
                "data token exceeds the literal byte limit",
                start..index,
                "literal bytes",
                limits.literal_bytes_max,
                literal_bytes,
            ));
        }
        if tokens.len() == limits.tokens_max {
            return Err(limit_error(
                "data expression exceeds the token limit",
                start..index,
                "tokens",
                limits.tokens_max,
                tokens.len().saturating_add(1),
            ));
        }
        tokens.push(Token {
            kind,
            span: start..index,
            depth,
        });
    }
    if let Some((_, start)) = delimiters.pop() {
        return Err(syntax_error(
            "data expression has an unclosed delimiter",
            start..start + 1,
            "Close every list with `]` and every record with `}`",
        ));
    }
    Ok(tokens)
}

fn stage_token_ranges(
    source: &str,
    tokens: &[Token],
) -> Result<Vec<Range<usize>>, DataSyntaxDiagnostic> {
    let mut stages = Vec::new();
    let mut start = 0_usize;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind == TokenKind::Pipe && token.depth == 0 {
            if start == index {
                return Err(syntax_error(
                    "data pipeline contains an empty stage",
                    token.span.clone(),
                    "Put a source or transform on both sides of `|`",
                ));
            }
            stages.push(start..index);
            start = index + 1;
        }
    }
    if start == tokens.len() {
        let end = source.len();
        return Err(syntax_error(
            "data pipeline ends with an empty stage",
            end..end,
            "Add a transform after `|` or remove the trailing operator",
        ));
    }
    stages.push(start..tokens.len());
    Ok(stages)
}

fn parse_source(
    source: &str,
    tokens: &[Token],
    limits: DataSyntaxLimits,
    nodes: &mut usize,
) -> Result<Spanned<DataSource>, DataSyntaxDiagnostic> {
    let span = token_range(tokens);
    count_node(nodes, limits, span.clone())?;
    let first = token_text(source, &tokens[0]);
    let value = match first {
        "pwd" if tokens.len() == 1 => DataSource::Pwd,
        "pwd" => {
            return Err(usage_error(
                span,
                "pwd does not accept arguments",
                "Use `pwd` by itself as the data source",
            ));
        }
        "files" | "ls" => DataSource::Files {
            path: decode_source_argument(source, tokens, false, limits)?,
        },
        "open" => DataSource::Open {
            path: decode_source_argument(source, tokens, true, limits)?.ok_or_else(|| {
                usage_error(
                    span.clone(),
                    "open requires exactly one path",
                    "Use `open <path>` and quote paths containing whitespace",
                )
            })?,
        },
        "^external" => {
            let command_start = tokens[0].span.end;
            let command_end = span.end;
            let raw = source[command_start..command_end].trim();
            if raw.is_empty() {
                return Err(usage_error(
                    span,
                    "external source requires a command",
                    "Use `^external <command>`",
                ));
            }
            if raw.len() > limits.literal_bytes_max {
                return Err(limit_error(
                    "external command exceeds the literal byte limit",
                    command_start..command_end,
                    "literal bytes",
                    limits.literal_bytes_max,
                    raw.len(),
                ));
            }
            let leading = source[command_start..command_end]
                .len()
                .saturating_sub(source[command_start..command_end].trim_start().len());
            let start = command_start + leading;
            DataSource::External {
                command: Spanned {
                    value: raw.to_owned(),
                    span: start..start + raw.len(),
                },
            }
        }
        _ => DataSource::Literal(parse_literal(source, tokens, limits, nodes)?),
    };
    Ok(Spanned { value, span })
}

fn parse_transform(
    source: &str,
    tokens: &[Token],
    limits: DataSyntaxLimits,
    nodes: &mut usize,
) -> Result<Spanned<DataTransform>, DataSyntaxDiagnostic> {
    let span = token_range(tokens);
    let command = token_text(source, &tokens[0]);
    let value = match command {
        "length" if tokens.len() == 1 => DataTransform::Length,
        "first" if tokens.len() == 1 => DataTransform::First,
        "lines" if tokens.len() == 1 => DataTransform::Lines,
        "get" if tokens.len() == 2 => DataTransform::Get {
            path: decode_bare_name(source, &tokens[1], "get field path")?,
        },
        "select" if tokens.len() >= 2 => {
            let field_count = tokens.len() - 1;
            check_field_count(field_count, limits, span.clone())?;
            DataTransform::Select {
                fields: tokens[1..]
                    .iter()
                    .map(|token| decode_bare_name(source, token, "select field"))
                    .collect::<Result<Vec<_>, _>>()?,
            }
        }
        "sort" if matches!(tokens.len(), 2 | 3) => {
            let field = decode_bare_name(source, &tokens[1], "sort field path")?;
            let direction = match tokens.get(2).map(|token| token_text(source, token)) {
                None | Some("asc") => SortDirection::Ascending,
                Some("desc") => SortDirection::Descending,
                Some(_) => {
                    return Err(usage_error(
                        tokens[2].span.clone(),
                        "sort direction must be `asc` or `desc`",
                        "Use `sort <field> [asc|desc]`",
                    ));
                }
            };
            DataTransform::Sort { field, direction }
        }
        "take" if tokens.len() == 2 => {
            let text = token_text(source, &tokens[1]);
            let count = text.parse::<u64>().map_err(|_| {
                usage_error(
                    tokens[1].span.clone(),
                    "take count must be a non-negative integer",
                    "Use `take <count>`",
                )
            })?;
            DataTransform::Take {
                count: Spanned {
                    value: count,
                    span: tokens[1].span.clone(),
                },
            }
        }
        "from" if tokens.len() == 2 && token_text(source, &tokens[1]) == "json" => {
            DataTransform::FromJson
        }
        "to" if tokens.len() == 2 && token_text(source, &tokens[1]) == "json" => {
            DataTransform::ToJson
        }
        "where" if tokens.len() >= 4 => {
            DataTransform::Where(parse_predicate(source, &tokens[1..], limits, nodes)?)
        }
        "where" => {
            return Err(usage_error(
                span,
                "where requires a complete comparison",
                "Use `where <field> <comparison> <value> [and|or ...]`",
            ));
        }
        known
            if matches!(
                known,
                "length" | "first" | "lines" | "get" | "select" | "sort" | "take" | "from" | "to"
            ) =>
        {
            return Err(usage_error(
                span,
                format!("invalid arguments for `{known}`"),
                "Check the focused data transform syntax in `help data`",
            ));
        }
        _ => {
            return Err(syntax_error(
                format!("unknown data transform `{command}`"),
                tokens[0].span.clone(),
                "Use `get`, `where`, `select`, `sort`, `take`, `first`, `length`, `lines`, `from json`, or `to json`",
            ));
        }
    };
    Ok(Spanned { value, span })
}

fn parse_predicate(
    source: &str,
    tokens: &[Token],
    limits: DataSyntaxLimits,
    nodes: &mut usize,
) -> Result<DataPredicate, DataSyntaxDiagnostic> {
    let mut conditions = Vec::new();
    let mut operators = Vec::new();
    let mut index = 0_usize;
    loop {
        let remaining = tokens.len().saturating_sub(index);
        if remaining < 3 {
            let span = tokens.get(index).map_or_else(
                || {
                    tokens
                        .last()
                        .map_or(0..0, |token| token.span.end..token.span.end)
                },
                |token| token.span.clone(),
            );
            return Err(usage_error(
                span,
                "where predicate ends before a complete comparison",
                "Add `<field> <comparison> <value>` after the Boolean operator",
            ));
        }
        count_node(
            nodes,
            limits,
            tokens[index].span.start..tokens[index + 2].span.end,
        )?;
        let field = decode_bare_name(source, &tokens[index], "predicate field")?;
        let comparison = Spanned {
            value: comparison_operator(&tokens[index + 1])?,
            span: tokens[index + 1].span.clone(),
        };
        let expected = parse_predicate_literal(source, &tokens[index + 2], limits, nodes)?;
        check_field_count(
            conditions.len().saturating_add(1),
            limits,
            token_range(tokens),
        )?;
        conditions.push(DataCondition {
            field,
            comparison,
            expected,
        });
        index += 3;
        if index == tokens.len() {
            break;
        }
        let operator_token = &tokens[index];
        let operator = match token_text(source, operator_token) {
            "and" => BooleanOperator::And,
            "or" => BooleanOperator::Or,
            _ => {
                return Err(usage_error(
                    operator_token.span.clone(),
                    "comparisons must be joined by `and` or `or`",
                    "Use `and` or `or` between complete comparisons",
                ));
            }
        };
        operators.push(Spanned {
            value: operator,
            span: operator_token.span.clone(),
        });
        index += 1;
    }
    Ok(DataPredicate {
        conditions,
        operators,
    })
}

fn parse_predicate_literal(
    source: &str,
    token: &Token,
    limits: DataSyntaxLimits,
    nodes: &mut usize,
) -> Result<SyntaxLiteral, DataSyntaxDiagnostic> {
    count_node(nodes, limits, token.span.clone())?;
    let kind = match token.kind {
        TokenKind::Quoted(_) => {
            SyntaxLiteralKind::String(decode_quoted(source, token, false, limits)?)
        }
        TokenKind::Bare => match token_text(source, token) {
            "null" => SyntaxLiteralKind::Nothing,
            "true" => SyntaxLiteralKind::Bool(true),
            "false" => SyntaxLiteralKind::Bool(false),
            text => {
                parse_number(text).unwrap_or_else(|| SyntaxLiteralKind::String(text.to_owned()))
            }
        },
        _ => {
            return Err(usage_error(
                token.span.clone(),
                "predicate value must be a scalar literal",
                "Use a quoted string, bare name, Boolean, null, or JSON number",
            ));
        }
    };
    Ok(SyntaxLiteral {
        kind,
        span: token.span.clone(),
    })
}

#[derive(Debug)]
enum LiteralFrame {
    List {
        start: usize,
        values: Vec<SyntaxLiteral>,
        expecting_value: bool,
    },
    Record {
        start: usize,
        fields: Vec<SyntaxRecordField>,
        state: RecordState,
        pending_name: Option<Spanned<String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordState {
    Name,
    Colon,
    Value,
    CommaOrEnd,
}

fn parse_literal(
    source: &str,
    tokens: &[Token],
    limits: DataSyntaxLimits,
    nodes: &mut usize,
) -> Result<SyntaxLiteral, DataSyntaxDiagnostic> {
    // This state machine remains centralized so container ownership, separator
    // transitions, and partial-node cleanup are reviewable as one transaction.
    // Each loop turn consumes a token or attaches one already-bounded node.
    let mut stack: Vec<LiteralFrame> = Vec::new();
    let mut completed: Option<SyntaxLiteral> = None;
    let mut root: Option<SyntaxLiteral> = None;
    let mut index = 0_usize;
    while index < tokens.len() || completed.is_some() {
        if let Some(value) = completed.take() {
            if let Some(frame) = stack.last_mut() {
                match frame {
                    LiteralFrame::List {
                        values,
                        expecting_value,
                        ..
                    } if *expecting_value => {
                        values.push(value);
                        *expecting_value = false;
                    }
                    LiteralFrame::Record {
                        fields,
                        state,
                        pending_name,
                        ..
                    } if *state == RecordState::Value => {
                        let Some(name) = pending_name.take() else {
                            return Err(syntax_error(
                                "record value has no field name",
                                value.span,
                                "Add a quoted field name and `:` before the value",
                            ));
                        };
                        fields.push(SyntaxRecordField { name, value });
                        *state = RecordState::CommaOrEnd;
                    }
                    _ => {
                        return Err(syntax_error(
                            "literal values require a comma separator",
                            value.span,
                            "Insert `,` between adjacent list items or record fields",
                        ));
                    }
                }
            } else if root.replace(value).is_some() {
                return Err(syntax_error(
                    "literal contains trailing tokens",
                    tokens[index.saturating_sub(1)].span.clone(),
                    "Keep exactly one JSON-compatible source value before the pipeline",
                ));
            }
            continue;
        }
        let token = &tokens[index];
        if let Some(frame) = stack.last_mut() {
            match frame {
                LiteralFrame::List {
                    start,
                    values,
                    expecting_value,
                } => {
                    if *expecting_value {
                        if token.kind == TokenKind::RightBracket {
                            if !values.is_empty() {
                                return Err(usage_error(
                                    token.span.clone(),
                                    "list has a trailing comma",
                                    "Remove the comma before `]`",
                                ));
                            }
                            let start = *start;
                            let values = std::mem::take(values);
                            stack.pop();
                            index += 1;
                            completed = Some(SyntaxLiteral {
                                kind: SyntaxLiteralKind::List(values),
                                span: start..token.span.end,
                            });
                            continue;
                        }
                    } else {
                        match token.kind {
                            TokenKind::Comma => {
                                *expecting_value = true;
                                index += 1;
                                continue;
                            }
                            TokenKind::RightBracket => {
                                let start = *start;
                                let values = std::mem::take(values);
                                stack.pop();
                                index += 1;
                                completed = Some(SyntaxLiteral {
                                    kind: SyntaxLiteralKind::List(values),
                                    span: start..token.span.end,
                                });
                                continue;
                            }
                            _ => {
                                return Err(usage_error(
                                    token.span.clone(),
                                    "list items require a comma separator",
                                    "Insert `,` between list items",
                                ));
                            }
                        }
                    }
                }
                LiteralFrame::Record {
                    start,
                    fields,
                    state,
                    pending_name,
                } => match *state {
                    RecordState::Name => {
                        if token.kind == TokenKind::RightBrace {
                            if !fields.is_empty() {
                                return Err(usage_error(
                                    token.span.clone(),
                                    "record has a trailing comma",
                                    "Remove the comma before `}`",
                                ));
                            }
                            let start = *start;
                            let fields = std::mem::take(fields);
                            stack.pop();
                            index += 1;
                            completed = Some(SyntaxLiteral {
                                kind: SyntaxLiteralKind::Record(fields),
                                span: start..token.span.end,
                            });
                            continue;
                        }
                        if token.kind != TokenKind::Quoted(QuoteStyle::Double) {
                            return Err(usage_error(
                                token.span.clone(),
                                "record field names must be double-quoted JSON strings",
                                "Write record fields as `\"name\": value`",
                            ));
                        }
                        check_field_count(
                            fields.len().saturating_add(1),
                            limits,
                            token.span.clone(),
                        )?;
                        let name = decode_quoted(source, token, true, limits)?;
                        if fields.iter().any(|field| field.name.value == name) {
                            return Err(syntax_error(
                                format!("record field `{name}` is duplicated"),
                                token.span.clone(),
                                "Keep each record field name unique",
                            ));
                        }
                        *pending_name = Some(Spanned {
                            value: name,
                            span: token.span.clone(),
                        });
                        *state = RecordState::Colon;
                        index += 1;
                        continue;
                    }
                    RecordState::Colon => {
                        if token.kind != TokenKind::Colon {
                            return Err(usage_error(
                                token.span.clone(),
                                "record field name must be followed by `:`",
                                "Insert `:` between the field name and value",
                            ));
                        }
                        *state = RecordState::Value;
                        index += 1;
                        continue;
                    }
                    RecordState::Value => {}
                    RecordState::CommaOrEnd => match token.kind {
                        TokenKind::Comma => {
                            *state = RecordState::Name;
                            index += 1;
                            continue;
                        }
                        TokenKind::RightBrace => {
                            let start = *start;
                            let fields = std::mem::take(fields);
                            stack.pop();
                            index += 1;
                            completed = Some(SyntaxLiteral {
                                kind: SyntaxLiteralKind::Record(fields),
                                span: start..token.span.end,
                            });
                            continue;
                        }
                        _ => {
                            return Err(usage_error(
                                token.span.clone(),
                                "record fields require a comma separator",
                                "Insert `,` between record fields",
                            ));
                        }
                    },
                },
            }
        } else if root.is_some() {
            return Err(syntax_error(
                "literal contains trailing tokens",
                token.span.clone(),
                "Keep exactly one JSON-compatible source value before the pipeline",
            ));
        }

        count_node(nodes, limits, token.span.clone())?;
        match token.kind {
            TokenKind::LeftBracket => {
                stack.push(LiteralFrame::List {
                    start: token.span.start,
                    values: Vec::new(),
                    expecting_value: true,
                });
                index += 1;
            }
            TokenKind::LeftBrace => {
                stack.push(LiteralFrame::Record {
                    start: token.span.start,
                    fields: Vec::new(),
                    state: RecordState::Name,
                    pending_name: None,
                });
                index += 1;
            }
            TokenKind::Quoted(QuoteStyle::Double) => {
                completed = Some(SyntaxLiteral {
                    kind: SyntaxLiteralKind::String(decode_quoted(source, token, true, limits)?),
                    span: token.span.clone(),
                });
                index += 1;
            }
            TokenKind::Quoted(QuoteStyle::Single) => {
                return Err(usage_error(
                    token.span.clone(),
                    "structured literals require JSON double-quoted strings",
                    "Replace single quotes with JSON double quotes",
                ));
            }
            TokenKind::Bare => {
                let text = token_text(source, token);
                let kind = match text {
                    "null" => SyntaxLiteralKind::Nothing,
                    "true" => SyntaxLiteralKind::Bool(true),
                    "false" => SyntaxLiteralKind::Bool(false),
                    _ => parse_number(text).ok_or_else(|| {
                        syntax_error(
                            format!("expected a data source or JSON literal, found `{text}`"),
                            token.span.clone(),
                            "Use `pwd`, `files`, `open`, `^external`, or a JSON-compatible literal",
                        )
                    })?,
                };
                completed = Some(SyntaxLiteral {
                    kind,
                    span: token.span.clone(),
                });
                index += 1;
            }
            _ => {
                return Err(syntax_error(
                    "expected a JSON-compatible literal value",
                    token.span.clone(),
                    "Use null, a Boolean, number, string, list, or record",
                ));
            }
        }
    }
    if !stack.is_empty() {
        return Err(syntax_error(
            "literal container is incomplete",
            token_range(tokens),
            "Close every list and record after its final value",
        ));
    }
    root.ok_or_else(|| {
        syntax_error(
            "literal is empty",
            token_range(tokens),
            "Add one JSON-compatible literal value",
        )
    })
}

fn parse_number(text: &str) -> Option<SyntaxLiteralKind> {
    if let Ok(value) = text.parse::<i64>() {
        return Some(SyntaxLiteralKind::Int(value));
    }
    if let Ok(value) = text.parse::<u64>() {
        return Some(SyntaxLiteralKind::UInt(value));
    }
    serde_json::from_str::<Number>(text)
        .ok()
        .map(|_| SyntaxLiteralKind::Decimal(text.to_owned()))
}

fn comparison_operator(token: &Token) -> Result<ComparisonOperator, DataSyntaxDiagnostic> {
    match token.kind {
        TokenKind::Equal => Ok(ComparisonOperator::Equal),
        TokenKind::NotEqual => Ok(ComparisonOperator::NotEqual),
        TokenKind::Less => Ok(ComparisonOperator::Less),
        TokenKind::LessOrEqual => Ok(ComparisonOperator::LessOrEqual),
        TokenKind::Greater => Ok(ComparisonOperator::Greater),
        TokenKind::GreaterOrEqual => Ok(ComparisonOperator::GreaterOrEqual),
        _ => Err(usage_error(
            token.span.clone(),
            "where comparison operator is invalid",
            "Use one of `==`, `!=`, `<`, `<=`, `>`, or `>=`",
        )),
    }
}

fn decode_word(
    source: &str,
    token: &Token,
    limits: DataSyntaxLimits,
) -> Result<Spanned<String>, DataSyntaxDiagnostic> {
    let value = match token.kind {
        TokenKind::Bare => token_text(source, token).to_owned(),
        TokenKind::Quoted(_) => decode_quoted(source, token, false, limits)?,
        _ => {
            return Err(usage_error(
                token.span.clone(),
                "path must be one bare or quoted value",
                "Quote paths containing whitespace or data punctuation",
            ));
        }
    };
    Ok(Spanned {
        value,
        span: token.span.clone(),
    })
}

fn decode_source_argument(
    source: &str,
    tokens: &[Token],
    required: bool,
    limits: DataSyntaxLimits,
) -> Result<Option<Spanned<String>>, DataSyntaxDiagnostic> {
    let first_end = tokens[0].span.end;
    let stage_end = tokens.last().map_or(first_end, |token| token.span.end);
    let untrimmed = &source[first_end..stage_end];
    let raw = untrimmed.trim();
    if raw.is_empty() {
        return if required {
            Err(usage_error(
                first_end..stage_end,
                "source requires a path argument",
                "Add one bare path or one quoted path",
            ))
        } else {
            Ok(None)
        };
    }
    let leading = untrimmed.len().saturating_sub(untrimmed.trim_start().len());
    let start = first_end + leading;
    let span = start..start + raw.len();
    if matches!(raw.as_bytes().first(), Some(b'\'' | b'"')) {
        if tokens.len() != 2 || tokens[1].span != span {
            return Err(usage_error(
                span,
                "source accepts exactly one path",
                "Quote the complete path as one value",
            ));
        }
        return decode_word(source, &tokens[1], limits).map(Some);
    }
    if raw.chars().any(char::is_whitespace) {
        return Err(usage_error(
            span,
            "unquoted path contains whitespace",
            "Quote paths containing whitespace",
        ));
    }
    if raw.len() > limits.literal_bytes_max {
        return Err(limit_error(
            "path exceeds the literal byte limit",
            span,
            "literal bytes",
            limits.literal_bytes_max,
            raw.len(),
        ));
    }
    Ok(Some(Spanned {
        value: raw.to_owned(),
        span,
    }))
}

fn decode_bare_name(
    source: &str,
    token: &Token,
    description: &str,
) -> Result<Spanned<String>, DataSyntaxDiagnostic> {
    if token.kind != TokenKind::Bare {
        return Err(usage_error(
            token.span.clone(),
            format!("{description} must be a bare name"),
            "Use a dotted bare field path without quotes",
        ));
    }
    Ok(Spanned {
        value: token_text(source, token).to_owned(),
        span: token.span.clone(),
    })
}

fn decode_quoted(
    source: &str,
    token: &Token,
    strict_json: bool,
    limits: DataSyntaxLimits,
) -> Result<String, DataSyntaxDiagnostic> {
    let raw = token_text(source, token);
    let value = if strict_json {
        serde_json::from_str::<String>(raw).map_err(|error| {
            syntax_error(
                format!("invalid JSON string: {error}"),
                token.span.clone(),
                "Use JSON escapes such as `\\n`, `\\t`, `\\u1234`, or `\\\"`",
            )
        })?
    } else {
        decode_shell_quoted(raw, token.span.clone())?
    };
    if value.len() > limits.literal_bytes_max {
        return Err(limit_error(
            "decoded value exceeds the literal byte limit",
            token.span.clone(),
            "literal bytes",
            limits.literal_bytes_max,
            value.len(),
        ));
    }
    Ok(value)
}

fn decode_shell_quoted(raw: &str, span: Range<usize>) -> Result<String, DataSyntaxDiagnostic> {
    let mut characters = raw[1..raw.len().saturating_sub(1)].chars();
    let mut output = String::new();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        let escaped = characters.next().ok_or_else(|| {
            syntax_error(
                "quoted value ends with an unfinished escape",
                span.clone(),
                "Add the escaped character before the closing quote",
            )
        })?;
        output.push(match escaped {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            other => other,
        });
    }
    Ok(output)
}

fn count_node(
    nodes: &mut usize,
    limits: DataSyntaxLimits,
    span: Range<usize>,
) -> Result<(), DataSyntaxDiagnostic> {
    let observed = nodes.saturating_add(1);
    if observed > limits.nodes_max {
        return Err(limit_error(
            "data expression exceeds the AST node limit",
            span,
            "AST nodes",
            limits.nodes_max,
            observed,
        ));
    }
    *nodes = observed;
    Ok(())
}

fn check_field_count(
    observed: usize,
    limits: DataSyntaxLimits,
    span: Range<usize>,
) -> Result<(), DataSyntaxDiagnostic> {
    if observed > limits.fields_max {
        return Err(limit_error(
            "data expression exceeds the field limit",
            span,
            "fields",
            limits.fields_max,
            observed,
        ));
    }
    Ok(())
}

fn token_text<'a>(source: &'a str, token: &Token) -> &'a str {
    &source[token.span.clone()]
}

fn token_range(tokens: &[Token]) -> Range<usize> {
    let start = tokens.first().map_or(0, |token| token.span.start);
    let end = tokens.last().map_or(start, |token| token.span.end);
    start..end
}

fn format_source(source: &DataSource) -> String {
    match source {
        DataSource::Pwd => "pwd".to_owned(),
        DataSource::Files { path: None } => "files".to_owned(),
        DataSource::Files { path: Some(path) } => format!("files {}", quote_word(&path.value)),
        DataSource::Open { path } => format!("open {}", quote_word(&path.value)),
        DataSource::External { command } => format!("^external {}", command.value),
        DataSource::Literal(value) => format_literal(value),
    }
}

fn format_transform(transform: &DataTransform) -> String {
    match transform {
        DataTransform::Length => "length".to_owned(),
        DataTransform::First => "first".to_owned(),
        DataTransform::Get { path } => format!("get {}", path.value),
        DataTransform::Where(predicate) => {
            let mut output = String::from("where ");
            for (index, condition) in predicate.conditions.iter().enumerate() {
                if index > 0 {
                    output.push(' ');
                    output.push_str(match predicate.operators[index - 1].value {
                        BooleanOperator::And => "and",
                        BooleanOperator::Or => "or",
                    });
                    output.push(' ');
                }
                output.push_str(&condition.field.value);
                output.push(' ');
                output.push_str(match condition.comparison.value {
                    ComparisonOperator::Equal => "==",
                    ComparisonOperator::NotEqual => "!=",
                    ComparisonOperator::Less => "<",
                    ComparisonOperator::LessOrEqual => "<=",
                    ComparisonOperator::Greater => ">",
                    ComparisonOperator::GreaterOrEqual => ">=",
                });
                output.push(' ');
                output.push_str(&format_literal(&condition.expected));
            }
            output
        }
        DataTransform::Select { fields } => format!(
            "select {}",
            fields
                .iter()
                .map(|field| field.value.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ),
        DataTransform::Sort { field, direction } => match direction {
            SortDirection::Ascending => format!("sort {}", field.value),
            SortDirection::Descending => format!("sort {} desc", field.value),
        },
        DataTransform::Take { count } => format!("take {}", count.value),
        DataTransform::Lines => "lines".to_owned(),
        DataTransform::FromJson => "from json".to_owned(),
        DataTransform::ToJson => "to json".to_owned(),
    }
}

fn format_literal(literal: &SyntaxLiteral) -> String {
    match &literal.kind {
        SyntaxLiteralKind::Nothing => "null".to_owned(),
        SyntaxLiteralKind::Bool(value) => value.to_string(),
        SyntaxLiteralKind::Int(value) => value.to_string(),
        SyntaxLiteralKind::UInt(value) => value.to_string(),
        SyntaxLiteralKind::Decimal(value) => value.clone(),
        SyntaxLiteralKind::String(value) => json_string(value),
        SyntaxLiteralKind::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(format_literal)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        SyntaxLiteralKind::Record(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|field| format!(
                    "{}: {}",
                    json_string(&field.name.value),
                    format_literal(&field.value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"<invalid string>\"".to_owned())
}

fn quote_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| !byte.is_ascii_whitespace() && !b"'\"[]{}:,|=!<>".contains(&byte))
    {
        value.to_owned()
    } else {
        json_string(value)
    }
}

fn is_keyword(value: &str) -> bool {
    matches!(
        value,
        "pwd"
            | "files"
            | "ls"
            | "open"
            | "^external"
            | "length"
            | "first"
            | "get"
            | "where"
            | "select"
            | "sort"
            | "take"
            | "lines"
            | "from"
            | "to"
            | "json"
            | "and"
            | "or"
            | "asc"
            | "desc"
    )
}

fn total_highlight(
    source_len: usize,
    classified: Vec<(Range<usize>, DataHighlightKind)>,
) -> Vec<DataHighlightSpan> {
    let mut output = Vec::with_capacity(classified.len().saturating_mul(2).saturating_add(1));
    let mut cursor = 0_usize;
    for (range, kind) in classified {
        if cursor < range.start {
            output.push(DataHighlightSpan {
                range: cursor..range.start,
                kind: DataHighlightKind::Whitespace,
            });
        }
        if range.start < range.end {
            cursor = range.end;
            output.push(DataHighlightSpan { range, kind });
        }
    }
    if cursor < source_len {
        output.push(DataHighlightSpan {
            range: cursor..source_len,
            kind: DataHighlightKind::Whitespace,
        });
    }
    output
}

fn diagnostic(
    kind: DataSyntaxDiagnosticKind,
    message: impl Into<String>,
    span: Range<usize>,
    help: impl Into<String>,
) -> DataSyntaxDiagnostic {
    DataSyntaxDiagnostic {
        kind,
        message: message.into(),
        start: span.start,
        end: span.end,
        help: help.into(),
    }
}

fn syntax_error(
    message: impl Into<String>,
    span: Range<usize>,
    help: impl Into<String>,
) -> DataSyntaxDiagnostic {
    diagnostic(DataSyntaxDiagnosticKind::Syntax, message, span, help)
}

fn usage_error(
    span: Range<usize>,
    message: impl Into<String>,
    help: impl Into<String>,
) -> DataSyntaxDiagnostic {
    syntax_error(message, span, help)
}

fn limit_error(
    message: impl Into<String>,
    span: Range<usize>,
    resource: &str,
    limit: usize,
    observed: usize,
) -> DataSyntaxDiagnostic {
    diagnostic(
        DataSyntaxDiagnosticKind::ResourceLimit,
        format!(
            "{} ({resource}: limit {limit}, observed {observed})",
            message.into()
        ),
        span,
        "Reduce the expression or raise the explicit parser limit at its owning boundary",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<DataExpression, DataSyntaxDiagnostic> {
        parse_data_expression(source, DataSyntaxLimits::DEFAULT)
    }

    #[test]
    fn parses_current_sources_bridges_transforms_and_spans() {
        let source = r#"^external printf "{\"ok\":true}" | from json | where ok == true | select ok | sort ok desc | take 1 | to json"#;
        let expression = parse(source).unwrap();
        std::assert_matches!(
            &expression.source.value,
            DataSource::External { command }
                if command.value == r#"printf "{\"ok\":true}""#
        );
        assert_eq!(expression.transforms.len(), 6);
        assert_eq!(expression.span, 0..source.len());
        std::assert_matches!(&expression.transforms[1].value, DataTransform::Where(predicate)
            if predicate.conditions.len() == 1);
    }

    #[test]
    fn bare_ls_is_a_typed_files_source_inside_the_data_grammar() {
        let expression = parse("ls ./src | take 2").unwrap();
        let DataSource::Files { path } = expression.source.value else {
            panic!("expected the Data-mode ls alias to produce a files source");
        };
        assert_eq!(path.map(|path| path.value), Some("./src".to_owned()));
        let [transform] = expression.transforms.as_slice() else {
            panic!("expected one bounded transform after Data-mode ls");
        };
        let DataTransform::Take { count } = &transform.value else {
            panic!("expected the take transform");
        };
        assert_eq!(count.value, 2);
    }

    #[test]
    fn literal_parser_is_iterative_bounded_and_preserves_utf8_spans() {
        let source = r#"{"naïve": [1, 2, {"ok": true}]} | get naïve"#;
        let expression = parse(source).unwrap();
        assert_eq!(
            &source[expression.source.span.clone()],
            r#"{"naïve": [1, 2, {"ok": true}]}"#
        );
        let DataSource::Literal(literal) = expression.source.value else {
            panic!("expected literal source");
        };
        assert_eq!(literal.to_json()["naïve"][2]["ok"], true);

        let mut limits = DataSyntaxLimits::DEFAULT;
        limits.nesting_depth_max = 3;
        let error = parse_data_expression("[[[[0]]]]", limits).unwrap_err();
        assert_eq!(error.kind, DataSyntaxDiagnosticKind::ResourceLimit);
        assert!(error.message.contains("limit 3, observed 4"));
    }

    #[test]
    fn every_growth_limit_rejects_the_first_excess_item() {
        let cases = [
            (
                "input bytes",
                "[0]",
                DataSyntaxLimits {
                    input_bytes_max: 2,
                    ..DataSyntaxLimits::DEFAULT
                },
            ),
            (
                "tokens",
                "[0]",
                DataSyntaxLimits {
                    tokens_max: 2,
                    ..DataSyntaxLimits::DEFAULT
                },
            ),
            (
                "AST nodes",
                "[0]",
                DataSyntaxLimits {
                    nodes_max: 2,
                    ..DataSyntaxLimits::DEFAULT
                },
            ),
            (
                "fields",
                "{\"a\":1,\"b\":2}",
                DataSyntaxLimits {
                    fields_max: 1,
                    ..DataSyntaxLimits::DEFAULT
                },
            ),
            (
                "literal bytes",
                "\"four\"",
                DataSyntaxLimits {
                    literal_bytes_max: 5,
                    ..DataSyntaxLimits::DEFAULT
                },
            ),
        ];
        for (resource, source, limits) in cases {
            let error = parse_data_expression(source, limits).unwrap_err();
            assert_eq!(error.kind, DataSyntaxDiagnosticKind::ResourceLimit);
            assert!(error.message.contains(resource), "{error}");
        }
    }

    #[test]
    fn malformed_utf8_reports_the_exact_invalid_boundary() {
        let error = parse_data_bytes(&[b'[', b'0', b',', 0xff, b']'], DataSyntaxLimits::DEFAULT)
            .unwrap_err();
        assert_eq!(error.kind, DataSyntaxDiagnosticKind::Encoding);
        assert_eq!((error.start, error.end), (3, 4));
    }

    #[test]
    fn valid_to_invalid_transitions_are_precise_and_non_panicking() {
        assert!(parse(r#"{"ok": [1, 2]} | get ok"#).is_ok());
        let mismatch = parse(r#"{"ok": [1, 2}} | get ok"#).unwrap_err();
        assert_eq!(mismatch.kind, DataSyntaxDiagnosticKind::Syntax);
        assert_eq!((mismatch.start, mismatch.end), (12, 13));

        let trailing = parse(r#"{"ok": [1, 2,]} | get ok"#).unwrap_err();
        assert!(trailing.message.contains("trailing comma"));
    }

    #[test]
    fn formatter_is_deterministic_idempotent_and_round_trips() {
        let source =
            r#" { "x" : [1,2], "s":"a|b"}|where x != null and s == 'a|b'|select x s|sort x asc "#;
        let once = format_data_expression(source, DataSyntaxLimits::DEFAULT).unwrap();
        let twice = format_data_expression(&once, DataSyntaxLimits::DEFAULT).unwrap();
        assert_eq!(once, twice);
        assert_eq!(
            once,
            r#"{"x": [1, 2], "s": "a|b"} | where x != null and s == "a|b" | select x s | sort x"#
        );
    }

    #[test]
    fn highlighting_is_total_sorted_and_uses_data_tokens() {
        let source = r#"open "a b.json" | where size >= 10"#;
        let spans = highlight_data_expression(source, DataSyntaxLimits::DEFAULT);
        assert_eq!(spans.first().unwrap().range.start, 0);
        assert_eq!(spans.last().unwrap().range.end, source.len());
        assert!(
            spans
                .iter()
                .any(|span| span.kind == DataHighlightKind::Keyword)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.kind == DataHighlightKind::Operator)
        );
    }

    #[test]
    fn parsing_external_source_never_executes_command_text() {
        let marker = std::env::temp_dir().join("quirl-data-parser-must-not-run");
        let source = format!("^external touch {} | lines", marker.display());
        let expression = parse(&source).unwrap();
        assert!(matches!(
            expression.source.value,
            DataSource::External { .. }
        ));
        assert!(!marker.exists());
    }

    #[test]
    fn source_paths_and_external_parentheses_preserve_legacy_boundaries() {
        let open = parse("open C:/data[a,b].json | get value").unwrap();
        let DataSource::Open { path } = open.source.value else {
            panic!("expected open source");
        };
        assert_eq!(path.value, "C:/data[a,b].json");

        let external = parse("^external printf $(left | right) | lines").unwrap();
        let DataSource::External { command } = external.source.value else {
            panic!("expected external source");
        };
        assert_eq!(command.value, "printf $(left | right)");
        assert_eq!(external.transforms.len(), 1);
    }
}
