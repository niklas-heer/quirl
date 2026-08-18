//! Bounded selection over the exact cells rendered in the current rich frame.

use quirl_core::escape_terminal_line;
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
};
use unicode_width::UnicodeWidthStr;

/// A terminal-cell boundary in the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ScreenPosition {
    pub(super) row: u16,
    pub(super) column: u16,
}

/// Ordered-independent endpoints for one selection in the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScreenSelection {
    anchor: ScreenPosition,
    head: ScreenPosition,
}

impl ScreenSelection {
    pub(super) const fn new(anchor: ScreenPosition, head: ScreenPosition) -> Self {
        Self { anchor, head }
    }

    pub(super) fn update(&mut self, head: ScreenPosition) {
        self.head = head;
    }

    fn ordered(self) -> (ScreenPosition, ScreenPosition) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CellSpan {
    column_start: u16,
    column_end: u16,
}

/// Hit-test metadata and the bounded selected text from the last complete frame.
///
/// Text is retained only for the active selection. The complete cell map is
/// bounded by the validated rich-terminal dimensions, while an arbitrarily
/// large combining grapheme cannot force a second unbounded copy of its bytes.
#[derive(Debug)]
pub(super) struct VisibleScreen {
    area: Rect,
    rows: Vec<Vec<CellSpan>>,
    selected_text: Result<Option<String>, usize>,
}

impl Default for VisibleScreen {
    fn default() -> Self {
        Self {
            area: Rect::default(),
            rows: Vec::new(),
            selected_text: Ok(None),
        }
    }
}

impl VisibleScreen {
    /// Capture cell boundaries and, when active, exact selected visible text.
    pub(super) fn capture(
        buffer: &Buffer,
        selection: Option<ScreenSelection>,
        selected_bytes_max: usize,
    ) -> Self {
        let area = buffer.area;
        let rows = (area.y..area.bottom())
            .map(|row| cell_spans(buffer, row))
            .collect();
        let selected_text = selection.map_or(Ok(None), |selection| {
            selected_text(buffer, selection, selected_bytes_max).map(Some)
        });
        Self {
            area,
            rows,
            selected_text,
        }
    }

    /// Map a visible cell to boundaries around the complete occupying grapheme.
    pub(super) fn hit_test(
        &self,
        column: u16,
        row: u16,
    ) -> Option<(ScreenPosition, ScreenPosition)> {
        if column < self.area.x
            || column >= self.area.right()
            || row < self.area.y
            || row >= self.area.bottom()
        {
            return None;
        }
        let row_index = usize::from(row.saturating_sub(self.area.y));
        let spans = self.rows.get(row_index)?;
        let span = spans
            .iter()
            .find(|span| column >= span.column_start && column < span.column_end)?;
        Some((
            ScreenPosition {
                row,
                column: span.column_start,
            },
            ScreenPosition {
                row,
                column: span.column_end,
            },
        ))
    }

