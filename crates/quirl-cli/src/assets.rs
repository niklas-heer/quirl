//! Provider-neutral management of downloadable runtime assets.
//!
//! # Failure model and invariants
//!
//! A manifest, network peer, filesystem entry, or persisted retry record may be
//! malformed or may change during an operation. Every retained document and
//! payload has an explicit byte/count bound, manifests reject unknown fields,
//! and asset names cannot become paths. Downloads use HTTPS without a shell,
//! stop at the manifest size or deadline, observe cancellation between bounded
//! chunks, and are hashed before publication. Compatibility is checked before
//! network work and recorded again in the installed receipt.
//!
//! Format admission runs in an RAII staging directory before downloaded bytes
//! enter their final generation. A valid digest does not make malformed format
//! data publishable; repeated failures must leave neither a new generation nor
//! a changed receipt. Staging cleanup never targets a final generation path.
//! A bounded directory snapshot reserves generation capacity before downloading;
//! failed or interrupted previous installs cannot make later attempts grow storage
//! past the retained-generation limit. Capacity admission never deletes entries.
//!
//! Each asset is installed under its content hash. The `current.json` receipt
//! is replaced only after the complete payload is durable, so a failed update
//! cannot displace an older valid asset. A process-local plus OS file lock owns
//! all state transitions and prevents duplicate downloads. Transient failures
//! use bounded persisted exponential backoff with jitter; integrity and
//! compatibility failures are permanent for that manifest identity and resume
//! only after the manifest changes or the user requests an explicit retry.
//! Temporary files are owned by RAII guards and removed on cancellation and all
//! ordinary failure paths. Startup scheduling creates one cancellable worker
//! and never waits for its network or filesystem work before returning.

use crate::coordination::{self, CoordinationKind, CoordinationWait};
use clap::{Subcommand, ValueEnum};
use quirl_core::{
    AtomicReplaceOptions, ErrorCode, ShellError, escape_json_terminal_controls,
    escape_terminal_controls, replace_file_atomically,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};

const MANIFEST_SCHEMA_VERSION: u32 = 2;
const RECEIPT_SCHEMA_VERSION: u32 = 1;
const RETRY_STATE_SCHEMA_VERSION: u32 = 1;
const STATUS_SCHEMA_VERSION: u32 = 1;
const ASSETS_MAX: usize = 16;
const MANIFEST_BYTES_MAX: usize = 256 * 1024;
const STATE_BYTES_MAX: usize = 64 * 1024;
const RECEIPT_BYTES_MAX: usize = 16 * 1024;
const ASSET_BYTES_MAX: u64 = 1024 * 1024 * 1024;
const ASSET_BYTES_TOTAL_MAX: u64 = 2 * 1024 * 1024 * 1024;
const COMPLETION_DATABASE_BYTES_MAX: u64 = 256 * 1024 * 1024;
const COMMAND_MODEL_BYTES_MAX: u64 = 256 * 1024 * 1024;
const ASSET_GENERATIONS_MAX: usize = 4;
const ASSET_DIRECTORY_ENTRIES_MAX: usize = 64;
const LOGICAL_NAME_BYTES_MAX: usize = 64;
const FORMAT_BYTES_MAX: usize = 64;
const URL_BYTES_MAX: usize = 2 * 1024;
const COMPATIBILITY_VALUES_MAX: usize = 16;
const ASSET_NOTICES_MAX: usize = 4;
const ASSET_NOTICE_BYTES_MAX: usize = 16 * 1024;
const RETRY_ENTRIES_MAX: usize = ASSETS_MAX + 1;
const MANIFEST_RETRY_KEY: &str = "manifest";
const REQUIRED_ASSETS: [(&str, &str, u32, u64); 2] = [
    ("command-model", "tar", 1, COMMAND_MODEL_BYTES_MAX),
    (
        "completion-database",
        "sqlite3",
        1,
        COMPLETION_DATABASE_BYTES_MAX,
    ),
];
const RETRY_ATTEMPTS_MAX: u8 = 10;
const RETRY_BASE_DELAY_MS: u64 = 30_000;
const RETRY_DELAY_MS_MAX: u64 = 6 * 60 * 60 * 1_000;
const LAST_ERROR_BYTES_MAX: usize = 512;
const DOWNLOAD_BUFFER_BYTES: usize = 64 * 1024;
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(10 * 60);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const HTTP_BODY_TIMEOUT: Duration = Duration::from_secs(60);
const HTTP_REDIRECTS_MAX: u32 = 4;
const DOWNLOAD_CHANNEL_CHUNKS_MAX: usize = 2;
const DOWNLOAD_CHANNEL_POLL: Duration = Duration::from_millis(25);
const TEMPORARY_ATTEMPTS_MAX: u64 = 64;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Manual runtime-asset operations.
#[derive(Debug, Subcommand)]
pub(crate) enum AssetsCommand {
    /// Inspect installed assets and bounded retry state without network access.
    Status {
        /// Output representation for runtime-asset state.
        #[arg(long, value_enum, default_value_t = AssetsOutputFormat::Text)]
        format: AssetsOutputFormat,
    },
    /// Fetch the release manifest and install missing or changed compatible assets.
    Update {
        /// Read a local manifest instead of the current release manifest URL.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Output representation for update results.
        #[arg(long, value_enum, default_value_t = AssetsOutputFormat::Text)]
        format: AssetsOutputFormat,
    },
    /// Clear permanent/backoff state, then explicitly retry the release manifest.
    Retry {
        /// Read a local manifest instead of the current release manifest URL.
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Output representation for retry results.
        #[arg(long, value_enum, default_value_t = AssetsOutputFormat::Text)]
        format: AssetsOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum AssetsOutputFormat {
    Text,
    Json,
}

/// Version-scoped, provider-neutral runtime asset manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetManifest {
    /// Manifest contract version.
    pub schema_version: u32,
    /// Exact Quirl version whose runtime contracts can consume these assets.
    pub quirl_version: String,
    /// Individually versioned downloadable assets.
    pub assets: Vec<AssetManifestEntry>,
}

/// One logical downloadable asset and its exact identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetManifestEntry {
    /// Stable consumer-facing name, independent of storage provider.
    pub logical_name: String,
    /// Published filename, retained independently from its provider URL.
    pub file: String,
    /// Payload encoding or container format.
    pub format: String,
    /// Consumer-visible payload schema version.
    pub format_version: u32,
    /// Exact payload length.
    pub byte_size: u64,
    /// Lowercase hexadecimal SHA-256 digest.
    pub sha256: String,
    /// Absolute HTTPS provider URL.
    pub url: String,
    /// Runtime compatibility required before admission.
    pub compatibility: AssetCompatibility,
    /// Lowercase Git revision from which this asset was generated.
    pub source_revision: String,
    /// Reproducible-build timestamp from the asset source revision.
    pub source_date_epoch: u64,
    /// Bounded license notices required when redistributing this payload.
    pub notices: Vec<AssetNotice>,
}

/// One retained third-party license notice carried with an asset manifest.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetNotice {
    /// Human-readable upstream project name.
    pub name: String,
    /// SPDX license identifier for the retained text.
    pub spdx_license: String,
    /// Content-addressed sidecar filename published beside the payload.
    pub file: String,
    /// Exact UTF-8 notice length in bytes.
    pub byte_size: u64,
    /// Lowercase SHA-256 digest of `text` and the published sidecar.
    pub sha256: String,
    /// Absolute provider URL for the notice sidecar.
    pub url: String,
    /// Complete retained license text, available even before sidecar download.
    pub text: String,
}

