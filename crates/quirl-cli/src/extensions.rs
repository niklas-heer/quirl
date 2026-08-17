use crate::lua_worker::{LuaWorkerCancellation as LuaCancellation, LuaWorkerRuntime as LuaRuntime};
use crate::{
    bounded_file::{read_optional_regular_file, read_regular_file, ReadFileOptions},
    extension_scheduler::{
        ExtensionScheduler, ExtensionSchedulerHandle, ExtensionWork, ExtensionWorkBatch,
        ExtensionWorkContext, WorkPriority,
    },
};
use quirl_catalog::{
    ArgumentKind, Catalog, CommandSpec, Effect, MAX_COMPLETION_QUERY_BYTES, MAX_COMPLETION_RESULTS,
};
#[cfg(test)]
use quirl_catalog::{Confidence, Provenance, ProvenanceInfo, Trust};
use quirl_core::{
    validate_contribution_set, ContributionKind, ContributionRegistration, ErrorCode,
    ExecutionEffect, ExecutionEffects, ExecutionInput, ExecutionOutcome, ExecutionOutput,
    ExecutionOutputTarget, ExecutionPlan, ExecutionRequest, ExecutionSource, ExtensionAction,
    ExtensionEvent, ExtensionEventData, ShellError, StructuredValue, StructuredValueKind,
    ValueInputContract, ValueOutputContract,
};
use quirl_lua::{
    CommandRegistration, ConfigStore, EventHandlerReport, LuaPolicy, LuaRunnerContext,
    PluginRegistrations, QuirlConfig, MAX_LUA_RUNNER_STREAM_VALUES,
};
use quirl_plugin::{
    doctor_plugin, normalize_plugin_commands, parse_plugin_manifest, validate_plugin_manifest,
    LockedPlugin, PluginLockfile, PluginManifest, PluginRuntime, PLUGIN_LOCK_FILE,
};
use quirl_syntax::{parse_command_list, Mode};
use quirl_ui::{
    ExtensionCompleter, ExtensionSuggestion, InteractivePanelBatch, InteractivePanelSnapshot,
    PanelModel, PANEL_COLUMNS_MAX, PANEL_COUNT_MAX, PANEL_FIELD_BYTES_MAX,
    PANEL_GENERATION_BYTES_MAX, PANEL_ROWS_MAX,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    hash::{Hash, Hasher},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

const MAX_PLUGIN_LOCK_BYTES: usize = 4 * 1024 * 1024;
const MAX_PLUGIN_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_PLUGIN_ENTRY_BYTES: usize = 4 * 1024 * 1024;
const MAX_PLUGIN_CANDIDATES: usize = 32;
const MAX_LOADED_PLUGIN_RUNTIMES: usize = 16;
const MAX_PLUGIN_GENERATION_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_HOST_EVENT_HANDLERS: usize = 64;
const MAX_HOST_PROMPT_SEGMENTS: usize = 64;
const MAX_HOST_CONTRIBUTIONS: usize = 64;
const MAX_HOST_MANAGED_COMMANDS: usize = 128;
const MAX_RETAINED_EXTENSION_ERRORS: usize = 64;
const MAX_CACHED_PROMPT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_EXTENSION_EVENT_BYTES: usize = 256 * 1024;
const MAX_EXTENSION_EVENT_ACTIONS: usize = 256;
const MAX_EXTENSION_COMPLETION_CALLBACKS: usize = 64;
const EXTENSION_COMPLETION_WALL_TIME: Duration = Duration::from_millis(250);
const EXTENSION_EVENT_WALL_TIME: Duration = Duration::from_millis(250);
const EXTENSION_PROMPT_REFRESH_WALL_TIME: Duration = Duration::from_millis(100);
const EXTENSION_PANEL_REFRESH_WALL_TIME: Duration = Duration::from_millis(250);
const EXTENSION_PANEL_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const EXTENSION_SAFE_POINT_WAIT: Duration = Duration::from_millis(125);
const PLUGIN_COMMAND_WALL_TIME: Duration = Duration::from_millis(50);
const PLUGIN_COMMAND_CLEANUP_WAIT: Duration = Duration::from_millis(125);
// These logical payload bounds leave fixed space inside the stricter raw Lua
// return envelope (4,096 nodes, depth 16, and 256 KiB retained text). The
// relationship is covered end to end at every exact boundary below.
const MAX_PLUGIN_VALUE_NODES: usize = 512;
const MAX_PLUGIN_VALUE_DEPTH: usize = 6;
const MAX_PLUGIN_VALUE_FIELDS: usize = 256;
const MAX_PLUGIN_VALUE_TEXT_BYTES: usize = 240 * 1024;

static NEXT_RUNTIME_KEY: AtomicU64 = AtomicU64::new(1);

pub type SharedLuaExtensions = Arc<Mutex<LuaExtensionHost>>;

/// A rendered plugin prompt segment, retaining the registration name so callers
/// can order it using `config.prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedExtensionSegment {
    pub name: String,
    pub value: String,
}

struct ExtensionRuntimeSlot {
    key: u64,
    runtime: Mutex<LuaRuntime>,
    registrations: PluginRegistrations,
    cancellation: LuaCancellation,
}

impl ExtensionRuntimeSlot {
    fn new(runtime: LuaRuntime, registrations: PluginRegistrations) -> Result<Self, ShellError> {
        let key = NEXT_RUNTIME_KEY
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|observed| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "extension runtime identity counter was exhausted",
                )
                .with_context(format!("observed identity: {observed}"))
                .with_help("Restart Quirl before loading another extension runtime")
            })?;
        let cancellation = runtime.cancellation_token();
        Ok(Self {
            key,
            runtime: Mutex::new(runtime),
            registrations,
            cancellation,
        })
    }

    fn with_runtime<T>(
        &self,
        operation: impl FnOnce(&LuaRuntime) -> Result<T, ShellError>,
    ) -> Result<T, ShellError> {
        let runtime = self.lock_runtime()?;
        runtime.clear_cancellation();
        let result = operation(&runtime);
        runtime.clear_cancellation();
        result
    }

    fn run_scheduled<T>(
        &self,
        control: &mut ExtensionWorkContext,
        preserve_prior_cancellation: bool,
        operation: impl FnOnce(&LuaRuntime) -> Result<T, ShellError>,
    ) -> ScheduledInvocation<T> {
        if control.is_cancelled() || Instant::now() >= control.deadline() {
            return ScheduledInvocation::Cancelled;
        }
        let runtime = match self.lock_runtime() {
            Ok(runtime) => runtime,
            Err(error) => return ScheduledInvocation::Finished(Err(error)),
        };
        if control.is_cancelled() || Instant::now() >= control.deadline() {
            return ScheduledInvocation::Cancelled;
        }

        // The gate prevents a deadline monitor that already cloned the
        // cancellation closure from setting the sticky Lua flag after this
        // invocation has completed and cleared it.
        let cancellation_enabled = Arc::new(Mutex::new(false));
        let cancellation_gate = Arc::clone(&cancellation_enabled);
        let cancellation = self.cancellation.clone();
        if !control.begin(Arc::new(move || {
            if *lock_recover(&cancellation_gate) {
                cancellation.cancel();
            }
        })) {
            return ScheduledInvocation::Cancelled;
        }

        let mut enabled = lock_recover(&cancellation_enabled);
        if !preserve_prior_cancellation {
            runtime.clear_cancellation();
        }
        *enabled = true;
        drop(enabled);

        let result = operation(&runtime);
        let cancelled = control.is_cancelled();
        let mut enabled = lock_recover(&cancellation_enabled);
        *enabled = false;
        runtime.clear_cancellation();
        drop(enabled);
        if cancelled {
            ScheduledInvocation::Cancelled
        } else {
            ScheduledInvocation::Finished(result)
        }
    }

    fn lock_runtime(&self) -> Result<MutexGuard<'_, LuaRuntime>, ShellError> {
        self.runtime.lock().map_err(|_| {
            ShellError::new(
                ErrorCode::Lua,
                "an extension runtime was quarantined after a panic",
            )
            .with_context(format!("runtime key: {}", self.key))
            .with_help("Disable the failing plugin and restart Quirl")
        })
    }

    fn prepare_execution_cancellation(
        &self,
    ) -> Result<quirl_core::ExecutionCancellation, ShellError> {
        let runtime = self.lock_runtime()?;
        runtime.clear_cancellation();
        Ok(runtime.execution_cancellation())
    }
}

enum ScheduledInvocation<T> {
    Finished(Result<T, ShellError>),
    Cancelled,
}

struct PromptPluginResult {
    plugin_index: usize,
    invocation: ScheduledInvocation<Vec<NamedExtensionSegment>>,
    errors: Vec<ShellError>,
}

struct PromptRefresh {
    request_id: u64,
    generation: u64,
    deadline: Instant,
    batch: ExtensionWorkBatch,
    receiver: Receiver<PromptPluginResult>,
    results: Vec<Option<PromptPluginResult>>,
}

struct PanelPluginResult {
    plugin_index: usize,
    invocation: ScheduledInvocation<Vec<InteractivePanelSnapshot>>,
    errors: Vec<ShellError>,
}

struct PanelRefresh {
    request_id: u64,
    generation: u64,
    deadline: Instant,
    batch: ExtensionWorkBatch,
    receiver: Receiver<PanelPluginResult>,
    results: Vec<Option<PanelPluginResult>>,
}

struct EventPluginResult {
    plugin_index: usize,
    invocation: ScheduledInvocation<Vec<EventHandlerReport>>,
}

struct PluginCommandResult {
    invocation: ScheduledInvocation<ExecutionOutcome>,
}

#[derive(Clone)]
struct PluginCommandBinding {
    command: CommandSpec,
    runtime: Arc<ExtensionRuntimeSlot>,
    allowed_effects: ExecutionEffects,
}

/// A command invocation resolved only from validated installed metadata.
///
/// Construction reads manifests and locked bytes but never evaluates Lua or
/// invokes an extension callback. Execution later reconciles this snapshot
/// with the active runtime generation before scheduling the callback.
#[derive(Debug, Clone)]
pub(crate) struct InstalledPluginCommand {
    command: CommandSpec,
    arguments: Vec<String>,
}

impl InstalledPluginCommand {
    pub(crate) fn effect_names(&self) -> Vec<String> {
        self.command
            .effects
            .iter()
            .map(|effect| catalog_effect_name(*effect).to_owned())
            .collect()
    }
}

/// A cancellation boundary detached from the extension-host mutex.
pub(crate) struct ExtensionCallbackQuiescence {
    scheduler: Option<ExtensionSchedulerHandle>,
    generation: u64,
}

