//! Bounded logical output retained by the full-screen terminal surface.

use std::{collections::VecDeque, ops::Range};
#[cfg(test)]
use unicode_segmentation::UnicodeSegmentation;
#[cfg(test)]
use unicode_width::UnicodeWidthStr;

/// Explicit memory and collection limits for one transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TranscriptLimits {
    /// Maximum number of logical lines retained at once.
    pub(super) line_count_max: usize,
    /// Maximum number of UTF-8 content bytes retained across all lines.
    ///
    /// Logical separators are bounded separately by `line_count_max` and are
    /// materialized only when selected text is copied.
    pub(super) retained_bytes_max: usize,
}

/// A byte position inside one retained logical line.
///
/// Positions supplied by terminal hit testing may fall inside a UTF-8 code
/// point or beyond an evicted line. Selection entry points clamp them backward
/// to a valid character boundary in the nearest retained line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct TextPosition {
    /// Zero-based line index relative to the oldest retained line.
    pub(super) line_index: usize,
    /// Zero-based UTF-8 byte offset in the logical line.
    pub(super) byte_offset: usize,
}

/// Changes caused by appending one logical line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct AppendOutcome {
    /// Complete oldest lines removed to restore the configured bounds.
    pub(super) evicted_line_count: usize,
    /// UTF-8 content bytes removed with complete oldest lines.
    pub(super) evicted_bytes: usize,
    /// Prefix bytes omitted when the appended line alone exceeded the byte bound.
    pub(super) truncated_prefix_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: TextPosition,
    head: TextPosition,
}

/// Plain-text output with bounded retention, viewport navigation, and selection.
///
/// Lines are stored without implicit newline bytes. Copying a multi-line
/// selection inserts exactly one `\n` between adjacent logical lines. Whole-line
/// eviction preserves a scrolled viewport when possible and clamps selection
/// endpoints whose text has been evicted.
#[derive(Debug)]
pub(super) struct Transcript {
    limits: TranscriptLimits,
    lines: VecDeque<String>,
    retained_bytes: usize,
    /// `None` follows the tail; `Some(index)` anchors the viewport from the front.
    scroll_top: Option<usize>,
    selection: Option<Selection>,
}

impl Transcript {
    /// Construct an empty transcript with positive explicit bounds.
    ///
    /// Limits are an internal configuration invariant rather than user input.
    pub(super) fn new(limits: TranscriptLimits) -> Self {
        assert!(
            limits.line_count_max > 0,
            "transcript line limit must be positive"
        );
        assert!(
            limits.retained_bytes_max > 0,
            "transcript byte limit must be positive"
        );
        Self {
            limits,
            lines: VecDeque::new(),
            retained_bytes: 0,
            scroll_top: None,
            selection: None,
        }
    }

    /// Return the number of retained logical lines.
    pub(super) fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Return the number of retained UTF-8 content bytes.
    #[cfg(test)]
    pub(super) fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Return a retained logical line by its zero-based index.
    pub(super) fn line(&self, line_index: usize) -> Option<&str> {
        self.lines.get(line_index).map(String::as_str)
    }

    /// Append one logical line and evict complete oldest lines as necessary.
    ///
    /// A line larger than the complete byte budget retains its largest
    /// UTF-8-safe suffix. Returning the truncation and eviction counts lets the
    /// renderer expose data loss without adding an unbounded diagnostic record.
    pub(super) fn append_line(&mut self, line: &str) -> AppendOutcome {
        let (line, truncated_prefix_bytes) = utf8_suffix(line, self.limits.retained_bytes_max);
        self.lines.push_back(line.to_owned());
        self.retained_bytes = self.retained_bytes.saturating_add(line.len());

        let mut outcome = AppendOutcome {
            truncated_prefix_bytes,
            ..AppendOutcome::default()
        };
        while self.lines.len() > self.limits.line_count_max
            || self.retained_bytes > self.limits.retained_bytes_max
        {
            let Some(evicted) = self.lines.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(evicted.len());
            outcome.evicted_line_count = outcome.evicted_line_count.saturating_add(1);
            outcome.evicted_bytes = outcome.evicted_bytes.saturating_add(evicted.len());
        }
        self.adjust_after_front_eviction(outcome.evicted_line_count);
        debug_assert!(self.lines.len() <= self.limits.line_count_max);
        debug_assert!(self.retained_bytes <= self.limits.retained_bytes_max);
        outcome
    }