/// Closed compatibility requirements for one asset.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetCompatibility {
    /// Exact Quirl requirement in the form `=VERSION`.
    pub quirl_version_requirement: String,
    /// Allowed `std::env::consts::OS` values; empty means all.
    #[serde(default)]
    pub operating_systems: Vec<String>,
    /// Allowed `std::env::consts::ARCH` values; empty means all.
    #[serde(default)]
    pub architectures: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InstalledReceipt {
    schema_version: u32,
    release_version: String,
    asset: AssetManifestEntry,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetryState {
    schema_version: u32,
    entries: BTreeMap<String, RetryEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RetryEntry {
    manifest_identity: String,
    attempts: u8,
    next_retry_unix_ms: u64,
    disposition: RetryDisposition,
    last_error: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RetryDisposition {
    Transient,
    Permanent,
}

#[derive(Debug, Serialize)]
struct AssetStatusReport {
    schema_version: u32,
    data_directory: PathBuf,
    cache_directory: PathBuf,
    degraded: bool,
    assets: Vec<AssetStatus>,
}

#[derive(Debug, Serialize)]
struct AssetStatus {
    logical_name: String,
    installed: bool,
    valid: bool,
    release_version: Option<String>,
    format_version: Option<u32>,
    byte_size: Option<u64>,
    retry: Option<RetryEntry>,
    diagnostic: Option<String>,
}

#[derive(Debug, Serialize)]
struct AssetUpdateReport {
    schema_version: u32,
    manifest_release_version: String,
    installed: usize,
    current: usize,
    deferred: usize,
    failed: usize,
    results: Vec<AssetUpdateResult>,
}

#[derive(Debug, Serialize)]
struct AssetUpdateResult {
    logical_name: String,
    state: &'static str,
    message: Option<String>,
}

#[derive(Debug)]
struct AssetFailure {
    error: ShellError,
    permanent: bool,
}

impl AssetFailure {
    fn transient(error: ShellError) -> Self {
        Self {
            error,
            permanent: false,
        }
    }

    fn permanent(error: ShellError) -> Self {
        Self {
            error,
            permanent: true,
        }
    }
}

trait Downloader: Send + Sync {
    fn open(
        &self,
        url: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Box<dyn Read + Send>, ShellError>;
}

struct HttpsDownloader {
    agent: ureq::Agent,
}

impl HttpsDownloader {
    fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(HTTP_REDIRECTS_MAX)
            .max_redirects_will_error(true)
            .timeout_global(Some(DOWNLOAD_DEADLINE))
            .timeout_connect(Some(HTTP_CONNECT_TIMEOUT))
            .timeout_recv_body(Some(HTTP_BODY_TIMEOUT))
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Downloader for HttpsDownloader {
    fn open(
        &self,
        url: &str,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Box<dyn Read + Send>, ShellError> {
        let (sender, receiver) = mpsc::sync_channel(DOWNLOAD_CHANNEL_CHUNKS_MAX);
        let agent = self.agent.clone();
        let url = url.to_owned();
        thread::Builder::new()
            .name("quirl-asset-https".to_owned())
            .spawn(move || stream_https(agent, &url, &sender))
            .map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not start the bounded asset HTTPS reader",
                )
                .with_context(error.to_string())
                .with_help("Check process limits and retry `quirl assets update`")
            })?;
        Ok(Box::new(ChannelReader::new(receiver, cancelled)))
    }
}

/// Opens `file://` payloads for local manifest testing. Never constructed
/// unless the manifest itself was already loaded from a local file — see
/// `install_one`'s `allow_file` gate.
struct FileDownloader;

impl Downloader for FileDownloader {
    fn open(
        &self,
        url: &str,
        _cancelled: Arc<AtomicBool>,
    ) -> Result<Box<dyn Read + Send>, ShellError> {
        let path = url.strip_prefix("file://").ok_or_else(|| {
            manifest_validation("local asset payload URL must use the file:// scheme")
        })?;
        let file = File::open(path).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not open the local asset payload")
                .with_context(error.to_string())
                .with_help("Check the file:// path in the local manifest")
        })?;
        Ok(Box::new(file))
    }
}

/// Cancellable ownership of the single startup asset worker.
pub(crate) struct BackgroundAssetRefresh {
    cancelled: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for BackgroundAssetRefresh {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct AssetSignalRegistration {
    signal_ids: Vec<signal_hook::SigId>,
}

impl AssetSignalRegistration {
    fn register(cancelled: Arc<AtomicBool>) -> Result<Self, ShellError> {
        let mut signal_ids = Vec::with_capacity(2);
        for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
            match signal_hook::flag::register(signal, Arc::clone(&cancelled)) {
                Ok(signal_id) => signal_ids.push(signal_id),
                Err(error) => {
                    for signal_id in signal_ids {
                        signal_hook::low_level::unregister(signal_id);
                    }
                    return Err(ShellError::new(
                        ErrorCode::Io,
                        "could not register runtime asset cancellation signals",
                    )
                    .with_context(error.to_string())
                    .with_help("Retry the update after restoring process signal capacity"));
                }
            }
        }
        Ok(Self { signal_ids })
    }
}

impl Drop for AssetSignalRegistration {
    fn drop(&mut self) {
        for signal_id in self.signal_ids.drain(..) {
            signal_hook::low_level::unregister(signal_id);
        }
    }
}

/// Schedule missing/current-release assets without waiting for any asset work.
pub(crate) fn schedule_background_update() -> Option<BackgroundAssetRefresh> {
    #[cfg(debug_assertions)]
    env::var_os("QUIRL_TEST_ASSET_REFRESH_ENABLE_NETWORK")?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_cancelled = Arc::clone(&cancelled);
    let worker = thread::Builder::new()
        .name("quirl-assets".to_owned())
        .spawn(move || {
            let downloader = HttpsDownloader::new();
            let _ = update_from_source(
                manifest_candidates(None),
                &downloader,
                &worker_cancelled,
                UpdateMode::Background,
            );
        })
        .ok()?;
    Some(BackgroundAssetRefresh {
        cancelled,
        worker: Some(worker),
    })
}

const PERIODIC_ASSET_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Cancellable ownership of the periodic asset-refresh worker. Held for the
/// interactive session's lifetime alongside [`BackgroundAssetRefresh`]'s
/// one-shot startup check.
pub(crate) struct PeriodicAssetRefresh {
    cancelled: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for PeriodicAssetRefresh {
    fn drop(&mut self) {
        {
            let _guard = match self.wake.0.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            self.cancelled.store(true, Ordering::Release);
        }
        self.wake.1.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Periodically re-check the manifest and install a newer completion
/// database or command model in the background, without waiting for any
/// asset work before returning.
///
/// On every successful install, `changed` is set so the interactive main
/// loop's existing `take_catalog_changed()` poll (already used to hot-swap
/// in a freshly-discovered local catalog between prompts) picks up the
/// newly-installed asset too, with no separate UI wiring.
pub(crate) fn schedule_periodic_update(changed: Arc<AtomicBool>) -> Option<PeriodicAssetRefresh> {
    #[cfg(debug_assertions)]
    env::var_os("QUIRL_TEST_ASSET_REFRESH_ENABLE_NETWORK")?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let wake = Arc::new((Mutex::new(()), Condvar::new()));
    let worker_cancelled = Arc::clone(&cancelled);
    let worker_wake = Arc::clone(&wake);
    let worker = thread::Builder::new()
        .name("quirl-assets-periodic".to_owned())
        .spawn(move || periodic_refresh_loop(&worker_cancelled, &worker_wake, &changed))
        .ok()?;
    Some(PeriodicAssetRefresh {
        cancelled,
        wake,
        worker: Some(worker),
    })
}

fn periodic_refresh_loop(
    cancelled: &Arc<AtomicBool>,
    wake: &(Mutex<()>, Condvar),
    changed: &AtomicBool,
) {
    let downloader = HttpsDownloader::new();
    loop {
        let guard = match wake.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match wake.1.wait_timeout(guard, PERIODIC_ASSET_REFRESH_INTERVAL) {
            Ok((guard, _)) => drop(guard),
            Err(poisoned) => drop(poisoned.into_inner()),
        }
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        if let Ok(report) = update_from_source(
            manifest_candidates(None),
            &downloader,
            cancelled,
            UpdateMode::Background,
        ) && report.installed > 0
        {
            changed.store(true, Ordering::Release);
        }
        if cancelled.load(Ordering::Acquire) {
            return;
        }
    }
}

pub(crate) fn wants_json(command: &AssetsCommand) -> bool {
    match command {
        AssetsCommand::Status { format }
        | AssetsCommand::Update { format, .. }
        | AssetsCommand::Retry { format, .. } => matches!(format, AssetsOutputFormat::Json),
    }
}

/// Return an admitted current payload without performing network work.
pub(crate) fn current_asset_path(logical_name: &str) -> Result<PathBuf, ShellError> {
    if !matches!(logical_name, "completion-database" | "command-model") {
        return Err(manifest_validation("unknown runtime asset logical name"));
    }
    let paths = AssetPaths::discover()?;
    current_asset_path_in(&paths, logical_name)
}

fn current_asset_path_in(paths: &AssetPaths, logical_name: &str) -> Result<PathBuf, ShellError> {
    admit_directory(&paths.data)?;
    let asset_root = paths.data.join(logical_name);
    admit_directory(&asset_root)?;
    let receipt = read_receipt(&asset_root.join("current.json"))?;
    if receipt.asset.logical_name != logical_name {
        return Err(manifest_validation(
            "installed asset receipt names a different logical asset",
        ));
    }
    validate_required_asset_contract(&receipt.asset)?;
    validate_compatibility(&receipt.asset).map_err(|failure| failure.error)?;
    let content = asset_root.join(&receipt.asset.sha256);
    admit_directory(&content)?;
    let payload = content.join("payload");
    validate_payload(&payload, &receipt.asset).map_err(|failure| failure.error)?;
    match receipt.asset.logical_name.as_str() {
        "completion-database" => {
            admit_format(&content, &payload, &receipt.asset).map_err(|failure| failure.error)?
        }
        "command-model" => {
            validate_command_model_archive(&payload).map_err(|failure| failure.error)?;
            let model_path = content.join("expanded/command-model");
            validate_command_model_directory(&model_path).map_err(|failure| failure.error)?;
            crate::intelligence::validate_managed_model(&model_path)?;
        }
        _ => return Err(manifest_validation("unknown runtime asset logical name")),
    }
    Ok(payload)
}

/// Return the admitted model directory expanded from the verified bundle.
pub(crate) fn current_command_model_path() -> Option<PathBuf> {
    let payload = current_asset_path("command-model").ok()?;
    Some(payload.parent()?.join("expanded/command-model"))
}

/// Return the current content identity without performing network work.
pub(crate) fn current_asset_identity(logical_name: &str) -> Option<String> {
    let paths = AssetPaths::discover().ok()?;
    let receipt = read_receipt(&paths.data.join(logical_name).join("current.json")).ok()?;
    Some(receipt.asset.sha256)
}

/// Read the exact admitted current payload under its manifest byte bound.
pub(crate) fn read_current_asset(logical_name: &str) -> Result<Vec<u8>, ShellError> {
    let payload = current_asset_path(logical_name)?;
    let paths = AssetPaths::discover()?;
    let receipt = read_receipt(&paths.data.join(logical_name).join("current.json"))?;
    let bytes_max = usize::try_from(receipt.asset.byte_size)
        .map_err(|_| resource_limit("current asset bytes", usize::MAX, receipt.asset.byte_size))?;
    let bytes = read_bounded_file(&payload, bytes_max)?;
    if bytes.len() != bytes_max {
        return Err(integrity_failure(
            &receipt.asset,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            None,
        )
        .error);
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if digest != receipt.asset.sha256 {
        return Err(integrity_failure(
            &receipt.asset,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            Some(&digest),
        )
        .error);
    }
    Ok(bytes)
}

pub(crate) fn execute(command: AssetsCommand) -> Result<i32, ShellError> {
    match command {
        AssetsCommand::Status { format } => {
            let report = status_report()?;
            present_status(&report, format)?;
            Ok(i32::from(report.degraded))
        }
        AssetsCommand::Update { manifest, format } => {
            execute_update(manifest, format, UpdateMode::Explicit)
        }
        AssetsCommand::Retry { manifest, format } => {
            let paths = AssetPaths::discover()?;
            create_private_directory(&paths.cache)?;
            let guard = coordination::acquire(
                &paths.state,
                CoordinationKind::Asset,
                CoordinationWait::Explicit,
            )?
            .ok_or_else(asset_lock_busy)?;
            write_retry_state(&paths.state, &RetryState::new())?;
            drop(guard);
            execute_update(manifest, format, UpdateMode::Retry)
        }
    }
}

fn execute_update(
    manifest: Option<PathBuf>,
    format: AssetsOutputFormat,
    mode: UpdateMode,
) -> Result<i32, ShellError> {
    let downloader = HttpsDownloader::new();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _signals = AssetSignalRegistration::register(Arc::clone(&cancelled))?;
    let sources = manifest_candidates(manifest);
    let report = update_from_source(sources, &downloader, &cancelled, mode)?;
    let failed = report.failed > 0;
    present_update(&report, format)?;
    Ok(i32::from(failed))
}

#[derive(Clone, Copy)]
enum UpdateMode {
    Background,
    Explicit,
    Retry,
}

#[derive(Clone)]
enum ManifestSource {
    File(PathBuf),
    Url(String),
}

impl ManifestSource {
    fn identity(&self) -> String {
        let mut hasher = Sha256::new();
        match self {
            Self::File(path) => hasher.update(path.as_os_str().to_string_lossy().as_bytes()),
            Self::Url(url) => hasher.update(url.as_bytes()),
        }
        format!("{:x}", hasher.finalize())
    }
}

struct AssetPaths {
    data: PathBuf,
    cache: PathBuf,
    state: PathBuf,
}

impl AssetPaths {
    fn discover() -> Result<Self, ShellError> {
        let data = asset_data_directory()?;
        let cache = asset_cache_directory()?;
        Ok(Self {
            state: cache.join("retry-state-v1.json"),
            data,
            cache,
        })
    }
}

impl RetryState {
    fn new() -> Self {
        Self {
            schema_version: RETRY_STATE_SCHEMA_VERSION,
            entries: BTreeMap::new(),
        }
    }
}

fn update_from_source(
    sources: Vec<ManifestSource>,
    downloader: &dyn Downloader,
    cancelled: &Arc<AtomicBool>,
    mode: UpdateMode,
) -> Result<AssetUpdateReport, ShellError> {
    let paths = AssetPaths::discover()?;
    create_private_directory(&paths.data)?;
    create_private_directory(&paths.cache)?;
    // One retry-state entry covers the whole candidate list: if every
    // candidate is currently failing, back off the set as a unit rather than
    // tracking per-mirror state, and if any candidate works the whole
    // resolution succeeds so backoff never applies.
    let source_identity = sources
        .iter()
        .map(ManifestSource::identity)
        .collect::<Vec<_>>()
        .join(",");
    let wait = match mode {
        UpdateMode::Background => CoordinationWait::Background,
        UpdateMode::Explicit | UpdateMode::Retry => CoordinationWait::Explicit,
    };
    let Some(_guard) = coordination::acquire(&paths.state, CoordinationKind::Asset, wait)? else {
        return Ok(AssetUpdateReport {
            schema_version: STATUS_SCHEMA_VERSION,
            manifest_release_version: "unknown".to_owned(),
            installed: 0,
            current: 0,
            deferred: 1,
            failed: 0,
            results: vec![AssetUpdateResult {
                logical_name: MANIFEST_RETRY_KEY.to_owned(),
                state: "deferred",
                message: Some("another Quirl process owns the asset update".to_owned()),
            }],
        });
    };
    let mut retry = read_retry_state(&paths.state)?;
    let now_ms = unix_time_ms();
    if matches!(mode, UpdateMode::Background)
        && let Some(entry) = retry.entries.get(MANIFEST_RETRY_KEY)
        && entry.manifest_identity == source_identity
        && (entry.disposition == RetryDisposition::Permanent || entry.next_retry_unix_ms > now_ms)
    {
        return Ok(AssetUpdateReport {
            schema_version: STATUS_SCHEMA_VERSION,
            manifest_release_version: "unknown".to_owned(),
            installed: 0,
            current: 0,
            deferred: 1,
            failed: 0,
            results: vec![AssetUpdateResult {
                logical_name: MANIFEST_RETRY_KEY.to_owned(),
                state: "deferred",
                message: Some(entry.last_error.clone()),
            }],
        });
    }
    let (manifest, allow_file) = match resolve_manifest(&sources, downloader, cancelled).and_then(
        |(manifest, allow_file)| {
            validate_manifest(&manifest, allow_file)?;
            Ok((manifest, allow_file))
        },
    ) {
        Ok(resolved) => resolved,
        Err(error) => {
            record_manifest_failure(&mut retry, source_identity, &error, now_ms)?;
            write_retry_state(&paths.state, &retry)?;
            return Err(error);
        }
    };
    retry.entries.remove(MANIFEST_RETRY_KEY);
    let mut report = AssetUpdateReport {
        schema_version: STATUS_SCHEMA_VERSION,
        manifest_release_version: manifest.quirl_version.clone(),
        installed: 0,
        current: 0,
        deferred: 0,
        failed: 0,
        results: Vec::with_capacity(manifest.assets.len()),
    };
    for asset in &manifest.assets {
        if cancelled.load(Ordering::Acquire) {
            report.deferred = report.deferred.saturating_add(1);
            report.results.push(AssetUpdateResult {
                logical_name: asset.logical_name.clone(),
                state: "cancelled",
                message: Some("asset update was cancelled".to_owned()),
            });
            continue;
        }
        let identity = manifest_identity(&manifest, asset, &source_identity);
        if let Some(entry) = retry.entries.get(&asset.logical_name)
            && entry.manifest_identity == identity
            && !matches!(mode, UpdateMode::Retry)
        {
            let blocked = entry.disposition == RetryDisposition::Permanent
                || (matches!(mode, UpdateMode::Background) && entry.next_retry_unix_ms > now_ms);
            if blocked {
                report.deferred = report.deferred.saturating_add(1);
                report.results.push(AssetUpdateResult {
                    logical_name: asset.logical_name.clone(),
                    state: "deferred",
                    message: Some(entry.last_error.clone()),
                });
                continue;
            }
        }
        match install_one(
            &paths,
            &manifest.quirl_version,
            asset,
            downloader,
            allow_file,
            cancelled,
        ) {
            Ok(InstallOutcome::Current) => {
                retry.entries.remove(&asset.logical_name);
                report.current = report.current.saturating_add(1);
                report.results.push(AssetUpdateResult {
                    logical_name: asset.logical_name.clone(),
                    state: "current",
                    message: None,
                });
            }
            Ok(InstallOutcome::Installed) => {
                retry.entries.remove(&asset.logical_name);
                report.installed = report.installed.saturating_add(1);
                report.results.push(AssetUpdateResult {
                    logical_name: asset.logical_name.clone(),
                    state: "installed",
                    message: None,
                });
            }
            Err(failure) => {
                record_failure(&mut retry, asset, identity, &failure, now_ms)?;
                report.failed = report.failed.saturating_add(1);
                report.results.push(AssetUpdateResult {
                    logical_name: asset.logical_name.clone(),
                    state: if failure.permanent {
                        "permanent_failure"
                    } else {
                        "transient_failure"
                    },
                    message: Some(failure.error.message),
                });
            }
        }
    }
    write_retry_state(&paths.state, &retry)?;
    Ok(report)
}

#[derive(Debug)]
enum InstallOutcome {
    Current,
    Installed,
}

fn install_one(
    paths: &AssetPaths,
    quirl_version: &str,
    asset: &AssetManifestEntry,
    downloader: &dyn Downloader,
    allow_file: bool,
    cancelled: &Arc<AtomicBool>,
) -> Result<InstallOutcome, AssetFailure> {
    let admission_started = Instant::now();
    validate_manifest_entry(asset, allow_file).map_err(AssetFailure::permanent)?;
    validate_required_asset_contract(asset).map_err(AssetFailure::permanent)?;
    validate_compatibility(asset)?;
    let asset_root = paths.data.join(&asset.logical_name);
    let receipt_path = asset_root.join("current.json");
    let previous_hash = read_receipt(&receipt_path)
        .ok()
        .map(|receipt| receipt.asset.sha256);
    if let Ok(receipt) = read_receipt(&receipt_path)
        && receipt.release_version == quirl_version
        && receipt.asset.sha256 == asset.sha256
        && validate_installed_payload(&asset_root, &receipt.asset).is_ok()
    {
        return Ok(InstallOutcome::Current);
    }
    create_private_directory(&asset_root).map_err(AssetFailure::transient)?;
    admit_generation_capacity(&asset_root, &asset.sha256).map_err(AssetFailure::permanent)?;
    let mut temporary = TemporaryDownload::create(&asset_root).map_err(AssetFailure::transient)?;
    let mut reader = if allow_file && asset.url.starts_with("file://") {
        FileDownloader
            .open(&asset.url, Arc::clone(cancelled))
            .map_err(AssetFailure::permanent)?
    } else {
        downloader
            .open(&asset.url, Arc::clone(cancelled))
            .map_err(AssetFailure::transient)?
    };
    let mut temporary_file = temporary.take_file().map_err(AssetFailure::transient)?;
    download_and_verify(
        reader.as_mut(),
        &mut temporary_file,
        temporary.path(),
        asset,
        cancelled,
    )?;
    drop(temporary_file);
    // The hash authenticates bytes, not their format. Validate while every
    // output still belongs to staging so a failed admission cannot consume a
    // retained generation or require deleting a pre-existing final path.
    {
        let staging = TemporaryDirectory::create(&asset_root, ".asset-admission")
            .map_err(AssetFailure::transient)?;
        admit_format_controlled(
            staging.path(),
            temporary.path(),
            asset,
            cancelled,
            admission_started,
        )?;
    }
    let content_directory = asset_root.join(&asset.sha256);
    create_private_directory(&content_directory).map_err(AssetFailure::transient)?;
    admit_directory(&asset_root).map_err(AssetFailure::permanent)?;
    admit_directory(&content_directory).map_err(AssetFailure::permanent)?;
    let payload = content_directory.join("payload");
    if path_exists(&payload).map_err(AssetFailure::transient)? {
        validate_payload_controlled(&payload, asset, cancelled, admission_started)?;
    } else {
        admit_directory(&asset_root).map_err(AssetFailure::permanent)?;
        admit_directory(&content_directory).map_err(AssetFailure::permanent)?;
        fs::rename(temporary.path(), &payload)
            .map_err(|error| AssetFailure::transient(asset_io_error("install", &payload, error)))?;
        temporary.installed = true;
        sync_directory(&content_directory).map_err(AssetFailure::transient)?;
    }
    validate_payload_controlled(&payload, asset, cancelled, admission_started)?;
    admit_format_controlled(
        &content_directory,
        &payload,
        asset,
        cancelled,
        admission_started,
    )?;
    let receipt = InstalledReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        release_version: quirl_version.to_owned(),
        asset: asset.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&receipt).map_err(|error| {
        AssetFailure::permanent(
            ShellError::new(ErrorCode::Validation, "could not encode the asset receipt")
                .with_context(error.to_string())
                .with_help("Report this Quirl asset-schema defect"),
        )
    })?;
    admission_checkpoint(cancelled, admission_started)?;
    write_atomic(&receipt_path, &bytes, RECEIPT_BYTES_MAX).map_err(AssetFailure::transient)?;
    cleanup_generations(&asset_root, &asset.sha256, previous_hash.as_deref())
        .map_err(AssetFailure::transient)?;
    Ok(InstallOutcome::Installed)
}

fn admit_format(
    content_directory: &Path,
    payload: &Path,
    asset: &AssetManifestEntry,
) -> Result<(), AssetFailure> {
    admit_format_controlled(
        content_directory,
        payload,
        asset,
        &AtomicBool::new(false),
        Instant::now(),
    )
}

fn admit_format_controlled(
    content_directory: &Path,
    payload: &Path,
    asset: &AssetManifestEntry,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<(), AssetFailure> {
    admission_checkpoint(cancelled, started)?;
    match (
        asset.logical_name.as_str(),
        asset.format.as_str(),
        asset.format_version,
    ) {
        ("completion-database", "sqlite3", 1) => {
            if asset.byte_size > COMPLETION_DATABASE_BYTES_MAX {
                return Err(AssetFailure::permanent(resource_limit(
                    "completion database bytes",
                    COMPLETION_DATABASE_BYTES_MAX,
                    asset.byte_size,
                )));
            }
            let bytes = read_bounded_file_controlled(
                payload,
                usize::try_from(COMPLETION_DATABASE_BYTES_MAX).unwrap_or(usize::MAX),
                cancelled,
                started,
            )
            .map_err(AssetFailure::permanent)?;
            quirl_catalog::NativeCatalogReader::from_bytes(
                &bytes,
                quirl_catalog::NativeCatalogLimits::embedded(),
            )
            .map_err(|diagnostic| {
                AssetFailure::permanent(
                    ShellError::new(
                        ErrorCode::Validation,
                        "downloaded completion database has an incompatible schema",
                    )
                    .with_context(diagnostic.message)
                    .with_help("Publish a database accepted by this Quirl release"),
                )
            })?;
            Ok(())
        }
        ("command-model", "tar", 1) => {
            if asset.byte_size > COMMAND_MODEL_BYTES_MAX {
                return Err(AssetFailure::permanent(resource_limit(
                    "command model bytes",
                    COMMAND_MODEL_BYTES_MAX,
                    asset.byte_size,
                )));
            }
            extract_command_model(content_directory, payload, cancelled, started)?;
            admission_checkpoint(cancelled, started)?;
            let model_path = content_directory.join("expanded/command-model");
            crate::intelligence::validate_managed_model(&model_path).map_err(|error| {
                AssetFailure::permanent(
                    error
                        .with_help("Publish a command-model bundle accepted by this Quirl release"),
                )
            })?;
            admission_checkpoint(cancelled, started)
        }
        _ => Err(AssetFailure::permanent(
            ShellError::new(
                ErrorCode::Validation,
                format!(
                    "runtime asset {} uses an unsupported format contract",
                    asset.logical_name
                ),
            )
            .with_context(format!(
                "format: {}; format version: {}",
                asset.format, asset.format_version
            ))
            .with_help("Publish one of this Quirl release's documented runtime assets"),
        )),
    }
}

fn admit_generation_capacity(asset_root: &Path, candidate_hash: &str) -> Result<(), ShellError> {
    let generations = scan_generations(asset_root)?;
    let already_retained = generations
        .iter()
        .any(|(name, _, _)| name == candidate_hash);
    let prospective_count = generations
        .len()
        .saturating_add(usize::from(!already_retained));
    if prospective_count > ASSET_GENERATIONS_MAX {
        return Err(resource_limit(
            "retained asset generations",
            ASSET_GENERATIONS_MAX,
            prospective_count,
        ));
    }
    Ok(())
}

fn scan_generations(asset_root: &Path) -> Result<Vec<(String, PathBuf, fs::FileType)>, ShellError> {
    let entries =
        fs::read_dir(asset_root).map_err(|error| asset_io_error("list", asset_root, error))?;
    let mut generations = Vec::new();
    // Admission covers the complete bounded snapshot before any deletion.
    // Receipts and unrelated entries cannot consume the generation scan slots.
    for (index, entry) in entries
        .take(ASSET_DIRECTORY_ENTRIES_MAX.saturating_add(1))
        .enumerate()
    {
        if index == ASSET_DIRECTORY_ENTRIES_MAX {
            return Err(resource_limit(
                "asset directory entries",
                ASSET_DIRECTORY_ENTRIES_MAX,
                index.saturating_add(1),
            ));
        }
        let entry = entry.map_err(|error| asset_io_error("list", asset_root, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            generations.push((
                name,
                entry.path(),
                entry
                    .file_type()
                    .map_err(|error| asset_io_error("inspect", &entry.path(), error))?,
            ));
        }
    }
    if generations.len() > ASSET_GENERATIONS_MAX {
        return Err(resource_limit(
            "retained asset generations",
            ASSET_GENERATIONS_MAX,
            generations.len(),
        ));
    }
    Ok(generations)
}

fn cleanup_generations(
    asset_root: &Path,
    current_hash: &str,
    previous_hash: Option<&str>,
) -> Result<(), ShellError> {
    let generations = scan_generations(asset_root)?;
    for (name, path, file_type) in generations {
        if name == current_hash || previous_hash.is_some_and(|previous| previous == name) {
            continue;
        }
        if !file_type.is_dir() || file_type.is_symlink() {
            return Err(manifest_validation(
                "runtime asset generation is not a real directory",
            ));
        }
        fs::remove_dir_all(&path)
            .map_err(|error| asset_io_error("remove stale generation", &path, error))?;
    }
    Ok(())
}

fn extract_command_model(
    content_directory: &Path,
    payload: &Path,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<(), AssetFailure> {
    let expanded = content_directory.join("expanded");
    if path_exists(&expanded).map_err(AssetFailure::transient)? {
        return validate_command_model_directory(&expanded.join("command-model"));
    }
    let mut temporary = TemporaryDirectory::create(content_directory, ".expanded")
        .map_err(AssetFailure::transient)?;
    let model_root = temporary.path().join("command-model");
    create_private_directory(&model_root).map_err(AssetFailure::transient)?;
    let (mut archive, _) = open_existing_regular(payload).map_err(AssetFailure::permanent)?;
    let mut admitted = BTreeSet::new();
    let mut header = [0_u8; 512];
    loop {
        admission_checkpoint(cancelled, started)?;
        archive.read_exact(&mut header).map_err(|error| {
            AssetFailure::permanent(asset_io_error("read model archive", payload, error))
        })?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        validate_tar_checksum(&header)?;
        let name = tar_path(&header)?;
        let size = parse_tar_octal(&header[124..136], "file size")?;
        let entry_type = header[156];
        if name == "command-model/" && matches!(entry_type, 0 | b'0' | b'5') {
            skip_tar_bytes(&mut archive, size, payload, cancelled, started)?;
            skip_tar_padding(&mut archive, size, payload, cancelled, started)?;
            continue;
        }
        let relative = name.strip_prefix("command-model/").ok_or_else(|| {
            AssetFailure::permanent(manifest_validation(
                "command model archive entry escaped its required root",
            ))
        })?;
        let bytes_max = command_model_file_limit(relative).ok_or_else(|| {
            AssetFailure::permanent(manifest_validation(
                "command model archive contains an unexpected entry",
            ))
        })?;
        if !matches!(entry_type, 0 | b'0') || size == 0 || size > bytes_max {
            return Err(AssetFailure::permanent(resource_limit(
                "command model entry bytes",
                bytes_max,
                size,
            )));
        }
        if !admitted.insert(relative.to_owned()) {
            return Err(AssetFailure::permanent(manifest_validation(
                "command model archive contains a duplicate entry",
            )));
        }
        let output = model_root.join(relative);
        let mut file = open_new_private_file(&output).map_err(AssetFailure::transient)?;
        copy_exact_tar_bytes(&mut archive, &mut file, size, payload, cancelled, started)?;
        file.sync_all()
            .map_err(|error| AssetFailure::transient(asset_io_error("sync", &output, error)))?;
        skip_tar_padding(&mut archive, size, payload, cancelled, started)?;
    }
    let expected: BTreeSet<String> = [
        "LICENSE",
        "README.md",
        "config.json",
        "model.safetensors",
        "quirl-model.json",
        "tokenizer.json",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if admitted != expected {
        return Err(AssetFailure::permanent(
            manifest_validation("command model archive is incomplete").with_context(format!(
                "expected entries: {}; observed entries: {}",
                expected.len(),
                admitted.len()
            )),
        ));
    }
    validate_command_model_directory(&model_root)?;
    sync_directory(&model_root).map_err(AssetFailure::transient)?;
    admit_directory(content_directory).map_err(AssetFailure::permanent)?;
    fs::rename(temporary.path(), &expanded).map_err(|error| {
        AssetFailure::transient(asset_io_error("install expanded model", &expanded, error))
    })?;
    temporary.installed = true;
    sync_directory(content_directory).map_err(AssetFailure::transient)?;
    Ok(())
}

fn validate_command_model_archive(payload: &Path) -> Result<(), AssetFailure> {
    validate_command_model_archive_controlled(payload, &AtomicBool::new(false), Instant::now())
}

fn validate_command_model_archive_controlled(
    payload: &Path,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<(), AssetFailure> {
    let (mut archive, metadata) =
        open_existing_regular(payload).map_err(AssetFailure::permanent)?;
    if metadata.len() == 0 || metadata.len() > COMMAND_MODEL_BYTES_MAX {
        return Err(AssetFailure::permanent(resource_limit(
            "command model archive bytes",
            COMMAND_MODEL_BYTES_MAX,
            metadata.len(),
        )));
    }
    let mut admitted = BTreeSet::new();
    let mut header = [0_u8; 512];
    loop {
        admission_checkpoint(cancelled, started)?;
        archive.read_exact(&mut header).map_err(|error| {
            AssetFailure::permanent(asset_io_error("read model archive", payload, error))
        })?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        validate_tar_checksum(&header)?;
        let name = tar_path(&header)?;
        let size = parse_tar_octal(&header[124..136], "file size")?;
        let entry_type = header[156];
        if name == "command-model/" && matches!(entry_type, 0 | b'0' | b'5') {
            skip_tar_bytes(&mut archive, size, payload, cancelled, started)?;
            skip_tar_padding(&mut archive, size, payload, cancelled, started)?;
            continue;
        }
        let relative = name.strip_prefix("command-model/").ok_or_else(|| {
            AssetFailure::permanent(manifest_validation(
                "command model archive entry escaped its required root",
            ))
        })?;
        let bytes_max = command_model_file_limit(relative).ok_or_else(|| {
            AssetFailure::permanent(manifest_validation(
                "command model archive contains an unexpected entry",
            ))
        })?;
        if !matches!(entry_type, 0 | b'0') || size == 0 || size > bytes_max {
            return Err(AssetFailure::permanent(resource_limit(
                "command model entry bytes",
                bytes_max,
                size,
            )));
        }
        if !admitted.insert(relative.to_owned()) {
            return Err(AssetFailure::permanent(manifest_validation(
                "command model archive contains a duplicate entry",
            )));
        }
        skip_tar_bytes(&mut archive, size, payload, cancelled, started)?;
        skip_tar_padding(&mut archive, size, payload, cancelled, started)?;
    }
    let expected: BTreeSet<String> = [
        "LICENSE",
        "README.md",
        "config.json",
        "model.safetensors",
        "quirl-model.json",
        "tokenizer.json",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    if admitted != expected {
        return Err(AssetFailure::permanent(
            manifest_validation("command model archive is incomplete").with_context(format!(
                "expected entries: {}; observed entries: {}",
                expected.len(),
                admitted.len()
            )),
        ));
    }
    Ok(())
}

fn validate_command_model_directory(path: &Path) -> Result<(), AssetFailure> {
    for name in [
        "LICENSE",
        "README.md",
        "config.json",
        "model.safetensors",
        "quirl-model.json",
        "tokenizer.json",
    ] {
        let file_path = path.join(name);
        let metadata = fs::symlink_metadata(&file_path).map_err(|error| {
            AssetFailure::permanent(asset_io_error("inspect", &file_path, error))
        })?;
        let limit = command_model_file_limit(name).unwrap_or(0);
        if !metadata.file_type().is_file() || metadata.len() == 0 || metadata.len() > limit {
            return Err(AssetFailure::permanent(resource_limit(
                "command model file bytes",
                limit,
                metadata.len(),
            )));
        }
    }
    Ok(())
}

fn command_model_file_limit(name: &str) -> Option<u64> {
    match name {
        "LICENSE" | "README.md" | "config.json" => Some(1024 * 1024),
        "quirl-model.json" => Some(64 * 1024),
        "tokenizer.json" => Some(16 * 1024 * 1024),
        "model.safetensors" => Some(COMMAND_MODEL_BYTES_MAX),
        _ => None,
    }
}

fn validate_tar_checksum(header: &[u8; 512]) -> Result<(), AssetFailure> {
    let expected = parse_tar_octal(&header[148..156], "header checksum")?;
    let observed = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum::<u64>();
    if observed != expected {
        return Err(AssetFailure::permanent(manifest_validation(
            "command model archive header checksum is invalid",
        )));
    }
    Ok(())
}

fn tar_path(header: &[u8; 512]) -> Result<String, AssetFailure> {
    let name = tar_text(&header[..100])?;
    let prefix = tar_text(&header[345..500])?;
    let path = if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    };
    if path.len() > 255 || path.contains("..") || path.starts_with('/') || path.contains('\\') {
        return Err(AssetFailure::permanent(manifest_validation(
            "command model archive contains an unsafe path",
        )));
    }
    Ok(path)
}

fn tar_text(bytes: &[u8]) -> Result<String, AssetFailure> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(bytes.get(..end).unwrap_or_default())
        .map(str::to_owned)
        .map_err(|error| {
            AssetFailure::permanent(
                manifest_validation("command model archive path is not UTF-8")
                    .with_context(error.to_string()),
            )
        })
}

fn parse_tar_octal(bytes: &[u8], label: &str) -> Result<u64, AssetFailure> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        AssetFailure::permanent(
            manifest_validation(format!("command model archive {label} is not ASCII"))
                .with_context(error.to_string()),
        )
    })?;
    let trimmed = text.trim_matches(|character: char| character == '\0' || character == ' ');
    u64::from_str_radix(trimmed, 8).map_err(|error| {
        AssetFailure::permanent(
            manifest_validation(format!("command model archive {label} is invalid"))
                .with_context(error.to_string()),
        )
    })
}

fn copy_exact_tar_bytes(
    archive: &mut File,
    output: &mut File,
    size: u64,
    archive_path: &Path,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<(), AssetFailure> {
    let mut limited = archive.take(size);
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        admission_checkpoint(cancelled, started)?;
        let count = limited.read(&mut buffer).map_err(|error| {
            AssetFailure::transient(asset_io_error("extract", archive_path, error))
        })?;
        if count == 0 {
            break;
        }
        output
            .write_all(buffer.get(..count).unwrap_or_default())
            .map_err(|error| {
                AssetFailure::transient(asset_io_error("extract", archive_path, error))
            })?;
        copied = copied.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    }
    if copied != size {
        return Err(AssetFailure::permanent(manifest_validation(
            "command model archive entry is truncated",
        )));
    }
    Ok(())
}

fn skip_tar_bytes(
    archive: &mut File,
    size: u64,
    path: &Path,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<(), AssetFailure> {
    let mut limited = archive.take(size);
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        admission_checkpoint(cancelled, started)?;
        let count = limited.read(&mut buffer).map_err(|error| {
            AssetFailure::transient(asset_io_error("read model archive", path, error))
        })?;
        if count == 0 {
            break;
        }
        copied = copied.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
    }
    if copied != size {
        return Err(AssetFailure::permanent(manifest_validation(
            "command model archive is truncated",
        )));
    }
    Ok(())
}

fn skip_tar_padding(
    archive: &mut File,
    size: u64,
    path: &Path,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<(), AssetFailure> {
    let remainder = size.checked_rem(512).unwrap_or_default();
    let padding = 512_u64
        .saturating_sub(remainder)
        .checked_rem(512)
        .unwrap_or_default();
    skip_tar_bytes(archive, padding, path, cancelled, started)
}

struct TemporaryDirectory {
    path: PathBuf,
    installed: bool,
}

impl TemporaryDirectory {
    fn create(parent: &Path, prefix: &str) -> Result<Self, ShellError> {
        for _ in 0..TEMPORARY_ATTEMPTS_MAX {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{prefix}-{}-{sequence}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        installed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(asset_io_error("create directory", &path, error)),
            }
        }
        Err(resource_limit(
            "temporary directory names",
            TEMPORARY_ATTEMPTS_MAX,
            TEMPORARY_ATTEMPTS_MAX,
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn download_and_verify(
    reader: &mut dyn Read,
    file: &mut File,
    path: &Path,
    asset: &AssetManifestEntry,
    cancelled: &AtomicBool,
) -> Result<(), AssetFailure> {
    let started = Instant::now();
    let mut observed = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(AssetFailure::transient(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "runtime asset download was cancelled",
                )
                .with_help("Run `quirl assets retry` when network access is available"),
            ));
        }
        if started.elapsed() > DOWNLOAD_DEADLINE {
            return Err(AssetFailure::transient(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "runtime asset download exceeded its wall deadline",
                )
                .with_context(format!(
                    "limit: {} ms; observed: at least {} ms",
                    DOWNLOAD_DEADLINE.as_millis(),
                    started.elapsed().as_millis()
                ))
                .with_help("Check the network and run `quirl assets retry`"),
            ));
        }
        let count = reader
            .read(&mut buffer)
            .map_err(|error| AssetFailure::transient(asset_io_error("download", path, error)))?;
        if count == 0 {
            break;
        }
        observed = observed.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if observed > asset.byte_size || observed > ASSET_BYTES_MAX {
            return Err(integrity_failure(asset, observed, None));
        }
        let chunk = buffer.get(..count).unwrap_or_default();
        file.write_all(chunk)
            .map_err(|error| AssetFailure::transient(asset_io_error("write", path, error)))?;
        hasher.update(chunk);
    }
    file.sync_all()
        .map_err(|error| AssetFailure::transient(asset_io_error("sync", path, error)))?;
    let digest = format!("{:x}", hasher.finalize());
    if observed != asset.byte_size || digest != asset.sha256 {
        return Err(integrity_failure(asset, observed, Some(&digest)));
    }
    Ok(())
}

fn validate_installed_payload(
    asset_root: &Path,
    asset: &AssetManifestEntry,
) -> Result<(), AssetFailure> {
    validate_payload(&asset_root.join(&asset.sha256).join("payload"), asset)
}

fn validate_payload(path: &Path, asset: &AssetManifestEntry) -> Result<(), AssetFailure> {
    validate_payload_controlled(path, asset, &AtomicBool::new(false), Instant::now())
}

fn validate_payload_controlled(
    path: &Path,
    asset: &AssetManifestEntry,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<(), AssetFailure> {
    let (mut file, metadata) = open_existing_regular(path).map_err(AssetFailure::permanent)?;
    if metadata.len() != asset.byte_size {
        return Err(integrity_failure(asset, metadata.len(), None));
    }
    let mut observed = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        admission_checkpoint(cancelled, started)?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| AssetFailure::permanent(asset_io_error("read", path, error)))?;
        if count == 0 {
            break;
        }
        observed = observed.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
        if observed > asset.byte_size {
            return Err(integrity_failure(asset, observed, None));
        }
        hasher.update(buffer.get(..count).unwrap_or_default());
    }
    let digest = format!("{:x}", hasher.finalize());
    if observed != asset.byte_size || digest != asset.sha256 {
        return Err(integrity_failure(asset, observed, Some(&digest)));
    }
    Ok(())
}

fn validate_manifest(manifest: &AssetManifest, allow_file: bool) -> Result<(), ShellError> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(manifest_validation(
            "unsupported asset manifest schema version",
        ));
    }
    if manifest.quirl_version.is_empty() || manifest.quirl_version.len() > 64 {
        return Err(manifest_validation("invalid asset manifest Quirl version"));
    }
    if manifest.quirl_version != env!("CARGO_PKG_VERSION") {
        return Err(
            manifest_validation("runtime asset manifest does not target this Quirl").with_context(
                format!(
                    "manifest Quirl version: {}; current Quirl version: {}",
                    manifest.quirl_version,
                    env!("CARGO_PKG_VERSION")
                ),
            ),
        );
    }
    if manifest.assets.is_empty() || manifest.assets.len() > ASSETS_MAX {
        return Err(resource_limit(
            "manifest assets",
            ASSETS_MAX,
            manifest.assets.len(),
        ));
    }
    let mut total = 0_u64;
    let mut names = BTreeSet::new();
    for asset in &manifest.assets {
        validate_manifest_entry(asset, allow_file)?;
        validate_required_asset_contract(asset)?;
        if asset.compatibility.quirl_version_requirement != format!("={}", manifest.quirl_version) {
            return Err(manifest_validation(format!(
                "runtime asset {} is not bound to manifest Quirl version {}",
                asset.logical_name, manifest.quirl_version
            )));
        }
        if !names.insert(asset.logical_name.as_str()) {
            return Err(manifest_validation("duplicate logical asset name"));
        }
        total = total.checked_add(asset.byte_size).ok_or_else(|| {
            resource_limit("manifest asset bytes", ASSET_BYTES_TOTAL_MAX, u64::MAX)
        })?;
        if total > ASSET_BYTES_TOTAL_MAX {
            return Err(resource_limit(
                "manifest asset bytes",
                ASSET_BYTES_TOTAL_MAX,
                total,
            ));
        }
    }
    for (logical_name, _, _, _) in REQUIRED_ASSETS {
        if !names.contains(logical_name) {
            return Err(manifest_validation(format!(
                "runtime asset manifest is missing required {logical_name}"
            )));
        }
    }
    if names.len() != REQUIRED_ASSETS.len() {
        return Err(manifest_validation(
            "runtime asset manifest contains an unexpected logical asset",
        ));
    }
    Ok(())
}

fn validate_required_asset_contract(asset: &AssetManifestEntry) -> Result<(), ShellError> {
    let Some((_, required_format, required_version, bytes_max)) = REQUIRED_ASSETS
        .iter()
        .find(|(logical_name, _, _, _)| *logical_name == asset.logical_name)
    else {
        return Err(manifest_validation(format!(
            "unexpected runtime asset {}",
            asset.logical_name
        )));
    };
    if asset.format != *required_format || asset.format_version != *required_version {
        return Err(manifest_validation(format!(
            "runtime asset {} has an unsupported format contract",
            asset.logical_name
        ))
        .with_context(format!(
            "expected: {required_format} v{required_version}; observed: {} v{}",
            asset.format, asset.format_version
        )));
    }
    if asset.byte_size == 0 || asset.byte_size > *bytes_max {
        return Err(resource_limit(
            "format-specific asset bytes",
            bytes_max,
            asset.byte_size,
        ));
    }
    Ok(())
}

fn validate_manifest_entry(asset: &AssetManifestEntry, allow_file: bool) -> Result<(), ShellError> {
    if asset.logical_name.is_empty()
        || asset.logical_name.len() > LOGICAL_NAME_BYTES_MAX
        || !asset
            .logical_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(manifest_validation("invalid logical asset name")
            .with_context(format!("observed: {:?}", asset.logical_name)));
    }
    if asset.format.is_empty()
        || asset.format.len() > FORMAT_BYTES_MAX
        || !asset
            .format
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(manifest_validation("invalid runtime asset format"));
    }
    if asset.file.is_empty()
        || asset.file.len() > 255
        || asset.file.contains('/')
        || asset.file.contains('\\')
        || asset.file == "."
        || asset.file == ".."
    {
        return Err(manifest_validation("invalid runtime asset filename"));
    }
    if asset.format_version == 0 || asset.byte_size == 0 || asset.byte_size > ASSET_BYTES_MAX {
        return Err(resource_limit(
            "asset bytes",
            ASSET_BYTES_MAX,
            asset.byte_size,
        ));
    }
    if asset.sha256.len() != 64
        || !asset
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(manifest_validation("invalid runtime asset SHA-256"));
    }
    if asset.source_revision.len() != 40
        || !asset
            .source_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(manifest_validation(
            "runtime asset source revision is not a lowercase 40-hex identity",
        ));
    }
    if asset.source_date_epoch == 0 {
        return Err(manifest_validation(
            "runtime asset source date epoch must be nonzero",
        ));
    }
    validate_asset_notices(asset, allow_file)?;
    if matches!(
        asset.logical_name.as_str(),
        "completion-database" | "command-model"
    ) && asset.file
        != content_addressed_file(
            &asset.logical_name,
            env!("CARGO_PKG_VERSION"),
            &asset.sha256,
        )?
    {
        return Err(manifest_validation(
            "runtime asset filename is not bound to its content digest",
        ));
    }
    let scheme_ok =
        asset.url.starts_with("https://") || (allow_file && asset.url.starts_with("file://"));
    if asset.url.len() > URL_BYTES_MAX || !scheme_ok {
        return Err(manifest_validation(if allow_file {
            "runtime asset URL must be bounded absolute HTTPS or file://"
        } else {
            "runtime asset URL must be bounded absolute HTTPS"
        }));
    }
    if !asset.url.ends_with(&format!("/{}", asset.file)) {
        return Err(manifest_validation(
            "runtime asset URL does not end in its content-addressed filename",
        ));
    }
    for values in [
        &asset.compatibility.operating_systems,
        &asset.compatibility.architectures,
    ] {
        if values.len() > COMPATIBILITY_VALUES_MAX
            || values
                .iter()
                .any(|value| value.is_empty() || value.len() > 32)
            || values.windows(2).any(|pair| {
                let Some([left, right]) = pair.first_chunk::<2>() else {
                    return false;
                };
                left >= right
            })
        {
            return Err(manifest_validation(
                "asset compatibility list is invalid, duplicated, or unsorted",
            ));
        }
    }
    Ok(())
}

