---@meta quirl

---@class quirl.Result
---@field ok boolean
---@field value? any
---@field error? string

---@class quirl.Config
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
