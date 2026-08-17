//! Restricted Lua 5.4 runtime for Quirl configuration, scripts, and trusted plugins.

use mlua::{
    Function, HookTriggers, Lua, LuaOptions, LuaSerdeExt, MultiValue, RegistryKey, StdLib, Table,
    Value, VmState,
};
use quirl_core::{
    AtomicReplaceOptions, ContributionKind, ContributionRegistration, EXECUTION_ARGUMENT_BYTES_MAX,
    EXECUTION_ARGUMENTS_MAX, EXECUTION_BYTES_MAX, ErrorCode, ErrorLabel, EventKind,
    EventSubscription, ExecutionCancellation, ExecutionCleanupState, ExecutionEffect,
    ExecutionEffects, ExecutionInput, ExecutionOutcome, ExecutionOutput, ExecutionOutputTarget,
    ExecutionStatus, ExtensionAction, ExtensionCapability, ExtensionEvent, ExtensionEventData,
    ProcessHost, ProcessRequest, ShellError, StructuredValue, ValueInputContract,
    ValueOutputContract, escape_terminal_controls, reject_json_terminal_controls,
    reject_terminal_controls, replace_file_atomically, validate_contribution_set,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Read,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const HOOK_GRANULARITY: u64 = 10_000;
const DEFAULT_PROMPT_DEADLINE_MS: u64 = 8;
const MAX_CALLBACK_DEADLINE_MS: u64 = 100;
const COMPLETION_CALLBACK_DEADLINE: Duration = Duration::from_millis(50);
const MAX_PROCESS_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_LUA_COMPLETION_RESULTS: usize = 1_000;
const MAX_LUA_COMPLETION_ITEM_BYTES: usize = 16 * 1024;
const MAX_LUA_COMPLETION_RETAINED_BYTES: usize = 256 * 1024;
const MAX_LUA_RETURN_RETAINED_BYTES: usize = 256 * 1024;
// A maximum-size ABI-v1 batch of 512 Size or Duration values expands to 4,109
// raw Lua key/value nodes after its typed and result envelopes are included.
// Keep a small fixed margin so every accepted logical payload reaches decoding.
const MAX_LUA_RETURN_NODES: usize = 4_112;
const MAX_LUA_RETURN_INDEX: i64 = 4_096;
const MAX_LUA_RETURN_DEPTH: usize = 16;
const MAX_PROMPT_RETURN_BYTES: usize = 16 * 1024;
const MAX_EVENT_ACTIONS: usize = 64;
const MAX_COMMAND_EXAMPLES: usize = 32;
const MAX_COMMAND_EFFECTS: usize = 32;
const MAX_COMMAND_ERROR_CODES: usize = 64;
const MAX_COMMAND_EXAMPLE_BYTES: usize = 4 * 1024;
const MAX_COMMAND_EFFECT_BYTES: usize = 256;
const MAX_COMMAND_ERROR_CODE_BYTES: usize = 256;
const MAX_COMMAND_ERROR_DESCRIPTION_BYTES: usize = 2 * 1024;
const MAX_PANEL_FALLBACK_BYTES: usize = 16 * 1024;
const MAX_REGISTRATION_INPUT_NODES: usize = 512;
const MAX_REGISTRATION_INPUT_DEPTH: usize = 4;
const MAX_REGISTRATION_INPUT_BYTES: usize = 64 * 1024;
/// Current version of the typed Lua runner ABI supplied to `main`.
pub const LUA_RUNNER_ABI_VERSION: u32 = 1;
/// Oldest runner ABI accepted by the deterministic compatibility path.
///
/// Version zero is the historical unversioned `{ args = ... }` contract. It is
/// accepted only as a bounded migration input and always produces the current
/// shared execution outcome on the Rust side.
pub const LUA_RUNNER_OLDEST_READABLE_ABI_VERSION: u32 = 0;
/// Canonical structural descriptor for the Lua runner ABI and migration policy.
pub const LUA_RUNNER_ABI_DESCRIPTOR: &str = "quirl.lua-runner@1{module{deny_unknown;abi_version:1;main:function;field_name_bytes=128};context{abi_version:1;args:array<string>(max=1024,bytes=1048576);env:map<string,string>(max=256,bytes=65536,utf8);cwd:string(bytes=4096,utf8);input:ExecutionInput(shared-bounds);output:ExecutionOutputTarget(value-only,shared-bounds);cancellation{is_cancelled:function,shared-atomic};effects:array<ExecutionEffect>(fixed)};result{deny_unknown;abi_version:1;ok:bool;status:i32?;output:ExecutionOutput?;error:ShellError?};error{deny_unknown;items_per_collection=32;field_bytes=16384;label_source_bytes=65536;total_bytes=262144;terminal_controls=rejected;utf8_spans=validated};streams:finite-ExecutionOutput.values(max=512,shared-value-bounds);live-stream-handles:rejected;migration:unversioned-or-v0-to-v1;future:fail-closed}";
/// Maximum environment entries exposed to one Lua `main` invocation.
pub const MAX_LUA_RUNNER_ENVIRONMENT_ENTRIES: usize = 256;
/// Maximum aggregate UTF-8 bytes retained across Lua runner environment keys and values.
pub const MAX_LUA_RUNNER_ENVIRONMENT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes retained for the Lua runner working directory.
pub const MAX_LUA_RUNNER_CWD_BYTES: usize = 4 * 1024;
/// Maximum values accepted in one finite Lua runner value batch.
pub const MAX_LUA_RUNNER_STREAM_VALUES: usize = 512;
const MAX_LUA_RUNNER_ERROR_ITEMS: usize = 32;
const MAX_LUA_RUNNER_ERROR_FIELD_BYTES: usize = 16 * 1024;
const MAX_LUA_RUNNER_ERROR_SOURCE_BYTES: usize = 64 * 1024;
const MAX_LUA_RUNNER_ERROR_TOTAL_BYTES: usize = 256 * 1024;
/// Maximum number of prompt callback declarations retained from one Lua plugin.
pub const MAX_PLUGIN_PROMPT_SEGMENTS: usize = 64;
/// Maximum retained UTF-8 metadata bytes across one plugin's prompt declarations.
pub const MAX_PLUGIN_PROMPT_BYTES: usize = 8 * 1024;
/// Maximum number of completion provider declarations retained from one Lua plugin.
pub const MAX_PLUGIN_COMPLETION_PROVIDERS: usize = 64;
/// Maximum retained UTF-8 metadata bytes across one plugin's completion declarations.
pub const MAX_PLUGIN_COMPLETION_BYTES: usize = 16 * 1024;
/// Maximum number of typed command declarations retained from one Lua plugin.
pub const MAX_PLUGIN_COMMANDS: usize = 64;
/// Maximum retained UTF-8 metadata bytes across one plugin's command declarations.
pub const MAX_PLUGIN_COMMAND_BYTES: usize = 256 * 1024;
/// Maximum number of event handler declarations retained from one Lua plugin.
pub const MAX_PLUGIN_EVENT_HANDLERS: usize = 64;
/// Maximum retained UTF-8 metadata bytes across one plugin's event declarations.
pub const MAX_PLUGIN_EVENT_BYTES: usize = 64 * 1024;
/// Maximum number of contribution declarations retained from one Lua plugin.
pub const MAX_PLUGIN_CONTRIBUTIONS: usize = 64;
/// Maximum retained UTF-8 metadata bytes across one plugin's contribution declarations.
pub const MAX_PLUGIN_CONTRIBUTION_BYTES: usize = 64 * 1024;
/// Maximum number of panel contributions retained from one Lua plugin.
pub const MAX_PLUGIN_PANELS: usize = 16;
/// Maximum retained UTF-8 metadata bytes across one plugin's panel declarations.
pub const MAX_PLUGIN_PANEL_BYTES: usize = 32 * 1024;
/// Maximum UTF-8 byte length of any registration or callback identifier.
pub const MAX_REGISTRATION_NAME_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a short registration description or signature.
pub const MAX_REGISTRATION_DESCRIPTION_BYTES: usize = 2 * 1024;
/// Maximum UTF-8 byte length of detailed command documentation.
pub const MAX_COMMAND_DETAILS_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 byte length of one semantic pipeline type expression.
pub const MAX_COMMAND_TYPE_BYTES: usize = 256;
/// Maximum number of custom palettes retained from one configuration.
pub const MAX_CUSTOM_THEMES: usize = 32;
/// Maximum UTF-8 byte length accepted for a selected or custom theme name.
pub const MAX_THEME_NAME_BYTES: usize = 64;
/// Number of popular named palettes shipped in addition to the `ansi` fallback.
pub const POPULAR_THEME_COUNT: usize = 30;
/// Built-in palette selected by a default or migrated configuration.
pub const DEFAULT_THEME_NAME: &str = "tokyo-night";
/// Configuration schema version emitted after validation and migration.
pub const CONFIG_SCHEMA_VERSION: u32 = 3;
/// Oldest configuration version accepted by the deterministic migration path.
///
/// Version zero represents an unversioned legacy configuration.
pub const CONFIG_OLDEST_READABLE_VERSION: u32 = 0;
/// Maximum UTF-8 source size, in bytes, accepted by runtime, check, and file-format paths.
pub const MAX_LUA_SOURCE_BYTES: usize = 4 * 1024 * 1024;
/// Canonical structural descriptor for the deny-unknown configuration contract.
///
/// The descriptor includes defaults, value domains, and migration policy so its
/// fingerprint changes whenever a reader-visible configuration rule changes.
pub const CONFIG_SCHEMA_DESCRIPTOR: &str = "quirl.config@3{QuirlConfig{deny_unknown;schema_version:u32(default=3,legacy=0|1|2-migrates-to-3);editor:EditorConfig(default);picker:PickerConfig(default);prompt:PromptConfig(default);ui:UiConfig(default);completion:CompletionConfig(default)};EditorConfig{deny_unknown;keymap:emacs|vim|helix(default=emacs);semantic_hints:bool(default=true);banner:full|compact|none(default=full)};PickerConfig{deny_unknown;layout:adaptive|bottom|full(default=adaptive);preview:bool(default=true)};PromptConfig{deny_unknown;symbols:auto|plain|unicode|nerd_font(default=auto);left:array<string>(default=directory,git_branch,git_state);right:array<string>(default=jobs,duration,status);transient:bool(default=true)};UiConfig{deny_unknown;surface:auto|rich|simple(default=auto);theme:string(default=tokyo-night);themes:map<string,ThemeColors>(max=32,default={});statusline:StatuslineConfig(default)};ThemeColors{deny_unknown;accent_command:#RRGGBB;accent_data:#RRGGBB;context_primary:#RRGGBB;context_secondary:#RRGGBB;muted:#RRGGBB;border:#RRGGBB;status_background:#RRGGBB;error:#RRGGBB;warning:#RRGGBB;hint:#RRGGBB;string:#RRGGBB;operator:#RRGGBB;expansion:#RRGGBB;number:#RRGGBB};StatuslineConfig{deny_unknown;hints:bool(default=true)};CompletionConfig{deny_unknown;auto:bool(default=false);min_chars:u16(0..=4096,default=2)};builtins:ansi|ayu-dark|catppuccin-mocha|cobalt-2|dracula|everforest-dark-medium|github-dark|gotham|gruvbox-dark|horizon-dark|kanagawa-wave|material|monokai-dark|moonfly|night-owl|nord|oceanic-next|one-dark|one-half-black|oxocarbon-dark|palenight|papercolor-dark|rose-pine-moon|snazzy|solarized-dark|sonokai|srcery|synthwave|tokyo-night|tomorrow-night|zenburn;migration:unversioned-or-v1-or-v2-to-v3}";

/// Return the deterministic fingerprint of [`CONFIG_SCHEMA_DESCRIPTOR`].
pub fn config_schema_hash() -> String {
    quirl_core::schema_fingerprint(CONFIG_SCHEMA_DESCRIPTOR)
}

/// Return the deterministic fingerprint of [`LUA_RUNNER_ABI_DESCRIPTOR`].
pub fn lua_runner_abi_hash() -> String {
    quirl_core::schema_fingerprint(LUA_RUNNER_ABI_DESCRIPTOR)
}

/// Validated, bounded context supplied to a versioned Lua module's `main` function.
///
/// The environment is an immutable UTF-8 snapshot. Input and output retain the
/// shared execution contract's byte/value distinction, and cancellation shares
/// the exact atomic identity observed by the VM hook and injected process host.
#[derive(Debug, Clone)]
pub struct LuaRunnerContext {
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    working_directory: String,
    input: ExecutionInput,
    output: ExecutionOutputTarget,
    declared_effects: ExecutionEffects,
    cancelled: Arc<AtomicBool>,
}

impl LuaRunnerContext {
    /// Validate and retain an explicit runner context.
    ///
    /// This constructor is intended for composition adapters and deterministic
    /// tests. Ordinary CLI callers should use [`Self::from_current_process`].
    pub fn new(
        arguments: Vec<String>,
        environment: BTreeMap<String, String>,
        working_directory: String,
        input: ExecutionInput,
        output: ExecutionOutputTarget,
        declared_effects: ExecutionEffects,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        let context = Self {
            arguments,
            environment,
            working_directory,
            input,
            output,
            declared_effects,
            cancelled,
        };
        context.validate()?;
        Ok(context)
    }

    /// Snapshot the current UTF-8 process environment and working directory under ABI bounds.
    pub fn from_current_process(
        arguments: &[String],
        input: ExecutionInput,
        output: ExecutionOutputTarget,
        declared_effects: ExecutionEffects,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        let working_directory = std::env::current_dir()
            .map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not read the Lua runner working directory",
                )
                .with_context(error.to_string())
                .with_help("Run the script from an accessible working directory")
            })?
            .into_os_string()
            .into_string()
            .map_err(|_| {
                ShellError::new(
                    ErrorCode::Validation,
                    "the Lua runner working directory is not valid UTF-8",
                )
                .with_help("Run the script from a UTF-8 working directory")
            })?;
        let mut environment = BTreeMap::new();
        let mut environment_bytes = 0_usize;
        for (key, value) in std::env::vars_os() {
            let key = key.into_string().map_err(|_| {
                ShellError::new(
                    ErrorCode::Validation,
                    "a Lua runner environment name is not valid UTF-8",
                )
                .with_help("Remove or re-encode non-UTF-8 environment entries before running Lua")
            })?;
            let value = value.into_string().map_err(|_| {
                ShellError::new(
                    ErrorCode::Validation,
                    "a Lua runner environment value is not valid UTF-8",
                )
                .with_context(format!("environment name: {key}"))
                .with_help("Remove or re-encode non-UTF-8 environment entries before running Lua")
            })?;
            environment_bytes = environment_bytes
                .saturating_add(key.len())
                .saturating_add(value.len());
            let observed_entries = environment.len().saturating_add(1);
            if observed_entries > MAX_LUA_RUNNER_ENVIRONMENT_ENTRIES {
                return Err(runner_limit_error(
                    "Lua runner environment entries",
                    MAX_LUA_RUNNER_ENVIRONMENT_ENTRIES,
                    observed_entries,
                ));
            }
            if environment_bytes > MAX_LUA_RUNNER_ENVIRONMENT_BYTES {
                return Err(runner_limit_error(
                    "Lua runner environment bytes",
                    MAX_LUA_RUNNER_ENVIRONMENT_BYTES,
                    environment_bytes,
                ));
            }
            environment.insert(key, value);
        }
        Self::new(
            arguments.to_vec(),
            environment,
            working_directory,
            input,
            output,
            declared_effects,
            cancelled,
        )
    }

    /// Exact bounded arguments in source order.
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    /// Immutable bounded environment snapshot.
    pub fn environment(&self) -> &BTreeMap<String, String> {
        &self.environment
    }

    /// UTF-8 working directory captured before module evaluation.
    pub fn working_directory(&self) -> &str {
        &self.working_directory
    }

    /// Shared byte or structured-value input representation.
    pub fn input(&self) -> &ExecutionInput {
        &self.input
    }

    /// Output representation requested by the composition adapter.
    pub const fn output(&self) -> ExecutionOutputTarget {
        self.output
    }

    /// Effects declared and validated before the Lua engine was selected.
    pub const fn declared_effects(&self) -> ExecutionEffects {
        self.declared_effects
    }

    fn validate(&self) -> Result<(), ShellError> {
        validate_runner_arguments(&self.arguments)?;
        validate_runner_environment(&self.environment)?;
        validate_runner_text(
            "Lua runner working directory",
            &self.working_directory,
            MAX_LUA_RUNNER_CWD_BYTES,
        )?;
        if self.working_directory.is_empty() {
            return Err(runner_validation_error(
                "Lua runner working directory must not be empty",
            ));
        }
        validate_runner_input(&self.input)?;
        validate_runner_output_target(self.output)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LuaRunnerResultWire {
    abi_version: u32,
    ok: bool,
    #[serde(default)]
    status: Option<i32>,
    #[serde(default)]
    output: Option<ExecutionOutput>,
    #[serde(default)]
    error: Option<LuaShellErrorWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LuaShellErrorWire {
    code: ErrorCode,
    message: String,
    #[serde(default)]
    labels: Vec<LuaErrorLabelWire>,
    #[serde(default)]
    context: Vec<String>,
    #[serde(default)]
    help: Vec<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    exit_status: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LuaErrorLabelWire {
    #[serde(default)]
    source: Option<String>,
    start: usize,
    end: usize,
    message: String,
}

#[derive(Debug, Clone, Copy)]
/// Resource and capability policy applied to every [`LuaRuntime`] execution.
///
/// The runtime enforces memory, approximate instruction, wall-time, and
/// cancellation checks together. Process access additionally requires a granted
/// capability and an explicitly composed bounded process host.
pub struct LuaPolicy {
    /// Whether the default capability set may include `process.spawn`.
    pub allow_process: bool,
    /// Maximum bytes the embedded Lua allocator may retain for this VM.
    pub memory_limit_bytes: usize,
    /// Approximate Lua instruction budget reset before each top-level invocation.
    pub instruction_limit: u64,
    /// Maximum wall-clock duration of one top-level invocation.
    pub wall_time: Duration,
}

impl LuaPolicy {
    /// Policy for user scripts: process-capable with an 8 MiB memory limit,
    /// two-million-instruction budget, and 250 ms wall deadline.
    pub const fn script() -> Self {
        Self {
            allow_process: true,
            memory_limit_bytes: 8 * 1024 * 1024,
            instruction_limit: 2_000_000,
            wall_time: Duration::from_millis(250),
        }
    }

    /// Stricter policy for configuration and static checks.
    ///
    /// Process access is disabled; memory is limited to 4 MiB, execution to
    /// 500,000 instructions, and wall time to 100 ms.
    pub const fn config() -> Self {
        Self {
            allow_process: false,
            memory_limit_bytes: 4 * 1024 * 1024,
            instruction_limit: 500_000,
            wall_time: Duration::from_millis(100),
        }
    }

    fn validate(self) -> Result<(), ShellError> {
        for (name, valid) in [
            ("memory_limit_bytes", self.memory_limit_bytes > 0),
            ("instruction_limit", self.instruction_limit > 0),
            ("wall_time", !self.wall_time.is_zero()),
        ] {
            if !valid {
                return Err(validation_error(
                    "Lua policy",
                    format!("{name} must be greater than zero"),
                ));
            }
        }
        Ok(())
    }
}

impl Default for LuaPolicy {
    fn default() -> Self {
        Self::script()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Validated top-level Quirl configuration returned across the Lua boundary.
///
/// Unknown fields are rejected. Versions zero and one are migrated to the current
/// schema only after Lua evaluation and typed deserialization succeed.
pub struct QuirlConfig {
    /// Serialized configuration version; validated against [`CONFIG_SCHEMA_VERSION`].
    #[serde(default = "default_config_schema_version")]
    pub schema_version: u32,
    /// Interactive editor behavior.
    #[serde(default)]
    pub editor: EditorConfig,
    /// Fuzzy picker presentation behavior.
    #[serde(default)]
    pub picker: PickerConfig,
    /// Prompt segments, symbols, and scrollback behavior.
    #[serde(default)]
    pub prompt: PromptConfig,
    /// Terminal surface and status-line behavior.
    #[serde(default)]
    pub ui: UiConfig,
    /// Automatic semantic-completion behavior.
    #[serde(default)]
    pub completion: CompletionConfig,
}

impl Default for QuirlConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            editor: EditorConfig::default(),
            picker: PickerConfig::default(),
            prompt: PromptConfig::default(),
            ui: UiConfig::default(),
            completion: CompletionConfig::default(),
        }
    }
}

const fn default_config_schema_version() -> u32 {
    CONFIG_SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Configuration for the interactive input editor.
pub struct EditorConfig {
    /// Editing model: `emacs`, `vim`, or experimental `helix`.
    pub keymap: String,
    /// Whether the editor displays catalog-backed semantic hints while typing.
    pub semantic_hints: bool,
    /// Welcome presentation: `full`, `compact`, or `none`.
    pub banner: String,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            keymap: "emacs".to_owned(),
            semantic_hints: true,
            banner: "full".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Presentation configuration for Quirl's shared fuzzy picker.
pub struct PickerConfig {
    /// Picker placement: `adaptive`, `bottom`, or `full`.
    pub layout: String,
    /// Whether a provider-specific preview pane may be shown.
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
/// Ordered prompt composition and scrollback behavior.
pub struct PromptConfig {
    /// Visual symbol profile. `auto` uses broadly supported Unicode and never
    /// assumes a patched font; `nerd_font` is an explicit opt-in.
    pub symbols: String,
    /// Segment names rendered before the command buffer, in display order.
    pub left: Vec<String>,
    /// Segment names rendered on the right side, in display order.
    pub right: Vec<String>,
    /// Whether accepted input collapses to a compact scrollback line before execution.
    pub transient: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            symbols: "auto".to_owned(),
            left: vec![
                "directory".to_owned(),
                "git_branch".to_owned(),
                "git_state".to_owned(),
            ],
            right: vec![
                "jobs".to_owned(),
                "duration".to_owned(),
                "status".to_owned(),
            ],
            transient: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Selection of terminal UI surface and its persistent status line.
pub struct UiConfig {
    /// Surface policy: `auto`, `rich`, or `simple`.
    pub surface: String,
    /// Built-in or custom semantic palette selected for interactive color surfaces.
    pub theme: String,
    /// Bounded custom palettes keyed by safe ASCII names.
    pub themes: BTreeMap<String, ThemeColors>,
    /// Status-line visibility settings.
    pub statusline: StatuslineConfig,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            surface: "auto".to_owned(),
            theme: DEFAULT_THEME_NAME.to_owned(),
            themes: BTreeMap::new(),
            statusline: StatuslineConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Fixed semantic color roles for one validated terminal palette.
///
/// Every value must be an exact `#RRGGBB` string. Configuration validation
/// bounds custom palette counts and names before this data reaches rendering.
pub struct ThemeColors {
    /// Command-mode accent color.
    pub accent_command: String,
    /// Typed-data-mode accent color.
    pub accent_data: String,
    /// Primary directory and syntax context color.
    pub context_primary: String,
    /// Secondary repository and status context color.
    pub context_secondary: String,
    /// Subdued hint and inactive text color.
    pub muted: String,
    /// Panel and chrome border color.
    pub border: String,
    /// Background color for the persistent status row.
    pub status_background: String,
    /// Error diagnostic color.
    pub error: String,
    /// Warning diagnostic color.
    pub warning: String,
    /// Advisory hint color.
    pub hint: String,
    /// Quoted-string syntax color.
    pub string: String,
    /// Operator and redirect syntax color.
    pub operator: String,
    /// Parameter and command-substitution color.
    pub expansion: String,
    /// Numeric literal color.
    pub number: String,
}

impl ThemeColors {
    fn validate(&self, source: &str, theme_name: &str) -> Result<(), ShellError> {
        for (role, color) in [
            ("accent_command", self.accent_command.as_str()),
            ("accent_data", self.accent_data.as_str()),
            ("context_primary", self.context_primary.as_str()),
            ("context_secondary", self.context_secondary.as_str()),
            ("muted", self.muted.as_str()),
            ("border", self.border.as_str()),
            ("status_background", self.status_background.as_str()),
            ("error", self.error.as_str()),
            ("warning", self.warning.as_str()),
            ("hint", self.hint.as_str()),
            ("string", self.string.as_str()),
            ("operator", self.operator.as_str()),
            ("expansion", self.expansion.as_str()),
            ("number", self.number.as_str()),
        ] {
            if color.len() > 7 {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    format!("ui.themes.{theme_name}.{role} exceeds its byte limit"),
                )
                .with_context(format!("bytes: {}; limit: 7", color.len()))
                .with_label(Some(source.to_owned()), 0, 0, "theme color is too long")
                .with_help("Use one exact #RRGGBB color"));
            }
            if !valid_theme_color(color) {
                return Err(validation_error(
                    source,
                    format!("ui.themes.{theme_name}.{role} must be an exact #RRGGBB color"),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Persistent status-line presentation settings.
pub struct StatuslineConfig {
    /// Whether shortcut and mode hints are included in the status line.
    pub hints: bool,
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self { hints: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Policy controlling automatic semantic completion in the editor.
pub struct CompletionConfig {
    /// Whether the completion menu may open without an explicit completion action.
    pub auto: bool,
    /// Minimum typed character count before automatic completion, from 0 through 4096.
    pub min_chars: u16,
}

impl Default for CompletionConfig {
    fn default() -> Self {
        Self {
            auto: false,
            min_chars: 2,
        }
    }
}

impl QuirlConfig {
    /// Revalidate configuration received from persistence or a worker protocol.
    pub fn validate(&self, source: &str) -> Result<(), ShellError> {
        if self.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(validation_error(
                source,
                format!(
                    "config schema_version {} is unsupported; expected {CONFIG_SCHEMA_VERSION}",
                    self.schema_version
                ),
            ));
        }
        if !matches!(self.editor.keymap.as_str(), "helix" | "emacs" | "vim") {
            return Err(validation_error(
                source,
                "editor.keymap must be `helix`, `emacs`, or `vim`",
            ));
        }
        if !matches!(self.editor.banner.as_str(), "full" | "compact" | "none") {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "editor.banner must be `full`, `compact`, or `none`",
            )
            .with_help("Use `full` for the welcome and shortcuts, `compact` for one line, or `none` to hide it"));
        }
        if !matches!(self.picker.layout.as_str(), "adaptive" | "bottom" | "full") {
            return Err(validation_error(
                source,
                "picker.layout must be `adaptive`, `bottom`, or `full`",
            ));
        }
        if !matches!(
            self.prompt.symbols.as_str(),
            "auto" | "plain" | "unicode" | "nerd_font"
        ) {
            return Err(validation_error(
                source,
                "prompt.symbols must be `auto`, `plain`, `unicode`, or `nerd_font`",
            ));
        }
        if !matches!(self.ui.surface.as_str(), "auto" | "rich" | "simple") {
            return Err(validation_error(
                source,
                "ui.surface must be `auto`, `rich`, or `simple`",
            ));
        }
        self.validate_theme_configuration(source)?;
        if self.completion.min_chars > 4096 {
            return Err(validation_error(
                source,
                "completion.min_chars must be at most 4096",
            ));
        }
        Ok(())
    }

    fn validate_theme_configuration(&self, source: &str) -> Result<(), ShellError> {
        validate_theme_name(source, "ui.theme", &self.ui.theme)?;
        if self.ui.themes.len() > MAX_CUSTOM_THEMES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "custom theme count exceeds its configured limit",
            )
            .with_context(format!(
                "themes: {}; limit: {MAX_CUSTOM_THEMES}",
                self.ui.themes.len()
            ))
            .with_label(Some(source.to_owned()), 0, 0, "too many custom themes")
            .with_help("Keep only the themes used by this configuration"));
        }
        for (name, colors) in &self.ui.themes {
            validate_theme_name(source, "custom theme name", name)?;
            if builtin_theme(name).is_some() {
                return Err(validation_error(
                    source,
                    format!("custom theme `{name}` must not shadow a built-in theme"),
                ));
            }
            colors.validate(source, name)?;
        }
        if builtin_theme(&self.ui.theme).is_none() && !self.ui.themes.contains_key(&self.ui.theme) {
            return Err(validation_error(
                source,
                format!(
                    "ui.theme `{}` is not built in or defined in ui.themes",
                    self.ui.theme
                ),
            ));
        }
        Ok(())
    }

    /// Resolve the selected built-in or validated custom theme.
    pub fn active_theme(&self) -> Result<ThemeColors, ShellError> {
        self.validate_theme_configuration("active configuration")?;
        if let Some(theme) = builtin_theme(&self.ui.theme) {
            return Ok(theme);
        }
        self.ui.themes.get(&self.ui.theme).cloned().ok_or_else(|| {
            validation_error(
                "active configuration",
                format!("ui.theme `{}` is unavailable", self.ui.theme),
            )
        })
    }
}

fn validate_theme_name(source: &str, description: &str, name: &str) -> Result<(), ShellError> {
    if name.len() > MAX_THEME_NAME_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("{description} exceeds its byte limit"),
        )
        .with_context(format!(
            "bytes: {}; limit: {MAX_THEME_NAME_BYTES}",
            name.len()
        ))
        .with_label(Some(source.to_owned()), 0, 0, "theme name is too long")
        .with_help("Use a shorter stable ASCII theme name"));
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(validation_error(
            source,
            format!("{description} `{name}` must use lowercase ASCII letters, digits, or dash"),
        ));
    }
    Ok(())
}

fn valid_theme_color(color: &str) -> bool {
    color.len() == 7
        && color.starts_with('#')
        && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

#[derive(Debug, Clone, Copy)]
struct BuiltinTheme {
    name: &'static str,
    colors: [&'static str; 14],
}

impl BuiltinTheme {
    fn to_owned_colors(self) -> ThemeColors {
        let [
            accent_command,
            accent_data,
            context_primary,
            context_secondary,
            muted,
            border,
            status_background,
            error,
            warning,
            hint,
            string,
            operator,
            expansion,
            number,
        ] = self.colors;
        ThemeColors {
            accent_command: accent_command.to_owned(),
            accent_data: accent_data.to_owned(),
            context_primary: context_primary.to_owned(),
            context_secondary: context_secondary.to_owned(),
            muted: muted.to_owned(),
            border: border.to_owned(),
            status_background: status_background.to_owned(),
            error: error.to_owned(),
            warning: warning.to_owned(),
            hint: hint.to_owned(),
            string: string.to_owned(),
            operator: operator.to_owned(),
            expansion: expansion.to_owned(),
            number: number.to_owned(),
        }
    }
}

macro_rules! builtin_theme {
    ($name:literal, $($color:literal),+ $(,)?) => {
        BuiltinTheme { name: $name, colors: [$($color),+] }
    };
}

// The popular palettes are sourced from Gogh's maintained terminal-theme
// catalog. Its ANSI colors map once into Quirl's fixed semantic roles, so
// render-time code remains theme-agnostic and bounded.
const BUILTIN_THEMES: &[BuiltinTheme] = &[
    builtin_theme!(
        "ansi", "#00aa00", "#aa00aa", "#00aaaa", "#aa00aa", "#555555", "#555555", "#000000",
        "#aa0000", "#aa5500", "#0000aa", "#aa5500", "#aaaaaa", "#0000aa", "#aa00aa"
    ),
    builtin_theme!(
        "ayu-dark", "#c2d94c", "#ffee99", "#95e6cb", "#ffee99", "#4d5566", "#4d5566", "#0a0e14",
        "#ff3333", "#ff8f40", "#59c2ff", "#c2d94c", "#95e6cb", "#59c2ff", "#ffee99"
    ),
    builtin_theme!(
        "catppuccin-mocha",
        "#a6e3a1",
        "#f5c2e7",
        "#94e2d5",
        "#f5c2e7",
        "#585b70",
        "#585b70",
        "#1e1e2e",
        "#f38ba8",
        "#f9e2af",
        "#89b4fa",
        "#a6e3a1",
        "#94e2d5",
        "#89b4fa",
        "#f5c2e7"
    ),
    builtin_theme!(
        "cobalt-2", "#3bd01d", "#ff55ff", "#6ae3fa", "#ff55ff", "#555555", "#555555", "#132738",
        "#f40e17", "#edc809", "#5555ff", "#38de21", "#6ae3fa", "#5555ff", "#ff005d"
    ),
    builtin_theme!(
        "dracula", "#50fa7b", "#ff79c6", "#8be9fd", "#ff79c6", "#7a7a7a", "#7a7a7a", "#282a36",
        "#ff5555", "#f1fa8c", "#bd93f9", "#42e66c", "#8be9fd", "#bd93f9", "#e356a7"
    ),
    builtin_theme!(
        "everforest-dark-medium",
        "#8da101",
        "#df69ba",
        "#35a77c",
        "#df69ba",
        "#5c6a72",
        "#5c6a72",
        "#2d353b",
        "#f85552",
        "#dfa000",
        "#3a94c5",
        "#a7c080",
        "#35a77c",
        "#3a94c5",
        "#d699b6"
    ),
    builtin_theme!(
        "github-dark",
        "#56d364",
        "#db61a2",
        "#2b7489",
        "#db61a2",
        "#4d4d4d",
        "#4d4d4d",
        "#101216",
        "#f78166",
        "#e3b341",
        "#6ca4f8",
        "#56d364",
        "#2b7489",
        "#6ca4f8",
        "#db61a2"
    ),
    builtin_theme!(
        "gotham", "#081f2d", "#888ba5", "#599caa", "#888ba5", "#10151b", "#10151b", "#0a0f14",
        "#d26939", "#245361", "#093748", "#26a98b", "#599caa", "#093748", "#4e5165"
    ),
    builtin_theme!(
        "gruvbox-dark",
        "#b8bb26",
        "#d3869b",
        "#8ec07c",
        "#d3869b",
        "#928374",
        "#928374",
        "#282828",
        "#fb4934",
        "#fabd2f",
        "#83a598",
        "#98971a",
        "#8ec07c",
        "#83a598",
        "#b16286"
    ),
    builtin_theme!(
        "horizon-dark",
        "#3fdaa4",
        "#f075b7",
        "#6be6e6",
        "#f075b7",
        "#232530",
        "#232530",
        "#1c1e26",
        "#ec6a88",
        "#fbc3a7",
        "#3fc6de",
        "#29d398",
        "#6be6e6",
        "#3fc6de",
        "#ee64ae"
    ),
    builtin_theme!(
        "kanagawa-wave",
        "#98bb6c",
        "#938aa9",
        "#7aa89f",
        "#938aa9",
        "#727169",
        "#727169",
        "#1f1f28",
        "#e82424",
        "#e6c384",
        "#7fb4ca",
        "#76946a",
        "#7aa89f",
        "#7fb4ca",
        "#957fb8"
    ),
    builtin_theme!(
        "material", "#c3e88d", "#6c71c3", "#34434d", "#6c71c3", "#002b36", "#002b36", "#1e282c",
        "#eb606b", "#f7eb95", "#7dc6bf", "#c3e88d", "#34434d", "#7dc6bf", "#ff2490"
    ),
    builtin_theme!(
        "monokai-dark",
        "#a6e22e",
        "#ae81ff",
        "#2aa198",
        "#ae81ff",
        "#272822",
        "#272822",
        "#272822",
        "#f92672",
        "#f4bf75",
        "#66d9ef",
        "#a6e22e",
        "#2aa198",
        "#66d9ef",
        "#ae81ff"
    ),
    builtin_theme!(
        "moonfly", "#36c692", "#ae81ff", "#85dc85", "#ae81ff", "#949494", "#949494", "#080808",
        "#ff5189", "#c2c292", "#74b2ff", "#8cc85f", "#85dc85", "#74b2ff", "#cf87e8"
    ),
    builtin_theme!(
        "night-owl",
        "#22da6e",
        "#c792ea",
        "#7fdbca",
        "#c792ea",
        "#575656",
        "#575656",
        "#011627",
        "#ef5350",
        "#ffeb95",
        "#82aaff",
        "#22da6e",
        "#7fdbca",
        "#82aaff",
        "#c792ea"
    ),
    builtin_theme!(
        "nord", "#a3be8c", "#b48ead", "#8fbcbb", "#b48ead", "#4c566a", "#4c566a", "#2e3440",
        "#bf616a", "#ebcb8b", "#81a1c1", "#a3be8c", "#8fbcbb", "#81a1c1", "#b48ead"
    ),
    builtin_theme!(
        "oceanic-next",
        "#89bd82",
        "#b77eb8",
        "#50a5a4",
        "#b77eb8",
        "#52606b",
        "#52606b",
        "#121b21",
        "#e44754",
        "#f7bd51",
        "#5486c0",
        "#89bd82",
        "#50a5a4",
        "#5486c0",
        "#b77eb8"
    ),
    builtin_theme!(
        "one-dark", "#98c379", "#c678dd", "#56b6c2", "#c678dd", "#5c6370", "#5c6370", "#1e2127",
        "#e06c75", "#d19a66", "#61afef", "#98c379", "#56b6c2", "#61afef", "#c678dd"
    ),
    builtin_theme!(
        "one-half-black",
        "#98c379",
        "#c678dd",
        "#56b6c2",
        "#c678dd",
        "#282c34",
        "#282c34",
        "#000000",
        "#e06c75",
        "#e5c07b",
        "#61afef",
        "#98c379",
        "#56b6c2",
        "#61afef",
        "#c678dd"
    ),
    builtin_theme!(
        "oxocarbon-dark",
        "#42be65",
        "#ff7eb6",
        "#3ddbd9",
        "#ff7eb6",
        "#393939",
        "#393939",
        "#161616",
        "#ee5396",
        "#ffe97b",
        "#33b1ff",
        "#42be65",
        "#3ddbd9",
        "#33b1ff",
        "#ff7eb6"
    ),
    builtin_theme!(
        "palenight",
        "#c3e88d",
        "#ffcb6b",
        "#676e95",
        "#ffcb6b",
        "#959dcb",
        "#959dcb",
        "#292d3e",
        "#f07178",
        "#ff5572",
        "#82aaff",
        "#c3e88d",
        "#676e95",
        "#82aaff",
        "#c792ea"
    ),
    builtin_theme!(
        "papercolor-dark",
        "#afd700",
        "#ff5faf",
        "#00afaf",
        "#ff5faf",
        "#585858",
        "#585858",
        "#1c1c1c",
        "#5faf5f",
        "#af87d7",
        "#ffaf00",
        "#5faf00",
        "#00afaf",
        "#ffaf00",
        "#808080"
    ),
    builtin_theme!(
        "rose-pine-moon",
        "#9ccfd8",
        "#c4a7e7",
        "#ea9a97",
        "#c4a7e7",
        "#6e6a86",
        "#6e6a86",
        "#232136",
        "#eb6f92",
        "#f6c177",
        "#3e8fb0",
        "#9ccfd8",
        "#ea9a97",
        "#3e8fb0",
        "#c4a7e7"
    ),
    builtin_theme!(
        "snazzy", "#5af78e", "#ff6ac1", "#9aedfe", "#ff6ac1", "#686868", "#686868", "#282a36",
        "#ff5c57", "#f3f99d", "#57c7ff", "#5af78e", "#9aedfe", "#57c7ff", "#ff6ac1"
    ),
    builtin_theme!(
        "solarized-dark",
        "#859900",
        "#d33682",
        "#2aa198",
        "#d33682",
        "#657b83",
        "#657b83",
        "#002b36",
        "#cb4b16",
        "#cf9a6b",
        "#6c71c4",
        "#859900",
        "#2aa198",
        "#6c71c4",
        "#d33682"
    ),
    builtin_theme!(
        "sonokai", "#9ed072", "#b39df3", "#76cce0", "#b39df3", "#7f8490", "#7f8490", "#2c2e34",
        "#fc5d7c", "#e7c664", "#f39660", "#9ed072", "#76cce0", "#f39660", "#b39df3"
    ),
    builtin_theme!(
        "srcery", "#98bc37", "#ff5c8f", "#2be4d0", "#ff5c8f", "#918175", "#918175", "#1c1b19",
        "#f75341", "#fed06e", "#68a8e4", "#519f50", "#2be4d0", "#68a8e4", "#e02c6d"
    ),
    builtin_theme!(
        "synthwave",
        "#72f1b8",
        "#ff7edb",
        "#03edf9",
        "#ff7edb",
        "#575656",
        "#575656",
        "#262335",
        "#fe4450",
        "#fede5d",
        "#03edf9",
        "#72f1b8",
        "#03edf9",
        "#03edf9",
        "#ff7edb"
    ),
    builtin_theme!(
        "tokyo-night",
        "#9ece6a",
        "#bb9af7",
        "#7dcfff",
        "#bb9af7",
        "#565f89",
        "#414868",
        "#24283b",
        "#f7768e",
        "#e0af68",
        "#7aa2f7",
        "#9ece6a",
        "#89ddff",
        "#7aa2f7",
        "#ff9e64"
    ),
    builtin_theme!(
        "tomorrow-night",
        "#b5bd68",
        "#b294bb",
        "#8abeb7",
        "#b294bb",
        "#969896",
        "#969896",
        "#1d1f21",
        "#cc6666",
        "#f0c674",
        "#81a2be",
        "#b5bd68",
        "#8abeb7",
        "#81a2be",
        "#b294bb"
    ),
    builtin_theme!(
        "zenburn", "#ffff87", "#d7afaf", "#93bea3", "#d7afaf", "#757575", "#757575", "#3a3a3a",
        "#dfaf87", "#ffcfaf", "#d7d7af", "#efef87", "#93bea3", "#d7d7af", "#bca3a3"
    ),
];

/// Iterate over every built-in palette name in deterministic catalog order.
pub fn builtin_theme_names() -> impl ExactSizeIterator<Item = &'static str> {
    BUILTIN_THEMES.iter().map(|theme| theme.name)
}

/// Resolve one built-in palette by its stable lowercase name.
///
/// Custom palettes are configuration-owned and are therefore not returned by
/// this catalog lookup.
pub fn builtin_theme(name: &str) -> Option<ThemeColors> {
    BUILTIN_THEMES
        .iter()
        .find(|theme| theme.name == name)
        .copied()
        .map(BuiltinTheme::to_owned_colors)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Metadata for a named plugin prompt callback.
pub struct PromptRegistration {
    /// Unique callback name, capped at [`MAX_REGISTRATION_NAME_BYTES`] UTF-8 bytes.
    pub name: String,
    /// Per-render wall deadline in milliseconds, from 1 through 100.
    ///
    /// Omitted values default to 8 ms. The runtime also caps this deadline by the
    /// enclosing [`LuaPolicy::wall_time`].
    #[serde(default = "default_prompt_deadline_ms")]
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Metadata for a plugin completion callback registered for one command.
pub struct CompletionRegistration {
    /// Unique command path, capped at [`MAX_REGISTRATION_NAME_BYTES`] UTF-8 bytes.
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Typed public contract for a command implemented by a Lua plugin callback.
///
/// Registration rejects empty core fields and requires examples, effects, and
/// error codes before the callback becomes visible to higher catalog consumers.
pub struct CommandRegistration {
    /// Unique command path capped at [`MAX_REGISTRATION_NAME_BYTES`] UTF-8 bytes.
    pub name: String,
    /// Human-readable invocation syntax capped at 2 KiB.
    pub signature: String,
    /// Short description for completion lists and tool manifests, capped at 2 KiB.
    pub summary: String,
    /// Behavioral documentation for help, hover, and agent context, capped at 16 KiB.
    pub details: String,
    /// Exact executable ABI-v1 input contract capped at 256 UTF-8 bytes.
    pub input_type: String,
    /// Exact executable ABI-v1 output contract capped at 256 UTF-8 bytes.
    pub output_type: String,
    /// At most 32 bounded invocation examples required by the quality gate.
    pub examples: Vec<String>,
    /// At most 32 bounded external effects, normalized by the catalog boundary.
    pub effects: Vec<String>,
    /// At most 64 bounded error-code names mapped to human-readable meanings.
    pub error_codes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Snapshot of metadata registered while loading one Lua plugin source.
///
/// Callback functions remain in the runtime registry; this serializable value
/// exposes only validated declarations to catalog, UI, and extension consumers.
/// Serialization and deserialization both repeat [`Self::validate`] so persisted
/// or protocol metadata cannot bypass the Lua ingestion bounds.
pub struct PluginRegistrations {
    /// Prompt callbacks registered under the `prompt.register` capability.
    pub prompt_segments: Vec<PromptRegistration>,
    /// Completion callbacks registered under the `completion.register` capability.
    pub completion_providers: Vec<CompletionRegistration>,
    /// Public commands registered under the `commands.register` capability.
    pub commands: Vec<CommandRegistration>,
    /// Typed event subscriptions and their requested mutation capabilities.
    pub events: Vec<EventSubscription>,
    /// Catalog, completion, and panel contribution declarations.
    pub contributions: Vec<ContributionRegistration>,
}

#[derive(Serialize)]
struct PluginRegistrationsRef<'a> {
    prompt_segments: &'a [PromptRegistration],
    completion_providers: &'a [CompletionRegistration],
    commands: &'a [CommandRegistration],
    events: &'a [EventSubscription],
    contributions: &'a [ContributionRegistration],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginRegistrationsWire {
    prompt_segments: Vec<PromptRegistration>,
    completion_providers: Vec<CompletionRegistration>,
    commands: Vec<CommandRegistration>,
    events: Vec<EventSubscription>,
    contributions: Vec<ContributionRegistration>,
}

impl Serialize for PluginRegistrations {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        PluginRegistrationsRef {
            prompt_segments: &self.prompt_segments,
            completion_providers: &self.completion_providers,
            commands: &self.commands,
            events: &self.events,
            contributions: &self.contributions,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PluginRegistrations {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PluginRegistrationsWire::deserialize(deserializer)?;
        let registrations = Self {
            prompt_segments: wire.prompt_segments,
            completion_providers: wire.completion_providers,
            commands: wire.commands,
            events: wire.events,
            contributions: wire.contributions,
        };
        registrations.validate().map_err(serde::de::Error::custom)?;
        Ok(registrations)
    }
}

impl PluginRegistrations {
    /// Validate all registration bounds and identities at a serialization or protocol boundary.
    ///
    /// Lua callbacks validate before mutating runtime state; this second pass protects readers
    /// and persisted writers from invalid values constructed directly through the public fields.
    pub fn validate(&self) -> Result<(), ShellError> {
        validate_registration_collection(
            "prompt segments",
            self.prompt_segments.len(),
            self.prompt_segments.iter().map(prompt_registration_bytes),
            MAX_PLUGIN_PROMPT_SEGMENTS,
            MAX_PLUGIN_PROMPT_BYTES,
        )?;
        validate_registration_collection(
            "completion providers",
            self.completion_providers.len(),
            self.completion_providers
                .iter()
                .map(completion_registration_bytes),
            MAX_PLUGIN_COMPLETION_PROVIDERS,
            MAX_PLUGIN_COMPLETION_BYTES,
        )?;
        validate_registration_collection(
            "plugin commands",
            self.commands.len(),
            self.commands.iter().map(command_registration_bytes),
            MAX_PLUGIN_COMMANDS,
            MAX_PLUGIN_COMMAND_BYTES,
        )?;
        validate_registration_collection(
            "event handlers",
            self.events.len(),
            self.events.iter().map(event_registration_bytes),
            MAX_PLUGIN_EVENT_HANDLERS,
            MAX_PLUGIN_EVENT_BYTES,
        )?;
        validate_registration_collection(
            "contributions",
            self.contributions.len(),
            self.contributions
                .iter()
                .map(contribution_registration_bytes),
            MAX_PLUGIN_CONTRIBUTIONS,
            MAX_PLUGIN_CONTRIBUTION_BYTES,
        )?;
        let panels = self
            .contributions
            .iter()
            .filter(|registration| registration.kind == ContributionKind::Panel)
            .collect::<Vec<_>>();
        validate_registration_collection(
            "panel contributions",
            panels.len(),
            panels
                .iter()
                .map(|registration| contribution_registration_bytes(registration)),
            MAX_PLUGIN_PANELS,
            MAX_PLUGIN_PANEL_BYTES,
        )?;

        validate_unique_names(
            "prompt segment",
            self.prompt_segments.iter().map(|item| item.name.as_str()),
        )?;
        validate_unique_names(
            "completion provider",
            self.completion_providers
                .iter()
                .map(|item| item.command.as_str()),
        )?;
        validate_unique_names(
            "plugin command",
            self.commands.iter().map(|item| item.name.as_str()),
        )?;
        validate_unique_names(
            "event handler",
            self.events.iter().map(|item| item.name.as_str()),
        )?;
        for registration in &self.prompt_segments {
            validate_prompt_registration(registration)?;
        }
        for registration in &self.completion_providers {
            validate_completion_registration(registration)?;
        }
        for registration in &self.commands {
            validate_command_registration(registration)?;
        }
        for registration in &self.events {
            validate_event_registration(registration)?;
        }
        for registration in &self.contributions {
            validate_contribution_registration(registration)?;
        }
        validate_contribution_set(&self.contributions)
            .map_err(|error| registration_validation_error(error.message))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Isolated result of dispatching one typed event to one plugin handler.
///
/// A handler failure is retained beside an empty action list so later independent
/// handlers still run and the caller can report partial failure deterministically.
pub struct EventHandlerReport {
    /// Registered handler name.
    pub handler: String,
    /// Actions whose required capabilities were validated successfully.
    pub actions: Vec<ExtensionAction>,
    /// Callback, deserialization, deadline, cancellation, or capability error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ShellError>,
}

#[derive(Debug, Clone, Serialize)]
/// One named Lua parameter in a generated host API signature.
pub struct HostParameter {
    /// Parameter identifier emitted into LuaLS stubs and human documentation.
    pub name: &'static str,
    /// Lua-facing type expression emitted without runtime interpretation.
    pub lua_type: &'static str,
}

#[derive(Debug, Clone, Serialize)]
/// Single source-of-truth declaration for one function in Quirl's Lua host module.
///
/// [`HOST_API`] drives LuaLS stubs, stable JSON, Markdown, LSP intelligence, and
/// agent capability discovery. Runtime installation must preserve the same paths
/// and capability checks.
pub struct HostApiSpec {
    /// Fully qualified Lua function path, such as `quirl.process.run`.
    pub path: &'static str,
    /// Concise behavioral documentation shared by every generated view.
    pub summary: &'static str,
    /// Ordered Lua-facing parameters in the callable signature.
    pub parameters: &'static [HostParameter],
    /// Lua-facing return type, or `nil` for registration functions.
    pub returns: &'static str,
    /// Capability grant required to expose or call this function, when privileged.
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
const PLUGIN_COMMAND_PARAMETER: &[HostParameter] = &[HostParameter {
    name: "spec",
    lua_type: "quirl.PluginCommand",
}];
const EVENT_PARAMETER: &[HostParameter] = &[HostParameter {
    name: "spec",
    lua_type: "quirl.EventSubscription",
}];
const CONTRIBUTION_PARAMETER: &[HostParameter] = &[HostParameter {
    name: "spec",
    lua_type: "quirl.Contribution",
}];

/// Canonical host API used to generate editor, documentation, and agent surfaces.
///
/// This table describes availability, but grants no authority by itself. Each
/// privileged runtime function independently verifies its capability before use.
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
        summary: "Run a command through the composed bounded native process host.",
        parameters: COMMAND_PARAMETER,
        returns: "quirl.ProcessResult",
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
    HostApiSpec {
        path: "quirl.plugin.command",
        summary: "Register a documented command with exact ABI-v1 value I/O; live streams are rejected.",
        parameters: PLUGIN_COMMAND_PARAMETER,
        returns: "nil",
        capability: Some("commands.register"),
    },
    HostApiSpec {
        path: "quirl.events.subscribe",
        summary: "Observe immutable typed shell events and return declared actions.",
        parameters: EVENT_PARAMETER,
        returns: "nil",
        capability: Some("events.observe"),
    },
    HostApiSpec {
        path: "quirl.extension.contribute",
        summary: "Register a typed catalog, completion, or panel contribution.",
        parameters: CONTRIBUTION_PARAMETER,
        returns: "nil",
        capability: Some("extension.contribute"),
    },
];

#[derive(Debug)]
struct Budget {
    instruction_limit: u64,
    remaining_instructions: u64,
    started: Instant,
    deadline: Instant,
    wall_time: Duration,
    termination: Option<ShellError>,
}

#[derive(Clone)]
struct HostExecutionState {
    budget: Arc<Mutex<Budget>>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct PluginCallbacks {
    prompt_segments: HashMap<String, PromptCallback>,
    completion_providers: HashMap<String, RegistryKey>,
    commands: HashMap<String, RegistryKey>,
    events: HashMap<String, EventCallback>,
    contributions: HashMap<String, ContributionCallback>,
}

#[derive(Debug)]
struct PromptCallback {
    function: RegistryKey,
    deadline: Duration,
}

#[derive(Debug)]
struct EventCallback {
    function: RegistryKey,
    events: Vec<EventKind>,
    capabilities: Vec<ExtensionCapability>,
    deadline: Duration,
}

#[derive(Debug)]
struct ContributionCallback {
    function: RegistryKey,
    deadline: Duration,
}

#[derive(Debug, Clone)]
/// Cloneable cancellation handle shared with one [`LuaRuntime`].
///
/// Cancellation is cooperative: Lua observes it from the instruction hook and
/// composed process hosts receive the same atomic flag.
pub struct LuaCancellation {
    cancelled: Arc<AtomicBool>,
}

impl LuaCancellation {
    /// Request cancellation of current and subsequent work on the associated runtime.
    ///
    /// The flag remains set until [`LuaRuntime::clear_cancellation`] is called.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Default)]
/// Last-known-good configuration holder for atomic in-memory reload semantics.
pub struct ConfigStore {
    active: QuirlConfig,
}

impl ConfigStore {
    /// Borrow the currently active validated configuration.
    pub fn active(&self) -> &QuirlConfig {
        &self.active
    }

    /// Load and validate a candidate file, replacing the active value only on success.
    ///
    /// I/O, Lua, resource-limit, migration, or schema failures leave the previous
    /// configuration unchanged.
    pub fn reload(
        &mut self,
        runtime: &LuaRuntime,
        path: &Path,
    ) -> Result<&QuirlConfig, ShellError> {
        let candidate = runtime.load_config_file(path)?;
        self.active = candidate;
        Ok(&self.active)
    }

    /// Atomically install an already validated cross-process candidate.
    pub fn install(&mut self, candidate: QuirlConfig) -> Result<&QuirlConfig, ShellError> {
        candidate.validate("isolated Lua configuration")?;
        self.active = candidate;
        Ok(&self.active)
    }
}

/// Sandboxed Lua 5.4 VM with explicit policy, capabilities, and callback registries.
///
/// Only table, string, math, and UTF-8 standard libraries are installed. `io`,
/// `os`, `debug`, `package`, `require`, and the dynamic chunk loader remain
/// unavailable. Native Lua string patterns are disabled because their C
/// implementation cannot observe the wall deadline; explicit literal
/// `string.find` remains available. All public execution paths reset bounded
/// instruction and wall budgets and return [`ShellError`] rather than exposing raw
/// Lua values to higher crates. Policy termination propagates through guest `pcall`
/// and `xpcall`; ordinary guest errors remain catchable.
pub struct LuaRuntime {
    lua: Lua,
    policy: LuaPolicy,
    budget: Arc<Mutex<Budget>>,
    cancelled: Arc<AtomicBool>,
    registrations: Arc<Mutex<PluginRegistrations>>,
    callbacks: Arc<Mutex<PluginCallbacks>>,
    last_event_sequence: Arc<Mutex<Option<u64>>>,
}

impl LuaRuntime {
    /// Construct a restricted runtime with standard capabilities derived from `policy`.
    ///
    /// A process-capable policy grants the name `process.spawn`, but without an
    /// explicitly composed process host process calls still fail closed.
    pub fn new(policy: LuaPolicy) -> Result<Self, ShellError> {
        Self::new_with_capabilities(policy, &default_capabilities(policy))
    }

    /// Construct a runtime using the standard grants plus a composed process
    /// backend.  This is intended for the CLI composition root.
    pub fn new_with_process_host(
        policy: LuaPolicy,
        process_host: ProcessHost,
    ) -> Result<Self, ShellError> {
        Self::new_with_capabilities_and_process_host(
            policy,
            &default_capabilities(policy),
            Some(process_host),
        )
    }

    /// Construct a process-capable runtime that observes an existing cancellation flag.
    ///
    /// The CLI composition root uses this boundary so cancellation before VM
    /// initialization, Lua instruction hooks, callbacks, and injected process
    /// work all share one identity. The caller must not clear the flag while an
    /// invocation is running.
    pub fn new_with_process_host_and_cancellation(
        policy: LuaPolicy,
        process_host: ProcessHost,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        Self::new_with_capabilities_process_host_and_cancellation(
            policy,
            &default_capabilities(policy),
            Some(process_host),
            cancelled,
        )
    }

    /// Construct a restricted runtime that observes an existing cancellation
    /// flag without injecting a process capability.
    ///
    /// Composition roots use this when an execution plan does not declare
    /// [`ExecutionEffect::SpawnProcess`].
    pub fn new_with_cancellation(
        policy: LuaPolicy,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        Self::new_with_capabilities_process_host_and_cancellation(
            policy,
            &default_capabilities(policy),
            None,
            cancelled,
        )
    }

    /// Construct a runtime whose host handles are limited to explicit grants.
    pub fn new_with_capabilities(
        policy: LuaPolicy,
        granted_capabilities: &[String],
    ) -> Result<Self, ShellError> {
        Self::new_with_capabilities_and_process_host(policy, granted_capabilities, None)
    }

    /// Construct a runtime with an explicitly composed, bounded process host.
    /// Runtimes without this host fail closed when Lua asks to spawn a process.
    pub fn new_with_capabilities_and_process_host(
        policy: LuaPolicy,
        granted_capabilities: &[String],
        process_host: Option<ProcessHost>,
    ) -> Result<Self, ShellError> {
        Self::new_with_capabilities_process_host_and_cancellation(
            policy,
            granted_capabilities,
            process_host,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn new_with_capabilities_process_host_and_cancellation(
        policy: LuaPolicy,
        granted_capabilities: &[String],
        process_host: Option<ProcessHost>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ShellError> {
        policy.validate()?;
        let libraries = StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8;
        let lua = Lua::new_with(libraries, LuaOptions::default())
            .map_err(|error| lua_error(error, None, 0))?;
        lua.set_memory_limit(policy.memory_limit_bytes)
            .map_err(|error| lua_error(error, None, 0))?;
        let started = Instant::now();
        let deadline = started.checked_add(policy.wall_time).ok_or_else(|| {
            validation_error(
                "Lua policy",
                "wall_time is too large for the host monotonic clock",
            )
        })?;
        let budget = Arc::new(Mutex::new(Budget {
            instruction_limit: policy.instruction_limit,
            remaining_instructions: policy.instruction_limit,
            started,
            deadline,
            wall_time: policy.wall_time,
            termination: None,
        }));
        let registrations = Arc::new(Mutex::new(PluginRegistrations::default()));
        let callbacks = Arc::new(Mutex::new(PluginCallbacks::default()));
        let last_event_sequence = Arc::new(Mutex::new(None));

        install_restrictions(&lua, Arc::clone(&budget))
            .map_err(|error| lua_error(error, None, 0))?;
        install_budget_hook(&lua, Arc::clone(&budget), Arc::clone(&cancelled))
            .map_err(|error| lua_error(error, None, 0))?;
        install_protected_call_guards(&lua, Arc::clone(&budget))
            .map_err(|error| lua_error(error, None, 0))?;
        install_host_api(
            &lua,
            policy,
            granted_capabilities.iter().cloned().collect(),
            process_host,
            HostExecutionState {
                budget: Arc::clone(&budget),
                cancelled: Arc::clone(&cancelled),
            },
            Arc::clone(&registrations),
            Arc::clone(&callbacks),
        )
        .map_err(|error| lua_error(error, None, 0))?;
        Ok(Self {
            lua,
            policy,
            budget,
            cancelled,
            registrations,
            callbacks,
            last_event_sequence,
        })
    }
}

fn default_capabilities(policy: LuaPolicy) -> Vec<String> {
    let mut capabilities = Vec::new();
    for capability in HOST_API.iter().filter_map(|spec| spec.capability) {
        if capability != "process.spawn" || policy.allow_process {
            let capability = capability.to_owned();
            if !capabilities.contains(&capability) {
                capabilities.push(capability);
            }
        }
    }
    for capability in ["catalog.register", "ui.panel"] {
        capabilities.push(capability.to_owned());
    }
    capabilities
}

impl LuaRuntime {
    /// Evaluate bounded Lua source and deserialize its result to JSON.
    ///
    /// Source larger than [`MAX_LUA_SOURCE_BYTES`], resource exhaustion,
    /// cancellation, Lua errors, and non-serializable return values become
    /// [`ShellError`] values.
    pub fn eval(&self, source: &str) -> Result<serde_json::Value, ShellError> {
        validate_source_length(source, Path::new("eval"))?;
        self.reset_budget();
        let value = self
            .lua
            .load(source)
            .set_name("eval")
            .eval::<Value>()
            .map_err(|error| lua_error(error, None, source.len()))?;
        self.value_to_json(value, None, source.len())
    }

    /// Read and execute a Lua module under this runtime's policy.
    ///
    /// Versioned modules receive the complete typed runner context. Historical
    /// unversioned modules are migrated through the bounded v0 adapter.
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
        let context = LuaRunnerContext::from_current_process(
            arguments,
            ExecutionInput::None,
            ExecutionOutputTarget::Value,
            ExecutionEffects::from_effects(&[ExecutionEffect::SpawnProcess]),
            Arc::clone(&self.cancelled),
        )?;
        let outcome = self.run_source_with_context(source, source_name, &context)?;
        runner_output_to_legacy_json(outcome.output)
    }

    /// Execute a Lua module through the versioned ABI and return the shared execution outcome.
    ///
    /// ABI v1 modules declare `abi_version = 1` beside `main` and must return a
    /// deny-unknown typed result envelope. Unversioned or explicit v0 modules
    /// retain the historical arbitrary-JSON return shape, but Rust immediately
    /// validates and migrates it into a bounded [`ExecutionOutcome`]. Future
    /// versions fail closed before `main` is called.
    pub fn run_source_with_context(
        &self,
        source: &str,
        source_name: &str,
        context: &LuaRunnerContext,
    ) -> Result<ExecutionOutcome, ShellError> {
        let path = Path::new(source_name);
        validate_source_length(source, path)?;
        context.validate()?;
        if !Arc::ptr_eq(&context.cancelled, &self.cancelled) {
            return Err(runner_validation_error(
                "Lua runner context cancellation does not match the runtime",
            )
            .with_help("Create the context with the same cancellation flag passed to LuaRuntime"));
        }
        ensure_runner_active(&context.cancelled, "before Lua module evaluation")?;
        let source = normalize_shebang(source);
        lint_source(&source, path)?;
        self.reset_budget();
        let value = self
            .lua
            .load(&source)
            .set_name(source_name)
            .eval::<Value>()
            .map_err(|error| lua_error(error, Some(path), source.len()))?;
        ensure_runner_active(&context.cancelled, "after Lua module evaluation")?;
        self.dispatch_runner_module(value, context, path, source.len())
    }

    /// Evaluate a configuration file and validate it against the Rust schema.
    ///
    /// Unknown fields are rejected, legacy versions are migrated to
    /// [`CONFIG_SCHEMA_VERSION`], and invalid enum domains or bounds fail before a
    /// [`QuirlConfig`] crosses the Lua boundary.
    pub fn load_config_file(&self, path: &Path) -> Result<QuirlConfig, ShellError> {
        let source = read_source(path)?;
        self.load_config_source(&source, &path.display().to_string())
    }

    /// Evaluate an immutable configuration source snapshot and validate its schema.
    pub fn load_config_source(
        &self,
        source: &str,
        source_name: &str,
    ) -> Result<QuirlConfig, ShellError> {
        let path = Path::new(source_name);
        validate_source_length(source, path)?;
        lint_source(source, path)?;
        self.reset_budget();
        let value = self
            .lua
            .load(source)
            .set_name(source_name)
            .eval::<Value>()
            .map_err(|error| lua_error(error, Some(path), source.len()))?;
        let mut config = self.lua.from_value::<QuirlConfig>(value).map_err(|error| {
            validation_error(
                &path.display().to_string(),
                format!("configuration does not match the Rust schema: {error}"),
            )
        })?;
        if config.schema_version < CONFIG_SCHEMA_VERSION {
            config.schema_version = CONFIG_SCHEMA_VERSION;
        }
        config.validate(&path.display().to_string())?;
        Ok(config)
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin registry mutex may contain inconsistent registrations after a host callback panic"
    )]
    /// Read and load a plugin file, returning only its validated registration metadata.
    ///
    /// Managed integrity-sensitive callers should prefer [`LuaRuntime::load_plugin_source`]
    /// with bytes captured after lock verification.
    pub fn load_plugin_file(&self, path: &Path) -> Result<PluginRegistrations, ShellError> {
        let source = read_source(path)?;
        self.load_plugin_source(&source, &path.display().to_string())
    }

    /// Load a plugin from an immutable source snapshot.
    ///
    /// Managed plugin hosts use this after verifying locked bytes so an
    /// attacker cannot replace the file between integrity verification and
    /// execution.
    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin registry mutex may contain inconsistent registrations after a host callback panic"
    )]
    pub fn load_plugin_source(
        &self,
        source: &str,
        source_name: &str,
    ) -> Result<PluginRegistrations, ShellError> {
        let path = Path::new(source_name);
        validate_source_length(source, path)?;
        lint_source(source, path)?;
        self.clear_plugin_state();
        self.last_event_sequence
            .lock()
            .expect("plugin event sequence mutex poisoned")
            .take();
        self.reset_budget();
        if let Err(error) = self.lua.load(source).set_name(source_name).exec() {
            self.clear_plugin_state();
            return Err(lua_error(error, Some(path), source.len()));
        }
        let registrations = self
            .registrations
            .lock()
            .expect("plugin registration mutex poisoned")
            .clone();
        if registrations.prompt_segments.is_empty()
            && registrations.completion_providers.is_empty()
            && registrations.commands.is_empty()
            && registrations.events.is_empty()
            && registrations.contributions.is_empty()
        {
            return Err(validation_error(
                &path.display().to_string(),
                "plugin did not register a command, prompt segment, completion provider, event handler, or contribution",
            ));
        }
        if let Err(error) = registrations.validate() {
            self.clear_plugin_state();
            return Err(error.with_context("lua failure: registration validation"));
        }
        Ok(registrations)
    }

    #[allow(
        clippy::expect_used,
        reason = "poisoned plugin state cannot be recovered or exposed safely"
    )]
    fn clear_plugin_state(&self) {
        self.registrations
            .lock()
            .expect("plugin registration mutex poisoned")
            .clone_from(&PluginRegistrations::default());
        let mut callbacks = self
            .callbacks
            .lock()
            .expect("plugin callback mutex poisoned");
        callbacks.prompt_segments.clear();
        callbacks.completion_providers.clear();
        callbacks.commands.clear();
        callbacks.events.clear();
        callbacks.contributions.clear();
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin registry mutex may contain inconsistent registrations after a host callback panic"
    )]
    /// Clone the current plugin's validated registration metadata.
    ///
    /// Callback functions themselves remain private registry keys inside the VM.
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
    /// Invoke a named prompt callback with JSON context.
    ///
    /// The callback receives its declared 1–100 ms deadline, capped by the runtime
    /// policy, and may return no segment. Unknown names and callback failures are errors.
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
        let rendered = function
            .call::<Value>(context)
            .map_err(|error| lua_error(error, None, 0))?;
        match rendered {
            Value::Nil => Ok(None),
            Value::String(rendered) => {
                if rendered.as_bytes().len() > MAX_PROMPT_RETURN_BYTES {
                    return Err(lua_return_limit_error(
                        "prompt bytes",
                        rendered.as_bytes().len(),
                        MAX_PROMPT_RETURN_BYTES,
                    ));
                }
                let rendered = rendered.to_str().map_err(|error| {
                    validation_error(
                        name,
                        format!("prompt segment must return valid UTF-8 text: {error}"),
                    )
                })?;
                reject_terminal_controls("prompt segment", &rendered)?;
                Ok(Some(rendered.to_owned()))
            }
            other => Err(validation_error(
                name,
                format!(
                    "prompt segment must return text or nil, not Lua {}",
                    other.type_name()
                ),
            )),
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin callback mutex may contain inconsistent callbacks after a host callback panic"
    )]
    /// Invoke a registered completion provider and validate its JSON result.
    ///
    /// Execution is capped at 50 ms and by the runtime policy. Results must be an
    /// array of at most 1,000 scalar or typed completion items, with each item
    /// retaining no more than 16 KiB of string content.
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

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin callback mutex may contain inconsistent callbacks after a host callback panic"
    )]
    /// Invoke a named catalog, completion, or panel contribution callback.
    ///
    /// Registration kind and name select the callback. Its declared deadline is
    /// capped by the runtime policy, and the JSON result is rejected if it contains
    /// terminal control text before it reaches higher consumers.
    pub fn invoke_contribution(
        &self,
        kind: ContributionKind,
        name: &str,
        context: &serde_json::Value,
    ) -> Result<serde_json::Value, ShellError> {
        let key = format!("{kind:?}:{name}");
        let (function, deadline) = {
            let callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            let callback = callbacks.contributions.get(&key).ok_or_else(|| {
                validation_error(name, format!("unknown {kind:?} contribution `{name}`"))
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
        let value = function
            .call::<Value>(context)
            .map_err(|error| lua_error(error, None, 0))?;
        let value = self.value_to_json(value, None, 0)?;
        reject_json_terminal_controls("extension contribution output", &value)?;
        Ok(value)
    }

    /// Test-only legacy seam for bounded JSON callback migration coverage.
    ///
    /// The callback runs under a 50 ms deadline capped by the runtime policy, and
    /// its Lua return value must deserialize into JSON.
    #[cfg(test)]
    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin callback mutex may contain inconsistent callbacks after a host callback panic"
    )]
    fn run_plugin_command(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<serde_json::Value, ShellError> {
        if !arguments.is_object() {
            return Err(validation_error(
                name,
                "plugin command arguments must be a named object",
            ));
        }
        let typed_arguments = StructuredValue::from_json(arguments.clone());
        typed_arguments.validate()?;
        let function = {
            let callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            let key = callbacks.commands.get(name).ok_or_else(|| {
                validation_error(name, format!("unknown plugin command `{name}`"))
            })?;
            self.lua
                .registry_value::<Function>(key)
                .map_err(|error| lua_error(error, None, 0))?
        };
        let arguments = self
            .lua
            .to_value(arguments)
            .map_err(|error| lua_error(error, None, 0))?;
        self.reset_budget_with_deadline(COMPLETION_CALLBACK_DEADLINE);
        let value = function
            .call::<Value>(arguments)
            .map_err(|error| lua_error(error, None, 0))?;
        let value = self.value_to_json(value, None, 0)?;
        StructuredValue::from_json(value.clone()).validate()?;
        Ok(value)
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin callback mutex may contain inconsistent callbacks after a host callback panic"
    )]
    /// Dispatch a registered plugin command through the typed runner ABI.
    ///
    /// The callback receives the same immutable [`LuaRunnerContext`] as a
    /// versioned script `main` function and must return an ABI-v1
    /// [`ExecutionOutcome`] envelope. Unknown fields, legacy JSON results,
    /// byte output, unbounded values, cancellation, and malformed structured
    /// errors fail closed before crossing back to the composition root.
    pub fn run_plugin_command_with_context(
        &self,
        name: &str,
        context: &LuaRunnerContext,
        expires_at: Instant,
    ) -> Result<ExecutionOutcome, ShellError> {
        context.validate()?;
        if !Arc::ptr_eq(&context.cancelled, &self.cancelled) {
            return Err(runner_validation_error(
                "plugin command context cancellation does not match the runtime",
            )
            .with_help("Build the execution request from the selected plugin runtime"));
        }
        ensure_runner_active(&self.cancelled, "before plugin command callback")?;
        let function = {
            let callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            let key = callbacks.commands.get(name).ok_or_else(|| {
                validation_error(name, format!("unknown plugin command `{name}`"))
            })?;
            self.lua
                .registry_value::<Function>(key)
                .map_err(|error| lua_error(error, None, 0))?
        };
        let context = self.create_runner_context(context, Path::new(name), 0)?;
        ensure_runner_active(&self.cancelled, "after plugin input conversion")?;
        let remaining = expires_at
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "plugin command input conversion exceeded its absolute deadline",
                )
                .with_help("Reduce typed input size or callback work before retrying")
            })?;
        self.reset_budget_with_deadline(remaining);
        let value = function
            .call::<Value>(context)
            .map_err(|error| lua_error(error, Some(Path::new(name)), 0))?;
        ensure_runner_active(&self.cancelled, "after plugin command callback")?;
        self.decode_runner_result(value, Path::new(name))
    }

    #[allow(
        clippy::expect_used,
        reason = "a poisoned plugin callback mutex may contain inconsistent callbacks after a host callback panic"
    )]
    /// Dispatch one immutable, strictly sequenced event to subscribed handlers.
    ///
    /// Handlers run in name order under individual declared deadlines. Output text
    /// is redacted unless `output_read` was granted, and every returned action is
    /// validated against the handler's declared capabilities. Individual handler
    /// failures are reported in [`EventHandlerReport`] rather than aborting later handlers.
    pub fn dispatch_extension_event(
        &self,
        event: &ExtensionEvent,
    ) -> Result<Vec<EventHandlerReport>, ShellError> {
        {
            let mut previous = self
                .last_event_sequence
                .lock()
                .expect("plugin event sequence mutex poisoned");
            event.validate_after(*previous)?;
            *previous = Some(event.sequence);
        }
        let mut handlers = {
            let callbacks = self
                .callbacks
                .lock()
                .expect("plugin callback mutex poisoned");
            callbacks
                .events
                .iter()
                .filter(|(_, callback)| callback.events.contains(&event.data.kind()))
                .map(|(name, callback)| {
                    self.lua
                        .registry_value::<Function>(&callback.function)
                        .map(|function| {
                            (
                                name.clone(),
                                function,
                                callback.capabilities.clone(),
                                callback.deadline,
                            )
                        })
                })
                .collect::<mlua::Result<Vec<_>>>()
                .map_err(|error| lua_error(error, None, 0))?
        };
        handlers.sort_by(|left, right| left.0.cmp(&right.0));
        let mut reports = Vec::with_capacity(handlers.len());
        for (name, function, capabilities, deadline) in handlers {
            let mut visible_event = event.clone();
            if !capabilities.contains(&ExtensionCapability::OutputRead)
                && let ExtensionEventData::Output { text, .. } = &mut visible_event.data
            {
                *text = None;
            }
            let result = self
                .lua
                .to_value(&visible_event)
                .map_err(|error| lua_error(error, None, 0))
                .and_then(|record| {
                    self.reset_budget_with_deadline(deadline);
                    function
                        .call::<Value>(record)
                        .map_err(|error| lua_error(error, None, 0))
                })
                .and_then(|value| self.value_to_json(value, None, 0))
                .and_then(|value| {
                    serde_json::from_value::<Vec<ExtensionAction>>(value).map_err(|error| {
                        validation_error(
                            &name,
                            format!(
                                "event handler must return an array of declared actions: {error}"
                            ),
                        )
                    })
                })
                .and_then(|actions| {
                    if actions.len() > MAX_EVENT_ACTIONS {
                        return Err(lua_return_limit_error(
                            "event actions",
                            actions.len(),
                            MAX_EVENT_ACTIONS,
                        ));
                    }
                    for action in &actions {
                        action.validate(&capabilities)?;
                    }
                    Ok(actions)
                });
            match result {
                Ok(actions) => reports.push(EventHandlerReport {
                    handler: name,
                    actions,
                    error: None,
                }),
                Err(error) => reports.push(EventHandlerReport {
                    handler: name,
                    actions: Vec::new(),
                    error: Some(error),
                }),
            }
        }
        Ok(reports)
    }

    /// Compatibility adapter for the initial observation-only event API.
    pub fn dispatch_plugin_event(
        &self,
        event: &str,
        record: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, ShellError> {
        let kind = parse_event_kind(event).ok_or_else(|| {
            validation_error(event, format!("unknown typed extension event `{event}`"))
        })?;
        let functions = {
            let callbacks = self
                .callbacks
                .lock()
                .map_err(|_| validation_error(event, "plugin callback state is unavailable"))?;
            callbacks
                .events
                .values()
                .filter(|callback| callback.events.contains(&kind))
                .map(|callback| self.lua.registry_value::<Function>(&callback.function))
                .collect::<mlua::Result<Vec<_>>>()
                .map_err(|error| lua_error(error, None, 0))?
        };
        let mut values = Vec::new();
        for function in functions {
            let record = self
                .lua
                .to_value(record)
                .map_err(|error| lua_error(error, None, 0))?;
            self.reset_budget_with_deadline(COMPLETION_CALLBACK_DEADLINE);
            let value = function
                .call::<Value>(record)
                .map_err(|error| lua_error(error, None, 0))?;
            values.push(self.value_to_json(value, None, 0)?);
        }
        Ok(values)
    }

    /// Read and execute a Lua test module, returning the number of tests run.
    ///
    /// The module must return a table containing at least one `test_*` function.
    /// Tests execute in sorted name order with a fresh policy budget for each test.
    pub fn test_file(&self, path: &Path) -> Result<usize, ShellError> {
        let source = read_source(path)?;
        self.test_source(&source, &path.display().to_string())
    }

    /// Execute an in-memory Lua test module under the same contract as [`Self::test_file`].
    ///
    /// `source_name` is used for diagnostics; it does not grant filesystem access.
    pub fn test_source(&self, source: &str, source_name: &str) -> Result<usize, ShellError> {
        let path = Path::new(source_name);
        validate_source_length(source, path)?;
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

    /// Read, lint, and parse a Lua file without executing it.
    pub fn check_file(path: &Path) -> Result<(), ShellError> {
        let source = read_source(path)?;
        Self::check_source(&source, &path.display().to_string())
    }

    /// Parse and lint Lua source without executing it.
    pub fn check_source(source: &str, source_name: &str) -> Result<(), ShellError> {
        let path = Path::new(source_name);
        validate_source_length(source, path)?;
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

    /// Return a cloneable handle that can cooperatively cancel this runtime.
    pub fn cancellation_token(&self) -> LuaCancellation {
        LuaCancellation {
            cancelled: Arc::clone(&self.cancelled),
        }
    }

    /// Return the shared execution cancellation identity observed by this VM.
    ///
    /// Composition adapters use this when a persistent trusted-plugin runtime
    /// supplies the engine for an [`ExecutionRequest`](quirl_core::ExecutionRequest).
    /// The caller must quiesce other callbacks before clearing or reusing it.
    pub fn execution_cancellation(&self) -> ExecutionCancellation {
        ExecutionCancellation::from_atomic(Arc::clone(&self.cancelled))
    }

    /// Clear a prior cancellation request before deliberately reusing the runtime.
    ///
    /// Clearing does not reset instruction or wall budgets; the next public
    /// invocation performs its normal budget reset.
    pub fn clear_cancellation(&self) {
        self.cancelled.store(false, Ordering::Relaxed);
    }

    fn dispatch_runner_module(
        &self,
        value: Value,
        context: &LuaRunnerContext,
        path: &Path,
        source_len: usize,
    ) -> Result<ExecutionOutcome, ShellError> {
        let Value::Table(module) = value else {
            return self.migrate_legacy_runner_result(value, path, source_len);
        };
        let abi_version = module.get::<Option<u32>>("abi_version").map_err(|error| {
            runner_boundary_error(path, format!("invalid module ABI version: {error}"))
        })?;
        match abi_version {
            None | Some(LUA_RUNNER_OLDEST_READABLE_ABI_VERSION) => {
                let value =
                    self.call_runner_main_if_present(module, context, path, source_len, false)?;
                self.migrate_legacy_runner_result(value, path, source_len)
            }
            Some(LUA_RUNNER_ABI_VERSION) => {
                validate_runner_module_fields(&module, path)?;
                let value =
                    self.call_runner_main_if_present(module, context, path, source_len, true)?;
                self.decode_runner_result(value, path)
            }
            Some(version) => Err(unsupported_runner_abi_error(version)),
        }
    }

    fn call_runner_main_if_present(
        &self,
        module: Table,
        context: &LuaRunnerContext,
        path: &Path,
        source_len: usize,
        main_required: bool,
    ) -> Result<Value, ShellError> {
        let main = module
            .get::<Option<Function>>("main")
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        let Some(main) = main else {
            if main_required {
                return Err(runner_boundary_error(
                    path,
                    "Lua runner ABI v1 module must contain a `main` function",
                ));
            }
            return Ok(Value::Table(module));
        };
        let context = self.create_runner_context(context, path, source_len)?;
        let value = main
            .call::<Value>(context)
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        ensure_runner_active(&self.cancelled, "after Lua runner main")?;
        Ok(value)
    }

    fn create_runner_context(
        &self,
        context: &LuaRunnerContext,
        path: &Path,
        source_len: usize,
    ) -> Result<Table, ShellError> {
        let table = self
            .lua
            .create_table()
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        table
            .set("abi_version", LUA_RUNNER_ABI_VERSION)
            .and_then(|()| {
                let arguments = self
                    .lua
                    .create_sequence_from(context.arguments.iter().cloned())?;
                table.set("args", arguments)
            })
            .and_then(|()| table.set("env", self.lua.to_value(&context.environment)?))
            .and_then(|()| table.set("cwd", context.working_directory.as_str()))
            .and_then(|()| table.set("input", self.lua.to_value(&context.input)?))
            .and_then(|()| table.set("output", self.lua.to_value(&context.output)?))
            .and_then(|()| {
                table.set(
                    "effects",
                    self.lua
                        .create_sequence_from(runner_effect_names(context.declared_effects))?,
                )
            })
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        let cancellation = self
            .lua
            .create_table()
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        let cancelled = Arc::clone(&context.cancelled);
        cancellation
            .set(
                "is_cancelled",
                self.lua
                    .create_function(move |_, ()| Ok(cancelled.load(Ordering::Relaxed)))
                    .map_err(|error| lua_error(error, Some(path), source_len))?,
            )
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        table
            .set("cancellation", cancellation)
            .map_err(|error| lua_error(error, Some(path), source_len))?;
        Ok(table)
    }

    fn decode_runner_result(
        &self,
        value: Value,
        path: &Path,
    ) -> Result<ExecutionOutcome, ShellError> {
        validate_lua_return_shape(&value)?;
        let wire = self
            .lua
            .from_value::<LuaRunnerResultWire>(value)
            .map_err(|error| {
                runner_boundary_error(path, format!("invalid ABI v1 result: {error}"))
            })?;
        validate_runner_result(wire)
    }

    fn migrate_legacy_runner_result(
        &self,
        value: Value,
        path: &Path,
        source_len: usize,
    ) -> Result<ExecutionOutcome, ShellError> {
        let value = self.value_to_json(value, Some(path), source_len)?;
        let status = legacy_runner_status(&value)?;
        let value = StructuredValue::from_json(value);
        value.validate()?;
        ExecutionOutcome::new(
            ExecutionStatus::Exited(status),
            ExecutionOutput::Value { value },
            Vec::new(),
            ExecutionCleanupState::Complete,
        )
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
        validate_lua_return_shape(&value)?;
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
        let wall_time = deadline.min(self.policy.wall_time);
        let started = Instant::now();
        let expires = started.checked_add(wall_time).unwrap_or(started);
        budget.instruction_limit = self.policy.instruction_limit;
        budget.remaining_instructions = self.policy.instruction_limit;
        budget.started = started;
        budget.deadline = expires;
        budget.wall_time = wall_time;
        budget.termination = None;
    }
}

fn validate_runner_arguments(arguments: &[String]) -> Result<(), ShellError> {
    if arguments.len() > EXECUTION_ARGUMENTS_MAX {
        return Err(runner_limit_error(
            "Lua runner arguments",
            EXECUTION_ARGUMENTS_MAX,
            arguments.len(),
        ));
    }
    let bytes = arguments.iter().fold(0_usize, |total, argument| {
        total.saturating_add(argument.len())
    });
    if bytes > EXECUTION_ARGUMENT_BYTES_MAX {
        return Err(runner_limit_error(
            "Lua runner argument bytes",
            EXECUTION_ARGUMENT_BYTES_MAX,
            bytes,
        ));
    }
    Ok(())
}

fn validate_runner_environment(environment: &BTreeMap<String, String>) -> Result<(), ShellError> {
    if environment.len() > MAX_LUA_RUNNER_ENVIRONMENT_ENTRIES {
        return Err(runner_limit_error(
            "Lua runner environment entries",
            MAX_LUA_RUNNER_ENVIRONMENT_ENTRIES,
            environment.len(),
        ));
    }
    let bytes = environment.iter().fold(0_usize, |total, (key, value)| {
        total.saturating_add(key.len()).saturating_add(value.len())
    });
    if bytes > MAX_LUA_RUNNER_ENVIRONMENT_BYTES {
        return Err(runner_limit_error(
            "Lua runner environment bytes",
            MAX_LUA_RUNNER_ENVIRONMENT_BYTES,
            bytes,
        ));
    }
    for (key, value) in environment {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(runner_validation_error(
                "Lua runner environment names must be non-empty and contain neither `=` nor NUL",
            ));
        }
        if value.contains('\0') {
            return Err(runner_validation_error(
                "Lua runner environment values must not contain NUL",
            ));
        }
    }
    Ok(())
}

fn validate_runner_text(description: &str, value: &str, limit: usize) -> Result<(), ShellError> {
    if value.len() > limit {
        return Err(runner_limit_error(description, limit, value.len()));
    }
    Ok(())
}

fn validate_runner_input(input: &ExecutionInput) -> Result<(), ShellError> {
    match input {
        ExecutionInput::None => Ok(()),
        ExecutionInput::Bytes(bytes) if bytes.len() <= EXECUTION_BYTES_MAX => Ok(()),
        ExecutionInput::Bytes(bytes) => Err(runner_limit_error(
            "Lua runner input bytes",
            EXECUTION_BYTES_MAX,
            bytes.len(),
        )),
        ExecutionInput::Value(value) => value.validate(),
    }
}

fn validate_runner_output_target(output: ExecutionOutputTarget) -> Result<(), ShellError> {
    match output {
        ExecutionOutputTarget::Value => Ok(()),
        ExecutionOutputTarget::Inherit | ExecutionOutputTarget::Capture { .. } => Err(
            runner_validation_error("Lua runner ABI v1 supports typed value output only")
                .with_help("Select value output or use a byte-oriented execution adapter"),
        ),
    }
}

fn validate_runner_module_fields(module: &Table, path: &Path) -> Result<(), ShellError> {
    for pair in module.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|error| lua_error(error, Some(path), 0))?;
        let Value::String(key) = key else {
            return Err(runner_boundary_error(
                path,
                "Lua runner ABI v1 module keys must be strings",
            ));
        };
        let key = key.to_str().map_err(|error| {
            runner_boundary_error(path, format!("Lua runner module key is not UTF-8: {error}"))
        })?;
        if key.len() > MAX_REGISTRATION_NAME_BYTES {
            return Err(runner_limit_error(
                "Lua runner module field name bytes",
                MAX_REGISTRATION_NAME_BYTES,
                key.len(),
            ));
        }
        if !matches!(key.as_ref(), "abi_version" | "main") {
            return Err(runner_boundary_error(
                path,
                format!("unknown Lua runner module field `{key}`"),
            ));
        }
    }
    Ok(())
}

fn validate_runner_result(wire: LuaRunnerResultWire) -> Result<ExecutionOutcome, ShellError> {
    if wire.abi_version != LUA_RUNNER_ABI_VERSION {
        return Err(unsupported_runner_abi_error(wire.abi_version));
    }
    if wire.ok {
        if wire.error.is_some() {
            return Err(runner_validation_error(
                "successful Lua runner result must not contain `error`",
            ));
        }
        let status = wire.status.ok_or_else(|| {
            runner_validation_error("successful Lua runner result requires integer `status`")
        })?;
        let output = wire.output.ok_or_else(|| {
            runner_validation_error("successful Lua runner result requires typed `output`")
        })?;
        match &output {
            ExecutionOutput::Value { value } => value.validate()?,
            ExecutionOutput::Values { values } => {
                if values.len() > MAX_LUA_RUNNER_STREAM_VALUES {
                    return Err(runner_limit_error(
                        "Lua runner finite stream values",
                        MAX_LUA_RUNNER_STREAM_VALUES,
                        values.len(),
                    ));
                }
                for value in values {
                    value.validate()?;
                }
            }
            ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => {
                return Err(runner_validation_error(
                    "Lua runner ABI v1 returns typed values, not inherited or byte output",
                )
                .with_help(
                    "Return `output = { kind = 'value', value = ... }` or a bounded `values` batch",
                ));
            }
        }
        ExecutionOutcome::new(
            ExecutionStatus::Exited(status),
            output,
            Vec::new(),
            ExecutionCleanupState::Complete,
        )
    } else {
        if wire.status.is_some() || wire.output.is_some() {
            return Err(runner_validation_error(
                "failed Lua runner result must contain only structured `error` data",
            ));
        }
        let error = wire.error.ok_or_else(|| {
            runner_validation_error("failed Lua runner result requires structured `error`")
        })?;
        Err(error.into_shell_error()?)
    }
}

impl LuaShellErrorWire {
    fn from_shell_error(error: &ShellError) -> Self {
        Self {
            code: error.code,
            message: error.message.clone(),
            labels: error
                .details
                .labels
                .iter()
                .map(|label| LuaErrorLabelWire {
                    source: label.source.clone(),
                    start: label.start,
                    end: label.end,
                    message: label.message.clone(),
                })
                .collect(),
            context: error.details.context.clone(),
            help: error.details.help.clone(),
            command: error.details.command.clone(),
            exit_status: error.details.exit_status,
        }
    }

    fn into_shell_error(self) -> Result<ShellError, ShellError> {
        self.validate()?;
        let mut error = ShellError::new(self.code, self.message);
        error.details.labels = self
            .labels
            .into_iter()
            .map(|label| ErrorLabel {
                source: label.source,
                start: label.start,
                end: label.end,
                message: label.message,
            })
            .collect();
        error.details.context = self.context;
        error.details.help = self.help;
        error.details.command = self.command;
        error.details.exit_status = self.exit_status;
        Ok(error)
    }

    fn validate(&self) -> Result<(), ShellError> {
        validate_runner_error_text("Lua ShellError message", &self.message)?;
        validate_runner_error_items("labels", self.labels.len())?;
        validate_runner_error_items("context", self.context.len())?;
        validate_runner_error_items("help", self.help.len())?;
        let mut total_bytes = self.message.len();
        for item in self.context.iter().chain(&self.help) {
            validate_runner_error_text("Lua ShellError detail", item)?;
            total_bytes = total_bytes.saturating_add(item.len());
        }
        if let Some(command) = &self.command {
            validate_runner_error_text("Lua ShellError command", command)?;
            total_bytes = total_bytes.saturating_add(command.len());
        }
        for label in &self.labels {
            validate_runner_error_text("Lua ShellError label message", &label.message)?;
            total_bytes = total_bytes.saturating_add(label.message.len());
            if let Some(source) = &label.source {
                if source.len() > MAX_LUA_RUNNER_ERROR_SOURCE_BYTES {
                    return Err(runner_limit_error(
                        "Lua ShellError label source bytes",
                        MAX_LUA_RUNNER_ERROR_SOURCE_BYTES,
                        source.len(),
                    ));
                }
                reject_terminal_controls("Lua ShellError label source", source)?;
                let valid_span = label.start <= label.end
                    && label.end <= source.len()
                    && source.is_char_boundary(label.start)
                    && source.is_char_boundary(label.end);
                if !valid_span {
                    return Err(runner_validation_error(
                        "Lua ShellError label is not a valid UTF-8 byte range",
                    ));
                }
                total_bytes = total_bytes.saturating_add(source.len());
            } else if label.start != 0 || label.end != 0 {
                return Err(runner_validation_error(
                    "Lua ShellError label without source must use the empty 0..0 span",
                ));
            }
        }
        if total_bytes > MAX_LUA_RUNNER_ERROR_TOTAL_BYTES {
            return Err(runner_limit_error(
                "Lua ShellError retained bytes",
                MAX_LUA_RUNNER_ERROR_TOTAL_BYTES,
                total_bytes,
            ));
        }
        Ok(())
    }
}

fn validate_runner_error_items(description: &str, observed: usize) -> Result<(), ShellError> {
    if observed > MAX_LUA_RUNNER_ERROR_ITEMS {
        return Err(runner_limit_error(
            &format!("Lua ShellError {description}"),
            MAX_LUA_RUNNER_ERROR_ITEMS,
            observed,
        ));
    }
    Ok(())
}

fn validate_runner_error_text(description: &str, value: &str) -> Result<(), ShellError> {
    validate_runner_text(description, value, MAX_LUA_RUNNER_ERROR_FIELD_BYTES)?;
    reject_terminal_controls(description, value)
}

fn legacy_runner_status(value: &serde_json::Value) -> Result<i32, ShellError> {
    let Some(status) = value.get("status") else {
        return Ok(0);
    };
    let Some(status) = status.as_i64() else {
        return Err(
            runner_validation_error("legacy Lua runner result status must be an integer")
                .with_help("Return an ABI v1 typed result or omit the legacy `status` field"),
        );
    };
    i32::try_from(status).map_err(|_| {
        runner_validation_error("legacy Lua runner result status is outside the i32 range")
    })
}

fn runner_output_to_legacy_json(output: ExecutionOutput) -> Result<serde_json::Value, ShellError> {
    match output {
        ExecutionOutput::Value { value } => Ok(value.json_value()),
        ExecutionOutput::Values { values } => Ok(serde_json::Value::Array(
            values.into_iter().map(|value| value.json_value()).collect(),
        )),
        ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => Err(runner_validation_error(
            "Lua script adapter cannot convert byte or inherited output to a typed value",
        )),
    }
}

fn runner_effect_names(effects: ExecutionEffects) -> Vec<&'static str> {
    [
        (ExecutionEffect::ReadFilesystem, "read_filesystem"),
        (ExecutionEffect::WriteFilesystem, "write_filesystem"),
        (ExecutionEffect::SpawnProcess, "spawn_process"),
        (ExecutionEffect::ChangeDirectory, "change_directory"),
    ]
    .into_iter()
    .filter_map(|(effect, name)| effects.contains(effect).then_some(name))
    .collect()
}

fn ensure_runner_active(cancelled: &Arc<AtomicBool>, stage: &str) -> Result<(), ShellError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "Lua runner execution was cancelled",
        )
        .with_context(format!("cancellation observed {stage}"))
        .with_help("Retry with a fresh shared execution cancellation handle"));
    }
    Ok(())
}

