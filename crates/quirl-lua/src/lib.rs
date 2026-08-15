//! Restricted Lua 5.4 runtime for Quirl configuration, scripts, and trusted plugins.

use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, RegistryKey, StdLib, Table, Value,
    VmState,
};
use quirl_core::{CommandRunner, ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
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
pub struct PromptRegistration {
    pub name: String,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    prompt_segments: HashMap<String, RegistryKey>,
    completion_providers: HashMap<String, RegistryKey>,
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
        lint_source(&source, path)?;
        self.reset_budget();
        let value = self
            .lua
            .load(&source)
            .set_name(path.to_string_lossy())
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

    pub fn registrations(&self) -> PluginRegistrations {
        self.registrations
            .lock()
            .expect("plugin registration mutex poisoned")
            .clone()
    }

    pub fn render_prompt_segment(
        &self,
        name: &str,
        context: &serde_json::Value,
    ) -> Result<Option<String>, ShellError> {
        let function = {
            let callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            let key = callbacks.prompt_segments.get(name).ok_or_else(|| {
                validation_error(name, format!("unknown prompt segment `{name}`"))
            })?;
            self.lua
                .registry_value::<Function>(key)
                .map_err(|error| lua_error(error, None, 0))?
        };
        let context = self
            .lua
            .to_value(context)
            .map_err(|error| lua_error(error, None, 0))?;
        self.reset_budget();
        function
            .call::<Option<String>>(context)
            .map_err(|error| lua_error(error, None, 0))
    }

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
        self.reset_budget();
        let value = function
            .call::<Value>(context)
            .map_err(|error| lua_error(error, None, 0))?;
        self.value_to_json(value, None, 0)
    }

    pub fn test_file(&self, path: &Path) -> Result<usize, ShellError> {
        let source = read_source(path)?;
        lint_source(&source, path)?;
        self.reset_budget();
        let tests = self
            .lua
            .load(&source)
            .set_name(path.to_string_lossy())
            .eval::<Table>()
            .map_err(|error| lua_error(error, Some(path), source.len()))?;
        let mut count = 0;
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
        lint_source(&source, path)?;
        let runtime = Self::new(LuaPolicy::config())?;
        runtime
            .lua
            .load(&source)
            .set_name(path.to_string_lossy())
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

    fn reset_budget(&self) {
        let mut budget = self.budget.lock().expect("Lua budget mutex poisoned");
        budget.remaining_instructions = self.policy.instruction_limit;
        budget.deadline = Instant::now() + self.policy.wall_time;
    }
}

pub fn format_source(source: &str) -> String {
    let mut formatted = source
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    formatted.push('\n');
    formatted
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
    serde_json::to_string_pretty(HOST_API).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not serialize the Lua SDK")
            .with_context(error.to_string())
    })
}

pub fn sdk_markdown() -> String {
    let mut output = String::from("# Quirl Lua SDK\n\n");
    for spec in HOST_API {
        output.push_str(&format!("## `{}`\n\n{}\n\n", spec.path, spec.summary));
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
            let name = spec.get::<String>("name")?;
            let deadline_ms = spec.get::<Option<u64>>("deadline_ms")?.unwrap_or(8);
            let render = spec.get::<Function>("render").map_err(|_| {
                mlua::Error::RuntimeError("prompt segment `render` must be a function".to_owned())
            })?;
            let callback = lua.create_registry_value(render)?;
            prompt_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?
                .prompt_segments
                .push(PromptRegistration {
                    name: name.clone(),
                    deadline_ms,
                });
            prompt_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?
                .prompt_segments
                .insert(name, callback);
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
            let command = spec.get::<String>("command")?;
            let complete = spec.get::<Function>("complete").map_err(|_| {
                mlua::Error::RuntimeError(
                    "completion provider `complete` must be a function".to_owned(),
                )
            })?;
            let callback = lua.create_registry_value(complete)?;
            completion_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?
                .completion_providers
                .push(CompletionRegistration {
                    command: command.clone(),
                });
            completion_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?
                .completion_providers
                .insert(command, callback);
            Ok(())
        })?,
    )?;
    quirl.set("completion", completion)?;
    lua.globals().set("quirl", quirl)
}

fn lint_source(source: &str, path: &Path) -> Result<(), ShellError> {
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
    for line in source.lines() {
        let code = line.split_once("--").map_or(line, |(code, _)| code);
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

fn read_source(path: &Path) -> Result<String, ShellError> {
    let source = fs::read_to_string(path).map_err(|error| script_read_error(path, error))?;
    Ok(if let Some(source) = source.strip_prefix("#!") {
        format!("--{source}")
    } else {
        source
    })
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
    fn formatter_is_conservative_and_deterministic() {
        assert_eq!(format_source("return 42  \n\n"), "return 42\n\n");
    }

    #[test]
    fn generated_sdk_and_runtime_share_api_names() {
        let stub = sdk_lua();
        for spec in HOST_API {
            assert!(stub.contains(spec.path));
        }
        assert_eq!(include_str!("../../../docs/quirl.lua"), stub);
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
