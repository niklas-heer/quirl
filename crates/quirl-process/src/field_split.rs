//! POSIX field splitting (XCU 2.6.5) for one word after expansion.
//!
//! Only bytes inside [`ExpandedText::splittable`] can delimit fields, and only
//! when they are characters of IFS. Every other byte is field content, so
//! literal and quoted text join with the neighbouring expansion output. The
//! splitter makes one pass over the value, so its cost is linear in the value
//! length times the length of IFS, which is expected to be small.

#![allow(
    dead_code,
    reason = "wired into the native executor by the native-language integration"
)]

use quirl_core::{ErrorCode, ShellError};
use std::ops::Range;

/// One word after parameter, command, and arithmetic expansion.
pub(crate) struct ExpandedText<'a> {
    /// The expanded bytes of the whole word (UTF-8).
    pub(crate) value: &'a str,
    /// Byte ranges of `value` that came from UNQUOTED parameter expansion or command
    /// substitution. Only these bytes are subject to splitting; everything else
    /// (literal text, quoted text) is never a delimiter. Ranges are sorted, non-overlapping,
    /// on char boundaries, and within `value` (validate; invalid ranges → error).
    pub(crate) splittable: &'a [Range<usize>],
    /// Whether the original word contained any quoted part (even an empty `""`).
    pub(crate) has_quoted_part: bool,
}

/// The IFS used when the variable is unset, per XCU 2.6.5.
const DEFAULT_IFS: &str = " \t\n";

/// Split an expanded word into fields according to IFS.
///
/// `ifs` is `None` when IFS is unset, which behaves as `" \t\n"`. `Some("")`
/// disables splitting, so the whole value becomes one field. Only bytes inside
/// `word.splittable` can delimit fields. An unquoted word that yields no fields
/// produces an empty vector, while a quoted word that yields no fields produces
/// one empty field.
///
/// Returns a `Validation` error when a splittable range is reversed, out of
/// bounds, off a UTF-8 boundary, or not sorted and disjoint. Returns a
/// `ResourceLimit` error as soon as the result would hold more than
/// `fields_max` fields. Runs in time linear in `word.value.len()`.
pub(crate) fn split_fields(
    word: &ExpandedText<'_>,
    ifs: Option<&str>,
    fields_max: usize,
) -> Result<Vec<String>, ShellError> {
    validate_splittable_ranges(word)?;
    let ifs = ifs.unwrap_or(DEFAULT_IFS);
    let mut sink = FieldSink::new(fields_max);
    let mut current = String::new();
    // True while a run of IFS white space follows a non-empty field. The run
    // is held back because a non-white-space delimiter that follows absorbs it
    // into one delimiter, while content that follows simply ends the field.
    let mut in_whitespace_run = false;
    let mut ranges = RangeCursor::new(word.splittable);
    for (offset, character) in word.value.char_indices() {
        let role = if ranges.contains(offset) {
            classify_separator(ifs, character)
        } else {
            Role::Content
        };
        match role {
            Role::Whitespace => {
                // Leading white space has no field to end, so it is ignored.
                if !current.is_empty() {
                    in_whitespace_run = true;
                }
            }
            Role::Delimiter => {
                // Each non-white-space delimiter closes exactly one field, even
                // an empty one, which is how `a::b` keeps its middle field.
                sink.push(std::mem::take(&mut current))?;
                in_whitespace_run = false;
            }
            Role::Content => {
                if in_whitespace_run {
                    sink.push(std::mem::take(&mut current))?;
                    in_whitespace_run = false;
                }
                current.push(character);
            }
        }
    }
    // A trailing single delimiter never reaches this point with an empty
    // `current`, so it cannot create a trailing empty field.
    if !current.is_empty() {
        sink.push(current)?;
    }
    sink.finish(word.has_quoted_part)
}

/// How one character of a word takes part in field splitting.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Field content: literal or quoted text, or a splittable non-IFS character.
    Content,
    /// IFS white space, which delimits only as part of a run.
    Whitespace,
    /// A non-white-space IFS character, which always closes a field.
    Delimiter,
}

/// Classify a splittable character by whether and how IFS contains it.
///
/// Callers must pass only characters from splittable ranges. The check is a
/// scan of `ifs`, which is treated as a short set of characters.
fn classify_separator(ifs: &str, character: char) -> Role {
    if !ifs.contains(character) {
        Role::Content
    } else if is_blank(character) {
        Role::Whitespace
    } else {
        Role::Delimiter
    }
}

/// Report whether a character is IFS white space by its code point, before IFS
/// membership is checked.
fn is_blank(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n')
}

/// Walks sorted, disjoint splittable ranges alongside non-decreasing offsets.
struct RangeCursor<'r> {
    /// Ranges already validated as sorted and disjoint.
    ranges: &'r [Range<usize>],
    /// Index of the first range whose end has not yet been passed.
    next: usize,
}