fn unsupported_runner_abi_error(version: u32) -> ShellError {
    ShellError::new(ErrorCode::Validation, "unsupported Lua runner ABI version")
        .with_context(format!(
            "requested: {version}; supported: {LUA_RUNNER_ABI_VERSION}; oldest readable: {LUA_RUNNER_OLDEST_READABLE_ABI_VERSION}"
        ))
        .with_help("Regenerate the Lua SDK and migrate the module to the supported runner ABI")
}

fn runner_boundary_error(path: &Path, message: impl Into<String>) -> ShellError {
    let message = bounded_runner_diagnostic(&message.into());
    let path = bounded_runner_diagnostic(&path.display().to_string());
    ShellError::new(ErrorCode::Validation, "Lua runner ABI validation failed")
        .with_context(message)
        .with_label(Some(path), 0, 0, "runner ABI mismatch")
        .with_help("Follow the generated `quirl.Context` and `quirl.RunnerResult` contracts")
}

fn bounded_runner_diagnostic(value: &str) -> String {
    let value = escape_terminal_controls(value);
    if value.len() <= MAX_LUA_RUNNER_ERROR_FIELD_BYTES {
        return value;
    }
    let mut end = MAX_LUA_RUNNER_ERROR_FIELD_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = value[..end].to_owned();
    bounded.push('…');
    bounded
}