impl ExtensionCallbackQuiescence {
    /// Wait for all callbacks from the captured generation to release their Lua
    /// runtimes before the caller begins a process, terminal, job, or persistence
    /// transition.
    pub(crate) fn wait(self) -> Result<(), ShellError> {
        let Some(scheduler) = self.scheduler else {
            return Ok(());
        };
        if scheduler.wait_generation_idle(self.generation, EXTENSION_SAFE_POINT_WAIT) {
            return Ok(());
        }
        Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "extension callbacks did not quiesce before execution",
        )
        .with_context(format!(
            "generation: {}; wait limit: {} ms",
            self.generation,
            EXTENSION_SAFE_POINT_WAIT.as_millis()
        ))
        .with_help("Disable the blocked plugin before retrying the command"))
    }
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogContributionOutput {
    commands: Vec<CommandSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionContributionItem {
    value: String,
    #[serde(default)]
    display: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    detail: Option<String>,
}

/// The result of checking extension sources for a new valid generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionReloadState {
    Unchanged,
    Reloaded { revision: u64 },
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileFingerprint {
    Missing,
    Contents { bytes: usize, hash: u64 },
    Unreadable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PluginFingerprint {
    Files(Vec<(PathBuf, FileFingerprint)>),
    #[cfg(test)]
    UnreadableDirectory(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtensionFingerprint {
    config: Option<FileFingerprint>,
    plugins: PluginFingerprint,
}

#[derive(Debug)]
struct SourceSnapshot {
    fingerprint: ExtensionFingerprint,
    config: Option<PathBuf>,
    plugins: Vec<PluginCandidate>,
    errors: Vec<ShellError>,
}

#[derive(Debug)]
enum PluginSource {
    Fixed(Vec<PathBuf>),
    #[cfg(test)]
    Directory(PathBuf),
    Managed(PathBuf),
}

#[derive(Debug, Clone)]
struct PluginCandidate {
    path: PathBuf,
    /// Present for managed plugins whose locked bytes were verified during the
    /// snapshot. Loading these bytes instead of reopening `path` closes the
    /// integrity-check-to-execution race.
    verified_source: Option<String>,
    source_bytes: usize,
    runtime: PluginRuntime,
    grants: Vec<String>,
    catalog_commands: Vec<CommandSpec>,
    adapter_activation: Option<AdapterActivation>,
}

#[derive(Debug, Clone)]
struct AdapterActivation {
    manifest: PluginManifest,
    entry_bytes: Vec<u8>,
}

type BuiltExtensionGeneration = (
    ConfigStore,
    Vec<PathBuf>,
    Vec<Arc<ExtensionRuntimeSlot>>,
    Vec<CommandSpec>,
    Vec<PluginCommandBinding>,
);

pub struct LuaExtensionHost {
    /// `Some` for a config file that is watched even when it does not exist yet.
    config_path: Option<PathBuf>,
    plugin_source: PluginSource,
    plugin_paths: Vec<PathBuf>,
    config: ConfigStore,
    plugin_runtimes: Vec<Arc<ExtensionRuntimeSlot>>,
    managed_commands: Vec<CommandSpec>,
    command_bindings: Vec<PluginCommandBinding>,
    errors: Vec<ShellError>,
    error_overflow_count: usize,
    observed_fingerprint: Option<ExtensionFingerprint>,
    revision: u64,
    event_sequence: u64,
    scheduler: Option<ExtensionScheduler>,
    prompt_request_id: u64,
    prompt_refresh: Option<PromptRefresh>,
    prompt_cache: Vec<Vec<NamedExtensionSegment>>,
    panel_request_id: u64,
    panel_cache_generation: u64,
    panel_refresh: Option<PanelRefresh>,
    panel_cache: Vec<Vec<InteractivePanelSnapshot>>,
    panel_last_refresh: Option<Instant>,
}

impl LuaExtensionHost {
    pub fn discover() -> Self {
        let configuration = config_directory();
        let config_path = configuration
            .as_ref()
            .map(|directory| directory.join("config.lua"));
        match plugin_state_directory() {
            Some(root) => Self::from_managed_root(config_path, root),
            None => Self::from_paths(config_path, Vec::new()),
        }
    }

    /// Test-only legacy constructor for exercising atomic directory reloads.
    #[cfg(test)]
    pub fn from_directory(directory: PathBuf) -> Self {
        Self::with_source(
            Some(directory.join("config.lua")),
            PluginSource::Directory(directory.join("plugins")),
        )
    }

    pub fn from_paths(config_path: Option<PathBuf>, mut plugin_paths: Vec<PathBuf>) -> Self {
        plugin_paths.sort();
        Self::with_source(config_path, PluginSource::Fixed(plugin_paths))
    }

    pub fn from_managed_root(config_path: Option<PathBuf>, root: PathBuf) -> Self {
        Self::with_source(config_path, PluginSource::Managed(root))
    }

    fn with_source(config_path: Option<PathBuf>, plugin_source: PluginSource) -> Self {
        Self {
            config_path,
            plugin_source,
            plugin_paths: Vec::new(),
            config: ConfigStore::default(),
            plugin_runtimes: Vec::new(),
            managed_commands: Vec::new(),
            command_bindings: Vec::new(),
            errors: Vec::new(),
            error_overflow_count: 0,
            observed_fingerprint: None,
            revision: 0,
            event_sequence: 0,
            scheduler: None,
            prompt_request_id: 0,
            prompt_refresh: None,
            prompt_cache: Vec::new(),
            panel_request_id: 0,
            panel_cache_generation: 0,
            panel_refresh: None,
            panel_cache: Vec::new(),
            panel_last_refresh: None,
        }
    }

    /// Poll source files and atomically install a fully validated generation.
    ///
    /// Failed generations are remembered by fingerprint, so a malformed file
    /// reports one error and leaves the complete last-known-good generation live
    /// until its content changes again.
    pub fn reload_if_changed(&mut self) -> ExtensionReloadState {
        self.reload_if_changed_with_cancellation(&AtomicBool::new(false))
    }

    /// Poll source files and atomically install a generation unless the caller
    /// cancels a potentially process-backed adapter handshake.
    pub fn reload_if_changed_with_cancellation(
        &mut self,
        cancellation: &AtomicBool,
    ) -> ExtensionReloadState {
        let snapshot = self.snapshot_sources(cancellation);
        if self.observed_fingerprint.as_ref() == Some(&snapshot.fingerprint) {
            return ExtensionReloadState::Unchanged;
        }
        let candidate_fingerprint = snapshot.fingerprint.clone();

        let next_revision = match self.revision.checked_add(1) {
            Some(revision) => revision,
            None => {
                self.observed_fingerprint = Some(candidate_fingerprint);
                self.record_error(
                    ShellError::new(
                        ErrorCode::ResourceLimit,
                        "extension generation counter was exhausted",
                    )
                    .with_help("Restart Quirl before reloading extensions again"),
                );
                return ExtensionReloadState::Rejected;
            }
        };
        match self.build_candidate(snapshot, cancellation) {
            Ok((config, plugin_paths, plugin_runtimes, managed_commands, command_bindings)) => {
                let next_panel_generation = match self.panel_cache_generation.checked_add(1) {
                    Some(generation) => generation,
                    None => {
                        self.observed_fingerprint = Some(candidate_fingerprint);
                        self.record_error(
                            ShellError::new(
                                ErrorCode::ResourceLimit,
                                "extension panel generation counter was exhausted",
                            )
                            .with_help("Restart Quirl before reloading extension panels again"),
                        );
                        return ExtensionReloadState::Rejected;
                    }
                };
                if !plugin_runtimes.is_empty() && self.scheduler.is_none() {
                    self.scheduler = Some(ExtensionScheduler::new());
                }
                if let Some(error) = self
                    .scheduler
                    .as_mut()
                    .and_then(ExtensionScheduler::take_startup_error)
                {
                    self.observed_fingerprint = Some(candidate_fingerprint);
                    self.record_error(error.with_context(
                        "extension reload rejected; retaining the last known-good generation",
                    ));
                    return ExtensionReloadState::Rejected;
                }
                if let Some(scheduler) = &self.scheduler {
                    if let Err(error) = scheduler.activate_generation(next_revision) {
                        self.observed_fingerprint = Some(candidate_fingerprint);
                        self.record_error(error.with_context(
                            "extension reload rejected; retaining the last known-good generation",
                        ));
                        return ExtensionReloadState::Rejected;
                    }
                }
                self.config = config;
                self.plugin_paths = plugin_paths;
                self.plugin_runtimes = plugin_runtimes;
                self.managed_commands = managed_commands;
                self.command_bindings = command_bindings;
                self.prompt_refresh.take();
                self.prompt_cache = vec![Vec::new(); self.plugin_runtimes.len()];
                self.panel_refresh.take();
                self.panel_cache = vec![Vec::new(); self.plugin_runtimes.len()];
                self.panel_cache_generation = next_panel_generation;
                self.panel_last_refresh = None;
                self.revision = next_revision;
                self.observed_fingerprint = Some(candidate_fingerprint);
                ExtensionReloadState::Reloaded {
                    revision: self.revision,
                }
            }
            Err(error) => {
                if !cancellation.load(Ordering::Relaxed) {
                    self.observed_fingerprint = Some(candidate_fingerprint);
                }
                self.record_error(
                    error.with_context("extension reload rejected; retaining the last known-good configuration and plugins"),
                );
                ExtensionReloadState::Rejected
            }
        }
    }

    /// The configuration from the active, fully validated extension generation.
    pub fn active_config(&mut self) -> &QuirlConfig {
        self.ensure_loaded();
        self.config.active()
    }

    /// Increments only when a complete config/plugin generation is installed.
    pub fn config_revision(&self) -> u64 {
        self.revision
    }

    pub fn has_runtime_extensions(&self) -> bool {
        !self.plugin_runtimes.is_empty()
    }

    /// Reconcile a nonexecuting installed-command snapshot with the active Lua
    /// generation and construct the shared request using the VM's cancellation
    /// identity. A removed, replaced, non-Lua, or failed runtime never falls
    /// through to native process lookup.
    pub(crate) fn plugin_execution_request(
        &mut self,
        installed: &InstalledPluginCommand,
        source: &str,
        input: ExecutionInput,
    ) -> Result<ExecutionRequest, ShellError> {
        self.reload_if_changed();
        let binding = self
            .command_bindings
            .iter()
            .find(|binding| binding.command.id == installed.command.id)
            .ok_or_else(|| {
                let mut error = ShellError::new(
                    ErrorCode::InvalidCommand,
                    format!(
                        "installed plugin command `{}` has no active trusted-Lua runtime",
                        installed.command.path
                    ),
                )
                .with_command(source)
                .with_help("Run `quirl plugin doctor`, then enable a healthy trusted-Lua plugin");
                if let Some(cause) = self.errors.last() {
                    error = error.with_context(format!("activation failure: {}", cause.message));
                }
                error
            })?;
        if binding.command != installed.command {
            return Err(stale_plugin_snapshot_error(&installed.command, source));
        }
        validate_plugin_input(&binding.command, &input, source, None)?;
        let declared_effects = catalog_execution_effects(&binding.command.effects);
        let cancellation = binding.runtime.prepare_execution_cancellation()?;
        Ok(ExecutionRequest::new(
            ExecutionSource::new(binding.command.id.clone(), source)?,
            quirl_core::ExecutionMode::Plugin,
        )
        .with_cancellation(cancellation)
        .with_input(input)
        .with_arguments(installed.arguments.clone())
        .with_deadline(PLUGIN_COMMAND_WALL_TIME)
        .with_output(ExecutionOutputTarget::Value)
        .with_effects(declared_effects, binding.allowed_effects))
    }

    /// Run one validated plugin plan through the bounded shared scheduler.
    pub(crate) fn dispatch_plugin_plan(
        &mut self,
        plan: &ExecutionPlan,
    ) -> Result<ExecutionOutcome, ShellError> {
        plan.ensure_active("before plugin reconciliation")?;
        if self.reload_if_changed() == ExtensionReloadState::Rejected {
            let mut error = ShellError::new(
                ErrorCode::Validation,
                "plugin execution state could not be reconciled with its installed snapshot",
            )
            .with_command(plan.source().text())
            .with_help("Run `quirl plugin doctor`, restore the locked source, and retry");
            if let Some(cause) = self.errors.last() {
                error = error.with_context(format!("reload failure: {}", cause.message));
            }
            return Err(error);
        }
        let binding = self
            .command_bindings
            .iter()
            .find(|binding| binding.command.id == plan.source().name())
            .cloned()
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::InvalidCommand,
                    "plugin command disappeared before execution",
                )
                .with_command(plan.source().text())
                .with_context(format!("command id: {}", plan.source().name()))
                .with_help("Retry against the current installed plugin catalog")
            })?;
        let current = bind_plugin_invocation(
            std::slice::from_ref(&binding.command),
            plan.source().text(),
        )?
        .ok_or_else(|| stale_plugin_snapshot_error(&binding.command, plan.source().text()))?;
        if current.arguments != plan.arguments()
            || catalog_execution_effects(&binding.command.effects) != plan.declared_effects()
        {
            return Err(stale_plugin_snapshot_error(
                &binding.command,
                plan.source().text(),
            ));
        }
        validate_plugin_input(
            &binding.command,
            plan.input(),
            plan.source().text(),
            Some(plan),
        )?;
        if plan.output() != ExecutionOutputTarget::Value {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "plugin command `{}` requires typed value output",
                    binding.command.path
                ),
            )
            .with_command(plan.source().text())
            .with_help("Request the shared typed value output target"));
        }
        let context = LuaRunnerContext::from_current_process(
            plan.arguments(),
            plan.input().clone(),
            plan.output(),
            plan.declared_effects(),
            plan.cancellation().atomic(),
        )?;
        plan.ensure_active("after plugin context construction")?;
        let deadline = plan.deadline().expires_at();
        let (sender, receiver) = mpsc::sync_channel(1);
        let runtime = Arc::clone(&binding.runtime);
        let command_name = binding.command.path.clone();
        plan.deadline()
            .ensure_remaining("before plugin scheduler admission")?;
        let callback_expires_at = plan.deadline().expires_at();
        let work = vec![ExtensionWork::new(runtime.key, move |mut control| {
            let invocation = runtime.run_scheduled(&mut control, true, |runtime| {
                runtime.run_plugin_command_with_context(
                    &command_name,
                    &context,
                    callback_expires_at,
                )
            });
            let _ = sender.try_send(PluginCommandResult { invocation });
        })];
        let Some(scheduler) = &self.scheduler else {
            return Err(unavailable_plugin_scheduler_error());
        };
        let batch = scheduler.submit_batch(self.revision, deadline, WorkPriority::Command, work)?;
        let wait = plan
            .deadline()
            .ensure_remaining("before waiting for plugin execution")?;
        let result = match receiver.recv_timeout(wait) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                scheduler.cancel_batch(&batch);
                if !scheduler.wait_batch_idle(&batch, PLUGIN_COMMAND_CLEANUP_WAIT) {
                    return Err(ShellError::new(
                        ErrorCode::ResourceLimit,
                        format!(
                            "plugin command `{}` did not quiesce after cancellation",
                            binding.command.path
                        ),
                    )
                    .with_context(format!(
                        "cleanup wait: {} ms",
                        PLUGIN_COMMAND_CLEANUP_WAIT.as_millis()
                    ))
                    .with_help("Disable the blocked plugin and restart Quirl before retrying"));
                }
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    format!(
                        "plugin command `{}` did not finish before its deadline",
                        binding.command.path
                    ),
                )
                .with_context(format!(
                    "deadline: {} ms",
                    plan.deadline().budget().as_millis()
                ))
                .with_help("Reduce callback work or disable the blocked plugin"));
            }
            Err(RecvTimeoutError::Disconnected) => {
                scheduler.cancel_batch(&batch);
                return Err(ShellError::new(
                    ErrorCode::Lua,
                    format!(
                        "plugin command `{}` terminated without an outcome",
                        binding.command.path
                    ),
                )
                .with_help("Disable the failing plugin and restart Quirl before retrying"));
            }
        };
        match result.invocation {
            ScheduledInvocation::Finished(Ok(outcome)) => {
                plan.ensure_active("before plugin output conversion")?;
                validate_plugin_outcome(&binding.command, &outcome)?;
                plan.ensure_active("after plugin output conversion")?;
                Ok(outcome)
            }
            ScheduledInvocation::Finished(Err(error)) => Err(error),
            ScheduledInvocation::Cancelled => Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!("plugin command `{}` was cancelled", binding.command.path),
            )
            .with_help("Retry only after the owning cancellation handle is clear")),
        }
    }

    /// Render segments while retaining their registration names for config-driven
    /// ordering by the REPL/UI layer.
    pub fn named_prompt_segments(
        &mut self,
        mode: Mode,
        last_status: i32,
    ) -> Vec<NamedExtensionSegment> {
        self.ensure_loaded();
        self.poll_prompt_refresh();
        let snapshot = self
            .prompt_cache
            .iter()
            .flat_map(|segments| segments.iter().cloned())
            .take(MAX_HOST_PROMPT_SEGMENTS)
            .collect::<Vec<_>>();
        if self.plugin_runtimes.is_empty() {
            return snapshot;
        }

        let cwd = env::current_dir().unwrap_or_default();
        let context = Arc::new(json!({
            "cwd": cwd,
            "project_name": cwd.file_name().map(|name| name.to_string_lossy()),
            "mode": mode.to_string(),
            "last_status": last_status,
        }));
        self.start_prompt_refresh(context);
        snapshot
    }

    fn start_prompt_refresh(&mut self, context: Arc<Value>) {
        if let Some(previous) = self.prompt_refresh.take() {
            if let Some(scheduler) = &self.scheduler {
                scheduler.cancel_batch(&previous.batch);
            }
        }
        let request_id = match self.prompt_request_id.checked_add(1) {
            Some(request_id) => request_id,
            None => {
                self.record_error(
                    ShellError::new(
                        ErrorCode::ResourceLimit,
                        "extension prompt generation counter was exhausted",
                    )
                    .with_help("Restart Quirl before refreshing extension prompts again"),
                );
                return;
            }
        };
        self.prompt_request_id = request_id;
        let started = Instant::now();
        let Some(deadline) = started.checked_add(EXTENSION_PROMPT_REFRESH_WALL_TIME) else {
            self.record_error(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "extension prompt deadline exceeded the host monotonic clock",
                )
                .with_help("Restart Quirl before refreshing extension prompts again"),
            );
            return;
        };
        let (sender, receiver) = mpsc::sync_channel(self.plugin_runtimes.len());
        let mut work = Vec::with_capacity(self.plugin_runtimes.len());
        for (plugin_index, slot) in self.plugin_runtimes.iter().enumerate() {
            let slot = Arc::clone(slot);
            let registrations = slot.registrations.prompt_segments.clone();
            let context = Arc::clone(&context);
            let sender = sender.clone();
            work.push(ExtensionWork::new(slot.key, move |mut control| {
                let mut errors = Vec::new();
                let invocation = slot.run_scheduled(&mut control, false, |runtime| {
                    let mut rendered = Vec::new();
                    for segment in registrations {
                        match runtime.render_prompt_segment(&segment.name, &context) {
                            Ok(Some(value)) if !value.is_empty() => {
                                rendered.push(NamedExtensionSegment {
                                    name: segment.name,
                                    value,
                                });
                            }
                            Ok(_) => {}
                            Err(error) => errors.push(
                                error.with_context(format!("prompt segment: {}", segment.name)),
                            ),
                        }
                    }
                    Ok(rendered)
                });
                let _ = sender.try_send(PromptPluginResult {
                    plugin_index,
                    invocation,
                    errors,
                });
            }));
        }
        drop(sender);
        let Some(scheduler) = &self.scheduler else {
            return;
        };
        let batch =
            match scheduler.submit_batch(self.revision, deadline, WorkPriority::Prompt, work) {
                Ok(batch) => batch,
                Err(error) => {
                    self.record_error(error.with_context("extension prompt refresh"));
                    return;
                }
            };
        self.prompt_refresh = Some(PromptRefresh {
            request_id,
            generation: self.revision,
            deadline,
            batch,
            receiver,
            results: std::iter::repeat_with(|| None)
                .take(self.plugin_runtimes.len())
                .collect(),
        });
    }

    fn poll_prompt_refresh(&mut self) {
        let Some(mut refresh) = self.prompt_refresh.take() else {
            return;
        };
        loop {
            match refresh.receiver.try_recv() {
                Ok(result)
                    if result.plugin_index < refresh.results.len()
                        && refresh.generation == self.revision
                        && refresh.request_id == self.prompt_request_id =>
                {
                    let index = result.plugin_index;
                    refresh.results[index] = Some(result);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let complete = refresh.results.iter().all(Option::is_some);
        let expired = Instant::now() >= refresh.deadline;
        if !complete && !expired {
            self.prompt_refresh = Some(refresh);
            return;
        }
        if !complete {
            if let Some(scheduler) = &self.scheduler {
                scheduler.cancel_batch(&refresh.batch);
            }
        }
        if refresh.generation != self.revision || refresh.request_id != self.prompt_request_id {
            return;
        }

        let mut completed_plugins = 0_usize;
        for result in refresh.results.into_iter().flatten() {
            completed_plugins = completed_plugins.saturating_add(1);
            let callback_failed = !result.errors.is_empty();
            for error in result.errors {
                self.record_error(error);
            }
            match result.invocation {
                ScheduledInvocation::Finished(Ok(segments)) if !callback_failed => {
                    if self.prompt_cache_accepts(result.plugin_index, &segments) {
                        self.prompt_cache[result.plugin_index] = segments;
                    } else {
                        self.record_error(
                            ShellError::new(
                                ErrorCode::ResourceLimit,
                                "extension prompt cache byte limit was exceeded",
                            )
                            .with_context(format!(
                                "limit: {MAX_CACHED_PROMPT_BYTES}; plugin index: {}",
                                result.plugin_index
                            ))
                            .with_help("Shorten enabled extension prompt segment output"),
                        );
                    }
                }
                ScheduledInvocation::Finished(Ok(_)) => {}
                ScheduledInvocation::Finished(Err(error)) => self.record_error(error),
                ScheduledInvocation::Cancelled => {}
            }
        }
        if expired && completed_plugins < self.plugin_runtimes.len() {
            self.record_error(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "extension prompt refresh reached its aggregate deadline",
                )
                .with_context(format!(
                    "completed plugins: {completed_plugins}; total plugins: {}; deadline: {} ms",
                    self.plugin_runtimes.len(),
                    EXTENSION_PROMPT_REFRESH_WALL_TIME.as_millis()
                ))
                .with_help("Reduce slow prompt providers; the last cached snapshot remains active"),
            );
        }
    }

    fn prompt_cache_accepts(
        &self,
        plugin_index: usize,
        replacement: &[NamedExtensionSegment],
    ) -> bool {
        let mut bytes = 0_usize;
        for (index, segments) in self.prompt_cache.iter().enumerate() {
            let segments = if index == plugin_index {
                replacement
            } else {
                segments
            };
            for segment in segments {
                bytes = bytes.saturating_add(segment.name.len());
                bytes = bytes.saturating_add(segment.value.len());
                if bytes > MAX_CACHED_PROMPT_BYTES {
                    return false;
                }
            }
        }
        true
    }

    /// Return only completed panel cache state and schedule a bounded refresh.
    ///
    /// This method never invokes Lua on the caller. Provider callbacks are
    /// submitted to the existing extension scheduler and a failing refresh
    /// preserves the last complete per-plugin snapshot.
    pub(crate) fn cached_panel_snapshot(&mut self) -> InteractivePanelBatch {
        self.poll_panel_refresh();
        let snapshot = InteractivePanelBatch {
            generation: self.panel_cache_generation,
            panels: self
                .panel_cache
                .iter()
                .flatten()
                .take(PANEL_COUNT_MAX)
                .cloned()
                .collect(),
        };
        let refresh_due = self
            .panel_last_refresh
            .is_none_or(|last| last.elapsed() >= EXTENSION_PANEL_REFRESH_INTERVAL);
        if refresh_due && self.panel_refresh.is_none() && !self.plugin_runtimes.is_empty() {
            self.start_panel_refresh();
        }
        snapshot
    }

    fn start_panel_refresh(&mut self) {
        // Keep request identity, deadline construction, worker ownership, and
        // publication setup in one transaction. Splitting this state-machine
        // transition would make a partially submitted generation observable.
        let request_id = match self.panel_request_id.checked_add(1) {
            Some(request_id) => request_id,
            None => {
                self.record_error(
                    ShellError::new(
                        ErrorCode::ResourceLimit,
                        "extension panel request counter was exhausted",
                    )
                    .with_help("Restart Quirl before refreshing extension panels again"),
                );
                return;
            }
        };
        let started = Instant::now();
        let Some(deadline) = started.checked_add(EXTENSION_PANEL_REFRESH_WALL_TIME) else {
            self.record_error(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "extension panel deadline exceeded the host monotonic clock",
                )
                .with_help("Restart Quirl before refreshing extension panels again"),
            );
            return;
        };
        self.panel_request_id = request_id;
        self.panel_last_refresh = Some(started);
        let cwd = env::current_dir().unwrap_or_default();
        let context = Arc::new(json!({ "cwd": cwd }));
        let (sender, receiver) = mpsc::sync_channel(self.plugin_runtimes.len());
        let mut work = Vec::with_capacity(self.plugin_runtimes.len());
        for (plugin_index, slot) in self.plugin_runtimes.iter().enumerate() {
            let slot = Arc::clone(slot);
            let registrations = slot
                .registrations
                .contributions
                .iter()
                .filter(|registration| registration.kind == ContributionKind::Panel)
                .take(PANEL_COUNT_MAX)
                .cloned()
                .collect::<Vec<_>>();
            let context = Arc::clone(&context);
            let sender = sender.clone();
            work.push(ExtensionWork::new(slot.key, move |mut control| {
                let mut errors = Vec::new();
                let invocation = slot.run_scheduled(&mut control, false, |runtime| {
                    let mut panels = Vec::new();
                    for registration in registrations {
                        if panels.len() == PANEL_COUNT_MAX {
                            break;
                        }
                        let value = match runtime.invoke_contribution(
                            ContributionKind::Panel,
                            &registration.name,
                            context.as_ref(),
                        ) {
                            Ok(value) => value,
                            Err(error) => {
                                errors.push(error.with_context(format!(
                                    "panel contribution: {}",
                                    registration.name
                                )));
                                continue;
                            }
                        };
                        let panel = match serde_json::from_value::<PanelModel>(value) {
                            Ok(panel) => panel,
                            Err(error) => {
                                errors.push(contribution_shape_error(
                                    &registration.name,
                                    "panel providers must return the typed PanelModel object",
                                    error,
                                ));
                                continue;
                            }
                        };
                        if let Err(error) = validate_cached_panel(&registration, &panel) {
                            errors.push(error);
                            continue;
                        }
                        panels.push(InteractivePanelSnapshot {
                            id: registration.name,
                            model: panel,
                        });
                    }
                    Ok(panels)
                });
                let _ = sender.try_send(PanelPluginResult {
                    plugin_index,
                    invocation,
                    errors,
                });
            }));
        }
        drop(sender);
        let Some(scheduler) = &self.scheduler else {
            return;
        };
        let batch =
            match scheduler.submit_batch(self.revision, deadline, WorkPriority::Prompt, work) {
                Ok(batch) => batch,
                Err(error) => {
                    self.record_error(error.with_context("extension panel refresh"));
                    return;
                }
            };
        self.panel_refresh = Some(PanelRefresh {
            request_id,
            generation: self.revision,
            deadline,
            batch,
            receiver,
            results: std::iter::repeat_with(|| None)
                .take(self.plugin_runtimes.len())
                .collect(),
        });
    }

    fn poll_panel_refresh(&mut self) {
        // Completion, expiry, cancellation, and cache publication are one
        // transition: the old complete cache stays authoritative until every
        // accepted result for this generation has been classified.
        let Some(mut refresh) = self.panel_refresh.take() else {
            return;
        };
        loop {
            match refresh.receiver.try_recv() {
                Ok(result)
                    if result.plugin_index < refresh.results.len()
                        && refresh.generation == self.revision
                        && refresh.request_id == self.panel_request_id =>
                {
                    let index = result.plugin_index;
                    refresh.results[index] = Some(result);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let complete = refresh.results.iter().all(Option::is_some);
        let expired = Instant::now() >= refresh.deadline;
        if !complete && !expired {
            self.panel_refresh = Some(refresh);
            return;
        }
        if !complete {
            if let Some(scheduler) = &self.scheduler {
                scheduler.cancel_batch(&refresh.batch);
            }
        }
        if refresh.generation != self.revision || refresh.request_id != self.panel_request_id {
            return;
        }

        let mut changed = false;
        let mut completed_plugins = 0_usize;
        for result in refresh.results.into_iter().flatten() {
            completed_plugins = completed_plugins.saturating_add(1);
            let callback_failed = !result.errors.is_empty();
            for error in result.errors {
                self.record_error(error);
            }
            match result.invocation {
                ScheduledInvocation::Finished(Ok(panels)) if !callback_failed => {
                    if self.panel_cache_accepts(result.plugin_index, &panels) {
                        changed |= self.panel_cache[result.plugin_index] != panels;
                        self.panel_cache[result.plugin_index] = panels;
                    } else {
                        self.record_error(panel_cache_limit_error());
                    }
                }
                ScheduledInvocation::Finished(Ok(_)) | ScheduledInvocation::Cancelled => {}
                ScheduledInvocation::Finished(Err(error)) => self.record_error(error),
            }
        }
        if changed {
            match self.panel_cache_generation.checked_add(1) {
                Some(generation) => self.panel_cache_generation = generation,
                None => self.record_error(
                    ShellError::new(
                        ErrorCode::ResourceLimit,
                        "extension panel cache generation counter was exhausted",
                    )
                    .with_help("Restart Quirl before refreshing extension panels again"),
                ),
            }
        }
        if expired && completed_plugins < self.plugin_runtimes.len() {
            self.record_error(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "extension panel refresh reached its aggregate deadline",
                )
                .with_context(format!(
                    "completed plugins: {completed_plugins}; total plugins: {}; deadline: {} ms",
                    self.plugin_runtimes.len(),
                    EXTENSION_PANEL_REFRESH_WALL_TIME.as_millis()
                ))
                .with_help("Reduce slow panel providers; the last cached snapshot remains active"),
            );
        }
    }

    fn panel_cache_accepts(
        &self,
        plugin_index: usize,
        replacement: &[InteractivePanelSnapshot],
    ) -> bool {
        let mut count = 0_usize;
        let mut bytes = 0_usize;
        for (index, panels) in self.panel_cache.iter().enumerate() {
            let panels = if index == plugin_index {
                replacement
            } else {
                panels
            };
            for panel in panels {
                count = count.saturating_add(1);
                bytes = bytes.saturating_add(panel_bytes(&panel.id, &panel.model));
                if count > PANEL_COUNT_MAX || bytes > PANEL_GENERATION_BYTES_MAX {
                    return false;
                }
            }
        }
        true
    }

    pub fn complete(&mut self, line: &str, pos: usize) -> Vec<ExtensionSuggestion> {
        self.ensure_loaded();
        let position = floor_char_boundary(line, pos.min(line.len()));
        if line.len() > MAX_COMPLETION_QUERY_BYTES {
            self.record_error(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "extension completion query exceeds its byte limit",
                )
                .with_context(format!(
                    "bytes: {}; limit: {MAX_COMPLETION_QUERY_BYTES}",
                    line.len()
                ))
                .with_help("Shorten the input before requesting extension completion"),
            );
            return Vec::new();
        }
        let before = &line[..position];
        let token_start = before
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        let query = &before[token_start..];
        let context = json!({ "line": line, "cursor": position, "query": query });
        let mut suggestions = Vec::new();
        let started = Instant::now();
        let mut callback_count = 0_usize;
        let mut limit_reason = None;
        let runtimes = self.plugin_runtimes.clone();

        'plugins: for runtime in &runtimes {
            for provider in runtime.registrations.completion_providers.clone() {
                if !provider_applies(before, &provider.command) {
                    continue;
                }
                if callback_count == MAX_EXTENSION_COMPLETION_CALLBACKS {
                    limit_reason = Some(format!(
                        "extension completion callback limit reached: {MAX_EXTENSION_COMPLETION_CALLBACKS}"
                    ));
                    break 'plugins;
                }
                if started.elapsed() >= EXTENSION_COMPLETION_WALL_TIME {
                    limit_reason = Some(format!(
                        "extension completion deadline reached: {} ms",
                        EXTENSION_COMPLETION_WALL_TIME.as_millis()
                    ));
                    break 'plugins;
                }
                callback_count = callback_count.saturating_add(1);
                match runtime.with_runtime(|runtime| {
                    runtime.complete_with_provider(&provider.command, &context)
                }) {
                    Ok(Value::Array(values)) => {
                        for value in values {
                            if suggestions.len() == MAX_COMPLETION_RESULTS {
                                limit_reason = Some(format!(
                                    "extension completion result limit reached: {MAX_COMPLETION_RESULTS}"
                                ));
                                break 'plugins;
                            }
                            if let Some(suggestion) = extension_suggestion(
                                value,
                                query,
                                token_start,
                                position,
                                &provider.command,
                            ) {
                                suggestions.push(suggestion);
                            }
                        }
                    }
                    Ok(_) => self.record_error(ShellError::new(
                        ErrorCode::Validation,
                        format!(
                            "completion provider `{}` must return an array",
                            provider.command
                        ),
                    )),
                    Err(error) => self.record_error(
                        error.with_context(format!("completion provider: {}", provider.command)),
                    ),
                }
            }
            for registration in runtime
                .registrations
                .clone()
                .contributions
                .into_iter()
                .filter(|item| item.kind == ContributionKind::Completion)
            {
                if callback_count == MAX_EXTENSION_COMPLETION_CALLBACKS {
                    limit_reason = Some(format!(
                        "extension completion callback limit reached: {MAX_EXTENSION_COMPLETION_CALLBACKS}"
                    ));
                    break 'plugins;
                }
                if started.elapsed() >= EXTENSION_COMPLETION_WALL_TIME {
                    limit_reason = Some(format!(
                        "extension completion deadline reached: {} ms",
                        EXTENSION_COMPLETION_WALL_TIME.as_millis()
                    ));
                    break 'plugins;
                }
                callback_count = callback_count.saturating_add(1);
                match runtime.with_runtime(|runtime| {
                    runtime.invoke_contribution(
                        ContributionKind::Completion,
                        &registration.name,
                        &context,
                    )
                }) {
                    Ok(value) => match serde_json::from_value::<Vec<CompletionContributionItem>>(
                        value,
                    ) {
                        Ok(items) => {
                            for item in items {
                                if suggestions.len() == MAX_COMPLETION_RESULTS {
                                    limit_reason = Some(format!(
                                        "extension completion result limit reached: {MAX_COMPLETION_RESULTS}"
                                    ));
                                    break 'plugins;
                                }
                                if let Some(suggestion) = contribution_suggestion(
                                    item,
                                    query,
                                    token_start,
                                    position,
                                    &registration.name,
                                ) {
                                    suggestions.push(suggestion);
                                }
                            }
                        }
                        Err(error) => self.record_error(contribution_shape_error(
                            &registration.name,
                            "completion providers must return an array of typed completion items",
                            error,
                        )),
                    },
                    Err(error) => {
                        self.record_error(error.with_context(format!(
                            "completion contribution: {}",
                            registration.name
                        )))
                    }
                }
            }
        }
        if let Some(reason) = limit_reason {
            self.record_error(
                ShellError::new(ErrorCode::ResourceLimit, reason).with_help(
                    "Reduce enabled completion providers or narrow their returned items",
                ),
            );
        }
        suggestions
    }

    /// Retain the P07 dynamic-catalog contract as a focused compatibility
    /// harness. Production command discovery uses the nonexecuting installed
    /// snapshot below.
    #[cfg(test)]
    pub fn merge_catalog_contributions(&mut self, catalog: &mut Catalog) {
        self.ensure_loaded();
        if let Err(error) = validate_catalog_contribution(catalog, &self.managed_commands) {
            self.record_error(error.with_context("managed plugin command manifests"));
        } else {
            catalog.merge(self.managed_commands.clone());
        }
        let runtimes = self.plugin_runtimes.clone();
        for runtime in &runtimes {
            for registration in runtime
                .registrations
                .clone()
                .contributions
                .into_iter()
                .filter(|item| item.kind == ContributionKind::Catalog)
            {
                let value =
                    match runtime.with_runtime(|runtime| {
                        runtime.invoke_contribution(
                            ContributionKind::Catalog,
                            &registration.name,
                            &json!({"schema_version": catalog.schema_version}),
                        )
                    }) {
                        Ok(value) => value,
                        Err(error) => {
                            self.record_error(error.with_context(format!(
                                "catalog contribution: {}",
                                registration.name
                            )));
                            continue;
                        }
                    };
                let mut output = match serde_json::from_value::<CatalogContributionOutput>(value) {
                    Ok(output) => output,
                    Err(error) => {
                        self.record_error(contribution_shape_error(
                            &registration.name,
                            "catalog providers must return { commands = CommandSpec[] }",
                            error,
                        ));
                        continue;
                    }
                };
                if output.commands.len() > MAX_HOST_MANAGED_COMMANDS {
                    self.record_error(host_count_limit_error(
                        "catalog contribution commands",
                        output.commands.len(),
                        MAX_HOST_MANAGED_COMMANDS,
                    ));
                    continue;
                }
                let provenance = ProvenanceInfo {
                    source: Provenance::Plugin,
                    confidence: Confidence::Exact,
                    trust: Trust::Trusted,
                    origin: Some(registration.name.clone()),
                    fingerprint: None,
                    generated_at: None,
                };
                for command in &mut output.commands {
                    command.provenance = provenance.clone();
                    for argument in &mut command.options {
                        argument.provenance = provenance.clone();
                    }
                }
                if let Err(error) = validate_catalog_contribution(catalog, &output.commands) {
                    self.record_error(
                        error.with_context(format!("catalog contribution: {}", registration.name)),
                    );
                    continue;
                }
                catalog.merge(output.commands);
            }
        }
    }

    pub fn render_panel_contribution(
        &mut self,
        name: &str,
        context: &Value,
    ) -> Result<PanelModel, ShellError> {
        self.ensure_loaded();
        let runtimes = self.plugin_runtimes.clone();
        for runtime in &runtimes {
            let Some(registration) = runtime
                .registrations
                .clone()
                .contributions
                .into_iter()
                .find(|item| item.kind == ContributionKind::Panel && item.name == name)
            else {
                continue;
            };
            let value = runtime.with_runtime(|runtime| {
                runtime.invoke_contribution(ContributionKind::Panel, name, context)
            })?;
            let panel = serde_json::from_value::<PanelModel>(value).map_err(|error| {
                contribution_shape_error(
                    name,
                    "panel providers must return the typed PanelModel object",
                    error,
                )
            })?;
            panel.validate()?;
            if registration.plain_fallback.as_deref() != Some(panel.plain_fallback.as_str()) {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!("panel contribution `{name}` changed its declared plain fallback"),
                )
                .with_help("Return the same plain_fallback declared at registration"));
            }
            return Ok(panel);
        }
        Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("unknown panel contribution `{name}`"),
        )
        .with_help("Enable a plugin that declares this panel, then run plugin doctor"))
    }

    pub fn take_errors(&mut self) -> Vec<ShellError> {
        if self.error_overflow_count > 0 {
            let overflow = ShellError::new(
                ErrorCode::ResourceLimit,
                "extension diagnostic retention limit was reached",
            )
            .with_context(format!(
                "additional diagnostics dropped: {}; retained limit: {MAX_RETAINED_EXTENSION_ERRORS}",
                self.error_overflow_count
            ))
            .with_help("Disable repeatedly failing extension callbacks before retrying");
            if self.errors.len() == MAX_RETAINED_EXTENSION_ERRORS {
                self.errors.pop();
            }
            self.errors.push(overflow);
            self.error_overflow_count = 0;
        }
        std::mem::take(&mut self.errors)
    }

    /// Dispatch one immutable record to every active runtime. Individual
    /// handler failures are retained as diagnostics and never stop later
    /// runtimes or handlers.
    pub fn dispatch_event(
        &mut self,
        data: ExtensionEventData,
    ) -> Result<Vec<ExtensionAction>, ShellError> {
        self.ensure_loaded();
        if self.plugin_runtimes.is_empty() {
            return Ok(Vec::new());
        }
        let sequence = self.event_sequence.checked_add(1).ok_or_else(|| {
            ShellError::new(
                ErrorCode::ResourceLimit,
                "extension event sequence counter was exhausted",
            )
            .with_help("Restart Quirl before dispatching another extension event")
        })?;
        let event = Arc::new(ExtensionEvent::new(sequence, data));
        event.validate_after(None)?;
        let event_bytes = serde_json::to_vec(event.as_ref()).map_err(|error| {
            ShellError::new(
                ErrorCode::Validation,
                "extension event could not be encoded",
            )
            .with_context(error.to_string())
            .with_help("Reduce or repair the event payload before dispatch")
        })?;
        if event_bytes.len() > MAX_EXTENSION_EVENT_BYTES {
            let error = ShellError::new(
                ErrorCode::ResourceLimit,
                "extension event exceeds its host-side byte limit",
            )
            .with_context(format!(
                "bytes: {}; limit: {MAX_EXTENSION_EVENT_BYTES}",
                event_bytes.len()
            ))
            .with_help("Reduce the event payload before dispatching it to extensions");
            self.record_error(error.clone());
            return Err(error);
        }
        self.event_sequence = sequence;
        let started = Instant::now();
        let deadline = started
            .checked_add(EXTENSION_EVENT_WALL_TIME)
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "extension event deadline exceeded the host monotonic clock",
                )
                .with_help("Restart Quirl before dispatching another extension event")
            })?;
        let (sender, receiver) = mpsc::sync_channel(self.plugin_runtimes.len());
        let mut work = Vec::with_capacity(self.plugin_runtimes.len());
        for (plugin_index, runtime) in self.plugin_runtimes.iter().enumerate() {
            let runtime = Arc::clone(runtime);
            let event = Arc::clone(&event);
            let sender = sender.clone();
            work.push(ExtensionWork::new(runtime.key, move |mut control| {
                let invocation = runtime.run_scheduled(&mut control, false, |runtime| {
                    runtime.dispatch_extension_event(&event)
                });
                let _ = sender.try_send(EventPluginResult {
                    plugin_index,
                    invocation,
                });
            }));
        }
        drop(sender);
        let Some(scheduler) = &self.scheduler else {
            let error = unavailable_event_scheduler_error();
            self.record_error(error.clone());
            return Err(error);
        };
        let batch = match scheduler.submit_batch(self.revision, deadline, WorkPriority::Event, work)
        {
            Ok(batch) => batch,
            Err(error) => {
                self.record_error(error.clone().with_context("extension event dispatch"));
                return Err(error);
            }
        };

        let mut outcomes = std::iter::repeat_with(|| None)
            .take(self.plugin_runtimes.len())
            .collect::<Vec<Option<EventPluginResult>>>();
        let mut completed_plugins = 0_usize;
        while completed_plugins < outcomes.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match receiver.recv_timeout(remaining) {
                Ok(outcome) if outcome.plugin_index < outcomes.len() => {
                    let index = outcome.plugin_index;
                    if outcomes[index].is_none() {
                        completed_plugins = completed_plugins.saturating_add(1);
                        outcomes[index] = Some(outcome);
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        if completed_plugins < outcomes.len() {
            scheduler.cancel_batch(&batch);
        }

        let mut actions = Vec::new();
        let mut action_bytes = 0_usize;
        for outcome in outcomes.into_iter().flatten() {
            match outcome.invocation {
                ScheduledInvocation::Finished(Ok(reports)) => {
                    for report in reports {
                        if let Some(error) = report.error {
                            self.record_error(
                                error.with_context(format!("event handler: {}", report.handler)),
                            );
                        } else {
                            for action in report.actions {
                                if actions.len() == MAX_EXTENSION_EVENT_ACTIONS {
                                    self.record_error(host_count_limit_error(
                                        "extension event actions",
                                        actions.len().saturating_add(1),
                                        MAX_EXTENSION_EVENT_ACTIONS,
                                    ));
                                    break;
                                }
                                let bytes = serde_json::to_vec(&action)
                                    .map(|encoded| encoded.len())
                                    .unwrap_or(MAX_EXTENSION_EVENT_BYTES.saturating_add(1));
                                action_bytes = action_bytes.saturating_add(bytes);
                                if action_bytes > MAX_EXTENSION_EVENT_BYTES {
                                    self.record_error(
                                        ShellError::new(
                                            ErrorCode::ResourceLimit,
                                            "extension event actions exceed their retained byte limit",
                                        )
                                        .with_context(format!(
                                            "bytes: {action_bytes}; limit: {MAX_EXTENSION_EVENT_BYTES}"
                                        ))
                                        .with_help("Return fewer or smaller extension event actions"),
                                    );
                                    break;
                                }
                                actions.push(action);
                            }
                        }
                    }
                }
                ScheduledInvocation::Finished(Err(error)) => {
                    self.record_error(error.with_context("extension event dispatch"));
                }
                ScheduledInvocation::Cancelled => {}
            }
        }
        if completed_plugins < self.plugin_runtimes.len() {
            let error = ShellError::new(
                ErrorCode::ResourceLimit,
                "extension event dispatch reached its aggregate deadline",
            )
            .with_context(format!(
                "completed plugins: {completed_plugins}; total plugins: {}; deadline: {} ms",
                self.plugin_runtimes.len(),
                EXTENSION_EVENT_WALL_TIME.as_millis()
            ))
            .with_help("Disable blocked event providers before retrying this host transition");
            self.record_error(error.clone());
            return Err(error);
        }
        Ok(actions)
    }

    /// Cancel queued and active callbacks for the installed generation and
    /// return a detached wait handle for a pre-execution safe point.
    pub(crate) fn begin_callback_quiescence(&mut self) -> ExtensionCallbackQuiescence {
        self.poll_prompt_refresh();
        if let Some(refresh) = self.prompt_refresh.take() {
            if let Some(scheduler) = &self.scheduler {
                scheduler.cancel_batch(&refresh.batch);
            }
        }
        let scheduler = self.scheduler.as_ref().map(ExtensionScheduler::handle);
        if let Some(scheduler) = &scheduler {
            scheduler.cancel_generation(self.revision);
        }
        ExtensionCallbackQuiescence {
            scheduler,
            generation: self.revision,
        }
    }

    fn record_error(&mut self, error: ShellError) {
        if self.errors.len() < MAX_RETAINED_EXTENSION_ERRORS {
            self.errors.push(error);
        } else {
            self.error_overflow_count = self.error_overflow_count.saturating_add(1);
        }
    }

    fn ensure_loaded(&mut self) {
        if self.observed_fingerprint.is_none() {
            self.reload_if_changed();
        }
    }

    fn snapshot_sources(&self, cancellation: &AtomicBool) -> SourceSnapshot {
        let mut errors = Vec::new();
        let (config, config_fingerprint) = match &self.config_path {
            Some(path) => match fingerprint_file(path) {
                Ok(fingerprint @ FileFingerprint::Contents { .. }) => {
                    (Some(path.clone()), Some(fingerprint))
                }
                Ok(fingerprint) => (None, Some(fingerprint)),
                Err(error) => {
                    errors.push(error);
                    (
                        None,
                        Some(FileFingerprint::Unreadable(
                            "unable to read config.lua".to_owned(),
                        )),
                    )
                }
            },
            None => (None, None),
        };

        let (plugins, plugins_fingerprint) = match &self.plugin_source {
            PluginSource::Fixed(paths) => snapshot_legacy_plugin_paths(paths, &mut errors),
            #[cfg(test)]
            PluginSource::Directory(directory) => match fs::read_dir(directory) {
                Ok(entries) => {
                    let mut paths = Vec::new();
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                let path = entry.path();
                                if path.extension().is_some_and(|extension| extension == "lua") {
                                    paths.push(path);
                                    if paths.len() > MAX_PLUGIN_CANDIDATES {
                                        errors.push(host_count_limit_error(
                                            "extension directory plugin candidates",
                                            paths.len(),
                                            MAX_PLUGIN_CANDIDATES,
                                        ));
                                        break;
                                    }
                                }
                            }
                            Err(error) => {
                                errors.push(io_error(directory, error));
                                return SourceSnapshot {
                                    fingerprint: ExtensionFingerprint {
                                        config: config_fingerprint,
                                        plugins: PluginFingerprint::UnreadableDirectory(
                                            "unable to enumerate plugins directory".to_owned(),
                                        ),
                                    },
                                    config,
                                    plugins: Vec::new(),
                                    errors,
                                };
                            }
                        }
                    }
                    paths.sort();
                    snapshot_legacy_plugin_paths(&paths, &mut errors)
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    (Vec::new(), PluginFingerprint::Files(Vec::new()))
                }
                Err(error) => {
                    errors.push(io_error(directory, error));
                    (
                        Vec::new(),
                        PluginFingerprint::UnreadableDirectory(
                            "unable to read plugins directory".to_owned(),
                        ),
                    )
                }
            },
            PluginSource::Managed(root) => {
                snapshot_managed_plugins(root, &mut errors, cancellation, true)
            }
        };

        SourceSnapshot {
            fingerprint: ExtensionFingerprint {
                config: config_fingerprint,
                plugins: plugins_fingerprint,
            },
            config,
            plugins,
            errors,
        }
    }

    fn build_candidate(
        &self,
        snapshot: SourceSnapshot,
        cancellation: &AtomicBool,
    ) -> Result<BuiltExtensionGeneration, ShellError> {
        if let Some(error) = snapshot.errors.into_iter().next() {
            return Err(error);
        }
        if snapshot.plugins.len() > MAX_PLUGIN_CANDIDATES {
            return Err(host_count_limit_error(
                "extension plugin candidates",
                snapshot.plugins.len(),
                MAX_PLUGIN_CANDIDATES,
            ));
        }
        let runtime_count = snapshot
            .plugins
            .iter()
            .filter(|plugin| plugin.runtime == PluginRuntime::TrustedLua)
            .count();
        if runtime_count > MAX_LOADED_PLUGIN_RUNTIMES {
            return Err(host_count_limit_error(
                "loaded trusted-Lua plugin runtimes",
                runtime_count,
                MAX_LOADED_PLUGIN_RUNTIMES,
            ));
        }
        let source_bytes = snapshot.plugins.iter().try_fold(0_usize, |total, plugin| {
            total.checked_add(plugin.source_bytes).ok_or_else(|| {
                host_byte_limit_error(
                    "extension generation source",
                    usize::MAX,
                    MAX_PLUGIN_GENERATION_SOURCE_BYTES,
                )
            })
        })?;
        if source_bytes > MAX_PLUGIN_GENERATION_SOURCE_BYTES {
            return Err(host_byte_limit_error(
                "extension generation source",
                source_bytes,
                MAX_PLUGIN_GENERATION_SOURCE_BYTES,
            ));
        }

        let mut config = ConfigStore::default();
        if let Some(path) = &snapshot.config {
            let runtime = LuaRuntime::new(LuaPolicy::config())?;
            config.install(runtime.load_config_file(path)?)?;
        }

        let mut plugin_runtimes = Vec::with_capacity(snapshot.plugins.len());
        let mut command_bindings = Vec::new();
        let mut contributions = Vec::new();
        let mut managed_commands = Vec::new();
        let mut prompt_segments = 0_usize;
        let mut event_handlers = 0_usize;
        for plugin in &snapshot.plugins {
            if let Some(adapter) = &plugin.adapter_activation {
                crate::plugin::execute_out_of_process_adapter(
                    &adapter.manifest,
                    &plugin.path,
                    &adapter.entry_bytes,
                    &plugin.grants,
                    Some(cancellation),
                )?;
            }
            managed_commands.extend(plugin.catalog_commands.clone());
            if managed_commands.len() > MAX_HOST_MANAGED_COMMANDS {
                return Err(host_count_limit_error(
                    "managed plugin commands",
                    managed_commands.len(),
                    MAX_HOST_MANAGED_COMMANDS,
                ));
            }
            if plugin.runtime == PluginRuntime::TrustedLua {
                let runtime = crate::plugin::trusted_lua_runtime(&plugin.grants)?;
                let registrations = if let Some(source) = &plugin.verified_source {
                    runtime.load_plugin_source(source, &plugin.path.display().to_string())?
                } else {
                    runtime.load_plugin_file(&plugin.path)?
                };
                prompt_segments =
                    prompt_segments.saturating_add(registrations.prompt_segments.len());
                event_handlers = event_handlers.saturating_add(registrations.events.len());
                contributions.extend(registrations.contributions.clone());
                if prompt_segments > MAX_HOST_PROMPT_SEGMENTS {
                    return Err(host_count_limit_error(
                        "extension prompt segments",
                        prompt_segments,
                        MAX_HOST_PROMPT_SEGMENTS,
                    ));
                }
                if event_handlers > MAX_HOST_EVENT_HANDLERS {
                    return Err(host_count_limit_error(
                        "extension event handlers",
                        event_handlers,
                        MAX_HOST_EVENT_HANDLERS,
                    ));
                }
                if contributions.len() > MAX_HOST_CONTRIBUTIONS {
                    return Err(host_count_limit_error(
                        "extension contributions",
                        contributions.len(),
                        MAX_HOST_CONTRIBUTIONS,
                    ));
                }
                validate_registered_commands(&plugin.catalog_commands, &registrations.commands)?;
                let allowed_effects = granted_execution_effects(&plugin.grants);
                let runtime = Arc::new(ExtensionRuntimeSlot::new(runtime, registrations)?);
                for command in &plugin.catalog_commands {
                    command_bindings.push(PluginCommandBinding {
                        command: command.clone(),
                        runtime: Arc::clone(&runtime),
                        allowed_effects,
                    });
                }
                plugin_runtimes.push(runtime);
            }
        }
        validate_contribution_set(&contributions)?;
        command_bindings.sort_by(|left, right| left.command.path.cmp(&right.command.path));
        Ok((
            config,
            snapshot
                .plugins
                .into_iter()
                .map(|plugin| plugin.path)
                .collect(),
            plugin_runtimes,
            managed_commands,
            command_bindings,
        ))
    }
}

