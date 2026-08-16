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
  run = function(_ctx)
    return {
      abi_version = 1,
      ok = true,
      status = 0,
      output = {
        kind = "value",
        value = {
          type = "record",
          value = {
            platform = { type = "string", value = "quirl" },
            version = { type = "int", value = 1 },
          },
        },
      },
    }
  end,
}
