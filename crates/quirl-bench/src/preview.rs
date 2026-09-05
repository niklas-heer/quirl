use portable_pty::{Child, CommandBuilder, PtySize, native_pty_system};
use quirl_catalog::Catalog;
use quirl_lua::QuirlConfig;
use quirl_syntax::Mode;
use quirl_ui::{CatalogCompleter, LiveBuffer, LiveSample, QuirlPrompt};
use reedline::{Completer, Prompt, PromptEditMode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    error::Error,
    fs,
    hint::black_box,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEFAULT_COLD_SAMPLES: usize = 31;
const DEFAULT_EDIT_SAMPLES: usize = 2_000;
const DEFAULT_PROMPT_SAMPLES: usize = 500;
const DEFAULT_PTY_SAMPLES: usize = 31;
const DEFAULT_RELEASE_PTY_SAMPLES: usize = 101;
const DEFAULT_PTY_TIMEOUT_MS: usize = 2_000;
const DEFAULT_STREAM_SAMPLES: usize = 100_000;
const MINIMUM_ACCEPTED_PTY_SAMPLES: usize = 20;
// Binary-size policy uses binary mebibytes: one MiB is exactly 1,048,576 bytes.
// The ideal and soft cap are advisory; the hard ceiling is an enforcing gate.
const BINARY_IDEAL_BYTES: u64 = 5 * 1024 * 1024;
const BINARY_SOFT_CAP_BYTES: u64 = 8 * 1024 * 1024;
const BINARY_HARD_CEILING_BYTES: u64 = 10 * 1024 * 1024;
const _: () = assert!(BINARY_IDEAL_BYTES < BINARY_SOFT_CAP_BYTES);
const _: () = assert!(BINARY_SOFT_CAP_BYTES < BINARY_HARD_CEILING_BYTES);
const COLD_START_TARGET_MS: f64 = 25.0;
const EDIT_FRAME_TARGET_MS: f64 = 8.0;
const FIRST_PROMPT_TARGET_MS: f64 = 21.0;

#[derive(Debug, Serialize)]
struct PreviewReport {
    schema_version: u32,
    suite: &'static str,
    measured_at_utc: String,
    environment: Environment,
    methodology: Methodology,
    measurements: Vec<Measurement>,
    stream_window: StreamWindowMeasurement,
    binary_size: BinarySizeMeasurement,
    evidence_gate_passed: bool,
    performance_gate_passed: bool,
    warnings: Vec<String>,
    gate_failures: Vec<String>,
    release_gate_status: String,
}

#[derive(Debug, Serialize)]
struct Environment {
    hostname: String,
    operating_system: String,
    architecture: &'static str,
    cpu: String,
    logical_cpus: usize,
    memory_bytes: Option<u64>,
    rustc: String,
    cargo: String,
    source_commit: String,
    source_dirty: Option<bool>,
    artifact_digest_verified: bool,
    artifact_profile_verified: bool,
    artifact_source_verified: bool,
    harness_source_verified: bool,
    build_profile: String,
    optimization_level: String,
    panic_strategy: String,
    quirl_binary: String,
    quirl_binary_bytes: Option<u64>,
    quirl_binary_sha256: Option<String>,
    quirl_version: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QuirlBuildInfo {
    schema_version: u32,
    version: String,
    build_profile: String,
    optimization_level: String,
    panic_strategy: String,
    operating_system: String,
    architecture: String,
    source_commit: String,
    build_timestamp: String,
    official_release: bool,
    source_dirty: Option<bool>,
}

#[derive(Debug, Serialize)]
struct Methodology {
    percentile_method: &'static str,
    pty_end_to_end: &'static str,
    cold_start: &'static str,
    headless_edit_frame: &'static str,
    first_prompt: &'static str,
    stream_window: &'static str,
    binary_size: &'static str,
    limitations: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct StreamWindowMeasurement {
    id: &'static str,
    input_samples_per_capacity: usize,
    capacities: Vec<StreamCapacityEvidence>,
    invariant_valid: bool,
    release_gate_accepted: bool,
    explanation: &'static str,
}

#[derive(Debug, Serialize)]
struct StreamCapacityEvidence {
    capacity: usize,
    retained_samples: usize,
    dropped_samples: u64,
    serialized_snapshot_bytes: usize,
}

#[derive(Debug, Serialize)]
struct BinarySizeMeasurement {
    id: &'static str,
    bytes: Option<u64>,
    ideal_bytes: u64,
    soft_cap_bytes: u64,
    hard_ceiling_bytes: u64,
    enforced_limit_bytes: u64,
    policy_result: &'static str,
    target_result: &'static str,
    measurement_valid: bool,
    hard_gate_passed: bool,
    release_gate_accepted: bool,
    warning: Option<String>,
    explanation: &'static str,
}

#[derive(Debug, Serialize)]
struct Measurement {
    id: &'static str,
    label: &'static str,
    samples: usize,
    successful_samples: usize,
    failures: Vec<String>,
    includes_terminal_io: bool,
    min_ms: Option<f64>,
    p50_ms: Option<f64>,
    p95_ms: Option<f64>,
    max_ms: Option<f64>,
    target: TargetAssessment,
}

#[derive(Debug, Serialize)]
struct TargetAssessment {
    specification_target: &'static str,
    measured_percentile: &'static str,
    limit_ms: f64,
    target_result: &'static str,
    measurement_valid: bool,
    release_gate_accepted: bool,
    explanation: &'static str,
}

#[derive(Debug)]
struct Statistics {
    samples: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Debug)]
struct PtyStatistics {
    requested_samples: usize,
    successful_samples: usize,
    prompt_paint: Option<Statistics>,
    cold_to_editable: Option<Statistics>,
    keystroke_to_frame: Option<Statistics>,
    failures: Vec<String>,
}

#[derive(Debug)]
struct PtySample {
    prompt_paint: Duration,
    cold_to_editable: Duration,
    keystroke_to_frame: Duration,
}

#[derive(Clone, Copy)]
struct PtyMeasurementSpec {
    id: &'static str,
    label: &'static str,
    specification_target: &'static str,
    measured_percentile: &'static str,
    limit_ms: f64,
    explanation: &'static str,
}

enum PtyEvent {
    Data(Vec<u8>),
    End,
    Error(String),
}

struct PtySession {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    receiver: Receiver<PtyEvent>,
    reader_thread: Option<JoinHandle<()>>,
    parser: vt100::Parser,
    query_tail: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
enum HighlightClass {
    Whitespace,
    KnownCommand,
    UnknownCommand,
    Option,
    Quoted,
    Value,
}

pub fn run(enforce: bool) -> Result<(), Box<dyn Error>> {
    if enforce && cfg!(debug_assertions) {
        return Err(
            "the release gate must be run from a release-built quirl-bench; use `cargo build --release -p quirl-cli -p quirl-bench`, record the Quirl binary's SHA-256, then run `target/release/quirl-bench release --quirl target/release/quirl --expected-sha256 <sha256>`"
                .into(),
        );
    }
    let source_quirl = quirl_binary()?;
    let expected_sha256 = expected_sha256_argument(enforce)?;
    let staged_quirl = StagedArtifact::copy_from(&source_quirl)?;
    if let Some(expected) = expected_sha256.as_deref() {
        verify_expected_sha256(expected, staged_quirl.sha256())?;
    }
    let artifact_digest_verified = expected_sha256.is_some();
    let quirl = staged_quirl.path().to_path_buf();
    let build_info = quirl_build_info(&quirl);
    let artifact_profile_verified = build_info
        .as_ref()
        .is_some_and(build_info_matches_benchmark);
    if enforce && !artifact_profile_verified {
        return Err(
            "the release gate requires the measured quirl binary to report the canonical build profile, optimization level, panic strategy, operating system, and architecture matching quirl-bench; build both together with `cargo build --release -p quirl-cli -p quirl-bench` and pass that build's quirl binary"
                .into(),
        );
    }
    let harness_source_verified = build_info
        .as_ref()
        .is_some_and(build_info_matches_harness_source);
    if enforce && !harness_source_verified {
        return Err(
            "the release gate requires quirl and quirl-bench to be built cleanly from the same source commit; build both together from a clean candidate with `cargo build --release -p quirl-cli -p quirl-bench`"
                .into(),
        );
    }
    let pty_samples = sample_argument("--pty-samples", default_pty_samples(enforce))?;
    let pty_timeout_ms = sample_argument("--pty-timeout-ms", DEFAULT_PTY_TIMEOUT_MS)?;
    let cold_samples = sample_argument("--cold-samples", DEFAULT_COLD_SAMPLES)?;
    let edit_samples = sample_argument("--edit-samples", DEFAULT_EDIT_SAMPLES)?;
    let prompt_samples = sample_argument("--prompt-samples", DEFAULT_PROMPT_SAMPLES)?;
    let stream_samples = sample_argument("--stream-samples", DEFAULT_STREAM_SAMPLES)?;
    let binary_limit = binary_limit_argument()?;

    let rich_status_identity = build_info.as_ref().map(rich_status_identity);
    let pty = measure_pty(
        &quirl,
        pty_samples,
        Duration::from_millis(u64::try_from(pty_timeout_ms).unwrap_or(u64::MAX)),
        rich_status_identity.as_deref(),
    );
    let cold = measure_cli_startup(&quirl, cold_samples)?;
    let edit = measure_headless_edit_frame(edit_samples)?;
    let prompt = measure_first_prompt(prompt_samples)?;
    let stream_window = measure_stream_window(stream_samples)?;
    let binary_size = measure_binary_size(&quirl, binary_limit);
    let mut environment = discover_environment(
        &quirl,
        build_info.as_ref(),
        artifact_digest_verified,
        artifact_profile_verified,
        harness_source_verified,
    );
    environment.artifact_source_verified = build_info.as_ref().is_some_and(|info| {
        info.source_dirty == Some(false)
            && info.source_commit != "unknown"
            && info.source_commit == environment.source_commit
    });
    let pty_valid = pty.requested_samples >= MINIMUM_ACCEPTED_PTY_SAMPLES
        && pty.successful_samples == pty.requested_samples
        && pty.prompt_paint.is_some()
        && pty.cold_to_editable.is_some()
        && pty.keystroke_to_frame.is_some()
        && !cfg!(debug_assertions)
        && artifact_profile_verified;
    let measurements = vec![
        pty_measurement(
            PtyMeasurementSpec {
                id: "pty_cold_to_editable",
                label: "process start to first prompt accepting and painting a short input marker",
                specification_target: "cold start to editable prompt P50 <=25 ms",
                measured_percentile: "P50",
                limit_ms: COLD_START_TARGET_MS,
                explanation: "A PTY terminal model observed the prompt and a short input marker rendered in the editable buffer.",
            },
            &pty,
            pty.cold_to_editable.as_ref(),
            pty.cold_to_editable.as_ref().map(|stats| stats.p50_ms),
            pty_valid,
        ),
        pty_measurement(
            PtyMeasurementSpec {
                id: "pty_keystroke_to_frame",
                label: "injected final keystroke to corresponding terminal frame",
                specification_target: "keystroke to frame P95 <=8 ms",
                measured_percentile: "P95",
                limit_ms: EDIT_FRAME_TARGET_MS,
                explanation: "The timer starts immediately before the final byte and ends when the PTY terminal model contains the complete expected edited buffer.",
            },
            &pty,
            pty.keystroke_to_frame.as_ref(),
            pty.keystroke_to_frame.as_ref().map(|stats| stats.p95_ms),
            pty_valid,
        ),
        pty_measurement(
            PtyMeasurementSpec {
                id: "pty_first_prompt_paint",
                label: "process start to first rendered prompt frame",
                specification_target: "first prompt paint P95 <=21 ms",
                measured_percentile: "P95",
                limit_ms: FIRST_PROMPT_TARGET_MS,
                explanation: "A VT100 terminal model observed Quirl's command prompt indicator; physical monitor scanout is outside this software benchmark.",
            },
            &pty,
            pty.prompt_paint.as_ref(),
            pty.prompt_paint.as_ref().map(|stats| stats.p95_ms),
            pty_valid,
        ),
        Measurement {
            id: "cli_process_startup_to_version_exit",
            label: "fresh CLI subprocess to `quirl --version` exit",
            samples: cold.samples,
            successful_samples: cold.samples,
            failures: Vec::new(),
            includes_terminal_io: false,
            min_ms: Some(cold.min_ms),
            p50_ms: Some(cold.p50_ms),
            p95_ms: Some(cold.p95_ms),
            max_ms: Some(cold.max_ms),
            target: lower_bound_assessment(
                cold.p50_ms,
                COLD_START_TARGET_MS,
                "cold start to editable prompt P50 <=25 ms",
                "P50",
                "This subprocess lower bound excludes interactive editor and prompt initialization, so a result below the target cannot pass the release gate.",
            ),
        },
        Measurement {
            id: "headless_edit_cpu_proxy",
            label: "completion + semantic-highlight proxy + prompt render",
            samples: edit.samples,
            successful_samples: edit.samples,
            failures: Vec::new(),
            includes_terminal_io: false,
            min_ms: Some(edit.min_ms),
            p50_ms: Some(edit.p50_ms),
            p95_ms: Some(edit.p95_ms),
            max_ms: Some(edit.max_ms),
            target: proxy_assessment(
                edit.p95_ms,
                EDIT_FRAME_TARGET_MS,
                "keystroke to frame P95 <=8 ms",
                "P95",
                "Headless CPU proxy excludes Reedline layout, terminal writes, terminal synchronization, and display latency.",
            ),
        },
        Measurement {
            id: "first_prompt_construct_and_render_cpu",
            label: "fresh prompt construction and string rendering",
            samples: prompt.samples,
            successful_samples: prompt.samples,
            failures: Vec::new(),
            includes_terminal_io: false,
            min_ms: Some(prompt.min_ms),
            p50_ms: Some(prompt.p50_ms),
            p95_ms: Some(prompt.p95_ms),
            max_ms: Some(prompt.max_ms),
            target: proxy_assessment(
                prompt.p95_ms,
                FIRST_PROMPT_TARGET_MS,
                "first prompt paint P95 <=21 ms",
                "P95",
                "Measures prompt construction and render methods only; it does not measure terminal paint or time to an editable input loop.",
            ),
        },
    ];

    let timing_failures = timing_gate_failures(&measurements);
    let source_identity_valid = environment.source_commit != "unknown"
        && environment.source_dirty == Some(false)
        && environment.artifact_digest_verified
        && environment.artifact_source_verified
        && environment.harness_source_verified
        && environment.quirl_binary_sha256.is_some();
    let evidence_gate_passed = pty_valid
        && stream_window.release_gate_accepted
        && binary_size.measurement_valid
        && source_identity_valid;
    let mut gate_failures = timing_failures;
    let warnings = binary_size.warning.iter().cloned().collect::<Vec<_>>();
    if !stream_window.release_gate_accepted {
        gate_failures
            .push("stream retention did not remain bounded by the configured window".to_owned());
    }
    if let Some(failure) = binary_size_gate_failure(&binary_size) {
        gate_failures.push(failure);
    }
    if environment.source_commit == "unknown" {
        gate_failures.push("source commit could not be identified".to_owned());
    }
    if !environment.artifact_digest_verified {
        gate_failures.push(
            "release binary SHA-256 was not compared with an independently supplied digest"
                .to_owned(),
        );
    }
    if !environment.artifact_source_verified {
        gate_failures.push(
            "the measured quirl binary was not built cleanly from the recorded source commit"
                .to_owned(),
        );
    }
    if !environment.harness_source_verified {
        gate_failures.push(
            "quirl and quirl-bench were not built cleanly from the same source commit".to_owned(),
        );
    }
    match environment.source_dirty {
        Some(false) => {}
        Some(true) => gate_failures.push(
            "source tree contains tracked or untracked changes, so the artifact is not reproducible from its recorded commit"
                .to_owned(),
        ),
        None => gate_failures.push("source dirty state could not be determined".to_owned()),
    }
    if environment.quirl_binary_sha256.is_none() {
        gate_failures.push("release binary SHA-256 could not be measured".to_owned());
    }
    let performance_gate_passed = evidence_gate_passed && gate_failures.is_empty();
    let release_gate_status = if performance_gate_passed {
        "passed_all_release_budgets"
    } else if evidence_gate_passed {
        "failed_one_or_more_release_budgets"
    } else {
        "not_accepted_measurements_incomplete"
    };

    let report = PreviewReport {
        schema_version: 7,
        suite: "quirl_1.0_release_performance",
        measured_at_utc: measured_at_utc(),
        environment,
        methodology: Methodology {
            percentile_method: "nearest-rank over independently timed wall-clock samples",
            pty_end_to_end: "Each sample opens a fresh 120x40 pseudo-terminal, starts the release Quirl process, answers terminal cursor-position requests, and feeds output into a VT100 terminal model. It records process start to the first prompt frame, validates editability with a short input marker, then records a final representative keystroke until the expected edited buffer is present in the terminal frame. `release` defaults to 101 independent PTY samples for a steadier nearest-rank P95; `preview` retains 31, and `--pty-samples` overrides either mode.",
            cold_start: "Starts a new Quirl process for every sample and waits for `quirl --version` to exit. This measures process creation, dynamic loading, and CLI argument parsing, not cold-to-editable startup. OS filesystem caches are not flushed.",
            headless_edit_frame: "Calls Quirl's real CatalogCompleter and Prompt render methods, plus a benchmark-owned equivalent of the current semantic token classification, for `git commit --am`. No Reedline layout or terminal I/O occurs.",
            first_prompt: "Constructs a fresh configured QuirlPrompt and renders left, right, and indicator strings for every sample. Filesystem metadata may be served from OS cache. No terminal I/O occurs.",
            stream_window: "Pushes a fixed-size typed sample sequence through Quirl's production LiveBuffer at capacities 1, 16, and 256, then verifies retained and dropped counts and records serialized snapshot bytes. This proves retention is bounded by window size for bounded records; it is not a producer-backpressure or allocator-RSS measurement.",
            binary_size: "Copies the executable passed with `--quirl` into a private read-only staging directory, verifies its SHA-256 when enforcing the release gate, then measures that exact staged executable. Binary units are MiB (1 MiB = 1,048,576 bytes): 5 MiB is ideal, more than 8 MiB warns, and more than 10 MiB fails the hard gate. `--max-binary-bytes` may impose a stricter hard limit but cannot relax the 10 MiB project ceiling.",
            limitations: vec![
                "A completed frame means the expected screen state was reconstructed from the PTY byte stream; physical terminal-emulator scheduling, GPU composition, and monitor scanout are not measured.",
                "The UI highlighter is private; the edit proxy reproduces its command/option/quote classification but not StyledText allocation or rendering.",
                "The benchmark does not control CPU frequency, other machine load, thermal state, or OS filesystem caches.",
                "Results from debug builds are emitted but are not suitable for release decisions; run with `cargo run --release`.",
            ],
        },
        measurements,
        stream_window,
        binary_size,
        evidence_gate_passed,
        performance_gate_passed,
        warnings,
        gate_failures,
        release_gate_status: release_gate_status.to_owned(),
    };

    if env::args().any(|argument| argument == "--json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
    }
    if enforce && !report.performance_gate_passed {
        return Err(format!(
            "release performance gate failed: {}",
            report.gate_failures.join("; ")
        )
        .into());
    }
    Ok(())
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "sample counts are bounded benchmark parameters validated before the loop"
)]
fn measure_pty(
    path: &Path,
    samples: usize,
    timeout: Duration,
    rich_status_identity: Option<&str>,
) -> PtyStatistics {
    let fixture = match PtyFixture::create() {
        Ok(fixture) => fixture,
        Err(error) => {
            return PtyStatistics {
                requested_samples: samples,
                successful_samples: 0,
                prompt_paint: None,
                cold_to_editable: None,
                keystroke_to_frame: None,
                failures: vec![format!("could not create isolated PTY fixture: {error}")],
            };
        }
    };
    let mut prompt_paint = Vec::with_capacity(samples);
    let mut cold_to_editable = Vec::with_capacity(samples);
    let mut keystroke_to_frame = Vec::with_capacity(samples);
    let mut failures = Vec::new();

    for sample in 0..samples {
        match measure_pty_sample(path, &fixture, timeout, rich_status_identity) {
            Ok(measurement) => {
                prompt_paint.push(measurement.prompt_paint);
                cold_to_editable.push(measurement.cold_to_editable);
                keystroke_to_frame.push(measurement.keystroke_to_frame);
            }
            Err(error) => failures.push(format!("sample {}: {error}", sample + 1)),
        }
    }

    let successful_samples = prompt_paint.len();
    PtyStatistics {
        requested_samples: samples,
        successful_samples,
        prompt_paint: statistics(prompt_paint),
        cold_to_editable: statistics(cold_to_editable),
        keystroke_to_frame: statistics(keystroke_to_frame),
        failures,
    }
}

fn rich_status_identity(info: &QuirlBuildInfo) -> String {
    if info.official_release {
        return format!("🌀 v{}", info.version);
    }
    let short_commit = info.source_commit.chars().take(7).collect::<String>();
    let dirty = if info.source_dirty == Some(true) {
        "*"
    } else {
        ""
    };
    format!("🌀 dev@{}+{short_commit}{dirty}", info.build_timestamp)
}

fn editable_command_frame(screen: &str, rich_status_identity: Option<&str>) -> bool {
    let prompt_is_visible = screen.lines().any(|line| line.trim() == "❯");
    let Some(status) = screen.lines().next_back() else {
        return false;
    };
    let mode_is_visible = status.contains(" NORMAL ");
    let identity_is_visible = rich_status_identity.is_none_or(|identity| status.contains(identity));
    prompt_is_visible && mode_is_visible && identity_is_visible
}

#[allow(
    clippy::string_slice,
    reason = "the captured prompt marker is ASCII and find returns a UTF-8 boundary"
)]
fn measure_pty_sample(
    path: &Path,
    fixture: &PtyFixture,
    timeout: Duration,
    rich_status_identity: Option<&str>,
) -> Result<PtySample, Box<dyn Error>> {
    let started = Instant::now();
    let mut session = PtySession::spawn(path, fixture)?;
    let result = (|| {
        // Readiness requires the command prompt, mode label, and the exact
        // binary identity in one modeled frame. Status notices may replace the
        // optional shortcut hints, so presentation copy is not a readiness
        // invariant.
        session.wait_for_editable_command_frame(rich_status_identity, timeout)?;
        let prompt_paint = started.elapsed();

        // A short prefix of the representative edit establishes that the newly
        // painted editor accepts input. Its first paint is the cold-start
        // endpoint; stabilization below is deliberately outside that timing.
        let marker = "qz";
        session.assert_absent(marker, "editable prompt marker")?;
        session.send(marker.as_bytes())?;
        session.wait_for_screen(marker, timeout, "editable prompt marker")?;
        let cold_to_editable = started.elapsed();
        session.wait_for_stable_screen(marker, timeout, "editable prompt marker")?;

        // Advance only after each prefix is stable: viewport growth can issue
        // a cursor-position request, and a benchmark must not inject the next
        // synthetic key into that protocol exchange. This setup is untimed.
        let mut painted = marker.to_owned();
        let baseline = "qzbenchmarkloa";
        session.type_until_painted(
            &mut painted,
            &baseline[marker.len()..],
            timeout,
            "representative edit baseline",
        )?;
        let edit_started = Instant::now();
        session.assert_absent("qzbenchmarkload", "edited terminal frame")?;
        session.send(b"d")?;
        session.wait_for_screen("qzbenchmarkload", timeout, "edited terminal frame")?;

        Ok(PtySample {
            prompt_paint,
            cold_to_editable,
            keystroke_to_frame: edit_started.elapsed(),
        })
    })();
    session.finish();
    result
}

