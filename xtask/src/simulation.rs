//! Reproducible black-box compatibility sessions.
//!
//! Failure model and invariants:
//! - Generated source, output, files, depth, session count, steps, and wall time
//!   are bounded before their evidence is retained.
//! - Each runner receives an identical fresh filesystem and explicit minimal
//!   environment, and runs in its own process group so timeout cleanup cannot
//!   leave a generated child behind.
//! - Interpreter probes and generated sessions share one RAII-owned temporary
//!   root outside the workspace, which is removed on success and every error.
//! - The Quirl executable is admitted through one regular-file handle and copied
//!   once into that private root before any session. Copying retains a 64 KiB
//!   buffer and scans at most 256 MiB plus one byte; the exact copied bytes are
//!   SHA-256 hashed. Concurrent path replacement cannot switch later sessions,
//!   and observed in-place source changes reject the snapshot before execution.
//!   Partial snapshots remain under the root's RAII cleanup. Only a complete,
//!   synchronized snapshot receives owner-only read/execute permissions.
//! - Bash and Zsh must agree before their result can be treated as an oracle.
//! - `summary.json` is installed atomically and last; its presence means every
//!   trace and mismatch artifact needed by the scheduled reporter is durable.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::TaskError;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

const REPORT_SCHEMA_VERSION: u32 = 1;
const SUMMARY_SCHEMA_VERSION: u32 = 2;
const EXECUTABLE_BYTES_MAX: u64 = 256 * 1024 * 1024;
const EXECUTABLE_COPY_BUFFER_BYTES: usize = 64 * 1024;
const SESSION_SOURCE_BYTES_MAX: usize = 64 * 1024;
const SESSION_OUTPUT_BYTES_MAX: usize = 64 * 1024;
const SESSION_FILES_MAX: usize = 64;
const SESSION_FILE_BYTES_MAX: usize = 64 * 1024;
const SESSION_PATH_DEPTH_MAX: usize = 8;
const SESSION_DEADLINE: Duration = Duration::from_secs(5);

pub struct SimulationOptions {
    pub quirl: PathBuf,
    pub seed: u64,
    pub session_count: usize,
    pub steps_max: usize,
    pub only_session: Option<usize>,
    pub output_root: PathBuf,
}

pub struct SimulationResult {
    pub seed: u64,
    pub sessions_evaluated: usize,
    pub mismatch_count: usize,
    pub run_directory: PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct InterpreterIdentity {
    executable: String,
    version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ObservableOutcome {
    status: i32,
    stdout: String,
    stderr: String,
}

#[derive(Debug, Clone, Serialize)]
struct RunnerCapture {
    outcome: Option<ObservableOutcome>,
    process_status: i32,
    timed_out: bool,
    raw_stdout: String,
    raw_stderr: String,
    diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FileRecord {
    path: String,
    contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct FilesystemManifest {
    files: Vec<FileRecord>,
    total_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Classification {
    Match,
    NativeMismatch,
    ReferenceDivergence,
}

#[derive(Debug, Serialize)]
struct RunnerEvidence {
    capture: RunnerCapture,
    filesystem: FilesystemManifest,
}

#[derive(Debug, Serialize)]
struct SessionRecord {
    schema_version: u32,
    seed: u64,
    session: usize,
    steps: Vec<String>,
    source: String,
    classification: Classification,
    quirl: RunnerEvidence,
    bash: RunnerEvidence,
    zsh: RunnerEvidence,
}

#[derive(Debug, Serialize)]
struct PersistedSummary<'a> {
    schema_version: u32,
    result: &'a str,
    seed: u64,
    sessions_generated: usize,
    sessions_evaluated: usize,
    steps_max: usize,
    only_session: Option<usize>,
    mismatch_count: usize,
    native_mismatch_count: usize,
    reference_divergence_count: usize,
    first_mismatch_session: Option<usize>,
    quirl_executable: String,
    quirl_snapshot_sha256: String,
    quirl_snapshot_bytes: u64,
    bash: &'a InterpreterIdentity,
    zsh: &'a InterpreterIdentity,
    report: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuirlScriptOutcome {
    status: i32,
    stdout: String,
    stderr: String,
}

#[derive(Debug)]
struct GeneratedSession {
    steps: Vec<String>,
    source: String,
}

struct EvaluationContext<'a> {
    options: &'a SimulationOptions,
    temporary_root: &'a Path,
    quirl: &'a Path,
    bash: &'a str,
    zsh: &'a str,
    search_path: &'a OsStr,
}

struct DeterministicRng(u64);

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "the deterministic generator uses specified wrapping arithmetic"
    )]
    fn bounded(&mut self, upper_exclusive: usize) -> usize {
        debug_assert!(upper_exclusive > 0);
        let upper = u64::try_from(upper_exclusive).unwrap_or(u64::MAX);
        usize::try_from(self.next() % upper).unwrap_or(0)
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "generated word lengths are bounded to a tiny fixed range"
    )]
    fn word(&mut self) -> String {
        let length = 1 + self.bounded(12);
        (0..length)
            .map(|_| {
                let offset = u8::try_from(self.bounded(26)).unwrap_or(0);
                char::from(b'a'.saturating_add(offset))
            })
            .collect()
    }
}

struct TemporaryRoot {
    path: PathBuf,
}

impl TemporaryRoot {
    fn create(seed: u64) -> io::Result<Self> {
        let parent = env::temp_dir();
        Self::create_in(&parent, seed)
    }

    fn create_in(parent: &Path, seed: u64) -> io::Result<Self> {
        for attempt in 0..64_u8 {
            let path = parent.join(format!(
                "quirl-simulation-{}-{seed}-{attempt}",
                std::process::id()
            ));
            let mut builder = fs::DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique simulation directory",
        ))
    }
}

impl Drop for TemporaryRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct ExecutableSnapshot {
    source: PathBuf,
    path: PathBuf,
    sha256: String,
    byte_size: u64,
}

