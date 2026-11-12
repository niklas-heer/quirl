//! Process-owned implementations of stateful and bounded native built-ins.

use quirl_core::{CommandOutcome, ErrorCode, ShellError};
use std::{
    env,
    path::{Path, PathBuf},
};

/// Built-ins that change session state or job control, and therefore must run
/// in the shell's own process rather than in a pipeline stage.
pub(crate) const STATEFUL_BUILTINS: [&str; 10] = [
    "cd", "export", "unset", "set", "eval", "source", ".", "jobs", "fg", "bg",
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

/// The allexport state one `set` command selects.
///
/// Only the allexport option is native: `set -a`, `set +a`,
/// `set -o allexport`, and `set +o allexport`; when several are given, the
/// last wins, as in `sh`. Every other form is refused with a diagnostic rather
/// than run as an external program, which would fail with "not found".
pub(crate) fn set_allexport_operand(words: &[String]) -> Result<bool, ShellError> {
    quirl_syntax::native_set_allexport(words).ok_or_else(|| {
        let command = words.join(" ");
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!(
                "`{command}` is not supported; Quirl's `set` only takes `-a`, `+a`, `-o allexport`, or `+o allexport`"
            ),
        )
        .with_command(command)
        .with_help(
            "Use `set -a` to export later assignments and `set +a` to stop; run other shell options inside `sh -c '...'` or a `bash { ... }` island",
        )
    })
}

/// Refuse `set` as a stage of a multi-command pipeline. It changes session
/// state, which a pipeline stage cannot do, and without this check it would
/// run as an external program and fail with "not found".
pub(crate) fn refuse_set_in_pipeline(
    commands: &[quirl_syntax::SimpleCommand],
    source: &str,
) -> Result<(), ShellError> {
    let in_pipeline = commands.len() > 1
        && commands
            .iter()
            .any(|command| command_name(&command.words).is_some_and(|name| name == "set"));
    if !in_pipeline {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::InvalidArgument,
        "`set` cannot run inside a pipeline",
    )
    .with_command(source)
    .with_help(
        "Run `set -a` or `set +a` as its own command, for example `set -a; source .env; set +a`; use `env` to list exported variables",
    ))
}

/// The command a stage runs, past the runner and the `NAME=value` words
/// that expansion puts in front of it, so `A=1 set -a` names `set`.
fn command_name(words: &[String]) -> Option<&str> {
    let words = match words.split_first() {
        Some((runner, rest)) if runner == crate::PREFIX_ASSIGNMENT_RUNNER => rest,
        _ => words,
    };
    words
        .iter()
        .map(String::as_str)
        .find(|word| !is_assignment_word(word))
}

/// Whether `word` has the `NAME=value` shape of a prefix assignment.
fn is_assignment_word(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut characters = name.chars();
        characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
            && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

/// Operands of one `unset [-v | -f] NAME...` command.
pub(crate) struct UnsetOperands {
    /// Whether `-f` selected functions instead of variables.
    pub(crate) functions: bool,
    /// Names to remove.
    pub(crate) names: Vec<String>,
}

/// Parse `unset [-v | -f] NAME...`. The last of `-v` and `-f` wins, as in Bash.
pub(crate) fn unset_operands(words: &[String]) -> Result<UnsetOperands, ShellError> {
    let mut functions = false;
    let mut names = Vec::new();
    for operand in words.iter().skip(1) {
        match operand.as_str() {
            "-v" if names.is_empty() => functions = false,
            "-f" if names.is_empty() => functions = true,
            _ => names.push(operand.clone()),
        }
    }
    Ok(UnsetOperands { functions, names })
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
