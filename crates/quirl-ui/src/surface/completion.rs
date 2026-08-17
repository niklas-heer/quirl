use super::super::{
    extension_replacement_is_valid, CompletionWorker, ExtensionCompleter, ExtensionSuggestion,
};
use quirl_catalog::{
    Catalog, CommandSpec, Completion, CompletionCancellation, CompletionOutcome, CompletionRequest,
    Effect, Provenance, Trust, COMPLETION_PROTOCOL_VERSION, MAX_COMPLETION_DEADLINE_MS,
    MAX_COMPLETION_RESULTS,
};
use quirl_core::ShellError;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
};

const COMPLETION_ITEMS_MAX: usize = MAX_COMPLETION_RESULTS;
const COMPLETION_ITEM_BYTES_MAX: usize = 16 * 1_024;
const COMPLETION_RETAINED_BYTES_MAX: usize = 2 * 1_024 * 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Command,
    Flag,
    Path,
    Value,
    History,
    Job,
    Data,
}

impl CompletionKind {
    pub const fn glyph(self, unicode: bool) -> &'static str {
        match (self, unicode) {
            (Self::Command, true) => "λ",
            (Self::Flag, true) => "–",
            (Self::Path, true) => "/",
            (Self::Value, true) => "≡",
            (Self::History, true) => "↺",
            (Self::Job, true) => "◉",
            (Self::Data, true) => "◆",
            (Self::Command, false) => "c",
            (Self::Flag, false) => "f",
            (Self::Path, false) => "p",
            (Self::Value, false) => "v",
            (Self::History, false) => "h",
            (Self::Job, false) => "j",
            (Self::Data, false) => "d",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub value: String,
    pub display: String,
    pub summary: String,
    pub detail: String,
    pub replace_start: usize,
    pub replace_end: usize,
    pub match_indices: Vec<usize>,
    pub kind: CompletionKind,
    pub source: &'static str,
    pub trust: &'static str,
}

#[derive(Debug, Clone)]
struct ExtensionRequest {
    request_id: u64,
    line: String,
    cursor: usize,
}

#[derive(Debug)]
struct ExtensionResponse {
    request_id: u64,
    items: Vec<ExtensionSuggestion>,
}

#[derive(Default)]
struct ExtensionQueue {
    pending: Option<ExtensionRequest>,
    shutdown: bool,
}

struct ExtensionWorker {
    queue: Arc<(Mutex<ExtensionQueue>, Condvar)>,
    response: Arc<Mutex<Option<ExtensionResponse>>>,
    latest_request_id: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

/// Latest-query-only adapter from the local AI intent provider to rich
/// completion items. The worker is created lazily and owns at most one queued
/// query plus one completed response.
#[derive(Default)]
pub(super) struct IntentCompletionState {
    source: Option<Box<dyn ExtensionCompleter + Send>>,
    worker: Option<ExtensionWorker>,
    request_id: u64,
}

impl IntentCompletionState {
    pub fn install(&mut self, source: Box<dyn ExtensionCompleter + Send>) {
        self.cancel();
        self.worker = None;
        self.source = Some(source);
    }

    pub fn request(&mut self, line: &str, cursor: usize) {
        self.cancel();
        if line.trim().len() < 3 || line.len() > quirl_catalog::MAX_COMPLETION_QUERY_BYTES {
            return;
        }
        if self.worker.is_none() {
            self.worker = self.source.take().map(ExtensionWorker::new);
        }
        let Some(worker) = &self.worker else {
            return;
        };
        self.request_id = self.request_id.saturating_add(1);
        worker.submit(ExtensionRequest {
            request_id: self.request_id,
            line: line.to_owned(),
            cursor,
        });
    }

    pub fn cancel(&mut self) {
        if self.request_id > 0 {
            if let Some(worker) = &self.worker {
                worker.cancel(self.request_id);
            }
        }
    }

