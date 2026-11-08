//! In-process evaluation of the POSIX `test` and `[` utilities.
//!
//! Shell conditions use this module instead of forking `/bin/[`. The entry
//! point takes words that are already expanded, applies the POSIX
//! argument-count rules for up to four operands, and parses longer
//! expressions with `!` binding tighter than `-a`, which binds tighter than
//! `-o`. Bash's builtin `test` is the reference wherever POSIX leaves the
//! behavior open.
//!
//! Resource bounds: an expression may have at most `OPERANDS_MAX` operands
//! and at most `NESTING_DEPTH_MAX` open `(` groups and `!` prefixes. The
//! parser descends only through those two forms and checks the depth before
//! each descent, so input cannot drive unbounded recursion or allocation. The
//! only side effects are filesystem metadata reads, `access(2)` checks, and
//! terminal queries on descriptors 0 to 2.

#![allow(
    dead_code,
    reason = "wired into the native executor by the native-language integration"
)]

#[cfg(unix)]
use nix::unistd::{AccessFlags, access};
use quirl_core::{ErrorCode, ShellError};
#[cfg(unix)]
use rustix::process::{getegid, geteuid};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::{fs, io::IsTerminal, time::SystemTime};

/// Largest number of operand words, excluding the command name and any closing `]`.
const OPERANDS_MAX: usize = 4096;

/// Deepest permitted nesting of `(` groups and `!` prefixes combined.
const NESTING_DEPTH_MAX: usize = 64;

/// Set-user-ID permission bit in a Unix file mode.
#[cfg(unix)]
const MODE_SET_USER_ID: u32 = 0o4000;

/// Set-group-ID permission bit in a Unix file mode.
#[cfg(unix)]
const MODE_SET_GROUP_ID: u32 = 0o2000;

/// Sticky permission bit in a Unix file mode.
#[cfg(unix)]
const MODE_STICKY: u32 = 0o1000;

/// Unary operators that inspect a path through [`file_predicate`].
const FILE_UNARY_OPERATORS: &[&str] = &[
    "-e", "-f", "-d", "-r", "-w", "-x", "-s", "-L", "-h", "-p", "-S", "-b", "-c", "-g", "-u", "-k",
    "-O", "-G", "-N",
];

/// Binary operators that compare two operands, excluding the logical `-a` and `-o`.
const BINARY_OPERATORS: &[&str] = &[
    "=", "==", "!=", "<", ">", "-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef",
];

/// Outcome of evaluating one `test` or `[` invocation.
///
/// Maps to the shell exit status: [`TestOutcome::True`] is status 0 and
/// [`TestOutcome::False`] is status 1. A malformed invocation is not an
/// outcome; it is reported as a [`ShellError`] that maps to status 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestOutcome {
    /// The expression holds; the exit status is 0.
    True,
    /// The expression does not hold; the exit status is 1.
    False,
}

/// Evaluates a `test` or `[` invocation whose words are already expanded.
///
/// `words` includes the command name: `["test", ...]` or `["[", ..., "]"]`.
/// For `[`, the final word must be `]` and is not an operand. For `test`, a
/// trailing `]` is an ordinary operand. Strings are compared byte-wise, and
/// integer operands are signed 64-bit decimals with optional surrounding
/// whitespace.
///
/// # Errors
///
/// Returns `InvalidCommand` when the first word is neither `test` nor `[`,
/// `InvalidArgument` for a missing `]`, an unknown operator, a non-integer
/// operand to an integer comparison, or a malformed expression, and
/// `ResourceLimit` when the operand count or nesting depth exceeds its bound.
pub(crate) fn evaluate_test(words: &[String]) -> Result<TestOutcome, ShellError> {
    let operands = operands_of(words)?;
    if operands.len() > OPERANDS_MAX {
        return Err(operand_limit(operands.len()));
    }
    let holds = evaluate_operands(operands)?;
    Ok(if holds {
        TestOutcome::True
    } else {
        TestOutcome::False
    })
}

/// Strips the command name and, for `[`, the closing `]`, leaving the operands.
fn operands_of(words: &[String]) -> Result<&[String], ShellError> {
    match words.split_first() {
        Some((name, operands)) if name.as_str() == "test" => Ok(operands),
        Some((name, operands)) if name.as_str() == "[" => match operands.split_last() {
            Some((closing, inner)) if closing.as_str() == "]" => Ok(inner),
            _ => Err(missing_bracket()),
        },
        Some((name, _)) => Err(ShellError::new(
            ErrorCode::InvalidCommand,
            format!("test builtin invoked as `{name}`"),
        )
        .with_help("Invoke this builtin as `test` or `[`")),
        None => Err(ShellError::new(
            ErrorCode::InvalidCommand,
            "test builtin invoked without a command name",
        )
        .with_help("Pass `test` or `[` as the first word")),
    }
}

/// Dispatches on operand count, applying the POSIX rules for one to four
/// operands before falling back to the precedence parser.
fn evaluate_operands(operands: &[String]) -> Result<bool, ShellError> {
    match operands {
        [] => Ok(false),
        [word] => Ok(!word.is_empty()),
        [op, operand] => two_operands(op, operand),
        [_, _, _] => three_operands(operands),
        [bang, rest @ ..] if bang.as_str() == "!" && rest.len() == 3 => Ok(!three_operands(rest)?),
        [open, first, second, close] if open.as_str() == "(" && close.as_str() == ")" => {
            two_operands(first, second)
        }
        _ => Parser::new(operands).parse_expression(),
    }
}

