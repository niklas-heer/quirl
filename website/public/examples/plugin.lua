quirl.prompt.add_segment {
  name = "project",
  deadline_ms = 8,
  render = function(ctx)
    return ctx.project_name
  end,
}

quirl.completion.add_provider {
  command = "deploy --environment",
  complete = function(_ctx)
    return { "staging", "production" }
  end,
}
