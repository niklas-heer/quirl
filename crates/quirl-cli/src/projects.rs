//! Bounded Git-repository discovery and its rebuildable local cache.
//!
//! Startup returns a loading snapshot without opening SQLite on the caller.
//! The existing single worker admits the database, publishes the bounded cached
//! generation, then scans. Early refresh requests remain coalesced. Initial
//! publication cannot replace a snapshot already updated by a foreground visit.
//! An initialization error is retained once and reported through `snapshot`;
//! neither failed loading nor cancellation removes cached data. Cancellation is
//! checked around startup I/O and before traversal, and Drop joins the worker.
//! SQLite retains its 250 ms busy timeout; filesystem syscalls are not assumed
//! interruptible. Snapshots retain at most 16,384 rows and 16 MiB of path bytes,
//! and the targeted request queue retains at most 64 paths.

use crate::{
    bounded_file::{ReadFileOptions, read_optional_regular_file},
    coordination::{self, CoordinationKind, CoordinationWait},
};
use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{MAX_PROJECT_EXCLUDES, MAX_PROJECT_PATH_BYTES, MAX_PROJECT_ROOTS};
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, PermissionsExt},
};

const DATABASE_APPLICATION_ID: i64 = 1_364_548_752;
const DATABASE_SCHEMA_VERSION: i64 = 2;
const DATABASE_SCHEMA_VERSION_V1: i64 = 1;
const BUSY_TIMEOUT: Duration = Duration::from_millis(250);
const DATABASE_BYTES_MAX: usize = 64 * 1024 * 1024;
const DATABASE_WAL_BYTES_MAX: usize = 64 * 1024 * 1024;
const DATABASE_WAL_AUTOCHECKPOINT_PAGES: i64 = 1_024;
const DEFAULT_SCAN_DEADLINE: Duration = Duration::from_secs(20);
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(5 * 60);
const DEFAULT_DEPTH_MAX: usize = 10;
const DEFAULT_ENTRIES_MAX: usize = 1_000_000;
const DEFAULT_DIRECTORIES_MAX: usize = 250_000;
const DEFAULT_REPOSITORIES_MAX: usize = 16_384;
const DEFAULT_RETAINED_PATH_BYTES_MAX: usize = 16 * 1024 * 1024;
const PATH_BYTES_MAX: usize = MAX_PROJECT_PATH_BYTES;
const TARGETED_REFRESHES_MAX: usize = 64;
const TARGETED_ANCESTORS_MAX: usize = 32;
const GIT_POINTER_BYTES_MAX: usize = 4 * 1024;

/// Resource limits applied independently to each complete discovery pass.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProjectDiscoveryLimits {
    /// Maximum directory depth below each configured root.
    pub(crate) depth_max: usize,
    /// Maximum directory entries inspected, including non-directories.
    pub(crate) entries_max: usize,
    /// Maximum directories admitted to the traversal queue.
    pub(crate) directories_max: usize,
    /// Maximum repositories retained by one scan.
    pub(crate) repositories_max: usize,
    /// Maximum total encoded path bytes retained by one scan.
    pub(crate) retained_path_bytes_max: usize,
    /// Wall-clock budget for one complete scan.
    pub(crate) deadline: Duration,
}

impl Default for ProjectDiscoveryLimits {
    fn default() -> Self {
        Self {
            depth_max: DEFAULT_DEPTH_MAX,
            entries_max: DEFAULT_ENTRIES_MAX,
            directories_max: DEFAULT_DIRECTORIES_MAX,
            repositories_max: DEFAULT_REPOSITORIES_MAX,
            retained_path_bytes_max: DEFAULT_RETAINED_PATH_BYTES_MAX,
            deadline: DEFAULT_SCAN_DEADLINE,
        }
    }
}

/// Immutable configuration copied into the background discovery worker.
#[derive(Debug, Clone)]
pub(crate) struct ProjectDiscoveryConfig {
    automatic_roots: Vec<PathBuf>,
    configured_roots: Vec<PathBuf>,
    excluded_subtrees: Vec<PathBuf>,
    limits: ProjectDiscoveryLimits,
    refresh_interval: Duration,
    stale_after: Duration,
    follow_symlinks: bool,
}

impl ProjectDiscoveryConfig {
    /// Translate the validated runtime project policy into traversal-owned paths and bounds.
    pub(crate) fn from_config(
        runtime: &quirl_lua::ProjectsConfig,
    ) -> Result<Option<Self>, ShellError> {
        if runtime.discovery == "disabled" {
            return Ok(None);
        }
        if runtime.discovery != "auto" {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "projects.discovery must be `auto` or `disabled`",
            )
            .with_help("Set projects.discovery to `auto` or `disabled`"));
        }
        let home = env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let mut config = Self {
            automatic_roots: home.iter().cloned().collect(),
            configured_roots: Vec::new(),
            excluded_subtrees: Vec::new(),
            limits: ProjectDiscoveryLimits::default(),
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            stale_after: DEFAULT_STALE_AFTER,
            follow_symlinks: false,
        };
        for root in &runtime.roots {
            config.add_configured_root(expand_configured_path(root, home.as_deref())?)?;
        }
        config.follow_symlinks = runtime.follow_symlinks;
        for excluded in &runtime.excludes {
            let expanded = expand_configured_path(excluded, home.as_deref())?;
            let exclusion = if config.follow_symlinks {
                fs::canonicalize(&expanded).unwrap_or(expanded)
            } else {
                expanded
            };
            config.add_excluded_subtree(exclusion)?;
        }
        if config.automatic_roots.is_empty() && config.configured_roots.is_empty() {
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                "project discovery has no usable root",
            )
            .with_help("Set HOME or add an absolute path to projects.roots"));
        }
        config.limits.depth_max = usize::from(runtime.max_depth);
        config.refresh_interval = Duration::from_secs(u64::from(runtime.refresh_interval_seconds));
        config.stale_after = config.refresh_interval;
        Ok(Some(config))
    }

    /// Add a user-selected root that remains distinguished from automatic roots.
    pub(crate) fn add_configured_root(&mut self, root: PathBuf) -> Result<(), ShellError> {
        if self.configured_roots.contains(&root) || self.automatic_roots.contains(&root) {
            return Ok(());
        }
        let observed = self.configured_roots.len().saturating_add(1);
        if observed > MAX_PROJECT_ROOTS {
            return Err(project_limit_error(
                "configured project discovery roots",
                MAX_PROJECT_ROOTS,
                observed,
            ));
        }
        self.configured_roots.push(root);
        Ok(())
    }

    /// Exclude one exact directory subtree in addition to platform cache exclusions.
    pub(crate) fn add_excluded_subtree(&mut self, path: PathBuf) -> Result<(), ShellError> {
        validate_path_bound(&path)?;
        if self.excluded_subtrees.contains(&path) {
            return Ok(());
        }
        let observed = self.excluded_subtrees.len().saturating_add(1);
        if observed > MAX_PROJECT_EXCLUDES {
            return Err(project_limit_error(
                "configured project exclusions",
                MAX_PROJECT_EXCLUDES,
                observed,
            ));
        }
        self.excluded_subtrees.push(path);
        Ok(())
    }

    #[cfg(test)]
    fn for_root(root: PathBuf, limits: ProjectDiscoveryLimits) -> Self {
        Self {
            automatic_roots: vec![root],
            configured_roots: Vec::new(),
            excluded_subtrees: Vec::new(),
            limits,
            refresh_interval: Duration::from_secs(60),
            stale_after: Duration::ZERO,
            follow_symlinks: false,
        }
    }
}

/// How a repository entered the project index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectSource {
    /// Found below a root inferred by Quirl.
    Automatic,
    /// Found below a root explicitly supplied by the user.
    Configured,
    /// Learned from a directory-change or Git-command hint.
    Visited,
}

impl ProjectSource {
    fn database_value(self) -> i64 {
        match self {
            Self::Automatic => 0,
            Self::Configured => 1,
            Self::Visited => 2,
        }
    }

    fn from_database(value: i64) -> Result<Self, rusqlite::Error> {
        match value {
            0 => Ok(Self::Automatic),
            1 => Ok(Self::Configured),
            2 => Ok(Self::Visited),
            _ => Err(rusqlite::Error::IntegralValueOutOfRange(0, value)),
        }
    }
}

/// One cached repository suitable for fuzzy project-picker candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectRepository {
    /// Losslessly decoded repository working-tree path on Unix.
    pub(crate) path: PathBuf,
    /// Short display name derived from the final path component.
    pub(crate) name: OsString,
    /// Inferred cluster root used to explain automatic discovery.
    pub(crate) inferred_root: PathBuf,
    /// Confidence in the inferred root, expressed from 0 through 1,000.
    pub(crate) inferred_root_confidence: u16,
    /// Signal that originally admitted this repository.
    pub(crate) source: ProjectSource,
    /// Last time this repository was observed by a complete or targeted scan.
    pub(crate) last_seen_unix_ms: u64,
    /// Newest cheap working-tree or Git-metadata timestamp observed during discovery.
    pub(crate) observed_activity_unix_ms: Option<u64>,
    /// Last time the project was opened through Quirl, when recorded.
    pub(crate) last_opened_unix_ms: Option<u64>,
    /// Saturating project-open count for frecency ranking.
    pub(crate) open_count: u64,
}

/// A bounded, immutable view of the latest locally cached generation.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectSnapshot {
    /// Complete generation number; partial scans never advance it.
    pub(crate) generation: u64,
    /// Wall-clock timestamp of the latest complete generation.
    pub(crate) last_complete_unix_ms: Option<u64>,
    /// Timestamp of the latest full-scan attempt, including incomplete attempts.
    pub(crate) last_attempt_unix_ms: Option<u64>,
    /// Current bounded background activity or result state.
    pub(crate) scan_state: ProjectScanState,
    /// Repositories ordered by effective activity, frequency, then path.
    pub(crate) repositories: Vec<ProjectRepository>,
    // Keep the original typed initialization failure for the existing provider
    // error boundary instead of making an unreadable cache look empty.
    startup_error: Option<ShellError>,
}

/// Nonblocking discovery activity exposed to the interactive surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ProjectScanState {
    /// The worker has not yet published the initial cached generation.
    Loading,
    /// Cached results are loaded and no scan has run in this session yet.
    #[default]
    Cached,
    /// The sole admitted process is currently traversing configured roots.
    Scanning,
    /// The latest attempted scan completed and may have removed stale rows.
    Complete,
    /// The latest attempted scan hit an I/O, cancellation, or resource boundary.
    Incomplete,
    /// Another Quirl process owns discovery; cached rows remain available.
    Deferred,
}

/// Summary of one full discovery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectScanReport {
    /// Number of filesystem entries inspected.
    pub(crate) entries_scanned: usize,
    /// Number of directories admitted.
    pub(crate) directories_scanned: usize,
    /// Number of repositories discovered before completion or interruption.
    pub(crate) repositories_found: usize,
    /// Whether this generation was complete and therefore allowed to remove stale rows.
    pub(crate) complete: bool,
}

#[derive(Debug, Clone)]
struct DiscoveredRepository {
    path: PathBuf,
    inferred_root: PathBuf,
    inferred_root_confidence: u16,
    source: ProjectSource,
    observed_activity_unix_ms: Option<u64>,
}

struct ScanOutput {
    report: ProjectScanReport,
    repositories: Vec<DiscoveredRepository>,
    incomplete_error: Option<ShellError>,
}

