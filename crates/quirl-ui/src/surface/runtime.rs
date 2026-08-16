//! Bounded immutable runtime snapshots composed into the interactive frame.

use crate::{LiveBuffer, LiveSample, PanelModel};
use quirl_core::{escape_terminal_line, ErrorCode, ShellError, StructuredValue};
use std::collections::VecDeque;

/// Maximum extension panels retained by the rich surface.
pub const PANEL_COUNT_MAX: usize = 8;
/// Maximum columns accepted in one interactive panel.
pub const PANEL_COLUMNS_MAX: usize = 16;
/// Maximum offscreen and visible rows retained in one interactive panel.
pub const PANEL_ROWS_MAX: usize = 128;
/// Maximum UTF-8 bytes retained in one panel identity, title, heading, or cell.
pub const PANEL_FIELD_BYTES_MAX: usize = 4 * 1024;
/// Maximum aggregate panel text retained in one published generation.
pub const PANEL_GENERATION_BYTES_MAX: usize = 128 * 1024;
/// Maximum individual panel updates represented by the bounded batch queue.
pub const PANEL_QUEUE_UPDATES_MAX: usize = 32;
/// Maximum panel updates committed during one event-loop turn.
pub const PANEL_UPDATES_PER_TURN_MAX: usize = 8;
/// Maximum data rows rendered from the focused panel in one frame.
pub const PANEL_VISIBLE_ROWS_MAX: usize = 6;
/// Maximum successful typed results retained for picker composition.
pub const DATA_ITEMS_MAX: usize = 128;
/// Maximum aggregate encoded bytes retained by the data picker cache.
pub const DATA_RETAINED_BYTES_MAX: usize = 512 * 1024;
/// Maximum Unicode scalar values displayed in one data picker label.
pub const DATA_LABEL_CHARS_MAX: usize = 256;
/// Maximum state-valid job actions retained for picker composition.
pub const JOB_ACTION_ITEMS_MAX: usize = 256;
/// Maximum aggregate terminal-safe text retained by job picker snapshots.
pub const JOB_RETAINED_BYTES_MAX: usize = 512 * 1024;
const LIVE_GENERATIONS_MAX: usize = 4;
const PANEL_BATCHES_MAX: usize = PANEL_QUEUE_UPDATES_MAX / PANEL_COUNT_MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Process state copied from the process-owned job table.
pub enum InteractiveJobStatus {
    /// At least one child is running.
    Running,
    /// Every remaining live child is stopped.
    Stopped,
}

impl InteractiveJobStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Revalidated native command inserted when a job picker item is accepted.
pub enum InteractiveJobAction {
    /// Bring the job into the foreground.
    Foreground,
    /// Continue a stopped job in the background.
    Background,
}