/// Evaluates the two-operand form: `! word` or a unary operator applied to a word.
///
/// `! x` is the negation of the one-operand test, so it holds exactly when `x`
/// is empty.
fn two_operands(op: &str, operand: &str) -> Result<bool, ShellError> {
    if op == "!" {
        return Ok(operand.is_empty());
    }
    if is_unary(op) {
        return unary(op, operand);
    }
    Err(not_an_operator(op, operand))
}

/// Evaluates the three-operand form, which takes precedence over longer
/// expression parsing.
///
/// A middle `-a` or `-o` is a logical connective, and a middle binary operator
/// makes a binary test even when the first word is `!` or `(`. This is the
/// POSIX rule that makes `[ = = = ]` a comparison of `=` with `=`. Anything
/// else falls back to the precedence parser, which reports the unparsed word.
fn three_operands(words: &[String]) -> Result<bool, ShellError> {
    let [left, op, right] = words else {
        return Parser::new(words).parse_expression();
    };
    match op.as_str() {
        "-a" => Ok(!left.is_empty() && !right.is_empty()),
        "-o" => Ok(!left.is_empty() || !right.is_empty()),
        _ if is_binary(op) => binary(left, op, right),
        _ if left.as_str() == "!" => two_operands(op, right).map(|holds| !holds),
        _ if left.as_str() == "(" && right.as_str() == ")" => Ok(!op.is_empty()),
        _ if is_unknown_operator(op) => Err(unknown_binary(op)),
        _ => Parser::new(words).parse_expression(),
    }
}

/// Evaluates a binary operator over two operands.
///
/// Integer operators are checked before either comparison runs, so a malformed
/// integer is reported regardless of the operator's result.
fn binary(left: &str, op: &str, right: &str) -> Result<bool, ShellError> {
    match op {
        "=" | "==" => Ok(left == right),
        "!=" => Ok(left != right),
        "<" => Ok(left < right),
        ">" => Ok(left > right),
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => compare_integers(left, op, right),
        "-nt" => Ok(modified_after(left, right)),
        "-ot" => Ok(modified_after(right, left)),
        "-ef" => Ok(same_file(left, right)),
        _ => Err(unknown_binary(op)),
    }
}

/// Compares two signed integer operands with one of the six integer operators.
fn compare_integers(left: &str, op: &str, right: &str) -> Result<bool, ShellError> {
    let left = parse_integer(left)?;
    let right = parse_integer(right)?;
    Ok(match op {
        "-eq" => left == right,
        "-ne" => left != right,
        "-lt" => left < right,
        "-le" => left <= right,
        "-gt" => left > right,
        "-ge" => left >= right,
        _ => return Err(unknown_binary(op)),
    })
}

/// Parses an optionally signed decimal `i64`, ignoring surrounding whitespace.
///
/// # Errors
///
/// Returns `InvalidArgument` naming the word when it is not an integer in the
/// `i64` range.
fn parse_integer(word: &str) -> Result<i64, ShellError> {
    word.trim()
        .parse::<i64>()
        .map_err(|_| integer_expected(word))
}

/// Evaluates a unary operator. String operators are handled here; file
/// operators are delegated to [`file_predicate`].
fn unary(op: &str, operand: &str) -> Result<bool, ShellError> {
    match op {
        "-z" => Ok(operand.is_empty()),
        "-n" => Ok(!operand.is_empty()),
        "-t" => terminal_descriptor(operand),
        _ if FILE_UNARY_OPERATORS.contains(&op) => Ok(file_predicate(op, operand)),
        _ => Err(unknown_unary(op)),
    }
}

/// Reports whether a file descriptor is a terminal.
///
/// Only descriptors 0, 1, and 2 can be terminals in the shell's view; any other
/// number is valid syntax and yields false.
fn terminal_descriptor(operand: &str) -> Result<bool, ShellError> {
    let descriptor = parse_integer(operand)?;
    Ok(match descriptor {
        0 => std::io::stdin().is_terminal(),
        1 => std::io::stdout().is_terminal(),
        2 => std::io::stderr().is_terminal(),
        _ => false,
    })
}

/// Reports whether `path` is a symbolic link, without following it.
fn is_symbolic_link(path: &str) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Evaluates a file operator on `path`. A missing path makes every operator
/// except `-L` and `-h` false.
///
/// `-L` and `-h` inspect the link itself, so a dangling link still satisfies
/// them. Other operators follow symbolic links. `-r`, `-w`, and `-x` use the
/// real user ID through `access(2)`, which matches the effective ID for a
/// process that is not setuid.
#[cfg(unix)]
fn file_predicate(op: &str, path: &str) -> bool {
    if matches!(op, "-L" | "-h") {
        return is_symbolic_link(path);
    }
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let file_type = metadata.file_type();
    let mode = metadata.mode();
    match op {
        "-e" => true,
        "-f" => file_type.is_file(),
        "-d" => file_type.is_dir(),
        "-s" => metadata.len() > 0,
        "-r" => access(path, AccessFlags::R_OK).is_ok(),
        "-w" => access(path, AccessFlags::W_OK).is_ok(),
        "-x" => access(path, AccessFlags::X_OK).is_ok(),
        "-p" => file_type.is_fifo(),
        "-S" => file_type.is_socket(),
        "-b" => file_type.is_block_device(),
        "-c" => file_type.is_char_device(),
        "-g" => mode & MODE_SET_GROUP_ID != 0,
        "-u" => mode & MODE_SET_USER_ID != 0,
        "-k" => mode & MODE_STICKY != 0,
        "-O" => metadata.uid() == geteuid().as_raw(),
        "-G" => metadata.gid() == getegid().as_raw(),
        "-N" => metadata
            .modified()
            .ok()
            .zip(metadata.accessed().ok())
            .is_some_and(|(modified, accessed)| modified > accessed),
        _ => false,
    }
}

