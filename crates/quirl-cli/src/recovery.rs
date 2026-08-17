use clap::{Subcommand, ValueEnum};
use quirl_core::{
    escape_json_terminal_controls, escape_terminal_controls, CommandOutcome, ErrorCode, ShellError,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

#[cfg(unix)]
use nix::{
    fcntl::{open, OFlag},
    sys::stat::Mode,
};

const DOCUMENT_TYPE: &str = "quirl.recovery.snapshot";
pub const RECOVERY_SCHEMA_VERSION: u32 = 2;
pub const RECOVERY_OLDEST_READABLE_VERSION: u32 = 1;
pub const RECOVERY_SCHEMA_DESCRIPTOR: &str = "quirl.recovery.snapshot@2{RecoverySnapshot{deny_unknown;document_type:string;schema_version:u32;id:string;created_unix_ms:u128;command:string;cwd:string;environment:EnvironmentDiff;output:CapturedOutput;duration_ms:u128;status:null|i32;error_chain:array<string>};EnvironmentDiff{deny_unknown;changed:map<string,string>;removed:array<string>;truncated:bool};CapturedOutput{deny_unknown;stdout:null|string;stderr:null|string;truncated:bool;stdout_discarded_bytes:u64;stderr_discarded_bytes:u64};migration:v1-to-v2-adds-unavailable-command-cwd-empty-environment-zero-discard-counts;redaction:preserved;replay:none}";
const MAX_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_CONTEXT_BYTES: usize = 4 * 1024;
const MAX_ENVIRONMENT_CHANGES: usize = 256;
const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024;
const MAX_SNAPSHOTS: usize = 32;
const MAX_RECOVERY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RECOVERY_DIRECTORY_ENTRIES: usize = 1_024;
const RECOVERY_DIRECTORY_DEPTH_MAX: usize = 64;
const TEMPORARY_NAME_ATTEMPTS_MAX: usize = 64;
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

// Failure cleanup never unlinks an armed transaction name. The recovery
// directory is private (0700), so successful writes may remove their one hidden
// temporary under the narrower assumption that this private namespace remains
// cooperative through the final post-commit unlink.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotWriteStage {
    PartialWrite,
    ContentSynced,
    Installed,
}

struct SnapshotTransaction {
    temporary: Option<SnapshotOwnedPath>,
    destination: PathBuf,
    destination_owned: bool,
}