    pub fn poll(&self, line: &str, _cursor: usize) -> Option<Vec<CompletionItem>> {
        let suggestions = self.worker.as_ref()?.try_recv_latest(self.request_id)?;
        let items = suggestions
            .into_iter()
            .filter(|item| extension_replacement_is_valid(line, item))
            .map(|item| CompletionItem {
                kind: infer_kind(&item.value),
                value: item.value,
                display: item.display,
                summary: item.summary,
                detail: item.detail,
                replace_start: item.replace_start,
                replace_end: item.replace_end,
                match_indices: Vec::new(),
                source: "ai",
                trust: "local",
            })
            .collect();
        Some(bounded_items(items))
    }
}

impl ExtensionWorker {
    fn new(mut completer: Box<dyn ExtensionCompleter + Send>) -> Self {
        let queue = Arc::new((Mutex::new(ExtensionQueue::default()), Condvar::new()));
        let worker_queue = Arc::clone(&queue);
        let response = Arc::new(Mutex::new(None));
        let worker_response = Arc::clone(&response);
        let latest_request_id = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_request_id);
        let worker = thread::spawn(move || loop {
            let request = {
                let (lock, ready) = &*worker_queue;
                let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                while state.pending.is_none() && !state.shutdown {
                    state = ready
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                if state.shutdown {
                    return;
                }
                state.pending.take()
            };
            let Some(request) = request else {
                continue;
            };
            let items = completer
                .complete(&request.line, request.cursor)
                .into_iter()
                .take(COMPLETION_ITEMS_MAX)
                .filter(|item| extension_replacement_is_valid(&request.line, item))
                .collect();
            if worker_latest.load(Ordering::Acquire) == request.request_id {
                let mut slot = worker_response
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if worker_latest.load(Ordering::Acquire) == request.request_id {
                    *slot = Some(ExtensionResponse {
                        request_id: request.request_id,
                        items,
                    });
                }
            }
        });
        Self {
            queue,
            response,
            latest_request_id,
            worker: Some(worker),
        }
    }

    fn submit(&self, request: ExtensionRequest) {
        self.latest_request_id
            .store(request.request_id, Ordering::Release);
        let (lock, ready) = &*self.queue;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // One replaceable slot bounds queued work while preserving the newest edit.
        state.pending = Some(request);
        ready.notify_one();
    }

    fn cancel(&self, request_id: u64) {
        let _ = self.latest_request_id.compare_exchange(
            request_id,
            request_id.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        let (lock, _) = &*self.queue;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state
            .pending
            .as_ref()
            .is_some_and(|request| request.request_id == request_id)
        {
            state.pending = None;
        }
        drop(state);
        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if response
            .as_ref()
            .is_some_and(|response| response.request_id == request_id)
        {
            response.take();
        }
    }

    fn try_recv_latest(&self, request_id: u64) -> Option<Vec<ExtensionSuggestion>> {
        let mut response = self
            .response
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        response.take().and_then(|response| {
            (response.request_id == request_id
                && self.latest_request_id.load(Ordering::Acquire) == request_id)
                .then_some(response.items)
        })
    }
}

impl Drop for ExtensionWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.queue;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.shutdown = true;
        state.pending = None;
        ready.notify_one();
        drop(state);
        // Completion callbacks are host/plugin code. Once one is executing,
        // synchronous cancellation cannot make joining safe for terminal
        // teardown. All worker-owned state is Arc-backed, so detach after
        // recording shutdown and let a bounded callback finish independently.
        self.worker.take();
    }
}

pub struct CompletionState {
    worker: Option<CompletionWorker>,
    catalog: Option<Arc<Catalog>>,
    extension_source: Option<Box<dyn ExtensionCompleter + Send>>,
    extension_worker: Option<ExtensionWorker>,
    extension_pending: Option<Vec<ExtensionSuggestion>>,
    catalog_ready: bool,
    extension_ready: bool,
    request_id: u64,
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub open: bool,
    pub streaming: bool,
    pub automatic: bool,
    pub source_label: &'static str,
    resource_notice: Option<String>,
}

impl CompletionState {
    pub fn new(
        catalog: impl Into<Arc<Catalog>>,
        extensions: Option<Box<dyn ExtensionCompleter + Send>>,
    ) -> Self {
        Self {
            worker: None,
            catalog: Some(catalog.into()),
            extension_source: extensions,
            extension_worker: None,
            extension_pending: None,
            catalog_ready: false,
            extension_ready: false,
            request_id: 0,
            items: Vec::new(),
            selected: 0,
            open: false,
            streaming: false,
            automatic: false,
            source_label: "catalog",
            resource_notice: None,
        }
    }