    /// Return whether the viewport is pinned to the newest retained output.
    pub(super) fn follows_tail(&self) -> bool {
        self.scroll_top.is_none()
    }

    /// Return the half-open retained-line range visible at the requested height.
    pub(super) fn visible_range(&self, visible_line_count: usize) -> Range<usize> {
        if visible_line_count == 0 || self.lines.is_empty() {
            return 0..0;
        }
        let maximum_start = self.lines.len().saturating_sub(visible_line_count);
        let start = self.scroll_top.unwrap_or(maximum_start).min(maximum_start);
        start
            ..start
                .saturating_add(visible_line_count)
                .min(self.lines.len())
    }

    /// Move toward older output by one overlapping page.
    ///
    /// Returns `true` only when the visible range changes.
    pub(super) fn page_up(&mut self, visible_line_count: usize) -> bool {
        self.scroll_up(
            visible_line_count.saturating_sub(1).max(1),
            visible_line_count,
        )
    }

    /// Move toward older output by at most `line_count` logical lines.
    ///
    /// This is the bounded primitive used for terminal mouse-wheel events.
    /// Page navigation delegates to it with one line of overlap.
    pub(super) fn scroll_up(&mut self, line_count: usize, visible_line_count: usize) -> bool {
        if line_count == 0 || visible_line_count == 0 || self.lines.is_empty() {
            return false;
        }
        let current_start = self.visible_range(visible_line_count).start;
        let next_start = current_start.saturating_sub(line_count);
        if next_start == current_start {
            return false;
        }
        self.scroll_top = Some(next_start);
        true
    }

    /// Move toward newer output by one overlapping page.
    ///
    /// Reaching the newest complete page resumes automatic tail following.
    pub(super) fn page_down(&mut self, visible_line_count: usize) -> bool {
        self.scroll_down(
            visible_line_count.saturating_sub(1).max(1),
            visible_line_count,
        )
    }

    /// Move toward newer output by at most `line_count` logical lines.
    ///
    /// Reaching the newest complete page resumes automatic tail following.
    pub(super) fn scroll_down(&mut self, line_count: usize, visible_line_count: usize) -> bool {
        if line_count == 0 || visible_line_count == 0 || self.lines.is_empty() {
            return false;
        }
        let current_start = self.visible_range(visible_line_count).start;
        let maximum_start = self.lines.len().saturating_sub(visible_line_count);
        let next_start = current_start.saturating_add(line_count).min(maximum_start);
        if next_start == current_start && self.follows_tail() {
            return false;
        }
        self.scroll_top = (next_start < maximum_start).then_some(next_start);
        true
    }

    /// Resume following the newest retained output.
    ///
    /// Returns `true` when an explicit scroll position was cleared.
    pub(super) fn scroll_to_end(&mut self) -> bool {
        self.scroll_top.take().is_some()
    }

    /// Move to an exact retained-line viewport start.
    ///
    /// Starts at or beyond the newest complete viewport resume tail following.
    /// This is the bounded target used by scrollbar pointer interaction.
    pub(super) fn scroll_to(&mut self, start: usize, visible_line_count: usize) -> bool {
        if visible_line_count == 0 || self.lines.is_empty() {
            return false;
        }
        let previous = self.visible_range(visible_line_count).start;
        let maximum_start = self.lines.len().saturating_sub(visible_line_count);
        let next = start.min(maximum_start);
        self.scroll_top = (next < maximum_start).then_some(next);
        previous != next
    }

