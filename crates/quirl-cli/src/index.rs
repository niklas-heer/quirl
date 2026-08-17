use crate::intelligence;
use clap::{ArgAction, Subcommand, ValueEnum};
use quirl_catalog::{
    import_bash, import_fish, import_help, import_man, import_zsh, Catalog, CommandSpec,
    Confidence, Effect, ImportDiagnostic, ImportReport, IoContract, Provenance, ProvenanceInfo,
};
use quirl_core::{
    escape_json_terminal_controls, escape_terminal_controls, replace_file_atomically,
    AtomicReplaceOptions, ErrorCode, ShellError,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const INDEX_READ_LIMIT: usize = 16 * 1024 * 1024;
const COMPLETION_READ_LIMIT: usize = 4 * 1024 * 1024;
const DOCUMENTATION_READ_LIMIT: usize = 1024 * 1024;
const INDEX_ROOTS_MAX: usize = 128;
const INDEX_DIRECTORY_ENTRIES_MAX: usize = 8_192;
const INDEX_FILES_MAX: usize = 4_096;
const INDEX_PATH_BYTES_MAX: usize = 1024 * 1024;
const INDEX_SOURCE_BYTES_TOTAL_MAX: usize = 16 * 1024 * 1024;
const INDEX_RECORDS_MAX: usize = 65_536;
const INDEX_RETAINED_BYTES_MAX: usize = 16 * 1024 * 1024;
const INDEX_DIAGNOSTICS_MAX: usize = 4_096;
const INDEX_TEMPORARY_ATTEMPTS_MAX: usize = 64;
const AUTOMATIC_MAN_PAGES_MAX: usize = 512;
const AUTOMATIC_MAN_DIAGNOSTICS_MAX: usize = 128;
const MAN_CANDIDATE_PATH_BYTES_MAX: usize = 1024 * 1024;
const INDEX_DIAGNOSTIC_ORIGIN_BYTES_MAX: usize = 1024;
const INDEX_DIAGNOSTIC_MESSAGE_BYTES_MAX: usize = 512;
const DISCOVERY_STATE_VERSION: u32 = 1;
const DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const DISCOVERY_STALE_AFTER: Duration = Duration::from_secs(15 * 60);
const DISCOVERY_DEADLINE: Duration = Duration::from_millis(750);
const BACKGROUND_DISCOVERY_DEADLINE: Duration = Duration::from_secs(30);
static NEXT_INDEX_TEMPORARY: AtomicU64 = AtomicU64::new(0);
static DATABASE_PUBLICATION: Mutex<()> = Mutex::new(());

/// A cancellable, bounded background refresh owned by one interactive session.
/// Dropping the guard wakes and joins its single worker before terminal shutdown
/// can finish, so no cache task survives the shell that created it.
pub struct CatalogRefresh {
    cancelled: Arc<AtomicBool>,
    changed: Arc<AtomicBool>,
    requested_generation: Arc<AtomicU64>,
    wake: Arc<(Mutex<()>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

pub(crate) trait CatalogRefreshObserver: Send + Sync {
    fn refresh_started(&self);
    fn refresh_published(&self);
    fn refresh_unchanged(&self);
    fn refresh_failed(&self, error: &ShellError);
}

impl CatalogRefresh {
    /// Report one completed cache replacement to the prompt-boundary owner.
    /// Multiple replacements coalesce because only the newest atomic catalog
    /// matters to the next editor generation.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn request_refresh(&self) -> Result<(), ShellError> {
        let _guard = self.wake.0.lock().map_err(|_| {
            ShellError::new(
                ErrorCode::Io,
                "the catalog refresh request lock was poisoned",
            )
            .with_help("Restart Quirl to create a fresh catalog worker")
        })?;
        increment_generation(
            &self.requested_generation,
            "catalog refresh request generation",
        )?;
        self.wake.1.notify_one();
        Ok(())
    }

    pub(crate) fn cancel(&self) {
        let _guard = match self.wake.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.cancelled.store(true, Ordering::Release);
        self.wake.1.notify_all();
    }
}

impl Drop for CatalogRefresh {
    fn drop(&mut self) {
        self.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Clone)]
struct DiscoveryConfig {
    index_path: PathBuf,
    path_roots: Vec<PathBuf>,
    fish_roots: Vec<PathBuf>,
    bash_roots: Vec<PathBuf>,
    zsh_roots: Vec<PathBuf>,
    help_roots: Vec<PathBuf>,
    man_roots: Vec<PathBuf>,
    stale_after: Duration,
}

struct CatalogRefreshWorker {
    config: DiscoveryConfig,
    cancelled: Arc<AtomicBool>,
    changed: Arc<AtomicBool>,
    requested_generation: Arc<AtomicU64>,
    wake: Arc<(Mutex<()>, Condvar)>,
    observer: Arc<dyn CatalogRefreshObserver>,
    refresh_interval: Duration,
    refresh_deadline: Duration,
    reload_environment: bool,
}

#[derive(Clone, Copy)]
struct RefreshDeadline {
    expires_at: Instant,
    limit: Duration,
}

impl RefreshDeadline {
    fn starting_now(limit: Duration) -> Self {
        Self {
            expires_at: Instant::now() + limit,
            limit,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
enum DiscoverySourceKind {
    PathExecutable,
    Fish,
    Bash,
    Zsh,
    Help,
    Man,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
struct DiscoverySource {
    kind: DiscoverySourceKind,
    path: PathBuf,
    bytes: u64,
    modified_unix_nanos: u64,
    fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryState {
    version: u32,
    catalog_schema_version: u32,
    refreshed_unix_ms: u64,
    source_fingerprint: String,
    catalog_fingerprint: String,
    sources: Vec<DiscoverySource>,
    #[serde(default)]
    diagnostics: Vec<ImportDiagnostic>,
}

struct DiscoverySnapshot {
    sources: Vec<DiscoverySource>,
    executables: Vec<PathBuf>,
    fish_files: Vec<PathBuf>,
    bash_files: Vec<PathBuf>,
    zsh_files: Vec<PathBuf>,
    help_files: Vec<PathBuf>,
    man_files: Vec<PathBuf>,
    diagnostics: Vec<ImportDiagnostic>,
    fingerprint: String,
}

struct ManCandidate {
    command: String,
    path: PathBuf,
    root_priority: usize,
    compressed: bool,
}

// Failure cleanup preserves every armed name because identity validation plus
// pathname unlink is racy. Only the bounded post-commit path removes the hidden
// temporary, under the explicit assumption that the containing namespace is
// cooperative for that final success cleanup.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexWriteStage {
    ContentSynced,
    Installed,
}

#[derive(Debug)]
struct IndexOwnedPath {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl IndexOwnedPath {
    fn from_file(path: PathBuf, file: &File) -> Result<Self, ShellError> {
        let metadata = file
            .metadata()
            .map_err(|error| index_io_error("inspect", &path, error))?;
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
        // This is success-only cleanup of one bounded transaction name. The
        // containing namespace must remain cooperative during this final unlink.
        fs::remove_file(&self.path)
    }
}

struct IndexTemporary {
    temporary: Option<IndexOwnedPath>,
    destination: Option<PathBuf>,
}

impl IndexTemporary {
    fn new(path: PathBuf, file: &File) -> Result<Self, ShellError> {
        Ok(Self {
            temporary: Some(IndexOwnedPath::from_file(path, file)?),
            destination: None,
        })
    }

    fn path(&self) -> &Path {
        self.temporary
            .as_ref()
            .map(IndexOwnedPath::path)
            .unwrap_or_else(|| Path::new("<removed-index-temporary>"))
    }

    fn installed(&mut self, path: &Path) {
        self.destination = Some(path.to_path_buf());
    }

    fn owns(&self, path: &Path) -> bool {
        self.temporary
            .as_ref()
            .is_some_and(|temporary| temporary.matches(path))
    }

    fn cleanup(&mut self, mut error: ShellError) -> ShellError {
        if let Some(destination) = self.destination.take() {
            error = error.with_context(format!(
                "failure cleanup preserved installed index {}",
                destination.display()
            ));
        }
        if let Some(temporary) = self.temporary.take() {
            error = error.with_context(format!(
                "failure cleanup preserved index temporary {}",
                temporary.path().display()
            ));
        }
        error
    }

    fn disarm(&mut self) {
        self.temporary = None;
        self.destination = None;
    }
}

impl Drop for IndexTemporary {
    fn drop(&mut self) {
        self.destination = None;
        self.temporary = None;
    }
}

#[derive(Clone, Copy)]
struct IndexBounds {
    roots_max: usize,
    entries_max: usize,
    files_max: usize,
    path_bytes_max: usize,
    source_bytes_max: usize,
    records_max: usize,
    retained_bytes_max: usize,
    diagnostics_max: usize,
}

impl IndexBounds {
    const PRODUCTION: Self = Self {
        roots_max: INDEX_ROOTS_MAX,
        entries_max: INDEX_DIRECTORY_ENTRIES_MAX,
        files_max: INDEX_FILES_MAX,
        path_bytes_max: INDEX_PATH_BYTES_MAX,
        source_bytes_max: INDEX_SOURCE_BYTES_TOTAL_MAX,
        records_max: INDEX_RECORDS_MAX,
        retained_bytes_max: INDEX_RETAINED_BYTES_MAX,
        diagnostics_max: INDEX_DIAGNOSTICS_MAX,
    };
}

struct IndexBuildBudget {
    bounds: IndexBounds,
    roots: usize,
    entries: usize,
    files: usize,
    path_bytes: usize,
    source_bytes: usize,
    records: usize,
    retained_bytes: usize,
    diagnostics: usize,
}

impl IndexBuildBudget {
    fn new(bounds: IndexBounds) -> Self {
        Self {
            bounds,
            roots: 0,
            entries: 0,
            files: 0,
            path_bytes: 0,
            source_bytes: 0,
            records: 0,
            retained_bytes: 0,
            diagnostics: 0,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum IndexCommand {
    /// Import completion declarations and atomically write the catalog index.
    #[command(disable_help_flag = true)]
    Build {
        /// Print build command help. Long --help is reserved for help-text inputs.
        #[arg(short = 'h', action = ArgAction::Help)]
        usage_help: Option<bool>,
        /// Fish completion file or directory. Repeat to import several roots.
        #[arg(long)]
        fish: Vec<PathBuf>,
        /// Bash completion file or directory. Repeat to import several roots.
        #[arg(long)]
        bash: Vec<PathBuf>,
        /// Zsh completion file or directory. Repeat to import several roots.
        #[arg(long)]
        zsh: Vec<PathBuf>,
        /// Supplied command-help text file or directory. Never executes a command.
        #[arg(long = "help", value_name = "PATH")]
        help_sources: Vec<PathBuf>,
        /// Supplied rendered/raw man text file or directory. Never invokes man.
        #[arg(long, value_name = "PATH")]
        man: Vec<PathBuf>,
        /// Index destination. Defaults to Quirl's user cache directory.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Output representation for the build report.
        #[arg(long, value_enum, default_value_t = IndexOutputFormat::Text)]
        format: IndexOutputFormat,
    },
    /// Explain the provenance of a command and each retained option.
    Explain {
        /// Space-separated command path, for example `git commit`.
        #[arg(required = true, num_args = 1..)]
        command: Vec<String>,
        /// Read a specific index instead of Quirl's user cache.
        #[arg(long)]
        index: Option<PathBuf>,
        /// Output representation for the provenance explanation.
        #[arg(long, value_enum, default_value_t = IndexOutputFormat::Text)]
        format: IndexOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum IndexOutputFormat {
    Text,
    Json,
}

pub fn wants_json(command: &IndexCommand) -> bool {
    matches!(
        command,
        IndexCommand::Build {
            format: IndexOutputFormat::Json,
            ..
        } | IndexCommand::Explain {
            format: IndexOutputFormat::Json,
            ..
        }
    )
}

#[derive(Debug, Serialize)]
struct BuildReport {
    index: PathBuf,
    source_files: usize,
    commands: usize,
    options: usize,
    diagnostics: Vec<ImportDiagnostic>,
}

pub fn execute(command: IndexCommand) -> Result<i32, ShellError> {
    match command {
        IndexCommand::Build {
            usage_help: _,
            fish,
            bash,
            zsh,
            help_sources,
            man,
            output,
            format,
        } => build_index(fish, bash, zsh, help_sources, man, output, format),
        IndexCommand::Explain {
            command,
            index,
            format,
        } => explain_index(&command.join(" "), index, format),
    }
}

/// Load the default attributed index for completion/help consumers. Cached
/// imported facts augment, but can never replace, the builtins compiled into
/// this binary. A missing, unreadable, corrupt, or incompatible cache is
/// recoverable and falls back to those builtins.
pub fn load_default_catalog() -> Catalog {
    let Some(path) = default_index_path() else {
        return Catalog::builtin();
    };
    load_catalog_at(&path)
}

/// Return the configured SQLite command-database path for CLI intelligence tools.
pub(crate) fn default_database_path() -> Result<PathBuf, ShellError> {
    default_index_path().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "cannot determine the local command-database path",
        )
        .with_help("Set QUIRL_INDEX_PATH, XDG_CACHE_HOME, or HOME and retry")
    })
}

/// Search the current database with semantic embeddings when available and a
/// deterministic lexical fallback otherwise.
pub(crate) fn search_default_database(
    query: &str,
    limit: usize,
) -> Result<Vec<intelligence::SearchResult>, ShellError> {
    let path = default_database_path()?;
    let bytes = read_index(&path).map_err(|error| {
        error.with_help("Run `quirl index build` to create the command database")
    })?;
    let model_path = intelligence::default_model_path();
    intelligence::search(&bytes, &path, query, limit, model_path.as_deref())
}

/// Open one validated, reusable search generation for interactive AI mode.
pub(crate) fn open_default_search_session() -> Result<intelligence::SearchSession, ShellError> {
    let path = default_database_path()?;
    let bytes = read_index(&path).map_err(|error| {
        error.with_help("Run `quirl index build` to create the command database")
    })?;
    let model_path = intelligence::default_model_path();
    intelligence::SearchSession::open(&bytes, &path, model_path.as_deref())
}

/// Rebuild potion-base-8M embeddings in one in-memory transaction and atomically
/// replace the database only after every vector passes validation.
pub(crate) fn build_default_embeddings() -> Result<intelligence::EmbeddingReport, ShellError> {
    let path = default_database_path()?;
    let model_path = intelligence::default_model_path().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "cannot determine the local model path",
        )
        .with_help("Set QUIRL_MODEL_PATH or HOME and retry")
    })?;
    let bytes = read_index(&path)
        .map_err(|error| error.with_help("Run `quirl index build` before indexing embeddings"))?;
    let (encoded, report) = intelligence::build_embeddings(&bytes, &path, &model_path)?;
    write_index_bytes_atomically(&path, &encoded, intelligence::DATABASE_BYTES_MAX)?;
    Ok(report)
}

/// Build embeddings for one requested catalog generation and publish them only
/// while the source database and request generation are still current.
pub(crate) fn build_default_embeddings_if_current(
    cancelled: &AtomicBool,
    requested_generation: &AtomicU64,
    generation: u64,
) -> Result<Option<intelligence::EmbeddingReport>, ShellError> {
    let path = default_database_path()?;
    let model_path = intelligence::default_model_path().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "cannot determine the local model path",
        )
        .with_help("Set QUIRL_MODEL_PATH or HOME and retry")
    })?;
    let source = read_index(&path).map_err(|error| {
        error.with_help("Wait for command discovery before indexing embeddings")
    })?;
    let (encoded, report) = intelligence::build_embeddings_cancellable(
        &source,
        &path,
        &model_path,
        intelligence::AUTOMATIC_EMBEDDING_BATCH_SIZE,
        || {
            if cancelled.load(Ordering::Acquire)
                || requested_generation.load(Ordering::Acquire) != generation
            {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "automatic embedding build was cancelled",
                )
                .with_help("The newest catalog generation will be indexed instead"));
            }
            Ok(())
        },
    )?;
    if !publish_embeddings_if_current(
        &path,
        &source,
        &encoded,
        cancelled,
        requested_generation,
        generation,
    )? {
        return Ok(None);
    }
    Ok(Some(report))
}

fn publish_embeddings_if_current(
    path: &Path,
    source: &[u8],
    encoded: &[u8],
    cancelled: &AtomicBool,
    requested_generation: &AtomicU64,
    generation: u64,
) -> Result<bool, ShellError> {
    if cancelled.load(Ordering::Acquire)
        || requested_generation.load(Ordering::Acquire) != generation
    {
        return Ok(false);
    }
    let _publication = DATABASE_PUBLICATION.lock().map_err(|_| {
        ShellError::new(
            ErrorCode::Io,
            "the command-database publication lock was poisoned",
        )
        .with_help("Restart Quirl before rebuilding local command intelligence")
    })?;
    let current = read_index(path)?;
    if current != source
        || cancelled.load(Ordering::Acquire)
        || requested_generation.load(Ordering::Acquire) != generation
    {
        return Ok(false);
    }
    write_index_bytes_atomically_unlocked(path, encoded, intelligence::DATABASE_BYTES_MAX)?;
    Ok(true)
}

/// Read and validate bounded row counts from the default command database.
pub(crate) fn default_database_stats() -> Result<intelligence::DatabaseStats, ShellError> {
    let path = default_database_path()?;
    let bytes = read_index(&path)?;
    intelligence::database_stats(&bytes, &path)
}

pub(crate) fn default_embeddings_are_current() -> Result<bool, ShellError> {
    let path = default_database_path()?;
    let bytes = read_index(&path)?;
    intelligence::embeddings_are_current(&bytes, &path)
}

#[cfg(debug_assertions)]
pub(crate) fn mark_default_embeddings_current_for_test() -> Result<(), ShellError> {
    let path = default_database_path()?;
    let _publication = DATABASE_PUBLICATION.lock().map_err(|_| {
        ShellError::new(
            ErrorCode::Io,
            "the command-database publication lock was poisoned",
        )
        .with_help("Restart Quirl before updating the test command database")
    })?;
    let source = read_index(&path)?;
    let encoded = intelligence::mark_embeddings_current_for_test(&source, &path)?;
    write_index_bytes_atomically_unlocked(&path, &encoded, intelligence::DATABASE_BYTES_MAX)
}

/// Start the one periodic catalog worker after interactive catalog admission.
/// The initial full scan begins immediately, uses the longer background
/// deadline, and never owns terminal state.
pub(crate) fn start_interactive_catalog_refresh(
    observer: Arc<dyn CatalogRefreshObserver>,
) -> Option<CatalogRefresh> {
    let config = DiscoveryConfig::from_environment()?;
    start_catalog_refresh_with_config(
        config,
        observer,
        DISCOVERY_REFRESH_INTERVAL,
        BACKGROUND_DISCOVERY_DEADLINE,
        true,
    )
}

fn start_catalog_refresh_with_config(
    config: DiscoveryConfig,
    observer: Arc<dyn CatalogRefreshObserver>,
    refresh_interval: Duration,
    refresh_deadline: Duration,
    reload_environment: bool,
) -> Option<CatalogRefresh> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let changed = Arc::new(AtomicBool::new(false));
    let requested_generation = Arc::new(AtomicU64::new(1));
    let wake = Arc::new((Mutex::new(()), Condvar::new()));
    let worker_state = CatalogRefreshWorker {
        config,
        cancelled: Arc::clone(&cancelled),
        changed: Arc::clone(&changed),
        requested_generation: Arc::clone(&requested_generation),
        wake: Arc::clone(&wake),
        observer,
        refresh_interval,
        refresh_deadline,
        reload_environment,
    };
    let worker = thread::Builder::new()
        .name("quirl-catalog-refresh".to_owned())
        .spawn(move || refresh_loop(worker_state))
        .ok()?;
    Some(CatalogRefresh {
        cancelled,
        changed,
        requested_generation,
        wake,
        worker: Some(worker),
    })
}

