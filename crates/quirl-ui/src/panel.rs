use quirl_core::{Entry, ErrorCode, ShellError, escape_terminal_line, reject_terminal_controls};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Terminal-independent table model with a mandatory plain-text fallback.
///
/// Titles, headings, and cells are single-line typed text and may not contain
/// terminal controls. Callers that mutate public fields after construction must
/// call [`Self::validate`] again before rendering or crossing a trust boundary.
pub struct PanelModel {
    /// Single-line panel heading.
    pub title: String,
    /// Non-empty ordered column headings.
    pub columns: Vec<String>,
    /// Table rows, each containing exactly one cell per column.
    pub rows: Vec<Vec<String>>,
    /// Non-empty terminal-safe text used when typed/table rendering is unavailable.
    pub plain_fallback: String,
}

impl PanelModel {
    /// Construct and validate a complete panel atomically.
    ///
    /// Returns [`ErrorCode::Validation`] for terminal controls, multi-line table
    /// text, an empty fallback or column set, or a row-width mismatch. This
    /// model does not impose row or byte limits; the owning provider must bound
    /// those before construction.
    pub fn new(
        title: impl Into<String>,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        plain_fallback: impl Into<String>,
    ) -> Result<Self, ShellError> {
        let panel = Self {
            title: title.into(),
            columns,
            rows,
            plain_fallback: plain_fallback.into(),
        };
        panel.validate()?;
        Ok(panel)
    }

    /// Validate text safety and the rectangular table invariant.
    ///
    /// The plain fallback may contain newlines and tabs, but all table fields
    /// must occupy one line. Returns [`ErrorCode::Validation`] on the first
    /// invalid field.
    pub fn validate(&self) -> Result<(), ShellError> {
        reject_panel_text("panel title", &self.title)?;
        reject_terminal_controls("panel plain fallback", &self.plain_fallback)?;
        if self.plain_fallback.trim().is_empty() {
            return Err(panel_error("panel plain fallback must not be empty"));
        }
        if self.columns.is_empty() {
            return Err(panel_error("panel must declare at least one column"));
        }
        for column in &self.columns {
            reject_panel_text("panel column", column)?;
        }
        for row in &self.rows {
            if row.len() != self.columns.len() {
                return Err(panel_error(format!(
                    "panel row has {} cells for {} columns",
                    row.len(),
                    self.columns.len()
                )));
            }
            for cell in row {
                reject_panel_text("panel cell", cell)?;
            }
        }
        Ok(())
    }

    /// Render a deterministic terminal-safe table or the empty-state fallback.
    ///
    /// An empty row set returns the trimmed fallback plus one newline. Otherwise
    /// headings and rows are padded using UTF-8 byte widths and the result ends
    /// with a newline. Call [`Self::validate`] after any direct field mutation.
    pub fn render_plain(&self) -> String {
        if self.rows.is_empty() {
            return format!("{}\n", self.plain_fallback.trim_end());
        }
        let cells = self
            .rows
            .iter()
            .map(Vec::len)
            .chain(std::iter::once(self.columns.len()))
            .max()
            .unwrap_or_default();
        let mut widths = vec![0; cells];
        for (index, column) in self.columns.iter().enumerate() {
            widths[index] = column.len();
        }
        for row in &self.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.len());
            }
        }
        let mut output = String::new();
        render_row(&mut output, &self.columns, &widths);
        for row in &self.rows {
            render_row(&mut output, row, &widths);
        }
        output
    }
}

/// Convert bounded directory entries into a terminal-safe metadata panel.
///
/// Filenames and `path` are escaped before inclusion. Modification timestamps
/// are whole Unix seconds and sizes are bytes. Returns [`ErrorCode::Validation`]
/// only if the constructed panel violates [`PanelModel`] invariants; the entry
/// count is inherited from the caller and should already be bounded by the
/// directory-listing API.
pub fn directory_panel(path: &str, entries: &[Entry]) -> Result<PanelModel, ShellError> {
    let rows = entries
        .iter()
        .map(|entry| {
            vec![
                entry.display_name(),
                format!("{:?}", entry.kind).to_lowercase(),
                entry.size.to_string(),
                entry
                    .modified_unix_seconds
                    .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            ]
        })
        .collect();
    PanelModel::new(
        format!("directory {}", panel_line(path)),
        vec![
            "name".to_owned(),
            "kind".to_owned(),
            "bytes".to_owned(),
            "modified".to_owned(),
        ],
        rows,
        format!("directory {} has no visible entries", panel_line(path)),
    )
}