    /// Map one terminal cell to the UTF-8 boundaries around its grapheme.
    ///
    /// The first position is immediately before the hit grapheme and the second
    /// is immediately after it. Wide graphemes map every occupied cell to the
    /// same pair. Columns at or beyond line end map to the final byte boundary.
    #[cfg(test)]
    pub(super) fn hit_test(
        &self,
        line_index: usize,
        display_column: usize,
    ) -> Option<(TextPosition, TextPosition)> {
        let line = self.lines.get(line_index)?;
        let mut occupied_columns = 0_usize;
        for (byte_offset, grapheme) in line.grapheme_indices(true) {
            let grapheme_columns = UnicodeWidthStr::width(grapheme);
            let next_columns = occupied_columns.saturating_add(grapheme_columns);
            if grapheme_columns > 0 && display_column < next_columns {
                return Some((
                    TextPosition {
                        line_index,
                        byte_offset,
                    },
                    TextPosition {
                        line_index,
                        byte_offset: byte_offset.saturating_add(grapheme.len()),
                    },
                ));
            }
            occupied_columns = next_columns;
        }
        let end = TextPosition {
            line_index,
            byte_offset: line.len(),
        };
        Some((end, end))
    }

    /// Start a selection at the nearest valid retained text position.
    pub(super) fn begin_selection(&mut self, position: TextPosition) {
        self.selection = self.clamp_position(position).map(|position| Selection {
            anchor: position,
            head: position,
        });
    }

    /// Move the active selection head to the nearest valid retained position.
    pub(super) fn update_selection(&mut self, position: TextPosition) {
        let Some(position) = self.clamp_position(position) else {
            self.selection = None;
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            selection.head = position;
        }
    }

    /// Clear the active selection, if any.
    pub(super) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Return the active selection as exact logical text within an allocation limit.
    ///
    /// A caret selection returns an empty string. The complete byte cost is
    /// computed before allocation; `Err` reports the observed size when it
    /// exceeds `bytes_max`.
    pub(super) fn selected_text_bounded(&self, bytes_max: usize) -> Result<Option<String>, usize> {
        let Some(selection) = self.selection else {
            return Ok(None);
        };
        let (start, end) = ordered(selection.anchor, selection.head);
        let mut selected_bytes = 0_usize;
        for line_index in start.line_index..=end.line_index {
            let line = self.lines.get(line_index).ok_or(0_usize)?;
            let byte_start = if line_index == start.line_index {
                start.byte_offset
            } else {
                0
            };
            let byte_end = if line_index == end.line_index {
                end.byte_offset
            } else {
                line.len()
            };
            selected_bytes = selected_bytes
                .saturating_add(byte_end.saturating_sub(byte_start))
                .saturating_add(usize::from(line_index < end.line_index));
        }
        if selected_bytes > bytes_max {
            return Err(selected_bytes);
        }

        let mut selected = String::with_capacity(selected_bytes);
        for line_index in start.line_index..=end.line_index {
            let line = self.lines.get(line_index).ok_or(selected_bytes)?;
            let byte_start = if line_index == start.line_index {
                start.byte_offset
            } else {
                0
            };
            let byte_end = if line_index == end.line_index {
                end.byte_offset
            } else {
                line.len()
            };
            selected.push_str(line.get(byte_start..byte_end).ok_or(selected_bytes)?);
            if line_index < end.line_index {
                selected.push('\n');
            }
        }
        Ok(Some(selected))
    }

    /// Replace the most recently appended logical line in place.
    ///
    /// Used for carriage-return-driven progress updates (`git push`, `curl`,
    /// package-manager progress bars, and similar) whose child process
    /// repeatedly overwrites one in-flight line rather than emitting a new
    /// one. Appending each update would grow the transcript unboundedly and
    /// leave stale intermediate frames in scrollback; replacing keeps exactly
    /// one retained line for the in-progress update, matching what a real
    /// terminal displays. Falls back to [`Self::append_line`] when the
    /// transcript is empty. Any active selection is re-clamped against the
    /// replaced line's new length, since an in-place update can shrink or
    /// grow the line underneath an in-progress mouse selection.
    pub(super) fn replace_last_line(&mut self, line: &str) -> AppendOutcome {
        if let Some(previous) = self.lines.pop_back() {
            self.retained_bytes = self.retained_bytes.saturating_sub(previous.len());
        }
        let outcome = self.append_line(line);
        if let Some(selection) = self.selection {
            self.selection = self.clamp_position(selection.anchor).and_then(|anchor| {
                self.clamp_position(selection.head)
                    .map(|head| Selection { anchor, head })
            });
        }
        outcome
    }