struct QueuedDirectory {
    path: PathBuf,
    depth: usize,
    root_index: usize,
}

struct ScanRoot {
    path: PathBuf,
    source: ProjectSource,
    #[cfg(unix)]
    device: u64,
}

#[cfg(unix)]
type DirectoryIdentity = (u64, u64);

#[cfg(not(unix))]
type DirectoryIdentity = PathBuf;

/// Session-owned project refresh service with one cancellable worker.
pub(crate) struct ProjectRefresh {
    database_path: PathBuf,
    snapshot: Arc<RwLock<ProjectSnapshot>>,
    changed: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    requests: Arc<(Mutex<RefreshRequests>, Condvar)>,
    stale_after: Duration,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct RefreshRequests {
    full: bool,
    targeted: VecDeque<PathBuf>,
    targeted_set: HashSet<PathBuf>,
}

impl RefreshRequests {
    fn request_full(&mut self) {
        self.full = true;
        self.targeted.clear();
        self.targeted_set.clear();
    }

    fn request_targeted(&mut self, path: PathBuf) -> Result<(), ShellError> {
        if self.full || self.targeted_set.contains(&path) {
            return Ok(());
        }
        let observed = self.targeted.len().saturating_add(1);
        if observed > TARGETED_REFRESHES_MAX {
            return Err(project_limit_error(
                "queued targeted project refreshes",
                TARGETED_REFRESHES_MAX,
                observed,
            ));
        }
        self.targeted_set.insert(path.clone());
        self.targeted.push_back(path);
        Ok(())
    }

    fn drain(&mut self) -> (bool, Vec<PathBuf>) {
        let full = self.full;
        self.full = false;
        let targeted = self.targeted.drain(..).collect();
        self.targeted_set.clear();
        (full, targeted)
    }
}

#[cfg(test)]
#[derive(Default)]
struct ProjectStartupHooks {
    before_open: Option<Box<dyn FnOnce() + Send>>,
    after_cache: Option<Box<dyn FnOnce() + Send>>,
}

impl ProjectRefresh {
    /// Start one background worker that loads cached projects before discovery.
    ///
    /// Returns a loading snapshot without database I/O on the caller. Database
    /// initialization failures are subsequently returned by `snapshot`.
    pub(crate) fn start(config: ProjectDiscoveryConfig) -> Result<Self, ShellError> {
        let path = default_database_path()?;
        Self::start_at(path, config)
    }

    fn start_at(path: PathBuf, config: ProjectDiscoveryConfig) -> Result<Self, ShellError> {
        Self::start_at_inner(
            path,
            config,
            #[cfg(test)]
            ProjectStartupHooks::default(),
        )
    }

    fn start_at_inner(
        path: PathBuf,
        config: ProjectDiscoveryConfig,
        #[cfg(test)] hooks: ProjectStartupHooks,
    ) -> Result<Self, ShellError> {
        let snapshot = Arc::new(RwLock::new(ProjectSnapshot {
            scan_state: ProjectScanState::Loading,
            ..ProjectSnapshot::default()
        }));
        let changed = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let requests = Arc::new((
            Mutex::new(RefreshRequests {
                full: true,
                ..RefreshRequests::default()
            }),
            Condvar::new(),
        ));
        let worker_snapshot = Arc::clone(&snapshot);
        let worker_changed = Arc::clone(&changed);
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_requests = Arc::clone(&requests);
        let worker_path = path.clone();
        let stale_after = config.stale_after;
        let worker = thread::Builder::new()
            .name("quirl-project-discovery".to_owned())
            .spawn(move || {
                project_worker(
                    &worker_path,
                    &config,
                    &worker_snapshot,
                    &worker_changed,
                    &worker_cancelled,
                    &worker_requests,
                    #[cfg(test)]
                    hooks,
                );
            })
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not start project discovery")
                    .with_context(error.to_string())
                    .with_help("Check the process thread limit and restart Quirl")
            })?;
        Ok(Self {
            database_path: path,
            snapshot,
            changed,
            cancelled,
            requests,
            stale_after,
            worker: Some(worker),
        })
    }

    /// Copy the latest bounded project generation without performing filesystem I/O.
    ///
    /// Returns the original initialization error if the worker could not load
    /// the cache; a pending load instead returns an empty `Loading` snapshot.
    pub(crate) fn snapshot(&self) -> Result<ProjectSnapshot, ShellError> {
        self.snapshot
            .read()
            .map_err(|_| {
                ShellError::new(ErrorCode::Io, "the project snapshot lock was poisoned")
                    .with_help("Restart Quirl to create a fresh project worker")
            })
            .and_then(|snapshot| match &snapshot.startup_error {
                Some(error) => Err(error.clone()),
                None => Ok(snapshot.clone()),
            })
    }

    /// Report whether a newer snapshot was published since the preceding call.
    pub(crate) fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }

    /// Coalesce a complete reconciliation request with any one already pending.
    pub(crate) fn request_full_refresh(&self) -> Result<(), ShellError> {
        let mut requests = self.requests.0.lock().map_err(project_request_lock_error)?;
        requests.request_full();
        self.requests.1.notify_one();
        Ok(())
    }

    /// Queue a bounded local probe after changing directory.
    pub(crate) fn hint_directory(&self, directory: &Path) -> Result<(), ShellError> {
        self.request_targeted(directory)
    }

    /// Queue a bounded local probe after a Git command completes.
    pub(crate) fn hint_git_command(&self, directory: &Path) -> Result<(), ShellError> {
        self.request_targeted(directory)
    }

    /// Refresh stale results when the project picker is opened, while returning cached rows.
    pub(crate) fn hint_picker_open(&self) -> Result<(), ShellError> {
        let snapshot = self.snapshot()?;
        let now = unix_time_ms();
        let stale_ms = u64::try_from(self.stale_after.as_millis()).unwrap_or(u64::MAX);
        let stale = snapshot_is_stale(&snapshot, stale_ms, now);
        if stale {
            self.request_full_refresh()?;
        }
        Ok(())
    }

    /// Record a directory change when the exact destination is still a Git repository.
    ///
    /// The validation uses `symlink_metadata` for the directory and marker, so this
    /// cheap foreground path never follows a link before affecting frecency.
    pub(crate) fn record_opened_if_repository(&self, path: &Path) -> Result<bool, ShellError> {
        self.record_repository(path, true)
    }

    /// Publish a completed clone immediately, without recording a directory visit.
    pub(crate) fn record_cloned(&self, path: &Path) -> Result<bool, ShellError> {
        self.record_repository(path, false)
    }

    fn record_repository(&self, path: &Path, opened: bool) -> Result<bool, ShellError> {
        let Some(repository) = admitted_repository(path)? else {
            return Ok(false);
        };
        let mut database = ProjectDatabase::open(&self.database_path)?;
        database.upsert_targeted(&repository)?;
        if opened && !database.record_opened(path)? {
            return Ok(false);
        }
        let mut next = database.snapshot()?;
        let mut snapshot = self.snapshot.write().map_err(|_| {
            ShellError::new(ErrorCode::Io, "the project snapshot lock was poisoned")
                .with_help("Restart Quirl to create a fresh project worker")
        })?;
        // A successful foreground write does not restart a failed worker.
        if let Some(error) = &snapshot.startup_error {
            next.startup_error = Some(error.clone());
            next.scan_state = ProjectScanState::Incomplete;
        }
        *snapshot = next;
        self.changed.store(true, Ordering::Release);
        Ok(true)
    }

    fn request_targeted(&self, directory: &Path) -> Result<(), ShellError> {
        validate_path_bound(directory)?;
        let mut requests = self.requests.0.lock().map_err(project_request_lock_error)?;
        requests.request_targeted(directory.to_path_buf())?;
        self.requests.1.notify_one();
        Ok(())
    }

    /// Signal cancellation; dropping the service additionally joins the worker.
    pub(crate) fn cancel(&self) {
        let _guard = match self.requests.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.cancelled.store(true, Ordering::Release);
        self.requests.1.notify_all();
    }
}

/// Revalidate a canonical checkout before a previously offered project is opened.
pub(crate) fn validate_project_directory(path: &Path) -> Result<bool, ShellError> {
    validate_path_bound(path)?;
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(scan_io_error(path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() || !probe_git_marker(path)? {
        return Ok(false);
    }
    fs::canonicalize(path)
        .map(|canonical| canonical == path)
        .map_err(|error| scan_io_error(path, error))
}

fn admitted_repository(path: &Path) -> Result<Option<DiscoveredRepository>, ShellError> {
    if !validate_project_directory(path)? {
        return Ok(None);
    }
    Ok(Some(DiscoveredRepository {
        path: path.to_path_buf(),
        inferred_root: path.parent().unwrap_or(path).to_path_buf(),
        inferred_root_confidence: 400,
        source: ProjectSource::Visited,
        observed_activity_unix_ms: repository_activity_unix_ms(path, unix_time_ms()),
    }))
}

/// Admit an explicit CLI clone to the shared cache without starting a scanner.
pub(crate) fn record_clone_default(path: &Path) -> Result<bool, ShellError> {
    let Some(repository) = admitted_repository(path)? else {
        return Ok(false);
    };
    ProjectDatabase::open(&default_database_path()?)?.upsert_targeted(&repository)?;
    Ok(true)
}

fn snapshot_is_stale(snapshot: &ProjectSnapshot, stale_ms: u64, now_unix_ms: u64) -> bool {
    snapshot
        .last_attempt_unix_ms
        .or(snapshot.last_complete_unix_ms)
        .is_none_or(|attempted| now_unix_ms.saturating_sub(attempted) >= stale_ms)
}

/// Read the bounded default cache without starting discovery or touching project roots.
pub(crate) fn cached_default() -> Result<ProjectSnapshot, ShellError> {
    ProjectDatabase::open(&default_database_path()?)?.snapshot()
}

/// Run one foreground-coordinated discovery pass and return its published snapshot.
pub(crate) fn refresh_default(
    config: &ProjectDiscoveryConfig,
) -> Result<ProjectSnapshot, ShellError> {
    let path = default_database_path()?;
    let mut database = ProjectDatabase::open(&path)?;
    let _guard =
        coordination::acquire(&path, CoordinationKind::Project, CoordinationWait::Explicit)?
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "project discovery remained owned by another Quirl process",
                )
                .with_help("Wait for background discovery to finish and retry")
            })?;
    let cancelled = AtomicBool::new(false);
    let output = discover_repositories(config, &cancelled);
    database.persist_scan(&output)?;
    if let Some(error) = output.incomplete_error {
        return Err(error);
    }
    let mut snapshot = database.snapshot()?;
    snapshot.scan_state = ProjectScanState::Complete;
    Ok(snapshot)
}

