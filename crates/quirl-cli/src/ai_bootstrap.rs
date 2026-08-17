//! Bounded background installation and indexing for local command intelligence.

use crate::{index, intelligence};
use quirl_core::{escape_terminal_line, ErrorCode, ShellError};
use quirl_ui::{
    InteractiveActivityProvider, InteractiveActivitySnapshot, ACTIVITY_MESSAGE_BYTES_MAX,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex, Weak,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[cfg(unix)]
use nix::{
    fcntl::{open, OFlag},
    sys::stat::Mode as FileMode,
};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

const MODEL_REVISION: &str = "bf8b056651a2c21b8d2565580b8569da283cab23";
const MODEL_BASE_URL: &str = "https://huggingface.co/minishlab/potion-base-8M/resolve";
const HTTP_REDIRECTS_MAX: u32 = 4;
const HTTP_GLOBAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_BODY_TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_BUFFER_BYTES: usize = 64 * 1024;
const DOWNLOAD_CHANNEL_CHUNKS_MAX: usize = 2;
const DOWNLOAD_CHANNEL_POLL: Duration = Duration::from_millis(25);
const TEMPORARY_ATTEMPTS_MAX: u64 = 64;
const ACTIVITY_ERROR_MESSAGE_BYTES_MAX: usize = 256;
const ACTIVITY_ERROR_CONTEXT_BYTES_MAX: usize = 192;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
struct AssetSpec {
    name: &'static str,
    bytes: u64,
    sha256: &'static str,
}

const ASSETS: [AssetSpec; 3] = [
    AssetSpec {
        name: "config.json",
        bytes: 202,
        sha256: "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553",
    },
    AssetSpec {
        name: "tokenizer.json",
        bytes: 683_666,
        sha256: "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
    },
    AssetSpec {
        name: "model.safetensors",
        bytes: 30_236_760,
        sha256: "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
    },
];

/// Validate exact pinned file sizes and hashes before Model2Vec consumes them.
pub(crate) fn validate_pinned_model(path: &Path) -> Result<(), ShellError> {
    validate_model_assets(path, &ASSETS)
}

fn validate_model_assets(path: &Path, assets: &[AssetSpec]) -> Result<(), ShellError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| model_io_error("inspect", path, error))?;
    if !metadata.file_type().is_dir() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "local AI model root {} is not a real directory",
                path.display()
            ),
        )
        .with_help("Replace links and special files with a private model directory"));
    }
    index::create_index_directories(path)?;
    for &asset in assets {
        validate_asset(&path.join(asset.name), asset)?;
    }
    Ok(())
}

/// Session-owned controller for the single background AI worker.
pub(crate) struct InteractiveAiBootstrap {
    shared: Arc<Shared>,
}

impl InteractiveAiBootstrap {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared::new()),
        }
    }

    pub(crate) fn activity_provider(&self) -> ActivityAdapter {
        ActivityAdapter {
            shared: Arc::clone(&self.shared),
            observed_generation: 0,
        }
    }

    pub(crate) fn request_reindex(&self) {
        if automatic_ai_disabled() {
            return;
        }
        self.shared.start_worker();
        if let Err(error) = self.shared.request() {
            self.shared
                .publish(format!("AI unavailable: {}", error.message));
        }
    }

    pub(crate) fn catalog_admitted(&self) {
        if let Err(error) = self.shared.admit_catalog() {
            self.shared
                .publish(format!("AI unavailable: {}", error.message));
        }
    }

    pub(crate) fn take_catalog_changed(&self) -> bool {
        self.shared
            .catalog_refresh
            .lock()
            .ok()
            .and_then(|refresh| refresh.as_ref().map(index::CatalogRefresh::take_changed))
            .unwrap_or(false)
    }

    pub(crate) fn request_catalog_refresh(&self) {
        let result = self
            .shared
            .catalog_refresh
            .lock()
            .ok()
            .and_then(|refresh| refresh.as_ref().map(index::CatalogRefresh::request_refresh));
        if let Some(Err(error)) = result {
            self.shared
                .publish(format!("AI discovery deferred: {}", error.message));
        }
    }
}

impl Drop for InteractiveAiBootstrap {
    fn drop(&mut self) {
        self.shared.cancel();
        if let Ok(slot) = self.shared.worker.lock() {
            let mut slot = slot;
            if let Some(worker) = slot.take() {
                let _ = worker.join();
            }
        }
    }
}

pub(crate) struct ActivityAdapter {
    shared: Arc<Shared>,
    observed_generation: u64,
}