#[derive(Debug)]
struct SnapshotOwnedPath {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SnapshotOwnedPath {
    fn new(path: PathBuf, file: &File) -> Result<Self, ShellError> {
        let metadata = file
            .metadata()
            .map_err(|error| recovery_io_error("inspect", &path, error))?;
        Ok(Self {
            path,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    fn matches(&self, path: &Path) -> bool {
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
    }

    #[cfg(not(unix))]
    fn matches(&self, _path: &Path) -> bool {
        false
    }

    fn remove_committed(&self) -> io::Result<()> {
        // Recovery uses a private 0700 directory. This success-only unlink is
        // bounded to one transaction name and assumes that namespace remains
        // cooperative through the final post-commit cleanup.
        fs::remove_file(&self.path)
    }
}

impl SnapshotTransaction {
    fn new(temporary: PathBuf, file: &File, destination: PathBuf) -> Result<Self, ShellError> {
        Ok(Self {
            temporary: Some(SnapshotOwnedPath::new(temporary, file)?),
            destination,
            destination_owned: false,
        })
    }

    fn temporary(&self) -> &Path {
        self.temporary
            .as_ref()
            .map(SnapshotOwnedPath::path)
            .unwrap_or_else(|| Path::new("<removed-temporary>"))
    }

    fn owns(&self, path: &Path) -> bool {
        self.temporary
            .as_ref()
            .is_some_and(|temporary| temporary.matches(path))
    }

    fn cleanup(&mut self, mut error: ShellError) -> ShellError {
        if self.destination_owned {
            self.destination_owned = false;
            error = error.with_context(format!(
                "failure cleanup preserved recovery destination {}",
                self.destination.display()
            ));
        }
        if let Some(temporary) = self.temporary.take() {
            error = error.with_context(format!(
                "failure cleanup preserved recovery temporary {}",
                temporary.path().display()
            ));
        }
        error
    }

    fn commit(&mut self) {
        self.destination_owned = false;
        self.temporary = None;
    }
}

impl Drop for SnapshotTransaction {
    fn drop(&mut self) {
        self.destination_owned = false;
        self.temporary = None;
    }
}

#[derive(Debug, Subcommand)]
pub enum RecoveryCommand {
    /// List recoverable failure snapshots, newest first.
    List {
        /// Output representation for the snapshot list.
        #[arg(long, value_enum, default_value_t = RecoveryFormat::Text)]
        format: RecoveryFormat,
    },
    /// Inspect one snapshot, or the newest snapshot when ID is omitted.
    Show {
        /// Snapshot identifier; omit to select the newest snapshot.
        id: Option<String>,
        /// Output representation for the snapshot details.
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LegacyRecoverySnapshotV1 {
    document_type: String,
    schema_version: u32,
    id: String,
    created_unix_ms: u128,
    output: LegacyCapturedOutputV1,
    duration_ms: u128,
    status: Option<i32>,
    error_chain: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LegacyCapturedOutputV1 {
    stdout: Option<String>,
    stderr: Option<String>,
    truncated: bool,
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
        create_recovery_directories(&self.directory)?;
        secure_recovery_directory(&self.directory)?;
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
            schema_version: RECOVERY_SCHEMA_VERSION,
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
        self.write_snapshot_with_hook(snapshot, |_| Ok(()))
    }

    fn write_snapshot_with_hook(
        &self,
        snapshot: &RecoverySnapshot,
        mut after_stage: impl FnMut(SnapshotWriteStage) -> io::Result<()>,
    ) -> Result<(), ShellError> {
        let destination = self.directory.join(format!("{}.json", snapshot.id));
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not serialize recovery snapshot")
                .with_context(error.to_string())
                .with_help("Report this as a recovery schema defect")
        })?;
        if bytes.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(oversized_snapshot_error(&snapshot.id, bytes.len() as u64));
        }
        reject_existing_snapshot_destination(&destination)?;
        let (temporary, mut file) = create_snapshot_temporary(&destination)?;
        let mut transaction =
            SnapshotTransaction::new(temporary.clone(), &file, destination.clone()).map_err(
                |error| {
                    error.with_context(format!(
                        "failure cleanup preserved recovery temporary {}",
                        temporary.display()
                    ))
                },
            )?;
        let split = bytes.len().div_ceil(2);
        if let Err(error) = file.write_all(&bytes[..split]).and_then(|()| {
            after_stage(SnapshotWriteStage::PartialWrite)?;
            file.write_all(&bytes[split..])?;
            file.sync_all()?;
            after_stage(SnapshotWriteStage::ContentSynced)
        }) {
            return Err(transaction.cleanup(recovery_io_error(
                "write",
                transaction.temporary(),
                error,
            )));
        }
        if let Err(error) = validate_snapshot_file(transaction.temporary(), &file, 1) {
            return Err(transaction.cleanup(error));
        }
        drop(file);
        if let Err(error) = fs::hard_link(transaction.temporary(), &destination) {
            return Err(transaction.cleanup(recovery_io_error("install", &destination, error)));
        }
        transaction.destination_owned = true;
        if !transaction.owns(&destination) {
            return Err(transaction.cleanup(
                ShellError::new(
                    ErrorCode::Validation,
                    format!(
                        "recovery destination {} changed during installation",
                        destination.display()
                    ),
                )
                .with_help("Review the preserved destination and temporary before retrying"),
            ));
        }
        if let Err(error) = validate_snapshot_path(&destination, 2) {
            return Err(transaction.cleanup(error));
        }
        if let Err(error) = after_stage(SnapshotWriteStage::Installed) {
            return Err(transaction.cleanup(recovery_io_error("install", &destination, error)));
        }
        if let Err(error) = sync_recovery_directory(&self.directory) {
            return Err(transaction.cleanup(error));
        }
        transaction
            .temporary
            .as_ref()
            .map(SnapshotOwnedPath::remove_committed)
            .transpose()
            .map_err(|error| {
                transaction.cleanup(recovery_io_error("clean", transaction.temporary(), error))
            })?;
        transaction.temporary = None;
        // The installed snapshot is already durable. Failure to persist only
        // the temporary-name removal must not turn the committed write into an
        // ambiguous returned failure.
        let _ = sync_recovery_directory(&self.directory);
        transaction.commit();
        self.enforce_retention()
    }

    fn enforce_retention(&self) -> Result<(), ShellError> {
        let ids = self.ids()?;
        let mut retained = 0_usize;
        let mut retained_bytes = 0_u64;
        for id in ids {
            let path = self.directory.join(format!("{id}.json"));
            let size = validate_snapshot_path(&path, 1)?.len();
            if retained < MAX_SNAPSHOTS && retained_bytes.saturating_add(size) <= MAX_RECOVERY_BYTES
            {
                retained += 1;
                retained_bytes = retained_bytes.saturating_add(size);
            } else {
                // Retention is successful policy cleanup inside the secured
                // 0700 recovery namespace, not transaction failure cleanup.
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
        for (entry_index, entry) in entries.enumerate() {
            if entry_index >= MAX_RECOVERY_DIRECTORY_ENTRIES {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "recovery directory exceeds its entry limit",
                )
                .with_context(format!(
                    "limit: {MAX_RECOVERY_DIRECTORY_ENTRIES}; observed: at least {}",
                    entry_index + 1
                ))
                .with_help("Remove stale recovery entries before retrying"));
            }
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
        validate_snapshot_path(&path, 1)?;
        let file = open_snapshot_no_follow(&path)
            .map_err(|error| recovery_io_error("read", &path, error))?;
        validate_snapshot_file(&path, &file, 1)?;
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
        decode_snapshot(&bytes, id)
    }
}

fn reject_existing_snapshot_destination(path: &Path) -> Result<(), ShellError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(recovery_io_error("inspect", path, error)),
        Ok(_) => Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "recovery snapshot destination {} already exists",
                path.display()
            ),
        )
        .with_help("Remove the stale entry only after confirming it is not needed")),
    }
}