fn runner_validation_error(message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Validation, "Lua runner ABI validation failed")
        .with_context(message)
        .with_help("Follow the generated typed Lua runner ABI")
}

fn runner_limit_error(description: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{description} exceeds its configured limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Reduce the retained runner data or use a bounded engine-owned stream")
}

/// Format Lua source with Quirl's deterministic, literal-aware indentation rules.
///
/// The formatter preserves shebangs, ignores keywords inside quoted strings and
/// comments, uses two-space indentation, trims trailing whitespace, and emits one
/// final newline. Sources containing Lua long brackets are conservatively left
/// unchanged except for ensuring that final newline.
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

/// Format one bounded Lua file and report whether its contents differed.
///
/// With `check` set, no write occurs. Otherwise changed content uses the shared
/// crash-safe replacement transaction: links and special files fail closed,
/// permissions are preserved, concurrent changes observed before commit are
/// rejected, and the original remains recoverable across every durability
/// stage. Inputs and formatted output larger than [`MAX_LUA_SOURCE_BYTES`] fail
/// with a resource-limit error.
pub fn format_file(path: &Path, check: bool) -> Result<bool, ShellError> {
    let source = read_source_bounded(path)?;
    let formatted = format_source(&source);
    let changed = source != formatted;
    if changed && !check {
        replace_file_atomically(
            path,
            source.as_bytes(),
            formatted.as_bytes(),
            AtomicReplaceOptions {
                bytes_max: MAX_LUA_SOURCE_BYTES,
            },
        )?;
    }
    Ok(changed)
}

