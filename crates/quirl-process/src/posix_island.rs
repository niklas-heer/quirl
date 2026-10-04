//! `eval`, `source`, and `.` through the platform POSIX shell.
//!
//! Shell setup code such as `eval "$(ssh-agent -s)"`, `eval "$(brew shellenv)"`,
//! or `source venv/bin/activate` is written for `sh`. Rather than approximate
//! that language, Quirl runs it in `/bin/sh` as an ordinary foreground
//! pipeline and then imports what it changed: the exported environment and the
//! working directory.
//!
//! Failure model and invariants:
//!
//! - The island is one child pipeline. Terminal handoff, job control,
//!   cancellation, deadlines, and output capture are exactly those of any
//!   other foreground command, because the executor spawns it the same way.
//! - Results travel through a file in a private (0700) directory created for
//!   this one island and removed by [`IslandState`]'s `Drop`, including on
//!   every error path. The file is written by an `EXIT` trap, so `exit` inside
//!   sourced code still reports the state it reached.
//! - Nothing is imported unless the dump is complete and well formed: a shell
//!   killed by a signal, a truncated record, or a dump above its byte bound
//!   leaves the session unchanged and the command's status stands.
//! - Session locals are passed to the shell as single-quoted assignments, so
//!   sourced code sees the parameters Quirl expands. Variables the code sets
//!   without `export` stay inside the island, as they would in a subshell.

use quirl_core::{ErrorCode, ShellError};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, DirBuilder, File},
    io::Read,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::DirBuilderExt,
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Interpreter for every island. POSIX requires `/bin/sh` on Unix systems.
pub(crate) const POSIX_SHELL_PATH: &str = "/bin/sh";

/// Largest environment dump accepted: the session environment bound plus
/// room for the working directory and record separators.
const DUMP_BYTES_MAX: usize = crate::SESSION_ENVIRONMENT_BYTES_MAX + 64 * 1024;

/// Variables a shell maintains for itself; importing them would describe the
/// island rather than the session.
const SHELL_MAINTAINED_VARIABLES: [&str; 2] = ["_", "SHLVL"];

/// Session locals the island's shell sets itself and treats as read-only,
/// so passing them in would fail before the user's code runs.
const SHELL_READONLY_VARIABLES: [&str; 3] = ["PPID", "UID", "EUID"];

/// Wrapper run by `sh -c`. Positional parameters are the dump path, the
/// island kind, the code or file, then arguments for sourced code. The
/// session's local variables are spliced in at `@LOCALS@` as quoted data.
const ISLAND_SCRIPT: &str = r#"__quirl_dump=$1
__quirl_kind=$2
shift 2
trap '__quirl_status=$?; { printf "%s\0" "$PWD"; /usr/bin/env -0; } >"$__quirl_dump"; exit "$__quirl_status"' EXIT
@LOCALS@
case $__quirl_kind in
eval) __quirl_code=$1; shift; eval "$__quirl_code" ;;
source) __quirl_file=$1; shift; . "$__quirl_file" ;;
esac
"#;

static ISLAND_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Which builtin requested the island.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IslandKind {
    /// `eval word...`: the words joined by spaces are evaluated.
    Eval,
    /// `source file [arg...]` or `. file [arg...]`.
    Source,
}

impl IslandKind {
    /// Map a builtin name to its island, or `None` for other commands.
    pub(crate) fn for_builtin(name: &str) -> Option<Self> {
        match name {
            "eval" => Some(Self::Eval),
            "source" | "." => Some(Self::Source),
            _ => None,
        }
    }
}

/// The session state a completed island reported.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct IslandOutcome {
    /// Logical working directory the shell ended in.
    pub(crate) directory: PathBuf,
    /// Complete exported environment the shell ended with.
    pub(crate) exported: BTreeMap<OsString, OsString>,
}

/// Private scratch directory owning the dump file for one island.
pub(crate) struct IslandState {
    directory: PathBuf,
}