impl<'r> RangeCursor<'r> {
    /// Start a walk at the first range.
    fn new(ranges: &'r [Range<usize>]) -> Self {
        Self { ranges, next: 0 }
    }

    /// Report whether the character starting at byte `offset` is splittable.
    ///
    /// Offsets must be passed in non-decreasing order, which keeps the total
    /// work linear. Because range ends are char boundaries, a character that
    /// starts inside a range lies wholly inside it.
    fn contains(&mut self, offset: usize) -> bool {
        while self
            .ranges
            .get(self.next)
            .is_some_and(|range| range.end <= offset)
        {
            self.next = self.next.saturating_add(1);
        }
        self.ranges
            .get(self.next)
            .is_some_and(|range| range.start <= offset)
    }
}

/// Reject splittable ranges that would make the scan read outside the value.
fn validate_splittable_ranges(word: &ExpandedText<'_>) -> Result<(), ShellError> {
    let value_len = word.value.len();
    let mut previous_end = 0;
    for range in word.splittable {
        if range.start > range.end {
            return Err(invalid_range_error(range, value_len, "start is after end"));
        }
        if range.end > value_len {
            return Err(invalid_range_error(
                range,
                value_len,
                "end is past the word",
            ));
        }
        if !word.value.is_char_boundary(range.start) || !word.value.is_char_boundary(range.end) {
            return Err(invalid_range_error(
                range,
                value_len,
                "an endpoint is not on a UTF-8 boundary",
            ));
        }
        if range.start < previous_end {
            return Err(invalid_range_error(
                range,
                value_len,
                "ranges overlap or are not sorted",
            ));
        }
        previous_end = range.end;
    }
    Ok(())
}

/// Build the error for a splittable range that breaks the validation contract.
fn invalid_range_error(range: &Range<usize>, value_len: usize, reason: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, "splittable word range is invalid")
        .with_context(format!(
            "range {}..{} in a {value_len}-byte word: {reason}",
            range.start, range.end
        ))
        .with_help(
            "Expansion must report sorted, disjoint byte ranges inside the word on UTF-8 boundaries",
        )
}

/// Collects fields in order and enforces the caller's field bound.
struct FieldSink {
    /// Fields accepted so far, in word order.
    fields: Vec<String>,
    /// The most fields this word may produce.
    fields_max: usize,
}

impl FieldSink {
    /// Start an empty sink that accepts at most `fields_max` fields.
    fn new(fields_max: usize) -> Self {
        Self {
            fields: Vec::new(),
            fields_max,
        }
    }

    /// Append one field, or fail before exceeding `fields_max`.
    ///
    /// The check runs as each field is produced, so a hostile word cannot make
    /// the splitter allocate beyond the bound.
    fn push(&mut self, field: String) -> Result<(), ShellError> {
        if self.fields.len() >= self.fields_max {
            return Err(field_limit_error(self.fields_max));
        }
        self.fields.push(field);
        Ok(())
    }

    /// Apply the empty-word rule and return the accepted fields.
    ///
    /// A word that produced no fields keeps one empty field only when it had a
    /// quoted part, because `""` must survive while an unquoted empty
    /// expansion disappears.
    fn finish(mut self, has_quoted_part: bool) -> Result<Vec<String>, ShellError> {
        if self.fields.is_empty() && has_quoted_part {
            self.push(String::new())?;
        }
        Ok(self.fields)
    }
}