/// Initialize or repair the default cache within the interactive discovery
/// deadline. If first-run discovery fails, publish an atomic builtin-only
/// SQLite fallback for local intelligence without replacing a valid prior
/// database. The caller intentionally ignores remaining persistence failures,
/// so cache permissions cannot prevent terminal setup.
pub fn initialize_interactive_catalog() {
    let Some(config) = DiscoveryConfig::from_environment() else {
        return;
    };
    #[cfg(debug_assertions)]
    let deadline = if env::var_os("QUIRL_TEST_CATALOG_FORCE_TIMEOUT").is_some() {
        Instant::now()
    } else {
        Instant::now() + DISCOVERY_DEADLINE
    };
    #[cfg(not(debug_assertions))]
    let deadline = Instant::now() + DISCOVERY_DEADLINE;
    let _ = initialize_interactive_catalog_with_deadline(&config, deadline);
}

fn initialize_interactive_catalog_with_deadline(
    config: &DiscoveryConfig,
    deadline: Instant,
) -> Result<bool, ShellError> {
    let cancelled = AtomicBool::new(false);
    match refresh_catalog_cache(
        config,
        RefreshDeadline {
            expires_at: deadline,
            limit: DISCOVERY_DEADLINE,
        },
        &cancelled,
    ) {
        Ok(refreshed) => Ok(refreshed),
        Err(discovery_error) => ensure_builtin_database(&config.index_path).map_err(|error| {
            error.with_context(format!(
                "catalog discovery failed before fallback publication: {}",
                discovery_error.message
            ))
        }),
    }
}

fn ensure_builtin_database(path: &Path) -> Result<bool, ShellError> {
    let encoded = intelligence::encode_database(&Catalog::builtin(), None)?;
    let _publication = DATABASE_PUBLICATION.lock().map_err(|_| {
        ShellError::new(
            ErrorCode::Io,
            "the command-database publication lock was poisoned",
        )
        .with_help("Restart Quirl before repairing the local command database")
    })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        create_index_directories(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_index_reader_metadata(
                path,
                &metadata,
                "Use an unlinked regular index file with no group/other write access",
            )?;
            let expected = read_index_bytes(
                path,
                intelligence::DATABASE_BYTES_MAX,
                "command database",
                "Use an unlinked regular command database",
            )?;
            if intelligence::decode_database(&expected, path).is_ok() {
                return Ok(false);
            }
            replace_file_atomically(
                path,
                &expected,
                &encoded,
                AtomicReplaceOptions {
                    bytes_max: intelligence::DATABASE_BYTES_MAX,
                },
            )?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            install_new_index(path, &encoded, parent.unwrap_or_else(|| Path::new(".")))?;
            Ok(true)
        }
        Err(error) => Err(index_io_error("inspect", path, error)),
    }
}

impl DiscoveryConfig {
    fn from_environment() -> Option<Self> {
        Some(Self {
            index_path: default_index_path()?,
            path_roots: env::var_os("PATH")
                .as_deref()
                .map(env::split_paths)
                .into_iter()
                .flatten()
                .collect(),
            fish_roots: default_fish_roots(),
            bash_roots: default_bash_roots(),
            zsh_roots: default_zsh_roots(),
            help_roots: default_documentation_roots("QUIRL_HELP_PATH", "help"),
            man_roots: default_man_roots(),
            stale_after: DISCOVERY_STALE_AFTER,
        })
    }
}

fn refresh_loop(worker: CatalogRefreshWorker) {
    let mut completed_generation = 0_u64;
    loop {
        let requested = match wait_for_refresh_request(
            completed_generation,
            &worker.cancelled,
            &worker.requested_generation,
            &worker.wake,
            worker.refresh_interval,
        ) {
            Ok(Some(requested)) => requested,
            Ok(None) | Err(_) => return,
        };
        let current = if worker.reload_environment {
            DiscoveryConfig::from_environment().unwrap_or_else(|| worker.config.clone())
        } else {
            worker.config.clone()
        };
        worker.observer.refresh_started();
        match refresh_catalog_cache(
            &current,
            RefreshDeadline::starting_now(worker.refresh_deadline),
            &worker.cancelled,
        ) {
            Ok(true) => {
                worker.changed.store(true, Ordering::Release);
                worker.observer.refresh_published();
            }
            Ok(false) => worker.observer.refresh_unchanged(),
            Err(error) => worker.observer.refresh_failed(&error),
        }
        if worker.cancelled.load(Ordering::Acquire) {
            return;
        }
        completed_generation = requested;
    }
}

fn wait_for_refresh_request(
    completed_generation: u64,
    cancelled: &AtomicBool,
    requested_generation: &AtomicU64,
    wake: &(Mutex<()>, Condvar),
    refresh_interval: Duration,
) -> Result<Option<u64>, ShellError> {
    let mut guard = wake.0.lock().map_err(|_| {
        ShellError::new(
            ErrorCode::Io,
            "the catalog refresh worker lock was poisoned",
        )
        .with_help("Restart Quirl to create a fresh catalog worker")
    })?;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        let requested = requested_generation.load(Ordering::Acquire);
        if requested > completed_generation {
            return Ok(Some(requested));
        }
        let (next_guard, wait) = wake.1.wait_timeout(guard, refresh_interval).map_err(|_| {
            ShellError::new(ErrorCode::Io, "the catalog refresh wait lock was poisoned")
                .with_help("Restart Quirl to create a fresh catalog worker")
        })?;
        guard = next_guard;
        if wait.timed_out() {
            increment_generation(requested_generation, "catalog refresh timer generation")?;
        }
    }
}

fn increment_generation(counter: &AtomicU64, name: &str) -> Result<u64, ShellError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map(|value| value + 1)
        .map_err(|_| {
            ShellError::new(ErrorCode::ResourceLimit, format!("{name} was exhausted"))
                .with_help("Restart Quirl to reset the bounded generation counter")
        })
}

