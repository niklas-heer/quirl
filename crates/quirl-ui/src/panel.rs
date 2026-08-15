use quirl_core::{reject_terminal_controls, Entry, ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PanelModel {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub plain_fallback: String,
}

impl PanelModel {
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

pub fn directory_panel(path: &str, entries: &[Entry]) -> Result<PanelModel, ShellError> {
    let rows = entries
        .iter()
        .map(|entry| {
            vec![
                entry.name.clone(),
                format!("{:?}", entry.kind).to_lowercase(),
                entry.size.to_string(),
                entry
                    .modified_unix_seconds
                    .map_or_else(|| "-".to_owned(), |value| value.to_string()),
            ]
        })
        .collect();
    PanelModel::new(
        format!("directory {path}"),
        vec![
            "name".to_owned(),
            "kind".to_owned(),
            "bytes".to_owned(),
            "modified".to_owned(),
        ],
        rows,
        format!("directory {path} has no visible entries"),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessPanelRow {
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub state: String,
    pub command: String,
}

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
pub struct LiveSample {
    pub sequence: u64,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LiveSnapshot {
    pub capacity: usize,
    pub dropped: u64,
    pub cancelled: bool,
    pub samples: Vec<LiveSample>,
}

#[derive(Debug)]
pub struct LiveBuffer {
    capacity: usize,
    samples: VecDeque<LiveSample>,
    dropped: u64,
    cancelled: bool,
}

impl LiveBuffer {
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

    pub fn cancel(&mut self) {
        self.cancelled = true;
    }

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