impl Drop for ProjectRefresh {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct ProjectDatabase {
    connection: Connection,
    path: PathBuf,
}

impl ProjectDatabase {
    fn open(path: &Path) -> Result<Self, ShellError> {
        if let Some(parent) = path.parent() {
            let existed = parent.exists();
            fs::create_dir_all(parent).map_err(|error| database_error(path, error))?;
            #[cfg(unix)]
            if !existed {
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .map_err(|error| database_error(path, error))?;
            }
        }
        match path.symlink_metadata() {
            Ok(metadata) => validate_database_file(path, &metadata, DATABASE_BYTES_MAX)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(database_error(path, error)),
        }
        validate_database_sidecar(&database_sidecar_path(path, "-wal"), DATABASE_WAL_BYTES_MAX)?;
        validate_database_sidecar(&database_sidecar_path(path, "-shm"), DATABASE_WAL_BYTES_MAX)?;
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| database_error(path, error))?;
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
        let page_size: i64 = connection
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .map_err(|error| database_error(path, error))?;
        if page_size <= 0 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "project database reported an invalid SQLite page size",
            )
            .with_context(format!("{}: page size {page_size}", path.display()))
            .with_help("Move projects.sqlite3 aside and restart Quirl"));
        }
        let database_bytes_max = i64::try_from(DATABASE_BYTES_MAX).unwrap_or(i64::MAX);
        let page_count_max = database_bytes_max
            .checked_div(page_size)
            .unwrap_or(1)
            .max(1);
        connection
            .pragma_update(None, "max_page_count", page_count_max)
            .and_then(|()| {
                connection.pragma_update(
                    None,
                    "journal_size_limit",
                    i64::try_from(DATABASE_WAL_BYTES_MAX).unwrap_or(i64::MAX),
                )
            })
            .and_then(|()| {
                connection.pragma_update(
                    None,
                    "wal_autocheckpoint",
                    DATABASE_WAL_AUTOCHECKPOINT_PAGES,
                )
            })
            .map_err(|error| database_error(path, error))?;
        initialize_schema(&mut connection, path)?;
        set_private_sidecar_permissions(path)?;
        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    fn snapshot(&self) -> Result<ProjectSnapshot, ShellError> {
        let (row_count, retained_bytes, component_bytes_max): (i64, i64, i64) = self
            .connection
            .query_row(
                "SELECT count(*),
                        coalesce(sum(length(path) + length(name) + length(inferred_root)), 0),
                        coalesce(max(max(length(path), length(name), length(inferred_root))), 0)
                 FROM repositories",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(project_sql_error)?;
        let row_count = usize::try_from(row_count).unwrap_or(usize::MAX);
        let retained_bytes = usize::try_from(retained_bytes).unwrap_or(usize::MAX);
        let component_bytes_max = usize::try_from(component_bytes_max).unwrap_or(usize::MAX);
        if row_count > DEFAULT_REPOSITORIES_MAX {
            return Err(project_limit_error(
                "cached project rows",
                DEFAULT_REPOSITORIES_MAX,
                row_count,
            ));
        }
        if retained_bytes > DEFAULT_RETAINED_PATH_BYTES_MAX {
            return Err(project_limit_error(
                "cached project path bytes",
                DEFAULT_RETAINED_PATH_BYTES_MAX,
                retained_bytes,
            ));
        }
        if component_bytes_max > PATH_BYTES_MAX {
            return Err(project_limit_error(
                "cached project path component bytes",
                PATH_BYTES_MAX,
                component_bytes_max,
            ));
        }
        let (generation, last_complete, last_attempt, last_scan_complete): (
            i64,
            Option<i64>,
            Option<i64>,
            bool,
        ) = self
            .connection
            .query_row(
                "SELECT generation, last_complete_unix_ms, last_attempt_unix_ms,
                        last_scan_complete
                 FROM project_metadata WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(project_sql_error)?;
        let now = unix_time_ms();
        let mut statement = self
            .connection
            .prepare(
                "SELECT path, name, inferred_root, inferred_root_confidence, source, last_seen_unix_ms,
                        min(observed_activity_unix_ms, ?2), last_opened_unix_ms, open_count
                 FROM repositories
                 ORDER BY max(coalesce(last_opened_unix_ms, 0),
                              coalesce(min(observed_activity_unix_ms, ?2), 0)) DESC,
                          open_count DESC, path ASC
                 LIMIT ?1",
            )
            .map_err(project_sql_error)?;
        let rows = statement
            .query_map(
                params![
                    i64::try_from(DEFAULT_REPOSITORIES_MAX).unwrap_or(i64::MAX),
                    i64::try_from(now).unwrap_or(i64::MAX)
                ],
                |row| {
                    let path: Vec<u8> = row.get(0)?;
                    let name: Vec<u8> = row.get(1)?;
                    let inferred_root: Vec<u8> = row.get(2)?;
                    let inferred_root_confidence: i64 = row.get(3)?;
                    let source = ProjectSource::from_database(row.get(4)?)?;
                    let last_seen: i64 = row.get(5)?;
                    let observed_activity: Option<i64> = row.get(6)?;
                    let last_opened: Option<i64> = row.get(7)?;
                    let open_count: i64 = row.get(8)?;
                    Ok(ProjectRepository {
                        path: bytes_to_path(path),
                        name: bytes_to_os_string(name),
                        inferred_root: bytes_to_path(inferred_root),
                        inferred_root_confidence: u16::try_from(inferred_root_confidence)
                            .unwrap_or(0),
                        source,
                        last_seen_unix_ms: u64::try_from(last_seen).unwrap_or(0),
                        observed_activity_unix_ms: observed_activity
                            .and_then(|value| u64::try_from(value).ok()),
                        last_opened_unix_ms: last_opened
                            .and_then(|value| u64::try_from(value).ok()),
                        open_count: u64::try_from(open_count).unwrap_or(0),
                    })
                },
            )
            .map_err(project_sql_error)?;
        let repositories = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(project_sql_error)?;
        Ok(ProjectSnapshot {
            generation: u64::try_from(generation).unwrap_or(0),
            last_complete_unix_ms: last_complete.and_then(|value| u64::try_from(value).ok()),
            last_attempt_unix_ms: last_attempt.and_then(|value| u64::try_from(value).ok()),
            scan_state: if last_scan_complete {
                ProjectScanState::Cached
            } else {
                ProjectScanState::Incomplete
            },
            repositories,
            startup_error: None,
        })
    }

    fn persist_scan(&mut self, output: &ScanOutput) -> Result<(), ShellError> {
        let now = unix_time_ms();
        let transaction = self.connection.transaction().map_err(project_sql_error)?;
        let generation: i64 = transaction
            .query_row(
                "SELECT generation FROM project_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(project_sql_error)?;
        let next_generation = if output.report.complete {
            generation.checked_add(1).ok_or_else(|| {
                ShellError::new(ErrorCode::ResourceLimit, "project generation overflowed")
                    .with_help("Move projects.sqlite3 aside and restart Quirl")
            })?
        } else {
            generation
        };
        for repository in &output.repositories {
            let path = path_to_bytes(&repository.path);
            let name = repository
                .path
                .file_name()
                .map_or_else(Vec::new, os_str_to_bytes);
            let inferred_root = path_to_bytes(&repository.inferred_root);
            transaction
                .execute(
                    "INSERT INTO repositories
                     (path, name, inferred_root, inferred_root_confidence, source, first_seen_unix_ms,
                      last_seen_unix_ms, seen_generation, observed_activity_unix_ms,
                      last_opened_unix_ms, open_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, NULL, 0)
                     ON CONFLICT(path) DO UPDATE SET
                       name = excluded.name,
                       inferred_root = excluded.inferred_root,
                       inferred_root_confidence = excluded.inferred_root_confidence,
                       source = CASE
                           WHEN repositories.source = 1 THEN 1
                           WHEN excluded.source = 1 THEN 1
                           ELSE excluded.source
                       END,
                       last_seen_unix_ms = excluded.last_seen_unix_ms,
                       seen_generation = excluded.seen_generation,
                       observed_activity_unix_ms = CASE
                           WHEN excluded.observed_activity_unix_ms IS NOT NULL
                           THEN excluded.observed_activity_unix_ms
                           ELSE nullif(min(
                               coalesce(repositories.observed_activity_unix_ms, 0),
                               excluded.last_seen_unix_ms
                           ), 0)
                       END",
                    params![
                        path,
                        name,
                        inferred_root,
                        repository.inferred_root_confidence,
                        repository.source.database_value(),
                        i64::try_from(now).unwrap_or(i64::MAX),
                        next_generation,
                        repository
                            .observed_activity_unix_ms
                            .map(|value| value.min(now))
                            .and_then(|value| i64::try_from(value).ok()),
                    ],
                )
                .map_err(project_sql_error)?;
        }
        if output.report.complete {
            transaction
                .execute(
                    "DELETE FROM repositories
                     WHERE source != ?1 AND seen_generation != ?2",
                    params![ProjectSource::Visited.database_value(), next_generation],
                )
                .map_err(project_sql_error)?;
            transaction
                .execute(
                    "UPDATE project_metadata
                     SET generation = ?1, last_complete_unix_ms = ?2,
                         last_attempt_unix_ms = ?2, last_scan_complete = 1
                     WHERE singleton = 1",
                    params![next_generation, i64::try_from(now).unwrap_or(i64::MAX)],
                )
                .map_err(project_sql_error)?;
        } else {
            transaction
                .execute(
                    "UPDATE project_metadata
                     SET last_attempt_unix_ms = ?1, last_scan_complete = 0
                     WHERE singleton = 1",
                    [i64::try_from(now).unwrap_or(i64::MAX)],
                )
                .map_err(project_sql_error)?;
        }
        prune_database(&transaction, now)?;
        transaction.commit().map_err(project_sql_error)?;
        set_private_sidecar_permissions(&self.path)?;
        Ok(())
    }

    fn upsert_targeted(&mut self, repository: &DiscoveredRepository) -> Result<(), ShellError> {
        let now = unix_time_ms();
        let transaction = self.connection.transaction().map_err(project_sql_error)?;
        let generation: i64 = transaction
            .query_row(
                "SELECT generation FROM project_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(project_sql_error)?;
        transaction
            .execute(
                "INSERT INTO repositories
                 (path, name, inferred_root, inferred_root_confidence, source,
                  first_seen_unix_ms, last_seen_unix_ms, seen_generation,
                  observed_activity_unix_ms, last_opened_unix_ms, open_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, NULL, 0)
                 ON CONFLICT(path) DO UPDATE SET
                   name = excluded.name,
                   inferred_root = excluded.inferred_root,
                   inferred_root_confidence = excluded.inferred_root_confidence,
                   source = CASE WHEN repositories.source = 1 THEN 1 ELSE 2 END,
                   last_seen_unix_ms = excluded.last_seen_unix_ms,
                   observed_activity_unix_ms = CASE
                       WHEN excluded.observed_activity_unix_ms IS NOT NULL
                       THEN excluded.observed_activity_unix_ms
                       ELSE nullif(min(
                           coalesce(repositories.observed_activity_unix_ms, 0),
                           excluded.last_seen_unix_ms
                       ), 0)
                   END",
                params![
                    path_to_bytes(&repository.path),
                    repository
                        .path
                        .file_name()
                        .map_or_else(Vec::new, os_str_to_bytes),
                    path_to_bytes(&repository.inferred_root),
                    repository.inferred_root_confidence,
                    ProjectSource::Visited.database_value(),
                    i64::try_from(now).unwrap_or(i64::MAX),
                    generation,
                    repository
                        .observed_activity_unix_ms
                        .map(|value| value.min(now))
                        .and_then(|value| i64::try_from(value).ok()),
                ],
            )
            .map_err(project_sql_error)?;
        prune_database(&transaction, now)?;
        transaction.commit().map_err(project_sql_error)?;
        Ok(())
    }

    fn record_opened(&mut self, path: &Path) -> Result<bool, ShellError> {
        let changed = self
            .connection
            .execute(
                "UPDATE repositories SET last_opened_unix_ms = ?2,
                    open_count = min(open_count + 1, 9223372036854775807)
                 WHERE path = ?1",
                params![
                    path_to_bytes(path),
                    i64::try_from(unix_time_ms()).unwrap_or(i64::MAX)
                ],
            )
            .map_err(project_sql_error)?;
        Ok(changed != 0)
    }
}

fn project_worker(
    path: &Path,
    config: &ProjectDiscoveryConfig,
    snapshot: &RwLock<ProjectSnapshot>,
    changed: &AtomicBool,
    cancelled: &AtomicBool,
    requests: &(Mutex<RefreshRequests>, Condvar),
    #[cfg(test)] mut hooks: ProjectStartupHooks,
) {
    #[cfg(test)]
    if let Some(before_open) = hooks.before_open.take() {
        before_open();
    }
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    let loaded = ProjectDatabase::open(path)
        .and_then(|database| database.snapshot().map(|cached| (database, cached)));
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    let mut database = match loaded {
        Ok((database, cached)) => {
            publish_initial_snapshot(snapshot, changed, Ok(cached));
            database
        }
        Err(error) => {
            publish_initial_snapshot(snapshot, changed, Err(error));
            return;
        }
    };
    #[cfg(test)]
    if let Some(after_cache) = hooks.after_cache.take() {
        after_cache();
    }
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let (full, targeted) = {
            let Ok(mut queued) = requests.0.lock() else {
                return;
            };
            if !queued.full && queued.targeted.is_empty() {
                let Ok((next, wait)) = requests.1.wait_timeout(queued, config.refresh_interval)
                else {
                    return;
                };
                queued = next;
                if wait.timed_out() {
                    queued.request_full();
                }
            }
            queued.drain()
        };
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let mut published = false;
        if full {
            // Full scans are the expensive cross-process operation. A losing
            // interactive shell keeps its worker-loaded snapshot and lets
            // the lock owner publish the next complete generation.
            if let Ok(Some(_guard)) = coordination::acquire(
                path,
                CoordinationKind::Project,
                CoordinationWait::Background,
            ) {
                publish_scan_state(snapshot, changed, ProjectScanState::Scanning);
                let output = discover_repositories(config, cancelled);
                let next_state = if output.report.complete {
                    ProjectScanState::Complete
                } else {
                    ProjectScanState::Incomplete
                };
                if database.persist_scan(&output).is_ok() {
                    published = true;
                } else {
                    publish_scan_state(snapshot, changed, ProjectScanState::Incomplete);
                }
                if published && let Ok(mut next) = database.snapshot() {
                    next.scan_state = next_state;
                    publish_snapshot(snapshot, changed, next);
                    published = false;
                }
            } else if let Ok(mut next) = database.snapshot() {
                next.scan_state = ProjectScanState::Deferred;
                publish_snapshot(snapshot, changed, next);
            } else {
                publish_scan_state(snapshot, changed, ProjectScanState::Deferred);
            }
        } else {
            for directory in targeted {
                if cancelled.load(Ordering::Acquire) {
                    return;
                }
                if let Ok(Some(repository)) = probe_repository_ancestors(&directory, cancelled)
                    && database.upsert_targeted(&repository).is_ok()
                {
                    published = true;
                }
            }
        }
        if published && let Ok(next) = database.snapshot() {
            publish_snapshot(snapshot, changed, next);
        }
    }
}

fn publish_initial_snapshot(
    snapshot: &RwLock<ProjectSnapshot>,
    changed: &AtomicBool,
    loaded: Result<ProjectSnapshot, ShellError>,
) {
    if let Ok(mut current) = snapshot.write() {
        match loaded {
            Ok(cached) if current.scan_state == ProjectScanState::Loading => *current = cached,
            Ok(_) => return,
            Err(error) => {
                // A foreground visit can publish rows while startup is pending.
                // Preserve them, but do not conceal that this worker has failed.
                current.scan_state = ProjectScanState::Incomplete;
                current.startup_error = Some(error);
            }
        }
        changed.store(true, Ordering::Release);
    }
}

fn publish_scan_state(
    snapshot: &RwLock<ProjectSnapshot>,
    changed: &AtomicBool,
    state: ProjectScanState,
) {
    if let Ok(mut current) = snapshot.write() {
        current.scan_state = state;
        changed.store(true, Ordering::Release);
    }
}

fn publish_snapshot(
    snapshot: &RwLock<ProjectSnapshot>,
    changed: &AtomicBool,
    next: ProjectSnapshot,
) {
    if let Ok(mut current) = snapshot.write() {
        *current = next;
        changed.store(true, Ordering::Release);
    }
}

fn discover_repositories(config: &ProjectDiscoveryConfig, cancelled: &AtomicBool) -> ScanOutput {
    let started = Instant::now();
    let scan_unix_ms = unix_time_ms();
    let mut incomplete_error = None;
    let mut roots = Vec::new();
    for (path, source) in config
        .automatic_roots
        .iter()
        .map(|path| (path, ProjectSource::Automatic))
        .chain(
            config
                .configured_roots
                .iter()
                .map(|path| (path, ProjectSource::Configured)),
        )
    {
        if is_excluded_path(path, &config.excluded_subtrees) {
            continue;
        }
        match admit_scan_root(path, source, config.follow_symlinks) {
            Ok(Some(root)) => roots.push(root),
            Ok(None) => {}
            Err(error) => {
                incomplete_error.get_or_insert(error);
            }
        }
    }

    let mut queue = VecDeque::new();
    let mut directories_scanned = 0_usize;
    let mut entries_scanned = 0_usize;
    for (root_index, root) in roots.iter().enumerate() {
        let observed = directories_scanned.saturating_add(1);
        if observed > config.limits.directories_max {
            incomplete_error.get_or_insert_with(|| {
                project_limit_error(
                    "project directories",
                    config.limits.directories_max,
                    observed,
                )
            });
            break;
        }
        directories_scanned = observed;
        let queued = QueuedDirectory {
            path: root.path.clone(),
            depth: 0,
            root_index,
        };
        if root.source == ProjectSource::Configured {
            queue.push_front(queued);
        } else {
            queue.push_back(queued);
        }
    }

    let mut repositories = Vec::new();
    let mut retained_path_bytes = 0_usize;
    let mut visited_directories = HashSet::<DirectoryIdentity>::new();
    for root in &roots {
        match directory_identity(&root.path) {
            Ok(identity) => {
                visited_directories.insert(identity);
            }
            Err(error) => {
                incomplete_error.get_or_insert(error);
            }
        }
    }
    while let Some(directory) = queue.pop_front() {
        if cancelled.load(Ordering::Acquire) {
            incomplete_error = Some(cancelled_error());
            break;
        }
        if started.elapsed() >= config.limits.deadline {
            incomplete_error = Some(project_limit_error(
                "project scan deadline milliseconds",
                usize::try_from(config.limits.deadline.as_millis()).unwrap_or(usize::MAX),
                usize::try_from(started.elapsed().as_millis()).unwrap_or(usize::MAX),
            ));
            break;
        }
        let is_repository = match probe_git_marker(&directory.path) {
            Ok(is_repository) => is_repository,
            Err(error) => {
                incomplete_error.get_or_insert(error);
                // An unreadable marker cannot safely be distinguished from a
                // repository boundary, so conservatively prune this subtree.
                continue;
            }
        };
        if is_repository {
            let encoded_bytes = path_encoded_len(&directory.path);
            let observed_repositories = repositories.len().saturating_add(1);
            let observed_bytes = retained_path_bytes.saturating_add(encoded_bytes);
            if observed_repositories > config.limits.repositories_max {
                incomplete_error = Some(project_limit_error(
                    "discovered repositories",
                    config.limits.repositories_max,
                    observed_repositories,
                ));
                break;
            }
            if encoded_bytes > PATH_BYTES_MAX {
                incomplete_error = Some(project_limit_error(
                    "repository path bytes",
                    PATH_BYTES_MAX,
                    encoded_bytes,
                ));
                break;
            }
            if observed_bytes > config.limits.retained_path_bytes_max {
                incomplete_error = Some(project_limit_error(
                    "retained repository path bytes",
                    config.limits.retained_path_bytes_max,
                    observed_bytes,
                ));
                break;
            }
            retained_path_bytes = observed_bytes;
            let Some(root) = roots.get(directory.root_index) else {
                incomplete_error.get_or_insert_with(internal_root_error);
                break;
            };
            repositories.push(DiscoveredRepository {
                observed_activity_unix_ms: repository_activity_unix_ms(
                    &directory.path,
                    scan_unix_ms,
                ),
                path: directory.path,
                inferred_root: root.path.clone(),
                inferred_root_confidence: 0,
                source: root.source,
            });
            continue;
        }
        if directory.depth >= config.limits.depth_max {
            continue;
        }
        let read = match fs::read_dir(&directory.path) {
            Ok(read) => read,
            Err(error) => {
                incomplete_error.get_or_insert_with(|| scan_io_error(&directory.path, error));
                continue;
            }
        };
        for entry in read {
            if cancelled.load(Ordering::Acquire) {
                incomplete_error = Some(cancelled_error());
                break;
            }
            if started.elapsed() >= config.limits.deadline {
                incomplete_error = Some(project_limit_error(
                    "project scan deadline milliseconds",
                    usize::try_from(config.limits.deadline.as_millis()).unwrap_or(usize::MAX),
                    usize::try_from(started.elapsed().as_millis()).unwrap_or(usize::MAX),
                ));
                break;
            }
            entries_scanned = entries_scanned.saturating_add(1);
            if entries_scanned > config.limits.entries_max {
                incomplete_error = Some(project_limit_error(
                    "project scan directory entries",
                    config.limits.entries_max,
                    entries_scanned,
                ));
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    incomplete_error.get_or_insert_with(|| scan_io_error(&directory.path, error));
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    incomplete_error.get_or_insert_with(|| scan_io_error(&entry.path(), error));
                    continue;
                }
            };
            let traversable_symlink = file_type.is_symlink() && config.follow_symlinks;
            if !(file_type.is_dir() || traversable_symlink) {
                continue;
            }
            let name = entry.file_name();
            let next_depth = directory.depth.saturating_add(1);
            let mut child = entry.path();
            if is_excluded_path(&child, &config.excluded_subtrees) {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(metadata) if metadata.is_dir() => metadata,
                Ok(_) => continue,
                Err(error) => {
                    incomplete_error.get_or_insert_with(|| scan_io_error(&child, error));
                    continue;
                }
            };
            if file_type.is_symlink() {
                child = match fs::canonicalize(&child) {
                    Ok(path) => path,
                    Err(error) => {
                        incomplete_error.get_or_insert_with(|| scan_io_error(&child, error));
                        continue;
                    }
                };
                if is_excluded_path(&child, &config.excluded_subtrees) {
                    continue;
                }
            }
            let child_is_repository = match probe_git_marker(&child) {
                Ok(is_repository) => is_repository,
                Err(error) => {
                    incomplete_error.get_or_insert(error);
                    continue;
                }
            };
            if should_exclude_directory(&name, next_depth, child_is_repository) {
                continue;
            }
            #[cfg(unix)]
            {
                let Some(root) = roots.get(directory.root_index) else {
                    incomplete_error.get_or_insert_with(internal_root_error);
                    break;
                };
                if metadata.dev() != root.device {
                    continue;
                }
            }
            let identity = match directory_identity_from_metadata(&child, &metadata) {
                Ok(identity) => identity,
                Err(error) => {
                    incomplete_error.get_or_insert(error);
                    continue;
                }
            };
            if !visited_directories.insert(identity) {
                continue;
            }
            let observed = directories_scanned.saturating_add(1);
            if observed > config.limits.directories_max {
                incomplete_error = Some(project_limit_error(
                    "project directories",
                    config.limits.directories_max,
                    observed,
                ));
                break;
            }
            directories_scanned = observed;
            let queued = QueuedDirectory {
                path: child,
                depth: next_depth,
                root_index: directory.root_index,
            };
            if is_priority_directory(&name, next_depth) {
                queue.push_front(queued);
            } else {
                queue.push_back(queued);
            }
        }
        if incomplete_error
            .as_ref()
            .is_some_and(|error| error.code == ErrorCode::ResourceLimit)
        {
            break;
        }
    }

    repositories.sort_by(|left, right| left.path.cmp(&right.path));
    repositories.dedup_by(|left, right| left.path == right.path);
    infer_cluster_roots(&mut repositories, &roots);
    ProjectScanReport {
        entries_scanned,
        directories_scanned,
        repositories_found: repositories.len(),
        complete: incomplete_error.is_none(),
    }
    .with_repositories(repositories, incomplete_error)
}

impl ProjectScanReport {
    fn with_repositories(
        self,
        repositories: Vec<DiscoveredRepository>,
        incomplete_error: Option<ShellError>,
    ) -> ScanOutput {
        ScanOutput {
            report: self,
            repositories,
            incomplete_error,
        }
    }
}

fn infer_cluster_roots(repositories: &mut [DiscoveredRepository], roots: &[ScanRoot]) {
    // This bounded heuristic forms one group per immediate child of an automatic
    // root, then compresses each group to its deepest common ancestor. It keeps
    // HOME out of the frontier, yields `~/Code` for branched Code trees and
    // `~/Work/company` for a unary Work/company tree, and is deterministic
    // without depending on locale or fashionable directory names.
    let mut groups = BTreeMap::<(usize, PathBuf), Vec<usize>>::new();
    for (repository_index, repository) in repositories.iter().enumerate() {
        let Some((root_index, root)) = roots
            .iter()
            .enumerate()
            .filter(|(_, root)| repository.path.starts_with(&root.path))
            .max_by_key(|(_, root)| root.path.components().count())
        else {
            continue;
        };
        let group = if root.source == ProjectSource::Configured {
            root.path.clone()
        } else {
            repository
                .path
                .strip_prefix(&root.path)
                .ok()
                .and_then(|relative| relative.components().next())
                .map_or_else(
                    || repository.path.clone(),
                    |component| root.path.join(component),
                )
        };
        groups
            .entry((root_index, group))
            .or_default()
            .push(repository_index);
    }
    for ((root_index, _), indexes) in groups {
        let Some(first_index) = indexes.first().copied() else {
            continue;
        };
        let Some(root) = roots.get(root_index) else {
            debug_assert!(false, "cluster root index must reference its source root");
            continue;
        };
        let Some(first_repository) = repositories.get(first_index) else {
            debug_assert!(false, "cluster member index must reference a repository");
            continue;
        };
        let mut inferred = first_repository.path.clone();
        for index in indexes.iter().skip(1) {
            let Some(repository) = repositories.get(*index) else {
                debug_assert!(false, "cluster member index must reference a repository");
                continue;
            };
            inferred = common_path_ancestor(&inferred, &repository.path, &root.path);
        }
        if root.source == ProjectSource::Configured && indexes.len() > 1 {
            inferred = common_path_ancestor(&inferred, &root.path, &root.path);
        }
        let confidence = if root.source == ProjectSource::Configured {
            1_000
        } else if indexes.len() >= 2 {
            900
        } else {
            500
        };
        for index in indexes {
            let Some(repository) = repositories.get_mut(index) else {
                debug_assert!(false, "cluster member index must reference a repository");
                continue;
            };
            repository.inferred_root.clone_from(&inferred);
            repository.inferred_root_confidence = confidence;
        }
    }
}

fn common_path_ancestor(left: &Path, right: &Path, floor: &Path) -> PathBuf {
    let mut common = PathBuf::new();
    for (left_component, right_component) in left.components().zip(right.components()) {
        if left_component != right_component {
            break;
        }
        common.push(left_component.as_os_str());
    }
    if common.starts_with(floor) {
        common
    } else {
        floor.to_path_buf()
    }
}

fn admit_scan_root(
    path: &Path,
    source: ProjectSource,
    follow_symlinks: bool,
) -> Result<Option<ScanRoot>, ShellError> {
    validate_path_bound(path)?;
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(scan_io_error(path, error)),
    };
    if metadata.file_type().is_symlink() && follow_symlinks {
        let resolved = fs::canonicalize(path).map_err(|error| scan_io_error(path, error))?;
        let metadata = fs::metadata(&resolved).map_err(|error| scan_io_error(&resolved, error))?;
        if !metadata.is_dir() {
            return Err(invalid_discovery_root(path));
        }
        return Ok(Some(ScanRoot {
            path: resolved,
            source,
            #[cfg(unix)]
            device: metadata.dev(),
        }));
    }
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_discovery_root(path));
    }
    Ok(Some(ScanRoot {
        path: path.to_path_buf(),
        source,
        #[cfg(unix)]
        device: metadata.dev(),
    }))
}