/// An immutable executable copy and the private temporary directory that owns it.
///
/// Retain this guard until all children using [`Self::path`] have been reaped.
/// Dropping it removes the snapshot and its directory on a best-effort basis.
/// The original executable may be replaced after creation without changing the
/// copied bytes or their recorded identity.
pub(crate) struct PinnedExecutable {
    snapshot: ExecutableSnapshot,
    _temporary_root: TemporaryRoot,
}

impl PinnedExecutable {
    /// Copy a regular executable into a newly reserved private directory.
    ///
    /// Admission scans at most 256 MiB plus one byte using a 64 KiB buffer and
    /// hashes the copied bytes with SHA-256. On Unix, nonblocking, no-follow
    /// admission rejects special files; the complete copy has mode 0500 and its
    /// directory has mode 0700. Observed source mutation or any I/O failure
    /// rejects the copy, and partial initialization drops the temporary root.
    /// Directory reservation makes at most 64 attempts; `seed` distinguishes
    /// deterministic runs and never controls executable contents.
    pub(crate) fn create(source: &Path, seed: u64) -> io::Result<Self> {
        let temporary_root = TemporaryRoot::create(seed)?;
        let snapshot = snapshot_executable(source, &temporary_root.path, EXECUTABLE_BYTES_MAX)?;
        Ok(Self {
            snapshot,
            _temporary_root: temporary_root,
        })
    }

    /// Return the copied executable path used for every child in this run.
    pub(crate) fn path(&self) -> &Path {
        &self.snapshot.path
    }

    /// Return the canonical original path for provenance, never for execution.
    pub(crate) fn source(&self) -> &Path {
        &self.snapshot.source
    }

    /// Return the lowercase SHA-256 digest of the exact copied executable bytes.
    pub(crate) fn sha256(&self) -> &str {
        &self.snapshot.sha256
    }

    /// Return the number of copied executable bytes, bounded by 256 MiB.
    pub(crate) fn byte_size(&self) -> u64 {
        self.snapshot.byte_size
    }
}

fn snapshot_executable(
    source: &Path,
    root: &Path,
    bytes_max: u64,
) -> io::Result<ExecutableSnapshot> {
    let source = source.canonicalize()?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    let mut original = options.open(&source)?;
    let before = original.metadata()?;
    if !before.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "simulation executable is not a regular file",
        ));
    }
    if before.len() > bytes_max {
        return Err(executable_size_error(bytes_max, before.len()));
    }
    let path = root.join(if cfg!(windows) {
        "quirl-snapshot.exe"
    } else {
        "quirl-snapshot"
    });
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut snapshot = options.open(&path)?;
    let (sha256, byte_size) = copy_executable_bytes(&mut original, &mut snapshot, bytes_max)?;
    let after = original.metadata()?;
    if byte_size != before.len() || executable_source_changed(&before, &after) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "simulation executable changed while being copied; finish the build and retry",
        ));
    }
    snapshot.sync_all()?;
    #[cfg(unix)]
    snapshot.set_permissions(fs::Permissions::from_mode(0o500))?;
    snapshot.sync_all()?;
    Ok(ExecutableSnapshot {
        source,
        path,
        sha256,
        byte_size,
    })
}

fn copy_executable_bytes(
    source: &mut File,
    output: &mut File,
    bytes_max: u64,
) -> io::Result<(String, u64)> {
    let mut source = source.take(bytes_max.saturating_add(1));
    let mut buffer = vec![0_u8; EXECUTABLE_COPY_BUFFER_BYTES];
    let mut digest = Sha256::new();
    let mut byte_size = 0_u64;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        byte_size = byte_size.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if byte_size > bytes_max {
            return Err(executable_size_error(bytes_max, byte_size));
        }
        let chunk = buffer.get(..count).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "executable read exceeded its copy buffer",
            )
        })?;
        output.write_all(chunk)?;
        digest.update(chunk);
    }
    Ok((format!("{:x}", digest.finalize()), byte_size))
}

fn executable_source_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return true;
    }
    #[cfg(unix)]
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return true;
    }
    false
}

fn executable_size_error(limit: u64, observed: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("simulation executable exceeds its byte limit: limit {limit}; observed {observed}"),
    )
}

#[derive(Debug)]
struct BoundedOutput {
    output: Output,
    timed_out: bool,
}

#[derive(Debug)]
enum CaptureEvent {
    OutputLimit {
        stream: &'static str,
        observed_bytes: usize,
    },
    ReadFailed {
        stream: &'static str,
        error: io::Error,
    },
}

pub fn run(options: SimulationOptions) -> Result<SimulationResult, TaskError> {
    validate_options(&options)?;
    let search_path = env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not configured"))?;
    // Identity probes use the same isolated root as generated sessions. Passing
    // the repository here would let configure_command create a repo-root tmp/.
    let temporary = TemporaryRoot::create(options.seed)?;
    let bash = interpreter_identity("bash", &search_path, &temporary.path)?;
    let zsh = interpreter_identity("zsh", &search_path, &temporary.path)?;
    let quirl = snapshot_executable(&options.quirl, &temporary.path, EXECUTABLE_BYTES_MAX)?;
    let run_directory = create_run_directory(&options)?;
    let report_path = run_directory.join("report.jsonl");
    let report_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)?;
    let mut report = BufWriter::new(report_file);
    let mut generator = DeterministicRng::new(options.seed);
    let mut counts = SimulationCounts::default();
    let context = EvaluationContext {
        options: &options,
        temporary_root: &temporary.path,
        quirl: &quirl.path,
        bash: &bash.executable,
        zsh: &zsh.executable,
        search_path: &search_path,
    };

    for session_index in 0..options.session_count {
        let generated = generate_session(&mut generator, options.steps_max)?;
        if options
            .only_session
            .is_some_and(|selected| selected != session_index)
        {
            continue;
        }
        let record = evaluate_session(&context, session_index, generated)?;
        counts.observe(record.classification, session_index);
        serde_json::to_writer(&mut report, &record)?;
        report.write_all(b"\n")?;
        if record.classification != Classification::Match {
            write_failure(&run_directory, &record)?;
        }
    }
    report.flush()?;
    report.get_ref().sync_all()?;
    write_summary(&options, &run_directory, &quirl, &bash, &zsh, &counts)?;

    Ok(SimulationResult {
        seed: options.seed,
        sessions_evaluated: counts.evaluated,
        mismatch_count: counts.mismatches,
        run_directory,
    })
}

