//! Terminal-independent typed fuzzy selection.

use quirl_core::{ErrorCode, ShellError, VersionPolicy};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

/// Version of the serialized picker request and response protocol.
pub const PICKER_PROTOCOL_VERSION: u32 = 1;
/// Compatibility policy for picker protocol documents.
pub const PICKER_VERSION_POLICY: VersionPolicy = VersionPolicy::frozen(PICKER_PROTOCOL_VERSION);
/// Maximum UTF-8 bytes accepted in a picker query.
pub const MAX_PICKER_QUERY_BYTES: usize = 4 * 1024;
/// Maximum candidates accepted in one picker request.
pub const MAX_PICKER_ITEMS: usize = 20_000;
/// Maximum total JSON-encoded bytes across one item's text fields.
pub const MAX_PICKER_ITEM_TEXT_BYTES: usize = 16 * 1024;
/// Maximum encoded bytes accepted in one item's typed JSON value.
pub const MAX_PICKER_ITEM_VALUE_BYTES: usize = 16 * 1024;
/// Maximum estimated encoded bytes accepted in a complete request.
pub const MAX_PICKER_REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// Maximum nesting depth accepted in an item's JSON value.
pub const MAX_PICKER_VALUE_DEPTH: usize = 64;
/// Maximum ranking deadline accepted from a caller, in milliseconds.
pub const MAX_PICKER_DEADLINE_MS: u64 = 250;
/// Canonical descriptor hashed to identify the picker protocol contract.
pub const PICKER_SCHEMA_DESCRIPTOR: &str = "quirl.picker@1{PickItem{deny_unknown;id:string;kind:history|file|directory|action|completion|job|data;label:string;description:string;preview:null|string;value:json(depth<=64,value<=16384-bytes)};PickMatch{deny_unknown;index:usize;score:i32;match_indices:array<usize>};PickerRequest{deny_unknown;protocol_version:u32;request_id:u64(strictly-increasing);query:utf8<=4096-bytes;items:array<PickItem><=20000;limit:usize<=20000;deadline_ms:1..250;total<=4194304-bytes};PickerCancellation{deny_unknown;protocol_version:u32;request_id:u64};PickerResponse{deny_unknown;protocol_version:u32;request_id:u64;outcome:PickerOutcome};PickerOutcome:tag(status)[ready{matches:array<PickMatch>}|cancelled{}|deadline_exceeded{}];policy:frozen-major-v1;query:space-separated-AND,apostrophe-exact,bang-exclude;ordering:score-desc,label-asc,id-asc;selection:stable-index-into-input;worker:newer-request-or-cancellation-never-overwrites-newer-result}";

/// Domain of the typed value offered for selection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A previously executed command line.
    History,
    /// A filesystem file.
    File,
    /// A filesystem directory.
    Directory,
    /// A command or UI action.
    Action,
    /// A semantic command-completion candidate.
    Completion,
    /// A native process job.
    Job,
    /// A structured data value.
    Data,
}

/// A display model that retains the original typed value in `value`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PickItem {
    /// Stable identity used for deterministic tie-breaking.
    pub id: String,
    /// Domain of the original typed value.
    pub kind: ItemKind,
    /// Primary user-visible text used for matching.
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    /// Optional secondary searchable and display text.
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Optional preview rendered by the owning UI and covered by request text bounds.
    pub preview: Option<String>,
    /// Original typed value returned when this item is selected.
    pub value: serde_json::Value,
}

/// Deterministic ranking result referencing an input item by index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PickMatch {
    /// Stable zero-based index into the request's input items.
    pub index: usize,
    /// Ranking score; larger values sort before smaller values.
    pub score: i32,
    /// Grapheme indices to highlight in the item's primary label.
    pub match_indices: Vec<usize>,
}