impl InteractiveActivityProvider for ActivityAdapter {
    fn catalog_admitted(&mut self) -> Result<(), ShellError> {
        self.shared.admit_catalog()
    }

    fn poll_cached(&mut self) -> Result<Option<InteractiveActivitySnapshot>, ShellError> {
        let snapshot = self.shared.snapshot()?;
        if snapshot.generation <= self.observed_generation {
            return Ok(None);
        }
        self.observed_generation = snapshot.generation;
        Ok(Some(snapshot))
    }
}

struct Shared {
    admitted: AtomicBool,
    started: AtomicBool,
    cancelled: Arc<AtomicBool>,
    requested_generation: AtomicU64,
    activity_generation: AtomicU64,
    activity: Mutex<ActivityState>,
    wake: (Mutex<()>, Condvar),
    worker: Mutex<Option<JoinHandle<()>>>,
    catalog_refresh: Mutex<Option<index::CatalogRefresh>>,
}

#[derive(Default)]
struct ActivityState {
    intelligence: Option<String>,
    discovery: Option<String>,
}

impl Shared {
    fn new() -> Self {
        Self {
            admitted: AtomicBool::new(false),
            started: AtomicBool::new(false),
            cancelled: Arc::new(AtomicBool::new(false)),
            requested_generation: AtomicU64::new(0),
            activity_generation: AtomicU64::new(0),
            activity: Mutex::new(ActivityState::default()),
            wake: (Mutex::new(()), Condvar::new()),
            worker: Mutex::new(None),
            catalog_refresh: Mutex::new(None),
        }
    }

    fn start_worker(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let shared = Arc::clone(self);
        match thread::Builder::new()
            .name("quirl-ai-bootstrap".to_owned())
            .spawn(move || worker_loop(&shared))
        {
            Ok(worker) => {
                if let Ok(mut slot) = self.worker.lock() {
                    *slot = Some(worker);
                }
            }
            Err(error) => self.publish(format!("AI unavailable: {error}")),
        }
    }

    fn admit_catalog(self: &Arc<Self>) -> Result<(), ShellError> {
        if self.admitted.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if !automatic_catalog_refresh_disabled() {
            self.start_catalog_refresh();
        }
        if automatic_ai_disabled() {
            return Ok(());
        }
        self.publish("AI: preparing local model".to_owned());
        self.start_worker();
        self.request()
    }

    fn start_catalog_refresh(self: &Arc<Self>) {
        let observer: Arc<dyn index::CatalogRefreshObserver> = Arc::new(CatalogObserver {
            shared: Arc::downgrade(self),
        });
        let Some(refresh) = index::start_interactive_catalog_refresh(observer) else {
            self.publish("AI discovery deferred: catalog worker unavailable".to_owned());
            return;
        };
        match self.catalog_refresh.lock() {
            Ok(mut slot) if slot.is_none() => *slot = Some(refresh),
            Ok(_) => {}
            Err(_) => self.publish("AI discovery deferred: refresh lock poisoned".to_owned()),
        }
    }

    fn request(&self) -> Result<(), ShellError> {
        let _guard = self.wake.0.lock().map_err(|_| {
            ShellError::new(ErrorCode::Io, "the AI worker request lock was poisoned")
                .with_help("Restart Quirl to create a fresh AI worker")
        })?;
        increment_counter(&self.requested_generation, "AI request generation")?;
        self.wake.1.notify_one();
        Ok(())
    }

    fn cancel(&self) {
        if let Ok(refresh) = self.catalog_refresh.lock() {
            if let Some(refresh) = refresh.as_ref() {
                refresh.cancel();
            }
        }
        let _guard = match self.wake.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        self.cancelled.store(true, Ordering::Release);
        self.wake.1.notify_all();
    }

    fn publish(&self, message: String) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.intelligence = Some(truncate_utf8(
                &escape_terminal_line(&message),
                ACTIVITY_MESSAGE_BYTES_MAX,
            ));
            let _ = increment_counter(&self.activity_generation, "AI activity generation");
        }
    }

    fn clear(&self) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.intelligence = None;
            let _ = increment_counter(&self.activity_generation, "AI activity generation");
        }
    }

    fn publish_discovery(&self, message: String) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.discovery = Some(truncate_utf8(
                &escape_terminal_line(&message),
                ACTIVITY_MESSAGE_BYTES_MAX,
            ));
            let _ = increment_counter(&self.activity_generation, "AI activity generation");
        }
    }

    fn clear_discovery(&self) {
        if let Ok(mut activity) = self.activity.lock() {
            activity.discovery = None;
            let _ = increment_counter(&self.activity_generation, "AI activity generation");
        }
    }

    fn snapshot(&self) -> Result<InteractiveActivitySnapshot, ShellError> {
        let activity = self.activity.lock().map_err(|_| {
            ShellError::new(ErrorCode::Io, "the AI activity cache lock was poisoned")
                .with_help("Restart Quirl to create a fresh activity cache")
        })?;
        Ok(InteractiveActivitySnapshot {
            generation: self.activity_generation.load(Ordering::Acquire),
            message: activity
                .discovery
                .clone()
                .or_else(|| activity.intelligence.clone()),
        })
    }
}