/// Evaluates a file operator on `path` where Unix permission bits are not
/// available. Permission operators approximate with existence, and operators
/// that need Unix file types are false.
#[cfg(not(unix))]
fn file_predicate(op: &str, path: &str) -> bool {
    if matches!(op, "-L" | "-h") {
        return is_symbolic_link(path);
    }
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    match op {
        "-e" | "-r" | "-w" | "-x" => true,
        "-f" => metadata.is_file(),
        "-d" => metadata.is_dir(),
        "-s" => metadata.len() > 0,
        _ => false,
    }
}

/// Reports whether `newer` has a modification time after `older`.
///
/// A file that exists is newer than one that does not, and a missing file is
/// never newer than anything.
fn modified_after(newer: &str, older: &str) -> bool {
    match (modified_time(newer), modified_time(older)) {
        (Some(newer_time), Some(older_time)) => newer_time > older_time,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Returns the modification time of `path`, or `None` when it cannot be read.
fn modified_time(path: &str) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// Reports whether both paths name the same file on the same device.
#[cfg(unix)]
fn same_file(left: &str, right: &str) -> bool {
    match (fs::metadata(left), fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

/// Reports whether both paths resolve to the same canonical location. This is
/// the closest identity check available without device and inode numbers.
#[cfg(not(unix))]
fn same_file(left: &str, right: &str) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Reports whether `word` is a unary operator this module evaluates.
fn is_unary(word: &str) -> bool {
    matches!(word, "-z" | "-n" | "-t") || FILE_UNARY_OPERATORS.contains(&word)
}

/// Reports whether `word` is a binary operator this module evaluates.
fn is_binary(word: &str) -> bool {
    BINARY_OPERATORS.contains(&word)
}

/// Reports whether `word` is a logical connective.
fn is_logical(word: &str) -> bool {
    matches!(word, "-a" | "-o")
}

/// Reports whether `word` has the shape of an operator, a dash followed by
/// letters, without being a known operator. Such words are rejected by name
/// rather than silently treated as operands.
fn is_unknown_operator(word: &str) -> bool {
    !is_unary(word) && !is_binary(word) && !is_logical(word) && looks_like_operator(word)
}

/// Reports whether `word` is a dash followed by one or more ASCII letters.
fn looks_like_operator(word: &str) -> bool {
    word.strip_prefix('-')
        .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_alphabetic()))
}

/// Recursive-descent parser for expressions longer than the POSIX fixed forms.
///
/// Grammar, lowest precedence first:
///
/// ```text
/// or      := and ("-o" and)*
/// and     := not ("-a" not)*
/// not     := "!" not | primary
/// primary := "(" or ")" | binary | unary operand | operand
/// ```
///
/// Recursion happens only through `(` and `!`. Each descent is counted against
/// `NESTING_DEPTH_MAX` before it happens, so deep input fails with
/// `ResourceLimit` instead of growing the stack. Both operands of `-a` and `-o`
/// are always parsed and evaluated, so a malformed integer on the right is
/// reported even when the left operand has already decided the result.
struct Parser<'a> {
    /// Words not yet consumed.
    rest: &'a [String],
    /// Number of `(` groups and `!` prefixes currently open.
    depth: usize,
}

impl<'a> Parser<'a> {
    /// Starts a parse over `words` with no nesting.
    fn new(words: &'a [String]) -> Self {
        Self {
            rest: words,
            depth: 0,
        }
    }

    /// Parses the whole slice as one expression. Any word left over after the
    /// expression is an error, because it cannot join the expression.
    fn parse_expression(mut self) -> Result<bool, ShellError> {
        let holds = self.parse_or()?;
        match self.rest.first() {
            None => Ok(holds),
            Some(word) => Err(unexpected_argument(word)),
        }
    }

    /// Parses an `-o` chain. Each operand is parsed and evaluated, so every
    /// syntax error in the chain is reported.
    fn parse_or(&mut self) -> Result<bool, ShellError> {
        let mut holds = self.parse_and()?;
        while self.next_is("-o") {
            self.consume(1);
            let right = self.parse_and()?;
            holds |= right;
        }
        Ok(holds)
    }

    /// Parses an `-a` chain of negated primaries.
    fn parse_and(&mut self) -> Result<bool, ShellError> {
        let mut holds = self.parse_not()?;
        while self.next_is("-a") {
            self.consume(1);
            let right = self.parse_not()?;
            holds &= right;
        }
        Ok(holds)
    }

    /// Parses zero or more `!` prefixes before a primary.
    fn parse_not(&mut self) -> Result<bool, ShellError> {
        if !self.negation_applies() {
            return self.parse_primary();
        }
        self.descend()?;
        self.consume(1);
        let holds = self.parse_not();
        self.ascend();
        Ok(!holds?)
    }

    /// Reports whether the next word is a `!` that negates what follows.
    ///
    /// A `!` before a binary operator is the left operand instead, so `! = x`
    /// compares the string `!` with `x`. A `!` with nothing after it is an
    /// ordinary operand.
    fn negation_applies(&self) -> bool {
        matches!(self.rest, [bang, second, ..] if bang.as_str() == "!" && !is_binary(second))
    }

    /// Parses one primary: a group, a binary test, a unary test, or a bare word.
    fn parse_primary(&mut self) -> Result<bool, ShellError> {
        let tail = self.rest;
        match tail {
            [] => Err(missing_operand()),
            [open, second, ..] if open.as_str() == "(" && !is_binary(second) => {
                self.descend()?;
                self.consume(1);
                let holds = self.parse_or();
                self.ascend();
                let holds = holds?;
                if !self.next_is(")") {
                    return Err(missing_close_paren());
                }
                self.consume(1);
                Ok(holds)
            }
            [left, op, right, ..] if is_binary(op) => {
                self.consume(3);
                binary(left, op, right)
            }
            [op, operand, ..] if is_unary(op) => {
                self.consume(2);
                unary(op, operand)
            }
            [word, ..] if is_logical(word) || is_binary(word) => Err(missing_operand_for(word)),
            [word, _, ..] if looks_like_operator(word) => Err(unknown_unary(word)),
            [word, ..] => {
                self.consume(1);
                Ok(!word.is_empty())
            }
        }
    }

    /// Returns true when the next word is `op`.
    fn next_is(&self, op: &str) -> bool {
        self.rest.first().is_some_and(|next| next.as_str() == op)
    }

    /// Drops the first `count` unconsumed words, or all of them if fewer remain.
    fn consume(&mut self, count: usize) {
        self.rest = self.rest.get(count..).unwrap_or_default();
    }

    /// Records one more level of nesting, failing before the parser recurses
    /// past `NESTING_DEPTH_MAX`.
    fn descend(&mut self) -> Result<(), ShellError> {
        let depth = self.depth.saturating_add(1);
        if depth > NESTING_DEPTH_MAX {
            return Err(nesting_limit(depth));
        }
        self.depth = depth;
        Ok(())
    }

    /// Leaves one level of nesting entered by [`Parser::descend`].
    fn ascend(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
}

/// Error for an operand count above `OPERANDS_MAX`, with the observed count.
fn operand_limit(observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("test expression has more than {OPERANDS_MAX} operands"),
    )
    .with_context(format!(
        "observed {observed} operands; limit is {OPERANDS_MAX}"
    ))
    .with_help("Split the condition into several `test` commands joined with `&&` or `||`")
}

/// Error for nesting deeper than `NESTING_DEPTH_MAX`, with the depth that was reached.
fn nesting_limit(observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("test expression nests deeper than {NESTING_DEPTH_MAX} levels"),
    )
    .with_context(format!(
        "observed nesting depth {observed}; limit is {NESTING_DEPTH_MAX}"
    ))
    .with_help("Flatten the parentheses and `!` prefixes, or split the condition into several `test` commands")
}

