---@type quirl.Config
local config = quirl.config {
  schema_version = 4,
  editor = { keymap = "emacs", semantic_hints = true, banner = "full" },
  picker = { layout = "adaptive", preview = true },
  prompt = {
    -- auto is Unicode-safe and never assumes a patched font. Use "plain" for
    -- ASCII everywhere, or opt in to Powerline/Nerd Font glyphs with "nerd_font".
    symbols = "auto",
    left = { "directory", "git_branch", "git_state" },
    right = { "rust_version", "jobs", "duration", "status" },
    transient = true,
  },
  ui = { theme = "tokyo-night", surface = "auto", statusline = { hints = true } },
  completion = { auto = false, min_chars = 2 },
}

return config
