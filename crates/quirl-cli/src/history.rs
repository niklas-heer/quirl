//! Bounded, directory-aware interactive history persistence.

use quirl_core::{ErrorCode, ShellError};
use quirl_syntax::Mode;
use quirl_ui::InteractiveHistoryEntry;
use rusqlite::{params, Connection, OpenFlags};
use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const HISTORY_ROWS_MAX: usize = 50_000;
const HISTORY_SNAPSHOT_MAX: usize = 4_096;
const HISTORY_RETAINED_BYTES_MAX: usize = 8 * 1024 * 1024;
const HISTORY_COMMAND_BYTES_MAX: usize = 64 * 1024;
const HISTORY_DIRECTORY_BYTES_MAX: usize = 4 * 1024;
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| database_error(&path, error))?;
        }
        match path.symlink_metadata() {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    "interactive history database must be a regular file",
                )
                .with_context(path.display().to_string())
                .with_help("Remove the link or special file, or set QUIRL_HISTORY_DB"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(database_error(&path, error)),
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| database_error(&path, error))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(|error| database_error(&path, error))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(|error| database_error(&path, error))?;
        validate_or_stamp_schema(&connection, &path)?;
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
                    ON history(directory, id DESC);",
            )
            .map_err(|error| database_error(&path, error))?;
        set_private_permissions(&path)?;
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

    /// Return a recent deduplicated snapshot with a strong same-directory preference.
    pub(crate) fn snapshot(
        &self,
        current_directory: &Path,
    ) -> Result<Vec<InteractiveHistoryEntry>, ShellError> {
        let current_directory = current_directory.to_string_lossy();
        let mut statement = self
            .connection
            .prepare(
                "SELECT command_line, directory, exit_status
                 FROM history ORDER BY id DESC LIMIT ?1",
            )
            .map_err(history_sql_error)?;
        let rows = statement
            .query_map([HISTORY_SNAPSHOT_MAX as i64], |row| {
                let command_line: String = row.get(0)?;
                let directory: String = row.get(1)?;
                let status: Option<i32> = row.get(2)?;
                Ok((command_line, directory, status))
            })
            .map_err(history_sql_error)?;
        let mut seen = HashSet::with_capacity(HISTORY_SNAPSHOT_MAX);
        let mut history = Vec::with_capacity(HISTORY_SNAPSHOT_MAX);
        let mut retained_bytes = 0_usize;
        for row in rows {
            let (command_line, directory, status) = row.map_err(history_sql_error)?;
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
        .query_map([HISTORY_ROWS_MAX.saturating_add(1) as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
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

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), ShellError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| database_error(path, error))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), ShellError> {
    Ok(())
}

fn history_sql_error(error: rusqlite::Error) -> ShellError {
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
        let snapshot = database.snapshot(Path::new("/work")).unwrap();
        assert_eq!(snapshot.len(), 2);
        let local = snapshot
            .iter()
            .find(|entry| entry.command_line == "git status")
            .unwrap();
        assert_eq!(local.rank_bias, 4_000);
        assert_eq!(local.status, Some(1));
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