fn create_snapshot_temporary(destination: &Path) -> Result<(PathBuf, File), ShellError> {
    for _ in 0..TEMPORARY_NAME_ATTEMPTS_MAX {
        let sequence = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
        let name = destination.file_name().ok_or_else(|| {
            ShellError::new(
                ErrorCode::InvalidArgument,
                "recovery snapshot destination has no file name",
            )
            .with_help("Use a recovery directory containing regular snapshot file names")
        })?;
        let mut temporary_name = OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".{}-{sequence}.tmp", std::process::id()));
        let temporary = destination.with_file_name(temporary_name);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary) {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                    return Err(recovery_io_error("secure", &temporary, error).with_context(
                        format!(
                            "failure cleanup preserved recovery temporary {}",
                            temporary.display()
                        ),
                    ));
                }
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(recovery_io_error("create", &temporary, error)),
        }
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        "recovery snapshot temporary-name attempts exhausted",
    )
    .with_context(format!(
        "limit: {TEMPORARY_NAME_ATTEMPTS_MAX}; observed: {TEMPORARY_NAME_ATTEMPTS_MAX}"
    ))
    .with_help("Remove stale hidden recovery temporary files before retrying"))
}

fn validate_snapshot_path(path: &Path, links_expected: u64) -> Result<fs::Metadata, ShellError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| recovery_io_error("inspect", path, error))?;
    validate_snapshot_metadata(path, &metadata, links_expected)?;
    Ok(metadata)
}

fn validate_snapshot_file(path: &Path, file: &File, links_expected: u64) -> Result<(), ShellError> {
    let path_metadata = validate_snapshot_path(path, links_expected)?;
    let file_metadata = file
        .metadata()
        .map_err(|error| recovery_io_error("inspect", path, error))?;
    validate_snapshot_metadata(path, &file_metadata, links_expected)?;
    #[cfg(unix)]
    if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "recovery snapshot {} changed during admission",
                path.display()
            ),
        )
        .with_help("Retry after removing the conflicting recovery entry"));
    }
    Ok(())
}

fn validate_snapshot_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    links_expected: u64,
) -> Result<(), ShellError> {
    if !metadata.file_type().is_file() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("recovery snapshot {} is not a regular file", path.display()),
        )
        .with_help("Remove links and special files from the recovery directory"));
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != links_expected {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("recovery snapshot {} has hard-link aliases", path.display()),
            )
            .with_context(format!(
                "expected links: {links_expected}; observed: {}",
                metadata.nlink()
            ))
            .with_help("Copy the snapshot to an unlinked private regular file"));
        }
        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "recovery snapshot {} has unsafe permissions",
                    path.display()
                ),
            )
            .with_context(format!("expected mode: 0o600; observed mode: {mode:#o}"))
            .with_help("Set the snapshot mode to 0600 before reading it"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_snapshot_no_follow(path: &Path) -> io::Result<File> {
    let descriptor = open(
        path,
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    Ok(File::from(descriptor))
}

#[cfg(not(unix))]
fn open_snapshot_no_follow(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn sync_recovery_directory(path: &Path) -> Result<(), ShellError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| recovery_io_error("synchronize", path, error))
}

#[cfg(not(unix))]
fn sync_recovery_directory(_path: &Path) -> Result<(), ShellError> {
    Ok(())
}

fn create_recovery_directories(directory: &Path) -> Result<(), ShellError> {
    let mut missing = Vec::new();
    let mut cursor = directory;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(metadata) if metadata.file_type().is_dir() => break,
            Ok(_) => {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!("recovery path {} is not a real directory", cursor.display()),
                )
                .with_help("Replace links and special files with private directories"));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if missing.len() >= RECOVERY_DIRECTORY_DEPTH_MAX {
                    return Err(ShellError::new(
                        ErrorCode::ResourceLimit,
                        format!(
                            "recovery path {} exceeds its depth limit",
                            directory.display()
                        ),
                    )
                    .with_context(format!(
                        "limit: {RECOVERY_DIRECTORY_DEPTH_MAX}; observed: at least {}",
                        missing.len() + 1
                    ))
                    .with_help("Choose a shallower recovery directory"));
                }
                missing.push(cursor.to_path_buf());
                cursor = cursor
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
            }
            Err(error) => return Err(recovery_io_error("inspect", cursor, error)),
        }
    }

    let mut created = Vec::<PathBuf>::new();
    for path in missing.into_iter().rev() {
        if let Err(error) = fs::create_dir(&path) {
            let mut shell_error = recovery_io_error("create", &path, error);
            while let Some(created_path) = created.pop() {
                shell_error = shell_error.with_context(format!(
                    "recovery directory {} was preserved because cleanup cannot atomically prove path ownership",
                    created_path.display()
                ));
            }
            return Err(shell_error);
        }
        #[cfg(unix)]
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
            let mut shell_error = recovery_io_error("secure", &path, error);
            shell_error = shell_error.with_context(format!(
                "recovery directory {} was preserved because cleanup cannot atomically prove path ownership",
                path.display()
            ));
            while let Some(created_path) = created.pop() {
                shell_error = shell_error.with_context(format!(
                    "recovery directory {} was preserved because cleanup cannot atomically prove path ownership",
                    created_path.display()
                ));
            }
            return Err(shell_error);
        }
        created.push(path);
    }
    Ok(())
}