impl InteractiveJobAction {
    pub(crate) const fn command(self) -> &'static str {
        match self {
            Self::Foreground => "fg",
            Self::Background => "bg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Immutable process-owned job metadata safe to retain for one prompt.
pub struct InteractiveJobSnapshot {
    /// Stable non-zero process job identity.
    pub id: u32,
    /// State observed when the snapshot was built.
    pub status: InteractiveJobStatus,
    /// Terminal-safe command description.
    pub command: String,
    /// State-valid commands offered by the picker.
    pub actions: Vec<InteractiveJobAction>,
}

#[derive(Debug, Clone, PartialEq)]
/// One successful typed result retained without rerunning its source.
pub struct InteractiveDataSnapshot {
    /// Monotonic session-local result identity.
    pub id: u64,
    /// Short terminal-safe display label.
    pub label: String,
    /// Optional bounded terminal-safe detail text.
    pub preview: Option<String>,
    /// Exact bounded typed value returned by the data runtime.
    pub value: StructuredValue,
    /// Data expression inserted when selected.
    pub insertion: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
/// Complete immutable job and data sources for one prompt generation.
pub struct InteractiveRuntimeSnapshot {
    /// Monotonically increasing composition-root generation.
    pub generation: u64,
    /// Process-owned jobs copied after pruning and refresh.
    pub jobs: Vec<InteractiveJobSnapshot>,
    /// Previously successful typed values retained by the bounded CLI cache.
    pub data: Vec<InteractiveDataSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Named panel model returned by one completed extension refresh.
pub struct InteractivePanelSnapshot {
    /// Stable provider identity within the active extension generation.
    pub id: String,
    /// Validated terminal-independent panel model.
    pub model: PanelModel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Complete provider set published atomically by the extension host.
pub struct InteractivePanelBatch {
    /// Monotonic cache generation used to reject stale results.
    pub generation: u64,
    /// Provider panels in deterministic registration order.
    pub panels: Vec<InteractivePanelSnapshot>,
}

/// Nonblocking source of completed extension panel snapshots.
///
/// Implementations may schedule bounded asynchronous refresh work, but this
/// method must return immediately from cache and must never execute Lua or a
/// plugin callback on the render thread.
pub trait InteractivePanelProvider: Send {
    /// Return a newer complete snapshot when one is available.
    fn poll_cached(&mut self) -> Result<Option<InteractivePanelBatch>, ShellError>;
}

pub(crate) struct RuntimeSurfaceState {
    snapshot_generation: u64,
    jobs: Vec<InteractiveJobSnapshot>,
    data: Vec<InteractiveDataSnapshot>,
    panel_generation: u64,
    panels: Vec<PanelSlot>,
    panel_focus: usize,
    panel_queue: VecDeque<InteractivePanelBatch>,
    provider: Option<Box<dyn InteractivePanelProvider>>,
    notice: Option<String>,
}

struct PanelSlot {
    id: String,
    model: PanelModel,
    updates: LiveBuffer,
}

impl RuntimeSurfaceState {
    pub(crate) fn new() -> Self {
        Self {
            snapshot_generation: 0,
            jobs: Vec::new(),
            data: Vec::new(),
            panel_generation: 0,
            panels: Vec::new(),
            panel_focus: 0,
            panel_queue: VecDeque::with_capacity(PANEL_BATCHES_MAX),
            provider: None,
            notice: None,
        }
    }

    pub(crate) fn set_provider(&mut self, provider: Box<dyn InteractivePanelProvider>) {
        self.provider = Some(provider);
    }

    pub(crate) fn install_snapshot(&mut self, mut snapshot: InteractiveRuntimeSnapshot) {
        if snapshot.generation < self.snapshot_generation {
            return;
        }
        self.snapshot_generation = snapshot.generation;
        self.notice = None;
        self.jobs = bounded_jobs(&mut snapshot.jobs, &mut self.notice);
        self.data = bounded_data(&mut snapshot.data, &mut self.notice);
    }

    pub(crate) fn poll_panels(&mut self) -> bool {
        let result = self
            .provider
            .as_mut()
            .map(|provider| provider.poll_cached());
        match result {
            Some(Ok(Some(batch))) => self.enqueue_panel_batch(batch),
            Some(Err(error)) => {
                self.notice = Some(escape_terminal_line(&error.message));
                false
            }
            Some(Ok(None)) | None => false,
        }
    }

    fn enqueue_panel_batch(&mut self, batch: InteractivePanelBatch) -> bool {
        if batch.generation <= self.panel_generation
            || self
                .panel_queue
                .back()
                .is_some_and(|queued| batch.generation <= queued.generation)
        {
            return false;
        }
        if let Err(error) = validate_panel_batch(&batch) {
            self.notice = Some(escape_terminal_line(&error.message));
            return false;
        }
        if self.panel_queue.len() == PANEL_BATCHES_MAX {
            self.panel_queue.pop_front();
            self.notice = Some(format!(
                "panel update queue reached its {PANEL_QUEUE_UPDATES_MAX}-update limit"
            ));
        }
        self.panel_queue.push_back(batch);
        self.apply_panel_batch()
    }

    fn apply_panel_batch(&mut self) -> bool {
        let Some(batch) = self.panel_queue.pop_front() else {
            return false;
        };
        let focused_id = self
            .panels
            .get(self.panel_focus)
            .map(|slot| slot.id.clone());
        let mut previous = std::mem::take(&mut self.panels);
        let mut next = Vec::with_capacity(batch.panels.len());
        for panel in batch.panels.into_iter().take(PANEL_UPDATES_PER_TURN_MAX) {
            let mut slot = previous
                .iter()
                .position(|slot| slot.id == panel.id)
                .map(|index| previous.swap_remove(index))
                .unwrap_or_else(|| PanelSlot {
                    id: panel.id.clone(),
                    model: panel.model.clone(),
                    updates: LiveBuffer::new(LIVE_GENERATIONS_MAX)
                        .unwrap_or_else(|_| unreachable!("fixed live-buffer capacity is valid")),
                });
            let encoded = serde_json::to_value(&panel.model).unwrap_or(serde_json::Value::Null);
            let _ = slot.updates.push(LiveSample {
                sequence: batch.generation,
                value: encoded,
            });
            slot.model = panel.model;
            next.push(slot);
        }
        self.panels = next;
        self.panel_generation = batch.generation;
        self.panel_focus = focused_id
            .and_then(|id| self.panels.iter().position(|slot| slot.id == id))
            .unwrap_or(0)
            .min(self.panels.len().saturating_sub(1));
        true
    }

    pub(crate) fn cycle_panel_focus(&mut self) -> bool {
        if self.panels.len() < 2 {
            return false;
        }
        self.panel_focus = self.panel_focus.saturating_add(1) % self.panels.len();
        true
    }

    pub(crate) fn focused_panel(&self) -> Option<(&str, &PanelModel)> {
        self.panels
            .get(self.panel_focus)
            .map(|slot| (slot.id.as_str(), &slot.model))
    }

    pub(crate) fn panel_count(&self) -> usize {
        self.panels.len()
    }

    pub(crate) fn panel_focus_position(&self) -> usize {
        self.panel_focus.saturating_add(1).min(self.panels.len())
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    pub(crate) fn job_items(&self, replace_end: usize) -> Vec<super::completion::CompletionItem> {
        let mut items = Vec::new();
        for job in &self.jobs {
            for action in &job.actions {
                if items.len() == JOB_ACTION_ITEMS_MAX {
                    return items;
                }
                let command = format!("{} {}", action.command(), job.id);
                items.push(super::completion::CompletionItem {
                    value: command.clone(),
                    display: format!("[{}] {} {}", job.id, job.status.label(), job.command),
                    summary: format!("{} job {}", action.command(), job.id),
                    detail: format!(
                        "snapshot status: {}\ncommand: {}\naction: {}",
                        job.status.label(),
                        job.command,
                        command
                    ),
                    replace_start: 0,
                    replace_end,
                    match_indices: Vec::new(),
                    kind: super::completion::CompletionKind::Job,
                    source: "jobs",
                    trust: "process snapshot",
                });
            }
        }
        items
    }

    pub(crate) fn data_items(&self, replace_end: usize) -> Vec<super::completion::CompletionItem> {
        self.data
            .iter()
            .take(DATA_ITEMS_MAX)
            .map(|item| super::completion::CompletionItem {
                value: item.insertion.clone(),
                display: item.label.clone(),
                summary: "cached typed result".to_owned(),
                detail: item.preview.clone().unwrap_or_else(|| item.label.clone()),
                replace_start: 0,
                replace_end,
                match_indices: Vec::new(),
                kind: super::completion::CompletionKind::Data,
                source: "data cache",
                trust: "validated",
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn live_generation_count(&self) -> usize {
        self.panels
            .first()
            .map_or(0, |slot| slot.updates.snapshot().samples.len())
    }

    #[cfg(test)]
    pub(crate) fn install_panel_batch(&mut self, batch: InteractivePanelBatch) -> bool {
        self.enqueue_panel_batch(batch)
    }
}

impl Default for RuntimeSurfaceState {
    fn default() -> Self {
        Self::new()
    }
}

fn bounded_jobs(
    jobs: &mut Vec<InteractiveJobSnapshot>,
    notice: &mut Option<String>,
) -> Vec<InteractiveJobSnapshot> {
    let mut retained = Vec::new();
    let mut retained_bytes = 0_usize;
    let mut actions = 0_usize;
    for mut job in jobs.drain(..) {
        if job.id == 0 {
            continue;
        }
        job.command = truncate_chars(&escape_terminal_line(&job.command), DATA_LABEL_CHARS_MAX);
        job.actions.retain(|action| {
            job.status == InteractiveJobStatus::Stopped
                || *action != InteractiveJobAction::Background
        });
        job.actions.truncate(3);
        let bytes = job.command.len().saturating_add(64);
        if actions.saturating_add(job.actions.len()) > JOB_ACTION_ITEMS_MAX
            || retained_bytes.saturating_add(bytes) > JOB_RETAINED_BYTES_MAX
        {
            *notice = Some(format!(
                "job picker snapshot reached its {JOB_ACTION_ITEMS_MAX}-item or {JOB_RETAINED_BYTES_MAX}-byte limit"
            ));
            break;
        }
        actions = actions.saturating_add(job.actions.len());
        retained_bytes = retained_bytes.saturating_add(bytes);
        retained.push(job);
    }
    retained
}

fn bounded_data(
    data: &mut Vec<InteractiveDataSnapshot>,
    notice: &mut Option<String>,
) -> Vec<InteractiveDataSnapshot> {
    let mut retained = Vec::new();
    let mut retained_bytes = 0_usize;
    for mut item in data.drain(..).rev().take(DATA_ITEMS_MAX) {
        if item.value.validate().is_err() {
            continue;
        }
        item.label = truncate_chars(&escape_terminal_line(&item.label), DATA_LABEL_CHARS_MAX);
        item.preview = item
            .preview
            .map(|value| truncate_chars(&escape_terminal_line(&value), PANEL_FIELD_BYTES_MAX));
        item.insertion = truncate_bytes(&item.insertion, PANEL_FIELD_BYTES_MAX);
        let value_bytes = serde_json::to_vec(&item.value)
            .map(|value| value.len())
            .unwrap_or(DATA_RETAINED_BYTES_MAX.saturating_add(1));
        let bytes = value_bytes
            .saturating_add(item.label.len())
            .saturating_add(item.preview.as_ref().map_or(0, String::len))
            .saturating_add(item.insertion.len());
        if retained_bytes.saturating_add(bytes) > DATA_RETAINED_BYTES_MAX {
            *notice = Some(format!(
                "data picker cache reached its {DATA_RETAINED_BYTES_MAX}-byte limit"
            ));
            break;
        }
        retained_bytes = retained_bytes.saturating_add(bytes);
        retained.push(item);
    }
    retained.reverse();
    retained
}

fn validate_panel_batch(batch: &InteractivePanelBatch) -> Result<(), ShellError> {
    if batch.panels.len() > PANEL_COUNT_MAX {
        return Err(panel_limit_error(
            "panel count",
            PANEL_COUNT_MAX,
            batch.panels.len(),
        ));
    }
    let mut retained_bytes = 0_usize;
    for panel in &batch.panels {
        panel.model.validate()?;
        if panel.id.len() > PANEL_FIELD_BYTES_MAX {
            return Err(panel_limit_error(
                "panel identity bytes",
                PANEL_FIELD_BYTES_MAX,
                panel.id.len(),
            ));
        }
        if panel.model.columns.len() > PANEL_COLUMNS_MAX {
            return Err(panel_limit_error(
                "panel columns",
                PANEL_COLUMNS_MAX,
                panel.model.columns.len(),
            ));
        }
        if panel.model.rows.len() > PANEL_ROWS_MAX {
            return Err(panel_limit_error(
                "panel rows",
                PANEL_ROWS_MAX,
                panel.model.rows.len(),
            ));
        }
        for field in std::iter::once(&panel.id)
            .chain(std::iter::once(&panel.model.title))
            .chain(panel.model.columns.iter())
            .chain(panel.model.rows.iter().flatten())
            .chain(std::iter::once(&panel.model.plain_fallback))
        {
            if field.len() > PANEL_FIELD_BYTES_MAX {
                return Err(panel_limit_error(
                    "panel field bytes",
                    PANEL_FIELD_BYTES_MAX,
                    field.len(),
                ));
            }
            retained_bytes = retained_bytes.saturating_add(field.len());
        }
    }
    if retained_bytes > PANEL_GENERATION_BYTES_MAX {
        return Err(panel_limit_error(
            "panel generation bytes",
            PANEL_GENERATION_BYTES_MAX,
            retained_bytes,
        ));
    }
    Ok(())
}

fn panel_limit_error(description: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("interactive {description} exceeded its configured limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Reduce cached panel output before the next asynchronous refresh")
}

fn truncate_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn truncate_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel(title: &str) -> PanelModel {
        PanelModel::new(
            title,
            vec!["value".to_owned()],
            vec![vec![title.to_owned()]],
            "empty",
        )
        .unwrap()
    }

    #[test]
    fn stale_panel_generations_are_ignored_and_live_history_is_bounded() {
        let mut state = RuntimeSurfaceState::new();
        for generation in 1..=6 {
            assert!(state.enqueue_panel_batch(InteractivePanelBatch {
                generation,
                panels: vec![InteractivePanelSnapshot {
                    id: "demo".to_owned(),
                    model: panel(&generation.to_string()),
                }],
            }));
        }
        assert_eq!(state.live_generation_count(), LIVE_GENERATIONS_MAX);
        assert!(!state.enqueue_panel_batch(InteractivePanelBatch {
            generation: 5,
            panels: Vec::new(),
        }));
        assert_eq!(state.focused_panel().unwrap().1.title, "6");
    }

    #[test]
    fn provider_removal_replaces_the_complete_panel_set() {
        let mut state = RuntimeSurfaceState::new();
        assert!(state.enqueue_panel_batch(InteractivePanelBatch {
            generation: 1,
            panels: vec![InteractivePanelSnapshot {
                id: "demo".to_owned(),
                model: panel("ready"),
            }],
        }));
        assert!(state.enqueue_panel_batch(InteractivePanelBatch {
            generation: 2,
            panels: Vec::new(),
        }));
        assert!(state.focused_panel().is_none());
    }

    #[test]
    fn panel_shape_and_bytes_fail_before_retention() {
        let oversized = InteractivePanelBatch {
            generation: 1,
            panels: vec![InteractivePanelSnapshot {
                id: "demo".to_owned(),
                model: PanelModel::new(
                    "demo",
                    vec!["value".to_owned()],
                    vec![vec!["x".repeat(PANEL_FIELD_BYTES_MAX + 1)]],
                    "empty",
                )
                .unwrap(),
            }],
        };
        assert_eq!(
            validate_panel_batch(&oversized).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn job_and_data_picker_items_use_real_snapshot_kinds_and_revalidated_commands() {
        let mut state = RuntimeSurfaceState::new();
        state.install_snapshot(InteractiveRuntimeSnapshot {
            generation: 1,
            jobs: vec![
                InteractiveJobSnapshot {
                    id: 7,
                    status: InteractiveJobStatus::Running,
                    command: "build all".to_owned(),
                    actions: vec![
                        InteractiveJobAction::Foreground,
                        InteractiveJobAction::Background,
                    ],
                },
                InteractiveJobSnapshot {
                    id: 8,
                    status: InteractiveJobStatus::Stopped,
                    command: "deploy".to_owned(),
                    actions: vec![
                        InteractiveJobAction::Foreground,
                        InteractiveJobAction::Background,
                    ],
                },
            ],
            data: vec![InteractiveDataSnapshot {
                id: 1,
                label: "42".to_owned(),
                preview: Some("typed integer".to_owned()),
                value: StructuredValue::Int(42),
                insertion: "42".to_owned(),
            }],
        });

        let jobs = state.job_items(0);
        assert_eq!(
            jobs.iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            ["fg 7", "fg 8", "bg 8"]
        );
        assert!(jobs
            .iter()
            .all(|item| item.kind == super::super::completion::CompletionKind::Job));
        let data = state.data_items(0);
        assert_eq!(data[0].value, "42");
        assert_eq!(data[0].kind, super::super::completion::CompletionKind::Data);
    }
}