/// A bounded, versioned ranking request. `deadline_ms` begins when the worker
/// starts this request; callers cancel queued or in-flight work with
/// [`PickerCancellation`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PickerRequest {
    /// Picker protocol version supplied by the caller.
    pub protocol_version: u32,
    /// Session-local identity that [`PickerWorker`] requires to increase per submission.
    pub request_id: u64,
    /// Space-separated fuzzy, exact, or exclusion terms.
    pub query: String,
    /// Candidates limited to [`MAX_PICKER_ITEMS`] by [`Self::validate`].
    pub items: Vec<PickItem>,
    /// Maximum ranked matches to return.
    pub limit: usize,
    /// Wall-clock ranking deadline in milliseconds.
    pub deadline_ms: u64,
}

impl PickerRequest {
    /// Validate protocol compatibility and all size and deadline bounds.
    ///
    /// Request-ID ordering is session state and is enforced by
    /// [`PickerWorker::submit`], not by this method.
    pub fn validate(&self) -> Result<(), ShellError> {
        PICKER_VERSION_POLICY.validate("picker request", self.protocol_version)?;
        if self.query.len() > MAX_PICKER_QUERY_BYTES {
            return Err(resource_limit("picker query", MAX_PICKER_QUERY_BYTES));
        }
        if self.items.len() > MAX_PICKER_ITEMS {
            return Err(resource_limit("picker items", MAX_PICKER_ITEMS));
        }
        if self.limit > MAX_PICKER_ITEMS {
            return Err(resource_limit("picker result limit", MAX_PICKER_ITEMS));
        }
        if !(1..=MAX_PICKER_DEADLINE_MS).contains(&self.deadline_ms) {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "picker deadline must be between 1 and {MAX_PICKER_DEADLINE_MS} milliseconds"
                ),
            )
            .with_help("Use a small positive deadline and submit a new query when it expires"));
        }
        // 128 is a deliberate upper bound for the fixed request keys, braces,
        // commas, and integer fields (including a u64 request ID). Individual
        // string and JSON values below are measured in their JSON-encoded form.
        let mut request_bytes = 128 + json_string_bytes(&self.query);
        for item in &self.items {
            let text_len = json_string_bytes(&item.id)
                + json_string_bytes(&item.label)
                + json_string_bytes(&item.description)
                + item
                    .preview
                    .as_ref()
                    .map_or(0, |preview| json_string_bytes(preview));
            if text_len > MAX_PICKER_ITEM_TEXT_BYTES {
                return Err(resource_limit(
                    "picker item text",
                    MAX_PICKER_ITEM_TEXT_BYTES,
                ));
            }
            let value_len = json_value_bytes_and_depth(&item.value)?;
            if value_len > MAX_PICKER_ITEM_VALUE_BYTES {
                return Err(resource_limit(
                    "picker item value",
                    MAX_PICKER_ITEM_VALUE_BYTES,
                ));
            }
            // 96 covers this item's stable keys, kind tag, punctuation, and
            // optional-field separators. It intentionally overestimates.
            request_bytes = request_bytes
                .checked_add(96)
                .and_then(|total| total.checked_add(text_len))
                .and_then(|total| total.checked_add(value_len))
                .ok_or_else(|| resource_limit("picker request", MAX_PICKER_REQUEST_BYTES))?;
            if request_bytes > MAX_PICKER_REQUEST_BYTES {
                return Err(resource_limit("picker request", MAX_PICKER_REQUEST_BYTES));
            }
        }
        Ok(())
    }
}

/// Versioned cancellation targeting one picker request ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PickerCancellation {
    /// Picker protocol version supplied by the caller.
    pub protocol_version: u32,
    /// Request to invalidate if it is still the newest request.
    pub request_id: u64,
}

impl PickerCancellation {
    /// Validate cancellation protocol compatibility.
    pub fn validate(&self) -> Result<(), ShellError> {
        PICKER_VERSION_POLICY.validate("picker cancellation", self.protocol_version)
    }
}

/// Terminal outcome of one bounded ranking request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "status", content = "data")]
pub enum PickerOutcome {
    /// Ranking completed before cancellation and its deadline.
    Ready {
        /// Deterministically ordered matches, bounded by the request limit.
        matches: Vec<PickMatch>,
    },
    /// A newer request or explicit cancellation invalidated this work.
    Cancelled,
    /// The request exhausted its wall-clock deadline.
    DeadlineExceeded,
}

