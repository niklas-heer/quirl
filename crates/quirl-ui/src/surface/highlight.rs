use quirl_catalog::{Catalog, CommandSpec, Confidence};
use quirl_syntax::{
    highlight, parse_command_list, CommandList, HighlightKind, HighlightSpan, Mode,
};
use std::{
    collections::{HashSet, VecDeque},
    env,
    ffi::OsString,
    fs,
    ops::Range,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TIMING_WINDOW: usize = 128;
const MAX_PATH_DIRECTORIES: usize = 256;
const MAX_PATH_ENTRIES_PER_DIRECTORY: usize = 4_096;
const MAX_PATH_COMMANDS: usize = 65_536;
const MAX_PATH_NAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The rendering contract includes hints even though the first analyzer emits
// only errors and high-confidence flag warnings.
#[allow(dead_code)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Hint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceDiagnostic {
    pub message: String,
    pub severity: DiagnosticSeverity,
    pub range: Option<Range<usize>>,
}

impl SurfaceDiagnostic {
    #[cfg(test)]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            severity: DiagnosticSeverity::Error,
            range: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct InputAnalysis {
    pub spans: Vec<HighlightSpan>,
    pub diagnostic: Option<SurfaceDiagnostic>,
}

/// Owns all control-plane state needed by syntax styling and advisory diagnostics.
///
/// The editor revision is the cache key. Filesystem discovery runs on a bounded
/// worker and only publishes complete snapshots, so a keystroke never scans PATH.
pub struct InputAnalyzer {
    catalog: Option<Arc<Catalog>>,
    path_commands: PathCommandCache,
    cached_revision: Option<u64>,
    cached_mode: Mode,
    current: InputAnalysis,
    highlight_times: VecDeque<Duration>,
}

impl InputAnalyzer {
    pub fn new(catalog: impl Into<Arc<Catalog>>) -> Self {
        Self {
            catalog: Some(catalog.into()),
            path_commands: PathCommandCache::dormant(),
            cached_revision: None,
            cached_mode: Mode::Command,
            current: InputAnalysis::default(),
            highlight_times: VecDeque::with_capacity(TIMING_WINDOW),
        }
    }

    pub fn unpublished() -> Self {
        Self {
            catalog: None,
            path_commands: PathCommandCache::dormant(),
            cached_revision: None,
            cached_mode: Mode::Command,
            current: InputAnalysis::default(),
            highlight_times: VecDeque::with_capacity(TIMING_WINDOW),
        }
    }

    pub fn publish_catalog(&mut self, catalog: Arc<Catalog>) {
        assert!(
            self.catalog.is_none(),
            "catalog publication must be one-shot"
        );
        self.catalog = Some(catalog);
        self.cached_revision = None;
    }

    #[cfg(test)]
    pub(super) fn published_catalog(&self) -> Option<&Arc<Catalog>> {
        self.catalog.as_ref()
    }

    /// Refresh PATH at prompt boundaries, never at the per-keystroke boundary.
    pub fn prepare_prompt(&mut self) {
        self.path_commands.request_if_changed(env::var_os("PATH"));
    }

    /// Publish a worker snapshot and invalidate command resolution if it changed.
    pub fn poll_path(&mut self) -> bool {
        if self.path_commands.poll() {
            self.cached_revision = None;
            true
        } else {
            false
        }
    }

    pub fn ensure(&mut self, revision: u64, buffer: &str, mode: Mode) {
        if self.cached_revision == Some(revision) && self.cached_mode == mode {
            return;
        }
        let started = Instant::now();
        let spans = highlight(buffer, mode);
        let diagnostic = self
            .catalog
            .as_deref()
            .and_then(|catalog| diagnostic_for(catalog, &self.path_commands, buffer, mode, &spans));
        self.current = InputAnalysis { spans, diagnostic };
        self.cached_revision = Some(revision);
        self.cached_mode = mode;
        record_timing(&mut self.highlight_times, started.elapsed());
    }

    pub const fn current(&self) -> &InputAnalysis {
        &self.current
    }

    pub fn p95(&self) -> Option<Duration> {
        timing_p95(&self.highlight_times)
    }