/// Generate deterministic LuaLS annotations and stubs from [`HOST_API`].
///
/// The returned source is the canonical editor SDK checked into `docs/quirl.lua`;
/// callers should regenerate it rather than hand-editing that artifact.
pub fn sdk_lua() -> String {
    let mut output = String::from(
        "---@meta quirl\n\n---@class quirl.ErrorLabel\n---@field source? string\n---@field start integer Inclusive UTF-8 byte offset.\n---@field end integer Exclusive UTF-8 byte offset.\n---@field message string\n\n---@alias quirl.ErrorCode 'invalid_command'|'invalid_argument'|'data'|'io'|'process_spawn'|'script_read'|'lua'|'validation'|'resource_limit'\n\n---@class quirl.ShellError\n---@field code quirl.ErrorCode\n---@field message string\n---@field labels? quirl.ErrorLabel[]\n---@field context? string[]\n---@field help? string[]\n---@field command? string\n---@field exit_status? integer\n\n---@class quirl.Result\n---@field ok boolean\n---@field value? any\n---@field error? quirl.ShellError\n\n---@class quirl.ProcessResult: quirl.Result\n---@field status integer\n---@field value string Captured stdout.\n---@field stderr string Captured stderr.\n\n---@alias quirl.ExecutionEffect 'read_filesystem'|'write_filesystem'|'spawn_process'|'change_directory'\n\n---@class quirl.CancellationContext\n---@field is_cancelled fun(): boolean Returns the shared cancellation flag without clearing it.\n\n---@class quirl.Context\n---@field abi_version 1\n---@field args string[] Bounded arguments in source order.\n---@field env table<string, string> Immutable bounded environment snapshot.\n---@field cwd string UTF-8 working directory captured before evaluation.\n---@field input table Shared deny-unknown ExecutionInput representation.\n---@field output table Shared value-only ExecutionOutputTarget representation.\n---@field cancellation quirl.CancellationContext\n---@field effects quirl.ExecutionEffect[] Effects declared before dispatch.\n\n---@class quirl.RunnerResult\n---@field abi_version 1\n---@field ok boolean\n---@field status? integer Required exactly when ok is true.\n---@field output? table Typed value or bounded finite values output; live streams are not transferable.\n---@field error? quirl.ShellError Required exactly when ok is false.\n\n---@class quirl.RunnerModule\n---@field abi_version 1\n---@field main fun(context: quirl.Context): quirl.RunnerResult\n\n---@alias quirl.PromptSymbols 'auto'|'plain'|'unicode'|'nerd_font'\n---@alias quirl.WelcomeBanner 'full'|'compact'|'none'\n---@alias quirl.Surface 'auto'|'rich'|'simple'\n\n---@class quirl.EditorConfig\n---@field keymap? 'emacs'|'vim'|'helix' Emacs is the complete default.\n---@field semantic_hints? boolean\n---@field banner? quirl.WelcomeBanner\n\n---@class quirl.PickerConfig\n---@field layout? 'adaptive'|'bottom'|'full'\n---@field preview? boolean\n\n---@class quirl.PromptConfig\n---@field symbols? quirl.PromptSymbols Auto never assumes a patched font; nerd_font enables Powerline glyphs explicitly.\n---@field left? string[] Ordered prompt segments before the input.\n---@field right? string[] Ordered prompt segments aligned on the right.\n---@field transient? boolean Collapse accepted input to one scrollback line before execution.\n\n---@class quirl.ThemeColors\n---@field accent_command string #RRGGBB color for command-mode accents.\n---@field accent_data string #RRGGBB color for data-mode accents.\n---@field context_primary string #RRGGBB color for primary context.\n---@field context_secondary string #RRGGBB color for secondary context.\n---@field muted string #RRGGBB color for subdued text.\n---@field border string #RRGGBB color for borders.\n---@field status_background string #RRGGBB status background color.\n---@field error string #RRGGBB error color.\n---@field warning string #RRGGBB color for warnings.\n---@field hint string #RRGGBB color for hints.\n---@field string string #RRGGBB string syntax color.\n---@field operator string #RRGGBB operator syntax color.\n---@field expansion string #RRGGBB expansion syntax color.\n---@field number string #RRGGBB number syntax color.\n\n---@class quirl.StatuslineConfig\n---@field hints? boolean\n\n---@class quirl.UiConfig\n---@field surface? quirl.Surface\n---@field theme? string Built-in or custom theme name; defaults to tokyo-night.\n---@field themes? table<string, quirl.ThemeColors> At most 32 custom themes.\n---@field statusline? quirl.StatuslineConfig\n\n---@class quirl.CompletionConfig\n---@field auto? boolean\n---@field min_chars? integer\n\n---@class quirl.Config\n---@field schema_version? integer\n---@field editor? quirl.EditorConfig\n---@field picker? quirl.PickerConfig\n---@field prompt? quirl.PromptConfig\n---@field ui? quirl.UiConfig\n---@field completion? quirl.CompletionConfig\n\n---@class quirl.PromptSegment\n---@field name string\n---@field deadline_ms? integer\n---@field render fun(context: table): string?\n\n---@class quirl.CompletionProvider\n---@field command string\n---@field complete fun(context: table): table\n\n---@class quirl.PluginCommand\n---@field name string\n---@field signature string\n---@field summary string\n---@field details string\n---@field input_type string\n---@field output_type string\n---@field examples string[]\n---@field effects string[]\n---@field error_codes table<string, string>\n---@field run fun(arguments: table): any\n\n---@alias quirl.EventKind 'session_start'|'session_restore'|'directory_changed'|'command_plan'|'execution_progress'|'output'|'cancellation'|'result'|'error'\n---@alias quirl.ExtensionCapability 'events_observe'|'plan_rewrite'|'environment_mutate'|'output_read'|'execution_block'|'catalog_contribute'|'completion_contribute'|'ui_panel'\n---@class quirl.EventSubscription\n---@field name string\n---@field events quirl.EventKind[]\n---@field capabilities quirl.ExtensionCapability[]\n---@field deadline_ms integer\n---@field observe fun(event: table): table[]\n\n---@alias quirl.ContributionKind 'catalog'|'completion'|'panel'\n---@class quirl.Contribution\n---@field kind quirl.ContributionKind\n---@field name string\n---@field deadline_ms integer\n---@field plain_fallback? string\n---@field provide fun(context: table): any\n\nquirl = {}\n\n",
    );
    output = output.replace(
        "---@field run fun(arguments: table): any",
        "---@field run fun(context: quirl.Context): quirl.RunnerResult",
    );
    output = output.replace(
        "---@class quirl.PluginCommand",
        "---@alias quirl.PluginInputType 'Nothing'|'Bool'|'Int'|'UInt'|'Decimal'|'String'|'List'|'Record'|'Path'|'Duration'|'Size'|'DateTime'|'Pattern'\n---@alias quirl.PluginOutputType quirl.PluginInputType|'Values<Nothing>'|'Values<Bool>'|'Values<Int>'|'Values<UInt>'|'Values<Decimal>'|'Values<String>'|'Values<List>'|'Values<Record>'|'Values<Path>'|'Values<Duration>'|'Values<Size>'|'Values<DateTime>'|'Values<Pattern>'\n\n---@class quirl.PluginCommand",
    );
    output = output.replace(
        "---@field input_type string\n---@field output_type string",
        "---@field input_type quirl.PluginInputType Exact top-level input kind; Nothing accepts no input.\n---@field output_type quirl.PluginOutputType Exact value kind or bounded finite Values<T>; live streams are unsupported.",
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

/// Serialize [`HOST_API`] as the stable, versioned JSON document used by agents.
pub fn sdk_json() -> Result<String, ShellError> {
    #[derive(Serialize)]
    struct HostApiDocument<'a> {
        document_type: &'static str,
        schema_version: u32,
        module: &'static str,
        module_version: &'static str,
        runner_abi_version: u32,
        runner_abi_hash: String,
        runner_abi_descriptor: &'static str,
        functions: &'a [HostApiSpec],
    }
    let document = HostApiDocument {
        document_type: "quirl.host_api",
        schema_version: 2,
        module: "quirl",
        module_version: env!("CARGO_PKG_VERSION"),
        runner_abi_version: LUA_RUNNER_ABI_VERSION,
        runner_abi_hash: lua_runner_abi_hash(),
        runner_abi_descriptor: LUA_RUNNER_ABI_DESCRIPTOR,
        functions: HOST_API,
    };
    serde_json::to_string_pretty(&document).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not serialize the Lua SDK")
            .with_context(error.to_string())
    })
}