/// Versioned response associated with exactly one picker request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PickerResponse {
    /// Picker protocol version used to encode this response.
    pub protocol_version: u32,
    /// Request identity whose work produced this response.
    pub request_id: u64,
    /// Ready, cancelled, or deadline-exceeded result.
    pub outcome: PickerOutcome,
}

/// A single-worker picker executor. Newer request IDs atomically invalidate
/// older requests, so stale results can never be observed by the caller.
pub struct PickerWorker {
    requests: Option<Sender<PickerRequest>>,
    responses: Receiver<PickerResponse>,
    latest_request_id: Arc<AtomicU64>,
    submitted_request_id: u64,
    worker: Option<JoinHandle<()>>,
}

impl PickerWorker {
    /// Spawn a worker with no submitted request IDs.
    pub fn new() -> Self {
        let (requests, request_receiver) = mpsc::channel();
        let (response_sender, responses) = mpsc::channel();
        let latest_request_id = Arc::new(AtomicU64::new(0));
        let worker_latest = Arc::clone(&latest_request_id);
        let worker = thread::spawn(move || {
            picker_worker_loop(request_receiver, response_sender, worker_latest)
        });
        Self {
            requests: Some(requests),
            responses,
            latest_request_id,
            submitted_request_id: 0,
            worker: Some(worker),
        }
    }

    /// Validate and submit a request whose ID is newer than every prior request.
    pub fn submit(&mut self, request: PickerRequest) -> Result<(), ShellError> {
        request.validate()?;
        if request.request_id <= self.submitted_request_id {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "picker request IDs must be strictly increasing",
            )
            .with_help("Allocate a new request ID for every query change"));
        }
        self.submitted_request_id = request.request_id;
        self.latest_request_id
            .store(request.request_id, Ordering::Release);
        self.requests
            .as_ref()
            .ok_or_else(|| {
                ShellError::new(ErrorCode::ResourceLimit, "picker worker is unavailable")
                    .with_help("Create a new picker worker for the next interactive session")
            })?
            .send(request)
            .map_err(|_| {
                ShellError::new(ErrorCode::ResourceLimit, "picker worker is unavailable")
                    .with_help("Create a new picker worker for the next interactive session")
            })
    }

    /// Invalidate `cancellation.request_id` if it is still current.
    pub fn cancel(&self, cancellation: PickerCancellation) -> Result<(), ShellError> {
        cancellation.validate()?;
        let _ = self.latest_request_id.compare_exchange(
            cancellation.request_id,
            cancellation.request_id.saturating_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Ok(())
    }

    /// Return only a response for the newest submitted request, dropping every
    /// stale response that was produced before a new keystroke arrived.
    pub fn try_recv_latest(&self) -> Option<PickerResponse> {
        let expected = self.submitted_request_id;
        if self.latest_request_id.load(Ordering::Acquire) != expected {
            while self.responses.try_recv().is_ok() {}
            return None;
        }
        let mut newest = None;
        loop {
            match self.responses.try_recv() {
                Ok(response) if response.request_id == expected => newest = Some(response),
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return newest,
            }
        }
    }
}