#[derive(Default)]
struct SimulationCounts {
    evaluated: usize,
    mismatches: usize,
    native_mismatches: usize,
    reference_divergences: usize,
    first_mismatch: Option<usize>,
}

impl SimulationCounts {
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "simulation counters are bounded by the configured case count"
    )]
    fn observe(&mut self, classification: Classification, session: usize) {
        self.evaluated += 1;
        match classification {
            Classification::Match => {}
            Classification::NativeMismatch => {
                self.native_mismatches += 1;
                self.mismatches += 1;
            }
            Classification::ReferenceDivergence => {
                self.reference_divergences += 1;
                self.mismatches += 1;
            }
        }
        if classification != Classification::Match && self.first_mismatch.is_none() {
            self.first_mismatch = Some(session);
        }
    }
}

fn validate_options(options: &SimulationOptions) -> io::Result<()> {
    if !(1..=10_000).contains(&options.session_count) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "simulation sessions must be between 1 and 10000",
        ));
    }
    if !(3..=32).contains(&options.steps_max) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "simulation steps must be between 3 and 32",
        ));
    }
    if options
        .only_session
        .is_some_and(|session| session >= options.session_count)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "selected session is outside the generated swarm",
        ));
    }
    Ok(())
}

fn create_run_directory(options: &SimulationOptions) -> io::Result<PathBuf> {
    fs::create_dir_all(&options.output_root)?;
    let suffix = options
        .only_session
        .map_or_else(String::new, |session| format!("-session-{session}"));
    let run_directory = options
        .output_root
        .join(format!("seed-{}{suffix}", options.seed));
    fs::create_dir(&run_directory).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                error.kind(),
                format!(
                    "simulation output {} already exists; preserve it and choose another --output directory",
                    run_directory.display()
                ),
            )
        } else {
            error
        }
    })?;
    sync_directory(&options.output_root)?;
    Ok(run_directory)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "session and command counts are bounded by validated simulation limits"
)]
fn generate_session(
    generator: &mut DeterministicRng,
    steps_max: usize,
) -> io::Result<GeneratedSession> {
    let step_count = 3 + generator.bounded(steps_max - 2);
    let token = generator.word();
    let mut steps = Vec::with_capacity(step_count);
    steps.push("mkdir workspace && cd workspace && printf 'base' > state".to_owned());
    steps.push(format!("export QUIRL_SIM_VALUE={token}"));
    for _ in 2..step_count - 1 {
        steps.push(generate_step(generator));
    }
    steps.push("printf '<state='; cat state; printf '><token=%s>' $QUIRL_SIM_VALUE".to_owned());
    let source = steps.join("; ");
    if source.len() > SESSION_SOURCE_BYTES_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "generated source exceeded {SESSION_SOURCE_BYTES_MAX} bytes: {}",
                source.len()
            ),
        ));
    }
    Ok(GeneratedSession { steps, source })
}