/// Render human-readable host API signatures, parameter types, and capabilities.
///
/// All function facts are projected directly from [`HOST_API`] in table order.
pub fn sdk_markdown() -> String {
    let mut output = format!(
        "# Quirl Lua SDK\n\nModule: `quirl`\n\nVersion: `{}`\n\nSchema version: `2`\n\nRunner ABI: `{}`\n\nRunner ABI hash: `{}`\n\n",
        env!("CARGO_PKG_VERSION"),
        LUA_RUNNER_ABI_VERSION,
        lua_runner_abi_hash(),
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

fn install_restrictions(lua: &Lua, budget: Arc<Mutex<Budget>>) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in [
        "debug", "dofile", "io", "load", "loadfile", "os", "package", "print", "require", "warn",
    ] {
        globals.set(name, Value::Nil)?;
    }
    let string = globals.get::<Table>("string")?;
    // Lua's pattern matcher runs entirely in C and adversarial patterns can take
    // exponential time without invoking the instruction hook. Literal search is
    // the useful bounded subset that the existing library exposes explicitly.
    for name in ["gmatch", "gsub", "match"] {
        let pattern_budget = Arc::clone(&budget);
        string.set(
            name,
            lua.create_function(move |_, _: MultiValue| {
                let error = lua_resource_error(
                    "wall_time",
                    format!(
                        "string.{name} uses native pattern matching that cannot observe the wall deadline"
                    ),
                    "Use bounded literal string.find(..., true) or perform typed matching in Rust",
                );
                Err::<MultiValue, _>(terminate_shared_budget(&pattern_budget, error)?)
            })?,
        )?;
    }
    let original_find = string.get::<Function>("find")?;
    let find_budget = Arc::clone(&budget);
    string.set(
        "find",
        lua.create_function(move |_, arguments: MultiValue| {
            let plain = arguments.get(3).is_some_and(|value| value == &Value::Boolean(true));
            if !plain {
                let error = lua_resource_error(
                    "wall_time",
                    "string.find pattern matching cannot observe the wall deadline",
                    "Pass true as string.find's fourth argument to request bounded literal matching",
                );
                return Err(terminate_shared_budget(&find_budget, error)?);
            }
            original_find.call::<MultiValue>(arguments)
        })?,
    )?;
    Ok(())
}

fn install_protected_call_guards(lua: &Lua, budget: Arc<Mutex<Budget>>) -> mlua::Result<()> {
    let globals = lua.globals();
    // Preserve protected calls for ordinary guest errors, but inspect the Rust-owned
    // terminal state before returning. Each nested guard therefore propagates policy
    // termination outward instead of converting it into a resumable Lua result.
    for name in ["pcall", "xpcall"] {
        let protected_call = globals.get::<Function>(name)?;
        let protected_budget = Arc::clone(&budget);
        globals.set(
            name,
            lua.create_function(move |_, arguments: MultiValue| {
                let result = protected_call.call::<MultiValue>(arguments)?;
                ensure_budget_not_terminated(&protected_budget)?;
                Ok(result)
            })?,
        )?;
    }
    Ok(())
}

fn ensure_budget_not_terminated(budget: &Arc<Mutex<Budget>>) -> mlua::Result<()> {
    let budget = budget
        .lock()
        .map_err(|_| mlua::Error::RuntimeError("quirl budget state is unavailable".to_owned()))?;
    match &budget.termination {
        Some(error) => Err(mlua::Error::external(error.clone())),
        None => Ok(()),
    }
}

fn terminate_budget(budget: &mut Budget, error: ShellError) -> mlua::Error {
    let error = budget.termination.get_or_insert(error).clone();
    mlua::Error::external(error)
}

fn terminate_shared_budget(
    budget: &Arc<Mutex<Budget>>,
    error: ShellError,
) -> mlua::Result<mlua::Error> {
    let mut budget = budget
        .lock()
        .map_err(|_| mlua::Error::RuntimeError("quirl budget state is unavailable".to_owned()))?;
    Ok(terminate_budget(&mut budget, error))
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
            if let Some(error) = &budget.termination {
                return Err(mlua::Error::external(error.clone()));
            }
            if cancelled.load(Ordering::Relaxed) {
                let error = lua_resource_error(
                    "cancellation",
                    "cancellation flag: set",
                    "Clear cancellation only before deliberately reusing the runtime",
                );
                return Err(terminate_budget(&mut budget, error));
            }
            if budget.remaining_instructions < HOOK_GRANULARITY {
                let observed = budget
                    .instruction_limit
                    .saturating_sub(budget.remaining_instructions);
                let error = lua_resource_error(
                    "instruction",
                    format!(
                        "instructions observed: approximately {observed}; limit: {}",
                        budget.instruction_limit
                    ),
                    "Reduce Lua work or raise instruction_limit after review",
                );
                return Err(terminate_budget(&mut budget, error));
            }
            if Instant::now() > budget.deadline {
                let observed = budget.started.elapsed().as_millis();
                let error = lua_resource_error(
                    "wall_time",
                    format!(
                        "elapsed: {observed} ms; limit: {} ms",
                        budget.wall_time.as_millis()
                    ),
                    "Reduce callback work or raise wall_time after review",
                );
                return Err(terminate_budget(&mut budget, error));
            }
            budget.remaining_instructions -= HOOK_GRANULARITY;
            Ok(VmState::Continue)
        },
    )
}

fn install_host_api(
    lua: &Lua,
    policy: LuaPolicy,
    granted_capabilities: HashSet<String>,
    process_host: Option<ProcessHost>,
    execution: HostExecutionState,
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
    let process_grants = granted_capabilities.clone();
    let process_budget = Arc::clone(&execution.budget);
    let process_cancelled = Arc::clone(&execution.cancelled);
    process.set(
        "run",
        lua.create_function(move |lua, command: String| {
            let capability = host_api_capability("quirl.process.run")?;
            if !policy.allow_process
                || !process_capability_granted(&process_grants, capability, &command)
            {
                return Err(mlua::Error::RuntimeError(
                    format!("capability denied: {capability}"),
                ));
            }
            let Some(process_host) = process_host.as_ref() else {
                return Err(mlua::Error::RuntimeError(
                    "process host is unavailable; run Lua through the Quirl CLI or configure a process host"
                        .to_owned(),
                ));
            };
            if process_cancelled.load(Ordering::Relaxed) {
                let error = lua_resource_error(
                    "cancellation",
                    "cancellation flag: set",
                    "Clear cancellation only before deliberately reusing the runtime",
                );
                return Err(terminate_shared_budget(&process_budget, error)?);
            }
            let deadline = process_budget
                .lock()
                .map_err(|_| {
                    mlua::Error::RuntimeError("quirl budget state is unavailable".to_owned())
                })?
                .deadline
                .saturating_duration_since(Instant::now());
            if deadline.is_zero() {
                let error = lua_resource_error(
                    "wall_time",
                    "remaining wall time: 0 ms",
                    "Reduce host-call work or raise wall_time after review",
                );
                return Err(terminate_shared_budget(&process_budget, error)?);
            }
            let outcome = match process_host(ProcessRequest {
                command,
                // The budget is reset to each callback's declared deadline before invoking Lua.
                // A host call must consume the same remaining budget, rather than giving a short
                // callback another full policy-sized process window.
                deadline,
                cancelled: Arc::clone(&process_cancelled),
                max_output_bytes: MAX_PROCESS_OUTPUT_BYTES,
            }) {
                Ok(outcome) => outcome,
                Err(error) if error.code == ErrorCode::ResourceLimit => {
                    return Err(terminate_shared_budget(&process_budget, error)?);
                }
                Err(error) => return Err(mlua::Error::external(error)),
            };
            let result = lua.create_table()?;
            let ok = outcome.status == 0;
            result.set("ok", ok)?;
            result.set("status", outcome.status)?;
            result.set("value", outcome.stdout.unwrap_or_default())?;
            result.set("stderr", outcome.stderr.unwrap_or_default())?;
            if !ok {
                let mut error = ShellError::new(
                    ErrorCode::InvalidCommand,
                    "process invoked from Lua exited with a non-zero status",
                )
                .with_context(format!("exit status: {}", outcome.status))
                .with_help("Inspect `stderr` and handle the status explicitly");
                error.details.exit_status = Some(outcome.status);
                let error = LuaShellErrorWire::from_shell_error(&error);
                error.validate().map_err(mlua::Error::external)?;
                result.set("error", lua.to_value(&error)?)?;
            }
            Ok(result)
        })?,
    )?;
    quirl.set("process", process)?;

    let prompt = lua.create_table()?;
    let prompt_grants = granted_capabilities.clone();
    let prompt_registrations = Arc::clone(&registrations);
    let prompt_callbacks = Arc::clone(&callbacks);
    prompt.set(
        "add_segment",
        lua.create_function(move |lua, spec: Table| {
            require_host_api_grant(&prompt_grants, "quirl.prompt.add_segment")?;
            let render = spec.get::<Function>("render").map_err(|_| {
                mlua::Error::external(registration_validation_error(
                    "prompt segment `render` must be a function",
                ))
            })?;
            let registration: PromptRegistration =
                deserialize_registration(lua, &spec, "render", "prompt segment")?;
            validate_prompt_registration(&registration).map_err(mlua::Error::external)?;
            let mut callbacks = prompt_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            if callbacks.prompt_segments.contains_key(&registration.name) {
                return Err(mlua::Error::external(registration_validation_error(
                    format!("duplicate prompt segment `{}`", registration.name),
                )));
            }
            let mut registrations = prompt_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            validate_registration_addition(
                "prompt segments",
                registrations.prompt_segments.len(),
                registrations
                    .prompt_segments
                    .iter()
                    .map(prompt_registration_bytes)
                    .sum(),
                prompt_registration_bytes(&registration),
                MAX_PLUGIN_PROMPT_SEGMENTS,
                MAX_PLUGIN_PROMPT_BYTES,
            )
            .map_err(mlua::Error::external)?;
            let callback = lua.create_registry_value(render)?;
            registrations.prompt_segments.push(registration.clone());
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
    let completion_grants = granted_capabilities.clone();
    let completion_registrations = Arc::clone(&registrations);
    let completion_callbacks = Arc::clone(&callbacks);
    completion.set(
        "add_provider",
        lua.create_function(move |lua, spec: Table| {
            require_host_api_grant(&completion_grants, "quirl.completion.add_provider")?;
            let complete = spec.get::<Function>("complete").map_err(|_| {
                mlua::Error::external(registration_validation_error(
                    "completion provider `complete` must be a function",
                ))
            })?;
            let registration: CompletionRegistration =
                deserialize_registration(lua, &spec, "complete", "completion provider")?;
            validate_completion_registration(&registration).map_err(mlua::Error::external)?;
            let mut callbacks = completion_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            if callbacks
                .completion_providers
                .contains_key(&registration.command)
            {
                return Err(mlua::Error::external(registration_validation_error(
                    format!("duplicate completion provider `{}`", registration.command),
                )));
            }
            let mut registrations = completion_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            validate_registration_addition(
                "completion providers",
                registrations.completion_providers.len(),
                registrations
                    .completion_providers
                    .iter()
                    .map(completion_registration_bytes)
                    .sum(),
                completion_registration_bytes(&registration),
                MAX_PLUGIN_COMPLETION_PROVIDERS,
                MAX_PLUGIN_COMPLETION_BYTES,
            )
            .map_err(mlua::Error::external)?;
            let callback = lua.create_registry_value(complete)?;
            registrations
                .completion_providers
                .push(registration.clone());
            callbacks
                .completion_providers
                .insert(registration.command, callback);
            Ok(())
        })?,
    )?;
    quirl.set("completion", completion)?;

    let plugin = lua.create_table()?;
    let command_grants = granted_capabilities.clone();
    let command_registrations = Arc::clone(&registrations);
    let command_callbacks = Arc::clone(&callbacks);
    plugin.set(
        "command",
        lua.create_function(move |lua, spec: Table| {
            require_host_api_grant(&command_grants, "quirl.plugin.command")?;
            let run = spec.get::<Function>("run").map_err(|_| {
                mlua::Error::external(registration_validation_error(
                    "plugin command `run` must be a function",
                ))
            })?;
            let registration: CommandRegistration =
                deserialize_registration(lua, &spec, "run", "plugin command")?;
            validate_command_registration(&registration).map_err(mlua::Error::external)?;
            let mut callbacks = command_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            if callbacks.commands.contains_key(&registration.name) {
                return Err(mlua::Error::external(registration_validation_error(
                    format!("duplicate plugin command `{}`", registration.name),
                )));
            }
            let mut registrations = command_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            validate_registration_addition(
                "plugin commands",
                registrations.commands.len(),
                registrations
                    .commands
                    .iter()
                    .map(command_registration_bytes)
                    .sum(),
                command_registration_bytes(&registration),
                MAX_PLUGIN_COMMANDS,
                MAX_PLUGIN_COMMAND_BYTES,
            )
            .map_err(mlua::Error::external)?;
            let callback = lua.create_registry_value(run)?;
            registrations.commands.push(registration.clone());
            callbacks.commands.insert(registration.name, callback);
            Ok(())
        })?,
    )?;
    quirl.set("plugin", plugin)?;

    let events = lua.create_table()?;
    let event_grants = granted_capabilities.clone();
    let event_registrations = Arc::clone(&registrations);
    let event_callbacks = Arc::clone(&callbacks);
    events.set(
        "subscribe",
        lua.create_function(move |lua, spec: Table| {
            require_host_api_grant(&event_grants, "quirl.events.subscribe")?;
            let observe = spec.get::<Function>("observe").map_err(|_| {
                mlua::Error::external(registration_validation_error(
                    "event subscription `observe` must be a function",
                ))
            })?;
            let registration: EventSubscription =
                deserialize_registration(lua, &spec, "observe", "event subscription")?;
            validate_event_registration(&registration).map_err(mlua::Error::external)?;
            for capability in &registration.capabilities {
                require_grant(&event_grants, extension_capability_grant(*capability))?;
            }
            let mut callbacks = event_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            if callbacks.events.contains_key(&registration.name) {
                return Err(mlua::Error::external(registration_validation_error(
                    format!("duplicate event handler `{}`", registration.name),
                )));
            }
            let mut registrations = event_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            validate_registration_addition(
                "event handlers",
                registrations.events.len(),
                registrations
                    .events
                    .iter()
                    .map(event_registration_bytes)
                    .sum(),
                event_registration_bytes(&registration),
                MAX_PLUGIN_EVENT_HANDLERS,
                MAX_PLUGIN_EVENT_BYTES,
            )
            .map_err(mlua::Error::external)?;
            let callback = lua.create_registry_value(observe)?;
            registrations.events.push(registration.clone());
            callbacks.events.insert(
                registration.name,
                EventCallback {
                    function: callback,
                    events: registration.events,
                    capabilities: registration.capabilities,
                    deadline: Duration::from_millis(registration.deadline_ms),
                },
            );
            Ok(())
        })?,
    )?;
    quirl.set("events", events)?;

    let extension = lua.create_table()?;
    let contribution_grants = granted_capabilities;
    let contribution_registrations = registrations;
    let contribution_callbacks = callbacks;
    extension.set(
        "contribute",
        lua.create_function(move |lua, spec: Table| {
            require_host_api_grant(&contribution_grants, "quirl.extension.contribute")?;
            let provide = spec.get::<Function>("provide").map_err(|_| {
                mlua::Error::external(registration_validation_error(
                    "extension contribution `provide` must be a function",
                ))
            })?;
            let registration: ContributionRegistration =
                deserialize_registration(lua, &spec, "provide", "extension contribution")?;
            validate_contribution_registration(&registration).map_err(mlua::Error::external)?;
            require_grant(
                &contribution_grants,
                contribution_capability_grant(registration.kind),
            )?;
            let key = format!("{:?}:{}", registration.kind, registration.name);
            let mut callbacks = contribution_callbacks
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            if callbacks.contributions.contains_key(&key) {
                return Err(mlua::Error::external(registration_validation_error(
                    format!(
                        "duplicate {:?} contribution `{}`",
                        registration.kind, registration.name
                    ),
                )));
            }
            let mut registrations = contribution_registrations
                .lock()
                .map_err(|_| mlua::Error::RuntimeError("plugin state unavailable".to_owned()))?;
            let added_bytes = contribution_registration_bytes(&registration);
            validate_registration_addition(
                "contributions",
                registrations.contributions.len(),
                registrations
                    .contributions
                    .iter()
                    .map(contribution_registration_bytes)
                    .sum(),
                added_bytes,
                MAX_PLUGIN_CONTRIBUTIONS,
                MAX_PLUGIN_CONTRIBUTION_BYTES,
            )
            .map_err(mlua::Error::external)?;
            if registration.kind == ContributionKind::Panel {
                let panels = registrations
                    .contributions
                    .iter()
                    .filter(|item| item.kind == ContributionKind::Panel);
                validate_registration_addition(
                    "panel contributions",
                    panels.clone().count(),
                    panels.map(contribution_registration_bytes).sum(),
                    added_bytes,
                    MAX_PLUGIN_PANELS,
                    MAX_PLUGIN_PANEL_BYTES,
                )
                .map_err(mlua::Error::external)?;
            }
            let callback = lua.create_registry_value(provide)?;
            registrations.contributions.push(registration.clone());
            callbacks.contributions.insert(
                key,
                ContributionCallback {
                    function: callback,
                    deadline: Duration::from_millis(registration.deadline_ms),
                },
            );
            Ok(())
        })?,
    )?;
    quirl.set("extension", extension)?;
    lua.globals().set("quirl", quirl)
}

fn require_grant(grants: &HashSet<String>, capability: &str) -> mlua::Result<()> {
    if grants.contains(capability) {
        Ok(())
    } else {
        Err(mlua::Error::external(
            ShellError::new(
                ErrorCode::Validation,
                format!("capability denied: {capability}"),
            )
            .with_context(format!("capability denied: {capability}"))
            .with_context("lua failure: registration")
            .with_help("Grant only the capability approved by the plugin policy"),
        ))
    }
}

fn host_api_capability(path: &str) -> mlua::Result<&'static str> {
    HOST_API
        .iter()
        .find(|spec| spec.path == path)
        .and_then(|spec| spec.capability)
        .ok_or_else(|| {
            mlua::Error::RuntimeError(format!(
                "internal host API declaration for `{path}` has no capability"
            ))
        })
}

fn require_host_api_grant(grants: &HashSet<String>, path: &str) -> mlua::Result<()> {
    require_grant(grants, host_api_capability(path)?)
}

fn extension_capability_grant(capability: ExtensionCapability) -> &'static str {
    match capability {
        ExtensionCapability::EventsObserve => "events.observe",
        ExtensionCapability::PlanRewrite => "plan.rewrite",
        ExtensionCapability::EnvironmentMutate => "environment.mutate",
        ExtensionCapability::OutputRead => "output.read",
        ExtensionCapability::ExecutionBlock => "execution.block",
        ExtensionCapability::CatalogContribute => "catalog.register",
        ExtensionCapability::CompletionContribute => "completion.register",
        ExtensionCapability::UiPanel => "ui.panel",
    }
}

fn contribution_capability_grant(kind: ContributionKind) -> &'static str {
    match kind {
        ContributionKind::Catalog => "catalog.register",
        ContributionKind::Completion => "completion.register",
        ContributionKind::Panel => "ui.panel",
    }
}