impl IslandState {
    /// Create a fresh private directory under the system temporary directory.
    pub(crate) fn create() -> Result<Self, ShellError> {
        let sequence = ISLAND_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let directory =
            std::env::temp_dir().join(format!("quirl-island-{}-{sequence}", std::process::id()));
        DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not prepare a POSIX shell island")
                    .with_context(format!("{}: {error}", directory.display()))
                    .with_help("Check that the temporary directory is writable")
            })?;
        Ok(Self { directory })
    }

    fn dump_path(&self) -> PathBuf {
        self.directory.join("environment")
    }

    /// Command words that run `kind` with `operands` in `/bin/sh`.
    ///
    /// `operands` are the builtin's arguments after its name. `locals` are the
    /// session's unexported variables, already validated as shell names.
    pub(crate) fn command_words<'a>(
        &self,
        kind: IslandKind,
        operands: &[String],
        locals: impl Iterator<Item = (&'a OsString, &'a OsString)>,
    ) -> Result<Vec<String>, ShellError> {
        let dump = self
            .dump_path()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                ShellError::new(ErrorCode::Io, "the temporary directory path is not UTF-8")
                    .with_help("Set TMPDIR to a UTF-8 path")
            })?;
        let script = ISLAND_SCRIPT.replace("@LOCALS@", &local_assignments(locals));
        let mut words = vec![
            POSIX_SHELL_PATH.to_owned(),
            "-c".to_owned(),
            script,
            "quirl".to_owned(),
            dump,
        ];
        match kind {
            IslandKind::Eval => {
                words.push("eval".to_owned());
                words.push(operands.join(" "));
            }
            IslandKind::Source => {
                let Some((file, arguments)) = operands.split_first() else {
                    return Err(ShellError::new(
                        ErrorCode::InvalidArgument,
                        "source needs a file to read",
                    )
                    .with_help("Usage: source FILE [ARGUMENT...]"));
                };
                words.push("source".to_owned());
                words.push(source_operand(file));
                words.extend(arguments.iter().cloned());
            }
        }
        Ok(words)
    }

    /// Read what the island reported, or `None` when it reported nothing
    /// complete, such as after being killed by a signal.
    pub(crate) fn outcome(&self) -> Result<Option<IslandOutcome>, ShellError> {
        let file = match File::open(self.dump_path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ShellError::new(
                    ErrorCode::Io,
                    "could not read the POSIX shell island state",
                )
                .with_context(error.to_string())
                .with_help("Retry the command"));
            }
        };
        let mut bytes = Vec::new();
        let limit = u64::try_from(DUMP_BYTES_MAX.saturating_add(1)).unwrap_or(u64::MAX);
        file.take(limit).read_to_end(&mut bytes).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read the POSIX shell island state")
                .with_context(error.to_string())
                .with_help("Retry the command")
        })?;
        if bytes.len() > DUMP_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "the POSIX shell island's environment exceeds its limit",
            )
            .with_context(format!(
                "limit {DUMP_BYTES_MAX} bytes; observed more than {DUMP_BYTES_MAX}"
            ))
            .with_help("Export fewer or shorter variables from the sourced code"));
        }
        Ok(parse_dump(bytes))
    }
}

impl Drop for IslandState {
    fn drop(&mut self) {
        // Best effort: the directory is private to this user and process.
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// `sh`'s `.` searches only `PATH` for a name without a slash, but people
/// write `source .env` meaning the file here, as Bash and Zsh allow.
fn source_operand(file: &str) -> String {
    if !file.contains('/') && Path::new(file).is_file() {
        return format!("./{file}");
    }
    file.to_owned()
}

/// Render locals as `NAME='value'` lines. Names are validated identifiers
/// and every value is single-quoted, so no session data becomes code.
fn local_assignments<'a>(locals: impl Iterator<Item = (&'a OsString, &'a OsString)>) -> String {
    let mut rendered = String::new();
    for (name, value) in locals {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            continue;
        };
        if !is_shell_name(name) || SHELL_READONLY_VARIABLES.contains(&name) {
            continue;
        }
        rendered.push_str(name);
        rendered.push_str("='");
        rendered.push_str(&value.replace('\'', r"'\''"));
        rendered.push_str("'\n");
    }
    rendered
}

