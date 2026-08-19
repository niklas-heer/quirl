use crate::bounded_file::{ReadFileOptions, read_regular_file};
use crate::lua_worker::LuaWorkerRuntime as LuaRuntime;
use clap::ValueEnum;
use quirl_core::{
    AtomicReplaceOptions, EXECUTION_CAPTURE_BYTES_MAX, ErrorCode, ExecutionCancellation,
    ExecutionCleanupState, ExecutionDeadline, ExecutionEffects, ExecutionInput, ExecutionMode,
    ExecutionOutcome, ExecutionOutput, ExecutionOutputTarget, ExecutionPlan, ExecutionRequest,
    ExecutionSource, ExecutionStatus, ProcessRequest, ShellError, StructuredValue,
    escape_json_terminal_controls, replace_file_atomically,
};
use quirl_data::{
    DataRuntime,
    syntax::{
        DataSyntaxDiagnosticKind, DataSyntaxLimits, format_data_expression, parse_data_expression,
    },
};
use quirl_lua::{LuaPolicy, LuaRunnerContext, MAX_LUA_SOURCE_BYTES, format_file};
use quirl_process::{ChildProcessTree, NativeExecutor, sandboxed_process_host};
use quirl_syntax::{check_script, parse_command_list};
use quirl_ui::render_error;
use serde::Serialize;
use serde_json::json;
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read},
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
use quirl_core::ExecutionEffect;
#[cfg(test)]
use serde_json::Value;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

const QUIRL_CANONICAL_EXTENSION: &str = "qrl";
const SCRIPT_EXECUTION_DEADLINE: Duration = Duration::from_secs(30);
const QUIRL_EXTENSION_ALIASES: [&str; 2] = ["quirl", "🌀"];
const AUTHORING_DISCOVERY_LIMITS: ScriptDiscoveryLimits = ScriptDiscoveryLimits {
    depth_max: 32,
    directories_max: 4_096,
    entries_per_directory_max: 4_096,
    entries_total_max: 65_536,
    supported_files_max: 8_192,
    retained_path_bytes_max: 4 * 1024 * 1024,
    scanned_name_bytes_max: 4 * 1024 * 1024,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScriptDiscoveryLimits {
    depth_max: usize,
    directories_max: usize,
    entries_per_directory_max: usize,
    entries_total_max: usize,
    supported_files_max: usize,
    retained_path_bytes_max: usize,
    scanned_name_bytes_max: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScriptLanguage {
    Lua,
    Quirl,
    Bash,
    Zsh,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScriptRunOutput {
    pub status: i32,
    pub value: Value,
}

#[derive(Clone, Debug, Default)]
pub struct ScriptCancellation {
    cancelled: Arc<AtomicBool>,
}

impl ScriptCancellation {
    /// Share an existing cancellation identity with a composed execution request.
    pub(crate) fn from_atomic(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }
}

/// Signal registrations kept alive for one planned `quirl run` invocation.
#[derive(Debug)]
pub(crate) struct ScriptSignalGuard {
    signal_ids: Vec<signal_hook::SigId>,
}

impl ScriptSignalGuard {
    fn register(
        signals: &[i32],
        cancelled: Arc<AtomicBool>,
        message: &'static str,
    ) -> Result<Self, ShellError> {
        Self::register_with(signals, cancelled, message, signal_hook::flag::register)
    }

    fn register_with(
        signals: &[i32],
        cancelled: Arc<AtomicBool>,
        message: &'static str,
        mut register: impl FnMut(i32, Arc<AtomicBool>) -> io::Result<signal_hook::SigId>,
    ) -> Result<Self, ShellError> {
        let mut guard = Self {
            signal_ids: Vec::with_capacity(signals.len()),
        };
        for signal in signals {
            let signal_id = register(*signal, Arc::clone(&cancelled)).map_err(|error| {
                ShellError::new(ErrorCode::Io, message)
                    .with_context(error.to_string())
                    .with_help("Retry after restoring the process signal state")
            })?;
            guard.signal_ids.push(signal_id);
        }
        Ok(guard)
    }
}

impl Drop for ScriptSignalGuard {
    fn drop(&mut self) {
        for signal_id in self.signal_ids.drain(..) {
            signal_hook::low_level::unregister(signal_id);
        }
    }
}

#[derive(Debug, Serialize)]
struct AnalysisReport {
    document_type: &'static str,
    schema_version: u32,
    operation: &'static str,
    valid: bool,
    files: Vec<AnalysisEntry>,
}

#[derive(Debug, Serialize)]
struct AnalysisEntry {
    file: String,
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ShellError>,
}

#[derive(Debug, Clone)]
struct QuirlSyntaxDiagnostic {
    message: String,
    start: usize,
    end: usize,
    help: String,
    resource_limit: bool,
    source: &'static str,
}

pub fn analyze(path: &Path, json_output: bool, lint: bool) -> Result<i32, ShellError> {
    let operation = if lint { "lint" } else { "check" };
    let report = match analysis_report(path, operation, AUTHORING_DISCOVERY_LIMITS) {
        Ok(report) => report,
        Err(error) if json_output => AnalysisReport {
            document_type: "quirl.script.analysis",
            schema_version: 1,
            operation,
            valid: false,
            files: vec![AnalysisEntry {
                file: path.display().to_string(),
                valid: false,
                error: Some(error),
            }],
        },
        Err(error) => return Err(error),
    };
    render_analysis_report(&report, json_output, lint)?;
    Ok(i32::from(!report.valid))
}

fn analysis_report(
    path: &Path,
    operation: &'static str,
    discovery_limits: ScriptDiscoveryLimits,
) -> Result<AnalysisReport, ShellError> {
    let files = discover_supported_files(path, discovery_limits)?;
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let result = check_script_file(&file);
        entries.push(AnalysisEntry {
            file: file.display().to_string(),
            valid: result.is_ok(),
            error: result.err(),
        });
    }
    let report = AnalysisReport {
        document_type: "quirl.script.analysis",
        schema_version: 1,
        operation,
        valid: entries.iter().all(|entry| entry.valid),
        files: entries,
    };
    Ok(report)
}

fn render_analysis_report(
    report: &AnalysisReport,
    json_output: bool,
    lint: bool,
) -> Result<(), ShellError> {
    if json_output {
        let json = serde_json::to_string_pretty(&report).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not serialize script diagnostics")
                .with_context(error.to_string())
                .with_help("Report this as a Quirl script diagnostic schema defect")
        })?;
        println!("{}", escape_json_terminal_controls(&json));
    } else {
        for entry in &report.files {
            if let Some(error) = &entry.error {
                eprintln!("{}", render_error(error, false));
            } else {
                println!(
                    "✓ {} is {}",
                    entry.file,
                    if lint { "lint clean" } else { "valid" }
                );
            }
        }
    }
    Ok(())
}

pub fn format_paths(path: &Path, check: bool) -> Result<i32, ShellError> {
    let files = discover_supported_files(path, AUTHORING_DISCOVERY_LIMITS)?;
    let mut drift = Vec::new();
    let mut failures = Vec::new();
    for file in files {
        let result = if script_language_for_path(&file) == Some(ScriptLanguage::Quirl) {
            format_quirl_file(&file, check)
        } else {
            format_file(&file, check)
        };
        match result {
            Ok(true) => {
                drift.push(file.clone());
                println!(
                    "{} {}",
                    if check {
                        "needs formatting"
                    } else {
                        "formatted"
                    },
                    file.display()
                );
            }
            Ok(false) => println!("unchanged {}", file.display()),
            Err(error) => failures.push(error),
        }
    }
    for error in &failures {
        eprintln!("{}", render_error(error, false));
    }
    if check && !drift.is_empty() {
        eprintln!(
            "{} file(s) need formatting; run `quirl fmt {}`",
            drift.len(),
            path.display()
        );
    }
    Ok(i32::from(
        !failures.is_empty() || (check && !drift.is_empty()),
    ))
}

fn format_quirl_file(path: &Path, check: bool) -> Result<bool, ShellError> {
    let source = read_script_file(path)?;
    let formatted = format_quirl_source(&source, &path.display().to_string())?;
    let changed = source != formatted;
    if changed && !check {
        replace_file_atomically(
            path,
            source.as_bytes(),
            formatted.as_bytes(),
            AtomicReplaceOptions {
                bytes_max: MAX_LUA_SOURCE_BYTES,
            },
        )?;
    }
    Ok(changed)
}

fn format_quirl_source(source: &str, source_name: &str) -> Result<String, ShellError> {
    let statements = native_script_statements(source, source_name)?;
    let mut replacements = Vec::new();
    for statement in statements
        .iter()
        .filter(|statement| statement.kind == NativeStatementKind::Data)
    {
        let body = &source[statement.body.clone()];
        let formatted = format_data_expression(body.trim(), DataSyntaxLimits::DEFAULT).map_err(
            |diagnostic| {
                let leading_bytes = body.len().saturating_sub(body.trim_start().len());
                let code = if diagnostic.kind == DataSyntaxDiagnosticKind::ResourceLimit {
                    ErrorCode::ResourceLimit
                } else {
                    ErrorCode::InvalidCommand
                };
                ShellError::new(code, diagnostic.message)
                    .with_label(
                        Some(source_name.to_owned()),
                        statement.body.start + leading_bytes + diagnostic.start,
                        statement.body.start + leading_bytes + diagnostic.end,
                        "invalid data expression",
                    )
                    .with_help(diagnostic.help)
            },
        )?;
        let replacement = if statement.explicit {
            let indentation = body
                .lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| &line[..line.len().saturating_sub(line.trim_start().len())])
                .unwrap_or("  ");
            let newline = if body.contains("\r\n") { "\r\n" } else { "\n" };
            format!("{indentation}{formatted}{newline}")
        } else {
            formatted
        };
        replacements.push((statement.body.clone(), replacement));
    }
    let mut output = source.to_owned();
    for (range, replacement) in replacements.into_iter().rev() {
        output.replace_range(range, &replacement);
    }
    Ok(output)
}

pub fn test_paths(path: &Path) -> Result<i32, ShellError> {
    let explicit_file = path.is_file();
    let mut files = discover_supported_files(path, AUTHORING_DISCOVERY_LIMITS)?;
    files.retain(|file| {
        script_language_for_path(file) == Some(ScriptLanguage::Lua)
            && (explicit_file || is_test_module(file))
    });
    if files.is_empty() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("no Lua test modules found under {}", path.display()),
        )
        .with_help(
            "Name tests `test_*.lua`, `*_test.lua`, or `*_tests.lua`, or pass a Lua test file",
        ));
    }

    let mut total = 0;
    let mut failed = 0;
    for file in files {
        let runtime =
            LuaRuntime::new_with_process_host(LuaPolicy::script(), sandboxed_process_host())?;
        match runtime.test_file(&file) {
            Ok(count) => {
                total += count;
                println!("✓ {count} Lua tests passed in {}", file.display());
            }
            Err(error) => {
                failed += 1;
                eprintln!("{}", render_error(&error, false));
            }
        }
    }
    if failed == 0 {
        println!("✓ {total} Lua tests passed");
    }
    Ok(i32::from(failed > 0))
}

