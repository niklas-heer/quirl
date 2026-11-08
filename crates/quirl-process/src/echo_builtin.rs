//! Pure implementation of Bash's `echo` builtin with `xpg_echo` off.
//!
//! Shell loops that call `echo` would otherwise fork `/bin/echo` on every
//! iteration. This module renders the bytes one invocation writes to standard
//! output, so the executor can write them directly. Output is accumulated under
//! an explicit byte limit, and escape decoding stops as soon as `\c` appears.

#![allow(
    dead_code,
    reason = "wired into the native executor by the native-language integration"
)]

use quirl_core::{ErrorCode, ShellError};
use std::{iter::Peekable, str::Chars};

/// Bytes one `echo` invocation writes to standard output.
///
/// `words` is the expanded argv and must include `"echo"` as `words[0]`; an
/// empty slice is treated as an invocation without operands. Leading option
/// words are consumed while each is `-` followed by one or more of `n`, `e`,
/// `E`; the first other word, including `-`, `--`, and `-nx`, is printed. The
/// last `-e` or `-E` wins. Operands are joined with single spaces and end with
/// `\n` unless `-n` was given.
///
/// With escapes enabled, the sequences `\a \b \e \E \f \n \r \t \v \\` decode,
/// `\c` stops all further output, `\0nnn` (up to three octal digits after the
/// zero) and `\xHH` produce raw bytes, and `\uHHHH` / `\UHHHHHHHH` produce the
/// UTF-8 encoding of a Unicode scalar. A malformed hex, Unicode, or invalid
/// scalar escape prints its original text, and an unknown escape keeps its
/// backslash.
///
/// `output_bytes_max` bounds the returned length. Each append is checked
/// before its bytes are stored, so an oversized invocation fails with
/// [`ErrorCode::ResourceLimit`] without retaining more than the limit. The
/// function never panics on any input.
pub(crate) fn render_echo(
    words: &[String],
    output_bytes_max: usize,
) -> Result<Vec<u8>, ShellError> {
    let operands = words.get(1..).unwrap_or_default();
    let (options, operands) = parse_options(operands);
    let mut output = EchoOutput {
        bytes: Vec::new(),
        output_bytes_max,
    };
    for (index, word) in operands.iter().enumerate() {
        if index > 0 {
            output.push(b" ")?;
        }
        let flow = if options.escapes {
            write_escaped_word(&mut output, word)?
        } else {
            output.push(word.as_bytes())?;
            Flow::Continue
        };
        if flow == Flow::Stop {
            return Ok(output.bytes);
        }
    }
    if options.newline {
        output.push(b"\n")?;
    }
    Ok(output.bytes)
}

/// Settings chosen by the leading option words of one `echo` invocation.
struct EchoOptions {
    /// Whether the invocation ends with a newline. `-n` clears it.
    newline: bool,
    /// Whether backslash sequences are decoded. `-e` sets it and `-E` clears it.
    escapes: bool,
}

/// Whether a leading operand is an option cluster such as `-n`, `-ne`, or `-En`.
///
/// A word counts only when it is a dash followed by at least one character,
/// all of them `n`, `e`, or `E`. Bare `-`, `--`, and words with any other
/// letter are operands.
fn is_option_word(word: &str) -> bool {
    word.strip_prefix('-').is_some_and(|flags| {
        !flags.is_empty() && flags.chars().all(|flag| matches!(flag, 'n' | 'e' | 'E'))
    })
}

/// Splits leading option words from the operands, returning the remaining words.
///
/// Option parsing ends at the first word that [`is_option_word`] rejects. Later
/// option-looking words are operands, as in Bash.
fn parse_options(operands: &[String]) -> (EchoOptions, &[String]) {
    let mut options = EchoOptions {
        newline: true,
        escapes: false,
    };
    let mut rest = operands;
    while let Some((word, tail)) = rest.split_first() {
        if !is_option_word(word) {
            break;
        }
        for flag in word.chars().skip(1) {
            match flag {
                'n' => options.newline = false,
                'e' => options.escapes = true,
                'E' => options.escapes = false,
                _ => {}
            }
        }
        rest = tail;
    }
    (options, rest)
}