/// Parse `PWD\0NAME=VALUE\0...`. Any malformed record rejects the dump.
fn parse_dump(bytes: Vec<u8>) -> Option<IslandOutcome> {
    if bytes.last() != Some(&0) {
        return None;
    }
    let mut records = bytes.split(|byte| *byte == 0);
    let directory = records.next().filter(|record| !record.is_empty())?;
    let directory = PathBuf::from(OsString::from_vec(directory.to_vec()));
    let mut exported = BTreeMap::new();
    for record in records {
        if record.is_empty() {
            continue;
        }
        let separator = record.iter().position(|byte| *byte == b'=')?;
        let (name, value) = record.split_at(separator);
        let value = value.get(1..)?;
        let name = OsString::from_vec(name.to_vec());
        if SHELL_MAINTAINED_VARIABLES
            .iter()
            .any(|maintained| name.as_bytes() == maintained.as_bytes())
        {
            continue;
        }
        exported.insert(name, OsString::from_vec(value.to_vec()));
    }
    Some(IslandOutcome {
        directory,
        exported,
    })
}

/// Whether `name` is a POSIX shell variable name.
pub(crate) fn is_shell_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_parse_directory_and_exports_without_shell_bookkeeping() {
        let parsed = parse_dump(b"/tmp\0A=1\0SHLVL=2\0_=/usr/bin/env\0B=x=y\0".to_vec()).unwrap();
        assert_eq!(parsed.directory, PathBuf::from("/tmp"));
        assert_eq!(
            parsed.exported,
            BTreeMap::from([
                (OsString::from("A"), OsString::from("1")),
                (OsString::from("B"), OsString::from("x=y")),
            ])
        );
    }

    #[test]
    fn truncated_or_malformed_dumps_import_nothing() {
        assert_eq!(parse_dump(Vec::new()), None);
        assert_eq!(parse_dump(b"/tmp\0A=1".to_vec()), None);
        assert_eq!(parse_dump(b"/tmp\0NOEQUALS\0".to_vec()), None);
        assert_eq!(parse_dump(b"\0A=1\0".to_vec()), None);
    }

    #[test]
    fn locals_become_quoted_assignments_and_never_code() {
        let name = OsString::from("GREETING");
        let value = OsString::from("it's $(rm -rf /)");
        let invalid = OsString::from("not-a-name");
        let readonly = OsString::from("PPID");
        let rendered = local_assignments(
            [(&name, &value), (&invalid, &value), (&readonly, &value)].into_iter(),
        );
        assert_eq!(rendered, "GREETING='it'\\''s $(rm -rf /)'\n");
    }

    #[test]
    fn island_runs_source_and_reports_exports_directory_and_status() {
        let state = IslandState::create().unwrap();
        let script = state.directory.join("setup.sh");
        fs::write(
            &script,
            "export ISLAND_VALUE=\"$GREETING-$1\"\ncd /\nexit 3\n",
        )
        .unwrap();
        let greeting = (OsString::from("GREETING"), OsString::from("it's"));
        let words = state
            .command_words(
                IslandKind::Source,
                &[script.display().to_string(), "arg".to_owned()],
                std::iter::once((&greeting.0, &greeting.1)),
            )
            .unwrap();
        let status = std::process::Command::new(&words[0])
            .args(&words[1..])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(3));
        let outcome = state.outcome().unwrap().unwrap();
        assert_eq!(outcome.directory, PathBuf::from("/"));
        assert_eq!(
            outcome.exported.get(&OsString::from("ISLAND_VALUE")),
            Some(&OsString::from("it's-arg"))
        );
        assert!(!outcome.exported.contains_key(&OsString::from("GREETING")));
    }

    #[test]
    fn island_state_directory_is_removed_on_drop() {
        let state = IslandState::create().unwrap();
        let directory = state.directory.clone();
        assert!(directory.is_dir());
        drop(state);
        assert!(!directory.exists());
    }

    #[test]
    fn source_without_a_file_explains_its_usage() {
        let state = IslandState::create().unwrap();
        let error = state
            .command_words(IslandKind::Source, &[], std::iter::empty())
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
}
