//! `eval`, `source`, and `.` through the platform POSIX shell.
//!
//! Shell setup code such as `eval "$(ssh-agent -s)"`, `eval "$(brew shellenv)"`,
//! `source venv/bin/activate`, or `source .env` is written for `sh`. Rather
//! than approximate that language, Quirl runs it in `/bin/sh` as an ordinary
//! foreground pipeline and then imports what it changed:
//!
//! - the exported environment, which replaces the session's exports;
//! - the logical working directory;
//! - unexported shell variables the code assigned, which become session
//!   locals, and session locals the code unset, which the session removes.
//!   A variable counts as assigned when, at exit, it is set, unexported, and
//!   its value or export attribute differs from a baseline taken after the
//!   session locals were injected and before the code ran. Shell bookkeeping
//!   such as `RANDOM`, `LINENO`, `BASHPID`, or `BASH_*` never counts, and
//!   `IFS` or `PS1` count only when the code changed them, because the
//!   baseline already holds the shell's defaults.
//!
//! Functions, aliases, arrays, readonly attributes, traps, positional
//! parameters, and shell options stay inside the island.
//!
//! Trust boundary: sourced code is trusted. It runs with the user's rights and
//! can already set any variable, so the import does not defend against it.
//! The import guarantees only that variable values stay data, so no value can
//! spoof another variable's name or inject code into the session, and that a
//! broken or interrupted dump imports nothing.
//!
//! Failure model and invariants:
//!
//! - The island is one child pipeline. Terminal handoff, job control,
//!   cancellation, deadlines, and output capture are exactly those of any
//!   other foreground command, because the executor spawns it the same way.
//! - Results travel through two files in a private (0700) directory created
//!   for this one island and removed by [`IslandState`]'s `Drop`, including on
//!   every error path. The baseline is written before the code runs; the final
//!   dump is written by an `EXIT` trap, so `exit` inside sourced code still
//!   reports the state it reached.
//! - Values are never parsed or evaluated. `set` output only nominates
//!   candidate names; the shell confirms each one with `${NAME+x}` and prints
//!   `NAME=value` with its builtin `printf`, NUL-terminated. A multi-line value
//!   containing a line such as `FAKE=spoofed` nominates `FAKE`, which is then
//!   dropped unless the code really set it. A name that some line lists as a
//!   Bash array is checked with `declare -p`, so a value cannot make an array
//!   look scalar. The only `eval` in the dump code receives a name that
//!   matched ASCII ranges under `LC_ALL=C`.
//! - The `EXIT` trap turns off tracing and removes aliases and functions
//!   shadowing the commands it runs before it reads any variable, so code
//!   that left `set -x` on or redefined `printf` neither prints values nor
//!   rewrites the dump.
//! - Nothing is imported unless both dumps are complete and well formed:
//!   framed by fixed header and trailer records, with every variable name
//!   valid. A shell killed by a signal, a truncated record, a dump above its
//!   byte bound, or a state the trap cannot report (`readonly IFS`,
//!   `unset PWD`, a replaced `EXIT` trap) leaves the session unchanged, the
//!   command's status stands, and the executor warns that nothing was
//!   imported, with the [`MissingState`] reason.
//! - Each island costs two `set` snapshots and two dump loops, linear in the
//!   number of set variables: about 10 to 15 ms for a 30-line dotenv file
//!   under Bash 3.2 on an Apple Silicon Mac.
//! - Session locals are passed to the shell as single-quoted assignments, so
//!   sourced code sees the parameters Quirl expands.
//! - With the session's allexport flag on, the code runs after `set -a`, so
//!   its bare assignments come back exported, as `set -a; source .env` does
//!   in Bash.

use quirl_core::{ErrorCode, ShellError};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
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

/// Largest dump file accepted. The final dump holds the exported environment
/// and then every set variable, exported ones included, so it can hold the
/// bounded session environment twice, plus room for the working directory
/// and framing records.
const DUMP_BYTES_MAX: usize = crate::SESSION_ENVIRONMENT_BYTES_MAX
    .saturating_mul(2)
    .saturating_add(64 * 1024);

/// Exported variables a shell maintains for itself; importing them would
/// describe the island rather than the session.
const SHELL_MAINTAINED_VARIABLES: [&str; 2] = ["_", "SHLVL"];

/// Variables a shell maintains for itself, or that describe the island's
/// process rather than the session. They never become session locals and are
/// never removed from the session. Bash's own `BASH`, `BASHOPTS`, `BASHPID`,
/// and every `BASH_*` name are excluded too, because Bash adds such variables
/// between versions; see [`is_bookkeeping`]. Keep the `source` and `eval`
/// help in `quirl-catalog` in step with this list.
const SHELL_BOOKKEEPING_VARIABLES: [&str; 25] = [
    "_",
    "SHLVL",
    "PPID",
    "UID",
    "EUID",
    "OPTIND",
    "OPTARG",
    "LINENO",
    "RANDOM",
    "SRANDOM",
    "SECONDS",
    "EPOCHREALTIME",
    "EPOCHSECONDS",
    "PIPESTATUS",
    "FUNCNAME",
    "GROUPS",
    "DIRSTACK",
    "HISTCMD",
    "SHELLOPTS",
    "POSIXLY_CORRECT",
    "BASH",
    "BASHOPTS",
    "BASHPID",
    // The working directory reaches the session through `cd`, not as text.
    "PWD",
    "OLDPWD",
];