#[cfg(unix)]
fn secure_recovery_directory(path: &Path) -> Result<(), ShellError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| recovery_io_error("inspect", path, error))?;
    if !metadata.file_type().is_dir() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("recovery path {} is not a real directory", path.display()),
        )
        .with_help("Replace links and special files with a private directory"));
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)
        .map_err(|error| recovery_io_error("secure", path, error))?;
    let mode = fs::symlink_metadata(path)
        .map_err(|error| recovery_io_error("inspect", path, error))?
        .mode()
        & 0o777;
    if mode == 0o700 {
        Ok(())
    } else {
        Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "recovery directory {} has unsafe permissions",
                path.display()
            ),
        )
        .with_context(format!("expected mode: 0o700; observed mode: {mode:#o}"))
        .with_help("Set the recovery directory mode to 0700 before retrying"))
    }
}

#[cfg(not(unix))]
fn secure_recovery_directory(_path: &Path) -> Result<(), ShellError> {
    // Windows ACL inheritance is managed by the selected state directory.
    Ok(())
}

fn decode_snapshot(bytes: &[u8], expected_id: &str) -> Result<RecoverySnapshot, ShellError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("recovery snapshot `{expected_id}` is invalid"),
        )
        .with_context(error.to_string())
        .with_help("Remove or repair the invalid snapshot before retrying")
    })?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| unsupported_snapshot_error(expected_id, None))?;
    let snapshot = match version {
        2 => serde_json::from_value::<RecoverySnapshot>(value).map_err(|error| {
            ShellError::new(
                ErrorCode::Validation,
                format!("recovery snapshot `{expected_id}` is invalid"),
            )
            .with_context(error.to_string())
            .with_help("Remove or repair the invalid snapshot before retrying")
        })?,
        1 => {
            let legacy =
                serde_json::from_value::<LegacyRecoverySnapshotV1>(value).map_err(|error| {
                    ShellError::new(
                        ErrorCode::Validation,
                        format!("legacy recovery snapshot `{expected_id}` is invalid"),
                    )
                    .with_context(error.to_string())
                    .with_help("Repair the v1 snapshot before migrating it")
                })?;
            RecoverySnapshot {
                document_type: legacy.document_type,
                schema_version: RECOVERY_SCHEMA_VERSION,
                id: legacy.id,
                created_unix_ms: legacy.created_unix_ms,
                command: "[unavailable in recovery schema v1]".to_owned(),
                cwd: "[unavailable in recovery schema v1]".to_owned(),
                environment: EnvironmentDiff::default(),
                output: CapturedOutput {
                    stdout: legacy.output.stdout,
                    stderr: legacy.output.stderr,
                    truncated: legacy.output.truncated,
                    stdout_discarded_bytes: 0,
                    stderr_discarded_bytes: 0,
                },
                duration_ms: legacy.duration_ms,
                status: legacy.status,
                error_chain: legacy.error_chain,
            }
        }
        _ => return Err(unsupported_snapshot_error(expected_id, Some(version))),
    };
    if snapshot.document_type != DOCUMENT_TYPE
        || snapshot.schema_version != RECOVERY_SCHEMA_VERSION
        || snapshot.id != expected_id
    {
        return Err(unsupported_snapshot_error(expected_id, Some(version)));
    }
    Ok(snapshot)
}

