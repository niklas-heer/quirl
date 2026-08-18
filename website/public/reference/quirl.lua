---@meta quirl

---@class quirl.ErrorLabel
---@field source? string
---@field start integer Inclusive UTF-8 byte offset.
---@field end integer Exclusive UTF-8 byte offset.
---@field message string

---@alias quirl.ErrorCode 'invalid_command'|'invalid_argument'|'data'|'io'|'process_spawn'|'script_read'|'lua'|'validation'|'resource_limit'

---@class quirl.ShellError
---@field code quirl.ErrorCode
---@field message string
---@field labels? quirl.ErrorLabel[]
---@field context? string[]
---@field help? string[]
---@field command? string
---@field exit_status? integer

---@class quirl.Result
---@field ok boolean
---@field value? any
---@field error? quirl.ShellError

---@class quirl.ProcessResult: quirl.Result
---@field status integer
---@field value string Captured stdout.
---@field stderr string Captured stderr.

---@alias quirl.ExecutionEffect 'read_filesystem'|'write_filesystem'|'spawn_process'|'change_directory'

---@class quirl.CancellationContext
---@field is_cancelled fun(): boolean Returns the shared cancellation flag without clearing it.

---@class quirl.Context
---@field abi_version 1
---@field args string[] Bounded arguments in source order.
---@field env table<string, string> Immutable bounded environment snapshot.
---@field cwd string UTF-8 working directory captured before evaluation.
---@field input table Shared deny-unknown ExecutionInput representation.
---@field output table Shared value-only ExecutionOutputTarget representation.
---@field cancellation quirl.CancellationContext
---@field effects quirl.ExecutionEffect[] Effects declared before dispatch.

---@class quirl.RunnerResult
---@field abi_version 1
---@field ok boolean
---@field status? integer Required exactly when ok is true.
---@field output? table Typed value or bounded finite values output; live streams are not transferable.
---@field error? quirl.ShellError Required exactly when ok is false.

---@class quirl.RunnerModule
---@field abi_version 1
---@field main fun(context: quirl.Context): quirl.RunnerResult

---@alias quirl.PromptSymbols 'auto'|'plain'|'unicode'|'nerd_font'
---@alias quirl.WelcomeBanner 'full'|'compact'|'none'
---@alias quirl.Surface 'auto'|'rich'|'simple'

---@class quirl.EditorConfig
---@field keymap? 'emacs'|'vim'|'helix' Emacs is the complete default.
---@field semantic_hints? boolean
---@field banner? quirl.WelcomeBanner

---@class quirl.PickerConfig
---@field layout? 'adaptive'|'bottom'|'full'
---@field preview? boolean

---@class quirl.PromptConfig
---@field symbols? quirl.PromptSymbols Auto detects terminals with bundled Nerd symbols; nerd_font enables them explicitly elsewhere.
---@field left? string[] Ordered prompt segments before the input.
---@field right? string[] Ordered prompt segments aligned on the right.
---@field transient? boolean Collapse accepted input to one scrollback line before execution.

---@class quirl.ThemeColors
---@field accent_command string #RRGGBB color for command-mode accents.
---@field accent_data string #RRGGBB color for data-mode accents.
---@field context_primary string #RRGGBB color for primary context.
---@field context_secondary string #RRGGBB color for secondary context.
---@field muted string #RRGGBB color for subdued text.
---@field border string #RRGGBB color for borders.
---@field status_background string #RRGGBB status background color.
---@field error string #RRGGBB error color.
---@field warning string #RRGGBB color for warnings.
---@field hint string #RRGGBB color for hints.
---@field string string #RRGGBB string syntax color.
---@field operator string #RRGGBB operator syntax color.
---@field expansion string #RRGGBB expansion syntax color.
---@field number string #RRGGBB number syntax color.

---@class quirl.StatuslineConfig
---@field hints? boolean

---@class quirl.UiConfig
---@field surface? quirl.Surface
---@field theme? string Built-in or custom theme name; defaults to tokyo-night.
---@field themes? table<string, quirl.ThemeColors> At most 32 custom themes.
---@field statusline? quirl.StatuslineConfig

---@class quirl.CompletionConfig
---@field auto? boolean
---@field min_chars? integer

---@class quirl.Config
---@field schema_version? integer
---@field editor? quirl.EditorConfig
---@field picker? quirl.PickerConfig
---@field prompt? quirl.PromptConfig
---@field ui? quirl.UiConfig
---@field completion? quirl.CompletionConfig

---@class quirl.PromptSegment
---@field name string
---@field deadline_ms? integer
---@field render fun(context: table): string?

---@class quirl.CompletionProvider
---@field command string
---@field complete fun(context: table): table

---@alias quirl.PluginInputType 'Nothing'|'Bool'|'Int'|'UInt'|'Decimal'|'String'|'List'|'Record'|'Path'|'Duration'|'Size'|'DateTime'|'Pattern'
---@alias quirl.PluginOutputType quirl.PluginInputType|'Values<Nothing>'|'Values<Bool>'|'Values<Int>'|'Values<UInt>'|'Values<Decimal>'|'Values<String>'|'Values<List>'|'Values<Record>'|'Values<Path>'|'Values<Duration>'|'Values<Size>'|'Values<DateTime>'|'Values<Pattern>'

---@class quirl.PluginCommand
---@field name string
---@field signature string
---@field summary string
---@field details string
---@field input_type quirl.PluginInputType Exact top-level input kind; Nothing accepts no input.
---@field output_type quirl.PluginOutputType Exact value kind or bounded finite Values<T>; live streams are unsupported.
---@field examples string[]
---@field effects string[]
---@field error_codes table<string, string>
---@field run fun(context: quirl.Context): quirl.RunnerResult

---@alias quirl.EventKind 'session_start'|'session_restore'|'directory_changed'|'command_plan'|'execution_progress'|'output'|'cancellation'|'result'|'error'
---@alias quirl.ExtensionCapability 'events_observe'|'plan_rewrite'|'environment_mutate'|'output_read'|'execution_block'|'catalog_contribute'|'completion_contribute'|'ui_panel'
---@class quirl.EventSubscription
---@field name string
---@field events quirl.EventKind[]
---@field capabilities quirl.ExtensionCapability[]
---@field deadline_ms integer
---@field observe fun(event: table): table[]

---@alias quirl.ContributionKind 'catalog'|'completion'|'panel'
---@class quirl.Contribution
---@field kind quirl.ContributionKind
---@field name string
---@field deadline_ms integer
---@field plain_fallback? string
---@field provide fun(context: table): any

quirl = {}

---Return the current working directory.
---@return string
function quirl.cwd() end

---Run a command through the composed bounded native process host.
---@param command string
---@return quirl.ProcessResult
function quirl.process.run(command) end

---Return configuration for Rust schema validation.
---@param value quirl.Config
---@return quirl.Config
function quirl.config(value) end

---Register a deadline-bounded prompt segment.
---@param spec quirl.PromptSegment
function quirl.prompt.add_segment(spec) end

---Register a semantic completion provider.
---@param spec quirl.CompletionProvider
function quirl.completion.add_provider(spec) end

---Register a documented command with exact ABI-v1 value I/O; live streams are rejected.
---@param spec quirl.PluginCommand
function quirl.plugin.command(spec) end

---Observe immutable typed shell events and return declared actions.
---@param spec quirl.EventSubscription
function quirl.events.subscribe(spec) end

---Register a typed catalog, completion, or panel contribution.
---@param spec quirl.Contribution
function quirl.extension.contribute(spec) end
