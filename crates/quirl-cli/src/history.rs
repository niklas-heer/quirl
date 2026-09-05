//! Bounded, directory-aware interactive history persistence.
//!
//! # Failure model and invariants
//!
//! Database creation, WAL creation, or recovery of existing SQLite sidecars may
//! fail. The database and every existing sidecar are admitted as regular files
//! and made private before SQLite can read or write command history. New Unix
//! files start with mode 0600; SQLite inherits that mode for new sidecars. No
//! cleanup removes existing history after partial initialization fails.
//!
//! Persisted rows may violate writer limits. SQLite caps encoded cell/row bytes
//! before decoding, and snapshot readers check individual field lengths before
//! allocating strings. Each SQLite row is capped at 69 KiB; command and directory
//! strings are capped at 64 KiB and 4 KiB. A snapshot scans at most 4096 rows,
//! retains at most 8 MiB of text, and keeps a separately bounded deduplication set.
//! Concurrent cooperative SQLite connections retain SQLite's transaction and
//! busy-timeout semantics; an uncooperative replacement of the containing
//! namespace remains outside the portable pathname admission guarantee.

use quirl_core::{ErrorCode, ShellError};
use quirl_syntax::Mode;
use quirl_ui::InteractiveHistoryEntry;
use rusqlite::{Connection, OpenFlags, limits::Limit, params};
use std::{
    collections::HashSet,
    env, fs,
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const HISTORY_ROWS_MAX: usize = 50_000;
const HISTORY_SNAPSHOT_MAX: usize = 4_096;
const HISTORY_RETAINED_BYTES_MAX: usize = 8 * 1024 * 1024;
const HISTORY_COMMAND_BYTES_MAX: usize = 64 * 1024;
const HISTORY_DIRECTORY_BYTES_MAX: usize = 4 * 1024;
// SQLite counts record headers and scalar fields as well as text payloads.
const HISTORY_SQLITE_ROW_BYTES_MAX: i32 = 64 * 1024 + 4 * 1024 + 1024;
const DATABASE_APPLICATION_ID: i64 = 1_364_547_912;
const DATABASE_SCHEMA_VERSION: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);

/// Durable interactive history store, intentionally separate from command intelligence.
pub(crate) struct HistoryDatabase {
    connection: Connection,
}

impl HistoryDatabase {
    /// Open the configured database and validate or initialize its schema.
    pub(crate) fn open_default(legacy_history_path: &Path) -> Result<Self, ShellError> {
        let path = history_database_path(legacy_history_path);
        Self::open(&path)
    }

