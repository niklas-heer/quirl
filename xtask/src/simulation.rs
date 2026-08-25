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
//! - Bash and Zsh must agree before their result can be treated as an oracle.
//! - `summary.json` is installed atomically and last; its presence means every
//!   trace and mismatch artifact needed by the scheduled reporter is durable.

use serde::{Deserialize, Serialize};
use std::{
    env,
    error::Error,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const REPORT_SCHEMA_VERSION: u32 = 1;
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
            match fs::create_dir(&path) {
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

pub fn run(options: SimulationOptions) -> Result<SimulationResult, Box<dyn Error>> {
    validate_options(&options)?;
    let search_path = env::var_os("PATH")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is not configured"))?;
    // Identity probes use the same isolated root as generated sessions. Passing
    // the repository here would let configure_command create a repo-root tmp/.
    let temporary = TemporaryRoot::create(options.seed)?;
    let bash = interpreter_identity("bash", &search_path, &temporary.path)?;
    let zsh = interpreter_identity("zsh", &search_path, &temporary.path)?;
    let quirl = options.quirl.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not resolve built Quirl executable {}: {error}",
                options.quirl.display()
            ),
        )
    })?;
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
        quirl: &quirl,
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
) -> Result<SessionRecord, Box<dyn Error>> {
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
) -> Result<RunnerCapture, Box<dyn Error>> {
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
) -> Result<RunnerCapture, Box<dyn Error>> {
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
fn filesystem_manifest(root: &Path) -> Result<FilesystemManifest, Box<dyn Error>> {
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
) -> Result<InterpreterIdentity, Box<dyn Error>> {
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

fn write_failure(run_directory: &Path, record: &SessionRecord) -> Result<(), Box<dyn Error>> {
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
    quirl: &Path,
    bash: &InterpreterIdentity,
    zsh: &InterpreterIdentity,
    counts: &SimulationCounts,
) -> Result<(), Box<dyn Error>> {
    let result = if counts.mismatches == 0 {
        "passed"
    } else {
        "mismatch"
    };
    let summary = PersistedSummary {
        schema_version: REPORT_SCHEMA_VERSION,
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
        quirl_executable: quirl.display().to_string(),
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
