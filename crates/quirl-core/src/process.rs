use crate::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, UNIX_EPOCH},
};

/// Bounded process request issued by a sandboxed host such as the Lua runtime.
/// The composition root supplies the implementation, keeping platform process
/// code out of the extension runtime's dependency graph.
#[derive(Clone)]
pub struct ProcessRequest {
    /// Complete command source interpreted by the composed process backend.
    pub command: String,
    /// Remaining wall-time budget for this request.
    ///
    /// The backend starts this relative duration when it accepts the request;
    /// a zero duration must fail with [`ErrorCode::ResourceLimit`].
    pub deadline: Duration,
    /// Shared cancellation flag that the backend must observe during execution.
    pub cancelled: Arc<AtomicBool>,
    /// Maximum bytes the caller permits the backend to retain per captured stream.
    ///
    /// A backend may impose a tighter limit. It must continue draining excess
    /// child output to avoid deadlock before returning a resource-limit error.
    pub max_output_bytes: usize,
}

/// Thread-safe process capability injected into runtimes that cannot own host processes.
///
/// Implementations must enforce every bound in [`ProcessRequest`], reap all
/// children on every exit path, and report operating failures as [`ShellError`].
pub type ProcessHost =
    Arc<dyn Fn(ProcessRequest) -> Result<CommandOutcome, ShellError> + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// Process-style result shared by native, data, Lua, and recovery surfaces.
pub struct CommandOutcome {
    /// Exit status reported by the command; zero conventionally indicates success.
    pub status: i32,
    /// Captured standard output, or `None` when output was inherited.
    pub stdout: Option<String>,
    /// Captured standard error, or `None` when output was inherited.
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// Filesystem metadata for one immediate child of a listed directory.
///
/// Paths and names remain machine data. Human-facing renderers must escape them
/// before writing to a terminal.
pub struct Entry {
    /// The filename as supplied by the filesystem. This is machine data, not
    /// terminal text: use [`Entry::display_name`] when rendering it for a
    /// person.
    pub name: String,
    /// Full path produced by joining the requested directory and entry name.
    pub path: PathBuf,
    /// Kind derived from symlink metadata without following links.
    pub kind: EntryKind,
    /// Metadata length in bytes; directory and special-file meanings are platform-defined.
    pub size: u64,
    /// Whole seconds since the Unix epoch, or `None` when unavailable or pre-epoch.
    pub modified_unix_seconds: Option<u64>,
    /// Whether the filename begins with `.` under Quirl's portable hidden-file rule.
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
/// Non-following filesystem classification for a directory entry.
pub enum EntryKind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// A symbolic link, regardless of its target kind.
    Symlink,
    /// A socket, device, FIFO, or another platform-specific file type.
    Other,
}

/// The attribute used to produce a deterministic directory listing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum DirectorySort {
    /// Case-folded filename, with exact name and path as deterministic tie-breakers.
    #[default]
    Name,
    /// Metadata length in ascending byte order before tie-breakers.
    Size,
    /// Optional Unix modification time in ascending order before tie-breakers.
    Modified,
    /// Directory, file, symlink, then other before tie-breakers.
    Kind,
}

/// Bounded, explicit behaviour for the native directory lister.
///
/// Directory traversal itself is deliberately non-recursive. Metadata is
/// collected only for entries returned by this call; symlink target resolution
/// is an opt-in enrichment because it performs an additional filesystem read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryOptions {
    /// Include dot-prefixed entries when `true`.
    pub show_all: bool,
    /// Primary deterministic ordering key.
    pub sort: DirectorySort,
    /// Reverse the selected ordering within directory/file groups.
    pub reverse: bool,
    /// Keep directories ahead of non-directories independently of [`Self::reverse`].
    pub directories_first: bool,
    /// Maximum number of retained entries; zero is invalid.
    pub max_entries: usize,
    /// Read symbolic-link targets as an explicit additional filesystem operation.
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
        crate::escape_terminal_line(&self.name)
    }
}

/// Enumerate one directory using the bounded default options.
///
/// Results are non-recursive, capped at 10,000 retained entries, sorted by name
/// with directories first, and do not resolve symlink targets. Dot-prefixed
/// names are included only when `show_all` is true. Returns [`ErrorCode::Io`]
/// for directory or metadata failures and [`ErrorCode::ResourceLimit`] when the
/// retained-entry limit is exceeded.
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
///
/// The function never recurses or follows symlinks for classification. Entries
/// that vanish concurrently are skipped; other filesystem failures are
/// returned as [`ErrorCode::Io`]. A zero `max_entries` returns
/// [`ErrorCode::InvalidArgument`], while observing more retained entries than
/// the configured bound returns [`ErrorCode::ResourceLimit`].
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
            let observed = options.max_entries.saturating_add(1);
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!(
                    "directory {} exceeds the {} entry listing limit",
                    path.display(),
                    options.max_entries
                ),
            )
            .with_context(format!(
                "limit: {}; observed: {observed}",
                options.max_entries
            ))
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

fn is_not_found(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env, fs,
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
