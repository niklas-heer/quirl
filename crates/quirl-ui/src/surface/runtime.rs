//! Bounded immutable runtime snapshots composed into the interactive frame.

use crate::{LiveBuffer, LiveSample, PanelModel};
use quirl_core::{ErrorCode, ShellError, StructuredValue, escape_terminal_line};
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
/// Maximum environment variables retained by the environment inspector.
pub const ENVIRONMENT_ITEMS_MAX: usize = 4_096;
/// Maximum aggregate terminal-safe environment text retained by the inspector.
pub const ENVIRONMENT_RETAINED_BYTES_MAX: usize = 1024 * 1024;
/// Maximum state-valid job actions retained for picker composition.
pub const JOB_ACTION_ITEMS_MAX: usize = 256;
/// Maximum aggregate terminal-safe text retained by job picker snapshots.
pub const JOB_RETAINED_BYTES_MAX: usize = 512 * 1024;
/// Maximum UTF-8 bytes retained for one bottom-bar activity message.
pub const ACTIVITY_MESSAGE_BYTES_MAX: usize = 512;
const LIVE_GENERATIONS_MAX: usize = 4;
const PANEL_BATCHES_MAX: usize = PANEL_QUEUE_UPDATES_MAX / PANEL_COUNT_MAX;
const ENVIRONMENT_FIELD_BYTES_MAX: usize = 6 * 1024;

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

#[derive(Debug, Clone, PartialEq, Eq)]
/// One variable from the private environment inherited by future child processes.
pub struct InteractiveEnvironmentSnapshot {
    /// Platform environment variable name, converted lossily when it is not UTF-8.
    pub name: String,
    /// Platform environment variable value, converted lossily when it is not UTF-8.
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
/// Complete immutable job, data, and environment sources for one prompt generation.
pub struct InteractiveRuntimeSnapshot {
    /// Monotonically increasing composition-root generation.
    pub generation: u64,
    /// Process-owned jobs copied after pruning and refresh.
    pub jobs: Vec<InteractiveJobSnapshot>,
    /// Previously successful typed values retained by the bounded CLI cache.
    pub data: Vec<InteractiveDataSnapshot>,
    /// New session-private variables inherited by future child processes.
    /// `None` retains the environment from the preceding generation.
    pub environment: Option<Vec<InteractiveEnvironmentSnapshot>>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
/// One immutable background-activity publication for the bottom status bar.
pub struct InteractiveActivitySnapshot {
    /// Monotonic provider generation used to reject stale publications.
    pub generation: u64,
    /// Active terminal-independent message, or `None` to clear prior activity.
    pub message: Option<String>,
}

/// Nonblocking source of cached background activity.
///
/// Implementations must never perform network, filesystem, database, or user
/// callback work in these methods. Catalog admission may only schedule bounded
/// work and polling must return an already-published immutable snapshot.
pub trait InteractiveActivityProvider: Send {
    /// Notify the provider after the complete deferred catalog is published.
    fn catalog_admitted(&mut self) -> Result<(), ShellError> {
        Ok(())
    }

    /// Return a newer cached activity snapshot when one is available.
    fn poll_cached(&mut self) -> Result<Option<InteractiveActivitySnapshot>, ShellError>;
}

pub(crate) struct RuntimeSurfaceState {
    snapshot_generation: u64,
    jobs: Vec<InteractiveJobSnapshot>,
    data: Vec<InteractiveDataSnapshot>,
    environment: Vec<InteractiveEnvironmentSnapshot>,
    panel_generation: u64,
    panels: Vec<PanelSlot>,
    panel_focus: usize,
    panel_queue: VecDeque<InteractivePanelBatch>,
    provider: Option<Box<dyn InteractivePanelProvider>>,
    activity_provider: Option<Box<dyn InteractiveActivityProvider>>,
    activity_generation: u64,
    activity: Option<String>,
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
            environment: Vec::new(),
            panel_generation: 0,
            panels: Vec::new(),
            panel_focus: 0,
            panel_queue: VecDeque::with_capacity(PANEL_BATCHES_MAX),
            provider: None,
            activity_provider: None,
            activity_generation: 0,
            activity: None,
            notice: None,
        }
    }

    pub(crate) fn set_provider(&mut self, provider: Box<dyn InteractivePanelProvider>) {
        self.provider = Some(provider);
    }

    pub(crate) fn set_activity_provider(&mut self, provider: Box<dyn InteractiveActivityProvider>) {
        self.activity_provider = Some(provider);
    }

    pub(crate) fn catalog_admitted(&mut self) {
        if let Some(provider) = self.activity_provider.as_mut()
            && let Err(error) = provider.catalog_admitted()
        {
            self.notice = Some(truncate_bytes(
                &escape_terminal_line(&error.message),
                ACTIVITY_MESSAGE_BYTES_MAX,
            ));
        }
    }