/// Outcome of decoding one word, which tells the caller whether to continue.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    /// Decoding finished normally, and the next word may follow.
    Continue,
    /// A `\c` escape ended the whole invocation, so nothing more is written.
    Stop,
}

/// Bytes produced by one `echo` invocation, checked against its byte limit.
struct EchoOutput {
    /// Bytes written so far. Never longer than `output_bytes_max`.
    bytes: Vec<u8>,
    /// Maximum total bytes this invocation may write.
    output_bytes_max: usize,
}

impl EchoOutput {
    /// Appends `chunk` unless the result would exceed the byte limit.
    ///
    /// The check runs before anything is stored, so a rejected chunk leaves the
    /// buffer unchanged and the error reports the total that would have been
    /// needed.
    fn push(&mut self, chunk: &[u8]) -> Result<(), ShellError> {
        let needed = self.bytes.len().saturating_add(chunk.len());
        if needed > self.output_bytes_max {
            return Err(self.limit_error(needed));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    /// Appends one character as its UTF-8 encoding under the byte limit.
    fn push_char(&mut self, character: char) -> Result<(), ShellError> {
        let mut buffer = [0_u8; 4];
        self.push(character.encode_utf8(&mut buffer).as_bytes())
    }

    /// Builds the resource-limit error with the configured limit and observed need.
    fn limit_error(&self, needed: usize) -> ShellError {
        ShellError::new(
            ErrorCode::ResourceLimit,
            format!(
                "echo output exceeds the {} byte limit",
                self.output_bytes_max
            ),
        )
        .with_context(format!(
            "output_bytes_max is {}; this echo needed {needed} bytes",
            self.output_bytes_max
        ))
        .with_help("Print less output in one echo, or split it across several commands")
    }
}

/// Decodes one operand with backslash escapes and appends the result.
///
/// Characters are read whole, so a backslash before a multi-byte character
/// keeps that character intact. A trailing lone backslash is printed.
fn write_escaped_word(output: &mut EchoOutput, word: &str) -> Result<Flow, ShellError> {
    let mut characters = word.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\\' {
            output.push_char(character)?;
            continue;
        }
        let Some(escaped) = characters.next() else {
            output.push(b"\\")?;
            break;
        };
        match escaped {
            'a' => output.push(b"\x07")?,
            'b' => output.push(b"\x08")?,
            'e' | 'E' => output.push(b"\x1b")?,
            'f' => output.push(b"\x0c")?,
            'n' => output.push(b"\n")?,
            'r' => output.push(b"\r")?,
            't' => output.push(b"\t")?,
            'v' => output.push(b"\x0b")?,
            '\\' => output.push(b"\\")?,
            'c' => return Ok(Flow::Stop),
            '0' => write_octal_escape(output, &mut characters)?,
            'x' => write_hex_escape(output, &mut characters)?,
            'u' => write_unicode_escape(output, &mut characters, 'u', 4)?,
            'U' => write_unicode_escape(output, &mut characters, 'U', 8)?,
            other => {
                output.push(b"\\")?;
                output.push_char(other)?;
            }
        }
    }
    Ok(Flow::Continue)
}

/// Appends the raw byte named by `\0` followed by up to three octal digits.
///
/// Three octal digits can name 511, which exceeds one byte. Bash keeps the low
/// byte, so the accumulation wraps at 256 instead of rejecting the escape.
fn write_octal_escape(
    output: &mut EchoOutput,
    characters: &mut Peekable<Chars<'_>>,
) -> Result<(), ShellError> {
    let digits = take_digits(characters, 8, 3);
    let byte = digits.bytes().fold(0_u8, |acc, digit| {
        acc.wrapping_mul(8).wrapping_add(digit.wrapping_sub(b'0'))
    });
    output.push(&[byte])
}

/// Appends the raw byte named by `\x` followed by one or two hex digits.
///
/// `\x` with no hex digit is printed as the literal two characters `\x`.
fn write_hex_escape(
    output: &mut EchoOutput,
    characters: &mut Peekable<Chars<'_>>,
) -> Result<(), ShellError> {
    let digits = take_digits(characters, 16, 2);
    match u8::from_str_radix(&digits, 16) {
        Ok(byte) => output.push(&[byte]),
        Err(_) => output.push(b"\\x"),
    }
}

