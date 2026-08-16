use crate::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, UNIX_EPOCH},
};

/// Bounded process request issued by a sandboxed host such as the Lua runtime.
/// The composition root supplies the implementation, keeping platform process
/// code out of the extension runtime's dependency graph.
#[derive(Clone)]
pub struct ProcessRequest {
    pub command: String,
    pub deadline: Duration,
    pub cancelled: Arc<AtomicBool>,
    pub max_output_bytes: usize,
}

pub type ProcessHost =
    Arc<dyn Fn(ProcessRequest) -> Result<CommandOutcome, ShellError> + Send + Sync>;

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
    /// The filename as supplied by the filesystem. This is machine data, not
    /// terminal text: use [`Entry::display_name`] when rendering it for a
    /// person.
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified_unix_seconds: Option<u64>,
    pub hidden: bool,
    /// A link target only when it was explicitly requested through
    /// [`DirectoryOptions::resolve_symlink_targets`]. Keeping this optional
    /// avoids an extra filesystem operation for the common listing path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<PathBuf>,
    /// Whether the containing metadata is read-only according to the host
    /// filesystem. This is advisory metadata, not an authorization decision.
    #[serde(default)]
    pub readonly: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// The attribute used to produce a deterministic directory listing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DirectorySort {
    #[default]
    Name,
    Size,
    Modified,
    Kind,
}

/// Bounded, explicit behaviour for the native directory lister.
///
/// Directory traversal itself is deliberately non-recursive. Metadata is
/// collected only for entries returned by this call; symlink target resolution
/// is an opt-in enrichment because it performs an additional filesystem read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryOptions {
    pub show_all: bool,
    pub sort: DirectorySort,
    pub reverse: bool,
    pub directories_first: bool,
    pub max_entries: usize,
    pub resolve_symlink_targets: bool,
}

impl Default for DirectoryOptions {
    fn default() -> Self {
        Self {
            show_all: false,
            sort: DirectorySort::Name,
            reverse: false,
            directories_first: false,
            // Listing a directory must not accidentally turn an interactive
            // command into an unbounded allocation on a pathological tree.
            max_entries: 10_000,
            resolve_symlink_targets: false,
        }
    }
}

impl Entry {
    /// A terminal-safe representation of [`Entry::name`].
    ///
    /// JSON callers retain the original filename (where serde_json escapes
    /// controls correctly); human renderers must use this representation.
    pub fn display_name(&self) -> String {
        crate::escape_terminal_controls(&self.name)
            .replace('\n', "\\n")
            .replace('\t', "\\t")
    }
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
                    options.sort = parse_directory_sort(&value[7..], words)?;
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
                    options.max_entries = parse_max_entries(&value[14..], words)?;
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
                    json = parse_ls_format(&value[9..], words)?;
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
                    .with_command(words.join(" ")));
                }
            }
            index += 1;
        }
        let path = path.unwrap_or_else(|| PathBuf::from("."));
        let entries = directory_entries_with_options(&path, &options)?;
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

pub fn directory_entries(path: &Path, show_all: bool) -> Result<Vec<Entry>, ShellError> {
    directory_entries_with_options(
        path,
        &DirectoryOptions {
            show_all,
            // Preserve the historical public helper's presentation order.
            directories_first: true,
            ..DirectoryOptions::default()
        },
    )
}

