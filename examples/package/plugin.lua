local plugin = {}

local function shell_quote(word)
  word = tostring(word or "")
  if word:match("^[%w_./-]+$") then
    return word
  end
  return "'" .. word:gsub("'", "'\\''") .. "'"
end

function plugin.deploy(environment)
  if type(environment) == "table" then
    environment = environment.environment or environment[1]
  end
  return quirl.process.run("deploy " .. shell_quote(environment))
end

return plugin