fn validate_asset_notices(asset: &AssetManifestEntry, allow_file: bool) -> Result<(), ShellError> {
    if asset.notices.len() > ASSET_NOTICES_MAX {
        return Err(resource_limit(
            "asset license notices",
            ASSET_NOTICES_MAX,
            asset.notices.len(),
        ));
    }
    if asset.logical_name == "command-model" {
        if asset.notices.is_empty() {
            return Ok(());
        }
        return Err(manifest_validation(
            "command model license is embedded in its archive, not a sidecar",
        ));
    }
    if asset.logical_name != "completion-database" || asset.notices.len() != 1 {
        return Err(manifest_validation(
            "completion database must retain exactly one Carapace license notice",
        ));
    }
    let notice = asset.notices.first().ok_or_else(|| {
        manifest_validation("completion database is missing its required license notice")
    })?;
    let text_bytes = notice.text.as_bytes();
    let observed_size = u64::try_from(text_bytes.len()).unwrap_or(u64::MAX);
    if notice.name != "Carapace"
        || notice.spdx_license != "MIT"
        || text_bytes.is_empty()
        || text_bytes.len() > ASSET_NOTICE_BYTES_MAX
        || notice.byte_size != observed_size
        || notice.sha256.len() != 64
        || !notice
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || format!("{:x}", Sha256::digest(text_bytes)) != notice.sha256
        || notice.file != format!("quirl-carapace-license-{}.txt", notice.sha256)
    {
        return Err(manifest_validation(
            "completion database Carapace license notice is invalid",
        ));
    }
    let scheme_ok =
        notice.url.starts_with("https://") || (allow_file && notice.url.starts_with("file://"));
    if notice.url.len() > URL_BYTES_MAX
        || !scheme_ok
        || !notice.url.ends_with(&format!("/{}", notice.file))
    {
        return Err(manifest_validation(
            "runtime asset notice URL is invalid or not content-addressed",
        ));
    }
    Ok(())
}