fn unsupported_snapshot_error(id: &str, version: Option<u64>) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "recovery snapshot `{id}` uses unsupported schema {}",
            version.map_or_else(|| "<missing>".to_owned(), |value| value.to_string())
        ),
    )
    .with_help(format!(
        "Expected {DOCUMENT_TYPE} schema {RECOVERY_OLDEST_READABLE_VERSION}..={RECOVERY_SCHEMA_VERSION}"
    ))
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
                RecoveryFormat::Json => {
                    let json = serde_json::to_string_pretty(&ids).map_err(json_error)?;
                    println!("{}", escape_json_terminal_controls(&json));
                }
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
                RecoveryFormat::Json => {
                    let json = serde_json::to_string_pretty(&snapshot).map_err(json_error)?;
                    println!("{}", escape_json_terminal_controls(&json));
                }
                RecoveryFormat::Text => print!("{}", render_snapshot_text(&snapshot)),
            }
        }
    }
    Ok(0)
}

fn render_snapshot_text(snapshot: &RecoverySnapshot) -> String {
    let mut rendered = format!(
        "snapshot: {}\ncommand: {}\ncwd: {}\nstatus: {}\nduration: {} ms\n",
        escape_terminal_controls(&snapshot.id),
        escape_terminal_controls(&snapshot.command),
        escape_terminal_controls(&snapshot.cwd),
        snapshot
            .status
            .map_or_else(|| "error".to_owned(), |status| status.to_string()),
        snapshot.duration_ms
    );
    if let Some(stdout) = snapshot.output.stdout.as_deref() {
        rendered.push_str("stdout:\n");
        rendered.push_str(&escape_terminal_controls(stdout));
        if !stdout.ends_with('\n') {
            rendered.push('\n');
        }
    }
    if let Some(stderr) = snapshot.output.stderr.as_deref() {
        rendered.push_str("stderr:\n");
        rendered.push_str(&escape_terminal_controls(stderr));
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
        rendered.push_str(&escape_terminal_controls(error));
        rendered.push('\n');
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

fn is_secret_parameter(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '.'], "_");
    matches!(
        key.as_str(),
        "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "api_key"
            | "apikey"
            | "client_secret"
            | "secret"
            | "password"
            | "passwd"
            | "authorization"
            | "auth"
            | "signature"
            | "sig"
            | "x_amz_signature"
    ) || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_password")
        || key.ends_with("_passwd")
        || key.ends_with("_api_key")
        || key.ends_with("_auth")
        || key.ends_with("_signature")
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
    let mut authorization_scheme_next = false;
    let mut authorization_credential_next = false;
    let mut redact_authorization_line = false;
    for (start, end) in spans {
        let separator = &redacted[cursor..start];
        rendered.push_str(separator);
        if separator.contains(['\n', '\r']) {
            redact_next = false;
            authorization_scheme_next = false;
            authorization_credential_next = false;
            redact_authorization_line = false;
        }
        let token = &redacted[start..end];
        if redact_authorization_line {
            rendered.push_str(&redacted_token(token));
        } else if authorization_credential_next {
            rendered.push_str(&redacted_token(token));
            authorization_credential_next = false;
        } else if authorization_scheme_next {
            authorization_scheme_next = false;
            match authorization_scheme(token) {
                Some("digest") => {
                    rendered.push_str(token);
                    redact_authorization_line = true;
                }
                Some(_) => {
                    rendered.push_str(token);
                    authorization_credential_next = true;
                }
                None => rendered.push_str(&redacted_token(token)),
            }
        } else if is_authorization_header_marker(token) {
            rendered.push_str(token);
            authorization_scheme_next = true;
        } else if redact_next {
            rendered.push_str(&redacted_token(token));
            redact_next = false;
        } else if let Some(structured) = redact_structured_token(token) {
            rendered.push_str(&structured);
        } else if let Some((key, value)) = token.split_once('=') {
            if !key.contains("://") && is_secret_parameter(key.trim_start_matches('-')) {
                rendered.push_str(key);
                rendered.push('=');
                rendered.push_str(&redacted_token(value));
            } else {
                rendered.push_str(token);
            }
        } else {
            rendered.push_str(token);
            redact_next = is_secret_parameter(token.trim_start_matches('-'));
        }
        cursor = end;
    }
    rendered.push_str(&redacted[cursor..]);
    rendered
}

fn authorization_scheme(token: &str) -> Option<&str> {
    let token = token.trim_matches(['\'', '"']).to_ascii_lowercase();
    match token.as_str() {
        "basic" => Some("basic"),
        "bearer" => Some("bearer"),
        "digest" => Some("digest"),
        "negotiate" => Some("negotiate"),
        "aws4-hmac-sha256" => Some("aws4-hmac-sha256"),
        _ => None,
    }
}

fn is_authorization_header_marker(token: &str) -> bool {
    matches!(
        token
            .trim_matches(['\'', '"'])
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "authorization:" | "proxy-authorization:"
    )
}

