---@meta quirl

---@class quirl.Result
---@field ok boolean
---@field value? any
---@field error? string

---@class quirl.Config
---@field schema_version integer
---@field editor table
---@field picker table
---@field prompt table

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