struct CatalogObserver {
    shared: Weak<Shared>,
}

impl index::CatalogRefreshObserver for CatalogObserver {
    fn refresh_started(&self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.publish_discovery("AI: discovering local commands".to_owned());
            #[cfg(debug_assertions)]
            if std::env::var_os("QUIRL_TEST_AI_BOOTSTRAP_FAKE").is_some() {
                let _ = wait_test_discovery_stage(&shared);
            }
        }
    }

    fn refresh_published(&self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.clear_discovery();
            if !automatic_ai_disabled() {
                if let Err(error) = shared.request() {
                    shared.publish(format!("AI index deferred: {}", error.message));
                }
            }
        }
    }

    fn refresh_unchanged(&self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.clear_discovery();
        }
    }

    fn refresh_failed(&self, error: &ShellError) {
        if let Some(shared) = self.shared.upgrade() {
            let message = truncate_utf8(
                &escape_terminal_line(&error.message),
                ACTIVITY_ERROR_MESSAGE_BYTES_MAX,
            );
            let context = error
                .details
                .context
                .first()
                .map(|context| {
                    format!(
                        " ({})",
                        truncate_utf8(
                            &escape_terminal_line(context),
                            ACTIVITY_ERROR_CONTEXT_BYTES_MAX,
                        )
                    )
                })
                .unwrap_or_default();
            shared.publish_discovery(format!("AI discovery failed: {message}{context}"));
        }
    }
}

#[cfg(debug_assertions)]
fn automatic_ai_disabled() -> bool {
    std::env::var_os("QUIRL_TEST_AI_BOOTSTRAP_DISABLED").is_some()
}

#[cfg(not(debug_assertions))]
fn automatic_ai_disabled() -> bool {
    false
}

#[cfg(debug_assertions)]
fn automatic_catalog_refresh_disabled() -> bool {
    std::env::var_os("QUIRL_TEST_CATALOG_REFRESH_DISABLED").is_some()
}

#[cfg(not(debug_assertions))]
fn automatic_catalog_refresh_disabled() -> bool {
    false
}

fn worker_loop(shared: &Shared) {
    let fetcher = match UreqFetcher::new() {
        Ok(fetcher) => fetcher,
        Err(error) => {
            shared.publish(format!("AI unavailable: {}", error.message));
            return;
        }
    };
    let mut completed_generation = 0_u64;
    loop {
        let requested = {
            let Ok(mut guard) = shared.wake.0.lock() else {
                return;
            };
            loop {
                if shared.cancelled.load(Ordering::Acquire) {
                    return;
                }
                let requested = shared.requested_generation.load(Ordering::Acquire);
                if requested > completed_generation {
                    break requested;
                }
                let Ok(next_guard) = shared.wake.1.wait(guard) else {
                    return;
                };
                guard = next_guard;
            }
        };
        if run_generation(shared, &fetcher, requested) {
            completed_generation = requested;
        }
    }
}

fn run_generation(shared: &Shared, fetcher: &UreqFetcher, generation: u64) -> bool {
    #[cfg(debug_assertions)]
    if std::env::var_os("QUIRL_TEST_AI_BOOTSTRAP_FAKE").is_some() {
        shared.publish("AI: downloading potion-base-8M (50%)".to_owned());
        if wait_test_stage(shared) {
            shared.publish("AI: indexing local commands".to_owned());
            if wait_test_stage(shared) {
                let _ = index::mark_default_embeddings_current_for_test();
                shared.clear();
            }
        }
        return true;
    }
    let Some(model_path) = intelligence::default_model_path() else {
        shared.publish("AI unavailable: local model path is not configured".to_owned());
        return true;
    };
    if validate_pinned_model(&model_path).is_err() {
        shared.publish("AI: downloading potion-base-8M (0%)".to_owned());
        if let Err(error) = install_model(fetcher, &model_path, shared) {
            shared.publish(format!("AI download failed: {}", error.message));
            return true;
        }
    }
    if shared.cancelled.load(Ordering::Acquire) {
        return true;
    }
    if index::default_embeddings_are_current().unwrap_or(false) {
        shared.clear();
        return true;
    }
    shared.publish("AI: indexing local commands".to_owned());
    match index::build_default_embeddings_if_current(
        &shared.cancelled,
        &shared.requested_generation,
        generation,
    ) {
        Ok(Some(report)) => {
            shared.publish(format!(
                "AI ready: {} commands and options",
                report.documents
            ));
            shared.clear();
            true
        }
        Ok(None) => generation_was_superseded_or_cancelled(shared, generation),
        Err(_) if generation_was_superseded_or_cancelled(shared, generation) => true,
        Err(error) => {
            shared.publish(format!("AI index failed: {}", error.message));
            true
        }
    }
}