    #[cfg(test)]
    fn sample_count(&self) -> usize {
        self.highlight_times.len()
    }
}

struct PathScanRequest {
    generation: u64,
    path: Option<OsString>,
}

struct PathScanResponse {
    generation: u64,
    snapshot: PathSnapshot,
}

#[derive(Debug, Default)]
struct PathSnapshot {
    commands: HashSet<String>,
    truncated: bool,
}

struct PathCommandCache {
    requests: Option<SyncSender<PathScanRequest>>,
    request_receiver: Option<Receiver<PathScanRequest>>,
    response_sender: Option<SyncSender<PathScanResponse>>,
    responses: Receiver<PathScanResponse>,
    worker: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    generation: u64,
    requested_path: Option<OsString>,
    commands: HashSet<String>,
    ready: bool,
}

impl PathCommandCache {
    fn dormant() -> Self {
        let (request_sender, request_receiver) = mpsc::sync_channel::<PathScanRequest>(1);
        let (response_sender, responses) = mpsc::sync_channel::<PathScanResponse>(2);
        let cancel = Arc::new(AtomicBool::new(false));
        Self {
            requests: Some(request_sender),
            request_receiver: Some(request_receiver),
            response_sender: Some(response_sender),
            responses,
            worker: None,
            cancel,
            generation: 0,
            requested_path: None,
            commands: HashSet::new(),
            ready: false,
        }
    }

    #[cfg(test)]
    fn new(path: Option<OsString>) -> Self {
        let mut cache = Self::dormant();
        cache.request(path);
        cache
    }

