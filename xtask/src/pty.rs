//! Bounded Unix PTY ownership and the deterministic VT screen used by xtask.

use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    poll::{PollFd, PollFlags, PollTimeout, poll},
    pty::{ForkptyResult, Winsize, forkpty},
    sys::{
        signal::{Signal, kill},
        termios::{Termios, tcgetattr},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::{Pid, getpgrp, tcgetpgrp},
};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, AsRawFd},
        unix::ffi::OsStrExt,
    },
    path::PathBuf,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthChar;

use crate::TaskError;

/// Default deadline for one bounded PTY wait.
///
/// A shared, CPU-constrained CI runner can be slow enough under load that a
/// correct interactive round trip (render a completion popup, stream a large
/// file, exit cleanly) misses a short deadline with nothing actually wrong;
/// reproduced locally by adding artificial CPU contention, and observed on
/// the actual CI runner even at 15s. `GITHUB_ACTIONS` (set by every GitHub
/// Actions job) selects a much more generous bound there, since wasting a
/// few extra seconds of CI wall time beats a false failure; a real hang is
/// still caught well within a job's multi-minute budget either way.
///
/// A check waiting on completed automatic command discovery can need more
/// than one background pass: `index::BACKGROUND_DISCOVERY_DEADLINE` (30s)
/// bounds a single attempt, but a fresh command not covered by the quick
/// first-run fallback may only be found after `index::
/// DISCOVERY_REFRESH_INTERVAL` (60s) brings the next one around -- 60s alone
/// was confirmed insufficient on real CI even after raising it once. 100s
/// clears one attempt plus one full refresh interval with real margin, and
/// stays under `validate_timeout`'s own 150s ceiling.
pub(super) fn default_timeout() -> Duration {
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        Duration::from_secs(100)
    } else {
        Duration::from_secs(15)
    }
}
const DEFAULT_OUTPUT_BYTES_MAX: usize = 16 * 1024 * 1024;
const READ_BYTES_MAX: usize = 64 * 1024;
const SCREEN_CELLS_MAX: usize = 512 * 512;
// Independent of retained PTY output, which sustained tests may clear. The
// largest grid retains at most 64 MiB of cell text, plus fixed cell metadata.
const CELL_TEXT_BYTES_MAX: usize = 256;
const CONTROL_SEQUENCE_CHARS_MAX: usize = 128;
const SVG_BYTES_MAX: usize = 16 * 1024 * 1024;
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
// Each drain waits out its quantum, even if the expected state arrived early.
// Keep predicate polling responsive without changing read bounds or deadlines.
const WAIT_POLL_QUANTUM: Duration = Duration::from_millis(16);

pub(super) mod key {
    pub const ALT_Q: &[u8] = b"\x1bq";
    pub const CTRL_C: &[u8] = b"\x03";
    pub const CTRL_D: &[u8] = b"\x04";
    pub const CTRL_L: &[u8] = b"\x0c";
    pub const CTRL_U: &[u8] = b"\x15";
    pub const ESCAPE: &[u8] = b"\x1b";
    pub const ENTER: &[u8] = b"\r";
    pub const TAB: &[u8] = b"\t";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParserState {
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

#[derive(Debug, Default)]
enum FrameFinalization {
    #[default]
    None,
    AttributesReset,
    CursorShown,
}

// Style is owned by each fixed-grid cell: scrolling/resizing cannot separate
// a glyph from its attributes. SGR only changes the pen for subsequent writes;
// erases use its background. CSI and SVG limits also bound style parsing/output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb(u8, u8, u8);

const DEFAULT_FOREGROUND: Rgb = Rgb(216, 222, 233);
const DEFAULT_BACKGROUND: Rgb = Rgb(16, 20, 28);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CellStyle {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    underline_color: Option<Rgb>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: u8,
    inverse: bool,
    hidden: bool,
    strike: bool,
    overline: bool,
}

#[derive(Debug, Clone)]
struct Cell {
    text: String,
    style: CellStyle,
}

impl Cell {
    fn blank(style: CellStyle) -> Self {
        Self {
            text: " ".to_owned(),
            style,
        }
    }
}

/// Bounded terminal model for assertions about visible cells rather than stale bytes.
#[derive(Debug)]
pub(super) struct VirtualScreen {
    rows: usize,
    columns: usize,
    cells: Vec<Vec<Cell>>,
    pen: CellStyle,
    cell_text_overflow: bool,
    cursor_row: usize,
    cursor_column: usize,
    cursor_visible: bool,
    saved_cursor: (usize, usize),
    saved_pen: CellStyle,
    scroll_top: usize,
    scroll_bottom: usize,
    wrap_pending: bool,
    state: ParserState,
    control: String,
    utf8_pending: Vec<u8>,
    frame_finalization: FrameFinalization,
    frame_complete: bool,
}

impl VirtualScreen {
    pub(super) fn new(rows: usize, columns: usize, initial_cursor_row: usize) -> io::Result<Self> {
        validate_screen_size(rows, columns)?;
        let cursor_row = initial_cursor_row.min(rows.saturating_sub(1));
        Ok(Self {
            rows,
            columns,
            cells: blank_cells(rows, columns, CellStyle::default()),
            pen: CellStyle::default(),
            cell_text_overflow: false,
            cursor_row,
            cursor_column: 0,
            cursor_visible: true,
            saved_cursor: (cursor_row, 0),
            saved_pen: CellStyle::default(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            wrap_pending: false,
            state: ParserState::Ground,
            control: String::with_capacity(CONTROL_SEQUENCE_CHARS_MAX),
            utf8_pending: Vec::with_capacity(READ_BYTES_MAX.saturating_add(4)),
            frame_finalization: FrameFinalization::None,
            frame_complete: false,
        })
    }

    pub(super) fn resize(&mut self, rows: usize, columns: usize) -> io::Result<()> {
        validate_screen_size(rows, columns)?;
        // The kernel need not emit a resize event when geometry is unchanged.
        // Preserve the visible frame and parser/cursor state in that case;
        // otherwise a valid screen can remain falsely incomplete forever.
        if self.rows == rows && self.columns == columns {
            return Ok(());
        }
        self.frame_complete = false;
        self.frame_finalization = FrameFinalization::None;
        let mut resized = blank_cells(rows, columns, CellStyle::default());
        for (target, source) in resized.iter_mut().zip(&self.cells) {
            for (target_cell, source_cell) in target.iter_mut().zip(source) {
                target_cell.clone_from(source_cell);
            }
            // A narrower grid cannot retain a leading wide glyph after its
            // continuation cell was clipped away.
            if let Some(last) = target.last_mut()
                && last.text.chars().next().and_then(UnicodeWidthChar::width) == Some(2)
            {
                last.text = " ".to_owned();
            }
        }
        self.rows = rows;
        self.columns = columns;
        self.cells = resized;
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_column = self.cursor_column.min(columns.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.wrap_pending = false;
        Ok(())
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "the escape scanner validates the fixed CSI prefix length before slicing"
    )]
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        if self.cell_text_overflow {
            return Vec::new();
        }
        self.utf8_pending.extend_from_slice(bytes);
        let mut characters = Vec::new();
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(text) => {
                    characters.extend(text.chars());
                    self.utf8_pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        if let Ok(text) = std::str::from_utf8(&self.utf8_pending[..valid]) {
                            characters.extend(text.chars());
                        }
                        self.utf8_pending.drain(..valid);
                    }
                    let Some(invalid_bytes) = error.error_len() else {
                        break;
                    };
                    self.utf8_pending
                        .drain(..invalid_bytes.min(self.utf8_pending.len()));
                    characters.push('\u{fffd}');
                }
            }
        }