pub struct LuaCompletionAdapter {
    host: SharedLuaExtensions,
}

impl LuaCompletionAdapter {
    pub fn new(host: SharedLuaExtensions) -> Self {
        Self { host }
    }
}

impl ExtensionCompleter for LuaCompletionAdapter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<ExtensionSuggestion> {
        self.host
            .lock()
            .map(|mut host| host.complete(line, pos))
            .unwrap_or_default()
    }
}

/// Merge enabled installed command manifests without evaluating plugin source
/// or invoking a contribution callback.
pub(crate) fn merge_installed_catalog_snapshot(catalog: &mut Catalog) -> Result<(), ShellError> {
    let commands = installed_plugin_commands()?;
    validate_catalog_contribution(catalog, &commands)?;
    catalog.merge(commands);
    Ok(())
}

/// Resolve one simple installed plugin invocation from the nonexecuting catalog
/// snapshot. Native commands and explicit dialect islands return `None`.
pub(crate) fn resolve_installed_plugin_command(
    source: &str,
) -> Result<Option<InstalledPluginCommand>, ShellError> {
    let commands = installed_plugin_commands()?;
    if commands
        .iter()
        .any(|command| literal_invocation_starts_with(source, &command.path))
    {
        return bind_plugin_invocation(&commands, source);
    }
    let first_word = source.split_whitespace().next();
    if first_word
        .is_some_and(|name| disabled_plugin_names().is_ok_and(|names| names.contains(name)))
    {
        let name = first_word.unwrap_or_default();
        return Err(ShellError::new(
            ErrorCode::InvalidCommand,
            format!("plugin `{name}` is disabled"),
        )
        .with_command(source)
        .with_help(format!(
            "Run `quirl plugin enable {name}` after reviewing its permissions"
        )));
    }
    Ok(None)
}