// Every branch must be a construct bash and zsh are guaranteed to evaluate
// identically (POSIX-portable syntax, or common utilities invoked with
// arguments that never depend on shell-specific expansion rules). `classify`
// treats a bash/zsh disagreement as a `ReferenceDivergence`, which the nightly
// workflow reports exactly like a genuine Quirl bug, so a branch that can
// legitimately diverge between the two reference shells (unmatched globs,
// bash-only arrays, `[[ ]]`, brace expansion, `$RANDOM`, locale-sensitive
// case conversion, and similar) does not belong here even though Quirl would
// likely still agree with at least one of them.
fn generate_step(generator: &mut DeterministicRng) -> String {
    let left = generator.word();
    let right = generator.word();
    match generator.bounded(42) {
        0 => format!("printf '%s:%s' '{left}' '{right}'"),
        1 => format!("printf '%s' '{left}' | tr a-z A-Z"),
        2 => format!("true && printf '%s' '{left}' || printf '%s' '{right}'"),
        3 => format!("false && printf '%s' '{left}' || printf '%s' '{right}'"),
        4 => {
            let first = generator.bounded(100);
            let second = generator.bounded(100);
            format!("printf '%s' $(({first} + {second}))")
        }
        5 => format!("printf '[%s]' $(printf '%s' '{left}')"),
        6 => format!("cat <<< '{left}'"),
        7 => format!("printf '%s' '{left}' >> state"),
        8 => format!("printf '%s' '{left}' > scratch && cat scratch"),
        9 => "cat ../input.txt".to_owned(),
        10 => "sh -c 'printf warning >&2; exit 7' || printf recovered".to_owned(),
        11 => "false; printf '%s' $?".to_owned(),
        12 => "printf '[%s]' $QUIRL_SIM_VALUE".to_owned(),
        13 => format!("printf '%s\\n' '{left}' | wc -c"),
        14 => format!("printf '%s %s\\n' '{left}' '{right}' | cut -d' ' -f1"),
        15 => format!("printf '%s\\n%s\\n' '{right}' '{left}' | sort"),
        16 => format!("printf '%s\\n%s\\n%s\\n' '{left}' '{right}' '{left}' | sort -u"),
        17 => format!("printf '%s' '{left}' | sed 's/a/X/g'"),
        18 => format!("printf '%s' '{left}' | grep -c '.'"),
        19 => format!("basename '/tmp/{left}/{right}'"),
        20 => format!("dirname '/tmp/{left}/{right}'"),
        21 => {
            let first = generator.bounded(100);
            let second = generator.bounded(100);
            format!("expr {first} + {second}")
        }
        22 => "seq 1 5 | tail -n 1".to_owned(),
        23 => format!("printf '%s\\n%s\\n' '{left}' '{right}' | grep -c '.'"),
        24 => format!("printf '%s' '{left}' | tr '[:lower:]' '[:upper:]'"),
        25 => format!("printf '%s-%s-%s' '{left}' '{right}' '{left}' | cut -d'-' -f2"),
        26 => format!("printf '%s' '{left}' | wc -l"),
        // `if`/`for`/`while`/`case`/function-definition keywords are C2
        // dialect control forms that the native `command { ... }` block
        // rejects outright (`bash { ... }`/`zsh { ... }` own that grammar
        // instead), so equivalent behavior here stays within `&&`/`||`.
        27 => format!("[ '{left}' = '{right}' ] && printf 'same' || printf 'diff'"),
        28 => {
            format!("printf '%s' '{left}' > out1 && printf '%s' '{right}' > out2 && cat out1 out2")
        }
        29 => format!("printf '%s' '{left}' | tr -d 'aeiou'"),
        30 => format!("printf '%s\\n%s\\n' '{left}' '{left}' | uniq | wc -l"),
        31 => format!("mkdir -p sub && printf '%s' '{left}' > sub/file.txt && cat sub/file.txt"),
        32 => format!("printf '' > '{left}.marker' && ls -- *.marker | sort | wc -l"),
        33 => format!("printf '%s' '{left}' | awk '{{ print length($0) }}'"),
        34 => format!("printf '%s' '{left}{right}' | head -c 3"),
        35 => {
            // Native arithmetic expansion supports only `+ - * /` and
            // parentheses on integer literals; `%` is rejected outright.
            let first = generator.bounded(100);
            let second = generator.bounded(100);
            format!("printf '%s' $(({first} - {second}))")
        }
        // The `:` no-op builtin is not implemented natively; the C1 executor
        // tries (and fails) to exec it as an external program instead.
        36 => format!("true; printf '%s' '{left}'"),
        // Native descriptor duplication supports only `2>&1`, not `1>&2`.
        37 => format!("printf '%s' '{left}' > out3 2>&1 && cat out3"),
        // Parameter expansion needs `export NAME=value`, not a bare `NAME=value`
        // statement: the C1 executor does not recognize a lone assignment and
        // tries to exec it as an external program instead.
        38 => format!("export x='{left}{right}'; printf '%s' \"${{x%?}}\""),
        39 => format!("export x='{left}{right}'; printf '%s' \"${{x#?}}\""),
        40 => format!("printf '%s' \"${{QUIRL_SIM_NEVER_SET:-{left}}}\""),
        41 => "printf '%s' \"${#QUIRL_SIM_VALUE}\"".to_owned(),
        _ => format!("printf '%s' '{right}' | cat"),
    }
}

fn evaluate_session(
    context: &EvaluationContext<'_>,
    session_index: usize,
    generated: GeneratedSession,
) -> Result<SessionRecord, TaskError> {
    let session_root = context
        .temporary_root
        .join(format!("session-{session_index:05}"));
    fs::create_dir(&session_root)?;
    let quirl_root = prepare_runner_root(&session_root, "quirl")?;
    let bash_root = prepare_runner_root(&session_root, "bash")?;
    let zsh_root = prepare_runner_root(&session_root, "zsh")?;
    let script_path = session_root.join("session.qrl");
    fs::write(
        &script_path,
        format!("command {{\n{}\n}}\n", generated.source),
    )?;

    let quirl_capture = run_quirl(
        context.quirl,
        &script_path,
        &quirl_root,
        context.search_path,
    )?;
    let bash_capture = run_reference(
        context.bash,
        &generated.source,
        &bash_root,
        context.search_path,
        true,
    )?;
    let zsh_capture = run_reference(
        context.zsh,
        &generated.source,
        &zsh_root,
        context.search_path,
        false,
    )?;
    let quirl_filesystem = filesystem_manifest(&quirl_root)?;
    let bash_filesystem = filesystem_manifest(&bash_root)?;
    let zsh_filesystem = filesystem_manifest(&zsh_root)?;
    let classification = classify(
        &quirl_capture,
        &bash_capture,
        &zsh_capture,
        &quirl_filesystem,
        &bash_filesystem,
        &zsh_filesystem,
    );
    let record = SessionRecord {
        schema_version: REPORT_SCHEMA_VERSION,
        seed: context.options.seed,
        session: session_index,
        steps: generated.steps,
        source: generated.source,
        classification,
        quirl: RunnerEvidence {
            capture: quirl_capture,
            filesystem: quirl_filesystem,
        },
        bash: RunnerEvidence {
            capture: bash_capture,
            filesystem: bash_filesystem,
        },
        zsh: RunnerEvidence {
            capture: zsh_capture,
            filesystem: zsh_filesystem,
        },
    };
    fs::remove_dir_all(&session_root)?;
    Ok(record)
}

fn prepare_runner_root(session_root: &Path, name: &str) -> io::Result<PathBuf> {
    let root = session_root.join(name);
    fs::create_dir(&root)?;
    fs::create_dir(root.join("tmp"))?;
    fs::write(root.join("input.txt"), "fixture\n")?;
    Ok(root)
}

