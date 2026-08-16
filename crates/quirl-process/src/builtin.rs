//! Process-owned implementations of stateful and bounded native built-ins.

use crate::DEFAULT_CAPTURE_BYTES;
use quirl_core::{
    directory_entries_with_options, CommandOutcome, DirectoryOptions, DirectorySort, Entry,
    EntryKind, ErrorCode, ShellError,
};
use std::{env, path::PathBuf};

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
    env::set_current_dir(&path).map_err(|error| {
        ShellError::new(ErrorCode::Io, format!("cannot enter {}", path.display()))
            .with_command(words.join(" "))
            .with_context(error.to_string())
            .with_help("Check that the directory exists and is accessible")
    })?;
    Ok(success_with_output(String::new()))
}

pub(crate) fn execute_ls(words: &[String]) -> Result<CommandOutcome, ShellError> {
    let (path, options, long, json) = parse_ls_options(words)?;
    let entries = directory_entries_with_options(&path, &options)?;
    let output = if json {
        serde_json::to_string_pretty(&entries).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not serialize directory entries")
                .with_context(error.to_string())
                .with_help("Retry the listing in plain format")
        })?
    } else {
        render_entries(&entries, long)
    };
    if output.len() > DEFAULT_CAPTURE_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "native ls output exceeds its byte limit",
        )
        .with_context(format!(
            "limit {DEFAULT_CAPTURE_BYTES} bytes; observed {} bytes",
            output.len()
        ))
        .with_help("Choose a narrower directory or lower --max-entries"));
    }
    Ok(success_with_output(output))
}

fn parse_ls_options(
    words: &[String],
) -> Result<(PathBuf, DirectoryOptions, bool, bool), ShellError> {
    let mut options = DirectoryOptions::default();
    let mut long = false;
    let mut json = false;
    let mut path = None;
    let mut index = 1;
    while let Some(word) = words.get(index) {
        match word.as_str() {
            "-a" | "--all" => options.show_all = true,
            "-l" | "--long" => long = true,
            "--json" => json = true,
            "--plain" => json = false,
            "-r" | "--reverse" => options.reverse = true,
            "--directories-first" | "--dirs-first" => options.directories_first = true,
            "--resolve-links" => options.resolve_symlink_targets = true,
            "--sort" => {
                index += 1;
                let value = words.get(index).ok_or_else(|| {
                    ShellError::new(ErrorCode::InvalidArgument, "ls --sort needs a value")
                        .with_command(words.join(" "))
                        .with_help("Use one of: name, size, modified, kind")
                })?;
                options.sort = parse_directory_sort(value, words)?;
            }
            value if value.starts_with("--sort=") => {
                options.sort =
                    parse_directory_sort(value.strip_prefix("--sort=").unwrap_or_default(), words)?;
            }
            "--max-entries" => {
                index += 1;
                let value = words.get(index).ok_or_else(|| {
                    ShellError::new(
                        ErrorCode::InvalidArgument,
                        "ls --max-entries needs a positive value",
                    )
                    .with_command(words.join(" "))
                    .with_help("Use a positive bound, for example `ls --max-entries 1000`")
                })?;
                options.max_entries = parse_max_entries(value, words)?;
            }
            value if value.starts_with("--max-entries=") => {
                options.max_entries = parse_max_entries(
                    value.strip_prefix("--max-entries=").unwrap_or_default(),
                    words,
                )?;
            }
            "--format" => {
                index += 1;
                let value = words.get(index).ok_or_else(|| {
                    ShellError::new(ErrorCode::InvalidArgument, "ls --format needs a value")
                        .with_command(words.join(" "))
                        .with_help("Use `--format plain` or `--format json`")
                })?;
                json = parse_ls_format(value, words)?;
            }
            value if value.starts_with("--format=") => {
                json = parse_ls_format(value.strip_prefix("--format=").unwrap_or_default(), words)?;
            }
            short if short.starts_with('-') && !short.starts_with("--") => {
                for flag in short[1..].chars() {
                    match flag {
                        'a' => options.show_all = true,
                        'l' => long = true,
                        'r' => options.reverse = true,
                        _ => {
                            return Err(ShellError::new(
                                ErrorCode::InvalidArgument,
                                format!("unknown ls option `-{flag}` in `{short}`"),
                            )
                            .with_command(words.join(" "))
                            .with_help("Use `ls --help` to inspect supported options"));
                        }
                    }
                }
            }
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
                .with_command(words.join(" "))
                .with_help("Pass at most one directory path to native ls"));
            }
        }
        index += 1;
    }
    Ok((
        path.unwrap_or_else(|| PathBuf::from(".")),
        options,
        long,
        json,
    ))
}

