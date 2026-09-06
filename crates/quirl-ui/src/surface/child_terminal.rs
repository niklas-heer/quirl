//! Bounded child-terminal presentation, isolated from the physical terminal.
//!
//! Failure model: child bytes may split UTF-8 or controls, request clipboard or
//! device actions, amplify one CSI into many allocations, flood replies, or
//! restore an obsolete cursor after resize. Admission owns those boundaries:
//! OSC/DCS never enter the dependency or host terminal, controls retain at most
//! 256 bytes, strings scan at most 64 KiB, and each call admits at most 64 KiB.
//! Each turn additionally admits at most 1,048,576 cell/edit work units.
//! Errors are sticky. The process owner must stop/reap the child on error.
//!
//! vt100 0.15.2 stores six code points per cell. Two 512×256 grids plus 256
//! scrollback rows retain at most 393,216 fixed-size cells. Additional combining
//! marks are omitted with an observable notice. Drawing visits only the clipped
//! viewport. Snapshots retain at most 512 rows plus one notice / 4 MiB. This
//! module owns no child process, host clipboard, title, terminal mode or files.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use quirl_core::{ErrorCode, ShellError, escape_terminal_line};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
};
use unicode_width::UnicodeWidthChar;

/// Maximum child output admitted in one event-loop turn, in bytes.
pub const CHILD_TERMINAL_OUTPUT_BYTES_MAX: usize = 64 * 1024;
/// Maximum encoded child input, including paste delimiters, in one event.
pub const CHILD_TERMINAL_INPUT_BYTES_MAX: usize = 64 * 1024 + 12;
/// Maximum unread child input admitted for recovery as editable text, in bytes.
pub const CHILD_TERMINAL_RECOVERY_BYTES_MAX: usize = 64 * 1024;
const CELL_WORK_PER_TURN_MAX: usize = 1024 * 1024;
const CONTROL_BYTES_MAX: usize = 256;
const STRING_BYTES_MAX: usize = 64 * 1024;
const REPLY_BYTES_MAX: usize = 8 * 1024;
const SCROLLBACK_ROWS_MAX: usize = 256;
const SNAPSHOT_BYTES_MAX: usize = 4 * 1024 * 1024;
const DEFAULT_FOREGROUND: Color = Color::Rgb(216, 222, 233);
const DEFAULT_BACKGROUND: Color = Color::Rgb(16, 20, 28);
const SCROLLBACK_NOTICE: &str =
    "Terminal retained the latest 256 scrollback rows; older rows were omitted.";
const COMBINED_NOTICE: &str = "Terminal omitted older scrollback beyond 256 rows and combining marks beyond six code points per cell.";
const COMBINING_NOTICE: &str = "Terminal omitted combining marks beyond six code points per cell.";

/// Editable text recovered from a completed child terminal, never an execution request.
#[derive(Debug, PartialEq, Eq)]
pub struct RecoveredTerminalInput {
    /// Printable UTF-8 and layout text; at most the admitted input byte count.
    /// CR and CRLF are normalized to newline. No terminal controls are retained.
    pub text: String,
    /// Whether controls, invalid UTF-8 or unfinished sequences were omitted.
    /// The caller should show a loss notice and offer the text for human review.
    pub omitted_controls: bool,
}

/// Recover at most 64 KiB of unread child input as bounded, inert editor text.
///
/// Device replies, arrows, function keys, bracketed-paste delimiters, OSC/DCS
/// strings and incomplete escape sequences are removed as complete sequences,
/// rather than leaving their parameter bytes in shell source. Printable Unicode,
/// newline and tab are preserved. Invalid UTF-8 is omitted with a notice flag.
/// This function performs no I/O, editor action or execution; callers must never
/// interpret recovered newline bytes as Enter key events or auto-submit text.
/// Memory and scanning are linear in the input size, with constant parser state.
pub fn recover_terminal_input(bytes: &[u8]) -> Result<RecoveredTerminalInput, ShellError> {
    if bytes.len() > CHILD_TERMINAL_RECOVERY_BYTES_MAX {
        return Err(limit(
            "recovered terminal input bytes",
            CHILD_TERMINAL_RECOVERY_BYTES_MAX,
            bytes.len(),
        ));
    }
    let mut recovered = RecoveredTerminalInput {
        text: String::with_capacity(bytes.len()),
        omitted_controls: false,
    };
    let mut input = bytes.iter();
    let mut gate = Gate::Ground;
    let mut previous_cr = false;
    let mut csi_may_be_mouse = false;
    let mut mouse_units = 0_u8;
    while !input.as_slice().is_empty() {
        let character = recovery_character(&mut input);
        if mouse_units > 0 {
            mouse_units = mouse_units.saturating_sub(1);
            continue;
        }
        let Some(character) = character else {
            recovered.omitted_controls = true;
            previous_cr = false;
            continue;
        };
        // C1 introducers also cancel an incomplete CSI/escape. Otherwise the
        // first ASCII final inside a nested string could expose its remaining
        // payload as editor text. UTF-8 tokenization distinguishes true C1
        // controls from continuation bytes inside ordinary Unicode characters.
        if !matches!(gate, Gate::String { .. }) {
            let c1 = match character {
                '\u{9b}' => Some(Gate::Csi),
                '\u{9d}' => Some(Gate::String {
                    osc: true,
                    escaped: false,
                }),
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => Some(Gate::String {
                    osc: false,
                    escaped: false,
                }),
                _ => None,
            };
            if let Some(c1) = c1 {
                gate = c1;
                csi_may_be_mouse = matches!(c1, Gate::Csi);
                recovered.omitted_controls = true;
                previous_cr = false;
                continue;
            }
        }
        match gate {
            Gate::Ground => {
                let introducer = match character {
                    '\u{1b}' => Some(Gate::Escape),
                    '\u{9b}' => Some(Gate::Csi),
                    '\u{9d}' => Some(Gate::String {
                        osc: true,
                        escaped: false,
                    }),
                    '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => Some(Gate::String {
                        osc: false,
                        escaped: false,
                    }),
                    _ => None,
                };
                if let Some(introducer) = introducer {
                    gate = introducer;
                    csi_may_be_mouse = matches!(introducer, Gate::Csi);
                    recovered.omitted_controls = true;
                } else if character == '\r' {
                    recovered.text.push('\n');
                } else if character == '\n' {
                    if !previous_cr {
                        recovered.text.push('\n');
                    }
                } else if character == '\t' || !character.is_control() {
                    recovered.text.push(character);
                } else {
                    recovered.omitted_controls = true;
                }
                previous_cr = character == '\r';
            }
            Gate::Escape => {
                csi_may_be_mouse = character == '[';
                gate = match character {
                    '[' | 'O' => Gate::Csi,
                    ']' => Gate::String {
                        osc: true,
                        escaped: false,
                    },
                    'P' | 'X' | '^' | '_' => Gate::String {
                        osc: false,
                        escaped: false,
                    },
                    '\u{1b}' => Gate::Escape,
                    ' '..='/' => Gate::Intermediate,
                    _ => Gate::Ground,
                };
            }
            Gate::Intermediate | Gate::Csi => {
                if matches!(gate, Gate::Csi) && csi_may_be_mouse && character == 'M' {
                    // X10/UTF-8 mouse reports have three additional encoded
                    // coordinates after CSI M, unlike SGR mouse CSI <...M.
                    mouse_units = 3;
                }
                csi_may_be_mouse = false;
                if character == '\u{1b}' {
                    gate = Gate::Escape;
                } else if matches!(character, '\u{18}' | '\u{1a}')
                    || (matches!(gate, Gate::Intermediate) && ('0'..='~').contains(&character))
                    || ('@'..='~').contains(&character)
                {
                    gate = Gate::Ground;
                }
            }
            Gate::String { osc, escaped } => {
                gate = if character == '\u{9c}'
                    || (osc && character == '\u{7}')
                    || (escaped && character == '\\')
                {
                    Gate::Ground
                } else {
                    Gate::String {
                        osc,
                        escaped: character == '\u{1b}',
                    }
                };
            }
        }
    }
    Ok(recovered)
}

