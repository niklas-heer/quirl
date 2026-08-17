use quirl_catalog::{ArgumentKind, Catalog, CommandSpec, Confidence};
use quirl_syntax::{
    highlight, parse_command_list, CommandList, HighlightKind, HighlightSpan, Mode, Quoting,
    SimpleCommand, Word,
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

use super::editor::MAX_EDITOR_BUFFER_BYTES;

const TIMING_WINDOW: usize = 128;
const MAX_PATH_DIRECTORIES: usize = 256;
const MAX_PATH_ENTRIES_PER_DIRECTORY: usize = 4_096;
const MAX_PATH_COMMANDS: usize = 65_536;
const MAX_PATH_NAME_BYTES: usize = 1024 * 1024;
const COMMAND_SUGGESTION_TOKEN_BYTES_MAX: usize = 256;
const COMMAND_SUGGESTION_CANDIDATES_MAX: usize = 1_024;
const COMMAND_SUGGESTION_DISTANCE_MAX: usize = 3;

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

    pub fn replace_catalog(&mut self, catalog: Arc<Catalog>) {
        self.catalog = Some(catalog);
        self.cached_revision = None;
        self.current = InputAnalysis::default();
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
        let within_input_bound = buffer.len() <= MAX_EDITOR_BUFFER_BYTES;
        let spans = if within_input_bound {
            highlight(buffer, mode)
        } else {
            vec![HighlightSpan {
                range: 0..buffer.len(),
                kind: HighlightKind::Argument,
            }]
        };
        let diagnostic = if within_input_bound {
            self.catalog.as_deref().and_then(|catalog| {
                diagnostic_for(catalog, &self.path_commands, buffer, mode, &spans)
            })
        } else {
            None
        };
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
    if buffer.trim().is_empty() || mode != Mode::Command {
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
    let first_command = parsed.pipelines.first()?.commands.first()?;
    let catalog_command =
        resolve_catalog_command(catalog, first_command).map(|(command, _)| command);
    if catalog_command.is_none() && !command.contains('/') {
        let path_status = path_commands.command_status(command);
        if path_status == Some(false)
            && buffer[command_span.range.end..].contains(char::is_whitespace)
        {
            let suggestion = (command.len() <= COMMAND_SUGGESTION_TOKEN_BYTES_MAX)
                .then(|| {
                    catalog
                        .commands
                        .iter()
                        .filter_map(|item| item.path.split_whitespace().next())
                        .take(COMMAND_SUGGESTION_CANDIDATES_MAX)
                        .filter_map(|candidate| {
                            edit_distance_bounded(
                                command,
                                candidate,
                                COMMAND_SUGGESTION_DISTANCE_MAX,
                            )
                            .map(|distance| (candidate, distance))
                        })
                        .min_by_key(|(candidate, distance)| (*distance, *candidate))
                        .map(|(candidate, _)| candidate)
                })
                .flatten();
            let command_label = bounded_command_label(command);
            let message = suggestion.map_or_else(
                || format!("unknown command `{command_label}`"),
                |candidate| {
                    format!("unknown command `{command_label}` — did you mean `{candidate}`?")
                },
            );
            return Some(SurfaceDiagnostic {
                message,
                severity: DiagnosticSeverity::Error,
                range: Some(command_span.range.clone()),
            });
        }
    }

    let (command, unknown) = unknown_option(catalog, &parsed, spans)?;
    let flag = buffer.get(unknown.clone())?;
    Some(SurfaceDiagnostic {
        message: format!("unknown flag `{flag}` for `{}`", command.path),
        severity: DiagnosticSeverity::Warning,
        range: Some(unknown),
    })
}

fn resolve_catalog_command<'a>(
    catalog: &'a Catalog,
    command: &SimpleCommand,
) -> Option<(&'a CommandSpec, usize)> {
    catalog
        .commands
        .iter()
        .flat_map(|candidate| {
            std::iter::once(candidate.path.as_str())
                .chain(candidate.aliases.iter().map(String::as_str))
                .filter_map(move |invocation| {
                    invocation_matches(invocation, &command.words)
                        .then_some((candidate, invocation.split_whitespace().count()))
                })
        })
        .max_by_key(|(_, word_count)| *word_count)
}

fn invocation_matches(invocation: &str, words: &[String]) -> bool {
    let mut invocation_words = invocation.split_whitespace();
    let matches = invocation_words
        .by_ref()
        .zip(words)
        .all(|(expected, actual)| expected == actual);
    matches && invocation_words.next().is_none()
}

fn unknown_option<'a>(
    catalog: &'a Catalog,
    parsed: &CommandList,
    spans: &[HighlightSpan],
) -> Option<(&'a CommandSpec, Range<usize>)> {
    let mut flag_ranges = spans
        .iter()
        .filter(|span| span.kind == HighlightKind::Flag)
        .map(|span| span.range.clone());
    for pipeline in &parsed.pipelines {
        for parsed_command in &pipeline.commands {
            let resolved = resolve_catalog_command(catalog, parsed_command);
            let short_options = resolved.and_then(|(command, _)| {
                (command.provenance.confidence >= Confidence::High)
                    .then(|| ShortOptionLookup::new(command))
            });
            let mut consumes_next = false;
            let mut options_terminated = false;
            for (word_index, word) in parsed_command.word_ir.iter().enumerate() {
                let range = (word_index > 0 && presentation_flag(word))
                    .then(|| flag_ranges.next())
                    .flatten();
                let Some((command, invocation_word_count)) = resolved else {
                    continue;
                };
                if word_index < invocation_word_count
                    || command.provenance.confidence < Confidence::High
                {
                    continue;
                }
                let token = parsed_command.words.get(word_index)?;
                if consumes_next {
                    consumes_next = false;
                    continue;
                }
                if options_terminated {
                    continue;
                }
                if token == "--" {
                    options_terminated = true;
                    continue;
                }
                if token == "-" || !token.starts_with('-') {
                    continue;
                }
                let Some(short_options) = short_options.as_ref() else {
                    continue;
                };
                match option_token(command, short_options, token) {
                    OptionToken::Known => {}
                    OptionToken::ConsumesNext => consumes_next = true,
                    OptionToken::Unknown => {
                        if let Some(range) = range {
                            return Some((command, range));
                        }
                    }
                }
            }
        }
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionToken {
    Known,
    ConsumesNext,
    Unknown,
}

fn option_token(
    command: &CommandSpec,
    short_options: &ShortOptionLookup<'_>,
    token: &str,
) -> OptionToken {
    if token.starts_with("--") {
        return long_option_token(command, token);
    }
    if let Some(argument) = named_option(command, token) {
        return match argument.kind {
            ArgumentKind::Option => OptionToken::ConsumesNext,
            ArgumentKind::Flag => OptionToken::Known,
            ArgumentKind::Positional => OptionToken::Unknown,
        };
    }

    let Some(cluster) = token.strip_prefix('-') else {
        return OptionToken::Unknown;
    };
    for (offset, member) in cluster.char_indices() {
        let Some(argument) = short_options.get(member) else {
            return OptionToken::Unknown;
        };
        match argument.kind {
            ArgumentKind::Flag => {}
            ArgumentKind::Option => {
                let suffix_start = offset.saturating_add(member.len_utf8());
                return if suffix_start < cluster.len() {
                    OptionToken::Known
                } else {
                    OptionToken::ConsumesNext
                };
            }
            ArgumentKind::Positional => return OptionToken::Unknown,
        }
    }
    if cluster.is_empty() {
        OptionToken::Unknown
    } else {
        OptionToken::Known
    }
}

fn long_option_token(command: &CommandSpec, token: &str) -> OptionToken {
    let (name, has_attached_value) = token
        .split_once('=')
        .map_or((token, false), |(name, _)| (name, true));
    let Some(argument) = named_option(command, name) else {
        return OptionToken::Unknown;
    };
    match argument.kind {
        ArgumentKind::Option if has_attached_value => OptionToken::Known,
        ArgumentKind::Option => OptionToken::ConsumesNext,
        ArgumentKind::Flag if has_attached_value => OptionToken::Unknown,
        ArgumentKind::Flag => OptionToken::Known,
        ArgumentKind::Positional => OptionToken::Unknown,
    }
}

fn named_option<'a>(
    command: &'a CommandSpec,
    name: &str,
) -> Option<&'a quirl_catalog::ArgumentSpec> {
    command.options.iter().find(|argument| {
        argument.kind != ArgumentKind::Positional
            && argument.names.iter().any(|candidate| candidate == name)
    })
}