fn content_addressed_file(
    logical_name: &str,
    quirl_version: &str,
    sha256: &str,
) -> Result<String, ShellError> {
    let (stem, extension) = match logical_name {
        "completion-database" => ("quirl-completion-database", "sqlite3"),
        "command-model" => ("quirl-command-model", "tar"),
        _ => return Err(manifest_validation("unknown runtime asset logical name")),
    };
    Ok(format!("{stem}-v{quirl_version}-{sha256}.{extension}"))
}

fn validate_compatibility(asset: &AssetManifestEntry) -> Result<(), AssetFailure> {
    let compatible_version =
        asset.compatibility.quirl_version_requirement == format!("={}", env!("CARGO_PKG_VERSION"));
    let compatible_os = asset.compatibility.operating_systems.is_empty()
        || asset
            .compatibility
            .operating_systems
            .iter()
            .any(|value| value == env::consts::OS);
    let compatible_arch = asset.compatibility.architectures.is_empty()
        || asset
            .compatibility
            .architectures
            .iter()
            .any(|value| value == env::consts::ARCH);
    if compatible_version && compatible_os && compatible_arch {
        return Ok(());
    }
    Err(AssetFailure::permanent(
        ShellError::new(
            ErrorCode::Validation,
            format!(
                "runtime asset {} is incompatible with this Quirl",
                asset.logical_name
            ),
        )
        .with_context(format!(
            "required Quirl: {}; current Quirl: {}; current platform: {}/{}",
            asset.compatibility.quirl_version_requirement,
            env!("CARGO_PKG_VERSION"),
            env::consts::OS,
            env::consts::ARCH
        ))
        .with_help("Install a compatible Quirl release or publish a corrected asset manifest"),
    ))
}