/// Discover supported scripts without recursion or unbounded directory collection.
///
/// Failure model: a deep, wide, or path-heavy tree is rejected before another
/// path is retained; per-directory and aggregate entries, scanned filename
/// bytes, directories, depth, supported files, and live path bytes each have an
/// explicit caller-provided limit. Directory symlinks are skipped. Stable
/// filesystem identities reject bind-mount or other directory aliases instead
/// of traversing a cycle twice. Permission errors and entries that disappear
/// after enumeration are reported as I/O errors. Entries are sorted within each
/// bounded directory and the bounded result is sorted globally, so traversal
/// and output remain deterministic. No cancellation input exists on these
/// synchronous authoring commands; every turn is instead bounded by these
/// admission limits.
fn discover_supported_files(
    path: &Path,
    limits: ScriptDiscoveryLimits,
) -> Result<Vec<PathBuf>, ShellError> {
    discover_supported_files_with_hook(path, limits, |_, _| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscoveryStage {
    ReadDirectory,
    InspectEntry,
}

#[derive(Debug)]
struct PendingDirectory {
    path: PathBuf,
    depth: usize,
    retained_path_bytes: usize,
}

#[derive(Debug)]
struct DiscoveredPath {
    path: PathBuf,
    file_type: fs::FileType,
    retained_path_bytes: usize,
}

#[derive(Debug)]
struct RetainedPathBytes {
    current: usize,
    maximum: usize,
}

impl RetainedPathBytes {
    fn new(maximum: usize) -> Self {
        Self {
            current: 0,
            maximum,
        }
    }

    fn retain(&mut self, path: &Path) -> Result<usize, ShellError> {
        let bytes = path.as_os_str().as_encoded_bytes().len();
        let observed = self.current.saturating_add(bytes);
        if observed > self.maximum {
            return Err(discovery_limit_error(
                path,
                "retained path bytes",
                self.maximum,
                observed,
            ));
        }
        self.current = observed;
        Ok(bytes)
    }

    fn release(&mut self, bytes: usize) {
        debug_assert!(bytes <= self.current);
        self.current = self.current.saturating_sub(bytes);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    canonical_path: PathBuf,
}

fn discover_supported_files_with_hook(
    path: &Path,
    limits: ScriptDiscoveryLimits,
    mut before_stage: impl FnMut(DiscoveryStage, &Path) -> io::Result<()>,
) -> Result<Vec<PathBuf>, ShellError> {
    validate_discovery_limits(limits)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} is a symbolic link", path.display()),
        )
        .with_help(
            "Pass a real script file or directory; recursive discovery does not follow links",
        ));
    }
    if metadata.is_file() {
        if script_language_for_path(path).is_some() {
            let mut retained = RetainedPathBytes::new(limits.retained_path_bytes_max);
            retained.retain(path)?;
            return Ok(vec![path.to_path_buf()]);
        }
        return Err(unsupported_language_error(
            path.extension().and_then(|extension| extension.to_str()),
        ));
    }
    if !metadata.is_dir() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} is not a regular file or directory", path.display()),
        )
        .with_help("Pass a .lua, .qrl, .quirl, or .🌀 file, or a directory containing scripts"));
    }

    let mut retained = RetainedPathBytes::new(limits.retained_path_bytes_max);
    let root_bytes = retained.retain(path)?;
    let mut directories = vec![PendingDirectory {
        path: path.to_path_buf(),
        depth: 0,
        retained_path_bytes: root_bytes,
    }];
    let root_identity = directory_identity(path, &metadata)?;
    retain_directory_identity(&mut retained, &root_identity)?;
    let mut visited = BTreeSet::from([root_identity]);
    let mut directory_count = 1_usize;
    if directory_count > limits.directories_max {
        return Err(discovery_limit_error(
            path,
            "directories",
            limits.directories_max,
            directory_count,
        ));
    }
    let mut entry_count = 0_usize;
    let mut scanned_name_bytes = 0_usize;
    let mut files = Vec::new();

    while let Some(directory) = directories.pop() {
        before_stage(DiscoveryStage::ReadDirectory, &directory.path)
            .map_err(|error| path_error(&directory.path, error))?;
        let iterator =
            fs::read_dir(&directory.path).map_err(|error| path_error(&directory.path, error))?;
        let mut entries = Vec::new();
        let mut directory_entry_count = 0_usize;
        for item in iterator {
            directory_entry_count = directory_entry_count.saturating_add(1);
            if directory_entry_count > limits.entries_per_directory_max {
                return Err(discovery_limit_error(
                    &directory.path,
                    "entries per directory",
                    limits.entries_per_directory_max,
                    directory_entry_count,
                ));
            }
            entry_count = entry_count.saturating_add(1);
            if entry_count > limits.entries_total_max {
                return Err(discovery_limit_error(
                    &directory.path,
                    "total entries",
                    limits.entries_total_max,
                    entry_count,
                ));
            }
            let entry = item.map_err(|error| path_error(&directory.path, error))?;
            let name = entry.file_name();
            let name_bytes = name.as_encoded_bytes().len();
            scanned_name_bytes = scanned_name_bytes.saturating_add(name_bytes);
            if scanned_name_bytes > limits.scanned_name_bytes_max {
                return Err(discovery_limit_error(
                    &directory.path,
                    "scanned filename bytes",
                    limits.scanned_name_bytes_max,
                    scanned_name_bytes,
                ));
            }
            let entry_path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| path_error(&entry_path, error))?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() && matches!(name.to_str(), Some(".git" | "target")) {
                continue;
            }
            let supported_file =
                file_type.is_file() && script_language_for_path(&entry_path).is_some();
            if !file_type.is_dir() && !supported_file {
                continue;
            }
            let retained_path_bytes = retained.retain(&entry_path)?;
            entries.push(DiscoveredPath {
                path: entry_path,
                file_type,
                retained_path_bytes,
            });
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));

        let mut nested_directories = Vec::new();
        for entry in entries {
            before_stage(DiscoveryStage::InspectEntry, &entry.path)
                .map_err(|error| path_error(&entry.path, error))?;
            let current_metadata = fs::symlink_metadata(&entry.path)
                .map_err(|error| path_error(&entry.path, error))?;
            if entry.file_type.is_dir() {
                if !current_metadata.is_dir() || current_metadata.file_type().is_symlink() {
                    retained.release(entry.retained_path_bytes);
                    continue;
                }
                let depth = directory.depth.saturating_add(1);
                if depth > limits.depth_max {
                    return Err(discovery_limit_error(
                        &entry.path,
                        "directory depth",
                        limits.depth_max,
                        depth,
                    ));
                }
                let observed_directories = directory_count.saturating_add(1);
                if observed_directories > limits.directories_max {
                    return Err(discovery_limit_error(
                        &entry.path,
                        "directories",
                        limits.directories_max,
                        observed_directories,
                    ));
                }
                let identity = directory_identity(&entry.path, &current_metadata)?;
                retain_directory_identity(&mut retained, &identity)?;
                if !visited.insert(identity) {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        format!("directory alias detected at {}", entry.path.display()),
                    )
                    .with_help("Remove bind-mount or filesystem aliases from the script tree"));
                }
                directory_count = observed_directories;
                nested_directories.push(PendingDirectory {
                    path: entry.path,
                    depth,
                    retained_path_bytes: entry.retained_path_bytes,
                });
            } else if current_metadata.is_file() && !current_metadata.file_type().is_symlink() {
                let observed_files = files.len().saturating_add(1);
                if observed_files > limits.supported_files_max {
                    return Err(discovery_limit_error(
                        &entry.path,
                        "supported files",
                        limits.supported_files_max,
                        observed_files,
                    ));
                }
                files.push(entry.path);
            } else {
                retained.release(entry.retained_path_bytes);
            }
        }
        retained.release(directory.retained_path_bytes);
        for nested in nested_directories.into_iter().rev() {
            directories.push(nested);
        }
    }

    files.sort();
    if files.is_empty() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("no supported scripts found under {}", path.display()),
        )
        .with_help("Add a .lua, .qrl, .quirl, or .🌀 file, or pass a different path"));
    }
    Ok(files)
}

