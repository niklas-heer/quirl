use crate::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::UNIX_EPOCH,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandOutcome {
    pub status: i32,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
}

impl CommandOutcome {
    fn success_with_output(output: String) -> Self {
        Self {
            status: 0,
            stdout: Some(output),
            stderr: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified_unix_seconds: Option<u64>,
    pub hidden: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub struct CommandRunner {
    shell: PathBuf,
}

impl Default for CommandRunner {
    fn default() -> Self {
        Self::new(
            env::var_os("SHELL")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/bin/sh")),
        )
    }
}

impl CommandRunner {
    pub fn new(shell: impl Into<PathBuf>) -> Self {
        Self {
            shell: shell.into(),
        }
    }

    /// Execute a command with inherited terminal streams, as an interactive shell should.
    pub fn execute(&self, input: &str) -> Result<CommandOutcome, ShellError> {
        self.execute_inner(input, false)
    }

    /// Execute a command with captured streams for tools and tests.
    pub fn execute_capture(&self, input: &str) -> Result<CommandOutcome, ShellError> {
        self.execute_inner(input, true)
    }

    fn execute_inner(&self, input: &str, capture: bool) -> Result<CommandOutcome, ShellError> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(CommandOutcome {
                status: 0,
                stdout: None,
                stderr: None,
            });
        }

        let forced_external = input.starts_with('^');
        let external_input = input.strip_prefix('^').unwrap_or(input);
        let words = shlex::split(input);
        if !forced_external {
            if let Some(words) = words.as_deref() {
                match words.first().map(String::as_str) {
                    Some("cd") => return self.change_directory(words),
                    Some("ls") => return self.list_directory(words),
                    _ => {}
                }
            }
        }

        self.execute_external(external_input, capture)
    }

    fn execute_external(&self, input: &str, capture: bool) -> Result<CommandOutcome, ShellError> {
        let mut command = Command::new(&self.shell);
        command.arg("-c").arg(input).stdin(Stdio::inherit());
        if capture {
            let output = command.output().map_err(|error| {
                ShellError::new(
                    ErrorCode::ProcessSpawn,
                    format!("could not start {}", self.shell.display()),
                )
                .with_command(input)
                .with_context(error.to_string())
                .with_help("Check that $SHELL names an executable shell")
            })?;
            Ok(CommandOutcome {
                status: output.status.code().unwrap_or(1),
                stdout: Some(String::from_utf8_lossy(&output.stdout).into_owned()),
                stderr: Some(String::from_utf8_lossy(&output.stderr).into_owned()),
            })
        } else {
            let status = command
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|error| {
                    ShellError::new(
                        ErrorCode::ProcessSpawn,
                        format!("could not start {}", self.shell.display()),
                    )
                    .with_command(input)
                    .with_context(error.to_string())
                    .with_help("Check that $SHELL names an executable shell")
                })?;
            Ok(CommandOutcome {
                status: status.code().unwrap_or(1),
                stdout: None,
                stderr: None,
            })
        }
    }

    fn change_directory(&self, words: &[String]) -> Result<CommandOutcome, ShellError> {
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
        env::set_current_dir(&path).map_err(|error| {
            ShellError::new(ErrorCode::Io, format!("cannot enter {}", path.display()))
                .with_command(words.join(" "))
                .with_context(error.to_string())
        })?;
        Ok(CommandOutcome {
            status: 0,
            stdout: None,
            stderr: None,
        })
    }

    fn list_directory(&self, words: &[String]) -> Result<CommandOutcome, ShellError> {
        let mut show_all = false;
        let mut long = false;
        let mut json = false;
        let mut path = None;
        for word in words.iter().skip(1) {
            match word.as_str() {
                "-a" | "--all" => show_all = true,
                "-l" | "--long" => long = true,
                "--json" => json = true,
                option if option.starts_with('-') => {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        format!("unknown ls option `{option}`"),
                    )
                    .with_command(words.join(" "))
                    .with_help("Try `help ls` to see Quirl's native options")
                    .with_help("Use `^ls ...` to force the external ls"));
                }
                value if path.is_none() => path = Some(PathBuf::from(value)),
                value => {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        format!("unexpected second path `{value}`"),
                    )
                    .with_command(words.join(" ")));
                }
            }
        }
        let path = path.unwrap_or_else(|| PathBuf::from("."));
        let entries = read_entries(&path, show_all)?;
        let output = if json {
            serde_json::to_string_pretty(&entries).map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not serialize directory entries")
                    .with_context(error.to_string())
            })?
        } else {
            render_entries(&entries, long)
        };
        Ok(CommandOutcome::success_with_output(output))
    }
}

fn read_entries(path: &Path, show_all: bool) -> Result<Vec<Entry>, ShellError> {
    let iterator = fs::read_dir(path).map_err(|error| {
        ShellError::new(ErrorCode::Io, format!("cannot read {}", path.display()))
            .with_context(error.to_string())
            .with_help("Check that the directory exists and is readable")
    })?;
    let mut entries = Vec::new();
    for item in iterator {
        let item = item.map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read a directory entry")
                .with_context(error.to_string())
        })?;
        let name = item.file_name().to_string_lossy().into_owned();
        let hidden = name.starts_with('.');
        if hidden && !show_all {
            continue;
        }
        let metadata = fs::symlink_metadata(item.path()).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                format!("cannot inspect {}", item.path().display()),
            )
            .with_context(error.to_string())
        })?;
        let file_type = item.file_type().map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                format!("cannot identify {}", item.path().display()),
            )
            .with_context(error.to_string())
        })?;
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        entries.push(Entry {
            name,
            path: item.path(),
            kind,
            size: metadata.len(),
            modified_unix_seconds: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs()),
            hidden,
        });
    }
    entries.sort_by(|left, right| {
        matches!(right.kind, EntryKind::Directory)
            .cmp(&matches!(left.kind, EntryKind::Directory))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    Ok(entries)
}

fn render_entries(entries: &[Entry], long: bool) -> String {
    let mut output = String::new();
    for entry in entries {
        if long {
            let kind = match entry.kind {
                EntryKind::Directory => "dir ",
                EntryKind::File => "file",
                EntryKind::Symlink => "link",
                EntryKind::Other => "other",
            };
            output.push_str(&format!("{kind}  {:>10}  {}\n", entry.size, entry.name));
        } else {
            output.push_str(&entry.name);
            if matches!(entry.kind, EntryKind::Directory) {
                output.push('/');
            }
            output.push('\n');
        }
    }
    output.trim_end_matches('\n').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_commands_keep_shell_syntax() {
        let runner = CommandRunner::new("/bin/sh");
        let result = runner
            .execute_capture("printf 'hello' | tr a-z A-Z")
            .unwrap();
        assert_eq!(result.status, 0);
        assert_eq!(result.stdout.as_deref(), Some("HELLO"));
    }

    #[test]
    fn native_ls_can_emit_json() {
        let runner = CommandRunner::new("/bin/sh");
        let result = runner.execute_capture("ls --json .").unwrap();
        let entries: Vec<Entry> = serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
        assert!(entries.iter().any(|entry| entry.name == "Cargo.toml"));
    }
}