    fn open(path: &Path) -> Result<Self, ShellError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| database_error(path, error))?;
        }
        // SQLite NOFOLLOW also rejects symbolic-link ancestors, including the
        // standard macOS /var alias. Resolve parents while retaining the final
        // entry for no-follow admission.
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path.file_name().ok_or_else(|| invalid_history_file(path))?;
        let resolved = fs::canonicalize(parent)
            .map_err(|error| database_error(path, error))?
            .join(name);
        let path = resolved.as_path();
        prepare_private_database(path)?;
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(|error| database_error(path, error))?;
        connection
            .set_limit(Limit::SQLITE_LIMIT_LENGTH, HISTORY_SQLITE_ROW_BYTES_MAX)
            .map_err(history_sql_error)?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| database_error(path, error))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(|error| database_error(path, error))?;
        validate_or_stamp_schema(&connection, path)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS history (
                    id INTEGER PRIMARY KEY,
                    command_line TEXT NOT NULL,
                    directory TEXT NOT NULL,
                    started_unix_ms INTEGER NOT NULL,
                    duration_ms INTEGER,
                    exit_status INTEGER,
                    mode TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS history_directory_id
                    ON history(directory, id DESC);
                 CREATE INDEX IF NOT EXISTS history_mode_id
                    ON history(mode, id DESC);",
            )
            .map_err(|error| database_error(path, error))?;
        Ok(Self { connection })
    }

    /// Save one completed execution and prune oldest rows to explicit count and byte bounds.
    pub(crate) fn record(
        &mut self,
        command_line: &str,
        directory: &Path,
        mode: Mode,
        exit_status: i32,
        duration: Option<Duration>,
    ) -> Result<(), ShellError> {
        if command_line.is_empty() || command_line.len() > HISTORY_COMMAND_BYTES_MAX {
            return Ok(());
        }
        let directory = directory.to_string_lossy();
        if directory.len() > HISTORY_DIRECTORY_BYTES_MAX {
            return Ok(());
        }
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let started_unix_ms = i64::try_from(started_unix_ms).unwrap_or(i64::MAX);
        let duration_ms = duration
            .map(|value| value.as_millis())
            .map(|value| i64::try_from(value).unwrap_or(i64::MAX));
        let transaction = self.connection.transaction().map_err(history_sql_error)?;
        transaction
            .execute(
                "INSERT INTO history
                 (command_line, directory, started_unix_ms, duration_ms, exit_status, mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    command_line,
                    directory.as_ref(),
                    started_unix_ms,
                    duration_ms,
                    exit_status,
                    mode.to_string()
                ],
            )
            .map_err(history_sql_error)?;
        prune_history(&transaction)?;
        transaction.commit().map_err(history_sql_error)
    }

    /// Return recent commands for one grammar with a strong same-directory preference.
    pub(crate) fn snapshot(
        &self,
        current_directory: &Path,
        mode: Mode,
    ) -> Result<Vec<InteractiveHistoryEntry>, ShellError> {
        let current_directory = current_directory.to_string_lossy();
        let mut statement = self
            .connection
            .prepare(
                "SELECT command_line, directory, exit_status,
                        length(CAST(command_line AS BLOB)), length(CAST(directory AS BLOB))
                 FROM history WHERE mode = ?2 ORDER BY id DESC LIMIT ?1",
            )
            .map_err(history_sql_error)?;
        let mut rows = statement
            .query(params![
                i64::try_from(HISTORY_SNAPSHOT_MAX).unwrap_or(i64::MAX),
                mode.to_string()
            ])
            .map_err(history_sql_error)?;
        let mut seen = HashSet::with_capacity(HISTORY_SNAPSHOT_MAX);
        let mut history = Vec::with_capacity(HISTORY_SNAPSHOT_MAX);
        let mut retained_bytes = 0_usize;
        while let Some(row) = rows.next().map_err(history_sql_error)? {
            let command_bytes: i64 = row.get(3).map_err(history_sql_error)?;
            let directory_bytes: i64 = row.get(4).map_err(history_sql_error)?;
            validate_persisted_field("command", command_bytes, HISTORY_COMMAND_BYTES_MAX)?;
            validate_persisted_field("directory", directory_bytes, HISTORY_DIRECTORY_BYTES_MAX)?;
            let command_line: String = row.get(0).map_err(history_sql_error)?;
            let directory: String = row.get(1).map_err(history_sql_error)?;
            let status: Option<i32> = row.get(2).map_err(history_sql_error)?;
            let next_bytes = retained_bytes
                .saturating_add(command_line.len())
                .saturating_add(directory.len());
            if next_bytes > HISTORY_RETAINED_BYTES_MAX || !seen.insert(command_line.clone()) {
                continue;
            }
            retained_bytes = next_bytes;
            let is_local = directory == current_directory;
            history.push(InteractiveHistoryEntry {
                command_line,
                directory: Some(directory),
                status,
                rank_bias: if is_local { 4_000 } else { 0 },
            });
        }
        history.reverse();
        Ok(history)
    }
}

fn history_database_path(legacy_history_path: &Path) -> PathBuf {
    env::var_os("QUIRL_HISTORY_DB")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| legacy_history_path.with_file_name("history.sqlite3"))
}