        let mut replies = Vec::new();
        for character in characters {
            if self.cell_text_overflow {
                break;
            }
            if let Some(reply) = self.feed_character(character) {
                replies.push(reply);
            }
        }
        replies
    }

    /// Number of terminal columns represented by the current grid.
    pub(super) const fn columns(&self) -> usize {
        self.columns
    }

    pub(super) fn lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    pub(super) fn text(&self) -> String {
        self.lines().join("\n")
    }

    pub(super) fn bottom_line(&self) -> String {
        self.cells
            .last()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .unwrap_or_default()
    }

    /// Whether current cells end at Ratatui's completed visible-cursor frame.
    /// Ratatui emits an attribute reset, cursor show, and final cursor position
    /// after its cell diff. Later drawing or cursor movement invalidates this
    /// observation. Incomplete UTF-8 or VT sequences are never frame boundaries.
    /// This recognizes this harness's rich surface, not arbitrary terminal apps.
    pub(super) fn has_completed_frame(&self) -> bool {
        !self.cell_text_overflow
            && self.frame_complete
            && self.state == ParserState::Ground
            && self.utf8_pending.is_empty()
    }

    /// Render styled VT cells and the cursor into a bounded, self-contained SVG.
    ///
    /// ANSI colors use a fixed xterm palette and explicit default colors; RGB
    /// colors are retained exactly. Bold, dim, italic, underline, inverse, conceal,
    /// strike and overline are modeled. Blink, font shaping, terminal themes and
    /// actual terminal pixels are not reproduced. Cells occupy 10 by 20 SVG units
    /// with a 12-unit margin. Reject output above 16 MiB before appending excess.
    pub(super) fn to_svg(&self) -> io::Result<String> {
        self.validate_model()?;
        let width = self.columns.saturating_mul(10).saturating_add(24);
        let height = self.rows.saturating_mul(20).saturating_add(24);
        let mut svg = SvgBuffer(String::new());
        svg.push(&format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">\n<title>Styled VT cell model</title>\n<desc>Modeled SGR colors, styles and cursor using a fixed xterm palette; not terminal pixels, font shaping, terminal themes or animated blink.</desc>\n<rect width=\"100%\" height=\"100%\" fill=\"#10141c\"/>\n"
        ))?;
        // Paint all backgrounds first: a wide glyph must not be covered by its
        // continuation cell's background when foregrounds are drawn afterward.
        for (row_index, row) in self.cells.iter().enumerate() {
            for (column, cell) in row.iter().enumerate() {
                let (_, background) = cell.style.colors();
                if background != DEFAULT_BACKGROUND {
                    let x = column.saturating_mul(10).saturating_add(12);
                    let y = row_index.saturating_mul(20).saturating_add(12);
                    svg.push(&format!(
                        "<rect x=\"{x}\" y=\"{y}\" width=\"10\" height=\"20\" fill=\"{}\"/>\n",
                        background.hex()
                    ))?;
                }
            }
        }
        svg.push("<g font-family=\"monospace\" font-size=\"16\" xml:space=\"preserve\">\n")?;
        for (row_index, row) in self.cells.iter().enumerate() {
            for (column, cell) in row.iter().enumerate() {
                svg.cell(cell, row_index, column)?;
            }
        }
        svg.push("</g>\n")?;
        if self.cursor_visible {
            let x = self.cursor_column.saturating_mul(10).saturating_add(12);
            let y = self.cursor_row.saturating_mul(20).saturating_add(12);
            svg.push(&format!("<rect id=\"cursor\" x=\"{x}\" y=\"{y}\" width=\"10\" height=\"20\" fill=\"none\" stroke=\"#d8dee9\"/>\n"))?;
        }
        svg.push("</svg>\n")?;
        Ok(svg.0)
    }

    /// An overflowing grapheme invalidates this model permanently; captures and
    /// PTY reads fail rather than treating truncated cells as visual evidence.
    fn validate_model(&self) -> io::Result<()> {
        if self.cell_text_overflow {
            return Err(io::Error::other(format!(
                "terminal cell text exceeds byte limit {CELL_TEXT_BYTES_MAX}; screen model is invalid"
            )));
        }
        Ok(())
    }

    fn feed_character(&mut self, character: char) -> Option<Vec<u8>> {
        match self.state {
            ParserState::Ground => self.ground(character),
            ParserState::Escape => self.escape(character),
            ParserState::Csi => self.csi(character),
            ParserState::Osc => {
                if character == '\u{7}' {
                    self.state = ParserState::Ground;
                } else if character == '\u{1b}' {
                    self.state = ParserState::OscEscape;
                }
                None
            }
            ParserState::OscEscape => {
                self.state = if character == '\\' {
                    ParserState::Ground
                } else {
                    ParserState::Osc
                };
                None
            }
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "terminal cursor coordinates are maintained within the fixed screen bounds"
    )]
    fn ground(&mut self, character: char) -> Option<Vec<u8>> {
        if character != '\u{1b}' {
            self.frame_finalization = FrameFinalization::None;
        }
        if matches!(character, '\r' | '\n' | '\u{b}' | '\u{c}' | '\u{8}' | '\t')
            || (character >= ' ' && character != '\u{7f}')
        {
            self.frame_complete = false;
        }
        match character {
            '\u{1b}' => self.state = ParserState::Escape,
            '\r' => {
                self.cursor_column = 0;
                self.wrap_pending = false;
            }
            '\n' | '\u{b}' | '\u{c}' => self.line_feed(),
            '\u{8}' => {
                self.cursor_column = self.cursor_column.saturating_sub(1);
                self.wrap_pending = false;
            }
            '\t' => {
                self.cursor_column = (((self.cursor_column / 8).saturating_add(1)) * 8)
                    .min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            value if value >= ' ' && value != '\u{7f}' => self.put(value),
            _ => {}
        }
        None
    }

    fn escape(&mut self, character: char) -> Option<Vec<u8>> {
        self.state = ParserState::Ground;
        if character != '[' {
            self.frame_finalization = FrameFinalization::None;
        }
        if matches!(character, '8' | 'D' | 'E' | 'M' | 'c') {
            self.frame_complete = false;
        }
        match character {
            '[' => {
                self.state = ParserState::Csi;
                self.control.clear();
            }
            ']' => self.state = ParserState::Osc,
            '7' => {
                self.saved_cursor = (self.cursor_row, self.cursor_column);
                self.saved_pen = self.pen;
            }
            '8' => {
                (self.cursor_row, self.cursor_column) = self.saved_cursor;
                self.pen = self.saved_pen;
                self.clamp_cursor();
            }
            'D' | 'E' => {
                if character == 'E' {
                    self.cursor_column = 0;
                }
                self.line_feed();
            }
            'M' => self.reverse_index(),
            'c' => self.reset(),
            _ => {}
        }
        None
    }

    fn csi(&mut self, character: char) -> Option<Vec<u8>> {
        if ('@'..='~').contains(&character) {
            let control = std::mem::take(&mut self.control);
            self.state = ParserState::Ground;
            return self.apply_csi(&control, character);
        }
        if self.control.len() >= CONTROL_SEQUENCE_CHARS_MAX {
            self.control.clear();
            self.state = ParserState::Ground;
        } else {
            self.control.push(character);
        }
        None
    }

    #[allow(
        clippy::string_slice,
        clippy::arithmetic_side_effects,
        reason = "the CSI introducer is validated as ASCII and coordinates remain within fixed screen bounds"
    )]
    fn apply_csi(&mut self, control: &str, final_character: char) -> Option<Vec<u8>> {
        let private = control.starts_with(['?', '>', '!']);
        let body = if private { &control[1..] } else { control };
        let parameters = csi_parameters(body);
        let first = parameters.first().copied().unwrap_or(0);
        let amount = first.max(1);
        let completed_frame = matches!(self.frame_finalization, FrameFinalization::CursorShown)
            && final_character == 'H'
            && !private
            && parameters.len() == 2;
        self.frame_finalization = match (control, final_character) {
            ("0", 'm') => FrameFinalization::AttributesReset,
            ("?25", 'h')
                if matches!(self.frame_finalization, FrameFinalization::AttributesReset) =>
            {
                FrameFinalization::CursorShown
            }
            _ => FrameFinalization::None,
        };
        if (control == "0" && final_character == 'm')
            || matches!(
                final_character,
                'H' | 'f'
                    | 'A'
                    | 'B'
                    | 'C'
                    | 'D'
                    | 'E'
                    | 'F'
                    | 'G'
                    | '`'
                    | 'd'
                    | 'J'
                    | 'K'
                    | 'S'
                    | 'T'
                    | 'r'
                    | 'u'
            )
            || (control == "?25" && matches!(final_character, 'h' | 'l'))
        {
            self.frame_complete = false;
        }
        match final_character {
            'H' | 'f' => {
                let row = parameters.first().copied().unwrap_or(1).max(1);
                let column = parameters.get(1).copied().unwrap_or(1).max(1);
                self.cursor_row = row.saturating_sub(1).min(self.rows.saturating_sub(1));
                self.cursor_column = column.saturating_sub(1).min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            'A' => {
                self.cursor_row = self.cursor_row.saturating_sub(amount).max(self.scroll_top);
                self.wrap_pending = false;
            }
            'B' => {
                self.cursor_row = self
                    .cursor_row
                    .saturating_add(amount)
                    .min(self.scroll_bottom);
                self.wrap_pending = false;
            }
            'C' => {
                self.cursor_column = self
                    .cursor_column
                    .saturating_add(amount)
                    .min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            'D' => {
                self.cursor_column = self.cursor_column.saturating_sub(amount);
                self.wrap_pending = false;
            }
            'E' | 'F' => {
                self.cursor_row = if final_character == 'E' {
                    self.cursor_row
                        .saturating_add(amount)
                        .min(self.scroll_bottom)
                } else {
                    self.cursor_row.saturating_sub(amount).max(self.scroll_top)
                };
                self.cursor_column = 0;
                self.wrap_pending = false;
            }
            'G' | '`' => {
                self.cursor_column = amount.saturating_sub(1).min(self.columns.saturating_sub(1));
                self.wrap_pending = false;
            }
            'd' => {
                self.cursor_row = amount.saturating_sub(1).min(self.rows.saturating_sub(1));
                self.wrap_pending = false;
            }
            'm' if !private => self.pen.apply_sgr(control),
            'J' => self.erase_display(first),
            'K' => self.erase_line(first),
            'S' => {
                let rows = self
                    .scroll_bottom
                    .saturating_sub(self.scroll_top)
                    .saturating_add(1);
                for _ in 0..amount.min(rows) {
                    self.scroll_up();
                }
            }
            'T' => {
                let rows = self
                    .scroll_bottom
                    .saturating_sub(self.scroll_top)
                    .saturating_add(1);
                for _ in 0..amount.min(rows) {
                    self.scroll_down();
                }
            }
            'r' if !private => {
                let top = parameters.first().copied().unwrap_or(1).max(1);
                let bottom = parameters.get(1).copied().unwrap_or(self.rows).max(1);
                if top < bottom && bottom <= self.rows {
                    self.scroll_top = top.saturating_sub(1);
                    self.scroll_bottom = bottom.saturating_sub(1);
                    self.cursor_row = 0;
                    self.cursor_column = 0;
                    self.wrap_pending = false;
                }
            }
            'h' | 'l' if control.starts_with('?') && parameters.contains(&25) => {
                self.cursor_visible = final_character == 'h';
            }
            's' => self.saved_cursor = (self.cursor_row, self.cursor_column),
            'u' => {
                (self.cursor_row, self.cursor_column) = self.saved_cursor;
                self.clamp_cursor();
            }
            'n' if !private && first == 6 => {
                return Some(
                    format!("\x1b[{};{}R", self.cursor_row + 1, self.cursor_column + 1)
                        .into_bytes(),
                );
            }
            _ => {}
        }
        if completed_frame {
            self.frame_complete = true;
        }
        None
    }

    #[allow(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "row and column coordinates are clamped to the fixed terminal grid before access"
    )]
    fn put(&mut self, character: char) {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width == 0 {
            // A pending wrap still refers to the rightmost glyph. A wide
            // glyph's empty continuation cell must not receive its marks.
            let mut column = if self.wrap_pending {
                self.cursor_column
            } else {
                self.cursor_column.saturating_sub(1)
            };
            if self.cells[self.cursor_row][column].text.is_empty() {
                column = column.saturating_sub(1);
            }
            let cell = &mut self.cells[self.cursor_row][column];
            if cell.text.len().saturating_add(character.len_utf8()) > CELL_TEXT_BYTES_MAX {
                self.cell_text_overflow = true;
                return;
            }
            cell.text.push(character);
            return;
        }
        if self.wrap_pending {
            self.cursor_column = 0;
            self.line_feed();
            self.wrap_pending = false;
        }
        if width == 2 && self.cursor_column == self.columns.saturating_sub(1) {
            self.cursor_column = 0;
            self.line_feed();
        }
        self.clear_glyph_at(self.cursor_column);
        if width == 2 && self.cursor_column + 1 < self.columns {
            self.clear_glyph_at(self.cursor_column + 1);
        }
        self.cells[self.cursor_row][self.cursor_column] = Cell {
            text: character.to_string(),
            style: self.pen,
        };
        if width == 2 && self.cursor_column + 1 < self.columns {
            self.cells[self.cursor_row][self.cursor_column + 1] = Cell {
                text: String::new(),
                style: self.pen,
            };
        }
        let final_column = self.cursor_column.saturating_add(width);
        if final_column >= self.columns {
            self.cursor_column = self.columns.saturating_sub(1);
            self.wrap_pending = true;
        } else {
            self.cursor_column = final_column;
        }
    }

    /// Overwriting either half of a wide glyph clears the whole glyph so
    /// stale leading or continuation cells cannot distort future coordinates.
    #[allow(
        clippy::indexing_slicing,
        reason = "callers keep the column inside the fixed row"
    )]
    fn clear_glyph_at(&mut self, column: usize) {
        let row = &mut self.cells[self.cursor_row];
        if row[column].text.is_empty() && column > 0 {
            row[column.saturating_sub(1)] = Cell::blank(self.pen);
        } else if row[column]
            .text
            .chars()
            .next()
            .and_then(UnicodeWidthChar::width)
            == Some(2)
            && column.saturating_add(1) < self.columns
        {
            row[column.saturating_add(1)] = Cell::blank(self.pen);
        }
        row[column] = Cell::blank(self.pen);
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up();
        } else {
            self.cursor_row = self
                .cursor_row
                .saturating_add(1)
                .min(self.rows.saturating_sub(1));
        }
    }

    fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.cursor_row == self.scroll_top {
            self.scroll_down();
        } else {
            self.cursor_row = self.cursor_row.saturating_sub(1);
        }
    }

    fn scroll_up(&mut self) {
        self.cells.remove(self.scroll_top);
        self.cells
            .insert(self.scroll_bottom, blank_row(self.columns, self.pen));
    }

    fn scroll_down(&mut self) {
        self.cells.remove(self.scroll_bottom);
        self.cells
            .insert(self.scroll_top, blank_row(self.columns, self.pen));
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "erase ranges are clamped to the fixed terminal grid"
    )]
    fn erase_display(&mut self, mode: usize) {
        match mode {
            2 | 3 => self.cells = blank_cells(self.rows, self.columns, self.pen),
            1 => {
                for row in 0..self.cursor_row {
                    self.cells[row] = blank_row(self.columns, self.pen);
                }
                for column in 0..=self.cursor_column {
                    self.clear_glyph_at(column);
                }
            }
            _ => {
                for column in self.cursor_column..self.columns {
                    self.clear_glyph_at(column);
                }
                for row in self.cursor_row.saturating_add(1)..self.rows {
                    self.cells[row] = blank_row(self.columns, self.pen);
                }
            }
        }
        self.wrap_pending = false;
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "erase ranges are clamped to the fixed terminal row"
    )]
    fn erase_line(&mut self, mode: usize) {
        let (start, end) = match mode {
            1 => (0, self.cursor_column.saturating_add(1)),
            2 => (0, self.columns),
            _ => (self.cursor_column, self.columns),
        };
        for column in start..end {
            self.clear_glyph_at(column);
        }
        self.wrap_pending = false;
    }

    fn reset(&mut self) {
        self.pen = CellStyle::default();
        self.cells = blank_cells(self.rows, self.columns, self.pen);
        self.cursor_row = 0;
        self.cursor_column = 0;
        self.cursor_visible = true;
        self.saved_cursor = (0, 0);
        self.saved_pen = CellStyle::default();
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
        self.wrap_pending = false;
    }

    fn clamp_cursor(&mut self) {
        self.cursor_row = self.cursor_row.min(self.rows.saturating_sub(1));
        self.cursor_column = self.cursor_column.min(self.columns.saturating_sub(1));
        self.wrap_pending = false;
    }
}

