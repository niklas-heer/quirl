use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{ConfigStore, LuaPolicy, LuaRuntime};
use quirl_syntax::Mode;
use quirl_ui::{ExtensionCompleter, ExtensionSuggestion};
use serde_json::{json, Value};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub type SharedLuaExtensions = Arc<Mutex<LuaExtensionHost>>;

pub struct LuaExtensionHost {
    config_path: Option<PathBuf>,
    plugin_paths: Vec<PathBuf>,
    config: ConfigStore,
    plugin_runtimes: Vec<LuaRuntime>,
    errors: Vec<ShellError>,
    loaded: bool,
}

impl LuaExtensionHost {
    pub fn discover() -> Self {
        let directory = config_directory();
        let config_path = directory
            .as_ref()
            .map(|directory| directory.join("config.lua"))
            .filter(|path| path.is_file());
        let plugin_paths = directory
            .as_ref()
            .map(|directory| discover_plugins(&directory.join("plugins")))
            .unwrap_or_default();
        Self::from_paths(config_path, plugin_paths)
    }

    pub fn from_paths(config_path: Option<PathBuf>, mut plugin_paths: Vec<PathBuf>) -> Self {
        plugin_paths.sort();
        Self {
            config_path,
            plugin_paths,
            config: ConfigStore::default(),
            plugin_runtimes: Vec::new(),
            errors: Vec::new(),
            loaded: false,
        }
    }

    pub fn prompt_segments(&mut self, mode: Mode, last_status: i32) -> Vec<String> {
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
                    Ok(Some(value)) if !value.is_empty() => rendered.push(value),
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
        if self.loaded {
            return;
        }
        self.loaded = true;
        if let Some(path) = &self.config_path {
            match LuaRuntime::new(LuaPolicy::config()) {
                Ok(runtime) => {
                    if let Err(error) = self.config.reload(&runtime, path) {
                        self.errors.push(error);
                    }
                }
                Err(error) => self.errors.push(error),
            }
        }
        for path in &self.plugin_paths {
            match LuaRuntime::new(LuaPolicy::config()) {
                Ok(runtime) => match runtime.load_plugin_file(path) {
                    Ok(_) => self.plugin_runtimes.push(runtime),
                    Err(error) => self.errors.push(error),
                },
                Err(error) => self.errors.push(error),
            }
        }
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

fn discover_plugins(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "lua"))
        .collect()
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

    #[test]
    fn plugin_drives_prompt_and_completion_surfaces() {
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugin.lua");
        let mut host = LuaExtensionHost::from_paths(None, vec![plugin]);
        let prompt = host.prompt_segments(Mode::Command, 0);
        assert_eq!(prompt.len(), 1);
        assert!(!prompt[0].is_empty());

        let suggestions = host.complete("deploy --environment prod", 25);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, "production");
        assert!(host.take_errors().is_empty());
    }
}
