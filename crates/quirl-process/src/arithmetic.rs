//! POSIX shell arithmetic expansion `$(( ))` (XCU 2.6.4) over signed 64-bit
//! integers.
//!
//! The evaluator is a recursive-descent parser that evaluates while it parses,
//! so assignments take effect in source order. It follows Bash and dash for the
//! POSIX operator set, and adds `**` because users paste Bash scripts. Integer
//! literals are read as unsigned 64-bit magnitudes and reinterpreted in two's
//! complement, as Bash does, so `0xFFFFFFFFFFFFFFFF` is `-1`.
//!
//! Recursion is bounded. Every construct that can nest (parentheses, prefix
//! signs, `**` exponents, ternary branches, and assignment right-hand sides)
//! checks [`ARITHMETIC_DEPTH_MAX`] before descending. Binary precedence
//! climbing recurses at most once per precedence level, a fixed bound of ten.
//!
//! Variable values are parsed as literals and never re-evaluated as
//! expressions. Bash recurses into variable contents, so a variable holding
//! `a+1` can run code-like text stored as data. We reject that instead: a
//! variable's value is bounded, has no side effects, and cannot execute
//! anything.

use crate::{ARITHMETIC_DEPTH_MAX, ARITHMETIC_SOURCE_BYTES_MAX};
use quirl_core::{ErrorCode, ShellError};
use std::ops::Range;

/// Variable access for arithmetic evaluation.
pub(crate) trait ArithmeticVariables {
    /// Current value of a shell variable, `None` when unset.
    fn get(&self, name: &str) -> Option<String>;
    /// Assign an integer result for `=` or a compound assignment.
    ///
    /// Called once per evaluated assignment, in source order. It is never
    /// called for an assignment in a short-circuit or ternary branch that was
    /// not taken. An error aborts the expansion, and assignments already made
    /// remain in place, as they do in Bash.
    fn set(&mut self, name: &str, value: i64) -> Result<(), ShellError>;
}

/// Evaluate one arithmetic expansion body, the text between `$((` and `))`.
///
/// Whitespace (space, tab, newline) may separate tokens anywhere. Empty or
/// blank source evaluates to 0, as in Bash and dash. The source must be at
/// most [`ARITHMETIC_SOURCE_BYTES_MAX`] bytes and nest at most
/// [`ARITHMETIC_DEPTH_MAX`] levels; otherwise the error is
/// [`ErrorCode::ResourceLimit`]. Division or remainder by zero is an error
/// even on an unevaluated side, matching Bash.
pub(crate) fn evaluate_arithmetic(
    source: &str,
    variables: &mut dyn ArithmeticVariables,
) -> Result<i64, ShellError> {
    if source.len() > ARITHMETIC_SOURCE_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "arithmetic expansion exceeds its source byte limit",
        )
        .with_context(format!(
            "limit {ARITHMETIC_SOURCE_BYTES_MAX} bytes; observed {} bytes",
            source.len()
        ))
        .with_help("Split the calculation into smaller expressions"));
    }
    let mut parser = Parser {
        source,
        input: source.as_bytes(),
        index: 0,
        variables,
        evaluate: true,
    };
    parser.skip_whitespace();
    if parser.index >= parser.input.len() {
        return Ok(0);
    }
    let value = parser.comma(0)?;
    parser.skip_whitespace();
    if parser.index != parser.input.len() {
        return Err(parser.unexpected_token());
    }
    Ok(value)
}

/// Binary operators in order of increasing binding (precedence climbing).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Binary {
    Or,
    And,
    BitOr,
    BitXor,
    BitAnd,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    ShiftLeft,
    ShiftRight,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

impl Binary {
    /// Map an operator token to its binary operator, or `None` when the token
    /// is not a binary operator (for example `?`, `=`, or `**`).
    fn from_token(token: &str) -> Option<Self> {
        Some(match token {
            "||" => Self::Or,
            "&&" => Self::And,
            "|" => Self::BitOr,
            "^" => Self::BitXor,
            "&" => Self::BitAnd,
            "==" => Self::Equal,
            "!=" => Self::NotEqual,
            "<" => Self::Less,
            "<=" => Self::LessEqual,
            ">" => Self::Greater,
            ">=" => Self::GreaterEqual,
            "<<" => Self::ShiftLeft,
            ">>" => Self::ShiftRight,
            "+" => Self::Add,
            "-" => Self::Subtract,
            "*" => Self::Multiply,
            "/" => Self::Divide,
            "%" => Self::Remainder,
            _ => return None,
        })
    }

    /// Binding strength; a larger number binds tighter. All binary operators
    /// are left-associative. The lowest level is 1, so a caller that passes 1
    /// accepts every binary operator.
    fn precedence(self) -> u8 {
        match self {
            Self::Or => 1,
            Self::And => 2,
            Self::BitOr => 3,
            Self::BitXor => 4,
            Self::BitAnd => 5,
            Self::Equal | Self::NotEqual => 6,
            Self::Less | Self::LessEqual | Self::Greater | Self::GreaterEqual => 7,
            Self::ShiftLeft | Self::ShiftRight => 8,
            Self::Add | Self::Subtract => 9,
            Self::Multiply | Self::Divide | Self::Remainder => 10,
        }
    }

    /// Whether the right operand is not evaluated, given the left value. This
    /// is `0 &&` and `nonzero ||`, so the right side performs no assignments.
    fn skips_right(self, left: i64) -> bool {
        match self {
            Self::And => left == 0,
            Self::Or => left != 0,
            _ => false,
        }
    }
}