fn literal_invocation_starts_with(source: &str, command_path: &str) -> bool {
    let mut source_words = source.split_whitespace();
    command_path
        .split_whitespace()
        .all(|part| source_words.next() == Some(part))
}

fn installed_plugin_commands() -> Result<Vec<CommandSpec>, ShellError> {
    let Some(root) = plugin_state_directory() else {
        return Ok(Vec::new());
    };
    installed_plugin_commands_from_root(&root)
}

fn installed_plugin_commands_from_root(root: &Path) -> Result<Vec<CommandSpec>, ShellError> {
    let mut errors = Vec::new();
    let (plugins, _) = snapshot_managed_plugins(root, &mut errors, &AtomicBool::new(false), false);
    if let Some(error) = errors.into_iter().next() {
        return Err(error.with_context("installed plugin catalog snapshot"));
    }
    let mut commands = plugins
        .into_iter()
        .flat_map(|plugin| plugin.catalog_commands)
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| left.path.cmp(&right.path));
    if commands.len() > MAX_HOST_MANAGED_COMMANDS {
        return Err(host_count_limit_error(
            "installed plugin catalog commands",
            commands.len(),
            MAX_HOST_MANAGED_COMMANDS,
        ));
    }
    // An empty snapshot has no metadata or collision semantics to validate.
    // Skipping it avoids constructing a second builtin catalog during the
    // common no-installed-plugin startup path; the composed catalog owner also
    // validates the complete contribution immediately before merging it.
    if !commands.is_empty() {
        validate_catalog_contribution(&Catalog::builtin(), &commands)?;
    }
    Ok(commands)
}

fn disabled_plugin_names() -> Result<BTreeSet<String>, ShellError> {
    let Some(root) = plugin_state_directory() else {
        return Ok(BTreeSet::new());
    };
    let path = root.join(PLUGIN_LOCK_FILE);
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let bytes = read_bounded_plugin_file(&path, MAX_PLUGIN_LOCK_BYTES, "plugin lockfile")?;
    let lock = PluginLockfile::from_json(&bytes).map_err(|error| {
        ShellError::new(ErrorCode::Validation, "managed plugin lockfile is corrupt")
            .with_context(error.to_string())
            .with_help("Restore plugins.lock.json.bak or re-add plugins after review")
    })?;
    lock.validate()?;
    Ok(lock
        .plugins
        .into_iter()
        .filter(|plugin| !plugin.enabled)
        .map(|plugin| plugin.name)
        .collect())
}

fn bind_plugin_invocation(
    commands: &[CommandSpec],
    source: &str,
) -> Result<Option<InstalledPluginCommand>, ShellError> {
    ExecutionSource::new("<plugin-resolution>", source)?;
    let parsed = parse_command_list(source).map_err(|diagnostic| {
        ShellError::new(ErrorCode::InvalidCommand, diagnostic.message)
            .with_label(
                Some(source.to_owned()),
                diagnostic.start,
                diagnostic.end,
                "invalid plugin command invocation",
            )
            .with_help(diagnostic.help)
    })?;
    let first_words = parsed
        .pipelines
        .first()
        .and_then(|pipeline| pipeline.commands.first())
        .map(|command| command.words.as_slice())
        .unwrap_or_default();
    let matched = commands
        .iter()
        .filter_map(|command| {
            let path = command.path.split_whitespace().collect::<Vec<_>>();
            first_words
                .starts_with(
                    &path
                        .iter()
                        .map(|part| (*part).to_owned())
                        .collect::<Vec<_>>(),
                )
                .then_some((path.len(), command))
        })
        .max_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| right.1.id.cmp(&left.1.id))
        });
    let Some((path_words, command)) = matched else {
        return Ok(None);
    };
    let simple = parsed.pipelines.len() == 1
        && parsed.connectors.is_empty()
        && parsed.pipelines[0].commands.len() == 1
        && !parsed.pipelines[0].background
        && parsed.pipelines[0].commands[0].redirects.is_empty();
    if !simple {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "plugin command `{}` requires a single typed invocation",
                command.path
            ),
        )
        .with_command(source)
        .with_help("Remove byte pipes, redirects, backgrounding, and command-list operators"));
    }
    let arguments = first_words[path_words..].to_vec();
    validate_plugin_arguments(command, &arguments)?;
    Ok(Some(InstalledPluginCommand {
        command: command.clone(),
        arguments,
    }))
}

