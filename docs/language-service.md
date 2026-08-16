# Quirl language service

`quirl lsp` is Quirl's deterministic editor service for Lua extension files
and native Quirl scripts. `.qrl` is the canonical native extension; `.quirl`
and `.🌀` are accepted input aliases. It communicates over standard LSP `Content-Length`
framing on stdin/stdout, so editors can launch the Quirl binary directly.

The server accepts messages up to 4 MiB. Framing headers are limited to 8 KiB
and 64 fields, duplicate `Content-Length` fields are rejected, and truncated
bodies fail without dispatch. It implements:

- `initialize`, `initialized`, `shutdown`, and `exit`
- `textDocument/didOpen`, `didChange`, and `didClose` with full-document sync
- `textDocument/completion`, `hover`, `signatureHelp`, and `diagnostic`
- `quirl/moduleDocs`, which returns generated Markdown for the Lua host module
  and installed command catalog

Lua completion, hover, signatures, and module docs are derived from
`quirl_lua::HOST_API`. Diagnostics use the restricted Lua compiler and linter,
but never call the resulting function or evaluate document text. This means
editing a file containing `os.execute(...)`, an infinite loop, or plugin
registration cannot cause the language server to perform that operation.

For native Quirl files, command and option intelligence comes from the same
versioned `Catalog` used by the REPL and `quirl complete`. The Phase 2 service
also reports deterministic structural diagnostics for mismatched delimiters,
unterminated strings, and empty pipeline stages. It does not spawn commands or
resolve ambient shell state.

The CLI composition root supplies the complete native analyzer to the LSP.
Command blocks continue to use `quirl-syntax`; inline and explicit data bodies
use `quirl-data`'s bounded parser and retain its UTF-8 byte spans and
`quirl-data` diagnostic source. The callback receives only the already-bounded
document text and has no process, filesystem, adapter, or evaluator capability.
This preserves ADR 0016: `quirl-lsp` does not depend on `quirl-data`.

## Editor command

Configure an LSP client with this stdio command:

```text
quirl lsp
```

Use `lua` as the language id for Lua files and `quirl` for native Quirl files,
regardless of whether they use `.qrl`, `.quirl`, or `.🌀`.
The server uses UTF-16 LSP positions and publishes a complete replacement
diagnostic set after every open or full-text change.

The service retains at most 128 open documents and 16 MiB of aggregate
document state. Each URI is limited to 8 KiB, each language identifier to 64
bytes, and each document body to 1 MiB. `didChange` accepts exactly one
full-document replacement, matching the advertised full-sync mode. Duplicate
`didOpen` notifications atomically replace the existing document; a rejected
open or change preserves the prior text, version, and accounting. Close,
shutdown, and exit release retained state. These are UTF-8 byte limits, and
limit failures include configured and observed usage where it is safe to do
so.

## Custom module documentation request

Clients and documentation tools can request the exact generated module view:

```json
{"jsonrpc":"2.0","id":1,"method":"quirl/moduleDocs","params":{}}
```

The response is a Markdown marked string. Because it is generated at request
time from `HOST_API` and the loaded catalog, editor help does not drift from
the runtime and CLI command metadata.