struct PtyFixture {
    root: PathBuf,
    config_dir: PathBuf,
    history: PathBuf,
    index: PathBuf,
}

impl PtyFixture {
    fn create() -> Result<Self, Box<dyn Error>> {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let root =
            env::temp_dir().join(format!("quirl-preview-pty-{}-{nonce}", std::process::id()));
        let config_dir = root.join("config");
        fs::create_dir_all(&config_dir)?;
        // macOS exposes /var as a symlink to /private/var. Canonicalizing once
        // prevents Quirl's no-symlink database admission from rejecting the
        // benchmark's own otherwise-private fixture path.
        let root = fs::canonicalize(root)?;
        let config_dir = root.join("config");
        Ok(Self {
            history: root.join("history"),
            index: root.join("catalog.json"),
            root,
            config_dir,
        })
    }
}

impl Drop for PtyFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl PtySession {
    #[allow(
        clippy::indexing_slicing,
        reason = "the PTY descriptor pair is populated by openpty before either fixed slot is read"
    )]
    fn spawn(path: &Path, fixture: &PtyFixture) -> Result<Self, Box<dyn Error>> {
        let pair = native_pty_system().openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut command = CommandBuilder::new(path);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env(
            "LC_ALL",
            if cfg!(target_os = "macos") {
                "en_US.UTF-8"
            } else {
                "C.UTF-8"
            },
        );
        command.env_remove("NO_COLOR");
        command.env("QUIRL_CONFIG_DIR", &fixture.config_dir);
        command.env("QUIRL_HISTORY", &fixture.history);
        command.env("QUIRL_INDEX_PATH", &fixture.index);
        command.cwd(env::current_dir()?);

        let child = pair.slave.spawn_command(command)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        drop(pair.slave);
        drop(pair.master);

        // Retain at most 16 × 8 KiB chunks while the consumer models a frame.
        // Backpressure stops a noisy candidate from growing the benchmark heap.
        // finish drops the receiver so a blocked sender can always unwind.
        let (sender, receiver) = mpsc::sync_channel(16);
        let reader_thread = thread::spawn(move || {
            let mut buffer = vec![0_u8; 8 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = sender.send(PtyEvent::End);
                        break;
                    }
                    Ok(length) => {
                        if sender
                            .send(PtyEvent::Data(buffer[..length].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(PtyEvent::Error(error.to_string()));
                        break;
                    }
                }
            }
        });

        Ok(Self {
            child,
            writer,
            receiver,
            reader_thread: Some(reader_thread),
            parser: vt100::Parser::new(40, 120, 0),
            query_tail: Vec::new(),
        })
    }

    fn send(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    fn type_until_painted(
        &mut self,
        painted: &mut String,
        suffix: &str,
        timeout: Duration,
        phase: &str,
    ) -> Result<(), Box<dyn Error>> {
        if !suffix.is_ascii() {
            return Err("PTY benchmark input must remain ASCII".into());
        }
        for byte in suffix.bytes() {
            painted.push(char::from(byte));
            self.send(&[byte])?;
            // The VT100 model intentionally trims trailing blank cells from
            // `contents()`. The following visible byte acknowledges both the
            // whitespace and itself without weakening the final prefix check.
            if !byte.is_ascii_whitespace() {
                self.wait_for_stable_screen(painted, timeout, phase)?;
            }
        }
        Ok(())
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the benchmark deadline"
    )]
    fn wait_for_stable_screen(
        &mut self,
        marker: &str,
        timeout: Duration,
        phase: &str,
    ) -> Result<(), Box<dyn Error>> {
        self.wait_for_screen(marker, timeout, phase)?;
        let deadline = Instant::now() + timeout;
        let quiet_period = Duration::from_millis(20);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {:.0} ms waiting for stable {phase} `{marker}`; screen={:?}",
                    millis(timeout),
                    screen_tail(&self.parser.screen().contents())
                )
                .into());
            }
            match self.receiver.recv_timeout(remaining.min(quiet_period)) {
                Ok(PtyEvent::Data(bytes)) => {
                    self.answer_cursor_queries(&bytes)?;
                    self.parser.process(&bytes);
                    if !self.parser.screen().contents().contains(marker) {
                        self.wait_for_screen(marker, remaining, phase)?;
                    }
                }
                Ok(PtyEvent::End) => {
                    return Err(format!("PTY ended while stabilizing {phase} `{marker}`").into());
                }
                Ok(PtyEvent::Error(error)) => {
                    return Err(
                        format!("PTY read failed while stabilizing {phase}: {error}").into(),
                    );
                }
                Err(RecvTimeoutError::Timeout) => return Ok(()),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("PTY reader disconnected while stabilizing {phase}").into());
                }
            }
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the benchmark deadline"
    )]
    fn wait_for_editable_command_frame(
        &mut self,
        rich_status_identity: Option<&str>,
        timeout: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.parser.screen().contents();
            if editable_command_frame(&screen, rich_status_identity) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {:.0} ms waiting for first editable command frame; screen={:?}",
                    millis(timeout),
                    screen_tail(&screen)
                )
                .into());
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(PtyEvent::Data(bytes)) => {
                    self.answer_cursor_queries(&bytes)?;
                    self.parser.process(&bytes);
                }
                Ok(PtyEvent::End) => {
                    return Err("PTY ended while waiting for first editable command frame".into());
                }
                Ok(PtyEvent::Error(error)) => {
                    return Err(format!(
                        "PTY read failed while waiting for first editable command frame: {error}"
                    )
                    .into());
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "timed out after {:.0} ms waiting for first editable command frame; screen={:?}",
                        millis(timeout),
                        screen_tail(&self.parser.screen().contents())
                    )
                    .into());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(
                        "PTY reader disconnected while waiting for first editable command frame"
                            .into(),
                    );
                }
            }
        }
    }

    fn assert_absent(&self, marker: &str, phase: &str) -> Result<(), Box<dyn Error>> {
        let screen = self.parser.screen().contents();
        if screen.contains(marker) {
            return Err(format!(
                "{phase} marker `{marker}` was already on screen before injection; screen={:?}",
                screen_tail(&screen)
            )
            .into());
        }
        Ok(())
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "poll counts are bounded by the benchmark deadline"
    )]
    fn wait_for_screen(
        &mut self,
        marker: &str,
        timeout: Duration,
        phase: &str,
    ) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            let screen = self.parser.screen().contents();
            if screen.contains(marker) {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "timed out after {:.0} ms waiting for {phase} `{marker}`; screen={:?}",
                    millis(timeout),
                    screen_tail(&screen)
                )
                .into());
            }
            match self.receiver.recv_timeout(remaining) {
                Ok(PtyEvent::Data(bytes)) => {
                    self.answer_cursor_queries(&bytes)?;
                    self.parser.process(&bytes);
                }
                Ok(PtyEvent::End) => {
                    return Err(format!("PTY ended while waiting for {phase} `{marker}`").into());
                }
                Ok(PtyEvent::Error(error)) => {
                    return Err(
                        format!("PTY read failed while waiting for {phase}: {error}").into(),
                    );
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "timed out after {:.0} ms waiting for {phase} `{marker}`; screen={:?}",
                        millis(timeout),
                        screen_tail(&self.parser.screen().contents())
                    )
                    .into());
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("PTY reader disconnected while waiting for {phase}").into());
                }
            }
        }
    }

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "the cursor reply count is bounded by the captured PTY byte limit"
    )]
    fn answer_cursor_queries(&mut self, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
        let previous_length = self.query_tail.len();
        self.query_tail.extend_from_slice(bytes);
        let queries = cursor_query_count(&self.query_tail, previous_length);
        for _ in 0..queries {
            self.writer.write_all(b"\x1b[1;1R")?;
        }
        if queries > 0 {
            self.writer.flush()?;
        }
        let retained = self.query_tail.len().min(4);
        self.query_tail.drain(..self.query_tail.len() - retained);
        Ok(())
    }

    fn finish(mut self) {
        let _ = self.writer.write_all(b"\x04");
        let _ = self.writer.flush();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        drop(self.writer);
        drop(self.receiver);
        if let Some(handle) = self.reader_thread.take()
            && handle.is_finished()
        {
            let _ = handle.join();
        }
    }
}