fn validate_or_stamp_schema(connection: &Connection, path: &Path) -> Result<(), ShellError> {
    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|error| database_error(path, error))?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| database_error(path, error))?;
    if application_id == 0 && version == 0 {
        let user_table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| database_error(path, error))?;
        if user_table_count != 0 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "refusing to claim an existing unmarked SQLite database as history",
            )
            .with_context(path.display().to_string())
            .with_help("Set QUIRL_HISTORY_DB to a new file path"));
        }
        connection
            .pragma_update(None, "application_id", DATABASE_APPLICATION_ID)
            .and_then(|()| connection.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION))
            .map_err(|error| database_error(path, error))?;
        return Ok(());
    }
    if application_id == DATABASE_APPLICATION_ID && version == DATABASE_SCHEMA_VERSION {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::Validation,
        "interactive history database has an incompatible schema",
    )
    .with_context(format!(
        "{} has application id {application_id} and schema version {version}",
        path.display()
    ))
    .with_help("Move the database aside and restart Quirl, or set QUIRL_HISTORY_DB"))
}

fn prune_history(transaction: &rusqlite::Transaction<'_>) -> Result<(), ShellError> {
    let mut statement = transaction
        .prepare(
            "SELECT id, length(CAST(command_line AS BLOB)) + length(CAST(directory AS BLOB))
             FROM history ORDER BY id DESC LIMIT ?1",
        )
        .map_err(history_sql_error)?;
    let rows = statement
        .query_map(
            [i64::try_from(HISTORY_ROWS_MAX.saturating_add(1)).unwrap_or(i64::MAX)],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(history_sql_error)?;
    let mut retained_bytes = 0_usize;
    let mut minimum_id = None;
    for (index, row) in rows.enumerate() {
        let (id, bytes) = row.map_err(history_sql_error)?;
        let bytes = usize::try_from(bytes).unwrap_or(usize::MAX);
        if index >= HISTORY_ROWS_MAX
            || retained_bytes.saturating_add(bytes) > HISTORY_RETAINED_BYTES_MAX
        {
            break;
        }
        retained_bytes = retained_bytes.saturating_add(bytes);
        minimum_id = Some(id);
    }
    drop(statement);
    if let Some(minimum_id) = minimum_id {
        transaction
            .execute("DELETE FROM history WHERE id < ?1", [minimum_id])
            .map_err(history_sql_error)?;
    }
    Ok(())
}

fn prepare_private_database(path: &Path) -> Result<(), ShellError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => validate_private_file(path, &file)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            secure_existing_file(path)?;
        }
        Err(error) => return Err(database_error(path, error)),
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let sidecar = PathBuf::from(name);
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => secure_existing_file(&sidecar)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(database_error(&sidecar, error)),
        }
    }
    Ok(())
}

fn secure_existing_file(path: &Path) -> Result<(), ShellError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| database_error(path, error))?;
    if !metadata.file_type().is_file() {
        return Err(invalid_history_file(path));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| database_error(path, error))?;
    validate_private_file(path, &file)
}

fn validate_private_file(path: &Path, file: &File) -> Result<(), ShellError> {
    let named = fs::symlink_metadata(path).map_err(|error| database_error(path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| database_error(path, error))?;
    if !named.file_type().is_file() || !opened.file_type().is_file() {
        return Err(invalid_history_file(path));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if named.dev() != opened.dev() || named.ino() != opened.ino() || opened.nlink() != 1 {
            return Err(invalid_history_file(path));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| database_error(path, error))?;
    }
    Ok(())
}

fn invalid_history_file(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "interactive history files must be regular files without link aliases",
    )
    .with_context(path.display().to_string())
    .with_help("Remove the link or special file, or set QUIRL_HISTORY_DB to a private file")
}

