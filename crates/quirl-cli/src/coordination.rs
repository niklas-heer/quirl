//! Bounded cross-process coordination for replaceable CLI-owned data.

use quirl_core::{ErrorCode, ShellError};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const EXPLICIT_LOCK_ATTEMPTS_MAX: usize = 101;
const EXPLICIT_LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);

static HELD_LOCK_PATHS: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

#[derive(Clone, Copy)]
pub(crate) enum CoordinationKind {
    /// Coordinate plugin lockfile read-modify-write transactions.
    Plugin,
    /// Coordinate catalog indexing, encoding, and atomic publication.
    Catalog,
    /// Coordinate mutable project-index discovery and publication.
    Project,
    /// Coordinate model validation, download, quarantine, and installation.
    #[cfg(test)]
    Model,
    /// Coordinate provider-neutral runtime-asset state and installation.
    Asset,
}

impl CoordinationKind {
    fn label(self) -> &'static str {
        match self {
            Self::Plugin => "plugin-lockfile",
            Self::Catalog => "command-database",
            Self::Project => "project-discovery",
            #[cfg(test)]
            Self::Model => "AI-model",
            Self::Asset => "runtime-asset",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum CoordinationWait {
    /// Try once so interactive control-plane work never waits on another process.
    Background,
    /// Retry with the fixed explicit-command attempt and delay bounds.
    Explicit,
}

/// RAII ownership of one admitted process-local and operating-system lock.
#[derive(Debug)]
pub(crate) struct CoordinationGuard {
    file: Option<File>,
    registry_path: PathBuf,
}

impl Drop for CoordinationGuard {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = File::unlock(&file);
            drop(file);
        }
        release_process_lock(&self.registry_path);
    }
}

/// Acquire a stable sibling lock without using an unbounded file-lock call.
/// Background contention returns `Ok(None)` immediately; explicit contention
/// retries a fixed number of times and then returns `ResourceLimit`.
pub(crate) fn acquire(
    target: &Path,
    kind: CoordinationKind,
    wait: CoordinationWait,
) -> Result<Option<CoordinationGuard>, ShellError> {
    let lock_path = lock_path(target, kind)?;
    let file = open_lock_file(&lock_path, kind)?;
    validate_lock_file(&lock_path, &file, kind)?;
    let registry_path = fs::canonicalize(&lock_path)
        .map_err(|error| coordination_io_error("resolve", &lock_path, kind, error))?;
    let attempts_max = match wait {
        CoordinationWait::Background => 1,
        CoordinationWait::Explicit => EXPLICIT_LOCK_ATTEMPTS_MAX,
    };

    for attempt in 0..attempts_max {
        if reserve_process_lock(&registry_path, kind)? {
            match file.try_lock() {
                Ok(()) => {
                    if let Err(error) = validate_lock_file(&lock_path, &file, kind) {
                        let _ = File::unlock(&file);
                        release_process_lock(&registry_path);
                        return Err(error);
                    }
                    return Ok(Some(CoordinationGuard {
                        file: Some(file),
                        registry_path,
                    }));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    release_process_lock(&registry_path);
                }
                Err(fs::TryLockError::Error(error)) => {
                    release_process_lock(&registry_path);
                    return Err(coordination_io_error("lock", &lock_path, kind, error));
                }
            }
        }
        if attempt.saturating_add(1) < attempts_max {
            thread::sleep(EXPLICIT_LOCK_RETRY_DELAY);
        }
    }

    match wait {
        CoordinationWait::Background => Ok(None),
        CoordinationWait::Explicit => Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("the {} coordination lock remained busy", kind.label()),
        )
        .with_context(format!(
            "attempt limit: {EXPLICIT_LOCK_ATTEMPTS_MAX}; retry delay: {} ms",
            EXPLICIT_LOCK_RETRY_DELAY.as_millis()
        ))
        .with_help("Wait for the other Quirl instance to finish and retry")),
    }
}

fn lock_path(target: &Path, kind: CoordinationKind) -> Result<PathBuf, ShellError> {
    let name = target.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("the {} target has no file name", kind.label()),
        )
        .with_help("Choose a nested file or directory target and retry")
    })?;
    let mut lock_name = OsString::from(".");
    lock_name.push(name);
    lock_name.push(".quirl-lock");
    Ok(target.with_file_name(lock_name))
}

