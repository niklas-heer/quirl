# Data runtime in 0.1.0

`quirl data` evaluates native structured values and renders them explicitly:

```text
quirl data 'open users.csv | where enabled == true | select name' --format table
quirl data 'open config.toml | get service.port' --format plain
quirl data 'open users.json' --format json
quirl data '^external printf "{\"ok\":true}" | from json'
```

`--format json` emits a tagged `Value` or `Stream` envelope so a script
does not need to infer whether an array is an ordinary value or a pipeline
stream. Values themselves retain an ABI tag (`int`, `decimal`, `path`, `size`,
and so on) rather than making domain values look like strings. The current
native parser emits JSON-compatible scalar/list/record values; domain tags are
available to adapters and host boundaries. `Option`, `Result` (`ok` or
`error`), and `Task` (`pending`, `complete`, `cancelled`, or `failed`) remain
explicit in the same ABI. Failures remain `ShellError` until a caller
intentionally puts one in a result or task envelope.

Supported adapters are JSON, YAML, TOML, CSV, uncompressed POSIX tar archive
inspection, and filesystem rows (`files [path]`, with `ls` retained as an
alias). CSV and tar entries are pull-based: a row is parsed when the consumer
asks for it, and cancellation is checked before each pull. The public CLI
writes plain and JSON rows directly to stdout as it pulls them, keeping those
paths `O(window)` rather than constructing a complete output string. It keeps
that laziness through `where`, `get`, `select`, and `take`; `sort` and table
rendering are deliberate bounded collection boundaries. `DataRuntime::render`
is a collected convenience API, while `render_to` is the streaming boundary.
JSON, YAML, TOML, and directory entries are validated then materialized because
their current underlying parsers expose whole-document APIs.

Every adapter enforces the default 8 MiB file size, 100,000 row, 256 field,
64 nesting-depth, and 256 KiB expression limits. Library callers can set
`DataLimits` explicitly. CSV requires a single unique header row and does not
support multiline quoted fields. Tar inspection lists headers only: it never
extracts entries and intentionally supports only uncompressed POSIX `.tar`
archives (not zip, gzip, bzip2, xz, or PAX/GNU extended-name semantics); each
header checksum is verified before the entry is reported.

Byte/value crossings are explicit. `lines` turns one string byte value into a
lazy stream of newline-delimited strings; `from json` parses a string byte
value (or each string stream item); and `to json` serializes a value or each
stream item. `^external <command>` is the only external byte producer. A
standalone `DataRuntime` has no ambient process capability and rejects it. The
CLI injects the sandboxed process host with a 2-second deadline and a 1 MiB
combined retained-output limit; cancellation is passed through the shareable
token used for that command. Non-zero exits remain `ShellError` failures with
the bounded stderr context, rather than being silently converted into values.

This release intentionally does not implement SQLite, zip/compressed archive
inspection, HTTP, background task scheduling, or a
fully streaming JSON/YAML/TOML parser. HTTP is not implied by `open`: a future
HTTP adapter must expose explicit request, timeout, redirect, byte, and
capability limits. Those remain design targets rather than silently available
0.1.0 behavior.