fn invalid_discovery_root(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "project discovery root must be a traversable directory",
    )
    .with_context(path.display().to_string())
    .with_help("Choose a directory, or enable projects.follow_symlinks for a linked directory")
}

fn is_excluded_path(path: &Path, excluded_subtrees: &[PathBuf]) -> bool {
    excluded_subtrees
        .iter()
        .any(|excluded| path.starts_with(excluded))
}

#[cfg(unix)]
fn directory_identity(path: &Path) -> Result<DirectoryIdentity, ShellError> {
    let metadata = fs::metadata(path).map_err(|error| scan_io_error(path, error))?;
    directory_identity_from_metadata(path, &metadata)
}

#[cfg(not(unix))]
fn directory_identity(path: &Path) -> Result<DirectoryIdentity, ShellError> {
    fs::canonicalize(path).map_err(|error| scan_io_error(path, error))
}

#[cfg(unix)]
fn directory_identity_from_metadata(
    _path: &Path,
    metadata: &fs::Metadata,
) -> Result<DirectoryIdentity, ShellError> {
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn directory_identity_from_metadata(
    path: &Path,
    _metadata: &fs::Metadata,
) -> Result<DirectoryIdentity, ShellError> {
    directory_identity(path)
}

fn probe_repository_ancestors(
    directory: &Path,
    cancelled: &AtomicBool,
) -> Result<Option<DiscoveredRepository>, ShellError> {
    for path in directory.ancestors().take(TARGETED_ANCESTORS_MAX) {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let Ok(metadata) = path.symlink_metadata() else {
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            continue;
        }
        if probe_git_marker(path)? {
            return Ok(Some(DiscoveredRepository {
                path: path.to_path_buf(),
                inferred_root: path.parent().unwrap_or(path).to_path_buf(),
                inferred_root_confidence: 400,
                source: ProjectSource::Visited,
                observed_activity_unix_ms: repository_activity_unix_ms(path, unix_time_ms()),
            }));
        }
    }
    Ok(None)
}

fn probe_git_marker(path: &Path) -> Result<bool, ShellError> {
    match path.join(".git").symlink_metadata() {
        Ok(metadata) => Ok(metadata.is_dir() || metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(scan_io_error(&path.join(".git"), error)),
    }
}

/// Observe repository activity using a fixed number of metadata reads.
///
/// A directory timestamp notices entries created or removed at the repository
/// root. Git's index, HEAD, and HEAD reflog cover staging, checkout, commit,
/// rebase, and related operations. Existing-file edits that have not reached
/// Git remain represented by Quirl visits until a future filesystem watcher
/// supplies a stronger signal; discovery never recursively scans a worktree.
fn repository_activity_unix_ms(path: &Path, observed_at_unix_ms: u64) -> Option<u64> {
    let mut newest = modified_unix_ms(path).map(|modified| modified.min(observed_at_unix_ms));
    let Some(git_directory) = git_metadata_directory(path) else {
        return newest;
    };
    for candidate in [
        git_directory.clone(),
        git_directory.join("HEAD"),
        git_directory.join("index"),
        git_directory.join("logs/HEAD"),
    ] {
        if let Some(modified) = modified_unix_ms(&candidate) {
            let clamped = modified.min(observed_at_unix_ms);
            newest = Some(newest.map_or(clamped, |current| current.max(clamped)));
        }
    }
    newest
}

fn git_metadata_directory(repository: &Path) -> Option<PathBuf> {
    let marker = repository.join(".git");
    let metadata = marker.symlink_metadata().ok()?;
    if metadata.is_dir() {
        return Some(marker);
    }
    if !metadata.is_file() {
        return None;
    }
    let bytes = read_optional_regular_file(ReadFileOptions {
        path: &marker,
        bytes_max: GIT_POINTER_BYTES_MAX,
        context: "Git directory pointer",
        help: "Repair the repository's bounded .git file before refreshing projects",
        io_error_code: ErrorCode::Io,
    })
    .ok()??;
    let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let value = line.strip_prefix(b"gitdir: ")?;
    if value.is_empty() || value.contains(&b'\n') || value.contains(&0) {
        return None;
    }
    let pointer = PathBuf::from(bytes_to_os_string(value.to_vec()));
    let directory = if pointer.is_absolute() {
        pointer
    } else {
        repository.join(pointer)
    };
    validate_path_bound(&directory).ok()?;
    Some(directory)
}

fn modified_unix_ms(path: &Path) -> Option<u64> {
    let metadata = path.symlink_metadata().ok()?;
    if metadata.file_type().is_symlink() {
        return None;
    }
    metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis()
        .try_into()
        .ok()
}

fn should_exclude_directory(name: &OsStr, depth: usize, is_repository: bool) -> bool {
    let bytes = os_str_to_bytes(name);
    if depth == 1 && matches!(bytes.as_slice(), b"Library" | b"snap") {
        return true;
    }
    if matches!(
        bytes.as_slice(),
        b".cache"
            | b".local"
            | b".var"
            | b".cargo"
            | b".rustup"
            | b".npm"
            | b".pnpm-store"
            | b".gradle"
            | b".m2"
            | b".nuget"
            | b".Trash"
            | b"node_modules"
            | b"target"
            | b"dist"
            | b"build"
    ) {
        return true;
    }
    if is_repository {
        return false;
    }
    if bytes.starts_with(b".") {
        // Hidden directories directly below a root are inspected for a .git marker,
        // but arbitrary hidden trees are neither useful nor predictably bounded.
        return true;
    }
    depth > 1 && matches!(bytes.as_slice(), b"vendor" | b"deps")
}

fn is_priority_directory(name: &OsStr, depth: usize) -> bool {
    depth == 1
        && matches!(
            os_str_to_bytes(name).as_slice(),
            b"Code" | b"code" | b"Projects" | b"projects" | b"Work" | b"work" | b"Developer"
        )
}

fn default_database_path() -> Result<PathBuf, ShellError> {
    if let Some(path) = env::var_os("QUIRL_PROJECTS_DB").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(cache) = env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(cache).join("quirl/projects.sqlite3"));
    }
    env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".cache/quirl/projects.sqlite3"))
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::InvalidArgument,
                "cannot determine the project database path",
            )
            .with_help("Set QUIRL_PROJECTS_DB, XDG_CACHE_HOME, or HOME")
        })
}

