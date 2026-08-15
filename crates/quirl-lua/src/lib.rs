//! Restricted Lua 5.4 runtime for Quirl configuration, scripts, and trusted plugins.

use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, RegistryKey, StdLib, Table, Value,
    VmState,
};
use quirl_core::{CommandRunner, ErrorCode, ShellError};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const HOOK_GRANULARITY: u64 = 10_000;
const RESOURCE_LIMIT_SENTINEL: &str = "quirl resource limit exceeded";
const DEFAULT_PROMPT_DEADLINE_MS: u64 = 8;
const MAX_CALLBACK_DEADLINE_MS: u64 = 100;
const COMPLETION_CALLBACK_DEADLINE: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Copy)]
pub struct LuaPolicy {
    pub allow_process: bool,
    pub memory_limit_bytes: usize,
    pub instruction_limit: u64,
    pub wall_time: Duration,
}

impl LuaPolicy {
    pub const fn script() -> Self {
        Self {
            allow_process: true,
            memory_limit_bytes: 8 * 1024 * 1024,
            instruction_limit: 2_000_000,
            wall_time: Duration::from_millis(250),
        }
    }

    pub const fn config() -> Self {
        Self {
            allow_process: false,
            memory_limit_bytes: 4 * 1024 * 1024,
            instruction_limit: 500_000,
            wall_time: Duration::from_millis(100),
        }
    }
}

