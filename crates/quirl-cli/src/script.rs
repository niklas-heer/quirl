use clap::ValueEnum;
use quirl_core::{ErrorCode, ShellError};
use quirl_data::DataRuntime;
use quirl_lua::{format_file, LuaPolicy, LuaRuntime};
use quirl_process::NativeExecutor;
use quirl_syntax::check_script;
use quirl_ui::render_error;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScriptLanguage {
    Lua,
    Quirl,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScriptRunOutput {
    pub status: i32,
    pub value: Value,
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
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not serialize script diagnostics")
                    .with_context(error.to_string())
                    .with_help("Report this as a Quirl script diagnostic schema defect")
            })?
        );
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
        let runtime = LuaRuntime::new(LuaPolicy::script())?;
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
        .with_help("Pass a .lua/.quirl file or a directory containing scripts"));
    }
    files.sort();
    if files.is_empty() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("no supported scripts found under {}", path.display()),
        )
        .with_help("Add a .lua or .quirl file, or pass a different path"));
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
        Some("quirl") => Some(ScriptLanguage::Quirl),
        _ => None,
    }
}

fn check_script_file(path: &Path) -> Result<(), ShellError> {
    match script_language_for_path(path) {
        Some(ScriptLanguage::Lua) => LuaRuntime::check_file(path),
        Some(ScriptLanguage::Quirl) => {
            let source = fs::read_to_string(path).map_err(|error| path_error(path, error))?;
            check_quirl_source(&source, &path.display().to_string())
        }
        None => Err(unsupported_language_error(
            path.extension().and_then(|extension| extension.to_str()),
        )),
    }
}

fn check_quirl_source(source: &str, source_name: &str) -> Result<(), ShellError> {
    let diagnostics = check_script(source);
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
        let mut source = String::new();
        io::stdin().read_to_string(&mut source).map_err(|error| {
            ShellError::new(
                ErrorCode::ScriptRead,
                "cannot read script from standard input",
            )
            .with_context(error.to_string())
            .with_help("Pipe UTF-8 Lua or Quirl source to `quirl run --lang <language> -`")
        })?;
        (source, "<stdin>".to_owned())
    } else {
        let source = fs::read_to_string(file).map_err(|error| {
            ShellError::new(
                ErrorCode::ScriptRead,
                format!("cannot read script {}", file.display()),
            )
            .with_context(error.to_string())
            .with_help("Check the script path and read permissions")
        })?;
        (source, file.display().to_string())
    };
    run_source(
        &source,
        &source_name,
        (file != Path::new("-")).then_some(file),
        requested_language,
        arguments,
    )
}

pub fn run_source(
    source: &str,
    source_name: &str,
    path: Option<&Path>,
    requested_language: Option<ScriptLanguage>,
    arguments: &[String],
) -> Result<ScriptRunOutput, ShellError> {
    let language = detect_language(source, path, requested_language)?;
    match language {
        ScriptLanguage::Lua => {
            let runtime = LuaRuntime::new(LuaPolicy::script())?;
            let value = runtime.run_source(source, source_name, arguments)?;
            let status = structured_status(&value)?;
            Ok(ScriptRunOutput { status, value })
        }
        ScriptLanguage::Quirl => run_quirl_source(source, source_name, arguments),
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
            "quirl" => Ok(ScriptLanguage::Quirl),
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
                .with_help("Use `--lang lua` or `--lang quirl` in the shebang")
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
        } else if matches!(executable, "bash" | "zsh" | "sh") {
            return Err(unsupported_shebang_language(executable));
        }
    }
    Ok(None)
}

fn language_name(language: &str) -> Option<ScriptLanguage> {
    match language.to_ascii_lowercase().as_str() {
        "lua" | "lua5.4" => Some(ScriptLanguage::Lua),
        "quirl" => Some(ScriptLanguage::Quirl),
        _ => None,
    }
}

fn unsupported_language_error(extension: Option<&str>) -> ShellError {
    let context = extension.map_or_else(
        || "the script has no recognized shebang or extension".to_owned(),
        |extension| format!("the .{extension} extension has no Phase 2 runner"),
    );
    ShellError::new(
        ErrorCode::InvalidArgument,
        "cannot determine the script language",
    )
    .with_context(context)
    .with_help("Use a .lua or .quirl file, a recognized shebang, or pass `--lang lua|quirl`")
}

fn unsupported_shebang_language(language: &str) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("script shebang requests unsupported language `{language}`"),
    )
    .with_help("Phase 2 supports `lua` and `quirl`; Bash and Zsh runners arrive in Phase 3")
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

fn run_quirl_source(
    source: &str,
    source_name: &str,
    arguments: &[String],
) -> Result<ScriptRunOutput, ShellError> {
    let mut executor = NativeExecutor::default();
    let data = DataRuntime::new();
    let mut value = json!({ "args": arguments, "status": 0 });
    for (line_index, line) in source.lines().enumerate() {
        if line_index == 0 && line.starts_with("#!") {
            continue;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(expression) = line.strip_prefix("data ") {
            value = data.eval(expression).map_err(|error| {
                error.with_label(
                    Some(source_name.to_owned()),
                    line_offset(source, line_index),
                    line_offset(source, line_index) + line.len(),
                    "failed Quirl data statement",
                )
            })?;
            continue;
        }
        let outcome = executor.execute_capture(line).map_err(|error| {
            error.with_label(
                Some(source_name.to_owned()),
                line_offset(source, line_index),
                line_offset(source, line_index) + line.len(),
                "failed Quirl command statement",
            )
        })?;
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
    Ok(ScriptRunOutput { status: 0, value })
}

fn line_offset(source: &str, line_index: usize) -> usize {
    source
        .split_inclusive('\n')
        .take(line_index)
        .map(str::len)
        .sum()
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
            "#!/usr/bin/env bash\necho nope",
            Some(Path::new("misleading.lua")),
            None,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.message.contains("unsupported language"));
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
            "failure.quirl",
            Some(Path::new("failure.quirl")),
            None,
            &[],
        )
        .unwrap();
        assert_ne!(output.status, 0);
        assert_eq!(output.value["stdout"], "");
    }

    #[test]
    fn recursive_discovery_is_sorted_and_skips_git_target_and_symlink_directories() {
        let root = test_directory("discover");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("z.lua"), "return 1").unwrap();
        fs::write(root.join("a.quirl"), "pwd").unwrap();
        fs::write(root.join("nested/m.lua"), "return 2").unwrap();
        fs::write(root.join("target/ignored.lua"), "invalid(").unwrap();
        fs::write(root.join(".git/ignored.quirl"), "echo ignored").unwrap();
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
                PathBuf::from("a.quirl"),
                PathBuf::from("nested/m.lua"),
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
        fs::write(root.join("bad.quirl"), "printf ok |\n").unwrap();

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
        let quirl = root.join("workflow.quirl");
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
}