fn load_manifest(
    source: ManifestSource,
    downloader: &dyn Downloader,
    cancelled: Arc<AtomicBool>,
) -> Result<AssetManifest, ShellError> {
    let bytes = match source {
        ManifestSource::File(path) => read_bounded_file(&path, MANIFEST_BYTES_MAX)?,
        ManifestSource::Url(url) => {
            if url.len() > URL_BYTES_MAX || !url.starts_with("https://") {
                return Err(manifest_validation(
                    "asset manifest URL must be bounded absolute HTTPS",
                ));
            }
            let mut reader = downloader.open(&url, cancelled)?;
            read_bounded(&mut reader, MANIFEST_BYTES_MAX, "asset manifest")?
        }
    };
    serde_json::from_slice(&bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "could not parse the runtime asset manifest",
        )
        .with_context(error.to_string())
        .with_help("Publish a versioned JSON manifest with no unknown fields")
    })
}

/// Try each candidate manifest source in order, returning the first that
/// fetches and parses successfully, plus whether that source was local
/// (which in turn permits `file://` asset payload URLs downstream).
///
/// Only a fetch/parse-level failure falls through to the next candidate; a
/// successfully-parsed-but-invalid manifest is a hard error so a real bug at
/// one mirror is never silently papered over by trying another.
fn resolve_manifest(
    sources: &[ManifestSource],
    downloader: &dyn Downloader,
    cancelled: &Arc<AtomicBool>,
) -> Result<(AssetManifest, bool), ShellError> {
    let mut last_error = None;
    for source in sources {
        match load_manifest(source.clone(), downloader, Arc::clone(cancelled)) {
            Ok(manifest) => return Ok((manifest, matches!(source, ManifestSource::File(_)))),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| manifest_validation("no manifest source was configured")))
}

fn read_receipt(path: &Path) -> Result<InstalledReceipt, ShellError> {
    let bytes = read_bounded_file(path, RECEIPT_BYTES_MAX)?;
    let receipt: InstalledReceipt = serde_json::from_slice(&bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "could not parse an installed asset receipt",
        )
        .with_context(error.to_string())
        .with_help("Run `quirl assets retry` to install a fresh verified receipt")
    })?;
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION {
        return Err(manifest_validation(
            "unsupported installed asset receipt version",
        ));
    }
    // Already-installed content is independently sha256-verified against its
    // payload on disk regardless of what `url` says; the field is read back
    // here purely as provenance, not re-fetched, so a local-sourced receipt
    // stays readable on every later `status`/startup check.
    validate_manifest_entry(&receipt.asset, true)?;
    Ok(receipt)
}

fn read_retry_state(path: &Path) -> Result<RetryState, ShellError> {
    if !path_exists(path)? {
        return Ok(RetryState::new());
    }
    let bytes = read_bounded_file(path, STATE_BYTES_MAX)?;
    let state: RetryState = serde_json::from_slice(&bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "could not parse runtime asset retry state",
        )
        .with_context(error.to_string())
        .with_help("Run `quirl assets retry` to replace the bounded retry state")
    })?;
    if state.schema_version != RETRY_STATE_SCHEMA_VERSION || state.entries.len() > RETRY_ENTRIES_MAX
    {
        return Err(manifest_validation("invalid runtime asset retry state"));
    }
    Ok(state)
}

fn write_retry_state(path: &Path, state: &RetryState) -> Result<(), ShellError> {
    if state.entries.len() > RETRY_ENTRIES_MAX {
        return Err(resource_limit(
            "retry entries",
            RETRY_ENTRIES_MAX,
            state.entries.len(),
        ));
    }
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "could not encode runtime asset retry state",
        )
        .with_context(error.to_string())
        .with_help("Report this Quirl asset-state defect")
    })?;
    write_atomic(path, &bytes, STATE_BYTES_MAX)
}

fn record_failure(
    retry: &mut RetryState,
    asset: &AssetManifestEntry,
    manifest_identity: String,
    failure: &AssetFailure,
    now_ms: u64,
) -> Result<(), ShellError> {
    if !retry.entries.contains_key(&asset.logical_name) && retry.entries.len() == RETRY_ENTRIES_MAX
    {
        return Err(resource_limit(
            "retry entries",
            RETRY_ENTRIES_MAX,
            retry.entries.len().saturating_add(1),
        ));
    }
    let prior_attempts = retry
        .entries
        .get(&asset.logical_name)
        .filter(|entry| entry.manifest_identity == manifest_identity)
        .map_or(0, |entry| entry.attempts);
    let attempts = prior_attempts.saturating_add(1).min(RETRY_ATTEMPTS_MAX);
    let disposition = if failure.permanent || attempts == RETRY_ATTEMPTS_MAX {
        RetryDisposition::Permanent
    } else {
        RetryDisposition::Transient
    };
    let next_retry_unix_ms = if disposition == RetryDisposition::Permanent {
        u64::MAX
    } else {
        now_ms.saturating_add(retry_delay_ms(attempts))
    };
    retry.entries.insert(
        asset.logical_name.clone(),
        RetryEntry {
            manifest_identity,
            attempts,
            next_retry_unix_ms,
            disposition,
            last_error: truncate_utf8(&failure.error.message, LAST_ERROR_BYTES_MAX),
        },
    );
    Ok(())
}

fn record_manifest_failure(
    retry: &mut RetryState,
    source_identity: String,
    error: &ShellError,
    now_ms: u64,
) -> Result<(), ShellError> {
    let prior_attempts = retry
        .entries
        .get(MANIFEST_RETRY_KEY)
        .filter(|entry| entry.manifest_identity == source_identity)
        .map_or(0, |entry| entry.attempts);
    let attempts = prior_attempts.saturating_add(1).min(RETRY_ATTEMPTS_MAX);
    let permanent_error = matches!(error.code, ErrorCode::Validation | ErrorCode::ResourceLimit);
    let disposition = if permanent_error || attempts == RETRY_ATTEMPTS_MAX {
        RetryDisposition::Permanent
    } else {
        RetryDisposition::Transient
    };
    retry.entries.insert(
        MANIFEST_RETRY_KEY.to_owned(),
        RetryEntry {
            manifest_identity: source_identity,
            attempts,
            next_retry_unix_ms: if disposition == RetryDisposition::Permanent {
                u64::MAX
            } else {
                now_ms.saturating_add(retry_delay_ms(attempts))
            },
            disposition,
            last_error: truncate_utf8(&error.message, LAST_ERROR_BYTES_MAX),
        },
    );
    Ok(())
}

fn retry_delay_ms(attempts: u8) -> u64 {
    let exponent = u32::from(attempts.saturating_sub(1)).min(20);
    let base = RETRY_BASE_DELAY_MS
        .saturating_mul(1_u64.checked_shl(exponent).unwrap_or(u64::MAX))
        .min(RETRY_DELAY_MS_MAX);
    let jitter_max = base / 4;
    let mut random = [0_u8; 8];
    let jitter = if getrandom::fill(&mut random).is_ok() {
        u64::from_le_bytes(random)
            .checked_rem(jitter_max.saturating_add(1))
            .unwrap_or_default()
    } else {
        unix_time_ms()
            .checked_rem(jitter_max.saturating_add(1))
            .unwrap_or_default()
    };
    base.saturating_add(jitter).min(RETRY_DELAY_MS_MAX)
}

fn status_report() -> Result<AssetStatusReport, ShellError> {
    let paths = AssetPaths::discover()?;
    status_report_with_paths(paths)
}

fn status_report_with_paths(paths: AssetPaths) -> Result<AssetStatusReport, ShellError> {
    let retry = read_retry_state(&paths.state).unwrap_or_else(|_| RetryState::new());
    let mut assets = Vec::with_capacity(REQUIRED_ASSETS.len());
    for (logical_name, _, _, _) in REQUIRED_ASSETS {
        let receipt_path = paths.data.join(logical_name).join("current.json");
        let retry_entry = retry.entries.get(logical_name).cloned();
        match read_receipt(&receipt_path) {
            Ok(receipt) => {
                let validation = current_asset_path_in(&paths, logical_name);
                assets.push(AssetStatus {
                    logical_name: logical_name.to_owned(),
                    installed: true,
                    valid: validation.is_ok(),
                    release_version: Some(receipt.release_version),
                    format_version: Some(receipt.asset.format_version),
                    byte_size: Some(receipt.asset.byte_size),
                    retry: retry_entry,
                    diagnostic: validation.err().map(|error| {
                        format!(
                            "{}; run `quirl assets retry` after correcting the asset source",
                            error.message
                        )
                    }),
                });
            }
            Err(error) => assets.push(AssetStatus {
                logical_name: logical_name.to_owned(),
                installed: false,
                valid: false,
                release_version: None,
                format_version: None,
                byte_size: None,
                retry: retry_entry,
                diagnostic: Some(if error.code == ErrorCode::Io {
                    format!(
                        "{logical_name} is not installed; run `quirl assets update` or `quirl assets retry`"
                    )
                } else {
                    error.message
                }),
            }),
        }
    }
    assets.sort_by(|left, right| left.logical_name.cmp(&right.logical_name));
    let degraded = assets.iter().any(|asset| !asset.valid);
    Ok(AssetStatusReport {
        schema_version: STATUS_SCHEMA_VERSION,
        data_directory: paths.data,
        cache_directory: paths.cache,
        degraded,
        assets,
    })
}

fn present_status(
    report: &AssetStatusReport,
    format: AssetsOutputFormat,
) -> Result<(), ShellError> {
    match format {
        AssetsOutputFormat::Json => print_json(report),
        AssetsOutputFormat::Text => {
            print!("{}", render_status_text(report));
            Ok(())
        }
    }
}

fn render_status_text(report: &AssetStatusReport) -> String {
    let mut output = format!(
        "runtime assets: {} ({})\n",
        if report.degraded { "degraded" } else { "ready" },
        escape_terminal_controls(&report.data_directory.display().to_string())
    );
    for asset in &report.assets {
        output.push_str(&format!(
            "  {}: {}\n",
            escape_terminal_controls(&asset.logical_name),
            if asset.valid { "ready" } else { "unavailable" }
        ));
        if asset.logical_name == "command-model" {
            output.push_str(
                "    used to build local semantic embeddings for AI search and ranking; inspect model details with `quirl ai status`\n",
            );
        }
        if let Some(diagnostic) = &asset.diagnostic {
            output.push_str(&format!(
                "    diagnostic: {}\n",
                escape_terminal_controls(diagnostic)
            ));
        }
        if let Some(retry) = &asset.retry {
            let disposition = match retry.disposition {
                RetryDisposition::Transient => "transient",
                RetryDisposition::Permanent => "permanent",
            };
            let next = if retry.next_retry_unix_ms == u64::MAX {
                "manual `quirl assets retry` required".to_owned()
            } else {
                format!("Unix time {} ms", retry.next_retry_unix_ms)
            };
            output.push_str(&format!(
                "    retry: {disposition}; attempts: {}; next: {next}\n",
                retry.attempts
            ));
            output.push_str(&format!(
                "    last error: {}\n",
                escape_terminal_controls(&retry.last_error)
            ));
        }
    }
    if report.degraded {
        output.push_str("  core shell features remain available in degraded mode\n");
    }
    output
}

fn present_update(
    report: &AssetUpdateReport,
    format: AssetsOutputFormat,
) -> Result<(), ShellError> {
    match format {
        AssetsOutputFormat::Json => print_json(report),
        AssetsOutputFormat::Text => {
            println!(
                "runtime assets for {}: {} installed, {} current, {} deferred, {} failed",
                escape_terminal_controls(&report.manifest_release_version),
                report.installed,
                report.current,
                report.deferred,
                report.failed
            );
            for result in &report.results {
                println!(
                    "  {}: {}{}",
                    escape_terminal_controls(&result.logical_name),
                    result.state,
                    result
                        .message
                        .as_deref()
                        .map(|message| format!(" ({})", escape_terminal_controls(message)))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
    }
}

fn print_json(value: &impl Serialize) -> Result<(), ShellError> {
    let json = serde_json::to_string_pretty(value).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "could not encode runtime asset output",
        )
        .with_context(error.to_string())
        .with_help("Retry with text output and report this serialization defect")
    })?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}