impl Default for LuaPolicy {
    fn default() -> Self {
        Self::script()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuirlConfig {
    #[serde(default)]
    pub editor: EditorConfig,
    #[serde(default)]
    pub picker: PickerConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    pub keymap: String,
    pub semantic_hints: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            keymap: "helix".to_owned(),
            semantic_hints: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PickerConfig {
    pub layout: String,
    pub preview: bool,
}

impl Default for PickerConfig {
    fn default() -> Self {
        Self {
            layout: "adaptive".to_owned(),
            preview: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub left: Vec<String>,
    pub right: Vec<String>,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            left: vec!["directory".to_owned(), "git_branch".to_owned()],
            right: vec![
                "jobs".to_owned(),
                "duration".to_owned(),
                "status".to_owned(),
            ],
        }
    }
}

impl QuirlConfig {
    fn validate(&self, source: &str) -> Result<(), ShellError> {
        if !matches!(self.editor.keymap.as_str(), "helix" | "emacs" | "vim") {
            return Err(validation_error(
                source,
                "editor.keymap must be `helix`, `emacs`, or `vim`",
            ));
        }
        if !matches!(self.picker.layout.as_str(), "adaptive" | "bottom" | "full") {
            return Err(validation_error(
                source,
                "picker.layout must be `adaptive`, `bottom`, or `full`",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PromptRegistration {
    pub name: String,
    #[serde(default = "default_prompt_deadline_ms")]
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompletionRegistration {
    pub command: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRegistrations {
    pub prompt_segments: Vec<PromptRegistration>,
    pub completion_providers: Vec<CompletionRegistration>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostParameter {
    pub name: &'static str,
    pub lua_type: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostApiSpec {
    pub path: &'static str,
    pub summary: &'static str,
    pub parameters: &'static [HostParameter],
    pub returns: &'static str,
    pub capability: Option<&'static str>,
}

const COMMAND_PARAMETER: &[HostParameter] = &[HostParameter {
    name: "command",
    lua_type: "string",
}];
const CONFIG_PARAMETER: &[HostParameter] = &[HostParameter {
    name: "value",
    lua_type: "quirl.Config",
}];
const PROMPT_PARAMETER: &[HostParameter] = &[HostParameter {
    name: "spec",
    lua_type: "quirl.PromptSegment",
}];
const COMPLETION_PARAMETER: &[HostParameter] = &[HostParameter {
    name: "spec",
    lua_type: "quirl.CompletionProvider",
}];

pub const HOST_API: &[HostApiSpec] = &[
    HostApiSpec {
        path: "quirl.cwd",
        summary: "Return the current working directory.",
        parameters: &[],
        returns: "string",
        capability: None,
    },
    HostApiSpec {
        path: "quirl.process.run",
        summary: "Run a command through Quirl's compatibility shell.",
        parameters: COMMAND_PARAMETER,
        returns: "quirl.Result",
        capability: Some("process.spawn"),
    },
    HostApiSpec {
        path: "quirl.config",
        summary: "Return configuration for Rust schema validation.",
        parameters: CONFIG_PARAMETER,
        returns: "quirl.Config",
        capability: None,
    },
    HostApiSpec {
        path: "quirl.prompt.add_segment",
        summary: "Register a deadline-bounded prompt segment.",
        parameters: PROMPT_PARAMETER,
        returns: "nil",
        capability: Some("prompt.register"),
    },
    HostApiSpec {
        path: "quirl.completion.add_provider",
        summary: "Register a semantic completion provider.",
        parameters: COMPLETION_PARAMETER,
        returns: "nil",
        capability: Some("completion.register"),
    },
];

#[derive(Debug)]
struct Budget {
    remaining_instructions: u64,
    deadline: Instant,
}

#[derive(Debug, Default)]
struct PluginCallbacks {
    prompt_segments: HashMap<String, PromptCallback>,
    completion_providers: HashMap<String, RegistryKey>,
}

#[derive(Debug)]
struct PromptCallback {
    function: RegistryKey,
    deadline: Duration,
}

#[derive(Debug, Clone)]
pub struct LuaCancellation {
    cancelled: Arc<AtomicBool>,
}

impl LuaCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConfigStore {
    active: QuirlConfig,
}

impl ConfigStore {
    pub fn active(&self) -> &QuirlConfig {
        &self.active
    }

    pub fn reload(
        &mut self,
        runtime: &LuaRuntime,
        path: &Path,
    ) -> Result<&QuirlConfig, ShellError> {
        let candidate = runtime.load_config_file(path)?;
        self.active = candidate;
        Ok(&self.active)
    }
}

pub struct LuaRuntime {
    lua: Lua,
    policy: LuaPolicy,
    budget: Arc<Mutex<Budget>>,
    cancelled: Arc<AtomicBool>,
    registrations: Arc<Mutex<PluginRegistrations>>,
    callbacks: Arc<Mutex<PluginCallbacks>>,
}

impl LuaRuntime {
    pub fn new(policy: LuaPolicy) -> Result<Self, ShellError> {
        let libraries = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8;
        let lua = Lua::new_with(libraries, LuaOptions::default())
            .map_err(|error| lua_error(error, None, 0))?;
        let budget = Arc::new(Mutex::new(Budget {
            remaining_instructions: policy.instruction_limit,
            deadline: Instant::now() + policy.wall_time,
        }));
        let cancelled = Arc::new(AtomicBool::new(false));
        let registrations = Arc::new(Mutex::new(PluginRegistrations::default()));
        let callbacks = Arc::new(Mutex::new(PluginCallbacks::default()));

        install_restrictions(&lua).map_err(|error| lua_error(error, None, 0))?;
        install_budget_hook(&lua, Arc::clone(&budget), Arc::clone(&cancelled))
            .map_err(|error| lua_error(error, None, 0))?;
        install_host_api(
            &lua,
            policy,
            Arc::clone(&registrations),
            Arc::clone(&callbacks),
        )
        .map_err(|error| lua_error(error, None, 0))?;
        lua.set_memory_limit(policy.memory_limit_bytes)
            .map_err(|error| lua_error(error, None, 0))?;

        Ok(Self {
            lua,
            policy,
            budget,
            cancelled,
            registrations,
            callbacks,
        })
    }

    pub fn eval(&self, source: &str) -> Result<serde_json::Value, ShellError> {
        self.reset_budget();
        let value = self
            .lua
            .load(source)
            .set_name("eval")
            .eval::<Value>()
            .map_err(|error| lua_error(error, None, source.len()))?;
        self.value_to_json(value, None, source.len())
    }

    pub fn run_file(
        &self,
        path: &Path,
        arguments: &[String],
    ) -> Result<serde_json::Value, ShellError> {
        let source = read_source(path)?;
        self.run_source(&source, &path.display().to_string(), arguments)
    }

    /// Run source supplied by a non-file runner, such as `quirl run --lang lua -`.
    pub fn run_source(
        &self,
        source: &str,
        source_name: &str,
        arguments: &[String],
    ) -> Result<serde_json::Value, ShellError> {
        let path = Path::new(source_name);
        let source = normalize_shebang(source);
        lint_source(&source, path)?;
        self.reset_budget();
        let value = self
            .lua
            .load(&source)
            .set_name(source_name)
            .eval::<Value>()
            .map_err(|error| lua_error(error, Some(path), source.len()))?;
        let value = self.call_main_if_present(value, arguments, path, source.len())?;
        self.value_to_json(value, Some(path), source.len())
    }

    pub fn load_config_file(&self, path: &Path) -> Result<QuirlConfig, ShellError> {
        let source = read_source(path)?;
        lint_source(&source, path)?;
        self.reset_budget();
        let value = self
            .lua
            .load(&source)
            .set_name(path.to_string_lossy())
            .eval::<Value>()
            .map_err(|error| lua_error(error, Some(path), source.len()))?;
        let config = self.lua.from_value::<QuirlConfig>(value).map_err(|error| {
            validation_error(
                &path.display().to_string(),
                format!("configuration does not match the Rust schema: {error}"),
            )
        })?;
        config.validate(&path.display().to_string())?;
        Ok(config)
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin registry mutex may contain inconsistent registrations after a host callback panic"
    )]
    pub fn load_plugin_file(&self, path: &Path) -> Result<PluginRegistrations, ShellError> {
        let source = read_source(path)?;
        lint_source(&source, path)?;
        self.registrations
            .lock()
            .expect("plugin registration mutex poisoned")
            .clone_from(&PluginRegistrations::default());
        {
            let mut callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            callbacks.prompt_segments.clear();
            callbacks.completion_providers.clear();
        }
        self.reset_budget();
        self.lua
            .load(&source)
            .set_name(path.to_string_lossy())
            .exec()
            .map_err(|error| lua_error(error, Some(path), source.len()))?;
        let registrations = self
            .registrations
            .lock()
            .expect("plugin registration mutex poisoned")
            .clone();
        if registrations.prompt_segments.is_empty() && registrations.completion_providers.is_empty()
        {
            return Err(validation_error(
                &path.display().to_string(),
                "plugin did not register a prompt segment or completion provider",
            ));
        }
        Ok(registrations)
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin registry mutex may contain inconsistent registrations after a host callback panic"
    )]
    pub fn registrations(&self) -> PluginRegistrations {
        self.registrations
            .lock()
            .expect("plugin registration mutex poisoned")
            .clone()
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin callback mutex may contain inconsistent callbacks after a host callback panic"
    )]
    pub fn render_prompt_segment(
        &self,
        name: &str,
        context: &serde_json::Value,
    ) -> Result<Option<String>, ShellError> {
        let (function, deadline) = {
            let callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            let callback = callbacks.prompt_segments.get(name).ok_or_else(|| {
                validation_error(name, format!("unknown prompt segment `{name}`"))
            })?;
            let function = self
                .lua
                .registry_value::<Function>(&callback.function)
                .map_err(|error| lua_error(error, None, 0))?;
            (function, callback.deadline)
        };
        let context = self
            .lua
            .to_value(context)
            .map_err(|error| lua_error(error, None, 0))?;
        self.reset_budget_with_deadline(deadline);
        function
            .call::<Option<String>>(context)
            .map_err(|error| lua_error(error, None, 0))
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin callback mutex may contain inconsistent callbacks after a host callback panic"
    )]
    pub fn complete_with_provider(
        &self,
        command: &str,
        context: &serde_json::Value,
    ) -> Result<serde_json::Value, ShellError> {
        let function = {
            let callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            let key = callbacks.completion_providers.get(command).ok_or_else(|| {
                validation_error(command, format!("unknown completion provider `{command}`"))
            })?;
            self.lua
                .registry_value::<Function>(key)
                .map_err(|error| lua_error(error, None, 0))?
        };
        let context = self
            .lua
            .to_value(context)
            .map_err(|error| lua_error(error, None, 0))?;
        self.reset_budget_with_deadline(COMPLETION_CALLBACK_DEADLINE);
        let value = function
            .call::<Value>(context)
            .map_err(|error| lua_error(error, None, 0))?;
        let value = self.value_to_json(value, None, 0)?;
        validate_completion_result(&value, command)?;
        Ok(value)
    }

    pub fn test_file(&self, path: &Path) -> Result<usize, ShellError> {
        let source = read_source(path)?;
        self.test_source(&source, &path.display().to_string())
    }

    pub fn test_source(&self, source: &str, source_name: &str) -> Result<usize, ShellError> {
        let path = Path::new(source_name);
        let source = normalize_shebang(source);
        lint_source(&source, path)?;
        self.reset_budget();
        let tests = self
            .lua
            .load(&source)
            .set_name(source_name)
            .eval::<Table>()
            .map_err(|error| lua_error(error, Some(path), source.len()))?;
        let mut named_tests = Vec::new();
        for pair in tests.pairs::<String, Value>() {
            let (name, value) = pair.map_err(|error| lua_error(error, Some(path), source.len()))?;
            if !name.starts_with("test_") {
                continue;
            }
            let Value::Function(test) = value else {
                return Err(validation_error(
                    &path.display().to_string(),
                    format!("{name} must be a function"),
                ));
            };
            named_tests.push((name, test));
        }
        named_tests.sort_by(|left, right| left.0.cmp(&right.0));
        let mut count = 0;
        for (name, test) in named_tests {
            self.reset_budget();
            test.call::<()>(()).map_err(|error| {
                lua_error(error, Some(path), source.len()).with_context(format!("test: {name}"))
            })?;
            count += 1;
        }
        if count == 0 {
            return Err(validation_error(
                &path.display().to_string(),
                "test module must return at least one `test_*` function",
            ));
        }
        Ok(count)
    }

    pub fn check_file(path: &Path) -> Result<(), ShellError> {
        let source = read_source(path)?;
        Self::check_source(&source, &path.display().to_string())
    }

    /// Parse and lint Lua source without executing it.
    pub fn check_source(source: &str, source_name: &str) -> Result<(), ShellError> {
        let path = Path::new(source_name);
        let source = normalize_shebang(source);
        lint_source(&source, path)?;
        let runtime = Self::new(LuaPolicy::config())?;
        runtime
            .lua
            .load(&source)
            .set_name(source_name)
            .into_function()
            .map(|_| ())
            .map_err(|error| lua_error(error, Some(path), source.len()))
    }

    pub fn cancellation_token(&self) -> LuaCancellation {
        LuaCancellation {
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    pub fn clear_cancellation(&self) {
        self.cancelled.store(false, Ordering::Relaxed);
    }

    fn call_main_if_present(
        &self,
        value: Value,
        arguments: &[String],
        path: &Path,
        source_len: usize,
    ) -> Result<Value, ShellError> {
        let Value::Table(module) = value else {
            return Ok(value);
        };
        let Some(main) = module
            .get::<Option<Function>>("main")
            .map_err(|error| lua_error(error, Some(path), source_len))?
        else {
            return Ok(Value::Table(module));
        };
        let context = self
            .lua
            .create_table()
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        let lua_arguments = self
            .lua
            .create_sequence_from(arguments.iter().cloned())
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        context
            .set("args", lua_arguments)
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        main.call::<Value>(context)
            .map_err(|error| lua_error(error, Some(path), source_len))
    }

    fn value_to_json(
        &self,
        value: Value,
        path: Option<&Path>,
        source_len: usize,
    ) -> Result<serde_json::Value, ShellError> {
        if matches!(value, Value::Nil) {
            return Ok(serde_json::Value::Null);
        }
        self.lua
            .from_value(value)
            .map_err(|error| lua_error(error, path, source_len))
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned Lua budget mutex may contain an inconsistent budget after a host callback panic"
    )]
    fn reset_budget(&self) {
        self.reset_budget_with_deadline(self.policy.wall_time);
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned Lua budget mutex may contain an inconsistent budget after a host callback panic"
    )]
    fn reset_budget_with_deadline(&self, deadline: Duration) {
        let mut budget = self.budget.lock().expect("Lua budget mutex poisoned");
        budget.remaining_instructions = self.policy.instruction_limit;
        budget.deadline = Instant::now() + deadline.min(self.policy.wall_time);
    }
}

pub fn format_source(source: &str) -> String {
    if contains_long_bracket(source) {
        return format_trailing_whitespace(source);
    }
    let mut indentation = 0_usize;
    let mut lines = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let line = line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            lines.push(String::new());
            continue;
        }
        if index == 0 && trimmed.starts_with("#!") {
            lines.push(trimmed.to_owned());
            continue;
        }

        let shape = lua_code_shape(trimmed).trim().to_owned();
        let closes_block = starts_with_block_closer(&shape);
        if closes_block {
            indentation = indentation.saturating_sub(1);
        }
        lines.push(format!("{}{}", "  ".repeat(indentation), trimmed));
        if closes_block && starts_with_reopening_block(&shape) {
            indentation = indentation.saturating_add(1);
        } else {
            indentation = indentation.saturating_add(blocks_opened(&shape));
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let mut formatted = lines.join("\n");
    formatted.push('\n');
    formatted
}

fn format_trailing_whitespace(source: &str) -> String {
    if source.ends_with('\n') {
        source.to_owned()
    } else {
        format!("{source}\n")
    }
}

fn contains_long_bracket(source: &str) -> bool {
    let bytes = source.as_bytes();
    (0..bytes.len()).any(|index| long_bracket_open(bytes, index).is_some())
}

fn lua_code_shape(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut characters = line.chars().peekable();
    let mut quote = None;
    let mut escaped = false;
    while let Some(character) = characters.next() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            output.extend(std::iter::repeat_n(' ', character.len_utf8()));
            continue;
        }
        if matches!(character, '\'' | '"') {
            quote = Some(character);
            output.push(' ');
        } else if character == '-' && characters.peek() == Some(&'-') {
            break;
        } else {
            output.push(character);
        }
    }
    output
}

#[derive(Debug)]
struct MaskedLuaSource {
    code: String,
    line_comment_starts: HashSet<usize>,
}

fn mask_lua_source(source: &str) -> MaskedLuaSource {
    #[derive(Clone, Copy)]
    enum State {
        Normal,
        Quoted { quote: u8, escaped: bool },
        Long { equals: usize, comment: bool },
    }

    let bytes = source.as_bytes();
    let mut masked = bytes.to_vec();
    let mut comments = HashSet::new();
    let mut state = State::Normal;
    let mut index = 0;
    while index < bytes.len() {
        match state {
            State::Normal if matches!(bytes[index], b'\'' | b'"') => {
                mask_string_byte(&mut masked, index);
                state = State::Quoted {
                    quote: bytes[index],
                    escaped: false,
                };
                index += 1;
            }
            State::Normal if bytes[index..].starts_with(b"--") => {
                comments.insert(index);
                if let Some((equals, opener_len)) = long_bracket_open(bytes, index + 2) {
                    for position in index..index + 2 + opener_len {
                        mask_byte(&mut masked, position);
                    }
                    state = State::Long {
                        equals,
                        comment: true,
                    };
                    index += 2 + opener_len;
                } else {
                    while index < bytes.len() && bytes[index] != b'\n' {
                        mask_byte(&mut masked, index);
                        index += 1;
                    }
                }
            }
            State::Normal => {
                if let Some((equals, opener_len)) = long_bracket_open(bytes, index) {
                    for position in index..index + opener_len {
                        mask_string_byte(&mut masked, position);
                    }
                    state = State::Long {
                        equals,
                        comment: false,
                    };
                    index += opener_len;
                } else {
                    index += 1;
                }
            }
            State::Quoted { quote, escaped } => {
                let character = bytes[index];
                mask_string_byte(&mut masked, index);
                state = if escaped {
                    State::Quoted {
                        quote,
                        escaped: false,
                    }
                } else if character == b'\\' {
                    State::Quoted {
                        quote,
                        escaped: true,
                    }
                } else if character == quote {
                    State::Normal
                } else {
                    State::Quoted {
                        quote,
                        escaped: false,
                    }
                };
                index += 1;
            }
            State::Long { equals, comment } => {
                if long_bracket_close(bytes, index, equals) {
                    let closer_len = equals + 2;
                    for position in index..index + closer_len {
                        if comment {
                            mask_byte(&mut masked, position);
                        } else {
                            mask_string_byte(&mut masked, position);
                        }
                    }
                    index += closer_len;
                    state = State::Normal;
                } else {
                    if comment {
                        mask_byte(&mut masked, index);
                    } else {
                        mask_string_byte(&mut masked, index);
                    }
                    index += 1;
                }
            }
        }
    }
    let code = match String::from_utf8(masked) {
        Ok(code) => code,
        Err(_) => source.to_owned(),
    };
    MaskedLuaSource {
        code,
        line_comment_starts: comments,
    }
}

fn long_bracket_open(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    if bytes.get(index) != Some(&b'[') {
        return None;
    }
    let mut cursor = index + 1;
    while bytes.get(cursor) == Some(&b'=') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'[')).then_some((cursor - index - 1, cursor - index + 1))
}

fn long_bracket_close(bytes: &[u8], index: usize, equals: usize) -> bool {
    bytes.get(index) == Some(&b']')
        && bytes
            .get(index + 1..index + 1 + equals)
            .is_some_and(|characters| characters.iter().all(|character| *character == b'='))
        && bytes.get(index + 1 + equals) == Some(&b']')
}

fn mask_byte(bytes: &mut [u8], index: usize) {
    if !matches!(bytes[index], b'\n' | b'\r') {
        bytes[index] = b' ';
    }
}

fn mask_string_byte(bytes: &mut [u8], index: usize) {
    if !matches!(bytes[index], b'\n' | b'\r') {
        bytes[index] = b'_';
    }
}

fn starts_with_block_closer(shape: &str) -> bool {
    ["end", "else", "elseif", "until"]
        .iter()
        .any(|keyword| starts_with_keyword(shape, keyword))
        || shape.starts_with('}')
}

fn starts_with_reopening_block(shape: &str) -> bool {
    starts_with_keyword(shape, "else") || starts_with_keyword(shape, "elseif")
}

fn starts_with_keyword(source: &str, keyword: &str) -> bool {
    source == keyword
        || source
            .strip_prefix(keyword)
            .and_then(|rest| rest.chars().next())
            .is_some_and(|next| next.is_whitespace() || matches!(next, ',' | ';' | ')' | '}' | ']'))
}

fn blocks_opened(shape: &str) -> usize {
    if shape.is_empty() || shape.ends_with(" end") || shape == "end" {
        return 0;
    }
    let keyword_block = starts_with_keyword(shape, "function")
        || shape.contains(" function(")
        || shape.contains(" function (")
        || starts_with_keyword(shape, "local function")
        || starts_with_keyword(shape, "repeat")
        || shape.ends_with(" then")
        || shape.ends_with(" do");
    let braces = shape.chars().filter(|character| *character == '{').count();
    let closing_braces = shape.chars().filter(|character| *character == '}').count();
    usize::from(keyword_block).saturating_add(braces.saturating_sub(closing_braces))
}

pub fn format_file(path: &Path, check: bool) -> Result<bool, ShellError> {
    let source = fs::read_to_string(path).map_err(|error| script_read_error(path, error))?;
    let formatted = format_source(&source);
    let changed = source != formatted;
    if changed && !check {
        fs::write(path, formatted).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                format!("cannot write formatted Lua file {}", path.display()),
            )
            .with_context(error.to_string())
        })?;
    }
    Ok(changed)
}

pub fn sdk_lua() -> String {
    let mut output = String::from(
        "---@meta quirl\n\n---@class quirl.Result\n---@field ok boolean\n---@field value? any\n---@field error? string\n\n---@class quirl.Config\n---@field editor table\n---@field picker table\n---@field prompt table\n\n---@class quirl.PromptSegment\n---@field name string\n---@field deadline_ms? integer\n---@field render fun(context: table): string?\n\n---@class quirl.CompletionProvider\n---@field command string\n---@field complete fun(context: table): table\n\nquirl = {}\n\n",
    );
    for spec in HOST_API {
        output.push_str(&format!("---{}\n", spec.summary));
        for parameter in spec.parameters {
            output.push_str(&format!(
                "---@param {} {}\n",
                parameter.name, parameter.lua_type
            ));
        }
        if spec.returns != "nil" {
            output.push_str(&format!("---@return {}\n", spec.returns));
        }
        let arguments = spec
            .parameters
            .iter()
            .map(|parameter| parameter.name)
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("function {}({arguments}) end\n\n", spec.path));
    }
    output.pop();
    output
}