    pub(crate) fn install_snapshot(&mut self, mut snapshot: InteractiveRuntimeSnapshot) {
        if snapshot.generation < self.snapshot_generation {
            return;
        }
        self.snapshot_generation = snapshot.generation;
        self.notice = None;
        self.jobs = bounded_jobs(&mut snapshot.jobs, &mut self.notice);
        self.data = bounded_data(&mut snapshot.data, &mut self.notice);
        if let Some(mut environment) = snapshot.environment {
            self.environment = bounded_environment(&mut environment, &mut self.notice);
        }
    }

    pub(crate) fn poll_panels(&mut self) -> bool {
        let result = self
            .provider
            .as_mut()
            .map(|provider| provider.poll_cached());
        match result {
            Some(Ok(Some(batch))) => self.enqueue_panel_batch(batch),
            Some(Err(error)) => {
                self.notice = Some(truncate_bytes(
                    &escape_terminal_line(&error.message),
                    ACTIVITY_MESSAGE_BYTES_MAX,
                ));
                false
            }
            Some(Ok(None)) | None => false,
        }
    }

    pub(crate) fn poll_activity(&mut self) -> bool {
        let result = self
            .activity_provider
            .as_mut()
            .map(|provider| provider.poll_cached());
        match result {
            Some(Ok(Some(snapshot))) => self.install_activity(snapshot),
            Some(Err(error)) => {
                self.notice = Some(truncate_bytes(
                    &escape_terminal_line(&error.message),
                    ACTIVITY_MESSAGE_BYTES_MAX,
                ));
                false
            }
            Some(Ok(None)) | None => false,
        }
    }

