use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{ConfigStore, LuaPolicy, LuaRuntime, QuirlConfig};
use quirl_syntax::Mode;
use quirl_ui::{ExtensionCompleter, ExtensionSuggestion};
use serde_json::{json, Value};
use std::{
    collections::hash_map::DefaultHasher,
    env, fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub type SharedLuaExtensions = Arc<Mutex<LuaExtensionHost>>;

/// A rendered plugin prompt segment, retaining the registration name so callers
/// can order it using `config.prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedExtensionSegment {
    pub name: String,
    pub value: String,
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
    plugins: Vec<PathBuf>,
    errors: Vec<ShellError>,
}

#[derive(Debug)]
enum PluginSource {
    Fixed(Vec<PathBuf>),
    Directory(PathBuf),
}

pub struct LuaExtensionHost {
    /// `Some` for a config file that is watched even when it does not exist yet.
    config_path: Option<PathBuf>,
    plugin_source: PluginSource,
    plugin_paths: Vec<PathBuf>,
    config: ConfigStore,
    plugin_runtimes: Vec<LuaRuntime>,
    errors: Vec<ShellError>,
    observed_fingerprint: Option<ExtensionFingerprint>,
    revision: u64,
}

impl LuaExtensionHost {
    pub fn discover() -> Self {
        config_directory().map_or_else(|| Self::from_paths(None, Vec::new()), Self::from_directory)
    }

    /// Creates a host which watches `config.lua` and `plugins/*.lua` below a
    /// configuration directory. Plugin discovery is repeated on every poll.
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

    fn with_source(config_path: Option<PathBuf>, plugin_source: PluginSource) -> Self {
        Self {
            config_path,
            plugin_source,
            plugin_paths: Vec::new(),
            config: ConfigStore::default(),
            plugin_runtimes: Vec::new(),
            errors: Vec::new(),
            observed_fingerprint: None,
            revision: 0,
        }
    }

    /// Poll source files and atomically install a fully validated generation.
    ///
    /// Failed generations are remembered by fingerprint, so a malformed file
    /// reports one error and leaves the complete last-known-good generation live
    /// until its content changes again.
    pub fn reload_if_changed(&mut self) -> ExtensionReloadState {
        let snapshot = self.snapshot_sources();
        if self.observed_fingerprint.as_ref() == Some(&snapshot.fingerprint) {
            return ExtensionReloadState::Unchanged;
        }
        self.observed_fingerprint = Some(snapshot.fingerprint.clone());

        match self.build_candidate(snapshot) {
            Ok((config, plugin_paths, plugin_runtimes)) => {
                self.config = config;
                self.plugin_paths = plugin_paths;
                self.plugin_runtimes = plugin_runtimes;
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
        let before = &line[..position];
        let token_start = before
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
            .map_or(0, |(index, character)| index + character.len_utf8());
        let query = &before[token_start..];
        let context = json!({ "line": line, "cursor": position, "query": query });
        let mut suggestions = Vec::new();

        for runtime in &self.plugin_runtimes {
            for provider in runtime.registrations().completion_providers {
                if !provider_applies(before, &provider.command) {
                    continue;
                }
                match runtime.complete_with_provider(&provider.command, &context) {
                    Ok(Value::Array(values)) => {
                        for value in values {
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
        }
        suggestions
    }

    pub fn take_errors(&mut self) -> Vec<ShellError> {
        std::mem::take(&mut self.errors)
    }

    fn ensure_loaded(&mut self) {
        if self.observed_fingerprint.is_none() {
            self.reload_if_changed();
        }
    }

    fn snapshot_sources(&self) -> SourceSnapshot {
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
            PluginSource::Fixed(paths) => snapshot_plugin_paths(paths, &mut errors),
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
                    snapshot_plugin_paths(&paths, &mut errors)
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
    ) -> Result<(ConfigStore, Vec<PathBuf>, Vec<LuaRuntime>), ShellError> {
        if let Some(error) = snapshot.errors.into_iter().next() {
            return Err(error);
        }

        let mut config = ConfigStore::default();
        if let Some(path) = &snapshot.config {
            let runtime = LuaRuntime::new(LuaPolicy::config())?;
            config.reload(&runtime, path)?;
        }

        let mut plugin_runtimes = Vec::with_capacity(snapshot.plugins.len());
        for path in &snapshot.plugins {
            let runtime = LuaRuntime::new(LuaPolicy::config())?;
            runtime.load_plugin_file(path)?;
            plugin_runtimes.push(runtime);
        }
        Ok((config, snapshot.plugins, plugin_runtimes))
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
    env::var_os("QUIRL_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("XDG_CONFIG_HOME").map(|path| PathBuf::from(path).join("quirl")))
        .or_else(|| env::var_os("HOME").map(|path| PathBuf::from(path).join(".config/quirl")))
}

fn snapshot_plugin_paths(
    paths: &[PathBuf],
    errors: &mut Vec<ShellError>,
) -> (Vec<PathBuf>, PluginFingerprint) {
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
    (paths.to_vec(), PluginFingerprint::Files(fingerprints))
}

fn fingerprint_file(path: &Path) -> Result<FileFingerprint, ShellError> {
    match fs::read(path) {
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
    fn directory_sources_detect_add_remove_and_content_changes() {
        let directory = temporary_extension_directory();
        let config = directory.join("config.lua");
        let plugin = directory.join("plugins/fruit.lua");
        let mut host = LuaExtensionHost::from_directory(directory.clone());

        assert_eq!(
            host.reload_if_changed(),
            ExtensionReloadState::Reloaded { revision: 1 }
        );
        assert_eq!(host.active_config().editor.keymap, "helix");
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
        assert_eq!(host.active_config().editor.keymap, "helix");
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
}