fn refresh_catalog_cache(
    config: &DiscoveryConfig,
    deadline: RefreshDeadline,
    cancelled: &AtomicBool,
) -> Result<bool, ShellError> {
    let mut budget = IndexBuildBudget::new(IndexBounds::PRODUCTION);
    budget.roots = config
        .path_roots
        .len()
        .saturating_add(config.fish_roots.len())
        .saturating_add(config.bash_roots.len())
        .saturating_add(config.zsh_roots.len())
        .saturating_add(config.help_roots.len())
        .saturating_add(config.man_roots.len());
    ensure_index_limit("roots", budget.bounds.roots_max, budget.roots)?;
    let snapshot = discover_sources(config, &mut budget, deadline, cancelled)?;
    if discovery_cache_is_current(config, &snapshot)? {
        return Ok(false);
    }
    ensure_refresh_active(deadline, cancelled, "before source import")?;
    let (mut catalog, mut import_diagnostics) = catalog_from_files_checked(
        &snapshot.fish_files,
        &snapshot.bash_files,
        &snapshot.zsh_files,
        &snapshot.help_files,
        &snapshot.man_files,
        &mut budget,
        || ensure_refresh_active(deadline, cancelled, "while importing sources"),
    )?;
    catalog.merge(external_commands(&snapshot.executables, &snapshot.sources));
    ensure_refresh_active(deadline, cancelled, "before cache commit")?;
    let catalog_fingerprint = fingerprint_bytes(&encode_catalog(&catalog)?);
    let mut diagnostics = snapshot.diagnostics;
    diagnostics.append(&mut import_diagnostics);
    let state = DiscoveryState {
        version: DISCOVERY_STATE_VERSION,
        catalog_schema_version: catalog.schema_version,
        refreshed_unix_ms: unix_time_ms(),
        source_fingerprint: snapshot.fingerprint,
        catalog_fingerprint,
        sources: snapshot.sources,
        diagnostics,
    };
    write_catalog_atomically(&config.index_path, &catalog, Some(&state))?;
    Ok(true)
}

fn discovery_cache_is_current(
    config: &DiscoveryConfig,
    snapshot: &DiscoverySnapshot,
) -> Result<bool, ShellError> {
    let Ok(bytes) = read_index(&config.index_path) else {
        return Ok(false);
    };
    let Ok((catalog, state_json)) = intelligence::decode_database(&bytes, &config.index_path)
    else {
        return Ok(false);
    };
    let Some(state_json) = state_json else {
        return Ok(false);
    };
    let Ok(state) = serde_json::from_str::<DiscoveryState>(&state_json) else {
        return Ok(false);
    };
    let age_ms = unix_time_ms().saturating_sub(state.refreshed_unix_ms);
    let stale_ms = u64::try_from(config.stale_after.as_millis()).unwrap_or(u64::MAX);
    if state.version != DISCOVERY_STATE_VERSION
        || state.catalog_schema_version != Catalog::builtin().schema_version
        || state.source_fingerprint != snapshot.fingerprint
        || state.sources != snapshot.sources
        || age_ms >= stale_ms
    {
        return Ok(false);
    }
    if fingerprint_bytes(&encode_catalog(&catalog)?) != state.catalog_fingerprint {
        return Ok(false);
    }
    Ok(true)
}

fn ensure_refresh_active(
    deadline: RefreshDeadline,
    cancelled: &AtomicBool,
    stage: &str,
) -> Result<(), ShellError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(
            ShellError::new(ErrorCode::ResourceLimit, "catalog discovery was cancelled")
                .with_context(stage.to_owned())
                .with_help("Retry discovery in a running interactive session"),
        );
    }
    if Instant::now() >= deadline.expires_at {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "catalog discovery exceeded its refresh deadline",
        )
        .with_context(format!(
            "limit: {} ms; observed: at least {} ms; stage: {stage}",
            deadline.limit.as_millis(),
            deadline.limit.as_millis(),
        ))
        .with_help("Reduce PATH or declarative completion sources and retry"));
    }
    Ok(())
}

fn load_catalog_at(path: &Path) -> Catalog {
    match read_index(path) {
        Ok(bytes) => decode_catalog(&bytes, path)
            .map(merge_cached_catalog)
            .unwrap_or_else(|_| Catalog::builtin()),
        Err(_) => Catalog::builtin(),
    }
}

fn merge_cached_catalog(mut cached: Catalog) -> Catalog {
    // The index cache contains imported discovery facts, not authenticated
    // installation state. Only the validated plugin lock snapshot may confer
    // plugin provenance and make a command eligible for agent execution. A
    // cached builtin is also an obsolete copy of the running binary's contract:
    // discard it whole so removed flags and renamed mode values cannot merge
    // back into the current authoritative record.
    let mut current = Catalog::builtin();
    let builtin_ids = current
        .commands
        .iter()
        .map(|command| command.id.clone())
        .collect::<BTreeSet<_>>();
    cached.commands.retain(|command| {
        command.provenance.source != Provenance::Plugin
            && !(command.provenance.source == Provenance::Builtin
                && builtin_ids.contains(&command.id))
    });
    current.merge(cached.commands);
    current
}

fn build_index(
    fish_roots: Vec<PathBuf>,
    bash_roots: Vec<PathBuf>,
    zsh_roots: Vec<PathBuf>,
    help_roots: Vec<PathBuf>,
    man_roots: Vec<PathBuf>,
    output: Option<PathBuf>,
    format: IndexOutputFormat,
) -> Result<i32, ShellError> {
    let fish_roots = if fish_roots.is_empty() {
        default_fish_roots()
    } else {
        fish_roots
    };
    let bash_roots = if bash_roots.is_empty() {
        default_bash_roots()
    } else {
        bash_roots
    };
    let zsh_roots = if zsh_roots.is_empty() {
        default_zsh_roots()
    } else {
        zsh_roots
    };
    let mut budget = IndexBuildBudget::new(IndexBounds::PRODUCTION);
    budget.roots = fish_roots
        .len()
        .saturating_add(bash_roots.len())
        .saturating_add(zsh_roots.len())
        .saturating_add(help_roots.len())
        .saturating_add(man_roots.len());
    ensure_index_limit("roots", budget.bounds.roots_max, budget.roots)?;
    let fish_files = completion_files(&fish_roots, Some("fish"), &mut budget)?;
    let bash_files = completion_files(&bash_roots, None, &mut budget)?;
    let zsh_files = completion_files(&zsh_roots, None, &mut budget)?;
    let help_files = completion_files(&help_roots, None, &mut budget)?;
    let man_files = completion_files(&man_roots, None, &mut budget)?;
    let (catalog, diagnostics) = catalog_from_files(
        &fish_files,
        &bash_files,
        &zsh_files,
        &help_files,
        &man_files,
        &mut budget,
    )?;
    let output = output.or_else(default_index_path).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "cannot determine a completion-index path",
        )
        .with_help("Pass an explicit destination with `quirl index build --output <path>`")
    })?;
    write_catalog_atomically(&output, &catalog, None)?;
    let report = BuildReport {
        index: output,
        source_files: fish_files.len()
            + bash_files.len()
            + zsh_files.len()
            + help_files.len()
            + man_files.len(),
        commands: catalog.commands.len(),
        options: catalog
            .commands
            .iter()
            .map(|command| command.options.len())
            .sum(),
        diagnostics,
    };
    match format {
        IndexOutputFormat::Text => {
            println!(
                "indexed {} commands and {} options from {} files into {}",
                report.commands,
                report.options,
                report.source_files,
                escape_terminal_controls(&report.index.display().to_string())
            );
            for diagnostic in &report.diagnostics {
                eprintln!(
                    "{}:{}: skipped completion declaration: {}",
                    escape_terminal_controls(&diagnostic.origin),
                    diagnostic.line,
                    escape_terminal_controls(&diagnostic.message)
                );
            }
        }
        IndexOutputFormat::Json => print_json(&report)?,
    }
    Ok(0)
}

fn explain_index(
    command: &str,
    index: Option<PathBuf>,
    format: IndexOutputFormat,
) -> Result<i32, ShellError> {
    let path = index.or_else(default_index_path).ok_or_else(|| {
        ShellError::new(ErrorCode::InvalidArgument, "cannot determine an index path")
            .with_help("Pass `--index <path>` or configure HOME/XDG_CACHE_HOME")
    })?;
    let source = read_index(&path)
        .map_err(|error| error.with_help("Build the index first with `quirl index build`"))?;
    let catalog = decode_catalog(&source, &path)?;
    let explanation = catalog.explain(command).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidCommand,
            format!("the completion index has no command `{command}`"),
        )
        .with_help("Run `quirl index build` to refresh installed completion metadata")
    })?;
    match format {
        IndexOutputFormat::Json => print_json(&explanation)?,
        IndexOutputFormat::Text => {
            println!("{}", escape_terminal_controls(&explanation.command));
            for fact in explanation.facts {
                let origin = fact
                    .provenance
                    .origin
                    .as_deref()
                    .unwrap_or("compiled into Quirl");
                let fingerprint = fact
                    .provenance
                    .fingerprint
                    .as_deref()
                    .map_or(String::new(), |value| format!(" · {value}"));
                println!(
                    "  {} `{}` ← {:?} / {:?} · {}{}",
                    escape_terminal_controls(&fact.fact),
                    escape_terminal_controls(&fact.value),
                    fact.provenance.source,
                    fact.provenance.confidence,
                    escape_terminal_controls(origin),
                    escape_terminal_controls(&fingerprint)
                );
            }
        }
    }
    Ok(0)
}

fn catalog_from_files(
    fish_files: &[PathBuf],
    bash_files: &[PathBuf],
    zsh_files: &[PathBuf],
    help_files: &[PathBuf],
    man_files: &[PathBuf],
    budget: &mut IndexBuildBudget,
) -> Result<(Catalog, Vec<ImportDiagnostic>), ShellError> {
    catalog_from_files_checked(
        fish_files,
        bash_files,
        zsh_files,
        help_files,
        man_files,
        budget,
        || Ok(()),
    )
}