impl Default for PickerWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PickerWorker {
    fn drop(&mut self) {
        // Dropping the sender closes the worker's receive loop. Joining avoids
        // allowing a background query to outlive an interactive session.
        self.requests.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn picker_worker_loop(
    requests: Receiver<PickerRequest>,
    responses: Sender<PickerResponse>,
    latest_request_id: Arc<AtomicU64>,
) {
    while let Ok(request) = requests.recv() {
        let request_id = request.request_id;
        let response = execute_request(&request, || {
            latest_request_id.load(Ordering::Acquire) != request_id
        });
        // A stale response is intentionally not published. The caller also
        // filters defensively in case it submitted a newer request concurrently.
        if latest_request_id.load(Ordering::Acquire) == request_id
            && responses.send(response).is_err()
        {
            return;
        }
    }
}

/// Execute a request synchronously for hosts that already own a worker.
///
/// The cancellation probe is checked between items. Callers must first use
/// [`PickerRequest::validate`]; this function assumes its size, depth, limit,
/// and deadline invariants already hold.
pub fn execute_request(
    request: &PickerRequest,
    mut cancelled: impl FnMut() -> bool,
) -> PickerResponse {
    let started = Instant::now();
    let deadline = Duration::from_millis(request.deadline_ms);
    let terms = request
        .query
        .split_whitespace()
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    let mut matches = Vec::new();
    for (index, item) in request.items.iter().enumerate() {
        if cancelled() {
            return picker_response(request.request_id, PickerOutcome::Cancelled);
        }
        if started.elapsed() >= deadline {
            return picker_response(request.request_id, PickerOutcome::DeadlineExceeded);
        }
        if let Some((score, match_indices)) = rank_item(item, &terms) {
            matches.push(PickMatch {
                index,
                score,
                match_indices,
            });
        }
    }
    if cancelled() {
        return picker_response(request.request_id, PickerOutcome::Cancelled);
    }
    if started.elapsed() >= deadline {
        return picker_response(request.request_id, PickerOutcome::DeadlineExceeded);
    }
    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                request.items[left.index]
                    .label
                    .cmp(&request.items[right.index].label)
            })
            .then_with(|| {
                request.items[left.index]
                    .id
                    .cmp(&request.items[right.index].id)
            })
    });
    matches.truncate(request.limit);
    picker_response(request.request_id, PickerOutcome::Ready { matches })
}

fn picker_response(request_id: u64, outcome: PickerOutcome) -> PickerResponse {
    PickerResponse {
        protocol_version: PICKER_PROTOCOL_VERSION,
        request_id,
        outcome,
    }
}

fn resource_limit(subject: &str, maximum: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{subject} exceeds its limit of {maximum}"),
    )
    .with_help("Reduce the request size before sending it to the interactive picker")
}

/// Measure untrusted JSON without serializing it again. Object keys count
/// toward the budget too, and a fixed depth prevents pathological nesting from
/// reaching fuzzy-picker consumers.
fn json_value_bytes_and_depth(value: &serde_json::Value) -> Result<usize, ShellError> {
    let bytes = json_value_bytes(value, 1)?;
    Ok(bytes)
}

fn json_value_bytes(value: &serde_json::Value, depth: usize) -> Result<usize, ShellError> {
    if depth > MAX_PICKER_VALUE_DEPTH {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("picker item value exceeds nesting limit of {MAX_PICKER_VALUE_DEPTH}"),
        )
        .with_help("Flatten nested JSON before sending it to the interactive picker"));
    }
    match value {
        serde_json::Value::Null => Ok(4),
        serde_json::Value::Bool(value) => Ok(if *value { 4 } else { 5 }),
        serde_json::Value::Number(number) => Ok(number.to_string().len()),
        serde_json::Value::String(value) => Ok(json_string_bytes(value)),
        serde_json::Value::Array(values) => values.iter().try_fold(2_usize, |total, value| {
            let value = json_value_bytes(value, depth + 1)?;
            total
                .checked_add(1)
                .and_then(|total| total.checked_add(value))
                .ok_or_else(|| resource_limit("picker item value", MAX_PICKER_ITEM_VALUE_BYTES))
        }),
        serde_json::Value::Object(values) => {
            values.iter().try_fold(2_usize, |total, (key, value)| {
                let value = json_value_bytes(value, depth + 1)?;
                total
                    .checked_add(1)
                    .and_then(|total| total.checked_add(json_string_bytes(key)))
                    .and_then(|total| total.checked_add(1))
                    .and_then(|total| total.checked_add(value))
                    .ok_or_else(|| resource_limit("picker item value", MAX_PICKER_ITEM_VALUE_BYTES))
            })
        }
    }
}

fn json_string_bytes(value: &str) -> usize {
    2 + value
        .chars()
        .map(|character| match character {
            '"' | '\\' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        })
        .sum::<usize>()
}