/// Resolve where to look for the runtime asset manifest.
///
/// `explicit_file` is the `--manifest` CLI flag, when given. Otherwise:
/// `QUIRL_ASSET_MANIFEST_FILE` (a local path, for a fully offline dev loop —
/// this is what lets the interactive session's own background checker, which
/// never goes through the `assets` subcommand, pick up a local build too),
/// then `QUIRL_ASSET_MANIFEST_URL` (a single full override, unchanged from
/// its previous behavior), then the built-in candidate list: quirl.dev
/// first, quirl.vercel.app as a same-deployment fallback if quirl.dev's DNS
/// specifically is the problem, then GitHub Releases (already published
/// there for Homebrew/binary releases regardless, so this is free
/// resilience if both website hosts are ever unreachable).
fn manifest_candidates(explicit_file: Option<PathBuf>) -> Vec<ManifestSource> {
    manifest_candidates_from(
        explicit_file,
        env::var_os("QUIRL_ASSET_MANIFEST_FILE").map(PathBuf::from),
        env::var("QUIRL_ASSET_MANIFEST_URL").ok(),
    )
}

/// Pure resolution logic behind [`manifest_candidates`], taking the
/// already-read environment as parameters so it's directly testable without
/// mutating real process-global env state.
fn manifest_candidates_from(
    explicit_file: Option<PathBuf>,
    env_file: Option<PathBuf>,
    env_url: Option<String>,
) -> Vec<ManifestSource> {
    if let Some(path) = explicit_file {
        return vec![ManifestSource::File(path)];
    }
    if let Some(path) = env_file {
        return vec![ManifestSource::File(path)];
    }
    if let Some(url) = env_url {
        return vec![ManifestSource::Url(url)];
    }
    vec![
        ManifestSource::Url(format!(
            "https://quirl.dev/reference/v{}/asset-manifest-v2.json",
            env!("CARGO_PKG_VERSION")
        )),
        ManifestSource::Url(format!(
            "https://quirl.vercel.app/reference/v{}/asset-manifest-v2.json",
            env!("CARGO_PKG_VERSION")
        )),
        ManifestSource::Url(format!(
            "https://github.com/niklas-heer/quirl/releases/download/v{0}/asset-manifest-v2.json",
            env!("CARGO_PKG_VERSION")
        )),
    ]
}

fn asset_data_directory() -> Result<PathBuf, ShellError> {
    if let Some(path) = env::var_os("QUIRL_ASSET_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    let root = env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let root = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let root = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    root.map(|root| root.join("quirl/assets")).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "runtime asset data directory is unavailable",
        )
        .with_help("Set QUIRL_ASSET_DATA_DIR to a private writable directory")
    })
}

fn asset_cache_directory() -> Result<PathBuf, ShellError> {
    if let Some(path) = env::var_os("QUIRL_ASSET_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    let root = env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library/Caches"));
    #[cfg(target_os = "windows")]
    let root = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let root = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")));
    root.map(|root| root.join("quirl/assets")).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "runtime asset cache directory is unavailable",
        )
        .with_help("Set QUIRL_ASSET_CACHE_DIR to a private writable directory")
    })
}

struct TemporaryDownload {
    path: PathBuf,
    file: Option<File>,
    installed: bool,
}

impl TemporaryDownload {
    fn create(parent: &Path) -> Result<Self, ShellError> {
        for _ in 0..TEMPORARY_ATTEMPTS_MAX {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".asset-download-{}-{sequence}", std::process::id()));
            match open_new_private_file(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        installed: false,
                    });
                }
                Err(error)
                    if error
                        .details
                        .context
                        .iter()
                        .any(|value| value.contains("exists")) => {}
                Err(error) => return Err(error),
            }
        }
        Err(resource_limit(
            "temporary asset names",
            TEMPORARY_ATTEMPTS_MAX,
            TEMPORARY_ATTEMPTS_MAX,
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn take_file(&mut self) -> Result<File, ShellError> {
        self.file.take().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "temporary runtime asset file is unavailable")
                .with_help("Retry the runtime asset transaction")
        })
    }
}

impl Drop for TemporaryDownload {
    fn drop(&mut self) {
        if !self.installed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn open_new_private_file(path: &Path) -> Result<File, ShellError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .map_err(|error| asset_io_error("create", path, error))
}

fn create_private_directory(path: &Path) -> Result<(), ShellError> {
    match fs::symlink_metadata(path) {
        Ok(_) => return admit_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(asset_io_error("inspect directory", path, error)),
    }
    let parent = path.parent().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "runtime asset directory has no parent",
        )
        .with_help("Use a nested platform data or cache directory")
    })?;
    if parent != path {
        create_private_directory(parent)?;
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(false);
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => admit_directory(path),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => admit_directory(path),
        Err(error) => Err(asset_io_error("create directory", path, error)),
    }
}

fn admit_directory(path: &Path) -> Result<(), ShellError> {
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|error| asset_io_error("inspect directory", path, error))?;
    if !path_metadata.file_type().is_dir() || path_metadata.file_type().is_symlink() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "runtime asset directory {} is redirected or not a directory",
                path.display()
            ),
        )
        .with_help("Replace it with a private real directory and retry"));
    }
    #[cfg(unix)]
    {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_DIRECTORY);
        let directory = options
            .open(path)
            .map_err(|error| asset_io_error("open directory", path, error))?;
        let file_metadata = directory
            .metadata()
            .map_err(|error| asset_io_error("inspect directory", path, error))?;
        if !file_metadata.file_type().is_dir()
            || path_metadata.dev() != file_metadata.dev()
            || path_metadata.ino() != file_metadata.ino()
        {
            return Err(manifest_validation(
                "runtime asset directory changed during admission",
            ));
        }
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8], bytes_max: usize) -> Result<(), ShellError> {
    if bytes.is_empty() || bytes.len() > bytes_max {
        return Err(resource_limit(
            "atomic document bytes",
            bytes_max,
            bytes.len(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "asset state path has no parent directory",
        )
        .with_help("Use a nested Quirl asset directory")
    })?;
    create_private_directory(parent)?;
    if path_exists(path)? {
        let expected = read_bounded_file(path, bytes_max)?;
        return replace_file_atomically(path, &expected, bytes, AtomicReplaceOptions { bytes_max });
    }
    let mut temporary = TemporaryDownload::create(parent)?;
    {
        let mut file = temporary.take_file()?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| asset_io_error("write", temporary.path(), error))?;
    }
    admit_directory(parent)?;
    fs::rename(temporary.path(), path).map_err(|error| asset_io_error("install", path, error))?;
    temporary.installed = true;
    sync_directory(parent)
}

fn read_bounded_file(path: &Path, bytes_max: usize) -> Result<Vec<u8>, ShellError> {
    let (mut file, metadata) = open_existing_regular(path)?;
    if metadata.len() > u64::try_from(bytes_max).unwrap_or(u64::MAX) {
        return Err(resource_limit(
            "asset document bytes",
            bytes_max,
            metadata.len(),
        ));
    }
    read_bounded(&mut file, bytes_max, "asset document")
}

fn read_bounded_file_controlled(
    path: &Path,
    bytes_max: usize,
    cancelled: &AtomicBool,
    started: Instant,
) -> Result<Vec<u8>, ShellError> {
    let (mut file, metadata) = open_existing_regular(path)?;
    if metadata.len() > u64::try_from(bytes_max).unwrap_or(u64::MAX) {
        return Err(resource_limit(
            "asset document bytes",
            bytes_max,
            metadata.len(),
        ));
    }
    let capacity = usize::try_from(metadata.len())
        .unwrap_or(bytes_max)
        .min(bytes_max);
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        admission_checkpoint(cancelled, started).map_err(|failure| failure.error)?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| asset_io_error("read", path, error))?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > bytes_max {
            return Err(resource_limit(
                "asset document bytes",
                bytes_max,
                bytes.len().saturating_add(count),
            ));
        }
        bytes.extend_from_slice(buffer.get(..count).unwrap_or_default());
    }
    Ok(bytes)
}

fn admission_checkpoint(cancelled: &AtomicBool, started: Instant) -> Result<(), AssetFailure> {
    if cancelled.load(Ordering::Acquire) {
        return Err(AssetFailure::transient(
            ShellError::new(
                ErrorCode::ResourceLimit,
                "runtime asset admission was cancelled",
            )
            .with_help("Run `quirl assets retry` when ready"),
        ));
    }
    if started.elapsed() > DOWNLOAD_DEADLINE {
        return Err(AssetFailure::transient(
            ShellError::new(
                ErrorCode::ResourceLimit,
                "runtime asset admission exceeded its wall deadline",
            )
            .with_context(format!("limit: {} ms", DOWNLOAD_DEADLINE.as_millis()))
            .with_help("Check local storage and retry the bounded asset update"),
        ));
    }
    Ok(())
}

fn open_existing_regular(path: &Path) -> Result<(File, fs::Metadata), ShellError> {
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| asset_io_error("inspect", path, error))?;
    if !path_metadata.file_type().is_file() {
        return Err(manifest_validation(
            "runtime asset input is not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .map_err(|error| asset_io_error("open", path, error))?;
    let file_metadata = file
        .metadata()
        .map_err(|error| asset_io_error("inspect", path, error))?;
    if !file_metadata.file_type().is_file() {
        return Err(manifest_validation(
            "runtime asset input changed away from a regular file",
        ));
    }
    #[cfg(unix)]
    if path_metadata.dev() != file_metadata.dev()
        || path_metadata.ino() != file_metadata.ino()
        || file_metadata.nlink() != 1
    {
        return Err(manifest_validation(
            "runtime asset input changed during admission or has hard-link aliases",
        ));
    }
    Ok((file, file_metadata))
}

fn read_bounded(
    reader: &mut dyn Read,
    bytes_max: usize,
    label: &str,
) -> Result<Vec<u8>, ShellError> {
    let mut bytes = Vec::with_capacity(bytes_max.min(64 * 1024));
    let mut limited = reader.take(
        u64::try_from(bytes_max)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    );
    limited.read_to_end(&mut bytes).map_err(|error| {
        ShellError::new(ErrorCode::Io, format!("could not read {label}"))
            .with_context(error.to_string())
            .with_help("Check the asset source and retry")
    })?;
    if bytes.len() > bytes_max {
        return Err(resource_limit(label, bytes_max, bytes.len()));
    }
    Ok(bytes)
}

fn path_exists(path: &Path) -> Result<bool, ShellError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(asset_io_error("inspect", path, error)),
    }
}

fn sync_directory(path: &Path) -> Result<(), ShellError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| asset_io_error("sync directory", path, error))?;
    Ok(())
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
    let mut buffer = vec![0_u8; DOWNLOAD_BUFFER_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                let _ = sender.send(DownloadMessage::End);
                return;
            }
            Ok(count) => {
                let Some(chunk) = buffer.get(..count) else {
                    let _ = sender.send(DownloadMessage::Error(
                        "HTTPS reader returned an invalid byte count".to_owned(),
                    ));
                    return;
                };
                if sender.send(DownloadMessage::Data(chunk.to_vec())).is_err() {
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

enum DownloadMessage {
    Data(Vec<u8>),
    End,
    Error(String),
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
                    "runtime asset download cancelled",
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
                        "bounded runtime asset HTTPS reader disconnected",
                    ));
                }
            }
        }
    }
}