fn validate_persisted_field(label: &str, bytes: i64, limit: usize) -> Result<(), ShellError> {
    if bytes < 0 || usize::try_from(bytes).unwrap_or(usize::MAX) > limit {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("persisted history {label} exceeds its byte limit"),
        )
        .with_context(format!("limit: {limit}; observed: {bytes}"))
        .with_help("Move the invalid history database aside and restart Quirl"));
    }
    Ok(())
}

fn history_sql_error(error: rusqlite::Error) -> ShellError {
    if error.sqlite_error_code() == Some(rusqlite::ErrorCode::TooBig) {
        return ShellError::new(
            ErrorCode::ResourceLimit,
            "history row exceeds its SQLite byte limit",
        )
        .with_context(format!("limit: {HISTORY_SQLITE_ROW_BYTES_MAX}; {}", error))
        .with_help("Move the oversized history database aside and restart Quirl");
    }
    ShellError::new(ErrorCode::Io, "could not update interactive history")
        .with_context(error.to_string())
        .with_help("Check QUIRL_HISTORY_DB and the available disk space")
}

fn database_error(path: &Path, error: impl std::fmt::Display) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not open history database at {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Set QUIRL_HISTORY_DB to a private writable file path")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let path = env::temp_dir().join(format!(
                "quirl-history-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn database(&self) -> PathBuf {
            self.0.join("history.sqlite3")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn database_and_live_sidecars_are_private_before_history_is_recorded() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TestDirectory::new();
        let path = directory.database();
        let mut database = HistoryDatabase::open(&path).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let entry = directory.0.join(format!("history.sqlite3{suffix}"));
            assert_eq!(
                fs::metadata(entry).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        database
            .record(
                "private command",
                Path::new("/work"),
                Mode::Command,
                0,
                None,
            )
            .unwrap();
        // Reproduce the permissions left by older builds while the WAL is live.
        for suffix in ["", "-wal", "-shm"] {
            let entry = directory.0.join(format!("history.sqlite3{suffix}"));
            fs::set_permissions(entry, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let reopened = HistoryDatabase::open(&path).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let entry = directory.0.join(format!("history.sqlite3{suffix}"));
            assert_eq!(
                fs::metadata(entry).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            reopened
                .snapshot(Path::new("/work"), Mode::Command)
                .unwrap()[0]
                .command_line,
            "private command"
        );
    }

    #[cfg(unix)]
    #[test]
    fn redirected_sidecars_are_rejected_without_modifying_their_targets() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = TestDirectory::new();
        let outside = directory.0.join("outside");
        fs::write(&outside, b"preserved").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
        symlink(&outside, directory.0.join("history.sqlite3-wal")).unwrap();
        let error = HistoryDatabase::open(&directory.database()).err().unwrap();
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(fs::read(&outside).unwrap(), b"preserved");
        assert_eq!(
            fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn snapshot_validates_persisted_field_limits_before_allocating_strings() {
        let directory = TestDirectory::new();
        let database = HistoryDatabase::open(&directory.database()).unwrap();
        let command = "x".repeat(HISTORY_COMMAND_BYTES_MAX);
        let cwd = "d".repeat(HISTORY_DIRECTORY_BYTES_MAX);
        database.connection.execute(
            "INSERT INTO history(command_line, directory, started_unix_ms, mode) VALUES (?1, ?2, 0, ?3)",
            params![command, cwd, Mode::Command.to_string()],
        ).unwrap();
        assert_eq!(
            database
                .snapshot(Path::new("/work"), Mode::Command)
                .unwrap()
                .len(),
            1
        );
        for field in ["command_line", "directory"] {
            database
                .connection
                .execute(&format!("UPDATE history SET {field} = {field} || 'x'"), [])
                .unwrap();
            let error = database
                .snapshot(Path::new("/work"), Mode::Command)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            database
                .connection
                .execute(
                    "UPDATE history SET command_line = ?1, directory = ?2",
                    params![command, cwd],
                )
                .unwrap();
        }
    }

    #[test]
    fn sqlite_rejects_oversized_persisted_records_before_snapshot_decoding() {
        let directory = TestDirectory::new();
        let database = HistoryDatabase::open(&directory.database()).unwrap();
        // A separate writer represents a database produced outside the bounded API.
        let writer = Connection::open(directory.database()).unwrap();
        writer.execute(
            "INSERT INTO history(command_line, directory, started_unix_ms, mode) VALUES (?1, '/work', 0, ?2)",
            params!["x".repeat(usize::try_from(HISTORY_SQLITE_ROW_BYTES_MAX).unwrap() + 1), Mode::Command.to_string()],
        ).unwrap();
        let error = database
            .snapshot(Path::new("/work"), Mode::Command)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn snapshot_prefers_the_current_directory_and_deduplicates_commands() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE history (
                id INTEGER PRIMARY KEY,
                command_line TEXT NOT NULL,
                directory TEXT NOT NULL,
                started_unix_ms INTEGER NOT NULL,
                duration_ms INTEGER,
                exit_status INTEGER,
                mode TEXT NOT NULL
             );",
            )
            .unwrap();
        let mut database = HistoryDatabase { connection };
        database
            .record("git status", Path::new("/other"), Mode::Command, 0, None)
            .unwrap();
        database
            .record("git status", Path::new("/work"), Mode::Command, 1, None)
            .unwrap();
        database
            .record("cargo test", Path::new("/other"), Mode::Command, 0, None)
            .unwrap();
        let snapshot = database
            .snapshot(Path::new("/work"), Mode::Command)
            .unwrap();
        assert_eq!(snapshot.len(), 2);
        let local = snapshot
            .iter()
            .find(|entry| entry.command_line == "git status")
            .unwrap();
        assert_eq!(local.rank_bias, 4_000);
        assert_eq!(local.status, Some(1));
    }

    #[test]
    fn snapshot_excludes_commands_from_other_interactive_grammars() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE history (
                id INTEGER PRIMARY KEY,
                command_line TEXT NOT NULL,
                directory TEXT NOT NULL,
                started_unix_ms INTEGER NOT NULL,
                duration_ms INTEGER,
                exit_status INTEGER,
                mode TEXT NOT NULL
             );",
            )
            .unwrap();
        let mut database = HistoryDatabase { connection };
        database
            .record("ls -al", Path::new("/work"), Mode::Command, 0, None)
            .unwrap();
        database
            .record("ls | take 3", Path::new("/work"), Mode::Data, 0, None)
            .unwrap();

        let data = database.snapshot(Path::new("/work"), Mode::Data).unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].command_line, "ls | take 3");
    }

    #[test]
    fn schema_validation_refuses_to_claim_an_unmarked_foreign_database() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE foreign_data (value TEXT NOT NULL);")
            .unwrap();
        let error = validate_or_stamp_schema(&connection, Path::new("foreign.sqlite3"))
            .expect_err("an existing foreign table must not be claimed");
        assert_eq!(error.code, ErrorCode::Validation);
        let application_id: i64 = connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        assert_eq!(application_id, 0);
    }

    #[test]
    fn oversized_history_input_is_rejected_before_sqlite_retention() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE history (
                    id INTEGER PRIMARY KEY,
                    command_line TEXT NOT NULL,
                    directory TEXT NOT NULL,
                    started_unix_ms INTEGER NOT NULL,
                    duration_ms INTEGER,
                    exit_status INTEGER,
                    mode TEXT NOT NULL
                 );",
            )
            .unwrap();
        let mut database = HistoryDatabase { connection };
        database
            .record(
                &"x".repeat(HISTORY_COMMAND_BYTES_MAX.saturating_add(1)),
                Path::new("/work"),
                Mode::Command,
                0,
                None,
            )
            .unwrap();
        let count: i64 = database
            .connection
            .query_row("SELECT count(*) FROM history", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