/// Error for a `[` invocation whose last word is not `]`.
fn missing_bracket() -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, "missing `]`")
        .with_help("Close the expression with `]`, for example `[ -f file ]`, or use `test`")
}

/// Error for an expression that ends where an operand was required.
fn missing_operand() -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        "expression ended where an operand was expected",
    )
    .with_help("Supply a value after each operator, for example `[ -n \"$name\" -a -f \"$path\" ]`")
}

/// Error for a logical or binary operator that has no operand before or after it.
fn missing_operand_for(op: &str) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("operator `{op}` is missing an operand"),
    )
    .with_help("Give each binary operator a value on each side, and each `-a` or `-o` a condition on each side")
}

/// Error for a `(` group with no matching `)`.
fn missing_close_paren() -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, "missing `)`")
        .with_help("Close each `(` with a matching `)`")
}

/// Error for a word that cannot join the expression.
fn unexpected_argument(word: &str) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("unexpected argument `{word}`"),
    )
    .with_help(
        "Join conditions with `-a` or `-o`, group them with `( )`, or check for a missing operator",
    )
}

/// Error for a two-word form whose first word is neither `!` nor a unary operator.
fn not_an_operator(op: &str, operand: &str) -> ShellError {
    if is_binary(op) || is_logical(op) {
        return missing_operand_for(op);
    }
    if looks_like_operator(op) {
        return unknown_unary(op);
    }
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("`{op}` is not a unary operator; found `{op} {operand}`"),
    )
    .with_help(
        "Use a unary operator such as `-n`, `-z`, or `-f` before the operand, or `!` to negate it",
    )
}

/// Error for an unknown unary operator, named in the message.
fn unknown_unary(op: &str) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("unknown unary operator `{op}`"),
    )
    .with_help("Supported unary operators are -z, -n, -t, and the file tests -e, -f, -d, -r, -w, -x, -s, -L, -h, -p, -S, -b, -c, -g, -u, -k, -O, -G, -N")
}

/// Error for an unknown binary operator, named in the message.
fn unknown_binary(op: &str) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("unknown binary operator `{op}`"),
    )
    .with_help("Supported binary operators are =, ==, !=, <, >, -eq, -ne, -lt, -le, -gt, -ge, -nt, -ot, -ef, -a, and -o")
}

