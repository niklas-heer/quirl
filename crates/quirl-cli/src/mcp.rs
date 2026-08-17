//! Deliberately small, capability-gated MCP server over stdio.
//!
//! This adapter does not expose a shell, filesystem paths, plugins, or network
//! access. Its check and format tools work only on bounded source supplied in a
//! JSON-RPC request, so an MCP client cannot obtain ambient execution rights by
//! discovering a tool.

use crate::lua_worker::LuaWorkerRuntime as LuaRuntime;
use clap::{Subcommand, ValueEnum};
use quirl_catalog::Catalog;
use quirl_core::{escape_terminal_controls, ErrorCode, ShellError};
use quirl_lua::format_source;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";
const LEGACY_PROTOCOL_VERSIONS: [&str; 3] = ["2025-03-26", "2025-06-18", "2025-11-25"];
pub const MCP_SCHEMA_VERSION: u32 = 1;
pub const MCP_SCHEMA_DESCRIPTOR: &str = "quirl.mcp@1{transport:stdio-json-rpc-lines;request:deny_unknown{jsonrpc:'2.0';id:null|string|number;method:string<=128;params:Value<=262144,depth<=32};era:modern-2026-07-28(server/discover,per-request-_meta)|legacy-2025-03-26-to-2025-11-25(initialize);methods:tools/list|tools/call;tools:catalog|complete|check|format;capability_grants:explicit;source:no-filesystem,no-network,no-execution;limits:message<=1048576,source<=262144,response<=1048576}";
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_TOOL_INPUT_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Subcommand)]
pub enum ServeCommand {
    /// Serve a bounded capability-gated Model Context Protocol subset over stdio.
    Mcp {
        /// Comma-separated tools to expose; no authority is granted by default.
        #[arg(long, value_delimiter = ',', value_enum, required = true)]
        capabilities: Vec<McpCapability>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq, PartialOrd, Ord)]
pub enum McpCapability {
    Catalog,
    Complete,
    Check,
    Format,
}

impl McpCapability {
    fn tool_name(self) -> &'static str {
        match self {
            Self::Catalog => "quirl_catalog",
            Self::Complete => "quirl_complete",
            Self::Check => "quirl_check",
            Self::Format => "quirl_format",
        }
    }
}

pub fn execute(command: ServeCommand) -> Result<i32, ShellError> {
    match command {
        ServeCommand::Mcp { capabilities } => {
            let stdin = io::stdin();
            let stdout = io::stdout();
            serve(&mut stdin.lock(), &mut stdout.lock(), capabilities)?;
            Ok(0)
        }
    }
}

fn serve(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    capabilities: Vec<McpCapability>,
) -> Result<(), ShellError> {
    let mut server = McpServer::new(capabilities);
    while let Some(bytes) = read_message(reader)? {
        if bytes.is_empty() {
            continue;
        }
        let response = server.handle_bytes(&bytes);
        if let Some(response) = response {
            write_message(writer, &bounded_response(response))?;
        }
    }
    Ok(())
}

fn bounded_response(response: Response) -> Response {
    if serialized_response_size(&response) <= MAX_MESSAGE_BYTES {
        response
    } else {
        let mut error = Response::error(
            response.id,
            -32000,
            "MCP tool result exceeds the configured response limit",
        );
        error.meta = response.meta;
        error
    }
}

#[derive(Debug)]
struct McpServer {
    era: Option<ProtocolEra>,
    capabilities: Vec<McpCapability>,
    catalog: Catalog,
}

impl McpServer {
    fn new(mut capabilities: Vec<McpCapability>) -> Self {
        capabilities.sort();
        capabilities.dedup();
        Self {
            era: None,
            capabilities,
            // Plugins and user configuration are intentionally not loaded: an
            // MCP process is an authority boundary, not an interactive shell.
            catalog: Catalog::builtin(),
        }
    }

