//! Deterministic, non-executing language services for Lua and native Quirl files.
//! `.qrl` is canonical; `.quirl` and `.🌀` are accepted aliases.
//!
//! The server deliberately consumes the same generated host API and command
//! catalog as the CLI. It speaks the LSP JSON-RPC subset over standard
//! `Content-Length` framing and never evaluates document text.

use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{LuaRuntime, HOST_API};
use quirl_syntax::check_script;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
};

const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
struct Document {
    language_id: String,
    version: i64,
    text: String,
}

/// Stateful language-service protocol implementation.
#[derive(Debug, Clone)]
pub struct LanguageService {
    catalog: Catalog,
    documents: HashMap<String, Document>,
    shutdown: bool,
    exit: bool,
}

impl Default for LanguageService {
    fn default() -> Self {
        Self::new(Catalog::builtin())
    }
}

impl LanguageService {
    /// Create an empty language-service session backed by `catalog`.
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            documents: HashMap::new(),
            shutdown: false,
            exit: false,
        }
    }

    /// Handle one JSON-RPC message and return zero or more response/notification messages.
    pub fn handle(&mut self, message: Value) -> Vec<Value> {
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return id
                .map(|id| vec![rpc_error(id, -32600, "invalid JSON-RPC request", None)])
                .unwrap_or_default();
        };
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        match self.dispatch(method, params) {
            Ok(Dispatch::Result(result)) => id
                .map(|id| vec![json!({"jsonrpc": "2.0", "id": id, "result": result})])
                .unwrap_or_default(),
            Ok(Dispatch::Messages(messages)) => messages,
            Ok(Dispatch::None) => Vec::new(),
            Err(error) => id
                .map(|id| {
                    let code = if error.code == ErrorCode::InvalidCommand {
                        -32601
                    } else {
                        -32602
                    };
                    vec![rpc_error(
                        id,
                        code,
                        &error.message,
                        serde_json::to_value(&error).ok(),
                    )]
                })
                .unwrap_or_default(),
        }
    }

    /// Return whether the client has sent the terminal `exit` notification.
    pub fn should_exit(&self) -> bool {
        self.exit
    }

    fn dispatch(&mut self, method: &str, params: Value) -> Result<Dispatch, ShellError> {
        if self.shutdown && method != "exit" {
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                "the language service has already shut down",
            )
            .with_help("Send `exit`, then start a new language-service process"));
        }
        match method {
            "initialize" => Ok(Dispatch::Result(initialize_result())),
            "initialized" => Ok(Dispatch::None),
            "shutdown" => {
                self.shutdown = true;
                Ok(Dispatch::Result(Value::Null))
            }
            "exit" => {
                self.exit = true;
                Ok(Dispatch::None)
            }
            "textDocument/didOpen" => self.did_open(&params),
            "textDocument/didChange" => self.did_change(&params),
            "textDocument/didClose" => self.did_close(&params),
            "textDocument/completion" => self.completion(&params),
            "textDocument/hover" => self.hover(&params),
            "textDocument/signatureHelp" => self.signature_help(&params),
            "textDocument/diagnostic" => self.document_diagnostic(&params),
            "quirl/moduleDocs" => Ok(Dispatch::Result(json!({
                "kind": "markdown",
                "value": module_docs(&self.catalog),
            }))),
            _ => Err(ShellError::new(
                ErrorCode::InvalidCommand,
                format!("language-service method `{method}` is not supported"),
            )
            .with_help("Use initialize to discover the supported LSP capabilities")),
        }
    }

    fn did_open(&mut self, params: &Value) -> Result<Dispatch, ShellError> {
        let item = field(params, "textDocument")?;
        let uri = string_field(item, "uri")?.to_owned();
        let document = Document {
            language_id: string_field(item, "languageId")?.to_owned(),
            version: item.get("version").and_then(Value::as_i64).unwrap_or(0),
            text: string_field(item, "text")?.to_owned(),
        };
        let diagnostics = diagnostics(&uri, &document);
        let version = document.version;
        self.documents.insert(uri.clone(), document);
        Ok(Dispatch::Messages(vec![publish_diagnostics(
            &uri,
            version,
            diagnostics,
        )]))
    }

    fn did_change(&mut self, params: &Value) -> Result<Dispatch, ShellError> {
        let item = field(params, "textDocument")?;
        let uri = string_field(item, "uri")?.to_owned();
        let version = item.get("version").and_then(Value::as_i64).unwrap_or(0);
        let text = params
            .get("contentChanges")
            .and_then(Value::as_array)
            .and_then(|changes| changes.last())
            .and_then(|change| change.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_params("didChange requires a full contentChanges text value"))?
            .to_owned();
        let document = self
            .documents
            .get_mut(&uri)
            .ok_or_else(|| invalid_params("didChange refers to a document that is not open"))?;
        document.text = text;
        document.version = version;
        let diagnostics = diagnostics(&uri, document);
        Ok(Dispatch::Messages(vec![publish_diagnostics(
            &uri,
            version,
            diagnostics,
        )]))
    }

    fn did_close(&mut self, params: &Value) -> Result<Dispatch, ShellError> {
        let uri = document_uri(params)?;
        self.documents.remove(uri);
        Ok(Dispatch::Messages(vec![publish_diagnostics(
            uri,
            0,
            Vec::new(),
        )]))
    }

    fn completion(&self, params: &Value) -> Result<Dispatch, ShellError> {
        let (uri, document, offset) = self.document_at(params)?;
        let prefix = token_before(&document.text, offset);
        let items = if is_lua(uri, document) {
            lua_completions(prefix)
        } else {
            quirl_completions(&self.catalog, &document.text, offset, prefix)
        };
        Ok(Dispatch::Result(json!({
            "isIncomplete": false,
            "items": items,
        })))
    }

    fn hover(&self, params: &Value) -> Result<Dispatch, ShellError> {
        let (uri, document, offset) = self.document_at(params)?;
        let value = if is_lua(uri, document) {
            lua_hover(&document.text, offset)
        } else {
            quirl_hover(&self.catalog, &document.text, offset)
        };
        Ok(Dispatch::Result(value.unwrap_or(Value::Null)))
    }

    fn signature_help(&self, params: &Value) -> Result<Dispatch, ShellError> {
        let (uri, document, offset) = self.document_at(params)?;
        let value = if is_lua(uri, document) {
            lua_signature(&document.text, offset)
        } else {
            quirl_signature(&self.catalog, &document.text, offset)
        };
        Ok(Dispatch::Result(value.unwrap_or(Value::Null)))
    }

    fn document_diagnostic(&self, params: &Value) -> Result<Dispatch, ShellError> {
        let uri = document_uri(params)?;
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| invalid_params("diagnostic refers to a document that is not open"))?;
        Ok(Dispatch::Result(json!({
            "kind": "full",
            "items": diagnostics(uri, document),
        })))
    }

    fn document_at<'a>(
        &'a self,
        params: &'a Value,
    ) -> Result<(&'a str, &'a Document, usize), ShellError> {
        let uri = document_uri(params)?;
        let document = self
            .documents
            .get(uri)
            .ok_or_else(|| invalid_params("request refers to a document that is not open"))?;
        let position = field(params, "position")?;
        let line = usize_field(position, "line")?;
        let character = usize_field(position, "character")?;
        let offset = position_to_offset(&document.text, line, character)
            .ok_or_else(|| invalid_params("position is outside the document"))?;
        Ok((uri, document, offset))
    }
}

