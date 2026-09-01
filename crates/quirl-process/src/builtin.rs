//! Process-owned implementations of stateful and bounded native built-ins.

use quirl_core::{CommandOutcome, ErrorCode, ShellError};
use std::{
    env,
    path::{Path, PathBuf},
};

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