fn run_quirl(
    executable: &Path,
    script: &Path,
    working_directory: &Path,
    search_path: &OsStr,
) -> Result<RunnerCapture, TaskError> {
    let mut command = Command::new(executable);
    command.args([
        OsStr::new("run"),
        script.as_os_str(),
        OsStr::new("--lang"),
        OsStr::new("quirl"),
    ]);
    configure_command(&mut command, working_directory, search_path)?;
    let bounded = bounded_output(&mut command, SESSION_DEADLINE)?;
    let raw_stdout = String::from_utf8_lossy(&bounded.output.stdout).into_owned();
    let raw_stderr = String::from_utf8_lossy(&bounded.output.stderr).into_owned();
    let parsed = serde_json::from_slice::<QuirlScriptOutcome>(&bounded.output.stdout);
    let (outcome, diagnostic) = match parsed {
        Ok(value) if !bounded.timed_out => (
            Some(ObservableOutcome {
                status: value.status,
                stdout: value.stdout,
                stderr: value.stderr,
            }),
            None,
        ),
        Ok(_) => (None, Some("Quirl session exceeded its deadline".to_owned())),
        Err(error) => (
            None,
            Some(format!(
                "Quirl did not emit a structured session outcome: {error}"
            )),
        ),
    };
    Ok(RunnerCapture {
        outcome,
        process_status: exit_status_code(bounded.output.status),
        timed_out: bounded.timed_out,
        raw_stdout,
        raw_stderr,
        diagnostic,
    })
}

fn run_reference(
    executable: &str,
    source: &str,
    working_directory: &Path,
    search_path: &OsStr,
    is_bash: bool,
) -> Result<RunnerCapture, TaskError> {
    let mut command = Command::new(executable);
    if is_bash {
        command.args(["--noprofile", "--norc", "-c", source]);
    } else {
        command.args(["-f", "-c", source]);
    }
    configure_command(&mut command, working_directory, search_path)?;
    let bounded = bounded_output(&mut command, SESSION_DEADLINE)?;
    let status = exit_status_code(bounded.output.status);
    let stdout = String::from_utf8_lossy(&bounded.output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&bounded.output.stderr).into_owned();
    let outcome = (!bounded.timed_out).then(|| ObservableOutcome {
        status,
        stdout: stdout.clone(),
        stderr: stderr.clone(),
    });
    Ok(RunnerCapture {
        outcome,
        process_status: status,
        timed_out: bounded.timed_out,
        raw_stdout: stdout,
        raw_stderr: stderr,
        diagnostic: bounded
            .timed_out
            .then(|| "reference session exceeded its deadline".to_owned()),
    })
}

fn configure_command(
    command: &mut Command,
    working_directory: &Path,
    search_path: &OsStr,
) -> io::Result<()> {
    let temporary = working_directory.join("tmp");
    fs::create_dir_all(&temporary)?;
    command
        .current_dir(working_directory)
        .env_clear()
        .env("PATH", search_path)
        .env("HOME", working_directory)
        .env("TMPDIR", temporary)
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env("TERM", "dumb")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    Ok(())
}

fn bounded_output(command: &mut Command, deadline: Duration) -> io::Result<BoundedOutput> {
    let mut child = command.spawn()?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            cleanup_spawned_child(&mut child);
            return Err(io::Error::other(
                "bounded command did not expose its configured stdout pipe",
            ));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            drop(stdout);
            cleanup_spawned_child(&mut child);
            return Err(io::Error::other(
                "bounded command did not expose its configured stderr pipe",
            ));
        }
    };
    let (events, notifications) = mpsc::channel();
    let stdout_reader = spawn_bounded_reader(stdout, "stdout", events.clone());
    let stderr_reader = spawn_bounded_reader(stderr, "stderr", events);
    let started = Instant::now();
    let mut timed_out = false;
    let mut capture_error = None;
    let mut control_error = None;
    let status = loop {
        match notifications.try_recv() {
            Ok(event) => {
                capture_error = Some(capture_event_error(event));
                break None;
            }
            Err(mpsc::TryRecvError::Disconnected | mpsc::TryRecvError::Empty) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(error) => {
                control_error = Some(error);
                break None;
            }
        }
        if started.elapsed() >= deadline {
            timed_out = true;
            break None;
        }
        thread::sleep(Duration::from_millis(2));
    };

    // The group leader may exit while a generated descendant still owns a
    // pipe writer. Always terminate the group before joining the readers so a
    // descendant cannot turn a completed command into an unbounded wait.
    let termination = terminate_process_group(&mut child);
    let status = match status {
        Some(status) => Ok(status),
        None => child.wait(),
    };
    let stdout = join_bounded_reader(stdout_reader, "stdout");
    let stderr = join_bounded_reader(stderr_reader, "stderr");
    if let Some(error) = capture_error {
        return Err(error);
    }
    if let Some(error) = control_error {
        return Err(error);
    }
    let stdout = stdout?;
    let stderr = stderr?;
    termination?;
    let status = status?;
    Ok(BoundedOutput {
        output: Output {
            status,
            stdout,
            stderr,
        },
        timed_out,
    })
}

fn cleanup_spawned_child(child: &mut Child) {
    let _ = terminate_process_group(child);
    let _ = child.wait();
}

fn spawn_bounded_reader<R>(
    reader: R,
    stream: &'static str,
    events: mpsc::Sender<CaptureEvent>,
) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let retained_bytes_max = SESSION_OUTPUT_BYTES_MAX.saturating_add(1);
        let retained_bytes_max = u64::try_from(retained_bytes_max).unwrap_or(u64::MAX);
        let mut bytes = Vec::new();
        let result = reader.take(retained_bytes_max).read_to_end(&mut bytes);
        match result {
            Ok(_) if bytes.len() > SESSION_OUTPUT_BYTES_MAX => {
                let _ = events.send(CaptureEvent::OutputLimit {
                    stream,
                    observed_bytes: bytes.len(),
                });
                Err(output_limit_error(stream, bytes.len()))
            }
            Ok(_) => Ok(bytes),
            Err(error) => {
                let event_error = io::Error::new(error.kind(), error.to_string());
                let _ = events.send(CaptureEvent::ReadFailed {
                    stream,
                    error: event_error,
                });
                Err(error)
            }
        }
    })
}

fn join_bounded_reader(
    reader: JoinHandle<io::Result<Vec<u8>>>,
    stream: &'static str,
) -> io::Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| io::Error::other(format!("session {stream} reader panicked")))?
}