enum Dispatch {
    Result(Value),
    Messages(Vec<Value>),
    None,
}

/// Serve the language service over LSP `Content-Length` framing.
pub fn serve<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    catalog: Catalog,
) -> Result<(), ShellError> {
    let mut service = LanguageService::new(catalog);
    while let Some(message) = read_message(reader)? {
        for outgoing in service.handle(message) {
            write_message(writer, &outgoing)?;
        }
        if service.should_exit() {
            break;
        }
    }
    Ok(())
}

fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": {"openClose": true, "change": 1},
            "completionProvider": {"triggerCharacters": [".", "-", " "]},
            "hoverProvider": true,
            "signatureHelpProvider": {"triggerCharacters": ["(", ",", " "]},
            "diagnosticProvider": {
                "identifier": "quirl",
                "interFileDependencies": false,
                "workspaceDiagnostics": false
            }
        },
        "serverInfo": {"name": "quirl-lsp", "version": env!("CARGO_PKG_VERSION")}
    })
}

fn diagnostics(uri: &str, document: &Document) -> Vec<Value> {
    if is_lua(uri, document) {
        match LuaRuntime::check_source(&document.text, uri) {
            Ok(()) => Vec::new(),
            Err(error) => shell_error_diagnostics(&document.text, &error),
        }
    } else {
        quirl_diagnostics(&document.text)
    }
}

