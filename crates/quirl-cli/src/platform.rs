use crate::extensions::LuaExtensionHost;
use clap::{Args, Subcommand, ValueEnum};
use quirl_core::{
    directory_entries, ContributionKind, ErrorCode, EventKind, ExtensionCapability, ExtensionEvent,
    ShellError, EXTENSION_PROTOCOL_VERSION,
};
use quirl_data::DataRuntime;
use quirl_ui::{directory_panel, process_panel, LiveBuffer, LiveSample, ProcessPanelRow};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

const MAX_PROCESS_ROWS: usize = 512;
const MAX_PROCESS_OUTPUT_BYTES: u64 = 256 * 1024;
const CANCELLATION_POLL_MS: u64 = 25;

#[derive(Debug, Subcommand)]
pub enum EventsCommand {
    /// Print the installed immutable event/action/contribution protocol.
    Schema {
        #[arg(long, value_enum, default_value_t = PlatformOutputFormat::Text)]
        format: PlatformOutputFormat,
    },
    /// Validate a versioned event trace without invoking extensions.
    Validate {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = PlatformOutputFormat::Text)]
        format: PlatformOutputFormat,
    },
}

#[derive(Debug, Subcommand)]
pub enum ViewCommand {
    /// Render a directory as an escape-safe typed panel or JSON model.
    Directory {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        all: bool,
        #[arg(long, value_enum, default_value_t = PlatformOutputFormat::Text)]
        format: PlatformOutputFormat,
    },
    /// Render the process table as an escape-safe typed panel or JSON model.
    Processes {
        #[arg(long, value_enum, default_value_t = PlatformOutputFormat::Text)]
        format: PlatformOutputFormat,
    },
    /// Render one enabled plugin's typed panel contribution.
    Panel {
        name: String,
        #[arg(long, value_enum, default_value_t = PlatformOutputFormat::Text)]
        format: PlatformOutputFormat,
    },
}