fn prune_database(connection: &Connection, now_unix_ms: u64) -> Result<(), ShellError> {
    connection
        .execute(
            "DELETE FROM repositories WHERE rowid IN (
                SELECT rowid FROM (
                    SELECT rowid,
                           row_number() OVER (
                               ORDER BY max(coalesce(last_opened_unix_ms, 0),
                                            coalesce(min(observed_activity_unix_ms, ?3), 0)) DESC,
                                        open_count DESC, last_seen_unix_ms DESC, path ASC
                           ) AS retained_count,
                           sum(length(path) + length(name) + length(inferred_root)) OVER (
                               ORDER BY max(coalesce(last_opened_unix_ms, 0),
                                            coalesce(min(observed_activity_unix_ms, ?3), 0)) DESC,
                                        open_count DESC, last_seen_unix_ms DESC, path ASC
                           ) AS retained_bytes
                    FROM repositories
                )
                WHERE retained_count > ?1 OR retained_bytes > ?2
             )",
            params![
                i64::try_from(DEFAULT_REPOSITORIES_MAX).unwrap_or(i64::MAX),
                i64::try_from(DEFAULT_RETAINED_PATH_BYTES_MAX).unwrap_or(i64::MAX),
                i64::try_from(now_unix_ms).unwrap_or(i64::MAX)
            ],
        )
        .map_err(project_sql_error)?;
    Ok(())
}