fn validate_plugin_arguments(
    command: &CommandSpec,
    arguments: &[String],
) -> Result<(), ShellError> {
    let positionals = command
        .options
        .iter()
        .filter(|argument| argument.kind == ArgumentKind::Positional)
        .collect::<Vec<_>>();
    let mut positional_index = 0_usize;
    let mut observed = BTreeMap::<String, usize>::new();
    let mut index = 0_usize;
    while index < arguments.len() {
        let token = &arguments[index];
        let (specification, consumed_value) = if token.starts_with('-') && token != "-" {
            let (name, inline_value) = token
                .split_once('=')
                .map_or((token.as_str(), None), |(name, value)| (name, Some(value)));
            let specification = command
                .options
                .iter()
                .find(|argument| {
                    argument.kind != ArgumentKind::Positional
                        && argument.names.iter().any(|candidate| candidate == name)
                })
                .ok_or_else(|| {
                    plugin_argument_error(command, format!("unknown option `{name}`"))
                })?;
            match specification.kind {
                ArgumentKind::Flag if inline_value.is_some() => {
                    return Err(plugin_argument_error(
                        command,
                        format!("flag `{name}` does not accept a value"),
                    ));
                }
                ArgumentKind::Flag => (specification, None),
                ArgumentKind::Option => {
                    let value = match inline_value {
                        Some(value) => value,
                        None => {
                            index = index.saturating_add(1);
                            arguments.get(index).map(String::as_str).ok_or_else(|| {
                                plugin_argument_error(
                                    command,
                                    format!("option `{name}` requires a value"),
                                )
                            })?
                        }
                    };
                    (specification, Some(value))
                }
                ArgumentKind::Positional => unreachable!("named lookup excluded positionals"),
            }
        } else {
            let specification = positionals.get(positional_index).copied().ok_or_else(|| {
                plugin_argument_error(command, format!("unexpected positional argument `{token}`"))
            })?;
            if !specification.repeatable {
                positional_index = positional_index.saturating_add(1);
            }
            (specification, Some(token.as_str()))
        };
        if let Some(value) = consumed_value {
            validate_plugin_argument_value(command, specification, value)?;
        }
        let canonical = specification.names.first().cloned().unwrap_or_default();
        let count = observed.entry(canonical.clone()).or_default();
        *count = count.saturating_add(1);
        if *count > 1 && !specification.repeatable {
            return Err(plugin_argument_error(
                command,
                format!("argument `{canonical}` is not repeatable"),
            ));
        }
        index = index.saturating_add(1);
    }
    for specification in &command.options {
        let canonical = specification.names.first().cloned().unwrap_or_default();
        if specification.required && !observed.contains_key(&canonical) {
            return Err(plugin_argument_error(
                command,
                format!("required argument `{canonical}` is missing"),
            ));
        }
        if observed.contains_key(&canonical) {
            for conflict in &specification.conflicts {
                if observed.contains_key(conflict) {
                    return Err(plugin_argument_error(
                        command,
                        format!("arguments `{canonical}` and `{conflict}` conflict"),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn validate_plugin_argument_value(
    command: &CommandSpec,
    specification: &quirl_catalog::ArgumentSpec,
    value: &str,
) -> Result<(), ShellError> {
    if let Some(quirl_catalog::CompletionSource::Static { values }) = &specification.values {
        if !values.iter().any(|candidate| candidate == value) {
            return Err(plugin_argument_error(
                command,
                format!(
                    "argument `{}` must be one of: {}",
                    specification.names.first().cloned().unwrap_or_default(),
                    values.join(", ")
                ),
            ));
        }
    }
    let normalized = specification.value_type.to_ascii_lowercase();
    let valid = match normalized.as_str() {
        "int" | "integer" => value.parse::<i64>().is_ok(),
        "uint" | "unsigned-integer" => value.parse::<u64>().is_ok(),
        "positive-integer" => value.parse::<u64>().is_ok_and(|number| number > 0),
        "bool" | "boolean" => matches!(value, "true" | "false"),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(plugin_argument_error(
            command,
            format!(
                "argument `{}` is not a valid {}",
                specification.names.first().cloned().unwrap_or_default(),
                specification.value_type
            ),
        ))
    }
}

fn plugin_argument_error(command: &CommandSpec, message: String) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidArgument,
        format!("invalid arguments for plugin command `{}`", command.path),
    )
    .with_context(message)
    .with_help(format!("Expected: {}", command.signature))
}

fn validate_registered_commands(
    commands: &[CommandSpec],
    registrations: &[CommandRegistration],
) -> Result<(), ShellError> {
    let declared = commands
        .iter()
        .map(|command| command.path.as_str())
        .collect::<BTreeSet<_>>();
    let registered = registrations
        .iter()
        .map(|registration| registration.name.as_str())
        .collect::<BTreeSet<_>>();
    if declared != registered {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "trusted-Lua command registrations differ from the locked manifest",
        )
        .with_context(format!(
            "declared: {declared:?}; registered: {registered:?}"
        ))
        .with_help("Register every manifest command exactly once and no undeclared commands"));
    }
    for command in commands {
        let registration = registrations
            .iter()
            .find(|registration| registration.name == command.path)
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::Validation,
                    "plugin command registration disappeared",
                )
                .with_context(format!("command: {}", command.path))
                .with_help("Reload the plugin from its locked source")
            })?;
        let registered_effects = registration
            .effects
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let catalog_effects = command
            .effects
            .iter()
            .map(|effect| catalog_effect_name(*effect).to_owned())
            .collect::<BTreeSet<_>>();
        let registered_exits = registration
            .error_codes
            .iter()
            .map(|(code, description)| {
                code.parse::<i32>()
                    .map(|code| (code, description.clone()))
                    .map_err(|_| {
                        ShellError::new(
                            ErrorCode::Validation,
                            format!(
                                "plugin command `{}` registered non-numeric status `{code}`",
                                command.path
                            ),
                        )
                        .with_help("Use the same signed integer status keys as plugin.toml")
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let matches = registration.signature == command.signature
            && registration.summary == command.summary
            && registration.details == command.details
            && registration.input_type == command.io.input
            && registration.output_type == command.io.output
            && registration.examples == command.examples
            && registered_effects == catalog_effects
            && registered_exits == command.exit_codes;
        if !matches {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "plugin command `{}` registration does not match its locked manifest",
                    command.path
                ),
            )
            .with_help("Keep Lua registration metadata byte-for-byte aligned with plugin.toml"));
        }
    }
    Ok(())
}

fn granted_execution_effects(grants: &[String]) -> ExecutionEffects {
    let effects = [
        grants
            .iter()
            .any(|grant| grant.starts_with("filesystem.read:"))
            .then_some(ExecutionEffect::ReadFilesystem),
        grants
            .iter()
            .any(|grant| grant.starts_with("filesystem.write:"))
            .then_some(ExecutionEffect::WriteFilesystem),
        grants
            .iter()
            .any(|grant| grant == "process.spawn" || grant.starts_with("process.spawn:"))
            .then_some(ExecutionEffect::SpawnProcess),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    ExecutionEffects::from_effects(&effects)
}

fn catalog_execution_effects(effects: &[Effect]) -> ExecutionEffects {
    let effects = effects
        .iter()
        .map(|effect| match effect {
            Effect::ReadFilesystem => ExecutionEffect::ReadFilesystem,
            Effect::WriteFilesystem => ExecutionEffect::WriteFilesystem,
            Effect::SpawnProcess => ExecutionEffect::SpawnProcess,
            Effect::ChangeDirectory => ExecutionEffect::ChangeDirectory,
        })
        .collect::<Vec<_>>();
    ExecutionEffects::from_effects(&effects)
}

fn catalog_effect_name(effect: Effect) -> &'static str {
    match effect {
        Effect::ReadFilesystem => "read_filesystem",
        Effect::WriteFilesystem => "write_filesystem",
        Effect::SpawnProcess => "spawn_process",
        Effect::ChangeDirectory => "change_directory",
    }
}

fn validate_plugin_input(
    command: &CommandSpec,
    input: &ExecutionInput,
    source: &str,
    plan: Option<&ExecutionPlan>,
) -> Result<(), ShellError> {
    let contract = ValueInputContract::parse_exact(&command.io.input).ok_or_else(|| {
        ShellError::new(
            ErrorCode::Validation,
            format!(
                "plugin command `{}` has an unsupported executable input contract",
                command.path
            ),
        )
        .with_command(source)
        .with_context(format!("declared input: {}", command.io.input))
        .with_help("Re-add the plugin with a manifest-v2 Nothing or exact value-kind input")
    })?;
    if !contract.matches(input) {
        let (message, observed) = match (contract, input) {
            (ValueInputContract::Nothing, _) => (
                format!(
                    "plugin command `{}` does not accept pipeline input",
                    command.path
                ),
                plugin_input_name(input),
            ),
            (ValueInputContract::Value(_), ExecutionInput::None) => (
                format!("plugin command `{}` requires pipeline input", command.path),
                "missing".to_owned(),
            ),
            (ValueInputContract::Value(_), _) => (
                format!("plugin command `{}` input type did not match", command.path),
                plugin_input_name(input),
            ),
        };
        let expected = match contract {
            ValueInputContract::Nothing => "Nothing",
            ValueInputContract::Value(kind) => kind.name(),
        };
        return Err(ShellError::new(ErrorCode::Validation, message)
            .with_command(source)
            .with_context(format!("expected: {expected}; observed: {observed}"))
            .with_help("Supply exactly the typed input declared by the installed plugin command"));
    }
    if let ExecutionInput::Value(value) = input {
        let mut budget = PluginValueBudget::default();
        validate_plugin_value(value, "plugin command input", &mut budget, plan)?;
    }
    Ok(())
}

fn plugin_input_name(input: &ExecutionInput) -> String {
    match input {
        ExecutionInput::None => "missing".to_owned(),
        ExecutionInput::Bytes(_) => "Bytes".to_owned(),
        ExecutionInput::Value(value) => StructuredValueKind::of(value).name().to_owned(),
    }
}

#[derive(Default)]
struct PluginValueBudget {
    nodes: usize,
    fields: usize,
    text_bytes: usize,
}

fn validate_plugin_value(
    value: &StructuredValue,
    description: &str,
    budget: &mut PluginValueBudget,
    plan: Option<&ExecutionPlan>,
) -> Result<(), ShellError> {
    let mut stack = vec![(value, 0_usize)];
    while let Some((value, depth)) = stack.pop() {
        if let Some(plan) = plan {
            plan.ensure_active("during plugin value conversion")?;
        }
        budget.nodes = budget.nodes.saturating_add(1);
        validate_plugin_value_limit(description, "nodes", budget.nodes, MAX_PLUGIN_VALUE_NODES)?;
        match value {
            StructuredValue::Decimal(value)
            | StructuredValue::String(value)
            | StructuredValue::Path(value)
            | StructuredValue::DateTime(value)
            | StructuredValue::Pattern(value) => {
                budget.text_bytes = budget.text_bytes.saturating_add(value.len());
            }
            StructuredValue::List(values) => {
                validate_plugin_container_depth(description, depth)?;
                stack.extend(values.iter().rev().map(|value| (value, depth + 1)));
            }
            StructuredValue::Record(values) => {
                validate_plugin_container_depth(description, depth)?;
                budget.fields = budget.fields.saturating_add(values.len());
                validate_plugin_value_limit(
                    description,
                    "record fields",
                    budget.fields,
                    MAX_PLUGIN_VALUE_FIELDS,
                )?;
                for (key, value) in values.iter().rev() {
                    budget.text_bytes = budget.text_bytes.saturating_add(key.len());
                    stack.push((value, depth + 1));
                }
            }
            StructuredValue::Nothing
            | StructuredValue::Bool(_)
            | StructuredValue::Int(_)
            | StructuredValue::UInt(_)
            | StructuredValue::Duration { .. }
            | StructuredValue::Size { .. } => {}
        }
        validate_plugin_value_limit(
            description,
            "string and key bytes",
            budget.text_bytes,
            MAX_PLUGIN_VALUE_TEXT_BYTES,
        )?;
    }
    Ok(())
}

fn validate_plugin_container_depth(description: &str, depth: usize) -> Result<(), ShellError> {
    if depth < MAX_PLUGIN_VALUE_DEPTH {
        return Ok(());
    }
    validate_plugin_value_limit(
        description,
        "depth",
        depth.saturating_add(1),
        MAX_PLUGIN_VALUE_DEPTH,
    )
}

fn validate_plugin_value_limit(
    description: &str,
    unit: &str,
    observed: usize,
    limit: usize,
) -> Result<(), ShellError> {
    if observed <= limit {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{description} exceeded its Lua ABI-v1 conversion limit"),
    )
    .with_context(format!("{unit}: {observed}; limit: {limit}"))
    .with_help("Reduce the typed value before invoking the plugin command"))
}

fn validate_plugin_outcome(
    command: &CommandSpec,
    outcome: &ExecutionOutcome,
) -> Result<(), ShellError> {
    if !command.exit_codes.contains_key(&outcome.status_code()) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "plugin command `{}` returned undocumented status {}",
                command.path,
                outcome.status_code()
            ),
        )
        .with_help(
            "Declare every possible signed status in plugin.toml and the Lua registration",
        ));
    }
    let contract = ValueOutputContract::parse_exact(&command.io.output).ok_or_else(|| {
        ShellError::new(
            ErrorCode::Validation,
            format!(
                "plugin command `{}` has an unsupported executable output contract",
                command.path
            ),
        )
        .with_context(format!("declared output: {}", command.io.output))
        .with_help("Re-add the plugin with one exact value kind or bounded `Values<T>` output")
    })?;
    if !contract.matches(&outcome.output) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "plugin command `{}` returned output that does not match `{}`",
                command.path, command.io.output
            ),
        )
        .with_help(
            "Return exactly the top-level value kind and finite shape declared in plugin.toml",
        ));
    }
    let mut budget = PluginValueBudget::default();
    match &outcome.output {
        ExecutionOutput::Value { value } => {
            validate_plugin_value(value, "plugin command output", &mut budget, None)
        }
        ExecutionOutput::Values { values } => {
            if values.len() > MAX_LUA_RUNNER_STREAM_VALUES {
                return validate_plugin_value_limit(
                    "plugin command output batch",
                    "values",
                    values.len(),
                    MAX_LUA_RUNNER_STREAM_VALUES,
                );
            }
            for value in values {
                validate_plugin_value(value, "plugin command output batch", &mut budget, None)?;
            }
            Ok(())
        }
        ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => {
            unreachable!("the closed value output contract rejected byte-oriented output")
        }
    }
}

fn stale_plugin_snapshot_error(command: &CommandSpec, source: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("plugin command `{}` changed after discovery", command.path),
    )
    .with_command(source)
    .with_context(format!("command id: {}", command.id))
    .with_help("Retry against a fresh installed catalog snapshot")
}

fn unavailable_plugin_scheduler_error() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "plugin command scheduler is unavailable",
    )
    .with_help("Restart Quirl; disable the plugin if scheduler startup fails again")
}

fn config_directory() -> Option<PathBuf> {
    resolve_config_directory(
        env::var_os("QUIRL_CONFIG_DIR"),
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
    )
}

fn plugin_state_directory() -> Option<PathBuf> {
    resolve_plugin_state_directory(
        env::var_os("QUIRL_PLUGIN_HOME"),
        env::var_os("QUIRL_CONFIG_DIR"),
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
    )
}

pub(crate) fn resolve_config_directory(
    config_dir: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    config_dir
        .map(PathBuf::from)
        .or_else(|| xdg_config_home.map(|path| PathBuf::from(path).join("quirl")))
        .or_else(|| home.map(|path| PathBuf::from(path).join(".config/quirl")))
}

pub(crate) fn resolve_plugin_state_directory(
    plugin_home: Option<OsString>,
    config_dir: Option<OsString>,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    plugin_home.map(PathBuf::from).or_else(|| {
        resolve_config_directory(config_dir, xdg_config_home, home)
            .map(|directory| directory.join("plugins"))
    })
}

fn snapshot_legacy_plugin_paths(
    paths: &[PathBuf],
    errors: &mut Vec<ShellError>,
) -> (Vec<PluginCandidate>, PluginFingerprint) {
    if paths.len() > MAX_PLUGIN_CANDIDATES {
        errors.push(host_count_limit_error(
            "extension plugin candidates",
            paths.len(),
            MAX_PLUGIN_CANDIDATES,
        ));
        return (Vec::new(), PluginFingerprint::Files(Vec::new()));
    }
    let mut fingerprints = Vec::with_capacity(paths.len());
    for path in paths {
        match fingerprint_file(path) {
            Ok(fingerprint) => fingerprints.push((path.clone(), fingerprint)),
            Err(error) => {
                errors.push(error);
                fingerprints.push((
                    path.clone(),
                    FileFingerprint::Unreadable("unable to read plugin".to_owned()),
                ));
            }
        }
    }
    let grants = legacy_registration_grants();
    (
        fingerprints
            .iter()
            .map(|(path, fingerprint)| PluginCandidate {
                path: path.clone(),
                verified_source: None,
                source_bytes: match fingerprint {
                    FileFingerprint::Contents { bytes, .. } => *bytes,
                    FileFingerprint::Missing | FileFingerprint::Unreadable(_) => 0,
                },
                runtime: PluginRuntime::TrustedLua,
                grants: grants.clone(),
                catalog_commands: Vec::new(),
                adapter_activation: None,
            })
            .collect(),
        PluginFingerprint::Files(fingerprints),
    )
}

fn snapshot_managed_plugins(
    root: &Path,
    errors: &mut Vec<ShellError>,
    cancellation: &AtomicBool,
    activate_adapters: bool,
) -> (Vec<PluginCandidate>, PluginFingerprint) {
    let lock_path = root.join(PLUGIN_LOCK_FILE);
    let mut fingerprints = vec![(
        lock_path.clone(),
        fingerprint_file(&lock_path).unwrap_or_else(|error| {
            errors.push(error);
            FileFingerprint::Unreadable("unable to read managed plugin lock".to_owned())
        }),
    )];
    if !lock_path.exists() {
        return (Vec::new(), PluginFingerprint::Files(fingerprints));
    }
    let bytes = match read_bounded_plugin_file(&lock_path, MAX_PLUGIN_LOCK_BYTES, "plugin lockfile")
    {
        Ok(bytes) => bytes,
        Err(error) => {
            errors.push(error);
            return (Vec::new(), PluginFingerprint::Files(fingerprints));
        }
    };
    let lock = match PluginLockfile::from_json(&bytes) {
        Ok(lock) => lock,
        Err(error) => {
            errors.push(
                ShellError::new(ErrorCode::Validation, "managed plugin lockfile is corrupt")
                    .with_context(error.to_string())
                    .with_help("Restore plugins.lock.json.bak or re-add plugins after review"),
            );
            return (Vec::new(), PluginFingerprint::Files(fingerprints));
        }
    };
    if let Err(error) = lock.validate() {
        errors.push(error);
        return (Vec::new(), PluginFingerprint::Files(fingerprints));
    }

    let enabled_count = lock.plugins.iter().filter(|plugin| plugin.enabled).count();
    if enabled_count > MAX_PLUGIN_CANDIDATES {
        errors.push(host_count_limit_error(
            "enabled managed plugin candidates",
            enabled_count,
            MAX_PLUGIN_CANDIDATES,
        ));
        return (Vec::new(), PluginFingerprint::Files(fingerprints));
    }

    let mut candidates = Vec::new();
    for locked in lock.plugins.iter().filter(|plugin| plugin.enabled) {
        if cancellation.load(Ordering::Relaxed) {
            errors.push(
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "managed plugin reload was cancelled",
                )
                .with_help("Retry extension reload when cancellation is no longer requested"),
            );
            break;
        }
        match managed_plugin_candidate_with_activation(locked, &mut fingerprints, activate_adapters)
        {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => errors.push(error),
        }
    }
    candidates.sort_by(|left, right| left.path.cmp(&right.path));
    (candidates, PluginFingerprint::Files(fingerprints))
}

#[cfg(test)]
fn managed_plugin_candidate(
    locked: &LockedPlugin,
    fingerprints: &mut Vec<(PathBuf, FileFingerprint)>,
) -> Result<PluginCandidate, ShellError> {
    managed_plugin_candidate_with_activation(locked, fingerprints, true)
}

fn managed_plugin_candidate_with_activation(
    locked: &LockedPlugin,
    fingerprints: &mut Vec<(PathBuf, FileFingerprint)>,
    activate_adapter: bool,
) -> Result<PluginCandidate, ShellError> {
    if locked.runtime == PluginRuntime::WasmComponent {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "enabled plugin `{}` has no executable Wasm component runtime",
                locked.name
            ),
        )
        .with_help("Disable it until a component runtime is installed"));
    }
    let manifest_path = managed_manifest_path(&locked.source)?;
    fingerprints.push((manifest_path.clone(), fingerprint_file(&manifest_path)?));
    let manifest_bytes =
        read_bounded_plugin_file(&manifest_path, MAX_PLUGIN_MANIFEST_BYTES, "plugin manifest")?;
    let manifest_source = String::from_utf8(manifest_bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "managed plugin manifest is not valid UTF-8",
        )
        .with_context(error.to_string())
        .with_help("Encode plugin.toml as UTF-8")
    })?;
    let manifest = parse_plugin_manifest(&manifest_source, &manifest_path.display().to_string())?;
    if manifest.plugin.name != locked.name
        || manifest.plugin.version != locked.version
        || manifest.plugin.runtime != locked.runtime
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "managed plugin `{}` identity differs from its lock",
                locked.name
            ),
        )
        .with_help("Restore the locked source or remove and re-add the plugin after review"));
    }
    let entry = Path::new(&manifest.plugin.entry);
    if entry.is_absolute()
        || entry.as_os_str().is_empty()
        || !entry
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("managed plugin `{}` entry escapes its package", locked.name),
        )
        .with_help("Use a relative entry path without parent components"));
    }
    let package_root = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let entry_path = fs::canonicalize(package_root.join(entry))
        .map_err(|error| io_error(&package_root.join(entry), error))?;
    if !entry_path.starts_with(package_root) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "managed plugin `{}` entry resolves outside its package",
                locked.name
            ),
        )
        .with_help("Keep the entry inside the package; external symlink targets are rejected"));
    }
    fingerprints.push((entry_path.clone(), fingerprint_file(&entry_path)?));
    let entry_bytes =
        read_bounded_plugin_file(&entry_path, MAX_PLUGIN_ENTRY_BYTES, "plugin entry")?;
    let report = doctor_plugin(locked, manifest_source.as_bytes(), &entry_bytes);
    if !report.healthy {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "managed plugin `{}` failed its locked integrity check",
                locked.name
            ),
        )
        .with_context(
            report
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        )
        .with_help("Run `quirl plugin doctor` and restore the locked source before activation"));
    }
    validate_plugin_manifest(&manifest, &entry_bytes, env!("CARGO_PKG_VERSION"))?;
    let verified_source = if locked.runtime == PluginRuntime::TrustedLua {
        Some(
            std::str::from_utf8(&entry_bytes)
                .map(str::to_owned)
                .map_err(|error| {
                    ShellError::new(
                        ErrorCode::Validation,
                        format!("managed plugin `{}` entry is not valid UTF-8", locked.name),
                    )
                    .with_context(error.to_string())
                    .with_help("Encode the locked trusted-Lua entry as UTF-8")
                })?,
        )
    } else {
        None
    };
    let catalog_commands =
        normalize_plugin_commands(&manifest, &locked.source, &locked.source_checksum)?;
    let source_bytes = entry_bytes.len();
    let adapter_activation = (locked.runtime == PluginRuntime::OutOfProcess && activate_adapter)
        .then_some(AdapterActivation {
            manifest,
            entry_bytes,
        });
    Ok(PluginCandidate {
        path: entry_path,
        verified_source,
        source_bytes,
        runtime: locked.runtime,
        grants: locked.granted_capabilities.clone(),
        catalog_commands,
        adapter_activation,
    })
}