/// Assignment operators. `Plain` is `=`, and `Compound` applies its operator
/// to the current value before storing the result.
#[derive(Clone, Copy)]
enum Assignment {
    Plain,
    Compound(Binary),
}

/// Map an assignment operator token to its kind, or `None` for any other token.
fn assignment_kind(token: &str) -> Option<Assignment> {
    Some(match token {
        "=" => Assignment::Plain,
        "+=" => Assignment::Compound(Binary::Add),
        "-=" => Assignment::Compound(Binary::Subtract),
        "*=" => Assignment::Compound(Binary::Multiply),
        "/=" => Assignment::Compound(Binary::Divide),
        "%=" => Assignment::Compound(Binary::Remainder),
        "<<=" => Assignment::Compound(Binary::ShiftLeft),
        ">>=" => Assignment::Compound(Binary::ShiftRight),
        "&=" => Assignment::Compound(Binary::BitAnd),
        "^=" => Assignment::Compound(Binary::BitXor),
        "|=" => Assignment::Compound(Binary::BitOr),
        _ => return None,
    })
}

/// Every operator token, longest first within each length class. The first
/// entry that matches at a position is therefore the longest match, so `<<=`
/// is never read as `<`. `++` and `--` are listed so they can be rejected by
/// name; they are not supported.
const OPERATORS: &[&str] = &[
    "<<=", ">>=", "||", "&&", "==", "!=", "<=", ">=", "<<", ">>", "**", "*=", "/=", "%=", "+=",
    "-=", "&=", "^=", "|=", "++", "--", "=", "<", ">", "|", "^", "&", "+", "-", "*", "/", "%", "?",
    ":", ",", "!", "~",
];

/// Return the longest operator token that starts at `index`, if any. The caller
/// must already have skipped whitespace.
fn operator_at(input: &[u8], index: usize) -> Option<&'static str> {
    OPERATORS.iter().copied().find(|operator| {
        input
            .get(index..)
            .is_some_and(|rest| rest.starts_with(operator.as_bytes()))
    })
}

/// Return the index of the first non-whitespace byte at or after `index`.
fn skip_whitespace_from(input: &[u8], mut index: usize) -> usize {
    while input.get(index).is_some_and(u8::is_ascii_whitespace) {
        index = index.saturating_add(1);
    }
    index
}

/// Whether `byte` may start a variable name.
fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// Return the end index of the identifier starting at `start`, or `None` when
/// `start` does not begin an identifier. Identifiers match
/// `[A-Za-z_][A-Za-z0-9_]*`.
fn identifier_end(input: &[u8], start: usize) -> Option<usize> {
    if !input.get(start).copied().is_some_and(is_identifier_start) {
        return None;
    }
    let mut end = start;
    while input
        .get(end)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        end = end.saturating_add(1);
    }
    Some(end)
}

/// Return the observed depth after descending one nesting level, or a
/// resource-limit error when that level exceeds [`ARITHMETIC_DEPTH_MAX`]. Call
/// this before every recursive descent into a nestable construct.
fn nested_depth(depth: usize) -> Result<usize, ShellError> {
    let observed_depth = depth.saturating_add(1);
    if observed_depth > ARITHMETIC_DEPTH_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "arithmetic expansion exceeds its nesting depth limit",
        )
        .with_context(format!(
            "limit {ARITHMETIC_DEPTH_MAX} levels; observed at least {observed_depth} levels"
        ))
        .with_help("Flatten the arithmetic expression"));
    }
    Ok(observed_depth)
}

/// Parse an unsigned literal in decimal, octal (leading `0`), or hex (`0x` or
/// `0X`) form and reinterpret the 64-bit value as two's complement. Returns
/// `None` for an empty or malformed digit run, a missing hex digit, or a value
/// beyond 2^64 - 1.
fn parse_literal(text: &str) -> Option<i64> {
    let (digits, radix) =
        if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
            (hex, 16)
        } else if text.len() > 1 && text.starts_with('0') {
            (text, 8)
        } else {
            (text, 10)
        };
    let all_digits =
        !digits.is_empty() && digits.bytes().all(|byte| char::from(byte).is_digit(radix));
    if !all_digits {
        return None;
    }
    u64::from_str_radix(digits, radix)
        .ok()
        .map(u64::cast_signed)
}

/// Parse a variable's content as a signed literal. Surrounding whitespace is
/// ignored, and one leading `+` or `-` is accepted. Returns `None` when the
/// content is not an integer literal.
fn parse_variable_value(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let (negative, unsigned) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, trimmed.get(1..)?),
        Some(b'+') => (false, trimmed.get(1..)?),
        _ => (false, trimmed),
    };
    let magnitude = parse_literal(unsigned)?;
    Some(if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    })
}

/// Apply a prefix operator token (`+`, `-`, `!`, or `~`) with wrapping
/// semantics. Any other token leaves the value unchanged.
fn apply_unary(token: &str, value: i64) -> i64 {
    match token {
        "-" => value.wrapping_neg(),
        "!" => i64::from(value == 0),
        "~" => !value,
        _ => value,
    }
}