pub fn sdk_json() -> Result<String, ShellError> {
    #[derive(Serialize)]
    struct HostApiDocument<'a> {
        document_type: &'static str,
        schema_version: u32,
        module: &'static str,
        module_version: &'static str,
        functions: &'a [HostApiSpec],
    }
    let document = HostApiDocument {
        document_type: "quirl.host_api",
        schema_version: 1,
        module: "quirl",
        module_version: env!("CARGO_PKG_VERSION"),
        functions: HOST_API,
    };
    serde_json::to_string_pretty(&document).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not serialize the Lua SDK")
            .with_context(error.to_string())
    })
}

pub fn sdk_markdown() -> String {
    let mut output = format!(
        "# Quirl Lua SDK\n\nModule: `quirl`\n\nVersion: `{}`\n\nSchema version: `1`\n\n",
        env!("CARGO_PKG_VERSION")
    );
    for spec in HOST_API {
        let parameters = spec
            .parameters
            .iter()
            .map(|parameter| format!("{}: {}", parameter.name, parameter.lua_type))
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "## `{}`\n\n`{}({parameters}) -> {}`\n\n{}\n\n",
            spec.path, spec.path, spec.returns, spec.summary
        ));
        if !spec.parameters.is_empty() {
            output.push_str("| Parameter | Type |\n| --- | --- |\n");
            for parameter in spec.parameters {
                output.push_str(&format!(
                    "| `{}` | `{}` |\n",
                    parameter.name, parameter.lua_type
                ));
            }
            output.push('\n');
        }
        output.push_str(&format!("Returns: `{}`\n\n", spec.returns));
        if let Some(capability) = spec.capability {
            output.push_str(&format!("Capability: `{capability}`\n\n"));
        }
    }
    output
}