fn managed_manifest_path(source: &str) -> Result<PathBuf, ShellError> {
    let value = source.strip_prefix("file:").unwrap_or(source);
    if value.contains(':') && !Path::new(value).exists() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("managed plugin source `{source}` is not local"),
        )
        .with_help("Platform v0.1 activates only locked local file sources"));
    }
    let path = fs::canonicalize(value).map_err(|error| io_error(Path::new(value), error))?;
    if path.is_dir() {
        let manifest = fs::canonicalize(path.join("plugin.toml"))
            .map_err(|error| io_error(&path.join("plugin.toml"), error))?;
        if !manifest.starts_with(&path) {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "managed plugin manifest resolves outside its package",
            )
            .with_help(
                "Keep plugin.toml inside the package; external symlink targets are rejected",
            ));
        }
        Ok(manifest)
    } else {
        Ok(path)
    }
}

fn legacy_registration_grants() -> Vec<String> {
    let grants = [
        "catalog.register",
        "commands.register",
        "completion.register",
        "events.observe",
        "extension.contribute",
        "prompt.register",
        "ui.panel",
    ];
    grants.into_iter().map(str::to_owned).collect()
}

fn fingerprint_file(path: &Path) -> Result<FileFingerprint, ShellError> {
    match read_optional_regular_file(plugin_read_options(
        path,
        MAX_PLUGIN_LOCK_BYTES,
        "plugin source",
    ))? {
        Some(contents) => {
            let mut hasher = DefaultHasher::new();
            contents.hash(&mut hasher);
            Ok(FileFingerprint::Contents {
                bytes: contents.len(),
                hash: hasher.finish(),
            })
        }
        None => Ok(FileFingerprint::Missing),
    }
}

fn read_bounded_plugin_file(
    path: &Path,
    limit: usize,
    context: &str,
) -> Result<Vec<u8>, ShellError> {
    read_regular_file(plugin_read_options(path, limit, context))
}

fn plugin_read_options<'a>(path: &'a Path, limit: usize, context: &'a str) -> ReadFileOptions<'a> {
    ReadFileOptions {
        path,
        bytes_max: limit,
        context,
        help: "Restore the plugin input as a readable regular file within its byte limit",
        io_error_code: ErrorCode::Io,
    }
}

fn io_error(path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not read extension source {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Fix the file or directory permissions, then save the extension again")
}

fn provider_applies(before: &str, command: &str) -> bool {
    before == command
        || before
            .strip_prefix(command)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn extension_suggestion(
    value: Value,
    query: &str,
    replace_start: usize,
    replace_end: usize,
    provider: &str,
) -> Option<ExtensionSuggestion> {
    let (value, display, summary, detail) = match value {
        Value::String(value) => (
            value.clone(),
            value,
            format!("Suggested by {provider}"),
            "Lua completion provider".to_owned(),
        ),
        Value::Object(object) => {
            let value = object.get("value")?.as_str()?.to_owned();
            let display = object
                .get("display")
                .and_then(Value::as_str)
                .unwrap_or(&value)
                .to_owned();
            let summary = object
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or("Lua plugin suggestion")
                .to_owned();
            let detail = object
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or(provider)
                .to_owned();
            (value, display, summary, detail)
        }
        _ => return None,
    };
    if !query.is_empty() && !is_subsequence(query, &value) {
        return None;
    }
    Some(ExtensionSuggestion {
        value,
        display,
        summary,
        detail,
        replace_start,
        replace_end,
    })
}

fn contribution_suggestion(
    item: CompletionContributionItem,
    query: &str,
    replace_start: usize,
    replace_end: usize,
    provider: &str,
) -> Option<ExtensionSuggestion> {
    if item.value.is_empty() || (!query.is_empty() && !is_subsequence(query, &item.value)) {
        return None;
    }
    Some(ExtensionSuggestion {
        display: item.display.unwrap_or_else(|| item.value.clone()),
        summary: item
            .summary
            .unwrap_or_else(|| format!("Suggested by {provider}")),
        detail: item.detail.unwrap_or_else(|| provider.to_owned()),
        value: item.value,
        replace_start,
        replace_end,
    })
}

fn validate_catalog_contribution(
    installed: &Catalog,
    commands: &[CommandSpec],
) -> Result<(), ShellError> {
    let mut paths = installed
        .commands
        .iter()
        .map(|command| command.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut ids = installed
        .commands
        .iter()
        .map(|command| command.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    for command in commands {
        if !paths.insert(&command.path) || !ids.insert(&command.id) {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "plugin catalog command `{}` collides with installed semantic facts",
                    command.path
                ),
            )
            .with_help("Namespace plugin commands and use unique stable command IDs"));
        }
    }
    let candidate = Catalog {
        schema_version: installed.schema_version,
        commands: commands.to_vec(),
    };
    let issues = candidate.quality_issues();
    if issues.is_empty() {
        Ok(())
    } else {
        Err(ShellError::new(
            ErrorCode::Validation,
            "plugin catalog contribution has incomplete exact metadata",
        )
        .with_context(issues.join("; "))
        .with_help("Provide versioned command, argument, I/O, example, effect, and exit metadata"))
    }
}

fn contribution_shape_error(name: &str, expected: &str, error: serde_json::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("extension contribution `{name}` returned the wrong shape"),
    )
    .with_context(error.to_string())
    .with_help(expected)
}

fn validate_cached_panel(
    registration: &ContributionRegistration,
    panel: &PanelModel,
) -> Result<(), ShellError> {
    panel.validate()?;
    if registration.plain_fallback.as_deref() != Some(panel.plain_fallback.as_str()) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "panel contribution `{}` changed its declared plain fallback",
                registration.name
            ),
        )
        .with_help("Return the same plain_fallback declared at registration"));
    }
    if panel.columns.len() > PANEL_COLUMNS_MAX {
        return Err(host_count_limit_error(
            "panel columns",
            panel.columns.len(),
            PANEL_COLUMNS_MAX,
        ));
    }
    if panel.rows.len() > PANEL_ROWS_MAX {
        return Err(host_count_limit_error(
            "panel rows",
            panel.rows.len(),
            PANEL_ROWS_MAX,
        ));
    }
    for field in std::iter::once(&registration.name)
        .chain(std::iter::once(&panel.title))
        .chain(panel.columns.iter())
        .chain(panel.rows.iter().flatten())
        .chain(std::iter::once(&panel.plain_fallback))
    {
        if field.len() > PANEL_FIELD_BYTES_MAX {
            return Err(host_byte_limit_error(
                "panel field",
                field.len(),
                PANEL_FIELD_BYTES_MAX,
            ));
        }
    }
    Ok(())
}

fn panel_bytes(id: &str, panel: &PanelModel) -> usize {
    std::iter::once(id)
        .chain(std::iter::once(panel.title.as_str()))
        .chain(panel.columns.iter().map(String::as_str))
        .chain(panel.rows.iter().flatten().map(String::as_str))
        .chain(std::iter::once(panel.plain_fallback.as_str()))
        .fold(0_usize, |bytes, field| bytes.saturating_add(field.len()))
}

fn panel_cache_limit_error() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "extension panel cache exceeded its count or byte limit",
    )
    .with_context(format!(
        "panels: {PANEL_COUNT_MAX}; retained bytes: {PANEL_GENERATION_BYTES_MAX}"
    ))
    .with_help("Reduce enabled panel providers or shorten their typed output")
}

fn host_count_limit_error(resource: &str, observed: usize, limit: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{resource} exceed the host-side count limit"),
    )
    .with_context(format!("observed: {observed}; limit: {limit}"))
    .with_help("Reduce enabled plugin registrations before reloading extensions")
}

fn host_byte_limit_error(resource: &str, observed: usize, limit: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{resource} exceeds the host-side byte limit"),
    )
    .with_context(format!("bytes: {observed}; limit: {limit}"))
    .with_help("Reduce enabled plugin source or retained callback output")
}