/// Apply a non-assignment binary operator with two's-complement wrapping.
/// Returns `None` for division or remainder by zero. `i64::MIN / -1` and
/// `i64::MIN % -1` wrap to `i64::MIN` and `0`, as in Bash.
fn apply_binary(operator: Binary, left: i64, right: i64) -> Option<i64> {
    if matches!(operator, Binary::Divide | Binary::Remainder) && right == 0 {
        return None;
    }
    // Only the low six bits of the count are used, so it is always in 0..=63.
    // The fallback is unreachable and keeps this function panic-free.
    let shift = u32::try_from(right & 63).unwrap_or_default();
    Some(match operator {
        Binary::Or => i64::from(left != 0 || right != 0),
        Binary::And => i64::from(left != 0 && right != 0),
        Binary::BitOr => left | right,
        Binary::BitXor => left ^ right,
        Binary::BitAnd => left & right,
        Binary::Equal => i64::from(left == right),
        Binary::NotEqual => i64::from(left != right),
        Binary::Less => i64::from(left < right),
        Binary::LessEqual => i64::from(left <= right),
        Binary::Greater => i64::from(left > right),
        Binary::GreaterEqual => i64::from(left >= right),
        Binary::ShiftLeft => left.wrapping_shl(shift),
        Binary::ShiftRight => left.wrapping_shr(shift),
        Binary::Add => left.wrapping_add(right),
        Binary::Subtract => left.wrapping_sub(right),
        Binary::Multiply => left.wrapping_mul(right),
        // A zero divisor was rejected above, so `checked_*` fails only for
        // `i64::MIN` divided by -1, whose wrapped results are `i64::MIN` and 0.
        Binary::Divide => left.checked_div(right).unwrap_or(i64::MIN),
        Binary::Remainder => left.checked_rem(right).unwrap_or_default(),
    })
}

/// Raise `base` to `exponent` modulo 2^64 by square-and-multiply. At most 64
/// rounds run, one per bit of the exponent.
fn pow_wrapping(mut base: i64, mut exponent: u64) -> i64 {
    let mut result: i64 = 1;
    while exponent != 0 {
        if exponent & 1 != 0 {
            result = result.wrapping_mul(base);
        }
        base = base.wrapping_mul(base);
        exponent >>= 1;
    }
    result
}

/// An assignment target located by lookahead, before any input is consumed.
struct AssignmentTarget {
    /// Byte range of the variable name in the source.
    name: Range<usize>,
    /// Byte index just past the assignment operator token.
    operator_end: usize,
    /// Which assignment operator this is.
    kind: Assignment,
}

/// Recursive-descent evaluator state for one expansion body.
struct Parser<'a, 'v> {
    /// The whole expansion body, used for error context and name slices.
    source: &'a str,
    /// Byte view of `source`. Tokens are ASCII, so every token boundary is a
    /// character boundary.
    input: &'a [u8],
    /// Byte offset of the next unread token.
    index: usize,
    /// Variable table read and written during evaluation.
    variables: &'v mut dyn ArithmeticVariables,
    /// `false` on the unevaluated side of `&&`, `||`, or `?:`. Assignments are
    /// then skipped, so the side has no effect.
    evaluate: bool,
}

impl Parser<'_, '_> {
    /// Advance past whitespace bytes.
    fn skip_whitespace(&mut self) {
        self.index = skip_whitespace_from(self.input, self.index);
    }

    /// Advance by `bytes`, saturating at the end of input.
    fn advance(&mut self, bytes: usize) {
        self.index = self.index.saturating_add(bytes);
    }