fn redact_structured_token(token: &str) -> Option<String> {
    let redacted_url = redact_url_credentials(token);
    if redacted_url != token {
        return Some(redacted_url);
    }
    if contains_embedded_authorization_header(token) || looks_like_common_secret(token) {
        return Some(redacted_token(token));
    }
    None
}

fn contains_embedded_authorization_header(token: &str) -> bool {
    let lowercase = token.to_ascii_lowercase();
    lowercase.contains("authorization:") || lowercase.contains("proxy-authorization:")
}

fn looks_like_common_secret(token: &str) -> bool {
    let token = token.trim_matches(|character: char| {
        character.is_ascii_whitespace()
            || matches!(character, '\'' | '"' | ',' | ';' | '(' | ')' | '[' | ']')
    });
    let strong_prefix = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "xoxa-",
        "xoxr-",
        "sk_live_",
        "sk_test_",
        "rk_live_",
        "rk_test_",
    ]
    .iter()
    .any(|prefix| token.starts_with(prefix) && token.len() >= prefix.len() + 8);
    let aws_access_key = token.len() == 20
        && token.starts_with("AKIA")
        && token
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit());
    let jwt = token.len() >= 32
        && token.starts_with("eyJ")
        && token.split('.').count() == 3
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    strong_prefix || aws_access_key || jwt
}