fn capture_event_error(event: CaptureEvent) -> io::Error {
    match event {
        CaptureEvent::OutputLimit {
            stream,
            observed_bytes,
        } => output_limit_error(stream, observed_bytes),
        CaptureEvent::ReadFailed { stream, error } => io::Error::new(
            error.kind(),
            format!("could not read session {stream}: {error}"),
        ),
    }
}

fn output_limit_error(stream: &str, observed_bytes: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "session {stream} exceeded {SESSION_OUTPUT_BYTES_MAX} bytes; observed at least {observed_bytes} bytes"
        ),
    )
}

fn terminate_process_group(child: &mut Child) -> io::Result<()> {
    #[cfg(unix)]
    {
        use nix::{
            errno::Errno,
            sys::signal::{Signal, killpg},
            unistd::Pid,
        };

        let process_id = i32::try_from(child.id())
            .map_err(|_| io::Error::other("child process identifier exceeded i32"))?;
        if process_id <= 1 {
            return Err(io::Error::other(format!(
                "refusing to signal unsafe process group {process_id}"
            )));
        }
        if let Err(error) = killpg(Pid::from_raw(process_id), Signal::SIGKILL)
            && error != Errno::ESRCH
        {
            let _ = child.kill();
            return Err(io::Error::from(error));
        }
    }
    #[cfg(not(unix))]
    child.kill()?;
    Ok(())
}

fn classify(
    quirl: &RunnerCapture,
    bash: &RunnerCapture,
    zsh: &RunnerCapture,
    quirl_filesystem: &FilesystemManifest,
    bash_filesystem: &FilesystemManifest,
    zsh_filesystem: &FilesystemManifest,
) -> Classification {
    let references_agree = !bash.timed_out
        && !zsh.timed_out
        && bash.outcome == zsh.outcome
        && bash_filesystem == zsh_filesystem;
    if !references_agree {
        return Classification::ReferenceDivergence;
    }
    if quirl.timed_out || quirl.outcome != bash.outcome || quirl_filesystem != bash_filesystem {
        return Classification::NativeMismatch;
    }
    Classification::Match
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "manifest entry counts are bounded by the simulation filesystem limit"
)]
fn filesystem_manifest(root: &Path) -> Result<FilesystemManifest, TaskError> {
    let mut stack = vec![(root.to_path_buf(), 0_usize)];
    let mut files = Vec::new();
    let mut total_bytes = 0_usize;
    while let Some((directory, depth)) = stack.pop() {
        if depth > SESSION_PATH_DEPTH_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("session path depth exceeded {SESSION_PATH_DEPTH_MAX}"),
            )
            .into());
        }
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("generated session created a symlink: {}", path.display()),
                )
                .into());
            }
            if metadata.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "generated session created a special file: {}",
                        path.display()
                    ),
                )
                .into());
            }
            if files.len() >= SESSION_FILES_MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("session created more than {SESSION_FILES_MAX} files"),
                )
                .into());
            }
            let bytes = fs::read(&path)?;
            total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "session file size overflowed")
            })?;
            if total_bytes > SESSION_FILE_BYTES_MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("session files exceeded {SESSION_FILE_BYTES_MAX} bytes"),
                )
                .into());
            }
            let relative = path.strip_prefix(root)?.to_string_lossy().into_owned();
            let contents = String::from_utf8(bytes).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("session file {relative} was not UTF-8: {error}"),
                )
            })?;
            files.push(FileRecord {
                path: relative,
                contents,
            });
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(FilesystemManifest { files, total_bytes })
}

fn interpreter_identity(
    name: &str,
    search_path: &OsStr,
    workspace_root: &Path,
) -> Result<InterpreterIdentity, TaskError> {
    let executable = resolve_executable(name, search_path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("required reference interpreter `{name}` is unavailable on PATH"),
        )
    })?;
    let mut command = Command::new(&executable);
    command.arg("--version");
    configure_command(&mut command, workspace_root, search_path)?;
    let bounded = bounded_output(&mut command, SESSION_DEADLINE)?;
    if bounded.timed_out || !bounded.output.status.success() {
        return Err(io::Error::other(format!(
            "could not identify reference interpreter {}",
            executable.display()
        ))
        .into());
    }
    let version = String::from_utf8_lossy(&bounded.output.stdout)
        .lines()
        .next()
        .unwrap_or("unknown version")
        .to_owned();
    Ok(InterpreterIdentity {
        executable: executable.display().to_string(),
        version,
    })
}

fn resolve_executable(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    env::split_paths(search_path)
        .map(|directory| directory.join(format!("{name}{}", env::consts::EXE_SUFFIX)))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| candidate.canonicalize().ok())
}

fn write_failure(run_directory: &Path, record: &SessionRecord) -> Result<(), TaskError> {
    let failures_directory = run_directory.join("failures");
    create_directory_durable(&failures_directory)?;
    let directory = failures_directory.join(format!("session-{:05}", record.session));
    create_directory_durable(&directory)?;
    write_atomic(
        &directory.join("source.sh"),
        format!("{}\n", record.source).as_bytes(),
    )?;
    write_atomic(
        &directory.join("source.qrl"),
        format!("command {{\n{}\n}}\n", record.source).as_bytes(),
    )?;
    let result = serde_json::to_vec_pretty(record)?;
    write_atomic(&directory.join("result.json"), &result)?;
    Ok(())
}