fn cursor_query_count(bytes: &[u8], previous_length: usize) -> usize {
    [b"\x1b[6n".as_slice(), b"\x1b[?6n".as_slice()]
        .into_iter()
        .map(|query| {
            bytes
                .windows(query.len())
                .enumerate()
                .filter(|(start, window)| {
                    *window == query && start.saturating_add(query.len()) > previous_length
                })
                .count()
        })
        .sum()
}

fn screen_tail(screen: &str) -> String {
    screen
        .chars()
        .rev()
        .take(240)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

fn pty_measurement(
    spec: PtyMeasurementSpec,
    pty: &PtyStatistics,
    statistics: Option<&Statistics>,
    measured_ms: Option<f64>,
    valid: bool,
) -> Measurement {
    Measurement {
        id: spec.id,
        label: spec.label,
        samples: pty.requested_samples,
        successful_samples: pty.successful_samples,
        failures: pty.failures.clone(),
        includes_terminal_io: true,
        min_ms: statistics.map(|stats| stats.min_ms),
        p50_ms: statistics.map(|stats| stats.p50_ms),
        p95_ms: statistics.map(|stats| stats.p95_ms),
        max_ms: statistics.map(|stats| stats.max_ms),
        target: actual_assessment(
            measured_ms,
            spec.limit_ms,
            spec.specification_target,
            spec.measured_percentile,
            valid,
            spec.explanation,
        ),
    }
}

fn measure_cli_startup(path: &Path, samples: usize) -> Result<Statistics, Box<dyn Error>> {
    for _ in 0..3 {
        run_version(path)?;
    }
    timed(samples, || run_version(path))
}

fn run_version(path: &Path) -> Result<(), Box<dyn Error>> {
    let status = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("{} --version exited with {status}", path.display()).into());
    }
    Ok(())
}