impl Rgb {
    fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    /// Fixed xterm palette, rather than an unknown terminal's configured theme.
    fn indexed(index: u8) -> Self {
        const ANSI: [Rgb; 16] = [
            Rgb(0, 0, 0),
            Rgb(205, 0, 0),
            Rgb(0, 205, 0),
            Rgb(205, 205, 0),
            Rgb(0, 0, 238),
            Rgb(205, 0, 205),
            Rgb(0, 205, 205),
            Rgb(229, 229, 229),
            Rgb(127, 127, 127),
            Rgb(255, 0, 0),
            Rgb(0, 255, 0),
            Rgb(255, 255, 0),
            Rgb(92, 92, 255),
            Rgb(255, 0, 255),
            Rgb(0, 255, 255),
            Rgb(255, 255, 255),
        ];
        if let Some(color) = ANSI.get(usize::from(index)) {
            return *color;
        }
        if index >= 232 {
            let grey = index
                .saturating_sub(232)
                .saturating_mul(10)
                .saturating_add(8);
            return Self(grey, grey, grey);
        }
        let cube = index.saturating_sub(16);
        let channel = |value: u8| {
            if value == 0 {
                0
            } else {
                value.saturating_mul(40).saturating_add(55)
            }
        };
        Self(
            channel(cube / 36),
            channel((cube / 6) % 6),
            channel(cube % 6),
        )
    }
}

impl CellStyle {
    fn colors(self) -> (Rgb, Rgb) {
        let foreground = self.foreground.unwrap_or(DEFAULT_FOREGROUND);
        let background = self.background.unwrap_or(DEFAULT_BACKGROUND);
        if self.inverse {
            (background, foreground)
        } else {
            (foreground, background)
        }
    }

    fn apply_sgr(&mut self, control: &str) {
        let mut parts = control.split(';');
        while let Some(part) = parts.next() {
            if part.contains(':') {
                self.apply_colon_sgr(part);
                continue;
            }
            let Ok(code) = (if part.is_empty() {
                Ok(0)
            } else {
                part.parse::<u16>()
            }) else {
                continue;
            };
            if matches!(code, 38 | 48 | 58) {
                // Consume the entire color group even when a component is invalid,
                // so malformed RGB components cannot become unrelated SGR codes.
                let color = match parts.next() {
                    Some("5") => parts.next().and_then(parse_channel).map(Rgb::indexed),
                    Some("2") => {
                        let red = parts.next().and_then(parse_channel);
                        let green = parts.next().and_then(parse_channel);
                        let blue = parts.next().and_then(parse_channel);
                        red.zip(green).zip(blue).map(|((r, g), b)| Rgb(r, g, b))
                    }
                    _ => return,
                };
                if let Some(color) = color {
                    self.set_color(code, color);
                }
            } else {
                self.apply_simple_sgr(code);
            }
        }
    }

    fn apply_colon_sgr(&mut self, part: &str) {
        let fields: Vec<_> = part.split(':').collect();
        match fields.as_slice() {
            ["4", "0"] => self.underline = 0,
            ["4", "1"] => self.underline = 1,
            ["4", "2"] => self.underline = 2,
            [target, "5", index] => {
                if let Some(color) = parse_channel(index).map(Rgb::indexed) {
                    self.set_color(target.parse().unwrap_or(0), color);
                }
            }
            [target, "2", r, g, b] | [target, "2", "" | "0", r, g, b] => {
                if let Some(((r, g), b)) =
                    parse_channel(r).zip(parse_channel(g)).zip(parse_channel(b))
                {
                    self.set_color(target.parse().unwrap_or(0), Rgb(r, g, b));
                }
            }
            _ => {}
        }
    }

    fn set_color(&mut self, code: u16, color: Rgb) {
        match code {
            38 => self.foreground = Some(color),
            48 => self.background = Some(color),
            58 => self.underline_color = Some(color),
            _ => {}
        }
    }