/// Enumerate a single directory with a bounded, deterministic result.
pub fn directory_entries_with_options(
    path: &Path,
    options: &DirectoryOptions,
) -> Result<Vec<Entry>, ShellError> {
    if options.max_entries == 0 {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "directory listing entry limit must be greater than zero",
        )
        .with_help("Set `max_entries` to a positive bound"));
    }
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
        if hidden && !options.show_all {
            continue;
        }
        if entries.len() == options.max_entries {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!(
                    "directory {} exceeds the {} entry listing limit",
                    path.display(),
                    options.max_entries
                ),
            )
            .with_help("Choose a narrower directory or increase the explicit listing limit"));
        }
        let entry_path = item.path();
        let metadata = match fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata,
            // A directory may change while it is listed. A vanished entry is
            // not an error in the listing itself; retain failures for all
            // other metadata errors so permissions problems remain visible.
            Err(error) if is_not_found(&error) => continue,
            Err(error) => {
                return Err(ShellError::new(
                    ErrorCode::Io,
                    format!("cannot inspect {}", entry_path.display()),
                )
                .with_context(error.to_string())
                .with_help("Check that the directory remains readable while it is listed"));
            }
        };
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        let symlink_target = if options.resolve_symlink_targets && file_type.is_symlink() {
            match fs::read_link(&entry_path) {
                Ok(target) => Some(target),
                Err(error) if is_not_found(&error) => continue,
                Err(error) => {
                    return Err(ShellError::new(
                        ErrorCode::Io,
                        format!("cannot resolve symlink {}", entry_path.display()),
                    )
                    .with_context(error.to_string())
                    .with_help(
                        "Check that the symlink remains readable, or list without --resolve-links",
                    ));
                }
            }
        } else {
            None
        };
        entries.push(Entry {
            name,
            path: entry_path,
            kind,
            size: metadata.len(),
            modified_unix_seconds: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs()),
            hidden,
            symlink_target,
            readonly: metadata.permissions().readonly(),
        });
    }
    entries.sort_by(|left, right| compare_entries(left, right, options));
    Ok(entries)
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
    let parsed = value.parse::<usize>().ok().filter(|value| *value > 0);
    parsed.ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("ls --max-entries needs a positive integer, got `{value}`"),
        )
        .with_command(words.join(" "))
        .with_help("Use a positive bound, for example `ls --max-entries=1000`")
    })
}

