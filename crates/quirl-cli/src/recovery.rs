use clap::{Subcommand, ValueEnum};
use quirl_core::{CommandOutcome, ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const DOCUMENT_TYPE: &str = "quirl.recovery.snapshot";
const SCHEMA_VERSION: u32 = 2;
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_CONTEXT_BYTES: usize = 4 * 1024;
const MAX_ENVIRONMENT_CHANGES: usize = 256;
const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024;
const MAX_SNAPSHOTS: usize = 32;
const MAX_RECOVERY_BYTES: u64 = 4 * 1024 * 1024;
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Subcommand)]
pub enum RecoveryCommand {
    /// List recoverable failure snapshots, newest first.
    List {
        #[arg(long, value_enum, default_value_t = RecoveryFormat::Text)]
        format: RecoveryFormat,
    },
    /// Inspect one snapshot, or the newest snapshot when ID is omitted.
    Show {
        id: Option<String>,
        #[arg(long, value_enum, default_value_t = RecoveryFormat::Text)]
        format: RecoveryFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RecoveryFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecoverySnapshot {
    pub document_type: String,
    pub schema_version: u32,
    pub id: String,
    pub created_unix_ms: u128,
    pub command: String,
    pub cwd: String,
    pub environment: EnvironmentDiff,
    pub output: CapturedOutput,
    pub duration_ms: u128,
    pub status: Option<i32>,
    pub error_chain: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDiff {
    pub changed: BTreeMap<String, String>,
    pub removed: Vec<String>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapturedOutput {
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub truncated: bool,
    #[serde(default)]
    pub stdout_discarded_bytes: u64,
    #[serde(default)]
    pub stderr_discarded_bytes: u64,
}

pub struct RecoveryContext {
    command: String,
    cwd: PathBuf,
    environment: BTreeMap<String, String>,
}

pub struct RecoveryJournal {
    directory: PathBuf,
    baseline_environment: BTreeMap<String, String>,
}

impl RecoveryJournal {
    pub fn discover() -> Result<Self, ShellError> {
        let directory = if let Some(path) = env::var_os("QUIRL_RECOVERY_DIR") {
            PathBuf::from(path)
        } else if let Some(path) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(path).join("quirl/recovery")
        } else if let Some(path) = env::var_os("HOME") {
            PathBuf::from(path).join(".local/state/quirl/recovery")
        } else {
            env::current_dir()
                .map_err(|error| recovery_io_error("locate", Path::new("."), error))?
                .join(".quirl/recovery")
        };
        Ok(Self::new(directory, environment()))
    }

    fn new(directory: PathBuf, baseline_environment: BTreeMap<String, String>) -> Self {
        Self {
            directory,
            baseline_environment,
        }
    }

    pub fn capture_context(&self, command: &str) -> Result<RecoveryContext, ShellError> {
        Ok(RecoveryContext {
            command: command.to_owned(),
            cwd: env::current_dir()
                .map_err(|error| recovery_io_error("inspect", Path::new("."), error))?,
            environment: environment(),
        })
    }

    pub fn record_failure(
        &self,
        context: &RecoveryContext,
        duration: Duration,
        outcome: Option<&CommandOutcome>,
        error: Option<&ShellError>,
    ) -> Result<String, ShellError> {
        self.record_failure_with_context(context, duration, outcome, error)
    }

    fn record_failure_with_context(
        &self,
        context: &RecoveryContext,
        duration: Duration,
        outcome: Option<&CommandOutcome>,
        error: Option<&ShellError>,
    ) -> Result<String, ShellError> {
        fs::create_dir_all(&self.directory)
            .map_err(|error| recovery_io_error("create", &self.directory, error))?;
        let created_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "system clock predates the Unix epoch")
                    .with_context(error.to_string())
                    .with_help("Correct the system clock before recording recovery data")
            })?
            .as_millis();
        let id = format!(
            "{created_unix_ms:020}-{:010}-{:020}",
            std::process::id(),
            NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
        );
        let mut secrets = secret_values(&self.baseline_environment);
        secrets.extend(secret_values(&context.environment));
        let environment = environment_diff(&self.baseline_environment, &context.environment);
        let (output, status) = outcome.map_or_else(
            || (CapturedOutput::default(), None),
            |outcome| {
                (
                    captured_output(
                        outcome.stdout.as_deref(),
                        outcome.stderr.as_deref(),
                        &secrets,
                    ),
                    Some(outcome.status),
                )
            },
        );
        let error_chain = error.map_or_else(Vec::new, |error| {
            std::iter::once(error.message.as_str())
                .chain(error.details.context.iter().map(String::as_str))
                .map(|part| redact_text(bounded_str(part, MAX_CONTEXT_BYTES), &secrets))
                .collect()
        });
        let snapshot = RecoverySnapshot {
            document_type: DOCUMENT_TYPE.to_owned(),
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            created_unix_ms,
            command: redact_text(bounded_str(&context.command, MAX_CAPTURE_BYTES), &secrets),
            cwd: context.cwd.display().to_string(),
            environment,
            output,
            duration_ms: duration.as_millis(),
            status,
            error_chain,
        };
        self.write_snapshot(&snapshot)?;
        Ok(id)
    }