/// Prefix of the wrapper's own variables. They describe the island, so they
/// are never imported, and a session export with this prefix is kept as is.
pub(crate) const RESERVED_PREFIX: &str = "__quirl_";

/// Session locals the island's shell sets itself and treats as read-only,
/// so passing them in would stop the island before the user's code runs.
const SHELL_READONLY_VARIABLES: [&str; 6] = [
    "PPID",
    "UID",
    "EUID",
    "SHELLOPTS",
    "BASHOPTS",
    "BASH_VERSINFO",
];

/// Whether the island's shell owns `name`, so assigning it at `@LOCALS@`
/// would stop the island before the user's code runs. Bash 3.2 exits with
/// status 127 and no message on an assignment to `GROUPS`, `FUNCNAME`, or
/// one of its `BASH_*` arrays such as `BASH_ARGV`, so every `BASH_*` name is
/// skipped rather than only those known to be fatal today.
fn is_shell_special_variable(name: &str) -> bool {
    SHELL_READONLY_VARIABLES.contains(&name)
        || name == "GROUPS"
        || name == "FUNCNAME"
        || name.starts_with("BASH_")
}

/// First record of the final dump.
const DUMP_HEADER: &[u8] = b"quirl-island-v1";
/// Record that opens a variable section, in both dumps.
const VARIABLES_HEADER: &[u8] = b"quirl-island-variables";
/// Last record of both dumps. Its presence proves the writer finished.
const DUMP_TRAILER: &[u8] = b"quirl-island-end";

/// Wrapper run by `sh -c`. Positional parameters are the final dump path,
/// the baseline path, the island kind, the code or file, then arguments for
/// sourced code. The session's local variables are spliced in at `@LOCALS@`
/// as quoted data, `@ALLEXPORT@` becomes `set -a; ` when the session exports
/// every assignment, and `@VARIABLES@` becomes [`VARIABLES_DUMP`].
///
/// The `EXIT` trap first records the status, then turns off tracing (`-x`,
/// `-v`) so the dump never echoes values to standard error, and removes every
/// alias and any function shadowing a command the dump uses, so code that
/// redefined `printf` or aliased `eval` cannot rewrite the dump. In POSIX
/// mode, special builtins such as `unset` and `set` cannot be shadowed by
/// functions, and quoting their names bypasses aliases. Code that leaves
/// POSIX mode (`set +o posix`) and then redefines `set`, or marks a function
/// read-only with `readonly -f`, can still defeat this scrub; such code is
/// trusted, and the reader rejects the empty or malformed dump it leaves. It
/// then records the code's `IFS` and `LC_ALL` and turns off options that
/// would stop the dump (`-e`, `-u`) or export its own scratch variables
/// (`-a`). The code's functions and
/// options cannot outlive the shell, so changing them at exit is invisible.
/// Both dumps run with a newline `IFS` and `LC_ALL=C`, set only after `env -0`
/// has reported the code's exported values, which makes Bash's pattern
/// matching byte-wise and roughly 40% faster; the baseline puts the session's
/// `IFS` and `LC_ALL` back before the code runs.
const ISLAND_SCRIPT: &str = r#"__quirl_dump=$1
__quirl_baseline=$2
__quirl_kind=$3
shift 3
trap '__quirl_status=$?
{ \set +x +v; } 2>/dev/null
\unset -f unalias printf builtin declare 2>/dev/null
\unalias -a 2>/dev/null
__quirl_ifs_set=${IFS+x}
__quirl_ifs=${IFS-}
__quirl_lc_set=${LC_ALL+x}
__quirl_lc=${LC_ALL-}
\set +a +e +u
__quirl_set=$(\set)
{ \printf "%s\0" quirl-island-v1 "$PWD"
/usr/bin/env -0
IFS="
"
\set -f
LC_ALL=C
@VARIABLES@
} >"$__quirl_dump"
\exit "$__quirl_status"' EXIT
@LOCALS@
__quirl_ifs_set=${IFS+x}
__quirl_ifs=${IFS-}
__quirl_lc_set=${LC_ALL+x}
__quirl_lc=${LC_ALL-}
__quirl_set=$(set)
IFS="
"
set -f
LC_ALL=C
{ @VARIABLES@
} >"$__quirl_baseline"
set +f
case $__quirl_ifs_set in x) IFS=$__quirl_ifs ;; *) unset IFS ;; esac
case $__quirl_lc_set in x) LC_ALL=$__quirl_lc ;; *) unset LC_ALL ;; esac
unset __quirl_set __quirl_line __quirl_name __quirl_value __quirl_isset __quirl_arrays __quirl_flags __quirl_ifs __quirl_ifs_set __quirl_lc __quirl_lc_set
case $__quirl_kind in
eval) __quirl_code=$1; shift; @ALLEXPORT@eval "$__quirl_code" ;;
source) __quirl_file=$1; shift; @ALLEXPORT@. "$__quirl_file" ;;
esac
"#;