fn measure_headless_edit_frame(samples: usize) -> Result<Statistics, Box<dyn Error>> {
    let catalog = Catalog::builtin();
    let mut completer = CatalogCompleter::new(catalog.clone());
    let prompt = QuirlPrompt::with_config(Mode::Command, &QuirlConfig::default())
        .with_status(0)
        .with_jobs(1);
    let input = "git commit --am";
    for _ in 0..50 {
        edit_frame(&catalog, &mut completer, &prompt, input);
    }
    timed(samples, || {
        edit_frame(&catalog, &mut completer, &prompt, input);
        Ok(())
    })
}

fn edit_frame(
    catalog: &Catalog,
    completer: &mut CatalogCompleter,
    prompt: &QuirlPrompt,
    input: &str,
) {
    let completions = completer.complete(input, input.len());
    let highlighted = semantic_highlight_proxy(catalog, input);
    let left = prompt.render_prompt_left();
    let right = prompt.render_prompt_right();
    let indicator = prompt.render_prompt_indicator(PromptEditMode::Default);
    black_box((completions, highlighted, left, right, indicator));
}

fn measure_first_prompt(samples: usize) -> Result<Statistics, Box<dyn Error>> {
    let config = QuirlConfig::default();
    for _ in 0..20 {
        construct_and_render_prompt(&config);
    }
    timed(samples, || {
        construct_and_render_prompt(&config);
        Ok(())
    })
}