fn parse_event_kind(value: &str) -> Option<EventKind> {
    match value {
        "session_start" | "session.start" => Some(EventKind::SessionStart),
        "session_restore" | "session.restore" => Some(EventKind::SessionRestore),
        "directory_changed" | "directory.changed" => Some(EventKind::DirectoryChanged),
        "command_plan" | "command.plan" => Some(EventKind::CommandPlan),
        "execution_progress" | "execution.progress" => Some(EventKind::ExecutionProgress),
        "output" | "execution.output" => Some(EventKind::Output),
        "cancellation" | "execution.cancellation" => Some(EventKind::Cancellation),
        "result" | "execution.result" => Some(EventKind::Result),
        "error" | "execution.error" => Some(EventKind::Error),
        _ => None,
    }
}

fn process_capability_granted(grants: &HashSet<String>, capability: &str, command: &str) -> bool {
    if grants.contains(capability) {
        return true;
    }
    // Scoped grants describe exactly one executable invocation. The injected
    // native host accepts command source, so allow only one physical line and
    // a deliberately small argv alphabet; tabs/newlines and shell operators
    // must never reach it.
    if command.is_empty()
        || command.trim() != command
        || command.chars().any(|character| character.is_control())
    {
        return false;
    }
    if !command.chars().all(|character| {
        character.is_ascii_alphanumeric() || character == ' ' || "-_./:=,@%+".contains(character)
    }) {
        return false;
    }
    let executable = command.split_whitespace().next().unwrap_or_default();
    grants.contains(&format!("{capability}:{executable}"))
}

fn validate_prompt_registration(registration: &PromptRegistration) -> Result<(), ShellError> {
    validate_registration_name("prompt segment name", &registration.name)?;
    if !(1..=MAX_CALLBACK_DEADLINE_MS).contains(&registration.deadline_ms) {
        return Err(registration_validation_error(format!(
            "prompt segment `deadline_ms` must be between 1 and {MAX_CALLBACK_DEADLINE_MS}"
        )));
    }
    Ok(())
}

fn validate_completion_registration(
    registration: &CompletionRegistration,
) -> Result<(), ShellError> {
    validate_registration_name("completion provider command", &registration.command)
}

fn validate_command_registration(registration: &CommandRegistration) -> Result<(), ShellError> {
    validate_bounded_text(
        "plugin command name",
        &registration.name,
        MAX_REGISTRATION_NAME_BYTES,
    )?;
    for (field, value, limit) in [
        (
            "signature",
            registration.signature.as_str(),
            MAX_REGISTRATION_DESCRIPTION_BYTES,
        ),
        (
            "summary",
            registration.summary.as_str(),
            MAX_REGISTRATION_DESCRIPTION_BYTES,
        ),
        (
            "details",
            registration.details.as_str(),
            MAX_COMMAND_DETAILS_BYTES,
        ),
        (
            "input_type",
            registration.input_type.as_str(),
            MAX_COMMAND_TYPE_BYTES,
        ),
        (
            "output_type",
            registration.output_type.as_str(),
            MAX_COMMAND_TYPE_BYTES,
        ),
    ] {
        validate_bounded_text(&format!("plugin command {field}"), value, limit)?;
    }
    if ValueInputContract::parse_exact(&registration.input_type).is_none() {
        return Err(registration_validation_error(format!(
            "plugin command input_type `{}` is unsupported; use `Nothing` or one exact structured value kind",
            registration.input_type
        )));
    }
    if ValueOutputContract::parse_exact(&registration.output_type).is_none() {
        return Err(registration_validation_error(format!(
            "plugin command output_type `{}` is unsupported; use one exact value kind or bounded `Values<T>`, never `Stream<T>`",
            registration.output_type
        )));
    }
    validate_string_collection(
        "plugin command examples",
        &registration.examples,
        MAX_COMMAND_EXAMPLES,
        MAX_COMMAND_EXAMPLE_BYTES,
    )?;
    validate_string_collection(
        "plugin command effects",
        &registration.effects,
        MAX_COMMAND_EFFECTS,
        MAX_COMMAND_EFFECT_BYTES,
    )?;
    if registration.error_codes.is_empty() {
        return Err(registration_validation_error(
            "plugin command error_codes must not be empty",
        ));
    }
    if registration.error_codes.len() > MAX_COMMAND_ERROR_CODES {
        return Err(registration_limit_error(
            "plugin command error codes",
            "entries",
            registration.error_codes.len(),
            MAX_COMMAND_ERROR_CODES,
        ));
    }
    for (code, description) in &registration.error_codes {
        validate_bounded_text(
            "plugin command error code",
            code,
            MAX_COMMAND_ERROR_CODE_BYTES,
        )?;
        validate_bounded_text(
            "plugin command error description",
            description,
            MAX_COMMAND_ERROR_DESCRIPTION_BYTES,
        )?;
    }
    Ok(())
}

fn validate_event_registration(registration: &EventSubscription) -> Result<(), ShellError> {
    validate_registration_name("event handler name", &registration.name)?;
    registration
        .validate()
        .map_err(|error| registration_validation_error(error.message))?;
    if registration.events.len() > EventKind::ALL.len() {
        return Err(registration_limit_error(
            "event names",
            "entries",
            registration.events.len(),
            EventKind::ALL.len(),
        ));
    }
    if registration.capabilities.len() > ExtensionCapability::ALL.len() {
        return Err(registration_limit_error(
            "event capabilities",
            "entries",
            registration.capabilities.len(),
            ExtensionCapability::ALL.len(),
        ));
    }
    let unique = registration
        .capabilities
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    if unique.len() != registration.capabilities.len() {
        return Err(registration_validation_error(
            "event handler contains duplicate capabilities",
        ));
    }
    Ok(())
}

fn validate_contribution_registration(
    registration: &ContributionRegistration,
) -> Result<(), ShellError> {
    validate_registration_name("contribution name", &registration.name)?;
    if let Some(fallback) = &registration.plain_fallback {
        validate_bounded_text("panel plain_fallback", fallback, MAX_PANEL_FALLBACK_BYTES)?;
    }
    registration
        .validate()
        .map_err(|error| registration_validation_error(error.message))
}

fn validate_string_collection(
    description: &str,
    values: &[String],
    count_max: usize,
    item_bytes_max: usize,
) -> Result<(), ShellError> {
    if values.is_empty() {
        return Err(registration_validation_error(format!(
            "{description} must not be empty"
        )));
    }
    if values.len() > count_max {
        return Err(registration_limit_error(
            description,
            "entries",
            values.len(),
            count_max,
        ));
    }
    for value in values {
        validate_bounded_text(description, value, item_bytes_max)?;
    }
    Ok(())
}

fn validate_bounded_text(
    description: &str,
    value: &str,
    bytes_max: usize,
) -> Result<(), ShellError> {
    if value.trim().is_empty() {
        return Err(registration_validation_error(format!(
            "{description} must not be empty"
        )));
    }
    if value.len() > bytes_max {
        return Err(registration_limit_error(
            description,
            "bytes",
            value.len(),
            bytes_max,
        ));
    }
    reject_terminal_controls(description, value)
        .map_err(|error| registration_validation_error(error.message))
}

fn validate_registration_name(description: &str, value: &str) -> Result<(), ShellError> {
    validate_bounded_text(description, value, MAX_REGISTRATION_NAME_BYTES)
}

fn validate_registration_collection(
    description: &str,
    count: usize,
    mut retained_sizes: impl Iterator<Item = usize>,
    count_max: usize,
    retained_bytes_max: usize,
) -> Result<(), ShellError> {
    if count > count_max {
        return Err(registration_limit_error(
            description,
            "registrations",
            count,
            count_max,
        ));
    }
    let retained_bytes = retained_sizes.try_fold(0_usize, |total, size| {
        total.checked_add(size).ok_or_else(|| {
            registration_limit_error(description, "bytes", usize::MAX, retained_bytes_max)
        })
    })?;
    if retained_bytes > retained_bytes_max {
        return Err(registration_limit_error(
            description,
            "bytes",
            retained_bytes,
            retained_bytes_max,
        ));
    }
    Ok(())
}

fn validate_registration_addition(
    description: &str,
    current_count: usize,
    current_retained_bytes: usize,
    added_retained_bytes: usize,
    count_max: usize,
    retained_bytes_max: usize,
) -> Result<(), ShellError> {
    let observed_count = current_count.checked_add(1).ok_or_else(|| {
        registration_limit_error(description, "registrations", usize::MAX, count_max)
    })?;
    if observed_count > count_max {
        return Err(registration_limit_error(
            description,
            "registrations",
            observed_count,
            count_max,
        ));
    }
    let observed_bytes = current_retained_bytes
        .checked_add(added_retained_bytes)
        .ok_or_else(|| {
            registration_limit_error(description, "bytes", usize::MAX, retained_bytes_max)
        })?;
    if observed_bytes > retained_bytes_max {
        return Err(registration_limit_error(
            description,
            "bytes",
            observed_bytes,
            retained_bytes_max,
        ));
    }
    Ok(())
}

fn validate_unique_names<'a>(
    description: &str,
    names: impl Iterator<Item = &'a str>,
) -> Result<(), ShellError> {
    let mut unique = HashSet::new();
    for name in names {
        if !unique.insert(name) {
            return Err(registration_validation_error(format!(
                "duplicate {description} `{name}`"
            )));
        }
    }
    Ok(())
}

fn prompt_registration_bytes(registration: &PromptRegistration) -> usize {
    registration.name.len()
}

fn completion_registration_bytes(registration: &CompletionRegistration) -> usize {
    registration.command.len()
}

fn command_registration_bytes(registration: &CommandRegistration) -> usize {
    let scalar_bytes = registration.name.len()
        + registration.signature.len()
        + registration.summary.len()
        + registration.details.len()
        + registration.input_type.len()
        + registration.output_type.len();
    let list_bytes = registration
        .examples
        .iter()
        .chain(&registration.effects)
        .map(String::len)
        .sum::<usize>();
    let map_bytes = registration
        .error_codes
        .iter()
        .map(|(key, value)| key.len() + value.len())
        .sum::<usize>();
    scalar_bytes
        .checked_add(list_bytes)
        .and_then(|total| total.checked_add(map_bytes))
        .unwrap_or(usize::MAX)
}

fn event_registration_bytes(registration: &EventSubscription) -> usize {
    registration.name.len()
        + registration.events.len() * std::mem::size_of::<EventKind>()
        + registration.capabilities.len() * std::mem::size_of::<ExtensionCapability>()
}

fn contribution_registration_bytes(registration: &ContributionRegistration) -> usize {
    registration.name.len() + registration.plain_fallback.as_ref().map_or(0, String::len)
}

fn registration_limit_error(
    description: &str,
    unit: &str,
    observed: usize,
    limit: usize,
) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{description} exceed the configured registration limit"),
    )
    .with_context(format!("{unit}: {observed}; limit: {limit}"))
    .with_context("lua failure: registration")
    .with_help("Reduce plugin registration metadata or split the plugin after review")
}

fn registration_validation_error(message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Validation, "Lua registration failed validation")
        .with_context(message)
        .with_context("lua failure: registration")
        .with_help("Fix the registration before loading the plugin")
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
    validate_registration_input_shape(spec, callback_field, description)
        .map_err(mlua::Error::external)?;
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
        mlua::Error::external(registration_validation_error(format!(
            "invalid {description} registration: {error}"
        )))
    })
}

fn validate_registration_input_shape(
    spec: &Table,
    callback_field: &str,
    description: &str,
) -> Result<(), ShellError> {
    let mut stack = Vec::new();
    let mut seen_tables = HashSet::new();
    seen_tables.insert(spec.to_pointer());
    let mut scheduled_nodes = 0_usize;
    for pair in spec.clone().pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| {
            registration_validation_error(format!(
                "cannot inspect {description} registration: {error}"
            ))
        })?;
        let is_callback = matches!(
            &key,
            Value::String(name)
                if name.to_str().is_ok_and(|name| name.as_bytes() == callback_field.as_bytes())
        );
        schedule_registration_value(&mut stack, &mut scheduled_nodes, key, 1, description)?;
        if !is_callback {
            schedule_registration_value(&mut stack, &mut scheduled_nodes, value, 1, description)?;
        }
    }

    let mut retained_bytes = 0_usize;
    let mut observed_nodes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        observed_nodes += 1;
        match value {
            Value::String(value) => {
                retained_bytes = retained_bytes
                    .checked_add(value.as_bytes().len())
                    .ok_or_else(|| {
                        registration_limit_error(
                            description,
                            "bytes",
                            usize::MAX,
                            MAX_REGISTRATION_INPUT_BYTES,
                        )
                    })?;
                if retained_bytes > MAX_REGISTRATION_INPUT_BYTES {
                    return Err(registration_limit_error(
                        description,
                        "bytes",
                        retained_bytes,
                        MAX_REGISTRATION_INPUT_BYTES,
                    ));
                }
            }
            Value::Table(table) => {
                if depth >= MAX_REGISTRATION_INPUT_DEPTH {
                    return Err(registration_limit_error(
                        description,
                        "depth",
                        depth + 1,
                        MAX_REGISTRATION_INPUT_DEPTH,
                    ));
                }
                if !seen_tables.insert(table.to_pointer()) {
                    return Err(registration_validation_error(format!(
                        "{description} registration contains a cyclic or repeated table"
                    )));
                }
                for pair in table.pairs::<Value, Value>() {
                    let (key, value) = pair.map_err(|error| {
                        registration_validation_error(format!(
                            "cannot inspect nested {description} metadata: {error}"
                        ))
                    })?;
                    schedule_registration_value(
                        &mut stack,
                        &mut scheduled_nodes,
                        key,
                        depth + 1,
                        description,
                    )?;
                    schedule_registration_value(
                        &mut stack,
                        &mut scheduled_nodes,
                        value,
                        depth + 1,
                        description,
                    )?;
                }
            }
            Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_) => {}
            unsupported => {
                return Err(registration_validation_error(format!(
                    "{description} metadata contains unsupported Lua {}",
                    unsupported.type_name()
                )));
            }
        }
    }
    debug_assert_eq!(observed_nodes, scheduled_nodes);
    Ok(())
}

fn schedule_registration_value(
    stack: &mut Vec<(Value, usize)>,
    scheduled_nodes: &mut usize,
    value: Value,
    depth: usize,
    description: &str,
) -> Result<(), ShellError> {
    *scheduled_nodes = scheduled_nodes.checked_add(1).ok_or_else(|| {
        registration_limit_error(
            description,
            "nodes",
            usize::MAX,
            MAX_REGISTRATION_INPUT_NODES,
        )
    })?;
    if *scheduled_nodes > MAX_REGISTRATION_INPUT_NODES {
        return Err(registration_limit_error(
            description,
            "nodes",
            *scheduled_nodes,
            MAX_REGISTRATION_INPUT_NODES,
        ));
    }
    stack.push((value, depth));
    Ok(())
}

fn validate_completion_result(value: &serde_json::Value, command: &str) -> Result<(), ShellError> {
    let serde_json::Value::Array(items) = value else {
        return Err(validation_error(
            command,
            "completion provider must return an array",
        ));
    };
    if items.len() > MAX_LUA_COMPLETION_RESULTS {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("completion provider `{command}` returned too many items"),
        )
        .with_context(format!(
            "items: {}; limit: {MAX_LUA_COMPLETION_RESULTS}",
            items.len()
        ))
        .with_help("Return the most relevant completion items and keep the result bounded"));
    }
    let mut total_retained_bytes = 0_usize;
    for (index, item) in items.iter().enumerate() {
        let retained_bytes = match item {
            serde_json::Value::String(value) => value.len(),
            serde_json::Value::Object(object)
                if object
                    .get("value")
                    .is_some_and(serde_json::Value::is_string)
                    && object.iter().all(|(key, value)| match key.as_str() {
                        "value" | "display" | "summary" | "detail" => value.is_string(),
                        _ => false,
                    }) =>
            {
                object
                    .values()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::len)
                    .sum()
            }
            _ => {
                return Err(validation_error(
                    command,
                    format!(
                        "completion item {index} must be a string or an object with a string `value` and optional string display fields"
                    ),
                ));
            }
        };
        if retained_bytes > MAX_LUA_COMPLETION_ITEM_BYTES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!("completion item {index} from `{command}` is too large"),
            )
            .with_context(format!(
                "bytes: {retained_bytes}; limit: {MAX_LUA_COMPLETION_ITEM_BYTES}"
            ))
            .with_help("Shorten completion display, summary, and detail text"));
        }
        total_retained_bytes = total_retained_bytes
            .checked_add(retained_bytes)
            .ok_or_else(|| {
                lua_return_limit_error(
                    "completion retained bytes",
                    usize::MAX,
                    MAX_LUA_COMPLETION_RETAINED_BYTES,
                )
            })?;
        if total_retained_bytes > MAX_LUA_COMPLETION_RETAINED_BYTES {
            return Err(lua_return_limit_error(
                "completion retained bytes",
                total_retained_bytes,
                MAX_LUA_COMPLETION_RETAINED_BYTES,
            ));
        }
    }
    Ok(())
}

fn validate_lua_return_shape(value: &Value) -> Result<(), ShellError> {
    let mut stack = vec![(value.clone(), 0_usize)];
    let mut seen_tables = HashSet::new();
    let mut scheduled_nodes = 1_usize;
    let mut retained_bytes = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        match value {
            Value::Nil | Value::Boolean(_) | Value::Integer(_) | Value::Number(_) => {}
            Value::LightUserData(data) if data.0.is_null() => {}
            Value::String(value) => {
                retained_bytes = retained_bytes
                    .checked_add(value.as_bytes().len())
                    .ok_or_else(|| {
                        lua_return_limit_error(
                            "retained bytes",
                            usize::MAX,
                            MAX_LUA_RETURN_RETAINED_BYTES,
                        )
                    })?;
                if retained_bytes > MAX_LUA_RETURN_RETAINED_BYTES {
                    return Err(lua_return_limit_error(
                        "retained bytes",
                        retained_bytes,
                        MAX_LUA_RETURN_RETAINED_BYTES,
                    ));
                }
            }
            Value::Table(table) => {
                if depth >= MAX_LUA_RETURN_DEPTH {
                    return Err(lua_return_limit_error(
                        "depth",
                        depth + 1,
                        MAX_LUA_RETURN_DEPTH,
                    ));
                }
                if !seen_tables.insert(table.to_pointer()) {
                    return Err(validation_error(
                        "Lua callback return",
                        "return value contains a cyclic or repeated table",
                    ));
                }
                for pair in table.pairs::<Value, Value>() {
                    let (key, value) = pair.map_err(|error| {
                        validation_error(
                            "Lua callback return",
                            format!("cannot inspect returned table: {error}"),
                        )
                    })?;
                    match &key {
                        Value::String(_) => {}
                        Value::Integer(index) if (1..=MAX_LUA_RETURN_INDEX).contains(index) => {}
                        _ => {
                            return Err(validation_error(
                                "Lua callback return",
                                "returned tables require bounded positive integer or string keys",
                            ));
                        }
                    }
                    schedule_lua_return_value(&mut stack, &mut scheduled_nodes, key, depth + 1)?;
                    schedule_lua_return_value(&mut stack, &mut scheduled_nodes, value, depth + 1)?;
                }
            }
            unsupported => {
                return Err(validation_error(
                    "Lua callback return",
                    format!(
                        "return value contains unsupported Lua {}",
                        unsupported.type_name()
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn schedule_lua_return_value(
    stack: &mut Vec<(Value, usize)>,
    scheduled_nodes: &mut usize,
    value: Value,
    depth: usize,
) -> Result<(), ShellError> {
    *scheduled_nodes = scheduled_nodes
        .checked_add(1)
        .ok_or_else(|| lua_return_limit_error("nodes", usize::MAX, MAX_LUA_RETURN_NODES))?;
    if *scheduled_nodes > MAX_LUA_RETURN_NODES {
        return Err(lua_return_limit_error(
            "nodes",
            *scheduled_nodes,
            MAX_LUA_RETURN_NODES,
        ));
    }
    stack.push((value, depth));
    Ok(())
}

fn lua_return_limit_error(description: &str, observed: usize, limit: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "Lua callback return exceeded its configured shape limit",
    )
    .with_context("lua failure: returned shape")
    .with_context(format!("{description}: {observed}; limit: {limit}"))
    .with_help("Return a smaller, shallower typed value")
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
        (
            "load(",
            "dynamic chunks are unavailable because untrusted binary bytecode is not admitted",
        ),
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
    let source = read_source_bounded(path)?;
    Ok(normalize_shebang(&source))
}

fn read_source_bounded(path: &Path) -> Result<String, ShellError> {
    let bytes_max_u64 = u64::try_from(MAX_LUA_SOURCE_BYTES).unwrap_or(u64::MAX);
    let file = match open_source_nonblocking(path) {
        Ok(file) => file,
        Err(error) if open_error_identifies_special_file(&error) => {
            return Err(nonregular_source_error(path).with_context(error.to_string()));
        }
        Err(error) => return Err(script_read_error(path, error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| script_read_error(path, error))?;
    if !metadata.file_type().is_file() {
        return Err(nonregular_source_error(path));
    }
    if metadata.len() > bytes_max_u64 {
        return Err(lua_source_limit_error(path, metadata.len(), false));
    }
    let capacity = usize::try_from(metadata.len())
        .unwrap_or(MAX_LUA_SOURCE_BYTES)
        .min(MAX_LUA_SOURCE_BYTES);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(bytes_max_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| script_read_error(path, error))?;
    if bytes.len() > MAX_LUA_SOURCE_BYTES {
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        return Err(lua_source_limit_error(path, observed, true));
    }
    String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::ScriptRead,
            format!("Lua source {} is not valid UTF-8", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Encode Lua source as UTF-8")
    })
}

fn open_source_nonblocking(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(nix::libc::O_NONBLOCK);
    }
    options.open(path)
}

fn nonregular_source_error(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("Lua source {} is not a regular file", path.display()),
    )
    .with_context("directories, FIFOs, sockets, and device nodes are rejected")
    .with_help("Pass executable Lua source in a bounded regular file")
}

#[cfg(unix)]
fn open_error_identifies_special_file(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == nix::libc::EOPNOTSUPP
                || code == nix::libc::ENXIO
                || code == nix::libc::ENODEV
    )
}

#[cfg(not(unix))]
fn open_error_identifies_special_file(_error: &std::io::Error) -> bool {
    false
}

fn lua_source_limit_error(path: &Path, observed: u64, is_lower_bound: bool) -> ShellError {
    let observed = if is_lower_bound {
        format!("at least {observed}")
    } else {
        observed.to_string()
    };
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("Lua source {} exceeds its read limit", path.display()),
    )
    .with_context(format!(
        "limit: {MAX_LUA_SOURCE_BYTES}; observed: {observed}"
    ))
    .with_help("Keep executable Lua source below 4 MiB and load data through bounded host APIs")
}

fn validate_source_length(source: &str, path: &Path) -> Result<(), ShellError> {
    if source.len() > MAX_LUA_SOURCE_BYTES {
        let observed = u64::try_from(source.len()).unwrap_or(u64::MAX);
        Err(lua_source_limit_error(path, observed, false))
    } else {
        Ok(())
    }
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
    if let Some(error) = error.downcast_ref::<ShellError>() {
        let mut error = error.clone();
        if error.details.context.is_empty() {
            let message = error.message.clone();
            error = error.with_context(message);
        }
        if let Some(path) = path {
            error = error.with_label(
                Some(path.display().to_string()),
                0,
                source_len,
                "failed Lua operation",
            );
        }
        return error;
    }
    let message = error.to_string();
    if lua_error_is_memory(&error) {
        return ShellError::new(
            ErrorCode::ResourceLimit,
            "Lua exceeded its configured memory budget",
        )
        .with_context("lua failure: memory")
        .with_context(message)
        .with_help("Reduce retained Lua data or raise memory_limit_bytes after review")
        .with_label(
            path.map(|path| path.display().to_string()),
            0,
            source_len,
            "Lua allocator refused memory",
        );
    }
    ShellError::new(ErrorCode::Lua, "Lua could not load or evaluate the program")
        .with_context(message)
        .with_help("Run `quirl check <file> --format json` for a machine-readable diagnostic")
        .with_label(
            path.map(|path| path.display().to_string()),
            0,
            source_len,
            "invalid or failed Lua program",
        )
}

fn lua_error_is_memory(error: &mlua::Error) -> bool {
    match error {
        mlua::Error::MemoryError(_) => true,
        mlua::Error::BadArgument { cause, .. }
        | mlua::Error::CallbackError { cause, .. }
        | mlua::Error::WithContext { cause, .. } => lua_error_is_memory(cause),
        _ => false,
    }
}

fn lua_resource_error(
    resource: &str,
    observation: impl Into<String>,
    help: impl Into<String>,
) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("Lua exceeded its configured {resource} budget"),
    )
    .with_context(format!("lua failure: {resource}"))
    .with_context(observation)
    .with_help(help)
}