/// Shell code that prints every set scalar variable as a NUL-terminated
/// `NAME=value` record between [`VARIABLES_HEADER`] and [`DUMP_TRAILER`].
///
/// Expects `$__quirl_set` to hold `set` output, `IFS` to be a newline,
/// globbing to be off, and `LC_ALL=C`, so each output line is one loop word
/// and the bracket ranges below cover ASCII bytes only, whatever the user's
/// locale. A line only nominates the name before its first `=`; the name must
/// be a shell name, and the shell then confirms it is set before printing
/// the value. `IFS` and `LC_ALL` report the values saved before the dump
/// replaced them.
///
/// Bash lists arrays as `NAME=(...)`, while every scalar value beginning with
/// `(` is quoted, so those lines are skipped. A line inside a multi-line value
/// can still nominate an array's name with a scalar-looking value, so the
/// first pass collects every name some line lists as `NAME=(`, and the second
/// asks Bash's own `declare -p` whether each such name really is an array.
/// That costs one subshell per collected name only; Dash has no `declare`, and
/// no arrays either, so there the check finds a scalar. The code contains no
/// single quote, because it is spliced into the single-quoted `EXIT` trap.
const VARIABLES_DUMP: &str = r#"\printf "%s\0" quirl-island-variables
__quirl_arrays=" "
for __quirl_line in $__quirl_set
do case ${__quirl_line#*=} in "("*) __quirl_arrays="$__quirl_arrays${__quirl_line%%=*} " ;; esac
done
for __quirl_line in $__quirl_set
do case $__quirl_line in *=*) ;; *) continue ;; esac
__quirl_name=${__quirl_line%%=*}
case $__quirl_name in
""|[0-9]*|*[!_A-Za-z0-9]*) continue ;;
__quirl_*) continue ;;
IFS) __quirl_isset=$__quirl_ifs_set __quirl_value=$__quirl_ifs ;;
LC_ALL) __quirl_isset=$__quirl_lc_set __quirl_value=$__quirl_lc ;;
*) case ${__quirl_line#*=} in "("*) continue ;; esac
case $__quirl_arrays in *" $__quirl_name "*)
__quirl_flags=$(\builtin declare -p "$__quirl_name" 2>/dev/null)
__quirl_flags=${__quirl_flags#declare -}
case ${__quirl_flags%% *} in *[aA]*) continue ;; esac ;;
esac
\eval "__quirl_isset=\${$__quirl_name+x} __quirl_value=\${$__quirl_name-}" ;;
esac
case $__quirl_isset in x) \printf "%s=%s\0" "$__quirl_name" "$__quirl_value" ;; esac
done
\printf "%s\0" quirl-island-end"#;

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
    /// Every scalar variable set at exit, exported or not.
    variables: BTreeMap<OsString, OsString>,
    /// Every scalar variable set before the code ran.
    baseline: BTreeMap<OsString, OsString>,
}

/// Why an island reported no complete state, so the session imported
/// nothing from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MissingState {
    /// The shell stopped before the code ran, so no baseline exists.
    NoBaseline,
    /// The shell ended without its `EXIT` trap writing the final dump.
    NoFinalDump,
    /// A dump exists but is incomplete or malformed.
    Malformed,
    /// A dump exceeds its byte bound, so it was not read.
    Oversized,
}

impl MissingState {
    /// A short explanation for the warning the session prints.
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::NoBaseline => "the shell stopped before the code ran",
            Self::NoFinalDump => {
                "the shell ended before reporting its variables, as when it is killed by a signal, runs `exec`, or the code replaces the `EXIT` trap"
            }
            Self::Malformed => {
                "the shell's report of its variables was incomplete, as after `readonly IFS` or `unset PWD`"
            }
            Self::Oversized => "the shell's report of its variables exceeded its size limit",
        }
    }
}

/// How an island changed the session's unexported variables.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct LocalChanges {
    /// Unexported variables the code assigned, with their final values.
    pub(crate) assigned: BTreeMap<OsString, OsString>,
    /// Session locals the code unset.
    pub(crate) removed: BTreeSet<OsString>,
}

impl IslandOutcome {
    /// Compare the final variables with the baseline.
    ///
    /// `exported_before` and `locals_before` are the session's maps when the
    /// island started. A variable exported then and unexported now, as after
    /// `unset X; X=same`, counts as assigned even with an unchanged value,
    /// because its export attribute changed.
    pub(crate) fn local_changes(
        &self,
        exported_before: &BTreeMap<OsString, OsString>,
        locals_before: &BTreeMap<OsString, OsString>,
    ) -> LocalChanges {
        let mut changes = LocalChanges::default();
        for (name, value) in &self.variables {
            if is_bookkeeping(name) || self.exported.contains_key(name) {
                continue;
            }
            let value_changed = self.baseline.get(name) != Some(value);
            let export_dropped = exported_before.contains_key(name);
            if value_changed || export_dropped {
                changes.assigned.insert(name.clone(), value.clone());
            }
        }
        for name in locals_before.keys() {
            let unset_by_code = self.baseline.contains_key(name)
                && !self.variables.contains_key(name)
                && !self.exported.contains_key(name);
            if unset_by_code && !is_bookkeeping(name) {
                changes.removed.insert(name.clone());
            }
        }
        changes
    }
}