fn is_not_found(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
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

fn compare_entries(left: &Entry, right: &Entry, options: &DirectoryOptions) -> Ordering {
    let directories = if options.directories_first {
        matches!(right.kind, EntryKind::Directory).cmp(&matches!(left.kind, EntryKind::Directory))
    } else {
        Ordering::Equal
    };
    let selected = match options.sort {
        DirectorySort::Name => folded_name(left).cmp(&folded_name(right)),
        DirectorySort::Size => left.size.cmp(&right.size),
        DirectorySort::Modified => left.modified_unix_seconds.cmp(&right.modified_unix_seconds),
        DirectorySort::Kind => entry_kind_rank(left.kind).cmp(&entry_kind_rank(right.kind)),
    }
    // A complete tie-breaker prevents filesystem iteration order from
    // leaking into the result (including case-fold collisions).
    .then_with(|| left.name.cmp(&right.name))
    .then_with(|| left.path.cmp(&right.path));
    directories.then(if options.reverse {
        selected.reverse()
    } else {
        selected
    })
}

fn folded_name(entry: &Entry) -> String {
    entry.name.to_lowercase()
}

fn entry_kind_rank(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Directory => 0,
        EntryKind::File => 1,
        EntryKind::Symlink => 2,
        EntryKind::Other => 3,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn test_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "quirl-core-process-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

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

    #[test]
    fn native_ls_accepts_explicit_formats_and_sort_controls() {
        let directory = test_directory("native-options");
        fs::write(directory.join("small"), "x").unwrap();
        fs::write(directory.join("large"), "xxxx").unwrap();
        let runner = CommandRunner::new("/bin/sh");

        let command = format!(
            "ls --format json --sort=size --reverse {path}",
            path = directory.display()
        );
        let result = runner.execute_capture(&command).unwrap();
        let entries: Vec<Entry> = serde_json::from_str(result.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(entries[0].name, "large");

        let result = runner
            .execute_capture(&format!("ls -lr {path}", path = directory.display()))
            .unwrap();
        // kind, byte count, modification time, and terminal-safe name.
        assert_eq!(result.stdout.unwrap().split_whitespace().count(), 8);

        let limited = runner
            .execute_capture(&format!(
                "ls --max-entries 1 {path}",
                path = directory.display()
            ))
            .unwrap_err();
        assert_eq!(limited.code, ErrorCode::ResourceLimit);

        let one = runner
            .execute_capture(&format!(
                "ls --max-entries=2 --format json {path}",
                path = directory.display()
            ))
            .unwrap();
        let entries: Vec<Entry> = serde_json::from_str(one.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(entries.len(), 2);

        for value in ["0", "not-a-number"] {
            let error = runner
                .execute_capture(&format!(
                    "ls --max-entries={value} {path}",
                    path = directory.display()
                ))
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidArgument);
            assert!(!error.details.help.is_empty());
        }

        let error = runner
            .execute_capture(&format!(
                "ls --sort nonsense {path}",
                path = directory.display()
            ))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(!error.details.help.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn listing_is_bounded_and_reports_how_to_recover() {
        let directory = test_directory("limit");
        fs::write(directory.join("one"), "one").unwrap();
        fs::write(directory.join("two"), "two").unwrap();
        let error = directory_entries_with_options(
            &directory,
            &DirectoryOptions {
                max_entries: 0,
                ..DirectoryOptions::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(!error.details.help.is_empty());

        let error = directory_entries_with_options(
            &directory,
            &DirectoryOptions {
                max_entries: 1,
                ..DirectoryOptions::default()
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(!error.details.help.is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn listing_uses_a_total_deterministic_order_and_optional_directory_grouping() {
        let directory = test_directory("ordering");
        fs::write(directory.join("zebra"), "z").unwrap();
        fs::write(directory.join("Alpha"), "a").unwrap();
        fs::write(directory.join("beta"), "a").unwrap();
        fs::create_dir(directory.join("middle")).unwrap();

        let plain =
            directory_entries_with_options(&directory, &DirectoryOptions::default()).unwrap();
        let names = plain
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Alpha", "beta", "middle", "zebra"]);

        let directories_first = directory_entries_with_options(
            &directory,
            &DirectoryOptions {
                directories_first: true,
                reverse: true,
                ..DirectoryOptions::default()
            },
        )
        .unwrap();
        assert_eq!(directories_first[0].name, "middle");
        assert_eq!(directories_first[1].name, "zebra");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn plain_rendering_neutralizes_hostile_and_unicode_filenames() {
        let directory = test_directory("terminal-names");
        let hostile = "\u{1b}[2Jüber\nname";
        fs::write(directory.join(hostile), "content").unwrap();
        let entries =
            directory_entries_with_options(&directory, &DirectoryOptions::default()).unwrap();
        let output = render_entries(&entries, false);
        assert!(output.contains("\\u{1b}[2Jüber"));
        assert!(!output.contains('\u{1b}'));
        assert!(output.contains("name"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn long_listing_uses_a_fixed_width_kind_column() {
        let entries = [
            Entry {
                name: "directory".to_owned(),
                path: PathBuf::from("directory"),
                kind: EntryKind::Directory,
                size: 1,
                modified_unix_seconds: Some(2),
                hidden: false,
                symlink_target: None,
                readonly: false,
            },
            Entry {
                name: "other".to_owned(),
                path: PathBuf::from("other"),
                kind: EntryKind::Other,
                size: 1,
                modified_unix_seconds: Some(2),
                hidden: false,
                symlink_target: None,
                readonly: false,
            },
        ];
        let output = render_entries(&entries, true);
        let columns = output
            .lines()
            .map(|line| line.find("         1").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(columns, vec![7, 7]);
    }

    #[test]
    fn only_not_found_errors_are_treated_as_listing_races() {
        assert!(is_not_found(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
        assert!(!is_not_found(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_only_read_when_requested() {
        use std::os::unix::fs::symlink;

        let directory = test_directory("symlink");
        fs::write(directory.join("target"), "content").unwrap();
        symlink("target", directory.join("link")).unwrap();

        let without_target =
            directory_entries_with_options(&directory, &DirectoryOptions::default()).unwrap();
        let link = without_target
            .iter()
            .find(|entry| entry.name == "link")
            .unwrap();
        assert_eq!(link.kind, EntryKind::Symlink);
        assert_eq!(link.symlink_target, None);

        let with_target = directory_entries_with_options(
            &directory,
            &DirectoryOptions {
                resolve_symlink_targets: true,
                ..DirectoryOptions::default()
            },
        )
        .unwrap();
        let link = with_target
            .iter()
            .find(|entry| entry.name == "link")
            .unwrap();
        assert_eq!(link.symlink_target.as_deref(), Some(Path::new("target")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn older_entry_json_remains_readable() {
        let entry: Entry = serde_json::from_str(
            r#"{"name":"plain","path":"plain","kind":"file","size":4,"modified_unix_seconds":null,"hidden":false}"#,
        )
        .unwrap();
        assert_eq!(entry.symlink_target, None);
        assert!(!entry.readonly);
    }
}