#[derive(Debug, Args)]
pub struct WatchCommand {
    /// Native typed data expression evaluated for each sample.
    pub expression: String,
    /// Maximum number of samples before a deterministic stop.
    #[arg(long, default_value_t = 10)]
    pub samples: usize,
    /// Milliseconds between samples; zero is useful for scripts and tests.
    #[arg(long, default_value_t = 1_000)]
    pub interval_ms: u64,
    /// Retained sample bound; older completed samples are dropped from history.
    #[arg(long, default_value_t = 32)]
    pub capacity: usize,
    #[arg(long, value_enum, default_value_t = PlatformOutputFormat::Text)]
    pub format: PlatformOutputFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum PlatformOutputFormat {
    Text,
    Json,
}

pub fn events_wants_json(command: &EventsCommand) -> bool {
    matches!(
        command,
        EventsCommand::Schema {
            format: PlatformOutputFormat::Json
        } | EventsCommand::Validate {
            format: PlatformOutputFormat::Json,
            ..
        }
    )
}

pub fn view_wants_json(command: &ViewCommand) -> bool {
    matches!(
        command,
        ViewCommand::Directory {
            format: PlatformOutputFormat::Json,
            ..
        } | ViewCommand::Processes {
            format: PlatformOutputFormat::Json
        } | ViewCommand::Panel {
            format: PlatformOutputFormat::Json,
            ..
        }
    )
}

pub fn execute_events(command: EventsCommand) -> Result<i32, ShellError> {
    match command {
        EventsCommand::Schema { format } => {
            let schema = ProtocolSchema {
                document_type: "quirl.extension.protocol",
                schema_version: EXTENSION_PROTOCOL_VERSION,
                events: EventKind::ALL.to_vec(),
                capabilities: ExtensionCapability::ALL.to_vec(),
                contributions: ContributionKind::ALL.to_vec(),
            };
            match format {
                PlatformOutputFormat::Json => print_json(&schema)?,
                PlatformOutputFormat::Text => {
                    println!("Quirl extension protocol v{}", schema.schema_version);
                    println!("events: {}", names(&schema.events)?);
                    println!("capabilities: {}", names(&schema.capabilities)?);
                    println!("contributions: {}", names(&schema.contributions)?);
                }
            }
            Ok(0)
        }
        EventsCommand::Validate { file, format } => {
            let trace = read_trace(&file)?;
            validate_trace(&trace)?;
            match format {
                PlatformOutputFormat::Json => print_json(&TraceValidation {
                    document_type: "quirl.extension.trace.validation",
                    schema_version: EXTENSION_PROTOCOL_VERSION,
                    valid: true,
                    events: trace.events.len(),
                })?,
                PlatformOutputFormat::Text => println!(
                    "valid extension trace: {} strictly ordered events",
                    trace.events.len()
                ),
            }
            Ok(0)
        }
    }
}

pub fn execute_view(command: ViewCommand) -> Result<i32, ShellError> {
    let (panel, format) = match command {
        ViewCommand::Directory { path, all, format } => {
            let entries = directory_entries(&path, all)?;
            (
                directory_panel(&path.display().to_string(), &entries)?,
                format,
            )
        }
        ViewCommand::Processes { format } => {
            let output = read_process_table()?;
            let rows = parse_process_rows(&output);
            (process_panel(&rows)?, format)
        }
        ViewCommand::Panel { name, format } => {
            let mut extensions = LuaExtensionHost::discover();
            let panel = extensions.render_panel_contribution(
                &name,
                &serde_json::json!({"cwd": std::env::current_dir().unwrap_or_default()}),
            )?;
            (panel, format)
        }
    };
    match format {
        PlatformOutputFormat::Json => print_json(&panel)?,
        PlatformOutputFormat::Text => print!("{}", panel.render_plain()),
    }
    Ok(0)
}

pub fn execute_watch(command: WatchCommand) -> Result<i32, ShellError> {
    if !(1..=1_000).contains(&command.samples) {
        return Err(platform_error("--samples must be between 1 and 1000"));
    }
    if command.interval_ms > 60_000 {
        return Err(platform_error("--interval-ms must be at most 60000"));
    }
    let mut buffer = LiveBuffer::new(command.capacity)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&cancelled))
        .map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "could not install watch cancellation handler",
        )
        .with_context(error.to_string())
        .with_help("Retry the watch from an interactive terminal")
    })?;
    let result = (|| {
        let runtime = DataRuntime::new();
        for sequence in 1..=command.samples as u64 {
            if cancelled.load(Ordering::Relaxed) {
                buffer.cancel();
                break;
            }
            let value = runtime.eval_with_cancellation(&command.expression, &cancelled)?;
            if cancelled.load(Ordering::Relaxed) {
                buffer.cancel();
                break;
            }
            if matches!(command.format, PlatformOutputFormat::Text) {
                println!(
                    "[{sequence}] {}",
                    serde_json::to_string(&value).map_err(json_error)?
                );
            }
            if !buffer.push(LiveSample { sequence, value }) {
                break;
            }
            if sequence < command.samples as u64
                && command.interval_ms > 0
                && wait_interruptibly(command.interval_ms, &cancelled)
            {
                buffer.cancel();
                break;
            }
        }
        let snapshot = buffer.snapshot();
        match command.format {
            PlatformOutputFormat::Json => print_json(&snapshot)?,
            PlatformOutputFormat::Text if snapshot.cancelled => println!("watch cancelled"),
            PlatformOutputFormat::Text if snapshot.dropped > 0 => println!(
                "retained {} samples; dropped {} older completed samples from history",
                snapshot.samples.len(),
                snapshot.dropped
            ),
            PlatformOutputFormat::Text => {}
        }
        Ok(0)
    })();
    signal_hook::low_level::unregister(signal);
    result
}

#[derive(Debug, Serialize)]
struct ProtocolSchema {
    document_type: &'static str,
    schema_version: u32,
    events: Vec<EventKind>,
    capabilities: Vec<ExtensionCapability>,
    contributions: Vec<ContributionKind>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventTrace {
    document_type: String,
    schema_version: u32,
    events: Vec<ExtensionEvent>,
}

#[derive(Debug, Serialize)]
struct TraceValidation {
    document_type: &'static str,
    schema_version: u32,
    valid: bool,
    events: usize,
}

fn read_trace(path: &PathBuf) -> Result<EventTrace, ShellError> {
    let source = fs::read_to_string(path).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not read extension trace {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Pass a readable versioned JSON event trace")
    })?;
    serde_json::from_str(&source).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("{} is not a valid extension trace", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Generate the schema with `quirl events schema --format json`")
    })
}

fn validate_trace(trace: &EventTrace) -> Result<(), ShellError> {
    if trace.document_type != "quirl.extension.trace"
        || trace.schema_version != EXTENSION_PROTOCOL_VERSION
    {
        return Err(platform_error("extension trace header is unsupported"));
    }
    let mut previous = None;
    for event in &trace.events {
        event.validate_after(previous)?;
        previous = Some(event.sequence);
    }
    Ok(())
}