fn install_restrictions(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in [
        "debug", "dofile", "io", "loadfile", "os", "package", "require",
    ] {
        globals.set(name, Value::Nil)?;
    }
    Ok(())
}

fn install_budget_hook(
    lua: &Lua,
    budget: Arc<Mutex<Budget>>,
    cancelled: Arc<AtomicBool>,
) -> mlua::Result<()> {
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(HOOK_GRANULARITY as u32),
        move |_, _| {
            let mut budget = budget.lock().map_err(|_| {
                mlua::Error::RuntimeError("quirl budget state is unavailable".to_owned())
            })?;
            if cancelled.load(Ordering::Relaxed)
                || budget.remaining_instructions < HOOK_GRANULARITY
                || Instant::now() > budget.deadline
            {
                return Err(mlua::Error::RuntimeError(
                    RESOURCE_LIMIT_SENTINEL.to_owned(),
                ));
            }
            budget.remaining_instructions -= HOOK_GRANULARITY;
            Ok(VmState::Continue)
        },
    )
}

fn install_host_api(
    lua: &Lua,
    policy: LuaPolicy,
    registrations: Arc<Mutex<PluginRegistrations>>,
    callbacks: Arc<Mutex<PluginCallbacks>>,
) -> mlua::Result<()> {
    let quirl = lua.create_table()?;
    quirl.set("version", env!("CARGO_PKG_VERSION"))?;
    quirl.set(
        "cwd",
        lua.create_function(|_, ()| {
            Ok(std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_default())
        })?,
    )?;
    quirl.set("config", lua.create_function(|_, value: Table| Ok(value))?)?;

    let process = lua.create_table()?;
    process.set(
        "run",
        lua.create_function(move |lua, command: String| {
            if !policy.allow_process {
                return Err(mlua::Error::RuntimeError(
                    "capability denied: process.spawn".to_owned(),
                ));
            }
            let outcome = CommandRunner::default()
                .execute_capture(&command)
                .map_err(mlua::Error::external)?;
            let result = lua.create_table()?;
            result.set("ok", outcome.status == 0)?;
            result.set("status", outcome.status)?;
            result.set("value", outcome.stdout.unwrap_or_default())?;
            result.set("error", outcome.stderr.unwrap_or_default())?;
            Ok(result)
        })?,
    )?;
    quirl.set("process", process)?;

    let prompt = lua.create_table()?;
    let prompt_registrations = Arc::clone(&registrations);
    let prompt_callbacks = Arc::clone(&callbacks);
    prompt.set(
        "add_segment",
        lua.create_function(move |lua, spec: Table| {
            let render = spec.get::<Function>("render").map_err(|_| {
                mlua::Error::RuntimeError("prompt segment `render` must be a function".to_owned())
            })?;
            let registration: PromptRegistration =
                deserialize_registration(lua, &spec, "render", "prompt segment")?;
            validate_registration_name("prompt segment name", &registration.name)?;
            if !(1..=MAX_CALLBACK_DEADLINE_MS).contains(&registration.deadline_ms) {
                return Err(mlua::Error::RuntimeError(format!(
                    "prompt segment `deadline_ms` must be between 1 and {MAX_CALLBACK_DEADLINE_MS}"
                )));
            }
            let callback = lua.create_registry_value(render)?;
            let mut callbacks = prompt_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            if callbacks.prompt_segments.contains_key(&registration.name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "duplicate prompt segment `{}`",
                    registration.name
                )));
            }
            prompt_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?
                .prompt_segments
                .push(registration.clone());
            let deadline = Duration::from_millis(registration.deadline_ms);
            callbacks.prompt_segments.insert(
                registration.name,
                PromptCallback {
                    function: callback,
                    deadline,
                },
            );
            Ok(())
        })?,
    )?;
    quirl.set("prompt", prompt)?;

    let completion = lua.create_table()?;
    let completion_registrations = registrations;
    let completion_callbacks = callbacks;
    completion.set(
        "add_provider",
        lua.create_function(move |lua, spec: Table| {
            let complete = spec.get::<Function>("complete").map_err(|_| {
                mlua::Error::RuntimeError(
                    "completion provider `complete` must be a function".to_owned(),
                )
            })?;
            let registration: CompletionRegistration =
                deserialize_registration(lua, &spec, "complete", "completion provider")?;
            validate_registration_name("completion provider command", &registration.command)?;
            let callback = lua.create_registry_value(complete)?;
            let mut callbacks = completion_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            if callbacks
                .completion_providers
                .contains_key(&registration.command)
            {
                return Err(mlua::Error::RuntimeError(format!(
                    "duplicate completion provider `{}`",
                    registration.command
                )));
            }
            completion_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?
                .completion_providers
                .push(registration.clone());
            callbacks
                .completion_providers
                .insert(registration.command, callback);
            Ok(())
        })?,
    )?;
    quirl.set("completion", completion)?;
    lua.globals().set("quirl", quirl)
}