fn catalog_from_files_checked(
    fish_files: &[PathBuf],
    bash_files: &[PathBuf],
    zsh_files: &[PathBuf],
    help_files: &[PathBuf],
    man_files: &[PathBuf],
    budget: &mut IndexBuildBudget,
    mut check_active: impl FnMut() -> Result<(), ShellError>,
) -> Result<(Catalog, Vec<ImportDiagnostic>), ShellError> {
    let mut catalog = Catalog::builtin();
    let mut diagnostics = Vec::new();
    for path in fish_files {
        check_active()?;
        let source = read_completion(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_fish(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in bash_files {
        check_active()?;
        let source = read_completion(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_bash(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in zsh_files {
        check_active()?;
        let source = read_completion(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_zsh(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in help_files {
        check_active()?;
        let source = read_documentation(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_help(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in man_files {
        check_active()?;
        let source = match read_documentation(path, budget) {
            Ok(source) => source,
            Err(error) => {
                push_index_diagnostic(
                    &mut diagnostics,
                    budget,
                    path,
                    format_index_error("skipped unreadable man page", &error),
                )?;
                continue;
            }
        };
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_man(&source, &path.display().to_string()),
            budget,
        )?;
    }
    Ok((catalog, diagnostics))
}

fn merge_bounded_report(
    catalog: &mut Catalog,
    diagnostics: &mut Vec<ImportDiagnostic>,
    report: ImportReport,
    budget: &mut IndexBuildBudget,
) -> Result<(), ShellError> {
    let report_records = report.commands.iter().fold(0_usize, |count, command| {
        count
            .saturating_add(1)
            .saturating_add(command.options.len())
    });
    let records = budget.records.saturating_add(report_records);
    ensure_index_limit("catalog records", budget.bounds.records_max, records)?;
    let diagnostic_count = budget.diagnostics.saturating_add(report.diagnostics.len());
    ensure_index_limit(
        "import diagnostics",
        budget.bounds.diagnostics_max,
        diagnostic_count,
    )?;
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, &report).map_err(json_error)?;
    let retained_bytes = budget.retained_bytes.saturating_add(counter.0);
    ensure_index_limit(
        "retained index text",
        budget.bounds.retained_bytes_max,
        retained_bytes,
    )?;

    budget.records = records;
    budget.diagnostics = diagnostic_count;
    budget.retained_bytes = retained_bytes;
    diagnostics.extend(catalog.merge_report(report));
    Ok(())
}

struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BoundedBytesWriter {
    bytes: Vec<u8>,
    bytes_max: usize,
    exceeded: bool,
}

impl BoundedBytesWriter {
    fn new(bytes_max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            bytes_max,
            exceeded: false,
        }
    }
}

impl Write for BoundedBytesWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.bytes_max.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            self.bytes.extend_from_slice(&bytes[..remaining]);
            self.exceeded = true;
            return Err(io::Error::other("bounded index output exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn admit_index_path(path: &Path, budget: &mut IndexBuildBudget) -> Result<(), ShellError> {
    let files = budget.files.saturating_add(1);
    ensure_index_limit("source files", budget.bounds.files_max, files)?;
    let path_bytes = budget
        .path_bytes
        .saturating_add(path.as_os_str().as_encoded_bytes().len());
    ensure_index_limit(
        "retained path bytes",
        budget.bounds.path_bytes_max,
        path_bytes,
    )?;
    budget.files = files;
    budget.path_bytes = path_bytes;
    Ok(())
}

fn admit_source_bytes(bytes: usize, budget: &mut IndexBuildBudget) -> Result<(), ShellError> {
    let source_bytes = budget.source_bytes.saturating_add(bytes);
    ensure_index_limit("source bytes", budget.bounds.source_bytes_max, source_bytes)?;
    budget.source_bytes = source_bytes;
    Ok(())
}

fn ensure_index_limit(kind: &str, limit: usize, observed: usize) -> Result<(), ShellError> {
    if observed <= limit {
        Ok(())
    } else {
        Err(index_limit_error(kind, limit, observed))
    }
}

fn index_limit_error(kind: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("completion index exceeds its {kind} limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Reduce the number or size of index sources and retry")
}

fn nonregular_index_input(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "completion index input {} is not a regular file or directory",
            path.display()
        ),
    )
    .with_help("Remove symlinks and special files from index input roots")
}

fn completion_files(
    roots: &[PathBuf],
    required_extension: Option<&str>,
    budget: &mut IndexBuildBudget,
) -> Result<Vec<PathBuf>, ShellError> {
    completion_files_checked(roots, required_extension, budget, false, || Ok(()))
}

fn completion_files_checked(
    roots: &[PathBuf],
    required_extension: Option<&str>,
    budget: &mut IndexBuildBudget,
    follow_source_symlinks: bool,
    mut check_active: impl FnMut() -> Result<(), ShellError>,
) -> Result<Vec<PathBuf>, ShellError> {
    let mut files = Vec::new();
    for root in roots {
        check_active()?;
        let resolved_root;
        let root = if follow_source_symlinks
            && fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            resolved_root =
                fs::canonicalize(root).map_err(|error| index_io_error("resolve", root, error))?;
            resolved_root.as_path()
        } else {
            root.as_path()
        };
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_file() => {
                admit_index_path(root, budget)?;
                files.push(root.to_path_buf());
            }
            Ok(metadata) if metadata.file_type().is_dir() => {
                let entries =
                    fs::read_dir(root).map_err(|error| index_io_error("enumerate", root, error))?;
                for entry in entries {
                    check_active()?;
                    budget.entries = budget.entries.saturating_add(1);
                    ensure_index_limit(
                        "directory entries",
                        budget.bounds.entries_max,
                        budget.entries,
                    )?;
                    let entry = entry.map_err(|error| index_io_error("enumerate", root, error))?;
                    let path = entry.path();
                    let kind = entry
                        .file_type()
                        .map_err(|error| index_io_error("inspect", &path, error))?;
                    if !kind.is_file() {
                        if kind.is_symlink() {
                            if follow_source_symlinks {
                                let target = fs::canonicalize(&path)
                                    .map_err(|error| index_io_error("resolve", &path, error))?;
                                let target_metadata = fs::symlink_metadata(&target)
                                    .map_err(|error| index_io_error("inspect", &target, error))?;
                                if target_metadata.file_type().is_file()
                                    && required_extension.is_none_or(|extension| {
                                        path.extension()
                                            .is_some_and(|candidate| candidate == extension)
                                    })
                                {
                                    admit_index_path(&target, budget)?;
                                    files.push(target);
                                }
                                continue;
                            }
                            return Err(nonregular_index_input(&path));
                        }
                        continue;
                    }
                    if required_extension.is_none_or(|extension| {
                        path.extension()
                            .is_some_and(|candidate| candidate == extension)
                    }) {
                        admit_index_path(&path, budget)?;
                        files.push(path);
                    }
                }
            }
            Ok(_) => return Err(nonregular_index_input(root)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(index_io_error("inspect", root, error)),
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn discover_man_files(
    roots: &[PathBuf],
    command_names: &BTreeSet<String>,
    budget: &mut IndexBuildBudget,
    deadline: RefreshDeadline,
    cancelled: &AtomicBool,
) -> Result<(Vec<PathBuf>, Vec<ImportDiagnostic>), ShellError> {
    let mut candidates = Vec::new();
    let mut diagnostics = Vec::new();
    let mut candidate_path_bytes = 0usize;
    for (root_priority, root) in roots.iter().enumerate() {
        ensure_refresh_active(deadline, cancelled, "while scanning man-page roots")?;
        let metadata = match fs::metadata(root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                push_discovery_diagnostic(
                    &mut diagnostics,
                    budget,
                    root,
                    format!("skipped man-page root: {error}"),
                )?;
                continue;
            }
        };
        if metadata.file_type().is_file() {
            admit_man_candidate(
                root,
                root_priority,
                command_names,
                &mut candidates,
                &mut diagnostics,
                &mut candidate_path_bytes,
                budget,
            )?;
            continue;
        }
        if !metadata.file_type().is_dir() {
            continue;
        }
        collect_man_root_candidates(
            root,
            root_priority,
            command_names,
            &mut candidates,
            &mut diagnostics,
            &mut candidate_path_bytes,
            budget,
            deadline,
            cancelled,
        )?;
    }
    select_man_candidates(candidates, diagnostics, budget)
}

#[allow(clippy::too_many_arguments)]
fn collect_man_root_candidates(
    root: &Path,
    root_priority: usize,
    command_names: &BTreeSet<String>,
    candidates: &mut Vec<ManCandidate>,
    diagnostics: &mut Vec<ImportDiagnostic>,
    candidate_path_bytes: &mut usize,
    budget: &mut IndexBuildBudget,
    deadline: RefreshDeadline,
    cancelled: &AtomicBool,
) -> Result<(), ShellError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            push_discovery_diagnostic(
                diagnostics,
                budget,
                root,
                format!("skipped man-page root: {error}"),
            )?;
            return Ok(());
        }
    };
    for entry in entries {
        ensure_refresh_active(deadline, cancelled, "while scanning man-page entries")?;
        budget.entries = budget.entries.saturating_add(1);
        ensure_index_limit(
            "directory entries",
            budget.bounds.entries_max,
            budget.entries,
        )?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                push_discovery_diagnostic(
                    diagnostics,
                    budget,
                    root,
                    format!("skipped unreadable man-page entry: {error}"),
                )?;
                continue;
            }
        };
        admit_man_candidate(
            &entry.path(),
            root_priority,
            command_names,
            candidates,
            diagnostics,
            candidate_path_bytes,
            budget,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn admit_man_candidate(
    path: &Path,
    root_priority: usize,
    command_names: &BTreeSet<String>,
    candidates: &mut Vec<ManCandidate>,
    diagnostics: &mut Vec<ImportDiagnostic>,
    candidate_path_bytes: &mut usize,
    budget: &mut IndexBuildBudget,
) -> Result<(), ShellError> {
    let Some((command, compressed)) = man_page_command(path) else {
        return Ok(());
    };
    if !command_names.contains(&command) {
        return Ok(());
    }
    let candidate_path = if compressed {
        path.to_path_buf()
    } else {
        match resolve_plain_man_page(path) {
            Ok(candidate) => candidate,
            Err(error) => {
                push_discovery_diagnostic(diagnostics, budget, path, error)?;
                return Ok(());
            }
        }
    };
    let path_bytes = candidate_path.as_os_str().as_encoded_bytes().len();
    let observed = candidate_path_bytes.saturating_add(path_bytes);
    ensure_index_limit(
        "man candidate path bytes",
        MAN_CANDIDATE_PATH_BYTES_MAX,
        observed,
    )?;
    *candidate_path_bytes = observed;
    candidates.push(ManCandidate {
        command,
        path: candidate_path,
        root_priority,
        compressed,
    });
    Ok(())
}

fn resolve_plain_man_page(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("skipped unreadable man page: {error}"))?;
    let candidate = if metadata.file_type().is_symlink() {
        fs::canonicalize(path)
            .map_err(|error| format!("skipped unresolved man-page alias: {error}"))?
    } else {
        path.to_path_buf()
    };
    let metadata = fs::symlink_metadata(&candidate)
        .map_err(|error| format!("skipped unreadable man-page target: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("skipped non-regular man-page target".to_owned());
    }
    if metadata.len() > u64::try_from(DOCUMENTATION_READ_LIMIT).unwrap_or(u64::MAX) {
        return Err(format!(
            "skipped oversized man page: limit {} bytes; observed {} bytes",
            DOCUMENTATION_READ_LIMIT,
            metadata.len()
        ));
    }
    Ok(candidate)
}

fn select_man_candidates(
    mut candidates: Vec<ManCandidate>,
    mut diagnostics: Vec<ImportDiagnostic>,
    budget: &mut IndexBuildBudget,
) -> Result<(Vec<PathBuf>, Vec<ImportDiagnostic>), ShellError> {
    candidates.sort_by(|left, right| {
        left.command
            .cmp(&right.command)
            .then_with(|| left.compressed.cmp(&right.compressed))
            .then_with(|| left.root_priority.cmp(&right.root_priority))
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut files = Vec::new();
    let mut selected_commands = BTreeSet::new();
    let mut selected_targets = BTreeSet::new();
    for candidate in candidates {
        if files.len() == AUTOMATIC_MAN_PAGES_MAX {
            break;
        }
        if !selected_commands.insert(candidate.command) {
            continue;
        }
        if candidate.compressed {
            push_discovery_diagnostic(
                &mut diagnostics,
                budget,
                &candidate.path,
                "skipped compressed man page because automatic discovery has no decompressor",
            )?;
            continue;
        }
        if selected_targets.insert(candidate.path.clone()) {
            admit_index_path(&candidate.path, budget)?;
            files.push(candidate.path);
        }
    }
    files.sort();
    Ok((files, diagnostics))
}

fn man_page_command(path: &Path) -> Option<(String, bool)> {
    let file_name = path.file_name()?.to_str()?;
    let (name, compressed) = file_name
        .strip_suffix(".gz")
        .map_or((file_name, false), |name| (name, true));
    let command = name
        .rsplit_once('.')
        .filter(|(_, section)| {
            section.starts_with(|character: char| character.is_ascii_digit())
                && section
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .map(|(command, _)| command)
        .or_else(|| name.strip_suffix(".man.txt"))
        .or_else(|| name.strip_suffix(".man"))
        .or_else(|| name.strip_suffix(".txt"))?;
    (!command.is_empty()).then(|| (command.to_owned(), compressed))
}

fn push_discovery_diagnostic(
    diagnostics: &mut Vec<ImportDiagnostic>,
    budget: &mut IndexBuildBudget,
    origin: &Path,
    message: impl AsRef<str>,
) -> Result<(), ShellError> {
    if diagnostics.len() == AUTOMATIC_MAN_DIAGNOSTICS_MAX {
        return Ok(());
    }
    push_index_diagnostic(diagnostics, budget, origin, message)
}

fn push_index_diagnostic(
    diagnostics: &mut Vec<ImportDiagnostic>,
    budget: &mut IndexBuildBudget,
    origin: &Path,
    message: impl AsRef<str>,
) -> Result<(), ShellError> {
    let diagnostic = ImportDiagnostic {
        origin: truncate_utf8_owned(
            &origin.display().to_string(),
            INDEX_DIAGNOSTIC_ORIGIN_BYTES_MAX,
        ),
        line: 1,
        message: truncate_utf8_owned(message.as_ref(), INDEX_DIAGNOSTIC_MESSAGE_BYTES_MAX),
    };
    admit_index_diagnostic(&diagnostic, budget)?;
    diagnostics.push(diagnostic);
    Ok(())
}

fn admit_index_diagnostic(
    diagnostic: &ImportDiagnostic,
    budget: &mut IndexBuildBudget,
) -> Result<(), ShellError> {
    let diagnostics = budget.diagnostics.saturating_add(1);
    ensure_index_limit(
        "import diagnostics",
        budget.bounds.diagnostics_max,
        diagnostics,
    )?;
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, diagnostic).map_err(json_error)?;
    let retained_bytes = budget.retained_bytes.saturating_add(counter.0);
    ensure_index_limit(
        "retained index text",
        budget.bounds.retained_bytes_max,
        retained_bytes,
    )?;
    budget.diagnostics = diagnostics;
    budget.retained_bytes = retained_bytes;
    Ok(())
}

fn truncate_utf8_owned(value: &str, bytes_max: usize) -> String {
    if value.len() <= bytes_max {
        return value.to_owned();
    }
    let mut end = bytes_max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn format_index_error(prefix: &str, error: &ShellError) -> String {
    error.details.context.first().map_or_else(
        || format!("{prefix}: {}", error.message),
        |context| format!("{prefix}: {} ({context})", error.message),
    )
}

fn discover_sources(
    config: &DiscoveryConfig,
    budget: &mut IndexBuildBudget,
    deadline: RefreshDeadline,
    cancelled: &AtomicBool,
) -> Result<DiscoverySnapshot, ShellError> {
    let fish_files =
        completion_files_checked(&config.fish_roots, Some("fish"), budget, true, || {
            ensure_refresh_active(deadline, cancelled, "while scanning fish sources")
        })?;
    ensure_refresh_active(deadline, cancelled, "after fish discovery")?;
    let bash_files = completion_files_checked(&config.bash_roots, None, budget, true, || {
        ensure_refresh_active(deadline, cancelled, "while scanning Bash sources")
    })?;
    ensure_refresh_active(deadline, cancelled, "after Bash discovery")?;
    let zsh_files = completion_files_checked(&config.zsh_roots, None, budget, true, || {
        ensure_refresh_active(deadline, cancelled, "while scanning Zsh sources")
    })?;
    ensure_refresh_active(deadline, cancelled, "after Zsh discovery")?;
    let help_files = completion_files_checked(&config.help_roots, None, budget, true, || {
        ensure_refresh_active(deadline, cancelled, "while scanning help sources")
    })?;
    let executables = discover_path_executables(&config.path_roots, budget, deadline, cancelled)?;
    let command_names: BTreeSet<String> = executables
        .iter()
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    let (man_files, mut diagnostics) = discover_man_files(
        &config.man_roots,
        &command_names,
        budget,
        deadline,
        cancelled,
    )?;

    let mut sources = Vec::with_capacity(
        fish_files
            .len()
            .saturating_add(bash_files.len())
            .saturating_add(zsh_files.len())
            .saturating_add(help_files.len())
            .saturating_add(man_files.len())
            .saturating_add(executables.len()),
    );
    for (kind, files) in [
        (DiscoverySourceKind::Fish, fish_files.as_slice()),
        (DiscoverySourceKind::Bash, bash_files.as_slice()),
        (DiscoverySourceKind::Zsh, zsh_files.as_slice()),
        (DiscoverySourceKind::Help, help_files.as_slice()),
        (DiscoverySourceKind::PathExecutable, executables.as_slice()),
    ] {
        for path in files {
            ensure_refresh_active(deadline, cancelled, "while fingerprinting sources")?;
            sources.push(observe_source(kind, path, budget)?);
        }
    }
    let mut admitted_man_files = Vec::with_capacity(man_files.len());
    for path in man_files {
        ensure_refresh_active(deadline, cancelled, "while fingerprinting man pages")?;
        match observe_source(DiscoverySourceKind::Man, &path, budget) {
            Ok(source) => {
                sources.push(source);
                admitted_man_files.push(path);
            }
            Err(error) => push_discovery_diagnostic(
                &mut diagnostics,
                budget,
                &path,
                format_index_error("skipped man page during fingerprinting", &error),
            )?,
        }
    }
    sources.sort();
    let fingerprint = fingerprint_sources(&sources);
    Ok(DiscoverySnapshot {
        sources,
        executables,
        fish_files,
        bash_files,
        zsh_files,
        help_files,
        man_files: admitted_man_files,
        diagnostics,
        fingerprint,
    })
}

fn discover_path_executables(
    roots: &[PathBuf],
    budget: &mut IndexBuildBudget,
    deadline: RefreshDeadline,
    cancelled: &AtomicBool,
) -> Result<Vec<PathBuf>, ShellError> {
    let mut commands = Vec::new();
    let mut names = BTreeSet::new();
    for root in roots {
        ensure_refresh_active(deadline, cancelled, "while scanning PATH")?;
        match fs::metadata(root) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => continue,
            Err(error) if path_candidate_error_is_skippable(&error) => continue,
            Err(error) => return Err(index_io_error("inspect", root, error)),
        }
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if path_candidate_error_is_skippable(&error) => continue,
            Err(error) => return Err(index_io_error("enumerate", root, error)),
        };
        for entry in entries {
            budget.entries = budget.entries.saturating_add(1);
            ensure_index_limit(
                "directory entries",
                budget.bounds.entries_max,
                budget.entries,
            )?;
            ensure_refresh_active(deadline, cancelled, "while scanning PATH entries")?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if path_candidate_error_is_skippable(&error) => continue,
                Err(error) => return Err(index_io_error("enumerate", root, error)),
            };
            let path = entry.path();
            let metadata = match fs::metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => metadata,
                Ok(_) => continue,
                Err(error) if path_candidate_error_is_skippable(&error) => continue,
                Err(error) => return Err(index_io_error("inspect", &path, error)),
            };
            if !is_executable(&metadata) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.is_empty() || name.chars().any(char::is_whitespace) {
                continue;
            }
            if names.insert(name.to_owned()) {
                admit_index_path(&path, budget)?;
                commands.push(path);
            }
        }
    }
    commands.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(commands)
}

fn path_candidate_error_is_skippable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    )
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    metadata.mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

fn observe_source(
    kind: DiscoverySourceKind,
    path: &Path,
    budget: &mut IndexBuildBudget,
) -> Result<DiscoverySource, ShellError> {
    let metadata = fs::metadata(path).map_err(|error| index_io_error("inspect", path, error))?;
    let modified_unix_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
        .unwrap_or(0);
    let content_fingerprint = match kind {
        DiscoverySourceKind::PathExecutable => None,
        DiscoverySourceKind::Fish | DiscoverySourceKind::Bash | DiscoverySourceKind::Zsh => {
            Some(fingerprint_bytes(read_completion(path, budget)?.as_bytes()))
        }
        DiscoverySourceKind::Help | DiscoverySourceKind::Man => Some(fingerprint_bytes(
            read_documentation(path, budget)?.as_bytes(),
        )),
    };
    let mut identity = Vec::new();
    identity.extend_from_slice(path.as_os_str().as_encoded_bytes());
    identity.extend_from_slice(&metadata.len().to_le_bytes());
    identity.extend_from_slice(&modified_unix_nanos.to_le_bytes());
    if let Some(content_fingerprint) = content_fingerprint {
        identity.extend_from_slice(content_fingerprint.as_bytes());
    }
    Ok(DiscoverySource {
        kind,
        path: path.to_path_buf(),
        bytes: metadata.len(),
        modified_unix_nanos,
        fingerprint: fingerprint_bytes(&identity),
    })
}

fn external_commands(executables: &[PathBuf], sources: &[DiscoverySource]) -> Vec<CommandSpec> {
    executables
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            let source_fingerprint = sources
                .iter()
                .find(|source| {
                    source.kind == DiscoverySourceKind::PathExecutable && source.path == *path
                })?
                .fingerprint
                .clone();
            Some(CommandSpec {
                id: format!("external:{name}"),
                version: None,
                path: name.clone(),
                aliases: Vec::new(),
                parent: None,
                signature: name,
                summary: "Installed command discovered on PATH".to_owned(),
                details: "Executable presence was observed without running the command or loading shell startup files.".to_owned(),
                options: Vec::new(),
                examples: Vec::new(),
                io: IoContract::default(),
                effects: vec![Effect::SpawnProcess],
                exit_codes: Default::default(),
                provenance: ProvenanceInfo::imported(
                    Provenance::External,
                    Confidence::Medium,
                    path.display().to_string(),
                    source_fingerprint,
                ),
            })
        })
        .collect()
}

fn fingerprint_sources(sources: &[DiscoverySource]) -> String {
    let mut bytes = Vec::new();
    for source in sources {
        bytes.extend_from_slice(format!("{:?}\0", source.kind).as_bytes());
        bytes.extend_from_slice(source.path.as_os_str().as_encoded_bytes());
        bytes.extend_from_slice(&source.bytes.to_le_bytes());
        bytes.extend_from_slice(&source.modified_unix_nanos.to_le_bytes());
        bytes.extend_from_slice(source.fingerprint.as_bytes());
    }
    fingerprint_bytes(&bytes)
}

fn fingerprint_bytes(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn default_fish_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/share/fish/completions"),
        PathBuf::from("/usr/share/fish/vendor_completions.d"),
        PathBuf::from("/opt/homebrew/share/fish/completions"),
        PathBuf::from("/opt/homebrew/share/fish/vendor_completions.d"),
    ];
    if let Some(config) = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        roots.push(config.join("fish/completions"));
    } else if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".config/fish/completions"));
    }
    roots
}