fn validation_error(source: &str, message: impl Into<String>) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "Lua value failed Rust schema validation",
    )
    .with_context(message)
    .with_label(Some(source.to_owned()), 0, 0, "schema mismatch")
    .with_help("Check the documented Lua SDK shape and remove unknown fields or invalid values")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static TEST_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestFile(std::path::PathBuf);

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn test_file_path(label: &str) -> TestFile {
        TestFile(std::env::temp_dir().join(format!(
            "quirl-lua-{label}-{}-{}",
            std::process::id(),
            TEST_FILE_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed)
        )))
    }

    fn runner_context(
        runtime: &LuaRuntime,
        arguments: &[&str],
        input: ExecutionInput,
    ) -> LuaRunnerContext {
        LuaRunnerContext::new(
            arguments.iter().map(|value| (*value).to_owned()).collect(),
            BTreeMap::from([("QUIRL_TEST_ENV".to_owned(), "visible".to_owned())]),
            "/tmp/quirl-runner".to_owned(),
            input,
            ExecutionOutputTarget::Value,
            ExecutionEffects::from_effects(&[
                ExecutionEffect::ReadFilesystem,
                ExecutionEffect::SpawnProcess,
            ]),
            Arc::clone(&runtime.cancelled),
        )
        .unwrap()
    }

    fn test_theme(color: &str) -> ThemeColors {
        ThemeColors {
            accent_command: color.to_owned(),
            accent_data: color.to_owned(),
            context_primary: color.to_owned(),
            context_secondary: color.to_owned(),
            muted: color.to_owned(),
            border: color.to_owned(),
            status_background: color.to_owned(),
            error: color.to_owned(),
            warning: color.to_owned(),
            hint: color.to_owned(),
            string: color.to_owned(),
            operator: color.to_owned(),
            expansion: color.to_owned(),
            number: color.to_owned(),
        }
    }

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
        assert_eq!(config.schema_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(config.prompt.symbols, "auto");
    }

    #[test]
    fn prompt_symbol_profile_requires_an_explicit_known_value() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let value = runtime
            .lua
            .load("return quirl.config { prompt = { symbols = 'patched_magic' } }")
            .eval::<Value>()
            .unwrap();
        let config = runtime.lua.from_value::<QuirlConfig>(value).unwrap();
        let error = config.validate("symbols.lua").unwrap_err();

        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("prompt.symbols"));
    }

    #[test]
    fn legacy_configs_through_v2_migrate_to_v3_and_future_versions_fail() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let legacy = runtime
            .lua
            .load("return quirl.config { editor = { keymap = 'vim' } }")
            .eval::<Value>()
            .unwrap();
        let migrated = runtime.lua.from_value::<QuirlConfig>(legacy).unwrap();
        assert_eq!(migrated.schema_version, CONFIG_SCHEMA_VERSION);
        migrated.validate("legacy.lua").unwrap();

        for legacy_version in 0..CONFIG_SCHEMA_VERSION {
            let path = std::env::temp_dir().join(format!(
                "quirl-config-v{legacy_version}-{}.lua",
                std::process::id()
            ));
            fs::write(
                &path,
                format!("return quirl.config {{ schema_version = {legacy_version} }}"),
            )
            .unwrap();
            let migrated = runtime.load_config_file(&path).unwrap();
            assert_eq!(migrated.schema_version, CONFIG_SCHEMA_VERSION);
            assert_eq!(migrated.ui.theme, DEFAULT_THEME_NAME);
            fs::remove_file(path).unwrap();
        }

        let future = runtime
            .lua
            .load("return quirl.config { schema_version = 4 }")
            .eval::<Value>()
            .unwrap();
        let future = runtime.lua.from_value::<QuirlConfig>(future).unwrap();
        assert!(future.validate("future.lua").is_err());
    }

    #[test]
    fn config_schema_descriptor_has_a_stable_identity() {
        assert_eq!(CONFIG_OLDEST_READABLE_VERSION, 0);
        assert!(CONFIG_SCHEMA_DESCRIPTOR.contains("migration:unversioned-or-v1-or-v2-to-v3"));
        assert!(config_schema_hash().starts_with("fnv1a64:"));
    }

    #[test]
    fn default_and_custom_themes_resolve_to_fixed_color_roles() {
        let default = QuirlConfig::default();
        let tokyo_night = default.active_theme().unwrap();
        assert_eq!(default.ui.theme, "tokyo-night");
        assert_eq!(tokyo_night.accent_command, "#9ece6a");
        assert_eq!(tokyo_night.status_background, "#24283b");

        let mut custom = QuirlConfig::default();
        custom.ui.theme = "quiet".to_owned();
        custom
            .ui
            .themes
            .insert("quiet".to_owned(), test_theme("#123abc"));
        custom.validate("custom.lua").unwrap();
        assert_eq!(custom.active_theme().unwrap(), test_theme("#123abc"));

        let mut ansi = QuirlConfig::default();
        ansi.ui.theme = "ansi".to_owned();
        assert_eq!(ansi.active_theme().unwrap().accent_command, "#00aa00");
    }

    #[test]
    fn popular_builtin_theme_registry_is_complete_unique_and_valid() {
        let names = builtin_theme_names().collect::<Vec<_>>();
        assert_eq!(names.len(), POPULAR_THEME_COUNT + 1);
        assert_eq!(names.first(), Some(&"ansi"));
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));

        let unique = names.iter().copied().collect::<HashSet<_>>();
        assert_eq!(unique.len(), names.len());
        for name in names {
            validate_theme_name("builtins", "built-in theme", name).unwrap();
            let colors = builtin_theme(name).unwrap();
            colors.validate("builtins", name).unwrap();
            assert!(CONFIG_SCHEMA_DESCRIPTOR.contains(name));

            let mut config = QuirlConfig::default();
            config.ui.theme = name.to_owned();
            config.validate("builtins").unwrap();
            assert_eq!(config.active_theme().unwrap(), colors);
        }
    }

    #[test]
    fn representative_popular_theme_palettes_keep_their_source_identity() {
        assert_eq!(builtin_theme("dracula").unwrap().error, "#ff5555");
        assert_eq!(
            builtin_theme("catppuccin-mocha").unwrap().status_background,
            "#1e1e2e"
        );
        assert_eq!(
            builtin_theme("gruvbox-dark").unwrap().accent_command,
            "#b8bb26"
        );
        assert_eq!(builtin_theme("nord").unwrap().hint, "#81a1c1");
        assert_eq!(builtin_theme("solarized-dark").unwrap().operator, "#2aa198");
    }

    #[test]
    fn lua_config_deserializes_custom_theme_roles_exactly() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let value = runtime
            .lua
            .load(
                r##"return quirl.config {
                  ui = {
                    theme = "quiet",
                    themes = {
                      quiet = {
                        accent_command = "#010101", accent_data = "#020202",
                        context_primary = "#030303", context_secondary = "#040404",
                        muted = "#050505", border = "#060606",
                        status_background = "#070707", error = "#080808",
                        warning = "#090909", hint = "#0a0a0a", string = "#0b0b0b",
                        operator = "#0c0c0c", expansion = "#0d0d0d", number = "#0e0e0e",
                      },
                    },
                  },
                }"##,
            )
            .eval::<Value>()
            .unwrap();
        let config = runtime.lua.from_value::<QuirlConfig>(value).unwrap();
        config.validate("theme.lua").unwrap();
        let active = config.active_theme().unwrap();
        assert_eq!(active.accent_command, "#010101");
        assert_eq!(active.number, "#0e0e0e");
    }

    #[test]
    fn theme_validation_rejects_unknown_malformed_and_shadowing_values() {
        let mut unknown = QuirlConfig::default();
        unknown.ui.theme = "missing".to_owned();
        let error = unknown.validate("unknown.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("not built in"));

        let mut malformed = QuirlConfig::default();
        malformed.ui.theme = "quiet".to_owned();
        malformed
            .ui
            .themes
            .insert("quiet".to_owned(), test_theme("123456"));
        let error = malformed.validate("malformed.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("#RRGGBB"));

        let mut shadowing = QuirlConfig::default();
        shadowing
            .ui
            .themes
            .insert("tokyo-night".to_owned(), test_theme("#123456"));
        let error = shadowing.validate("shadow.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("must not shadow"));
    }

    #[test]
    fn theme_names_colors_and_collection_sizes_are_bounded() {
        let mut long_name = QuirlConfig::default();
        long_name.ui.theme = "a".repeat(MAX_THEME_NAME_BYTES + 1);
        let error = long_name.validate("long-name.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("bytes: 65; limit: 64"));

        let mut unsafe_name = QuirlConfig::default();
        unsafe_name.ui.theme = "Tokyo_Night".to_owned();
        let error = unsafe_name.validate("unsafe-name.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("lowercase ASCII"));

        let mut long_color = QuirlConfig::default();
        long_color.ui.theme = "quiet".to_owned();
        long_color
            .ui
            .themes
            .insert("quiet".to_owned(), test_theme("#1234567"));
        let error = long_color.validate("long-color.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("bytes: 8; limit: 7"));

        let mut too_many = QuirlConfig::default();
        for index in 0..=MAX_CUSTOM_THEMES {
            too_many
                .ui
                .themes
                .insert(format!("theme-{index}"), test_theme("#123456"));
        }
        let error = too_many.validate("too-many.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("themes: 33; limit: 32"));
    }

    #[test]
    fn welcome_banner_rejects_unknown_profiles() {
        let mut config = QuirlConfig::default();
        config.editor.banner = "sometimes".to_owned();

        let error = config.validate("config.lua").unwrap_err();

        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("editor.banner"));
        assert!(error.details.help[0].contains("compact"));
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
              ui = { theme = "ansi" },
            }"#,
        )
        .unwrap();
        let mut store = ConfigStore::default();
        store.reload(&runtime, &path).unwrap();
        assert_eq!(store.active().editor.keymap, "vim");
        assert_eq!(store.active().ui.theme, "ansi");

        fs::write(&path, "return quirl.config { ui = { theme = 'missing' } }").unwrap();
        assert!(store.reload(&runtime, &path).is_err());
        assert_eq!(store.active().editor.keymap, "vim");
        assert_eq!(store.active().ui.theme, "ansi");
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
    fn process_capability_fails_closed_without_a_composed_host() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let error = runtime
            .eval("return quirl.process.run('printf nope')")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Lua);
        assert!(error.details.context[0].contains("process host is unavailable"));
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn scoped_process_grant_cannot_smuggle_shell_operators() {
        let process_host: ProcessHost = Arc::new(|request| {
            assert!(request.deadline <= Duration::from_millis(100));
            assert!(!request.deadline.is_zero());
            assert_eq!(request.max_output_bytes, MAX_PROCESS_OUTPUT_BYTES);
            Ok(quirl_core::CommandOutcome {
                status: 0,
                stdout: Some(request.command),
                stderr: Some(String::new()),
            })
        });
        let runtime = LuaRuntime::new_with_capabilities_and_process_host(
            LuaPolicy {
                allow_process: true,
                ..LuaPolicy::config()
            },
            &["process.spawn:printf".to_owned()],
            Some(process_host),
        )
        .unwrap();
        let allowed = runtime
            .eval("return quirl.process.run('printf scoped')")
            .unwrap();
        assert_eq!(allowed["status"], 0);
        let error = runtime
            .eval("return quirl.process.run('printf safe; printf smuggled')")
            .unwrap_err();
        assert!(error.details.context[0].contains("capability denied"));
        let newline = runtime
            .eval("return quirl.process.run('printf safe\\nprintf smuggled')")
            .unwrap_err();
        assert!(newline.details.context[0].contains("capability denied"));
        let tab = runtime
            .eval("return quirl.process.run('printf\\tsmuggled')")
            .unwrap_err();
        assert!(tab.details.context[0].contains("capability denied"));
    }

    #[test]
    fn process_host_uses_the_remaining_callback_deadline() {
        let received_deadline = Arc::new(Mutex::new(None));
        let received_by_host = Arc::clone(&received_deadline);
        let process_host: ProcessHost = Arc::new(move |request| {
            *received_by_host.lock().unwrap() = Some(request.deadline);
            Ok(quirl_core::CommandOutcome {
                status: 0,
                stdout: Some("ok".to_owned()),
                stderr: Some(String::new()),
            })
        });
        let runtime = LuaRuntime::new_with_capabilities_and_process_host(
            LuaPolicy {
                allow_process: true,
                wall_time: Duration::from_secs(1),
                ..LuaPolicy::config()
            },
            &["prompt.register".to_owned(), "process.spawn".to_owned()],
            Some(process_host),
        )
        .unwrap();
        runtime
            .eval(
                r#"quirl.prompt.add_segment {
                    name = "bounded-process", deadline_ms = 1,
                    render = function() return quirl.process.run("printf bounded").value end,
                }"#,
            )
            .unwrap();

        assert_eq!(
            runtime
                .render_prompt_segment("bounded-process", &serde_json::json!({}))
                .unwrap(),
            Some("ok".to_owned())
        );
        let deadline = received_deadline.lock().unwrap().unwrap();
        assert!(deadline <= Duration::from_millis(1));
        assert!(!deadline.is_zero());
    }

    #[test]
    fn process_host_receives_lua_cancellation() {
        let process_host: ProcessHost = Arc::new(|request| {
            while !request.cancelled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(
                ShellError::new(ErrorCode::ResourceLimit, "process execution was cancelled")
                    .with_help("Use a shorter-running command"),
            )
        });
        let runtime = LuaRuntime::new_with_process_host(LuaPolicy::script(), process_host).unwrap();
        let cancellation = runtime.cancellation_token();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            cancellation.cancel();
        });
        let error = runtime
            .eval("return quirl.process.run('long-running-command')")
            .unwrap_err();
        canceller.join().unwrap();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("process execution was cancelled"));
    }

    #[test]
    fn cancellation_reaches_a_process_started_by_a_prompt_callback() {
        let process_host: ProcessHost = Arc::new(|request| {
            while !request.cancelled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(
                ShellError::new(ErrorCode::ResourceLimit, "process execution was cancelled")
                    .with_help("Use a shorter-running command"),
            )
        });
        let runtime = LuaRuntime::new_with_capabilities_and_process_host(
            LuaPolicy {
                allow_process: true,
                ..LuaPolicy::config()
            },
            &["prompt.register".to_owned(), "process.spawn".to_owned()],
            Some(process_host),
        )
        .unwrap();
        runtime
            .eval(
                r#"quirl.prompt.add_segment {
                    name = "cancelled-process", deadline_ms = 100,
                    render = function() return quirl.process.run("long-running-command").value end,
                }"#,
            )
            .unwrap();
        let cancellation = runtime.cancellation_token();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            cancellation.cancel();
        });
        let error = runtime
            .render_prompt_segment("cancelled-process", &serde_json::json!({}))
            .unwrap_err();
        canceller.join().unwrap();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("process execution was cancelled"));
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
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: instruction")
        );
    }

    #[test]
    fn pcall_cannot_resume_after_the_instruction_budget_terminates() {
        let runtime = LuaRuntime::new(LuaPolicy {
            instruction_limit: HOOK_GRANULARITY,
            ..LuaPolicy::script()
        })
        .unwrap();
        let started = Instant::now();
        let error = runtime
            .eval("return pcall(function() while true do end end)")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: instruction")
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(runtime.eval("return 42").unwrap(), serde_json::json!(42));
    }

    #[test]
    fn xpcall_cannot_resume_after_the_instruction_budget_terminates() {
        let runtime = LuaRuntime::new(LuaPolicy {
            instruction_limit: HOOK_GRANULARITY,
            ..LuaPolicy::script()
        })
        .unwrap();
        let started = Instant::now();
        let error = runtime
            .eval("return xpcall(function() while true do end end, function() return 'caught' end)")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: instruction")
        );
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn protected_calls_still_catch_ordinary_guest_errors() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let value = runtime
            .eval(
                r#"
                local pcall_ok, pcall_error = pcall(function() error("ordinary") end)
                local xpcall_ok, xpcall_error = xpcall(
                    function() error("ordinary") end,
                    function() return "handled" end
                )
                return { pcall_ok, type(pcall_error), xpcall_ok, xpcall_error }
                "#,
            )
            .unwrap();
        assert_eq!(
            value,
            serde_json::json!([false, "string", false, "handled"])
        );
    }

    #[test]
    fn cancellation_stops_lua_at_the_instruction_hook() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        runtime.cancellation_token().cancel();
        let error = runtime.eval("while true do end").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: cancellation")
        );
    }

    #[test]
    fn cancellation_cannot_be_swallowed_by_protected_calls() {
        for protected_call in ["pcall", "xpcall"] {
            let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
            runtime.cancellation_token().cancel();
            let source = if protected_call == "pcall" {
                "return pcall(function() while true do end end)"
            } else {
                "return xpcall(function() while true do end end, function() return 'caught' end)"
            };
            let started = Instant::now();
            let error = runtime.eval(source).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|item| item == "lua failure: cancellation")
            );
            assert!(started.elapsed() < Duration::from_millis(500));
        }
    }

    #[test]
    fn native_string_patterns_fail_closed_before_the_wall_deadline() {
        let runtime = LuaRuntime::new(LuaPolicy {
            instruction_limit: 100_000_000,
            wall_time: Duration::from_millis(25),
            ..LuaPolicy::config()
        })
        .unwrap();
        let started = Instant::now();
        let error = runtime
            .eval(
                "local s = string.rep('a', 24); local p = string.rep('a?', 24) .. string.rep('a', 24); return string.match(s, p)",
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: wall_time")
        );
        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn string_patterns_are_restricted_but_literal_find_remains_available() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        assert_eq!(
            runtime
                .eval(
                    "local first, last = string.find('a.b', '.', 1, true); return { first, last }"
                )
                .unwrap(),
            serde_json::json!([2, 2])
        );

        for operation in [
            "return string.find('abc', 'a.c')",
            "return string.match('abc', 'a.c')",
            "return string.gmatch('abc', '.')",
            "return string.gsub('abc', '.', 'x')",
        ] {
            let error = runtime.eval(operation).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|item| item == "lua failure: wall_time")
            );
        }
    }

    #[test]
    fn allocator_refusal_is_distinct_and_the_runtime_recovers() {
        let baseline = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let memory_limit_bytes = baseline.lua.used_memory() + 64 * 1024;
        drop(baseline);
        let runtime = LuaRuntime::new(LuaPolicy {
            memory_limit_bytes,
            ..LuaPolicy::config()
        })
        .unwrap();
        let error = runtime
            .eval(
                "local values = {}; for i = 1, 100000 do values[i] = string.rep('x', 128) end; return values",
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: memory")
        );
        assert_eq!(runtime.eval("collectgarbage(); return 42").unwrap(), 42);
    }

    #[test]
    fn invalid_and_tiny_policies_fail_without_leaking_partial_initialization() {
        for policy in [
            LuaPolicy {
                memory_limit_bytes: 0,
                ..LuaPolicy::config()
            },
            LuaPolicy {
                instruction_limit: 0,
                ..LuaPolicy::config()
            },
            LuaPolicy {
                wall_time: Duration::ZERO,
                ..LuaPolicy::config()
            },
        ] {
            let error = LuaRuntime::new(policy).err().unwrap();
            assert_eq!(error.code, ErrorCode::Validation);
        }

        let error = LuaRuntime::new(LuaPolicy {
            memory_limit_bytes: 1,
            ..LuaPolicy::config()
        })
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: memory")
        );
        LuaRuntime::new(LuaPolicy::config()).unwrap();
    }

    #[test]
    fn oversized_source_is_rejected_before_lua_compilation() {
        let source = " ".repeat(MAX_LUA_SOURCE_BYTES + 1);
        let error = LuaRuntime::check_source(&source, "oversized.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit"));
    }

    #[test]
    fn source_file_reader_accepts_exact_limit_and_rejects_limit_plus_one() {
        let path = test_file_path("source-limit.lua");
        fs::write(&path.0, vec![b' '; MAX_LUA_SOURCE_BYTES]).unwrap();
        assert_eq!(
            read_source_bounded(&path.0).unwrap().len(),
            MAX_LUA_SOURCE_BYTES
        );

        fs::write(&path.0, vec![b' '; MAX_LUA_SOURCE_BYTES + 1]).unwrap();
        let error = read_source_bounded(&path.0).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit:"));
        assert!(error.details.context[0].contains("observed:"));
    }

    #[cfg(unix)]
    #[test]
    fn source_file_reader_rejects_fifo_without_blocking() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        use std::{sync::mpsc, time::Duration};

        let path = test_file_path("source-fifo.lua");
        mkfifo(&path.0, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let worker_path = path.0.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = sender.send(read_source_bounded(&worker_path));
        });
        let error = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Lua FIFO admission must not block")
            .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("not a regular file"));
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
                "return { io == nil, os == nil, debug == nil, package == nil, require == nil, dofile == nil, load == nil, loadfile == nil, print == nil, warn == nil, coroutine == nil, table ~= nil, string ~= nil, math ~= nil, utf8 ~= nil }",
            )
            .unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                true, true, true, true, true, true, true, true, true, true, true, true, true, true,
                true
            ])
        );
    }

    #[test]
    fn every_registration_surface_rejects_count_amplification_and_rolls_back() {
        let cases = [
            (
                MAX_PLUGIN_PROMPT_SEGMENTS,
                r#"quirl.prompt.add_segment { name = "prompt" .. i, render = function() return nil end }"#,
            ),
            (
                MAX_PLUGIN_COMPLETION_PROVIDERS,
                r#"quirl.completion.add_provider { command = "complete" .. i, complete = function() return {} end }"#,
            ),
            (
                MAX_PLUGIN_COMMANDS,
                r#"quirl.plugin.command { name = "command" .. i, signature = "command", summary = "summary", details = "details", input_type = "Nothing", output_type = "Record", examples = { "command" }, effects = { "none" }, error_codes = { E = "error" }, run = function() return {} end }"#,
            ),
            (
                MAX_PLUGIN_EVENT_HANDLERS,
                r#"quirl.events.subscribe { name = "handler" .. i, events = { "result" }, capabilities = { "events_observe" }, deadline_ms = 10, observe = function() return {} end }"#,
            ),
            (
                MAX_PLUGIN_CONTRIBUTIONS,
                r#"quirl.extension.contribute { kind = "catalog", name = "contribution" .. i, deadline_ms = 10, provide = function() return {} end }"#,
            ),
            (
                MAX_PLUGIN_PANELS,
                r#"quirl.extension.contribute { kind = "panel", name = "panel" .. i, deadline_ms = 10, plain_fallback = "unavailable", provide = function() return {} end }"#,
            ),
        ];
        for (limit, registration) in cases {
            let exact_source = format!("for i = 1, {limit} do {registration} end");
            let exact_runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
            exact_runtime
                .load_plugin_source(&exact_source, "exact-boundary.lua")
                .unwrap();

            let source = format!("for i = 1, {} do {registration} end", limit + 1);
            let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
            let error = runtime
                .load_plugin_source(&source, "amplification.lua")
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|item| item == "lua failure: registration")
            );
            assert_eq!(runtime.registrations(), PluginRegistrations::default());
        }
    }

    #[test]
    fn registration_fields_and_aggregate_bytes_are_bounded_before_commit() {
        let oversized_name = "x".repeat(MAX_REGISTRATION_NAME_BYTES + 1);
        let oversized_description = "x".repeat(MAX_REGISTRATION_DESCRIPTION_BYTES + 1);
        let oversized_details = "x".repeat(MAX_COMMAND_DETAILS_BYTES + 1);
        let oversized_type = "x".repeat(MAX_COMMAND_TYPE_BYTES + 1);
        let oversized_fallback = "x".repeat(MAX_PANEL_FALLBACK_BYTES + 1);
        let cases = [
            format!(
                "quirl.prompt.add_segment {{ name = '{oversized_name}', render = function() end }}"
            ),
            format!(
                "quirl.completion.add_provider {{ command = '{oversized_name}', complete = function() return {{}} end }}"
            ),
            format!(
                "quirl.plugin.command {{ name = 'command', signature = 'command', summary = '{oversized_description}', details = 'details', input_type = 'Nothing', output_type = 'Record', examples = {{ 'command' }}, effects = {{ 'none' }}, error_codes = {{ E = 'error' }}, run = function() end }}"
            ),
            format!(
                "quirl.plugin.command {{ name = 'command', signature = 'command', summary = 'summary', details = '{oversized_details}', input_type = 'Nothing', output_type = 'Record', examples = {{ 'command' }}, effects = {{ 'none' }}, error_codes = {{ E = 'error' }}, run = function() end }}"
            ),
            format!(
                "quirl.plugin.command {{ name = 'command', signature = 'command', summary = 'summary', details = 'details', input_type = '{oversized_type}', output_type = 'Record', examples = {{ 'command' }}, effects = {{ 'none' }}, error_codes = {{ E = 'error' }}, run = function() end }}"
            ),
            format!(
                "quirl.events.subscribe {{ name = '{oversized_name}', events = {{ 'result' }}, capabilities = {{ 'events_observe' }}, deadline_ms = 10, observe = function() return {{}} end }}"
            ),
            format!(
                "quirl.extension.contribute {{ kind = 'catalog', name = '{oversized_name}', deadline_ms = 10, provide = function() return {{}} end }}"
            ),
            format!(
                "quirl.extension.contribute {{ kind = 'panel', name = 'panel', deadline_ms = 10, plain_fallback = '{oversized_fallback}', provide = function() return {{}} end }}"
            ),
        ];
        for source in cases {
            let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
            let error = runtime
                .load_plugin_source(&source, "oversized-registration.lua")
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert_eq!(runtime.registrations(), PluginRegistrations::default());
        }
    }

    #[test]
    fn plugin_command_registration_accepts_only_executable_io_contracts() {
        let registration = |input_type: &str, output_type: &str| {
            format!(
                r#"quirl.plugin.command {{
                  name = "demo run", signature = "demo run", summary = "Run demo",
                  details = "Exercise one executable contract.",
                  input_type = "{input_type}", output_type = "{output_type}",
                  examples = {{ "demo run" }}, effects = {{ "none" }},
                  error_codes = {{ ["0"] = "success" }}, run = function() end,
                }}"#
            )
        };
        for (input_type, output_type) in [
            ("Unknown", "String"),
            ("String | Path", "String"),
            ("Stream<String>", "String"),
            ("Nothing", "Unknown"),
            ("Nothing", "String | Path"),
            ("Nothing", "Stream<String>"),
        ] {
            let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
            let error = runtime
                .load_plugin_source(&registration(input_type, output_type), "contract.lua")
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation);
            assert!(
                error
                    .details
                    .context
                    .iter()
                    .any(|item| item.contains("unsupported"))
            );
            assert_eq!(runtime.registrations(), PluginRegistrations::default());
        }

        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let registrations = runtime
            .load_plugin_source(&registration("Path", "Values<String>"), "contract.lua")
            .unwrap();
        assert_eq!(registrations.commands[0].input_type, "Path");
        assert_eq!(registrations.commands[0].output_type, "Values<String>");
    }

    #[test]
    fn failed_plugin_load_discards_partial_callbacks_and_allows_recovery() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = runtime
            .load_plugin_source(
                r#"
                quirl.prompt.add_segment { name = "partial", render = function() return "bad" end }
                quirl.prompt.add_segment { name = "partial", render = function() return "duplicate" end }
                "#,
                "partial.lua",
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(runtime.registrations(), PluginRegistrations::default());
        assert!(
            runtime
                .render_prompt_segment("partial", &serde_json::json!({}))
                .is_err()
        );

        runtime
            .load_plugin_source(
                r#"quirl.prompt.add_segment { name = "recovered", render = function() return "ok" end }"#,
                "recovered.lua",
            )
            .unwrap();
        assert_eq!(
            runtime
                .render_prompt_segment("recovered", &serde_json::json!({}))
                .unwrap(),
            Some("ok".to_owned())
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
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "lua failure: wall_time")
        );
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
    fn completion_results_reject_excess_items_and_retained_text() {
        let too_many = serde_json::Value::Array(
            (0..=MAX_LUA_COMPLETION_RESULTS)
                .map(|_| serde_json::Value::String("value".to_owned()))
                .collect(),
        );
        let error = validate_completion_result(&too_many, "bounded").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("too many items"));

        let too_large = serde_json::json!(["x".repeat(MAX_LUA_COMPLETION_ITEM_BYTES + 1)]);
        let error = validate_completion_result(&too_large, "bounded").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("too large"));

        let aggregate = serde_json::Value::Array(
            (0..=MAX_LUA_COMPLETION_RETAINED_BYTES / MAX_LUA_COMPLETION_ITEM_BYTES)
                .map(|_| serde_json::Value::String("x".repeat(MAX_LUA_COMPLETION_ITEM_BYTES)))
                .collect(),
        );
        let error = validate_completion_result(&aggregate, "bounded").unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item.contains("completion retained bytes"))
        );
    }

    #[test]
    fn callback_returns_enforce_bytes_nodes_depth_cycles_and_action_counts() {
        let prompt = LuaRuntime::new(LuaPolicy::config()).unwrap();
        prompt
            .eval(&format!(
                "quirl.prompt.add_segment {{ name = 'large', render = function() return string.rep('x', {}) end }}",
                MAX_PROMPT_RETURN_BYTES + 1
            ))
            .unwrap();
        let error = prompt
            .render_prompt_segment("large", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);

        let shape = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let deeply_nested = format!(
            "local value = 1; {} return value",
            (0..=MAX_LUA_RETURN_DEPTH)
                .map(|_| "value = { value };")
                .collect::<String>()
        );
        let error = shape.eval(&deeply_nested).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        let error = shape
            .eval("local value = {}; value.self = value; return value")
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        let error = shape
            .eval(&format!(
                "local value = {{}}; for i = 1, {MAX_LUA_RETURN_NODES} do value[i] = i end; return value"
            ))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);

        let events = LuaRuntime::new(LuaPolicy::config()).unwrap();
        events
            .eval(&format!(
                r#"quirl.events.subscribe {{
                    name = "many", events = {{ "result" }},
                    capabilities = {{ "events_observe" }}, deadline_ms = 10,
                    observe = function()
                        local actions = {{}}
                        for i = 1, {} do
                            actions[i] = {{ action = "diagnose", message = "message" }}
                        end
                        return actions
                    end,
                }}"#,
                MAX_EVENT_ACTIONS + 1
            ))
            .unwrap();
        let reports = events
            .dispatch_extension_event(&ExtensionEvent::new(
                1,
                ExtensionEventData::Result {
                    status: 0,
                    duration_ms: 1,
                },
            ))
            .unwrap();
        assert_eq!(
            reports[0].error.as_ref().unwrap().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn serialized_registrations_validate_on_writer_and_reader_boundaries() {
        let invalid = PluginRegistrations {
            prompt_segments: vec![PromptRegistration {
                name: "x".repeat(MAX_REGISTRATION_NAME_BYTES + 1),
                deadline_ms: 8,
            }],
            ..PluginRegistrations::default()
        };
        assert!(serde_json::to_string(&invalid).is_err());

        let wire = serde_json::json!({
            "prompt_segments": [{
                "name": "x".repeat(MAX_REGISTRATION_NAME_BYTES + 1),
                "deadline_ms": 8
            }],
            "completion_providers": [],
            "commands": [],
            "events": [],
            "contributions": []
        });
        assert!(serde_json::from_value::<PluginRegistrations>(wire).is_err());

        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let valid = runtime
            .load_plugin_source(
                r#"quirl.prompt.add_segment { name = "valid", render = function() end }"#,
                "valid.lua",
            )
            .unwrap();
        let encoded = serde_json::to_vec(&valid).unwrap();
        assert_eq!(
            serde_json::from_slice::<PluginRegistrations>(&encoded).unwrap(),
            valid
        );
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
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item == "test: test_a")
        );
        assert!(
            error
                .details
                .context
                .iter()
                .any(|item| item.contains("a failed"))
        );
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
            runner_abi_version: u32,
            runner_abi_hash: String,
            runner_abi_descriptor: String,
            functions: Vec<serde_json::Value>,
        }
        let json = sdk_json().unwrap();
        let envelope: SdkEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope.document_type, "quirl.host_api");
        assert_eq!(envelope.schema_version, 2);
        assert_eq!(envelope.module, "quirl");
        assert_eq!(envelope.module_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(envelope.runner_abi_version, LUA_RUNNER_ABI_VERSION);
        assert_eq!(envelope.runner_abi_hash, lua_runner_abi_hash());
        assert_eq!(envelope.runner_abi_descriptor, LUA_RUNNER_ABI_DESCRIPTOR);
        assert_eq!(envelope.functions.len(), HOST_API.len());
        let mut unknown: serde_json::Value = serde_json::from_str(&json).unwrap();
        unknown["unexpected"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<SdkEnvelope>(unknown).is_err());

        let markdown = sdk_markdown();
        assert!(markdown.contains("Runner ABI: `1`"));
        assert!(markdown.contains("`quirl.process.run(command: string) -> quirl.ProcessResult`"));
        assert!(markdown.contains("| `command` | `string` |"));
        assert!(markdown.contains("Returns: `quirl.ProcessResult`"));
    }

    #[test]
    fn installed_runtime_functions_and_primary_grants_exactly_match_host_api() {
        fn collect_functions(table: &Table, prefix: &str, paths: &mut Vec<String>) {
            for pair in table.clone().pairs::<String, Value>() {
                let (name, value) = pair.unwrap();
                let path = format!("{prefix}.{name}");
                match value {
                    Value::Function(_) => paths.push(path),
                    Value::Table(table) => collect_functions(&table, &path, paths),
                    _ => {}
                }
            }
        }

        let runtime = LuaRuntime::new_with_capabilities(LuaPolicy::config(), &[]).unwrap();
        let quirl = runtime.lua.globals().get::<Table>("quirl").unwrap();
        let mut installed = Vec::new();
        collect_functions(&quirl, "quirl", &mut installed);
        installed.sort();
        let mut declared = HOST_API
            .iter()
            .map(|specification| specification.path.to_owned())
            .collect::<Vec<_>>();
        declared.sort();
        declared.dedup();
        assert_eq!(
            declared.len(),
            HOST_API.len(),
            "HOST_API paths must be unique"
        );
        assert_eq!(installed, declared);

        for specification in HOST_API {
            let Some(capability) = specification.capability else {
                continue;
            };
            let mut parts = specification.path.split('.');
            assert_eq!(parts.next(), Some("quirl"));
            let mut table = quirl.clone();
            let mut function = None;
            let components = parts.collect::<Vec<_>>();
            for (index, component) in components.iter().enumerate() {
                if index + 1 == components.len() {
                    function = Some(table.get::<Function>(*component).unwrap());
                } else {
                    table = table.get::<Table>(*component).unwrap();
                }
            }
            let argument = if specification.path == "quirl.process.run" {
                Value::String(runtime.lua.create_string("true").unwrap())
            } else {
                Value::Table(runtime.lua.create_table().unwrap())
            };
            let error = function.unwrap().call::<Value>(argument).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("capability denied: {capability}")),
                "{} did not enforce its declared primary grant",
                specification.path
            );
        }
    }

    #[test]
    fn linter_rejects_ambient_operating_system_access() {
        let error = lint_source("return os.execute('whoami')", Path::new("plugin.lua"))
            .expect_err("os access must require a capability");
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(
            error.details.labels[0]
                .message
                .contains("explicit Quirl capability")
        );
    }

    #[test]
    fn linter_rejects_dynamic_chunk_loading() {
        let error = lint_source("return load('return 42')", Path::new("plugin.lua"))
            .expect_err("dynamic chunks must not bypass source admission");
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(
            error.details.labels[0]
                .message
                .contains("untrusted binary bytecode")
        );
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

    #[test]
    fn typed_commands_require_an_explicit_grant_and_run_under_a_deadline() {
        let source = r#"quirl.plugin.command {
          name = "demo run", signature = "demo run", summary = "Run demo",
          details = "Return one typed record.", input_type = "Nothing",
          output_type = "Record", examples = { "demo run" },
          effects = { "read_filesystem" }, error_codes = { ["0"] = "success" },
          run = function(args)
            if args.abi_version == 1 then
              if args.args[1] == "malformed" then
                return { abi_version = 1, ok = true, status = 0 }
              end
              return { abi_version = 1, ok = true, status = 0,
                output = { kind = "value", value = { type = "string", value = args.args[1] } } }
            end
            return { ok = true, value = args.value }
          end,
        }"#;
        let denied = LuaRuntime::new_with_capabilities(LuaPolicy::config(), &[]).unwrap();
        let error = denied.eval(source).unwrap_err();
        assert!(error.details.context[0].contains("capability denied: commands.register"));

        let runtime = LuaRuntime::new_with_capabilities(
            LuaPolicy::config(),
            &["commands.register".to_owned()],
        )
        .unwrap();
        runtime.eval(source).unwrap();
        let output = runtime
            .run_plugin_command("demo run", &serde_json::json!({"value": 42}))
            .unwrap();
        assert_eq!(output, serde_json::json!({"ok": true, "value": 42}));
        let cancellation = runtime.execution_cancellation();
        let context = LuaRunnerContext::from_current_process(
            &["typed".to_owned()],
            ExecutionInput::None,
            ExecutionOutputTarget::Value,
            ExecutionEffects::from_effects(&[ExecutionEffect::ReadFilesystem]),
            cancellation.atomic(),
        )
        .unwrap();
        let typed = runtime
            .run_plugin_command_with_context(
                "demo run",
                &context,
                Instant::now() + Duration::from_millis(50),
            )
            .unwrap();
        assert_eq!(typed.status_code(), 0);
        assert_eq!(
            typed.output,
            ExecutionOutput::Value {
                value: StructuredValue::String("typed".to_owned())
            }
        );
        let malformed_context = LuaRunnerContext::from_current_process(
            &["malformed".to_owned()],
            ExecutionInput::None,
            ExecutionOutputTarget::Value,
            ExecutionEffects::from_effects(&[ExecutionEffect::ReadFilesystem]),
            cancellation.atomic(),
        )
        .unwrap();
        let malformed = runtime
            .run_plugin_command_with_context(
                "demo run",
                &malformed_context,
                Instant::now() + Duration::from_millis(50),
            )
            .unwrap_err();
        assert_eq!(malformed.code, ErrorCode::Validation);
        assert!(malformed.details.context[0].contains("requires typed `output`"));
        cancellation.cancel();
        let cancelled = runtime
            .run_plugin_command_with_context(
                "demo run",
                &malformed_context,
                Instant::now() + Duration::from_millis(50),
            )
            .unwrap_err();
        assert_eq!(cancelled.code, ErrorCode::ResourceLimit);
        runtime.clear_cancellation();
        let invalid = runtime
            .run_plugin_command("demo run", &serde_json::json!([42]))
            .unwrap_err();
        assert_eq!(invalid.code, ErrorCode::Validation);
        assert!(invalid.details.context[0].contains("named object"));
    }

    #[test]
    fn typed_event_handlers_are_ordered_and_fail_in_isolation() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        runtime
            .eval(
                r#"
                quirl.events.subscribe {
                  name = "z_bad", events = { "command_plan" },
                  capabilities = { "events_observe" }, deadline_ms = 20,
                  observe = function(_) return "not actions" end,
                }
                quirl.events.subscribe {
                  name = "a_good", events = { "command_plan" },
                  capabilities = { "events_observe" }, deadline_ms = 20,
                  observe = function(event)
                    return { { action = "diagnose", message = event.data.source } }
                  end,
                }
                "#,
            )
            .unwrap();
        let reports = runtime
            .dispatch_extension_event(&ExtensionEvent::new(
                1,
                ExtensionEventData::CommandPlan {
                    source: "echo safe".to_owned(),
                    effects: Vec::new(),
                },
            ))
            .unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].handler, "a_good");
        assert_eq!(reports[0].actions.len(), 1);
        assert!(reports[0].error.is_none());
        assert_eq!(reports[1].handler, "z_bad");
        assert!(reports[1].error.is_some());
    }

    #[test]
    fn event_deadlines_and_mutation_rights_are_enforced() {
        let denied = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = denied
            .eval(
                r#"quirl.events.subscribe {
                  name = "rewrite", events = { "command_plan" },
                  capabilities = { "events_observe", "plan_rewrite" }, deadline_ms = 10,
                  observe = function(_) return {} end,
                }"#,
            )
            .unwrap_err();
        assert!(error.details.context[0].contains("capability denied: plan.rewrite"));

        let runtime = LuaRuntime::new_with_capabilities(
            LuaPolicy {
                instruction_limit: 100_000_000,
                wall_time: Duration::from_secs(1),
                ..LuaPolicy::config()
            },
            &["events.observe".to_owned()],
        )
        .unwrap();
        runtime
            .eval(
                r#"quirl.events.subscribe {
                  name = "slow", events = { "command_plan" },
                  capabilities = { "events_observe" }, deadline_ms = 1,
                  observe = function(_) while true do end end,
                }"#,
            )
            .unwrap();
        let reports = runtime
            .dispatch_extension_event(&ExtensionEvent::new(
                1,
                ExtensionEventData::CommandPlan {
                    source: "true".to_owned(),
                    effects: Vec::new(),
                },
            ))
            .unwrap();
        assert_eq!(
            reports[0].error.as_ref().unwrap().code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn contributions_require_safe_fallbacks_and_deadline_boundaries() {
        let runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        runtime
            .eval(
                r#"quirl.extension.contribute {
                  kind = "panel", name = "cluster", deadline_ms = 10,
                  plain_fallback = "cluster unavailable",
                  provide = function(_) return "healthy" end,
                }"#,
            )
            .unwrap();
        let value = runtime
            .invoke_contribution(ContributionKind::Panel, "cluster", &serde_json::json!({}))
            .unwrap();
        assert_eq!(value, serde_json::json!("healthy"));

        let unsafe_runtime = LuaRuntime::new(LuaPolicy::config()).unwrap();
        let error = unsafe_runtime
            .eval(
                "quirl.extension.contribute { kind = 'panel', name = 'raw', deadline_ms = 10, plain_fallback = '\\27[31mraw', provide = function() return 'x' end }",
            )
            .unwrap_err();
        assert!(error.details.context[0].contains("terminal control"));
    }

    #[test]
    fn runner_v0_migrates_and_future_versions_fail_before_main() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let context = runner_context(&runtime, &["alpha", "beta"], ExecutionInput::None);
        let legacy = runtime
            .run_source_with_context(
                "return { main = function(ctx) return { status = 7, first = ctx.args[1], cwd = ctx.cwd } end }",
                "legacy.lua",
                &context,
            )
            .unwrap();
        assert_eq!(legacy.status_code(), 7);
        let ExecutionOutput::Value { value } = legacy.output else {
            panic!("legacy migration must produce one typed value");
        };
        assert_eq!(value.json_value()["first"], "alpha");
        assert_eq!(value.json_value()["cwd"], "/tmp/quirl-runner");

        let error = runtime
            .run_source_with_context(
                "return { abi_version = 2, main = function() error('must not run') end }",
                "future.lua",
                &context,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.details.context[0].contains("requested: 2"));
    }

    #[test]
    fn runner_v1_context_preserves_typed_input_environment_effects_and_cancellation() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let context = runner_context(
            &runtime,
            &["deploy"],
            ExecutionInput::Value(StructuredValue::Path("services.toml".to_owned())),
        );
        let outcome = runtime
            .run_source_with_context(
                r#"return {
                  abi_version = 1,
                  main = function(ctx)
                    return {
                      abi_version = 1, ok = true, status = 3,
                      output = { kind = "value", value = {
                        type = "record", value = {
                          argument = { type = "string", value = ctx.args[1] },
                          environment = { type = "string", value = ctx.env.QUIRL_TEST_ENV },
                          cwd = { type = "path", value = ctx.cwd },
                          input_kind = { type = "string", value = ctx.input.kind },
                          input_type = { type = "string", value = ctx.input.content.type },
                          effect = { type = "string", value = ctx.effects[2] },
                          cancelled = { type = "bool", value = ctx.cancellation.is_cancelled() },
                          output_kind = { type = "string", value = ctx.output.kind },
                        }
                      }}
                    }
                  end
                }"#,
                "context.lua",
                &context,
            )
            .unwrap();
        assert_eq!(outcome.status_code(), 3);
        let ExecutionOutput::Value { value } = outcome.output else {
            panic!("typed runner must preserve value output");
        };
        let json = value.json_value();
        assert_eq!(json["argument"], "deploy");
        assert_eq!(json["environment"], "visible");
        assert_eq!(json["cwd"], "/tmp/quirl-runner");
        assert_eq!(json["input_kind"], "value");
        assert_eq!(json["input_type"], "path");
        assert_eq!(json["effect"], "spawn_process");
        assert_eq!(json["cancelled"], false);
        assert_eq!(json["output_kind"], "value");
    }

    #[test]
    fn runner_structured_shell_error_round_trips_and_rejects_unknown_fields() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let context = runner_context(&runtime, &[], ExecutionInput::None);
        let error = runtime
            .run_source_with_context(
                r#"return { abi_version = 1, main = function()
                  return { abi_version = 1, ok = false, error = {
                    code = "invalid_argument", message = "bad deployment target",
                    labels = {{ source = "deploy prod", start = 7, ["end"] = 11, message = "unknown target" }},
                    context = {"target: prod"}, help = {"Use staging"},
                    command = "deploy prod", exit_status = 64,
                  }}
                end }"#,
                "structured-error.lua",
                &context,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(error.message, "bad deployment target");
        assert_eq!(error.details.labels[0].start, 7);
        assert_eq!(error.details.labels[0].end, 11);
        assert_eq!(error.details.context, ["target: prod"]);
        assert_eq!(error.details.help, ["Use staging"]);
        assert_eq!(error.details.command.as_deref(), Some("deploy prod"));
        assert_eq!(error.details.exit_status, Some(64));

        let unknown = runtime
            .run_source_with_context(
                r#"return { abi_version = 1, main = function()
                  return { abi_version = 1, ok = true, status = 0,
                    output = { kind = "value", value = { type = "nothing" } }, surprise = true }
                end }"#,
                "unknown-result.lua",
                &context,
            )
            .unwrap_err();
        assert_eq!(unknown.code, ErrorCode::Validation);
        assert!(unknown.details.context[0].contains("unknown field"));
    }

    #[test]
    fn runner_rejects_hostile_returns_and_unbounded_stream_materialization() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let context = runner_context(&runtime, &[], ExecutionInput::None);
        let hostile = runtime
            .run_source_with_context(
                r#"return { abi_version = 1, main = function()
                  local cycle = {}; cycle.self = cycle
                  return { abi_version = 1, ok = true, status = 0,
                    output = { kind = "value", value = cycle } }
                end }"#,
                "hostile.lua",
                &context,
            )
            .unwrap_err();
        assert_eq!(hostile.code, ErrorCode::Validation);
        assert!(hostile.details.context[0].contains("cyclic"));

        let source = format!(
            r#"return {{ abi_version = 1, main = function()
              local values = {{}}
              for i = 1, {} do values[i] = {{ type = "nothing" }} end
              return {{ abi_version = 1, ok = true, status = 0,
                output = {{ kind = "values", values = values }} }}
            end }}"#,
            MAX_LUA_RUNNER_STREAM_VALUES + 1
        );
        let stream = runtime
            .run_source_with_context(&source, "stream-limit.lua", &context)
            .unwrap_err();
        assert_eq!(stream.code, ErrorCode::ResourceLimit);
        assert!(stream.message.contains("finite stream values"));
        assert!(stream.details.context[0].contains("observed: 513"));
    }

    #[test]
    fn runner_environment_error_and_context_cancellation_are_bounded() {
        let runtime = LuaRuntime::new(LuaPolicy::script()).unwrap();
        let environment = (0..=MAX_LUA_RUNNER_ENVIRONMENT_ENTRIES)
            .map(|index| (format!("KEY_{index}"), "value".to_owned()))
            .collect();
        let error = LuaRunnerContext::new(
            Vec::new(),
            environment,
            "/tmp".to_owned(),
            ExecutionInput::None,
            ExecutionOutputTarget::Value,
            ExecutionEffects::none(),
            Arc::clone(&runtime.cancelled),
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("observed: 257"));

        let context = runner_context(&runtime, &[], ExecutionInput::None);
        context.cancelled.store(true, Ordering::Relaxed);
        let cancelled = runtime
            .run_source_with_context(
                "return { abi_version = 1, main = function() error('must not run') end }",
                "cancelled.lua",
                &context,
            )
            .unwrap_err();
        assert_eq!(cancelled.code, ErrorCode::ResourceLimit);
        assert!(cancelled.details.context[0].contains("before Lua module evaluation"));
    }

    #[test]
    fn process_results_expose_structured_errors_and_bounded_stderr() {
        let process_host: ProcessHost = Arc::new(|_| {
            Ok(quirl_core::CommandOutcome {
                status: 9,
                stdout: Some("partial".to_owned()),
                stderr: Some("failed safely".to_owned()),
            })
        });
        let runtime = LuaRuntime::new_with_process_host(LuaPolicy::script(), process_host).unwrap();
        let value = runtime
            .eval(
                "local result = quirl.process.run('false'); return { code = result.error.code, status = result.error.exit_status, stderr = result.stderr }",
            )
            .unwrap();
        assert_eq!(value["code"], "invalid_command");
        assert_eq!(value["status"], 9);
        assert_eq!(value["stderr"], "failed safely");
    }
}