    /// Return the selected text captured from the same frame as the cell map.
    pub(super) fn selected_text_bounded(&self) -> Result<Option<&str>, usize> {
        match &self.selected_text {
            Ok(text) => Ok(text.as_deref()),
            Err(observed_bytes) => Err(*observed_bytes),
        }
    }
}

/// Overlay selection style after widgets render, without changing their text.
pub(super) fn style_selection(
    buffer: &mut Buffer,
    selection: Option<ScreenSelection>,
    style: Style,
) {
    let Some(selection) = selection else {
        return;
    };
    let (start, end) = selection.ordered();
    for row in start.row..=end.row {
        if row < buffer.area.y || row >= buffer.area.bottom() {
            continue;
        }
        let column_start = if row == start.row {
            start.column
        } else {
            buffer.area.x
        }
        .max(buffer.area.x);
        let column_end = if row == end.row {
            end.column
        } else {
            buffer.area.right()
        }
        .min(buffer.area.right());
        for column in column_start..column_end {
            if let Some(cell) = buffer.cell_mut(Position::new(column, row)) {
                cell.set_style(style);
            }
        }
    }
}

fn cell_spans(buffer: &Buffer, row: u16) -> Vec<CellSpan> {
    let mut spans = Vec::with_capacity(usize::from(buffer.area.width));
    let mut column = buffer.area.x;
    while column < buffer.area.right() {
        let symbol = buffer
            .cell(Position::new(column, row))
            .map_or(" ", |cell| cell.symbol());
        let width = u16::try_from(UnicodeWidthStr::width(symbol))
            .unwrap_or(u16::MAX)
            .max(1)
            .min(buffer.area.right().saturating_sub(column));
        spans.push(CellSpan {
            column_start: column,
            column_end: column.saturating_add(width),
        });
        column = column.saturating_add(width);
    }
    spans
}

fn selected_text(
    buffer: &Buffer,
    selection: ScreenSelection,
    selected_bytes_max: usize,
) -> Result<String, usize> {
    let (start, end) = selection.ordered();
    let row_start = start.row.max(buffer.area.y);
    let row_end = end.row.min(buffer.area.bottom().saturating_sub(1));
    if row_start > row_end || start == end {
        return Ok(String::new());
    }

    let mut selected_bytes = 0_usize;
    for row in row_start..=row_end {
        selected_bytes = selected_bytes
            .saturating_add(selected_row_bytes(buffer, selection, row))
            .saturating_add(usize::from(row < row_end));
    }
    if selected_bytes > selected_bytes_max {
        return Err(selected_bytes);
    }

    let mut text = String::with_capacity(selected_bytes);
    for row in row_start..=row_end {
        push_selected_row(&mut text, buffer, selection, row);
        while text.ends_with(' ') {
            text.pop();
        }
        if row < row_end {
            text.push('\n');
        }
    }
    debug_assert_eq!(text.len(), selected_bytes);
    Ok(text)
}

fn selected_row_bytes(buffer: &Buffer, selection: ScreenSelection, row: u16) -> usize {
    let mut committed_bytes = 0_usize;
    let mut trailing_spaces = 0_usize;
    for_each_selected_symbol(buffer, selection, row, |symbol| {
        let safe_bytes = escaped_terminal_line_bytes(symbol);
        if symbol.bytes().all(|byte| byte == b' ') {
            trailing_spaces = trailing_spaces.saturating_add(safe_bytes);
        } else {
            committed_bytes = committed_bytes
                .saturating_add(trailing_spaces)
                .saturating_add(safe_bytes);
            trailing_spaces = 0;
        }
    });
    committed_bytes
}

fn push_selected_row(text: &mut String, buffer: &Buffer, selection: ScreenSelection, row: u16) {
    for_each_selected_symbol(buffer, selection, row, |symbol| {
        text.push_str(&escape_terminal_line(symbol));
    });
}

fn for_each_selected_symbol(
    buffer: &Buffer,
    selection: ScreenSelection,
    row: u16,
    mut visit: impl FnMut(&str),
) {
    let (start, end) = selection.ordered();
    let column_start = if row == start.row {
        start.column
    } else {
        buffer.area.x
    }
    .max(buffer.area.x);
    let column_end = if row == end.row {
        end.column
    } else {
        buffer.area.right()
    }
    .min(buffer.area.right());
    if column_start >= column_end {
        return;
    }

    let mut column = buffer.area.x;
    while column < buffer.area.right() {
        let symbol = buffer
            .cell(Position::new(column, row))
            .map_or(" ", |cell| cell.symbol());
        let width = u16::try_from(UnicodeWidthStr::width(symbol))
            .unwrap_or(u16::MAX)
            .max(1)
            .min(buffer.area.right().saturating_sub(column));
        let symbol_end = column.saturating_add(width);
        if symbol_end > column_start && column < column_end {
            visit(symbol);
        }
        column = symbol_end;
    }
}

fn escaped_terminal_line_bytes(symbol: &str) -> usize {
    symbol.chars().fold(0_usize, |bytes, character| {
        let character_bytes = if character.is_control() || character == '\u{009b}' {
            character
                .escape_default()
                .fold(0_usize, |escaped_bytes, escaped| {
                    escaped_bytes.saturating_add(escaped.len_utf8())
                })
        } else {
            character.len_utf8()
        };
        bytes.saturating_add(character_bytes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;

    fn position(row: u16, column: u16) -> ScreenPosition {
        ScreenPosition { row, column }
    }

    #[test]
    fn hit_testing_maps_every_wide_grapheme_cell_to_complete_boundaries() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 1));
        buffer.set_string(0, 0, "a界b", Style::default());
        let screen = VisibleScreen::capture(&buffer, None, 64);

        assert_eq!(
            screen.hit_test(1, 0),
            Some((position(0, 1), position(0, 3)))
        );
        assert_eq!(
            screen.hit_test(2, 0),
            Some((position(0, 1), position(0, 3)))
        );
    }

    #[test]
    fn selection_copies_visible_rows_and_trims_only_trailing_cell_padding() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 3));
        buffer.set_string(0, 0, "folder", Style::default());
        buffer.set_string(9, 0, "git", Style::default());
        buffer.set_string(0, 1, "❯ run", Style::default());
        buffer.set_string(0, 2, "NORMAL bar", Style::default());
        let selection = ScreenSelection::new(position(0, 0), position(2, 10));
        let screen = VisibleScreen::capture(&buffer, Some(selection), 1_024);

        assert_eq!(
            screen.selected_text_bounded(),
            Ok(Some("folder   git\n❯ run\nNORMAL bar"))
        );
    }

    #[test]
    fn selection_reports_the_complete_size_before_allocating_over_the_copy_limit() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 2));
        buffer.set_string(0, 0, "abcdefgh", Style::default());
        buffer.set_string(0, 1, "ijklmnop", Style::default());
        let selection = ScreenSelection::new(position(0, 0), position(1, 8));
        let screen = VisibleScreen::capture(&buffer, Some(selection), 8);

        assert_eq!(screen.selected_text_bounded(), Err(17));
    }

    #[test]
    fn copied_controls_are_escaped_even_if_a_low_level_cell_contains_one() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer[(0, 0)].set_symbol("\u{1b}");
        buffer[(1, 0)].set_char('x');
        let selection = ScreenSelection::new(position(0, 0), position(0, 2));
        let screen = VisibleScreen::capture(&buffer, Some(selection), 64);

        assert_eq!(screen.selected_text_bounded(), Ok(Some("\\u{1b}x")));
    }

    #[test]
    fn escaped_size_is_computed_without_constructing_the_safe_copy() {
        assert_eq!(escaped_terminal_line_bytes("a\n\t\u{1b}界"), 14);
    }
}