fn validate_discovery_limits(limits: ScriptDiscoveryLimits) -> Result<(), ShellError> {
    for (name, value) in [
        ("directories_max", limits.directories_max),
        (
            "entries_per_directory_max",
            limits.entries_per_directory_max,
        ),
        ("entries_total_max", limits.entries_total_max),
        ("supported_files_max", limits.supported_files_max),
        ("retained_path_bytes_max", limits.retained_path_bytes_max),
        ("scanned_name_bytes_max", limits.scanned_name_bytes_max),
    ] {
        if value == 0 {
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("script discovery limit {name} must be greater than zero"),
            )
            .with_help("Pass explicit positive script discovery limits"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn directory_identity(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Result<DirectoryIdentity, ShellError> {
    use std::os::unix::fs::MetadataExt;
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn directory_identity(
    path: &Path,
    _metadata: &fs::Metadata,
) -> Result<DirectoryIdentity, ShellError> {
    let canonical_path = fs::canonicalize(path).map_err(|error| path_error(path, error))?;
    Ok(DirectoryIdentity { canonical_path })
}

#[cfg(unix)]
fn retain_directory_identity(
    _retained: &mut RetainedPathBytes,
    _identity: &DirectoryIdentity,
) -> Result<(), ShellError> {
    Ok(())
}

#[cfg(not(unix))]
fn retain_directory_identity(
    retained: &mut RetainedPathBytes,
    identity: &DirectoryIdentity,
) -> Result<(), ShellError> {
    retained.retain(&identity.canonical_path).map(|_| ())
}

fn discovery_limit_error(path: &Path, resource: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!(
            "script discovery {resource} limit exceeded under {}",
            path.display()
        ),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Narrow the script path or split the tree into smaller bounded directories")
}

fn path_error(path: &Path, error: io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("cannot inspect script path {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Check that the path is readable")
}

fn script_language_for_path(path: &Path) -> Option<ScriptLanguage> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("lua") => Some(ScriptLanguage::Lua),
        Some(extension) if is_quirl_extension(extension) => Some(ScriptLanguage::Quirl),
        _ => None,
    }
}

fn is_quirl_extension(extension: &str) -> bool {
    extension == QUIRL_CANONICAL_EXTENSION || QUIRL_EXTENSION_ALIASES.contains(&extension)
}

fn check_script_file(path: &Path) -> Result<(), ShellError> {
    match script_language_for_path(path) {
        Some(ScriptLanguage::Lua) => LuaRuntime::check_file(path),
        Some(ScriptLanguage::Quirl) => {
            let source = read_script_file(path)?;
            check_quirl_source(&source, &path.display().to_string())
        }
        Some(ScriptLanguage::Bash | ScriptLanguage::Zsh) => Err(unsupported_language_error(
            path.extension().and_then(|extension| extension.to_str()),
        )),
        None => Err(unsupported_language_error(
            path.extension().and_then(|extension| extension.to_str()),
        )),
    }
}

pub(crate) fn check_quirl_source(source: &str, source_name: &str) -> Result<(), ShellError> {
    let diagnostics = quirl_source_diagnostics(source, source_name)?;
    if diagnostics.is_empty() {
        return Ok(());
    }
    let code = if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.resource_limit)
    {
        ErrorCode::ResourceLimit
    } else {
        ErrorCode::InvalidCommand
    };
    let mut error = ShellError::new(code, "quirl script has validation diagnostics");
    for diagnostic in diagnostics {
        error = error
            .with_label(
                Some(source_name.to_owned()),
                diagnostic.start,
                diagnostic.end,
                diagnostic.message,
            )
            .with_help(diagnostic.help);
    }
    Err(error)
}

fn quirl_source_diagnostics(
    source: &str,
    source_name: &str,
) -> Result<Vec<QuirlSyntaxDiagnostic>, ShellError> {
    let statements = native_script_statements(source, source_name)?;
    let mut diagnostics = check_script(&script_without_explicit_blocks(source, &statements))
        .into_iter()
        .map(|diagnostic| QuirlSyntaxDiagnostic {
            message: diagnostic.message,
            start: diagnostic.start,
            end: diagnostic.end,
            help: diagnostic.help,
            resource_limit: false,
            source: "quirl-syntax",
        })
        .collect::<Vec<_>>();
    for statement in &statements {
        if statement.explicit && statement.kind == NativeStatementKind::Command {
            diagnostics.extend(
                command_block_diagnostics(source, statement)
                    .into_iter()
                    .map(|diagnostic| QuirlSyntaxDiagnostic {
                        message: diagnostic.message,
                        start: diagnostic.start,
                        end: diagnostic.end,
                        help: diagnostic.help,
                        resource_limit: false,
                        source: "quirl-syntax",
                    }),
            );
        }
        if statement.kind == NativeStatementKind::Data {
            let body = &source[statement.body.clone()];
            let trimmed = body.trim();
            let leading_bytes = body.len().saturating_sub(body.trim_start().len());
            if let Err(diagnostic) = parse_data_expression(trimmed, DataSyntaxLimits::DEFAULT) {
                diagnostics.push(QuirlSyntaxDiagnostic {
                    message: diagnostic.message,
                    start: statement.body.start + leading_bytes + diagnostic.start,
                    end: statement.body.start + leading_bytes + diagnostic.end,
                    help: diagnostic.help,
                    resource_limit: diagnostic.kind == DataSyntaxDiagnosticKind::ResourceLimit,
                    source: "quirl-data",
                });
            }
        }
    }
    diagnostics.sort_by(|left, right| {
        (left.start, left.end, &left.message).cmp(&(right.start, right.end, &right.message))
    });
    Ok(diagnostics)
}

pub(crate) fn lsp_native_diagnostics(source: &str) -> Vec<quirl_lsp::NativeDiagnostic> {
    match quirl_source_diagnostics(source, "language-service document") {
        Ok(diagnostics) => diagnostics
            .into_iter()
            .map(|diagnostic| quirl_lsp::NativeDiagnostic {
                message: diagnostic.message,
                start: diagnostic.start,
                end: diagnostic.end,
                source: diagnostic.source,
            })
            .collect(),
        Err(error) => {
            if error.details.labels.is_empty() {
                return vec![quirl_lsp::NativeDiagnostic {
                    message: error.message,
                    start: 0,
                    end: source.chars().next().map(char::len_utf8).unwrap_or(0),
                    source: "quirl-script",
                }];
            }
            error
                .details
                .labels
                .into_iter()
                .map(|label| quirl_lsp::NativeDiagnostic {
                    message: format!("{}: {}", error.message, label.message),
                    start: label.start,
                    end: label.end,
                    source: "quirl-script",
                })
                .collect()
        }
    }
}

fn is_test_module(path: &Path) -> bool {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || path
            .components()
            .any(|component| component.as_os_str() == "tests")
}

#[cfg(test)]
pub(crate) fn run(
    file: &Path,
    requested_language: Option<ScriptLanguage>,
    arguments: &[String],
) -> Result<ScriptRunOutput, ShellError> {
    let (source, source_name) = if file == Path::new("-") {
        let source = read_script_stdin()?;
        (source, "<stdin>".to_owned())
    } else {
        let source = read_script_file(file)?;
        (source, file.display().to_string())
    };
    let path = (file != Path::new("-")).then_some(file);
    let language = detect_language(&source, path, requested_language)?;
    let cancellation = ScriptCancellation::default();
    let signal_guard = matches!(
        language,
        ScriptLanguage::Lua | ScriptLanguage::Bash | ScriptLanguage::Zsh
    )
    .then(|| {
        ScriptSignalGuard::register(
            &[signal_hook::consts::SIGINT],
            Arc::clone(&cancellation.cancelled),
            "could not install script cancellation handler",
        )
    })
    .transpose()?;
    let result = run_source_with_cancellation(
        &source,
        &source_name,
        path,
        Some(language),
        arguments,
        &cancellation,
    );
    drop(signal_guard);
    result
}

/// Read, classify, and lower one script invocation into the shared execution
/// request. The returned guard keeps the request's exact cancellation atomic
/// connected to `SIGINT` until execution and cleanup finish.
pub(crate) fn execution_request(
    file: &Path,
    requested_language: Option<ScriptLanguage>,
    arguments: &[String],
) -> Result<(ExecutionRequest, ScriptSignalGuard), ShellError> {
    let (source, source_name) = if file == Path::new("-") {
        (read_script_stdin()?, "<stdin>".to_owned())
    } else {
        (read_script_file(file)?, file.display().to_string())
    };
    let path = (file != Path::new("-")).then_some(file);
    let language = detect_language(&source, path, requested_language)?;
    let mode = match language {
        ScriptLanguage::Lua => ExecutionMode::LuaScript,
        ScriptLanguage::Quirl => ExecutionMode::QuirlScript,
        ScriptLanguage::Bash => ExecutionMode::Bash,
        ScriptLanguage::Zsh => ExecutionMode::Zsh,
    };
    let output = match language {
        ScriptLanguage::Bash | ScriptLanguage::Zsh => ExecutionOutputTarget::Capture {
            max_bytes_per_stream: EXECUTION_CAPTURE_BYTES_MAX,
        },
        ScriptLanguage::Lua | ScriptLanguage::Quirl => ExecutionOutputTarget::Value,
    };
    let cancellation = ExecutionCancellation::default();
    let signal_guard = ScriptSignalGuard::register(
        &[signal_hook::consts::SIGINT],
        cancellation.atomic(),
        "could not install script cancellation handler",
    )?;
    let request = ExecutionRequest::new(ExecutionSource::new(source_name, source)?, mode)
        .with_cancellation(cancellation)
        .with_input(ExecutionInput::None)
        .with_arguments(arguments.to_vec())
        .with_deadline(SCRIPT_EXECUTION_DEADLINE)
        .with_output(output)
        .with_effects(ExecutionEffects::all(), ExecutionEffects::all());
    Ok((request, signal_guard))
}

fn read_script_file(path: &Path) -> Result<String, ShellError> {
    let source_name = path.display().to_string();
    let bytes = read_regular_file(ReadFileOptions {
        path,
        bytes_max: MAX_LUA_SOURCE_BYTES,
        context: "script source",
        help: "Pass a readable UTF-8 script in a regular file at or below 4 MiB",
        io_error_code: ErrorCode::ScriptRead,
    })?;
    decode_script_bytes(bytes, &source_name)
}

fn read_script_stdin() -> Result<String, ShellError> {
    read_script_bytes(io::stdin(), "standard input")
}

fn read_script_bytes(reader: impl Read, source_name: &str) -> Result<String, ShellError> {
    let mut bytes = Vec::new();
    let bytes_max_u64 = u64::try_from(MAX_LUA_SOURCE_BYTES).unwrap_or(u64::MAX);
    reader
        .take(bytes_max_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ShellError::new(
                ErrorCode::ScriptRead,
                format!("cannot read script from {source_name}"),
            )
            .with_context(error.to_string())
            .with_help("Check the script source and read permissions")
        })?;
    if bytes.len() > MAX_LUA_SOURCE_BYTES {
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        return Err(script_source_limit_error(source_name, observed));
    }
    decode_script_bytes(bytes, source_name)
}

fn decode_script_bytes(bytes: Vec<u8>, source_name: &str) -> Result<String, ShellError> {
    String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::ScriptRead,
            format!("script source from {source_name} is not valid UTF-8"),
        )
        .with_context(error.to_string())
        .with_help("Encode script source as UTF-8")
    })
}

fn script_source_limit_error(source_name: &str, size: u64) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("script source from {source_name} exceeds its read limit"),
    )
    .with_context(format!("bytes: {size}; limit: {MAX_LUA_SOURCE_BYTES}"))
    .with_help("Keep executable source below 4 MiB and load data through bounded inputs")
}

#[cfg(test)]
pub(crate) fn run_source(
    source: &str,
    source_name: &str,
    path: Option<&Path>,
    requested_language: Option<ScriptLanguage>,
    arguments: &[String],
) -> Result<ScriptRunOutput, ShellError> {
    run_source_with_cancellation(
        source,
        source_name,
        path,
        requested_language,
        arguments,
        &ScriptCancellation::default(),
    )
}

#[cfg(test)]
pub(crate) fn run_source_with_cancellation(
    source: &str,
    source_name: &str,
    path: Option<&Path>,
    requested_language: Option<ScriptLanguage>,
    arguments: &[String],
    cancellation: &ScriptCancellation,
) -> Result<ScriptRunOutput, ShellError> {
    let language = detect_language(source, path, requested_language)?;
    match language {
        ScriptLanguage::Lua => {
            let runtime = LuaRuntime::new_with_process_host_and_cancellation(
                LuaPolicy::script(),
                sandboxed_process_host(),
                Arc::clone(&cancellation.cancelled),
            )?;
            let context = LuaRunnerContext::from_current_process(
                arguments,
                ExecutionInput::None,
                ExecutionOutputTarget::Value,
                ExecutionEffects::from_effects(&[ExecutionEffect::SpawnProcess]),
                Arc::clone(&cancellation.cancelled),
            )?;
            let outcome = runtime.run_source_with_context(source, source_name, &context)?;
            let status = outcome.status_code();
            let value = match outcome.output {
                ExecutionOutput::Value { value } => value.json_value(),
                ExecutionOutput::Values { values } => {
                    Value::Array(values.into_iter().map(|value| value.json_value()).collect())
                }
                ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => {
                    return Err(ShellError::new(
                        ErrorCode::Validation,
                        "Lua runner returned non-value output to the script front door",
                    )
                    .with_help("Return a typed value or bounded finite value batch"));
                }
            };
            Ok(ScriptRunOutput { status, value })
        }
        ScriptLanguage::Quirl => run_quirl_source(source, source_name, arguments),
        ScriptLanguage::Bash | ScriptLanguage::Zsh => {
            let outcome = run_reference_script(ReferenceScriptRequest {
                source,
                source_name,
                language,
                arguments,
                cancellation,
                executable: language.executable(),
                deadline: script_deadline()?,
                max_bytes_per_stream: EXECUTION_CAPTURE_BYTES_MAX,
                executor: None,
            })?;
            Ok(ScriptRunOutput {
                status: outcome.status,
                value: json!({
                    "language": language.executable(),
                    "status": outcome.status,
                    "stdout": outcome.stdout.unwrap_or_default(),
                    "stderr": outcome.stderr.unwrap_or_default(),
                }),
            })
        }
    }
}

/// Run an explicit dialect island through the bounded, noninteractive reference boundary.
///
/// Standard input is closed. `SIGINT` cancels the interpreter, and on Unix
/// `SIGTSTP` also cancels instead of suspending Quirl without a retained job.
/// The caller owns rendering, so this returns the common command outcome.
pub(crate) struct InteractiveIslandRequest<'a> {
    pub(crate) language: ScriptLanguage,
    pub(crate) source: &'a str,
    pub(crate) source_name: &'a str,
    pub(crate) arguments: &'a [String],
    pub(crate) deadline: ExecutionDeadline,
    pub(crate) max_bytes_per_stream: usize,
    pub(crate) cancellation: &'a ScriptCancellation,
    pub(crate) executor: &'a NativeExecutor,
}