    /// Skip whitespace and return the operator token at the cursor, if any.
    fn peek_operator(&mut self) -> Option<&'static str> {
        self.skip_whitespace();
        operator_at(self.input, self.index)
    }

    /// Consume `token` when it is the next operator. Returns whether it was consumed.
    fn consume_operator(&mut self, token: &str) -> bool {
        if self.peek_operator() == Some(token) {
            self.advance(token.len());
            true
        } else {
            false
        }
    }

    /// Build an operating error that carries the expansion source as context.
    fn error(&self, message: &str, help: &str) -> ShellError {
        ShellError::new(ErrorCode::InvalidCommand, message)
            .with_help(help)
            .with_context(format!("in `{}`", self.source))
    }

    /// Error for a division or remainder whose divisor is zero.
    fn division_error(&self) -> ShellError {
        self.error(
            "division by zero in arithmetic expansion",
            "Use a non-zero divisor",
        )
    }

    /// Error for the token at the cursor, which cannot start or continue an
    /// expression. Increment and decrement operators get their own message.
    fn unexpected_token(&self) -> ShellError {
        let rest = self.source.get(self.index..).unwrap_or_default();
        if rest.starts_with("++") || rest.starts_with("--") {
            return self.error(
                "increment and decrement operators are not supported in arithmetic expansion",
                "Write `x = x + 1` or `x += 1` instead",
            );
        }
        let character = rest.chars().next().map_or_else(String::new, String::from);
        self.error(
            &format!("unexpected `{character}` in arithmetic expansion"),
            "Use integer literals, variable names, and the POSIX arithmetic operators",
        )
    }

    /// Run `parse` with evaluation enabled only when it is already enabled and
    /// `enabled` is also true. Assignments are skipped for the duration.
    fn with_evaluation(
        &mut self,
        enabled: bool,
        parse: impl FnOnce(&mut Self) -> Result<i64, ShellError>,
    ) -> Result<i64, ShellError> {
        let saved = self.evaluate;
        self.evaluate = saved && enabled;
        let result = parse(self);
        self.evaluate = saved;
        result
    }

    /// Comma operator: evaluate each assignment-level expression left to right
    /// and yield the last value.
    fn comma(&mut self, depth: usize) -> Result<i64, ShellError> {
        let mut value = self.assignment(depth)?;
        while self.consume_operator(",") {
            value = self.assignment(depth)?;
        }
        Ok(value)
    }

    /// Assignment (right-associative). The left side must be a plain
    /// identifier, so the target is found by lookahead before any parsing.
    fn assignment(&mut self, depth: usize) -> Result<i64, ShellError> {
        self.skip_whitespace();
        if let Some(target) = self.assignment_target() {
            return self.assign(target, depth);
        }
        let value = self.conditional(depth)?;
        if self
            .peek_operator()
            .is_some_and(|token| assignment_kind(token).is_some())
        {
            return Err(self.error(
                "assignment target must be a variable",
                "Assign only to a plain identifier, such as `x = 1`",
            ));
        }
        Ok(value)
    }

    /// Look ahead for `name op`, where `op` is an assignment operator. Consumes
    /// nothing. The cursor must already be past whitespace.
    fn assignment_target(&self) -> Option<AssignmentTarget> {
        let name_end = identifier_end(self.input, self.index)?;
        let operator_start = skip_whitespace_from(self.input, name_end);
        let token = operator_at(self.input, operator_start)?;
        let kind = assignment_kind(token)?;
        Some(AssignmentTarget {
            name: self.index..name_end,
            operator_end: operator_start.saturating_add(token.len()),
            kind,
        })
    }

    /// Evaluate `name op= right`. A compound assignment reads the current
    /// value before the right side, which is the C order. The result is stored
    /// only when evaluation is enabled.
    fn assign(&mut self, target: AssignmentTarget, depth: usize) -> Result<i64, ShellError> {
        let source = self.source;
        let name = source.get(target.name).ok_or_else(|| {
            self.error(
                "arithmetic variable name is invalid",
                "Use ASCII identifiers",
            )
        })?;
        self.index = target.operator_end;
        let current = match target.kind {
            Assignment::Plain => 0,
            Assignment::Compound(_) => self.read_variable(name)?,
        };
        let right = self.assignment(nested_depth(depth)?)?;
        let value = match target.kind {
            Assignment::Plain => right,
            Assignment::Compound(operator) => {
                apply_binary(operator, current, right).ok_or_else(|| self.division_error())?
            }
        };
        if self.evaluate {
            self.variables.set(name, value)?;
        }
        Ok(value)
    }

    /// Ternary `condition ? then : else`. It is right-associative, and only the
    /// taken branch performs assignments.
    fn conditional(&mut self, depth: usize) -> Result<i64, ShellError> {
        let condition = self.binary(1, depth)?;
        if !self.consume_operator("?") {
            return Ok(condition);
        }
        let branch_depth = nested_depth(depth)?;
        let taken = condition != 0;
        let then_value = self.with_evaluation(taken, |parser| parser.comma(branch_depth))?;
        if !self.consume_operator(":") {
            return Err(self.error(
                "missing `:` in conditional expression",
                "Write the conditional as `condition ? then : else`",
            ));
        }
        let else_value = self.with_evaluation(!taken, |parser| parser.conditional(branch_depth))?;
        Ok(if taken { then_value } else { else_value })
    }

    /// Left-associative binary operators by precedence climbing. The loop
    /// keeps the operators whose precedence is at least `minimum_precedence`.
    fn binary(&mut self, minimum_precedence: u8, depth: usize) -> Result<i64, ShellError> {
        let mut left = self.power(depth)?;
        loop {
            let Some(token) = self.peek_operator() else {
                return Ok(left);
            };
            let Some(operator) = Binary::from_token(token) else {
                return Ok(left);
            };
            if operator.precedence() < minimum_precedence {
                return Ok(left);
            }
            self.advance(token.len());
            let right_precedence = operator.precedence().saturating_add(1);
            let right = if operator.skips_right(left) {
                self.with_evaluation(false, |parser| parser.binary(right_precedence, depth))?
            } else {
                self.binary(right_precedence, depth)?
            };
            left = apply_binary(operator, left, right).ok_or_else(|| self.division_error())?;
        }
    }

    /// Exponentiation `base ** exponent`. The base is a signed primary, so
    /// `-2**2` is `4` as in Bash. The operator is right-associative, and the
    /// exponent is a power expression that may carry its own sign.
    fn power(&mut self, depth: usize) -> Result<i64, ShellError> {
        let base = self.signed_primary(depth)?;
        if !self.consume_operator("**") {
            return Ok(base);
        }
        let exponent = self.power(nested_depth(depth)?)?;
        let exponent = u64::try_from(exponent).map_err(|_| {
            self.error(
                "negative exponent in arithmetic expansion",
                "Use an exponent of zero or more",
            )
        })?;
        Ok(pow_wrapping(base, exponent))
    }

    /// A primary with any leading `+`, `-`, `!`, or `~` applied. Each prefix
    /// adds one nesting level.
    fn signed_primary(&mut self, depth: usize) -> Result<i64, ShellError> {
        match self.peek_operator() {
            Some(token @ ("+" | "-" | "!" | "~")) => {
                self.advance(token.len());
                let operand = self.signed_primary(nested_depth(depth)?)?;
                Ok(apply_unary(token, operand))
            }
            _ => self.primary(depth),
        }
    }

    /// A parenthesized expression, an integer literal, or a variable read.
    fn primary(&mut self, depth: usize) -> Result<i64, ShellError> {
        self.skip_whitespace();
        match self.input.get(self.index).copied() {
            Some(b'(') => {
                self.advance(1);
                let value = self.comma(nested_depth(depth)?)?;
                self.skip_whitespace();
                if self.input.get(self.index) != Some(&b')') {
                    return Err(self.error(
                        "unbalanced parentheses in arithmetic expansion",
                        "Balance parentheses in `$((...))`",
                    ));
                }
                self.advance(1);
                Ok(value)
            }
            Some(byte) if byte.is_ascii_digit() => self.literal(),
            Some(byte) if is_identifier_start(byte) => self.variable(),
            Some(_) => Err(self.unexpected_token()),
            None => Err(self.error(
                "missing operand in arithmetic expansion",
                "Use integer literals, variable names, or parentheses",
            )),
        }
    }

    /// Read an alphanumeric token and parse it as a literal. Reading the whole
    /// run first makes `08` and `12abc` one bad token rather than two tokens.
    fn literal(&mut self) -> Result<i64, ShellError> {
        let source = self.source;
        let start = self.index;
        while self
            .input
            .get(self.index)
            .is_some_and(u8::is_ascii_alphanumeric)
        {
            self.advance(1);
        }
        let text = source.get(start..self.index).unwrap_or_default();
        parse_literal(text).ok_or_else(|| {
            self.error(
                &format!("invalid integer literal `{text}` in arithmetic expansion"),
                "Use decimal, octal with a leading 0, or hex with 0x; values must fit in 64 bits",
            )
        })
    }

    /// Read a variable name and return its value.
    fn variable(&mut self) -> Result<i64, ShellError> {
        let source = self.source;
        let start = self.index;
        let end = identifier_end(self.input, start).ok_or_else(|| self.unexpected_token())?;
        let name = source.get(start..end).unwrap_or_default();
        self.index = end;
        self.read_variable(name)
    }

    /// Value of a variable. Unset or blank content reads as 0. Other content
    /// must be an integer literal, and anything else is an error. Content is
    /// never evaluated as an expression.
    fn read_variable(&self, name: &str) -> Result<i64, ShellError> {
        let Some(text) = self.variables.get(name) else {
            return Ok(0);
        };
        if text.trim().is_empty() {
            return Ok(0);
        }
        parse_variable_value(&text).ok_or_else(|| {
            self.error(
                &format!("`{name}` is not an integer: `{text}`"),
                "Store a decimal, octal (leading 0), or hex (0x) integer in the variable, or unset it",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;

    /// Variable table backed by string values, the same shape the executor uses.
    #[derive(Default)]
    struct Variables(BTreeMap<String, String>);

    impl Variables {
        /// Build a table from `(name, value)` pairs.
        fn with(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
            )
        }

        /// Stored value of `name`, if any.
        fn value(&self, name: &str) -> Option<&str> {
            self.0.get(name).map(String::as_str)
        }
    }

    impl ArithmeticVariables for Variables {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }

        fn set(&mut self, name: &str, value: i64) -> Result<(), ShellError> {
            self.0.insert(name.to_owned(), value.to_string());
            Ok(())
        }
    }

    /// Variable table whose `set` always fails, to prove the error propagates.
    struct RejectingVariables;

    impl ArithmeticVariables for RejectingVariables {
        fn get(&self, _name: &str) -> Option<String> {
            None
        }

        fn set(&mut self, _name: &str, _value: i64) -> Result<(), ShellError> {
            Err(ShellError::new(
                ErrorCode::InvalidCommand,
                "store rejected the write",
            ))
        }
    }

    /// Evaluate `source` with no variables set.
    fn evaluate(source: &str) -> Result<i64, ShellError> {
        evaluate_arithmetic(source, &mut Variables::default())
    }

    /// Evaluate `source` against `variables`, returning the table for inspection.
    fn evaluate_in(source: &str, mut variables: Variables) -> (Result<i64, ShellError>, Variables) {
        let result = evaluate_arithmetic(source, &mut variables);
        (result, variables)
    }

    /// Evaluate `source` and require success.
    fn value(source: &str) -> i64 {
        evaluate(source).unwrap_or_else(|error| panic!("`{source}` failed: {error:?}"))
    }

    /// Evaluate `source` and require an error, returning it.
    fn rejected(source: &str) -> ShellError {
        evaluate(source).expect_err("expected an arithmetic error")
    }

    #[test]
    fn decimal_octal_and_hex_literals_have_their_base_values() {
        assert_eq!(value("42"), 42);
        assert_eq!(value("0"), 0);
        assert_eq!(value("010"), 8);
        assert_eq!(value("0x1F"), 31);
        assert_eq!(value("0X1f"), 31);
        assert_eq!(value("0xff"), 255);
    }

    #[test]
    fn literals_beyond_i64_max_reinterpret_as_two_complement_like_bash() {
        assert_eq!(value("0xFFFFFFFFFFFFFFFF"), -1);
        assert_eq!(value("9223372036854775808"), i64::MIN);
        assert_eq!(value("0x8000000000000000"), i64::MIN);
    }

    #[test]
    fn malformed_literals_are_rejected_with_help_and_source_context() {
        for source in ["08", "0x", "12abc", "1.5", "18446744073709551616", "0xG"] {
            let error = rejected(source);
            assert_eq!(error.code, ErrorCode::InvalidCommand, "source `{source}`");
            assert!(
                !error.details.help.is_empty(),
                "source `{source}` must carry help"
            );
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|line| line.contains(source)),
                "source `{source}` must appear in context"
            );
        }
    }

    #[test]
    fn whitespace_including_newlines_separates_tokens_anywhere() {
        assert_eq!(value(" \t1\n+\t2 \n"), 3);
        assert_eq!(value("(\n1 ,\t2\n)"), 2);
    }

    #[test]
    fn empty_or_blank_expression_evaluates_to_zero() {
        assert_eq!(value(""), 0);
        assert_eq!(value("  \t\n"), 0);
    }

    #[test]
    fn multiplication_binds_tighter_than_addition_and_operators_associate_left() {
        assert_eq!(value("1+2*3"), 7);
        assert_eq!(value("(1+2)*3"), 9);
        assert_eq!(value("10-4-3"), 3);
        assert_eq!(value("100/10/5"), 2);
        assert_eq!(value("7%4*2"), 6);
    }

    #[test]
    fn shift_relational_and_bitwise_precedence_follows_c() {
        assert_eq!(value("1<<2+1"), 8);
        assert_eq!(value("1+1<<2"), 8);
        assert_eq!(value("3>2==1"), 1);
        assert_eq!(value("1|2&3"), 3);
        assert_eq!(value("1^3&2"), 3);
        assert_eq!(value("6&3==3"), 0);
    }

    #[test]
    fn bitwise_operators_act_on_every_bit() {
        assert_eq!(value("12&10"), 8);
        assert_eq!(value("12|3"), 15);
        assert_eq!(value("12^10"), 6);
        assert_eq!(value("~0"), -1);
        assert_eq!(value("~5"), -6);
    }

    #[test]
    fn unary_operators_negate_complement_and_negate_logically() {
        assert_eq!(value("-5"), -5);
        assert_eq!(value("+5"), 5);
        assert_eq!(value("!0"), 1);
        assert_eq!(value("!7"), 0);
        assert_eq!(value("- -3"), 3);
        assert_eq!(value("1 - -1"), 2);
        assert_eq!(value("1 + +2"), 3);
    }

    #[test]
    fn comparisons_yield_zero_or_one() {
        assert_eq!(value("3<4"), 1);
        assert_eq!(value("4<3"), 0);
        assert_eq!(value("3<=3"), 1);
        assert_eq!(value("4>=5"), 0);
        assert_eq!(value("5>4"), 1);
        assert_eq!(value("5>=5"), 1);
        assert_eq!(value("2==2"), 1);
        assert_eq!(value("2!=2"), 0);
    }

    #[test]
    fn logical_operators_yield_zero_or_one_and_short_circuit_on_the_left() {
        assert_eq!(value("0 && 5"), 0);
        assert_eq!(value("2 && 5"), 1);
        assert_eq!(value("0 || 0"), 0);
        assert_eq!(value("3 || 0"), 1);
        assert_eq!(value("1 || 0 && 0"), 1);
    }

    #[test]
    fn shift_counts_are_taken_modulo_64() {
        assert_eq!(value("1<<64"), 1);
        assert_eq!(value("1<<65"), 2);
        assert_eq!(value("1<<-1"), i64::MIN);
        assert_eq!(value("-8>>1"), -4);
        assert_eq!(value("8>>66"), 2);
    }

    #[test]
    fn addition_multiplication_and_negation_wrap_on_overflow() {
        assert_eq!(value("9223372036854775807+1"), i64::MIN);
        assert_eq!(value("9223372036854775807*2"), -2);
        assert_eq!(value("-(-9223372036854775807-1)"), i64::MIN);
    }

    #[test]
    fn division_and_remainder_truncate_toward_zero() {
        assert_eq!(value("-7/2"), -3);
        assert_eq!(value("-7%2"), -1);
        assert_eq!(value("7/-2"), -3);
        assert_eq!(value("7%-2"), 1);
    }

    #[test]
    fn minimum_divided_by_negative_one_wraps_as_bash_does() {
        assert_eq!(value("(-9223372036854775807-1)/-1"), i64::MIN);
        assert_eq!(value("(-9223372036854775807-1)%-1"), 0);
    }

    #[test]
    fn division_by_zero_is_an_error_even_on_an_unevaluated_side() {
        for source in ["1/0", "1%0", "5/(3-3)", "0 && 1/0", "1 || 1%0"] {
            let error = rejected(source);
            assert_eq!(error.code, ErrorCode::InvalidCommand, "source `{source}`");
            assert!(
                error.message.contains("division by zero"),
                "{}",
                error.message
            );
        }
        let error = evaluate_in("x/=0", Variables::with(&[("x", "4")])).0;
        assert!(error.is_err());
    }

    #[test]
    fn exponent_is_right_associative_and_binds_tighter_than_multiplication() {
        assert_eq!(value("2**3**2"), 512);
        assert_eq!(value("3*2**3"), 24);
        assert_eq!(value("0**0"), 1);
        assert_eq!(value("2**63"), i64::MIN);
        assert_eq!(value("2**2**0"), 2);
    }

    #[test]
    fn exponent_sign_binds_to_the_base_as_bash_does() {
        assert_eq!(value("-2**2"), 4);
        assert_eq!(value("(-2)**3"), -8);
    }

    #[test]
    fn negative_exponent_is_an_error_naming_the_exponent() {
        let error = rejected("2**-1");
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert!(
            error.message.contains("negative exponent"),
            "{}",
            error.message
        );
    }

    #[test]
    fn ternary_is_right_associative_and_selects_one_branch() {
        assert_eq!(value("1?2:3?4:5"), 2);
        assert_eq!(value("0?2:3?4:5"), 4);
        assert_eq!(value("0?2:0?4:5"), 5);
        assert_eq!(value("1?(2,3):4"), 3);
    }

    #[test]
    fn ternary_performs_assignments_only_in_the_taken_branch() {
        let (result, vars) = evaluate_in("1 ? (x=1) : (y=2)", Variables::default());
        assert_eq!(result.expect("taken branch succeeds"), 1);
        assert_eq!(vars.value("x"), Some("1"));
        assert_eq!(vars.value("y"), None);

        let (result, vars) = evaluate_in("0 ? (x=1) : (y=2)", Variables::default());
        assert_eq!(result.expect("else branch succeeds"), 2);
        assert_eq!(vars.value("x"), None);
        assert_eq!(vars.value("y"), Some("2"));
    }

    #[test]
    fn assignment_chains_are_right_associative_and_store_each_value() {
        let (result, vars) = evaluate_in("a=b=3", Variables::default());
        assert_eq!(result.expect("chain succeeds"), 3);
        assert_eq!(vars.value("a"), Some("3"));
        assert_eq!(vars.value("b"), Some("3"));
    }

    #[test]
    fn compound_assignments_update_the_stored_variable() {
        let cases: [(&str, i64); 10] = [
            ("x+=2", 12),
            ("x-=2", 8),
            ("x*=3", 30),
            ("x/=4", 2),
            ("x%=4", 2),
            ("x<<=2", 40),
            ("x>>=1", 5),
            ("x&=6", 2),
            ("x^=3", 9),
            ("x|=5", 15),
        ];
        for (source, expected) in cases {
            let (result, vars) = evaluate_in(source, Variables::with(&[("x", "10")]));
            assert_eq!(result.expect(source), expected, "source `{source}`");
            assert_eq!(
                vars.value("x"),
                Some(expected.to_string().as_str()),
                "stored value after `{source}`"
            );
        }
    }

    #[test]
    fn comma_operator_evaluates_left_to_right_and_yields_the_last_value() {
        let (result, vars) = evaluate_in("x=1, y=x+1, y*10", Variables::default());
        assert_eq!(result.expect("comma sequence succeeds"), 20);
        assert_eq!(vars.value("x"), Some("1"));
        assert_eq!(vars.value("y"), Some("2"));
    }

    #[test]
    fn set_receives_each_assignment_in_source_order() {
        let (result, vars) = evaluate_in("x=1, x=x+1, x*10", Variables::default());
        assert_eq!(result.expect("ordered assignments succeed"), 20);
        assert_eq!(vars.value("x"), Some("2"));
    }

    #[test]
    fn assignment_target_must_be_a_plain_variable() {
        for source in ["1=2", "(x)=2", "x+1=2", "x?1:2=3"] {
            let error = rejected(source);
            assert!(
                error
                    .message
                    .contains("assignment target must be a variable"),
                "source `{source}`: {}",
                error.message
            );
        }
    }

    #[test]
    fn short_circuit_skips_assignments_on_the_unevaluated_side() {
        let (result, vars) = evaluate_in("0 && (x=5)", Variables::default());
        assert_eq!(result.expect("short circuit succeeds"), 0);
        assert_eq!(vars.value("x"), None);

        let (result, vars) = evaluate_in("1 || (x=5)", Variables::default());
        assert_eq!(result.expect("short circuit succeeds"), 1);
        assert_eq!(vars.value("x"), None);
    }

    #[test]
    fn logical_right_side_assigns_when_it_is_evaluated() {
        let (result, vars) = evaluate_in("1 && (x=5)", Variables::default());
        assert_eq!(result.expect("evaluated right side succeeds"), 1);
        assert_eq!(vars.value("x"), Some("5"));
    }

    #[test]
    fn variables_holding_hex_octal_and_signed_values_read_as_integers() {
        let vars = Variables::with(&[("x", "0x10"), ("y", "010"), ("z", "-3"), ("w", " +7 ")]);
        let (result, _) = evaluate_in("x+y+z+w", vars);
        assert_eq!(result.expect("integer variables read"), 16 + 8 - 3 + 7);
        assert_eq!(value_with("x*2", &[("x", "-0x10")]), -32);
    }

    #[test]
    fn unset_or_blank_variables_read_as_zero() {
        assert_eq!(evaluate("unset_name").expect("unset reads as zero"), 0);
        assert_eq!(value_with("e", &[("e", "")]), 0);
        assert_eq!(value_with("b+1", &[("b", "   ")]), 1);
    }

    #[test]
    fn variable_with_non_integer_content_is_an_error_naming_the_variable() {
        for (source, content) in [("x", "abc"), ("x", "08"), ("x+1", "1.5")] {
            let error = evaluate_in(source, Variables::with(&[("x", content)])).0;
            let error = error.expect_err("non-integer content must fail");
            assert_eq!(error.code, ErrorCode::InvalidCommand);
            assert!(
                error
                    .message
                    .contains(&format!("`x` is not an integer: `{content}`")),
                "{}",
                error.message
            );
        }
    }

    #[test]
    fn variable_content_is_read_as_data_and_never_evaluated_as_an_expression() {
        let error = evaluate_in("x", Variables::with(&[("x", "y"), ("y", "5")])).0;
        assert!(error.is_err(), "a name in a variable must not be followed");
    }

    #[test]
    fn variable_assignment_error_from_the_store_propagates_unchanged() {
        let error = evaluate_arithmetic("x=1", &mut RejectingVariables)
            .expect_err("store failure must propagate");
        assert_eq!(error.message, "store rejected the write");
    }

    #[test]
    fn identifiers_may_contain_underscores_and_digits_after_the_first_character() {
        let (result, _) = evaluate_in("_a1 = 4, _a1*2", Variables::default());
        assert_eq!(result.expect("identifier assignment succeeds"), 8);
    }

    #[test]
    fn parentheses_nest_up_to_the_depth_limit_and_no_further() {
        let at_limit = format!(
            "{}1{}",
            "(".repeat(ARITHMETIC_DEPTH_MAX),
            ")".repeat(ARITHMETIC_DEPTH_MAX)
        );
        assert_eq!(
            evaluate(&at_limit).expect("depth at the limit is accepted"),
            1
        );

        let over_limit = format!(
            "{}1{}",
            "(".repeat(ARITHMETIC_DEPTH_MAX + 1),
            ")".repeat(ARITHMETIC_DEPTH_MAX + 1)
        );
        let error = rejected(&over_limit);
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|line| line.contains("limit 64"))
        );
    }

    #[test]
    fn prefix_operator_chains_trip_the_depth_limit() {
        // A space keeps each sign a separate token; `--` would lex as the
        // unsupported decrement operator instead.
        let source = format!("{}1", "- ".repeat(ARITHMETIC_DEPTH_MAX * 2));
        assert_eq!(rejected(&source).code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn ternary_chains_trip_the_depth_limit() {
        let source = format!("{}2", "0?1:".repeat(ARITHMETIC_DEPTH_MAX * 2));
        assert_eq!(rejected(&source).code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn assignment_chains_trip_the_depth_limit() {
        let source = format!("{}1", "a=".repeat(ARITHMETIC_DEPTH_MAX * 2));
        assert_eq!(rejected(&source).code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn exponent_chains_trip_the_depth_limit() {
        let source = format!("{}1", "2**".repeat(ARITHMETIC_DEPTH_MAX * 2));
        assert_eq!(rejected(&source).code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn source_longer_than_the_byte_limit_is_a_resource_limit_error() {
        let source = format!("1{}", "+1".repeat(ARITHMETIC_SOURCE_BYTES_MAX));
        let error = rejected(&source);
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|line| line.contains("16384"))
        );
    }

    #[test]
    fn source_exactly_at_the_byte_limit_is_accepted() {
        let source = format!("1{}", " ".repeat(ARITHMETIC_SOURCE_BYTES_MAX - 1));
        assert_eq!(source.len(), ARITHMETIC_SOURCE_BYTES_MAX);
        assert_eq!(value(&source), 1);
    }

    #[test]
    fn malformed_expressions_return_errors_without_panicking() {
        for source in [
            "+", "(", ")", "1 2", "1 +", "x y", "1 ? 2", "$(1)", "#", "\"1\"", "1 ++ 2", "x++",
            "1 ==", "(1", "2 ** ", "1 @ 2", "é", "1 ? 2 : ", ",", "x,", "~", "!", "1)", "x=",
        ] {
            assert!(evaluate(source).is_err(), "`{source}` must be rejected");
        }
    }

    #[test]
    fn increment_and_decrement_operators_are_rejected_by_name() {
        for source in ["++1", "x++", "1--1"] {
            let error = rejected(source);
            assert!(
                error.message.contains("increment and decrement"),
                "source `{source}`: {}",
                error.message
            );
        }
    }

    #[test]
    fn side_effect_free_expressions_match_the_posix_shell() {
        if !Path::new("/bin/sh").exists() {
            return;
        }
        let expressions = [
            "1+2*3",
            "(1+2)*3",
            "x*x-1",
            "x<<2|1",
            "x>>1",
            "x&3",
            "x^2",
            "~x",
            "!x",
            "-x%4",
            "x/2",
            "-x/2",
            "x%-3",
            "x==7",
            "x!=7",
            "x<=6",
            "x>=7",
            "x<8 && x>6",
            "0 || x",
            "x ? 10 : 20",
            "0 ? 1 : x ? 2 : 3",
            "010 + 0x10",
            "0x7f & 0177",
            "(x,3)",
            "1 - -x",
            "x*(x+1)/2",
            "(-9223372036854775807 - 1) / 2",
            "x > 3 ? x - 3 : 3 - x",
            "1 << 62 >> 61",
            "0xff ^ 0x0f",
            "2 * 3 % 4",
            "x - 10 + 3",
            "((((x))))",
            "x | 8 & 12 ^ 1",
            "-7 >> 1",
        ];
        assert!(expressions.len() >= 30);
        for expression in expressions {
            let script = format!("x=7; echo $(({expression}))");
            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .output()
                .expect("/bin/sh runs");
            assert!(output.status.success(), "/bin/sh failed on `{expression}`");
            let reference = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<i64>()
                .expect("/bin/sh prints an integer");
            let ours = evaluate_in(expression, Variables::with(&[("x", "7")]))
                .0
                .expect("expression evaluates");
            assert_eq!(ours, reference, "expression `{expression}`");
        }
    }

    /// Evaluate `source` with the given variable pairs, requiring success.
    fn value_with(source: &str, pairs: &[(&str, &str)]) -> i64 {
        evaluate_in(source, Variables::with(pairs))
            .0
            .unwrap_or_else(|error| panic!("`{source}` failed: {error:?}"))
    }
}