fn shell_error_diagnostics(text: &str, error: &ShellError) -> Vec<Value> {
    if error.details.labels.is_empty() {
        return vec![diagnostic_value(
            text,
            0,
            text.chars().next().map(char::len_utf8).unwrap_or(0),
            &error.message,
            "quirl-lua",
        )];
    }
    error
        .details
        .labels
        .iter()
        .map(|label| {
            diagnostic_value(
                text,
                label.start,
                label.end,
                &format!("{}: {}", error.message, label.message),
                "quirl-lua",
            )
        })
        .collect()
}

fn quirl_diagnostics(text: &str) -> Vec<Value> {
    check_script(text)
        .into_iter()
        .map(|diagnostic| {
            diagnostic_value(
                text,
                diagnostic.start,
                diagnostic.end,
                &diagnostic.message,
                "quirl-syntax",
            )
        })
        .collect()
}

fn diagnostic_value(text: &str, start: usize, end: usize, message: &str, source: &str) -> Value {
    json!({
        "range": range(text, start.min(text.len()), end.min(text.len())),
        "severity": 1,
        "source": source,
        "message": message,
    })
}

fn lua_completions(prefix: &str) -> Vec<Value> {
    let prefix = prefix.to_ascii_lowercase();
    HOST_API
        .iter()
        .filter(|spec| prefix.is_empty() || spec.path.to_ascii_lowercase().starts_with(&prefix))
        .map(|spec| {
            json!({
                "label": spec.path,
                "kind": 3,
                "detail": host_signature(spec.path, spec.parameters, spec.returns),
                "documentation": {"kind": "markdown", "value": spec.summary},
                "insertText": spec.path,
            })
        })
        .collect()
}

fn quirl_completions(catalog: &Catalog, text: &str, offset: usize, prefix: &str) -> Vec<Value> {
    let line = current_line_before(text, offset).trim_start();
    let prefix = prefix.to_ascii_lowercase();
    let mut items = Vec::new();
    for command in &catalog.commands {
        if prefix.is_empty()
            || command.path.to_ascii_lowercase().starts_with(&prefix)
            || command.path.starts_with(line)
        {
            items.push(json!({
                "label": command.path,
                "kind": 3,
                "detail": command.signature,
                "documentation": {"kind": "markdown", "value": command.details},
                "insertText": command.path,
            }));
        }
        if line.starts_with(&command.path) {
            for option in &command.options {
                if let Some(name) = option.names.first() {
                    if prefix.is_empty() || name.to_ascii_lowercase().starts_with(&prefix) {
                        items.push(json!({
                            "label": name,
                            "kind": 5,
                            "detail": option.value_type,
                            "documentation": {"kind": "markdown", "value": option.documentation},
                            "insertText": name,
                        }));
                    }
                }
            }
        }
    }
    items
}

fn lua_hover(text: &str, offset: usize) -> Option<Value> {
    let (_, _, token) = token_at(text, offset);
    if token == "quirl" {
        return Some(markdown_hover(
            "`quirl` is the restricted host module. Its API is generated by Quirl and calls are checked without executing this document.",
        ));
    }
    HOST_API.iter().find(|spec| spec.path == token).map(|spec| {
        markdown_hover(&format!(
            "```lua\n{}\n```\n\n{}{}",
            host_signature(spec.path, spec.parameters, spec.returns),
            spec.summary,
            spec.capability
                .map(|capability| format!("\n\nCapability: `{capability}`"))
                .unwrap_or_default()
        ))
    })
}

fn quirl_hover(catalog: &Catalog, text: &str, offset: usize) -> Option<Value> {
    command_at(catalog, current_line(text, offset)).map(|command| {
        markdown_hover(&format!(
            "```quirl\n{}\n```\n\n{}\n\n{}",
            command.signature, command.summary, command.details
        ))
    })
}