pub(crate) fn run_interactive_island(
    request: InteractiveIslandRequest<'_>,
) -> Result<quirl_core::CommandOutcome, ShellError> {
    #[cfg(unix)]
    let signals = [signal_hook::consts::SIGINT, signal_hook::consts::SIGTSTP];
    #[cfg(not(unix))]
    let signals = [signal_hook::consts::SIGINT];
    let _signal_guard = ScriptSignalGuard::register(
        &signals,
        Arc::clone(&request.cancellation.cancelled),
        "could not install interactive island cancellation handlers",
    )?;
    let executable = request.language.executable();
    run_reference_script(ReferenceScriptRequest {
        source: request.source,
        source_name: request.source_name,
        language: request.language,
        arguments: request.arguments,
        cancellation: request.cancellation,
        executable,
        deadline: request.deadline.expires_at(),
        max_bytes_per_stream: request.max_bytes_per_stream,
        executor: Some(request.executor),
    })
}

/// Execute a validated script plan without introducing a script-specific
/// status or result envelope.
pub(crate) fn execute_plan(
    executor: &mut NativeExecutor,
    plan: &ExecutionPlan,
) -> Result<ExecutionOutcome, ShellError> {
    match plan.mode() {
        ExecutionMode::LuaScript => execute_lua_script_plan(plan),
        ExecutionMode::QuirlScript => execute_quirl_script_plan(executor, plan),
        _ => Err(ShellError::new(
            ErrorCode::Validation,
            "script adapter received a non-script execution mode",
        )
        .with_command(plan.source().text())
        .with_help("Dispatch the validated plan through its selected execution adapter")),
    }
}

fn execute_lua_script_plan(plan: &ExecutionPlan) -> Result<ExecutionOutcome, ShellError> {
    if plan.output() != ExecutionOutputTarget::Value {
        return Err(script_representation_error(plan, "a structured value"));
    }
    let mut policy = LuaPolicy::script();
    policy.wall_time = policy.wall_time.min(
        plan.deadline()
            .ensure_remaining("before Lua script initialization")?,
    );
    let runtime = if plan
        .declared_effects()
        .contains(quirl_core::ExecutionEffect::SpawnProcess)
    {
        LuaRuntime::new_with_process_host_and_cancellation(
            policy,
            sandboxed_process_host(),
            plan.cancellation().atomic(),
        )?
    } else {
        LuaRuntime::new_with_cancellation(policy, plan.cancellation().atomic())?
    };
    plan.ensure_active("after Lua script initialization")?;
    let context = LuaRunnerContext::from_current_process(
        plan.arguments(),
        plan.input().clone(),
        plan.output(),
        plan.declared_effects(),
        plan.cancellation().atomic(),
    )?;
    let outcome =
        runtime.run_source_with_context(plan.source().text(), plan.source().name(), &context)?;
    plan.ensure_active("after Lua script execution")?;
    Ok(outcome)
}

fn execute_quirl_script_plan(
    executor: &mut NativeExecutor,
    plan: &ExecutionPlan,
) -> Result<ExecutionOutcome, ShellError> {
    if plan.output() != ExecutionOutputTarget::Value {
        return Err(script_representation_error(plan, "a structured value"));
    }
    if !plan
        .declared_effects()
        .contains(quirl_core::ExecutionEffect::SpawnProcess)
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "Quirl script plan does not declare process authority",
        )
        .with_command(plan.source().text())
        .with_help("Declare and allow process spawning before executing a Quirl script"));
    }
    let data = DataRuntime::with_process_host(sandboxed_process_host());
    let cancelled = plan.cancellation().atomic();
    let mut value = StructuredValue::from_json(json!({
        "args": plan.arguments(),
        "status": 0,
    }));
    let mut status = 0;
    for statement in native_script_statements(plan.source().text(), plan.source().name())? {
        plan.ensure_active("between Quirl script statements")?;
        let statement_source = &plan.source().text()[statement.body.clone()];
        match statement.kind {
            NativeStatementKind::Data => {
                let envelope = data
                    .eval_output_with_cancellation_handle(
                        statement_source.trim(),
                        Arc::clone(&cancelled),
                    )?
                    .into_envelope(&cancelled)
                    .map_err(|error| {
                        error.with_label(
                            Some(plan.source().name().to_owned()),
                            statement.body.start,
                            statement.body.end,
                            "failed Quirl data statement",
                        )
                    })?;
                value = match envelope {
                    quirl_data::DataEnvelope::Value { value } => value,
                    quirl_data::DataEnvelope::Stream { items } => StructuredValue::List(items),
                    quirl_data::DataEnvelope::Option { .. }
                    | quirl_data::DataEnvelope::Result { .. }
                    | quirl_data::DataEnvelope::Task { .. } => {
                        return Err(ShellError::new(
                            ErrorCode::Data,
                            "Quirl script data statement produced a value that still needs unwrapping",
                        )
                        .with_help(
                            "Call .unwrap(), .resolve(), or .await on the Option, Result, or Task before returning it",
                        ));
                    }
                };
            }
            NativeStatementKind::Command => {
                let outcome = if statement.explicit {
                    execute_command_block(executor, plan, statement.body.clone())?
                } else {
                    execute_native_command(
                        executor,
                        plan,
                        statement_source,
                        statement.body.clone(),
                    )?
                };
                status = outcome.status;
                value = StructuredValue::from_json(json!({
                    "status": outcome.status,
                    "stdout": outcome.stdout.unwrap_or_default(),
                    "stderr": outcome.stderr.unwrap_or_default(),
                }));
                if status != 0 {
                    break;
                }
            }
        }
        value.validate()?;
    }
    plan.ensure_active("after Quirl script execution")?;
    ExecutionOutcome::new(
        ExecutionStatus::Exited(status),
        ExecutionOutput::Value { value },
        Vec::new(),
        ExecutionCleanupState::Complete,
    )
}

fn script_representation_error(plan: &ExecutionPlan, produced: &str) -> ShellError {
    let requested = match plan.output() {
        ExecutionOutputTarget::Inherit => "inherited output",
        ExecutionOutputTarget::Capture { .. } => "captured byte output",
        ExecutionOutputTarget::Value => "a structured value",
    };
    ShellError::new(
        ErrorCode::Validation,
        format!("script produced {produced} but the execution request expects {requested}"),
    )
    .with_command(plan.source().text())
    .with_help("Report this as a Quirl execution-plan mismatch")
}

impl ScriptLanguage {
    fn executable(self) -> &'static str {
        match self {
            Self::Lua => "lua",
            Self::Quirl => "quirl",
            Self::Bash => "bash",
            Self::Zsh => "zsh",
        }
    }
}

pub fn detect_language(
    source: &str,
    path: Option<&Path>,
    requested_language: Option<ScriptLanguage>,
) -> Result<ScriptLanguage, ShellError> {
    if let Some(language) = requested_language {
        return Ok(language);
    }
    if let Some(language) = source
        .lines()
        .next()
        .map(language_from_shebang)
        .transpose()?
        .flatten()
    {
        return Ok(language);
    }
    if let Some(extension) = path
        .and_then(Path::extension)
        .and_then(|value| value.to_str())
    {
        return match extension {
            "lua" => Ok(ScriptLanguage::Lua),
            extension if is_quirl_extension(extension) => Ok(ScriptLanguage::Quirl),
            "sh" | "bash" => Ok(ScriptLanguage::Bash),
            "zsh" => Ok(ScriptLanguage::Zsh),
            _ => Err(unsupported_language_error(Some(extension))),
        };
    }
    Err(unsupported_language_error(None))
}

fn language_from_shebang(line: &str) -> Result<Option<ScriptLanguage>, ShellError> {
    let Some(shebang) = line.strip_prefix("#!") else {
        return Ok(None);
    };
    let shebang = shebang.trim();
    let words: Vec<_> = shebang.split_whitespace().collect();
    for (index, word) in words.iter().enumerate() {
        if let Some(language) = word.strip_prefix("--lang=") {
            return language_name(language)
                .map(Some)
                .ok_or_else(|| unsupported_shebang_language(language));
        }
        if *word == "--lang" {
            let language = words.get(index + 1).ok_or_else(|| {
                ShellError::new(
                    ErrorCode::InvalidArgument,
                    "script shebang has `--lang` without a language",
                )
                .with_help("Use `--lang lua|quirl|bash|zsh` in the shebang")
            })?;
            return language_name(language)
                .map(Some)
                .ok_or_else(|| unsupported_shebang_language(language));
        }
    }
    for word in &words {
        let Some(executable) = Path::new(word).file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if matches!(executable, "lua" | "lua5.4") {
            return Ok(Some(ScriptLanguage::Lua));
        } else if executable == "quirl-script" {
            return Ok(Some(ScriptLanguage::Quirl));
        } else if executable == "bash" {
            return Ok(Some(ScriptLanguage::Bash));
        } else if executable == "zsh" {
            return Ok(Some(ScriptLanguage::Zsh));
        } else if executable == "sh" {
            return Err(unsupported_shebang_language(executable));
        }
    }
    Ok(None)
}

fn language_name(language: &str) -> Option<ScriptLanguage> {
    match language.to_ascii_lowercase().as_str() {
        "lua" | "lua5.4" => Some(ScriptLanguage::Lua),
        "quirl" => Some(ScriptLanguage::Quirl),
        "bash" => Some(ScriptLanguage::Bash),
        "zsh" => Some(ScriptLanguage::Zsh),
        _ => None,
    }
}

fn unsupported_language_error(extension: Option<&str>) -> ShellError {
    let context = extension.map_or_else(
        || "the script has no recognized shebang or extension".to_owned(),
        |extension| format!("the .{extension} extension has no registered runner"),
    );
    ShellError::new(
        ErrorCode::InvalidArgument,
        "cannot determine the script language",
    )
    .with_context(context)
    .with_help(
        "Use a .lua, .qrl, .quirl, .🌀, .sh, .bash, or .zsh file, a recognized shebang, or pass `--lang lua|quirl|bash|zsh`",
    )
}

fn unsupported_shebang_language(language: &str) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("script shebang requests unsupported language `{language}`"),
    )
    .with_help("Use `--lang bash` or `--lang zsh`; generic `sh` remains intentionally ambiguous")
}

struct ReferenceScriptRequest<'a> {
    source: &'a str,
    source_name: &'a str,
    language: ScriptLanguage,
    arguments: &'a [String],
    cancellation: &'a ScriptCancellation,
    executable: &'a str,
    deadline: Instant,
    max_bytes_per_stream: usize,
    executor: Option<&'a NativeExecutor>,
}