    fn install_activity(&mut self, snapshot: InteractiveActivitySnapshot) -> bool {
        if snapshot.generation <= self.activity_generation {
            return false;
        }
        self.activity_generation = snapshot.generation;
        self.activity = snapshot.message.map(|message| {
            truncate_bytes(&escape_terminal_line(&message), ACTIVITY_MESSAGE_BYTES_MAX)
        });
        true
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
            let mut slot = if let Some(index) = previous.iter().position(|slot| slot.id == panel.id)
            {
                previous.swap_remove(index)
            } else {
                let updates = match LiveBuffer::new(LIVE_GENERATIONS_MAX) {
                    Ok(updates) => updates,
                    Err(error) => {
                        self.notice = Some(escape_terminal_line(&error.message));
                        return false;
                    }
                };
                PanelSlot {
                    id: panel.id.clone(),
                    model: panel.model.clone(),
                    updates,
                }
            };
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

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "the increment is reduced modulo a validated non-empty panel list"
    )]
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

    pub(crate) fn activity(&self) -> Option<&str> {
        self.activity.as_deref()
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

    pub(crate) fn environment(&self) -> &[InteractiveEnvironmentSnapshot] {
        &self.environment
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

fn bounded_environment(
    environment: &mut Vec<InteractiveEnvironmentSnapshot>,
    notice: &mut Option<String>,
) -> Vec<InteractiveEnvironmentSnapshot> {
    environment.sort_by(|left, right| left.name.cmp(&right.name));
    let mut retained = Vec::new();
    let mut retained_bytes = 0_usize;
    for mut variable in environment.drain(..) {
        let escaped_name = escape_terminal_line(&variable.name);
        let escaped_value = escape_terminal_line(&variable.value);
        let field_was_truncated = escaped_name.len() > ENVIRONMENT_FIELD_BYTES_MAX
            || escaped_value.len() > ENVIRONMENT_FIELD_BYTES_MAX;
        variable.name = truncate_bytes(&escaped_name, ENVIRONMENT_FIELD_BYTES_MAX);
        variable.value = truncate_bytes(&escaped_value, ENVIRONMENT_FIELD_BYTES_MAX);
        let bytes = variable.name.len().saturating_add(variable.value.len());
        if field_was_truncated
            || retained.len() == ENVIRONMENT_ITEMS_MAX
            || retained_bytes.saturating_add(bytes) > ENVIRONMENT_RETAINED_BYTES_MAX
        {
            *notice = Some(format!(
                "environment inspector reached its {ENVIRONMENT_ITEMS_MAX}-item, {ENVIRONMENT_RETAINED_BYTES_MAX}-byte, or {ENVIRONMENT_FIELD_BYTES_MAX}-byte field limit"
            ));
            if retained.len() == ENVIRONMENT_ITEMS_MAX
                || retained_bytes.saturating_add(bytes) > ENVIRONMENT_RETAINED_BYTES_MAX
            {
                break;
            }
        }
        retained_bytes = retained_bytes.saturating_add(bytes);
        retained.push(variable);
    }
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

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the decrement is guarded by a nonzero offset and stops at a UTF-8 boundary"
)]
fn truncate_bytes(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.get(..end).unwrap_or_default().to_owned()
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

    struct ActivityProvider {
        snapshots: VecDeque<InteractiveActivitySnapshot>,
    }

    struct AdmissionFailureProvider;

    impl InteractiveActivityProvider for AdmissionFailureProvider {
        fn catalog_admitted(&mut self) -> Result<(), ShellError> {
            Err(ShellError::new(
                ErrorCode::Io,
                format!("failed\u{1b}[31m{}", "x".repeat(700)),
            ))
        }

        fn poll_cached(&mut self) -> Result<Option<InteractiveActivitySnapshot>, ShellError> {
            Err(ShellError::new(
                ErrorCode::Io,
                format!("poll failed\u{1b}[31m{}", "y".repeat(700)),
            ))
        }
    }

    impl InteractiveActivityProvider for ActivityProvider {
        fn poll_cached(&mut self) -> Result<Option<InteractiveActivitySnapshot>, ShellError> {
            Ok(self.snapshots.pop_front())
        }
    }

    #[test]
    fn activity_poll_is_live_generation_safe_escaped_and_bounded() {
        let mut state = RuntimeSurfaceState::new();
        state.set_activity_provider(Box::new(ActivityProvider {
            snapshots: VecDeque::from([
                InteractiveActivitySnapshot {
                    generation: 2,
                    message: Some(format!("download\u{1b}[31m{}", "x".repeat(700))),
                },
                InteractiveActivitySnapshot {
                    generation: 1,
                    message: Some("stale".to_owned()),
                },
                InteractiveActivitySnapshot {
                    generation: 3,
                    message: None,
                },
            ]),
        }));
        assert!(state.poll_activity());
        let activity = state.activity().unwrap();
        assert!(!activity.contains('\u{1b}'));
        assert!(activity.len() <= ACTIVITY_MESSAGE_BYTES_MAX);
        assert!(!state.poll_activity());
        assert_ne!(state.activity(), Some("stale"));
        assert!(state.poll_activity());
        assert_eq!(state.activity(), None);
        assert!(!state.poll_activity());
    }

    #[test]
    fn activity_admission_failure_is_a_bounded_nonfatal_notice() {
        let mut state = RuntimeSurfaceState::new();
        state.set_activity_provider(Box::new(AdmissionFailureProvider));
        state.catalog_admitted();
        let notice = state.notice().unwrap();
        assert!(!notice.contains('\u{1b}'));
        assert!(notice.len() <= ACTIVITY_MESSAGE_BYTES_MAX);
        assert!(!state.poll_activity());
        let poll_notice = state.notice().unwrap();
        assert!(!poll_notice.contains('\u{1b}'));
        assert!(poll_notice.len() <= ACTIVITY_MESSAGE_BYTES_MAX);
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
    fn runtime_picker_items_use_bounded_typed_snapshots() {
        let mut state = RuntimeSurfaceState::new();
        let path = std::env::join_paths(["/usr/bin", "/opt/tools"])
            .unwrap()
            .to_string_lossy()
            .into_owned();
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
            environment: Some(vec![
                InteractiveEnvironmentSnapshot {
                    name: "TOKEN".to_owned(),
                    value: "safe\u{1b}[31m".to_owned(),
                },
                InteractiveEnvironmentSnapshot {
                    name: "PATH".to_owned(),
                    value: path.clone(),
                },
            ]),
        });

        let jobs = state.job_items(0);
        assert_eq!(
            jobs.iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            ["fg 7", "fg 8", "bg 8"]
        );
        assert!(
            jobs.iter()
                .all(|item| item.kind == super::super::completion::CompletionKind::Job)
        );
        let data = state.data_items(0);
        assert_eq!(data[0].value, "42");
        assert_eq!(data[0].kind, super::super::completion::CompletionKind::Data);

        let environment = state.environment();
        assert_eq!(environment.len(), 2);
        assert_eq!(environment[0].name, "PATH");
        assert_eq!(environment[0].value, path);
        assert_eq!(environment[1].name, "TOKEN");
        assert!(!environment[1].value.contains('\u{1b}'));
    }

    #[test]
    fn environment_snapshot_truncates_oversized_terminal_text_with_a_notice() {
        let mut state = RuntimeSurfaceState::new();
        state.install_snapshot(InteractiveRuntimeSnapshot {
            generation: 1,
            environment: Some(vec![InteractiveEnvironmentSnapshot {
                name: "OVERSIZED".to_owned(),
                value: format!("\u{1b}{}", "x".repeat(ENVIRONMENT_FIELD_BYTES_MAX + 1)),
            }]),
            ..InteractiveRuntimeSnapshot::default()
        });

        assert_eq!(state.environment.len(), 1);
        assert!(state.environment[0].value.len() <= ENVIRONMENT_FIELD_BYTES_MAX);
        assert!(!state.environment[0].value.contains('\u{1b}'));
        assert!(
            state.notice().is_some_and(|notice| {
                notice.contains(&ENVIRONMENT_FIELD_BYTES_MAX.to_string())
            })
        );
    }
}