fn construct_and_render_prompt(config: &QuirlConfig) {
    let prompt = QuirlPrompt::with_config(Mode::Command, config)
        .with_status(0)
        .with_jobs(0);
    let left = prompt.render_prompt_left();
    let right = prompt.render_prompt_right();
    let indicator = prompt.render_prompt_indicator(PromptEditMode::Default);
    black_box((&prompt, left, right, indicator));
}

fn measure_stream_window(samples: usize) -> Result<StreamWindowMeasurement, Box<dyn Error>> {
    let mut capacities = Vec::new();
    let mut invariant_valid = true;
    for capacity in [1, 16, 256] {
        let mut buffer = LiveBuffer::new(capacity)?;
        for sequence in 0..samples {
            let sequence = u64::try_from(sequence)?;
            let accepted = buffer.push(LiveSample {
                sequence,
                value: serde_json::json!({ "bounded_fixture": sequence % 10 }),
            });
            invariant_valid &= accepted;
        }
        let snapshot = buffer.snapshot();
        let expected_retained = samples.min(capacity);
        let expected_dropped = u64::try_from(samples.saturating_sub(capacity))?;
        invariant_valid &= snapshot.capacity == capacity
            && snapshot.samples.len() == expected_retained
            && snapshot.dropped == expected_dropped;
        capacities.push(StreamCapacityEvidence {
            capacity,
            retained_samples: snapshot.samples.len(),
            dropped_samples: snapshot.dropped,
            serialized_snapshot_bytes: serde_json::to_vec(&snapshot)?.len(),
        });
    }
    Ok(StreamWindowMeasurement {
        id: "live_stream_window_retention",
        input_samples_per_capacity: samples,
        capacities,
        invariant_valid,
        release_gate_accepted: invariant_valid,
        explanation: "Production LiveBuffer retention equals min(input, capacity) and dropped counts equal input minus capacity at every supported boundary tested.",
    })
}

fn measure_binary_size(path: &Path, limit_bytes: u64) -> BinarySizeMeasurement {
    let bytes = fs::metadata(path).ok().map(|metadata| metadata.len());
    binary_size_measurement(bytes, limit_bytes)
}

fn binary_size_measurement(bytes: Option<u64>, enforced_limit_bytes: u64) -> BinarySizeMeasurement {
    let measurement_valid = bytes.is_some();
    let target_result = match bytes {
        Some(bytes) if bytes <= enforced_limit_bytes => "measured_within_target",
        Some(_) => "measured_miss",
        None => "invalid_or_incomplete_measurement",
    };
    let policy_result = match bytes {
        Some(bytes) if bytes <= BINARY_IDEAL_BYTES => "within_ideal",
        Some(bytes) if bytes <= BINARY_SOFT_CAP_BYTES => "within_soft_cap",
        Some(bytes) if bytes <= BINARY_HARD_CEILING_BYTES => "soft_cap_warning",
        Some(_) => "hard_ceiling_exceeded",
        None => "invalid_or_incomplete_measurement",
    };
    let hard_gate_passed = bytes.is_some_and(|bytes| bytes <= enforced_limit_bytes);
    let warning = bytes
        .filter(|bytes| *bytes > BINARY_SOFT_CAP_BYTES)
        .map(|bytes| {
            format!(
                "release binary is {bytes} bytes, above the {BINARY_SOFT_CAP_BYTES}-byte soft cap; the hard ceiling is {BINARY_HARD_CEILING_BYTES} bytes"
            )
        });
    BinarySizeMeasurement {
        id: "release_binary_size",
        bytes,
        ideal_bytes: BINARY_IDEAL_BYTES,
        soft_cap_bytes: BINARY_SOFT_CAP_BYTES,
        hard_ceiling_bytes: BINARY_HARD_CEILING_BYTES,
        enforced_limit_bytes,
        policy_result,
        target_result,
        measurement_valid,
        hard_gate_passed,
        release_gate_accepted: hard_gate_passed,
        warning,
        explanation: "Binary units are MiB (1 MiB = 1,048,576 bytes). At or below 5 MiB is ideal; more than 8 MiB emits a warning; more than 10 MiB fails the release gate.",
    }
}

fn binary_size_gate_failure(measurement: &BinarySizeMeasurement) -> Option<String> {
    if !measurement.measurement_valid {
        return Some("release binary size could not be measured".to_owned());
    }
    (!measurement.hard_gate_passed).then(|| {
        format!(
            "release binary exceeds the enforced {}-byte hard limit (project hard ceiling: {} bytes)",
            measurement.enforced_limit_bytes, measurement.hard_ceiling_bytes
        )
    })
}

fn timing_gate_failures(measurements: &[Measurement]) -> Vec<String> {
    measurements
        .iter()
        .filter(|measurement| measurement.includes_terminal_io)
        .filter(|measurement| measurement.target.target_result != "measured_within_target")
        .map(|measurement| format!("{}: {}", measurement.id, measurement.target.target_result))
        .collect()
}

fn semantic_highlight_proxy<'line>(
    catalog: &Catalog,
    line: &'line str,
) -> Vec<(HighlightClass, &'line str)> {
    let known = catalog.commands.iter().any(|command| {
        line == command.path
            || line.starts_with(&format!("{} ", command.path))
            || command
                .path
                .starts_with(line.split_whitespace().next().unwrap_or_default())
    });
    let mut first_word = true;
    split_preserving_whitespace(line)
        .into_iter()
        .map(|segment| {
            let class = if segment.trim().is_empty() {
                HighlightClass::Whitespace
            } else if first_word {
                first_word = false;
                if known {
                    HighlightClass::KnownCommand
                } else {
                    HighlightClass::UnknownCommand
                }
            } else if segment.starts_with('-') {
                HighlightClass::Option
            } else if segment.starts_with('"') || segment.starts_with('\'') {
                HighlightClass::Quoted
            } else {
                HighlightClass::Value
            };
            (class, segment)
        })
        .collect()
}

#[allow(
    clippy::string_slice,
    reason = "segment offsets are produced exclusively by char_indices"
)]
fn split_preserving_whitespace(input: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut whitespace = input.chars().next().is_some_and(char::is_whitespace);
    for (index, character) in input.char_indices() {
        if character.is_whitespace() != whitespace {
            segments.push(&input[start..index]);
            start = index;
            whitespace = !whitespace;
        }
    }
    if start < input.len() {
        segments.push(&input[start..]);
    }
    segments
}