// Decode one bounded token before scanning strings. A raw ST byte is a control;
// the same byte inside a valid multibyte character must not terminate an OSC
// payload and accidentally expose its remaining bytes as editable source.
#[allow(
    clippy::indexing_slicing,
    reason = "UTF-8 width is at most four and every buffer index is below that width."
)]
fn recovery_character(input: &mut std::slice::Iter<'_, u8>) -> Option<char> {
    let byte = *input.next()?;
    if byte < 0xa0 {
        return Some(char::from(byte));
    }
    let width = match byte {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let mut bytes = [0_u8; 4];
    bytes[0] = byte;
    for slot in bytes.iter_mut().take(width).skip(1) {
        if !input
            .as_slice()
            .first()
            .is_some_and(|byte| (0x80..=0xbf).contains(byte))
        {
            return None;
        }
        *slot = *input.next()?;
    }
    std::str::from_utf8(&bytes[..width]).ok()?.chars().next()
}

/// Validated child viewport dimensions, independent of the host status row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildTerminalSize {
    /// Visible rows, from 1 through 256.
    pub rows: u16,
    /// Visible columns, from 2 through 512; one-column wide-glyph grids are unsupported.
    pub columns: u16,
}

#[derive(Clone, Copy, Default)]
enum Gate {
    #[default]
    Ground,
    Escape,
    Intermediate,
    Csi,
    String {
        osc: bool,
        escaped: bool,
    },
}

/// Stateful, bounded VT screen and child-directed input/reply encoder.
///
/// The composition root owns event scheduling, PTY I/O, cancellation and process
/// cleanup. Never forward child output directly to the physical terminal.
pub struct ChildTerminal {
    parser: vt100::Parser,
    size: ChildTerminalSize,
    gate: Gate,
    control: Vec<u8>,
    string_bytes: usize,
    utf8: Vec<u8>,
    failure: Option<ShellError>,
    combining_omitted: bool,
    scrollback_omitted: bool,
    focus_reporting: bool,
    cell_work: usize,
}

impl ChildTerminal {
    /// Allocate the bounded primary grid; alternate rows populate lazily.
    /// Retain up to 256 primary-screen scrollback rows.
    /// Invalid dimensions fail before allocating the parser.
    pub fn new(size: ChildTerminalSize) -> Result<Self, ShellError> {
        validate_size(size)?;
        Ok(Self {
            parser: vt100::Parser::new(size.rows, size.columns, SCROLLBACK_ROWS_MAX),
            size,
            gate: Gate::Ground,
            control: Vec::with_capacity(CONTROL_BYTES_MAX),
            string_bytes: 0,
            utf8: Vec::with_capacity(4),
            failure: None,
            combining_omitted: false,
            scrollback_omitted: false,
            focus_reporting: false,
            cell_work: 0,
        })
    }

    /// Admit at most 64 KiB and return at most 8 KiB of terminal-query replies.
    ///
    /// At most 1,048,576 cell/edit work units are admitted, preventing compact
    /// controls from amplifying a turn into repeated full-screen clears.
    /// Replies go only to the child PTY. OSC clipboard/title actions and all
    /// DCS/APC/PM/SOS payloads are discarded. Split sequences persist within the
    /// documented bounds. A resource failure poisons later processing calls.
    pub fn process(&mut self, bytes: &[u8]) -> Result<Vec<u8>, ShellError> {
        self.healthy()?;
        if bytes.len() > CHILD_TERMINAL_OUTPUT_BYTES_MAX {
            return self.fail(limit(
                "child output bytes per turn",
                CHILD_TERMINAL_OUTPUT_BYTES_MAX,
                bytes.len(),
            ));
        }
        self.cell_work = 0;
        let mut replies = Vec::new();
        for &byte in bytes {
            if let Err(error) = self
                .charge_work(1)
                .and_then(|()| self.admit_byte(byte, &mut replies))
            {
                return self.fail(error);
            }
            self.scrollback_omitted |= self.parser.screen().scrollback_rows_discarded() > 0;
        }
        Ok(replies)
    }

    /// Resize both grids without resetting parser/input modes or retained history.
    /// Dimensions are checked before mutation; saved cursors are clamped when restored.
    pub fn resize(&mut self, size: ChildTerminalSize) -> Result<(), ShellError> {
        self.healthy()?;
        validate_size(size)?;
        self.parser.set_size(size.rows, size.columns);
        self.size = size;
        self.clamp_cursor();
        Ok(())
    }

    /// Return the current child grid size in cells.
    pub fn size(&self) -> ChildTerminalSize {
        self.size
    }