/// Error for an operand that an integer operator cannot read.
fn integer_expected(word: &str) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("integer expression expected: `{word}`"),
    )
    .with_help("Give integer operands as optionally signed decimal numbers within 64-bit range, such as `-1`, `0`, or `42`")
}

#[cfg(test)]
mod tests {
    use super::{TestOutcome, evaluate_test};
    use quirl_core::{ErrorCode, ShellError};
    #[cfg(unix)]
    use rustix::process::getegid;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::{
        env, fs,
        path::{Path, PathBuf},
        process,
        time::{Duration, SystemTime},
    };

    /// Runs `[ operands ]` and returns whether it holds, panicking on error.
    fn check(operands: &[&str]) -> bool {
        bracket(operands).expect("expression should evaluate without error")
    }

    /// Runs `[ operands ]` and returns the error, panicking if it succeeds.
    fn failure(operands: &[&str]) -> ShellError {
        let error = bracket(operands).expect_err("expression should be rejected");
        assert!(
            !error.details.help.is_empty(),
            "every error should carry actionable help"
        );
        error
    }

    /// Runs `[ operands ]` and returns its result.
    fn bracket(operands: &[&str]) -> Result<bool, ShellError> {
        let mut words = vec!["[".to_owned()];
        words.extend(operands.iter().map(|word| (*word).to_owned()));
        words.push("]".to_owned());
        evaluate_test(&words).map(|outcome| outcome == TestOutcome::True)
    }

    /// Runs `test operands` and returns its result.
    fn test_command(operands: &[&str]) -> Result<bool, ShellError> {
        let mut words = vec!["test".to_owned()];
        words.extend(operands.iter().map(|word| (*word).to_owned()));
        evaluate_test(&words).map(|outcome| outcome == TestOutcome::True)
    }