fn lua_signature(text: &str, offset: usize) -> Option<Value> {
    let before = &text[..offset.min(text.len())];
    let open = before.rfind('(')?;
    let name = token_before(before, open);
    let spec = HOST_API.iter().find(|spec| spec.path == name)?;
    let active_parameter = before[open + 1..].matches(',').count();
    Some(json!({
        "signatures": [{
            "label": host_signature(spec.path, spec.parameters, spec.returns),
            "documentation": {"kind": "markdown", "value": spec.summary},
            "parameters": spec.parameters.iter().map(|parameter| json!({
                "label": parameter.name,
                "documentation": format!("`{}`", parameter.lua_type),
            })).collect::<Vec<_>>()
        }],
        "activeSignature": 0,
        "activeParameter": active_parameter.min(spec.parameters.len().saturating_sub(1)),
    }))
}

fn quirl_signature(catalog: &Catalog, text: &str, offset: usize) -> Option<Value> {
    let command = command_at(catalog, current_line(text, offset))?;
    Some(json!({
        "signatures": [{
            "label": command.signature,
            "documentation": {"kind": "markdown", "value": command.details},
            "parameters": []
        }],
        "activeSignature": 0,
        "activeParameter": 0,
    }))
}

fn command_at<'a>(catalog: &'a Catalog, line: &str) -> Option<&'a CommandSpec> {
    catalog
        .commands
        .iter()
        .filter(|command| line.trim_start().starts_with(&command.path))
        .max_by_key(|command| command.path.len())
}

fn module_docs(catalog: &Catalog) -> String {
    let mut output = String::from("# Quirl language service modules\n\n## Lua host module\n\n");
    for spec in HOST_API {
        output.push_str(&format!(
            "### `{}`\n\n{}\n\n",
            host_signature(spec.path, spec.parameters, spec.returns),
            spec.summary
        ));
    }
    output.push_str("## Quirl command module\n\n");
    output.push_str(&catalog.to_markdown());
    output
}

fn host_signature(path: &str, parameters: &[quirl_lua::HostParameter], returns: &str) -> String {
    let parameters = parameters
        .iter()
        .map(|parameter| format!("{}: {}", parameter.name, parameter.lua_type))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{path}({parameters}) -> {returns}")
}

fn markdown_hover(value: &str) -> Value {
    json!({"contents": {"kind": "markdown", "value": value}})
}

fn publish_diagnostics(uri: &str, version: i64, diagnostics: Vec<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {"uri": uri, "version": version, "diagnostics": diagnostics}
    })
}

fn range(text: &str, start: usize, end: usize) -> Value {
    let (start_line, start_character) = offset_to_position(text, start);
    let (end_line, end_character) = offset_to_position(text, end);
    json!({
        "start": {"line": start_line, "character": start_character},
        "end": {"line": end_line, "character": end_character},
    })
}

fn offset_to_position(text: &str, offset: usize) -> (usize, usize) {
    let mut safe_offset = offset.min(text.len());
    while !text.is_char_boundary(safe_offset) {
        safe_offset = safe_offset.saturating_sub(1);
    }
    let before = &text[..safe_offset];
    let line = before.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let character = text[line_start..safe_offset].encode_utf16().count();
    (line, character)
}

fn position_to_offset(text: &str, target_line: usize, target_character: usize) -> Option<usize> {
    let mut line = 0;
    let mut character = 0;
    for (offset, ch) in text.char_indices() {
        if line == target_line && character == target_character {
            return Some(offset);
        }
        if ch == '\n' {
            if line == target_line {
                return (character == target_character).then_some(offset);
            }
            line += 1;
            character = 0;
        } else if line == target_line {
            let width = ch.len_utf16();
            if character + width > target_character {
                return None;
            }
            character += width;
        }
    }
    (line == target_line && character == target_character).then_some(text.len())
}

fn token_at(text: &str, offset: usize) -> (usize, usize, &str) {
    let offset = offset.min(text.len());
    let mut start = offset;
    let mut end = offset;
    while let Some(ch) = text[..start].chars().next_back() {
        if !token_char(ch) {
            break;
        }
        start -= ch.len_utf8();
    }
    while let Some(ch) = text[end..].chars().next() {
        if !token_char(ch) {
            break;
        }
        end += ch.len_utf8();
    }
    (start, end, &text[start..end])
}