/// Appends the UTF-8 encoding of a `\u` or `\U` escape.
///
/// `marker` is `u` or `U` and `digits_max` is 4 or 8. An escape with no hex
/// digit, or whose value is not a Unicode scalar such as a surrogate, prints its
/// original text so the user can see what was rejected.
fn write_unicode_escape(
    output: &mut EchoOutput,
    characters: &mut Peekable<Chars<'_>>,
    marker: char,
    digits_max: usize,
) -> Result<(), ShellError> {
    let digits = take_digits(characters, 16, digits_max);
    match u32::from_str_radix(&digits, 16)
        .ok()
        .and_then(char::from_u32)
    {
        Some(scalar) => output.push_char(scalar),
        None => {
            output.push(b"\\")?;
            output.push_char(marker)?;
            output.push(digits.as_bytes())
        }
    }
}

/// Consumes up to `digits_max` ASCII digits of `radix` and returns them as text.
///
/// Only the digits are taken, so a following non-digit character is left in the
/// iterator for the caller.
fn take_digits(characters: &mut Peekable<Chars<'_>>, radix: u32, digits_max: usize) -> String {
    let mut digits = String::new();
    while digits.len() < digits_max {
        let Some(digit) = characters.next_if(|character| character.is_digit(radix)) else {
            break;
        };
        digits.push(digit);
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, process::Command};

    /// Output limit for tests that do not exercise the limit itself.
    const TEST_OUTPUT_BYTES_MAX: usize = 1 << 20;

    /// Runs `render_echo` on `operands` with `echo` as argv[0].
    fn echo(operands: &[&str]) -> Vec<u8> {
        let words: Vec<String> = std::iter::once("echo")
            .chain(operands.iter().copied())
            .map(str::to_owned)
            .collect();
        render_echo(&words, TEST_OUTPUT_BYTES_MAX).unwrap()
    }

    /// Runs `render_echo` with a custom limit on `operands`.
    fn echo_with_limit(operands: &[&str], output_bytes_max: usize) -> Result<Vec<u8>, ShellError> {
        let words: Vec<String> = std::iter::once("echo")
            .chain(operands.iter().copied())
            .map(str::to_owned)
            .collect();
        render_echo(&words, output_bytes_max)
    }

    #[test]
    fn joins_plain_words_with_single_spaces_and_a_newline() {
        assert_eq!(echo(&["hello", "big", "world"]), b"hello big world\n");
    }

    #[test]
    fn prints_only_a_newline_without_operands() {
        assert_eq!(echo(&[]), b"\n");
    }

    #[test]
    fn treats_an_empty_argv_as_an_invocation_without_operands() {
        assert_eq!(render_echo(&[], TEST_OUTPUT_BYTES_MAX).unwrap(), b"\n");
    }

    #[test]
    fn prints_an_empty_operand_as_an_empty_field_between_spaces() {
        assert_eq!(echo(&["a", "", "b"]), b"a  b\n");
    }

    #[test]
    fn suppresses_the_trailing_newline_with_n() {
        assert_eq!(echo(&["-n", "x"]), b"x");
    }

    #[test]
    fn decodes_each_simple_escape_when_e_is_given() {
        let cases: [(&str, &[u8]); 10] = [
            (r"\a", b"\x07"),
            (r"\b", b"\x08"),
            (r"\e", b"\x1b"),
            (r"\E", b"\x1b"),
            (r"\f", b"\x0c"),
            (r"\n", b"\n"),
            (r"\r", b"\r"),
            (r"\t", b"\t"),
            (r"\v", b"\x0b"),
            (r"\\", b"\\"),
        ];
        for (operand, expected) in cases {
            let mut want = expected.to_vec();
            want.push(b'\n');
            assert_eq!(echo(&["-e", operand]), want, "escape {operand}");
        }
    }

    #[test]
    fn leaves_backslash_escapes_undecoded_by_default() {
        assert_eq!(echo(&[r"a\tb"]), b"a\\tb\n");
        assert_eq!(echo(&[r"a\tb\n"]), b"a\\tb\\n\n");
    }

    #[test]
    fn disables_escapes_with_capital_e() {
        assert_eq!(echo(&["-e", "-E", r"a\tb"]), b"a\\tb\n");
        assert_eq!(echo(&["-E", r"a\tb"]), b"a\\tb\n");
    }

    #[test]
    fn last_escape_option_wins_between_e_and_capital_e() {
        assert_eq!(echo(&["-Ee", r"a\tb"]), b"a\tb\n");
        assert_eq!(echo(&["-eE", r"a\tb"]), b"a\\tb\n");
    }

    #[test]
    fn accepts_combined_option_letters_in_one_word() {
        assert_eq!(echo(&["-neE", "x"]), b"x");
        assert_eq!(echo(&["-En", r"\t"]), b"\\t");
        assert_eq!(echo(&["-ne", r"\t"]), b"\t");
    }

    #[test]
    fn accepts_repeated_option_words_before_operands() {
        assert_eq!(echo(&["-n", "-n", "-e", r"\x41"]), b"A");
    }

    #[test]
    fn prints_a_lone_dash_and_double_dash_literally() {
        assert_eq!(echo(&["-"]), b"-\n");
        assert_eq!(echo(&["--"]), b"--\n");
        assert_eq!(echo(&["-", "--", "x"]), b"- -- x\n");
    }

    #[test]
    fn prints_an_option_with_an_unknown_letter_literally() {
        assert_eq!(echo(&["-nx"]), b"-nx\n");
        assert_eq!(echo(&["-x", "y"]), b"-x y\n");
        assert_eq!(echo(&["-n", "-nx", "y"]), b"-nx y");
    }

    #[test]
    fn prints_option_looking_words_after_the_first_operand() {
        assert_eq!(echo(&["a", "-n", "b"]), b"a -n b\n");
        assert_eq!(echo(&["a", "-e", r"\t"]), b"a -e \\t\n");
    }

    #[test]
    fn stops_output_at_a_backslash_c_in_the_middle_of_a_word() {
        assert_eq!(echo(&["-e", r"abc\cdef"]), b"abc");
    }

    #[test]
    fn backslash_c_drops_following_words_and_the_trailing_newline() {
        assert_eq!(echo(&["-e", r"one\c", "two", "three"]), b"one");
        assert_eq!(echo(&["-e", "a", r"\c", "b"]), b"a ");
    }

    #[test]
    fn backslash_c_as_the_first_escape_prints_nothing() {
        assert_eq!(echo(&["-e", r"\c"]), b"");
    }

    #[test]
    fn decodes_octal_zero_escapes_to_bytes() {
        assert_eq!(echo(&["-e", r"\0101"]), b"A\n");
        assert_eq!(echo(&["-e", r"\0"]), b"\x00\n");
        assert_eq!(echo(&["-e", r"\09"]), b"\x009\n");
        assert_eq!(echo(&["-e", r"\0377"]), b"\xff\n");
    }

    #[test]
    fn wraps_octal_zero_escapes_that_exceed_one_byte() {
        assert_eq!(echo(&["-e", r"\0777"]), b"\xff\n");
    }

    #[test]
    fn decodes_hex_escapes_with_one_or_two_digits() {
        assert_eq!(echo(&["-e", r"\x41"]), b"A\n");
        assert_eq!(echo(&["-e", r"\x4"]), b"\x04\n");
        assert_eq!(echo(&["-e", r"\x41B"]), b"AB\n");
        assert_eq!(echo(&["-e", r"\xff"]), b"\xff\n");
    }

    #[test]
    fn keeps_a_hex_escape_without_digits_as_literal_text() {
        assert_eq!(echo(&["-e", r"\x"]), b"\\x\n");
        assert_eq!(echo(&["-e", r"\xG1"]), b"\\xG1\n");
    }

    #[test]
    fn decodes_unicode_escapes_into_utf8() {
        assert_eq!(echo(&["-e", r"\u00e9"]), "é\n".as_bytes());
        assert_eq!(echo(&["-e", r"\u0041"]), b"A\n");
        assert_eq!(echo(&["-e", r"\U0001F600"]), "\u{1F600}\n".as_bytes());
        assert_eq!(echo(&["-e", r"\U00000041"]), b"A\n");
    }

    #[test]
    fn keeps_an_invalid_unicode_scalar_escape_as_its_original_text() {
        assert_eq!(echo(&["-e", r"\uD800"]), b"\\uD800\n");
        assert_eq!(echo(&["-e", r"\U00110000"]), b"\\U00110000\n");
    }

    #[test]
    fn keeps_a_unicode_escape_without_digits_as_literal_text() {
        assert_eq!(echo(&["-e", r"\u"]), b"\\u\n");
        assert_eq!(echo(&["-e", r"\Uzz"]), b"\\Uzz\n");
    }

    #[test]
    fn decodes_only_the_hex_digits_of_a_unicode_escape_and_keeps_the_rest() {
        assert_eq!(echo(&["-e", r"\u00e9g"]), "\u{e9}g\n".as_bytes());
        assert_eq!(echo(&["-e", r"\u00412"]), b"A2\n");
    }

    #[test]
    fn preserves_utf8_text_without_escapes() {
        assert_eq!(echo(&["é", "日本"]), "é 日本\n".as_bytes());
    }

    #[test]
    fn keeps_the_backslash_of_an_unknown_escape() {
        assert_eq!(echo(&["-e", r"a\qb"]), b"a\\qb\n");
        assert_eq!(echo(&["-e", r"\1"]), b"\\1\n");
    }

    #[test]
    fn keeps_a_multibyte_character_intact_after_a_backslash() {
        assert_eq!(echo(&["-e", "\\é"]), "\\é\n".as_bytes());
    }

    #[test]
    fn prints_a_trailing_lone_backslash_within_its_word() {
        assert_eq!(echo(&["-e", "a\\", "b"]), b"a\\ b\n");
    }

    #[test]
    fn rejects_output_that_would_exceed_the_byte_limit() {
        let error = echo_with_limit(&["abcdef"], 3).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("3 byte limit"), "{}", error.message);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|line| line.contains("needed 6 bytes")),
            "{:?}",
            error.details.context
        );
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn accepts_output_that_exactly_fills_the_byte_limit() {
        assert_eq!(echo_with_limit(&["abc"], 4).unwrap(), b"abc\n");
    }

    #[test]
    fn counts_separators_and_the_newline_against_the_byte_limit() {
        assert!(echo_with_limit(&["a", "b"], 3).is_err());
        assert_eq!(echo_with_limit(&["a", "b"], 4).unwrap(), b"a b\n");
    }

    #[test]
    fn stops_before_the_limit_when_backslash_c_appears_first() {
        assert_eq!(echo_with_limit(&["-e", r"a\cbbbbbbbbbb"], 1).unwrap(), b"a");
    }

    #[test]
    fn zero_byte_limit_rejects_any_visible_output() {
        assert_eq!(echo_with_limit(&["-n"], 0).unwrap(), b"");
        assert!(echo_with_limit(&["x"], 0).is_err());
    }

    #[test]
    fn matches_bash_builtin_echo_output_for_reference_cases() {
        let bash = Path::new("/bin/bash");
        if !bash.exists() {
            return;
        }
        let cases: &[&[&str]] = &[
            &[],
            &["a", "b c"],
            &["-n", "x"],
            &["-e", r"a\tb\nc"],
            &["-e", r"\0101\x41"],
            &["-E", r"a\nb"],
            &["-neE", "x\\c", "y"],
            &["-", "--", "-nx"],
            &["-e", r"a\c", "b"],
            &["-e", r"\0"],
            &["-e", r"\09"],
            &["-e", r"\0777"],
            &["-e", r"\x", r"\xG", r"\q"],
            &["-e", "trail\\"],
            &["-e", r"\x4"],
            &["-e", r"\0377"],
            &["-e", r"\a\b\f\r\v\\"],
            &["-e", "é\\\\"],
            &["-n", "-e", "x", "-n"],
            &["a", "-e", r"\t"],
        ];
        for operands in cases {
            let expected = Command::new(bash)
                .args(["-c", r#"echo "$@""#, "bash"])
                .args(*operands)
                .output()
                .unwrap()
                .stdout;
            assert_eq!(echo(operands), expected, "operands {operands:?}");
        }
    }
}