    fn apply_simple_sgr(&mut self, code: u16) {
        match code {
            0 => *self = Self::default(),
            1 => self.bold = true,
            2 => self.dim = true,
            3 => self.italic = true,
            4 => self.underline = 1,
            7 => self.inverse = true,
            8 => self.hidden = true,
            9 => self.strike = true,
            21 => self.underline = 2,
            22 => {
                self.bold = false;
                self.dim = false;
            }
            23 => self.italic = false,
            24 => self.underline = 0,
            27 => self.inverse = false,
            28 => self.hidden = false,
            29 => self.strike = false,
            30..=37 => {
                self.foreground = u8::try_from(code.saturating_sub(30)).ok().map(Rgb::indexed)
            }
            39 => self.foreground = None,
            40..=47 => {
                self.background = u8::try_from(code.saturating_sub(40)).ok().map(Rgb::indexed)
            }
            49 => self.background = None,
            53 => self.overline = true,
            55 => self.overline = false,
            59 => self.underline_color = None,
            90..=97 => {
                self.foreground = u8::try_from(code.saturating_sub(82)).ok().map(Rgb::indexed)
            }
            100..=107 => {
                self.background = u8::try_from(code.saturating_sub(92)).ok().map(Rgb::indexed)
            }
            _ => {}
        }
    }
}

fn parse_channel(value: &str) -> Option<u8> {
    value.parse().ok()
}

/// Incremental SVG text writer with a bound on both retained and scanned bytes.
struct SvgBuffer(String);

impl SvgBuffer {
    fn cell(&mut self, cell: &Cell, row: usize, column: usize) -> io::Result<()> {
        let style = cell.style;
        let decorated = style.underline > 0 || style.strike || style.overline;
        if cell.text.is_empty() || style.hidden || (cell.text == " " && !decorated) {
            return Ok(());
        }
        let x = column.saturating_mul(10).saturating_add(12);
        let y = row.saturating_mul(20).saturating_add(28);
        let columns = cell
            .text
            .chars()
            .next()
            .and_then(UnicodeWidthChar::width)
            .unwrap_or(1)
            .max(1);
        let text_width = columns.saturating_mul(10);
        let (foreground, _) = style.colors();
        self.push(&format!("<g fill=\"{}\"", foreground.hex()))?;
        if style.bold {
            self.push(" font-weight=\"bold\"")?;
        }
        if style.italic {
            self.push(" font-style=\"italic\"")?;
        }
        if style.dim {
            self.push(" opacity=\"0.5\"")?;
        }
        self.push(">")?;
        self.push(&format!("<text x=\"{x}\" y=\"{y}\" textLength=\"{text_width}\" lengthAdjust=\"spacingAndGlyphs\">"))?;
        self.text(&cell.text)?;
        self.push("</text>")?;
        if style.underline > 0 {
            let color = style.underline_color.unwrap_or(foreground);
            self.decoration(x, y.saturating_add(2), text_width, color)?;
            if style.underline == 2 {
                self.decoration(x, y.saturating_add(4), text_width, color)?;
            }
        }
        if style.strike {
            self.decoration(x, y.saturating_sub(5), text_width, foreground)?;
        }
        if style.overline {
            self.decoration(x, y.saturating_sub(14), text_width, foreground)?;
        }
        self.push("</g>\n")
    }

    fn decoration(&mut self, x: usize, y: usize, width: usize, color: Rgb) -> io::Result<()> {
        let end = x.saturating_add(width);
        self.push(&format!(
            "<path d=\"M{x} {y}H{end}\" fill=\"none\" stroke=\"{}\"/>\n",
            color.hex()
        ))
    }

    fn push(&mut self, text: &str) -> io::Result<()> {
        let observed = self.0.len().saturating_add(text.len());
        if observed > SVG_BYTES_MAX {
            return Err(io::Error::other(format!(
                "terminal SVG exceeds byte limit {SVG_BYTES_MAX}, observed {observed}"
            )));
        }
        self.0.try_reserve(text.len()).map_err(|error| {
            io::Error::other(format!("cannot allocate bounded terminal SVG: {error}"))
        })?;
        self.0.push_str(text);
        Ok(())
    }