fn default_prompt_deadline_ms() -> u64 {
    DEFAULT_PROMPT_DEADLINE_MS
}

fn deserialize_registration<T: DeserializeOwned>(
    lua: &Lua,
    spec: &Table,
    callback_field: &str,
    description: &str,
) -> mlua::Result<T> {
    let metadata = lua.create_table()?;
    for pair in spec.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        let is_callback = match &key {
            Value::String(name) => name.to_str()?.as_bytes() == callback_field.as_bytes(),
            _ => false,
        };
        if !is_callback {
            metadata.raw_set(key, value)?;
        }
    }
    lua.from_value(Value::Table(metadata)).map_err(|error| {
        mlua::Error::RuntimeError(format!("invalid {description} registration: {error}"))
    })
}

fn validate_registration_name(description: &str, value: &str) -> mlua::Result<()> {
    if value.trim().is_empty() {
        return Err(mlua::Error::RuntimeError(format!(
            "{description} must not be empty"
        )));
    }
    Ok(())
}

fn validate_completion_result(value: &serde_json::Value, command: &str) -> Result<(), ShellError> {
    let serde_json::Value::Array(items) = value else {
        return Err(validation_error(
            command,
            "completion provider must return an array",
        ));
    };
    for (index, item) in items.iter().enumerate() {
        match item {
            serde_json::Value::String(_) => {}
            serde_json::Value::Object(object)
                if object
                    .get("value")
                    .is_some_and(serde_json::Value::is_string)
                    && object.iter().all(|(key, value)| match key.as_str() {
                        "value" | "display" | "summary" | "detail" => value.is_string(),
                        _ => false,
                    }) => {}
            _ => {
                return Err(validation_error(
                    command,
                    format!(
                        "completion item {index} must be a string or an object with a string `value` and optional string display fields"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn lint_source(source: &str, path: &Path) -> Result<(), ShellError> {
    let masked = mask_lua_source(source);
    validate_annotations(source, path, &masked.line_comment_starts)?;
    validate_host_api_references(&masked.code, path)?;
    const FORBIDDEN: &[(&str, &str)] = &[
        (
            "io.",
            "filesystem access must use an explicit Quirl capability",
        ),
        (
            "os.",
            "operating-system access must use an explicit Quirl capability",
        ),
        ("debug.", "the debug library is unavailable in Quirl Lua"),
        (
            "require(",
            "module loading must use Quirl's package resolver",
        ),
        ("dofile(", "dofile is unavailable in Quirl Lua"),
        ("loadfile(", "loadfile is unavailable in Quirl Lua"),
    ];
    let mut error = ShellError::new(ErrorCode::Validation, "Lua validation failed")
        .with_help("Use the generated `quirl` SDK instead of ambient Lua capabilities");
    let mut offset = 0;
    for (line, code) in source.lines().zip(masked.code.lines()) {
        for (needle, message) in FORBIDDEN {
            if let Some(column) = code.find(needle) {
                error = error.with_label(
                    Some(path.display().to_string()),
                    offset + column,
                    offset + column + needle.len(),
                    *message,
                );
            }
        }
        offset += line.len() + 1;
    }
    if error.details.labels.is_empty() {
        Ok(())
    } else {
        Err(error)
    }
}

fn validate_host_api_references(code: &str, path: &Path) -> Result<(), ShellError> {
    let bytes = code.as_bytes();
    let mut error = ShellError::new(ErrorCode::Validation, "Lua host API validation failed")
        .with_help("Use a function and signature from `quirl sdk --format markdown`");
    let mut offset = 0;
    while offset + "quirl.".len() <= bytes.len() {
        let Some(relative) = code[offset..].find("quirl.") else {
            break;
        };
        let start = offset + relative;
        if start > 0 && is_lua_identifier_byte(bytes[start - 1]) {
            offset = start + "quirl.".len();
            continue;
        }
        let mut end = start + "quirl.".len();
        while end < bytes.len() && (is_lua_identifier_byte(bytes[end]) || bytes[end] == b'.') {
            end += 1;
        }
        while end > start && bytes[end - 1] == b'.' {
            end -= 1;
        }
        let symbol = &code[start..end];
        let specification = HOST_API.iter().find(|spec| spec.path == symbol);
        let namespace = HOST_API
            .iter()
            .any(|spec| spec.path.starts_with(&format!("{symbol}.")));
        if specification.is_none() && !namespace {
            error = error.with_label(
                Some(path.display().to_string()),
                start,
                end,
                format!("unknown Quirl host symbol `{symbol}`"),
            );
            offset = end.max(start + 1);
            continue;
        }
        if let Some(specification) = specification {
            let next = bytes[end..]
                .iter()
                .position(|byte| !byte.is_ascii_whitespace())
                .map(|relative| end + relative);
            let actual_arguments =
                match next.and_then(|next| bytes.get(next).map(|byte| (next, byte))) {
                    Some((open, b'(')) => count_parenthesized_arguments(code, open),
                    Some((_, b'{')) => Some(1),
                    _ => None,
                };
            if let Some(actual) = actual_arguments {
                let expected = specification.parameters.len();
                if actual != expected {
                    error = error.with_label(
                        Some(path.display().to_string()),
                        start,
                        end,
                        format!(
                            "`{symbol}` expects {expected} argument(s), but this call provides {actual}"
                        ),
                    );
                }
            }
        }
        offset = end.max(start + 1);
    }
    if error.details.labels.is_empty() {
        Ok(())
    } else {
        Err(error)
    }
}

fn is_lua_identifier_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn count_parenthesized_arguments(code: &str, open: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut parentheses = 1_usize;
    let mut braces = 0_usize;
    let mut brackets = 0_usize;
    let mut commas = 0_usize;
    let mut has_argument = false;
    let mut offset = open + 1;
    while let Some(byte) = bytes.get(offset) {
        match byte {
            b'(' => parentheses += 1,
            b')' => {
                parentheses = parentheses.saturating_sub(1);
                if parentheses == 0 {
                    return Some(if has_argument { commas + 1 } else { 0 });
                }
            }
            b'{' => braces += 1,
            b'}' => braces = braces.saturating_sub(1),
            b'[' => brackets += 1,
            b']' => brackets = brackets.saturating_sub(1),
            b',' if parentheses == 1 && braces == 0 && brackets == 0 => commas += 1,
            byte if !byte.is_ascii_whitespace()
                && parentheses == 1
                && braces == 0
                && brackets == 0 =>
            {
                has_argument = true;
            }
            _ => {}
        }
        offset += 1;
    }
    None
}

fn validate_annotations(
    source: &str,
    path: &Path,
    line_comment_starts: &HashSet<usize>,
) -> Result<(), ShellError> {
    let mut error = ShellError::new(ErrorCode::Validation, "Lua annotation validation failed")
        .with_help(
            "Use the supported LuaLS-compatible `meta`, `module`, `class`, `field`, `param`, `return`, or `type` annotations",
        );
    let mut offset = 0;
    for line in source.lines() {
        let leading = line.len().saturating_sub(line.trim_start().len());
        let trimmed = line.trim_start();
        let annotation_start = offset + leading;
        if let Some(annotation) = trimmed
            .strip_prefix("---@")
            .filter(|_| line_comment_starts.contains(&annotation_start))
        {
            let annotation = annotation.trim();
            let (kind, body) = annotation
                .split_once(char::is_whitespace)
                .map_or((annotation, ""), |(kind, body)| (kind, body.trim()));
            let problem = validate_annotation(kind, body);
            if let Some(problem) = problem {
                let start = offset + leading;
                error = error.with_label(
                    Some(path.display().to_string()),
                    start,
                    start + trimmed.len(),
                    problem,
                );
            }
        }
        offset += line.len() + 1;
    }
    if error.details.labels.is_empty() {
        Ok(())
    } else {
        Err(error)
    }
}

fn validate_annotation(kind: &str, body: &str) -> Option<String> {
    match kind {
        "meta" if body.is_empty() || valid_qualified_name(body) => None,
        "module" if !body.is_empty() => validate_type_expression(body)
            .err()
            .map(|problem| format!("invalid `@module` name: {problem}")),
        "class" => required_type_annotation(kind, body),
        "return" | "type" => required_type_annotation(kind, body),
        "field" | "param" => {
            let Some((name, lua_type)) = body.split_once(char::is_whitespace) else {
                return Some(format!("`@{kind}` requires a name and a type"));
            };
            if (kind == "param" && name == "...") || valid_annotation_name(name) {
                validate_type_expression(lua_type.trim())
                    .err()
                    .map(|problem| format!("invalid `@{kind}` type: {problem}"))
            } else {
                Some(format!("invalid `@{kind}` name `{name}`"))
            }
        }
        "" => Some("annotation name is missing".to_owned()),
        _ => Some(format!(
            "unsupported Lua annotation `@{kind}`; supported annotations are meta, module, class, field, param, return, and type"
        )),
    }
}

fn required_type_annotation(kind: &str, body: &str) -> Option<String> {
    if body.is_empty() {
        Some(format!("`@{kind}` requires a type"))
    } else {
        validate_type_expression(body)
            .err()
            .map(|problem| format!("invalid `@{kind}` type: {problem}"))
    }
}

fn valid_annotation_name(name: &str) -> bool {
    let name = name.strip_suffix('?').unwrap_or(name);
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn valid_qualified_name(name: &str) -> bool {
    !name.is_empty() && name.split('.').all(valid_annotation_name)
}

fn validate_type_expression(expression: &str) -> Result<(), &'static str> {
    if expression.trim().is_empty() {
        return Err("type is empty");
    }
    let mut delimiters = Vec::new();
    for character in expression.chars() {
        match character {
            '<' | '[' | '(' | '{' => delimiters.push(character),
            '>' | ']' | ')' | '}' => {
                let expected = match character {
                    '>' => '<',
                    ']' => '[',
                    ')' => '(',
                    '}' => '{',
                    _ => return Err("unexpected delimiter"),
                };
                if delimiters.pop() != Some(expected) {
                    return Err("delimiters are not balanced");
                }
            }
            character
                if character.is_ascii_alphanumeric()
                    || character.is_ascii_whitespace()
                    || matches!(
                        character,
                        '_' | '.' | '?' | '|' | ',' | ':' | '-' | '"' | '\''
                    ) => {}
            _ => return Err("type contains an unsupported character"),
        }
    }
    if delimiters.is_empty() {
        Ok(())
    } else {
        Err("delimiters are not balanced")
    }
}

fn read_source(path: &Path) -> Result<String, ShellError> {
    let source = fs::read_to_string(path).map_err(|error| script_read_error(path, error))?;
    Ok(normalize_shebang(&source))
}

fn normalize_shebang(source: &str) -> String {
    source
        .strip_prefix("#!")
        .map_or_else(|| source.to_owned(), |source| format!("--{source}"))
}

fn script_read_error(path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::ScriptRead,
        format!("cannot read Lua file {}", path.display()),
    )
    .with_context(error.to_string())
}

fn lua_error(error: mlua::Error, path: Option<&Path>, source_len: usize) -> ShellError {
    let message = error.to_string();
    let code = if message.contains(RESOURCE_LIMIT_SENTINEL) || message.contains("memory error") {
        ErrorCode::ResourceLimit
    } else {
        ErrorCode::Lua
    };
    let summary = if code == ErrorCode::ResourceLimit {
        "Lua exceeded its configured resource budget"
    } else {
        "Lua could not load or evaluate the program"
    };
    ShellError::new(code, summary)
        .with_context(message)
        .with_help("Run `quirl check <file> --format json` for a machine-readable diagnostic")
        .with_label(
            path.map(|path| path.display().to_string()),
            0,
            source_len,
            "invalid or failed Lua program",
        )
}

fn validation_error(source: &str, message: impl Into<String>) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "Lua value failed Rust schema validation",
    )
    .with_context(message)
    .with_label(Some(source.to_owned()), 0, 0, "schema mismatch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_values_in_a_persistent_vm() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        runtime.eval("answer = 40").unwrap();
        assert_eq!(runtime.eval("return answer + 2").unwrap(), 42);
    }

    #[test]
    fn config_is_deserialized_and_validated_by_rust() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let source = r#"return quirl.config {
          editor = { keymap = "helix", semantic_hints = true },
          picker = { layout = "adaptive", preview = true },
          prompt = { left = { "directory" }, right = { "status" } },
        }"#;
        let value = runtime.lua.load(source).eval::<Value>().unwrap();
        let config = runtime.lua.from_value::<QuirlConfig>(value).unwrap();
        config.validate("test").unwrap();
        assert_eq!(config.editor.keymap, "helix");
    }

    #[test]
    fn computed_config_is_evaluated_before_authoritative_schema_validation() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let value = runtime
            .lua
            .load(
                r#"local keymaps = { "vim", "helix" }
                local selected = 1
                return quirl.config {
                  editor = { keymap = keymaps[selected], semantic_hints = selected == 1 },
                  picker = { layout = "adaptive", preview = true },
                  prompt = { left = {}, right = {} },
                }"#,
            )
            .eval::<Value>()
            .unwrap();
        let config = runtime.lua.from_value::<QuirlConfig>(value).unwrap();
        config.validate("computed-config.lua").unwrap();
        assert_eq!(config.editor.keymap, "vim");
        assert!(config.editor.semantic_hints);
    }

    #[test]
    fn invalid_reload_preserves_last_known_good_config() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let path = std::env::temp_dir().join(format!(
            "quirl-config-store-{}-{}.lua",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::write(
            &path,
            r#"return quirl.config {
              editor = { keymap = "vim", semantic_hints = true },
              picker = { layout = "adaptive", preview = true },
              prompt = { left = {}, right = {} },
            }"#,
        )
        .unwrap();
        let mut store = ConfigStore::default();
        store.reload(&runtime, &path).unwrap();
        assert_eq!(store.active().editor.keymap, "vim");

        fs::write(
            &path,
            "return quirl.config { editor = { keymap = 'broken' } }",
        )
        .unwrap();
        assert!(store.reload(&runtime, &path).is_err());
        assert_eq!(store.active().editor.keymap, "vim");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn process_capability_is_denied_in_config() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = runtime
            .eval("return quirl.process.run('printf nope')")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Lua);
        assert!(error.details.context[0].contains("capability denied"));
    }

    #[test]
    fn instruction_budget_stops_runaway_code() {
        let runtime = LuaRuntime::new(LuaPolicy {
            instruction_limit: HOOK_GRANULARITY,
            ..LuaPolicy::script()
        })
        .unwrap();
        let error = runtime.eval("while true do end").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn cancellation_stops_lua_at_the_instruction_hook() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        runtime.cancellation_token().cancel();
        let error = runtime.eval("while true do end").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn clearing_cancellation_allows_the_persistent_vm_to_recover() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        runtime.cancellation_token().cancel();
        assert!(runtime.eval("while true do end").is_err());

        runtime.clear_cancellation();
        assert_eq!(runtime.eval("return 42").unwrap(), 42);
    }

    #[test]
    fn restricted_modules_are_absent_at_runtime() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let value = runtime
            .eval(
                "return { io == nil, os == nil, debug == nil, package == nil, require == nil, dofile == nil, loadfile == nil }",
            )
            .unwrap();
        assert_eq!(
            value,
            serde_json::json!([true, true, true, true, true, true, true])
        );
    }

    #[test]
    fn plugin_registration_rejects_unknown_empty_unbounded_and_duplicate_inputs() {
        let unknown = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = unknown
            .eval(
                r#"quirl.prompt.add_segment {
                    name = "project", deadline_ms = 8, unexpected = true,
                    render = function() return "project" end,
                }"#,
            )
            .unwrap_err();
        assert!(error.details.context[0].contains("unknown field"));

        let empty = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = empty
            .eval(
                r#"quirl.completion.add_provider {
                    command = "  ", complete = function() return {} end,
                }"#,
            )
            .unwrap_err();
        assert!(error.details.context[0].contains("must not be empty"));

        let unbounded = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = unbounded
            .eval(
                r#"quirl.prompt.add_segment {
                    name = "slow", deadline_ms = 101,
                    render = function() return "slow" end,
                }"#,
            )
            .unwrap_err();
        assert!(error.details.context[0].contains("between 1 and 100"));

        let duplicate = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = duplicate
            .eval(
                r#"
                local segment = { name = "same", render = function() return "same" end }
                quirl.prompt.add_segment(segment)
                quirl.prompt.add_segment(segment)
                "#,
            )
            .unwrap_err();
        assert!(error.details.context[0].contains("duplicate prompt segment"));

        let duplicate = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = duplicate
            .eval(
                r#"
                local provider = { command = "same", complete = function() return {} end }
                quirl.completion.add_provider(provider)
                quirl.completion.add_provider(provider)
                "#,
            )
            .unwrap_err();
        assert!(error.details.context[0].contains("duplicate completion provider"));
    }

    #[test]
    fn prompt_callbacks_obey_their_declared_deadline() {
        let runtime = LuaRuntime::new(LuaPolicy {
            instruction_limit: 100_000_000,
            wall_time: Duration::from_secs(1),
            ..LuaPolicy::config()
        })
        .unwrap();
        runtime
            .eval(
                r#"quirl.prompt.add_segment {
                    name = "runaway", deadline_ms = 1,
                    render = function() while true do end end,
                }"#,
            )
            .unwrap();

        let started = Instant::now();
        let error = runtime
            .render_prompt_segment("runaway", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn completion_callbacks_have_a_safe_deadline_and_validate_return_shape() {
        let runaway = LuaRuntime::new(LuaPolicy {
            instruction_limit: 100_000_000,
            wall_time: Duration::from_secs(1),
            ..LuaPolicy::config()
        })
        .unwrap();
        runaway
            .eval(
                r#"quirl.completion.add_provider {
                    command = "runaway", complete = function() while true do end end,
                }"#,
            )
            .unwrap();
        let started = Instant::now();
        let error = runaway
            .complete_with_provider("runaway", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(started.elapsed() < Duration::from_millis(500));

        let malformed = LuaRuntime::new(LuaPolicy::config()).unwrap();
        malformed
            .eval(
                r#"quirl.completion.add_provider {
                    command = "malformed", complete = function() return { 42 } end,
                }"#,
            )
            .unwrap();
        let error = malformed
            .complete_with_provider("malformed", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("completion item 0"));
    }

    #[test]
    fn formatter_is_conservative_and_deterministic() {
        let source = "local function main()  \nreturn {\nok = true,  \n}\nend\n\n";
        let expected = "local function main()\n  return {\n    ok = true,\n  }\nend\n";
        assert_eq!(format_source(source), expected);
        assert_eq!(format_source(expected), expected);

        let long_string = "local docs = [[\n  literal indentation  \n---@not_an_annotation\n]]\n";
        assert_eq!(format_source(long_string), long_string);
        for example in [
            include_str!("../../../examples/lua_tests.lua"),
            include_str!("../../../examples/plugin.lua"),
        ] {
            assert_eq!(format_source(example), example);
        }
    }

    #[test]
    fn checker_accepts_the_documented_luals_annotation_surface_without_execution() {
        let source = r#"---@meta quirl
---@class quirl.Context
---@field args string[]
---@param ctx quirl.Context
---@return quirl.Result<quirl.Output, quirl.ShellError>
local function main(ctx)
  error("checking must not execute Lua")
end
---@type fun(ctx: quirl.Context): table
local exported = main
return { main = exported }
"#;
        LuaRuntime::check_source(source, "annotated.lua").unwrap();
    }

    #[test]
    fn checker_rejects_unknown_and_malformed_annotations_with_spans() {
        let error = LuaRuntime::check_source(
            "---@parm ctx table\n---@return quirl.Result<table\nreturn {}\n",
            "bad-annotations.lua",
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(error.details.labels.len(), 2);
        assert_eq!(
            error.details.labels[0].source.as_deref(),
            Some("bad-annotations.lua")
        );
        assert!(error.details.labels[0].message.contains("unsupported"));
        assert!(error.details.labels[1].message.contains("balanced"));
    }

    #[test]
    fn checker_validates_known_host_symbols_modules_and_practical_arity() {
        LuaRuntime::check_source(
            r#"local process = quirl.process
            local cwd = quirl.cwd()
            local result = quirl.process.run("printf ok")
            return quirl.config { prompt = { left = {}, right = {} } }"#,
            "known-host.lua",
        )
        .unwrap();

        let source = "local missing = quirl.process.missing()\nreturn quirl.cwd(1)\n";
        let error = LuaRuntime::check_source(source, "bad-host.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(error.details.labels.len(), 2);
        assert_eq!(
            &source[error.details.labels[0].start..error.details.labels[0].end],
            "quirl.process.missing"
        );
        assert!(error.details.labels[0].message.contains("unknown"));
        assert_eq!(
            &source[error.details.labels[1].start..error.details.labels[1].end],
            "quirl.cwd"
        );
        assert!(error.details.labels[1].message.contains("expects 0"));
    }

    #[test]
    fn script_tests_run_in_lexical_name_order() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let error = runtime
            .test_source(
                r#"return {
                  test_z = function() error("z failed") end,
                  test_a = function() error("a failed") end,
                }"#,
                "ordered-tests.lua",
            )
            .unwrap_err();
        assert!(error
            .details
            .context
            .iter()
            .any(|item| item == "test: test_a"));
        assert!(error
            .details
            .context
            .iter()
            .any(|item| item.contains("a failed")));
    }

    #[test]
    fn generated_sdk_and_runtime_share_api_names() {
        let stub = sdk_lua();
        for spec in HOST_API {
            assert!(stub.contains(spec.path));
        }
        assert_eq!(include_str!("../../../docs/quirl.lua"), stub);

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct SdkEnvelope {
            document_type: String,
            schema_version: u32,
            module: String,
            module_version: String,
            functions: Vec<serde_json::Value>,
        }
        let json = sdk_json().unwrap();
        let envelope: SdkEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope.document_type, "quirl.host_api");
        assert_eq!(envelope.schema_version, 1);
        assert_eq!(envelope.module, "quirl");
        assert_eq!(envelope.module_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(envelope.functions.len(), HOST_API.len());
        let mut unknown: serde_json::Value = serde_json::from_str(&json).unwrap();
        unknown["unexpected"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<SdkEnvelope>(unknown).is_err());

        let markdown = sdk_markdown();
        assert!(markdown.contains("`quirl.process.run(command: string) -> quirl.Result`"));
        assert!(markdown.contains("| `command` | `string` |"));
        assert!(markdown.contains("Returns: `quirl.Result`"));
    }

    #[test]
    fn linter_rejects_ambient_operating_system_access() {
        let error = lint_source("return os.execute('whoami')", Path::new("plugin.lua"))
            .expect_err("os access must require a capability");
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.labels[0]
            .message
            .contains("explicit Quirl capability"));
    }

    #[test]
    fn linter_does_not_treat_strings_or_comments_as_capability_use() {
        lint_source(
            "local example = 'os.execute(\\\"nope\\\")' -- io.open('also-nope')\nreturn example",
            Path::new("docs.lua"),
        )
        .unwrap();
        lint_source(
            "local docs = [[\n---@not_an_annotation\nos.execute('still text')\n]]\nreturn docs",
            Path::new("long-docs.lua"),
        )
        .unwrap();
    }

    #[test]
    fn registered_plugin_callbacks_are_invokable() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plugin.lua");
        runtime.load_plugin_file(&plugin).unwrap();

        let prompt = runtime
            .render_prompt_segment("project", &serde_json::json!({"project_name": "Quirl"}))
            .unwrap();
        assert_eq!(prompt.as_deref(), Some("Quirl"));

        let completions = runtime
            .complete_with_provider("deploy --environment", &serde_json::json!({}))
            .unwrap();
        assert_eq!(completions, serde_json::json!(["staging", "production"]));
    }
}