    pub fn unpublished(extensions: Option<Box<dyn ExtensionCompleter + Send>>) -> Self {
        Self {
            worker: None,
            catalog: None,
            extension_source: extensions,
            extension_worker: None,
            extension_pending: None,
            catalog_ready: false,
            extension_ready: false,
            request_id: 0,
            items: Vec::new(),
            selected: 0,
            open: false,
            streaming: false,
            automatic: false,
            source_label: "catalog",
            resource_notice: None,
        }
    }

    pub fn publish_catalog(&mut self, catalog: Arc<Catalog>) {
        assert!(
            self.catalog.is_none(),
            "catalog publication must be one-shot"
        );
        assert!(
            self.worker.is_none(),
            "completion cannot start before catalog publication"
        );
        self.catalog = Some(catalog);
    }

    #[cfg(test)]
    pub(super) fn published_catalog(&self) -> Option<&Arc<Catalog>> {
        self.catalog.as_ref()
    }

    pub fn request(&mut self, line: &str, cursor: usize) -> Result<(), ShellError> {
        self.request_with_presentation(line, cursor, false)
    }

    pub fn request_automatic(&mut self, line: &str, cursor: usize) -> Result<(), ShellError> {
        self.request_with_presentation(line, cursor, true)
    }

    fn request_with_presentation(
        &mut self,
        line: &str,
        cursor: usize,
        automatic: bool,
    ) -> Result<(), ShellError> {
        if line.len() > quirl_catalog::MAX_COMPLETION_QUERY_BYTES {
            self.cancel_for_edit();
            self.resource_notice = Some(format!(
                "completion limited to {} query bytes; editing remains available",
                quirl_catalog::MAX_COMPLETION_QUERY_BYTES
            ));
            return Ok(());
        }
        self.start_workers()?;
        self.resource_notice = None;
        if self.request_id > 0 {
            if let Some(worker) = &mut self.worker {
                worker.cancel(CompletionCancellation {
                    protocol_version: COMPLETION_PROTOCOL_VERSION,
                    request_id: self.request_id,
                })?;
            }
        }
        self.request_id = self.request_id.saturating_add(1);
        self.items.clear();
        self.extension_pending = None;
        self.catalog_ready = false;
        self.extension_ready = self.extension_worker.is_none();
        self.selected = 0;
        self.open = true;
        self.streaming = true;
        self.automatic = automatic;
        self.source_label = "catalog";
        if let Some(worker) = &mut self.worker {
            worker.submit(CompletionRequest {
                protocol_version: COMPLETION_PROTOCOL_VERSION,
                request_id: self.request_id,
                line: line.to_owned(),
                cursor,
                limit: MAX_COMPLETION_RESULTS,
                deadline_ms: MAX_COMPLETION_DEADLINE_MS,
            })?;
        }
        if let Some(worker) = &self.extension_worker {
            worker.submit(ExtensionRequest {
                request_id: self.request_id,
                line: line.to_owned(),
                cursor,
            });
        }
        Ok(())
    }

    pub fn cancel_for_edit(&mut self) {
        if self.request_id > 0 {
            if let Some(worker) = &mut self.worker {
                let _ = worker.cancel(CompletionCancellation {
                    protocol_version: COMPLETION_PROTOCOL_VERSION,
                    request_id: self.request_id,
                });
            }
            if let Some(worker) = &self.extension_worker {
                worker.cancel(self.request_id);
            }
        }
        self.extension_pending = None;
        self.catalog_ready = false;
        self.extension_ready = self.extension_worker.is_none();
        self.resource_notice = None;
        self.dismiss();
    }

    pub fn resource_notice(&self) -> Option<&str> {
        self.resource_notice.as_deref()
    }