fn open_lock_file(path: &Path, kind: CoordinationKind) -> Result<File, ShellError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(invalid_lock_file(path, kind, "it is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(coordination_io_error("inspect", path, kind, error)),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    options
        .open(path)
        .map_err(|error| coordination_io_error("open", path, kind, error))
}

fn validate_lock_file(path: &Path, file: &File, kind: CoordinationKind) -> Result<(), ShellError> {
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| coordination_io_error("inspect", path, kind, error))?;
    let file_metadata = file
        .metadata()
        .map_err(|error| coordination_io_error("inspect", path, kind, error))?;
    if !path_metadata.file_type().is_file() || !file_metadata.file_type().is_file() {
        return Err(invalid_lock_file(path, kind, "it is not a regular file"));
    }
    #[cfg(unix)]
    {
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(invalid_lock_file(
                path,
                kind,
                "its pathname changed during admission",
            ));
        }
        if file_metadata.nlink() != 1 {
            return Err(
                invalid_lock_file(path, kind, "it has hard-link aliases").with_context(format!(
                    "expected links: 1; observed: {}",
                    file_metadata.nlink()
                )),
            );
        }
        let mode = file_metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(
                invalid_lock_file(path, kind, "it has unsafe writable permissions")
                    .with_context(format!("mode: {mode:#o}; forbidden write bits: 0o022")),
            );
        }
    }
    Ok(())
}

fn reserve_process_lock(path: &Path, kind: CoordinationKind) -> Result<bool, ShellError> {
    let mut held = held_lock_paths().lock().map_err(|_| {
        ShellError::new(
            ErrorCode::Io,
            format!("the {} process-lock registry was poisoned", kind.label()),
        )
        .with_help("Restart Quirl before retrying this operation")
    })?;
    Ok(held.insert(path.to_path_buf()))
}

fn release_process_lock(path: &Path) {
    let mut held = match held_lock_paths().lock() {
        Ok(held) => held,
        Err(poisoned) => poisoned.into_inner(),
    };
    held.remove(path);
}

fn held_lock_paths() -> &'static Mutex<BTreeSet<PathBuf>> {
    HELD_LOCK_PATHS.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn invalid_lock_file(path: &Path, kind: CoordinationKind, reason: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "the {} coordination lock {} is unsafe because {reason}",
            kind.label(),
            path.display()
        ),
    )
    .with_help("Replace it with a private, unlinked regular lock file and retry")
}

