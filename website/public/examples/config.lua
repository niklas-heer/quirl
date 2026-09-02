---@type quirl.Config
local config = quirl.config {
  schema_version = 5,
  editor = { keymap = "emacs", semantic_hints = true, banner = "full" },
  picker = { layout = "adaptive", preview = true },
  prompt = {
    -- auto detects terminals with bundled Nerd symbols and otherwise uses safe
    -- Unicode. Use "plain" for ASCII or "nerd_font" for other patched fonts.
    symbols = "auto",
    left = { "directory", "git_branch", "git_state" },
    right = { "rust_version", "jobs", "duration", "status" },
    transient = true,
  },
  ui = { theme = "tokyo-night", surface = "auto", statusline = { hints = true } },
  completion = { auto = false, min_chars = 2 },
}

return config