fn token_before(text: &str, offset: usize) -> &str {
    let offset = offset.min(text.len());
    let mut start = offset;
    while let Some(ch) = text[..start].chars().next_back() {
        if !token_char(ch) {
            break;
        }
        start -= ch.len_utf8();
    }
    &text[start..offset]
}

fn token_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.')
}

fn current_line(text: &str, offset: usize) -> &str {
    let offset = offset.min(text.len());
    let start = text[..offset]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    let end = text[offset..]
        .find('\n')
        .map(|index| offset + index)
        .unwrap_or(text.len());
    &text[start..end]
}

fn current_line_before(text: &str, offset: usize) -> &str {
    let offset = offset.min(text.len());
    let start = text[..offset]
        .rfind('\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    &text[start..offset]
}

fn is_lua(uri: &str, document: &Document) -> bool {
    document.language_id.eq_ignore_ascii_case("lua") || uri.ends_with(".lua")
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a Value, ShellError> {
    value
        .get(name)
        .ok_or_else(|| invalid_params(&format!("missing `{name}` parameter")))
}

fn string_field<'a>(value: &'a Value, name: &str) -> Result<&'a str, ShellError> {
    field(value, name)?
        .as_str()
        .ok_or_else(|| invalid_params(&format!("`{name}` must be a string")))
}

fn usize_field(value: &Value, name: &str) -> Result<usize, ShellError> {
    let raw = field(value, name)?
        .as_u64()
        .ok_or_else(|| invalid_params(&format!("`{name}` must be a non-negative integer")))?;
    usize::try_from(raw).map_err(|_| invalid_params(&format!("`{name}` is too large")))
}

fn document_uri(params: &Value) -> Result<&str, ShellError> {
    string_field(field(params, "textDocument")?, "uri")
}

fn invalid_params(message: &str) -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, message)
        .with_help("Send the standard LSP textDocument and position fields")
}

fn rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Value>, ShellError> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).map_err(io_error)?;
        if read == 0 {
            return if content_length.is_none() {
                Ok(None)
            } else {
                Err(protocol_error(
                    "unexpected end of input while reading headers",
                ))
            };
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))
        {
            content_length = Some(value.trim().parse::<usize>().map_err(|_| {
                protocol_error("Content-Length must be a non-negative decimal integer")
            })?);
        }
    }
    let length = content_length.ok_or_else(|| protocol_error("missing Content-Length header"))?;
    if length > MAX_MESSAGE_BYTES {
        return Err(protocol_error("language-service message exceeds 4 MiB"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(io_error)?;
    serde_json::from_slice(&body).map(Some).map_err(|error| {
        protocol_error("language-service message is not valid JSON").with_context(error.to_string())
    })
}

fn write_message<W: Write>(writer: &mut W, message: &Value) -> Result<(), ShellError> {
    let body = serde_json::to_vec(message).map_err(|error| {
        protocol_error("could not serialize a language-service response")
            .with_context(error.to_string())
    })?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len()).map_err(io_error)?;
    writer.write_all(&body).map_err(io_error)?;
    writer.flush().map_err(io_error)
}

fn io_error(error: std::io::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "language-service I/O failed")
        .with_context(error.to_string())
        .with_help("Restart the editor language-service connection")
}