    pub fn poll(&mut self, _line: &str, _cursor: usize) -> bool {
        let selected_value = self.selected_item().map(|item| item.value.clone());
        let mut changed = false;
        if let Some(response) = self
            .worker
            .as_ref()
            .and_then(CompletionWorker::try_recv_latest)
        {
            self.items = bounded_items(match response.outcome {
                CompletionOutcome::Ready { items } => items
                    .into_iter()
                    .filter_map(|item| {
                        self.catalog
                            .as_deref()
                            .map(|catalog| catalog_item(catalog, item))
                    })
                    .collect(),
                CompletionOutcome::Cancelled | CompletionOutcome::DeadlineExceeded => Vec::new(),
            });
            self.catalog_ready = true;
            if let Some(extension_items) = self.extension_pending.take() {
                if !extension_items.is_empty() {
                    self.source_label = "catalog + plugins";
                    merge_extension_items(&mut self.items, extension_items);
                }
            }
            changed = true;
        }
        if let Some(extension_items) = self
            .extension_worker
            .as_ref()
            .and_then(|worker| worker.try_recv_latest(self.request_id))
        {
            self.extension_ready = true;
            if self.catalog_ready {
                if !extension_items.is_empty() {
                    self.source_label = "catalog + plugins";
                    merge_extension_items(&mut self.items, extension_items);
                }
            } else {
                self.extension_pending = Some(extension_items);
            }
            changed = true;
        }
        if !changed {
            return false;
        }
        self.selected = selected_value
            .and_then(|value| self.items.iter().position(|item| item.value == value))
            .unwrap_or(0)
            .min(self.items.len().saturating_sub(1));
        self.streaming = !self.catalog_ready || !self.extension_ready;
        self.open = !self.items.is_empty() || self.streaming;
        true
    }

    fn start_workers(&mut self) -> Result<(), ShellError> {
        // The empty first frame cannot consume completion results. Defer both
        // worker threads until the first actual request so process startup and
        // first paint do not pay for idle control-plane resources.
        if self.worker.is_none() {
            let catalog = self.catalog.as_ref().ok_or_else(|| {
                ShellError::new(
                    quirl_core::ErrorCode::Io,
                    "interactive catalog publication is incomplete",
                )
                .with_help("Restart Quirl before requesting completion again")
            })?;
            self.worker = Some(CompletionWorker::new(catalog.as_ref().clone()));
        }
        if self.extension_worker.is_none() {
            self.extension_worker = self.extension_source.take().map(ExtensionWorker::new);
        }
        self.extension_ready = self.extension_worker.is_none();
        Ok(())
    }