fn parse_process_rows(source: &str) -> Vec<ProcessPanelRow> {
    #[cfg(windows)]
    return source
        .lines()
        .filter_map(parse_windows_process_row)
        .take(MAX_PROCESS_ROWS)
        .collect();

    #[cfg(not(windows))]
    source
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent_pid = fields.next()?.parse().ok();
            let state = fields.next()?.to_owned();
            let command = fields.collect::<Vec<_>>().join(" ");
            Some(ProcessPanelRow {
                pid,
                parent_pid,
                state,
                command,
            })
        })
        .take(MAX_PROCESS_ROWS)
        .collect()
}

fn read_process_table() -> Result<String, ShellError> {
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("tasklist");
        command.args(["/FO", "CSV", "/NH"]);
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("ps");
        command.args(["-eo", "pid=,ppid=,stat=,comm="]);
        command
    };
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        ShellError::new(
            ErrorCode::ProcessSpawn,
            "the process panel could not start the platform process lister",
        )
        .with_context(error.to_string())
        .with_help("Install the platform process tools or use `quirl view directory`")
    })?;
    let mut bytes = Vec::new();
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(platform_error("the process lister did not expose stdout"));
    };
    stdout
        .take(MAX_PROCESS_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            ShellError::new(ErrorCode::Io, "could not read the platform process table")
                .with_context(error.to_string())
                .with_help("Retry the process view")
        })?;
    if bytes.len() as u64 > MAX_PROCESS_OUTPUT_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "the platform process table exceeded its bounded capture",
        )
        .with_help("Use a platform process tool with a narrower filter"));
    }
    let status = child.wait().map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "could not observe the platform process lister",
        )
        .with_context(error.to_string())
        .with_help("Retry the process view")
    })?;
    if !status.success() {
        return Err(ShellError::new(
            ErrorCode::ProcessSpawn,
            "the platform process lister returned a failure",
        )
        .with_help("Use `quirl view directory` if process inspection is unavailable"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(windows)]
fn parse_windows_process_row(line: &str) -> Option<ProcessPanelRow> {
    let fields = line
        .trim()
        .trim_matches('"')
        .split("\",\"")
        .collect::<Vec<_>>();
    let command = fields.first()?.trim().to_owned();
    let pid = fields.get(1)?.replace(',', "").parse().ok()?;
    Some(ProcessPanelRow {
        pid,
        parent_pid: None,
        state: "running".to_owned(),
        command,
    })
}

fn wait_interruptibly(interval_ms: u64, cancelled: &AtomicBool) -> bool {
    let mut remaining = interval_ms;
    while remaining > 0 {
        if cancelled.load(Ordering::Relaxed) {
            return true;
        }
        let step = remaining.min(CANCELLATION_POLL_MS);
        thread::sleep(Duration::from_millis(step));
        remaining -= step;
    }
    cancelled.load(Ordering::Relaxed)
}

fn names<T: Serialize>(values: &[T]) -> Result<String, ShellError> {
    values
        .iter()
        .map(|value| {
            serde_json::to_value(value)
                .map_err(json_error)
                .and_then(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| platform_error("protocol name was not a string"))
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|values| values.join(", "))
}

fn print_json(value: &impl Serialize) -> Result<(), ShellError> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(json_error)?
    );
    Ok(())
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not serialize platform output")
        .with_context(error.to_string())
}

fn platform_error(message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, message)
        .with_help("Inspect the command contract with `quirl help`")
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_core::ExtensionEventData;

    #[test]
    fn event_trace_rejects_reordered_records() {
        let trace = EventTrace {
            document_type: "quirl.extension.trace".to_owned(),
            schema_version: EXTENSION_PROTOCOL_VERSION,
            events: vec![
                ExtensionEvent::new(2, ExtensionEventData::SessionStart { restored: false }),
                ExtensionEvent::new(
                    1,
                    ExtensionEventData::Result {
                        status: 0,
                        duration_ms: 1,
                    },
                ),
            ],
        };
        assert!(validate_trace(&trace).is_err());
    }

    #[test]
    fn process_rows_are_parsed_without_terminal_semantics() {
        let rows = parse_process_rows(" 12 1 S quirl\ninvalid\n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, 12);
        assert_eq!(rows[0].command, "quirl");
    }

    #[test]
    fn process_rows_and_watch_waits_are_bounded() {
        let source = (0..MAX_PROCESS_ROWS + 10)
            .map(|pid| format!("{pid} 1 S process-{pid}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(parse_process_rows(&source).len(), MAX_PROCESS_ROWS);

        let cancelled = AtomicBool::new(true);
        assert!(wait_interruptibly(60_000, &cancelled));
    }
}