    fn text(&mut self, text: &str) -> io::Result<()> {
        for character in text.chars() {
            match character {
                '&' => self.push("&amp;")?,
                '<' => self.push("&lt;")?,
                '>' => self.push("&gt;")?,
                '\"' => self.push("&quot;")?,
                '\'' => self.push("&apos;")?,
                // XML 1.0 cannot represent these scalar values, even as
                // character references. Preserve their presence visibly.
                value
                    if (value < ' ' && !matches!(value, '\n' | '\r' | '\t'))
                        || matches!(value, '\u{fffe}' | '\u{ffff}') =>
                {
                    self.push("�")?
                }
                value => self.push(value.encode_utf8(&mut [0; 4]))?,
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct SpawnOptions {
    pub(super) argv: Vec<OsString>,
    pub(super) cwd: PathBuf,
    pub(super) environment: BTreeMap<OsString, OsString>,
    pub(super) rows: usize,
    pub(super) columns: usize,
    pub(super) timeout: Duration,
    pub(super) output_bytes_max: usize,
    pub(super) stderr_path: Option<PathBuf>,
}

impl SpawnOptions {
    pub(super) fn new(argv: Vec<OsString>, cwd: PathBuf) -> Self {
        Self {
            argv,
            cwd,
            environment: BTreeMap::new(),
            rows: 30,
            columns: 120,
            timeout: default_timeout(),
            output_bytes_max: DEFAULT_OUTPUT_BYTES_MAX,
            stderr_path: None,
        }
    }
}

/// PTY child owner with bounded I/O, terminal replies, and process-group cleanup.
pub(super) struct PtySession {
    master: Option<File>,
    child: Option<Pid>,
    timeout: Duration,
    output_bytes_max: usize,
    output: Vec<u8>,
    screen: VirtualScreen,
}

impl PtySession {
    #[allow(
        clippy::indexing_slicing,
        reason = "openpty populates both fixed descriptor slots before they are read"
    )]
    pub(super) fn spawn(options: SpawnOptions) -> Result<Self, TaskError> {
        validate_timeout(options.timeout)?;
        validate_screen_size(options.rows, options.columns)?;
        if options.argv.is_empty() {
            return Err(invalid("PTY argv must not be empty").into());
        }
        if options.output_bytes_max == 0 {
            return Err(invalid("PTY output byte limit must be positive").into());
        }

        let program = cstring(options.argv[0].as_os_str())?;
        let arguments = options
            .argv
            .iter()
            .map(|argument| cstring(argument.as_os_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let mut argument_pointers = arguments
            .iter()
            .map(|argument| argument.as_ptr())
            .collect::<Vec<_>>();
        argument_pointers.push(std::ptr::null());
        let environment = options
            .environment
            .iter()
            .map(|(key, value)| {
                let mut entry = key.as_os_str().as_bytes().to_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_os_str().as_bytes());
                CString::new(entry).map_err(|_| invalid("PTY environment contains a NUL byte"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut environment_pointers = environment
            .iter()
            .map(|entry| entry.as_ptr())
            .collect::<Vec<_>>();
        environment_pointers.push(std::ptr::null());
        let cwd = cstring(options.cwd.as_os_str())?;
        let stderr = options
            .stderr_path
            .as_ref()
            .map(|path| {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)
            })
            .transpose()?;
        let winsize = Winsize {
            ws_row: u16::try_from(options.rows)?,
            ws_col: u16::try_from(options.columns)?,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let screen = VirtualScreen::new(
            options.rows,
            options.columns,
            options.rows.saturating_sub(1),
        )?;
        let output = Vec::with_capacity(options.output_bytes_max.min(64 * 1024));

        // SAFETY: xtask prepares every allocation and pointer vector before forkpty.
        // The child branch calls only async-signal-safe libc operations and then
        // execve or _exit, so it cannot observe or mutate another thread's allocator.
        let fork = unsafe { forkpty(&winsize, None) }?;
        let (master, child) = match fork {
            ForkptyResult::Child => {
                if let Some(stderr) = stderr.as_ref() {
                    // SAFETY: stderr is a live descriptor and fd 2 is the process stderr slot.
                    if unsafe { nix::libc::dup2(stderr.as_raw_fd(), nix::libc::STDERR_FILENO) } < 0
                    {
                        // SAFETY: _exit is required after a post-fork failure.
                        unsafe { nix::libc::_exit(127) };
                    }
                }
                // SAFETY: all pointers reference pre-fork CString storage that remains live
                // through execve; both pointer arrays are explicitly null-terminated.
                unsafe {
                    if nix::libc::chdir(cwd.as_ptr()) == 0 {
                        nix::libc::execve(
                            program.as_ptr(),
                            argument_pointers.as_ptr(),
                            environment_pointers.as_ptr(),
                        );
                    }
                    nix::libc::_exit(127);
                }
            }
            ForkptyResult::Parent { master, child } => (master, child),
        };
        // Own both process and descriptor before another fallible operation so
        // Drop can contain and reap the child after partial parent setup.
        let session = Self {
            master: Some(File::from(master)),
            child: Some(child),
            timeout: options.timeout,
            output_bytes_max: options.output_bytes_max,
            output,
            screen,
        };
        let master = session
            .master
            .as_ref()
            .ok_or_else(|| invalid("new PTY session has no master descriptor"))?;
        let flags = OFlag::from_bits_truncate(fcntl(master, FcntlArg::F_GETFL)?);
        fcntl(master, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(session)
    }

    pub(super) fn child_pid(&self) -> Option<Pid> {
        self.child
    }

    /// Discard retained output after a caller has observed settled readiness.
    /// This invalidates all caller-held output offsets; modeled cells and parser
    /// state remain intact, and subsequent retention uses the same byte limit.
    pub(super) fn clear_output(&mut self) {
        self.output.clear();
    }

    pub(super) fn output(&self) -> &[u8] {
        &self.output
    }

    pub(super) fn screen(&self) -> &VirtualScreen {
        &self.screen
    }

    pub(super) fn foreground_group(&self) -> Result<Pid, TaskError> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("PTY master is closed"))?;
        Ok(tcgetpgrp(master)?)
    }

    pub(super) fn terminal_modes(&self) -> Result<Termios, TaskError> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("PTY master is closed"))?;
        Ok(tcgetattr(master)?)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "bytes written cannot exceed the bounded caller-provided slice length"
    )]
    pub(super) fn send(&mut self, bytes: &[u8]) -> Result<(), TaskError> {
        let deadline = Instant::now() + self.timeout;
        self.send_until(bytes, deadline)
    }

    // A child can repaint while a large paste is still being written. Read
    // output when a write blocks so neither PTY direction waits on the other. Query
    // replies are deferred until the caller's source is complete: inserting a
    // reply into a bracketed paste would corrupt its literal contents. All work
    // shares the original deadline, with at most 1024 / 64 KiB deferred replies.
    #[allow(
        clippy::indexing_slicing,
        reason = "write offsets never exceed their bounded source slices"
    )]
    fn send_until(&mut self, bytes: &[u8], deadline: Instant) -> Result<(), TaskError> {
        let mut offset = 0;
        let mut replies = Vec::new();
        let mut reply_offset = 0;
        let mut reply_count = 0usize;
        while offset < bytes.len() || reply_offset < replies.len() {
            if Instant::now() >= deadline {
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "timed out writing PTY input").into(),
                );
            }
            let writing_source = offset < bytes.len();
            let pending = if writing_source {
                &bytes[offset..]
            } else {
                &replies[reply_offset..]
            };
            let result = self
                .master
                .as_mut()
                .ok_or_else(|| invalid("PTY input is closed"))?
                .write(pending);
            match result {
                Ok(0) => {
                    return Err(
                        io::Error::new(io::ErrorKind::BrokenPipe, "PTY input closed").into(),
                    );
                }
                Ok(written) if writing_source => {
                    offset = offset.saturating_add(written);
                    continue;
                }
                Ok(written) => {
                    reply_offset = reply_offset.saturating_add(written);
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.poll_master(
                        PollFlags::POLLIN | PollFlags::POLLOUT,
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(50)),
                    )?;
                }
                Err(error) => return Err(error.into()),
            }
            let (_, generated) = self.read_output_chunk()?;
            for reply in generated {
                reply_count = reply_count.saturating_add(1);
                let observed = replies.len().saturating_add(reply.len());
                if reply_count > 1024 || observed > 64 * 1024 {
                    return Err(io::Error::other(format!(
                        "PTY deferred replies exceed limit; count={reply_count} limit=1024 bytes={observed} limit=65536"
                    )).into());
                }
                replies.extend_from_slice(&reply);
            }
        }
        Ok(())
    }

    pub(super) fn type_text(&mut self, text: &str) -> Result<(), TaskError> {
        self.send(text.as_bytes())
    }

    pub(super) fn resize(&mut self, rows: usize, columns: usize) -> Result<(), TaskError> {
        validate_screen_size(rows, columns)?;
        self.screen.resize(rows, columns)?;
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("cannot resize a closed PTY"))?;
        let winsize = Winsize {
            ws_row: u16::try_from(rows)?,
            ws_col: u16::try_from(columns)?,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: master is a live PTY descriptor and winsize points to an
        // initialized kernel-compatible value for the duration of ioctl.
        let winsize_pointer = std::ptr::from_ref(&winsize);
        let result =
            unsafe { nix::libc::ioctl(master.as_raw_fd(), nix::libc::TIOCSWINSZ, winsize_pointer) };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    #[allow(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "read lengths are returned by the kernel for the fixed buffer and retained bytes are explicitly bounded"
    )]
    pub(super) fn drain_for(&mut self, duration: Duration) -> Result<Vec<u8>, TaskError> {
        validate_timeout(duration)?;
        let deadline = Instant::now() + duration;
        let output_start = self.output.len();
        while self.master.is_some() && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !self.poll_master(PollFlags::POLLIN, remaining.min(Duration::from_millis(50)))? {
                continue;
            }
            let (_, replies) = self.read_output_chunk()?;
            send_terminal_replies(replies, self.timeout, Instant::now, |reply, deadline| {
                self.send_until(reply, deadline)
            })?;
        }
        // A blocked query reply can itself drain output. Include those bytes
        // in the caller's observed interval as well as the retained transcript.
        Ok(self.output[output_start..].to_vec())
    }

    // One nonblocking read bounds work per send/drain turn. Retention and screen
    // validation are shared so duplex sends cannot bypass either output limit.
    #[allow(
        clippy::indexing_slicing,
        reason = "read lengths are bounded by the fixed read buffer"
    )]
    fn read_output_chunk(&mut self) -> Result<(Vec<u8>, Vec<Vec<u8>>), TaskError> {
        let mut buffer = vec![0_u8; READ_BYTES_MAX];
        let master = self
            .master
            .as_mut()
            .ok_or_else(|| invalid("PTY master is closed"))?;
        let read = match master.read(&mut buffer) {
            Ok(read) => read,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.raw_os_error() == Some(nix::libc::EIO) =>
            {
                return Ok((Vec::new(), Vec::new()));
            }
            Err(error) => return Err(error.into()),
        };
        let observed = self.output.len().saturating_add(read);
        if observed > self.output_bytes_max {
            return Err(io::Error::other(format!(
                "PTY output exceeded its byte limit; observed={observed} limit={}",
                self.output_bytes_max
            ))
            .into());
        }
        buffer.truncate(read);
        self.output.extend_from_slice(&buffer);
        let replies = self.screen.feed(&buffer);
        self.screen.validate_model()?;
        Ok((buffer, replies))
    }

    pub(super) fn wait_for(&mut self, marker: &[u8]) -> Result<Vec<u8>, TaskError> {
        self.wait_for_since(marker, self.output.len(), self.timeout)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the wait deadline"
    )]
    pub(super) fn wait_for_since(
        &mut self,
        marker: &[u8],
        start: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, TaskError> {
        validate_timeout(timeout)?;
        let deadline = Instant::now() + timeout;
        while !contains_bytes(self.output.get(start..).unwrap_or_default(), marker)
            && Instant::now() < deadline
        {
            self.drain_for(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(WAIT_POLL_QUANTUM)
                    .max(Duration::from_millis(1)),
            )?;
        }
        let observed = self.output.get(start..).unwrap_or_default().to_vec();
        if !contains_bytes(&observed, marker) {
            return Err(self.timeout_error(&format!("output marker {marker:?}"), &observed));
        }
        Ok(observed)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the wait deadline"
    )]
    pub(super) fn wait_for_screen(
        &mut self,
        description: &str,
        predicate: impl Fn(&VirtualScreen) -> bool,
    ) -> Result<String, TaskError> {
        let deadline = Instant::now() + self.timeout;
        while !predicate(&self.screen) && Instant::now() < deadline {
            self.drain_for(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(WAIT_POLL_QUANTUM)
                    .max(Duration::from_millis(1)),
            )?;
        }
        let snapshot = self.screen.text();
        if !predicate(&self.screen) {
            return Err(self.timeout_error(&format!("screen state {description}"), &[]));
        }
        Ok(snapshot)
    }

    pub(super) fn wait_for_screen_text(&mut self, marker: &str) -> Result<String, TaskError> {
        self.wait_for_screen(&format!("{marker:?}"), |screen| {
            screen.text().contains(marker)
        })
    }

    pub(super) fn wait_exit(&mut self) -> Result<i32, TaskError> {
        self.wait_exit_within(self.timeout)
    }

    /// Wait for child exit within an explicit validated deadline.
    ///
    /// Protocol-deadline tests may need a longer wait than ordinary key checks.
    /// This observation never resends input or extends the caller's deadline.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the process deadline"
    )]
    pub(super) fn wait_exit_within(&mut self, timeout: Duration) -> Result<i32, TaskError> {
        validate_timeout(timeout)?;
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.drain_for(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50))
                    .max(Duration::from_millis(1)),
            )?;
            if let Some(status) = self.try_reap()? {
                return Ok(wait_status_code(status));
            }
        }
        Err(self.timeout_error("child exit", &[]))
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the close deadline"
    )]
    pub(super) fn close(&mut self) -> Result<(), TaskError> {
        if let Some(master) = self.master.as_ref()
            && let Ok(group) = tcgetpgrp(master)
            && group.as_raw() > 0
            && group != getpgrp()
        {
            kill_group(group);
        }
        if let Some(child) = self.child {
            kill_group(child);
            let _ = kill(child, Signal::SIGKILL);
        }
        self.master.take();
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        while self.child.is_some() && Instant::now() < deadline {
            if self.try_reap()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(child) = self.child {
            return Err(io::Error::other(format!(
                "PTY child {} could not be reaped after SIGKILL",
                child.as_raw()
            ))
            .into());
        }
        Ok(())
    }

    fn poll_master(&self, flags: PollFlags, timeout: Duration) -> Result<bool, TaskError> {
        let master = self
            .master
            .as_ref()
            .ok_or_else(|| invalid("PTY master is closed"))?;
        let mut descriptors = [PollFd::new(master.as_fd(), flags)];
        let timeout = PollTimeout::try_from(timeout)?;
        Ok(poll(&mut descriptors, timeout)? > 0)
    }

    fn try_reap(&mut self) -> Result<Option<WaitStatus>, TaskError> {
        let Some(child) = self.child else {
            return Ok(None);
        };
        match waitpid(child, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => Ok(None),
            Ok(status) => {
                self.child = None;
                Ok(Some(status))
            }
            Err(Errno::ECHILD) => {
                self.child = None;
                Ok(Some(WaitStatus::Exited(child, 0)))
            }
            Err(error) => Err(error.into()),
        }
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "diagnostic output is sliced at a byte count clamped to the observed buffer length"
    )]
    fn timeout_error(&self, expected: &str, observed: &[u8]) -> TaskError {
        let source = if observed.is_empty() {
            &self.output
        } else {
            observed
        };
        let tail = &source[source.len().saturating_sub(1_000)..];
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "timed out waiting for {expected}; raw_tail={tail:?}; screen=\n{}; child_state=\n{}; process_tree=\n{}",
                self.screen.text(),
                self.child.map_or_else(
                    || "(no tracked child)".to_owned(),
                    |pid| child_kernel_state(pid.as_raw()),
                ),
                process_tree_snapshot()
            ),
        )
        .into()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