    pub fn next(&mut self) {
        self.automatic = false;
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub fn previous(&mut self) {
        self.automatic = false;
        if !self.items.is_empty() {
            self.selected = self
                .selected
                .checked_sub(1)
                .unwrap_or_else(|| self.items.len().saturating_sub(1));
        }
    }

    pub fn selected_item(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    pub fn accepts_enter(&self) -> bool {
        self.open && !self.automatic
    }

    pub fn dismiss(&mut self) {
        self.items.clear();
        self.selected = 0;
        self.open = false;
        self.streaming = false;
        self.automatic = false;
    }

    #[cfg(test)]
    pub fn open_manual(&mut self, items: Vec<CompletionItem>, source_label: &'static str) {
        self.items = bounded_items(items);
        self.selected = 0;
        self.open = !self.items.is_empty();
        self.streaming = false;
        self.automatic = false;
        self.source_label = source_label;
    }

    pub fn show_picker_results(&mut self, items: Vec<CompletionItem>, source_label: &'static str) {
        self.items = bounded_items(items);
        self.selected = 0;
        // A picker with no matches must stay visible so its query remains
        // editable and the user can recover without dismissing it.
        self.open = true;
        self.streaming = false;
        self.automatic = false;
        self.source_label = source_label;
    }
}

fn catalog_item(catalog: &Catalog, item: Completion) -> CompletionItem {
    let command = catalog_command(catalog, &item);
    let is_command = command.is_some_and(|command| {
        command.path == item.value || command.aliases.iter().any(|alias| alias == &item.value)
    });
    let kind = if is_command {
        CompletionKind::Command
    } else {
        infer_kind(&item.value)
    };
    let (source, trust) = command.map_or(("catalog", "validated"), |command| {
        (
            provenance_label(command.provenance.source),
            trust_label(command.provenance.trust),
        )
    });
    let detail = if is_command {
        command.map_or(item.detail.clone(), command_capability_detail)
    } else {
        item.detail.clone()
    };
    CompletionItem {
        value: item.value,
        display: item.display,
        summary: item.summary,
        detail,
        replace_start: item.replace_start,
        replace_end: item.replace_end,
        match_indices: item.match_indices,
        kind,
        source,
        trust,
    }
}

fn merge_extension_items(items: &mut Vec<CompletionItem>, extension: Vec<ExtensionSuggestion>) {
    let mut retained_bytes = items.iter().map(completion_item_bytes).sum::<usize>();
    for item in extension
        .into_iter()
        .take(COMPLETION_ITEMS_MAX.saturating_sub(items.len()))
    {
        if items.iter().any(|existing| existing.value == item.value) {
            continue;
        }
        let kind = infer_kind(&item.value);
        let item = CompletionItem {
            value: item.value,
            display: item.display,
            summary: item.summary,
            detail: item.detail,
            replace_start: item.replace_start,
            replace_end: item.replace_end,
            match_indices: Vec::new(),
            kind,
            source: "plugin",
            trust: "trusted",
        };
        let item_bytes = completion_item_bytes(&item);
        if item_bytes > COMPLETION_ITEM_BYTES_MAX
            || retained_bytes.saturating_add(item_bytes) > COMPLETION_RETAINED_BYTES_MAX
        {
            continue;
        }
        retained_bytes = retained_bytes.saturating_add(item_bytes);
        items.push(item);
    }
}

fn bounded_items(items: Vec<CompletionItem>) -> Vec<CompletionItem> {
    let mut bounded = Vec::with_capacity(items.len().min(COMPLETION_ITEMS_MAX));
    let mut retained_bytes = 0_usize;
    for item in items.into_iter().take(COMPLETION_ITEMS_MAX) {
        let item_bytes = completion_item_bytes(&item);
        if item_bytes > COMPLETION_ITEM_BYTES_MAX
            || retained_bytes.saturating_add(item_bytes) > COMPLETION_RETAINED_BYTES_MAX
        {
            continue;
        }
        retained_bytes = retained_bytes.saturating_add(item_bytes);
        bounded.push(item);
    }
    bounded
}

fn completion_item_bytes(item: &CompletionItem) -> usize {
    item.value
        .len()
        .saturating_add(item.display.len())
        .saturating_add(item.summary.len())
        .saturating_add(item.detail.len())
}

fn catalog_command<'catalog>(
    catalog: &'catalog Catalog,
    item: &Completion,
) -> Option<&'catalog CommandSpec> {
    catalog.commands.iter().find(|command| {
        command.path == item.value
            || command.aliases.iter().any(|alias| alias == &item.value)
            || command.signature == item.display
            || command.signature == item.detail
            || item
                .detail
                .strip_prefix(&command.signature)
                .is_some_and(|suffix| suffix.starts_with(" · "))
    })
}

fn command_capability_detail(command: &CommandSpec) -> String {
    let effects = if command.effects.is_empty() {
        "none".to_owned()
    } else {
        command
            .effects
            .iter()
            .map(|effect| match effect {
                Effect::ReadFilesystem => "read filesystem",
                Effect::WriteFilesystem => "write filesystem",
                Effect::SpawnProcess => "spawn process",
                Effect::ChangeDirectory => "change directory",
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let streaming = if command.io.streaming {
        "streaming"
    } else {
        "bounded result"
    };
    format!(
        "{} Capabilities: {} input -> {} output ({streaming}); effects: {effects}.",
        command.details, command.io.input, command.io.output
    )
}

const fn provenance_label(source: Provenance) -> &'static str {
    match source {
        Provenance::Builtin => "builtin",
        Provenance::External => "external",
        Provenance::Lua => "lua",
        Provenance::Plugin => "plugin",
        Provenance::Fish => "fish-import",
        Provenance::Bash => "bash-import",
        Provenance::Zsh => "zsh-import",
        Provenance::Help => "help-import",
        Provenance::Man => "man-import",
    }
}

const fn trust_label(trust: Trust) -> &'static str {
    match trust {
        Trust::Builtin => "builtin",
        Trust::Trusted => "trusted",
        Trust::Declared => "declared",
        Trust::Imported => "imported",
        Trust::Heuristic => "heuristic",
    }
}

fn infer_kind(value: &str) -> CompletionKind {
    if value.starts_with('-') {
        CompletionKind::Flag
    } else if value.contains('/') || value.starts_with('~') || value.starts_with('.') {
        CompletionKind::Path
    } else if value.contains(' ') {
        CompletionKind::Command
    } else {
        CompletionKind::Value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_catalog::Catalog;
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    struct SlowCompleter {
        delay: Duration,
    }

    struct InvalidSpanCompleter;

    impl ExtensionCompleter for InvalidSpanCompleter {
        fn complete(&mut self, _line: &str, _cursor: usize) -> Vec<ExtensionSuggestion> {
            vec![ExtensionSuggestion {
                value: "invalid".to_owned(),
                display: "invalid".to_owned(),
                summary: "invalid".to_owned(),
                detail: "invalid".to_owned(),
                replace_start: 1,
                replace_end: 2,
            }]
        }
    }

    impl ExtensionCompleter for SlowCompleter {
        fn complete(&mut self, line: &str, cursor: usize) -> Vec<ExtensionSuggestion> {
            std::thread::sleep(self.delay);
            vec![ExtensionSuggestion {
                value: format!("plugin-{line}"),
                display: format!("plugin-{line}"),
                summary: "slow plugin".to_owned(),
                detail: "arrived asynchronously".to_owned(),
                replace_start: 0,
                replace_end: cursor,
            }]
        }
    }

    struct GatedCompleter {
        gate: Arc<(Mutex<bool>, Condvar)>,
        started: mpsc::Sender<()>,
        finished: mpsc::Sender<()>,
    }

    impl ExtensionCompleter for GatedCompleter {
        fn complete(&mut self, _line: &str, _cursor: usize) -> Vec<ExtensionSuggestion> {
            let _ = self.started.send(());
            let (lock, ready) = &*self.gate;
            let mut released = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            while !*released {
                released = ready
                    .wait(released)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
            let _ = self.finished.send(());
            Vec::new()
        }
    }

    fn manual_item(value: String) -> CompletionItem {
        CompletionItem {
            display: value.clone(),
            value,
            summary: "test".to_owned(),
            detail: "detail".to_owned(),
            replace_start: 0,
            replace_end: 0,
            match_indices: Vec::new(),
            kind: CompletionKind::Value,
            source: "test",
            trust: "validated",
        }
    }

    #[test]
    fn catalog_completion_uses_the_frozen_worker_envelope() {
        let catalog = Catalog::builtin();
        let mut state = CompletionState::new(catalog, None);
        assert!(state.worker.is_none());
        state.request("git st", 6).unwrap();
        assert!(state.worker.is_some());
        for _ in 0..100 {
            if state.poll("git st", 6) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(state.items.iter().any(|item| item.value.contains("status")));
    }

    #[test]
    fn oversized_query_stays_in_the_editor_and_reports_the_protocol_bound() {
        let catalog = Catalog::builtin();
        let mut state = CompletionState::new(catalog, None);
        let line = "x".repeat(quirl_catalog::MAX_COMPLETION_QUERY_BYTES + 1);
        state.request(&line, line.len()).unwrap();
        assert!(!state.open);
        assert!(!state.streaming);
        assert!(state
            .resource_notice()
            .is_some_and(|notice| notice.contains("4096 query bytes")));
    }

    #[test]
    fn rich_extension_worker_discards_invalid_utf8_replacement_boundaries() {
        let mut state =
            CompletionState::new(Catalog::builtin(), Some(Box::new(InvalidSpanCompleter)));
        state.request("é", "é".len()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline && state.streaming {
            state.poll("é", "é".len());
            std::thread::yield_now();
        }
        assert!(!state.streaming);
        assert!(state.items.iter().all(|item| item.source != "plugin"));
    }

    #[test]
    fn catalog_completion_carries_source_and_trust_metadata() {
        let catalog = Catalog::builtin();
        let item = catalog
            .complete("git st", 6)
            .into_iter()
            .find(|item| item.value.contains("status"))
            .unwrap();
        let item = catalog_item(&catalog, item);
        assert_eq!(item.source, "external");
        assert_eq!(item.trust, "imported");
    }

    #[test]
    fn exact_command_completion_explains_catalog_capabilities() {
        let catalog = Catalog::builtin();
        let item = catalog
            .complete("ls", 2)
            .into_iter()
            .find(|item| item.value == "ls")
            .unwrap();
        let item = catalog_item(&catalog, item);

        assert_eq!(item.kind, CompletionKind::Command);
        assert!(item.summary.contains("directory"));
        assert!(item.detail.contains("Capabilities:"));
        assert!(item.detail.contains("read filesystem"));
        assert_eq!(item.source, "builtin");
    }

    #[test]
    fn automatic_information_preserves_enter_until_the_user_navigates() {
        let mut state = CompletionState::new(Catalog::builtin(), None);
        state.request_automatic("ls", 2).unwrap();
        assert!(state.open);
        assert!(!state.accepts_enter());

        state.next();
        assert!(state.accepts_enter());
    }

    #[test]
    fn manual_and_extension_results_enforce_aggregate_bounds() {
        let catalog = Catalog::builtin();
        let mut state = CompletionState::new(catalog, None);
        state.open_manual(
            (0..(COMPLETION_ITEMS_MAX + 10))
                .map(|index| manual_item(format!("item-{index}")))
                .collect(),
            "bounded",
        );
        assert_eq!(state.items.len(), COMPLETION_ITEMS_MAX);

        let oversized = ExtensionSuggestion {
            value: "x".repeat(COMPLETION_ITEM_BYTES_MAX + 1),
            display: "oversized".to_owned(),
            summary: "test".to_owned(),
            detail: "detail".to_owned(),
            replace_start: 0,
            replace_end: 0,
        };
        let before = state.items.len();
        merge_extension_items(&mut state.items, vec![oversized]);
        assert_eq!(state.items.len(), before);
    }

    #[test]
    fn slow_extensions_merge_after_catalog_without_blocking_first_paint() {
        let catalog = Catalog::builtin();
        let mut state = CompletionState::new(
            catalog,
            Some(Box::new(SlowCompleter {
                delay: Duration::from_millis(80),
            })),
        );
        state.request("git st", 6).unwrap();
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(60) && !state.poll("git st", 6) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(state.items.iter().any(|item| item.value.contains("status")));
        assert!(state.streaming, "the slow provider should still be pending");

        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline && !state.items.iter().any(|item| item.source == "plugin") {
            state.poll("git st", 6);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(state.items.iter().any(|item| item.source == "plugin"));
        assert!(!state.streaming);
    }

    #[test]
    fn cancelled_slow_extension_results_never_repaint_a_newer_query() {
        let catalog = Catalog::builtin();
        let mut state = CompletionState::new(
            catalog,
            Some(Box::new(SlowCompleter {
                delay: Duration::from_millis(30),
            })),
        );
        state.request("old", 3).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        state.cancel_for_edit();
        state.request("new", 3).unwrap();

        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline && state.streaming {
            state.poll("new", 3);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(state.items.iter().any(|item| item.value == "plugin-new"));
        assert!(!state.items.iter().any(|item| item.value == "plugin-old"));
    }

    #[test]
    fn blocked_extension_shutdown_is_nonblocking_and_flood_keeps_one_pending_query() {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let worker = ExtensionWorker::new(Box::new(GatedCompleter {
            gate: Arc::clone(&gate),
            started: started_tx,
            finished: finished_tx,
        }));
        worker.submit(ExtensionRequest {
            request_id: 1,
            line: "first".to_owned(),
            cursor: 5,
        });
        assert!(started_rx.recv_timeout(Duration::from_secs(1)).is_ok());

        const REQUESTS: u64 = 10_000;
        for request_id in 2..=REQUESTS {
            worker.submit(ExtensionRequest {
                request_id,
                line: "latest".to_owned(),
                cursor: 6,
            });
        }
        let pending = {
            let (lock, _) = &*worker.queue;
            lock.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pending
                .as_ref()
                .map(|request| request.request_id)
        };
        assert_eq!(pending, Some(REQUESTS));

        let (dropped_tx, dropped_rx) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(worker);
            let _ = dropped_tx.send(());
        });
        let drop_result = dropped_rx.recv_timeout(Duration::from_millis(250));

        let (lock, ready) = &*gate;
        *lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = true;
        ready.notify_all();
        assert!(finished_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        dropper.join().unwrap();
        assert!(
            drop_result.is_ok(),
            "worker shutdown waited for a blocked extension callback"
        );
    }
}