fn run_reference_script(
    request: ReferenceScriptRequest<'_>,
) -> Result<quirl_core::CommandOutcome, ShellError> {
    let ReferenceScriptRequest {
        source,
        source_name,
        language,
        arguments,
        cancellation,
        executable,
        deadline,
        max_bytes_per_stream,
        executor,
    } = request;
    if max_bytes_per_stream == 0 || max_bytes_per_stream > EXECUTION_CAPTURE_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "reference runner capture limit is outside the shared contract",
        )
        .with_context(format!(
            "maximum: {EXECUTION_CAPTURE_BYTES_MAX}; requested: {max_bytes_per_stream}"
        ))
        .with_help("Use the validated execution plan capture ceiling"));
    }
    let mut command = Command::new(executable);
    if let Some(executor) = executor {
        executor.configure_child(&mut command)?;
    }
    match language {
        ScriptLanguage::Bash => {
            command.args(["--noprofile", "--norc", "-c", source, source_name]);
            command.env_remove("BASH_ENV").env_remove("ENV");
        }
        ScriptLanguage::Zsh => {
            command.args(["-f", "-c", source, source_name]);
            command.env_remove("ZDOTDIR").env_remove("ENV");
        }
        _ => {
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                "reference runner requires Bash or Zsh",
            )
            .with_help("Report this as a Quirl script-language routing defect"));
        }
    }
    // The syntax-error classifier matches untranslated interpreter
    // diagnostics, so pin the message locale; localized environments would
    // otherwise turn labeled syntax errors into plain nonzero statuses.
    command.env("LC_ALL", "C");
    command
        .args(arguments)
        // Reference runners are a bounded compatibility service, not a second
        // interactive job-control implementation. Closing stdin prevents a
        // background process group from stopping forever on a terminal read.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let containment = ChildProcessTree::new()?;
    containment.configure(&mut command);
    let mut child = command.spawn().map_err(|error| {
        ShellError::new(
            ErrorCode::ProcessSpawn,
            format!("could not start reference interpreter `{executable}`"),
        )
        .with_context(error.to_string())
        .with_label(
            Some(source_name.to_owned()),
            0,
            source.lines().next().map_or(0, str::len),
            "interpreter selected here",
        )
        .with_help(format!(
            "Install {executable}, choose another `--lang`, or fix the script shebang"
        ))
    })?;
    containment.assign(&mut child).map_err(|error| {
        let _ = child.kill();
        let _ = child.wait();
        error.with_label(
            Some(source_name.to_owned()),
            0,
            source.lines().next().map_or(0, str::len),
            "reference interpreter containment failed",
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .map(|stream| spawn_stream_reader(stream, max_bytes_per_stream));
    let stderr = child
        .stderr
        .take()
        .map(|stream| spawn_stream_reader(stream, max_bytes_per_stream));
    let status = loop {
        if cancellation.cancelled.load(Ordering::Relaxed) {
            let termination = terminate_reference_process(&mut child, &containment);
            let _ = join_stream_reader(stdout, "reference runner stdout");
            let _ = join_stream_reader(stderr, "reference runner stderr");
            termination?;
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!("{executable} script execution was cancelled"),
            )
            .with_label(
                Some(source_name.to_owned()),
                0,
                source.len(),
                "cancelled reference script",
            )
            .with_help("Run the script again when cancellation is no longer requested"));
        }
        if Instant::now() >= deadline {
            let termination = terminate_reference_process(&mut child, &containment);
            let _ = join_stream_reader(stdout, "reference runner stdout");
            let _ = join_stream_reader(stderr, "reference runner stderr");
            termination?;
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!("{executable} script execution exceeded its absolute deadline"),
            )
            .with_label(
                Some(source_name.to_owned()),
                0,
                source.len(),
                "reference script deadline expired",
            )
            .with_help("Reduce script work or increase the shared execution deadline"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Err(error) => {
                let termination = terminate_reference_process(&mut child, &containment);
                termination?;
                return Err(ShellError::new(
                    ErrorCode::Io,
                    format!("could not observe `{executable}` script status"),
                )
                .with_context(error.to_string())
                .with_help(
                    "Retry the script; report this if the interpreter remains unobservable",
                ));
            }
        }
    };
    containment.terminate(&mut child)?;
    let stdout = join_stream_reader(stdout, "reference runner stdout")?;
    let stderr = join_stream_reader(stderr, "reference runner stderr")?;
    if Instant::now() >= deadline {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("{executable} script execution exceeded its absolute deadline"),
        )
        .with_label(
            Some(source_name.to_owned()),
            0,
            source.len(),
            "reference script deadline expired",
        )
        .with_help("Reduce script work or increase the shared execution deadline"));
    }
    ensure_reference_capture_within_limit(executable, "stdout", &stdout, max_bytes_per_stream)?;
    ensure_reference_capture_within_limit(executable, "stderr", &stderr, max_bytes_per_stream)?;
    let status_code = shell_status(status);
    if status_code != 0 && is_dialect_syntax_error(&stderr.value) {
        let (start, end) = dialect_error_span(source, &stderr.value);
        return Err(ShellError::new(
            ErrorCode::InvalidCommand,
            format!("{executable} rejected the script syntax"),
        )
        .with_context(truncate_capture(&stderr.value))
        .with_label(
            Some(source_name.to_owned()),
            start,
            end,
            format!("{executable} dialect error"),
        )
        .with_help(format!(
            "Run `{executable} -n {source_name}` for the interpreter's full syntax check"
        )));
    }
    Ok(quirl_core::CommandOutcome {
        status: status_code,
        stdout: Some(stdout.value),
        stderr: Some(stderr.value),
    })
}

fn shell_status(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    if let Some(signal) = status.signal() {
        return 128_i32.checked_add(signal).unwrap_or(1);
    }
    1
}

fn terminate_reference_process(
    child: &mut std::process::Child,
    containment: &ChildProcessTree,
) -> Result<(), ShellError> {
    let result = containment.terminate(child);
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

#[derive(Debug)]
struct StreamCapture {
    value: String,
    discarded_bytes: u64,
}

fn spawn_stream_reader(
    mut stream: impl Read + Send + 'static,
    max_bytes: usize,
) -> thread::JoinHandle<io::Result<StreamCapture>> {
    thread::spawn(move || {
        let mut retained = Vec::with_capacity(max_bytes);
        let mut discarded_bytes = 0_u64;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let available = max_bytes.saturating_sub(retained.len());
            let keep = available.min(read);
            retained.extend_from_slice(&buffer[..keep]);
            discarded_bytes = discarded_bytes.saturating_add((read - keep) as u64);
        }
        Ok(StreamCapture {
            value: String::from_utf8_lossy(&retained).into_owned(),
            discarded_bytes,
        })
    })
}

fn ensure_reference_capture_within_limit(
    executable: &str,
    stream: &str,
    capture: &StreamCapture,
    limit: usize,
) -> Result<(), ShellError> {
    if capture.discarded_bytes == 0 {
        return Ok(());
    }
    let observed = u64::try_from(capture.value.len())
        .unwrap_or(u64::MAX)
        .saturating_add(capture.discarded_bytes);
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{executable} {stream} exceeded the execution capture limit"),
    )
    .with_context(format!(
        "limit: {limit}; retained: {}; discarded: {}; observed: {observed}",
        capture.value.len(),
        capture.discarded_bytes
    ))
    .with_help("Reduce output, redirect it to a file, or request a larger valid capture ceiling"))
}

#[cfg(test)]
fn script_deadline() -> Result<Instant, ShellError> {
    Instant::now()
        .checked_add(SCRIPT_EXECUTION_DEADLINE)
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::ResourceLimit,
                "script deadline is outside the monotonic clock range",
            )
            .with_help("Retry with the configured bounded script deadline")
        })
}

fn join_stream_reader(
    reader: Option<thread::JoinHandle<io::Result<StreamCapture>>>,
    description: &str,
) -> Result<StreamCapture, ShellError> {
    let Some(reader) = reader else {
        return Ok(StreamCapture {
            value: String::new(),
            discarded_bytes: 0,
        });
    };
    match reader.join() {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(ShellError::new(
            ErrorCode::Io,
            format!("could not read {description}"),
        )
        .with_context(error.to_string())
        .with_help("Retry the script; report repeated output capture failures")),
        Err(_) => Err(
            ShellError::new(ErrorCode::Io, format!("{description} task failed"))
                .with_help("Retry the script; report repeated output capture failures"),
        ),
    }
}

fn is_dialect_syntax_error(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();
    [
        "syntax error",
        "parse error",
        "unexpected eof",
        "unexpected end",
    ]
    .iter()
    .any(|needle| stderr.contains(needle))
}

fn dialect_error_span(source: &str, stderr: &str) -> (usize, usize) {
    let line = stderr
        .split(|character: char| !character.is_ascii_digit())
        .filter_map(|digits| digits.parse::<usize>().ok())
        .find(|line| *line > 0 && *line <= source.lines().count())
        .unwrap_or(1);
    let start = source
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum();
    let end = start
        + source
            .lines()
            .nth(line.saturating_sub(1))
            .map_or(0, str::len);
    (start, end)
}

fn truncate_capture(value: &str) -> String {
    const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;
    if value.len() <= MAX_DIAGNOSTIC_BYTES {
        value.to_owned()
    } else {
        let boundary = (0..=MAX_DIAGNOSTIC_BYTES)
            .rev()
            .find(|index| value.is_char_boundary(*index))
            .unwrap_or(0);
        format!("{}…", &value[..boundary])
    }
}