fn generation_was_superseded_or_cancelled(shared: &Shared, generation: u64) -> bool {
    shared.cancelled.load(Ordering::Acquire)
        || shared.requested_generation.load(Ordering::Acquire) != generation
}

#[cfg(debug_assertions)]
fn wait_test_stage(shared: &Shared) -> bool {
    for _ in 0..20 {
        if shared.cancelled.load(Ordering::Acquire) {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

#[cfg(debug_assertions)]
fn wait_test_discovery_stage(shared: &Shared) -> bool {
    for _ in 0..10 {
        if shared.cancelled.load(Ordering::Acquire) {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
    true
}

trait Fetcher {
    fn open(
        &self,
        url: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Box<dyn Read + Send>, ShellError>;
}

struct UreqFetcher {
    agent: ureq::Agent,
}

impl UreqFetcher {
    fn new() -> Result<Self, ShellError> {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(HTTP_REDIRECTS_MAX)
            .max_redirects_will_error(true)
            .timeout_global(Some(HTTP_GLOBAL_TIMEOUT))
            .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
            .timeout_recv_body(Some(HTTP_BODY_TIMEOUT))
            .build();
        Ok(Self {
            agent: config.into(),
        })
    }
}

impl Fetcher for UreqFetcher {
    fn open(
        &self,
        url: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Box<dyn Read + Send>, ShellError> {
        let (sender, receiver) = mpsc::sync_channel(DOWNLOAD_CHANNEL_CHUNKS_MAX);
        let agent = self.agent.clone();
        let url = url.to_owned();
        thread::Builder::new()
            .name("quirl-ai-https".to_owned())
            .spawn(move || stream_https(agent, &url, &sender))
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not start the bounded HTTPS reader")
                    .with_context(error.to_string())
                    .with_help("Retry automatic model setup in a running Quirl session")
            })?;
        Ok(Box::new(ChannelReader::new(receiver, cancelled)))
    }
}

fn install_model(
    fetcher: &dyn Fetcher,
    destination: &Path,
    shared: &Shared,
) -> Result<(), ShellError> {
    install_model_with_assets(
        fetcher,
        destination,
        shared,
        &ASSETS,
        MODEL_BASE_URL,
        MODEL_REVISION,
        std::env::var_os("QUIRL_MODEL_PATH").is_none(),
    )
}

fn install_model_with_assets(
    fetcher: &dyn Fetcher,
    destination: &Path,
    shared: &Shared,
    assets: &[AssetSpec],
    base_url: &str,
    revision: &str,
    recover_corrupt: bool,
) -> Result<(), ShellError> {
    if path_entry_exists(destination)? {
        match validate_model_assets(destination, assets) {
            Ok(()) => return Ok(()),
            Err(error) if !recover_corrupt => return Err(error),
            Err(_) => {}
        }
    }
    let parent = destination.parent().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "the local model path has no parent directory",
        )
        .with_help("Set QUIRL_MODEL_PATH to a nested directory")
    })?;
    create_private_directories(parent)?;
    if path_entry_exists(destination)? {
        quarantine_corrupt_model(destination, parent)?;
    }
    let mut temporary = ModelTemporary::create(parent, assets)?;
    let total_bytes = assets.iter().map(|asset| asset.bytes).sum::<u64>();
    let mut completed_bytes = 0_u64;
    for &asset in assets {
        let url = format!("{base_url}/{revision}/{}", asset.name);
        let mut reader = fetcher.open(&url, Arc::clone(&shared.cancelled))?;
        let path = temporary.path.join(asset.name);
        download_asset(reader.as_mut(), &path, asset, &shared.cancelled, |bytes| {
            let observed = completed_bytes.saturating_add(bytes);
            let percent = observed.saturating_mul(100) / total_bytes.max(1);
            shared.publish(format!("AI: downloading potion-base-8M ({percent}%)"));
        })?;
        completed_bytes = completed_bytes.saturating_add(asset.bytes);
    }
    validate_model_assets(&temporary.path, assets)?;
    fs::rename(&temporary.path, destination)
        .map_err(|error| model_io_error("install", destination, error))?;
    temporary.installed = true;
    index::sync_index_directory(parent)?;
    Ok(())
}

fn download_asset(
    reader: &mut dyn Read,
    destination: &Path,
    asset: AssetSpec,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<(), ShellError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(destination)
        .map_err(|error| model_io_error("create", destination, error))?;
    let mut hasher = Sha256::new();
    let mut observed = 0_u64;
    let mut buffer = [0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "local AI download was cancelled",
            )
            .with_help("Restart Quirl to resume automatic setup"));
        }
        let count = reader
            .read(&mut buffer)
            .map_err(|error| model_io_error("download", destination, error))?;
        if count == 0 {
            break;
        }
        observed = observed.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if observed > asset.bytes {
            return Err(asset_integrity_error(asset, observed));
        }
        file.write_all(&buffer[..count])
            .map_err(|error| model_io_error("write", destination, error))?;
        hasher.update(&buffer[..count]);
        progress(observed);
    }
    file.sync_all()
        .map_err(|error| model_io_error("sync", destination, error))?;
    let digest = format!("{:x}", hasher.finalize());
    if observed != asset.bytes || digest != asset.sha256 {
        return Err(asset_integrity_error(asset, observed));
    }
    Ok(())
}