    /// Creates an empty scratch directory unique to this process and `name`.
    fn scratch(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("quirl-test-builtin-{}-{name}", process::id()));
        drop(fs::remove_dir_all(&dir));
        fs::create_dir_all(&dir).expect("scratch directory should be creatable");
        dir
    }

    /// Writes `contents` to `dir/name` and returns the path as text.
    fn file_text(dir: &Path, name: &str, contents: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, contents).expect("scratch file should be writable");
        path.display().to_string()
    }

    /// Returns a path as text for use as an operand.
    fn text(path: &Path) -> String {
        path.display().to_string()
    }

    #[test]
    fn empty_expression_is_false() {
        assert!(!check(&[]));
        let holds = test_command(&[]).expect("`test` with no operands should evaluate");
        assert!(!holds);
    }

    #[test]
    fn single_nonempty_word_is_true_even_when_it_looks_like_an_operator() {
        assert!(check(&["x"]));
        assert!(check(&["0"]));
        assert!(check(&["-n"]));
        assert!(check(&["-z"]));
        assert!(check(&["-a"]));
        assert!(check(&["!"]));
    }

    #[test]
    fn single_empty_word_is_false() {
        assert!(!check(&[""]));
    }

    #[test]
    fn trailing_bracket_is_an_operand_for_test() {
        assert!(test_command(&["]"]).expect("`test ]` should evaluate"));
        assert!(test_command(&["-n", "]"]).expect("`test -n ]` should evaluate"));
    }

    #[test]
    fn bracket_without_closing_bracket_is_rejected() {
        let error = evaluate_test(&["[".to_owned(), "x".to_owned()])
            .expect_err("`[` without `]` should be rejected");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(error.message, "missing `]`");
        let bare = evaluate_test(&["[".to_owned()]).expect_err("bare `[` should be rejected");
        assert_eq!(bare.message, "missing `]`");
    }

    #[test]
    fn empty_bracket_expression_is_false() {
        assert!(!check(&[]));
    }

    #[test]
    fn other_command_names_are_rejected() {
        let error = evaluate_test(&["cd".to_owned(), "x".to_owned()])
            .expect_err("a non-test command name should be rejected");
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        let missing = evaluate_test(&[]).expect_err("empty words should be rejected");
        assert_eq!(missing.code, ErrorCode::InvalidCommand);
    }

    #[test]
    fn bang_negates_a_single_word() {
        assert!(check(&["!", ""]));
        assert!(!check(&["!", "x"]));
    }

    #[test]
    fn bang_before_binary_operator_is_the_left_operand() {
        assert!(check(&["!", "=", "!"]));
        assert!(!check(&["!", "=", "x"]));
    }

    #[test]
    fn double_bang_with_nothing_after_is_a_single_operand() {
        assert!(!check(&["!", "!"]));
    }

    #[test]
    fn bang_negates_unary_tests_in_three_word_form() {
        assert!(check(&["!", "-z", "x"]));
        assert!(!check(&["!", "-n", "x"]));
    }

    #[test]
    fn four_word_bang_negates_the_whole_three_word_expression() {
        // POSIX: `! a op b` negates the three-argument test, so `! x -a ""`
        // is `!(x && "")`, which is true.
        assert!(check(&["!", "x", "-a", ""]));
        assert!(!check(&["!", "x", "-a", "y"]));
        // Likewise `! "" -o x` is `!("" || x)`, which is false.
        assert!(!check(&["!", "", "-o", "x"]));
    }

    #[test]
    fn bang_binds_tighter_than_and_in_longer_expressions() {
        // Parsed as ((!x) -a "") -o y = (false && ...) || true.
        assert!(check(&["!", "x", "-a", "", "-o", "y"]));
    }

    #[test]
    fn and_binds_tighter_than_or_in_longer_expressions() {
        // Parsed as a -o ("" -a ""), which is true.
        assert!(check(&["a", "-o", "", "-a", ""]));
        // Parsed as ("" -a x) -o (y -a ""), which is false.
        assert!(!check(&["", "-a", "x", "-o", "y", "-a", ""]));
    }

    #[test]
    fn parentheses_override_and_or_precedence() {
        // Without parentheses this would be a -o ("" -a ""), which is true.
        assert!(!check(&["(", "a", "-o", "", ")", "-a", ""]));
    }

    #[test]
    fn logical_operators_in_three_word_form_combine_two_single_word_tests() {
        assert!(check(&["x", "-a", "y"]));
        assert!(!check(&["x", "-a", ""]));
        assert!(check(&["x", "-o", ""]));
        assert!(!check(&["", "-o", ""]));
    }

    #[test]
    fn parenthesized_three_word_form_tests_the_middle_word() {
        assert!(check(&["(", "x", ")"]));
        assert!(!check(&["(", "", ")"]));
    }

    #[test]
    fn four_word_parenthesized_form_applies_a_two_word_test() {
        assert!(check(&["(", "-n", "x", ")"]));
        assert!(!check(&["(", "-z", "x", ")"]));
    }

    #[test]
    fn string_equality_operators_compare_exactly() {
        assert!(check(&["abc", "=", "abc"]));
        assert!(check(&["abc", "==", "abc"]));
        assert!(!check(&["abc", "=", "abd"]));
        assert!(!check(&["abc", "==", "ABC"]));
        assert!(check(&["", "=", ""]));
    }

    #[test]
    fn string_inequality_operator_negates_equality() {
        assert!(check(&["abc", "!=", "abd"]));
        assert!(!check(&["abc", "!=", "abc"]));
    }

    #[test]
    fn string_ordering_is_bytewise() {
        assert!(check(&["a", "<", "b"]));
        assert!(!check(&["b", "<", "a"]));
        assert!(check(&["", "<", "a"]));
        // Uppercase letters have smaller bytes than lowercase.
        assert!(check(&["B", "<", "a"]));
        assert!(check(&["b", ">", "a"]));
        assert!(!check(&["a", ">", "a"]));
    }

    #[test]
    fn equals_operators_in_three_word_form_are_binary_operators() {
        // POSIX makes `[ = = = ]` a comparison of "=" with "=".
        assert!(check(&["=", "=", "="]));
        assert!(!check(&["=", "!=", "="]));
    }

    #[test]
    fn zero_and_nonzero_length_tests_inspect_emptiness() {
        assert!(check(&["-z", ""]));
        assert!(!check(&["-z", "x"]));
        assert!(check(&["-n", "x"]));
        assert!(!check(&["-n", ""]));
    }

    #[test]
    fn each_integer_operator_compares_signed_values() {
        assert!(check(&["1", "-eq", "1"]));
        assert!(!check(&["1", "-eq", "2"]));
        assert!(check(&["1", "-ne", "2"]));
        assert!(!check(&["1", "-ne", "1"]));
        assert!(check(&["1", "-lt", "2"]));
        assert!(!check(&["2", "-lt", "2"]));
        assert!(check(&["2", "-le", "2"]));
        assert!(!check(&["3", "-le", "2"]));
        assert!(check(&["3", "-gt", "2"]));
        assert!(!check(&["2", "-gt", "2"]));
        assert!(check(&["2", "-ge", "2"]));
        assert!(!check(&["1", "-ge", "2"]));
        assert!(check(&["-3", "-lt", "0"]));
    }

    #[test]
    fn integer_operands_accept_sign_and_surrounding_whitespace() {
        assert!(check(&["+1", "-eq", "1"]));
        assert!(check(&[" 42 ", "-eq", "42"]));
        assert!(check(&["-7", "-lt", "-6"]));
    }

    #[test]
    fn non_integer_left_operand_is_rejected_by_name() {
        let error = failure(&["abc", "-eq", "1"]);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(error.message, "integer expression expected: `abc`");
    }

    #[test]
    fn non_integer_right_operand_is_rejected_by_name() {
        let error = failure(&["1", "-lt", "1.5"]);
        assert_eq!(error.message, "integer expression expected: `1.5`");
    }

    #[test]
    fn empty_integer_operand_is_rejected() {
        let error = failure(&["", "-eq", "0"]);
        assert_eq!(error.message, "integer expression expected: ``");
    }

    #[test]
    fn integer_beyond_64_bit_range_is_rejected() {
        let error = failure(&["99999999999999999999", "-eq", "0"]);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn existence_test_is_true_for_files_and_directories_and_false_when_missing() {
        let dir = scratch("existence");
        let file = file_text(&dir, "f", "x");
        assert!(check(&["-e", &file]));
        assert!(check(&["-e", &text(&dir)]));
        assert!(!check(&["-e", &text(&dir.join("missing"))]));
    }

    #[test]
    fn regular_file_and_directory_tests_distinguish_types() {
        let dir = scratch("types");
        let file = file_text(&dir, "f", "x");
        assert!(check(&["-f", &file]));
        assert!(!check(&["-d", &file]));
        assert!(check(&["-d", &text(&dir)]));
        assert!(!check(&["-f", &text(&dir)]));
        assert!(!check(&["-f", &text(&dir.join("missing"))]));
    }

    #[test]
    fn size_test_is_false_for_an_empty_file() {
        let dir = scratch("size");
        let empty = file_text(&dir, "empty", "");
        let full = file_text(&dir, "full", "x");
        assert!(!check(&["-s", &empty]));
        assert!(check(&["-s", &full]));
    }

    #[test]
    fn read_and_write_tests_follow_permission_bits() {
        let dir = scratch("rw");
        let file = file_text(&dir, "f", "x");
        assert!(check(&["-r", &file]));
        assert!(check(&["-w", &file]));
        assert!(!check(&["-x", &file]));
        assert!(!check(&["-r", &text(&dir.join("missing"))]));
    }

    #[cfg(unix)]
    #[test]
    fn execute_test_follows_the_execute_bit() {
        let dir = scratch("exec");
        let file = file_text(&dir, "f", "#!/bin/sh\n");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))
            .expect("scratch permissions should be settable");
        assert!(!check(&["-x", &file]));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o700))
            .expect("scratch permissions should be settable");
        assert!(check(&["-x", &file]));
        assert!(check(&["-x", "/bin/sh"]));
    }

    #[cfg(unix)]
    #[test]
    fn setuid_and_setgid_tests_read_their_mode_bits() {
        let dir = scratch("setid");
        let file = file_text(&dir, "f", "x");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o4755))
            .expect("scratch permissions should be settable");
        assert!(check(&["-u", &file]));
        assert!(!check(&["-g", &file]));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o2755))
            .expect("scratch permissions should be settable");
        assert!(check(&["-g", &file]));
        assert!(!check(&["-u", &file]));
    }

    #[cfg(unix)]
    #[test]
    fn sticky_test_reads_the_sticky_bit_of_a_directory() {
        let dir = scratch("sticky");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o1777))
            .expect("scratch permissions should be settable");
        assert!(check(&["-k", &text(&dir)]));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o0777))
            .expect("scratch permissions should be settable");
        assert!(!check(&["-k", &text(&dir)]));
    }

    #[cfg(unix)]
    #[test]
    fn owner_test_is_true_for_a_file_the_process_created() {
        let dir = scratch("owner");
        let file = file_text(&dir, "f", "x");
        assert!(check(&["-O", &file]));
        assert!(!check(&["-O", &text(&dir.join("missing"))]));
    }

    #[cfg(unix)]
    #[test]
    fn group_test_matches_the_effective_group_of_the_file() {
        let dir = scratch("group");
        let file = file_text(&dir, "f", "x");
        let file_group = fs::metadata(&file)
            .expect("scratch file should exist")
            .gid();
        assert_eq!(check(&["-G", &file]), file_group == getegid().as_raw());
    }

    #[cfg(unix)]
    #[test]
    fn modified_after_access_test_reads_timestamps() {
        let dir = scratch("modified");
        let file = file_text(&dir, "f", "x");
        let handle = fs::File::options()
            .write(true)
            .open(&file)
            .expect("scratch file should reopen");
        handle
            .set_modified(SystemTime::now() + Duration::from_secs(86_400))
            .expect("modification time should be settable");
        assert!(check(&["-N", &file]));
        handle
            .set_modified(SystemTime::now() - Duration::from_secs(3_600))
            .expect("modification time should be settable");
        assert!(!check(&["-N", &file]));
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_tests_inspect_the_link_while_others_follow_it() {
        let dir = scratch("links");
        let target = file_text(&dir, "target", "x");
        let link = dir.join("link");
        symlink(&target, &link).expect("symlink should be creatable");
        let link_text = text(&link);
        assert!(check(&["-L", &link_text]));
        assert!(check(&["-h", &link_text]));
        assert!(check(&["-f", &link_text]));
        assert!(!check(&["-L", &target]));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symbolic_link_is_a_link_but_not_an_existing_file() {
        let dir = scratch("dangling");
        let link = dir.join("dangling");
        symlink(dir.join("nowhere"), &link).expect("symlink should be creatable");
        let link_text = text(&link);
        assert!(check(&["-L", &link_text]));
        assert!(!check(&["-e", &link_text]));
    }

    #[cfg(unix)]
    #[test]
    fn socket_test_is_true_for_a_bound_unix_socket() {
        let dir = scratch("socket");
        let path = dir.join("sock");
        let _listener =
            std::os::unix::net::UnixListener::bind(&path).expect("socket should be bindable");
        let socket_text = text(&path);
        assert!(check(&["-S", &socket_text]));
        assert!(!check(&["-f", &socket_text]));
    }

    #[cfg(unix)]
    #[test]
    fn device_tests_recognize_character_devices() {
        assert!(check(&["-c", "/dev/null"]));
        assert!(!check(&["-b", "/dev/null"]));
        assert!(!check(&["-p", "/dev/null"]));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_test_is_true_for_a_named_pipe() {
        let dir = scratch("fifo");
        let path = dir.join("pipe");
        let path_text = text(&path);
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo should run");
        assert!(status.success());
        assert!(check(&["-p", &path_text]));
        assert!(!check(&["-f", &path_text]));
    }

    #[test]
    fn newer_and_older_compare_modification_times() {
        let dir = scratch("newer");
        let older = file_text(&dir, "older", "a");
        let newer = file_text(&dir, "newer", "b");
        let older_handle = fs::File::options()
            .write(true)
            .open(&older)
            .expect("scratch file should reopen");
        older_handle
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000))
            .expect("modification time should be settable");
        let newer_handle = fs::File::options()
            .write(true)
            .open(&newer)
            .expect("scratch file should reopen");
        newer_handle
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2_000))
            .expect("modification time should be settable");
        assert!(check(&[&newer, "-nt", &older]));
        assert!(!check(&[&older, "-nt", &newer]));
        assert!(check(&[&older, "-ot", &newer]));
        assert!(!check(&[&newer, "-ot", &older]));
    }

    #[test]
    fn newer_test_treats_a_missing_right_operand_as_older() {
        let dir = scratch("newer-missing");
        let present = file_text(&dir, "present", "x");
        let missing = text(&dir.join("missing"));
        assert!(check(&[&present, "-nt", &missing]));
        assert!(!check(&[&missing, "-nt", &present]));
        assert!(check(&[&missing, "-ot", &present]));
    }

    #[cfg(unix)]
    #[test]
    fn same_file_test_matches_hard_links_and_rejects_distinct_files() {
        let dir = scratch("same");
        let original = file_text(&dir, "original", "x");
        let other = file_text(&dir, "other", "x");
        let link = dir.join("hard");
        fs::hard_link(&original, &link).expect("hard link should be creatable");
        assert!(check(&[&original, "-ef", &text(&link)]));
        assert!(!check(&[&original, "-ef", &other]));
        assert!(!check(&[&original, "-ef", &text(&dir.join("missing"))]));
    }

    #[test]
    fn terminal_test_is_false_for_descriptors_other_than_the_standard_three() {
        assert!(!check(&["-t", "9"]));
        assert!(!check(&["-t", "-1"]));
    }

    #[test]
    fn terminal_test_rejects_a_non_integer_descriptor() {
        let error = failure(&["-t", "x"]);
        assert_eq!(error.message, "integer expression expected: `x`");
    }

    #[test]
    fn unknown_unary_operator_is_named_in_the_error() {
        let error = failure(&["-q", "x"]);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(error.message, "unknown unary operator `-q`");
    }

    #[test]
    fn unknown_binary_operator_is_named_in_the_error() {
        let error = failure(&["x", "-q", "y"]);
        assert_eq!(error.message, "unknown binary operator `-q`");
    }

    #[test]
    fn binary_operator_without_a_left_operand_is_rejected() {
        let error = failure(&["-eq", "1"]);
        assert_eq!(error.message, "operator `-eq` is missing an operand");
    }

    #[test]
    fn logical_operator_without_a_right_operand_is_rejected() {
        let error = failure(&["x", "-a"]);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn two_words_without_a_unary_operator_are_rejected() {
        let error = failure(&["x", "y"]);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        let binary_first = failure(&["=", "x"]);
        assert_eq!(binary_first.message, "operator `=` is missing an operand");
    }

    #[test]
    fn extra_word_after_a_complete_expression_is_rejected() {
        let error = failure(&["x", "y", "z", "w", "v"]);
        assert_eq!(error.message, "unexpected argument `y`");
    }

    #[test]
    fn unbalanced_parenthesis_is_rejected() {
        let error = failure(&["(", "a", "-a", "b"]);
        assert_eq!(error.message, "missing `)`");
    }

    #[test]
    fn nesting_at_the_limit_is_accepted() {
        let mut operands = vec!["("; 64];
        operands.push("x");
        operands.extend(std::iter::repeat_n(")", 64));
        assert!(check(&operands));
    }

    #[test]
    fn nesting_beyond_the_limit_is_a_resource_limit() {
        let mut operands = vec!["("; 65];
        operands.push("x");
        operands.extend(std::iter::repeat_n(")", 65));
        let error = failure(&operands);
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|line| line.contains("limit is 64")),
            "the error should state the configured limit"
        );
    }

    #[test]
    fn bang_chain_beyond_the_nesting_limit_is_a_resource_limit() {
        let mut operands = vec!["!"; 65];
        operands.extend(["x", "-a", "y"]);
        assert_eq!(failure(&operands).code, ErrorCode::ResourceLimit);
        let mut at_limit = vec!["!"; 64];
        at_limit.extend(["x", "-a", "y"]);
        assert!(check(&at_limit));
    }

    #[test]
    fn operand_count_at_the_limit_is_accepted() {
        // "!", then "x" and 2047 pairs of "-a" "x": 4096 operands, which is
        // !x && x && ... && x, false.
        let mut operands = vec!["!", "x"];
        for _ in 0..2047 {
            operands.extend(["-a", "x"]);
        }
        assert_eq!(operands.len(), 4096);
        assert!(!check(&operands));
    }

    #[test]
    fn operand_count_beyond_the_limit_is_a_resource_limit() {
        let mut operands = vec!["x"];
        for _ in 0..2048 {
            operands.extend(["-a", "x"]);
        }
        assert_eq!(operands.len(), 4097);
        assert_eq!(failure(&operands).code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn errors_for_invalid_operators_are_invalid_argument_not_resource_limits() {
        let error = failure(&["x", "-q", "y", "-a", "z"]);
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
}
