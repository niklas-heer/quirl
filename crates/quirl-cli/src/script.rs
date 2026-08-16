use clap::ValueEnum;
use quirl_core::{escape_json_terminal_controls, ErrorCode, ShellError};
use quirl_data::DataRuntime;
use quirl_lua::{format_file, LuaPolicy, LuaRuntime, MAX_LUA_SOURCE_BYTES};
use quirl_process::{sandboxed_process_host, ChildProcessTree, NativeExecutor};
use quirl_syntax::{check_script, parse_command_list};
use quirl_ui::render_error;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    ops::Range,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MAX_REFERENCE_CAPTURE_BYTES: usize = 64 * 1024;
const QUIRL_CANONICAL_EXTENSION: &str = "qrl";
const QUIRL_EXTENSION_ALIASES: [&str; 2] = ["quirl", "🌀"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScriptLanguage {
    Lua,
    Quirl,
    Bash,
    Zsh,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptRunOutput {
    pub status: i32,
    pub value: Value,
}

#[derive(Clone, Debug, Default)]
pub struct ScriptCancellation {
    cancelled: Arc<AtomicBool>,
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

pub fn analyze(path: &Path, json_output: bool, lint: bool) -> Result<i32, ShellError> {
    let operation = if lint { "lint" } else { "check" };
    let report = match analysis_report(path, operation) {
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

fn analysis_report(path: &Path, operation: &'static str) -> Result<AnalysisReport, ShellError> {
    let files = discover_supported_files(path)?;
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
    let files = discover_supported_files(path)?;
    let mut drift = Vec::new();
    let mut failures = Vec::new();
    for file in files {
        if script_language_for_path(&file) == Some(ScriptLanguage::Quirl) {
            println!("unchanged {}", file.display());
            continue;
        }
        match format_file(&file, check) {
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

pub fn test_paths(path: &Path) -> Result<i32, ShellError> {
    let explicit_file = path.is_file();
    let mut files = discover_supported_files(path)?;
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

fn discover_supported_files(path: &Path) -> Result<Vec<PathBuf>, ShellError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| path_error(path, error))?;
    let mut files = Vec::new();
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
            files.push(path.to_path_buf());
        } else {
            return Err(unsupported_language_error(
                path.extension().and_then(|extension| extension.to_str()),
            ));
        }
    } else if metadata.is_dir() {
        discover_directory(path, &mut files)?;
    } else {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} is not a regular file or directory", path.display()),
        )
        .with_help("Pass a .lua, .qrl, .quirl, or .🌀 file, or a directory containing scripts"));
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

fn discover_directory(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), ShellError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| path_error(directory, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| path_error(directory, error))?;
    entries.sort_by_key(fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| path_error(&path, error))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if matches!(entry.file_name().to_str(), Some(".git" | "target")) {
                continue;
            }
            discover_directory(&path, files)?;
        } else if file_type.is_file() && script_language_for_path(&path).is_some() {
            files.push(path);
        }
    }
    Ok(())
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
    let statements = native_script_statements(source, source_name)?;
    let mut diagnostics = check_script(&script_without_explicit_blocks(source, &statements));
    for statement in &statements {
        if statement.explicit && statement.kind == NativeStatementKind::Command {
            diagnostics.extend(command_block_diagnostics(source, statement));
        }
    }
    diagnostics.sort_by(|left, right| {
        (left.start, left.end, &left.message).cmp(&(right.start, right.end, &right.message))
    });
    if diagnostics.is_empty() {
        return Ok(());
    }
    let mut error = ShellError::new(ErrorCode::InvalidCommand, "Quirl script validation failed");
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

pub fn run(
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
    let signal_id = matches!(language, ScriptLanguage::Bash | ScriptLanguage::Zsh)
        .then(|| {
            signal_hook::flag::register(
                signal_hook::consts::SIGINT,
                Arc::clone(&cancellation.cancelled),
            )
            .map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not install script cancellation handler",
                )
                .with_context(error.to_string())
                .with_help("Retry the script; report repeated signal-handler failures")
            })
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
    if let Some(signal_id) = signal_id {
        signal_hook::low_level::unregister(signal_id);
    }
    result
}

fn read_script_file(path: &Path) -> Result<String, ShellError> {
    let file = fs::File::open(path).map_err(|error| path_error(path, error))?;
    let size = file
        .metadata()
        .map_err(|error| path_error(path, error))?
        .len();
    if size > MAX_LUA_SOURCE_BYTES as u64 {
        return Err(script_source_limit_error(&path.display().to_string(), size));
    }
    read_script_bytes(file, &path.display().to_string())
}

fn read_script_stdin() -> Result<String, ShellError> {
    read_script_bytes(io::stdin(), "standard input")
}

fn read_script_bytes(reader: impl Read, source_name: &str) -> Result<String, ShellError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_LUA_SOURCE_BYTES.saturating_add(1) as u64)
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
        return Err(script_source_limit_error(source_name, bytes.len() as u64));
    }
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
pub fn run_source(
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

pub fn run_source_with_cancellation(
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
            let runtime =
                LuaRuntime::new_with_process_host(LuaPolicy::script(), sandboxed_process_host())?;
            let value = runtime.run_source(source, source_name, arguments)?;
            let status = structured_status(&value)?;
            Ok(ScriptRunOutput { status, value })
        }
        ScriptLanguage::Quirl => run_quirl_source(source, source_name, arguments),
        ScriptLanguage::Bash | ScriptLanguage::Zsh => run_reference_script(
            source,
            source_name,
            language,
            arguments,
            cancellation,
            language.executable(),
        ),
    }
}