    fn write_snapshot(&self, snapshot: &RecoverySnapshot) -> Result<(), ShellError> {
        let destination = self.directory.join(format!("{}.json", snapshot.id));
        let temporary = self.directory.join(format!(".{}.tmp", snapshot.id));
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not serialize recovery snapshot")
                .with_context(error.to_string())
                .with_help("Report this as a recovery schema defect")
        })?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(oversized_snapshot_error(&snapshot.id, bytes.len() as u64));
        }
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| recovery_io_error("create", &temporary, error))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| recovery_io_error("write", &temporary, error))?;
        fs::rename(&temporary, &destination)
            .map_err(|error| recovery_io_error("install", &destination, error))?;
        self.enforce_retention()
    }

    fn enforce_retention(&self) -> Result<(), ShellError> {
        let ids = self.ids()?;
        let mut retained = 0_usize;
        let mut retained_bytes = 0_u64;
        for id in ids {
            let path = self.directory.join(format!("{id}.json"));
            let size = fs::metadata(&path)
                .map_err(|error| recovery_io_error("inspect", &path, error))?
                .len();
            if retained < MAX_SNAPSHOTS && retained_bytes.saturating_add(size) <= MAX_RECOVERY_BYTES
            {
                retained += 1;
                retained_bytes = retained_bytes.saturating_add(size);
            } else {
                fs::remove_file(&path).map_err(|error| recovery_io_error("prune", &path, error))?;
            }
        }
        Ok(())
    }

    fn ids(&self) -> Result<Vec<String>, ShellError> {
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(recovery_io_error("read", &self.directory, error)),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| recovery_io_error("read", &self.directory, error))?;
            if entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
            {
                if let Some(id) = entry
                    .path()
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|_| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .filter(|id| is_valid_id(id))
                {
                    ids.push(id.to_owned());
                }
            }
        }
        ids.sort_unstable_by(|left, right| {
            snapshot_order_key(right)
                .cmp(&snapshot_order_key(left))
                .then_with(|| right.cmp(left))
        });
        Ok(ids)
    }

    fn read(&self, id: &str) -> Result<RecoverySnapshot, ShellError> {
        validate_id(id)?;
        let path = self.directory.join(format!("{id}.json"));
        let file = File::open(&path).map_err(|error| recovery_io_error("read", &path, error))?;
        let size = file
            .metadata()
            .map_err(|error| recovery_io_error("inspect", &path, error))?
            .len();
        if size > MAX_SNAPSHOT_BYTES {
            return Err(oversized_snapshot_error(id, size));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.take(MAX_SNAPSHOT_BYTES.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| recovery_io_error("read", &path, error))?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(oversized_snapshot_error(id, bytes.len() as u64));
        }
        let snapshot: RecoverySnapshot = serde_json::from_slice(&bytes).map_err(|error| {
            ShellError::new(
                ErrorCode::Validation,
                format!("recovery snapshot `{id}` is invalid"),
            )
            .with_context(error.to_string())
            .with_help("Remove or repair the invalid snapshot before retrying")
        })?;
        if snapshot.document_type != DOCUMENT_TYPE || snapshot.schema_version != SCHEMA_VERSION {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("recovery snapshot `{id}` uses an unsupported schema"),
            )
            .with_help(format!(
                "Expected {DOCUMENT_TYPE} schema version {SCHEMA_VERSION}"
            )));
        }
        Ok(snapshot)
    }
}