fn default_bash_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/bash-completion/completions"),
        PathBuf::from("/etc/bash_completion.d"),
        PathBuf::from("/opt/homebrew/etc/bash_completion.d"),
        PathBuf::from("/usr/local/etc/bash_completion.d"),
    ]
}

fn default_zsh_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/zsh/site-functions"),
        PathBuf::from("/usr/local/share/zsh/site-functions"),
        PathBuf::from("/opt/homebrew/share/zsh/site-functions"),
    ]
}

fn default_man_roots() -> Vec<PathBuf> {
    let mut roots = default_documentation_roots("QUIRL_MAN_PATH", "man");
    roots.extend([
        PathBuf::from("/usr/share/man/man1"),
        PathBuf::from("/usr/local/share/man/man1"),
        PathBuf::from("/opt/homebrew/share/man/man1"),
    ]);
    roots
}

fn default_documentation_roots(variable: &str, kind: &str) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = env::var_os(variable)
        .as_deref()
        .map(env::split_paths)
        .into_iter()
        .flatten()
        .collect();
    if let Some(data) = env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        roots.push(data.join("quirl").join(kind));
    } else if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".local/share/quirl").join(kind));
    }
    roots.push(PathBuf::from("/usr/local/share/quirl").join(kind));
    roots.push(PathBuf::from("/usr/share/quirl").join(kind));
    roots
}

fn default_index_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("QUIRL_INDEX_PATH") {
        return Some(PathBuf::from(path));
    }
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|cache| cache.join("quirl/catalog.sqlite3"))
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn read_completion(path: &Path, budget: &mut IndexBuildBudget) -> Result<String, ShellError> {
    let source = read_index_utf8(
        path,
        COMPLETION_READ_LIMIT,
        "completion source",
        "Supply completion declarations in a readable UTF-8 regular file at or below 4 MiB",
    )?;
    admit_source_bytes(source.len(), budget)?;
    Ok(source)
}

fn read_documentation(path: &Path, budget: &mut IndexBuildBudget) -> Result<String, ShellError> {
    let source = read_index_utf8(
        path,
        DOCUMENTATION_READ_LIMIT,
        "documentation source",
        "Supply help or man text in a readable UTF-8 regular file at or below 1 MiB",
    )?;
    admit_source_bytes(source.len(), budget)?;
    Ok(source)
}

pub(crate) fn read_index(path: &Path) -> Result<Vec<u8>, ShellError> {
    read_index_bytes(
        path,
        intelligence::DATABASE_BYTES_MAX,
        "command database",
        "Build a readable regular database with `quirl index build`",
    )
}

fn read_index_utf8(
    path: &Path,
    bytes_max: usize,
    context: &str,
    help: &str,
) -> Result<String, ShellError> {
    let bytes = read_index_bytes(path, bytes_max, context, help)?;
    String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("{} is not UTF-8 {context} text", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })
}

fn read_index_bytes(
    path: &Path,
    bytes_max: usize,
    context: &str,
    help: &str,
) -> Result<Vec<u8>, ShellError> {
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not inspect {context} {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })?;
    validate_index_reader_metadata(path, &path_metadata, help)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    let file = options.open(path).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not open {context} {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })?;
    let metadata = file.metadata().map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not inspect {context} {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })?;
    validate_index_reader_metadata(path, &metadata, help)?;
    #[cfg(unix)]
    if path_metadata.dev() != metadata.dev() || path_metadata.ino() != metadata.ino() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "completion index {} changed during admission",
                path.display()
            ),
        )
        .with_help(help));
    }
    let bytes_max_u64 = u64::try_from(bytes_max).unwrap_or(u64::MAX);
    if metadata.len() > bytes_max_u64 {
        return Err(index_read_limit_error(
            path,
            context,
            help,
            bytes_max,
            metadata.len(),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(bytes_max)
            .min(bytes_max),
    );
    file.take(bytes_max_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| index_io_error("read", path, error))?;
    if bytes.len() > bytes_max {
        return Err(index_read_limit_error(
            path,
            context,
            help,
            bytes_max,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        ));
    }
    Ok(bytes)
}

fn validate_index_reader_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    help: &str,
) -> Result<(), ShellError> {
    if !metadata.file_type().is_file() {
        return Err(nonregular_index_input(path).with_help(help));
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != 1 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("completion index {} has hard-link aliases", path.display()),
            )
            .with_context(format!("expected links: 1; observed: {}", metadata.nlink()))
            .with_help(help));
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "completion index {} has unsafe writable permissions",
                    path.display()
                ),
            )
            .with_context(format!("mode: {mode:#o}; forbidden write bits: 0o022"))
            .with_help(help));
        }
    }
    Ok(())
}

fn index_read_limit_error(
    path: &Path,
    context: &str,
    help: &str,
    limit: usize,
    observed: u64,
) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{context} {} exceeds its read limit", path.display()),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help(help)
}

fn decode_catalog(source: &[u8], path: &Path) -> Result<Catalog, ShellError> {
    if source
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(b'{')
    {
        let source = std::str::from_utf8(source).map_err(|error| {
            ShellError::new(
                ErrorCode::Validation,
                format!("{} is not a valid legacy Quirl index", path.display()),
            )
            .with_context(error.to_string())
            .with_help("Rebuild it with `quirl index build`")
        })?;
        return Catalog::from_json(source).map_err(|error| {
            ShellError::new(
                ErrorCode::Validation,
                format!("{} is not a valid legacy Quirl index", path.display()),
            )
            .with_context(error.to_string())
            .with_help("Rebuild it with `quirl index build`")
        });
    }
    intelligence::decode_database(source, path).map(|(catalog, _)| catalog)
}

fn write_catalog_atomically(
    path: &Path,
    catalog: &Catalog,
    discovery_state: Option<&DiscoveryState>,
) -> Result<(), ShellError> {
    let state_json = discovery_state
        .map(serde_json::to_string)
        .transpose()
        .map_err(json_error)?;
    let encoded = intelligence::encode_database(catalog, state_json.as_deref())?;
    write_index_bytes_atomically(path, &encoded, intelligence::DATABASE_BYTES_MAX)
}