fn expand_configured_path(value: &str, home: Option<&Path>) -> Result<PathBuf, ShellError> {
    let path = Path::new(value);
    if path.is_absolute() {
        validate_path_bound(path)?;
        return Ok(path.to_path_buf());
    }
    if path == Path::new("~") {
        return home
            .map(Path::to_path_buf)
            .ok_or_else(missing_home_for_path);
    }
    if let Ok(relative) = path.strip_prefix("~/") {
        let expanded = home.ok_or_else(missing_home_for_path)?.join(relative);
        validate_path_bound(&expanded)?;
        return Ok(expanded);
    }
    Err(ShellError::new(
        ErrorCode::Validation,
        "project paths must be absolute or start with `~/`",
    )
    .with_context(value.to_owned())
    .with_help("Use an absolute path, or set HOME and use a home-relative path"))
}

fn missing_home_for_path() -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        "cannot expand a home-relative project path without HOME",
    )
    .with_help("Set HOME or configure an absolute project path")
}

fn initialize_schema(connection: &mut Connection, path: &Path) -> Result<(), ShellError> {
    // `IMMEDIATE` serializes the read-decide-write sequence. A second opener
    // waits at transaction admission, then re-reads the committed version
    // instead of attempting the same ALTER TABLE concurrently.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(project_sql_error)?;
    validate_or_migrate_schema(&transaction, path)?;
    transaction
        .execute(
            "CREATE TABLE IF NOT EXISTS project_metadata (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                generation INTEGER NOT NULL,
                last_complete_unix_ms INTEGER,
                last_attempt_unix_ms INTEGER,
                last_scan_complete INTEGER NOT NULL
             )",
            [],
        )
        .and_then(|_| {
            transaction.execute(
                "INSERT OR IGNORE INTO project_metadata
                 (singleton, generation, last_complete_unix_ms,
                  last_attempt_unix_ms, last_scan_complete)
                 VALUES (1, 0, NULL, NULL, 1)",
                [],
            )
        })
        .and_then(|_| {
            transaction.execute(
                "CREATE TABLE IF NOT EXISTS repositories (
                    path BLOB PRIMARY KEY,
                    name BLOB NOT NULL,
                    inferred_root BLOB NOT NULL,
                    inferred_root_confidence INTEGER NOT NULL,
                    source INTEGER NOT NULL CHECK (source BETWEEN 0 AND 2),
                    first_seen_unix_ms INTEGER NOT NULL,
                    last_seen_unix_ms INTEGER NOT NULL,
                    seen_generation INTEGER NOT NULL,
                    observed_activity_unix_ms INTEGER,
                    last_opened_unix_ms INTEGER,
                    open_count INTEGER NOT NULL DEFAULT 0
                 )",
                [],
            )
        })
        .and_then(|_| {
            transaction.execute(
                "CREATE INDEX IF NOT EXISTS repositories_rank ON repositories(
                    max(coalesce(last_opened_unix_ms, 0),
                        coalesce(observed_activity_unix_ms, 0)) DESC,
                    open_count DESC
                 )",
                [],
            )
        })
        .map_err(|error| database_error(path, error))?;
    transaction.commit().map_err(project_sql_error)
}

fn validate_or_migrate_schema(connection: &Connection, path: &Path) -> Result<(), ShellError> {
    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|error| database_error(path, error))?;
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| database_error(path, error))?;
    if application_id == 0 && version == 0 {
        let table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(|error| database_error(path, error))?;
        if table_count != 0 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "refusing to claim an existing unmarked SQLite database as projects",
            )
            .with_context(path.display().to_string())
            .with_help("Set QUIRL_PROJECTS_DB to a new file path"));
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
    if application_id == DATABASE_APPLICATION_ID && version == DATABASE_SCHEMA_VERSION_V1 {
        connection
            .execute(
                "ALTER TABLE repositories ADD COLUMN observed_activity_unix_ms INTEGER",
                [],
            )
            .and_then(|_| connection.execute("DROP INDEX IF EXISTS repositories_rank", []))
            .and_then(|_| connection.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION))
            .map_err(project_sql_error)?;
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::Validation,
        "project database has an incompatible schema",
    )
    .with_context(format!(
        "{} has application id {application_id} and schema version {version}",
        path.display()
    ))
    .with_help("Move projects.sqlite3 aside and restart Quirl, or set QUIRL_PROJECTS_DB"))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), ShellError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| database_error(path, error))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), ShellError> {
    Ok(())
}

fn set_private_sidecar_permissions(path: &Path) -> Result<(), ShellError> {
    set_private_permissions(path)?;
    #[cfg(unix)]
    for suffix in ["-wal", "-shm"] {
        let sidecar = database_sidecar_path(path, suffix);
        match sidecar.symlink_metadata() {
            Ok(metadata) if metadata.is_file() => {
                fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600))
                    .map_err(|error| database_error(&sidecar, error))?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(database_error(&sidecar, error)),
        }
    }
    Ok(())
}

fn database_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar_name = path.as_os_str().to_os_string();
    sidecar_name.push(suffix);
    PathBuf::from(sidecar_name)
}

fn validate_database_sidecar(path: &Path, bytes_max: usize) -> Result<(), ShellError> {
    match path.symlink_metadata() {
        Ok(metadata) => validate_database_file(path, &metadata, bytes_max),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(database_error(path, error)),
    }
}

fn validate_database_file(
    path: &Path,
    metadata: &fs::Metadata,
    bytes_max: usize,
) -> Result<(), ShellError> {
    if !metadata.file_type().is_file() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "project database files must be regular files",
        )
        .with_context(path.display().to_string())
        .with_help("Remove the unsafe database file or set QUIRL_PROJECTS_DB to a private path"));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "project database files must not have multiple hard links",
        )
        .with_context(format!("{} has {} links", path.display(), metadata.nlink()))
        .with_help("Remove the extra hard link or set QUIRL_PROJECTS_DB to a private path"));
    }
    validate_database_bytes(path, metadata.len(), bytes_max)
}

fn validate_database_bytes(path: &Path, bytes: u64, bytes_max: usize) -> Result<(), ShellError> {
    let observed = usize::try_from(bytes).unwrap_or(usize::MAX);
    if observed > bytes_max {
        return Err(
            project_limit_error("project database bytes", bytes_max, observed)
                .with_context(path.display().to_string()),
        );
    }
    Ok(())
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn validate_path_bound(path: &Path) -> Result<(), ShellError> {
    let observed = path_encoded_len(path);
    if observed > PATH_BYTES_MAX {
        return Err(project_limit_error(
            "project path bytes",
            PATH_BYTES_MAX,
            observed,
        ));
    }
    Ok(())
}

fn path_encoded_len(path: &Path) -> usize {
    os_str_to_bytes(path.as_os_str()).len()
}

#[cfg(unix)]
fn os_str_to_bytes(value: &OsStr) -> Vec<u8> {
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_str_to_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().into_owned().into_bytes()
}

fn path_to_bytes(path: &Path) -> Vec<u8> {
    os_str_to_bytes(path.as_os_str())
}

#[cfg(unix)]
fn bytes_to_os_string(bytes: Vec<u8>) -> OsString {
    OsString::from_vec(bytes)
}

#[cfg(not(unix))]
fn bytes_to_os_string(bytes: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

fn bytes_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(bytes_to_os_string(bytes))
}

fn project_limit_error(resource: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{resource} exceeded its configured limit"),
    )
    .with_context(format!("limit {limit}, observed {observed}"))
    .with_help("Narrow the project roots or raise the corresponding project discovery limit")
}

fn cancelled_error() -> ShellError {
    ShellError::new(ErrorCode::ResourceLimit, "project discovery was cancelled")
        .with_context("the session requested worker shutdown")
        .with_help("Restart Quirl to run a new project discovery pass")
}

fn scan_io_error(path: &Path, error: impl std::fmt::Display) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        "could not completely scan a project directory",
    )
    .with_context(format!("{}: {error}", path.display()))
    .with_help("Check directory permissions or exclude the unreadable path")
}

fn internal_root_error() -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        "project discovery lost its owning root invariant",
    )
    .with_help("Restart Quirl and report this internal project-discovery error")
}

fn project_sql_error(error: rusqlite::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not update the project database")
        .with_context(error.to_string())
        .with_help("Check QUIRL_PROJECTS_DB and the available disk space")
}

fn database_error(path: &Path, error: impl std::fmt::Display) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not open project database at {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Set QUIRL_PROJECTS_DB to a private writable file path")
}