/// Build the error for a word whose split would exceed the field bound.
fn field_limit_error(fields_max: usize) -> ShellError {
    let observed_at_least = fields_max.saturating_add(1);
    ShellError::new(ErrorCode::ResourceLimit, "word splits into too many fields")
        .with_context(format!(
            "limit {fields_max} fields; observed at least {observed_at_least}"
        ))
        .with_help(
            "Quote the expansion to keep it as one field, or split the input into smaller words",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a byte range for test fixtures; a call avoids the single-range array lint.
    fn range(start: usize, end: usize) -> Range<usize> {
        start..end
    }

    /// Split a word whose whole value is splittable, the common unquoted case.
    fn split_all(value: &str, ifs: Option<&str>) -> Vec<String> {
        let ranges = [range(0, value.len())];
        let word = ExpandedText {
            value,
            splittable: &ranges,
            has_quoted_part: false,
        };
        split_fields(&word, ifs, usize::MAX).unwrap()
    }

    #[test]
    fn unset_ifs_splits_on_space_tab_and_newline_runs() {
        assert_eq!(split_all(" \ta\n\n b\t", None), ["a", "b"]);
    }

    #[test]
    fn unset_ifs_matches_explicit_default_whitespace() {
        assert_eq!(
            split_all("a \t b\nc", None),
            split_all("a \t b\nc", Some(" \t\n"))
        );
    }

    #[test]
    fn empty_ifs_keeps_splittable_text_as_one_field() {
        assert_eq!(split_all("a b:c\td", Some("")), ["a b:c\td"]);
    }

    #[test]
    fn whitespace_ifs_collapses_runs_and_ignores_outer_runs() {
        assert_eq!(split_all("  a   b  ", Some(" ")), ["a", "b"]);
    }

    #[test]
    fn nonwhitespace_delimiter_keeps_adjacent_empty_field() {
        assert_eq!(split_all("a::b", Some(":")), ["a", "", "b"]);
    }

    #[test]
    fn leading_nonwhitespace_delimiter_yields_empty_first_field() {
        assert_eq!(split_all(":a", Some(":")), ["", "a"]);
    }

    #[test]
    fn trailing_nonwhitespace_delimiter_does_not_create_empty_field() {
        assert_eq!(split_all("a:", Some(":")), ["a"]);
    }

    #[test]
    fn lone_delimiter_yields_one_empty_field() {
        assert_eq!(split_all(":", Some(":")), [""]);
    }

    #[test]
    fn mixed_ifs_ignores_whitespace_around_outer_fields() {
        assert_eq!(split_all(" a : b ", Some(" :")), ["a", "b"]);
    }

    #[test]
    fn whitespace_next_to_delimiter_forms_one_delimiter() {
        assert_eq!(split_all("a : :b", Some(" :")), ["a", "", "b"]);
    }

    #[test]
    fn leading_whitespace_before_delimiter_still_yields_empty_field() {
        assert_eq!(split_all(" :a", Some(" :")), ["", "a"]);
    }

    #[test]
    fn whitespace_after_nonwhitespace_delimiter_is_skipped() {
        assert_eq!(split_all("a: b", Some(": ")), ["a", "b"]);
    }

    #[test]
    fn nonwhitespace_ifs_character_is_not_whitespace_for_runs() {
        assert_eq!(split_all("a:::b", Some(": ")), ["a", "", "", "b"]);
    }

    #[test]
    fn multibyte_ifs_character_delimits_fields() {
        assert_eq!(split_all("α→β→", Some("→")), ["α", "β"]);
    }

    #[test]
    fn unquoted_non_ifs_multibyte_text_is_kept_whole() {
        assert_eq!(split_all("αβγ", Some(":")), ["αβγ"]);
    }

    #[test]
    fn non_splittable_spaces_stay_inside_one_quoted_field() {
        let word = ExpandedText {
            value: "a b",
            splittable: &[],
            has_quoted_part: true,
        };
        assert_eq!(split_fields(&word, None, usize::MAX).unwrap(), ["a b"]);
    }

    #[test]
    fn non_splittable_ifs_characters_are_field_content() {
        let word = ExpandedText {
            value: "a:b c",
            splittable: &[],
            has_quoted_part: true,
        };
        assert_eq!(
            split_fields(&word, Some(": "), usize::MAX).unwrap(),
            ["a:b c"]
        );
    }

    #[test]
    fn literal_text_joins_with_splittable_expansion_output() {
        let word = ExpandedText {
            value: "xa b",
            splittable: &[range(1, 4)],
            has_quoted_part: false,
        };
        assert_eq!(split_fields(&word, None, usize::MAX).unwrap(), ["xa", "b"]);
    }

    #[test]
    fn splittable_range_in_middle_of_literal_text_splits_only_inside_it() {
        let word = ExpandedText {
            value: "x a y",
            splittable: &[range(1, 4)],
            has_quoted_part: false,
        };
        assert_eq!(
            split_fields(&word, None, usize::MAX).unwrap(),
            ["x", "a", "y"]
        );
    }

    #[test]
    fn literal_spaces_outside_splittable_range_are_preserved() {
        let word = ExpandedText {
            value: "a b c",
            splittable: &[range(2, 3)],
            has_quoted_part: false,
        };
        assert_eq!(split_fields(&word, None, usize::MAX).unwrap(), ["a b c"]);
    }

    #[test]
    fn unquoted_empty_expansion_produces_no_fields() {
        let word = ExpandedText {
            value: "",
            splittable: &[],
            has_quoted_part: false,
        };
        assert!(split_fields(&word, None, usize::MAX).unwrap().is_empty());
    }

    #[test]
    fn quoted_empty_word_produces_one_empty_field() {
        let word = ExpandedText {
            value: "",
            splittable: &[],
            has_quoted_part: true,
        };
        assert_eq!(split_fields(&word, None, usize::MAX).unwrap(), [""]);
    }

    #[test]
    fn unquoted_whitespace_only_expansion_produces_no_fields() {
        assert!(split_all("  \t\n", None).is_empty());
    }

    #[test]
    fn quoted_whitespace_only_word_stays_one_field() {
        let word = ExpandedText {
            value: "  ",
            splittable: &[],
            has_quoted_part: true,
        };
        assert_eq!(split_fields(&word, None, usize::MAX).unwrap(), ["  "]);
    }

    #[test]
    fn empty_ifs_keeps_unquoted_nonempty_value_as_one_field() {
        assert_eq!(split_all("x", Some("")), ["x"]);
    }

    #[test]
    fn adjacent_splittable_ranges_are_accepted() {
        let word = ExpandedText {
            value: "a:b",
            splittable: &[range(0, 1), range(1, 2), range(2, 3)],
            has_quoted_part: false,
        };
        assert_eq!(
            split_fields(&word, Some(":"), usize::MAX).unwrap(),
            ["a", "b"]
        );
    }

    #[test]
    fn reversed_splittable_range_is_rejected() {
        let word = ExpandedText {
            value: "abc",
            splittable: &[range(2, 1)],
            has_quoted_part: false,
        };
        let error = split_fields(&word, None, usize::MAX).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
    }

    #[test]
    fn splittable_range_past_end_of_value_is_rejected() {
        let word = ExpandedText {
            value: "ab",
            splittable: &[range(0, 3)],
            has_quoted_part: false,
        };
        let error = split_fields(&word, None, usize::MAX).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("end is past the word"));
    }

    #[test]
    fn splittable_range_off_char_boundary_is_rejected() {
        let word = ExpandedText {
            value: "é",
            splittable: &[range(0, 1)],
            has_quoted_part: false,
        };
        let error = split_fields(&word, None, usize::MAX).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
    }

    #[test]
    fn overlapping_or_unsorted_splittable_ranges_are_rejected() {
        let overlapping = ExpandedText {
            value: "abcd",
            splittable: &[range(0, 2), range(1, 3)],
            has_quoted_part: false,
        };
        let unsorted = ExpandedText {
            value: "abcd",
            splittable: &[range(2, 3), range(0, 1)],
            has_quoted_part: false,
        };
        assert_eq!(
            split_fields(&overlapping, None, usize::MAX)
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        assert_eq!(
            split_fields(&unsorted, None, usize::MAX).unwrap_err().code,
            ErrorCode::Validation
        );
    }

    #[test]
    fn field_count_at_limit_is_accepted() {
        let word = ExpandedText {
            value: "a b c",
            splittable: &[range(0, 5)],
            has_quoted_part: false,
        };
        assert_eq!(split_fields(&word, None, 3).unwrap(), ["a", "b", "c"]);
    }

    #[test]
    fn field_count_over_limit_is_rejected_with_limit_context() {
        let word = ExpandedText {
            value: "a b c",
            splittable: &[range(0, 5)],
            has_quoted_part: false,
        };
        let error = split_fields(&word, None, 2).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit 2 fields"));
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn empty_fields_count_toward_field_limit() {
        let word = ExpandedText {
            value: "::::",
            splittable: &[range(0, 4)],
            has_quoted_part: false,
        };
        assert_eq!(split_fields(&word, Some(":"), 4).unwrap(), ["", "", "", ""]);
        assert_eq!(
            split_fields(&word, Some(":"), 3).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn zero_field_limit_rejects_the_quoted_empty_field() {
        let word = ExpandedText {
            value: "",
            splittable: &[],
            has_quoted_part: true,
        };
        assert_eq!(
            split_fields(&word, None, 0).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn field_splitting_matches_posix_shell_for_reference_cases() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        // Values avoid glob characters so that `set -- $2` cannot expand paths,
        // and every case yields at least one field so `printf` prints each one.
        let cases: &[(&str, &str)] = &[
            (" \t\n", "  a\tb\n c  "),
            (" \t\n", "\t\na\n"),
            (" ", "a  b   c"),
            (" :", " a : b "),
            (" :", "a : :b"),
            (" :", " :a"),
            (":", "a::b"),
            (":", ":a"),
            (":", "a:"),
            (":", "::"),
            (":", ":"),
            ("", "a b:c"),
        ];
        for &(ifs, value) in cases {
            let ranges = [range(0, value.len())];
            let word = ExpandedText {
                value,
                splittable: &ranges,
                has_quoted_part: false,
            };
            let ours = split_fields(&word, Some(ifs), usize::MAX).unwrap();
            let expected: String = ours.iter().map(|field| format!("<{field}>")).collect();
            let output = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "IFS=$1; set -- $2; printf '<%s>' \"$@\"",
                    "sh",
                    ifs,
                    value,
                ])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                expected,
                "IFS={ifs:?} value={value:?}"
            );
        }
    }
}
