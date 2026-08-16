---@meta quirl

---@class quirl.Result
---@field ok boolean
---@field value? any
---@field error? string

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
---@field symbols? quirl.PromptSymbols Auto never assumes a patched font; nerd_font enables Powerline glyphs explicitly.
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
---@field warning string #RRGGBB warning color.
---@field hint string #RRGGBB hint color.
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

---@class quirl.PluginCommand
---@field name string
---@field signature string
---@field summary string
---@field details string
---@field input_type string
---@field output_type string
---@field examples string[]
---@field effects string[]
---@field error_codes table<string, string>
---@field run fun(arguments: table): any

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

---Run a command through Quirl's compatibility shell.
---@param command string
---@return quirl.Result
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

---Register a typed, documented plugin command.
---@param spec quirl.PluginCommand
function quirl.plugin.command(spec) end

---Observe immutable typed shell events and return declared actions.
---@param spec quirl.EventSubscription
function quirl.events.subscribe(spec) end

---Register a typed catalog, completion, analysis, view, panel, or knowledge contribution.
---@param spec quirl.Contribution
function quirl.extension.contribute(spec) end