/// Run the body of an explicit interactive dialect island through the same bounded reference
/// boundary used for Bash/Zsh scripts. The caller owns rendering, so this returns the common
/// command outcome instead of script JSON.
pub fn run_interactive_island(
    language: ScriptLanguage,
    source: &str,
    cancellation: &ScriptCancellation,
) -> Result<quirl_core::CommandOutcome, ShellError> {
    let signal_id = signal_hook::flag::register(
        signal_hook::consts::SIGINT,
        Arc::clone(&cancellation.cancelled),
    )
    .map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "could not install interactive island cancellation handler",
        )
        .with_context(error.to_string())
        .with_help("Retry the island; report repeated signal-handler failures")
    })?;
    let executable = language.executable();
    let result = run_reference_script(
        source,
        "<interactive island>",
        language,
        &[],
        cancellation,
        executable,
    );
    signal_hook::low_level::unregister(signal_id);
    let output = result?;
    let stdout = output
        .value
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let stderr = output
        .value
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(quirl_core::CommandOutcome {
        status: output.status,
        stdout: Some(stdout),
        stderr: Some(stderr),
    })
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

fn structured_status(value: &Value) -> Result<i32, ShellError> {
    let Some(status) = value.get("status") else {
        return Ok(0);
    };
    let Some(status) = status.as_i64() else {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "script result status must be an integer",
        )
        .with_help("Return `{ status = 0, ... }` or omit status from the Lua result"));
    };
    i32::try_from(status).map_err(|_| {
        ShellError::new(
            ErrorCode::Validation,
            "script result status is outside the supported integer range",
        )
        .with_help("Return a status between -2147483648 and 2147483647")
    })
}

fn run_reference_script(
    source: &str,
    source_name: &str,
    language: ScriptLanguage,
    arguments: &[String],
    cancellation: &ScriptCancellation,
    executable: &str,
) -> Result<ScriptRunOutput, ShellError> {
    let mut command = Command::new(executable);
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
            ))
        }
    }
    command
        .args(arguments)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let containment = ChildProcessTree::new()?;
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
    let stdout = child.stdout.take().map(spawn_stream_reader);
    let stderr = child.stderr.take().map(spawn_stream_reader);
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
    let stdout = join_stream_reader(stdout, "reference runner stdout")?;
    let stderr = join_stream_reader(stderr, "reference runner stderr")?;
    let status_code = status.code().unwrap_or(1);
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
    Ok(ScriptRunOutput {
        status: status_code,
        value: json!({
            "language": language.executable(),
            "status": status_code,
            "stdout": stdout.value,
            "stderr": stderr.value,
            "stdout_discarded_bytes": stdout.discarded_bytes,
            "stderr_discarded_bytes": stderr.discarded_bytes,
        }),
    })
}