fn timed(
    samples: usize,
    mut operation: impl FnMut() -> Result<(), Box<dyn Error>>,
) -> Result<Statistics, Box<dyn Error>> {
    if samples == 0 {
        return Err("sample counts must be greater than zero".into());
    }
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        operation()?;
        timings.push(start.elapsed());
    }
    statistics(timings).ok_or_else(|| "sample counts must be greater than zero".into())
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "the timing vector is proven non-empty and sorted before bounded percentile indexing and averaging"
)]
fn statistics(mut timings: Vec<Duration>) -> Option<Statistics> {
    if timings.is_empty() {
        return None;
    }
    timings.sort_unstable();
    Some(Statistics {
        samples: timings.len(),
        min_ms: millis(timings[0]),
        p50_ms: millis(nearest_rank(&timings, 50)),
        p95_ms: millis(nearest_rank(&timings, 95)),
        max_ms: millis(timings[timings.len() - 1]),
    })
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "callers pass a non-empty sorted sample and a percentile within zero through one hundred"
)]
fn nearest_rank(sorted: &[Duration], percentile: usize) -> Duration {
    let rank = percentile.saturating_mul(sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn lower_bound_assessment(
    measured: f64,
    limit: f64,
    specification_target: &'static str,
    measured_percentile: &'static str,
    explanation: &'static str,
) -> TargetAssessment {
    TargetAssessment {
        specification_target,
        measured_percentile,
        limit_ms: limit,
        target_result: if measured > limit {
            "definite_miss_lower_bound_exceeds_target"
        } else {
            "inconclusive_lower_bound_within_target"
        },
        measurement_valid: true,
        release_gate_accepted: false,
        explanation,
    }
}

fn proxy_assessment(
    measured: f64,
    limit: f64,
    specification_target: &'static str,
    measured_percentile: &'static str,
    explanation: &'static str,
) -> TargetAssessment {
    TargetAssessment {
        specification_target,
        measured_percentile,
        limit_ms: limit,
        target_result: if measured > limit {
            "proxy_miss"
        } else {
            "proxy_within_target"
        },
        measurement_valid: true,
        release_gate_accepted: false,
        explanation,
    }
}

fn actual_assessment(
    measured: Option<f64>,
    limit: f64,
    specification_target: &'static str,
    measured_percentile: &'static str,
    valid: bool,
    explanation: &'static str,
) -> TargetAssessment {
    TargetAssessment {
        specification_target,
        measured_percentile,
        limit_ms: limit,
        target_result: if !valid || measured.is_none() {
            "invalid_or_incomplete_measurement"
        } else if measured.is_some_and(|value| value > limit) {
            "measured_miss"
        } else {
            "measured_within_target"
        },
        measurement_valid: valid,
        // Acceptance records a valid end-to-end measurement. A numeric miss
        // remains a miss and must not be hidden by this gate status.
        release_gate_accepted: valid,
        explanation,
    }
}

fn sample_argument(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    let Some(value) = argument_value(name) else {
        return Ok(default);
    };
    let samples = value
        .parse::<usize>()
        .map_err(|error| format!("invalid {name} value `{value}`: {error}"))?;
    if samples == 0 {
        return Err(format!("{name} must be greater than zero").into());
    }
    Ok(samples)
}

fn default_pty_samples(enforce: bool) -> usize {
    if enforce {
        DEFAULT_RELEASE_PTY_SAMPLES
    } else {
        DEFAULT_PTY_SAMPLES
    }
}

fn byte_argument(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    let Some(value) = argument_value(name) else {
        return Ok(default);
    };
    let bytes = value
        .parse::<u64>()
        .map_err(|error| format!("invalid {name} value `{value}`: {error}"))?;
    if bytes == 0 {
        return Err(format!("{name} must be greater than zero").into());
    }
    Ok(bytes)
}

fn binary_limit_argument() -> Result<u64, Box<dyn Error>> {
    let bytes = byte_argument("--max-binary-bytes", BINARY_HARD_CEILING_BYTES)?;
    Ok(validate_binary_limit(bytes)?)
}

fn validate_binary_limit(bytes: u64) -> Result<u64, String> {
    if bytes > BINARY_HARD_CEILING_BYTES {
        return Err(format!(
            "--max-binary-bytes cannot exceed the project hard ceiling of {BINARY_HARD_CEILING_BYTES} bytes (10 MiB)"
        ));
    }
    Ok(bytes)
}

struct StagedArtifact {
    directory: PathBuf,
    path: PathBuf,
    sha256: String,
}

impl StagedArtifact {
    fn copy_from(source: &Path) -> Result<Self, Box<dyn Error>> {
        let directory = create_staging_directory()?;
        let filename = if cfg!(windows) {
            "quirl-staged.exe"
        } else {
            "quirl-staged"
        };
        let path = directory.join(filename);
        if let Err(error) = fs::copy(source, &path) {
            let _ = fs::remove_dir(&directory);
            return Err(format!(
                "could not stage Quirl artifact {}: {error}",
                source.display()
            )
            .into());
        }
        if let Err(error) = make_staged_artifact_read_only(&path) {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir(&directory);
            return Err(error);
        }
        let Some(sha256) = binary_sha256(&path) else {
            make_staged_artifact_writable_for_cleanup(&path);
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir(&directory);
            return Err(format!("could not hash staged Quirl artifact {}", path.display()).into());
        };
        Ok(Self {
            directory,
            path,
            sha256,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn sha256(&self) -> &str {
        &self.sha256
    }
}

impl Drop for StagedArtifact {
    fn drop(&mut self) {
        make_staged_artifact_writable_for_cleanup(&self.path);
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn create_staging_directory() -> Result<PathBuf, Box<dyn Error>> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    for attempt in 0..32_u8 {
        let directory = env::temp_dir().join(format!(
            "quirl-release-artifact-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&directory) {
            Ok(()) => {
                if let Err(error) = make_staging_directory_private(&directory) {
                    let _ = fs::remove_dir(&directory);
                    return Err(error);
                }
                return Ok(directory);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "could not create private artifact staging directory: {error}"
                )
                .into());
            }
        }
    }
    Err("could not allocate a unique artifact staging directory".into())
}

#[cfg(unix)]
fn make_staging_directory_private(path: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_staging_directory_private(_path: &Path) -> Result<(), Box<dyn Error>> {
    Ok(())
}

#[cfg(unix)]
fn make_staged_artifact_read_only(path: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o500))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_staged_artifact_read_only(path: &Path) -> Result<(), Box<dyn Error>> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(windows)]
fn make_staged_artifact_writable_for_cleanup(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(false);
        let _ = fs::set_permissions(path, permissions);
    }
}

#[cfg(not(windows))]
fn make_staged_artifact_writable_for_cleanup(_path: &Path) {}

fn expected_sha256_argument(enforce: bool) -> Result<Option<String>, Box<dyn Error>> {
    let value = argument_value("--expected-sha256");
    if enforce && value.is_none() {
        return Err(
            "the release gate requires `--expected-sha256 <64-hex-digit digest>` from a trusted, independently recorded artifact hash"
                .into(),
        );
    }
    value.map(|value| normalize_sha256(&value)).transpose()
}

fn normalize_sha256(value: &str) -> Result<String, Box<dyn Error>> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("--expected-sha256 must be exactly 64 hexadecimal digits".into());
    }
    Ok(value.to_ascii_lowercase())
}

fn verify_expected_sha256(expected: &str, actual: &str) -> Result<(), Box<dyn Error>> {
    if expected == actual {
        return Ok(());
    }
    Err(
        format!("staged Quirl artifact SHA-256 mismatch: expected {expected}, measured {actual}")
            .into(),
    )
}

fn quirl_binary() -> Result<PathBuf, Box<dyn Error>> {
    let path = argument_value("--quirl").map_or_else(
        || {
            env::current_exe().map(|executable| {
                executable
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(if cfg!(windows) { "quirl.exe" } else { "quirl" })
            })
        },
        |value| Ok(PathBuf::from(value)),
    )?;
    if !path.is_file() {
        return Err(format!(
            "Quirl CLI binary not found at {}; build it with `cargo build --release -p quirl-cli` or pass `--quirl <path>`",
            path.display()
        )
        .into());
    }
    Ok(path.canonicalize()?)
}

fn argument_value(name: &str) -> Option<String> {
    let mut arguments = env::args().skip(2);
    while let Some(argument) = arguments.next() {
        if argument == name {
            return arguments.next();
        }
    }
    None
}

fn discover_environment(
    quirl: &Path,
    build_info: Option<&QuirlBuildInfo>,
    artifact_digest_verified: bool,
    artifact_profile_verified: bool,
    harness_source_verified: bool,
) -> Environment {
    Environment {
        hostname: command_output("hostname", &[]).unwrap_or_else(|| "unknown".to_owned()),
        operating_system: operating_system(),
        architecture: env::consts::ARCH,
        cpu: cpu_name(),
        logical_cpus: std::thread::available_parallelism().map_or(1, usize::from),
        memory_bytes: memory_bytes(),
        rustc: command_output("rustc", &["--version", "--verbose"])
            .unwrap_or_else(|| "unknown".to_owned()),
        cargo: command_output("cargo", &["--version"]).unwrap_or_else(|| "unknown".to_owned()),
        source_commit: command_output("git", &["rev-parse", "HEAD"])
            .unwrap_or_else(|| "unknown".to_owned()),
        source_dirty: source_dirty(),
        artifact_digest_verified,
        artifact_profile_verified,
        artifact_source_verified: false,
        harness_source_verified,
        build_profile: build_info
            .map(|info| info.build_profile.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
        optimization_level: build_info
            .map(|info| info.optimization_level.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
        panic_strategy: build_info
            .map(|info| info.panic_strategy.clone())
            .unwrap_or_else(|| "unknown".to_owned()),
        quirl_binary: quirl.display().to_string(),
        quirl_binary_bytes: fs::metadata(quirl).ok().map(|metadata| metadata.len()),
        quirl_binary_sha256: binary_sha256(quirl),
        quirl_version: build_info
            .map(|info| format!("quirl {}", info.version))
            .unwrap_or_else(|| "unknown".to_owned()),
    }
}

fn source_dirty() -> Option<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()
        .ok()?;
    output.status.success().then_some(!output.stdout.is_empty())
}

fn quirl_build_info(quirl: &Path) -> Option<QuirlBuildInfo> {
    let output = Command::new(quirl).arg("--build-info").output().ok()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return None;
    }
    let info: QuirlBuildInfo = serde_json::from_slice(&output.stdout).ok()?;
    build_info_contract_is_current(&info).then_some(info)
}

fn build_info_contract_is_current(info: &QuirlBuildInfo) -> bool {
    let timestamp_is_valid = info.build_timestamp.parse::<u64>().is_ok();
    let official_release_is_clean = !info.official_release || info.source_dirty == Some(false);
    info.schema_version == 3 && timestamp_is_valid && official_release_is_clean
}

fn build_info_matches_benchmark(info: &QuirlBuildInfo) -> bool {
    let expected_profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let expected_panic = if cfg!(panic = "unwind") {
        "unwind"
    } else {
        "abort"
    };
    let expected_optimization = if cfg!(debug_assertions) { "0" } else { "z" };
    info.build_profile == expected_profile
        && info.optimization_level == expected_optimization
        && info.panic_strategy == expected_panic
        && info.operating_system == env::consts::OS
        && info.architecture == env::consts::ARCH
}

fn build_info_matches_harness_source(info: &QuirlBuildInfo) -> bool {
    let harness_commit = env!("QUIRL_BUILD_COMMIT");
    harness_commit != "unknown"
        && env!("QUIRL_BUILD_DIRTY") == "false"
        && info.source_commit == harness_commit
        && info.source_dirty == Some(false)
}

#[allow(
    clippy::indexing_slicing,
    reason = "the checksum parser first validates the required digest token"
)]
fn binary_sha256(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Some(format!("{:x}", digest.finalize()))
}