fn write_summary(
    options: &SimulationOptions,
    run_directory: &Path,
    quirl: &ExecutableSnapshot,
    bash: &InterpreterIdentity,
    zsh: &InterpreterIdentity,
    counts: &SimulationCounts,
) -> Result<(), TaskError> {
    let result = if counts.mismatches == 0 {
        "passed"
    } else {
        "mismatch"
    };
    let summary = PersistedSummary {
        schema_version: SUMMARY_SCHEMA_VERSION,
        result,
        seed: options.seed,
        sessions_generated: options.session_count,
        sessions_evaluated: counts.evaluated,
        steps_max: options.steps_max,
        only_session: options.only_session,
        mismatch_count: counts.mismatches,
        native_mismatch_count: counts.native_mismatches,
        reference_divergence_count: counts.reference_divergences,
        first_mismatch_session: counts.first_mismatch,
        quirl_executable: quirl.source.display().to_string(),
        quirl_snapshot_sha256: quirl.sha256.clone(),
        quirl_snapshot_bytes: quirl.byte_size,
        bash,
        zsh,
        report: "report.jsonl",
    };
    if let Some(session) = counts.first_mismatch {
        let issue = format!(
            "# Quirl shell simulation mismatch\n\nResult: mismatch\n\n- Seed: `{}`\n- First mismatch: session `{session}`\n- Sessions generated: `{}`\n- Maximum steps per session: `{}`\n- Bash: `{}`\n- Zsh: `{}`\n\nReplay only the first mismatch:\n\n```console\ncargo xtask simulate --seed {} --sessions {} --steps {} --session {session} --output target/simulations/replay\n```\n",
            options.seed,
            options.session_count,
            options.steps_max,
            bash.version,
            zsh.version,
            options.seed,
            options.session_count,
            options.steps_max,
        );
        write_atomic(&run_directory.join("issue.md"), issue.as_bytes())?;
    }
    // The summary is the workflow's completion marker, so install it only
    // after every report and mismatch artifact is durable.
    let bytes = serde_json::to_vec_pretty(&summary)?;
    write_atomic(&run_directory.join("summary.json"), &bytes)?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let file_name = path.file_name().and_then(OsStr::to_str).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "artifact path has no filename")
    })?;
    let temporary = path.with_file_name(format!(".{file_name}.xtask-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let install = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "artifact path has no parent")
        })?;
        sync_directory(parent)
    })();
    if install.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    install
}