fn validate_asset(path: &Path, asset: AssetSpec) -> Result<(), ShellError> {
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| model_io_error("inspect", path, error))?;
    if !path_metadata.file_type().is_file() || path_metadata.len() != asset.bytes {
        return Err(asset_integrity_error(asset, path_metadata.len()));
    }
    let mut file = open_model_file(path)?;
    let file_metadata = file
        .metadata()
        .map_err(|error| model_io_error("inspect", path, error))?;
    #[cfg(unix)]
    {
        if path_metadata.dev() != file_metadata.dev()
            || path_metadata.ino() != file_metadata.ino()
            || file_metadata.nlink() != 1
        {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("local AI asset {} changed during admission", path.display()),
            )
            .with_help("Use unlinked regular model files in a private directory"));
        }
        if file_metadata.mode() & 0o022 != 0 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "local AI asset {} has unsafe writable permissions",
                    path.display()
                ),
            )
            .with_context(format!("mode: {:#o}", file_metadata.mode() & 0o777))
            .with_help("Remove group/other write access from the model files"));
        }
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; DOWNLOAD_BUFFER_BYTES];
    let mut observed = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| model_io_error("read", path, error))?;
        if count == 0 {
            break;
        }
        observed = observed.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if observed > asset.bytes {
            return Err(asset_integrity_error(asset, observed));
        }
        hasher.update(&buffer[..count]);
    }
    let digest = format!("{:x}", hasher.finalize());
    if observed != asset.bytes || digest != asset.sha256 {
        return Err(asset_integrity_error(asset, observed));
    }
    Ok(())
}

#[cfg(unix)]
fn open_model_file(path: &Path) -> Result<File, ShellError> {
    let descriptor = open(
        path,
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        FileMode::empty(),
    )
    .map_err(|error| model_io_error("open", path, io::Error::from_raw_os_error(error as i32)))?;
    Ok(File::from(descriptor))
}

#[cfg(not(unix))]
fn open_model_file(path: &Path) -> Result<File, ShellError> {
    File::open(path).map_err(|error| model_io_error("open", path, error))
}

struct ModelTemporary {
    path: PathBuf,
    asset_names: Vec<&'static str>,
    installed: bool,
}

impl ModelTemporary {
    fn create(parent: &Path, assets: &[AssetSpec]) -> Result<Self, ShellError> {
        for _ in 0..TEMPORARY_ATTEMPTS_MAX {
            let id = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(
                ".potion-base-8M.download-{}-{id}",
                std::process::id()
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        asset_names: assets.iter().map(|asset| asset.name).collect(),
                        installed: false,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(model_io_error("create", &path, error)),
            }
        }
        Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "could not allocate a bounded model staging directory",
        )
        .with_context(format!("attempt limit: {TEMPORARY_ATTEMPTS_MAX}"))
        .with_help("Remove stale .potion-base-8M.download-* entries and retry"))
    }
}

impl Drop for ModelTemporary {
    fn drop(&mut self) {
        if self.installed {
            return;
        }
        for name in &self.asset_names {
            let _ = fs::remove_file(self.path.join(name));
        }
        let _ = fs::remove_dir(&self.path);
    }
}