    /// Return the ordered active selection endpoints for viewport styling.
    pub(super) fn selection_range(&self) -> Option<(TextPosition, TextPosition)> {
        self.selection
            .map(|selection| ordered(selection.anchor, selection.head))
    }

    fn clamp_position(&self, position: TextPosition) -> Option<TextPosition> {
        let line_index = position.line_index.min(self.lines.len().checked_sub(1)?);
        let line = self.lines.get(line_index)?;
        let mut byte_offset = position.byte_offset.min(line.len());
        while byte_offset > 0 && !line.is_char_boundary(byte_offset) {
            byte_offset = byte_offset.saturating_sub(1);
        }
        Some(TextPosition {
            line_index,
            byte_offset,
        })
    }

    fn adjust_after_front_eviction(&mut self, evicted_line_count: usize) {
        if evicted_line_count == 0 {
            return;
        }
        if let Some(scroll_top) = self.scroll_top.as_mut() {
            *scroll_top = scroll_top.saturating_sub(evicted_line_count);
        }
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        selection.anchor = after_front_eviction(selection.anchor, evicted_line_count);
        selection.head = after_front_eviction(selection.head, evicted_line_count);
        if self.lines.is_empty() {
            self.selection = None;
        }
    }
}

#[allow(
    clippy::string_slice,
    reason = "the suffix start is advanced until Rust confirms it is a UTF-8 boundary"
)]
fn utf8_suffix(text: &str, retained_bytes_max: usize) -> (&str, usize) {
    let mut start = text.len().saturating_sub(retained_bytes_max);
    while start < text.len() && !text.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    (&text[start..], start)
}

fn ordered(left: TextPosition, right: TextPosition) -> (TextPosition, TextPosition) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