/// Private scratch directory owning the dump files for one island.
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
        self.directory.join("final")
    }

    fn baseline_path(&self) -> PathBuf {
        self.directory.join("baseline")
    }

    /// Command words that run `kind` with `operands` in `/bin/sh`.
    ///
    /// `operands` are the builtin's arguments after its name. `locals` are the
    /// session's unexported variables, already validated as shell names.
    /// `allexport` runs the code after `set -a`.
    pub(crate) fn command_words<'a>(
        &self,
        kind: IslandKind,
        operands: &[String],
        locals: impl Iterator<Item = (&'a OsString, &'a OsString)>,
        allexport: bool,
    ) -> Result<Vec<String>, ShellError> {
        let utf8_path = |path: PathBuf| {
            path.to_str().map(str::to_owned).ok_or_else(|| {
                ShellError::new(ErrorCode::Io, "the temporary directory path is not UTF-8")
                    .with_help("Set TMPDIR to a UTF-8 path")
            })
        };
        let dump = utf8_path(self.dump_path())?;
        let baseline = utf8_path(self.baseline_path())?;
        let script = ISLAND_SCRIPT
            .replace("@VARIABLES@", VARIABLES_DUMP)
            .replace("@ALLEXPORT@", if allexport { "set -a; " } else { "" })
            .replace("@LOCALS@", &local_assignments(locals));
        let mut words = vec![
            POSIX_SHELL_PATH.to_owned(),
            "-c".to_owned(),
            script,
            "quirl".to_owned(),
            dump,
            baseline,
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

    /// Read what the island reported, or why it reported nothing complete,
    /// such as after being killed by a signal.
    pub(crate) fn outcome(&self) -> Result<Result<IslandOutcome, MissingState>, ShellError> {
        let baseline = match read_dump(&self.baseline_path(), MissingState::NoBaseline)? {
            Ok(baseline) => baseline,
            Err(missing) => return Ok(Err(missing)),
        };
        let final_dump = match read_dump(&self.dump_path(), MissingState::NoFinalDump)? {
            Ok(final_dump) => final_dump,
            Err(missing) => return Ok(Err(missing)),
        };
        Ok(parse_outcome(&final_dump, &baseline).ok_or(MissingState::Malformed))
    }
}

impl Drop for IslandState {
    fn drop(&mut self) {
        // Best effort: the directory is private to this user and process.
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Read one dump file within [`DUMP_BYTES_MAX`]. A file the shell never
/// wrote reports `absent`; a file above the bound reports
/// [`MissingState::Oversized`], so the command's status stands and the
/// session only warns, as for any other dump it cannot import.
fn read_dump(
    path: &Path,
    absent: MissingState,
) -> Result<Result<Vec<u8>, MissingState>, ShellError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Err(absent)),
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
        return Ok(Err(MissingState::Oversized));
    }
    Ok(Ok(bytes))
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
        // A reserved name would overwrite the wrapper's own dump paths.
        let reserved = name.starts_with(RESERVED_PREFIX);
        if !is_shell_name(name) || reserved || is_shell_special_variable(name) {
            continue;
        }
        rendered.push_str(name);
        rendered.push_str("='");
        rendered.push_str(&value.replace('\'', r"'\''"));
        rendered.push_str("'\n");
    }
    rendered
}

/// The NUL-terminated records of a dump, or `None` when the last record is
/// unterminated, as after a write cut short.
fn dump_records(bytes: &[u8]) -> Option<std::slice::Split<'_, u8, impl FnMut(&u8) -> bool>> {
    let body = bytes.strip_suffix(&[0])?;
    Some(body.split(|byte| *byte == 0))
}

/// Parse the final dump and the baseline. Any malformed record in either
/// rejects both, so a partial result never reaches the session.
///
/// The final dump is `HEADER PWD EXPORT... VARIABLES_HEADER VARIABLE...
/// TRAILER`; the baseline is `VARIABLES_HEADER VARIABLE... TRAILER`. Every
/// record ends in NUL.
fn parse_outcome(final_dump: &[u8], baseline: &[u8]) -> Option<IslandOutcome> {
    let mut records = dump_records(final_dump)?;
    if records.next()? != DUMP_HEADER {
        return None;
    }
    let directory = records.next().filter(|record| !record.is_empty())?;
    let directory = PathBuf::from(OsString::from_vec(directory.to_vec()));
    let mut exported = BTreeMap::new();
    loop {
        let record = records.next()?;
        if record == VARIABLES_HEADER {
            break;
        }
        let (name, value) = split_record(record)?;
        let maintained = SHELL_MAINTAINED_VARIABLES
            .iter()
            .any(|maintained| name == maintained.as_bytes());
        if maintained || name.starts_with(RESERVED_PREFIX.as_bytes()) {
            continue;
        }
        exported.insert(os_string(name), os_string(value));
    }
    let variables = parse_variables(&mut records)?;
    // A real shell always has variables, such as `PWD` and `IFS`. An empty
    // section means something other than the dump code answered, as when
    // code outside POSIX mode replaced `set` with a function, and importing
    // it would remove every session local.
    if variables.is_empty() {
        return None;
    }

    let mut baseline_records = dump_records(baseline)?;
    if baseline_records.next()? != VARIABLES_HEADER {
        return None;
    }
    let baseline = parse_variables(&mut baseline_records)?;
    Some(IslandOutcome {
        directory,
        exported,
        variables,
        baseline,
    })
}

/// Parse `NAME=value` records up to the trailer, which must be the last
/// record. A name that is not a shell name means the writer was not the dump
/// code, so it rejects the dump.
fn parse_variables<'a>(
    records: &mut impl Iterator<Item = &'a [u8]>,
) -> Option<BTreeMap<OsString, OsString>> {
    let mut variables = BTreeMap::new();
    loop {
        let record = records.next()?;
        if record == DUMP_TRAILER {
            return records.next().is_none().then_some(variables);
        }
        let (name, value) = split_record(record)?;
        let valid = std::str::from_utf8(name).is_ok_and(is_shell_name);
        if !valid {
            return None;
        }
        variables.insert(os_string(name), os_string(value));
    }
}