fn create_private_directories(path: &Path) -> Result<(), ShellError> {
    index::create_index_directories(path)
}

fn path_entry_exists(path: &Path) -> Result<bool, ShellError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(model_io_error("inspect", path, error)),
    }
}

fn quarantine_corrupt_model(destination: &Path, parent: &Path) -> Result<PathBuf, ShellError> {
    for _ in 0..TEMPORARY_ATTEMPTS_MAX {
        let id = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let quarantine = parent.join(format!(
            ".potion-base-8M.corrupt-{}-{id}",
            std::process::id()
        ));
        match fs::symlink_metadata(&quarantine) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(model_io_error("inspect", &quarantine, error)),
        }
        // The admitted parent is private from group/other writers. A same-user
        // namespace race remains an operating error detected by the rename.
        fs::rename(destination, &quarantine)
            .map_err(|error| model_io_error("quarantine", destination, error))?;
        index::sync_index_directory(parent)?;
        return Ok(quarantine);
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        "could not allocate a bounded corrupt-model quarantine name",
    )
    .with_context(format!("attempt limit: {TEMPORARY_ATTEMPTS_MAX}"))
    .with_help("Remove stale .potion-base-8M.corrupt-* entries and retry"))
}

enum DownloadMessage {
    Data(Vec<u8>),
    End,
    Error(String),
}

fn stream_https(agent: ureq::Agent, url: &str, sender: &mpsc::SyncSender<DownloadMessage>) {
    let response = match agent.get(url).call() {
        Ok(response) => response,
        Err(error) => {
            let _ = sender.send(DownloadMessage::Error(error.to_string()));
            return;
        }
    };
    let mut reader = response.into_body().into_reader();
    let mut buffer = [0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(DownloadMessage::End);
                return;
            }
            Ok(count) => {
                if sender
                    .send(DownloadMessage::Data(buffer[..count].to_vec()))
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(DownloadMessage::Error(error.to_string()));
                return;
            }
        }
    }
}

struct ChannelReader {
    receiver: mpsc::Receiver<DownloadMessage>,
    cancelled: Arc<AtomicBool>,
    current: io::Cursor<Vec<u8>>,
    finished: bool,
}

impl ChannelReader {
    fn new(receiver: mpsc::Receiver<DownloadMessage>, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            receiver,
            cancelled,
            current: io::Cursor::new(Vec::new()),
            finished: false,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            let count = self.current.read(buffer)?;
            if count > 0 {
                return Ok(count);
            }
            if self.finished {
                return Ok(0);
            }
            if self.cancelled.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "local AI download cancelled",
                ));
            }
            match self.receiver.recv_timeout(DOWNLOAD_CHANNEL_POLL) {
                Ok(DownloadMessage::Data(chunk)) => self.current = io::Cursor::new(chunk),
                Ok(DownloadMessage::End) => self.finished = true,
                Ok(DownloadMessage::Error(error)) => return Err(io::Error::other(error)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "bounded HTTPS reader disconnected",
                    ))
                }
            }
        }
    }
}

fn increment_counter(counter: &AtomicU64, name: &str) -> Result<u64, ShellError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map(|previous| previous + 1)
        .map_err(|observed| {
            ShellError::new(
                ErrorCode::ResourceLimit,
                format!("{name} counter was exhausted"),
            )
            .with_context(format!("limit: {}; observed: {observed}", u64::MAX))
            .with_help("Restart Quirl to create a fresh bounded generation space")
        })
}

fn truncate_utf8(value: &str, bytes_max: usize) -> String {
    if value.len() <= bytes_max {
        return value.to_owned();
    }
    let mut end = bytes_max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn asset_integrity_error(asset: AssetSpec, observed: u64) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "downloaded {} failed exact integrity verification",
            asset.name
        ),
    )
    .with_context(format!(
        "expected bytes: {}; observed bytes: {observed}; expected SHA-256: {}",
        asset.bytes, asset.sha256
    ))
    .with_help("Leave Quirl open to retry the pinned model download")
}

