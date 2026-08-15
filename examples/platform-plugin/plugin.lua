quirl.plugin.command {
  name = "platform-demo run",
  signature = "platform-demo run",
  summary = "Return a typed demonstration record",
  details = "Returns one deterministic record without ambient authority.",
  input_type = "Nothing",
  output_type = "Record",
  examples = { "platform-demo run" },
  effects = { "read_filesystem" },
  error_codes = { ["0"] = "success" },
  run = function()
    return { platform = "quirl", version = 1 }
  end,
}