/// Stateless deterministic fuzzy ranker for synchronous callers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Picker;

impl Picker {
    /// Rank items deterministically. Space-separated terms are ANDed, a leading `'`
    /// requests an exact substring, and `!` excludes matching items.
    ///
    /// This synchronous helper does not apply [`PickerRequest`] resource bounds
    /// or cancellation; use it only with inputs already bounded by the caller.
    pub fn rank(&self, items: &[PickItem], query: &str) -> Vec<PickMatch> {
        let terms = query
            .split_whitespace()
            .filter(|term| !term.is_empty())
            .collect::<Vec<_>>();
        let mut matches = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| rank_item(item, &terms).map(|rank| (index, rank)))
            .map(|(index, (score, match_indices))| PickMatch {
                index,
                score,
                match_indices,
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| items[left.index].label.cmp(&items[right.index].label))
                .then_with(|| items[left.index].id.cmp(&items[right.index].id))
        });
        matches
    }

    /// Return at most `limit` input items in deterministic rank order.
    ///
    /// Like [`Self::rank`], this helper assumes caller-bounded inputs.
    pub fn select<'items>(
        &self,
        items: &'items [PickItem],
        query: &str,
        limit: usize,
    ) -> Vec<&'items PickItem> {
        self.rank(items, query)
            .into_iter()
            .take(limit)
            .map(|matched| &items[matched.index])
            .collect()
    }
}

fn rank_item(item: &PickItem, terms: &[&str]) -> Option<(i32, Vec<usize>)> {
    let label_graphemes = item.label.graphemes(true).count();
    let searchable = if item.description.is_empty() {
        item.label.clone()
    } else {
        format!("{} {}", item.label, item.description)
    };
    let searchable = FoldedText::new(&searchable);
    let mut score = 0;
    let mut primary_indices = Vec::new();
    for raw_term in terms {
        let (inverse, term) = raw_term
            .strip_prefix('!')
            .map_or((false, *raw_term), |term| (true, term));
        let (exact, term) = term
            .strip_prefix('\'')
            .map_or((false, term), |term| (true, term));
        if term.is_empty() {
            continue;
        }
        let term = term.to_lowercase();
        let matched = if exact {
            searchable.value.find(&term).map(|start| {
                (
                    20_000 - i32::try_from(searchable.grapheme_at(start)).unwrap_or(i32::MAX),
                    searchable.indices_for(start, start + term.len()),
                )
            })
        } else {
            fuzzy_match(&term, &searchable)
        };
        if inverse {
            if matched.is_some() {
                return None;
            }
            continue;
        }
        let (term_score, indices) = matched?;
        score += term_score;
        if primary_indices.is_empty() && indices.iter().all(|index| *index < label_graphemes) {
            primary_indices = indices;
        }
    }
    Some((score, primary_indices))
}

struct FoldedText {
    value: String,
    grapheme_by_byte: Vec<usize>,
    grapheme_count: usize,
}

impl FoldedText {
    fn new(value: &str) -> Self {
        let mut folded = String::new();
        let mut grapheme_by_byte = Vec::new();
        let mut grapheme_count = 0;
        for (index, grapheme) in value.graphemes(true).enumerate() {
            let lowercase = grapheme.to_lowercase();
            folded.push_str(&lowercase);
            grapheme_by_byte.extend(std::iter::repeat_n(index, lowercase.len()));
            grapheme_count = index + 1;
        }
        Self {
            value: folded,
            grapheme_by_byte,
            grapheme_count,
        }
    }

    fn grapheme_at(&self, byte_index: usize) -> usize {
        self.grapheme_by_byte
            .get(byte_index)
            .copied()
            .unwrap_or(self.grapheme_count)
    }

    fn indices_for(&self, start: usize, end: usize) -> Vec<usize> {
        let mut indices = Vec::new();
        for index in self
            .grapheme_by_byte
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .copied()
        {
            if indices.last().copied() != Some(index) {
                indices.push(index);
            }
        }
        indices
    }
}