fn model_io_error(action: &str, path: &Path, error: io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not {action} local AI asset {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Check model-directory permissions and retry")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
        time::Instant,
    };

    const TEST_ASSETS: [AssetSpec; 3] = [
        AssetSpec {
            name: "config.json",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        },
        AssetSpec {
            name: "tokenizer.json",
            bytes: 4,
            sha256: "4c8a43980498636e9c1d1595fa5d115af7937c2422dfe68a2520a52b7a5fb4de",
        },
        AssetSpec {
            name: "model.safetensors",
            bytes: 5,
            sha256: "fa3ba64f2053ed06fc34ef5d5888983ca6ee22c7bd7d3d67d48b3faf8eac3a89",
        },
    ];

    struct MockFetcher {
        bodies: Mutex<VecDeque<Vec<u8>>>,
        opens: AtomicUsize,
    }

    impl MockFetcher {
        fn new(bodies: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                bodies: Mutex::new(bodies.into_iter().collect()),
                opens: AtomicUsize::new(0),
            }
        }
    }

    impl Fetcher for MockFetcher {
        fn open(
            &self,
            _url: &str,
            _cancelled: Arc<AtomicBool>,
        ) -> Result<Box<dyn Read + Send>, ShellError> {
            self.opens.fetch_add(1, AtomicOrdering::Relaxed);
            let body = self
                .bodies
                .lock()
                .map_err(|_| ShellError::new(ErrorCode::Io, "mock fetcher lock poisoned"))?
                .pop_front()
                .ok_or_else(|| ShellError::new(ErrorCode::Io, "mock response missing"))?;
            Ok(Box::new(io::Cursor::new(body)))
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let id = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quirl-ai-bootstrap-test-{name}-{}-{id}",
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

    fn install_fixture(
        fetcher: &dyn Fetcher,
        destination: &Path,
        cancelled: bool,
    ) -> Result<(), ShellError> {
        install_fixture_with_recovery(fetcher, destination, cancelled, false)
    }

    fn install_fixture_with_recovery(
        fetcher: &dyn Fetcher,
        destination: &Path,
        cancelled: bool,
        recover_corrupt: bool,
    ) -> Result<(), ShellError> {
        let shared = Shared::new();
        shared.cancelled.store(cancelled, Ordering::Release);
        install_model_with_assets(
            fetcher,
            destination,
            &shared,
            &TEST_ASSETS,
            "http://127.0.0.1",
            "fixture",
            recover_corrupt,
        )
    }

    fn valid_fetcher() -> MockFetcher {
        MockFetcher::new([b"abc".to_vec(), b"defg".to_vec(), b"hijkl".to_vec()])
    }

    #[test]
    fn complete_verified_assets_install_as_one_directory() {
        let directory = TestDirectory::new("success");
        let destination = directory.0.join("model");
        install_fixture(&valid_fetcher(), &destination, false).unwrap();
        validate_model_assets(&destination, &TEST_ASSETS).unwrap();
        assert!(staging_entries(&directory.0).is_empty());
    }

    #[test]
    fn bad_hash_never_installs_and_cleans_partial_assets() {
        let directory = TestDirectory::new("hash");
        let destination = directory.0.join("model");
        let fetcher = MockFetcher::new([b"abc".to_vec(), b"xxxx".to_vec(), b"hijkl".to_vec()]);
        assert_eq!(
            install_fixture(&fetcher, &destination, false)
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        assert!(!destination.exists());
        assert!(staging_entries(&directory.0).is_empty());
    }

    #[test]
    fn oversized_and_short_bodies_are_rejected_before_install() {
        for (name, first) in [("oversized", b"abcd".to_vec()), ("short", b"ab".to_vec())] {
            let directory = TestDirectory::new(name);
            let destination = directory.0.join("model");
            let fetcher = MockFetcher::new([first]);
            assert_eq!(
                install_fixture(&fetcher, &destination, false)
                    .unwrap_err()
                    .code,
                ErrorCode::Validation
            );
            assert!(!destination.exists());
            assert!(staging_entries(&directory.0).is_empty());
        }
    }

    #[test]
    fn cancellation_is_observed_before_a_download_chunk() {
        let directory = TestDirectory::new("cancel");
        let destination = directory.0.join("model");
        let error = install_fixture(&valid_fetcher(), &destination, true).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(!destination.exists());
        assert!(staging_entries(&directory.0).is_empty());
    }

    #[test]
    fn complete_cached_model_avoids_every_download() {
        let directory = TestDirectory::new("cached");
        let destination = directory.0.join("model");
        install_fixture(&valid_fetcher(), &destination, false).unwrap();
        let unused = MockFetcher::new([]);
        install_fixture(&unused, &destination, false).unwrap();
        assert_eq!(unused.opens.load(AtomicOrdering::Relaxed), 0);
    }

    #[test]
    fn corrupt_auto_owned_destination_is_quarantined_then_replaced() {
        let directory = TestDirectory::new("quarantine");
        let destination = directory.0.join("model");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("config.json"), b"bad").unwrap();
        install_fixture_with_recovery(&valid_fetcher(), &destination, false, true).unwrap();
        validate_model_assets(&destination, &TEST_ASSETS).unwrap();
        assert!(fs::read_dir(&directory.0).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().into_string().ok())
                .is_some_and(|name| name.starts_with(".potion-base-8M.corrupt-"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn model_parent_creation_rejects_an_intermediate_symlink() {
        let directory = TestDirectory::new("symlink-parent");
        let outside = directory.0.join("outside");
        let existing_child = outside.join("child");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&existing_child).unwrap();
        let linked = directory.0.join("linked");
        symlink(&outside, &linked).unwrap();
        let destination = linked.join("child/model");
        assert_eq!(
            install_fixture(&valid_fetcher(), &destination, false)
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        assert!(!existing_child.join("model").exists());
    }

    #[test]
    fn retained_activity_is_escaped_bounded_and_generations_do_not_wrap() {
        let shared = Shared::new();
        shared.publish(format!("stage\u{1b}[31m{}", "x".repeat(700)));
        let snapshot = shared.snapshot().unwrap();
        let message = snapshot.message.unwrap();
        assert!(!message.contains('\u{1b}'));
        assert!(message.len() <= ACTIVITY_MESSAGE_BYTES_MAX);
        shared
            .requested_generation
            .store(u64::MAX, Ordering::Release);
        assert_eq!(shared.request().unwrap_err().code, ErrorCode::ResourceLimit);
        assert_eq!(
            shared.requested_generation.load(Ordering::Acquire),
            u64::MAX
        );
    }

    #[test]
    fn discovery_activity_is_visible_and_publication_requests_latest_index() {
        let shared = Arc::new(Shared::new());
        let observer = CatalogObserver {
            shared: Arc::downgrade(&shared),
        };

        index::CatalogRefreshObserver::refresh_started(&observer);
        assert_eq!(
            shared.snapshot().unwrap().message.as_deref(),
            Some("AI: discovering local commands")
        );
        index::CatalogRefreshObserver::refresh_published(&observer);
        assert_eq!(shared.requested_generation.load(Ordering::Acquire), 1);
        assert_eq!(shared.snapshot().unwrap().message, None);
    }

    #[test]
    fn discovery_failure_activity_retains_bounded_escaped_cause_and_context() {
        let shared = Arc::new(Shared::new());
        let observer = CatalogObserver {
            shared: Arc::downgrade(&shared),
        };
        let error = ShellError::new(
            ErrorCode::ResourceLimit,
            format!("source limit\u{1b}[31m{}", "x".repeat(700)),
        )
        .with_context("limit: 4096; observed: 4097");

        index::CatalogRefreshObserver::refresh_failed(&observer, &error);

        let message = shared.snapshot().unwrap().message.unwrap();
        assert!(message.starts_with("AI discovery failed: source limit"));
        assert!(message.contains("limit: 4096; observed: 4097"));
        assert!(!message.contains('\u{1b}'));
        assert!(message.len() <= ACTIVITY_MESSAGE_BYTES_MAX);
    }

    #[test]
    fn database_change_during_embedding_retries_then_admits_latest_generation() {
        let shared = Shared::new();
        shared.requested_generation.store(1, Ordering::Release);

        assert!(!generation_was_superseded_or_cancelled(&shared, 1));
        increment_counter(&shared.requested_generation, "test generation").unwrap();
        assert!(generation_was_superseded_or_cancelled(&shared, 1));
        assert!(!generation_was_superseded_or_cancelled(&shared, 2));
    }

    #[test]
    fn stalled_https_channel_observes_cancellation_promptly() {
        let (_sender, receiver) = mpsc::sync_channel(DOWNLOAD_CHANNEL_CHUNKS_MAX);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            worker_cancelled.store(true, Ordering::Release);
        });
        let mut reader = ChannelReader::new(receiver, cancelled);
        let started = Instant::now();
        let error = reader.read(&mut [0_u8; 1]).unwrap_err();
        canceller.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[cfg(unix)]
    #[test]
    fn cached_model_rejects_hard_linked_assets() {
        let directory = TestDirectory::new("hardlink");
        let destination = directory.0.join("model");
        install_fixture(&valid_fetcher(), &destination, false).unwrap();
        fs::hard_link(
            destination.join("config.json"),
            directory.0.join("config-alias.json"),
        )
        .unwrap();
        assert_eq!(
            validate_model_assets(&destination, &TEST_ASSETS)
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
    }

    fn staging_entries(parent: &Path) -> Vec<PathBuf> {
        fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".potion-base-8M.download-"))
            })
            .collect()
    }
}