#[cfg(test)]
fn run_quirl_source(
    source: &str,
    source_name: &str,
    arguments: &[String],
) -> Result<ScriptRunOutput, ShellError> {
    let request = ExecutionRequest::new(
        ExecutionSource::new(source_name, source)?,
        ExecutionMode::QuirlScript,
    )
    .with_arguments(arguments.to_vec())
    .with_deadline(SCRIPT_EXECUTION_DEADLINE)
    .with_output(ExecutionOutputTarget::Value)
    .with_effects(ExecutionEffects::all(), ExecutionEffects::all());
    let plan = request.plan()?;
    let mut executor = NativeExecutor::default();
    let outcome = execute_quirl_script_plan(&mut executor, &plan)?;
    let status = outcome.status_code();
    let value = match outcome.output {
        ExecutionOutput::Value { value } => value.json_value(),
        ExecutionOutput::Values { values } => {
            Value::Array(values.into_iter().map(|value| value.json_value()).collect())
        }
        ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => {
            return Err(script_representation_error(&plan, "non-structured output"));
        }
    };
    Ok(ScriptRunOutput { status, value })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeStatementKind {
    Data,
    Command,
}

#[derive(Debug, Clone)]
struct NativeScriptStatement {
    kind: NativeStatementKind,
    /// The source range that identifies this statement, including explicit delimiters.
    span: Range<usize>,
    /// The expression or command text, excluding explicit block delimiters.
    body: Range<usize>,
    explicit: bool,
}

#[derive(Debug)]
struct OpenNativeBlock {
    kind: NativeStatementKind,
    span_start: usize,
    opener_span: Range<usize>,
    body_start: usize,
    indentation: String,
}

/// Parse explicit native script boundaries before handing command text to the shared grammar.
///
/// Delimiters intentionally occupy complete lines. A closing `}` must have the same indentation
/// as its opening `data {` or `command {`; this keeps indented JSON/object syntax in a data body
/// unambiguous without introducing another escaping convention.
fn native_script_statements(
    source: &str,
    source_name: &str,
) -> Result<Vec<NativeScriptStatement>, ShellError> {
    let mut statements = Vec::new();
    let mut open: Option<OpenNativeBlock> = None;
    let mut offset = 0;

    for (line_index, raw_line) in source.split_inclusive('\n').enumerate() {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let leading_len = line.len().saturating_sub(line.trim_start().len());
        let trimmed = line.trim();
        let trimmed_start = offset + leading_len;
        let trimmed_end = trimmed_start + trimmed.len();

        if let Some(block) = &open {
            if is_native_block_opener(trimmed).is_some() {
                return Err(native_block_error(
                    source_name,
                    trimmed_start..trimmed_end,
                    "native script blocks cannot nest",
                    "Close the current block before opening another `data {` or `command {` block",
                ));
            }
            if is_aligned_block_closer(line, &block.indentation) {
                let Some(block) = open.take() else {
                    return Err(native_block_error(
                        source_name,
                        trimmed_start..trimmed_end,
                        "native script block state was lost",
                        "Retry the script; report this internal parser state failure",
                    ));
                };
                if source[block.body_start..offset].trim().is_empty() {
                    return Err(native_block_error(
                        source_name,
                        block.opener_span,
                        "native script block is empty",
                        "Put a data expression or command statement between the delimiters",
                    ));
                }
                statements.push(NativeScriptStatement {
                    kind: block.kind,
                    span: block.span_start..(offset + raw_line.len()),
                    body: block.body_start..offset,
                    explicit: true,
                });
                offset += raw_line.len();
                continue;
            }
            if block.kind == NativeStatementKind::Command && trimmed == "}" {
                return Err(native_block_error(
                    source_name,
                    trimmed_start..trimmed_end,
                    "command block terminator is ambiguously indented",
                    "Align the closing `}` with the `command {` line",
                ));
            }
            offset += raw_line.len();
            continue;
        }

        if line_index == 0 && trimmed.starts_with("#!") {
            offset += raw_line.len();
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            offset += raw_line.len();
            continue;
        }
        if let Some(kind) = is_native_block_opener(trimmed) {
            open = Some(OpenNativeBlock {
                kind,
                span_start: trimmed_start,
                opener_span: trimmed_start..trimmed_end,
                body_start: offset + raw_line.len(),
                indentation: line[..leading_len].to_owned(),
            });
            offset += raw_line.len();
            continue;
        }
        if trimmed == "}" {
            return Err(native_block_error(
                source_name,
                trimmed_start..trimmed_end,
                "native script block closes without an opening delimiter",
                "Open a `data {` or `command {` block first, or remove this `}`",
            ));
        }
        if looks_like_ambiguous_command_block(trimmed) {
            return Err(native_block_error(
                source_name,
                trimmed_start..trimmed_end,
                "explicit command blocks must open on their own line",
                "Write `command {` on one line, put commands below it, and align a closing `}`",
            ));
        }

        let kind = if quirl_syntax::data_statement_expression(trimmed).is_some() {
            NativeStatementKind::Data
        } else {
            NativeStatementKind::Command
        };
        let body_start = match kind {
            NativeStatementKind::Data => {
                trimmed_start + trimmed.len()
                    - quirl_syntax::data_statement_expression(trimmed).map_or(0, str::len)
            }
            NativeStatementKind::Command => trimmed_start,
        };
        statements.push(NativeScriptStatement {
            kind,
            span: trimmed_start..trimmed_end,
            body: body_start..trimmed_end,
            explicit: false,
        });
        offset += raw_line.len();
    }

    if let Some(block) = open {
        return Err(native_block_error(
            source_name,
            block.opener_span,
            "native script block is not closed",
            "Add an aligned closing `}` for this `data {` or `command {` block",
        ));
    }
    Ok(statements)
}

fn is_native_block_opener(line: &str) -> Option<NativeStatementKind> {
    [
        ("data", NativeStatementKind::Data),
        ("command", NativeStatementKind::Command),
    ]
    .into_iter()
    .find_map(|(keyword, kind)| {
        line.strip_prefix(keyword)
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .and_then(|rest| (rest.trim() == "{").then_some(kind))
    })
}

fn looks_like_ambiguous_command_block(line: &str) -> bool {
    line.strip_prefix("command")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .is_some_and(|rest| rest.trim_start().starts_with('{'))
}

fn is_aligned_block_closer(line: &str, indentation: &str) -> bool {
    line.strip_suffix('\r').unwrap_or(line).trim_end() == format!("{indentation}}}")
}

fn native_block_error(
    source_name: &str,
    span: Range<usize>,
    message: impl Into<String>,
    help: impl Into<String>,
) -> ShellError {
    ShellError::new(ErrorCode::InvalidCommand, message)
        .with_label(
            Some(source_name.to_owned()),
            span.start,
            span.end,
            "native script block",
        )
        .with_help(help)
}

fn script_without_explicit_blocks(source: &str, statements: &[NativeScriptStatement]) -> String {
    let mut bytes = source.as_bytes().to_vec();
    for statement in statements.iter().filter(|statement| statement.explicit) {
        for byte in &mut bytes[statement.span.clone()] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    // Replacing valid UTF-8 bytes only with ASCII space preserves UTF-8 validity.
    String::from_utf8_lossy(&bytes).into_owned()
}

fn command_block_diagnostics(
    source: &str,
    statement: &NativeScriptStatement,
) -> Vec<quirl_syntax::CommandSyntaxError> {
    let mut diagnostics = Vec::new();
    let mut offset = statement.body.start;
    for raw_line in source[statement.body.clone()].split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let leading = line.len().saturating_sub(line.trim_start().len());
        let trimmed = line.trim();
        if !trimmed.is_empty()
            && !trimmed.starts_with('#')
            && let Err(mut error) = parse_command_list(trimmed)
        {
            error.start += offset + leading;
            error.end += offset + leading;
            diagnostics.push(error);
        }
        offset += raw_line.len();
    }
    diagnostics
}

fn execute_command_block(
    executor: &mut NativeExecutor,
    plan: &ExecutionPlan,
    body: Range<usize>,
) -> Result<quirl_core::CommandOutcome, ShellError> {
    let mut outcome = quirl_core::CommandOutcome {
        status: 0,
        stdout: Some(String::new()),
        stderr: Some(String::new()),
    };
    let mut offset = body.start;
    for raw_line in plan.source().text()[body].split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let leading = line.len().saturating_sub(line.trim_start().len());
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            let line_outcome = execute_native_command(
                executor,
                plan,
                trimmed,
                (offset + leading)..(offset + leading + trimmed.len()),
            )?;
            outcome.status = line_outcome.status;
            if let Some(stdout) = line_outcome.stdout {
                append_script_capture(
                    outcome.stdout.get_or_insert_with(String::new),
                    &stdout,
                    "stdout",
                )?;
            }
            if let Some(stderr) = line_outcome.stderr {
                append_script_capture(
                    outcome.stderr.get_or_insert_with(String::new),
                    &stderr,
                    "stderr",
                )?;
            }
            if outcome.status != 0 {
                return Ok(outcome);
            }
        }
        offset += raw_line.len();
    }
    Ok(outcome)
}

fn execute_native_command(
    executor: &mut NativeExecutor,
    plan: &ExecutionPlan,
    command: &str,
    span: Range<usize>,
) -> Result<quirl_core::CommandOutcome, ShellError> {
    let request = ProcessRequest {
        command: command.to_owned(),
        deadline: plan
            .deadline()
            .ensure_remaining("before Quirl script command startup")?,
        cancelled: plan.cancellation().atomic(),
        max_output_bytes: EXECUTION_CAPTURE_BYTES_MAX,
    };
    executor.execute_capture_request(request).map_err(|error| {
        error.with_label(
            Some(plan.source().name().to_owned()),
            span.start,
            span.end,
            "failed Quirl command statement",
        )
    })
}