    /// Return an explicit retained-history/combining loss notice, surviving resets.
    pub fn notice(&self) -> Option<&'static str> {
        match (self.scrollback_omitted, self.combining_omitted) {
            (true, true) => Some(COMBINED_NOTICE),
            (true, false) => Some(SCROLLBACK_NOTICE),
            (false, true) => Some(COMBINING_NOTICE),
            (false, false) => None,
        }
    }

    /// Draw the current grid and visible cursor only within `area`.
    ///
    /// The caller reserves host chrome outside this rectangle and resizes the
    /// child grid separately. Styles are translated into Ratatui cells; raw VT
    /// bytes, OSC actions and query responses are never written during drawing.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "Grid dimensions are validated; iteration is clipped to the Ratatui rectangle."
    )]
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let clipped = area.intersection(frame.area());
        let rows = clipped.height.min(self.size.rows);
        let columns = clipped.width.min(self.size.columns);
        let screen = self.parser.screen();
        for row in 0..rows {
            for column in 0..columns {
                let Some(source) = screen.cell(row, column) else {
                    continue;
                };
                let Some(target) = frame
                    .buffer_mut()
                    .cell_mut((clipped.x + column, clipped.y + row))
                else {
                    continue;
                };
                target.reset();
                target.set_style(cell_style(source));
                if !(source.is_wide_continuation() || source.is_wide() && column + 1 >= columns) {
                    let contents = source.contents();
                    if !contents.is_empty() {
                        target.set_symbol(&contents);
                    }
                }
            }
        }
        let (row, column) = screen.cursor_position();
        if !screen.hide_cursor() && row < rows && columns > 0 {
            frame.set_cursor_position(Position::new(
                clipped.x + column.min(columns - 1),
                clipped.y + row,
            ));
        }
    }

    /// Encode one host event for the child, observing negotiated input modes.
    ///
    /// Mouse coordinates must already be relative to the child rectangle;
    /// out-of-grid events are ignored. Key releases and resize events emit no
    /// bytes. Paste is capped at 64 KiB, strips executable terminal controls,
    /// and uses bracketed-paste delimiters only when requested by the child.
    pub fn encode_input(&self, event: &Event) -> Result<Vec<u8>, ShellError> {
        self.healthy()?;
        let bytes = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.encode_key(*key),
            Event::Paste(text) => self.encode_paste(text)?,
            Event::Mouse(mouse) => self.encode_mouse(*mouse),
            Event::FocusGained if self.focus_reporting => b"\x1b[I".to_vec(),
            Event::FocusLost if self.focus_reporting => b"\x1b[O".to_vec(),
            _ => Vec::new(),
        };
        if bytes.len() > CHILD_TERMINAL_INPUT_BYTES_MAX {
            return Err(limit(
                "encoded child input bytes",
                CHILD_TERMINAL_INPUT_BYTES_MAX,
                bytes.len(),
            ));
        }
        Ok(bytes)
    }

    /// Return at most 512 terminal-safe rows plus one notice / 4 MiB of history.
    ///
    /// Primary-screen scrollback is retained; an active alternate screen yields
    /// its visible rows. Trailing empty rows are removed. This is a presentation
    /// snapshot, not a byte log. A history/combining loss notice is included if needed.
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "History is at most 256 rows and each subtracted index is less than history."
    )]
    pub fn finish_snapshot(&mut self) -> Result<Vec<String>, ShellError> {
        let mut lines = Vec::new();
        let mut bytes = 0_usize;
        self.parser.set_scrollback(SCROLLBACK_ROWS_MAX);
        let history = self.parser.screen().scrollback();
        let history_result = (|| {
            for row in 0..history {
                self.parser.set_scrollback(history - row);
                if let Some(line) = self.parser.screen().rows(0, self.size.columns).next() {
                    append_snapshot(&mut lines, &mut bytes, line)?;
                }
            }
            Ok::<_, ShellError>(())
        })();
        // A failed snapshot must not leave the live screen viewing old history.
        self.parser.set_scrollback(0);
        history_result?;
        for line in self.parser.screen().rows(0, self.size.columns) {
            append_snapshot(&mut lines, &mut bytes, line)?;
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if let Some(notice) = self.notice() {
            append_snapshot(&mut lines, &mut bytes, notice.to_owned())?;
        }
        Ok(lines)
    }

    fn healthy(&self) -> Result<(), ShellError> {
        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn charge_work(&mut self, amount: usize) -> Result<(), ShellError> {
        self.cell_work = self.cell_work.saturating_add(amount);
        if self.cell_work > CELL_WORK_PER_TURN_MAX {
            return Err(limit(
                "child terminal cell work per turn",
                CELL_WORK_PER_TURN_MAX,
                self.cell_work,
            ));
        }
        Ok(())
    }

    fn fail<T>(&mut self, error: ShellError) -> Result<T, ShellError> {
        self.failure = Some(error.clone());
        Err(error)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "Validated dimensions are at most 512 by 256; work products fit usize."
    )]
    fn admit_byte(&mut self, byte: u8, replies: &mut Vec<u8>) -> Result<(), ShellError> {
        match self.gate {
            Gate::String { osc, escaped } => {
                self.string_bytes = self.string_bytes.saturating_add(1);
                if self.string_bytes > STRING_BYTES_MAX {
                    return Err(limit(
                        "child control-string bytes",
                        STRING_BYTES_MAX,
                        self.string_bytes,
                    ));
                }
                if byte == 0x9c || (osc && byte == 7) || (escaped && byte == b'\\') {
                    self.finish_string(replies)?;
                } else {
                    if self.control.len() < CONTROL_BYTES_MAX {
                        self.control.push(byte);
                    }
                    self.gate = Gate::String {
                        osc,
                        escaped: byte == 0x1b,
                    };
                }
                Ok(())
            }
            Gate::Ground => self.ground(byte),
            Gate::Escape => match byte {
                b'[' => {
                    self.gate = Gate::Csi;
                    self.control.clear();
                    Ok(())
                }
                b']' | b'P' | b'X' | b'^' | b'_' => {
                    self.start_string(byte == b']');
                    Ok(())
                }
                0x20..=0x2f => {
                    self.control.clear();
                    self.control.push(byte);
                    self.gate = Gate::Intermediate;
                    Ok(())
                }
                0x1b => Ok(()),
                _ => {
                    self.gate = Gate::Ground;
                    if byte == b'Z' {
                        push_reply(replies, b"\x1b[?1;2c")?;
                    } else if matches!(byte, b'D' | b'E') {
                        self.charge_work(
                            usize::from(self.size.columns) + usize::from(self.size.rows),
                        )?;
                        self.parser
                            .process(if byte == b'D' { b"\n" } else { b"\r\n" });
                    } else if matches!(byte, b'7' | b'8' | b'=' | b'>' | b'M' | b'c') {
                        if byte == b'c' {
                            self.charge_work(
                                2 * usize::from(self.size.columns) * usize::from(self.size.rows),
                            )?;
                        }
                        if byte == b'M' {
                            self.charge_work(
                                usize::from(self.size.columns) + usize::from(self.size.rows),
                            )?;
                        }
                        self.parser.process(&[0x1b, byte]);
                        self.clamp_cursor();
                        if byte == b'c' {
                            self.focus_reporting = false;
                        }
                    }
                    Ok(())
                }
            },
            Gate::Intermediate => {
                if byte == 0x1b {
                    self.control.clear();
                    self.gate = Gate::Escape;
                    return Ok(());
                }
                self.push_control(byte)?;
                if (0x30..=0x7e).contains(&byte) {
                    self.gate = Gate::Ground;
                    // Character-set designations are inert in this UTF-8 model.
                    self.control.clear();
                }
                Ok(())
            }
            Gate::Csi => {
                if byte == 0x1b {
                    self.control.clear();
                    self.gate = Gate::Escape;
                    return Ok(());
                }
                if matches!(byte, 0x18 | 0x1a) {
                    self.control.clear();
                    self.gate = Gate::Ground;
                    return Ok(());
                }
                // Only complete ASCII CSI syntax reaches vt100. Forwarding a
                // C1/UTF-8 string introducer here could desynchronize its VTE
                // state and bypass our bounded OSC/DCS admission on later calls.
                if !(0x20..=0x7e).contains(&byte) {
                    self.control.clear();
                    self.gate = Gate::Ground;
                    return self.ground(byte);
                }
                self.push_control(byte)?;
                if (0x40..=0x7e).contains(&byte) {
                    self.gate = Gate::Ground;
                    self.finish_csi(replies)?;
                    self.control.clear();
                }
                Ok(())
            }
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "Validated rows plus columns are at most 768."
    )]
    fn ground(&mut self, byte: u8) -> Result<(), ShellError> {
        if !self.utf8.is_empty() {
            if (0x80..=0xbf).contains(&byte) {
                self.utf8.push(byte);
                match std::str::from_utf8(&self.utf8) {
                    Ok(text) => {
                        let character = text.chars().next().unwrap_or('\u{fffd}');
                        self.utf8.clear();
                        self.print(character);
                    }
                    Err(error) if error.error_len().is_none() && self.utf8.len() < 4 => {}
                    Err(_) => {
                        self.utf8.clear();
                        self.print('\u{fffd}');
                    }
                }
                return Ok(());
            }
            self.utf8.clear();
            self.print('\u{fffd}');
        }
        match byte {
            0x1b => self.gate = Gate::Escape,
            0x9b => {
                self.control.clear();
                self.gate = Gate::Csi;
            }
            0x9d => self.start_string(true),
            0x90 | 0x98 | 0x9e | 0x9f => self.start_string(false),
            0xc2..=0xf4 => self.utf8.push(byte),
            0x20..=0x7e => self.print(char::from(byte)),
            8..=15 => {
                if matches!(byte, 10..=12) {
                    self.charge_work(usize::from(self.size.columns) + usize::from(self.size.rows))?;
                }
                self.parser.process(&[byte]);
            }
            0x80..=0xff => self.print('\u{fffd}'),
            _ => {}
        }
        Ok(())
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "Cursor indices are validated and subtraction follows positive-coordinate guards."
    )]
    fn print(&mut self, character: char) {
        if character.width() == Some(0) {
            let (mut row, column) = self.parser.screen().cursor_position();
            let mut column = if column > 0 {
                column - 1
            } else if row > 0 {
                row -= 1;
                self.size.columns - 1
            } else {
                0
            };
            let screen = self.parser.screen();
            if screen
                .cell(row, column)
                .is_some_and(vt100::Cell::is_wide_continuation)
            {
                column = column.saturating_sub(1);
            }
            if screen
                .cell(row, column)
                .is_some_and(|cell| cell.contents().chars().count() >= 6)
            {
                self.combining_omitted = true;
                return;
            }
        }
        self.parser
            .process(character.encode_utf8(&mut [0; 4]).as_bytes());
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "The admitted control length is at most 256 bytes."
    )]
    fn push_control(&mut self, byte: u8) -> Result<(), ShellError> {
        if self.control.len() >= CONTROL_BYTES_MAX {
            return Err(limit(
                "child CSI/escape bytes",
                CONTROL_BYTES_MAX,
                self.control.len() + 1,
            ));
        }
        self.control.push(byte);
        Ok(())
    }

    fn start_string(&mut self, osc: bool) {
        self.control.clear();
        self.string_bytes = 0;
        self.gate = Gate::String {
            osc,
            escaped: false,
        };
    }

    fn finish_string(&mut self, replies: &mut Vec<u8>) -> Result<(), ShellError> {
        if matches!(self.gate, Gate::String { osc: true, .. })
            && self.string_bytes <= CONTROL_BYTES_MAX
        {
            let query = self.control.strip_suffix(b"\x1b").unwrap_or(&self.control);
            let response: &[u8] = match query {
                b"10;?" => b"\x1b]10;rgb:d8d8/dede/e9e9\x1b\\",
                b"11;?" => b"\x1b]11;rgb:1010/1414/1c1c\x1b\\",
                b"12;?" => b"\x1b]12;rgb:d8d8/dede/e9e9\x1b\\",
                _ => b"",
            };
            push_reply(replies, response)?;
        }
        self.control.clear();
        self.gate = Gate::Ground;
        Ok(())
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "Validated cursor and dimensions are at most 512 columns and 256 rows."
    )]
    fn finish_csi(&mut self, replies: &mut Vec<u8>) -> Result<(), ShellError> {
        self.charge_work(csi_work(&self.control, self.size))?;
        let code = self.control.as_slice();
        let (row, column) = self.parser.screen().cursor_position();
        match code {
            b"5n" => return push_reply(replies, b"\x1b[0n"),
            b"6n" | b"?6n" => {
                let private = if code.starts_with(b"?") { "?" } else { "" };
                return push_reply(
                    replies,
                    format!(
                        "\x1b[{private}{};{}R",
                        row + 1,
                        column.min(self.size.columns - 1) + 1
                    )
                    .as_bytes(),
                );
            }
            b"c" | b"0c" => return push_reply(replies, b"\x1b[?1;2c"),
            b">c" | b">0c" => return push_reply(replies, b"\x1b[>0;0;0c"),
            b"18t" => {
                return push_reply(
                    replies,
                    format!("\x1b[8;{};{}t", self.size.rows, self.size.columns).as_bytes(),
                );
            }
            _ => {}
        }
        let Some((&final_byte, parameters)) = code.split_last() else {
            return Ok(());
        };
        if let Some(parameters) = parameters.strip_prefix(b"?")
            && matches!(final_byte, b'h' | b'l')
        {
            for parameter in parameters.split(|byte| *byte == b';') {
                if parameter == b"1004" {
                    self.focus_reporting = final_byte == b'h';
                }
            }
        }
        // Some dependency line/cell operations loop over the raw u16 count.
        // Counts above the viewport are equivalent to clearing that region.
        let maximum = match final_byte {
            b'@' | b'P' | b'X' => Some(self.size.columns),
            b'L' | b'M' | b'S' | b'T' => Some(self.size.rows),
            _ => None,
        };
        if let Some(maximum) = maximum.filter(|_| {
            parameters
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(*byte, b';' | b':'))
        }) {
            let count = edit_count(parameters, maximum);
            self.parser
                .process(format!("\x1b[{count}{}", char::from(final_byte)).as_bytes());
        } else {
            self.parser.process(b"\x1b[");
            self.parser.process(code);
        }
        self.clamp_cursor();
        Ok(())
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "Dimensions are validated nonzero and coordinates clamp before increment."
    )]
    fn clamp_cursor(&mut self) {
        let (row, column) = self.parser.screen().cursor_position();
        if row >= self.size.rows || column > self.size.columns {
            self.parser.process(
                format!(
                    "\x1b[{};{}H",
                    row.min(self.size.rows - 1) + 1,
                    column.min(self.size.columns - 1) + 1
                )
                .as_bytes(),
            );
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        reason = "Modifier bits and matched F-key ranges bound all protocol arithmetic and indexing."
    )]
    fn encode_key(&self, key: KeyEvent) -> Vec<u8> {
        let modifiers = key.modifiers;
        let modifier = 1
            + u8::from(modifiers.contains(KeyModifiers::SHIFT))
            + 2 * u8::from(modifiers.contains(KeyModifiers::ALT))
            + 4 * u8::from(modifiers.contains(KeyModifiers::CONTROL));
        let cursor = |last: char| {
            if modifier > 1 {
                format!("\x1b[1;{modifier}{last}").into_bytes()
            } else if self.parser.screen().application_cursor() {
                format!("\x1bO{last}").into_bytes()
            } else {
                format!("\x1b[{last}").into_bytes()
            }
        };
        let tilde = |number: u8| {
            if modifier > 1 {
                format!("\x1b[{number};{modifier}~").into_bytes()
            } else {
                format!("\x1b[{number}~").into_bytes()
            }
        };
        match key.code {
            KeyCode::Up => cursor('A'),
            KeyCode::Down => cursor('B'),
            KeyCode::Right => cursor('C'),
            KeyCode::Left => cursor('D'),
            KeyCode::Home => cursor('H'),
            KeyCode::End => cursor('F'),
            KeyCode::Insert => tilde(2),
            KeyCode::Delete => tilde(3),
            KeyCode::PageUp => tilde(5),
            KeyCode::PageDown => tilde(6),
            KeyCode::BackTab => b"\x1b[Z".to_vec(),
            KeyCode::F(number @ 1..=4) => {
                let last = char::from(b'P' + number - 1);
                if modifier > 1 {
                    format!("\x1b[1;{modifier}{last}").into_bytes()
                } else {
                    format!("\x1bO{last}").into_bytes()
                }
            }
            KeyCode::F(number @ 5..=12) => {
                tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(number - 5)])
            }
            KeyCode::Char(character) => {
                let mut bytes = Vec::new();
                if modifiers.contains(KeyModifiers::ALT) {
                    bytes.push(0x1b);
                }
                if modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii() {
                    let character = u8::try_from(u32::from(character)).unwrap_or(b'?');
                    bytes.push(if character == b'?' {
                        127
                    } else {
                        character.to_ascii_uppercase() & 0x1f
                    });
                } else {
                    bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
                }
                bytes
            }
            KeyCode::Enter => prefixed_key(modifiers, b'\r'),
            KeyCode::Backspace => prefixed_key(modifiers, 127),
            KeyCode::Tab => prefixed_key(modifiers, b'\t'),
            KeyCode::Esc => vec![0x1b],
            _ => Vec::new(),
        }
    }

    fn encode_paste(&self, text: &str) -> Result<Vec<u8>, ShellError> {
        if text.len() > CHILD_TERMINAL_OUTPUT_BYTES_MAX {
            return Err(limit(
                "child paste bytes",
                CHILD_TERMINAL_OUTPUT_BYTES_MAX,
                text.len(),
            ));
        }
        let mut bytes = Vec::new();
        let bracketed = self.parser.screen().bracketed_paste();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        for character in text.chars() {
            if character.is_control() && !matches!(character, '\r' | '\n' | '\t') {
                continue;
            }
            let character = if !bracketed && character == '\n' {
                '\r'
            } else {
                character
            };
            bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
        }
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        Ok(bytes)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "Mouse coordinates are validated against 512 by 256 and protocol button bits fit u8."
    )]
    fn encode_mouse(&self, mouse: MouseEvent) -> Vec<u8> {
        use vt100::{MouseProtocolEncoding as Encoding, MouseProtocolMode as Mode};
        if mouse.row >= self.size.rows || mouse.column >= self.size.columns {
            return Vec::new();
        }
        let mode = self.parser.screen().mouse_protocol_mode();
        if mode == Mode::None {
            return Vec::new();
        }
        let button = |button| match button {
            MouseButton::Left => 0_u8,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };
        let (mut code, released) = match mouse.kind {
            MouseEventKind::Down(value) => (button(value), false),
            MouseEventKind::Up(value) if mode != Mode::Press => (button(value), true),
            MouseEventKind::Drag(value) if matches!(mode, Mode::ButtonMotion | Mode::AnyMotion) => {
                (button(value) + 32, false)
            }
            MouseEventKind::Moved if mode == Mode::AnyMotion => (35, false),
            MouseEventKind::ScrollUp => (64, false),
            MouseEventKind::ScrollDown => (65, false),
            MouseEventKind::ScrollLeft => (66, false),
            MouseEventKind::ScrollRight => (67, false),
            _ => return Vec::new(),
        };
        code += 4 * u8::from(mouse.modifiers.contains(KeyModifiers::SHIFT))
            + 8 * u8::from(mouse.modifiers.contains(KeyModifiers::ALT))
            + 16 * u8::from(mouse.modifiers.contains(KeyModifiers::CONTROL));
        match self.parser.screen().mouse_protocol_encoding() {
            Encoding::Sgr => format!(
                "\x1b[<{code};{};{}{}",
                mouse.column + 1,
                mouse.row + 1,
                if released { 'm' } else { 'M' }
            )
            .into_bytes(),
            encoding => {
                if released {
                    code = (code & (4 | 8 | 16)) | 3;
                }
                let mut bytes = b"\x1b[M".to_vec();
                for value in [
                    u32::from(code) + 32,
                    u32::from(mouse.column) + 33,
                    u32::from(mouse.row) + 33,
                ] {
                    if encoding == Encoding::Default {
                        let Ok(value) = u8::try_from(value) else {
                            return Vec::new();
                        };
                        bytes.push(value);
                    } else if let Some(value) = char::from_u32(value) {
                        bytes.extend_from_slice(value.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                }
                bytes
            }
        }
    }
}

// This is an upper bound on cells edited and row-vector shifts, not a time
// estimate. Small controls cannot amplify one input turn into unlimited clears
// or repeated row allocation. Parameter storage itself remains fixed in VTE.
#[allow(
    clippy::arithmetic_side_effects,
    reason = "Validated dimensions and at most 256 parameter bytes bound all products below 2^25."
)]
fn csi_work(code: &[u8], size: ChildTerminalSize) -> usize {
    let Some((&final_byte, parameters)) = code.split_last() else {
        return 0;
    };
    let columns = usize::from(size.columns);
    let rows = usize::from(size.rows);
    match final_byte {
        b'J' => rows * columns,
        b'K' | b'X' => columns,
        b'@' | b'P' => usize::from(edit_count(parameters, size.columns)) * columns,
        b'L' | b'M' | b'S' | b'T' => {
            usize::from(edit_count(parameters, size.rows)) * (columns + rows)
        }
        b'h' | b'l' => parameters.strip_prefix(b"?").map_or(0, |parameters| {
            parameters
                .split(|byte| *byte == b';')
                .filter(|parameter| matches!(*parameter, b"47" | b"1047" | b"1049"))
                .count()
                * rows
                * columns
        }),
        _ => 0,
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "Only ASCII decimal digits enter subtraction; accumulation saturates."
)]
fn edit_count(parameters: &[u8], maximum: u16) -> u16 {
    parameters
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .fold(0_u16, |count, byte| {
            count
                .saturating_mul(10)
                .saturating_add(u16::from(byte - b'0'))
        })
        .clamp(1, maximum)
}

