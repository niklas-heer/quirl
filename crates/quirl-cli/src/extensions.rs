use quirl_catalog::{
    Catalog, CommandSpec, Confidence, Provenance, ProvenanceInfo, Trust,
    MAX_COMPLETION_QUERY_BYTES, MAX_COMPLETION_RESULTS,
};
use quirl_core::{
    validate_contribution_set, ContributionKind, ErrorCode, ExtensionAction, ExtensionEvent,
    ExtensionEventData, ShellError,
};
use quirl_lua::{ConfigStore, LuaPolicy, LuaRuntime, QuirlConfig};
use quirl_plugin::{
    doctor_plugin, normalize_plugin_commands, parse_plugin_manifest, validate_plugin_manifest,
    LockedPlugin, PluginLockfile, PluginRuntime, PLUGIN_LOCK_FILE,
};
use quirl_syntax::Mode;
use quirl_ui::{ExtensionCompleter, ExtensionSuggestion, PanelModel};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::hash_map::DefaultHasher,
    env,
    ffi::OsString,
    fs,
    hash::{Hash, Hasher},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_PLUGIN_LOCK_BYTES: usize = 4 * 1024 * 1024;
const MAX_PLUGIN_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_PLUGIN_ENTRY_BYTES: usize = 4 * 1024 * 1024;
const MAX_EXTENSION_COMPLETION_CALLBACKS: usize = 64;
const EXTENSION_COMPLETION_WALL_TIME: Duration = Duration::from_millis(250);

pub type SharedLuaExtensions = Arc<Mutex<LuaExtensionHost>>;

/// A rendered plugin prompt segment, retaining the registration name so callers
/// can order it using `config.prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedExtensionSegment {
    pub name: String,
    pub value: String,
}

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
    runtime: PluginRuntime,
    grants: Vec<String>,
    catalog_commands: Vec<CommandSpec>,
}

type BuiltExtensionGeneration = (ConfigStore, Vec<PathBuf>, Vec<LuaRuntime>, Vec<CommandSpec>);

pub struct LuaExtensionHost {
    /// `Some` for a config file that is watched even when it does not exist yet.
    config_path: Option<PathBuf>,
    plugin_source: PluginSource,
    plugin_paths: Vec<PathBuf>,
    config: ConfigStore,
    plugin_runtimes: Vec<LuaRuntime>,
    managed_commands: Vec<CommandSpec>,
    errors: Vec<ShellError>,
    observed_fingerprint: Option<ExtensionFingerprint>,
    revision: u64,
    event_sequence: u64,
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
            errors: Vec::new(),
            observed_fingerprint: None,
            revision: 0,
            event_sequence: 0,
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
        self.observed_fingerprint = Some(snapshot.fingerprint.clone());