fn redact_url_credentials(token: &str) -> String {
    let Some(scheme) = token.find("://") else {
        return token.to_owned();
    };
    let authority_start = scheme + 3;
    let authority_end = token[authority_start..]
        .find(['/', '?', '#'])
        .map_or(token.len(), |offset| authority_start + offset);
    let mut ranges = Vec::new();
    if let Some(at_offset) = token[authority_start..authority_end].rfind('@') {
        let at = authority_start + at_offset;
        if let Some(colon_offset) = token[authority_start..at].find(':') {
            ranges.push((authority_start + colon_offset + 1, at));
        }
    }
    if let Some(query_offset) = token[authority_end..].find('?') {
        let mut parameter_start = authority_end + query_offset + 1;
        while parameter_start < token.len() {
            let parameter_end = token[parameter_start..]
                .find(['&', ';', '#', '\'', '"'])
                .map_or(token.len(), |offset| parameter_start + offset);
            if let Some(equal_offset) = token[parameter_start..parameter_end].find('=') {
                let equal = parameter_start + equal_offset;
                if is_secret_parameter(&token[parameter_start..equal]) && equal + 1 < parameter_end
                {
                    ranges.push((equal + 1, parameter_end));
                }
            }
            if parameter_end >= token.len()
                || matches!(token.as_bytes()[parameter_end], b'#' | b'\'' | b'"')
            {
                break;
            }
            parameter_start = parameter_end + 1;
        }
    }
    if ranges.is_empty() {
        return token.to_owned();
    }
    ranges.sort_unstable_by_key(|(start, _)| *start);
    let mut output = String::with_capacity(token.len());
    let mut cursor = 0;
    for (start, end) in ranges {
        if start < cursor {
            continue;
        }
        output.push_str(&token[cursor..start]);
        output.push_str("[redacted]");
        cursor = end;
    }
    output.push_str(&token[cursor..]);
    output
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

    fn test_snapshot(id: &str) -> RecoverySnapshot {
        RecoverySnapshot {
            document_type: DOCUMENT_TYPE.to_owned(),
            schema_version: RECOVERY_SCHEMA_VERSION,
            id: id.to_owned(),
            created_unix_ms: 1,
            command: "false".to_owned(),
            cwd: "/tmp".to_owned(),
            environment: EnvironmentDiff::default(),
            output: CapturedOutput::default(),
            duration_ms: 1,
            status: Some(1),
            error_chain: vec!["failed".to_owned()],
        }
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
        #[cfg(unix)]
        {
            let directory_mode = fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
            let snapshot_mode = fs::metadata(directory.join(format!("{id}.json")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(directory_mode, 0o700);
            assert_eq!(snapshot_mode, 0o600);
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn partial_write_and_install_failures_preserve_owned_snapshot_entries() {
        for failed_stage in [
            SnapshotWriteStage::PartialWrite,
            SnapshotWriteStage::Installed,
        ] {
            let directory = test_directory("write-failure");
            fs::create_dir(&directory).unwrap();
            #[cfg(unix)]
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let journal = RecoveryJournal::new(directory.clone(), BTreeMap::new());
            let snapshot = test_snapshot("0001-0001-0001");

            let error = journal
                .write_snapshot_with_hook(&snapshot, |stage| {
                    if stage == failed_stage {
                        Err(io::Error::other("injected snapshot failure"))
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();

            assert_eq!(error.code, ErrorCode::Io);
            assert!(error
                .details
                .context
                .iter()
                .any(|context| context.contains("injected snapshot failure")));
            let expected_entries = if failed_stage == SnapshotWriteStage::Installed {
                2
            } else {
                1
            };
            assert_eq!(fs::read_dir(&directory).unwrap().count(), expected_entries);
            assert!(error
                .details
                .context
                .iter()
                .any(|context| context.contains("failure cleanup preserved recovery")));
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_failure_preserves_the_originating_snapshot_error() {
        let directory = test_directory("cleanup-failure");
        fs::create_dir(&directory).unwrap();
        let journal = RecoveryJournal::new(directory.clone(), BTreeMap::new());
        let snapshot = test_snapshot("0001-0001-0001");

        let error = journal
            .write_snapshot_with_hook(&snapshot, |stage| {
                if stage == SnapshotWriteStage::PartialWrite {
                    let temporary = fs::read_dir(&directory)?
                        .next()
                        .ok_or_else(|| io::Error::other("temporary was not visible"))??
                        .path();
                    fs::remove_file(&temporary)?;
                    fs::create_dir(&temporary)?;
                    return Err(io::Error::other("injected primary failure"));
                }
                Ok(())
            })
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::Io);
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("injected primary failure")));
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("failure cleanup preserved recovery temporary")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_temporary_replacement_is_preserved_during_rollback() {
        let directory = test_directory("concurrent-temporary");
        fs::create_dir(&directory).unwrap();
        let journal = RecoveryJournal::new(directory.clone(), BTreeMap::new());
        let snapshot = test_snapshot("0001-0001-0001");
        let moved = directory.join("moved-owned-temporary");

        let error = journal
            .write_snapshot_with_hook(&snapshot, |stage| {
                if stage == SnapshotWriteStage::ContentSynced {
                    let temporary = fs::read_dir(&directory)?
                        .next()
                        .ok_or_else(|| io::Error::other("temporary was not visible"))??
                        .path();
                    fs::rename(&temporary, &moved)?;
                    fs::write(&temporary, b"foreign")?;
                    return Err(io::Error::other("injected temporary replacement"));
                }
                Ok(())
            })
            .unwrap_err();

        let replacement = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| entry != &moved)
            .unwrap();
        assert_eq!(fs::read(replacement).unwrap(), b"foreign");
        assert!(moved.exists());
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("failure cleanup preserved recovery temporary")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_destination_replacement_is_preserved_during_rollback() {
        let directory = test_directory("concurrent-install");
        fs::create_dir(&directory).unwrap();
        let journal = RecoveryJournal::new(directory.clone(), BTreeMap::new());
        let snapshot = test_snapshot("0001-0001-0001");
        let destination = directory.join("0001-0001-0001.json");
        let moved_destination = directory.join("moved-owned-snapshot");
        let moved_temporary = directory.join("moved-owned-temporary");

        let error = journal
            .write_snapshot_with_hook(&snapshot, |stage| {
                if stage == SnapshotWriteStage::Installed {
                    let temporary = fs::read_dir(&directory)?
                        .map(|entry| entry.map(|entry| entry.path()))
                        .find(|entry| entry.as_ref().is_ok_and(|entry| entry != &destination))
                        .ok_or_else(|| io::Error::other("temporary was not visible"))??;
                    fs::rename(&temporary, &moved_temporary)?;
                    fs::rename(&destination, &moved_destination)?;
                    fs::write(&temporary, b"foreign")?;
                    fs::hard_link(&temporary, &destination)?;
                    fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))?;
                    return Err(io::Error::other("injected post-install failure"));
                }
                Ok(())
            })
            .unwrap_err();

        assert_eq!(fs::read(&destination).unwrap(), b"foreign");
        let replacement_temporary = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry != &destination && entry != &moved_temporary && entry != &moved_destination
            })
            .unwrap();
        assert_eq!(fs::read(replacement_temporary).unwrap(), b"foreign");
        assert!(moved_destination.exists());
        assert!(moved_temporary.exists());
        assert!(
            error
                .details
                .context
                .iter()
                .filter(|context| context.contains("failure cleanup preserved recovery"))
                .count()
                >= 2
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recovery_directory_entry_limit_accepts_exactly_limit_and_rejects_limit_plus_one() {
        let directory = test_directory("entry-limit");
        fs::create_dir(&directory).unwrap();
        let journal = RecoveryJournal::new(directory.clone(), BTreeMap::new());
        for index in 0..MAX_RECOVERY_DIRECTORY_ENTRIES {
            fs::write(directory.join(format!("ignored-{index}")), b"").unwrap();
        }
        assert!(journal.ids().unwrap().is_empty());
        fs::write(directory.join("one-too-many"), b"").unwrap();
        assert_eq!(journal.ids().unwrap_err().code, ErrorCode::ResourceLimit);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_ids_cannot_escape_the_recovery_directory() {
        let journal = RecoveryJournal::new(test_directory("traversal"), BTreeMap::new());
        let error = journal.read("../../secret").unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_symlink_cannot_escape_the_recovery_directory() {
        use std::os::unix::fs::symlink;

        let root = test_directory("symlink");
        let directory = root.join("journal");
        fs::create_dir_all(&directory).unwrap();
        let outside = root.join("outside.json");
        fs::write(&outside, b"{}").unwrap();
        let id = "00000000000000000001-0000000001-00000000000000000001";
        symlink(&outside, directory.join(format!("{id}.json"))).unwrap();
        let journal = RecoveryJournal::new(directory, BTreeMap::new());

        let error = journal.read(id).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("not a regular file"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recovery_directory_creation_rejects_intermediate_symlinks() {
        use std::os::unix::fs::symlink;

        let root = test_directory("directory-symlink");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let link = root.join("link");
        symlink(&outside, &link).unwrap();

        let error = create_recovery_directories(&link.join("nested")).unwrap_err();

        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!outside.join("nested").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn text_rendering_escapes_ansi_osc_and_carriage_return_controls() {
        let hostile = "visible\u{1b}[31mred\u{1b}[0m\u{1b}]0;owned\u{7}\rreplace\u{009b}31m";
        let snapshot = RecoverySnapshot {
            document_type: DOCUMENT_TYPE.to_owned(),
            schema_version: RECOVERY_SCHEMA_VERSION,
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
    fn recovery_v1_migrates_without_unredacting_or_losing_captured_output() {
        let id = "0001-0001-0001";
        let legacy = LegacyRecoverySnapshotV1 {
            document_type: DOCUMENT_TYPE.to_owned(),
            schema_version: 1,
            id: id.to_owned(),
            created_unix_ms: 42,
            output: LegacyCapturedOutputV1 {
                stdout: Some("[redacted] output".to_owned()),
                stderr: None,
                truncated: true,
            },
            duration_ms: 7,
            status: Some(1),
            error_chain: vec!["[redacted] failure".to_owned()],
        };
        let migrated = decode_snapshot(&serde_json::to_vec(&legacy).unwrap(), id).unwrap();
        assert_eq!(migrated.schema_version, RECOVERY_SCHEMA_VERSION);
        assert_eq!(migrated.output.stdout.as_deref(), Some("[redacted] output"));
        assert_eq!(migrated.command, "[unavailable in recovery schema v1]");
        assert!(migrated.environment.changed.is_empty());

        let mut future = serde_json::to_value(&migrated).unwrap();
        future["schema_version"] = serde_json::json!(3);
        assert!(decode_snapshot(&serde_json::to_vec(&future).unwrap(), id).is_err());
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
            let path = directory.join(format!("{id}.json"));
            fs::write(&path, vec![b'x'; MAX_SNAPSHOT_BYTES as usize - 1]).unwrap();
            #[cfg(unix)]
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
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
        let oversized_path = directory.join(format!("{oversized_id}.json"));
        fs::write(&oversized_path, vec![b'x'; MAX_SNAPSHOT_BYTES as usize + 1]).unwrap();
        #[cfg(unix)]
        fs::set_permissions(oversized_path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            journal.read(oversized_id).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn structured_redaction_covers_headers_urls_and_common_token_shapes() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.signature_part";
        let source = format!(
            "Authorization: Bearer {jwt}\n\
             Proxy-Authorization: Basic dXNlcjpwYXNzd29yZA==\n\
             Authorization: opaque-without-scheme\n\
             Authorization: Digest username=mufasa realm=test response=digest-secret\n\
             curl -H 'Authorization: Bearer ghp_abcdefghijklmnop' \
             'https://alice:hunter2@example.com/path?access_token=query-secret&safe=visible' \
             xoxb-12345678-secret sk_live_123456789 tokenize=true"
        );
        let redacted = redact_text(&source, &BTreeSet::new());

        for secret in [
            jwt,
            "dXNlcjpwYXNzd29yZA==",
            "opaque-without-scheme",
            "mufasa",
            "digest-secret",
            "ghp_abcdefghijklmnop",
            "hunter2",
            "query-secret",
            "xoxb-12345678-secret",
            "sk_live_123456789",
        ] {
            assert!(!redacted.contains(secret), "secret remained: {secret}");
        }
        assert!(redacted.contains("Authorization: Bearer [redacted]"));
        assert!(redacted.contains("https://alice:[redacted]@example.com"));
        assert!(redacted.contains("safe=visible"));
        assert!(redacted.contains("tokenize=true"));
    }

    #[test]
    fn structured_redaction_leaves_uncredentialed_urls_and_ordinary_ids_intact() {
        let source =
            "https://example.com/search?tokenize=true&signature_style=compact issue.AKIA.short";
        assert_eq!(redact_text(source, &BTreeSet::new()), source);
    }
}