fn terminate_reference_process(
    child: &mut std::process::Child,
    containment: &ChildProcessTree,
) -> Result<(), ShellError> {
    #[cfg(unix)]
    if let Ok(process_group) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(process_group),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
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
) -> thread::JoinHandle<io::Result<StreamCapture>> {
    thread::spawn(move || {
        let mut retained = Vec::with_capacity(MAX_REFERENCE_CAPTURE_BYTES);
        let mut discarded_bytes = 0_u64;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let available = MAX_REFERENCE_CAPTURE_BYTES.saturating_sub(retained.len());
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

fn run_quirl_source(
    source: &str,
    source_name: &str,
    arguments: &[String],
) -> Result<ScriptRunOutput, ShellError> {
    let mut executor = NativeExecutor::default();
    let data = DataRuntime::new();
    let mut value = json!({ "args": arguments, "status": 0 });
    for statement in native_script_statements(source, source_name)? {
        let statement_source = &source[statement.body.clone()];
        match statement.kind {
            NativeStatementKind::Data => {
                value = data.eval(statement_source.trim()).map_err(|error| {
                    error.with_label(
                        Some(source_name.to_owned()),
                        statement.body.start,
                        statement.body.end,
                        "failed Quirl data statement",
                    )
                })?;
            }
            NativeStatementKind::Command => {
                let outcome = if statement.explicit {
                    execute_command_block(
                        &mut executor,
                        source,
                        source_name,
                        statement.body.clone(),
                    )?
                } else {
                    execute_native_command(
                        &mut executor,
                        statement_source,
                        source_name,
                        statement.body.clone(),
                    )?
                };
                value = json!({
                    "status": outcome.status,
                    "stdout": outcome.stdout.unwrap_or_default(),
                    "stderr": outcome.stderr.unwrap_or_default(),
                });
                if outcome.status != 0 {
                    return Ok(ScriptRunOutput {
                        status: outcome.status,
                        value,
                    });
                }
            }
        }
    }
    Ok(ScriptRunOutput { status: 0, value })
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
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            if let Err(mut error) = parse_command_list(trimmed) {
                error.start += offset + leading;
                error.end += offset + leading;
                diagnostics.push(error);
            }
        }
        offset += raw_line.len();
    }
    diagnostics
}

fn execute_command_block(
    executor: &mut NativeExecutor,
    source: &str,
    source_name: &str,
    body: Range<usize>,
) -> Result<quirl_core::CommandOutcome, ShellError> {
    let mut outcome = quirl_core::CommandOutcome {
        status: 0,
        stdout: Some(String::new()),
        stderr: Some(String::new()),
    };
    let mut offset = body.start;
    for raw_line in source[body].split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let leading = line.len().saturating_sub(line.trim_start().len());
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            let line_outcome = execute_native_command(
                executor,
                trimmed,
                source_name,
                (offset + leading)..(offset + leading + trimmed.len()),
            )?;
            outcome.status = line_outcome.status;
            if let Some(stdout) = line_outcome.stdout {
                outcome
                    .stdout
                    .get_or_insert_with(String::new)
                    .push_str(&stdout);
            }
            if let Some(stderr) = line_outcome.stderr {
                outcome
                    .stderr
                    .get_or_insert_with(String::new)
                    .push_str(&stderr);
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
    command: &str,
    source_name: &str,
    span: Range<usize>,
) -> Result<quirl_core::CommandOutcome, ShellError> {
    executor.execute_capture(command).map_err(|error| {
        error.with_label(
            Some(source_name.to_owned()),
            span.start,
            span.end,
            "failed Quirl command statement",
        )
    })
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
    fn reference_runner_drains_but_does_not_retain_unbounded_output() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let output = run_source(
            "i=0; while [ \"$i\" -lt 7000 ]; do printf 0123456789; printf abcdefghij >&2; i=$((i+1)); done",
            "bounded.bash",
            None,
            Some(ScriptLanguage::Bash),
            &[],
        )
        .unwrap();
        assert_eq!(
            output.value["stdout"].as_str().unwrap().len(),
            MAX_REFERENCE_CAPTURE_BYTES
        );
        assert_eq!(
            output.value["stderr"].as_str().unwrap().len(),
            MAX_REFERENCE_CAPTURE_BYTES
        );
        assert_eq!(
            output.value["stdout_discarded_bytes"],
            70_000 - MAX_REFERENCE_CAPTURE_BYTES
        );
        assert_eq!(
            output.value["stderr_discarded_bytes"],
            70_000 - MAX_REFERENCE_CAPTURE_BYTES
        );
    }

    #[test]
    fn missing_reference_interpreter_has_a_labeled_actionable_error() {
        let error = run_reference_script(
            "echo unreachable",
            "missing.bash",
            ScriptLanguage::Bash,
            &[],
            &ScriptCancellation::default(),
            "/definitely/missing/quirl-reference-interpreter",
        )
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

    #[test]
    fn interactive_dialect_island_observes_cancellation() {
        if !reference_interpreter_available("bash") {
            eprintln!("skipping bash: interpreter is unavailable");
            return;
        }
        let cancellation = ScriptCancellation::default();
        let worker_cancellation = cancellation.clone();
        let worker = thread::spawn(move || {
            run_interactive_island(ScriptLanguage::Bash, "sleep 10", &worker_cancellation)
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
        assert!(error.details.labels[0]
            .message
            .contains("explicit Quirl capability"));
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

        let files = discover_supported_files(&root).unwrap();
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

        let report = analysis_report(&root, "check").unwrap();
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

        let files = discover_supported_files(&root).unwrap();
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
    fn script_reader_rejects_source_beyond_the_runtime_limit() {
        let source = vec![b'x'; MAX_LUA_SOURCE_BYTES + 1];
        let error = read_script_bytes(std::io::Cursor::new(source), "test input").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit"));
    }
}