#[cfg(target_os = "linux")]
const CHILD_THREADS_MAX: usize = 64;

/// Best-effort per-thread kernel sleep state for the harness's own tracked
/// child, read straight from Linux's `/proc` (a no-op elsewhere: macOS has
/// no equivalent, and this is diagnostic only).
///
/// `wchan` names the kernel function a thread is blocked in, which turns "a
/// wait timed out" into either "genuinely still computing" (short or empty
/// wchan, high recent CPU) or "asleep waiting on something that never
/// arrived" (a wchan like `futex_wait_queue` or `pipe_read`) without needing
/// a live debugger attached to the CI runner.
#[cfg(target_os = "linux")]
fn child_kernel_state(pid: i32) -> String {
    let task_directory = PathBuf::from(format!("/proc/{pid}/task"));
    let Ok(entries) = std::fs::read_dir(&task_directory) else {
        return format!("(could not read {})", task_directory.display());
    };
    let mut lines = Vec::new();
    for entry in entries.filter_map(Result::ok).take(CHILD_THREADS_MAX) {
        let tid = entry.file_name();
        let wchan = std::fs::read_to_string(entry.path().join("wchan"))
            .unwrap_or_else(|error| format!("(unreadable: {error})"));
        let comm = std::fs::read_to_string(entry.path().join("comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let state = std::fs::read_to_string(entry.path().join("stat"))
            .ok()
            .and_then(|stat| {
                stat.split(')')
                    .nth(1)?
                    .split_whitespace()
                    .next()
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "?".to_owned());
        lines.push(format!(
            "tid={} comm={comm:?} state={state} wchan={wchan}",
            tid.to_string_lossy()
        ));
    }
    if lines.is_empty() {
        return format!("(no threads found under {})", task_directory.display());
    }
    lines.join("\n")
}

#[cfg(not(target_os = "linux"))]
fn child_kernel_state(_pid: i32) -> String {
    "(child kernel state is only available on Linux)".to_owned()
}

const PROCESS_TREE_LINES_MAX: usize = 200;

/// Best-effort `ps -ef` snapshot attached to a timeout diagnostic, filtered
/// to userspace processes.
///
/// A PTY wait timing out this test harness could mean the harness itself is
/// just slow under CI load, or that a child (or something it spawned) is
/// genuinely still alive and blocking cleanup; this distinguishes the two
/// without needing to reproduce the hang interactively. On a real VM-backed
/// CI runner (unlike a container) `ps -ef` lists the whole host, hundreds of
/// bracketed kernel threads (`[kworker/...]`) and all; drop those so the
/// bound below is spent on the processes that could actually matter here.
/// Never fails the check itself: a `ps` failure just yields a short
/// diagnostic string.
fn process_tree_snapshot() -> String {
    match std::process::Command::new("ps").arg("-ef").output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let mut lines = text.lines();
            let header = lines.next().unwrap_or_default();
            let mut kept = vec![header.to_owned()];
            kept.extend(
                lines
                    .filter(|line| {
                        line.split_whitespace()
                            .last()
                            .is_none_or(|command| !command.starts_with('['))
                    })
                    .take(PROCESS_TREE_LINES_MAX)
                    .map(str::to_owned),
            );
            kept.join("\n")
        }
        Err(error) => format!("(ps -ef unavailable: {error})"),
    }
}

fn blank_cells(rows: usize, columns: usize, style: CellStyle) -> Vec<Vec<Cell>> {
    (0..rows).map(|_| blank_row(columns, style)).collect()
}

fn blank_row(columns: usize, style: CellStyle) -> Vec<Cell> {
    vec![Cell::blank(style); columns]
}

fn validate_timeout(timeout: Duration) -> io::Result<()> {
    if timeout.is_zero() || timeout > Duration::from_secs(150) {
        return Err(invalid(
            "PTY timeout must be greater than zero and at most 150 seconds",
        ));
    }
    Ok(())
}

fn validate_screen_size(rows: usize, columns: usize) -> io::Result<()> {
    if rows == 0
        || columns == 0
        || rows
            .checked_mul(columns)
            .is_none_or(|cells| cells > SCREEN_CELLS_MAX)
    {
        return Err(invalid(format!(
            "PTY screen dimensions must be positive and retain at most {SCREEN_CELLS_MAX} cells"
        )));
    }
    Ok(())
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| invalid("PTY value contains a NUL byte"))
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn csi_parameters(control: &str) -> Vec<usize> {
    let parameters = control
        .split_once(' ')
        .map_or(control, |(values, _)| values);
    if parameters.is_empty() {
        return Vec::new();
    }
    parameters
        .split(';')
        .map(|value| value.parse().unwrap_or(0))
        .collect()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "reply byte counts are bounded by the captured PTY byte limit"
)]
fn send_terminal_replies<E>(
    replies: impl IntoIterator<Item = Vec<u8>>,
    send_timeout: Duration,
    mut now: impl FnMut() -> Instant,
    mut send_until: impl FnMut(&[u8], Instant) -> Result<(), E>,
) -> Result<(), E> {
    for reply in replies {
        let deadline = now() + send_timeout;
        send_until(&reply, deadline)?;
    }
    Ok(())
}

