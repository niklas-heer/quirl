# Quirl MCP server

Run the explicitly scoped stdio server with the capabilities an MCP client
actually needs:

```sh
quirl serve mcp --capabilities catalog,complete,check,format
```

The server is newline-delimited JSON-RPC on standard input/output. It exposes
only the named tools (`quirl_catalog`, `quirl_complete`, `quirl_check`, and
`quirl_format`); an ungranted tool is not callable. Source-oriented check and
format requests accept bounded in-memory text only. They do not read paths,
load plugins, start a shell, write files, or open the network.

Modern clients negotiate `2026-07-28` with `server/discover`. Every modern
request must include its `_meta.io.modelcontextprotocol` envelope with that
protocol version, client information, and client capabilities. Responses carry
the matching server-information envelope, and `tools/list` supplies an
immutable deterministic cache hint.

For existing clients, Quirl also accepts legacy `initialize` negotiation for
`2025-03-26`, `2025-06-18`, and `2025-11-25`, followed by the usual
`notifications/initialized`, `tools/list`, and `tools/call` messages. A
connection selects exactly one era; modern metadata and legacy negotiation
cannot be mixed.

Messages, nested JSON, tool input, and tool output are bounded. Oversized tool
results return a JSON-RPC error while the stdio connection remains usable.