struct ShortOptionLookup<'a> {
    ascii: [Option<&'a quirl_catalog::ArgumentSpec>; 128],
    unicode: Vec<(char, &'a quirl_catalog::ArgumentSpec)>,
}

impl<'a> ShortOptionLookup<'a> {
    fn new(command: &'a CommandSpec) -> Self {
        let mut lookup = Self {
            ascii: [None; 128],
            unicode: Vec::new(),
        };
        for argument in &command.options {
            if argument.kind == ArgumentKind::Positional {
                continue;
            }
            for name in &argument.names {
                let Some(member) = single_short_member(name) else {
                    continue;
                };
                if member.is_ascii() {
                    let Ok(index) = usize::try_from(u32::from(member)) else {
                        continue;
                    };
                    if lookup.ascii[index].is_none() {
                        lookup.ascii[index] = Some(argument);
                    }
                } else {
                    lookup.unicode.push((member, argument));
                }
            }
        }
        lookup
    }

    fn get(&self, member: char) -> Option<&'a quirl_catalog::ArgumentSpec> {
        if member.is_ascii() {
            let index = usize::try_from(u32::from(member)).ok()?;
            return self.ascii.get(index).copied().flatten();
        }
        self.unicode
            .iter()
            .find_map(|(candidate, argument)| (*candidate == member).then_some(*argument))
    }
}