/// Split `NAME=value` at its first `=`. A record without one, or with an
/// empty name, is malformed.
fn split_record(record: &[u8]) -> Option<(&[u8], &[u8])> {
    let separator = record.iter().position(|byte| *byte == b'=')?;
    let (name, value) = record.split_at(separator);
    if name.is_empty() {
        return None;
    }
    Some((name, value.get(1..)?))
}

fn os_string(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

/// Whether `name` is shell bookkeeping or the wrapper's own scratch state,
/// which never becomes or removes a session local.
fn is_bookkeeping(name: &OsStr) -> bool {
    let bytes = name.as_bytes();
    bytes.starts_with(b"BASH_")
        || bytes.starts_with(RESERVED_PREFIX.as_bytes())
        || SHELL_BOOKKEEPING_VARIABLES
            .iter()
            .any(|bookkeeping| bytes == bookkeeping.as_bytes())
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

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn final_dump(exports: &str, variables: &str) -> Vec<u8> {
        format!(
            "quirl-island-v1\0/tmp\0{exports}quirl-island-variables\0{variables}quirl-island-end\0"
        )
        .into_bytes()
    }

    const EMPTY_BASELINE: &[u8] = b"quirl-island-variables\0quirl-island-end\0";

    #[test]
    fn dumps_parse_directory_exports_and_variables_without_bookkeeping() {
        let parsed = parse_outcome(
            &final_dump(
                "A=1\0SHLVL=2\0_=/usr/bin/env\0B=x=y\0__quirl_status=0\0",
                "A=1\0LOCAL=multi\nline\0",
            ),
            b"quirl-island-variables\0OLD=1\0quirl-island-end\0",
        )
        .unwrap();
        assert_eq!(parsed.directory, PathBuf::from("/tmp"));
        assert_eq!(parsed.exported, map(&[("A", "1"), ("B", "x=y")]));
        assert_eq!(
            parsed.variables,
            map(&[("A", "1"), ("LOCAL", "multi\nline")])
        );
        assert_eq!(parsed.baseline, map(&[("OLD", "1")]));
    }

    #[test]
    fn truncated_or_malformed_dumps_import_nothing() {
        let valid = final_dump("A=1\0", "B=2\0");
        assert!(parse_outcome(&valid, EMPTY_BASELINE).is_some());
        // Cut at every byte: only the complete dump parses.
        for length in 0..valid.len() {
            assert_eq!(parse_outcome(&valid[..length], EMPTY_BASELINE), None);
        }
        for length in 0..EMPTY_BASELINE.len() {
            assert_eq!(parse_outcome(&valid, &EMPTY_BASELINE[..length]), None);
        }
        // A missing header, as when an overridden `printf` prints nothing.
        assert_eq!(
            parse_outcome(
                b"A=1\0quirl-island-variables\0quirl-island-end\0",
                EMPTY_BASELINE
            ),
            None
        );
        assert_eq!(
            parse_outcome(&final_dump("NOEQUALS\0", ""), EMPTY_BASELINE),
            None
        );
        assert_eq!(parse_outcome(&final_dump("=1\0", ""), EMPTY_BASELINE), None);
        assert_eq!(
            parse_outcome(&final_dump("", "not-a-name=1\0"), EMPTY_BASELINE),
            None
        );
        assert_eq!(
            parse_outcome(&final_dump("", "B=2\0"), b"B=1\0quirl-island-end\0"),
            None
        );
        // A real shell always has variables; an empty section, as when code
        // outside POSIX mode replaced `set`, must not remove every local.
        assert_eq!(
            parse_outcome(&final_dump("A=1\0", ""), EMPTY_BASELINE),
            None
        );
        let mut trailing = valid.clone();
        trailing.extend_from_slice(b"C=3\0");
        assert_eq!(parse_outcome(&trailing, EMPTY_BASELINE), None);
        assert_eq!(
            parse_outcome(
                b"quirl-island-v1\0\0quirl-island-variables\0quirl-island-end\0",
                EMPTY_BASELINE
            ),
            None
        );
    }

    #[test]
    fn local_changes_follow_value_and_export_differences_from_the_baseline() {
        let outcome = IslandOutcome {
            directory: PathBuf::from("/"),
            exported: map(&[("EXPORTED", "1")]),
            variables: map(&[
                ("EXPORTED", "1"),
                ("NEW", "hello"),
                ("SAME", "same"),
                ("CHANGED", "after"),
                ("UNEXPORTED", "kept"),
                ("RANDOM", "42"),
                ("BASH_LINENO", "9"),
                ("BASH", "/bin/sh"),
                ("BASHOPTS", "x"),
                ("BASHPID", "9"),
                ("BASHFUL", "shy"),
                ("COLUMNS", "80"),
                ("LINES", "24"),
                ("IFS", " \t\n"),
                ("EMPTY", ""),
            ]),
            baseline: map(&[
                ("EXPORTED", "1"),
                ("SAME", "same"),
                ("CHANGED", "before"),
                ("UNEXPORTED", "kept"),
                ("RANDOM", "7"),
                ("GONE", "x"),
                ("PPID", "1"),
                ("IFS", " \t\n"),
            ]),
        };
        let changes = outcome.local_changes(
            &map(&[("UNEXPORTED", "kept"), ("EXPORTED", "1")]),
            &map(&[("GONE", "x"), ("SAME", "same"), ("NEVER_INJECTED", "x")]),
        );
        assert_eq!(
            changes.assigned,
            map(&[
                ("BASHFUL", "shy"),
                ("CHANGED", "after"),
                ("COLUMNS", "80"),
                ("EMPTY", ""),
                ("LINES", "24"),
                ("NEW", "hello"),
                ("UNEXPORTED", "kept"),
            ])
        );
        assert_eq!(changes.removed, BTreeSet::from([OsString::from("GONE")]));
    }

    #[test]
    fn locals_become_quoted_assignments_and_never_code() {
        let name = OsString::from("GREETING");
        let value = OsString::from("it's $(rm -rf /)");
        let invalid = OsString::from("not-a-name");
        let readonly = OsString::from("PPID");
        let shell_options = OsString::from("SHELLOPTS");
        let reserved = OsString::from("__quirl_baseline");
        let rendered = local_assignments(
            [
                (&name, &value),
                (&invalid, &value),
                (&readonly, &value),
                (&shell_options, &value),
                (&reserved, &value),
            ]
            .into_iter(),
        );
        assert_eq!(rendered, "GREETING='it'\\''s $(rm -rf /)'\n");
    }

    #[test]
    fn dump_code_is_spliceable_into_the_single_quoted_trap_and_uses_the_framing() {
        assert!(!VARIABLES_DUMP.contains('\''));
        for marker in [DUMP_HEADER, VARIABLES_HEADER, DUMP_TRAILER] {
            let marker = std::str::from_utf8(marker).unwrap();
            assert!(
                ISLAND_SCRIPT.contains(marker) || VARIABLES_DUMP.contains(marker),
                "{marker}"
            );
        }
    }

    fn run_island(
        kind: IslandKind,
        operands: &[String],
        locals: &BTreeMap<OsString, OsString>,
        allexport: bool,
    ) -> (IslandState, std::process::ExitStatus) {
        run_island_in(POSIX_SHELL_PATH, kind, operands, locals, allexport)
    }

    /// Run the island wrapper in `shell` instead of `/bin/sh`, so the dump
    /// code is checked against every reference shell on the machine. The
    /// child gets no inherited `LC_ALL`, as some CI runners export one, so
    /// tests control it through `locals` alone.
    fn run_island_in(
        shell: &str,
        kind: IslandKind,
        operands: &[String],
        locals: &BTreeMap<OsString, OsString>,
        allexport: bool,
    ) -> (IslandState, std::process::ExitStatus) {
        let state = IslandState::create().unwrap();
        let words = state
            .command_words(kind, operands, locals.iter(), allexport)
            .unwrap();
        let status = std::process::Command::new(shell)
            .args(&words[1..])
            .env_remove("LC_ALL")
            .status()
            .unwrap();
        (state, status)
    }

    #[test]
    fn every_reference_shell_reports_the_same_local_changes() {
        // Spoofed names inside values, hostile quoting, empty values, unset
        // locals, a changed IFS, and forms that must stay inside: a function,
        // a readonly attribute, and (in Bash only) an array. The multi-line
        // value also nominates the array with a scalar-looking line and marks
        // a real scalar with an array-looking one; neither may change the
        // result.
        let code = "QUIRL_MULTI='a\nQUIRL_FAKE=spoofed\nQUIRL_KEPT=spoofed\nQUIRL_ARRAY=spoofed\nQUIRL_EMPTY=(x'\n\
                    QUIRL_HOSTILE='$(id) `id` \\'\n\
                    QUIRL_EMPTY=\n\
                    unset QUIRL_GONE\n\
                    QUIRL_KEPT=kept\n\
                    readonly QUIRL_RO=ro\n\
                    f() { :; }\n\
                    IFS=:\n\
                    set -a\n\
                    QUIRL_EXPORTED=1\n\
                    eval 'QUIRL_ARRAY=(a b)' 2>/dev/null\n";
        let code = code.to_owned();
        let locals = map(&[("QUIRL_GONE", "x"), ("QUIRL_KEPT", "old"), ("LC_ALL", "C")]);
        let shells = ["/bin/sh", "/bin/dash", "/usr/bin/dash", "/bin/bash"];
        for shell in shells
            .into_iter()
            .filter(|shell| Path::new(shell).is_file())
        {
            let (state, _) = run_island_in(
                shell,
                IslandKind::Eval,
                std::slice::from_ref(&code),
                &locals,
                false,
            );
            let outcome = state.outcome().unwrap().unwrap();
            let changes = outcome.local_changes(&BTreeMap::new(), &locals);
            assert_eq!(
                changes.assigned,
                map(&[
                    ("IFS", ":"),
                    ("QUIRL_EMPTY", ""),
                    ("QUIRL_HOSTILE", "$(id) `id` \\"),
                    ("QUIRL_KEPT", "kept"),
                    (
                        "QUIRL_MULTI",
                        "a\nQUIRL_FAKE=spoofed\nQUIRL_KEPT=spoofed\nQUIRL_ARRAY=spoofed\nQUIRL_EMPTY=(x"
                    ),
                    ("QUIRL_RO", "ro"),
                ]),
                "{shell}"
            );
            assert_eq!(
                changes.removed,
                BTreeSet::from([OsString::from("QUIRL_GONE")]),
                "{shell}"
            );
            assert_eq!(
                outcome.exported.get(OsStr::new("QUIRL_EXPORTED")),
                Some(&OsString::from("1")),
                "{shell}"
            );
        }
    }

    #[test]
    fn dump_scratch_settings_never_leak_into_the_code_or_the_result() {
        // The dump changes IFS and LC_ALL for itself; the code must see the
        // session's values, and only the code's own changes come back.
        let locals = map(&[("IFS", ","), ("LC_ALL", "POSIX")]);
        let code = "[ \"$IFS|$LC_ALL\" = ',|POSIX' ] || exit 9; LC_ALL=C".to_owned();
        let (state, status) = run_island(IslandKind::Eval, &[code], &locals, false);
        assert!(status.success());
        let outcome = state.outcome().unwrap().unwrap();
        let changes = outcome.local_changes(&BTreeMap::new(), &locals);
        assert_eq!(changes.assigned, map(&[("LC_ALL", "C")]));

        let (state, status) =
            run_island(IslandKind::Eval, &[":".to_owned()], &BTreeMap::new(), false);
        assert!(status.success());
        let outcome = state.outcome().unwrap().unwrap();
        assert!(!outcome.exported.contains_key(OsStr::new("LC_ALL")));
        assert_eq!(
            outcome.local_changes(&BTreeMap::new(), &BTreeMap::new()),
            LocalChanges::default()
        );
    }

    #[test]
    fn exported_ifs_and_lc_all_keep_the_values_the_code_exported() {
        // Explicit export, allexport inside the code, and the session's
        // allexport flag all export the code's value, not the dump's.
        for (code, allexport) in [
            ("export IFS=: LC_ALL=POSIX", false),
            ("set -a; IFS=: LC_ALL=POSIX", false),
            ("IFS=: LC_ALL=POSIX", true),
        ] {
            let (state, status) = run_island(
                IslandKind::Eval,
                &[code.to_owned()],
                &BTreeMap::new(),
                allexport,
            );
            assert!(status.success(), "{code}");
            let outcome = state.outcome().unwrap().unwrap();
            assert_eq!(
                outcome.exported.get(OsStr::new("IFS")),
                Some(&OsString::from(":")),
                "{code}"
            );
            assert_eq!(
                outcome.exported.get(OsStr::new("LC_ALL")),
                Some(&OsString::from("POSIX")),
                "{code}"
            );
        }
    }

    #[test]
    fn options_and_ifs_left_by_the_code_do_not_stop_the_dump() {
        let code = "QUIRL_SET=1; set -euf; unset IFS; set -o noclobber".to_owned();
        let (state, status) = run_island(IslandKind::Eval, &[code], &BTreeMap::new(), false);
        assert!(status.success());
        let outcome = state.outcome().unwrap().unwrap();
        let changes = outcome.local_changes(&BTreeMap::new(), &BTreeMap::new());
        assert_eq!(changes.assigned, map(&[("QUIRL_SET", "1")]));
    }

    #[test]
    fn allexport_islands_export_the_code_assignments_but_not_injected_locals() {
        let locals = map(&[("QUIRL_INJECTED", "x")]);
        let (state, _) = run_island(
            IslandKind::Eval,
            &["QUIRL_AUTO=1".to_owned()],
            &locals,
            true,
        );
        let outcome = state.outcome().unwrap().unwrap();
        assert_eq!(
            outcome.exported.get(OsStr::new("QUIRL_AUTO")),
            Some(&OsString::from("1"))
        );
        assert!(!outcome.exported.contains_key(OsStr::new("QUIRL_INJECTED")));
        assert!(
            outcome
                .exported
                .keys()
                .all(|name| !name.as_bytes().starts_with(b"__quirl_"))
        );
        assert_eq!(
            outcome.local_changes(&BTreeMap::new(), &locals),
            LocalChanges::default()
        );
    }

    #[test]
    fn island_runs_source_and_reports_exports_directory_locals_and_status() {
        let scratch = IslandState::create().unwrap();
        let script = scratch.directory.join("setup.sh");
        fs::write(
            &script,
            "export ISLAND_VALUE=\"$GREETING-$1\"\nISLAND_LOCAL='a b'\ncd /\nexit 3\n",
        )
        .unwrap();
        let locals = map(&[("GREETING", "it's")]);
        let (state, status) = run_island(
            IslandKind::Source,
            &[script.display().to_string(), "arg".to_owned()],
            &locals,
            false,
        );
        assert_eq!(status.code(), Some(3));
        let outcome = state.outcome().unwrap().unwrap();
        assert_eq!(outcome.directory, PathBuf::from("/"));
        assert_eq!(
            outcome.exported.get(&OsString::from("ISLAND_VALUE")),
            Some(&OsString::from("it's-arg"))
        );
        assert!(!outcome.exported.contains_key(&OsString::from("GREETING")));
        let changes = outcome.local_changes(&BTreeMap::new(), &locals);
        assert_eq!(changes.assigned, map(&[("ISLAND_LOCAL", "a b")]));
        assert!(changes.removed.is_empty());
    }

    #[test]
    fn island_without_code_changes_reports_no_shell_bookkeeping() {
        let locals = map(&[("KEPT", "x"), ("IFS", ",")]);
        let (state, status) = run_island(
            IslandKind::Eval,
            &["cd /tmp; while false; do :; done; : \"$RANDOM\" \"$SECONDS\"; f() { :; }; f; set -a; set +a".to_owned()],
            &locals,
            false,
        );
        assert!(status.success());
        let outcome = state.outcome().unwrap().unwrap();
        assert_eq!(
            outcome.local_changes(&BTreeMap::new(), &locals),
            LocalChanges::default()
        );
    }

    #[test]
    fn overridden_commands_and_aliases_cannot_rewrite_the_dump() {
        // Each override would empty, spoof, or freeze the dump if the trap
        // used it: a silent or forwarding `printf`, an `eval` alias that
        // leaves the previous value in place, and a shadowed `declare`.
        let forwarding = "printf() { if [ \"$2\" = QUIRL_SECRET ]; then \
                          command printf \"$1\" \"$2\" spoofed; else command printf \"$@\"; fi; }";
        let locals = map(&[("QUIRL_KEPT", "keep")]);
        for override_code in [
            "printf() { :; }",
            "printf() { echo spoof; }",
            forwarding,
            "alias eval=: printf=: set=:",
            "declare() { :; }; builtin() { :; }",
        ] {
            let code = format!("QUIRL_SECRET=real\n{override_code}\nQUIRL_NEW=1");
            let (state, status) = run_island(IslandKind::Eval, &[code], &locals, false);
            assert!(status.success(), "{override_code}");
            let outcome = state.outcome().unwrap().unwrap();
            let changes = outcome.local_changes(&BTreeMap::new(), &locals);
            assert_eq!(
                changes.assigned,
                map(&[("QUIRL_NEW", "1"), ("QUIRL_SECRET", "real")]),
                "{override_code}"
            );
        }
    }

    #[test]
    fn tracing_left_on_by_the_code_never_prints_variables() {
        let locals = map(&[("QUIRL_SESSION_SECRET", "hunter2")]);
        let state = IslandState::create().unwrap();
        let code = "QUIRL_CODE_SECRET=swordfish; set -xv".to_owned();
        let words = state
            .command_words(IslandKind::Eval, &[code], locals.iter(), false)
            .unwrap();
        let output = std::process::Command::new(POSIX_SHELL_PATH)
            .args(&words[1..])
            .env_remove("LC_ALL")
            .output()
            .unwrap();
        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("hunter2"), "{stderr}");
        assert!(!stderr.contains("swordfish"), "{stderr}");
        assert!(state.outcome().unwrap().is_ok());
    }

    #[test]
    fn reserved_and_shell_readonly_locals_leave_the_wrapper_working() {
        let locals = map(&[
            ("__quirl_dump", "/nonexistent/dump"),
            ("__quirl_baseline", "/nonexistent/baseline"),
            ("__quirl_kind", "x"),
            ("SHELLOPTS", "x"),
            ("BASHOPTS", "x"),
            ("BASH_VERSINFO", "x"),
            ("UID", "0"),
            // Bash 3.2 exits with status 127 and no message on these.
            ("GROUPS", "x"),
            ("FUNCNAME", "x"),
            ("BASH_ARGC", "x"),
            ("BASH_ARGV", "x"),
            ("BASH_LINENO", "x"),
            ("BASH_SOURCE", "x"),
        ]);
        let (state, status) = run_island(
            IslandKind::Eval,
            &["QUIRL_RAN=1".to_owned()],
            &locals,
            false,
        );
        assert!(status.success());
        let outcome = state.outcome().unwrap().unwrap();
        assert_eq!(
            outcome.local_changes(&BTreeMap::new(), &locals).assigned,
            map(&[("QUIRL_RAN", "1")])
        );
    }

    #[test]
    fn a_killed_or_broken_shell_reports_why_nothing_came_back() {
        for (code, missing) in [
            ("kill -9 $$", MissingState::NoFinalDump),
            ("trap : EXIT", MissingState::NoFinalDump),
            ("readonly IFS", MissingState::Malformed),
            ("unset PWD", MissingState::Malformed),
        ] {
            let (state, _) = run_island(
                IslandKind::Eval,
                &[code.to_owned()],
                &BTreeMap::new(),
                false,
            );
            assert_eq!(state.outcome().unwrap(), Err(missing), "{code}");
        }
    }

    #[test]
    fn a_shell_that_replaces_set_outside_posix_mode_reports_a_malformed_dump() {
        // Outside POSIX mode Bash lets a function shadow `set`, and a
        // read-only function survives the trap's scrub. The empty variable
        // section must not read as "the code unset every local".
        let code = "set +o posix 2>/dev/null; set() { :; }; readonly -f set 2>/dev/null";
        let locals = map(&[("KEEP", "1")]);
        let (state, _) = run_island(IslandKind::Eval, &[code.to_owned()], &locals, false);
        match state.outcome().unwrap() {
            Ok(outcome) => assert!(
                outcome
                    .local_changes(&BTreeMap::new(), &locals)
                    .removed
                    .is_empty(),
                "a shell that kept POSIX `set` must report its locals"
            ),
            Err(missing) => assert_eq!(missing, MissingState::Malformed),
        }
    }

    #[test]
    fn an_oversized_dump_reports_missing_state_instead_of_failing() {
        let state = IslandState::create().unwrap();
        fs::write(state.baseline_path(), EMPTY_BASELINE).unwrap();
        let file = File::create(state.dump_path()).unwrap();
        let oversized = u64::try_from(DUMP_BYTES_MAX + 1).unwrap();
        file.set_len(oversized).unwrap();
        assert_eq!(state.outcome().unwrap(), Err(MissingState::Oversized));
        fs::write(state.baseline_path(), vec![b'x'; DUMP_BYTES_MAX + 1]).unwrap();
        assert_eq!(state.outcome().unwrap(), Err(MissingState::Oversized));
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
            .command_words(IslandKind::Source, &[], std::iter::empty(), false)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
}