fn fuzzy_match(query: &str, candidate: &FoldedText) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    if candidate.value.starts_with(query) {
        return Some((
            10_000 - i32::try_from(candidate.grapheme_count).unwrap_or(i32::MAX),
            candidate.indices_for(0, query.len()),
        ));
    }
    let mut indices = Vec::new();
    let mut characters = candidate.value.char_indices();
    for wanted in query.chars() {
        let (byte_index, _) = characters.find(|(_, actual)| *actual == wanted)?;
        let index = candidate.grapheme_at(byte_index);
        if indices.last().copied() != Some(index) {
            indices.push(index);
        }
    }
    let spread = i32::try_from(indices.last().copied().unwrap_or_default()).unwrap_or(i32::MAX);
    let length = i32::try_from(candidate.grapheme_count).unwrap_or(i32::MAX);
    Some((1_000 - spread - length, indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: ItemKind, label: &str) -> PickItem {
        PickItem {
            id: id.to_owned(),
            kind,
            label: label.to_owned(),
            description: String::new(),
            preview: None,
            value: serde_json::json!({ "original": id }),
        }
    }

    #[test]
    fn one_engine_ranks_history_files_actions_and_jobs_without_losing_values() {
        let items = vec![
            item("h1", ItemKind::History, "cargo test --workspace"),
            item("f1", ItemKind::File, "crates/quirl-core/src/lib.rs"),
            item("a1", ItemKind::Action, "Switch to data mode"),
            item("j1", ItemKind::Job, "deploy staging"),
        ];
        let selected = Picker.select(&items, "cts", 1);
        assert_eq!(selected[0].id, "h1");
        assert_eq!(selected[0].value["original"], "h1");
    }

    #[test]
    fn exact_inverse_and_multi_term_queries_are_deterministic() {
        let items = vec![
            item("1", ItemKind::File, "src/generated report.rs"),
            item("2", ItemKind::File, "src/final report.rs"),
            item("3", ItemKind::File, "docs/final report.md"),
        ];
        let selected = Picker.select(&items, "'final !docs", 10);
        assert_eq!(
            selected
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["2"]
        );
    }

    #[test]
    fn exact_and_fuzzy_matches_return_display_grapheme_indices() {
        let items = vec![
            item("1", ItemKind::File, "café🙂"),
            item("2", ItemKind::File, "İstanbul"),
        ];

        let exact = Picker.rank(&items, "'fé");
        assert_eq!(exact[0].match_indices, [2, 3]);

        let fuzzy = Picker.rank(&items, "fé");
        assert_eq!(fuzzy[0].match_indices, [2, 3]);

        let expanded_lowercase = Picker.rank(&items, "is");
        assert_eq!(expanded_lowercase[0].index, 1);
        assert_eq!(expanded_lowercase[0].match_indices, [0, 1]);
    }

    #[test]
    fn description_matches_do_not_claim_indices_in_the_display_label() {
        let mut described = item("1", ItemKind::File, "alpha");
        described.description = "unique description".to_owned();

        let ranked = Picker.rank(&[described], "'unique");
        assert_eq!(ranked.len(), 1);
        assert!(ranked[0].match_indices.is_empty());
    }

    #[test]
    fn serialized_picker_contract_rejects_unknown_fields() {
        let source = r#"{"id":"1","kind":"file","label":"a","description":"","preview":null,"value":null,"future":true}"#;
        assert!(serde_json::from_str::<PickItem>(source).is_err());
        let request = r#"{"protocol_version":1,"request_id":1,"query":"a","items":[],"limit":1,"deadline_ms":20,"future":true}"#;
        assert!(serde_json::from_str::<PickerRequest>(request).is_err());
        assert_eq!(PICKER_PROTOCOL_VERSION, 1);
        assert!(PICKER_SCHEMA_DESCRIPTOR.contains("selection:stable-index-into-input"));
    }

    fn request(request_id: u64, items: Vec<PickItem>) -> PickerRequest {
        PickerRequest {
            protocol_version: PICKER_PROTOCOL_VERSION,
            request_id,
            query: "cargo".to_owned(),
            items,
            limit: 10,
            deadline_ms: 100,
        }
    }

    #[test]
    fn versioned_requests_fail_closed_and_enforce_all_input_bounds() {
        let mut invalid_version = request(1, Vec::new());
        invalid_version.protocol_version = PICKER_PROTOCOL_VERSION + 1;
        assert!(invalid_version.validate().is_err());

        let mut huge_query = request(1, Vec::new());
        huge_query.query = "x".repeat(MAX_PICKER_QUERY_BYTES + 1);
        assert_eq!(
            huge_query.validate().unwrap_err().code,
            ErrorCode::ResourceLimit
        );

        let mut too_many_items = request(1, Vec::new());
        too_many_items.items = vec![item("x", ItemKind::File, "x"); MAX_PICKER_ITEMS + 1];
        assert_eq!(
            too_many_items.validate().unwrap_err().code,
            ErrorCode::ResourceLimit
        );

        let mut invalid_deadline = request(1, Vec::new());
        invalid_deadline.deadline_ms = 0;
        assert_eq!(
            invalid_deadline.validate().unwrap_err().code,
            ErrorCode::Validation
        );

        let mut oversized_value = item("value", ItemKind::Data, "value");
        oversized_value.value =
            serde_json::Value::String("x".repeat(MAX_PICKER_ITEM_VALUE_BYTES + 1));
        assert_eq!(
            request(1, vec![oversized_value])
                .validate()
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );

        let mut nested = serde_json::Value::Null;
        for _ in 0..=MAX_PICKER_VALUE_DEPTH {
            nested = serde_json::Value::Array(vec![nested]);
        }
        let mut deeply_nested = item("nested", ItemKind::Data, "nested");
        deeply_nested.value = nested;
        assert_eq!(
            request(1, vec![deeply_nested]).validate().unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn cancellation_is_observed_between_items_without_a_partial_result() {
        let items = (0..32)
            .map(|index| item(&index.to_string(), ItemKind::File, "cargo.toml"))
            .collect();
        let request = request(5, items);
        let mut checks = 0;
        let response = execute_request(&request, || {
            checks += 1;
            checks > 4
        });
        assert_eq!(response.request_id, 5);
        assert_eq!(response.outcome, PickerOutcome::Cancelled);
    }

    #[test]
    fn worker_never_returns_a_stale_result_after_a_newer_query() {
        let mut worker = PickerWorker::new();
        let slow_items = (0..MAX_PICKER_ITEMS)
            .map(|index| item(&index.to_string(), ItemKind::File, "cargo.toml"))
            .collect();
        worker.submit(request(1, slow_items)).unwrap();
        worker
            .submit(PickerRequest {
                query: "test".to_owned(),
                items: vec![item("new", ItemKind::Action, "cargo test")],
                ..request(2, Vec::new())
            })
            .unwrap();

        let until = Instant::now() + Duration::from_secs(1);
        let mut response = None;
        while Instant::now() < until {
            response = worker.try_recv_latest();
            if response.is_some() {
                break;
            }
            thread::yield_now();
        }
        let response = response.expect("newest picker response should arrive");
        assert_eq!(response.request_id, 2);
        assert!(matches!(response.outcome, PickerOutcome::Ready { .. }));
    }

    #[test]
    fn worker_cancellation_discards_already_queued_responses() {
        let mut worker = PickerWorker::new();
        worker
            .submit(request(1, vec![item("one", ItemKind::File, "cargo.toml")]))
            .unwrap();
        worker
            .cancel(PickerCancellation {
                protocol_version: PICKER_PROTOCOL_VERSION,
                request_id: 1,
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            assert!(worker.try_recv_latest().is_none());
            thread::yield_now();
        }
    }

    #[test]
    fn cancellation_envelope_rejects_unknown_versions() {
        let cancellation = PickerCancellation {
            protocol_version: 2,
            request_id: 1,
        };
        assert_eq!(
            cancellation.validate().unwrap_err().code,
            ErrorCode::Validation
        );
    }
}