fn snapshot_order_key(id: &str) -> (u128, u64, u64) {
    let mut parts = id.split('-');
    (
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0),
    )
}

pub fn execute(command: RecoveryCommand) -> Result<i32, ShellError> {
    let journal = RecoveryJournal::discover()?;
    match command {
        RecoveryCommand::List { format } => {
            let ids = journal.ids()?;
            match format {
                RecoveryFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&ids).map_err(json_error)?
                ),
                RecoveryFormat::Text => {
                    if ids.is_empty() {
                        println!("no recovery snapshots");
                    } else {
                        for id in ids {
                            println!("{id}");
                        }
                    }
                }
            }
        }
        RecoveryCommand::Show { id, format } => {
            let id = id.map_or_else(
                || {
                    journal.ids()?.into_iter().next().ok_or_else(|| {
                        ShellError::new(ErrorCode::InvalidArgument, "no recovery snapshots exist")
                            .with_help("A failed `quirl exec` command creates a snapshot")
                    })
                },
                Ok,
            )?;
            let snapshot = journal.read(&id)?;
            match format {
                RecoveryFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&snapshot).map_err(json_error)?
                ),
                RecoveryFormat::Text => print!("{}", render_snapshot_text(&snapshot)),
            }
        }
    }
    Ok(0)
}

fn render_snapshot_text(snapshot: &RecoverySnapshot) -> String {
    let mut rendered = format!(
        "snapshot: {}\ncommand: {}\ncwd: {}\nstatus: {}\nduration: {} ms\n",
        terminal_safe(&snapshot.id),
        terminal_safe(&snapshot.command),
        terminal_safe(&snapshot.cwd),
        snapshot
            .status
            .map_or_else(|| "error".to_owned(), |status| status.to_string()),
        snapshot.duration_ms
    );
    if let Some(stdout) = snapshot.output.stdout.as_deref() {
        rendered.push_str("stdout:\n");
        rendered.push_str(&terminal_safe(stdout));
        if !stdout.ends_with('\n') {
            rendered.push('\n');
        }
    }
    if let Some(stderr) = snapshot.output.stderr.as_deref() {
        rendered.push_str("stderr:\n");
        rendered.push_str(&terminal_safe(stderr));
        if !stderr.ends_with('\n') {
            rendered.push('\n');
        }
    }
    if snapshot.output.truncated {
        rendered.push_str(&format!(
            "capture truncated: stdout discarded {} bytes; stderr discarded {} bytes\n",
            snapshot.output.stdout_discarded_bytes, snapshot.output.stderr_discarded_bytes
        ));
    }
    for error in &snapshot.error_chain {
        rendered.push_str("error: ");
        rendered.push_str(&terminal_safe(error));
        rendered.push('\n');
    }
    rendered
}

fn terminal_safe(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        if (character.is_control() && !matches!(character, '\n' | '\t')) || character == '\u{009b}'
        {
            rendered.extend(character.escape_default());
        } else {
            rendered.push(character);
        }
    }
    rendered
}

pub fn wants_json(command: &RecoveryCommand) -> bool {
    matches!(
        command,
        RecoveryCommand::List {
            format: RecoveryFormat::Json
        } | RecoveryCommand::Show {
            format: RecoveryFormat::Json,
            ..
        }
    )
}