fn protocol_error(message: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message)
        .with_help("Send one JSON object using LSP Content-Length framing")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn open(service: &mut LanguageService, uri: &str, language_id: &str, text: &str) -> Vec<Value> {
        service.handle(json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {
                "uri": uri, "languageId": language_id, "version": 1, "text": text
            }}
        }))
    }

    fn request(service: &mut LanguageService, id: i64, method: &str, params: Value) -> Value {
        service
            .handle(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .remove(0)
    }

    #[test]
    fn initialize_advertises_the_deterministic_language_features() {
        let response = request(&mut LanguageService::default(), 1, "initialize", json!({}));
        assert_eq!(response["result"]["capabilities"]["hoverProvider"], true);
        assert_eq!(
            response["result"]["capabilities"]["diagnosticProvider"]["identifier"],
            "quirl"
        );
    }

    #[test]
    fn opening_lua_publishes_compile_diagnostics_without_executing_it() {
        let mut service = LanguageService::default();
        let messages = open(
            &mut service,
            "file:///broken.lua",
            "lua",
            "return os.execute('touch never')",
        );
        assert_eq!(messages[0]["method"], "textDocument/publishDiagnostics");
        assert!(!messages[0]["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn host_api_drives_completion_hover_and_signature_help() {
        let mut service = LanguageService::default();
        open(&mut service, "file:///plugin.lua", "lua", "quirl.cwd(");
        let params = json!({
            "textDocument": {"uri": "file:///plugin.lua"},
            "position": {"line": 0, "character": 10}
        });
        let completion = request(&mut service, 1, "textDocument/completion", params.clone());
        assert!(completion["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["label"] == "quirl.cwd"));
        let signature = request(&mut service, 2, "textDocument/signatureHelp", params);
        assert!(signature["result"]["signatures"][0]["label"]
            .as_str()
            .unwrap()
            .starts_with("quirl.cwd("));

        let hover = request(
            &mut service,
            3,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": "file:///plugin.lua"},
                "position": {"line": 0, "character": 3}
            }),
        );
        assert!(hover["result"]["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("quirl.cwd("));
    }

    #[test]
    fn canonical_qrl_documents_use_catalog_completion_and_structural_diagnostics() {
        let mut service = LanguageService::default();
        let messages = open(&mut service, "file:///flow.qrl", "quirl", "quirl che |");
        assert_eq!(
            messages[0]["params"]["diagnostics"][0]["source"],
            "quirl-syntax"
        );
        let completion = request(
            &mut service,
            1,
            "textDocument/completion",
            json!({
                "textDocument": {"uri": "file:///flow.qrl"},
                "position": {"line": 0, "character": 9}
            }),
        );
        assert!(completion["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["label"] == "quirl check"));
    }

    #[test]
    fn native_alias_documents_use_quirl_language_services() {
        for uri in ["file:///flow.quirl", "file:///flow.%F0%9F%8C%80"] {
            let mut service = LanguageService::default();
            let messages = open(&mut service, uri, "quirl", "quirl che |");
            assert_eq!(
                messages[0]["params"]["diagnostics"][0]["source"],
                "quirl-syntax"
            );
            let completion = request(
                &mut service,
                1,
                "textDocument/completion",
                json!({"textDocument": {"uri": uri}, "position": {"line": 0, "character": 9}}),
            );
            assert!(completion["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["label"] == "quirl check"));
        }
    }

    #[test]
    fn quirl_boolean_or_is_not_misreported_as_an_empty_pipeline() {
        let mut service = LanguageService::default();
        let messages = open(
            &mut service,
            "file:///fallback.quirl",
            "quirl",
            "false || echo recovered",
        );
        assert!(messages[0]["params"]["diagnostics"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn quirl_diagnostics_share_native_unsupported_constructs_and_spans() {
        let source = "echo $HOME\nprintf ok |\n";
        let expected = check_script(source);
        let actual = quirl_diagnostics(source);
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual["message"], expected.message);
            assert_eq!(actual["range"], range(source, expected.start, expected.end));
        }
    }

    #[test]
    fn module_docs_are_generated_from_host_api_and_catalog() {
        let response = request(
            &mut LanguageService::default(),
            1,
            "quirl/moduleDocs",
            json!({}),
        );
        let docs = response["result"]["value"].as_str().unwrap();
        assert!(docs.contains("quirl.cwd"));
        assert!(docs.contains("quirl check"));
    }

    #[test]
    fn stdio_server_reads_and_writes_lsp_framing() {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
        }))
        .unwrap();
        let input = format!("Content-Length: {}\r\n\r\n", body.len())
            .into_bytes()
            .into_iter()
            .chain(body)
            .collect::<Vec<_>>();
        let mut reader = BufReader::new(Cursor::new(input));
        let mut output = Vec::new();
        serve(&mut reader, &mut output, Catalog::builtin()).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.starts_with("Content-Length:"));
        assert!(rendered.contains("completionProvider"));
    }
}