        match self.build_candidate(snapshot) {
            Ok((config, plugin_paths, plugin_runtimes, managed_commands)) => {
                self.config = config;
                self.plugin_paths = plugin_paths;
                self.plugin_runtimes = plugin_runtimes;
                self.managed_commands = managed_commands;
                self.revision += 1;
                ExtensionReloadState::Reloaded {
                    revision: self.revision,
                }
            }
            Err(error) => {
                self.errors.push(
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

    /// Render segments while retaining their registration names for config-driven
    /// ordering by the REPL/UI layer.
    pub fn named_prompt_segments(
        &mut self,
        mode: Mode,
        last_status: i32,
    ) -> Vec<NamedExtensionSegment> {
        self.ensure_loaded();
        let cwd = env::current_dir().unwrap_or_default();
        let context = json!({
            "cwd": cwd,
            "project_name": cwd.file_name().map(|name| name.to_string_lossy()),
            "mode": mode.to_string(),
            "last_status": last_status,
        });
        let mut rendered = Vec::new();
        for runtime in &self.plugin_runtimes {
            for segment in runtime.registrations().prompt_segments {
                match runtime.render_prompt_segment(&segment.name, &context) {
                    Ok(Some(value)) if !value.is_empty() => rendered.push(NamedExtensionSegment {
                        name: segment.name.clone(),
                        value,
                    }),
                    Ok(_) => {}
                    Err(error) => self
                        .errors
                        .push(error.with_context(format!("prompt segment: {}", segment.name))),
                }
            }
        }
        rendered
    }

    pub fn complete(&mut self, line: &str, pos: usize) -> Vec<ExtensionSuggestion> {
        self.ensure_loaded();
        let position = floor_char_boundary(line, pos.min(line.len()));
        if line.len() > MAX_COMPLETION_QUERY_BYTES {
            self.errors.push(
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

        'plugins: for runtime in &self.plugin_runtimes {
            for provider in runtime.registrations().completion_providers {
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
                match runtime.complete_with_provider(&provider.command, &context) {
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
                    Ok(_) => self.errors.push(ShellError::new(
                        ErrorCode::Validation,
                        format!(
                            "completion provider `{}` must return an array",
                            provider.command
                        ),
                    )),
                    Err(error) => self.errors.push(
                        error.with_context(format!("completion provider: {}", provider.command)),
                    ),
                }
            }
            for registration in runtime
                .registrations()
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
                match runtime.invoke_contribution(
                    ContributionKind::Completion,
                    &registration.name,
                    &context,
                ) {
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
                        Err(error) => self.errors.push(contribution_shape_error(
                            &registration.name,
                            "completion providers must return an array of typed completion items",
                            error,
                        )),
                    },
                    Err(error) => {
                        self.errors.push(error.with_context(format!(
                            "completion contribution: {}",
                            registration.name
                        )))
                    }
                }
            }
        }
        if let Some(reason) = limit_reason {
            self.errors.push(
                ShellError::new(ErrorCode::ResourceLimit, reason).with_help(
                    "Reduce enabled completion providers or narrow their returned items",
                ),
            );
        }
        suggestions
    }

    /// Merge validated plugin command facts into the semantic catalog without
    /// allowing a provider to shadow an installed command or forge provenance.
    pub fn merge_catalog_contributions(&mut self, catalog: &mut Catalog) {
        self.ensure_loaded();
        if let Err(error) = validate_catalog_contribution(catalog, &self.managed_commands) {
            self.errors
                .push(error.with_context("managed plugin command manifests"));
        } else {
            catalog.merge(self.managed_commands.clone());
        }
        for runtime in &self.plugin_runtimes {
            for registration in runtime
                .registrations()
                .contributions
                .into_iter()
                .filter(|item| item.kind == ContributionKind::Catalog)
            {
                let value =
                    match runtime.invoke_contribution(
                        ContributionKind::Catalog,
                        &registration.name,
                        &json!({"schema_version": catalog.schema_version}),
                    ) {
                        Ok(value) => value,
                        Err(error) => {
                            self.errors.push(error.with_context(format!(
                                "catalog contribution: {}",
                                registration.name
                            )));
                            continue;
                        }
                    };
                let mut output = match serde_json::from_value::<CatalogContributionOutput>(value) {
                    Ok(output) => output,
                    Err(error) => {
                        self.errors.push(contribution_shape_error(
                            &registration.name,
                            "catalog providers must return { commands = CommandSpec[] }",
                            error,
                        ));
                        continue;
                    }
                };
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
                    self.errors.push(
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
        for runtime in &self.plugin_runtimes {
            let Some(registration) = runtime
                .registrations()
                .contributions
                .into_iter()
                .find(|item| item.kind == ContributionKind::Panel && item.name == name)
            else {
                continue;
            };
            let value = runtime.invoke_contribution(ContributionKind::Panel, name, context)?;
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
        std::mem::take(&mut self.errors)
    }

    /// Dispatch one immutable record to every active runtime. Individual
    /// handler failures are retained as diagnostics and never stop later
    /// runtimes or handlers.
    pub fn dispatch_event(&mut self, data: ExtensionEventData) -> Vec<ExtensionAction> {
        self.ensure_loaded();
        self.event_sequence = self.event_sequence.saturating_add(1);
        let event = ExtensionEvent::new(self.event_sequence, data);
        let mut actions = Vec::new();
        let outcomes = thread::scope(|scope| {
            let handles = self
                .plugin_runtimes
                .iter_mut()
                .map(|runtime| scope.spawn(|| runtime.dispatch_extension_event(&event)))
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| handle.join())
                .collect::<Vec<_>>()
        });
        for outcome in outcomes {
            match outcome {
                Ok(Ok(reports)) => {
                    for report in reports {
                        if let Some(error) = report.error {
                            self.errors.push(
                                error.with_context(format!("event handler: {}", report.handler)),
                            );
                        } else {
                            actions.extend(report.actions);
                        }
                    }
                }
                Ok(Err(error)) => self
                    .errors
                    .push(error.with_context("extension event dispatch")),
                Err(_) => self.errors.push(
                    ShellError::new(ErrorCode::Lua, "extension event worker panicked")
                        .with_help("Disable the failing plugin and restart Quirl"),
                ),
            }
        }
        actions
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
                snapshot_managed_plugins(root, &mut errors, cancellation)
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
    ) -> Result<BuiltExtensionGeneration, ShellError> {
        if let Some(error) = snapshot.errors.into_iter().next() {
            return Err(error);
        }

        let mut config = ConfigStore::default();
        if let Some(path) = &snapshot.config {
            let runtime = LuaRuntime::new(LuaPolicy::config())?;
            config.reload(&runtime, path)?;
        }

        let mut plugin_runtimes = Vec::with_capacity(snapshot.plugins.len());
        let mut contributions = Vec::new();
        let mut managed_commands = Vec::new();
        for plugin in &snapshot.plugins {
            managed_commands.extend(plugin.catalog_commands.clone());
            if plugin.runtime == PluginRuntime::TrustedLua {
                let runtime = crate::plugin::trusted_lua_runtime(&plugin.grants)?;
                let registrations = if let Some(source) = &plugin.verified_source {
                    runtime.load_plugin_source(source, &plugin.path.display().to_string())?
                } else {
                    runtime.load_plugin_file(&plugin.path)?
                };
                contributions.extend(registrations.contributions);
                plugin_runtimes.push(runtime);
            }
        }
        validate_contribution_set(&contributions)?;
        Ok((
            config,
            snapshot
                .plugins
                .into_iter()
                .map(|plugin| plugin.path)
                .collect(),
            plugin_runtimes,
            managed_commands,
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
        paths
            .iter()
            .cloned()
            .map(|path| PluginCandidate {
                path,
                verified_source: None,
                runtime: PluginRuntime::TrustedLua,
                grants: grants.clone(),
                catalog_commands: Vec::new(),
            })
            .collect(),
        PluginFingerprint::Files(fingerprints),
    )
}

fn snapshot_managed_plugins(
    root: &Path,
    errors: &mut Vec<ShellError>,
    cancellation: &AtomicBool,
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
            errors.push(io_error(&lock_path, error));
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
        match managed_plugin_candidate_with_cancellation(locked, &mut fingerprints, cancellation) {
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
    managed_plugin_candidate_with_cancellation(locked, fingerprints, &AtomicBool::new(false))
}

fn managed_plugin_candidate_with_cancellation(
    locked: &LockedPlugin,
    fingerprints: &mut Vec<(PathBuf, FileFingerprint)>,
    cancellation: &AtomicBool,
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
        read_bounded_plugin_file(&manifest_path, MAX_PLUGIN_MANIFEST_BYTES, "plugin manifest")
            .map_err(|error| io_error(&manifest_path, error))?;
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
    let entry_bytes = read_bounded_plugin_file(&entry_path, MAX_PLUGIN_ENTRY_BYTES, "plugin entry")
        .map_err(|error| io_error(&entry_path, error))?;
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
    if locked.runtime == PluginRuntime::OutOfProcess {
        crate::plugin::execute_out_of_process_adapter(
            &manifest,
            &entry_path,
            &entry_bytes,
            &locked.granted_capabilities,
            Some(cancellation),
        )?;
    }
    let catalog_commands =
        normalize_plugin_commands(&manifest, &locked.source, &locked.source_checksum)?;
    Ok(PluginCandidate {
        path: entry_path,
        verified_source,
        runtime: locked.runtime,
        grants: locked.granted_capabilities.clone(),
        catalog_commands,
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
    match read_bounded_plugin_file(path, MAX_PLUGIN_LOCK_BYTES, "plugin source") {
        Ok(contents) => {
            let mut hasher = DefaultHasher::new();
            contents.hash(&mut hasher);
            Ok(FileFingerprint::Contents {
                bytes: contents.len(),
                hash: hasher.finish(),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileFingerprint::Missing),
        Err(error) => Err(io_error(path, error)),
    }
}

fn read_bounded_plugin_file(
    path: &Path,
    limit: usize,
    context: &str,
) -> Result<Vec<u8>, std::io::Error> {
    let file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size > limit as u64 {
        return Err(std::io::Error::other(format!(
            "{context} is {size} bytes; limit is {limit} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::Error::other(format!(
            "{context} exceeded its {limit}-byte limit while reading"
        )));
    }
    Ok(bytes)
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
          run = function(_) return "managed" end,
        }"#;
        fs::write(&entry, entry_source).unwrap();
        let manifest_path = package.join("plugin.toml");
        let manifest_source = r#"schema_version = 1

[plugin]
name = "managed"
version = "0.1.0"
entry = "plugin.lua"
quirl = ">=0.1, <0.2"
api = "0.1.0"
runtime = "trusted_lua"
summary = "Managed prompt test"

[capabilities]
request = ["commands.register", "prompt.register"]

[contributes]
commands = ["managed run"]

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
"#;
        fs::write(&manifest_path, manifest_source).unwrap();
        let manifest = parse_plugin_manifest(manifest_source, "plugin.toml").unwrap();
        let source = format!("file:{}", manifest_path.display());
        let (locked, _) = resolve_plugin(
            &manifest,
            manifest_source.as_bytes(),
            entry_source.as_bytes(),
            &source,
            &["commands.register".to_owned(), "prompt.register".to_owned()],
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

    #[test]
    fn plugin_drives_prompt_and_completion_surfaces() {
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugin.lua");
        let mut host = LuaExtensionHost::from_paths(None, vec![plugin]);
        let prompt = host.named_prompt_segments(Mode::Command, 0);
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
            host.named_prompt_segments(Mode::Command, 0)[0].name,
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

        let built = host.build_candidate(snapshot);
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
            host.named_prompt_segments(Mode::Command, 0)[0].name,
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
        assert!(host.take_errors().is_empty());
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
        let actions = host.dispatch_event(ExtensionEventData::SessionStart { restored: false });
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
}