fn create_directory_durable(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "directory path has no parent")
            })?;
            sync_directory(parent)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn exit_status_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEED: AtomicU64 = AtomicU64::new(10_000);

    fn test_seed() -> u64 {
        TEST_SEED.fetch_add(1, Ordering::Relaxed)
    }

    fn fail_with_temporary_root(parent: &Path, observed_path: &mut PathBuf) -> io::Result<()> {
        let temporary = TemporaryRoot::create_in(parent, test_seed())?;
        observed_path.clone_from(&temporary.path);
        fs::write(temporary.path.join("partial"), "created before failure")?;
        Err(io::Error::other("induced simulation failure"))
    }

    #[cfg(unix)]
    fn adversarial_shell(script: &str) -> Command {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        command
    }

    #[test]
    fn pinned_executable_owns_its_copy_and_preserves_original_on_drop() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let source = parent.path.join("built-quirl");
        fs::write(&source, b"original executable bytes").unwrap();
        let copied_path;
        let copied_root;
        {
            let executable = PinnedExecutable::create(&source, test_seed()).unwrap();
            copied_path = executable.path().to_path_buf();
            copied_root = copied_path.parent().unwrap().to_path_buf();
            fs::remove_file(&source).unwrap();
            fs::write(&source, b"replacement executable bytes").unwrap();
            assert_eq!(
                fs::read(&copied_path).unwrap(),
                b"original executable bytes"
            );
            assert_eq!(executable.source(), source.canonicalize().unwrap());
            assert_eq!(executable.byte_size(), 25);
            assert_eq!(
                executable.sha256(),
                format!("{:x}", Sha256::digest(b"original executable bytes"))
            );
        }
        assert!(!copied_path.exists());
        assert!(!copied_root.exists());
        assert_eq!(fs::read(&source).unwrap(), b"replacement executable bytes");
    }

    #[test]
    fn executable_snapshot_keeps_original_bytes_after_source_replacement() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let source = parent.path.join("built-quirl");
        fs::write(&source, b"original executable bytes").unwrap();
        let root = TemporaryRoot::create_in(&parent.path, test_seed()).unwrap();
        let snapshot = snapshot_executable(&source, &root.path, 64).unwrap();
        let replacement = parent.path.join("rebuilt-quirl");
        fs::write(&replacement, b"replacement executable bytes").unwrap();
        fs::rename(&replacement, &source).unwrap();
        assert_eq!(
            fs::read(&snapshot.path).unwrap(),
            b"original executable bytes"
        );
        assert_eq!(snapshot.byte_size, 25);
        assert_eq!(
            snapshot.sha256,
            format!("{:x}", Sha256::digest(b"original executable bytes"))
        );
        assert_eq!(snapshot.source, source.canonicalize().unwrap());
    }

    #[test]
    fn executable_snapshot_and_streaming_copy_enforce_exact_byte_limit() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let source = parent.path.join("built-quirl");
        fs::write(&source, b"12345678").unwrap();
        let root = TemporaryRoot::create_in(&parent.path, test_seed()).unwrap();
        let snapshot = snapshot_executable(&source, &root.path, 8).unwrap();
        assert_eq!(snapshot.byte_size, 8);
        fs::write(&source, b"123456789").unwrap();
        let rejected_root = TemporaryRoot::create_in(&parent.path, test_seed()).unwrap();
        let error = snapshot_executable(&source, &rejected_root.path, 8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("limit 8; observed 9"));
        assert_eq!(fs::read_dir(&rejected_root.path).unwrap().count(), 0);
        // Exercise the streaming guard independently of the metadata precheck,
        // as when a regular file grows after admission.
        let mut input = File::open(&source).unwrap();
        let mut output = File::create(rejected_root.path.join("partial")).unwrap();
        let error = copy_executable_bytes(&mut input, &mut output, 8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(output.metadata().unwrap().len(), 0);
        drop(output);
        let rejected_path = rejected_root.path.clone();
        drop(rejected_root);
        assert!(!rejected_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn executable_snapshot_rejects_fifo_without_waiting_for_a_writer() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let fifo = parent.path.join("fifo");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        let started = Instant::now();
        let error = snapshot_executable(&fifo, &parent.path, 8).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn executable_snapshot_is_private_and_removed_with_its_temporary_root() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let source = parent.path.join("built-quirl");
        fs::write(&source, b"private bytes").unwrap();
        let snapshot_path;
        let root_path;
        {
            let root = TemporaryRoot::create_in(&parent.path, test_seed()).unwrap();
            root_path = root.path.clone();
            assert_eq!(
                fs::metadata(&root.path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let snapshot = snapshot_executable(&source, &root.path, 64).unwrap();
            snapshot_path = snapshot.path.clone();
            assert_eq!(
                fs::metadata(&snapshot.path).unwrap().permissions().mode() & 0o777,
                0o500
            );
        }
        assert!(!snapshot_path.exists());
        assert!(!root_path.exists());
    }

    #[test]
    fn summary_records_snapshot_identity_without_changing_session_schema() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let source = parent.path.join("built-quirl");
        fs::write(&source, b"executable").unwrap();
        let snapshot = snapshot_executable(&source, &parent.path, 64).unwrap();
        let interpreter = InterpreterIdentity {
            executable: "/test/shell".to_owned(),
            version: "test".to_owned(),
        };
        let options = SimulationOptions {
            quirl: source,
            seed: 1,
            session_count: 1,
            steps_max: 3,
            only_session: None,
            output_root: parent.path.clone(),
        };
        write_summary(
            &options,
            &parent.path,
            &snapshot,
            &interpreter,
            &interpreter,
            &SimulationCounts::default(),
        )
        .unwrap();
        let summary: serde_json::Value =
            serde_json::from_slice(&fs::read(parent.path.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary["schema_version"], 2);
        assert_eq!(
            summary["quirl_executable"],
            snapshot.source.display().to_string()
        );
        assert_eq!(summary["quirl_snapshot_sha256"], snapshot.sha256);
        assert_eq!(summary["quirl_snapshot_bytes"], 10);
        assert_eq!(REPORT_SCHEMA_VERSION, 1);
    }

    #[test]
    fn same_seed_generates_identical_stateful_sessions() {
        let mut first = DeterministicRng::new(42);
        let mut second = DeterministicRng::new(42);
        for _ in 0..128 {
            let left = generate_session(&mut first, 8).unwrap();
            let right = generate_session(&mut second, 8).unwrap();
            assert_eq!(left.source, right.source);
            assert_eq!(left.steps, right.steps);
        }
    }

    #[test]
    fn generated_sessions_keep_declared_bounds() {
        let mut generator = DeterministicRng::new(7);
        for _ in 0..1_000 {
            let session = generate_session(&mut generator, 32).unwrap();
            assert!((3..=32).contains(&session.steps.len()));
            assert!(session.source.len() <= SESSION_SOURCE_BYTES_MAX);
            assert!(session.source.contains("cd workspace"));
            assert!(session.source.contains("export QUIRL_SIM_VALUE="));
        }
    }

    #[test]
    fn temporary_root_removes_artifacts_after_success() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let temporary_path = {
            let temporary = TemporaryRoot::create_in(&parent.path, test_seed()).unwrap();
            fs::write(temporary.path.join("complete"), "complete").unwrap();
            temporary.path.clone()
        };

        assert!(!temporary_path.exists());
    }

    #[test]
    fn temporary_root_removes_artifacts_after_failure() {
        let parent = TemporaryRoot::create(test_seed()).unwrap();
        let mut temporary_path = PathBuf::new();

        let error = fail_with_temporary_root(&parent.path, &mut temporary_path).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(!temporary_path.exists());
    }

    #[test]
    fn reference_disagreement_is_not_misattributed_to_quirl() {
        let success = RunnerCapture {
            outcome: Some(ObservableOutcome {
                status: 0,
                stdout: "same".to_owned(),
                stderr: String::new(),
            }),
            process_status: 0,
            timed_out: false,
            raw_stdout: String::new(),
            raw_stderr: String::new(),
            diagnostic: None,
        };
        let mut divergent = success.clone();
        divergent.outcome.as_mut().unwrap().stdout = "different".to_owned();
        let manifest = FilesystemManifest {
            files: Vec::new(),
            total_bytes: 0,
        };
        assert_eq!(
            classify(
                &success, &success, &divergent, &manifest, &manifest, &manifest,
            ),
            Classification::ReferenceDivergence
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_is_rejected_at_the_ingestion_bound() {
        for (stream, script) in [
            ("stdout", "while :; do printf 0123456789abcdef; done"),
            ("stderr", "while :; do printf 0123456789abcdef >&2; done"),
        ] {
            let started = Instant::now();
            let error =
                bounded_output(&mut adversarial_shell(script), Duration::from_secs(2)).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
            assert!(error.to_string().contains(stream));
            assert!(
                error
                    .to_string()
                    .contains(&SESSION_OUTPUT_BYTES_MAX.to_string())
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[cfg(unix)]
    #[test]
    fn inherited_pipe_writer_cannot_extend_a_completed_command() {
        let started = Instant::now();
        let output = bounded_output(
            &mut adversarial_shell("sleep 30 & printf complete"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(!output.timed_out);
        assert_eq!(output.output.stdout, b"complete");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn deadline_kills_and_reaps_the_spawned_process_group() {
        use nix::{errno::Errno, sys::signal::killpg, unistd::Pid};

        let started = Instant::now();
        let output = bounded_output(
            &mut adversarial_shell("printf '%s\\n' $$; sleep 30"),
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(output.timed_out);
        assert!(started.elapsed() < Duration::from_secs(2));
        let process_group = String::from_utf8(output.output.stdout)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match killpg(Pid::from_raw(process_group), None) {
                Err(Errno::ESRCH) => break,
                Ok(()) | Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                result => panic!("process group {process_group} survived cleanup: {result:?}"),
            }
        }
    }
}
