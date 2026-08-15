---@type quirl.Config
local config = quirl.config {
  editor = { keymap = "helix", semantic_hints = true },
  picker = { layout = "adaptive", preview = true },
  prompt = {
    left = { "directory", "git_branch", "git_state" },
    right = { "jobs", "duration", "status" },
  },
}

return config