fn measured_at_utc() -> String {
    command_output("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs());
        format!("unix:{seconds}")
    })
}

fn operating_system() -> String {
    if env::consts::OS == "macos" {
        let version =
            command_output("sw_vers", &["-productVersion"]).unwrap_or_else(|| "unknown".to_owned());
        let build =
            command_output("sw_vers", &["-buildVersion"]).unwrap_or_else(|| "unknown".to_owned());
        format!("macOS {version} ({build})")
    } else if env::consts::OS == "linux" {
        fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|source| {
                source.lines().find_map(|line| {
                    line.strip_prefix("PRETTY_NAME=")
                        .map(|value| value.trim_matches('"').to_owned())
                })
            })
            .unwrap_or_else(|| "Linux (distribution unknown)".to_owned())
    } else {
        env::consts::OS.to_owned()
    }
}

fn cpu_name() -> String {
    if env::consts::OS == "macos" {
        command_output("sysctl", &["-n", "machdep.cpu.brand_string"])
            .unwrap_or_else(|| "unknown".to_owned())
    } else if env::consts::OS == "linux" {
        fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|source| {
                source.lines().find_map(|line| {
                    line.split_once(':')
                        .filter(|(key, _)| matches!(key.trim(), "model name" | "Hardware"))
                        .map(|(_, value)| value.trim().to_owned())
                })
            })
            .unwrap_or_else(|| "unknown".to_owned())
    } else {
        env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".to_owned())
    }
}

fn memory_bytes() -> Option<u64> {
    if env::consts::OS == "macos" {
        command_output("sysctl", &["-n", "hw.memsize"])?
            .parse()
            .ok()
    } else if env::consts::OS == "linux" {
        let source = fs::read_to_string("/proc/meminfo").ok()?;
        let kibibytes = source
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        kibibytes.checked_mul(1024)
    } else {
        None
    }
}

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn print_text(report: &PreviewReport) {
    println!("Quirl 1.0 release performance gate\n");
    println!(
        "{} · {} · {} · {} (opt={})",
        report.environment.operating_system,
        report.environment.architecture,
        report.environment.cpu,
        report.environment.build_profile,
        report.environment.optimization_level
    );
    println!(
        "source {}{} · panic={} · binary sha256={}",
        report.environment.source_commit,
        match report.environment.source_dirty {
            Some(true) => " (dirty)",
            Some(false) => "",
            None => " (dirty=unknown)",
        },
        report.environment.panic_strategy,
        report
            .environment
            .quirl_binary_sha256
            .as_deref()
            .unwrap_or("unknown")
    );
    println!(
        "{:<42} {:>10} {:>10} {:>10}",
        "measurement", "P50 ms", "P95 ms", "samples"
    );
    for measurement in &report.measurements {
        println!(
            "{:<42} {:>10} {:>10} {:>5}/{:<4}",
            measurement.id,
            format_millis(measurement.p50_ms),
            format_millis(measurement.p95_ms),
            measurement.successful_samples,
            measurement.samples
        );
        println!(
            "  target: {} ({})",
            measurement.target.specification_target, measurement.target.target_result
        );
    }
    println!(
        "{:<42} {:>10} {:>10}",
        report.stream_window.id,
        if report.stream_window.invariant_valid {
            "bounded"
        } else {
            "invalid"
        },
        format!("{} input", report.stream_window.input_samples_per_capacity)
    );
    println!(
        "{:<42} {:>10} {:>10}",
        report.binary_size.id,
        report
            .binary_size
            .bytes
            .map_or_else(|| "n/a".to_owned(), |bytes| bytes.to_string()),
        report.binary_size.policy_result
    );
    for warning in &report.warnings {
        println!("  warning: {warning}");
    }
    for failure in &report.gate_failures {
        println!("  failure: {failure}");
    }
    println!("\nRelease gate: {}.", report.release_gate_status);
}

