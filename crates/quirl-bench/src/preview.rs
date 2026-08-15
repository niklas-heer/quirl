use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};
use quirl_catalog::Catalog;
use quirl_lua::QuirlConfig;
use quirl_syntax::Mode;
use quirl_ui::{CatalogCompleter, QuirlPrompt};
use reedline::{Completer, Prompt, PromptEditMode};
use serde::Serialize;
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
const DEFAULT_PTY_TIMEOUT_MS: usize = 2_000;
const MINIMUM_ACCEPTED_PTY_SAMPLES: usize = 20;
const COLD_START_TARGET_MS: f64 = 25.0;
const EDIT_FRAME_TARGET_MS: f64 = 8.0;
const FIRST_PROMPT_TARGET_MS: f64 = 16.0;

#[derive(Debug, Serialize)]
struct PreviewReport {
    schema_version: u32,
    suite: &'static str,
    measured_at_utc: String,
    environment: Environment,
    methodology: Methodology,
    measurements: Vec<Measurement>,
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
    build_profile: &'static str,
    quirl_binary: String,
    quirl_binary_bytes: Option<u64>,
    quirl_version: String,
}

#[derive(Debug, Serialize)]
struct Methodology {
    percentile_method: &'static str,
    pty_end_to_end: &'static str,
    cold_start: &'static str,
    headless_edit_frame: &'static str,
    first_prompt: &'static str,
    limitations: Vec<&'static str>,
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

pub fn run() -> Result<(), Box<dyn Error>> {
    let quirl = quirl_binary()?;
    let pty_samples = sample_argument("--pty-samples", DEFAULT_PTY_SAMPLES)?;
    let pty_timeout_ms = sample_argument("--pty-timeout-ms", DEFAULT_PTY_TIMEOUT_MS)?;
    let cold_samples = sample_argument("--cold-samples", DEFAULT_COLD_SAMPLES)?;
    let edit_samples = sample_argument("--edit-samples", DEFAULT_EDIT_SAMPLES)?;
    let prompt_samples = sample_argument("--prompt-samples", DEFAULT_PROMPT_SAMPLES)?;

    let pty = measure_pty(
        &quirl,
        pty_samples,
        Duration::from_millis(pty_timeout_ms as u64),
    );
    let cold = measure_cli_startup(&quirl, cold_samples)?;
    let edit = measure_headless_edit_frame(edit_samples)?;
    let prompt = measure_first_prompt(prompt_samples)?;
    let pty_valid = pty.requested_samples >= MINIMUM_ACCEPTED_PTY_SAMPLES
        && pty.successful_samples == pty.requested_samples
        && pty.prompt_paint.is_some()
        && pty.cold_to_editable.is_some()
        && pty.keystroke_to_frame.is_some()
        && !cfg!(debug_assertions);
    let measurements = vec![
        pty_measurement(
            PtyMeasurementSpec {
                id: "pty_cold_to_editable",
                label: "process start to first prompt accepting and painting unique input",
                specification_target: "cold start to editable prompt P50 <=25 ms",
                measured_percentile: "P50",
                limit_ms: COLD_START_TARGET_MS,
                explanation: "A PTY terminal model observed the prompt and a unique injected marker in the editable buffer.",
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
                specification_target: "first prompt paint <=16 ms",
                measured_percentile: "P95 (conservative; specification does not assign a percentile)",
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
                "first prompt paint <=16 ms",
                "P95 (stricter diagnostic proxy; specification does not assign a percentile)",
                "Measures prompt construction and render methods only; it does not measure terminal paint or time to an editable input loop.",
            ),
        },
    ];

    let report = PreviewReport {
        schema_version: 1,
        suite: "quirl_preview_v0.1_performance",
        measured_at_utc: measured_at_utc(),
        environment: discover_environment(&quirl),
        methodology: Methodology {
            percentile_method: "nearest-rank over independently timed wall-clock samples",
            pty_end_to_end: "Each sample opens a fresh 120x40 pseudo-terminal, starts the release Quirl process, answers terminal cursor-position requests, and feeds output into a VT100 terminal model. It records process start to the first prompt frame, validates editability with a unique marker, then records a final representative keystroke until the expected edited buffer is present in the terminal frame.",
            cold_start: "Starts a new Quirl process for every sample and waits for `quirl --version` to exit. This measures process creation, dynamic loading, and CLI argument parsing, not cold-to-editable startup. OS filesystem caches are not flushed.",
            headless_edit_frame: "Calls Quirl's real CatalogCompleter and Prompt render methods, plus a benchmark-owned equivalent of the current semantic token classification, for `git commit --am`. No Reedline layout or terminal I/O occurs.",
            first_prompt: "Constructs a fresh configured QuirlPrompt and renders left, right, and indicator strings for every sample. Filesystem metadata may be served from OS cache. No terminal I/O occurs.",
            limitations: vec![
                "A completed frame means the expected screen state was reconstructed from the PTY byte stream; physical terminal-emulator scheduling, GPU composition, and monitor scanout are not measured.",
                "The UI highlighter is private; the edit proxy reproduces its command/option/quote classification but not StyledText allocation or rendering.",
                "The benchmark does not control CPU frequency, other machine load, thermal state, or OS filesystem caches.",
                "Results from debug builds are emitted but are not suitable for release decisions; run with `cargo run --release`.",
            ],
        },
        measurements,
        release_gate_status: if pty_valid {
            "accepted_end_to_end_measurements_valid_targets_recorded".to_owned()
        } else {
            "not_accepted_pty_measurements_incomplete".to_owned()
        },
    };

    if env::args().any(|argument| argument == "--json") {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(&report);
    }
    Ok(())
}

fn measure_pty(path: &Path, samples: usize, timeout: Duration) -> PtyStatistics {
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
        match measure_pty_sample(path, &fixture, timeout, sample) {
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

fn measure_pty_sample(
    path: &Path,
    fixture: &PtyFixture,
    timeout: Duration,
    sample: usize,
) -> Result<PtySample, Box<dyn Error>> {
    let started = Instant::now();
    let mut session = PtySession::spawn(path, fixture)?;
    let result = (|| {
        session.wait_for_screen("❯", timeout, "first command prompt")?;
        let prompt_paint = started.elapsed();

        let marker = format!("qrlready{sample:04}");
        session.send(marker.as_bytes())?;
        session.wait_for_screen(&marker, timeout, "editable prompt marker")?;
        let cold_to_editable = started.elapsed();

        session.send(b"\x15")?;
        let baseline = "git commit --amen";
        session.send(baseline.as_bytes())?;
        session.wait_for_screen(baseline, timeout, "representative edit baseline")?;
        let edit_started = Instant::now();
        session.send(b"d")?;
        session.wait_for_screen("git commit --amend", timeout, "edited terminal frame")?;

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

        let (sender, receiver) = mpsc::channel();
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
        if let Some(handle) = self.reader_thread.take() {
            if handle.is_finished() {
                let _ = handle.join();
            }
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

fn discover_environment(quirl: &Path) -> Environment {
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
        build_profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        quirl_binary: quirl.display().to_string(),
        quirl_binary_bytes: fs::metadata(quirl).ok().map(|metadata| metadata.len()),
        quirl_version: Command::new(quirl)
            .arg("--version")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_else(|| "unknown".to_owned()),
    }
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
    println!("Quirl Preview v0.1 performance probes\n");
    println!(
        "{} · {} · {} · {}",
        report.environment.operating_system,
        report.environment.architecture,
        report.environment.cpu,
        report.environment.build_profile
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
        assert!(segments
            .iter()
            .any(
                |(class, value)| matches!(class, HighlightClass::KnownCommand) && *value == "git"
            ));
        assert!(segments
            .iter()
            .any(|(class, value)| matches!(class, HighlightClass::Option) && *value == "--amend"));
        assert!(segments
            .iter()
            .any(|(class, value)| matches!(class, HighlightClass::Value) && *value == "now"));
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
}