fn environment() -> BTreeMap<String, String> {
    env::vars().collect()
}

fn environment_diff(
    baseline: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
) -> EnvironmentDiff {
    let mut changed = BTreeMap::new();
    let mut removed = Vec::new();
    let mut truncated = false;
    for (key, value) in current {
        if baseline.get(key) == Some(value) {
            continue;
        }
        if changed.len() >= MAX_ENVIRONMENT_CHANGES {
            truncated = true;
            continue;
        }
        changed.insert(
            key.clone(),
            if is_secret_key(key) {
                "[redacted]".to_owned()
            } else {
                bounded_str(value, MAX_CONTEXT_BYTES).to_owned()
            },
        );
    }
    for key in baseline.keys().filter(|key| !current.contains_key(*key)) {
        if removed.len() >= MAX_ENVIRONMENT_CHANGES {
            truncated = true;
            continue;
        }
        removed.push(key.clone());
    }
    EnvironmentDiff {
        changed,
        removed,
        truncated,
    }
}

fn secret_values(environment: &BTreeMap<String, String>) -> BTreeSet<String> {
    environment
        .iter()
        .filter(|(key, value)| is_secret_key(key) && value.len() >= 4)
        .map(|(_, value)| value.clone())
        .collect()
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase().replace(['-', '.'], "_");
    [
        "TOKEN",
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "API_KEY",
        "PRIVATE_KEY",
        "AUTH",
    ]
    .iter()
    .any(|marker| key.contains(marker))
}

fn redact_text(value: &str, secrets: &BTreeSet<String>) -> String {
    let mut redacted = value.to_owned();
    for secret in secrets {
        redacted = redacted.replace(secret, "[redacted]");
    }
    let spans = shell_token_spans(&redacted);
    let mut rendered = String::with_capacity(redacted.len());
    let mut cursor = 0;
    let mut redact_next = false;
    for (start, end) in spans {
        rendered.push_str(&redacted[cursor..start]);
        let token = &redacted[start..end];
        if redact_next {
            rendered.push_str(&redacted_token(token));
            redact_next = false;
        } else if let Some((key, value)) = token.split_once('=') {
            if is_secret_key(key.trim_start_matches('-')) {
                rendered.push_str(key);
                rendered.push('=');
                rendered.push_str(&redacted_token(value));
            } else {
                rendered.push_str(token);
            }
        } else {
            rendered.push_str(token);
            redact_next = is_secret_key(token.trim_start_matches('-'));
        }
        cursor = end;
    }
    rendered.push_str(&redacted[cursor..]);
    rendered
}

fn shell_token_spans(value: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if start.is_none() {
            if character.is_whitespace() {
                continue;
            }
            start = Some(index);
        }
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
        } else if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
        } else if matches!(character, '\'' | '"') {
            quote = Some(character);
        } else if character.is_whitespace() {
            if let Some(start) = start.take() {
                spans.push((start, index));
            }
        }
    }
    if let Some(start) = start {
        spans.push((start, value.len()));
    }
    spans
}

fn redacted_token(token: &str) -> String {
    if token.len() >= 2 {
        let first = token.as_bytes()[0];
        let last = token.as_bytes()[token.len() - 1];
        if matches!(first, b'\'' | b'"') && last == first {
            let quote = first as char;
            return format!("{quote}[redacted]{quote}");
        }
    }
    "[redacted]".to_owned()
}

fn captured_output(
    stdout: Option<&str>,
    stderr: Option<&str>,
    secrets: &BTreeSet<String>,
) -> CapturedOutput {
    let (stdout, stdout_discarded_bytes) = stdout.map_or((None, 0), |value| {
        let (value, discarded_bytes) = truncate(value);
        (Some(redact_text(value, secrets)), discarded_bytes)
    });
    let (stderr, stderr_discarded_bytes) = stderr.map_or((None, 0), |value| {
        let (value, discarded_bytes) = truncate(value);
        (Some(redact_text(value, secrets)), discarded_bytes)
    });
    CapturedOutput {
        stdout,
        stderr,
        truncated: stdout_discarded_bytes > 0 || stderr_discarded_bytes > 0,
        stdout_discarded_bytes,
        stderr_discarded_bytes,
    }
}

