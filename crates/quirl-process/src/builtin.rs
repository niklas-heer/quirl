//! Process-owned implementations of stateful and bounded native built-ins.

use quirl_core::{CommandOutcome, ErrorCode, ShellError};
use std::{
    env,
    path::{Path, PathBuf},
};

/// Built-ins that change session state or job control, and therefore must run
/// in the shell's own process rather than in a pipeline stage.
pub(crate) const STATEFUL_BUILTINS: [&str; 9] = [
    "cd", "export", "unset", "eval", "source", ".", "jobs", "fg", "bg",
];

/// Whether `name` is one of [`STATEFUL_BUILTINS`].
pub(crate) fn is_stateful(name: &str) -> bool {
    STATEFUL_BUILTINS.contains(&name)
}

/// Operands of one `export` command.
pub(crate) struct ExportOperands {
    /// `NAME=value` operands, set and exported.
    pub(crate) assignments: Vec<(String, String)>,
    /// Bare `NAME` operands, whose existing values gain the export attribute.
    pub(crate) names: Vec<String>,
}

/// Split `export` operands into `NAME=value` assignments and bare names that
/// gain the export attribute, as `export NAME` does after `NAME=value`.
pub(crate) fn export_operands(words: &[String]) -> Result<ExportOperands, ShellError> {
    let mut assignments = Vec::new();
    let mut names = Vec::new();
    for operand in words.iter().skip(1) {
        if operand == "-p" {
            return Err(
                ShellError::new(ErrorCode::InvalidArgument, "export -p is not supported")
                    .with_help("Use `env` to list exported variables"),
            );
        }
        match operand.split_once('=') {
            Some((name, value)) => assignments.push((name.to_owned(), value.to_owned())),
            None => names.push(operand.clone()),
        }
    }
    if assignments.is_empty() && names.is_empty() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "export needs at least one NAME or NAME=value",
        )
        .with_help("Use `export NAME=value` or `export NAME`"));
    }
    Ok(ExportOperands { assignments, names })
}

/// Names named by `unset [-v] NAME...`. Functions are not Quirl state.
pub(crate) fn unset_operands(words: &[String]) -> Result<Vec<String>, ShellError> {
    let mut names = Vec::new();
    for operand in words.iter().skip(1) {
        match operand.as_str() {
            "-v" => {}
            "-f" => {
                return Err(ShellError::new(
                    ErrorCode::InvalidArgument,
                    "unset -f is not supported because Quirl has no shell functions",
                )
                .with_help("Define and remove functions inside a `bash { ... }` island"));
            }
            _ => names.push(operand.clone()),
        }
    }
    Ok(names)
}

pub(crate) fn execute_cd(words: &[String]) -> Result<CommandOutcome, ShellError> {
    if words.len() > 2 {
        return Err(
            ShellError::new(ErrorCode::InvalidArgument, "cd accepts at most one path")
                .with_command(words.join(" "))
                .with_help("Usage: cd [path]"),
        );
    }
    let path = words
        .get(1)
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::InvalidArgument,
                "cd needs a path because no home directory is configured",
            )
            .with_help("Pass a path explicitly: cd /some/directory")
        })?;
    change_directory(&path).map_err(|error| error.with_command(words.join(" ")))?;
    Ok(success_with_output(String::new()))
}

pub(crate) fn change_directory(path: &Path) -> Result<(), ShellError> {
    env::set_current_dir(path).map_err(|error| {
        ShellError::new(ErrorCode::Io, format!("cannot enter {}", path.display()))
            .with_context(error.to_string())
            .with_help("Check that the directory exists and is accessible")
    })
}

fn success_with_output(output: String) -> CommandOutcome {
    CommandOutcome {
        status: 0,
        stdout: Some(output),
        stderr: Some(String::new()),
    }
}