fn append_script_capture(
    retained: &mut String,
    next: &str,
    stream: &str,
) -> Result<(), ShellError> {
    let observed = retained.len().saturating_add(next.len());
    if observed > EXECUTION_CAPTURE_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("Quirl script aggregate {stream} exceeded its capture limit"),
        )
        .with_context(format!(
            "limit: {EXECUTION_CAPTURE_BYTES_MAX}; observed: {observed}"
        ))
        .with_help("Reduce command-block output or redirect intermediate output to a file"));
    }
    retained.push_str(next);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory(name: &str) -> PathBuf {
        let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("quirl-script-{name}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn reference_interpreter_available(executable: &str) -> bool {
        Command::new(executable)
            .args(["-c", ":"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[cfg(unix)]
    #[test]
    fn signal_guard_unregisters_after_partial_install_failure_and_drop() {
        let partially_registered = Arc::new(AtomicBool::new(false));
        let mut registrations = 0_usize;
        let error = ScriptSignalGuard::register_with(
            &[signal_hook::consts::SIGWINCH, signal_hook::consts::SIGWINCH],
            Arc::clone(&partially_registered),
            "injected signal install failure",
            |signal, cancelled| {
                registrations += 1;
                if registrations == 2 {
                    return Err(io::Error::other("injected second registration failure"));
                }
                signal_hook::flag::register(signal, cancelled)
            },
        )
        .unwrap_err();
        assert!(error.message.contains("injected"));
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        assert!(!partially_registered.load(Ordering::Relaxed));

        let dropped_registration = Arc::new(AtomicBool::new(false));
        let guard = ScriptSignalGuard::register(
            &[signal_hook::consts::SIGWINCH],
            Arc::clone(&dropped_registration),
            "could not install test signal handler",
        )
        .unwrap();
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        assert!(dropped_registration.load(Ordering::Relaxed));
        dropped_registration.store(false, Ordering::Relaxed);
        drop(guard);
        signal_hook::low_level::raise(signal_hook::consts::SIGWINCH).unwrap();
        assert!(!dropped_registration.load(Ordering::Relaxed));
    }

    #[test]
    fn explicit_language_wins_over_shebang_and_extension() {
        assert_eq!(
            detect_language(
                "#!/usr/bin/env lua\nreturn 1",
                Some(Path::new("script.lua")),
                Some(ScriptLanguage::Quirl),
            )
            .unwrap(),
            ScriptLanguage::Quirl
        );
    }

    #[test]
    fn shebang_language_wins_over_extension() {
        assert_eq!(
            detect_language(
                "#!/usr/bin/env -S quirl run --lang quirl\npwd",
                Some(Path::new("script.lua")),
                None,
            )
            .unwrap(),
            ScriptLanguage::Quirl
        );
    }

    #[test]
    fn stdin_without_language_or_shebang_is_rejected() {
        let error = detect_language("return 42", None, None).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.details.help[0].contains("--lang"));
    }

    #[test]
    fn unsupported_shebang_language_is_not_silently_overridden_by_extension() {
        let error = detect_language(
            "#!/usr/bin/env sh\necho nope",
            Some(Path::new("misleading.lua")),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains("unsupported language"));
    }

    #[test]
    fn reference_extensions_and_shebangs_select_the_exact_dialect() {
        assert_eq!(
            detect_language("echo bash", Some(Path::new("script.sh")), None).unwrap(),
            ScriptLanguage::Bash
        );
        assert_eq!(
            detect_language("echo bash", Some(Path::new("script.bash")), None).unwrap(),
            ScriptLanguage::Bash
        );
        assert_eq!(
            detect_language("#!/usr/bin/env zsh\necho zsh", None, None).unwrap(),
            ScriptLanguage::Zsh
        );
    }

    #[test]
    fn native_quirl_extensions_select_the_same_language() {
        for path in ["script.qrl", "script.quirl", "script.🌀"] {
            assert_eq!(
                detect_language("pwd", Some(Path::new(path)), None).unwrap(),
                ScriptLanguage::Quirl,
                "{path}"
            );
        }
    }

    #[test]
    fn reference_runners_preserve_arguments_cwd_environment_status_and_captures() {
        for (language, executable) in [(ScriptLanguage::Bash, "bash"), (ScriptLanguage::Zsh, "zsh")]
        {
            if !reference_interpreter_available(executable) {
                eprintln!("skipping {executable}: interpreter is unavailable");
                continue;
            }
            let output = run_source(
                "printf 'arg=%s\\n' \"$1\"; printf 'cwd=%s\\n' \"$PWD\"; printf 'env=%s\\n' \"${PATH:+set}\"; printf 'problem\\n' >&2; exit 7",
                "contract-test",
                None,
                Some(language),
                &["expected".to_owned()],
            )
            .unwrap();
            assert_eq!(output.status, 7, "{executable}");
            assert_eq!(
                output.value["stdout"],
                format!(
                    "arg=expected\ncwd={}\nenv=set\n",
                    std::env::current_dir().unwrap().display()
                ),
                "{executable}"
            );
            assert_eq!(output.value["stderr"], "problem\n", "{executable}");
            assert_eq!(output.value["language"], executable, "{executable}");
        }
    }

    #[test]
    fn run_front_door_returns_shared_outcomes_for_script_engines() {
        let directory = test_directory("shared-run-outcome");
        fs::create_dir_all(&directory).unwrap();
        let cases = [
            (
                "runner.lua",
                r#"return { abi_version = 1, main = function(ctx)
                  return { abi_version = 1, ok = true, status = 7,
                    output = { kind = "value", value = { type = "string", value = ctx.args[1] } } }
                end }"#,
                Some(ScriptLanguage::Lua),
                7,
            ),
            ("runner.qrl", "data 42", Some(ScriptLanguage::Quirl), 0),
        ];
        for (name, source, language, expected_status) in cases {
            let path = directory.join(name);
            fs::write(&path, source).unwrap();
            let (request, _signals) =
                execution_request(&path, language, &["argument".to_owned()]).unwrap();
            let outcome =
                crate::execute_execution_request(&mut NativeExecutor::default(), request, None)
                    .unwrap();
            assert_eq!(outcome.status_code(), expected_status, "{name}");
            assert!(matches!(outcome.output, ExecutionOutput::Value { .. }));
        }

        for (name, language, executable) in [
            ("runner.bash", ScriptLanguage::Bash, "bash"),
            ("runner.zsh", ScriptLanguage::Zsh, "zsh"),
        ] {
            if !reference_interpreter_available(executable) {
                eprintln!("skipping {executable} run outcome: executable is unavailable");
                continue;
            }
            let path = directory.join(name);
            fs::write(&path, "printf '%s' \"$1\"; exit 9").unwrap();
            let (request, _signals) =
                execution_request(&path, Some(language), &["argument".to_owned()]).unwrap();
            let outcome =
                crate::execute_execution_request(&mut NativeExecutor::default(), request, None)
                    .unwrap();
            assert_eq!(outcome.status_code(), 9);
            assert!(matches!(
                outcome.output,
                ExecutionOutput::Bytes { ref stdout, .. } if stdout == b"argument"
            ));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reference_runner_drains_but_does_not_retain_unbounded_output() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let cancellation = ScriptCancellation::default();
        let error = run_reference_script(ReferenceScriptRequest {
            source: "i=0; while [ \"$i\" -lt 7000 ]; do printf 0123456789; printf abcdefghij >&2; i=$((i+1)); done",
            source_name: "bounded.bash",
            language: ScriptLanguage::Bash,
            arguments: &[],
            cancellation: &cancellation,
            executable: "bash",
            deadline: script_deadline().unwrap(),
            max_bytes_per_stream: 64 * 1024,
            executor: None,
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("discarded"));
    }

    #[test]
    fn reference_runners_close_stdin_under_the_noninteractive_policy() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let output = run_source(
            "if read -r value; then printf unexpected; else printf stdin-closed; fi",
            "noninteractive.bash",
            None,
            Some(ScriptLanguage::Bash),
            &[],
        )
        .unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.value["stdout"], "stdin-closed");
    }

    #[test]
    fn missing_reference_interpreter_has_a_labeled_actionable_error() {
        let cancellation = ScriptCancellation::default();
        let error = run_reference_script(ReferenceScriptRequest {
            source: "echo unreachable",
            source_name: "missing.bash",
            language: ScriptLanguage::Bash,
            arguments: &[],
            cancellation: &cancellation,
            executable: "/definitely/missing/quirl-reference-interpreter",
            deadline: script_deadline().unwrap(),
            max_bytes_per_stream: EXECUTION_CAPTURE_BYTES_MAX,
            executor: None,
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ProcessSpawn);
        assert_eq!(
            error.details.labels[0].source.as_deref(),
            Some("missing.bash")
        );
        assert!(error.details.help[0].contains("Install"));
    }

    #[test]
    fn reference_dialect_errors_retain_a_source_label() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let source = "echo before\nif then";
        let error =
            run_source(source, "broken.bash", None, Some(ScriptLanguage::Bash), &[]).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert_eq!(
            error.details.labels[0].source.as_deref(),
            Some("broken.bash")
        );
        assert!(error.details.labels[0].start < source.len());
    }

    #[test]
    fn reference_runner_cancellation_terminates_the_interpreter() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let cancellation = ScriptCancellation::default();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            run_source_with_cancellation(
                "sleep 10",
                "cancel.bash",
                None,
                Some(ScriptLanguage::Bash),
                &[],
                &worker_cancellation,
            )
        });
        thread::sleep(Duration::from_millis(20));
        let started = std::time::Instant::now();
        cancellation.cancelled.store(true, Ordering::Relaxed);
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn reference_script_preserves_normal_and_signaled_exit_status() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let normal = run_source(
            "exit 23",
            "normal-status.bash",
            None,
            Some(ScriptLanguage::Bash),
            &[],
        )
        .unwrap();
        let signaled = run_source(
            "kill -TERM $$",
            "signal-status.bash",
            None,
            Some(ScriptLanguage::Bash),
            &[],
        )
        .unwrap();

        assert_eq!(normal.status, 23);
        assert_eq!(signaled.status, 128 + signal_hook::consts::SIGTERM);
    }

    #[cfg(unix)]
    #[test]
    fn reference_script_inherits_explicit_executor_environment() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let name = "QUIRL_REFERENCE_SESSION_ENVIRONMENT";
        assert!(std::env::var_os(name).is_none());
        let mut executor = NativeExecutor::default();
        executor
            .set_environment_variable(name.to_owned(), "executor-owned".to_owned())
            .unwrap();
        let cancellation = ScriptCancellation::default();
        let deadline = ExecutionRequest::new(
            ExecutionSource::new("environment.bash", format!("printf %s \"${name}\"")).unwrap(),
            ExecutionMode::Bash,
        )
        .plan()
        .unwrap()
        .deadline();

        let source = format!("printf %s \"${name}\"");
        let outcome = run_interactive_island(InteractiveIslandRequest {
            language: ScriptLanguage::Bash,
            source: &source,
            source_name: "environment.bash",
            arguments: &[],
            deadline,
            max_bytes_per_stream: EXECUTION_CAPTURE_BYTES_MAX,
            cancellation: &cancellation,
            executor: &executor,
        })
        .unwrap();

        assert_eq!(outcome.stdout.as_deref(), Some("executor-owned"));
        assert!(std::env::var_os(name).is_none());
    }

    #[test]
    fn interactive_dialect_island_observes_cancellation() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let cancellation = ScriptCancellation::default();
        let worker_cancellation = cancellation.clone();
        let deadline = ExecutionRequest::new(
            ExecutionSource::new("island", "sleep 10").unwrap(),
            ExecutionMode::Bash,
        )
        .with_deadline(Duration::from_secs(1))
        .plan()
        .unwrap()
        .deadline();
        let worker = thread::spawn(move || {
            let executor = NativeExecutor::default();
            run_interactive_island(InteractiveIslandRequest {
                language: ScriptLanguage::Bash,
                source: "sleep 10",
                source_name: "island",
                arguments: &[],
                deadline,
                max_bytes_per_stream: EXECUTION_CAPTURE_BYTES_MAX,
                cancellation: &worker_cancellation,
                executor: &executor,
            })
        });
        thread::sleep(Duration::from_millis(20));
        cancellation.cancelled.store(true, Ordering::Relaxed);
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("cancelled"));
    }

    #[test]
    fn lua_stdin_source_runs_with_arguments_under_script_policy() {
        let output = run_source(
            "return { main = function(ctx) return { value = ctx.args[1] } end }",
            "<stdin>",
            None,
            Some(ScriptLanguage::Lua),
            &["argument".to_owned()],
        )
        .unwrap();
        assert_eq!(output.value, json!({ "value": "argument" }));
        assert_eq!(output.status, 0);
    }

    #[test]
    fn lua_typed_runner_preserves_status_and_value_output() {
        let output = run_source(
            r#"return { abi_version = 1, main = function(ctx)
              return { abi_version = 1, ok = true, status = 23,
                output = { kind = "value", value = { type = "string", value = ctx.args[1] } } }
            end }"#,
            "typed.lua",
            None,
            Some(ScriptLanguage::Lua),
            &["typed-value".to_owned()],
        )
        .unwrap();
        assert_eq!(output.status, 23);
        assert_eq!(output.value, json!("typed-value"));
    }

    #[test]
    fn script_cancellation_is_the_lua_runtime_cancellation() {
        let cancellation = ScriptCancellation::default();
        cancellation.cancelled.store(true, Ordering::Relaxed);
        let error = run_source_with_cancellation(
            "return { abi_version = 1, main = function() error('must not run') end }",
            "cancelled.lua",
            None,
            Some(ScriptLanguage::Lua),
            &[],
            &cancellation,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("before Lua module evaluation"));
    }

    #[test]
    fn lua_stdin_dispatch_does_not_bypass_runtime_restrictions() {
        let error = run_source(
            "return os.execute('whoami')",
            "<stdin>",
            None,
            Some(ScriptLanguage::Lua),
            &[],
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(
            error.details.labels[0]
                .message
                .contains("explicit Quirl capability")
        );
    }

    #[test]
    fn quirl_script_stops_at_a_failed_command() {
        let output = run_source(
            "false\nprintf should-not-run",
            "failure.qrl",
            Some(Path::new("failure.qrl")),
            None,
            &[],
        )
        .unwrap();
        assert_ne!(output.status, 0);
        assert_eq!(output.value["stdout"], "");
    }

    #[test]
    fn quirl_script_executes_data_statements_separated_by_tabs() {
        let output = run_source(
            "data\t[1, 2, 3] | length",
            "tabbed-data.qrl",
            Some(Path::new("tabbed-data.qrl")),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.value, json!(3));
    }

    #[test]
    fn native_aliases_run_canonical_multiline_data_blocks() {
        let source = "data {\n  [1, 2, 3] | length\n}\n";
        for path in ["workflow.qrl", "workflow.quirl", "workflow.🌀"] {
            let output = run_source(source, path, Some(Path::new(path)), None, &[]).unwrap();
            assert_eq!(output.status, 0, "{path}");
            assert_eq!(output.value, json!(3), "{path}");
        }
    }

    #[test]
    fn native_command_blocks_execute_each_nonempty_line_in_order() {
        let output = run_source(
            "command {\n  printf first\n  # deliberately ignored\n  printf second\n}\n",
            "workflow.qrl",
            Some(Path::new("workflow.qrl")),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.value["stdout"], "firstsecond");
    }

    #[test]
    fn indented_data_braces_do_not_close_the_outer_block() {
        let output = run_source(
            "data {\n  {\n    \"value\": [1, 2, 3]\n  } | get value | length\n}\n",
            "object.qrl",
            Some(Path::new("object.qrl")),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(output.value, json!(3));
    }

    #[test]
    fn legacy_data_object_expression_remains_line_oriented_compatible() {
        let output = run_source(
            "data {\"value\": [1, 2]} | get value | length",
            "legacy.qrl",
            Some(Path::new("legacy.qrl")),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(output.value, json!(2));
    }

    #[test]
    fn unclosed_native_block_has_a_labeled_actionable_diagnostic() {
        let source = "data {\n  [1, 2]\n";
        let error = run_source(
            source,
            "broken.qrl",
            Some(Path::new("broken.qrl")),
            None,
            &[],
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert_eq!(
            error.details.labels[0].source.as_deref(),
            Some("broken.qrl")
        );
        assert_eq!(error.details.labels[0].start, 0);
        assert!(error.details.help[0].contains("closing `}`"));
    }

    #[test]
    fn nested_native_blocks_are_rejected_at_the_nested_opener() {
        let source = "data {\n  command {\n    printf no\n  }\n}\n";
        let error = check_quirl_source(source, "nested.qrl").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert_eq!(
            error.details.labels[0].start,
            source.find("command {").unwrap()
        );
        assert!(error.message.contains("cannot nest"));
        assert!(error.details.help[0].contains("Close the current block"));
    }

    #[test]
    fn inline_command_block_is_rejected_as_ambiguous() {
        let source = "command { printf no }";
        let error = check_quirl_source(source, "ambiguous.qrl").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert_eq!(error.details.labels[0].start, 0);
        assert!(error.message.contains("own line"));
        assert!(error.details.help[0].contains("command {`"));
    }

    #[test]
    fn command_block_check_offsets_a_body_syntax_error() {
        let source = "command {\n  printf okay |\n}\n";
        let error = check_quirl_source(source, "invalid-command.qrl").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        let label = &error.details.labels[0];
        assert_eq!(label.source.as_deref(), Some("invalid-command.qrl"));
        assert!(label.start >= source.find("printf").unwrap());
        assert!(error.details.help[0].contains("command"));
    }

    #[test]
    fn block_and_compatibility_diagnostics_are_ordered_by_source_span() {
        let source = "command {\n  printf okay |\n}\nprintf later |\n";
        let error = check_quirl_source(source, "ordered.qrl").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert_eq!(error.details.labels.len(), 2);
        assert!(error.details.labels[0].start < error.details.labels[1].start);
        assert_eq!(error.details.labels[0].start, source.find("|\n}").unwrap());
        assert_eq!(error.details.labels[1].start, source.rfind('|').unwrap());
    }

    #[test]
    fn recursive_discovery_accepts_native_aliases_on_unicode_paths() {
        let root = test_directory("discover-über-🌀");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("z.lua"), "return 1").unwrap();
        fs::write(root.join("a.qrl"), "pwd").unwrap();
        fs::write(root.join("readable.quirl"), "pwd").unwrap();
        fs::write(root.join("novelty.🌀"), "pwd").unwrap();
        fs::write(root.join("nested/m.lua"), "return 2").unwrap();
        fs::write(root.join("target/ignored.lua"), "invalid(").unwrap();
        fs::write(root.join(".git/ignored.qrl"), "echo ignored").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("nested"), root.join("linked")).unwrap();

        let files = discover_supported_files(&root, AUTHORING_DISCOVERY_LIMITS).unwrap();
        let relative = files
            .iter()
            .map(|path| path.strip_prefix(&root).unwrap().to_path_buf())
            .collect::<Vec<_>>();
        assert_eq!(
            relative,
            vec![
                PathBuf::from("a.qrl"),
                PathBuf::from("nested/m.lua"),
                PathBuf::from("novelty.🌀"),
                PathBuf::from("readable.quirl"),
                PathBuf::from("z.lua")
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn discovery_limits_for_test() -> ScriptDiscoveryLimits {
        ScriptDiscoveryLimits {
            depth_max: 8,
            directories_max: 16,
            entries_per_directory_max: 16,
            entries_total_max: 64,
            supported_files_max: 16,
            retained_path_bytes_max: 16 * 1024,
            scanned_name_bytes_max: 16 * 1024,
        }
    }

    fn assert_discovery_limit(error: &ShellError, limit: usize, observed: usize) {
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|context| context == &format!("limit: {limit}; observed: {observed}"))
        );
    }

    #[test]
    fn discovery_depth_and_directory_limits_fail_at_the_exact_boundary() {
        let root = test_directory("discovery-depth");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/script.lua"), "return 1\n").unwrap();
        let mut limits = discovery_limits_for_test();
        limits.depth_max = 1;
        let error = discover_supported_files(&root, limits).unwrap_err();
        assert_discovery_limit(&error, 1, 2);
        fs::remove_dir_all(root).unwrap();

        let root = test_directory("discovery-directories");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        fs::write(root.join("a/script.lua"), "return 1\n").unwrap();
        let mut limits = discovery_limits_for_test();
        limits.directories_max = 2;
        let error = discover_supported_files(&root, limits).unwrap_err();
        assert_discovery_limit(&error, 2, 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_entry_and_supported_file_limits_fail_at_the_exact_boundary() {
        let root = test_directory("discovery-entry-per-directory");
        fs::write(root.join("a.lua"), "return 1\n").unwrap();
        fs::write(root.join("b.lua"), "return 2\n").unwrap();
        let mut limits = discovery_limits_for_test();
        limits.entries_per_directory_max = 1;
        let error = discover_supported_files(&root, limits).unwrap_err();
        assert_discovery_limit(&error, 1, 2);
        fs::remove_dir_all(root).unwrap();

        let root = test_directory("discovery-total-entries");
        fs::write(root.join("a.txt"), "ignored\n").unwrap();
        fs::write(root.join("b.lua"), "return 2\n").unwrap();
        let mut limits = discovery_limits_for_test();
        limits.entries_total_max = 1;
        let error = discover_supported_files(&root, limits).unwrap_err();
        assert_discovery_limit(&error, 1, 2);
        fs::remove_dir_all(root).unwrap();

        let root = test_directory("discovery-supported-files");
        fs::write(root.join("a.lua"), "return 1\n").unwrap();
        fs::write(root.join("b.qrl"), "pwd\n").unwrap();
        let mut limits = discovery_limits_for_test();
        limits.supported_files_max = 1;
        let error = discover_supported_files(&root, limits).unwrap_err();
        assert_discovery_limit(&error, 1, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_path_and_scanned_name_byte_limits_bound_retention_before_growth() {
        let root = test_directory("discovery-path-bytes");
        let script = root.join("retained.lua");
        fs::write(&script, "return 1\n").unwrap();
        let root_bytes = root.as_os_str().as_encoded_bytes().len();
        let script_bytes = script.as_os_str().as_encoded_bytes().len();
        let mut limits = discovery_limits_for_test();
        limits.retained_path_bytes_max = root_bytes;
        let error = discover_supported_files(&root, limits).unwrap_err();
        assert_discovery_limit(&error, root_bytes, root_bytes + script_bytes);
        fs::remove_dir_all(root).unwrap();

        let root = test_directory("discovery-name-bytes");
        fs::write(root.join("long-name.lua"), "return 1\n").unwrap();
        let mut limits = discovery_limits_for_test();
        limits.scanned_name_bytes_max = 4;
        let error = discover_supported_files(&root, limits).unwrap_err();
        assert_discovery_limit(&error, 4, "long-name.lua".len());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_reports_permission_and_disappearing_entry_failures() {
        let root = test_directory("discovery-permission");
        let denied = root.join("denied");
        fs::create_dir(&denied).unwrap();
        fs::write(denied.join("script.lua"), "return 1\n").unwrap();
        let error = discover_supported_files_with_hook(
            &root,
            discovery_limits_for_test(),
            |stage, path| {
                if stage == DiscoveryStage::ReadDirectory && path == denied {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected permission failure",
                    ))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert!(error.details.context[0].contains("injected permission failure"));
        fs::remove_dir_all(root).unwrap();

        let root = test_directory("discovery-disappearing");
        let disappearing = root.join("disappearing");
        fs::create_dir(&disappearing).unwrap();
        fs::write(disappearing.join("script.lua"), "return 1\n").unwrap();
        let error = discover_supported_files_with_hook(
            &root,
            discovery_limits_for_test(),
            |stage, path| {
                if stage == DiscoveryStage::InspectEntry && path == disappearing {
                    fs::remove_dir_all(path)?;
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert!(error.message.contains("disappearing"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lua_and_quirl_formatting_share_atomic_replacement() {
        let root = test_directory("atomic-format-integration");
        let lua = root.join("script.lua");
        let quirl = root.join("script.qrl");
        fs::write(&lua, "local function main()\nreturn 1\nend\n").unwrap();
        fs::write(&quirl, "data [1,2,3] | length\n").unwrap();

        assert_eq!(format_paths(&root, false).unwrap(), 0);
        assert_eq!(
            fs::read_to_string(&lua).unwrap(),
            "local function main()\n  return 1\nend\n"
        );
        assert_eq!(
            fs::read_to_string(&quirl).unwrap(),
            "data [1, 2, 3] | length\n"
        );
        let entries = fs::read_dir(&root).unwrap().count();
        assert_eq!(entries, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_analysis_aggregates_lua_and_quirl_failures() {
        let root = test_directory("analysis");
        fs::write(root.join("good.lua"), "return 42\n").unwrap();
        fs::write(
            root.join("bad.lua"),
            "---@parm value string\nreturn value\n",
        )
        .unwrap();
        fs::write(root.join("bad.qrl"), "printf ok |\n").unwrap();

        let report = analysis_report(&root, "check", AUTHORING_DISCOVERY_LIMITS).unwrap();
        assert!(!report.valid);
        assert_eq!(report.files.len(), 3);
        assert_eq!(report.files.iter().filter(|entry| !entry.valid).count(), 2);
        assert!(report.files.iter().any(|entry| {
            entry.file.ends_with("bad.lua")
                && entry.error.as_ref().is_some_and(|error| {
                    error.details.labels[0].source.as_deref() == Some(entry.file.as_str())
                })
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn test_discovery_uses_conventions_and_quirl_formatting_is_non_mutating() {
        let root = test_directory("test-modules");
        fs::write(root.join("module.lua"), "return {}\n").unwrap();
        fs::write(
            root.join("module_tests.lua"),
            "return { test_ok = function() assert(true) end }\n",
        )
        .unwrap();
        let quirl = root.join("workflow.qrl");
        let quirl_source = "printf preserved  \n";
        fs::write(&quirl, quirl_source).unwrap();

        let files = discover_supported_files(&root, AUTHORING_DISCOVERY_LIMITS).unwrap();
        let tests = files
            .iter()
            .filter(|path| is_test_module(path))
            .collect::<Vec<_>>();
        assert_eq!(tests.len(), 1);
        assert!(tests[0].ends_with("module_tests.lua"));
        assert_eq!(test_paths(&root).unwrap(), 0);
        assert_eq!(format_paths(&quirl, false).unwrap(), 0);
        assert_eq!(fs::read_to_string(&quirl).unwrap(), quirl_source);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quirl_check_parses_data_bodies_without_executing_sources() {
        let root = test_directory("data-check-nonexecution");
        let marker = root.join("must-not-exist");
        let source = format!("data ^external touch {} | from\n", marker.display());
        let error = check_quirl_source(&source, "check.qrl").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidCommand);
        assert!(
            error
                .details
                .labels
                .iter()
                .any(|label| label.message.contains("invalid arguments for `from`"))
        );
        let lsp = lsp_native_diagnostics(&source);
        assert!(lsp.iter().any(|diagnostic| {
            diagnostic.source == "quirl-data"
                && diagnostic.message.contains("invalid arguments for `from`")
        }));
        assert!(!marker.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn quirl_formatter_canonicalizes_inline_and_block_data_only() {
        let source =
            "data {\n    { \"x\" :[1,2]}|get x\n}\nprintf preserved  \ndata [1,2]|length\n";
        let formatted = format_quirl_source(source, "format.qrl").unwrap();
        assert_eq!(
            formatted,
            "data {\n    {\"x\": [1, 2]} | get x\n}\nprintf preserved  \ndata [1, 2] | length\n"
        );
        assert_eq!(
            format_quirl_source(&formatted, "format.qrl").unwrap(),
            formatted
        );
    }

    #[test]
    fn script_reader_rejects_source_beyond_the_runtime_limit() {
        let source = vec![b'x'; MAX_LUA_SOURCE_BYTES + 1];
        let error = read_script_bytes(std::io::Cursor::new(source), "test input").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit"));
    }
}