    fn start_worker(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let Some(request_receiver) = self.request_receiver.take() else {
            return;
        };
        let Some(response_sender) = self.response_sender.take() else {
            return;
        };
        let worker_cancel = Arc::clone(&self.cancel);
        self.worker = Some(thread::spawn(move || {
            while let Ok(mut request) = request_receiver.recv() {
                if worker_cancel.load(Ordering::Acquire) {
                    return;
                }
                // Skip queued intermediate PATH values before starting bounded I/O.
                while let Ok(newer) = request_receiver.try_recv() {
                    request = newer;
                }
                let snapshot = scan_path(request.path.as_deref(), &worker_cancel);
                if response_sender
                    .send(PathScanResponse {
                        generation: request.generation,
                        snapshot,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn request_if_changed(&mut self, path: Option<OsString>) {
        if self.requested_path != path {
            self.request(path);
        }
    }

    fn request(&mut self, path: Option<OsString>) {
        self.start_worker();
        let generation = self.generation.saturating_add(1);
        if let Some(requests) = &self.requests {
            match requests.try_send(PathScanRequest {
                generation,
                path: path.clone(),
            }) {
                Ok(()) => {
                    self.generation = generation;
                    self.requested_path = path;
                    self.commands.clear();
                    self.ready = false;
                }
                // Keep the last accepted PATH key so the next prompt retries
                // after the worker drains its single pending request.
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(response) = self.responses.try_recv() {
            if response.generation != self.generation {
                continue;
            }
            self.commands = response.snapshot.commands;
            // A truncated snapshot cannot safely prove absence, so diagnostics
            // stay conservative rather than reporting false unknown commands.
            self.ready = !response.snapshot.truncated;
            changed = true;
        }
        changed
    }

    fn command_status(&self, command: &str) -> Option<bool> {
        self.ready.then(|| self.commands.contains(command))
    }
}

impl Drop for PathCommandCache {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.requests.take();
        // Filesystem metadata can block inside the OS (notably on an unavailable
        // network mount). Detach rather than delaying terminal restoration; the
        // worker observes cancellation between every bounded scan step.
        self.worker.take();
    }
}

fn scan_path(path: Option<&std::ffi::OsStr>, cancel: &AtomicBool) -> PathSnapshot {
    let Some(path) = path else {
        return PathSnapshot::default();
    };
    let mut snapshot = PathSnapshot::default();
    let mut retained_name_bytes = 0_usize;
    let mut directories = env::split_paths(path);
    for directory_index in 0..=MAX_PATH_DIRECTORIES {
        if cancel.load(Ordering::Acquire) {
            snapshot.truncated = true;
            break;
        }
        let Some(directory) = directories.next() else {
            break;
        };
        if directory_index == MAX_PATH_DIRECTORIES {
            snapshot.truncated = true;
            break;
        }
        let directory = if directory.as_os_str().is_empty() {
            Path::new(".")
        } else {
            directory.as_path()
        };
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                snapshot.truncated = true;
                continue;
            }
        };
        for (entry_index, entry) in entries.enumerate() {
            if cancel.load(Ordering::Acquire) {
                snapshot.truncated = true;
                return snapshot;
            }
            if entry_index == MAX_PATH_ENTRIES_PER_DIRECTORY {
                snapshot.truncated = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    snapshot.truncated = true;
                    continue;
                }
            };
            let executable = match is_executable_file(&entry.path()) {
                Ok(executable) => executable,
                Err(_) => {
                    snapshot.truncated = true;
                    continue;
                }
            };
            if !executable {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if snapshot.commands.contains(&name) {
                continue;
            }
            let next_bytes = retained_name_bytes.saturating_add(name.len());
            if snapshot.commands.len() == MAX_PATH_COMMANDS || next_bytes > MAX_PATH_NAME_BYTES {
                snapshot.truncated = true;
                return snapshot;
            }
            retained_name_bytes = next_bytes;
            snapshot.commands.insert(name);
        }
    }
    snapshot
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> std::io::Result<bool> {
    // The rich surface is Tier 1 only on Unix, but preserve conservative build
    // behavior elsewhere without inventing Windows executable resolution here.
    fs::metadata(path).map(|metadata| metadata.is_file())
}

fn diagnostic_for(
    catalog: &Catalog,
    path_commands: &PathCommandCache,
    buffer: &str,
    mode: Mode,
    spans: &[HighlightSpan],
) -> Option<SurfaceDiagnostic> {
    if buffer.trim().is_empty() || mode == Mode::Data {
        return None;
    }
    let parsed = match parse_command_list(buffer) {
        Ok(parsed) => parsed,
        Err(error) => {
            if super::input_is_incomplete(buffer, mode) {
                return None;
            }
            return Some(SurfaceDiagnostic {
                message: error.message,
                severity: DiagnosticSeverity::Error,
                range: Some(error.start..error.end.max(error.start.saturating_add(1))),
            });
        }
    };

    let command_span = spans
        .iter()
        .find(|span| span.kind == HighlightKind::Command)?;
    let command = buffer.get(command_span.range.clone())?.trim();
    let catalog_command = resolve_catalog_command(catalog, &parsed);
    if catalog_command.is_none() && !command.contains('/') {
        let path_status = path_commands.command_status(command);
        if path_status == Some(false)
            && buffer[command_span.range.end..].contains(char::is_whitespace)
        {
            let suggestion = catalog
                .commands
                .iter()
                .filter_map(|item| item.path.split_whitespace().next())
                .min_by_key(|candidate| edit_distance(command, candidate));
            let message = suggestion.map_or_else(
                || format!("unknown command `{command}`"),
                |candidate| format!("unknown command `{command}` — did you mean `{candidate}`?"),
            );
            return Some(SurfaceDiagnostic {
                message,
                severity: DiagnosticSeverity::Error,
                range: Some(command_span.range.clone()),
            });
        }
    }

    let command = catalog_command?;
    if command.provenance.confidence < Confidence::High {
        return None;
    }
    let unknown = spans.iter().find(|span| {
        span.kind == HighlightKind::Flag
            && buffer.get(span.range.clone()).is_some_and(|flag| {
                !command
                    .options
                    .iter()
                    .flat_map(|option| &option.names)
                    .any(|known| known == flag)
            })
    })?;
    let flag = buffer.get(unknown.range.clone())?;
    Some(SurfaceDiagnostic {
        message: format!("unknown flag `{flag}` for `{}`", command.path),
        severity: DiagnosticSeverity::Warning,
        range: Some(unknown.range.clone()),
    })
}

fn resolve_catalog_command<'a>(
    catalog: &'a Catalog,
    parsed: &CommandList,
) -> Option<&'a CommandSpec> {
    let words = &parsed.pipelines.first()?.commands.first()?.words;
    catalog
        .commands
        .iter()
        .filter(|command| {
            let path_word_count = command.path.split_whitespace().count();
            path_word_count <= words.len()
                && command
                    .path
                    .split_whitespace()
                    .zip(words)
                    .all(|(expected, actual)| expected == actual)
        })
        .max_by_key(|command| command.path.split_whitespace().count())
        .or_else(|| {
            catalog.commands.iter().find(|command| {
                words
                    .first()
                    .is_some_and(|word| command.aliases.iter().any(|alias| alias == word))
            })
        })
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.chars().count()).collect::<Vec<_>>();
    for (left_index, left_char) in left.chars().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_char) in right.chars().enumerate() {
            let replace = previous[right_index] + usize::from(left_char != right_char);
            let insert = current[right_index] + 1;
            let delete = previous[right_index + 1] + 1;
            current.push(replace.min(insert).min(delete));
        }
        previous = current;
    }
    previous.last().copied().unwrap_or(0)
}

fn record_timing(samples: &mut VecDeque<Duration>, elapsed: Duration) {
    if samples.len() == TIMING_WINDOW {
        samples.pop_front();
    }
    samples.push_back(elapsed);
}

fn timing_p95(samples: &VecDeque<Duration>) -> Option<Duration> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.iter().copied().collect::<Vec<_>>();
    sorted.sort_unstable();
    let index = (sorted.len().saturating_sub(1) * 95) / 100;
    sorted.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static NEXT_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn revision_cache_analyzes_a_four_kib_line_once() {
        let catalog = Catalog::builtin();
        let mut analyzer = InputAnalyzer::new(catalog);
        let input = format!("quirl describe {}", "a".repeat(4_096 - 15));
        analyzer.ensure(7, &input, Mode::Command);
        let sample_count = analyzer.sample_count();
        analyzer.ensure(7, &input, Mode::Command);
        assert_eq!(analyzer.sample_count(), sample_count);
        assert_eq!(analyzer.current().spans.first().unwrap().range.start, 0);
        assert_eq!(
            analyzer.current().spans.last().unwrap().range.end,
            input.len()
        );
        assert!(analyzer.p95().unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn exact_catalog_commands_warn_about_unknown_flags() {
        let catalog = Catalog::builtin();
        let mut analyzer = InputAnalyzer::new(catalog);
        let input = "quirl describe --definitely-invalid";
        analyzer.ensure(1, input, Mode::Command);
        let diagnostic = analyzer.current().diagnostic.as_ref().unwrap();
        assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
        let range = diagnostic.range.clone().unwrap();
        assert_eq!(&input[range], "--definitely-invalid");
    }

    #[test]
    fn medium_confidence_commands_do_not_guess_about_flags() {
        let catalog = Catalog::builtin();
        let mut analyzer = InputAnalyzer::new(catalog);
        analyzer.ensure(1, "git --definitely-invalid", Mode::Command);
        assert!(analyzer.current().diagnostic.is_none());
    }

    #[test]
    fn complete_path_snapshot_enables_unknown_command_suggestions() {
        let catalog = Catalog::builtin();
        let mut path_commands = PathCommandCache::new(None);
        path_commands.ready = true;
        let input = "gti status";
        let spans = highlight(input, Mode::Command);
        let diagnostic =
            diagnostic_for(&catalog, &path_commands, input, Mode::Command, &spans).unwrap();
        assert_eq!(diagnostic.severity, DiagnosticSeverity::Error);
        assert!(diagnostic.message.contains("did you mean `git`"));
    }

    #[test]
    fn path_worker_stays_dormant_until_prompt_preparation() {
        let mut analyzer = InputAnalyzer::new(Catalog::builtin());
        assert!(analyzer.path_commands.worker.is_none());
        analyzer.prepare_prompt();
        assert!(analyzer.path_commands.worker.is_some());
    }

    #[test]
    fn path_scan_is_explicitly_bounded_and_finds_executable_files() {
        let root = std::env::temp_dir().join(format!(
            "quirl-path-cache-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        let command = root.join("quirl-test-command");
        fs::write(&command, b"#!/bin/sh\n").unwrap();
        fs::write(root.join("not-executable"), b"data\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&command).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&command, permissions).unwrap();
        }
        let path = env::join_paths([&root]).unwrap();
        let cancel = AtomicBool::new(false);
        let snapshot = scan_path(Some(path.as_os_str()), &cancel);
        assert!(snapshot.commands.contains("quirl-test-command"));
        #[cfg(unix)]
        assert!(!snapshot.commands.contains("not-executable"));
        assert!(snapshot.commands.len() <= MAX_PATH_COMMANDS);
        assert!(!snapshot.truncated);

        let mut cache = PathCommandCache::new(Some(path));
        for _ in 0..200 {
            if cache.poll() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(cache.command_status("quirl-test-command"), Some(true));
        drop(cache);
        fs::remove_dir_all(root).unwrap();
    }
}