fn single_short_member(name: &str) -> Option<char> {
    let short = name.strip_prefix('-')?;
    if short.starts_with('-') {
        return None;
    }
    let mut characters = short.chars();
    let member = characters.next()?;
    characters.next().is_none().then_some(member)
}

fn presentation_flag(word: &Word) -> bool {
    word.parts
        .first()
        .is_some_and(|part| part.quoting == Quoting::Unquoted && part.text.starts_with('-'))
}

fn edit_distance_bounded(left: &str, right: &str, maximum: usize) -> Option<usize> {
    if left.len() > COMMAND_SUGGESTION_TOKEN_BYTES_MAX
        || right.len() > COMMAND_SUGGESTION_TOKEN_BYTES_MAX
    {
        return None;
    }
    let left = left.chars().collect::<Vec<_>>();
    let right = right.chars().collect::<Vec<_>>();
    if left.len().abs_diff(right.len()) > maximum {
        return None;
    }

    let unreachable = maximum.saturating_add(1);
    let mut previous = (0..=right.len())
        .map(|index| index.min(unreachable))
        .collect::<Vec<_>>();
    let mut current = vec![unreachable; right.len().saturating_add(1)];
    for (left_index, left_character) in left.iter().enumerate() {
        current.fill(unreachable);
        let row = left_index.saturating_add(1);
        if row <= maximum {
            current[0] = row;
        }
        let start = row.saturating_sub(maximum).max(1);
        let end = row.saturating_add(maximum).min(right.len());
        let mut row_minimum = current[0];
        if start <= end {
            for column in start..=end {
                let replace = previous[column - 1]
                    .saturating_add(usize::from(*left_character != right[column - 1]));
                let insert = current[column - 1].saturating_add(1);
                let delete = previous[column].saturating_add(1);
                current[column] = replace.min(insert).min(delete).min(unreachable);
                row_minimum = row_minimum.min(current[column]);
            }
        }
        if row_minimum > maximum {
            return None;
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous
        .get(right.len())
        .copied()
        .filter(|distance| *distance <= maximum)
}

fn bounded_command_label(command: &str) -> String {
    if command.len() <= COMMAND_SUGGESTION_TOKEN_BYTES_MAX {
        return command.to_owned();
    }
    let mut end = COMMAND_SUGGESTION_TOKEN_BYTES_MAX;
    while end > 0 && !command.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &command[..end])
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
    use quirl_catalog::{import_bash, import_fish, import_help, import_zsh};
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
    fn semantic_analysis_stops_at_the_editor_utf8_byte_bound() {
        let catalog = Catalog::builtin();
        let mut analyzer = InputAnalyzer::new(catalog);
        let exact = format!("ls -{}", "a".repeat(MAX_EDITOR_BUFFER_BYTES - "ls -".len()));
        assert_eq!(exact.len(), MAX_EDITOR_BUFFER_BYTES);
        analyzer.ensure(1, &exact, Mode::Command);
        assert!(analyzer.current().diagnostic.is_none());
        assert_eq!(
            analyzer.current().spans.last().unwrap().range.end,
            exact.len()
        );

        let oversized = format!("{exact}é");
        analyzer.ensure(2, &oversized, Mode::Command);
        assert!(analyzer.current().diagnostic.is_none());
        assert_eq!(
            analyzer.current().spans,
            [HighlightSpan {
                range: 0..oversized.len(),
                kind: HighlightKind::Argument,
            }]
        );
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
    fn builtin_option_validation_handles_clusters_long_options_and_terminators() {
        let catalog = Catalog::builtin();
        for input in [
            "ls -al",
            "ls -la",
            "ls --all",
            "ls --format=json",
            "ls --format json",
            "ls --format -not-an-option",
            "ls -- -not-an-option",
            "ls -",
        ] {
            assert_no_diagnostic(catalog.clone(), input);
        }

        for (input, unknown) in [
            ("ls -alz", "-alz"),
            ("ls --al", "--al"),
            ("ls --all=yes", "--all=yes"),
            ("ls --formatjson", "--formatjson"),
        ] {
            assert_unknown_flag(catalog.clone(), input, unknown);
        }
    }

    #[test]
    fn short_option_with_a_value_ends_cluster_decomposition() {
        let mut catalog = Catalog::builtin();
        let command = catalog.find("ls").unwrap().clone();
        let mut output = command
            .options
            .iter()
            .find(|argument| argument.names == ["--format"])
            .unwrap()
            .clone();
        output.names = vec!["-o".to_owned(), "--output".to_owned()];
        catalog
            .commands
            .iter_mut()
            .find(|candidate| candidate.path == "ls")
            .unwrap()
            .options
            .push(output);

        for input in [
            "ls -ovalue",
            "ls -aovalue",
            "ls -oa",
            "ls -ao value",
            "ls -o -looks-like-a-flag",
        ] {
            assert_no_diagnostic(catalog.clone(), input);
        }
        assert_unknown_flag(catalog, "ls -ao value -z", "-z");
    }

    #[test]
    fn subcommands_multiword_aliases_and_utf8_short_names_resolve_exactly() {
        let mut catalog = Catalog::builtin();
        catalog
            .commands
            .iter_mut()
            .find(|command| command.path == "quirl index build")
            .unwrap()
            .aliases
            .push("quirl ib".to_owned());
        let ls = catalog
            .commands
            .iter_mut()
            .find(|command| command.path == "ls")
            .unwrap();
        ls.aliases.push("ll".to_owned());
        let mut utf8 = ls
            .options
            .iter()
            .find(|argument| argument.names.contains(&"-a".to_owned()))
            .unwrap()
            .clone();
        utf8.names = vec!["-é".to_owned()];
        ls.options.push(utf8);

        for input in [
            "ll -al",
            "quirl index build --format=json",
            "quirl ib --format json",
            "ls -éa",
        ] {
            assert_no_diagnostic(catalog.clone(), input);
        }
        assert_unknown_flag(
            catalog,
            "quirl index build --definitely-invalid",
            "--definitely-invalid",
        );
    }

    #[test]
    fn declarative_imports_validate_clusters_but_heuristic_imports_do_not_guess() {
        let mut catalog = Catalog::builtin();
        catalog.commands.clear();
        for report in [
            import_fish(
                "complete -c fish-ls -s a\ncomplete -c fish-ls -s l\ncomplete -c fish-ls -s o -r",
                "ls.fish",
            ),
            import_bash("complete -W '-a -l -o=' bash-ls", "ls.bash"),
            import_zsh(
                "#compdef zsh-ls\n_arguments '-a[all]' '-l[long]' '-o[output]:file:_files'",
                "_zsh-ls",
            ),
            import_help(
                "Usage: help-ls [OPTIONS]\n  -a, --all  all entries",
                "help-ls.txt",
            ),
        ] {
            catalog.merge_report(report);
        }

        for input in [
            "fish-ls -aloresult",
            "bash-ls -aloresult",
            "zsh-ls -aloresult",
            "help-ls -unknown",
        ] {
            assert_no_diagnostic(catalog.clone(), input);
        }
        for command in ["fish-ls", "bash-ls", "zsh-ls"] {
            let input = format!("{command} -alz");
            assert_unknown_flag(catalog.clone(), &input, "-alz");
        }
    }

    #[test]
    fn medium_confidence_commands_do_not_guess_about_flags() {
        let catalog = Catalog::builtin();
        let mut analyzer = InputAnalyzer::new(catalog);
        analyzer.ensure(1, "git --definitely-invalid", Mode::Command);
        assert!(analyzer.current().diagnostic.is_none());
    }

    fn assert_no_diagnostic(catalog: Catalog, input: &str) {
        let mut analyzer = InputAnalyzer::new(catalog);
        analyzer.ensure(1, input, Mode::Command);
        assert_eq!(analyzer.current().diagnostic, None, "input: {input}");
    }

    fn assert_unknown_flag(catalog: Catalog, input: &str, unknown: &str) {
        let mut analyzer = InputAnalyzer::new(catalog);
        analyzer.ensure(1, input, Mode::Command);
        let diagnostic = analyzer.current().diagnostic.as_ref().unwrap();
        assert_eq!(diagnostic.severity, DiagnosticSeverity::Warning);
        let range = diagnostic.range.clone().unwrap();
        assert_eq!(&input[range], unknown);
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
    fn command_suggestions_use_bounded_tokens_and_banded_distance() {
        assert_eq!(edit_distance_bounded("gti", "git", 3), Some(2));
        assert_eq!(edit_distance_bounded("git", "git", 0), Some(0));
        assert_eq!(edit_distance_bounded("a", "abcdefgh", 3), None);
        assert_eq!(
            edit_distance_bounded(
                &"a".repeat(COMMAND_SUGGESTION_TOKEN_BYTES_MAX + 1),
                "git",
                COMMAND_SUGGESTION_DISTANCE_MAX,
            ),
            None
        );

        let catalog = Catalog::builtin();
        let mut path_commands = PathCommandCache::new(None);
        path_commands.ready = true;
        let token = "x".repeat(COMMAND_SUGGESTION_TOKEN_BYTES_MAX + 1);
        let input = format!("{token} argument");
        let spans = highlight(&input, Mode::Command);
        let diagnostic =
            diagnostic_for(&catalog, &path_commands, &input, Mode::Command, &spans).unwrap();
        assert_eq!(
            diagnostic.message,
            format!(
                "unknown command `{}…`",
                "x".repeat(COMMAND_SUGGESTION_TOKEN_BYTES_MAX)
            )
        );
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