fn project_request_lock_error<T>(_error: std::sync::PoisonError<T>) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        "the project refresh request lock was poisoned",
    )
    .with_help("Restart Quirl to create a fresh project worker")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, atomic::AtomicU64};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "quirl-projects-{name}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_limits() -> ProjectDiscoveryLimits {
        ProjectDiscoveryLimits {
            depth_max: 8,
            entries_max: 1_000,
            directories_max: 1_000,
            repositories_max: 100,
            retained_path_bytes_max: 1024 * 1024,
            deadline: Duration::from_secs(5),
        }
    }

    #[test]
    fn discovery_finds_git_directories_and_git_files_and_prunes_repository_contents() {
        let temporary = TestDirectory::new("markers");
        let first = temporary.path().join("Code/first");
        let second = temporary.path().join("Code/second");
        fs::create_dir_all(first.join(".git")).unwrap();
        fs::create_dir_all(first.join("nested/not-a-project/.git")).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join(".git"), b"gitdir: ../worktrees/second\n").unwrap();

        let cancelled = AtomicBool::new(false);
        let output = discover_repositories(
            &ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits()),
            &cancelled,
        );

        assert!(output.report.complete);
        let paths = output
            .repositories
            .iter()
            .map(|repository| repository.path.as_path())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec![first.as_path(), second.as_path()]);
        assert!(
            output
                .repositories
                .iter()
                .all(|repository| repository.inferred_root == temporary.path().join("Code"))
        );
    }

    #[test]
    fn hidden_repositories_are_admitted_but_hidden_and_cache_trees_are_pruned() {
        let temporary = TestDirectory::new("exclusions");
        let dotfiles = temporary.path().join(".dotfiles");
        fs::create_dir_all(dotfiles.join(".git")).unwrap();
        fs::create_dir_all(temporary.path().join(".cache/hidden/.git")).unwrap();
        fs::create_dir_all(temporary.path().join("Library/project/.git")).unwrap();
        fs::create_dir_all(temporary.path().join("Code/node_modules/dependency/.git")).unwrap();

        let output = discover_repositories(
            &ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits()),
            &AtomicBool::new(false),
        );
        assert!(output.report.complete);
        assert_eq!(output.repositories.len(), 1);
        assert_eq!(output.repositories[0].path, dotfiles);
    }

    #[test]
    fn configured_exclusions_prune_only_the_exact_subtree() {
        let temporary = TestDirectory::new("configured-exclusion");
        let included = temporary.path().join("Code/include/project");
        let excluded = temporary.path().join("Code/exclude/project");
        fs::create_dir_all(included.join(".git")).unwrap();
        fs::create_dir_all(excluded.join(".git")).unwrap();
        let mut config =
            ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits());
        config
            .add_excluded_subtree(temporary.path().join("Code/exclude"))
            .unwrap();

        let output = discover_repositories(&config, &AtomicBool::new(false));
        assert!(output.report.complete);
        assert_eq!(output.repositories.len(), 1);
        assert_eq!(output.repositories[0].path, included);
    }

    #[cfg(unix)]
    #[test]
    fn followed_symlink_cycles_are_bounded_by_directory_identity() {
        use std::os::unix::fs::symlink;

        let temporary = TestDirectory::new("symlink-cycle");
        let project = temporary.path().join("Code/project");
        fs::create_dir_all(project.join(".git")).unwrap();
        symlink(temporary.path(), temporary.path().join("Code/back-to-root")).unwrap();
        let mut config =
            ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits());
        config.follow_symlinks = true;

        let output = discover_repositories(&config, &AtomicBool::new(false));
        assert!(output.report.complete);
        assert_eq!(output.repositories.len(), 1);
        assert_eq!(output.repositories[0].path, project);
        assert!(output.report.directories_scanned < 10);
    }

    #[test]
    fn inferred_root_frontier_compresses_unary_paths_without_selecting_home() {
        let temporary = TestDirectory::new("clusters");
        for relative in [
            "Code/personal/one",
            "Code/personal/two",
            "Code/client/three",
            "Work/company/four",
            "Work/company/five",
        ] {
            fs::create_dir_all(temporary.path().join(relative).join(".git")).unwrap();
        }

        let output = discover_repositories(
            &ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits()),
            &AtomicBool::new(false),
        );
        assert!(output.report.complete);
        let inferred = output
            .repositories
            .iter()
            .map(|repository| {
                (
                    repository.path.clone(),
                    repository.inferred_root.clone(),
                    repository.inferred_root_confidence,
                )
            })
            .collect::<Vec<_>>();
        for (path, root, confidence) in inferred {
            if path.starts_with(temporary.path().join("Code")) {
                assert_eq!(root, temporary.path().join("Code"));
            } else {
                assert_eq!(root, temporary.path().join("Work/company"));
            }
            assert_eq!(confidence, 900);
            assert_ne!(root, temporary.path());
        }
    }

    #[test]
    fn a_bound_interrupts_the_generation_with_a_resource_limit() {
        let temporary = TestDirectory::new("bound");
        fs::create_dir_all(temporary.path().join("one")).unwrap();
        fs::create_dir_all(temporary.path().join("two")).unwrap();
        let mut limits = test_limits();
        limits.directories_max = 1;

        let output = discover_repositories(
            &ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), limits),
            &AtomicBool::new(false),
        );
        assert!(!output.report.complete);
        assert_eq!(
            output.incomplete_error.as_ref().map(|error| error.code),
            Some(ErrorCode::ResourceLimit)
        );
    }

    #[test]
    fn cancellation_prevents_a_complete_generation() {
        let temporary = TestDirectory::new("cancel");
        let cancelled = AtomicBool::new(true);
        let output = discover_repositories(
            &ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits()),
            &cancelled,
        );
        assert!(!output.report.complete);
        assert_eq!(
            output.incomplete_error.as_ref().map(|error| error.code),
            Some(ErrorCode::ResourceLimit)
        );
    }

    #[cfg(unix)]
    #[test]
    fn marker_metadata_errors_are_not_treated_as_absent_markers() {
        let oversized_component = OsString::from("x".repeat(PATH_BYTES_MAX));
        let error = probe_git_marker(&PathBuf::from("/").join(oversized_component))
            .expect_err("an unrepresentable filesystem path must remain an operating error");
        assert_eq!(error.code, ErrorCode::Io);
    }

    #[test]
    fn oversized_database_is_rejected_before_sql_queries() {
        let temporary = TestDirectory::new("database-size");
        let path = temporary.path().join("projects.sqlite3");
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.set_len(u64::try_from(DATABASE_BYTES_MAX).unwrap() + 1)
            .unwrap();
        let error = ProjectDatabase::open(&path)
            .err()
            .expect("an oversized database must be rejected");
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[cfg(unix)]
    #[test]
    fn hard_linked_database_is_rejected_before_sqlite_can_modify_it() {
        let temporary = TestDirectory::new("database-hard-link");
        let path = temporary.path().join("projects.sqlite3");
        let alias = temporary.path().join("projects-alias.sqlite3");
        fs::File::create(&path).unwrap();
        fs::hard_link(&path, &alias).unwrap();

        let error = ProjectDatabase::open(&path)
            .err()
            .expect("a hard-linked database must be rejected");
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(fs::metadata(&alias).unwrap().nlink(), 2);
    }

    #[test]
    fn repository_activity_ranks_recent_metadata_then_newer_quirl_visits() {
        let temporary = TestDirectory::new("activity-ranking");
        let older = temporary.path().join("older");
        let recent = temporary.path().join("recent");
        let output = ProjectScanReport {
            entries_scanned: 2,
            directories_scanned: 2,
            repositories_found: 2,
            complete: true,
        }
        .with_repositories(
            vec![
                DiscoveredRepository {
                    path: older.clone(),
                    inferred_root: temporary.path().to_path_buf(),
                    inferred_root_confidence: 900,
                    source: ProjectSource::Automatic,
                    observed_activity_unix_ms: Some(1),
                },
                DiscoveredRepository {
                    path: recent.clone(),
                    inferred_root: temporary.path().to_path_buf(),
                    inferred_root_confidence: 900,
                    source: ProjectSource::Automatic,
                    observed_activity_unix_ms: Some(2),
                },
            ],
            None,
        );
        let mut database =
            ProjectDatabase::open(&temporary.path().join("projects.sqlite3")).unwrap();
        database.persist_scan(&output).unwrap();

        let snapshot = database.snapshot().unwrap();
        assert_eq!(snapshot.repositories[0].path, recent);
        assert_eq!(snapshot.repositories[0].observed_activity_unix_ms, Some(2));

        assert!(database.record_opened(&older).unwrap());
        let snapshot = database.snapshot().unwrap();
        assert_eq!(snapshot.repositories[0].path, older);
        assert!(snapshot.repositories[0].last_opened_unix_ms.is_some());
    }

    #[test]
    fn git_file_activity_resolves_the_worktree_metadata_directory() {
        let temporary = TestDirectory::new("worktree-activity");
        let repository = temporary.path().join("checkout");
        let git_directory = temporary.path().join("metadata/worktrees/checkout");
        fs::create_dir_all(git_directory.join("logs")).unwrap();
        fs::create_dir_all(&repository).unwrap();
        fs::write(
            repository.join(".git"),
            format!("gitdir: {}\n", git_directory.display()),
        )
        .unwrap();
        fs::write(git_directory.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
        fs::write(git_directory.join("index"), b"index").unwrap();
        fs::write(git_directory.join("logs/HEAD"), b"activity").unwrap();

        assert_eq!(git_metadata_directory(&repository), Some(git_directory));
        assert!(repository_activity_unix_ms(&repository, unix_time_ms()).is_some());
    }

    #[test]
    fn future_repository_activity_is_clamped_to_observation_time() {
        let temporary = TestDirectory::new("future-activity");
        let repository = temporary.path().join("project");
        fs::create_dir_all(repository.join(".git")).unwrap();
        let observed_at_unix_ms = 1;

        assert_eq!(
            repository_activity_unix_ms(&repository, observed_at_unix_ms),
            Some(observed_at_unix_ms)
        );
    }

    #[test]
    fn schema_one_project_cache_migrates_without_losing_rows() {
        let temporary = TestDirectory::new("schema-one-migration");
        let database_path = temporary.path().join("projects.sqlite3");
        let repository_path = temporary.path().join("project");
        let repository = DiscoveredRepository {
            path: repository_path.clone(),
            inferred_root: temporary.path().to_path_buf(),
            inferred_root_confidence: 900,
            source: ProjectSource::Automatic,
            observed_activity_unix_ms: Some(1),
        };
        let mut database = ProjectDatabase::open(&database_path).unwrap();
        database.upsert_targeted(&repository).unwrap();
        drop(database);

        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "DROP INDEX repositories_rank;
                 ALTER TABLE repositories DROP COLUMN observed_activity_unix_ms;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let database = ProjectDatabase::open(&database_path).unwrap();
        let snapshot = database.snapshot().unwrap();
        assert_eq!(snapshot.repositories.len(), 1);
        assert_eq!(snapshot.repositories[0].path, repository_path);
        assert_eq!(snapshot.repositories[0].observed_activity_unix_ms, None);
        let version: i64 = database
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, DATABASE_SCHEMA_VERSION);
    }

    #[test]
    fn simultaneous_schema_one_openers_serialize_and_recheck_the_migration() {
        let temporary = TestDirectory::new("concurrent-schema-migration");
        let database_path = temporary.path().join("projects.sqlite3");
        let database = ProjectDatabase::open(&database_path).unwrap();
        drop(database);
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                "DROP INDEX repositories_rank;
                 ALTER TABLE repositories DROP COLUMN observed_activity_unix_ms;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let worker_path = database_path.clone();
            let worker_barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                worker_barrier.wait();
                ProjectDatabase::open(&worker_path).is_ok()
            }));
        }
        barrier.wait();
        for worker in workers {
            assert!(worker.join().unwrap());
        }

        let database = ProjectDatabase::open(&database_path).unwrap();
        let version: i64 = database
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        let activity_columns: i64 = database
            .connection
            .query_row(
                "SELECT count(*) FROM pragma_table_info('repositories')
                 WHERE name = 'observed_activity_unix_ms'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, DATABASE_SCHEMA_VERSION);
        assert_eq!(activity_columns, 1);
    }

    #[test]
    fn complete_generations_replace_rows_but_partial_generations_preserve_them() {
        let temporary = TestDirectory::new("persistence");
        let database_path = temporary.path().join("projects.sqlite3");
        let first = temporary.path().join("Code/first");
        fs::create_dir_all(first.join(".git")).unwrap();
        let config = ProjectDiscoveryConfig::for_root(temporary.path().join("Code"), test_limits());
        let cancelled = AtomicBool::new(false);
        let first_output = discover_repositories(&config, &cancelled);
        let mut database = ProjectDatabase::open(&database_path).unwrap();
        database.persist_scan(&first_output).unwrap();
        let first_snapshot = database.snapshot().unwrap();
        assert_eq!(first_snapshot.generation, 1);
        assert_eq!(first_snapshot.repositories.len(), 1);

        fs::remove_dir_all(first.join(".git")).unwrap();
        let partial = ScanOutput {
            report: ProjectScanReport {
                entries_scanned: 1,
                directories_scanned: 1,
                repositories_found: 0,
                complete: false,
            },
            repositories: Vec::new(),
            incomplete_error: Some(cancelled_error()),
        };
        database.persist_scan(&partial).unwrap();
        let preserved = database.snapshot().unwrap();
        assert_eq!(preserved.generation, 1);
        assert_eq!(preserved.repositories.len(), 1);
        assert_eq!(preserved.scan_state, ProjectScanState::Incomplete);
        assert!(preserved.last_attempt_unix_ms.is_some());
        assert!(!snapshot_is_stale(
            &preserved,
            60_000,
            preserved.last_attempt_unix_ms.unwrap()
        ));

        let complete = discover_repositories(&config, &cancelled);
        assert!(complete.report.complete);
        database.persist_scan(&complete).unwrap();
        let removed = database.snapshot().unwrap();
        assert_eq!(removed.generation, 2);
        assert!(removed.repositories.is_empty());
    }

    // The gate owns release on assertion failure; the worker also has a broad
    // watchdog so a broken test cannot leave an unbounded detached waiter.
    struct StartupGate {
        entered: std::sync::mpsc::Receiver<()>,
        release: Option<std::sync::mpsc::SyncSender<()>>,
    }

    impl StartupGate {
        fn new() -> (Self, Box<dyn FnOnce() + Send>) {
            let (entered_sender, entered) = std::sync::mpsc::sync_channel(1);
            let (release, receiver) = std::sync::mpsc::sync_channel(1);
            let hook = Box::new(move || {
                entered_sender.send(()).unwrap();
                receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            (
                Self {
                    entered,
                    release: Some(release),
                },
                hook,
            )
        }

        fn wait(&self) {
            self.entered.recv_timeout(Duration::from_secs(5)).unwrap();
        }

        fn release(&mut self) {
            self.release.take().unwrap().send(()).unwrap();
        }
    }

    impl Drop for StartupGate {
        fn drop(&mut self) {
            if let Some(release) = self.release.take() {
                let _ = release.try_send(());
            }
        }
    }

    #[test]
    fn refresh_constructor_returns_before_database_io_and_cancel_joins_without_opening() {
        let temporary = TestDirectory::new("deferred-cancel");
        let path = temporary.path().join("projects.sqlite3");
        let config =
            ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits());
        let (gate, hook) = StartupGate::new();
        let refresh = ProjectRefresh::start_at_inner(
            path.clone(),
            config,
            ProjectStartupHooks {
                before_open: Some(hook),
                ..ProjectStartupHooks::default()
            },
        )
        .unwrap();
        // Declare the release owner after the service so assertion unwinding
        // releases the gate before the service joins its worker.
        let mut gate = gate;
        gate.wait();
        assert!(!path.exists());
        assert_eq!(
            refresh.snapshot().unwrap().scan_state,
            ProjectScanState::Loading
        );
        refresh.hint_picker_open().unwrap();
        assert!(refresh.requests.0.lock().unwrap().full);
        refresh.cancel();
        gate.release();
        drop(refresh);
        assert!(
            !path.exists(),
            "cancelled startup must not create the database"
        );
    }

    #[test]
    fn startup_publishes_cached_rows_before_scan_and_preserves_early_picker_refresh() {
        let temporary = TestDirectory::new("deferred-cache");
        let path = temporary.path().join("projects.sqlite3");
        let root = temporary.path().join("Code");
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir_all(old.join(".git")).unwrap();
        let config = ProjectDiscoveryConfig::for_root(root, test_limits());
        let mut database = ProjectDatabase::open(&path).unwrap();
        database
            .persist_scan(&discover_repositories(&config, &AtomicBool::new(false)))
            .unwrap();
        drop(database);
        fs::remove_dir_all(&old).unwrap();
        fs::create_dir_all(new.join(".git")).unwrap();
        let (gate, hook) = StartupGate::new();
        let refresh = ProjectRefresh::start_at_inner(
            path.clone(),
            config,
            ProjectStartupHooks {
                after_cache: Some(hook),
                ..ProjectStartupHooks::default()
            },
        )
        .unwrap();
        let mut gate = gate;
        gate.wait();
        let cached = refresh.snapshot().unwrap();
        assert_eq!(cached.generation, 1);
        assert_eq!(cached.scan_state, ProjectScanState::Cached);
        assert_eq!(cached.repositories.len(), 1);
        assert_eq!(cached.repositories[0].path, old);
        assert!(refresh.take_changed());
        refresh.hint_picker_open().unwrap();
        gate.release();
        let started = Instant::now();
        let scanned = loop {
            let snapshot = refresh.snapshot().unwrap();
            if snapshot.scan_state == ProjectScanState::Complete {
                break snapshot;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "scan state: {snapshot:?}"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(scanned.generation, 2);
        assert_eq!(scanned.repositories.len(), 1);
        assert_eq!(scanned.repositories[0].path, new);
        refresh.cancel();
        drop(refresh);
        assert_eq!(
            ProjectDatabase::open(&path)
                .unwrap()
                .snapshot()
                .unwrap()
                .generation,
            2
        );
    }

    #[test]
    fn asynchronous_cache_failure_retains_typed_error_and_preserves_database_bytes() {
        let temporary = TestDirectory::new("deferred-invalid");
        let path = temporary.path().join("projects.sqlite3");
        let invalid = b"not a SQLite database";
        fs::write(&path, invalid).unwrap();
        let config =
            ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits());
        let refresh = ProjectRefresh::start_at(path.clone(), config).unwrap();
        let started = Instant::now();
        while !refresh.worker.as_ref().unwrap().is_finished() {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(1));
        }
        assert!(refresh.take_changed());
        let error = refresh.snapshot().unwrap_err();
        assert_eq!(error.code, ErrorCode::Io, "{error:?}");
        assert!(!error.details.help.is_empty());
        assert!(!error.details.context.is_empty());
        assert_eq!(refresh.snapshot().unwrap_err().message, error.message);
        assert_eq!(
            refresh.snapshot.read().unwrap().scan_state,
            ProjectScanState::Incomplete
        );
        drop(refresh);
        assert_eq!(fs::read(path).unwrap(), invalid);
    }

    #[test]
    fn startup_failure_after_a_foreground_visit_preserves_rows_and_reports_the_dead_worker() {
        let temporary = TestDirectory::new("deferred-visit-failure");
        let path = temporary.path().join("projects.sqlite3");
        let repository = fs::canonicalize(temporary.path()).unwrap().join("project");
        fs::create_dir_all(repository.join(".git")).unwrap();
        let config =
            ProjectDiscoveryConfig::for_root(temporary.path().to_path_buf(), test_limits());
        let (gate, hook) = StartupGate::new();
        let refresh = ProjectRefresh::start_at_inner(
            path.clone(),
            config,
            ProjectStartupHooks {
                before_open: Some(hook),
                ..ProjectStartupHooks::default()
            },
        )
        .unwrap();
        let mut gate = gate;
        gate.wait();
        assert!(refresh.record_opened_if_repository(&repository).unwrap());
        assert_eq!(refresh.snapshot().unwrap().repositories[0].path, repository);
        // Keep the admitted database intact while replacing its pathname with
        // an invalid kind, modelling a startup open failure after the visit.
        let saved = temporary.path().join("saved.sqlite3");
        fs::rename(&path, &saved).unwrap();
        fs::create_dir(&path).unwrap();
        gate.release();
        let started = Instant::now();
        while !refresh.worker.as_ref().unwrap().is_finished() {
            assert!(started.elapsed() < Duration::from_secs(5));
            thread::sleep(Duration::from_millis(1));
        }
        let error = refresh.snapshot().unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation, "{error:?}");
        let retained = refresh.snapshot.read().unwrap();
        assert_eq!(retained.scan_state, ProjectScanState::Incomplete);
        assert_eq!(retained.repositories.len(), 1);
        assert_eq!(retained.repositories[0].path, repository);
        drop(retained);
        fs::remove_dir(&path).unwrap();
        fs::rename(&saved, &path).unwrap();
        assert!(refresh.record_opened_if_repository(&repository).unwrap());
        assert_eq!(refresh.snapshot().unwrap_err().message, error.message);
        drop(refresh);
        assert_eq!(
            ProjectDatabase::open(&path)
                .unwrap()
                .snapshot()
                .unwrap()
                .repositories[0]
                .path,
            repository
        );
    }

    #[test]
    fn initial_cache_publication_cannot_replace_an_early_foreground_update() {
        let snapshot = RwLock::new(ProjectSnapshot {
            generation: 7,
            last_complete_unix_ms: Some(123),
            ..ProjectSnapshot::default()
        });
        let changed = AtomicBool::new(false);
        publish_initial_snapshot(&snapshot, &changed, Ok(ProjectSnapshot::default()));
        let retained = snapshot.read().unwrap();
        assert_eq!(retained.generation, 7);
        assert_eq!(retained.last_complete_unix_ms, Some(123));
        assert!(retained.startup_error.is_none());
        assert!(!changed.load(Ordering::Acquire));
    }

    #[test]
    fn completed_clone_is_published_without_a_visit_and_survives_reconciliation() {
        let temporary = TestDirectory::new("clone-published");
        let repository = fs::canonicalize(temporary.path())
            .unwrap()
            .join("managed/project");
        fs::create_dir_all(repository.join(".git")).unwrap();
        let database_path = temporary.path().join("projects.sqlite3");
        let scan_root = temporary.path().join("other");
        fs::create_dir(&scan_root).unwrap();
        let config = ProjectDiscoveryConfig::for_root(scan_root, test_limits());
        let (mut gate, hook) = StartupGate::new();
        let refresh = ProjectRefresh::start_at_inner(
            database_path.clone(),
            config.clone(),
            ProjectStartupHooks {
                after_cache: Some(hook),
                ..ProjectStartupHooks::default()
            },
        )
        .unwrap();
        gate.wait();
        assert!(refresh.record_cloned(&repository).unwrap());
        let published = refresh.snapshot().unwrap();
        assert_eq!(published.repositories.len(), 1);
        assert_eq!(published.repositories[0].path, repository);
        assert_eq!(published.repositories[0].open_count, 0);
        assert!(published.repositories[0].last_opened_unix_ms.is_none());
        assert!(refresh.take_changed());
        refresh.cancel();
        gate.release();
        drop(refresh);
        let mut database = ProjectDatabase::open(&database_path).unwrap();
        database
            .persist_scan(&discover_repositories(&config, &AtomicBool::new(false)))
            .unwrap();
        assert_eq!(database.snapshot().unwrap().repositories.len(), 1);
        fs::remove_dir(repository.join(".git")).unwrap();
        assert!(!validate_project_directory(&repository).unwrap());
    }

    #[test]
    fn refresh_requests_coalesce_full_and_duplicate_targeted_work() {
        let mut requests = RefreshRequests::default();
        requests
            .request_targeted(PathBuf::from("/work/one"))
            .unwrap();
        requests
            .request_targeted(PathBuf::from("/work/one"))
            .unwrap();
        assert_eq!(requests.targeted.len(), 1);
        requests.request_full();
        requests
            .request_targeted(PathBuf::from("/work/two"))
            .unwrap();
        let (full, targeted) = requests.drain();
        assert!(full);
        assert!(targeted.is_empty());
    }
}