#[allow(
    clippy::as_conversions,
    reason = "nix Signal is a repr(i32) wrapper over the platform signal number"
)]
fn wait_status_code(status: WaitStatus) -> i32 {
    match status {
        WaitStatus::Exited(_, code) => code,
        WaitStatus::Signaled(_, signal, _) => 128_i32.saturating_add(signal as i32),
        _ => 1,
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "negating a valid positive child process group identifier is required by POSIX kill"
)]
fn kill_group(group: Pid) {
    let raw = group.as_raw();
    if raw > 0 {
        let _ = kill(Pid::from_raw(-raw), Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn test_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "quirl-{label}-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn default_timeout_is_more_generous_under_github_actions() {
        let expected = if std::env::var_os("GITHUB_ACTIONS").is_some() {
            Duration::from_secs(100)
        } else {
            Duration::from_secs(15)
        };
        assert_eq!(default_timeout(), expected);
    }

    #[test]
    fn svg_escapes_xml_and_positions_unicode_wide_cells_and_the_cursor() {
        let mut screen = VirtualScreen::new(2, 12, 0).unwrap();
        screen.feed("<&>\"'界e\u{301}".as_bytes());
        let svg = screen.to_svg().unwrap();
        assert!(svg.contains("width=\"144\" height=\"64\""));
        assert!(svg.contains("Styled VT cell model"));
        for escaped in ["&lt;", "&amp;", "&gt;", "&quot;", "&apos;"] {
            assert!(svg.contains(escaped));
        }
        assert!(svg.contains(
            "x=\"62\" y=\"28\" textLength=\"20\" lengthAdjust=\"spacingAndGlyphs\">界</text>"
        ));
        assert!(svg.contains(
            "x=\"82\" y=\"28\" textLength=\"10\" lengthAdjust=\"spacingAndGlyphs\">e\u{301}</text>"
        ));
        assert!(!svg.contains("<text x=\"72\""));
        assert!(svg.contains("id=\"cursor\" x=\"92\" y=\"12\""));
        screen.feed(b"\x1b[?25l");
        assert!(!screen.to_svg().unwrap().contains("id=\"cursor\""));
        screen.feed(b"\x1b[?25h");
        assert!(screen.to_svg().unwrap().contains("id=\"cursor\""));
        screen.resize(3, 20).unwrap();
        assert!(
            screen
                .to_svg()
                .unwrap()
                .contains("width=\"224\" height=\"84\"")
        );
    }

    #[test]
    fn sgr_colors_and_styles_survive_every_split_boundary_then_reset() {
        let bytes = b"\x1b[1;2;3;4;9;53;38;2;10;20;30;48;5;196mX\x1b[0mY";
        for split in 0..=bytes.len() {
            let mut screen = VirtualScreen::new(2, 20, 0).unwrap();
            screen.feed(&bytes[..split]);
            screen.feed(&bytes[split..]);
            let style = screen.cells[0][0].style;
            assert_eq!(style.foreground, Some(Rgb(10, 20, 30)));
            assert_eq!(style.background, Some(Rgb(255, 0, 0)));
            assert!(style.bold);
            assert!(style.dim);
            assert!(style.italic);
            assert_eq!(style.underline, 1);
            assert!(style.strike);
            assert!(style.overline);
            assert_eq!(screen.cells[0][1].style, CellStyle::default());
            let svg = screen.to_svg().unwrap();
            assert!(svg.contains(
                "fill=\"#0a141e\" font-weight=\"bold\" font-style=\"italic\" opacity=\"0.5\""
            ));
            assert!(svg.contains("width=\"10\" height=\"20\" fill=\"#ff0000\""));
            assert!(svg.contains("M12 30H22"));
            assert!(svg.contains("M12 23H22"));
            assert!(svg.contains("M12 14H22"));
        }
    }

    #[test]
    fn inverse_conceal_and_individual_resets_preserve_text_oracles() {
        let mut screen = VirtualScreen::new(2, 20, 0).unwrap();
        screen.feed(b"\x1b[31;44;7mX\x1b[8mY\x1b[27;28;39;49mZ");
        assert_eq!(screen.lines()[0], "XYZ");
        assert_eq!(
            screen.cells[0][0].style.colors(),
            (Rgb(0, 0, 238), Rgb(205, 0, 0))
        );
        assert_eq!(screen.cells[0][2].style, CellStyle::default());
        let svg = screen.to_svg().unwrap();
        assert!(svg.contains("fill=\"#0000ee\""));
        assert!(svg.contains(">X</text>"));
        assert!(!svg.contains(">Y</text>"));
        assert!(svg.contains(">Z</text>"));
        screen.feed(b"\x1b[1;2;3;4;9;53mA\x1b[22;23;24;29;55mB");
        assert_eq!(screen.cells[0][4].style, CellStyle::default());
    }

    #[test]
    fn indexed_and_colon_colors_are_exact_and_invalid_components_are_not_attributes() {
        assert_eq!(Rgb::indexed(16), Rgb(0, 0, 0));
        assert_eq!(Rgb::indexed(21), Rgb(0, 0, 255));
        assert_eq!(Rgb::indexed(231), Rgb(255, 255, 255));
        assert_eq!(Rgb::indexed(232), Rgb(8, 8, 8));
        assert_eq!(Rgb::indexed(255), Rgb(238, 238, 238));
        let mut screen = VirtualScreen::new(2, 20, 0).unwrap();
        screen.feed(b"\x1b[91;104mA\x1b[38:2::1:2:3;48:5:245;58:2:4:5:6;4:2mB");
        assert_eq!(screen.cells[0][0].style.foreground, Some(Rgb(255, 0, 0)));
        assert_eq!(screen.cells[0][0].style.background, Some(Rgb(92, 92, 255)));
        let expected = screen.pen;
        assert_eq!(expected.foreground, Some(Rgb(1, 2, 3)));
        assert_eq!(expected.background, Some(Rgb(138, 138, 138)));
        assert_eq!(expected.underline_color, Some(Rgb(4, 5, 6)));
        screen.feed(b"\x1b[38;2;999;0;1mC\x1b[48:2::0:999:0mD\x1b[38;5;256mE");
        for index in 2..5 {
            assert_eq!(screen.cells[0][index].style, expected);
        }
        let svg = screen.to_svg().unwrap();
        assert!(svg.contains("M22 30H32\" fill=\"none\" stroke=\"#040506\""));
        assert!(svg.contains("M22 32H32\" fill=\"none\" stroke=\"#040506\""));
        screen.feed(b"\x1b[59;4:0mF");
        assert_eq!(screen.pen.underline_color, None);
        assert_eq!(screen.pen.underline, 0);
    }

    #[test]
    fn erase_scroll_resize_and_terminal_reset_keep_styles_with_cells() {
        let mut screen = VirtualScreen::new(2, 4, 0).unwrap();
        screen.feed(b"\x1b[41mA\x1b[2K");
        assert!(
            screen.cells[0]
                .iter()
                .all(|cell| cell.style.background == Some(Rgb(205, 0, 0)))
        );
        screen.feed(b"\x1b[0m\x1b[2;1H\x1b[32mB\n");
        assert_eq!(screen.cells[0][0].text, "B");
        assert_eq!(screen.cells[0][0].style.foreground, Some(Rgb(0, 205, 0)));
        screen.resize(3, 6).unwrap();
        assert_eq!(screen.cells[0][0].style.foreground, Some(Rgb(0, 205, 0)));
        assert_eq!(screen.cells[0][5].style, CellStyle::default());
        screen.feed(b"\x1bc");
        assert_eq!(screen.pen, CellStyle::default());
        assert!(
            screen
                .cells
                .iter()
                .flatten()
                .all(|cell| cell.style == CellStyle::default())
        );
    }

    #[test]
    fn wide_glyph_backgrounds_are_painted_before_text_and_overwrites_reset_style() {
        let mut screen = VirtualScreen::new(2, 8, 0).unwrap();
        screen.feed("\x1b[48;2;1;2;3m界\u{301}".as_bytes());
        assert_eq!(screen.cells[0][0].style, screen.cells[0][1].style);
        let svg = screen.to_svg().unwrap();
        let continuation_background = svg.find("x=\"22\" y=\"12\" width=\"10\"").unwrap();
        let glyph = svg.find(">界\u{301}</text>").unwrap();
        assert!(continuation_background < glyph);
        screen.feed(b"\x1b[0m\x1b[1;2HX");
        assert_eq!(screen.cells[0][0].text, " ");
        assert_eq!(screen.cells[0][0].style, CellStyle::default());
        assert_eq!(screen.cells[0][1].style, CellStyle::default());
    }

    #[test]
    fn dec_saved_cursor_restores_its_rendition() {
        let mut screen = VirtualScreen::new(2, 8, 0).unwrap();
        screen.feed(b"\x1b[31m\x1b7\x1b[0mwrong\x1b8X");
        assert_eq!(screen.cells[0][0].text, "X");
        assert_eq!(screen.cells[0][0].style.foreground, Some(Rgb(205, 0, 0)));
        assert_eq!(screen.cells[0][1].style, CellStyle::default());
    }

    #[test]
    fn excessive_combining_marks_invalidate_capture_without_growing_the_cell() {
        let mut screen = VirtualScreen::new(2, 4, 0).unwrap();
        screen.feed(b"a");
        for _ in 0..127 {
            screen.feed("\u{301}".as_bytes());
        }
        assert_eq!(screen.cells[0][0].text.len(), 255);
        assert!(screen.validate_model().is_ok());
        screen.feed("\u{301}".as_bytes());
        assert_eq!(screen.cells[0][0].text.len(), 255);
        assert!(
            screen
                .validate_model()
                .unwrap_err()
                .to_string()
                .contains("256")
        );
        assert!(screen.to_svg().is_err());
        screen.feed(b"\x1bc\x1b[0m\x1b[?25h\x1b[1;1H");
        assert!(!screen.has_completed_frame());
        assert!(screen.to_svg().is_err());
    }

    #[test]
    fn frame_completion_waits_for_the_final_cursor_sequence_across_reads() {
        let mut screen = VirtualScreen::new(4, 80, 0).unwrap();
        screen.feed(b"Sources are visible while the border is still being drawn");
        assert!(!screen.has_completed_frame());
        screen.feed(b"\x1b[39m\x1b[49m\x1b[59m\x1b[0m\x1b[?25");
        assert!(!screen.has_completed_frame());
        screen.feed(b"h\x1b[4;3");
        assert!(!screen.has_completed_frame());
        screen.feed(b"H");
        assert!(screen.has_completed_frame());
        // Readiness controls do not alter the completed visible frame.
        screen.feed(b"\x1b[?1000h");
        assert!(screen.has_completed_frame());
        screen.feed(b"\x1b[0m");
        assert!(!screen.has_completed_frame());
        screen.feed(b"\x1b[?25h\x1b[4;3H");
        assert!(screen.has_completed_frame());
        // A subsequent partial repaint must not reuse the previous frame end.
        screen.feed(b"\x1b[2;1Hpartial next frame");
        assert!(!screen.has_completed_frame());
        screen.feed(b"\x1b[0m\x1b[?25h\x1b[4;3H");
        assert!(screen.has_completed_frame());
    }

    #[test]
    fn input_readiness_during_handoff_does_not_complete_the_new_frame() {
        let mut screen = VirtualScreen::new(4, 80, 0).unwrap();
        screen.feed(b"HANDOFF_DONE\x1b[4;1HNORMAL\x1b[0m\x1b[?25h\x1b[3;3H");
        assert!(screen.has_completed_frame());
        // resume_input clears the completed execution view before announcing
        // mouse readiness. The next read_line draw can span many PTY reads.
        screen.feed(b"\x1b[1;1H\x1b[J\x1b[?1000h");
        assert!(!screen.has_completed_frame());
        assert!(screen.bottom_line().is_empty());
        screen.feed(b"partial JSON row");
        assert!(!screen.has_completed_frame());
        screen.feed(b"\x1b[4;1HNORMAL");
        assert!(!screen.has_completed_frame());
        screen.feed(b"\x1b[0m\x1b[?25h\x1b[3;");
        assert!(!screen.has_completed_frame());
        screen.feed(b"3H");
        assert!(screen.has_completed_frame());
        assert_eq!(screen.bottom_line(), "NORMAL");
    }

    #[test]
    fn unchanged_resize_preserves_frame_and_terminal_state() {
        let mut screen = VirtualScreen::new(4, 8, 0).unwrap();
        screen.feed(b"\x1b[2;4rabcdefgh\x1b[0m\x1b[?25h\x1b[2;8H");
        assert!(screen.has_completed_frame());
        let before = format!("{screen:?}");
        screen.resize(4, 8).unwrap();
        assert_eq!(format!("{screen:?}"), before);
        assert!(screen.has_completed_frame());
        // Pending wrap and an incomplete escape also survive a no-op resize.
        screen.feed(b"x\x1b[");
        assert!(screen.wrap_pending);
        let before = format!("{screen:?}");
        screen.resize(4, 8).unwrap();
        assert_eq!(format!("{screen:?}"), before);
        screen.feed(b"0m\x1b[?25h\x1b[2;8H");
        assert!(screen.has_completed_frame());
        screen.resize(5, 8).unwrap();
        assert!(!screen.has_completed_frame());
        assert_eq!(screen.rows, 5);
    }

    #[test]
    fn ordinary_cursor_moves_and_interrupted_finalization_are_not_completed_frames() {
        let mut screen = VirtualScreen::new(4, 80, 0).unwrap();
        screen.feed(b"\x1b[?25h\x1b[4;3H");
        assert!(!screen.has_completed_frame());
        screen.feed(b"\x1b[0m\x1b[?25hstill drawing\x1b[4;3H");
        assert!(!screen.has_completed_frame());
        screen.feed(b"\x1b[0m\x1b[?25h\x1b[4;3H");
        assert!(screen.has_completed_frame());
        screen.resize(6, 100).unwrap();
        assert!(!screen.has_completed_frame());
    }

    #[test]
    fn svg_replaces_invalid_xml_scalars_and_rejects_the_first_excess_byte() {
        let mut svg = SvgBuffer(String::new());
        svg.text("a\u{1}\u{fffe}\u{ffff}b").unwrap();
        assert_eq!(svg.0, "a���b");
        svg.0 = "x".repeat(SVG_BYTES_MAX.saturating_sub(5));
        svg.text("&").unwrap();
        assert_eq!(svg.0.len(), SVG_BYTES_MAX);
        let error = svg.text("<").unwrap_err();
        assert!(error.to_string().contains("byte limit"));
        assert_eq!(svg.0.len(), SVG_BYTES_MAX);
    }

    #[test]
    fn combining_marks_follow_the_glyph_at_wrap_and_wide_cell_boundaries() {
        let mut screen = VirtualScreen::new(2, 4, 0).unwrap();
        screen.feed("abcd\u{301}".as_bytes());
        assert_eq!(screen.cells[0][3].text, "d\u{301}");
        assert_eq!(screen.cells[0][2].text, "c");
        screen.feed("e界\u{301}".as_bytes());
        assert_eq!(screen.cells[1][1].text, "界\u{301}");
        assert_eq!(screen.cells[1][2].text, "");
    }

    #[test]
    fn overwriting_or_erasing_half_a_wide_glyph_clears_its_other_half() {
        let mut screen = VirtualScreen::new(2, 6, 0).unwrap();
        screen.feed("界x\r\x1b[2GA".as_bytes());
        assert_eq!(screen.lines()[0], " Ax");
        screen.feed("\r界x\rA".as_bytes());
        assert_eq!(screen.lines()[0], "A x");
        screen.feed("\r界x\r\x1b[2G\x1b[K".as_bytes());
        assert_eq!(screen.lines()[0], "");
    }

    #[test]
    fn narrowing_the_screen_does_not_retain_half_a_wide_glyph() {
        let mut screen = VirtualScreen::new(2, 6, 0).unwrap();
        screen.feed("abc界".as_bytes());
        screen.resize(2, 4).unwrap();
        assert_eq!(screen.lines()[0], "abc");
        assert!(!screen.to_svg().unwrap().contains("界"));
        screen.resize(2, 6).unwrap();
        assert_eq!(screen.lines()[0], "abc");
    }

    #[test]
    fn cursor_query_reports_modeled_bottom_row() {
        let mut screen = VirtualScreen::new(6, 20, 5).unwrap();
        assert_eq!(screen.feed(b"\x1b[6n"), [b"\x1b[6;1R".to_vec()]);
    }

    #[test]
    fn chunked_utf8_and_cursor_sequences_update_visible_cells() {
        let mut screen = VirtualScreen::new(4, 12, 0).unwrap();
        screen.feed(b"old\x1b[2");
        screen.feed(b"J\x1b[2;3Hdata \xe2");
        screen.feed(b"\x97\x86");
        assert_eq!(screen.lines()[0], "");
        assert_eq!(screen.lines()[1], "  data ◆");
    }

    #[test]
    fn clear_to_end_removes_expanded_picker_rows() {
        let mut screen = VirtualScreen::new(6, 20, 0).unwrap();
        screen.feed(b"\x1b[1;1Hpicker\x1b[6;1Hdata | results");
        screen.feed(b"\x1b[1;1H\x1b[Jcommand\x1b[6;1Hcommand | ready");
        assert!(!screen.text().contains("picker"));
        assert_eq!(screen.bottom_line(), "command | ready");
    }

    #[test]
    fn huge_scroll_count_is_bounded_by_the_screen_height() {
        let mut screen = VirtualScreen::new(4, 20, 0).unwrap();
        screen.feed(b"marker\x1b[999999999S");
        assert!(screen.lines().iter().all(String::is_empty));
    }

    #[test]
    fn close_kills_and_reaps_the_session_leader() {
        let directory = test_directory("pty-close");
        let mut options = SpawnOptions::new(
            ["/bin/sh", "-c", "printf READY; sleep 30"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options
            .environment
            .insert("TERM".into(), "xterm-256color".into());
        options.rows = 8;
        options.columns = 40;
        options.timeout = Duration::from_secs(2);
        options.output_bytes_max = 4_096;
        let mut session = PtySession::spawn(options).unwrap();
        let child = session.child_pid().unwrap();
        session.wait_for(b"READY").unwrap();
        session.close().unwrap();
        assert!(matches!(
            waitpid(child, Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cell_limit_failure_keeps_the_session_owned_and_reaps_its_child() {
        let directory = test_directory("pty-cell-limit");
        let command = format!("printf 'a{}'; sleep 30", "\u{301}".repeat(128));
        let mut options = SpawnOptions::new(
            vec!["/bin/sh".into(), "-c".into(), command.into()],
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options.output_bytes_max = 4096;
        options.timeout = Duration::from_secs(2);
        let mut session = PtySession::spawn(options).unwrap();
        let child = session.child_pid().unwrap();
        let error = session.drain_for(Duration::from_secs(1)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cell text exceeds byte limit 256")
        );
        assert!(session.screen().to_svg().is_err());
        session.close().unwrap();
        assert!(matches!(
            waitpid(child, Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn output_limit_fails_before_unbounded_retention() {
        let directory = test_directory("pty-limit");
        let mut options = SpawnOptions::new(
            ["/bin/sh", "-c", "while :; do printf 1234567890; done"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options
            .environment
            .insert("TERM".into(), "xterm-256color".into());
        options.rows = 8;
        options.columns = 40;
        options.timeout = Duration::from_secs(2);
        options.output_bytes_max = 1_024;
        let mut session = PtySession::spawn(options).unwrap();
        let error = session.drain_for(Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("output exceeded its byte limit"));
        assert!(session.output().len() <= 1_024);
        session.close().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn large_writes_drain_echo_output_without_changing_source_or_deadline() {
        let directory = test_directory("pty-duplex");
        let mut options = SpawnOptions::new(
            ["/bin/sh", "-c", "stty raw -echo; printf READY; exec cat"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options.timeout = Duration::from_secs(5);
        options.output_bytes_max = 512 * 1024;
        let mut session = PtySession::spawn(options).unwrap();
        let child = session.child_pid().unwrap();
        session.wait_for(b"READY").unwrap();
        let start = session.output().len();
        // This exceeds both PTY queues: a write-only parent and echoing child
        // deadlock before either can consume the other's remaining bytes.
        let payload = b"0123456789abcdef".repeat(16 * 1024);
        session.send(&payload).unwrap();
        session
            .wait_for_since(&payload, start, Duration::from_secs(5))
            .unwrap();
        assert_eq!(&session.output()[start..], payload);
        session.close().unwrap();
        assert!(matches!(
            waitpid(child, Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cursor_reply_discovered_at_drain_deadline_gets_an_independent_send_deadline() {
        let mut screen = VirtualScreen::new(8, 40, 7).unwrap();
        let replies = screen.feed(b"\x1b[6n");
        let drain_deadline = Instant::now();
        let send_timeout = Duration::from_millis(250);
        let mut observed = Vec::new();

        send_terminal_replies(
            replies,
            send_timeout,
            || drain_deadline,
            |reply, deadline| {
                observed.push((reply.to_vec(), deadline));
                Ok::<(), io::Error>(())
            },
        )
        .unwrap();

        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].0, b"\x1b[8;1R");
        assert_eq!(observed[0].1.duration_since(drain_deadline), send_timeout);
    }

    #[test]
    fn blocked_terminal_reply_times_out_at_the_configured_send_bound() {
        let directory = test_directory("pty-reply-deadline");
        let send_timeout = Duration::from_millis(100);
        let mut options = SpawnOptions::new(
            ["/bin/sh", "-c", "stty raw -echo; printf READY; sleep 30"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            directory.clone(),
        );
        options
            .environment
            .insert("PATH".into(), "/usr/bin:/bin".into());
        options.timeout = send_timeout;
        let mut session = PtySession::spawn(options).unwrap();
        session.wait_for(b"READY").unwrap();
        let input = [b'x'; 4_096];
        let mut written = 0_usize;
        loop {
            assert!(
                written < 16 * 1024 * 1024,
                "PTY input did not become blocked"
            );
            let master = session.master.as_mut().unwrap();
            match master.write(&input) {
                Ok(count) => written = written.saturating_add(count),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("failed while filling PTY input: {error}"),
            }
        }

        // A `WouldBlock` on a 4 KiB write does not guarantee a much smaller
        // write stays blocked an instant later: on Linux the tty output
        // queue's low-water mark can free enough room, between the fill
        // loop above and the timed `send` below, for a 5-byte payload to
        // fit even though the queue is still effectively full. Measure the
        // timeout with a payload the same size as what actually established
        // `WouldBlock`, so the same fullness that blocked the fill loop
        // still blocks this write.
        let started = Instant::now();
        let error = session.send(&input).unwrap_err();
        let elapsed = started.elapsed();
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::TimedOut)
        );
        assert!(elapsed >= send_timeout);
        assert!(elapsed < send_timeout + Duration::from_millis(250));
        session.close().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