fn encode_catalog(catalog: &Catalog) -> Result<Vec<u8>, ShellError> {
    let mut writer = BoundedBytesWriter::new(INDEX_READ_LIMIT);
    if let Err(error) = serde_json::to_writer_pretty(&mut writer, catalog) {
        if writer.exceeded {
            return Err(index_limit_error(
                "serialized bytes",
                INDEX_READ_LIMIT,
                INDEX_READ_LIMIT.saturating_add(1),
            ));
        }
        return Err(json_error(error));
    }
    let encoded = writer.bytes;
    if encoded.len() > INDEX_READ_LIMIT {
        return Err(index_limit_error(
            "serialized bytes",
            INDEX_READ_LIMIT,
            encoded.len(),
        ));
    }
    Ok(encoded)
}

fn write_index_bytes_atomically(
    path: &Path,
    encoded: &[u8],
    bytes_max: usize,
) -> Result<(), ShellError> {
    let _publication = DATABASE_PUBLICATION.lock().map_err(|_| {
        ShellError::new(
            ErrorCode::Io,
            "the command-database publication lock was poisoned",
        )
        .with_help("Restart Quirl before updating the local command database")
    })?;
    write_index_bytes_atomically_unlocked(path, encoded, bytes_max)
}

fn write_index_bytes_atomically_unlocked(
    path: &Path,
    encoded: &[u8],
    bytes_max: usize,
) -> Result<(), ShellError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        create_index_directories(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_index_reader_metadata(
                path,
                &metadata,
                "Use an unlinked regular index file with no group/other write access",
            )?;
            let expected = read_index_bytes(
                path,
                bytes_max,
                "command database",
                "Use an unlinked regular command database",
            )?;
            replace_file_atomically(path, &expected, encoded, AtomicReplaceOptions { bytes_max })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            install_new_index(path, encoded, parent.unwrap_or_else(|| Path::new(".")))
        }
        Err(error) => Err(index_io_error("inspect", path, error)),
    }
}

fn install_new_index(path: &Path, encoded: &[u8], parent: &Path) -> Result<(), ShellError> {
    install_new_index_with_hook(path, encoded, parent, |_| Ok(()))
}

fn install_new_index_with_hook(
    path: &Path,
    encoded: &[u8],
    parent: &Path,
    mut after_stage: impl FnMut(IndexWriteStage) -> io::Result<()>,
) -> Result<(), ShellError> {
    let (temporary, mut file) = create_index_temporary(path)?;
    let mut guard = IndexTemporary::new(temporary.clone(), &file).map_err(|error| {
        error.with_context(format!(
            "failure cleanup preserved index temporary {}",
            temporary.display()
        ))
    })?;
    let split = encoded.len().div_ceil(2);
    file.write_all(&encoded[..split])
        .and_then(|()| file.write_all(&encoded[split..]))
        .and_then(|()| file.sync_all())
        .and_then(|()| after_stage(IndexWriteStage::ContentSynced))
        .map_err(|error| guard.cleanup(index_io_error("write", guard.path(), error)))?;
    validate_index_temporary(guard.path(), &file).map_err(|error| guard.cleanup(error))?;
    drop(file);
    fs::hard_link(guard.path(), path)
        .map_err(|error| guard.cleanup(index_io_error("install", path, error)))?;
    guard.installed(path);
    if let Err(error) = after_stage(IndexWriteStage::Installed) {
        return Err(guard.cleanup(index_io_error("install", path, error)));
    }
    if !guard.owns(path) {
        let error = ShellError::new(
            ErrorCode::Validation,
            format!(
                "index destination {} changed during installation",
                path.display()
            ),
        )
        .with_help("Remove the conflicting index entry and retry");
        return Err(guard.cleanup(error));
    }
    validate_index_installed(path).map_err(|error| guard.cleanup(error))?;
    if let Err(error) = sync_index_directory(parent) {
        return Err(guard.cleanup(error));
    }
    guard
        .temporary
        .as_ref()
        .map(IndexOwnedPath::remove_committed)
        .transpose()
        .map_err(|error| guard.cleanup(index_io_error("clean", guard.path(), error)))?;
    guard.disarm();
    let _ = sync_index_directory(parent);
    Ok(())
}

fn create_index_temporary(path: &Path) -> Result<(PathBuf, File), ShellError> {
    let name = path.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "completion index has no file name",
        )
        .with_help("Choose a regular index destination file")
    })?;
    for _ in 0..INDEX_TEMPORARY_ATTEMPTS_MAX {
        let sequence = NEXT_INDEX_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".quirl-{}-{sequence}.tmp", std::process::id()));
        let temporary = path.with_file_name(temporary_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary) {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                    return Err(
                        index_io_error("secure", &temporary, error).with_context(format!(
                            "failure cleanup preserved index temporary {}",
                            temporary.display()
                        )),
                    );
                }
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(index_io_error("create", &temporary, error)),
        }
    }
    Err(index_limit_error(
        "temporary-name attempts",
        INDEX_TEMPORARY_ATTEMPTS_MAX,
        INDEX_TEMPORARY_ATTEMPTS_MAX,
    ))
}

fn validate_index_temporary(path: &Path, file: &File) -> Result<(), ShellError> {
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| index_io_error("inspect", path, error))?;
    let file_metadata = file
        .metadata()
        .map_err(|error| index_io_error("inspect", path, error))?;
    validate_index_reader_metadata(
        path,
        &path_metadata,
        "Remove the conflicting index temporary and retry",
    )?;
    #[cfg(unix)]
    {
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "index temporary {} changed before installation",
                    path.display()
                ),
            )
            .with_help("Remove the conflicting index temporary and retry"));
        }
        let mode = file_metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("index temporary {} has unsafe permissions", path.display()),
            )
            .with_context(format!("expected mode: 0o600; observed mode: {mode:#o}"))
            .with_help("Remove the conflicting index temporary and retry"));
        }
    }
    Ok(())
}

fn validate_index_installed(path: &Path) -> Result<(), ShellError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| index_io_error("inspect", path, error))?;
    if !metadata.file_type().is_file() {
        return Err(nonregular_index_input(path));
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != 2 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "index destination {} changed during installation",
                    path.display()
                ),
            )
            .with_context(format!("expected links: 2; observed: {}", metadata.nlink()))
            .with_help("Remove the conflicting index entry and retry"));
        }
        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "index destination {} has unsafe permissions",
                    path.display()
                ),
            )
            .with_context(format!("expected mode: 0o600; observed mode: {mode:#o}"))
            .with_help("Remove the conflicting index entry and retry"));
        }
    }
    Ok(())
}

pub(crate) fn create_index_directories(directory: &Path) -> Result<(), ShellError> {
    const DEPTH_MAX: usize = 64;
    let mut missing = Vec::new();
    let mut cursor = directory;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                #[cfg(unix)]
                if metadata.mode() & 0o022 != 0 {
                    return Err(ShellError::new(
                        ErrorCode::Validation,
                        format!(
                            "index directory {} has unsafe writable permissions",
                            cursor.display()
                        ),
                    )
                    .with_context(format!(
                        "mode: {:#o}; forbidden write bits: 0o022",
                        metadata.mode() & 0o777
                    ))
                    .with_help(
                        "Use a cache directory that is not writable by group or other users",
                    ));
                }
                validate_existing_directory_ancestors(cursor)?;
                break;
            }
            Ok(_) => return Err(nonregular_index_input(cursor)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if missing.len() >= DEPTH_MAX {
                    return Err(index_limit_error(
                        "output directory depth",
                        DEPTH_MAX,
                        missing.len() + 1,
                    ));
                }
                missing.push(cursor.to_path_buf());
                cursor = cursor
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
            }
            Err(error) => return Err(index_io_error("inspect", cursor, error)),
        }
    }
    let mut created = Vec::<PathBuf>::new();
    for path in missing.into_iter().rev() {
        if let Err(error) = fs::create_dir(&path) {
            let mut shell_error = index_io_error("create", &path, error);
            while let Some(created_path) = created.pop() {
                shell_error = shell_error.with_context(format!(
                    "index directory {} was preserved because cleanup cannot atomically prove path ownership",
                    created_path.display()
                ));
            }
            return Err(shell_error);
        }
        #[cfg(unix)]
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
            return Err(index_io_error("secure", &path, error).with_context(format!(
                "index directory {} was preserved because cleanup cannot atomically prove path ownership",
                path.display()
            )));
        }
        created.push(path);
    }
    Ok(())
}

fn validate_existing_directory_ancestors(directory: &Path) -> Result<(), ShellError> {
    for ancestor in directory
        .ancestors()
        .skip(1)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
    {
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|error| index_io_error("inspect", ancestor, error))?;
        if !metadata.file_type().is_dir() {
            return Err(nonregular_index_input(ancestor));
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn sync_index_directory(path: &Path) -> Result<(), ShellError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| index_io_error("synchronize", path, error))
}

#[cfg(not(unix))]
pub(crate) fn sync_index_directory(_path: &Path) -> Result<(), ShellError> {
    Ok(())
}

fn index_io_error(action: &str, path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("cannot {action} completion index source {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Check that the path exists and is readable by the current user")
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not serialize completion index data")
        .with_context(error.to_string())
}