fn manifest_identity(
    manifest: &AssetManifest,
    asset: &AssetManifestEntry,
    source_identity: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"quirl-asset-retry-identity-v1");
    for value in [
        manifest.schema_version.to_string(),
        manifest.quirl_version.clone(),
        source_identity.to_owned(),
        asset.logical_name.clone(),
        asset.file.clone(),
        asset.format.clone(),
        asset.format_version.to_string(),
        asset.byte_size.to_string(),
        asset.sha256.clone(),
        asset.url.clone(),
        asset.compatibility.quirl_version_requirement.clone(),
        asset.compatibility.operating_systems.join("\n"),
        asset.compatibility.architectures.join("\n"),
        asset.source_revision.clone(),
        asset.source_date_epoch.to_string(),
        asset
            .notices
            .iter()
            .map(|notice| {
                format!(
                    "{}\n{}\n{}\n{}\n{}\n{}\n{}",
                    notice.name,
                    notice.spdx_license,
                    notice.file,
                    notice.byte_size,
                    notice.sha256,
                    notice.url,
                    notice.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n---\n"),
    ] {
        hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn integrity_failure(
    asset: &AssetManifestEntry,
    observed: u64,
    observed_sha256: Option<&str>,
) -> AssetFailure {
    AssetFailure::permanent(
        ShellError::new(
            ErrorCode::Validation,
            format!("runtime asset {} failed integrity verification", asset.logical_name),
        )
        .with_context(format!(
            "expected bytes: {}; observed bytes: {observed}; expected SHA-256: {}; observed SHA-256: {}",
            asset.byte_size,
            asset.sha256,
            observed_sha256.unwrap_or("unavailable")
        ))
        .with_help("Publish corrected immutable bytes, then run `quirl assets retry`"),
    )
}

fn manifest_validation(message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help(
        "This indicates a malformed Quirl release manifest; report it and run `quirl assets retry` once fixed",
    )
}

fn resource_limit<T: std::fmt::Display>(
    label: &str,
    limit: impl std::fmt::Display,
    observed: T,
) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("runtime assets exceeded the {label} limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Publish a smaller bounded asset set and retry")
}

fn asset_io_error(action: &str, path: &Path, error: io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not {action} runtime asset data {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Check Quirl asset-directory permissions and retry")
}

fn asset_lock_busy() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "runtime asset update lock remained busy",
    )
    .with_help("Wait for the other Quirl process and retry")
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn truncate_utf8(value: &str, bytes_max: usize) -> String {
    if value.len() <= bytes_max {
        return value.to_owned();
    }
    let mut end = bytes_max;
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.get(..end).map_or_else(String::new, str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, io::Cursor, sync::atomic::AtomicUsize};

    const TEST_NATIVE_DATABASE: &[u8] =
        include_bytes!("../../../catalog/generated/catalog.sqlite3");

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "quirl-assets-test-{name}-{}-{}",
                std::process::id(),
                NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }

        fn paths(&self) -> AssetPaths {
            AssetPaths {
                data: self.0.join("data"),
                cache: self.0.join("cache"),
                state: self.0.join("cache/retry-state-v1.json"),
            }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct FakeDownloader {
        responses: Mutex<BTreeMap<String, Vec<u8>>>,
        opens: AtomicUsize,
    }

    impl FakeDownloader {
        fn new(responses: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                opens: AtomicUsize::new(0),
            }
        }
    }

    impl Downloader for FakeDownloader {
        fn open(
            &self,
            url: &str,
            _cancelled: Arc<AtomicBool>,
        ) -> Result<Box<dyn Read + Send>, ShellError> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            let bytes = self
                .responses
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .ok_or_else(|| {
                    ShellError::new(ErrorCode::Io, "fake asset response is unavailable")
                        .with_help("Add the URL to the offline fake downloader")
                })?;
            Ok(Box::new(Cursor::new(bytes)))
        }
    }

    struct CancellingDownloader {
        bytes: Vec<u8>,
    }

    impl Downloader for CancellingDownloader {
        fn open(
            &self,
            _url: &str,
            cancelled: Arc<AtomicBool>,
        ) -> Result<Box<dyn Read + Send>, ShellError> {
            Ok(Box::new(CancellingReader {
                bytes: self.bytes.clone(),
                offset: 0,
                cancelled,
            }))
        }
    }

    struct CancellingReader {
        bytes: Vec<u8>,
        offset: usize,
        cancelled: Arc<AtomicBool>,
    }

    impl Read for CancellingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.offset == self.bytes.len() {
                return Ok(0);
            }
            let remaining = self.bytes.len().saturating_sub(self.offset);
            let count = remaining.min(buffer.len()).min(4);
            let end = self.offset.saturating_add(count);
            buffer[..count].copy_from_slice(&self.bytes[self.offset..end]);
            self.offset = end;
            self.cancelled.store(true, Ordering::Release);
            Ok(count)
        }
    }

    fn entry(name: &str, bytes: &[u8]) -> AssetManifestEntry {
        let format = match name {
            "completion-database" => "sqlite3",
            "command-model" => "tar",
            _ => "test",
        };
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let file = content_addressed_file(name, env!("CARGO_PKG_VERSION"), &sha256)
            .unwrap_or_else(|_| "asset.bin".to_owned());
        let notices = if name == "completion-database" {
            let text = "MIT License\n\nCopyright (c) Carapace contributors\n".to_owned();
            let notice_sha256 = format!("{:x}", Sha256::digest(text.as_bytes()));
            let notice_file = format!("quirl-carapace-license-{notice_sha256}.txt");
            vec![AssetNotice {
                name: "Carapace".to_owned(),
                spdx_license: "MIT".to_owned(),
                file: notice_file.clone(),
                byte_size: u64::try_from(text.len()).unwrap(),
                sha256: notice_sha256,
                url: format!("https://example.invalid/{notice_file}"),
                text,
            }]
        } else {
            Vec::new()
        };
        AssetManifestEntry {
            logical_name: name.to_owned(),
            file: file.clone(),
            format: format.to_owned(),
            format_version: 1,
            byte_size: u64::try_from(bytes.len()).unwrap(),
            sha256,
            url: format!("https://example.invalid/{file}"),
            compatibility: AssetCompatibility {
                quirl_version_requirement: format!("={}", env!("CARGO_PKG_VERSION")),
                operating_systems: vec![env::consts::OS.to_owned()],
                architectures: vec![env::consts::ARCH.to_owned()],
            },
            source_revision: "0".repeat(40),
            source_date_epoch: 1,
            notices,
        }
    }

    fn command_model_tar() -> Vec<u8> {
        let mut archive = Vec::new();
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap();
        let model_root = workspace
            .join("models/quirl-command-v3-int8")
            .join(crate::ai_bootstrap::MODEL_REVISION);
        for name in [
            "LICENSE",
            "README.md",
            "config.json",
            "model.safetensors",
            "quirl-model.json",
            "tokenizer.json",
        ] {
            let bytes = fs::read(model_root.join(name)).unwrap();
            let mut header = [0_u8; 512];
            let path = format!("command-model/{name}");
            header[..path.len()].copy_from_slice(path.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[108..116].copy_from_slice(b"0000000\0");
            header[116..124].copy_from_slice(b"0000000\0");
            let size = format!("{:011o}\0", bytes.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[148..156].fill(b' ');
            header[156] = b'0';
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            let checksum = header.iter().map(|byte| u64::from(*byte)).sum::<u64>();
            let checksum = format!("{checksum:06o}\0 ");
            header[148..156].copy_from_slice(checksum.as_bytes());
            archive.extend_from_slice(&header);
            archive.extend_from_slice(&bytes);
            archive.resize(archive.len().div_ceil(512).saturating_mul(512), 0);
        }
        archive.resize(archive.len().saturating_add(1024), 0);
        archive
    }

    #[test]
    fn valid_offline_download_installs_content_then_receipt() {
        let directory = TestDirectory::new("install");
        let paths = directory.paths();
        create_private_directory(&paths.data).unwrap();
        let bytes = TEST_NATIVE_DATABASE;
        let asset = entry("completion-database", bytes);
        let downloader = FakeDownloader::new([(asset.url.clone(), bytes.to_vec())]);
        let outcome = install_one(
            &paths,
            "0.1.0",
            &asset,
            &downloader,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed));
        let receipt = read_receipt(&paths.data.join("completion-database/current.json")).unwrap();
        validate_installed_payload(&paths.data.join("completion-database"), &receipt.asset)
            .unwrap();
    }

    #[test]
    fn invalid_asset_formats_leave_no_generation_and_preserve_the_current_asset() {
        let directory = TestDirectory::new("format-admission-cleanup");
        let paths = directory.paths();
        create_private_directory(&paths.data).unwrap();
        let current = entry("completion-database", TEST_NATIVE_DATABASE);
        let downloader =
            FakeDownloader::new([(current.url.clone(), TEST_NATIVE_DATABASE.to_vec())]);
        install_one(
            &paths,
            "0.1.0",
            &current,
            &downloader,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let asset_root = paths.data.join("completion-database");
        let receipt_path = asset_root.join("current.json");
        let original_receipt = fs::read(&receipt_path).unwrap();
        let unrelated = asset_root.join("unrelated.txt");
        fs::write(&unrelated, b"preserve this file").unwrap();

        // Each small payload has the correct digest but an invalid database
        // format. Repeated admission failures must not accumulate generations.
        for version in 0..6 {
            let bytes = format!("invalid database {version}").into_bytes();
            let invalid = entry("completion-database", &bytes);
            let downloader = FakeDownloader::new([(invalid.url.clone(), bytes)]);
            let failure = install_one(
                &paths,
                "0.1.1",
                &invalid,
                &downloader,
                false,
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err();
            assert!(failure.permanent);
            assert!(!asset_root.join(&invalid.sha256).exists());
            assert_eq!(fs::read(&receipt_path).unwrap(), original_receipt);
            assert_eq!(fs::read(&unrelated).unwrap(), b"preserve this file");
            validate_installed_payload(&asset_root, &current).unwrap();
            assert_eq!(fs::read_dir(&asset_root).unwrap().count(), 3);
        }
    }

    #[test]
    fn invalid_asset_admission_preserves_a_preexisting_hash_directory() {
        let directory = TestDirectory::new("format-admission-preexisting");
        let paths = directory.paths();
        let bytes = b"invalid database";
        let asset = entry("completion-database", bytes);
        let content_directory = paths.data.join(&asset.logical_name).join(&asset.sha256);
        create_private_directory(&content_directory).unwrap();
        let existing = content_directory.join("unrelated.txt");
        fs::write(&existing, b"do not remove an unowned generation").unwrap();
        let downloader = FakeDownloader::new([(asset.url.clone(), bytes.to_vec())]);
        assert!(
            install_one(
                &paths,
                "0.1.0",
                &asset,
                &downloader,
                false,
                &Arc::new(AtomicBool::new(false)),
            )
            .is_err()
        );
        assert_eq!(
            fs::read(&existing).unwrap(),
            b"do not remove an unowned generation"
        );
        assert_eq!(fs::read_dir(&content_directory).unwrap().count(), 1);
    }

    #[test]
    fn invalid_command_model_admission_cleans_staging_before_publication() {
        let directory = TestDirectory::new("format-admission-model");
        let paths = directory.paths();
        let bytes = b"invalid model archive";
        let asset = entry("command-model", bytes);
        let downloader = FakeDownloader::new([(asset.url.clone(), bytes.to_vec())]);
        assert!(
            install_one(
                &paths,
                "0.1.0",
                &asset,
                &downloader,
                false,
                &Arc::new(AtomicBool::new(false)),
            )
            .is_err()
        );
        let asset_root = paths.data.join(&asset.logical_name);
        assert_eq!(fs::read_dir(asset_root).unwrap().count(), 0);
    }

    #[test]
    fn a_fifth_generation_is_rejected_before_downloading_or_changing_existing_files() {
        let directory = TestDirectory::new("generation-capacity");
        let paths = directory.paths();
        let asset = entry("completion-database", TEST_NATIVE_DATABASE);
        let asset_root = paths.data.join(&asset.logical_name);
        create_private_directory(&asset_root).unwrap();
        for index in 0..ASSET_GENERATIONS_MAX {
            let hash = format!("{index:064x}");
            create_private_directory(&asset_root.join(&hash)).unwrap();
            fs::write(asset_root.join(&hash).join("payload"), hash).unwrap();
        }
        let receipt_path = asset_root.join("current.json");
        fs::write(&receipt_path, b"preserve the prior receipt").unwrap();
        let downloader = FakeDownloader::new([(asset.url.clone(), TEST_NATIVE_DATABASE.to_vec())]);
        let failure = install_one(
            &paths,
            "0.1.0",
            &asset,
            &downloader,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert_eq!(failure.error.code, ErrorCode::ResourceLimit);
        assert_eq!(downloader.opens.load(Ordering::Relaxed), 0);
        assert!(!asset_root.join(&asset.sha256).exists());
        assert_eq!(
            fs::read_dir(&asset_root).unwrap().count(),
            ASSET_GENERATIONS_MAX + 1
        );
        assert_eq!(
            fs::read(receipt_path).unwrap(),
            b"preserve the prior receipt"
        );
        for index in 0..ASSET_GENERATIONS_MAX {
            let hash = format!("{index:064x}");
            assert_eq!(
                fs::read(asset_root.join(&hash).join("payload")).unwrap(),
                hash.as_bytes()
            );
        }
        // Reusing an already retained identity consumes no additional slot.
        admit_generation_capacity(&asset_root, &format!("{:064x}", 0)).unwrap();
    }

    #[test]
    fn an_excessive_asset_directory_is_rejected_before_downloading() {
        let directory = TestDirectory::new("generation-directory-capacity");
        let paths = directory.paths();
        let asset = entry("completion-database", TEST_NATIVE_DATABASE);
        let asset_root = paths.data.join(&asset.logical_name);
        create_private_directory(&asset_root).unwrap();
        for index in 0..=ASSET_DIRECTORY_ENTRIES_MAX {
            fs::write(asset_root.join(format!("unrelated-{index}")), b"preserve").unwrap();
        }
        let downloader = FakeDownloader::new([(asset.url.clone(), TEST_NATIVE_DATABASE.to_vec())]);
        let failure = install_one(
            &paths,
            "0.1.0",
            &asset,
            &downloader,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert_eq!(failure.error.code, ErrorCode::ResourceLimit);
        assert_eq!(downloader.opens.load(Ordering::Relaxed), 0);
        assert_eq!(
            fs::read_dir(&asset_root).unwrap().count(),
            ASSET_DIRECTORY_ENTRIES_MAX + 1
        );
    }

    #[test]
    fn generation_cleanup_inspects_past_receipts_and_unrelated_entries() {
        let directory = TestDirectory::new("generation-scan");
        let root = directory.paths().data;
        create_private_directory(&root).unwrap();
        for index in 0..ASSET_DIRECTORY_ENTRIES_MAX - 3 {
            fs::write(root.join(format!("unrelated-{index}")), b"preserve").unwrap();
        }
        let current = "a".repeat(64);
        let previous = "b".repeat(64);
        let stale = "c".repeat(64);
        for name in [&current, &previous, &stale] {
            create_private_directory(&root.join(name)).unwrap();
            fs::write(root.join(name).join("payload"), name).unwrap();
        }
        cleanup_generations(&root, &current, Some(&previous)).unwrap();
        assert!(root.join(&current).is_dir());
        assert!(root.join(&previous).is_dir());
        assert!(!root.join(&stale).exists());
        for index in 0..ASSET_DIRECTORY_ENTRIES_MAX - 3 {
            assert_eq!(
                fs::read(root.join(format!("unrelated-{index}"))).unwrap(),
                b"preserve"
            );
        }
    }

    #[test]
    fn generation_cleanup_rejects_excess_directory_entries_before_deleting() {
        let directory = TestDirectory::new("generation-scan-bound");
        let root = directory.paths().data;
        create_private_directory(&root).unwrap();
        let stale = root.join("c".repeat(64));
        create_private_directory(&stale).unwrap();
        fs::write(stale.join("payload"), b"preserve until admission succeeds").unwrap();
        for index in 0..ASSET_DIRECTORY_ENTRIES_MAX {
            fs::write(root.join(format!("unrelated-{index}")), b"preserve").unwrap();
        }
        let error = cleanup_generations(&root, &"a".repeat(64), None).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(stale.is_dir());
        assert_eq!(
            fs::read_dir(&root).unwrap().count(),
            ASSET_DIRECTORY_ENTRIES_MAX + 1
        );
    }

    #[test]
    fn bounded_command_model_tar_is_admitted_and_expanded() {
        let directory = TestDirectory::new("model-tar");
        let paths = directory.paths();
        create_private_directory(&paths.data).unwrap();
        let bytes = command_model_tar();
        let asset = entry("command-model", &bytes);
        let downloader = FakeDownloader::new([(asset.url.clone(), bytes)]);
        install_one(
            &paths,
            "0.1.0",
            &asset,
            &downloader,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        validate_command_model_directory(
            &paths
                .data
                .join("command-model")
                .join(asset.sha256)
                .join("expanded/command-model"),
        )
        .unwrap();
    }

    #[test]
    fn corrupt_replacement_preserves_existing_valid_receipt() {
        let directory = TestDirectory::new("preserve");
        let paths = directory.paths();
        create_private_directory(&paths.data).unwrap();
        let first = TEST_NATIVE_DATABASE;
        let first_asset = entry("completion-database", first);
        let first_downloader = FakeDownloader::new([(first_asset.url.clone(), first.to_vec())]);
        install_one(
            &paths,
            "0.1.0",
            &first_asset,
            &first_downloader,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        let receipt_path = paths.data.join("completion-database/current.json");
        let before = fs::read(&receipt_path).unwrap();

        let expected = b"new payload";
        let next = entry("completion-database", expected);
        let corrupt = FakeDownloader::new([(next.url.clone(), b"truncated".to_vec())]);
        let failure = install_one(
            &paths,
            "0.1.1",
            &next,
            &corrupt,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(failure.permanent);
        assert_eq!(fs::read(&receipt_path).unwrap(), before);
        validate_installed_payload(&paths.data.join("completion-database"), &first_asset).unwrap();
    }

    #[test]
    fn unknown_manifest_fields_and_incompatible_assets_fail_permanently() {
        let mut manifest = single_asset_manifest(entry("completion-database", b"payload"));
        manifest
            .assets
            .push(entry("command-model", &command_model_tar()));
        validate_manifest(&manifest, false).unwrap();
        let completion = manifest
            .assets
            .iter()
            .find(|asset| asset.logical_name == "completion-database")
            .unwrap();
        let first_identity = manifest_identity(&manifest, completion, "provider-a");
        let mut changed = manifest.clone();
        changed
            .assets
            .iter_mut()
            .find(|asset| asset.logical_name == "completion-database")
            .unwrap()
            .url = "https://mirror.invalid/completion.sqlite3".to_owned();
        let changed_completion = changed
            .assets
            .iter()
            .find(|asset| asset.logical_name == "completion-database")
            .unwrap();
        assert_ne!(
            first_identity,
            manifest_identity(&changed, changed_completion, "provider-a")
        );
        assert_ne!(
            first_identity,
            manifest_identity(&manifest, completion, "provider-b")
        );

        let mut newer_source = manifest.clone();
        newer_source.assets[0].source_revision = "f".repeat(40);
        assert!(validate_manifest(&newer_source, false).is_ok());
        let newer_completion = newer_source
            .assets
            .iter()
            .find(|asset| asset.logical_name == "completion-database")
            .unwrap();
        assert_ne!(
            first_identity,
            manifest_identity(&newer_source, newer_completion, "provider-a")
        );

        let mut mutable_filename = manifest.clone();
        mutable_filename.assets[0].file = "quirl-completion-database-v0.1.0.sqlite3".to_owned();
        assert!(validate_manifest(&mutable_filename, false).is_err());
        let mut missing_notice = manifest.clone();
        missing_notice.assets[0].notices.clear();
        assert!(validate_manifest(&missing_notice, false).is_err());

        let json = br#"{
            "schema_version":2,"quirl_version":"0.1.0","assets":[],"provider":"github"
        }"#;
        assert!(serde_json::from_slice::<AssetManifest>(json).is_err());

        let mut asset = entry("model-bundle", b"payload");
        asset.compatibility.quirl_version_requirement = "=999.0.0".to_owned();
        assert!(validate_compatibility(&asset).unwrap_err().permanent);
    }

    #[test]
    fn manifest_candidates_follow_explicit_env_then_built_in_precedence() {
        let file = PathBuf::from("/tmp/local-manifest.json");
        assert!(matches!(
            manifest_candidates_from(Some(file.clone()), None, None).as_slice(),
            [ManifestSource::File(path)] if *path == file
        ));

        let env_file = PathBuf::from("/tmp/env-manifest.json");
        assert!(matches!(
            manifest_candidates_from(None, Some(env_file.clone()), Some("https://ignored.invalid/m.json".to_owned())).as_slice(),
            [ManifestSource::File(path)] if *path == env_file
        ));

        let candidates = manifest_candidates_from(
            None,
            None,
            Some("https://override.invalid/asset-manifest-v2.json".to_owned()),
        );
        assert!(matches!(
            candidates.as_slice(),
            [ManifestSource::Url(url)] if url == "https://override.invalid/asset-manifest-v2.json"
        ));

        let defaults = manifest_candidates_from(None, None, None);
        let urls = defaults
            .iter()
            .map(|source| match source {
                ManifestSource::Url(url) => url.clone(),
                ManifestSource::File(_) => panic!("default candidates must all be URLs"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            vec![
                format!(
                    "https://quirl.dev/reference/v{}/asset-manifest-v2.json",
                    env!("CARGO_PKG_VERSION")
                ),
                format!(
                    "https://quirl.vercel.app/reference/v{}/asset-manifest-v2.json",
                    env!("CARGO_PKG_VERSION")
                ),
                format!(
                    "https://github.com/niklas-heer/quirl/releases/download/v{}/asset-manifest-v2.json",
                    env!("CARGO_PKG_VERSION")
                ),
            ]
        );
    }

    fn single_asset_manifest(asset: AssetManifestEntry) -> AssetManifest {
        AssetManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            quirl_version: env!("CARGO_PKG_VERSION").to_owned(),
            assets: vec![asset],
        }
    }

    #[test]
    fn resolve_manifest_falls_through_an_unreachable_first_candidate() {
        let asset = entry("completion-database", b"payload");
        let manifest = single_asset_manifest(asset);
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let downloader = FakeDownloader::new([(
            "https://second.invalid/asset-manifest-v2.json".to_owned(),
            bytes,
        )]);
        let sources = vec![
            ManifestSource::Url("https://first.invalid/asset-manifest-v2.json".to_owned()),
            ManifestSource::Url("https://second.invalid/asset-manifest-v2.json".to_owned()),
        ];
        let (resolved, allow_file) =
            resolve_manifest(&sources, &downloader, &Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(resolved.quirl_version, manifest.quirl_version);
        assert!(!allow_file);
    }

    #[test]
    fn resolve_manifest_reports_no_local_trust_for_every_candidate_unreachable() {
        let downloader = FakeDownloader::new([]);
        let sources = vec![ManifestSource::Url(
            "https://first.invalid/asset-manifest-v2.json".to_owned(),
        )];
        assert!(
            resolve_manifest(&sources, &downloader, &Arc::new(AtomicBool::new(false))).is_err()
        );
    }

    #[test]
    fn resolve_manifest_trusts_local_files_only_from_a_local_source() {
        let directory = TestDirectory::new("resolve-local");
        let manifest_path = directory.0.join("manifest.json");
        let asset = entry("completion-database", b"payload");
        let manifest = single_asset_manifest(asset);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let sources = vec![ManifestSource::File(manifest_path)];
        let downloader = FakeDownloader::new([]);
        let (_, allow_file) =
            resolve_manifest(&sources, &downloader, &Arc::new(AtomicBool::new(false))).unwrap();
        assert!(allow_file);
    }

    #[test]
    fn local_asset_payload_is_installed_only_when_trusted() {
        let directory = TestDirectory::new("file-payload");
        let paths = directory.paths();
        create_private_directory(&paths.data).unwrap();
        let bytes = TEST_NATIVE_DATABASE;
        let mut asset = entry("completion-database", bytes);
        let payload_path = directory.0.join(&asset.file);
        fs::write(&payload_path, bytes).unwrap();
        asset.url = format!("file://{}", payload_path.display());
        let downloader = FakeDownloader::new([]);
        let cancelled = Arc::new(AtomicBool::new(false));

        assert!(
            install_one(&paths, "0.1.0", &asset, &downloader, false, &cancelled)
                .unwrap_err()
                .permanent
        );

        let outcome = install_one(&paths, "0.1.0", &asset, &downloader, true, &cancelled).unwrap();
        assert!(matches!(outcome, InstallOutcome::Installed));
        assert_eq!(downloader.opens.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cancelled_download_cleans_partial_file() {
        let directory = TestDirectory::new("cancel");
        let paths = directory.paths();
        create_private_directory(&paths.data).unwrap();
        let bytes = b"payload";
        let asset = entry("completion-database", bytes);
        let downloader = FakeDownloader::new([(asset.url.clone(), bytes.to_vec())]);
        let cancelled = Arc::new(AtomicBool::new(true));
        assert!(install_one(&paths, "0.1.0", &asset, &downloader, false, &cancelled).is_err());
        let root = paths.data.join("completion-database");
        let remaining = fs::read_dir(root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".asset-download-")
            })
            .count();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn cancellation_during_download_cleans_partial_file() {
        let directory = TestDirectory::new("cancel-in-progress");
        let paths = directory.paths();
        let bytes = b"payload";
        let asset = entry("completion-database", bytes);
        let downloader = CancellingDownloader {
            bytes: bytes.to_vec(),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        assert!(install_one(&paths, "0.1.0", &asset, &downloader, false, &cancelled).is_err());
        let asset_root = paths.data.join("completion-database");
        assert!(
            fs::read_dir(asset_root)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".asset-download-"))
        );
    }

    #[test]
    fn unsupported_or_oversized_contract_never_opens_downloader() {
        let directory = TestDirectory::new("preflight");
        let paths = directory.paths();
        let mut asset = entry("completion-database", b"payload");
        asset.format = "opaque".to_owned();
        let downloader = FakeDownloader::new([]);
        let failure = install_one(
            &paths,
            "0.1.0",
            &asset,
            &downloader,
            false,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
        assert!(failure.permanent);
        assert_eq!(downloader.opens.load(Ordering::Relaxed), 0);

        asset.format = "sqlite3".to_owned();
        asset.byte_size = COMPLETION_DATABASE_BYTES_MAX + 1;
        assert!(
            install_one(
                &paths,
                "0.1.0",
                &asset,
                &downloader,
                false,
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err()
            .permanent
        );
        assert_eq!(downloader.opens.load(Ordering::Relaxed), 0);
    }

    #[cfg(unix)]
    #[test]
    fn redirected_data_root_is_rejected_before_downloader_open() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new("redirected-root");
        let target = directory.0.join("redirect-target");
        fs::create_dir(&target).unwrap();
        let paths = AssetPaths {
            data: directory.0.join("data-link"),
            cache: directory.0.join("cache"),
            state: directory.0.join("cache/retry-state-v1.json"),
        };
        symlink(&target, &paths.data).unwrap();
        let bytes = TEST_NATIVE_DATABASE;
        let asset = entry("completion-database", bytes);
        let downloader = FakeDownloader::new([(asset.url.clone(), bytes.to_vec())]);
        assert!(
            install_one(
                &paths,
                "0.1.0",
                &asset,
                &downloader,
                false,
                &Arc::new(AtomicBool::new(false)),
            )
            .is_err()
        );
        assert_eq!(downloader.opens.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn status_always_reports_both_required_assets_and_deep_format_errors() {
        let directory = TestDirectory::new("status");
        let paths = directory.paths();
        let mut missing = status_report_with_paths(paths).unwrap();
        assert_eq!(missing.assets.len(), REQUIRED_ASSETS.len());
        assert!(missing.assets.iter().all(|asset| {
            asset
                .diagnostic
                .as_deref()
                .is_some_and(|message| message.contains("assets update"))
        }));
        missing.assets[0].retry = Some(RetryEntry {
            manifest_identity: "identity".to_owned(),
            attempts: 2,
            next_retry_unix_ms: u64::MAX,
            disposition: RetryDisposition::Permanent,
            last_error: "integrity failure".to_owned(),
        });
        let text = render_status_text(&missing);
        assert!(text.contains("quirl assets update"));
        assert!(text.contains("manual `quirl assets retry` required"));
        assert!(text.contains("attempts: 2"));
        assert!(text.contains("integrity failure"));

        let paths = directory.paths();
        let invalid = b"not sqlite";
        let asset = entry("completion-database", invalid);
        let asset_root = paths.data.join("completion-database");
        let content = asset_root.join(&asset.sha256);
        create_private_directory(&content).unwrap();
        fs::write(content.join("payload"), invalid).unwrap();
        let receipt = InstalledReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            release_version: "0.1.0".to_owned(),
            asset,
        };
        write_atomic(
            &asset_root.join("current.json"),
            &serde_json::to_vec_pretty(&receipt).unwrap(),
            RECEIPT_BYTES_MAX,
        )
        .unwrap();
        let report = status_report_with_paths(paths).unwrap();
        let completion = report
            .assets
            .iter()
            .find(|asset| asset.logical_name == "completion-database")
            .unwrap();
        assert!(completion.installed);
        assert!(!completion.valid);
        assert!(completion.diagnostic.is_some());
        let status_json = serde_json::to_value(&report).unwrap();
        let completion_json = status_json["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|asset| asset["logical_name"] == "completion-database")
            .unwrap();
        assert_eq!(completion_json["release_version"], "0.1.0");
        assert!(completion_json.get("quirl_version").is_none());
    }

    #[test]
    fn schema_one_receipt_and_machine_output_keep_release_version_fields() {
        let asset = entry("completion-database", TEST_NATIVE_DATABASE);
        let receipt = InstalledReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            release_version: "0.1.0".to_owned(),
            asset,
        };
        let receipt_json = serde_json::to_value(&receipt).unwrap();
        assert_eq!(receipt_json["release_version"], "0.1.0");
        assert!(receipt_json.get("quirl_version").is_none());

        let update = AssetUpdateReport {
            schema_version: STATUS_SCHEMA_VERSION,
            manifest_release_version: "0.1.0".to_owned(),
            installed: 0,
            current: 0,
            deferred: 0,
            failed: 0,
            results: Vec::new(),
        };
        let update_json = serde_json::to_value(&update).unwrap();
        assert_eq!(update_json["manifest_release_version"], "0.1.0");
        assert!(update_json.get("manifest_quirl_version").is_none());
    }

    #[test]
    fn retry_backoff_is_capped_and_permanent_after_attempt_limit() {
        for attempt in 1..=RETRY_ATTEMPTS_MAX {
            assert!(retry_delay_ms(attempt) <= RETRY_DELAY_MS_MAX);
        }
        let asset = entry("model-bundle", b"payload");
        let failure = AssetFailure::transient(
            ShellError::new(ErrorCode::Io, "offline").with_help("Reconnect and retry"),
        );
        let mut state = RetryState::new();
        for _ in 0..RETRY_ATTEMPTS_MAX {
            record_failure(&mut state, &asset, "identity".to_owned(), &failure, 1).unwrap();
        }
        assert_eq!(
            state.entries["model-bundle"].disposition,
            RetryDisposition::Permanent
        );

        let invalid_manifest = manifest_validation("invalid manifest");
        record_manifest_failure(
            &mut state,
            "release-manifest".to_owned(),
            &invalid_manifest,
            1,
        )
        .unwrap();
        assert_eq!(
            state.entries[MANIFEST_RETRY_KEY].disposition,
            RetryDisposition::Permanent
        );
    }
}