fn panel_line(value: &str) -> String {
    escape_terminal_line(value)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// One process-table row supplied by a platform-specific process collector.
pub struct ProcessPanelRow {
    /// Process identifier.
    pub pid: u32,
    /// Parent process identifier when the platform exposes it.
    pub parent_pid: Option<u32>,
    /// Single-line terminal-safe platform state label.
    pub state: String,
    /// Single-line terminal-safe command description.
    pub command: String,
}

/// Convert process rows into a validated process table.
///
/// Returns [`ErrorCode::Validation`] if collector-provided state or command
/// text contains terminal controls/newlines. The caller owns process-count and
/// field-size bounds before invoking this allocation-producing function.
pub fn process_panel(rows: &[ProcessPanelRow]) -> Result<PanelModel, ShellError> {
    PanelModel::new(
        "processes",
        vec![
            "pid".to_owned(),
            "ppid".to_owned(),
            "state".to_owned(),
            "command".to_owned(),
        ],
        rows.iter()
            .map(|row| {
                vec![
                    row.pid.to_string(),
                    row.parent_pid
                        .map_or_else(|| "-".to_owned(), |pid| pid.to_string()),
                    row.state.clone(),
                    row.command.clone(),
                ]
            })
            .collect(),
        "no processes were reported",
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// One ordered typed value retained by a [`LiveBuffer`].
pub struct LiveSample {
    /// Producer-assigned sequence identity; the buffer stores it without enforcing order.
    pub sequence: u64,
    /// Structured sample value.
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
/// Immutable serializable view of a live sample buffer.
pub struct LiveSnapshot {
    /// Maximum sample count retained by the source buffer.
    pub capacity: usize,
    /// Number of oldest samples evicted since buffer creation.
    pub dropped: u64,
    /// Whether the source buffer permanently stopped accepting samples.
    pub cancelled: bool,
    /// Retained samples from oldest to newest, never exceeding [`Self::capacity`].
    pub samples: Vec<LiveSample>,
}

#[derive(Debug)]
/// Fixed-capacity, cancellation-aware retention buffer for live panel values.
///
/// Capacity is restricted to `1..=256`. Once cancelled, the buffer remains
/// cancelled and rejects all subsequent samples without changing retained data.
pub struct LiveBuffer {
    capacity: usize,
    samples: VecDeque<LiveSample>,
    dropped: u64,
    cancelled: bool,
}

impl LiveBuffer {
    /// Construct an empty buffer with a sample-count bound in `1..=256`.
    ///
    /// Returns [`ErrorCode::Validation`] when `capacity` is outside that range.
    pub fn new(capacity: usize) -> Result<Self, ShellError> {
        if !(1..=256).contains(&capacity) {
            return Err(panel_error(
                "live buffer capacity must be between 1 and 256",
            ));
        }
        Ok(Self {
            capacity,
            samples: VecDeque::with_capacity(capacity),
            dropped: 0,
            cancelled: false,
        })
    }

    /// Retain a sample, evicting the oldest when the buffer is full.
    ///
    /// Returns `false` without mutation after cancellation; otherwise returns
    /// `true`. Sequence ordering is the producer's responsibility.
    pub fn push(&mut self, sample: LiveSample) -> bool {
        if self.cancelled {
            return false;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
            self.dropped += 1;
        }
        self.samples.push_back(sample);
        true
    }

    /// Permanently stop accepting new samples while preserving retained history.
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

    /// Clone retained state into an immutable oldest-to-newest snapshot.
    pub fn snapshot(&self) -> LiveSnapshot {
        LiveSnapshot {
            capacity: self.capacity,
            dropped: self.dropped,
            cancelled: self.cancelled,
            samples: self.samples.iter().cloned().collect(),
        }
    }
}

fn render_row(output: &mut String, row: &[String], widths: &[usize]) {
    for (index, cell) in row.iter().enumerate() {
        if index > 0 {
            output.push_str("  ");
        }
        output.push_str(cell);
        if index + 1 < row.len() {
            output.push_str(&" ".repeat(widths[index].saturating_sub(cell.len())));
        }
    }
    output.push('\n');
}

fn reject_panel_text(context: &str, text: &str) -> Result<(), ShellError> {
    reject_terminal_controls(context, text)?;
    if text.contains(['\n', '\t']) {
        return Err(panel_error(format!(
            "{context} must be a single line of typed text"
        )));
    }
    Ok(())
}

fn panel_error(message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Validation, message)
        .with_help("Provide a bounded typed panel model and a safe plain-text fallback")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_rejects_raw_terminal_escapes_and_uses_plain_fallback() {
        let error = PanelModel::new(
            "unsafe",
            vec!["value".to_owned()],
            vec![vec!["\u{1b}[31mred".to_owned()]],
            "plain",
        )
        .unwrap_err();
        assert!(error.message.contains("terminal control"));

        let empty = PanelModel::new(
            "empty",
            vec!["value".to_owned()],
            Vec::new(),
            "nothing to show",
        )
        .unwrap();
        assert_eq!(empty.render_plain(), "nothing to show\n");
    }

    #[test]
    fn directory_panel_renders_hostile_names_as_safe_single_lines() {
        let entry = Entry {
            name: "\u{1b}[2Jüber\nname".to_owned(),
            path: "ignored".into(),
            kind: quirl_core::EntryKind::File,
            size: 1,
            modified_unix_seconds: None,
            hidden: false,
            symlink_target: None,
            readonly: false,
        };
        let panel = directory_panel("root\npath", &[entry]).unwrap();
        assert_eq!(panel.rows[0][0], "\\u{1b}[2Jüber\\nname");
        assert_eq!(panel.title, "directory root\\npath");
    }

    #[test]
    fn render_plain_tolerates_deserialized_rows_wider_than_columns() {
        let panel: PanelModel = serde_json::from_str(
            r#"{
                "title": "wide",
                "columns": ["name"],
                "rows": [["alpha", "extra"]],
                "plain_fallback": "plain"
            }"#,
        )
        .unwrap();
        assert!(panel.validate().is_err());
        assert_eq!(panel.render_plain(), "name\nalpha  extra\n");
    }

    #[test]
    fn live_buffer_is_bounded_and_stops_accepting_after_cancel() {
        let mut buffer = LiveBuffer::new(2).unwrap();
        for sequence in 1..=4 {
            assert!(buffer.push(LiveSample {
                sequence,
                value: Value::from(sequence),
            }));
        }
        let snapshot = buffer.snapshot();
        assert_eq!(snapshot.dropped, 2);
        assert_eq!(snapshot.samples[0].sequence, 3);
        buffer.cancel();
        assert!(!buffer.push(LiveSample {
            sequence: 5,
            value: Value::Null,
        }));
    }
}
