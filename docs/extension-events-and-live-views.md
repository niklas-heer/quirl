# Extension events, typed views, and live pipelines

Quirl 0.1 exposes a versioned, deny-unknown extension protocol without giving
plugins direct terminal or mutable shell access. The machine-readable installed
contract is available with:

```console
quirl events schema --format json
```

## Immutable event records

Every record carries `protocol_version`, a strictly increasing `sequence`, and
one typed payload. The current event kinds cover session start/restore,
directory changes, command plans, execution progress, output, cancellation,
result, and error. `QUIRL_SESSION_ID` opts an invocation into an explicit
session-restore record. `quirl events validate trace.json` validates a complete trace without
loading an extension. Output text containing terminal control bytes is rejected.

Lua handlers register through `quirl.events.subscribe` with a stable name,
event kinds, a 1–250 ms deadline, and explicit capabilities. Observation is the
baseline right. Plan rewrites, environment changes, output reads, and execution
blocking are separate grants. A handler returns only declared action records;
Rust validates every action against its grants. Handlers execute in stable name
order within a plugin, while separate plugin runtimes are observed in parallel
and their actions are composed back in stable plugin order. Plan rewrite,
environment mutation, and blocking actions are applied before execution;
result annotations render separately from command output. A timeout, malformed
return, or denied action becomes that handler's diagnostic and does not prevent
later handlers from running.

```lua
quirl.events.subscribe {
  name = "deployment_guard",
  events = { "command_plan", "result" },
  capabilities = { "events_observe" },
  deadline_ms = 20,
  observe = function(event)
    return { { action = "diagnose", message = "observed " .. event.data.kind } }
  end,
}
```

Output payload text is redacted unless the handler has `output.read`. Extension
registrations are capability-gated by the plugin lock; declaring a right in the
handler does not create that right.

## Contributions and terminal safety

`quirl.extension.contribute` registers the Phase 3 surfaces composed today:
catalog, completion, or panel providers. Every registration has a bounded
deadline and a kind-specific permission (`catalog.register`,
`completion.register`, or `ui.panel`). Names collide only within the same
contribution kind. Panel providers must declare a non-empty plain-text
fallback. Catalog results use complete `CommandSpec` records and cannot shadow
installed paths or IDs; completion items and panels use deny-unknown shapes.

Plugins return typed values or plain text, never terminal paint. The core and UI
models reject ESC, CSI, NUL, and other raw control bytes. Quirl retains ownership
of layout, color, focus, accessibility, and terminal cleanup.

## Line-oriented panels

The initial directory and process panels are useful without a full-screen TUI:

```console
quirl view directory .
quirl view directory . --format json
quirl view processes
quirl view panel cluster
```

Text output is a stable line-oriented table. JSON returns the same validated
panel model. Empty models render their required plain fallback.

## Bounded live pipelines

`quirl watch` repeatedly evaluates a native typed data expression. Sampling is
finite by default, `Ctrl-C` cancels between pipeline stages and during refresh
waits, intervals are bounded, and completed samples use a capacity-limited
retention queue. When completed samples exceed retention capacity, the oldest
samples are dropped and the snapshot reports the count. This is bounded sample
history, not producer/consumer stream backpressure.

```console
quirl watch pwd --samples 3 --interval-ms 250
quirl watch 'ls . | length' --samples 20 --capacity 5 --format json
```

Text mode emits one accessible JSON value per line. JSON mode emits the bounded
snapshot with `capacity`, `dropped`, `cancelled`, and ordered `samples` fields.