fn print_json(value: &impl Serialize) -> Result<(), ShellError> {
    let json = serde_json::to_string_pretty(value).map_err(json_error)?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use quirl_catalog::Provenance;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier,
    };

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct TestRefreshObserver {
        started: AtomicUsize,
        published: AtomicUsize,
        unchanged: AtomicUsize,
        failed: AtomicUsize,
    }

    impl CatalogRefreshObserver for TestRefreshObserver {
        fn refresh_started(&self) {
            self.started.fetch_add(1, Ordering::Relaxed);
        }

        fn refresh_published(&self) {
            self.published.fetch_add(1, Ordering::Release);
        }

        fn refresh_unchanged(&self) {
            self.unchanged.fetch_add(1, Ordering::Release);
        }

        fn refresh_failed(&self, _error: &ShellError) {
            self.failed.fetch_add(1, Ordering::Release);
        }
    }

    #[derive(Debug, Parser)]
    struct IndexCli {
        #[command(subcommand)]
        command: IndexCommand,
    }

    fn temporary_directory() -> PathBuf {
        let path = env::temp_dir().join(format!(
            "quirl-index-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn test_budget() -> IndexBuildBudget {
        IndexBuildBudget::new(IndexBounds::PRODUCTION)
    }

    fn discovery_config(directory: &Path) -> DiscoveryConfig {
        let binaries = directory.join("bin");
        let fish = directory.join("fish");
        fs::create_dir_all(&binaries).unwrap();
        fs::create_dir_all(&fish).unwrap();
        DiscoveryConfig {
            index_path: directory.join("cache/catalog.json"),
            path_roots: vec![binaries],
            fish_roots: vec![fish],
            bash_roots: Vec::new(),
            zsh_roots: Vec::new(),
            help_roots: Vec::new(),
            man_roots: Vec::new(),
            stale_after: Duration::from_secs(60),
        }
    }

    fn write_executable(path: &Path) {
        fs::write(path, b"not executed").unwrap();
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn bsd_cp_man_page() -> &'static str {
        ".Dd March 28, 2024\n.Dt CP 1\n.Os\n.Sh NAME\n.Nm cp\n.Nd copy files\n.Sh DESCRIPTION\n.Bl -tag -width flag\n.It Fl R\nIf the source_file designates a directory,\n.Nm\ncopies the directory and the entire subtree.\n.It Fl a\nArchive mode. Preserves structure and attributes of files.\n.It Fl p\nCause\n.Nm\nto preserve modification time, access time, file flags, file mode, user ID, and group ID, as allowed by permissions.\n.El\n"
    }

    fn wait_for_observation(counter: &AtomicUsize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while counter.load(Ordering::Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "background catalog observation exceeded its test deadline"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn refresh(config: &DiscoveryConfig) -> Result<bool, ShellError> {
        refresh_catalog_cache(
            config,
            RefreshDeadline::starting_now(Duration::from_secs(5)),
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn file_imports_merge_fish_and_bash_into_one_catalog() {
        let directory = temporary_directory();
        let fish = directory.join("demo.fish");
        let bash = directory.join("demo.bash");
        fs::write(&fish, "complete -c demo -l fish-option").unwrap();
        fs::write(&bash, "complete -W '--bash-option' demo").unwrap();
        let (catalog, diagnostics) = catalog_from_files(
            std::slice::from_ref(&fish),
            std::slice::from_ref(&bash),
            &[],
            &[],
            &[],
            &mut test_budget(),
        )
        .unwrap();
        assert!(diagnostics.is_empty());
        let explanation = catalog.explain("demo").unwrap();
        assert!(explanation
            .facts
            .iter()
            .any(|fact| fact.provenance.source == Provenance::Fish));
        assert!(explanation
            .facts
            .iter()
            .any(|fact| fact.provenance.source == Provenance::Bash));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn zsh_help_and_man_files_merge_with_attributed_facts() {
        let directory = temporary_directory();
        let zsh = directory.join("_ship");
        let help = directory.join("serve.help");
        let man = directory.join("inspect.man");
        fs::write(&zsh, "#compdef ship\n_arguments '--port=[Port]:port:'\n").unwrap();
        fs::write(&help, "Usage: serve [OPTIONS]\n  --listen ADDR  Address\n").unwrap();
        fs::write(&man, ".SH SYNOPSIS\ninspect [OPTIONS]\n.B \\--json\n").unwrap();
        let (catalog, diagnostics) = catalog_from_files(
            &[],
            &[],
            std::slice::from_ref(&zsh),
            std::slice::from_ref(&help),
            std::slice::from_ref(&man),
            &mut test_budget(),
        )
        .unwrap();
        assert!(diagnostics.is_empty());
        for (command, provenance) in [
            ("ship", Provenance::Zsh),
            ("serve", Provenance::Help),
            ("inspect", Provenance::Man),
        ] {
            assert!(catalog
                .explain(command)
                .unwrap()
                .facts
                .iter()
                .any(|fact| fact.provenance.source == provenance));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn build_accepts_repeatable_help_man_and_zsh_inputs() {
        let cli = IndexCli::try_parse_from([
            "index", "build", "--zsh", "_one", "--zsh", "_two", "--help", "one.help", "--help",
            "two.help", "--man", "one.man",
        ])
        .unwrap();
        let IndexCommand::Build {
            zsh,
            help_sources,
            man,
            ..
        } = cli.command
        else {
            panic!("expected build command");
        };
        assert_eq!(zsh, [PathBuf::from("_one"), PathBuf::from("_two")]);
        assert_eq!(
            help_sources,
            [PathBuf::from("one.help"), PathBuf::from("two.help")]
        );
        assert_eq!(man, [PathBuf::from("one.man")]);
    }

    #[test]
    fn atomic_index_round_trips_and_checks_the_schema() {
        let directory = temporary_directory();
        let path = directory.join("catalog.sqlite3");
        let catalog = Catalog::builtin();
        write_catalog_atomically(&path, &catalog, None).unwrap();
        let source = fs::read(&path).unwrap();
        assert_eq!(decode_catalog(&source, &path).unwrap(), catalog);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_run_discovery_deadline_publishes_searchable_builtin_database() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);

        assert!(initialize_interactive_catalog_with_deadline(&config, Instant::now()).unwrap());

        let bytes = read_index(&config.index_path).unwrap();
        assert!(bytes.starts_with(b"SQLite format 3\0"));
        let stats = intelligence::database_stats(&bytes, &config.index_path).unwrap();
        assert!(stats.commands > 0);
        assert!(stats.documents > 0);
        let results =
            intelligence::search(&bytes, &config.index_path, "change directory", 8, None).unwrap();
        assert!(results.iter().any(|result| result.command == "cd"));

        write_executable(&config.path_roots[0].join("after-fallback"));
        assert!(refresh(&config).unwrap());
        assert!(load_catalog_at(&config.index_path)
            .find("after-fallback")
            .is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn background_discovery_replaces_timed_out_fallback_without_input() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        assert!(initialize_interactive_catalog_with_deadline(&config, Instant::now()).unwrap());
        write_executable(&config.path_roots[0].join("idle-background-tool"));
        let observed = Arc::new(TestRefreshObserver::default());
        let observer: Arc<dyn CatalogRefreshObserver> = observed.clone();

        let refresh = start_catalog_refresh_with_config(
            config.clone(),
            observer,
            Duration::from_secs(60),
            Duration::from_secs(5),
            false,
        )
        .unwrap();
        wait_for_observation(&observed.published);

        assert!(load_catalog_at(&config.index_path)
            .find("idle-background-tool")
            .is_some());
        assert_eq!(observed.started.load(Ordering::Acquire), 1);
        drop(refresh);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn background_discovery_failure_is_bounded_and_cancellable() {
        let directory = temporary_directory();
        let mut config = discovery_config(&directory);
        config.path_roots = vec![directory.clone(); INDEX_ROOTS_MAX + 1];
        let observed = Arc::new(TestRefreshObserver::default());
        let observer: Arc<dyn CatalogRefreshObserver> = observed.clone();
        let refresh = start_catalog_refresh_with_config(
            config,
            observer,
            Duration::from_secs(60),
            Duration::from_millis(1),
            false,
        )
        .unwrap();

        wait_for_observation(&observed.failed);
        let started = Instant::now();
        drop(refresh);
        assert!(started.elapsed() < Duration::from_secs(1));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn database_change_during_embedding_rejects_old_bytes_and_indexes_latest() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        write_catalog_atomically(&config.index_path, &Catalog::builtin(), None).unwrap();
        let old_source = read_index(&config.index_path).unwrap();
        let old_embeddings =
            intelligence::mark_embeddings_current_for_test(&old_source, &config.index_path)
                .unwrap();
        write_executable(&config.path_roots[0].join("newest-generation-tool"));
        assert!(refresh(&config).unwrap());
        let cancelled = AtomicBool::new(false);
        let requested = AtomicU64::new(1);

        assert!(!publish_embeddings_if_current(
            &config.index_path,
            &old_source,
            &old_embeddings,
            &cancelled,
            &requested,
            1,
        )
        .unwrap());

        let latest_source = read_index(&config.index_path).unwrap();
        let latest_embeddings =
            intelligence::mark_embeddings_current_for_test(&latest_source, &config.index_path)
                .unwrap();
        assert!(publish_embeddings_if_current(
            &config.index_path,
            &latest_source,
            &latest_embeddings,
            &cancelled,
            &requested,
            1,
        )
        .unwrap());
        let published = read_index(&config.index_path).unwrap();
        assert!(decode_catalog(&published, &config.index_path)
            .unwrap()
            .find("newest-generation-tool")
            .is_some());
        assert!(intelligence::embeddings_are_current(&published, &config.index_path).unwrap());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_run_discovery_limit_publishes_builtin_database() {
        let directory = temporary_directory();
        let mut config = discovery_config(&directory);
        config.path_roots = vec![directory.clone(); INDEX_ROOTS_MAX + 1];

        assert!(initialize_interactive_catalog_with_deadline(
            &config,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap());

        let bytes = read_index(&config.index_path).unwrap();
        let (catalog, state) = intelligence::decode_database(&bytes, &config.index_path).unwrap();
        assert!(catalog.find("quirl run").is_some());
        assert_eq!(state, None);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn discovery_failure_preserves_valid_prior_database_bytes() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        write_catalog_atomically(&config.index_path, &Catalog::builtin(), None).unwrap();
        let prior = read_index(&config.index_path).unwrap();

        assert!(!initialize_interactive_catalog_with_deadline(&config, Instant::now()).unwrap());

        assert_eq!(read_index(&config.index_path).unwrap(), prior);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_discovery_creates_durable_catalog_and_structured_state() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        write_executable(&config.path_roots[0].join("demo"));

        assert!(refresh(&config).unwrap());

        let catalog = load_catalog_at(&config.index_path);
        let command = catalog.find("demo").unwrap();
        assert_eq!(command.provenance.source, Provenance::External);
        assert!(command.provenance.fingerprint.is_some());
        let bytes = read_index(&config.index_path).unwrap();
        let (_, state_json) = intelligence::decode_database(&bytes, &config.index_path).unwrap();
        let state: DiscoveryState = serde_json::from_str(&state_json.unwrap()).unwrap();
        assert_eq!(state.version, DISCOVERY_STATE_VERSION);
        assert!(!state.sources.is_empty());
        assert!(state.source_fingerprint.starts_with("fnv1a64:"));
        assert!(state.catalog_fingerprint.starts_with("fnv1a64:"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn warm_discovery_reuses_matching_catalog_without_writing() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        write_executable(&config.path_roots[0].join("warm"));
        assert!(refresh(&config).unwrap());
        let before = fs::read(&config.index_path).unwrap();

        assert!(!refresh(&config).unwrap());

        assert_eq!(fs::read(&config.index_path).unwrap(), before);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stale_discovery_refreshes_even_when_sources_are_unchanged() {
        let directory = temporary_directory();
        let mut config = discovery_config(&directory);
        write_executable(&config.path_roots[0].join("stale"));
        assert!(refresh(&config).unwrap());
        config.stale_after = Duration::ZERO;

        assert!(refresh(&config).unwrap());

        assert!(load_catalog_at(&config.index_path).find("stale").is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn changed_path_and_declarative_sources_refresh_the_catalog() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        write_executable(&config.path_roots[0].join("first"));
        assert!(refresh(&config).unwrap());
        write_executable(&config.path_roots[0].join("second"));
        fs::write(
            config.fish_roots[0].join("ship.fish"),
            "complete -c ship -l port",
        )
        .unwrap();

        assert!(refresh(&config).unwrap());

        let catalog = load_catalog_at(&config.index_path);
        assert!(catalog.find("second").is_some());
        assert!(catalog.find("ship").is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_catalog_is_rebuilt_from_valid_discovery_state() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        write_executable(&config.path_roots[0].join("recover"));
        assert!(refresh(&config).unwrap());
        fs::write(&config.index_path, b"corrupt").unwrap();

        assert!(refresh(&config).unwrap());

        assert!(load_catalog_at(&config.index_path)
            .find("recover")
            .is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_discovery_writers_publish_only_complete_documents() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        write_executable(&config.path_roots[0].join("parallel"));
        let barrier = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let config = config.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                refresh(&config)
            }));
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();

        assert!(results.iter().any(Result::is_ok));
        assert!(load_catalog_at(&config.index_path)
            .find("parallel")
            .is_some());
        let bytes = read_index(&config.index_path).unwrap();
        let (_, state_json) = intelligence::decode_database(&bytes, &config.index_path).unwrap();
        let state: DiscoveryState = serde_json::from_str(&state_json.unwrap()).unwrap();
        assert_eq!(
            state.catalog_schema_version,
            Catalog::builtin().schema_version
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn path_discovery_limit_returns_resource_limit() {
        let directory = temporary_directory();
        write_executable(&directory.join("one"));
        write_executable(&directory.join("two"));
        let bounds = IndexBounds {
            entries_max: 1,
            ..IndexBounds::PRODUCTION
        };
        let mut budget = IndexBuildBudget::new(bounds);

        let error = discover_path_executables(
            std::slice::from_ref(&directory),
            &mut budget,
            RefreshDeadline::starting_now(Duration::from_secs(5)),
            &AtomicBool::new(false),
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 1"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn path_entry_permission_failure_is_a_skipped_candidate() {
        assert!(path_candidate_error_is_skippable(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(path_candidate_error_is_skippable(&io::Error::from(
            io::ErrorKind::NotFound
        )));
        assert!(!path_candidate_error_is_skippable(&io::Error::from(
            io::ErrorKind::InvalidData
        )));
    }

    #[test]
    fn host_sized_discovery_accepts_path_and_completion_sources_above_old_limit() {
        const EXECUTABLES: usize = 2_250;
        const COMPLETIONS: usize = 150;
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        for index in 0..EXECUTABLES {
            write_executable(&config.path_roots[0].join(format!("host-command-{index:04}")));
        }
        for index in 0..COMPLETIONS {
            fs::write(
                config.fish_roots[0].join(format!("host-command-{index:04}.fish")),
                format!("complete -c host-command-{index:04} -l verbose"),
            )
            .unwrap();
        }
        let mut budget = test_budget();

        let snapshot = discover_sources(
            &config,
            &mut budget,
            RefreshDeadline::starting_now(Duration::from_secs(10)),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(snapshot.executables.len(), EXECUTABLES);
        assert_eq!(snapshot.fish_files.len(), COMPLETIONS);
        assert_eq!(snapshot.sources.len(), EXECUTABLES + COMPLETIONS);
        assert!(snapshot.sources.len() > 2_048);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn automatic_man_discovery_merges_cp_intent_and_option_documents() {
        let directory = temporary_directory();
        let mut config = discovery_config(&directory);
        let man = directory.join("man1");
        fs::create_dir(&man).unwrap();
        config.man_roots = vec![man.clone()];
        write_executable(&config.path_roots[0].join("cp"));
        fs::write(man.join("cp.1"), bsd_cp_man_page()).unwrap();

        assert!(refresh(&config).unwrap());

        let catalog = load_catalog_at(&config.index_path);
        let cp = catalog.find("cp").unwrap();
        assert_eq!(cp.summary, "copy files");
        assert!(cp.options.iter().any(|option| {
            option.names == ["-R"] && option.documentation.contains("entire subtree")
        }));
        assert!(cp.options.iter().any(|option| {
            option.names == ["-p"] && option.documentation.contains("file mode")
        }));
        let bytes = read_index(&config.index_path).unwrap();
        let results = intelligence::search(
            &bytes,
            &config.index_path,
            "copy a directory while preserving permissions",
            8,
            None,
        )
        .unwrap();
        assert!(results.iter().any(|result| result.command == "cp"));
        let option_results = intelligence::search(
            &bytes,
            &config.index_path,
            "preserve file mode permissions",
            8,
            None,
        )
        .unwrap();
        assert!(option_results
            .iter()
            .any(|result| result.command == "cp" && result.target.contains("-p")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn automatic_man_discovery_isolates_bad_pages_and_deduplicates_aliases() {
        use std::os::unix::fs::symlink;

        let directory = temporary_directory();
        let mut config = discovery_config(&directory);
        let man = directory.join("man1");
        fs::create_dir(&man).unwrap();
        config.man_roots = vec![man.clone()];
        for command in ["bad", "copy", "cp", "dangling", "huge", "invalid", "locked"] {
            write_executable(&config.path_roots[0].join(command));
        }
        fs::write(man.join("cp.1"), bsd_cp_man_page()).unwrap();
        symlink(man.join("cp.1"), man.join("copy.1")).unwrap();
        symlink(man.join("missing.1"), man.join("dangling.1")).unwrap();
        fs::write(man.join("bad.1.gz"), b"compressed").unwrap();
        fs::write(man.join("huge.1"), vec![b'x'; DOCUMENTATION_READ_LIMIT + 1]).unwrap();
        fs::write(man.join("invalid.1"), [0xff, 0xfe]).unwrap();
        let locked = man.join("locked.1");
        fs::write(&locked, bsd_cp_man_page()).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let mut budget = test_budget();

        let snapshot = discover_sources(
            &config,
            &mut budget,
            RefreshDeadline::starting_now(Duration::from_secs(5)),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(
            snapshot.man_files,
            [fs::canonicalize(man.join("cp.1")).unwrap()]
        );
        let messages = snapshot
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        assert!(messages
            .iter()
            .any(|message| message.contains("compressed")));
        assert!(messages.iter().any(|message| message.contains("oversized")));
        assert!(messages
            .iter()
            .any(|message| message.contains("unresolved")));
        assert!(messages.iter().any(|message| message.contains("UTF-8")));
        assert!(messages
            .iter()
            .any(|message| message.contains("during fingerprinting")));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn man_page_names_preserve_configured_text_formats() {
        assert_eq!(
            man_page_command(Path::new("demo.man.txt")),
            Some(("demo".to_owned(), false))
        );
        assert_eq!(
            man_page_command(Path::new("demo.txt")),
            Some(("demo".to_owned(), false))
        );
    }

    #[test]
    fn refresh_deadline_error_reports_the_active_background_limit() {
        let limit = Duration::from_secs(30);
        let error = ensure_refresh_active(
            RefreshDeadline {
                expires_at: Instant::now(),
                limit,
            },
            &AtomicBool::new(false),
            "while scanning PATH",
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 30000 ms"));
        assert!(error.details.context[0].contains("stage: while scanning PATH"));
        assert!(!error.details.context[0].contains("limit: 750 ms"));
    }

    #[test]
    fn declarative_discovery_never_executes_startup_source() {
        let directory = temporary_directory();
        let config = discovery_config(&directory);
        let marker = directory.join("startup-executed");
        fs::write(
            config.fish_roots[0].join("unsafe.fish"),
            format!(
                "touch {}\ncomplete -c safe-command -l value",
                marker.display()
            ),
        )
        .unwrap();

        refresh(&config).unwrap();

        assert!(!marker.exists());
        assert!(load_catalog_at(&config.index_path)
            .find("safe-command")
            .is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refresh_failure_leaves_terminal_catalog_fallback_and_no_worker() {
        use std::os::unix::fs::symlink;

        let directory = temporary_directory();
        let mut config = discovery_config(&directory);
        let foreign = directory.join("foreign");
        fs::create_dir(&foreign).unwrap();
        let linked = directory.join("linked-cache");
        symlink(&foreign, &linked).unwrap();
        config.index_path = linked.join("catalog.json");

        assert!(refresh(&config).is_err());
        assert!(load_catalog_at(&config.index_path)
            .find("quirl run")
            .is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn refresh_guard_cancels_and_joins_worker_on_shutdown() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let changed = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(()), Condvar::new()));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_wake = Arc::clone(&wake);
        let worker_finished = Arc::clone(&finished);
        let worker = thread::spawn(move || {
            let guard = worker_wake.0.lock().unwrap();
            let _guard = worker_wake
                .1
                .wait_while(guard, |_| !worker_cancelled.load(Ordering::Acquire))
                .unwrap();
            worker_finished.store(true, Ordering::Release);
        });

        drop(CatalogRefresh {
            cancelled,
            changed,
            requested_generation: Arc::new(AtomicU64::new(1)),
            wake,
            worker: Some(worker),
        });

        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn index_budget_accepts_exact_limits_and_rejects_limit_plus_one() {
        let bounds = IndexBounds {
            roots_max: 2,
            entries_max: 2,
            files_max: 2,
            path_bytes_max: 8,
            source_bytes_max: 4,
            records_max: 2,
            retained_bytes_max: 128,
            diagnostics_max: 2,
        };
        let mut budget = IndexBuildBudget::new(bounds);
        budget.roots = 2;
        ensure_index_limit("roots", bounds.roots_max, budget.roots).unwrap();
        admit_index_path(Path::new("one"), &mut budget).unwrap();
        admit_index_path(Path::new("two"), &mut budget).unwrap();
        admit_source_bytes(4, &mut budget).unwrap();

        assert_eq!(
            admit_index_path(Path::new("x"), &mut budget)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            admit_source_bytes(1, &mut budget).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            ensure_index_limit("roots", bounds.roots_max, 3)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn failed_index_install_preserves_collision_and_temporary() {
        let directory = temporary_directory();
        let path = directory.join("catalog.json");
        fs::write(&path, b"foreign").unwrap();

        let error = install_new_index(&path, b"new", &directory).unwrap_err();

        assert_eq!(error.code, ErrorCode::Io);
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_cleanup_preserves_a_concurrent_temporary_replacement() {
        let directory = temporary_directory();
        let path = directory.join("catalog.json");
        let moved = directory.join("moved-owned-temporary");

        let error = install_new_index_with_hook(&path, b"new", &directory, |stage| {
            if stage == IndexWriteStage::ContentSynced {
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
            .any(|context| context.contains("failure cleanup preserved index temporary")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_cleanup_preserves_colliding_temporary_and_destination_entries() {
        let directory = temporary_directory();
        let path = directory.join("catalog.json");
        let moved_temporary = directory.join("moved-owned-temporary");
        let moved_destination = directory.join("moved-owned-destination");

        let error = install_new_index_with_hook(&path, b"new", &directory, |stage| {
            if stage == IndexWriteStage::Installed {
                let temporary = fs::read_dir(&directory)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .find(|entry| entry.as_ref().is_ok_and(|entry| entry != &path))
                    .ok_or_else(|| io::Error::other("temporary was not visible"))??;
                fs::rename(&temporary, &moved_temporary)?;
                fs::rename(&path, &moved_destination)?;
                fs::write(&temporary, b"foreign")?;
                fs::hard_link(&temporary, &path)?;
                return Err(io::Error::other("injected installed replacement"));
            }
            Ok(())
        })
        .unwrap_err();

        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        let replacement_temporary = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry != &path && entry != &moved_temporary && entry != &moved_destination
            })
            .unwrap();
        assert_eq!(fs::read(replacement_temporary).unwrap(), b"foreign");
        assert!(moved_temporary.exists());
        assert!(moved_destination.exists());
        assert!(
            error
                .details
                .context
                .iter()
                .filter(|context| context.contains("failure cleanup preserved"))
                .count()
                >= 2
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_reader_rejects_symlinks_hardlinks_and_special_files() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        use std::os::unix::fs::symlink;

        let directory = temporary_directory();
        let source = directory.join("source");
        fs::write(&source, b"{}").unwrap();
        let link = directory.join("link");
        symlink(&source, &link).unwrap();
        assert_eq!(read_index(&link).unwrap_err().code, ErrorCode::Validation);

        let alias = directory.join("alias");
        fs::hard_link(&source, &alias).unwrap();
        assert_eq!(read_index(&source).unwrap_err().code, ErrorCode::Validation);

        let socket = directory.join("socket");
        mkfifo(&socket, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert_eq!(read_index(&socket).unwrap_err().code, ErrorCode::Validation);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_cleanup_failure_retains_the_originating_error() {
        let directory = temporary_directory();
        let path = directory.join("temporary");
        let file = File::create(&path).unwrap();
        let mut guard = IndexTemporary::new(path.clone(), &file).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let error = guard.cleanup(
            ShellError::new(ErrorCode::Io, "originating index failure")
                .with_context("injected primary failure"),
        );

        assert_eq!(error.message, "originating index failure");
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("injected primary failure")));
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("failure cleanup preserved index temporary")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_or_incompatible_default_cache_recovers_to_current_builtins() {
        let missing = load_catalog_at(Path::new("/definitely/missing/quirl-index.json"));
        assert!(missing.find("quirl run").is_some());

        let corrupt = load_catalog_at(Path::new("/dev/null"));
        assert!(corrupt.find("quirl run").is_some());

        let directory = temporary_directory();
        let path = directory.join("old-schema.json");
        let mut incompatible = Catalog::builtin();
        incompatible.schema_version += 1;
        fs::write(&path, serde_json::to_string(&incompatible).unwrap()).unwrap();
        let recovered = load_catalog_at(&path);
        assert_eq!(recovered.schema_version, Catalog::builtin().schema_version);
        assert!(recovered.find("quirl agent manifest").is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn oversized_default_cache_falls_back_without_parsing_past_the_bound() {
        let directory = temporary_directory();
        let path = directory.join("oversized-catalog.json");
        fs::write(&path, vec![b' '; INDEX_READ_LIMIT + 1]).unwrap();

        let recovered = load_catalog_at(&path);

        assert_eq!(recovered.schema_version, Catalog::builtin().schema_version);
        assert!(recovered.find("quirl agent manifest").is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn catalog_serialization_accepts_exact_limit_and_rejects_plus_one() {
        let mut exact = BoundedBytesWriter::new(INDEX_READ_LIMIT);
        exact.write_all(&vec![0_u8; INDEX_READ_LIMIT]).unwrap();
        assert_eq!(exact.bytes.len(), INDEX_READ_LIMIT);
        assert!(!exact.exceeded);

        assert_eq!(
            exact.write(&[0_u8]).unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert!(exact.exceeded);
        assert_eq!(exact.bytes.len(), INDEX_READ_LIMIT);
    }

    #[test]
    fn compatible_stale_cache_cannot_remove_or_overwrite_current_builtins() {
        let mut stale = Catalog::builtin();
        stale.commands.retain(|command| command.path != "quirl lsp");
        let stale_run = stale
            .commands
            .iter_mut()
            .find(|command| command.path == "quirl run")
            .unwrap();
        stale_run.summary = "stale cached summary".to_owned();
        stale_run.options.push(quirl_catalog::ArgumentSpec {
            names: vec!["--removed-stale-flag".to_owned()],
            ..stale_run.options[0].clone()
        });

        let merged = merge_cached_catalog(stale);
        assert!(merged.find("quirl lsp").is_some());
        assert_ne!(
            merged.find("quirl run").unwrap().summary,
            "stale cached summary"
        );
        assert!(merged
            .find("quirl run")
            .unwrap()
            .options
            .iter()
            .all(|argument| !argument
                .names
                .iter()
                .any(|name| name == "--removed-stale-flag")));
    }

    #[test]
    fn cached_catalog_cannot_forge_installed_plugin_authority() {
        let mut cached = Catalog::builtin();
        let mut forged = cached.find("quirl run").unwrap().clone();
        forged.path = "forged plugin command".to_owned();
        forged.id = "plugin:forged:command".to_owned();
        forged.version = Some("9.9.9".to_owned());
        forged.provenance.source = Provenance::Plugin;
        cached.commands.push(forged);

        let merged = merge_cached_catalog(cached);

        assert!(merged.find("forged plugin command").is_none());
        assert!(merged.find("quirl run").is_some());
    }

    #[test]
    fn legacy_v3_cache_is_migrated_then_merged_with_current_builtins() {
        let directory = temporary_directory();
        let path = directory.join("catalog-v3.json");
        let source = serde_json::json!({
            "schema_version": 3,
            "commands": [{
                "path": "demo",
                "signature": "demo [--output FILE]",
                "summary": "Imported demo",
                "details": "Imported declarative completion metadata.",
                "options": [{
                    "names": ["--output"],
                    "value": "FILE",
                    "summary": "Write output",
                    "provenance": {
                        "source": "fish",
                        "confidence": "high",
                        "trust": "declared",
                        "origin": "demo.fish",
                        "fingerprint": "sha256:demo"
                    }
                }],
                "examples": [],
                "effects": ["spawn_process"],
                "provenance": {
                    "source": "fish",
                    "confidence": "high",
                    "trust": "declared",
                    "origin": "demo.fish",
                    "fingerprint": "sha256:demo"
                }
            }]
        });
        fs::write(&path, source.to_string()).unwrap();

        let catalog = load_catalog_at(&path);
        let imported = catalog.find("demo").unwrap();
        assert_eq!(catalog.schema_version, Catalog::builtin().schema_version);
        assert_eq!(imported.options[0].value_type, "FILE");
        assert_eq!(
            imported.provenance.confidence,
            quirl_catalog::Confidence::High
        );
        assert!(catalog.find("quirl lsp").is_some());
        fs::remove_dir_all(directory).unwrap();
    }
}