fn unavailable_event_scheduler_error() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "the extension event scheduler is unavailable",
    )
    .with_help("Restart Quirl after reducing process or extension resource pressure")
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    let mut query = query.chars().flat_map(char::to_lowercase);
    let mut expected = query.next();
    for character in candidate.chars().flat_map(char::to_lowercase) {
        if expected == Some(character) {
            expected = query.next();
            if expected.is_none() {
                return true;
            }
        }
    }
    expected.is_none()
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_plugin::resolve_plugin;
    use std::{
        process,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_TEMP_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    fn temporary_extension_directory() -> PathBuf {
        let directory = env::temp_dir().join(format!(
            "quirl-cli-extension-tests-{}-{}",
            process::id(),
            NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(directory.join("plugins")).unwrap();
        directory
    }

    fn write_plugin(path: &Path, segment: &str, completion: &str) {
        fs::write(
            path,
            format!(
                r#"
quirl.prompt.add_segment {{
  name = "{segment}",
  deadline_ms = 8,
  render = function(_ctx)
    return "{segment}-value"
  end,
}}

quirl.completion.add_provider {{
  command = "fruit",
  complete = function(_ctx)
    return {{ "{completion}" }}
  end,
}}
"#,
            ),
        )
        .unwrap();
    }

    fn write_config(path: &Path, keymap: &str) {
        fs::write(
            path,
            format!("return {{ editor = {{ keymap = \"{keymap}\" }} }}"),
        )
        .unwrap();
    }

    fn refreshed_prompt_segments(
        host: &mut LuaExtensionHost,
        mode: Mode,
        last_status: i32,
    ) -> Vec<NamedExtensionSegment> {
        let initial = host.named_prompt_segments(mode, last_status);
        if host.plugin_runtimes.is_empty() {
            return initial;
        }
        let scheduler = host.scheduler.as_ref().unwrap().handle();
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        host.poll_prompt_refresh();
        host.prompt_cache
            .iter()
            .flat_map(|segments| segments.iter().cloned())
            .collect()
    }

    fn write_managed_prompt_plugin(directory: &Path) -> (PathBuf, PluginLockfile) {
        let package = directory.join("managed-package");
        fs::create_dir_all(&package).unwrap();
        let entry = package.join("plugin.lua");
        let entry_source = r#"quirl.prompt.add_segment {
          name = "managed", deadline_ms = 8,
          render = function(_) return "managed-value" end,
        }
        quirl.plugin.command {
          name = "managed run", signature = "managed run", summary = "Run managed",
          details = "Return one managed test value.", input_type = "Nothing",
          output_type = "String", examples = { "managed run" },
          effects = { "read_filesystem" }, error_codes = { ["0"] = "success" },
          run = function(_)
            return { abi_version = 1, ok = true, status = 0,
              output = { kind = "value", value = { type = "string", value = "managed" } } }
          end,
        }
        quirl.plugin.command {
          name = "managed echo", signature = "managed echo", summary = "Echo managed path",
          details = "Return one typed path input.", input_type = "Path",
          output_type = "Path", examples = { "managed echo" },
          effects = { "read_filesystem" }, error_codes = { ["0"] = "success" },
          run = function(ctx)
            return { abi_version = 1, ok = true, status = 0,
              output = { kind = "value", value = ctx.input.content } }
          end,
        }
        quirl.plugin.command {
          name = "managed batch", signature = "managed batch", summary = "Batch managed values",
          details = "Return a bounded finite size batch.", input_type = "Int",
          output_type = "Values<Size>", examples = { "managed batch" },
          effects = { "read_filesystem" }, error_codes = { ["0"] = "success" },
          run = function(ctx)
            local values = {}
            for index = 1, ctx.input.content.value do
              values[index] = { type = "size", value = { bytes = index } }
            end
            return { abi_version = 1, ok = true, status = 0,
              output = { kind = "values", values = values } }
          end,
        }
        quirl.plugin.command {
          name = "managed list", signature = "managed list", summary = "Mirror managed list",
          details = "Return one bounded list input.", input_type = "List",
          output_type = "List", examples = { "managed list" },
          effects = { "read_filesystem" }, error_codes = { ["0"] = "success" },
          run = function(ctx)
            return { abi_version = 1, ok = true, status = 0,
              output = { kind = "value", value = ctx.input.content } }
          end,
        }
        quirl.plugin.command {
          name = "managed record", signature = "managed record", summary = "Mirror managed record",
          details = "Return one bounded record input.", input_type = "Record",
          output_type = "Record", examples = { "managed record" },
          effects = { "read_filesystem" }, error_codes = { ["0"] = "success" },
          run = function(ctx)
            return { abi_version = 1, ok = true, status = 0,
              output = { kind = "value", value = ctx.input.content } }
          end,
        }
        quirl.plugin.command {
          name = "managed text", signature = "managed text", summary = "Mirror managed text",
          details = "Return one bounded string input.", input_type = "String",
          output_type = "String", examples = { "managed text" },
          effects = { "read_filesystem" }, error_codes = { ["0"] = "success" },
          run = function(ctx)
            return { abi_version = 1, ok = true, status = 0,
              output = { kind = "value", value = ctx.input.content } }
          end,
        }"#;
        fs::write(&entry, entry_source).unwrap();
        let manifest_path = package.join("plugin.toml");
        let manifest_source = r#"schema_version = 2

[plugin]
name = "managed"
version = "0.1.0"
entry = "plugin.lua"
quirl = ">=0.1, <0.2"
api = "0.1.0"
runtime = "trusted_lua"
summary = "Managed prompt test"

[capabilities]
request = ["commands.register", "filesystem.read:.", "prompt.register"]

[contributes]
commands = ["managed batch", "managed echo", "managed list", "managed record", "managed run", "managed text"]

[[public_commands]]
path = "managed batch"
signature = "managed batch"
summary = "Batch managed values"
details = "Return a bounded finite size batch."
input_type = "Int"
output_type = "Values<Size>"
examples = ["managed batch"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }

[[public_commands]]
path = "managed echo"
signature = "managed echo"
summary = "Echo managed path"
details = "Return one typed path input."
input_type = "Path"
output_type = "Path"
examples = ["managed echo"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }

[[public_commands]]
path = "managed list"
signature = "managed list"
summary = "Mirror managed list"
details = "Return one bounded list input."
input_type = "List"
output_type = "List"
examples = ["managed list"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }

[[public_commands]]
path = "managed record"
signature = "managed record"
summary = "Mirror managed record"
details = "Return one bounded record input."
input_type = "Record"
output_type = "Record"
examples = ["managed record"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }

[[public_commands]]
path = "managed run"
signature = "managed run"
summary = "Run managed"
details = "Return one managed test value."
input_type = "Nothing"
output_type = "String"
examples = ["managed run"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }

[[public_commands]]
path = "managed text"
signature = "managed text"
summary = "Mirror managed text"
details = "Return one bounded string input."
input_type = "String"
output_type = "String"
examples = ["managed text"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }
"#;
        fs::write(&manifest_path, manifest_source).unwrap();
        let manifest = parse_plugin_manifest(manifest_source, "plugin.toml").unwrap();
        let source = format!("file:{}", manifest_path.display());
        let (locked, _) = resolve_plugin(
            &manifest,
            manifest_source.as_bytes(),
            entry_source.as_bytes(),
            &source,
            &[
                "commands.register".to_owned(),
                "filesystem.read:.".to_owned(),
                "prompt.register".to_owned(),
            ],
            env!("CARGO_PKG_VERSION"),
        )
        .unwrap();
        let lock = PluginLockfile::empty()
            .install(locked)
            .unwrap()
            .set_enabled("managed", true)
            .unwrap();
        (manifest_path, lock)
    }

    fn write_managed_lock(root: &Path, lock: &PluginLockfile) {
        fs::create_dir_all(root).unwrap();
        fs::write(
            root.join(PLUGIN_LOCK_FILE),
            serde_json::to_vec_pretty(lock).unwrap(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn write_managed_adapter(directory: &Path, spawn_log: &Path) -> (PathBuf, PluginLockfile) {
        let package = directory.join("managed-adapter");
        fs::create_dir_all(&package).unwrap();
        let entry = package.join("adapter");
        let entry_source = format!(
            "#!/bin/sh\nprintf 'spawn\\n' >> '{}'\nread request\nprintf '%s\\n' '{{\"protocol\":\"quirl.plugin.v1\",\"schema_version\":1,\"api_version\":\"0.1.0\",\"operation\":\"initialize\",\"status\":\"ready\"}}'\n",
            spawn_log.display()
        );
        fs::write(&entry, &entry_source).unwrap();
        let manifest_path = package.join("plugin.toml");
        let manifest_source = r#"schema_version = 2

[plugin]
name = "adapter"
version = "0.1.0"
entry = "adapter"
quirl = ">=0.1, <0.2"
api = "0.1.0"
runtime = "out_of_process"
summary = "Managed adapter lifecycle test"

[capabilities]
request = ["process.spawn:adapter"]

[adapter]
protocol = "quirl.plugin.v1"
executable = "adapter"
arguments = []
callback_timeout_ms = 1000
max_message_bytes = 65536
"#;
        fs::write(&manifest_path, manifest_source).unwrap();
        let manifest = parse_plugin_manifest(manifest_source, "plugin.toml").unwrap();
        let source = format!("file:{}", manifest_path.display());
        let (locked, _) = resolve_plugin(
            &manifest,
            manifest_source.as_bytes(),
            entry_source.as_bytes(),
            &source,
            &["process.spawn:adapter".to_owned()],
            env!("CARGO_PKG_VERSION"),
        )
        .unwrap();
        let lock = PluginLockfile::empty()
            .install(locked)
            .unwrap()
            .set_enabled("adapter", true)
            .unwrap();
        (entry, lock)
    }

    #[test]
    fn plugin_drives_prompt_and_completion_surfaces() {
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugin.lua");
        let mut host = LuaExtensionHost::from_paths(None, vec![plugin]);
        let prompt = refreshed_prompt_segments(&mut host, Mode::Command, 0);
        assert_eq!(prompt.len(), 1);
        assert!(!prompt[0].value.is_empty());

        let suggestions = host.complete("deploy --environment prod", 25);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "production");
        assert!(host.take_errors().is_empty());
    }

    #[test]
    fn managed_activation_obeys_enabled_state_integrity_and_exact_grants() {
        let directory = temporary_extension_directory();
        let root = directory.join("managed-state");
        let (_manifest, enabled) = write_managed_prompt_plugin(&directory);
        write_managed_lock(&root, &enabled);
        let mut host = LuaExtensionHost::from_managed_root(None, root.clone());

        assert!(matches!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { .. }
        ));
        assert_eq!(
            refreshed_prompt_segments(&mut host, Mode::Command, 0)[0].name,
            "managed"
        );
        let mut catalog = Catalog::builtin();
        host.merge_catalog_contributions(&mut catalog);
        assert_eq!(
            catalog.find("managed run").unwrap().provenance.source,
            Provenance::Plugin
        );

        let disabled = enabled.set_enabled("managed", false).unwrap();
        write_managed_lock(&root, &disabled);
        assert!(matches!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { .. }
        ));
        assert!(host.named_prompt_segments(Mode::Command, 0).is_empty());

        let mut denied = enabled;
        denied.plugins[0].granted_capabilities.clear();
        denied.validate().unwrap();
        write_managed_lock(&root, &denied);
        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Rejected);
        assert!(host.named_prompt_segments(Mode::Command, 0).is_empty());
        assert!(host.take_errors().iter().any(|error| error
            .details
            .context
            .iter()
            .any(|context| context.contains("capability denied: prompt.register"))));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn installed_commands_reach_first_completion_help_and_typed_dispatch() {
        let directory = temporary_extension_directory();
        let root = directory.join("managed-state");
        let (_manifest, lock) = write_managed_prompt_plugin(&directory);
        write_managed_lock(&root, &lock);

        let commands = installed_plugin_commands_from_root(&root).unwrap();
        assert_eq!(commands.len(), 6);
        assert_eq!(commands[0].path, "managed batch");
        let mut catalog = Catalog::builtin();
        catalog.merge(commands.clone());
        assert!(catalog.find("managed run").is_some());
        assert!(catalog
            .complete("managed r", "managed r".len())
            .iter()
            .any(|completion| completion.value == "managed run"));
        let first_help = catalog.find("managed run").unwrap();
        assert_eq!(first_help.signature, "managed run");
        assert_eq!(first_help.summary, "Run managed");
        let agent_catalog = crate::agent::installed_agent_catalog(&catalog).unwrap();
        let exported = agent_catalog
            .commands
            .iter()
            .find(|command| command.path == "managed echo")
            .unwrap();
        assert_eq!(exported.io.input, "Path");
        assert_eq!(exported.io.output, "Path");
        let installed = bind_plugin_invocation(&commands, "managed run")
            .unwrap()
            .unwrap();

        let host = Arc::new(Mutex::new(LuaExtensionHost::from_managed_root(
            None,
            root.clone(),
        )));
        let request = host
            .lock()
            .unwrap()
            .plugin_execution_request(&installed, "managed run", ExecutionInput::None)
            .unwrap();
        let outcome = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            request,
            Some(&host),
        )
        .unwrap();
        assert_eq!(outcome.status_code(), 0);
        assert_eq!(
            outcome.output,
            ExecutionOutput::Value {
                value: quirl_core::StructuredValue::String("managed".to_owned())
            }
        );

        let echo = bind_plugin_invocation(&commands, "managed echo")
            .unwrap()
            .unwrap();
        let missing = host
            .lock()
            .unwrap()
            .plugin_execution_request(&echo, "managed echo", ExecutionInput::None)
            .unwrap_err();
        assert!(missing.message.contains("requires pipeline input"));
        let mismatch = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &echo,
                "managed echo",
                ExecutionInput::Value(StructuredValue::String("wrong".to_owned())),
            )
            .unwrap_err();
        assert!(mismatch.message.contains("input type did not match"));
        let unexpected = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &installed,
                "managed run",
                ExecutionInput::Value(StructuredValue::String("unexpected".to_owned())),
            )
            .unwrap_err();
        assert!(unexpected
            .message
            .contains("does not accept pipeline input"));

        let echo_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &echo,
                "managed echo",
                ExecutionInput::Value(StructuredValue::Path("services.toml".to_owned())),
            )
            .unwrap();
        let echo_outcome = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            echo_request,
            Some(&host),
        )
        .unwrap();
        assert_eq!(
            echo_outcome.output,
            ExecutionOutput::Value {
                value: StructuredValue::Path("services.toml".to_owned())
            }
        );

        let mirror_list = bind_plugin_invocation(&commands, "managed list")
            .unwrap()
            .unwrap();
        let exact_nodes =
            StructuredValue::List(vec![StructuredValue::Nothing; MAX_PLUGIN_VALUE_NODES - 1]);
        let exact_nodes_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &mirror_list,
                "managed list",
                ExecutionInput::Value(exact_nodes.clone()),
            )
            .unwrap();
        let exact_nodes_outcome = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            exact_nodes_request,
            Some(&host),
        )
        .unwrap();
        assert_eq!(
            exact_nodes_outcome.output,
            ExecutionOutput::Value { value: exact_nodes }
        );
        let excess_nodes =
            StructuredValue::List(vec![StructuredValue::Nothing; MAX_PLUGIN_VALUE_NODES]);
        assert_eq!(
            host.lock()
                .unwrap()
                .plugin_execution_request(
                    &mirror_list,
                    "managed list",
                    ExecutionInput::Value(excess_nodes),
                )
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );

        let mut exact_depth = StructuredValue::Nothing;
        for _ in 0..MAX_PLUGIN_VALUE_DEPTH {
            exact_depth = StructuredValue::List(vec![exact_depth]);
        }
        let exact_depth_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &mirror_list,
                "managed list",
                ExecutionInput::Value(exact_depth.clone()),
            )
            .unwrap();
        let exact_depth_outcome = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            exact_depth_request,
            Some(&host),
        )
        .unwrap();
        assert_eq!(
            exact_depth_outcome.output,
            ExecutionOutput::Value {
                value: exact_depth.clone()
            }
        );
        assert_eq!(
            host.lock()
                .unwrap()
                .plugin_execution_request(
                    &mirror_list,
                    "managed list",
                    ExecutionInput::Value(StructuredValue::List(vec![exact_depth])),
                )
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );

        let mirror_record = bind_plugin_invocation(&commands, "managed record")
            .unwrap()
            .unwrap();
        let exact_fields = (0..MAX_PLUGIN_VALUE_FIELDS)
            .map(|index| (format!("f{index}"), StructuredValue::Nothing))
            .collect::<BTreeMap<_, _>>();
        let exact_fields_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &mirror_record,
                "managed record",
                ExecutionInput::Value(StructuredValue::Record(exact_fields.clone())),
            )
            .unwrap();
        let exact_fields_outcome = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            exact_fields_request,
            Some(&host),
        )
        .unwrap();
        assert_eq!(
            exact_fields_outcome.output,
            ExecutionOutput::Value {
                value: StructuredValue::Record(exact_fields)
            }
        );
        let excess_fields = (0..=MAX_PLUGIN_VALUE_FIELDS)
            .map(|index| (format!("f{index}"), StructuredValue::Nothing))
            .collect();
        assert_eq!(
            host.lock()
                .unwrap()
                .plugin_execution_request(
                    &mirror_record,
                    "managed record",
                    ExecutionInput::Value(StructuredValue::Record(excess_fields)),
                )
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );

        let mirror_text = bind_plugin_invocation(&commands, "managed text")
            .unwrap()
            .unwrap();
        let exact_text = "x".repeat(MAX_PLUGIN_VALUE_TEXT_BYTES);
        let exact_text_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &mirror_text,
                "managed text",
                ExecutionInput::Value(StructuredValue::String(exact_text.clone())),
            )
            .unwrap();
        let exact_text_outcome = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            exact_text_request,
            Some(&host),
        )
        .unwrap();
        assert_eq!(
            exact_text_outcome.output,
            ExecutionOutput::Value {
                value: StructuredValue::String(exact_text)
            }
        );
        assert_eq!(
            host.lock()
                .unwrap()
                .plugin_execution_request(
                    &mirror_text,
                    "managed text",
                    ExecutionInput::Value(StructuredValue::String(
                        "x".repeat(MAX_PLUGIN_VALUE_TEXT_BYTES + 1),
                    )),
                )
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );

        let batch = bind_plugin_invocation(&commands, "managed batch")
            .unwrap()
            .unwrap();
        let exact_batch_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &batch,
                "managed batch",
                ExecutionInput::Value(StructuredValue::Int(
                    i64::try_from(MAX_LUA_RUNNER_STREAM_VALUES).unwrap(),
                )),
            )
            .unwrap();
        let exact_batch = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            exact_batch_request,
            Some(&host),
        )
        .unwrap();
        assert!(matches!(
            exact_batch.output,
            ExecutionOutput::Values { ref values }
                if values.len() == MAX_LUA_RUNNER_STREAM_VALUES
        ));
        let overflow_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(
                &batch,
                "managed batch",
                ExecutionInput::Value(StructuredValue::Int(
                    i64::try_from(MAX_LUA_RUNNER_STREAM_VALUES + 1).unwrap(),
                )),
            )
            .unwrap();
        let overflow = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            overflow_request,
            Some(&host),
        )
        .unwrap_err();
        assert_eq!(overflow.code, ErrorCode::ResourceLimit);
        let cancelled_plan = host
            .lock()
            .unwrap()
            .plugin_execution_request(&installed, "managed run", ExecutionInput::None)
            .unwrap()
            .plan()
            .unwrap();
        cancelled_plan.cancellation().cancel();
        let cancelled = host
            .lock()
            .unwrap()
            .dispatch_plugin_plan(&cancelled_plan)
            .unwrap_err();
        assert_eq!(cancelled.code, ErrorCode::ResourceLimit);
        assert!(cancelled.message.contains("cancelled"));

        let expired_plan = host
            .lock()
            .unwrap()
            .plugin_execution_request(&installed, "managed run", ExecutionInput::None)
            .unwrap()
            .plan()
            .unwrap();
        std::thread::sleep(PLUGIN_COMMAND_WALL_TIME + Duration::from_millis(5));
        let expired = host
            .lock()
            .unwrap()
            .dispatch_plugin_plan(&expired_plan)
            .unwrap_err();
        assert_eq!(expired.code, ErrorCode::ResourceLimit);
        assert!(expired.message.contains("deadline"));

        let stale_request = host
            .lock()
            .unwrap()
            .plugin_execution_request(&installed, "managed run", ExecutionInput::None)
            .unwrap();
        write_managed_lock(&root, &PluginLockfile::empty());
        let removed = crate::execute_execution_request(
            &mut quirl_process::NativeExecutor::default(),
            stale_request,
            Some(&host),
        )
        .unwrap_err();
        assert_eq!(removed.code, ErrorCode::InvalidCommand);
        assert!(removed.message.contains("disappeared before execution"));
        let host_guard = host.lock().unwrap();
        assert!(host_guard
            .scheduler
            .as_ref()
            .unwrap()
            .handle()
            .wait_generation_idle(host_guard.revision, Duration::from_secs(1)));
        drop(host_guard);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn empty_installed_command_snapshot_has_no_catalog_semantics() {
        let directory = temporary_extension_directory();
        let root = directory.join("managed-state");

        assert!(installed_plugin_commands_from_root(&root)
            .unwrap()
            .is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn tampered_installed_lock_fails_closed_before_catalog_publication() {
        let directory = temporary_extension_directory();
        let root = directory.join("managed-state");
        let (_manifest, mut lock) = write_managed_prompt_plugin(&directory);
        lock.schema_hash = "tampered".to_owned();
        write_managed_lock(&root, &lock);

        let error = installed_plugin_commands_from_root(&root).unwrap_err();

        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(error.message, "managed plugin lockfile is corrupt");
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("installed plugin catalog snapshot")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn plugin_arguments_and_effects_fail_before_callback_invocation() {
        let command = CommandSpec {
            options: vec![quirl_catalog::ArgumentSpec {
                names: vec!["count".to_owned()],
                kind: ArgumentKind::Positional,
                value_type: "positive-integer".to_owned(),
                required: true,
                repeatable: false,
                values: None,
                conflicts: Vec::new(),
                documentation: "Count to return".to_owned(),
                examples: vec!["managed run 2".to_owned()],
                provenance: ProvenanceInfo::builtin(Provenance::Plugin),
            }],
            ..Catalog::builtin().commands[0].clone()
        };
        let error = bind_plugin_invocation(&[command.clone()], &format!("{} nope", command.path))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.details.context[0].contains("positive-integer"));

        let declared = catalog_execution_effects(&[Effect::ReadFilesystem]);
        let request = ExecutionRequest::new(
            ExecutionSource::new("plugin:test", "test").unwrap(),
            quirl_core::ExecutionMode::Plugin,
        )
        .with_effects(declared, ExecutionEffects::none());
        let denied = request.plan().unwrap_err();
        assert_eq!(denied.code, ErrorCode::Validation);
        assert!(denied.details.context[0].contains("ReadFilesystem"));
    }

    #[test]
    fn plugin_value_conversion_accepts_exact_limits_and_rejects_limit_plus_one() {
        let exact_nodes =
            StructuredValue::List(vec![StructuredValue::Nothing; MAX_PLUGIN_VALUE_NODES - 1]);
        validate_plugin_value(
            &exact_nodes,
            "test input",
            &mut PluginValueBudget::default(),
            None,
        )
        .unwrap();
        let excess_nodes =
            StructuredValue::List(vec![StructuredValue::Nothing; MAX_PLUGIN_VALUE_NODES]);
        assert_eq!(
            validate_plugin_value(
                &excess_nodes,
                "test input",
                &mut PluginValueBudget::default(),
                None,
            )
            .unwrap_err()
            .code,
            ErrorCode::ResourceLimit
        );

        let exact_fields = (0..MAX_PLUGIN_VALUE_FIELDS)
            .map(|index| (format!("f{index}"), StructuredValue::Nothing))
            .collect();
        validate_plugin_value(
            &StructuredValue::Record(exact_fields),
            "test input",
            &mut PluginValueBudget::default(),
            None,
        )
        .unwrap();
        let excess_fields = (0..=MAX_PLUGIN_VALUE_FIELDS)
            .map(|index| (format!("f{index}"), StructuredValue::Nothing))
            .collect();
        let field_error = validate_plugin_value(
            &StructuredValue::Record(excess_fields),
            "test input",
            &mut PluginValueBudget::default(),
            None,
        )
        .unwrap_err();
        assert!(field_error.details.context[0].contains("record fields"));

        validate_plugin_value(
            &StructuredValue::String("x".repeat(MAX_PLUGIN_VALUE_TEXT_BYTES)),
            "test input",
            &mut PluginValueBudget::default(),
            None,
        )
        .unwrap();
        assert_eq!(
            validate_plugin_value(
                &StructuredValue::String("x".repeat(MAX_PLUGIN_VALUE_TEXT_BYTES + 1)),
                "test input",
                &mut PluginValueBudget::default(),
                None,
            )
            .unwrap_err()
            .code,
            ErrorCode::ResourceLimit
        );

        let mut exact_depth = StructuredValue::Nothing;
        for _ in 0..MAX_PLUGIN_VALUE_DEPTH {
            exact_depth = StructuredValue::List(vec![exact_depth]);
        }
        validate_plugin_value(
            &exact_depth,
            "test input",
            &mut PluginValueBudget::default(),
            None,
        )
        .unwrap();
        let excess_depth = StructuredValue::List(vec![exact_depth]);
        let depth_error = validate_plugin_value(
            &excess_depth,
            "test input",
            &mut PluginValueBudget::default(),
            None,
        )
        .unwrap_err();
        assert!(depth_error.details.context[0].contains("depth"));

        let mut command = Catalog::builtin().commands[0].clone();
        command.path = "plugin mismatch".to_owned();
        command.io.input = "Nothing".to_owned();
        command.io.output = "String".to_owned();
        command.io.streaming = false;
        command.exit_codes.insert(0, "success".to_owned());
        let outcome = ExecutionOutcome::new(
            quirl_core::ExecutionStatus::Exited(0),
            ExecutionOutput::Value {
                value: StructuredValue::Int(1),
            },
            Vec::new(),
            quirl_core::ExecutionCleanupState::Complete,
        )
        .unwrap();
        let mismatch = validate_plugin_outcome(&command, &outcome).unwrap_err();
        assert!(mismatch.message.contains("does not match `String`"));
    }

    #[test]
    fn managed_reload_observes_cancellation_before_starting_plugin_activation() {
        let directory = temporary_extension_directory();
        let root = directory.join("managed-state");
        let (_manifest, enabled) = write_managed_prompt_plugin(&directory);
        write_managed_lock(&root, &enabled);
        let mut host = LuaExtensionHost::from_managed_root(None, root);
        let cancelled = AtomicBool::new(true);

        assert_eq!(
            host.reload_if_changed_with_cancellation(&cancelled),
            ExtensionReloadState::Rejected
        );
        assert!(host.take_errors().iter().any(|error| error
            .message
            .contains("managed plugin reload was cancelled")));
        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 1 }
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_adapter_is_not_respawned_during_hot_path_reload_polls() {
        let directory = temporary_extension_directory();
        let root = directory.join("managed-state");
        let spawn_log = directory.join("adapter-spawns");
        let (entry, lock) = write_managed_adapter(&directory, &spawn_log);
        write_managed_lock(&root, &lock);
        let mut host = LuaExtensionHost::from_managed_root(None, root);

        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 1 }
        );
        for _ in 0..4 {
            assert_eq!(host.reload_if_changed(), ExtensionReloadState::Unchanged);
        }
        assert_eq!(fs::read_to_string(&spawn_log).unwrap(), "spawn\n");

        fs::write(&entry, "#!/bin/sh\nexit 97\n").unwrap();
        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Rejected);
        assert_eq!(host.config_revision(), 1);
        assert_eq!(fs::read_to_string(&spawn_log).unwrap(), "spawn\n");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn managed_activation_executes_the_exact_bytes_that_passed_integrity_verification() {
        let directory = temporary_extension_directory();
        let root = directory.join("managed-state");
        let (manifest, enabled) = write_managed_prompt_plugin(&directory);
        write_managed_lock(&root, &enabled);
        let host = LuaExtensionHost::from_managed_root(None, root);

        let snapshot = host.snapshot_sources(&AtomicBool::new(false));
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        assert!(snapshot.plugins[0].verified_source.is_some());

        // Model a local attacker replacing the entry after checksum
        // verification but before the generation is built.
        fs::write(
            manifest.parent().unwrap().join("plugin.lua"),
            "error('replacement must not execute')",
        )
        .unwrap();

        let built = host.build_candidate(snapshot, &AtomicBool::new(false));
        assert!(built.is_ok(), "{:?}", built.as_ref().err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn managed_activation_rejects_checksum_matching_entry_symlink_escape() {
        use std::os::unix::fs::symlink;

        let directory = temporary_extension_directory();
        let (manifest, lock) = write_managed_prompt_plugin(&directory);
        let entry = manifest.parent().unwrap().join("plugin.lua");
        let outside = directory.join("outside.lua");
        fs::write(&outside, fs::read(&entry).unwrap()).unwrap();
        fs::remove_file(&entry).unwrap();
        symlink(&outside, &entry).unwrap();

        let mut fingerprints = Vec::new();
        let error = managed_plugin_candidate(&lock.plugins[0], &mut fingerprints).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("outside"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn managed_activation_rejects_oversized_entry_before_runtime_loading() {
        let directory = temporary_extension_directory();
        let (manifest, lock) = write_managed_prompt_plugin(&directory);
        fs::write(
            manifest.parent().unwrap().join("plugin.lua"),
            vec![b'x'; MAX_PLUGIN_ENTRY_BYTES + 1],
        )
        .unwrap();

        let mut fingerprints = Vec::new();
        let error = managed_plugin_candidate(&lock.plugins[0], &mut fingerprints).unwrap_err();
        assert!(matches!(
            error.code,
            ErrorCode::Io | ErrorCode::ResourceLimit
        ));
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("limit")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_sources_detect_add_remove_and_content_changes() {
        let directory = temporary_extension_directory();
        let config = directory.join("config.lua");
        let plugin = directory.join("plugins/fruit.lua");
        let mut host = LuaExtensionHost::from_directory(directory.clone());

        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 1 }
        );
        assert_eq!(host.active_config().editor.keymap, "emacs");
        assert!(host.complete("fruit ", 6).is_empty());

        write_config(&config, "emacs");
        write_plugin(&plugin, "fruit", "apple");
        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 2 }
        );
        assert_eq!(host.active_config().editor.keymap, "emacs");
        assert_eq!(host.complete("fruit ", 6)[0].value, "apple");
        assert_eq!(
            refreshed_prompt_segments(&mut host, Mode::Command, 0)[0].name,
            "fruit"
        );

        write_plugin(&plugin, "fruit", "apricot");
        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 3 }
        );
        assert_eq!(host.complete("fruit ", 6)[0].value, "apricot");

        fs::remove_file(&plugin).unwrap();
        fs::remove_file(&config).unwrap();
        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 4 }
        );
        assert_eq!(host.active_config().editor.keymap, "emacs");
        assert!(host.complete("fruit ", 6).is_empty());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn rejected_generation_keeps_the_last_known_good_state_and_is_reported_once() {
        let directory = temporary_extension_directory();
        let config = directory.join("config.lua");
        let plugin = directory.join("plugins/fruit.lua");
        write_config(&config, "emacs");
        write_plugin(&plugin, "fruit", "apple");
        let mut host = LuaExtensionHost::from_directory(directory.clone());

        assert!(matches!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { .. }
        ));
        assert_eq!(host.config_revision(), 1);
        assert_eq!(host.complete("fruit ", 6)[0].value, "apple");

        fs::write(&config, "return { editor = { keymap = 'invalid' } }").unwrap();
        write_plugin(&plugin, "fruit", "apricot");
        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Rejected);
        assert_eq!(host.config_revision(), 1);
        assert_eq!(host.active_config().editor.keymap, "emacs");
        assert_eq!(host.complete("fruit ", 6)[0].value, "apple");
        assert_eq!(host.take_errors().len(), 1);

        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Unchanged);
        assert!(host.take_errors().is_empty());

        write_config(&config, "vim");
        fs::write(&plugin, "this is not valid lua").unwrap();
        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Rejected);
        assert_eq!(host.config_revision(), 1);
        assert_eq!(host.active_config().editor.keymap, "emacs");
        assert_eq!(host.complete("fruit ", 6)[0].value, "apple");
        assert_eq!(host.take_errors().len(), 1);

        write_plugin(&plugin, "fruit", "apricot");
        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 2 }
        );
        assert_eq!(host.active_config().editor.keymap, "vim");
        assert_eq!(host.complete("fruit ", 6)[0].value, "apricot");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn directory_plugins_are_loaded_in_sorted_path_order() {
        let directory = temporary_extension_directory();
        // Create these in reverse lexical order to ensure discovery, rather than
        // the filesystem's directory iteration order, determines precedence.
        write_plugin(&directory.join("plugins/z-last.lua"), "z-last", "zebra");
        write_plugin(&directory.join("plugins/a-first.lua"), "a-first", "apple");
        let mut host = LuaExtensionHost::from_directory(directory.clone());

        assert!(matches!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { .. }
        ));
        let suggestions = host.complete("fruit ", 6);
        assert_eq!(
            suggestions
                .iter()
                .map(|suggestion| suggestion.value.as_str())
                .collect::<Vec<_>>(),
            vec!["apple", "zebra"]
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn typed_contributions_reach_catalog_completion_and_panel_consumers() {
        let directory = temporary_extension_directory();
        let plugin = directory.join("contributions.lua");
        fs::write(
            &plugin,
            r#"
quirl.extension.contribute {
  kind = "catalog", name = "demo-catalog", deadline_ms = 10,
  provide = function(_)
    return { commands = {{
      id = "command:demo", version = "0.1.0", path = "demo", aliases = {"d"},
      signature = "demo", summary = "Demonstrate contributed metadata",
      details = "A complete typed command contributed by a test plugin.",
      arguments = {{
        names = {"--ready"}, kind = "flag", value_type = "Bool",
        required = false, repeatable = false, conflicts = {"--not-ready"},
        documentation = "Report readiness", examples = {"demo --ready"},
        provenance = { source = "plugin", confidence = "exact", trust = "trusted" },
      }, {
        names = {"--not-ready"}, kind = "flag", value_type = "Bool",
        required = false, repeatable = false, conflicts = {"--ready"},
        documentation = "Report non-readiness", examples = {"demo --not-ready"},
        provenance = { source = "plugin", confidence = "exact", trust = "trusted" },
      }},
      examples = {"demo"},
      io = { input = "Nothing", output = "Nothing", streaming = false },
      effects = {"read_filesystem"}, exit_codes = { ["0"] = "success" },
      provenance = { source = "plugin", confidence = "exact", trust = "trusted" },
    }} }
  end,
}
quirl.extension.contribute {
  kind = "completion", name = "demo-completion", deadline_ms = 10,
  provide = function(_) return {{ value = "demo-value", summary = "typed" }} end,
}
quirl.extension.contribute {
  kind = "panel", name = "demo-panel", deadline_ms = 10,
  plain_fallback = "demo unavailable",
  provide = function(_)
    return {
      title = "demo", columns = {"value"}, rows = {{"ready"}},
      plain_fallback = "demo unavailable",
    }
  end,
}
"#,
        )
        .unwrap();
        let mut host = LuaExtensionHost::from_paths(None, vec![plugin]);
        let mut catalog = Catalog::builtin();
        host.merge_catalog_contributions(&mut catalog);
        let errors = host.take_errors();
        assert!(errors.is_empty(), "{errors:#?}");
        assert_eq!(
            catalog.find("demo").unwrap().provenance.source,
            Provenance::Plugin
        );
        assert!(host
            .complete("demo-v", 6)
            .iter()
            .any(|item| item.value == "demo-value"));
        let panel = host
            .render_panel_contribution("demo-panel", &json!({}))
            .unwrap();
        assert_eq!(panel.rows, vec![vec!["ready"]]);
        let first = host.cached_panel_snapshot();
        assert!(first.panels.is_empty());
        let scheduler = host.scheduler.as_ref().unwrap().handle();
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        let cached = host.cached_panel_snapshot();
        assert_eq!(cached.panels.len(), 1);
        assert_eq!(cached.panels[0].model.rows, vec![vec!["ready"]]);
        assert!(cached.generation > first.generation);
        assert!(host.take_errors().is_empty());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failing_panel_refresh_preserves_cache_and_provider_removal_publishes_empty_set() {
        let directory = temporary_extension_directory();
        let plugin = directory.join("plugins/panel.lua");
        fs::write(
            &plugin,
            r#"
local calls = 0
quirl.extension.contribute {
  kind = "panel", name = "demo-panel", deadline_ms = 10,
  plain_fallback = "demo unavailable",
  provide = function(_)
    calls = calls + 1
    if calls > 1 then error("injected panel failure") end
    return {
      title = "demo", columns = {"value"}, rows = {{"ready"}},
      plain_fallback = "demo unavailable",
    }
  end,
}
"#,
        )
        .unwrap();
        let mut host = LuaExtensionHost::from_directory(directory.clone());
        assert!(matches!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { .. }
        ));
        assert!(host.cached_panel_snapshot().panels.is_empty());
        let scheduler = host.scheduler.as_ref().unwrap().handle();
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        let ready = host.cached_panel_snapshot();
        assert_eq!(ready.panels[0].model.rows, vec![vec!["ready"]]);

        host.panel_last_refresh = None;
        let _ = host.cached_panel_snapshot();
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        let failed = host.cached_panel_snapshot();
        assert_eq!(failed.panels, ready.panels);
        assert!(!host.take_errors().is_empty());

        fs::remove_file(plugin).unwrap();
        assert!(matches!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { .. }
        ));
        let removed = host.cached_panel_snapshot();
        assert!(removed.panels.is_empty());
        assert!(removed.generation > failed.generation);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn event_actions_keep_plugin_order_and_isolate_handler_failures() {
        let directory = temporary_extension_directory();
        let plugin = |name: &str, body: &str| {
            format!(
                r#"quirl.events.subscribe {{
                  name = "{name}", events = {{ "session_start" }},
                  capabilities = {{ "events_observe" }}, deadline_ms = 10,
                  observe = function(_) {body} end,
                }}"#
            )
        };
        let first = directory.join("a.lua");
        let broken = directory.join("m.lua");
        let last = directory.join("z.lua");
        fs::write(
            &first,
            plugin(
                "first",
                "return {{ action = 'diagnose', message = 'first' }}",
            ),
        )
        .unwrap();
        fs::write(&broken, plugin("broken", "error('broken handler')")).unwrap();
        fs::write(
            &last,
            plugin("last", "return {{ action = 'diagnose', message = 'last' }}"),
        )
        .unwrap();
        let mut host = LuaExtensionHost::from_paths(None, vec![last, broken, first]);
        let actions = host
            .dispatch_event(ExtensionEventData::SessionStart { restored: false })
            .unwrap();
        let messages = actions
            .into_iter()
            .filter_map(|action| match action {
                ExtensionAction::Diagnose { message } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(messages, vec!["first", "last"]);
        assert_eq!(host.take_errors().len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn first_prompt_uses_only_cache_and_newest_generation_refreshes_later() {
        let directory = temporary_extension_directory();
        let plugin = directory.join("status.lua");
        fs::write(
            &plugin,
            r#"quirl.prompt.add_segment {
              name = "status", deadline_ms = 20,
              render = function(ctx)
                if ctx.last_status == 1 then return "one" end
                if ctx.last_status == 2 then return "two" end
                if ctx.last_status == 3 then return "three" end
                return "four"
              end,
            }"#,
        )
        .unwrap();
        let mut host = LuaExtensionHost::from_paths(None, vec![plugin]);

        assert!(host.named_prompt_segments(Mode::Command, 1).is_empty());
        let scheduler = host.scheduler.as_ref().unwrap().handle();
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        host.poll_prompt_refresh();
        assert_eq!(host.prompt_cache[0][0].value, "one");

        let stale = host.named_prompt_segments(Mode::Command, 2);
        assert_eq!(stale[0].value, "one");
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        host.poll_prompt_refresh();
        assert_eq!(host.prompt_cache[0][0].value, "two");

        let _ = host.named_prompt_segments(Mode::Command, 3);
        let _ = host.named_prompt_segments(Mode::Command, 4);
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        host.poll_prompt_refresh();
        assert_eq!(host.prompt_cache[0][0].value, "four");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unchanged_reload_preserves_the_persistent_lua_worker_state() {
        let directory = temporary_extension_directory();
        let plugin = directory.join("persistent.lua");
        fs::write(
            &plugin,
            r#"local renders = 0
quirl.prompt.add_segment {
  name = "persistent", deadline_ms = 20,
  render = function(_)
    renders = renders + 1
    return tostring(renders)
  end,
}"#,
        )
        .unwrap();
        let mut host = LuaExtensionHost::from_paths(None, vec![plugin]);

        assert_eq!(
            refreshed_prompt_segments(&mut host, Mode::Command, 0)[0].value,
            "1"
        );
        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Unchanged);
        assert_eq!(
            refreshed_prompt_segments(&mut host, Mode::Command, 0)[0].value,
            "2"
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failing_prompt_refresh_preserves_the_last_completed_snapshot() {
        let directory = temporary_extension_directory();
        let plugin = directory.join("stale.lua");
        fs::write(
            &plugin,
            r#"quirl.prompt.add_segment {
              name = "stale", deadline_ms = 20,
              render = function(ctx)
                if ctx.last_status == 0 then return "cached" end
                error("refresh failed")
              end,
            }"#,
        )
        .unwrap();
        let mut host = LuaExtensionHost::from_paths(None, vec![plugin]);
        assert_eq!(
            refreshed_prompt_segments(&mut host, Mode::Command, 0)[0].value,
            "cached"
        );
        let scheduler = host.scheduler.as_ref().unwrap().handle();
        let stale = host.named_prompt_segments(Mode::Command, 1);
        assert_eq!(stale[0].value, "cached");
        assert!(scheduler.wait_generation_idle(host.revision, Duration::from_secs(1)));
        host.poll_prompt_refresh();
        assert_eq!(host.prompt_cache[0][0].value, "cached");
        assert_eq!(host.take_errors().len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn plugin_cardinality_is_rejected_before_runtime_creation() {
        let directory = temporary_extension_directory();
        let mut plugins = Vec::new();
        for index in 0..=MAX_LOADED_PLUGIN_RUNTIMES {
            let path = directory.join(format!("runtime-{index:02}.lua"));
            write_plugin(&path, &format!("segment-{index}"), "value");
            plugins.push(path);
        }
        let mut host = LuaExtensionHost::from_paths(None, plugins);
        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Rejected);
        assert!(host.scheduler.is_none());
        let errors = host.take_errors();
        assert!(errors.iter().any(|error| error
            .details
            .context
            .iter()
            .any(|context| { context.contains(&format!("limit: {MAX_LOADED_PLUGIN_RUNTIMES}")) })));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn aggregate_registration_caps_reject_amplification_across_plugins() {
        for kind in ["prompt", "event", "contribution"] {
            let directory = temporary_extension_directory();
            let first = directory.join(format!("{kind}-a.lua"));
            let second = directory.join(format!("{kind}-b.lua"));
            fs::write(&first, registration_source(kind, "a", 64)).unwrap();
            fs::write(&second, registration_source(kind, "b", 1)).unwrap();
            let mut host = LuaExtensionHost::from_paths(None, vec![first, second]);
            assert_eq!(host.reload_if_changed(), ExtensionReloadState::Rejected);
            let errors = host.take_errors();
            assert!(errors.iter().any(|error| error
                .details
                .context
                .iter()
                .any(|context| context.contains("observed: 65; limit: 64"))));
            fs::remove_dir_all(directory).unwrap();
        }
    }

    #[test]
    fn event_action_retention_is_bounded_across_plugins() {
        let directory = temporary_extension_directory();
        let mut plugins = Vec::new();
        for plugin_index in 0..5 {
            let path = directory.join(format!("actions-{plugin_index}.lua"));
            let mut actions = String::new();
            for action_index in 0..64 {
                actions.push_str(&format!(
                    "{{ action = 'diagnose', message = 'p{plugin_index}-{action_index}' }},"
                ));
            }
            fs::write(
                &path,
                format!(
                    "quirl.events.subscribe {{ name = 'handler-{plugin_index}', events = {{ 'session_start' }}, capabilities = {{ 'events_observe' }}, deadline_ms = 20, observe = function(_) return {{ {actions} }} end }}"
                ),
            )
            .unwrap();
            plugins.push(path);
        }
        let mut host = LuaExtensionHost::from_paths(None, plugins);
        let actions = host
            .dispatch_event(ExtensionEventData::SessionStart { restored: false })
            .unwrap();
        assert_eq!(actions.len(), MAX_EXTENSION_EVENT_ACTIONS);
        assert!(host
            .take_errors()
            .iter()
            .any(|error| error.message.contains("event actions")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn contribution_collisions_reject_the_whole_extension_generation() {
        let directory = temporary_extension_directory();
        let contribution = r#"quirl.extension.contribute {
          kind = "panel", name = "cluster", deadline_ms = 10,
          plain_fallback = "cluster unavailable",
          provide = function(_) return "ok" end,
        }"#;
        fs::write(directory.join("plugins/a.lua"), contribution).unwrap();
        fs::write(directory.join("plugins/b.lua"), contribution).unwrap();
        let mut host = LuaExtensionHost::from_directory(directory.clone());

        assert_eq!(host.reload_if_changed(), ExtensionReloadState::Rejected);
        let errors = host.take_errors();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("duplicate Panel contribution"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn plugin_state_root_follows_config_dir_when_plugin_home_is_unset() {
        use std::ffi::OsString;

        assert_eq!(
            resolve_plugin_state_directory(
                None,
                Some(OsString::from("/tmp/custom")),
                Some(OsString::from("/tmp/xdg")),
                Some(OsString::from("/tmp/home")),
            ),
            Some(PathBuf::from("/tmp/custom/plugins"))
        );
        assert_eq!(
            resolve_plugin_state_directory(
                Some(OsString::from("/tmp/plugins")),
                Some(OsString::from("/tmp/custom")),
                Some(OsString::from("/tmp/xdg")),
                Some(OsString::from("/tmp/home")),
            ),
            Some(PathBuf::from("/tmp/plugins"))
        );
        assert_eq!(
            resolve_plugin_state_directory(
                None,
                None,
                Some(OsString::from("/tmp/xdg")),
                Some(OsString::from("/tmp/home")),
            ),
            Some(PathBuf::from("/tmp/xdg/quirl/plugins"))
        );
    }

    fn registration_source(kind: &str, prefix: &str, count: usize) -> String {
        let mut source = String::new();
        for index in 0..count {
            match kind {
                "prompt" => source.push_str(&format!(
                    "quirl.prompt.add_segment {{ name = '{prefix}-{index}', deadline_ms = 8, render = function(_) return '{prefix}' end }}\n"
                )),
                "event" => source.push_str(&format!(
                    "quirl.events.subscribe {{ name = '{prefix}-{index}', events = {{ 'session_start' }}, capabilities = {{ 'events_observe' }}, deadline_ms = 8, observe = function(_) return {{}} end }}\n"
                )),
                "contribution" => source.push_str(&format!(
                    "quirl.extension.contribute {{ kind = 'completion', name = '{prefix}-{index}', deadline_ms = 8, provide = function(_) return {{}} end }}\n"
                )),
                _ => unreachable!("test supplies a fixed registration kind"),
            }
        }
        source
    }
}