fn after_front_eviction(position: TextPosition, evicted_line_count: usize) -> TextPosition {
    if position.line_index < evicted_line_count {
        TextPosition {
            line_index: 0,
            byte_offset: 0,
        }
    } else {
        TextPosition {
            line_index: position.line_index.saturating_sub(evicted_line_count),
            byte_offset: position.byte_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(line_count_max: usize, retained_bytes_max: usize) -> Transcript {
        Transcript::new(TranscriptLimits {
            line_count_max,
            retained_bytes_max,
        })
    }

    fn position(line_index: usize, byte_offset: usize) -> TextPosition {
        TextPosition {
            line_index,
            byte_offset,
        }
    }

    fn visible_lines(transcript: &Transcript, visible_line_count: usize) -> Vec<&str> {
        transcript
            .visible_range(visible_line_count)
            .map(|line_index| transcript.line(line_index).unwrap())
            .collect()
    }

    fn selected_text(transcript: &Transcript) -> Option<String> {
        transcript.selected_text_bounded(1_024).unwrap()
    }

    #[test]
    fn append_evicts_complete_oldest_lines_at_both_limits() {
        let mut transcript = transcript(3, 8);
        assert_eq!(transcript.append_line("one"), AppendOutcome::default());
        assert_eq!(transcript.append_line("two"), AppendOutcome::default());

        let byte_eviction = transcript.append_line("three");
        assert_eq!(byte_eviction.evicted_line_count, 1);
        assert_eq!(byte_eviction.evicted_bytes, 3);
        assert_eq!(visible_lines(&transcript, 8), ["two", "three"]);
        assert_eq!(transcript.retained_bytes(), 8);

        transcript.append_line("");
        let line_eviction = transcript.append_line("");
        assert_eq!(line_eviction.evicted_line_count, 1);
        assert_eq!(visible_lines(&transcript, 8), ["three", "", ""]);
        assert_eq!(transcript.line_count(), 3);
    }

    #[test]
    fn replace_last_line_updates_content_without_growing_line_count() {
        let mut transcript = transcript(8, 1_024);
        transcript.append_line("one");
        transcript.append_line("Counting objects:  10%");

        transcript.replace_last_line("Counting objects:  55%");
        assert_eq!(transcript.line_count(), 2);
        assert_eq!(
            visible_lines(&transcript, 8),
            ["one", "Counting objects:  55%"]
        );

        transcript.replace_last_line("Counting objects: 100%, done.");
        assert_eq!(transcript.line_count(), 2);
        assert_eq!(
            visible_lines(&transcript, 8),
            ["one", "Counting objects: 100%, done."]
        );
    }

    #[test]
    fn replace_last_line_on_empty_transcript_behaves_like_append() {
        let mut transcript = transcript(8, 1_024);
        transcript.replace_last_line("first");
        assert_eq!(transcript.line_count(), 1);
        assert_eq!(visible_lines(&transcript, 8), ["first"]);
    }

    #[test]
    fn replace_last_line_reclamps_a_selection_shortened_underneath_it() {
        let mut transcript = transcript(8, 1_024);
        transcript.append_line("stable");
        transcript.append_line("progress: 10% almost there");
        transcript.begin_selection(position(1, 20));
        transcript.update_selection(position(1, 24));
        assert_eq!(selected_text(&transcript).as_deref(), Some(" the"));

        transcript.replace_last_line("progress: 20%");

        assert_eq!(selected_text(&transcript).as_deref(), Some(""));
        assert_eq!(visible_lines(&transcript, 8), ["stable", "progress: 20%"]);
    }

    #[test]
    fn oversized_line_retains_a_utf8_safe_bounded_suffix() {
        let mut transcript = transcript(4, 5);
        transcript.append_line("old");

        let outcome = transcript.append_line("ab💚cd");

        assert_eq!(outcome.truncated_prefix_bytes, 6);
        assert_eq!(outcome.evicted_line_count, 0);
        assert_eq!(transcript.line(1), Some("cd"));
        assert_eq!(transcript.retained_bytes(), 5);
    }

    #[test]
    fn page_navigation_preserves_scrolled_anchor_and_end_follows_tail() {
        let mut transcript = transcript(20, 1_024);
        for line in 0..10 {
            transcript.append_line(&format!("line-{line}"));
        }
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-6", "line-7", "line-8", "line-9"]
        );
        assert!(transcript.follows_tail());

        assert!(transcript.page_up(4));
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-3", "line-4", "line-5", "line-6"]
        );
        transcript.append_line("line-10");
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-3", "line-4", "line-5", "line-6"]
        );

        assert!(transcript.page_down(4));
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-6", "line-7", "line-8", "line-9"]
        );
        assert!(transcript.page_down(4));
        assert!(transcript.follows_tail());
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-7", "line-8", "line-9", "line-10"]
        );

        assert!(transcript.page_up(4));
        assert!(transcript.scroll_to_end());
        assert!(transcript.follows_tail());
    }

    #[test]
    fn line_navigation_supports_bounded_mouse_wheel_steps() {
        let mut transcript = transcript(20, 1_024);
        for line in 0..10 {
            transcript.append_line(&format!("line-{line}"));
        }

        assert!(transcript.scroll_up(3, 4));
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-3", "line-4", "line-5", "line-6"]
        );
        assert!(transcript.scroll_down(2, 4));
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-5", "line-6", "line-7", "line-8"]
        );
        assert!(transcript.scroll_down(3, 4));
        assert!(transcript.follows_tail());
        assert_eq!(
            visible_lines(&transcript, 4),
            ["line-6", "line-7", "line-8", "line-9"]
        );
        assert!(!transcript.scroll_up(0, 4));
        assert!(!transcript.scroll_down(1, 0));
    }

    #[test]
    fn exact_scroll_targets_clamp_and_resume_tail_following() {
        let mut transcript = transcript(20, 1_024);
        for line in 0..10 {
            transcript.append_line(&format!("line-{line}"));
        }

        assert!(transcript.scroll_to(2, 4));
        assert_eq!(transcript.visible_range(4), 2..6);
        assert!(!transcript.follows_tail());
        assert!(transcript.scroll_to(usize::MAX, 4));
        assert_eq!(transcript.visible_range(4), 6..10);
        assert!(transcript.follows_tail());
        assert!(!transcript.scroll_to(6, 4));
    }

    #[test]
    fn terminal_hit_testing_preserves_grapheme_and_utf8_boundaries() {
        let mut transcript = transcript(4, 1_024);
        transcript.append_line("a界e\u{301}");

        assert_eq!(
            transcript.hit_test(0, 0),
            Some((position(0, 0), position(0, 1)))
        );
        assert_eq!(
            transcript.hit_test(0, 1),
            Some((position(0, 1), position(0, 4)))
        );
        assert_eq!(
            transcript.hit_test(0, 2),
            Some((position(0, 1), position(0, 4)))
        );
        assert_eq!(
            transcript.hit_test(0, 3),
            Some((position(0, 4), position(0, 7)))
        );
        assert_eq!(
            transcript.hit_test(0, 20),
            Some((position(0, 7), position(0, 7)))
        );
        assert_eq!(transcript.hit_test(1, 0), None);
    }

    #[test]
    fn front_eviction_keeps_surviving_scrolled_content_anchored() {
        let mut transcript = transcript(5, 1_024);
        for line in 0..5 {
            transcript.append_line(&format!("line-{line}"));
        }
        assert!(transcript.page_up(2));
        assert!(transcript.page_up(2));
        assert_eq!(visible_lines(&transcript, 2), ["line-1", "line-2"]);

        transcript.append_line("line-5");

        assert_eq!(visible_lines(&transcript, 2), ["line-1", "line-2"]);
        assert!(!transcript.follows_tail());
    }

    #[test]
    fn selection_extracts_exact_forward_and_reverse_multiline_text() {
        let mut transcript = transcript(8, 1_024);
        transcript.append_line("alpha");
        transcript.append_line("βeta");
        transcript.append_line("omega");

        transcript.begin_selection(position(0, 2));
        transcript.update_selection(position(2, 3));
        assert_eq!(
            selected_text(&transcript).as_deref(),
            Some("pha\nβeta\nome")
        );

        transcript.begin_selection(position(2, 3));
        transcript.update_selection(position(0, 2));
        assert_eq!(
            selected_text(&transcript).as_deref(),
            Some("pha\nβeta\nome")
        );
        transcript.clear_selection();
        assert_eq!(selected_text(&transcript), None);
    }

    #[test]
    fn selection_clamps_utf8_positions_and_front_eviction() {
        let mut transcript = transcript(3, 1_024);
        transcript.append_line("old");
        transcript.append_line("a💚b");
        transcript.append_line("tail");
        transcript.begin_selection(position(0, 1));
        transcript.update_selection(position(1, 3));
        assert_eq!(selected_text(&transcript).as_deref(), Some("ld\na"));

        transcript.append_line("new");

        assert_eq!(selected_text(&transcript).as_deref(), Some("a"));
    }

    #[test]
    fn oversized_selection_reports_cost_before_copy_allocation() {
        let mut transcript = transcript(8, 1_024);
        transcript.append_line("alpha");
        transcript.append_line("omega");
        transcript.begin_selection(position(0, 0));
        transcript.update_selection(position(1, 5));

        assert_eq!(transcript.selected_text_bounded(5), Err(11));
        assert_eq!(
            transcript.selected_text_bounded(11).unwrap().as_deref(),
            Some("alpha\nomega")
        );
    }

    #[test]
    fn zero_height_navigation_is_a_bounded_no_op() {
        let mut transcript = transcript(2, 8);
        transcript.append_line("line");
        assert_eq!(transcript.visible_range(0), 0..0);
        assert!(!transcript.page_up(0));
        assert!(!transcript.page_down(0));
        assert!(transcript.follows_tail());
    }

    #[test]
    #[should_panic(expected = "transcript line limit must be positive")]
    fn zero_line_limit_violates_the_internal_configuration_invariant() {
        let _ = transcript(0, 8);
    }

    #[test]
    #[should_panic(expected = "transcript byte limit must be positive")]
    fn zero_byte_limit_violates_the_internal_configuration_invariant() {
        let _ = transcript(2, 0);
    }
}