fn parse_directory_sort(value: &str, words: &[String]) -> Result<DirectorySort, ShellError> {
    match value {
        "name" => Ok(DirectorySort::Name),
        "size" => Ok(DirectorySort::Size),
        "modified" | "time" => Ok(DirectorySort::Modified),
        "kind" | "type" => Ok(DirectorySort::Kind),
        _ => Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("unknown ls sort `{value}`"),
        )
        .with_command(words.join(" "))
        .with_help("Use one of: name, size, modified, kind")),
    }
}

fn parse_max_entries(value: &str, words: &[String]) -> Result<usize, ShellError> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::InvalidArgument,
                format!("ls --max-entries needs a positive integer, got `{value}`"),
            )
            .with_command(words.join(" "))
            .with_help("Use a positive bound, for example `ls --max-entries=1000`")
        })
}

fn parse_ls_format(value: &str, words: &[String]) -> Result<bool, ShellError> {
    match value {
        "plain" | "text" => Ok(false),
        "json" => Ok(true),
        _ => Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("unknown ls format `{value}`"),
        )
        .with_command(words.join(" "))
        .with_help("Use `--format plain` or `--format json`")),
    }
}

fn render_entries(entries: &[Entry], long: bool) -> String {
    let mut output = String::new();
    for entry in entries {
        if long {
            let kind = match entry.kind {
                EntryKind::Directory => "dir",
                EntryKind::File => "file",
                EntryKind::Symlink => "link",
                EntryKind::Other => "other",
            };
            let modified = entry
                .modified_unix_seconds
                .map_or_else(|| "-".to_owned(), |seconds| seconds.to_string());
            output.push_str(&format!(
                "{kind:<5}  {:>10}  {:>10}  {}\n",
                entry.size,
                modified,
                entry.display_name()
            ));
        } else {
            output.push_str(&entry.display_name());
            if matches!(entry.kind, EntryKind::Directory) {
                output.push('/');
            }
            output.push('\n');
        }
    }
    output.trim_end_matches('\n').to_owned()
}

fn success_with_output(output: String) -> CommandOutcome {
    CommandOutcome {
        status: 0,
        stdout: Some(output),
        stderr: Some(String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEMP_PATH: AtomicUsize = AtomicUsize::new(0);

    fn test_directory(label: &str) -> PathBuf {
        let directory = env::temp_dir().join(format!(
            "quirl-process-builtin-{label}-{}-{}",
            std::process::id(),
            NEXT_TEMP_PATH.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn native_ls_accepts_json_formats_and_bounded_sort_controls() {
        let directory = test_directory("options");
        fs::write(directory.join("small"), "x").unwrap();
        fs::write(directory.join("large"), "xxxx").unwrap();
        let words = vec![
            "ls".to_owned(),
            "--format=json".to_owned(),
            "--sort=size".to_owned(),
            "--reverse".to_owned(),
            directory.display().to_string(),
        ];
        let result = execute_ls(&words).unwrap();
        let entries: Vec<Entry> = serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(entries[0].name, "large");

        let error = execute_ls(&[
            "ls".to_owned(),
            "--max-entries=1".to_owned(),
            directory.display().to_string(),
        ])
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_ls_rejects_invalid_options_with_actionable_errors() {
        for words in [
            vec!["ls".to_owned(), "--sort=nonsense".to_owned()],
            vec!["ls".to_owned(), "--max-entries=0".to_owned()],
            vec!["ls".to_owned(), "--format=xml".to_owned()],
        ] {
            let error = execute_ls(&words).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidArgument);
            assert!(!error.details.help.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn native_ls_plain_rendering_neutralizes_terminal_controls() {
        let directory = test_directory("terminal-names");
        fs::write(directory.join("\u{1b}[2Jüber\nname"), "content").unwrap();
        let result = execute_ls(&["ls".to_owned(), directory.display().to_string()]).unwrap();
        let output = result.stdout.unwrap();
        assert!(output.contains("\\u{1b}[2Jüber"));
        assert!(!output.contains('\u{1b}'));
        fs::remove_dir_all(directory).unwrap();
    }
}