    fn handle_bytes(&mut self, bytes: &[u8]) -> Option<Response> {
        let raw = match std::str::from_utf8(bytes) {
            Ok(raw) => raw,
            Err(_) => return Some(Response::error(Value::Null, -32700, "request is not UTF-8")),
        };
        if raw.len() > MAX_MESSAGE_BYTES {
            return Some(Response::error(
                Value::Null,
                -32600,
                "request exceeds the MCP message limit",
            ));
        }
        let value: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(_) => {
                return Some(Response::error(
                    Value::Null,
                    -32700,
                    "invalid JSON-RPC JSON",
                ))
            }
        };
        if json_depth(&value) > MAX_DEPTH {
            return Some(Response::error(
                request_id(&value),
                -32600,
                "request exceeds the JSON nesting limit",
            ));
        }
        let id = request_id(&value);
        let notification = id.is_null();
        let request: Request = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(_) => {
                return (!notification)
                    .then(|| Response::error(id, -32600, "invalid JSON-RPC request"));
            }
        };
        if request.jsonrpc != "2.0" || !valid_id(&request.id) {
            return (!notification)
                .then(|| Response::error(id, -32600, "invalid JSON-RPC request"));
        }

        let result = match request.method.as_str() {
            "initialize" => self.initialize(request.params),
            "server/discover" => self.discover(request.params),
            "notifications/initialized" => match self.era {
                Some(ProtocolEra::Legacy(_)) => return None,
                Some(ProtocolEra::Modern) => Err(RpcError::new(
                    -32600,
                    "legacy initialized notification cannot follow modern discovery",
                )),
                None => Err(RpcError::new(-32000, "initialize must be called first")),
            },
            "tools/list" => self
                .negotiated_params(request.params)
                .and_then(|params| self.tools_list(params)),
            "tools/call" => self
                .negotiated_params(request.params)
                .and_then(|params| self.tools_call(params)),
            _ => Err(RpcError::new(-32601, "MCP method is not implemented")),
        };
        if notification {
            None
        } else {
            Some(match result {
                Ok(result) => Response::success(id, result, self.era),
                Err(error) => Response::error_for_era(id, error.code, &error.message, self.era),
            })
        }
    }

    fn initialize(&mut self, params: Value) -> Result<Value, RpcError> {
        if self.era.is_some() || params.get("_meta").is_some() {
            return Err(RpcError::new(
                -32600,
                "legacy initialize cannot be mixed with modern MCP metadata",
            ));
        }
        let params: InitializeParams = parse_params(params)?;
        let _ = (&params.capabilities, &params.client_info);
        let version = LEGACY_PROTOCOL_VERSIONS
            .iter()
            .copied()
            .find(|version| *version == params.protocol_version)
            .ok_or_else(|| RpcError::new(-32602, "unsupported legacy MCP protocol version"))?;
        self.era = Some(ProtocolEra::Legacy(version));
        Ok(json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "quirl", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Only explicitly granted, source-only Quirl tools are available. No filesystem, network, plugin, or shell execution is exposed."
        }))
    }

    fn discover(&mut self, params: Value) -> Result<Value, RpcError> {
        if self.era.is_some() {
            return Err(RpcError::new(
                -32600,
                "MCP protocol era is already negotiated for this connection",
            ));
        }
        let params = modern_params(params)?;
        let _: EmptyParams = parse_params(params)?;
        self.era = Some(ProtocolEra::Modern);
        Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "quirl", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Only explicitly granted, source-only Quirl tools are available. No filesystem, network, plugin, or shell execution is exposed."
        }))
    }

    fn negotiated_params(&self, params: Value) -> Result<Value, RpcError> {
        match self.era {
            Some(ProtocolEra::Legacy(_)) => {
                if params.get("_meta").is_some() {
                    Err(RpcError::new(
                        -32600,
                        "modern MCP metadata cannot be mixed into a legacy connection",
                    ))
                } else {
                    Ok(params)
                }
            }
            Some(ProtocolEra::Modern) => modern_params(params),
            None => Err(RpcError::new(-32000, "MCP discovery must be called first")),
        }
    }

    fn tools_list(&self, params: Value) -> Result<Value, RpcError> {
        self.require_initialized()?;
        let _: EmptyParams = parse_params(params)?;
        let tools = self
            .capabilities
            .iter()
            .map(|capability| tool_definition(*capability))
            .collect::<Vec<_>>();
        let mut result = json!({ "tools": tools });
        if matches!(self.era, Some(ProtocolEra::Modern)) {
            result["_meta"] = json!({
                "io.modelcontextprotocol/cache": {
                    "mode": "immutable",
                    "maxAgeSeconds": 3600,
                    "schemaVersion": MCP_SCHEMA_VERSION,
                    "schemaHash": quirl_core::schema_fingerprint(MCP_SCHEMA_DESCRIPTOR)
                }
            });
        }
        Ok(result)
    }

    fn tools_call(&self, params: Value) -> Result<Value, RpcError> {
        self.require_initialized()?;
        let params: ToolCallParams = parse_params(params)?;
        let capability = self
            .capabilities
            .iter()
            .copied()
            .find(|capability| capability.tool_name() == params.name)
            .ok_or_else(|| RpcError::new(-32602, "tool is not granted by this MCP server"))?;
        let result = match capability {
            McpCapability::Catalog => {
                let _: EmptyParams = parse_params(params.arguments)?;
                serde_json::to_value(&self.catalog)
                    .map_err(|_| RpcError::new(-32000, "could not serialize catalog"))?
            }
            McpCapability::Complete => self.complete(params.arguments)?,
            McpCapability::Check => self.check(params.arguments)?,
            McpCapability::Format => self.format(params.arguments)?,
        };
        Ok(tool_result(result))
    }

    fn complete(&self, arguments: Value) -> Result<Value, RpcError> {
        let arguments: CompleteArguments = parse_params(arguments)?;
        bounded_text(&arguments.input, "completion input")?;
        let cursor = arguments.cursor.unwrap_or(arguments.input.len());
        if cursor > arguments.input.len() || !arguments.input.is_char_boundary(cursor) {
            return Err(RpcError::new(
                -32602,
                "completion cursor must be a UTF-8 boundary",
            ));
        }
        serde_json::to_value(self.catalog.complete(&arguments.input, cursor))
            .map_err(|_| RpcError::new(-32000, "could not serialize completions"))
    }

    fn check(&self, arguments: Value) -> Result<Value, RpcError> {
        let arguments: SourceArguments = parse_params(arguments)?;
        bounded_text(&arguments.source, "source")?;
        let language = parse_language(&arguments.language)?;
        let error = match language {
            SourceLanguage::Lua => {
                LuaRuntime::check_source(&arguments.source, "mcp-input.lua").err()
            }
            SourceLanguage::Quirl => {
                crate::script::check_quirl_source(&arguments.source, "mcp-input.qrl").err()
            }
        };
        Ok(match error {
            Some(error) => json!({ "valid": false, "error": error }),
            None => json!({ "valid": true, "error": Value::Null }),
        })
    }

    fn format(&self, arguments: Value) -> Result<Value, RpcError> {
        let arguments: SourceArguments = parse_params(arguments)?;
        bounded_text(&arguments.source, "source")?;
        let source = arguments.source;
        let formatted = match parse_language(&arguments.language)? {
            SourceLanguage::Lua => format_source(&source),
            // Native Quirl is intentionally preserved by the CLI formatter.
            SourceLanguage::Quirl => source.clone(),
        };
        Ok(json!({ "source": formatted, "changed": formatted != source }))
    }

    fn require_initialized(&self) -> Result<(), RpcError> {
        self.era
            .is_some()
            .then_some(())
            .ok_or_else(|| RpcError::new(-32000, "initialize must be called first"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolEra {
    Legacy(&'static str),
    Modern,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default = "empty_object")]
    params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitializeParams {
    protocol_version: String,
    #[serde(default)]
    capabilities: Value,
    #[serde(default)]
    client_info: Option<ClientInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModernMetaEnvelope {
    #[serde(rename = "io.modelcontextprotocol")]
    context: ModernContext,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModernContext {
    protocol_version: String,
    client_info: Option<ClientInfo>,
    client_capabilities: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClientInfo {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCallParams {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteArguments {
    input: String,
    #[serde(default)]
    cursor: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceArguments {
    source: String,
    language: String,
}

#[derive(Debug, Clone, Copy)]
enum SourceLanguage {
    Lua,
    Quirl,
}

#[derive(Debug)]
struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Serialize)]
struct RpcErrorBody {
    code: i32,
    message: String,
}

impl Response {
    fn success(id: Value, result: Value, era: Option<ProtocolEra>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            meta: response_meta(era),
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Value, code: i32, message: &str) -> Self {
        Self::error_for_era(id, code, message, None)
    }

    fn error_for_era(id: Value, code: i32, message: &str, era: Option<ProtocolEra>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            meta: response_meta(era),
            result: None,
            error: Some(RpcErrorBody {
                code,
                message: safe_text(message),
            }),
        }
    }
}

fn response_meta(era: Option<ProtocolEra>) -> Option<Value> {
    matches!(era, Some(ProtocolEra::Modern)).then(|| {
        json!({
            "io.modelcontextprotocol/serverInfo": {
                "name": "quirl",
                "version": env!("CARGO_PKG_VERSION"),
                "protocolVersion": MCP_PROTOCOL_VERSION
            }
        })
    })
}

fn parse_params<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, RpcError> {
    if json_depth(&value) > MAX_DEPTH || serialized_size(&value) > MAX_TOOL_INPUT_BYTES {
        return Err(RpcError::new(
            -32602,
            "tool input exceeds the configured limit",
        ));
    }
    serde_json::from_value(value).map_err(|_| RpcError::new(-32602, "invalid tool parameters"))
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

/// Remove and validate the mandatory modern MCP per-request envelope.
fn modern_params(params: Value) -> Result<Value, RpcError> {
    let mut object = match params {
        Value::Object(object) => object,
        _ => return Err(RpcError::new(-32602, "modern MCP params must be an object")),
    };
    let meta = object
        .remove("_meta")
        .ok_or_else(|| RpcError::new(-32602, "modern MCP params require a _meta envelope"))?;
    let envelope: ModernMetaEnvelope = serde_json::from_value(meta)
        .map_err(|_| RpcError::new(-32602, "invalid modern MCP _meta envelope"))?;
    let _ = &envelope.context.client_capabilities;
    if let Some(client) = &envelope.context.client_info {
        let _ = (&client.name, &client.version);
    }
    if envelope.context.protocol_version != MCP_PROTOCOL_VERSION {
        return Err(RpcError::new(
            -32602,
            "modern MCP protocolVersion must be 2026-07-28",
        ));
    }
    Ok(Value::Object(object))
}

fn parse_language(value: &str) -> Result<SourceLanguage, RpcError> {
    match value {
        "lua" => Ok(SourceLanguage::Lua),
        "quirl" | "qrl" => Ok(SourceLanguage::Quirl),
        _ => Err(RpcError::new(-32602, "language must be lua or quirl")),
    }
}

fn bounded_text(value: &str, label: &str) -> Result<(), RpcError> {
    (value.len() <= MAX_TOOL_INPUT_BYTES)
        .then_some(())
        .ok_or_else(|| RpcError::new(-32602, format!("{label} exceeds the configured limit")))
}

fn tool_definition(capability: McpCapability) -> Value {
    match capability {
        McpCapability::Catalog => json!({
            "name": "quirl_catalog",
            "description": "Return Quirl's deterministic built-in semantic catalog.",
            "inputSchema": { "type": "object", "additionalProperties": false }
        }),
        McpCapability::Complete => json!({
            "name": "quirl_complete",
            "description": "Return deterministic built-in catalog completions for bounded input.",
            "inputSchema": { "type": "object", "additionalProperties": false,
                "properties": { "input": { "type": "string", "maxLength": MAX_TOOL_INPUT_BYTES }, "cursor": { "type": "integer", "minimum": 0 } },
                "required": ["input"] }
        }),
        McpCapability::Check => json!({
            "name": "quirl_check",
            "description": "Validate supplied Lua or native Quirl source without executing it.",
            "inputSchema": source_schema()
        }),
        McpCapability::Format => json!({
            "name": "quirl_format",
            "description": "Deterministically format supplied Lua source without writing files; native Quirl source is preserved.",
            "inputSchema": source_schema()
        }),
    }
}

fn source_schema() -> Value {
    json!({ "type": "object", "additionalProperties": false,
        "properties": { "source": { "type": "string", "maxLength": MAX_TOOL_INPUT_BYTES }, "language": { "type": "string", "enum": ["lua", "quirl", "qrl"] } },
        "required": ["source", "language"] })
}

fn tool_result(value: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": safe_text(&value.to_string()) }], "structuredContent": value, "isError": false })
}

fn safe_text(value: &str) -> String {
    escape_terminal_controls(value)
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn request_id(value: &Value) -> Value {
    value
        .get("id")
        .filter(|id| valid_id(id))
        .cloned()
        .unwrap_or(Value::Null)
}

fn valid_id(value: &Value) -> bool {
    value.is_null() || value.is_string() || value.is_number()
}

fn serialized_size(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

fn serialized_response_size(response: &Response) -> usize {
    serde_json::to_vec(response).map_or(usize::MAX, |bytes| bytes.len())
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or_default(),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or_default(),
        _ => 0,
    }
}

fn read_message(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>, ShellError> {
    let mut message = Vec::new();
    loop {
        let buffer = reader.fill_buf().map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read MCP stdio input")
                .with_context(error.to_string())
                .with_help("Keep MCP transport on a readable stdio stream")
        })?;
        if buffer.is_empty() {
            return Ok((!message.is_empty()).then_some(message));
        }
        let end = buffer.iter().position(|byte| *byte == b'\n');
        let chunk = end.map_or(buffer.len(), |index| index + 1);
        let content = if end.is_some() {
            &buffer[..chunk - 1]
        } else {
            &buffer[..chunk]
        };
        if message.len().saturating_add(content.len()) > MAX_MESSAGE_BYTES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "MCP message exceeds the configured byte limit",
            )
            .with_help("Send one JSON-RPC request no larger than 1 MiB"));
        }
        message.extend_from_slice(content);
        reader.consume(chunk);
        if end.is_some() {
            if message.last() == Some(&b'\r') {
                message.pop();
            }
            return Ok(Some(message));
        }
    }
}

fn write_message(writer: &mut impl Write, response: &Response) -> Result<(), ShellError> {
    let bytes = serde_json::to_vec(response).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not serialize MCP response")
            .with_context(error.to_string())
            .with_help("Report this as an MCP protocol serialization defect")
    })?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "MCP response exceeds the configured byte limit",
        )
        .with_help("Request a smaller capability result"));
    }
    writer
        .write_all(&bytes)
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not write MCP stdio response")
                .with_context(error.to_string())
                .with_help("Keep MCP transport on a writable stdio stream")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn request(id: u64, method: &str, params: Value) -> String {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
    }

    fn modern_params(mut params: Value) -> Value {
        params["_meta"] = json!({
            "io.modelcontextprotocol": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "clientInfo": { "name": "quirl-test", "version": "1" },
                "clientCapabilities": {}
            }
        });
        params
    }

    fn initialized_server(capabilities: Vec<McpCapability>) -> McpServer {
        let mut server = McpServer::new(capabilities);
        let response = server.handle_bytes(
            request(
                1,
                "initialize",
                json!({ "protocolVersion": LEGACY_PROTOCOL_VERSIONS[2] }),
            )
            .as_bytes(),
        );
        assert!(response.unwrap().error.is_none());
        server
    }

    #[test]
    fn mcp_metadata_is_truthfully_builtin_only_and_nonexecuting() {
        let server = McpServer::new(vec![McpCapability::Catalog]);
        assert_eq!(server.catalog, Catalog::builtin());
        assert!(server
            .catalog
            .commands
            .iter()
            .all(|command| command.provenance.source != quirl_catalog::Provenance::Plugin));
    }

    #[test]
    fn modern_discovery_requires_metadata_on_every_request_and_stamps_responses() {
        let mut server = McpServer::new(vec![McpCapability::Catalog]);
        let discovery = server
            .handle_bytes(request(1, "server/discover", modern_params(json!({}))).as_bytes())
            .unwrap();
        assert_eq!(
            discovery.result.unwrap()["protocolVersion"],
            MCP_PROTOCOL_VERSION
        );
        assert_eq!(
            discovery.meta.unwrap()["io.modelcontextprotocol/serverInfo"]["protocolVersion"],
            MCP_PROTOCOL_VERSION
        );

        let listed = server
            .handle_bytes(request(2, "tools/list", modern_params(json!({}))).as_bytes())
            .unwrap();
        assert_eq!(
            listed.result.unwrap()["_meta"]["io.modelcontextprotocol/cache"]["mode"],
            "immutable"
        );

        let missing_meta = server
            .handle_bytes(request(3, "tools/list", json!({})).as_bytes())
            .unwrap();
        assert_eq!(missing_meta.error.unwrap().code, -32602);
    }

    #[test]
    fn modern_and_legacy_handshakes_cannot_be_mixed() {
        let mut server = initialized_server(vec![McpCapability::Catalog]);
        let response = server
            .handle_bytes(request(2, "server/discover", modern_params(json!({}))).as_bytes())
            .unwrap();
        assert_eq!(response.error.unwrap().code, -32600);
    }

    #[test]
    fn initialized_server_lists_only_explicit_tools() {
        let mut server = initialized_server(vec![McpCapability::Format, McpCapability::Catalog]);
        let response = server
            .handle_bytes(request(2, "tools/list", json!({})).as_bytes())
            .unwrap();
        let tools = response.result.unwrap()["tools"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "quirl_catalog");
        assert_eq!(tools[1]["name"], "quirl_format");
    }

    #[test]
    fn tool_calls_are_denied_without_a_matching_grant() {
        let mut server = initialized_server(vec![McpCapability::Catalog]);
        let response = server
            .handle_bytes(
                request(
                    2,
                    "tools/call",
                    json!({ "name": "quirl_check", "arguments": {} }),
                )
                .as_bytes(),
            )
            .unwrap();
        assert_eq!(response.error.unwrap().code, -32602);
    }

    #[test]
    fn source_tools_are_bounded_deny_unknown_and_never_execute() {
        let mut server = initialized_server(vec![McpCapability::Check, McpCapability::Format]);
        let checked = server.handle_bytes(
            request(2, "tools/call", json!({ "name": "quirl_check", "arguments": { "language": "lua", "source": "os.execute('nope')" } })).as_bytes(),
        ).unwrap();
        assert!(checked.result.unwrap()["structuredContent"]["valid"].is_boolean());

        let formatted = server.handle_bytes(
            request(3, "tools/call", json!({ "name": "quirl_format", "arguments": { "language": "lua", "source": "local x=1  " } })).as_bytes(),
        ).unwrap();
        assert_eq!(
            formatted.result.unwrap()["structuredContent"]["source"],
            format_source("local x=1  ")
        );

        let rejected = server.handle_bytes(
            request(4, "tools/call", json!({ "name": "quirl_format", "arguments": { "language": "lua", "source": "x", "path": "/tmp/nope" } })).as_bytes(),
        ).unwrap();
        assert_eq!(rejected.error.unwrap().code, -32602);
    }

    #[test]
    fn stdio_framing_is_line_bounded_and_versioned() {
        let input = format!(
            "{}\n{}\n",
            request(
                1,
                "initialize",
                json!({ "protocolVersion": LEGACY_PROTOCOL_VERSIONS[2] })
            ),
            request(2, "tools/list", json!({}))
        );
        let mut output = Vec::new();
        serve(
            &mut Cursor::new(input),
            &mut output,
            vec![McpCapability::Complete],
        )
        .unwrap();
        let responses = std::str::from_utf8(&output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0]["result"]["protocolVersion"],
            LEGACY_PROTOCOL_VERSIONS[2]
        );
        assert_eq!(responses[1]["result"]["tools"][0]["name"], "quirl_complete");
    }

    #[test]
    fn oversized_stdio_message_is_a_resource_error_with_help() {
        let input = format!("{}\n", "x".repeat(MAX_MESSAGE_BYTES + 1));
        let error = serve(
            &mut Cursor::new(input),
            &mut Vec::new(),
            vec![McpCapability::Catalog],
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(!error.details.help.is_empty());
    }

    #[test]
    fn oversized_tool_result_becomes_a_json_rpc_error_without_stopping_stdio() {
        let response = Response::success(
            json!(9),
            json!({ "result": "x".repeat(MAX_MESSAGE_BYTES) }),
            Some(ProtocolEra::Modern),
        );
        let bounded = bounded_response(response);
        assert_eq!(bounded.error.unwrap().code, -32000);
        assert_eq!(bounded.id, 9);
        assert!(bounded.meta.is_some());
    }
}
