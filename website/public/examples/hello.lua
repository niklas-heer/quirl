#!/usr/bin/env -S quirl run

---@param ctx table
---@return table
local function main(ctx)
  local name = ctx.args[1] or "world"
  return {
    greeting = "Hello, " .. name .. "!",
    cwd = quirl.cwd(),
    runtime = _VERSION,
  }
end

return { main = main }