fn truncate(value: &str) -> (&str, u64) {
    if value.len() <= MAX_CAPTURE_BYTES {
        return (value, 0);
    }
    let boundary = (0..=MAX_CAPTURE_BYTES)
        .rev()
        .find(|index| value.is_char_boundary(*index))
        .unwrap_or(0);
    (
        &value[..boundary],
        u64::try_from(value.len().saturating_sub(boundary)).unwrap_or(u64::MAX),
    )
}

fn bounded_str(value: &str, limit: usize) -> &str {
    if value.len() <= limit {
        return value;
    }
    let boundary = (0..=limit)
        .rev()
        .find(|index| value.is_char_boundary(*index))
        .unwrap_or(0);
    &value[..boundary]
}

fn validate_id(id: &str) -> Result<(), ShellError> {
    if is_valid_id(id) {
        Ok(())
    } else {
        Err(
            ShellError::new(ErrorCode::InvalidArgument, "invalid recovery snapshot id")
                .with_help("Use an ID printed by `quirl recover list`"),
        )
    }
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|character| character.is_ascii_digit() || character == '-')
}

fn recovery_io_error(action: &str, path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not {action} recovery data at {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Check recovery directory permissions and available disk space")
}

fn oversized_snapshot_error(id: &str, size: u64) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("recovery snapshot `{id}` exceeds the read limit"),
    )
    .with_context(format!(
        "snapshot bytes: {size}; limit: {MAX_SNAPSHOT_BYTES}"
    ))
    .with_help("Remove the oversized snapshot or inspect it with a bounded external tool")
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not serialize recovery output")
        .with_context(error.to_string())
        .with_help("Report this as a recovery schema defect")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "quirl-recovery-{name}-{}-{}",
            std::process::id(),
            NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn snapshot_is_versioned_atomic_bounded_and_contains_no_secret_values() {
        let directory = test_directory("atomic");
        let baseline = BTreeMap::from([
            ("PLAIN".to_owned(), "before".to_owned()),
            ("API_TOKEN".to_owned(), "ultra-secret-value".to_owned()),
            ("REMOVED".to_owned(), "yes".to_owned()),
        ]);
        let current = BTreeMap::from([
            ("PLAIN".to_owned(), "after".to_owned()),
            ("API_TOKEN".to_owned(), "ultra-secret-value".to_owned()),
        ]);
        let journal = RecoveryJournal::new(directory.clone(), baseline);
        let outcome = CommandOutcome {
            status: 9,
            stdout: Some(format!(
                "ultra-secret-value{}",
                "x".repeat(MAX_CAPTURE_BYTES + 10)
            )),
            stderr: Some("API_TOKEN=ultra-secret-value".to_owned()),
        };
        let error = ShellError::new(ErrorCode::InvalidCommand, "ultra-secret-value failed")
            .with_context("API_TOKEN=ultra-secret-value");
        let context = RecoveryContext {
            command: "deploy  --token 'ultra-secret-value'\t--password other-secret".to_owned(),
            cwd: env::current_dir().unwrap(),
            environment: current,
        };
        let id = journal
            .record_failure_with_context(
                &context,
                Duration::from_millis(27),
                Some(&outcome),
                Some(&error),
            )
            .unwrap();
        let bytes = fs::read(directory.join(format!("{id}.json"))).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("ultra-secret-value"));
        assert!(!String::from_utf8_lossy(&bytes).contains("other-secret"));
        assert!(!fs::read_dir(&directory).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "tmp")
        }));
        let snapshot = journal.read(&id).unwrap();
        assert_eq!(snapshot.document_type, DOCUMENT_TYPE);
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(
            snapshot.command,
            "deploy  --token '[redacted]'\t--password [redacted]"
        );
        assert_eq!(snapshot.status, Some(9));
        assert_eq!(snapshot.duration_ms, 27);
        assert_eq!(snapshot.environment.changed["PLAIN"], "after");
        assert_eq!(snapshot.environment.removed, ["REMOVED"]);
        assert!(snapshot.output.truncated);
        assert_eq!(snapshot.output.stdout_discarded_bytes, 28);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_ids_cannot_escape_the_recovery_directory() {
        let journal = RecoveryJournal::new(test_directory("traversal"), BTreeMap::new());
        let error = journal.read("../../secret").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn text_rendering_escapes_ansi_osc_and_carriage_return_controls() {
        let hostile = "visible\u{1b}[31mred\u{1b}[0m\u{1b}]0;owned\u{7}\rreplace\u{009b}31m";
        let snapshot = RecoverySnapshot {
            document_type: DOCUMENT_TYPE.to_owned(),
            schema_version: SCHEMA_VERSION,
            id: "0001-0001-0001".to_owned(),
            created_unix_ms: 1,
            command: hostile.to_owned(),
            cwd: hostile.to_owned(),
            environment: EnvironmentDiff::default(),
            output: CapturedOutput {
                stdout: Some(hostile.to_owned()),
                stderr: Some(hostile.to_owned()),
                truncated: false,
                stdout_discarded_bytes: 0,
                stderr_discarded_bytes: 0,
            },
            duration_ms: 1,
            status: Some(1),
            error_chain: vec![hostile.to_owned()],
        };
        let text = render_snapshot_text(&snapshot);
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{7}'));
        assert!(!text.contains('\r'));
        assert!(!text.contains('\u{009b}'));
        assert!(text.contains("\\u{1b}]0;owned\\u{7}"));

        let json = serde_json::to_string(&snapshot).unwrap();
        let decoded: RecoverySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.output.stdout.as_deref(), Some(hostile));
    }

    #[test]
    fn retention_keeps_only_the_newest_bounded_snapshot_set() {
        let directory = test_directory("retention");
        let journal = RecoveryJournal::new(directory.clone(), BTreeMap::new());
        for index in 0..MAX_SNAPSHOTS + 3 {
            let context = RecoveryContext {
                command: format!("false {index}"),
                cwd: env::current_dir().unwrap(),
                environment: BTreeMap::new(),
            };
            journal
                .record_failure_with_context(
                    &context,
                    Duration::from_millis(1),
                    Some(&CommandOutcome {
                        status: 1,
                        stdout: Some(String::new()),
                        stderr: Some(String::new()),
                    }),
                    None,
                )
                .unwrap();
        }
        assert_eq!(journal.ids().unwrap().len(), MAX_SNAPSHOTS);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retention_enforces_byte_quota_and_reads_reject_oversized_files() {
        let directory = test_directory("byte-quota");
        fs::create_dir_all(&directory).unwrap();
        let journal = RecoveryJournal::new(directory.clone(), BTreeMap::new());
        for index in 0..20_u64 {
            let id = format!("{:020}-{:010}-{index:020}", 1_000 + index, 1);
            fs::write(
                directory.join(format!("{id}.json")),
                vec![b'x'; MAX_SNAPSHOT_BYTES as usize - 1],
            )
            .unwrap();
        }
        journal.enforce_retention().unwrap();
        let retained_bytes = journal
            .ids()
            .unwrap()
            .into_iter()
            .map(|id| {
                fs::metadata(directory.join(format!("{id}.json")))
                    .unwrap()
                    .len()
            })
            .sum::<u64>();
        assert!(retained_bytes <= MAX_RECOVERY_BYTES);

        let oversized_id = "99999999999999999999-0000000001-00000000000000000001";
        fs::write(
            directory.join(format!("{oversized_id}.json")),
            vec![b'x'; MAX_SNAPSHOT_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(
            journal.read(oversized_id).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