fn coordination_io_error(
    action: &str,
    path: &Path,
    kind: CoordinationKind,
    error: io::Error,
) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!(
            "could not {action} the {} coordination lock {}",
            kind.label(),
            path.display()
        ),
    )
    .with_context(error.to_string())
    .with_help("Check the target-directory permissions and retry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        env,
        panic::{AssertUnwindSafe, catch_unwind},
        process::{Command, Stdio},
        sync::atomic::{AtomicU64, Ordering},
        time::Instant,
    };

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);
    const HELPER_MODE: &str = "QUIRL_TEST_COORDINATION_HELPER_MODE";
    const HELPER_TARGET: &str = "QUIRL_TEST_COORDINATION_HELPER_TARGET";
    const HELPER_READY: &str = "QUIRL_TEST_COORDINATION_HELPER_READY";
    const HELPER_RELEASE: &str = "QUIRL_TEST_COORDINATION_HELPER_RELEASE";
    const HELPER_RESULT: &str = "QUIRL_TEST_COORDINATION_HELPER_RESULT";

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "quirl-coordination-test-{name}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn same_process_contention_defers_and_drop_allows_progress() {
        let directory = TestDirectory::new("same-process");
        let target = directory.0.join("catalog.sqlite3");
        let guard = acquire(
            &target,
            CoordinationKind::Catalog,
            CoordinationWait::Background,
        )
        .unwrap()
        .unwrap();
        assert!(
            acquire(
                &target,
                CoordinationKind::Catalog,
                CoordinationWait::Background,
            )
            .unwrap()
            .is_none()
        );
        drop(guard);
        assert!(
            acquire(
                &target,
                CoordinationKind::Catalog,
                CoordinationWait::Background,
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn unwind_releases_the_coordination_lock() {
        let directory = TestDirectory::new("unwind");
        let target = directory.0.join("model");
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = acquire(
                &target,
                CoordinationKind::Model,
                CoordinationWait::Background,
            )
            .unwrap()
            .unwrap();
            panic!("injected owner panic");
        }));
        assert!(
            acquire(
                &target,
                CoordinationKind::Model,
                CoordinationWait::Background,
            )
            .unwrap()
            .is_some()
        );
    }

    #[test]
    fn explicit_contention_has_a_fixed_attempt_bound() {
        let directory = TestDirectory::new("explicit-bound");
        let target = directory.0.join("catalog.sqlite3");
        let _guard = acquire(
            &target,
            CoordinationKind::Catalog,
            CoordinationWait::Background,
        )
        .unwrap()
        .unwrap();
        let started = Instant::now();
        let error = acquire(
            &target,
            CoordinationKind::Catalog,
            CoordinationWait::Explicit,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(error.details.context[0].contains("attempt limit: 101"));
    }

    #[cfg(unix)]
    #[test]
    fn lock_admission_rejects_symlinks_and_hard_links() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = TestDirectory::new("unsafe-lock");
        let target = directory.0.join("catalog.sqlite3");
        let lock = lock_path(&target, CoordinationKind::Catalog).unwrap();
        let foreign = directory.0.join("foreign");
        fs::write(&foreign, b"").unwrap();
        symlink(&foreign, &lock).unwrap();
        assert_eq!(
            acquire(
                &target,
                CoordinationKind::Catalog,
                CoordinationWait::Background,
            )
            .unwrap_err()
            .code,
            ErrorCode::Validation
        );
        fs::remove_file(&lock).unwrap();
        fs::hard_link(&foreign, &lock).unwrap();
        assert_eq!(
            acquire(
                &target,
                CoordinationKind::Catalog,
                CoordinationWait::Background,
            )
            .unwrap_err()
            .code,
            ErrorCode::Validation
        );
        fs::remove_file(&lock).unwrap();
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            acquire(
                &target,
                CoordinationKind::Catalog,
                CoordinationWait::Background,
            )
            .unwrap_err()
            .code,
            ErrorCode::Validation
        );
    }

    #[test]
    fn separate_process_contention_defers_then_later_progresses() {
        let directory = TestDirectory::new("process-contention");
        let target = directory.0.join("catalog.sqlite3");
        let ready = directory.0.join("ready");
        let release = directory.0.join("release");
        let result = directory.0.join("result");
        let mut owner = helper_command("hold", &target, &ready, &release, &result)
            .spawn()
            .unwrap();
        wait_for_path(&ready);

        let loser = helper_command("attempt", &target, &ready, &release, &result)
            .status()
            .unwrap();
        assert!(loser.success());
        assert_eq!(fs::read_to_string(&result).unwrap(), "busy");

        fs::write(&release, b"release").unwrap();
        assert!(owner.wait().unwrap().success());
        fs::remove_file(&result).unwrap();
        let follower = helper_command("attempt", &target, &ready, &release, &result)
            .status()
            .unwrap();
        assert!(follower.success());
        assert_eq!(fs::read_to_string(&result).unwrap(), "acquired");
    }

    #[test]
    fn process_exit_releases_the_operating_system_lock() {
        let directory = TestDirectory::new("process-exit");
        let target = directory.0.join("model");
        let ready = directory.0.join("ready");
        let release = directory.0.join("release");
        let result = directory.0.join("result");
        let status = helper_command("exit", &target, &ready, &release, &result)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(23));
        wait_for_path(&ready);
        let follower = helper_command("attempt", &target, &ready, &release, &result)
            .status()
            .unwrap();
        assert!(follower.success());
        assert_eq!(fs::read_to_string(&result).unwrap(), "acquired");
    }

    #[test]
    #[allow(
        clippy::exit,
        reason = "the subprocess helper must exit while holding the lock to test OS cleanup"
    )]
    fn cross_process_lock_helper() {
        let Ok(mode) = env::var(HELPER_MODE) else {
            return;
        };
        let target = PathBuf::from(env::var_os(HELPER_TARGET).unwrap());
        let ready = PathBuf::from(env::var_os(HELPER_READY).unwrap());
        let release = PathBuf::from(env::var_os(HELPER_RELEASE).unwrap());
        let result = PathBuf::from(env::var_os(HELPER_RESULT).unwrap());
        let acquired = acquire(
            &target,
            CoordinationKind::Catalog,
            CoordinationWait::Background,
        )
        .unwrap();
        match mode.as_str() {
            "hold" => {
                let _guard = acquired.unwrap();
                fs::write(&ready, b"ready").unwrap();
                wait_for_path(&release);
            }
            "attempt" => {
                fs::write(
                    &result,
                    if acquired.is_some() {
                        "acquired"
                    } else {
                        "busy"
                    },
                )
                .unwrap();
            }
            "exit" => {
                let _guard = acquired.unwrap();
                fs::write(&ready, b"ready").unwrap();
                std::process::exit(23);
            }
            _ => panic!("unknown helper mode"),
        }
    }

    fn helper_command(
        mode: &str,
        target: &Path,
        ready: &Path,
        release: &Path,
        result: &Path,
    ) -> Command {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .arg("coordination::tests::cross_process_lock_helper")
            .arg("--exact")
            .arg("--nocapture")
            .env(HELPER_MODE, mode)
            .env(HELPER_TARGET, target)
            .env(HELPER_READY, ready)
            .env(HELPER_RELEASE, release)
            .env(HELPER_RESULT, result)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn wait_for_path(path: &Path) {
        for _ in 0..500 {
            if path.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {}", path.display());
    }
}