fn format_millis(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.3}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_uses_sorted_sample_distribution() {
        let samples = (1..=100).map(Duration::from_millis).collect::<Vec<_>>();
        assert_eq!(nearest_rank(&samples, 50), Duration::from_millis(50));
        assert_eq!(nearest_rank(&samples, 95), Duration::from_millis(95));
    }

    #[test]
    fn semantic_proxy_classifies_command_option_and_value_segments() {
        let segments = semantic_highlight_proxy(&Catalog::builtin(), "git commit --amend now");
        assert!(
            segments.iter().any(
                |(class, value)| matches!(class, HighlightClass::KnownCommand) && *value == "git"
            )
        );
        assert!(
            segments.iter().any(
                |(class, value)| matches!(class, HighlightClass::Option) && *value == "--amend"
            )
        );
        assert!(
            segments
                .iter()
                .any(|(class, value)| matches!(class, HighlightClass::Value) && *value == "now")
        );
    }

    #[test]
    fn headless_proxies_never_accept_release_gates() {
        assert!(!proxy_assessment(1.0, 8.0, "target", "P95", "proxy").release_gate_accepted);
        assert!(
            !lower_bound_assessment(30.0, 25.0, "target", "P50", "lower bound")
                .release_gate_accepted
        );
    }

    #[test]
    fn cursor_query_detection_handles_standard_and_private_forms() {
        assert_eq!(cursor_query_count(b"before\x1b[6nafter\x1b[?6n", 0), 2);
        assert_eq!(cursor_query_count(b"\x1b[6", 0), 0);
        assert_eq!(cursor_query_count(b"\x1b[6n", 3), 1);
        assert_eq!(cursor_query_count(b"\x1b[6nmore", 4), 0);
    }

    #[test]
    fn actual_gate_accepts_valid_measurement_while_preserving_numeric_miss() {
        let assessment = actual_assessment(Some(30.0), 25.0, "target", "P50", true, "actual");
        assert_eq!(assessment.target_result, "measured_miss");
        assert!(assessment.release_gate_accepted);
        assert!(assessment.measurement_valid);
    }

    #[test]
    fn production_live_buffer_retention_is_bounded_at_every_supported_scale() {
        let evidence = measure_stream_window(257).unwrap();
        assert!(evidence.invariant_valid);
        assert!(evidence.release_gate_accepted);
        assert_eq!(evidence.capacities[0].retained_samples, 1);
        assert_eq!(evidence.capacities[1].retained_samples, 16);
        assert_eq!(evidence.capacities[2].retained_samples, 256);
        assert_eq!(evidence.capacities[2].dropped_samples, 1);
        assert!(
            evidence.capacities[0].serialized_snapshot_bytes
                < evidence.capacities[1].serialized_snapshot_bytes
        );
        assert!(
            evidence.capacities[1].serialized_snapshot_bytes
                < evidence.capacities[2].serialized_snapshot_bytes
        );
    }

    #[test]
    fn missing_release_binary_never_accepts_size_evidence() {
        let evidence = measure_binary_size(
            Path::new("/definitely/missing/quirl-release-binary"),
            BINARY_HARD_CEILING_BYTES,
        );
        assert_eq!(evidence.target_result, "invalid_or_incomplete_measurement");
        assert!(!evidence.measurement_valid);
        assert!(!evidence.release_gate_accepted);
    }

    #[test]
    fn binary_size_policy_enforces_every_exact_boundary() {
        let ideal = binary_size_measurement(Some(BINARY_IDEAL_BYTES), BINARY_HARD_CEILING_BYTES);
        assert_eq!(ideal.policy_result, "within_ideal");
        assert!(ideal.warning.is_none());
        assert!(ideal.hard_gate_passed);

        let above_ideal =
            binary_size_measurement(Some(BINARY_IDEAL_BYTES + 1), BINARY_HARD_CEILING_BYTES);
        assert_eq!(above_ideal.policy_result, "within_soft_cap");
        assert!(above_ideal.warning.is_none());
        assert!(above_ideal.hard_gate_passed);

        let soft_cap =
            binary_size_measurement(Some(BINARY_SOFT_CAP_BYTES), BINARY_HARD_CEILING_BYTES);
        assert_eq!(soft_cap.policy_result, "within_soft_cap");
        assert!(soft_cap.warning.is_none());

        let warning =
            binary_size_measurement(Some(BINARY_SOFT_CAP_BYTES + 1), BINARY_HARD_CEILING_BYTES);
        assert_eq!(warning.policy_result, "soft_cap_warning");
        assert!(warning.warning.is_some());
        assert!(warning.hard_gate_passed);
        assert!(warning.release_gate_accepted);
        assert!(binary_size_gate_failure(&warning).is_none());

        let hard_ceiling =
            binary_size_measurement(Some(BINARY_HARD_CEILING_BYTES), BINARY_HARD_CEILING_BYTES);
        assert_eq!(hard_ceiling.policy_result, "soft_cap_warning");
        assert!(hard_ceiling.hard_gate_passed);

        let rejected = binary_size_measurement(
            Some(BINARY_HARD_CEILING_BYTES + 1),
            BINARY_HARD_CEILING_BYTES,
        );
        assert_eq!(rejected.policy_result, "hard_ceiling_exceeded");
        assert_eq!(rejected.target_result, "measured_miss");
        assert!(!rejected.hard_gate_passed);
        assert!(!rejected.release_gate_accepted);
        assert!(
            binary_size_gate_failure(&rejected)
                .is_some_and(|failure| failure.contains("hard limit"))
        );
    }

    #[test]
    fn stricter_binary_limit_cannot_be_reported_as_a_gate_pass() {
        let stricter_limit = BINARY_IDEAL_BYTES;
        let evidence = binary_size_measurement(Some(stricter_limit + 1), stricter_limit);
        assert_eq!(evidence.policy_result, "within_soft_cap");
        assert_eq!(evidence.target_result, "measured_miss");
        assert!(!evidence.hard_gate_passed);
        assert!(!evidence.release_gate_accepted);
    }

    #[test]
    fn binary_limit_override_can_only_tighten_the_hard_ceiling() {
        assert_eq!(
            validate_binary_limit(BINARY_HARD_CEILING_BYTES),
            Ok(BINARY_HARD_CEILING_BYTES)
        );
        let error = validate_binary_limit(BINARY_HARD_CEILING_BYTES + 1).unwrap_err();
        assert!(error.contains("cannot exceed"));
        assert!(error.contains("10 MiB"));
    }

    #[test]
    fn artifact_identity_hashes_the_exact_measured_binary() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let hash = binary_sha256(&fixture).unwrap();

        assert_eq!(hash.len(), 64);
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(binary_sha256(Path::new("/definitely/missing/quirl")).is_none());
    }

    #[test]
    fn release_digest_must_be_independent_and_exact() {
        let digest = "A".repeat(64);
        let normalized = normalize_sha256(&digest).unwrap();
        assert_eq!(normalized, "a".repeat(64));
        assert!(verify_expected_sha256(&normalized, &normalized).is_ok());
        assert!(verify_expected_sha256(&normalized, &"b".repeat(64)).is_err());
        assert!(normalize_sha256("self-reported").is_err());
        assert!(normalize_sha256(&"g".repeat(64)).is_err());
    }

    #[test]
    fn staged_artifact_isolated_from_source_mutation_and_cleaned_up() {
        let source_directory = create_staging_directory().unwrap();
        let source = source_directory.join("source");
        fs::write(&source, b"reviewed artifact").unwrap();
        let source_hash = binary_sha256(&source).unwrap();
        let (staged_path, staged_directory) = {
            let staged = StagedArtifact::copy_from(&source).unwrap();
            let staged_path = staged.path.clone();
            let staged_directory = staged.directory.clone();
            assert_eq!(staged.sha256(), source_hash);
            fs::write(&source, b"mutated after staging").unwrap();
            assert_eq!(
                binary_sha256(staged.path()).as_deref(),
                Some(source_hash.as_str())
            );
            (staged_path, staged_directory)
        };
        assert!(!staged_path.exists());
        assert!(!staged_directory.exists());
        fs::remove_file(source).unwrap();
        fs::remove_dir(source_directory).unwrap();
    }

    #[test]
    fn artifact_metadata_requires_matching_binary_report() {
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let panic_strategy = if cfg!(panic = "unwind") {
            "unwind"
        } else {
            "abort"
        };
        let matching = QuirlBuildInfo {
            schema_version: 3,
            version: "0.1.0".to_owned(),
            build_profile: profile.to_owned(),
            optimization_level: if cfg!(debug_assertions) { "0" } else { "z" }.to_owned(),
            panic_strategy: panic_strategy.to_owned(),
            operating_system: env::consts::OS.to_owned(),
            architecture: env::consts::ARCH.to_owned(),
            source_commit: "abc123".to_owned(),
            build_timestamp: "1".to_owned(),
            official_release: false,
            source_dirty: Some(false),
        };

        assert!(build_info_contract_is_current(&matching));
        assert!(build_info_matches_benchmark(&matching));

        let legacy = QuirlBuildInfo {
            schema_version: 2,
            ..matching.clone()
        };
        assert!(!build_info_contract_is_current(&legacy));

        let invalid_timestamp = QuirlBuildInfo {
            build_timestamp: "not-a-timestamp".to_owned(),
            ..matching.clone()
        };
        assert!(!build_info_contract_is_current(&invalid_timestamp));

        let dirty_official_release = QuirlBuildInfo {
            official_release: true,
            source_dirty: Some(true),
            ..matching.clone()
        };
        assert!(!build_info_contract_is_current(&dirty_official_release));

        let mismatched = QuirlBuildInfo {
            build_profile: "other".to_owned(),
            ..matching
        };
        assert!(!build_info_matches_benchmark(&mismatched));
    }

    #[test]
    fn editable_frame_uses_stable_prompt_mode_and_identity_contracts() {
        let info = QuirlBuildInfo {
            schema_version: 3,
            version: "0.1.0".to_owned(),
            build_profile: "release".to_owned(),
            optimization_level: "z".to_owned(),
            panic_strategy: "unwind".to_owned(),
            operating_system: env::consts::OS.to_owned(),
            architecture: env::consts::ARCH.to_owned(),
            source_commit: "abcdef0123456789".to_owned(),
            build_timestamp: "1787388008".to_owned(),
            official_release: false,
            source_dirty: Some(false),
        };
        let identity = rich_status_identity(&info);
        let screen = format!(
            "welcome\n❯ \n\n NORMAL  │ AI discovery is unavailable                 {identity}"
        );

        assert_eq!(identity, "🌀 dev@1787388008+abcdef0");
        assert!(editable_command_frame(&screen, Some(&identity)));
        assert!(!editable_command_frame(
            &screen.replace("❯ ", ""),
            Some(&identity)
        ));
        assert!(!editable_command_frame(
            &screen.replace(" NORMAL ", " DATA "),
            Some(&identity)
        ));
        assert!(!editable_command_frame(&screen, Some("🌀 v0.1.0")));

        let official = QuirlBuildInfo {
            official_release: true,
            ..info
        };
        assert_eq!(rich_status_identity(&official), "🌀 v0.1.0");
    }

    #[test]
    fn pty_fixture_paths_are_canonical_before_database_admission() {
        let fixture = PtyFixture::create().unwrap();

        assert_eq!(fixture.root, fs::canonicalize(&fixture.root).unwrap());
        assert_eq!(fixture.index.parent(), Some(fixture.root.as_path()));
        assert_eq!(fixture.config_dir.parent(), Some(fixture.root.as_path()));
    }

    #[test]
    fn release_harness_requires_the_same_clean_source_as_the_binary() {
        let matching = QuirlBuildInfo {
            schema_version: 3,
            version: "0.1.0".to_owned(),
            build_profile: "release".to_owned(),
            optimization_level: "z".to_owned(),
            panic_strategy: "unwind".to_owned(),
            operating_system: env::consts::OS.to_owned(),
            architecture: env::consts::ARCH.to_owned(),
            source_commit: env!("QUIRL_BUILD_COMMIT").to_owned(),
            build_timestamp: "1".to_owned(),
            official_release: false,
            source_dirty: Some(false),
        };

        assert_eq!(
            build_info_matches_harness_source(&matching),
            env!("QUIRL_BUILD_COMMIT") != "unknown" && env!("QUIRL_BUILD_DIRTY") == "false"
        );

        let stale = QuirlBuildInfo {
            source_commit: "different-commit".to_owned(),
            ..matching
        };
        assert!(!build_info_matches_harness_source(&stale));
    }

    #[test]
    fn enforcing_release_uses_more_independent_pty_samples_than_preview() {
        let preview_samples = default_pty_samples(false);
        let release_samples = default_pty_samples(true);

        assert_eq!(preview_samples, DEFAULT_PTY_SAMPLES);
        assert_eq!(release_samples, DEFAULT_RELEASE_PTY_SAMPLES);
        assert!(release_samples > preview_samples);
    }
}