fn validate_size(size: ChildTerminalSize) -> Result<(), ShellError> {
    if !(1..=256).contains(&size.rows) || !(2..=512).contains(&size.columns) {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "child terminal size exceeds supported bounds",
        )
        .with_context(format!(
            "rows={} (1..=256), columns={} (2..=512)",
            size.rows, size.columns
        ))
        .with_help(
            "Resize the terminal to at least two columns and at most 512 columns by 256 rows.",
        ));
    }
    Ok(())
}

fn limit(domain: &str, maximum: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{domain} exceeds limit {maximum}"),
    )
    .with_context(format!("observed {observed}"))
    .with_help("Stop the child application or reduce its terminal output/input size.")
}

fn push_reply(replies: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ShellError> {
    let observed = replies.len().saturating_add(bytes.len());
    if observed > REPLY_BYTES_MAX {
        return Err(limit(
            "child terminal reply bytes per turn",
            REPLY_BYTES_MAX,
            observed,
        ));
    }
    replies.extend_from_slice(bytes);
    Ok(())
}

fn append_snapshot(
    lines: &mut Vec<String>,
    bytes: &mut usize,
    line: String,
) -> Result<(), ShellError> {
    let line = escape_terminal_line(line.trim_end());
    let observed = bytes.saturating_add(line.len()).saturating_add(1);
    if observed > SNAPSHOT_BYTES_MAX {
        return Err(limit(
            "child terminal snapshot bytes",
            SNAPSHOT_BYTES_MAX,
            observed,
        ));
    }
    *bytes = observed;
    lines.push(line);
    Ok(())
}

fn prefixed_key(modifiers: KeyModifiers, byte: u8) -> Vec<u8> {
    if modifiers.contains(KeyModifiers::ALT) {
        vec![0x1b, byte]
    } else {
        vec![byte]
    }
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default()
        .fg(color(cell.fgcolor(), DEFAULT_FOREGROUND))
        .bg(color(cell.bgcolor(), DEFAULT_BACKGROUND));
    for (enabled, modifier) in [
        (cell.bold(), Modifier::BOLD),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
        (cell.inverse(), Modifier::REVERSED),
    ] {
        if enabled {
            style = style.add_modifier(modifier);
        }
    }
    style
}

fn color(color: vt100::Color, default: Color) -> Color {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};

    fn terminal(rows: u16, columns: u16) -> ChildTerminal {
        ChildTerminal::new(ChildTerminalSize { rows, columns }).unwrap()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn recovered_input_preserves_unicode_layout_without_creating_key_events() {
        let bytes = "printf '界 café'\r\n\tmore\rlast\n".as_bytes();
        let recovered = recover_terminal_input(bytes).unwrap();
        assert_eq!(recovered.text, "printf '界 café'\n\tmore\nlast\n");
        assert!(!recovered.omitted_controls);
        assert!(recovered.text.len() <= bytes.len());
    }

    #[test]
    fn recovered_input_strips_whole_replies_keys_and_paste_delimiters() {
        let bytes = b"echo \x1b[4;7R\x1b[?1;2c\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[A\x1bOP\x1bO1;5A\x1b[200~hello\x1b[201~\x1b[<0;3;4M\x1b[M !!\x07\0\n";
        let recovered = recover_terminal_input(bytes).unwrap();
        assert_eq!(recovered.text, "echo hello\n");
        assert!(recovered.omitted_controls);
        let recovered = recover_terminal_input("a\x1b[M \u{12c}#b".as_bytes()).unwrap();
        assert_eq!(recovered.text, "ab");
    }

    #[test]
    fn recovered_string_payloads_and_partial_sequences_never_become_source() {
        for tail in [
            "\x1b",
            "\x1b[123;",
            "\x1bO1;",
            "\x1b]52;c;hidden",
            "\x1bPhidden",
            "\x1b_hidden",
            "\x1b[Ma",
        ] {
            let recovered = recover_terminal_input(format!("safe{tail}").as_bytes()).unwrap();
            assert_eq!(recovered.text, "safe", "{tail:?}");
            assert!(recovered.omitted_controls);
        }
        // The 0x9c continuation byte in Ŝ is not a standalone ST control.
        let recovered =
            recover_terminal_input("a\x1b]52;c;Ŝhidden\x1b\\b\x1bPsecret\x1b\\c".as_bytes())
                .unwrap();
        assert_eq!(recovered.text, "abc");
        let recovered = recover_terminal_input(b"a\x9d52;c;hidden\x9cb\x9b4;7Rc").unwrap();
        assert_eq!(recovered.text, "abc");
        let recovered = recover_terminal_input(b"safe\x1b[1\x9d52;c;hidden\x07visible").unwrap();
        assert_eq!(recovered.text, "safevisible");
        assert!(recovered.omitted_controls);
    }

    #[test]
    fn recovered_input_omits_invalid_utf8_and_enforces_exact_byte_admission() {
        let recovered = recover_terminal_input(b"a\xffb\xe2\x82").unwrap();
        assert_eq!(recovered.text, "ab");
        assert!(recovered.omitted_controls);
        let exact = vec![b'x'; CHILD_TERMINAL_RECOVERY_BYTES_MAX];
        let recovered = recover_terminal_input(&exact).unwrap();
        assert_eq!(recovered.text.as_bytes(), exact);
        assert!(!recovered.omitted_controls);
        assert_eq!(
            recover_terminal_input(&vec![b'x'; CHILD_TERMINAL_RECOVERY_BYTES_MAX + 1])
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(recover_terminal_input(b"").unwrap().text, "");
    }

    #[test]
    fn split_utf8_sgr_and_wide_cells_render_without_overwriting_host_chrome() {
        let mut child = terminal(3, 12);
        for byte in "\x1b[1;3;4;7;38;2;12;34;56;48;5;123m界e\u{301}\x1b[0mZ".bytes() {
            assert!(child.process(&[byte]).unwrap().is_empty());
        }
        let mut display = Terminal::new(TestBackend::new(14, 5)).unwrap();
        display
            .draw(|frame| {
                frame.render_widget(Paragraph::new("HOST"), Rect::new(0, 0, 14, 1));
                child.render(frame, Rect::new(1, 1, 12, 3));
            })
            .unwrap();
        let buffer = display.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "H");
        assert_eq!(buffer[(1, 1)].symbol(), "界");
        assert_eq!(buffer[(3, 1)].symbol(), "e\u{301}");
        assert_eq!(buffer[(1, 1)].fg, Color::Rgb(12, 34, 56));
        assert_eq!(buffer[(1, 1)].bg, Color::Indexed(123));
        assert!(buffer[(1, 1)].modifier.contains(
            Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED | Modifier::REVERSED
        ));
        assert_eq!(buffer[(4, 1)].modifier, Modifier::empty());
        assert_eq!(buffer[(4, 1)].fg, DEFAULT_FOREGROUND);
        assert_eq!(child.parser.screen().cursor_position(), (0, 4));
    }

    #[test]
    fn cursor_erase_alternate_screen_and_restore_follow_terminal_state() {
        let mut child = terminal(4, 12);
        child
            .process(b"primary\x1b7\x1b[?1049h\x1b[2;3Halt")
            .unwrap();
        assert!(child.parser.screen().alternate_screen());
        assert_eq!(child.finish_snapshot().unwrap(), ["", "  alt"]);
        child.process(b"\x1b[?1049l\x1b8!\x1b[1;3H\x1b[K").unwrap();
        assert!(!child.parser.screen().alternate_screen());
        assert_eq!(child.finish_snapshot().unwrap(), ["pr"]);
        child
            .process(b"\x1b[2J\x1b[Hone\x1bEtwo\x1bDthree")
            .unwrap();
        assert_eq!(child.finish_snapshot().unwrap(), ["one", "two", "   three"]);
    }

    #[test]
    fn dependency_restores_saved_cursors_inside_each_resized_screen() {
        let mut parser = vt100::Parser::new(8, 16, 256);
        parser.process(b"\x1b[8;16H\x1b7\x1b[?1049h\x1b[8;16H\x1b7");
        parser.set_size(2, 3);
        parser.process(b"\x1b8x\x1b[?1049l\x1b8y");
        assert!(parser.screen().cursor_position().0 < 2);
        assert!(parser.screen().cursor_position().1 <= 3);
        assert!(parser.screen().contents().contains('y'));
    }

    #[test]
    fn screen_and_saved_cursor_remain_valid_after_shrinking_through_a_wide_cell() {
        let mut child = terminal(8, 16);
        child
            .process("\x1b[8;15H界\x1b7\x1b[1;3H界".as_bytes())
            .unwrap();
        child
            .resize(ChildTerminalSize {
                rows: 2,
                columns: 3,
            })
            .unwrap();
        child.process(b"\x1b[1;3H\x1b[K\x1b8x").unwrap();
        let (row, column) = child.parser.screen().cursor_position();
        assert!(row < 2);
        assert!(column <= 3);
        assert!(
            child
                .finish_snapshot()
                .unwrap()
                .iter()
                .any(|line| line.contains('x'))
        );
    }

    #[test]
    fn scrollback_is_bounded_and_snapshot_restores_live_view() {
        let mut child = terminal(3, 16);
        for number in 0..400 {
            child
                .process(format!("row-{number:03}\r\n").as_bytes())
                .unwrap();
        }
        let lines = child.finish_snapshot().unwrap();
        assert_eq!(lines.len(), 259);
        assert_eq!(lines.first().unwrap(), "row-142");
        assert_eq!(lines.get(257).unwrap(), "row-399");
        assert_eq!(lines.last().unwrap(), SCROLLBACK_NOTICE);
        assert_eq!(child.parser.screen().scrollback_rows_discarded(), 142);
        assert_eq!(child.parser.screen().scrollback(), 0);
        child.process(b"live").unwrap();
        assert_eq!(child.parser.screen().cell(2, 0).unwrap().contents(), "l");
        child.process(b"\x1bc").unwrap();
        assert_eq!(child.notice(), Some(SCROLLBACK_NOTICE));
    }

    #[test]
    fn combining_excess_is_observable_without_stopping_the_child() {
        let mut child = terminal(2, 12);
        child
            .process(format!("x{}y", "\u{301}".repeat(100)).as_bytes())
            .unwrap();
        assert_eq!(
            child
                .parser
                .screen()
                .cell(0, 0)
                .unwrap()
                .contents()
                .chars()
                .count(),
            6
        );
        assert_eq!(child.parser.screen().cell(0, 1).unwrap().contents(), "y");
        assert_eq!(child.notice(), Some(COMBINING_NOTICE));
        assert_eq!(
            child.finish_snapshot().unwrap().last().unwrap(),
            COMBINING_NOTICE
        );
        child.process(b"z").unwrap();
    }

    #[test]
    fn device_queries_are_local_and_unsafe_control_strings_are_discarded() {
        let mut child = terminal(10, 40);
        child.process(b"\x1b[4;7H").unwrap();
        assert_eq!(
            child.process(b"\x1b[6n\x1b[5n\x1b[c\x1b[18t").unwrap(),
            b"\x1b[4;7R\x1b[0n\x1b[?1;2c\x1b[8;10;40t"
        );
        let mut replies = Vec::new();
        for byte in b"\x1b]52;c;ZXhlY3V0ZQ==\x07\x1bPsecret\x1b\\\x1b]10;?\x1b\\" {
            replies.extend(child.process(&[*byte]).unwrap());
        }
        assert_eq!(replies, b"\x1b]10;rgb:d8d8/dede/e9e9\x1b\\");
        assert!(child.finish_snapshot().unwrap().is_empty());
        // Malformed CSI cannot introduce OSC inside the dependency parser.
        child.process(b"\x1b[1\x9d52;c;hidden\x07visible").unwrap();
        assert_eq!(
            child.finish_snapshot().unwrap(),
            ["", "", "", "      visible"]
        );
    }

    #[test]
    fn control_reply_and_turn_limits_fail_stickily_at_admission() {
        let mut child = terminal(2, 10);
        child.process(b"\x1b[").unwrap();
        child.process(&[b'1'; CONTROL_BYTES_MAX]).unwrap();
        let error = child.process(b"m").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(child.process(b"safe").unwrap_err(), error);
        assert_eq!(
            child
                .encode_input(&key(KeyCode::Enter, KeyModifiers::NONE))
                .unwrap_err(),
            error
        );
        let mut child = terminal(2, 10);
        child.process(b"\x1bP").unwrap();
        child.process(&vec![b'x'; STRING_BYTES_MAX]).unwrap();
        assert_eq!(
            child.process(b"x").unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        let mut child = terminal(2, 10);
        assert_eq!(
            child.process(&b"\x1b[6n".repeat(2000)).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        let mut child = terminal(2, 10);
        assert_eq!(
            child
                .process(&vec![b'x'; CHILD_TERMINAL_OUTPUT_BYTES_MAX + 1])
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        assert!(child.finish_snapshot().unwrap().is_empty());
    }

    #[test]
    fn dimensions_are_validated_without_mutating_the_previous_grid() {
        for size in [
            ChildTerminalSize {
                rows: 0,
                columns: 80,
            },
            ChildTerminalSize {
                rows: 257,
                columns: 80,
            },
            ChildTerminalSize {
                rows: 1,
                columns: 1,
            },
            ChildTerminalSize {
                rows: 1,
                columns: 513,
            },
        ] {
            assert!(
                matches!(ChildTerminal::new(size), Err(error) if error.code == ErrorCode::ResourceLimit)
            );
            let mut child = terminal(2, 10);
            child.process(b"kept").unwrap();
            assert!(child.resize(size).is_err());
            assert_eq!(child.finish_snapshot().unwrap(), ["kept"]);
        }
        assert!(
            ChildTerminal::new(ChildTerminalSize {
                rows: 256,
                columns: 512
            })
            .is_ok()
        );
    }

    #[test]
    fn keyboard_focus_paste_and_mouse_observe_child_modes() {
        let mut child = terminal(10, 40);
        assert_eq!(
            child
                .encode_input(&key(KeyCode::Up, KeyModifiers::NONE))
                .unwrap(),
            b"\x1b[A"
        );
        child
            .process(b"\x1b[?1h\x1b[?1004h\x1b[?2004h\x1b[?1002h\x1b[?1006h")
            .unwrap();
        assert_eq!(
            child
                .encode_input(&key(KeyCode::Up, KeyModifiers::NONE))
                .unwrap(),
            b"\x1bOA"
        );
        assert_eq!(
            child
                .encode_input(&key(KeyCode::Left, KeyModifiers::CONTROL))
                .unwrap(),
            b"\x1b[1;5D"
        );
        assert_eq!(child.encode_input(&Event::FocusGained).unwrap(), b"\x1b[I");
        assert_eq!(
            child
                .encode_input(&Event::Paste("a\x1b[201~\u{7}\nb".into()))
                .unwrap(),
            b"\x1b[200~a[201~\nb\x1b[201~"
        );
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 6,
            row: 3,
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(
            child.encode_input(&Event::Mouse(mouse)).unwrap(),
            b"\x1b[<16;7;4M"
        );
        assert_eq!(
            child
                .encode_input(&Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    ..mouse
                }))
                .unwrap(),
            b"\x1b[<16;7;4m"
        );
        assert!(
            child
                .encode_input(&Event::Mouse(MouseEvent { row: 10, ..mouse }))
                .unwrap()
                .is_empty()
        );
        child.process(b"\x1bc").unwrap();
        assert!(child.encode_input(&Event::FocusGained).unwrap().is_empty());
        assert!(child.encode_input(&Event::Mouse(mouse)).unwrap().is_empty());
        assert_eq!(
            child.encode_input(&Event::Paste("a\nb".into())).unwrap(),
            b"a\rb"
        );
        assert_eq!(
            child
                .encode_input(&Event::Paste(
                    "x".repeat(CHILD_TERMINAL_OUTPUT_BYTES_MAX + 1)
                ))
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn compact_edits_charge_bounded_work_before_mutating_the_screen() {
        let mut child = terminal(100, 100);
        child.process(b"kept").unwrap();
        assert_eq!(
            child.process(&b"\x1b[2J".repeat(110)).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        assert!(child.process(b"later").is_err());
        assert_eq!(
            csi_work(
                b"65535;0L",
                ChildTerminalSize {
                    rows: 100,
                    columns: 100
                }
            ),
            20_000
        );
        let mut child = terminal(2, 10);
        child
            .process(b"abc\x1b[H\x1b[999999999999999999999999999999;0P")
            .unwrap();
        assert!(child.finish_snapshot().unwrap().is_empty());
        child
            .process(b"abc\x1b[H\x1b[999999999999999999999999999999:0P")
            .unwrap();
        assert!(child.finish_snapshot().unwrap().is_empty());
    }

    #[test]
    fn bounded_seeded_screen_transitions_preserve_valid_cells() {
        let mut child = terminal(8, 24);
        let controls: &[&[u8]] = &[
            b"\x1b[H",
            b"\x1b7",
            b"\x1b8",
            b"\x1b[2J",
            b"\x1b[K",
            b"\x1b[999999999999999999999999999999999999999999@",
            b"\x1b[65535L",
            b"\x1b[65535P",
            b"\x1b[65535T",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[2;7r",
            b"\x1b[?6h",
            b"\x1b[?6l",
            b"\r\n",
            "界\u{301}abc".as_bytes(),
        ];
        let mut seed = 0x05e9_2026_u64;
        for iteration in 0..4000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = usize::try_from(seed % u64::try_from(controls.len()).unwrap()).unwrap();
            child.process(controls[index]).unwrap();
            if iteration % 31 == 0 {
                child
                    .resize(ChildTerminalSize {
                        rows: u16::try_from(seed % 8 + 1).unwrap(),
                        columns: u16::try_from(seed % 23 + 2).unwrap(),
                    })
                    .unwrap();
            }
            let (row, column) = child.parser.screen().cursor_position();
            assert!(row < child.size.rows, "iteration {iteration}");
            assert!(column <= child.size.columns, "iteration {iteration}");
        }
        assert!(child.finish_snapshot().unwrap().len() <= 513);
    }
}
