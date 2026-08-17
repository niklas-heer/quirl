//! Deterministic, non-executing language services for Lua and native Quirl files.
//! `.qrl` is canonical; `.quirl` and `.🌀` are accepted aliases.
//!
//! The server deliberately consumes the same generated host API and command
//! catalog as the CLI. It speaks the LSP JSON-RPC subset over standard
//! `Content-Length` framing and never evaluates document text.

#![cfg_attr(
    test,
    allow(
        dead_code_pub_in_binary,
        reason = "the libtest harness is an executable, but these public items remain library API"
    )
)]

use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{HOST_API, LuaRuntime};
use quirl_syntax::check_script;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, ErrorKind, Read, Write},
};

const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_HEADER_COUNT: usize = 64;
// Correlation IDs are not payloads, so validate their encoded size before cloning or reflection.
const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_METHOD_BYTES: usize = 128;
const MAX_OPEN_DOCUMENTS: usize = 128;
const MAX_URI_BYTES: usize = 8 * 1024;
const MAX_LANGUAGE_ID_BYTES: usize = 64;
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_RETAINED_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONTENT_CHANGES: usize = 1;

#[derive(Debug, Clone)]
struct Document {
    language_id: String,
    version: i64,
    text: String,
}

/// Inert native-language diagnostic supplied by the CLI composition root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDiagnostic {
    /// Concise diagnostic message.
    pub message: String,
    /// Inclusive UTF-8 byte offset in the complete document.
    pub start: usize,
    /// Exclusive UTF-8 byte offset in the complete document.
    pub end: usize,
    /// Stable producer name shown to LSP clients.
    pub source: &'static str,
}

/// Side-effect-free native document analyzer injected without adding an LSP-to-data edge.
pub type NativeAnalyzer = fn(&str) -> Vec<NativeDiagnostic>;

/// Stateful language-service protocol implementation.
#[derive(Debug, Clone)]
pub struct LanguageService {
    catalog: Catalog,
    native_analyzer: Option<NativeAnalyzer>,
    documents: HashMap<String, Document>,
    retained_document_bytes: usize,
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
            native_analyzer: None,
            documents: HashMap::new(),
            retained_document_bytes: 0,
            shutdown: false,
            exit: false,
        }
    }

    /// Create a session with a composition-root native analyzer.
    ///
    /// The callback must only inspect the supplied bounded UTF-8 document. The
    /// LSP invokes it after enforcing the 1 MiB document limit and never grants
    /// filesystem, process, adapter, or runtime capabilities.
    pub fn new_with_native_analyzer(catalog: Catalog, analyzer: NativeAnalyzer) -> Self {
        let mut service = Self::new(catalog);
        service.native_analyzer = Some(analyzer);
        service
    }

    /// Handle one JSON-RPC message and return zero or more response/notification messages.
    pub fn handle(&mut self, message: Value) -> Vec<Value> {
        let id = bounded_request_id(&message);
        let Some(object) = message.as_object() else {
            return vec![rpc_error(
                Value::Null,
                -32600,
                "invalid JSON-RPC request",
                None,
            )];
        };
        let method = object.get("method").and_then(Value::as_str);
        let params_are_structured = object
            .get("params")
            .is_none_or(|params| params.is_array() || params.is_object());
        let valid_envelope = object.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
            && object.get("id").is_none_or(valid_request_id)
            && method.is_some_and(|method| method.len() <= MAX_METHOD_BYTES)
            && params_are_structured;
        if !valid_envelope {
            return vec![rpc_error(
                id.unwrap_or(Value::Null),
                -32600,
                "invalid JSON-RPC request",
                None,
            )];
        };
        let method = method.unwrap_or_default();
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
                self.documents.clear();
                self.retained_document_bytes = 0;
                self.shutdown = true;
                Ok(Dispatch::Result(Value::Null))
            }
            "exit" => {
                self.documents.clear();
                self.retained_document_bytes = 0;
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
        let uri = string_field(item, "uri")?;
        validate_byte_limit("document URI", uri.len(), MAX_URI_BYTES)?;
        let language_id = string_field(item, "languageId")?;
        validate_byte_limit(
            "document language identifier",
            language_id.len(),
            MAX_LANGUAGE_ID_BYTES,
        )?;
        let text = string_field(item, "text")?;
        validate_byte_limit("document text", text.len(), MAX_DOCUMENT_BYTES)?;
        let old_bytes = self
            .documents
            .get(uri)
            .map(|document| retained_bytes(uri, &document.language_id, &document.text))
            .transpose()?
            .unwrap_or(0);
        if !self.documents.contains_key(uri) {
            let observed =
                self.documents.len().checked_add(1).ok_or_else(|| {
                    count_overflow_error("open document count", MAX_OPEN_DOCUMENTS)
                })?;
            validate_count_limit("open document count", observed, MAX_OPEN_DOCUMENTS)?;
        }
        let candidate_bytes = retained_bytes(uri, language_id, text)?;
        let next_retained_bytes =
            retained_bytes_after_replace(self.retained_document_bytes, old_bytes, candidate_bytes)?;
        let document = Document {
            language_id: language_id.to_owned(),
            version: item.get("version").and_then(Value::as_i64).unwrap_or(0),
            text: text.to_owned(),
        };
        let diagnostics = diagnostics(uri, &document, self.native_analyzer)?;
        let version = document.version;
        self.documents.insert(uri.to_owned(), document);
        self.retained_document_bytes = next_retained_bytes;
        Ok(Dispatch::Messages(vec![publish_diagnostics(
            uri,
            version,
            diagnostics,
        )]))
    }

    fn did_change(&mut self, params: &Value) -> Result<Dispatch, ShellError> {
        let item = field(params, "textDocument")?;
        let uri = string_field(item, "uri")?;
        validate_byte_limit("document URI", uri.len(), MAX_URI_BYTES)?;
        let version = item.get("version").and_then(Value::as_i64).unwrap_or(0);
        let changes = params
            .get("contentChanges")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_params("didChange requires a contentChanges array"))?;
        if changes.is_empty() {
            return Err(invalid_params(
                "didChange requires one full contentChanges text value",
            ));
        }
        validate_count_limit("content change count", changes.len(), MAX_CONTENT_CHANGES)?;
        let text = changes
            .last()
            .and_then(|change| change.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_params("didChange requires a full contentChanges text value"))?;
        validate_byte_limit("document text", text.len(), MAX_DOCUMENT_BYTES)?;
        let previous = self
            .documents
            .get(uri)
            .ok_or_else(|| invalid_params("didChange refers to a document that is not open"))?;
        let old_bytes = retained_bytes(uri, &previous.language_id, &previous.text)?;
        let candidate_bytes = retained_bytes(uri, &previous.language_id, text)?;
        let next_retained_bytes =
            retained_bytes_after_replace(self.retained_document_bytes, old_bytes, candidate_bytes)?;
        let document = Document {
            language_id: previous.language_id.clone(),
            version,
            text: text.to_owned(),
        };
        let diagnostics = diagnostics(uri, &document, self.native_analyzer)?;
        self.documents.insert(uri.to_owned(), document);
        self.retained_document_bytes = next_retained_bytes;
        Ok(Dispatch::Messages(vec![publish_diagnostics(
            uri,
            version,
            diagnostics,
        )]))
    }

    fn did_close(&mut self, params: &Value) -> Result<Dispatch, ShellError> {
        let uri = document_uri(params)?;
        if let Some(document) = self.documents.get(uri) {
            let removed_bytes = retained_bytes(uri, &document.language_id, &document.text)?;
            let next_retained_bytes = self
                .retained_document_bytes
                .checked_sub(removed_bytes)
                .ok_or_else(retained_accounting_error)?;
            self.documents.remove(uri);
            self.retained_document_bytes = next_retained_bytes;
        }
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
            "items": diagnostics(uri, document, self.native_analyzer)?,
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
        let offset = position_to_offset(&document.text, line, character)?;
        Ok((uri, document, offset))
    }
}

#[derive(Debug)]
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
    serve_session(reader, writer, LanguageService::new(catalog))
}

/// Serve LSP with a side-effect-free native analyzer supplied by the composition root.
pub fn serve_with_native_analyzer<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    catalog: Catalog,
    analyzer: NativeAnalyzer,
) -> Result<(), ShellError> {
    serve_session(
        reader,
        writer,
        LanguageService::new_with_native_analyzer(catalog, analyzer),
    )
}

fn serve_session<R: BufRead, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut service: LanguageService,
) -> Result<(), ShellError> {
    while let Some(body) = read_message(reader)? {
        let outgoing = match serde_json::from_slice(&body) {
            Ok(message) => service.handle(message),
            Err(_) => vec![rpc_error(Value::Null, -32700, "parse error", None)],
        };
        for message in outgoing {
            write_message(writer, &message)?;
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

fn diagnostics(
    uri: &str,
    document: &Document,
    native_analyzer: Option<NativeAnalyzer>,
) -> Result<Vec<Value>, ShellError> {
    if is_lua(uri, document) {
        match LuaRuntime::check_source(&document.text, uri) {
            Ok(()) => Ok(Vec::new()),
            Err(error) => shell_error_diagnostics(&document.text, &error),
        }
    } else {
        quirl_diagnostics(&document.text, native_analyzer)
    }
}

fn shell_error_diagnostics(text: &str, error: &ShellError) -> Result<Vec<Value>, ShellError> {
    if error.details.labels.is_empty() {
        return Ok(vec![diagnostic_value(
            text,
            0,
            text.chars().next().map(char::len_utf8).unwrap_or(0),
            &error.message,
            "quirl-lua",
        )?]);
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

fn quirl_diagnostics(
    text: &str,
    native_analyzer: Option<NativeAnalyzer>,
) -> Result<Vec<Value>, ShellError> {
    if let Some(analyzer) = native_analyzer {
        analyzer(text)
            .into_iter()
            .map(|diagnostic| {
                diagnostic_value(
                    text,
                    diagnostic.start,
                    diagnostic.end,
                    &diagnostic.message,
                    diagnostic.source,
                )
            })
            .collect()
    } else {
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
}

fn diagnostic_value(
    text: &str,
    start: usize,
    end: usize,
    message: &str,
    source: &str,
) -> Result<Value, ShellError> {
    Ok(json!({
        "range": range(text, start, end)?,
        "severity": 1,
        "source": source,
        "message": message,
    }))
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
                "documentation": {"kind": "markdown", "value": command_documentation(command)},
                "insertText": command.path,
            }));
        }
        if command_starts_line(command, line) {
            for option in &command.options {
                if let Some(name) = option.names.first()
                    && (prefix.is_empty() || name.to_ascii_lowercase().starts_with(&prefix))
                {
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
            command.signature,
            command.summary,
            command_documentation(command)
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
            "documentation": {"kind": "markdown", "value": command_documentation(command)},
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
        .filter(|command| command_starts_line(command, line.trim_start()))
        .max_by_key(|command| command.path.len())
}

fn command_starts_line(command: &CommandSpec, line: &str) -> bool {
    line.strip_prefix(&command.path)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

fn command_documentation(command: &CommandSpec) -> String {
    format!(
        "{}\n\nInput: `{}`  \nOutput: `{}`  \nLive streaming: `{}`",
        command.details, command.io.input, command.io.output, command.io.streaming
    )
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

fn range(text: &str, start: usize, end: usize) -> Result<Value, ShellError> {
    if start > end {
        return Err(invalid_diagnostic_range(text, start, end));
    }
    let (start_line, start_character) = offset_to_position(text, start)
        .ok_or_else(|| invalid_diagnostic_range(text, start, end))?;
    let (end_line, end_character) =
        offset_to_position(text, end).ok_or_else(|| invalid_diagnostic_range(text, start, end))?;
    Ok(json!({
        "start": {"line": start_line, "character": start_character},
        "end": {"line": end_line, "character": end_character},
    }))
}

fn offset_to_position(text: &str, offset: usize) -> Option<(usize, usize)> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let before = &text[..offset];
    let line = before.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let character = text[line_start..offset].encode_utf16().count();
    Some((line, character))
}

fn position_to_offset(
    text: &str,
    target_line: usize,
    target_character: usize,
) -> Result<usize, ShellError> {
    let mut line = 0;
    let mut line_start = 0;
    for (offset, ch) in text.char_indices() {
        if ch == '\n' {
            if line == target_line {
                return character_to_offset(
                    &text[line_start..offset],
                    line_start,
                    target_character,
                );
            }
            line += 1;
            line_start = offset + 1;
        }
    }
    if line == target_line {
        character_to_offset(&text[line_start..], line_start, target_character)
    } else {
        Err(invalid_params(
            "position line is past the end of the document",
        ))
    }
}

fn character_to_offset(
    line: &str,
    line_start: usize,
    target_character: usize,
) -> Result<usize, ShellError> {
    let mut character = 0;
    for (offset, ch) in line.char_indices() {
        if character == target_character {
            return Ok(line_start + offset);
        }
        let next_character = character + ch.len_utf16();
        if target_character < next_character {
            return Err(invalid_params(
                "position character splits a UTF-16 surrogate pair",
            ));
        }
        character = next_character;
    }
    if character == target_character {
        Ok(line_start + line.len())
    } else {
        Err(invalid_params(
            "position character is past the end of the line",
        ))
    }
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
    let uri = string_field(field(params, "textDocument")?, "uri")?;
    validate_byte_limit("document URI", uri.len(), MAX_URI_BYTES)?;
    Ok(uri)
}

fn invalid_params(message: &str) -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, message)
        .with_help("Send the standard LSP textDocument and position fields")
}

fn invalid_diagnostic_range(text: &str, start: usize, end: usize) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "language-service diagnostic range is invalid for the document",
    )
    .with_context(format!(
        "start_bytes: {start}; end_bytes: {end}; document_bytes: {}",
        text.len()
    ))
    .with_help("Report the diagnostic producer that returned the invalid source range")
}

fn retained_bytes(uri: &str, language_id: &str, text: &str) -> Result<usize, ShellError> {
    uri.len()
        .checked_add(language_id.len())
        .and_then(|bytes| bytes.checked_add(text.len()))
        .ok_or_else(retained_accounting_error)
}

fn retained_bytes_after_replace(
    retained_bytes: usize,
    replaced_bytes: usize,
    candidate_bytes: usize,
) -> Result<usize, ShellError> {
    let without_replaced = retained_bytes
        .checked_sub(replaced_bytes)
        .ok_or_else(retained_accounting_error)?;
    let observed = without_replaced
        .checked_add(candidate_bytes)
        .ok_or_else(retained_accounting_error)?;
    validate_byte_limit(
        "aggregate retained document state",
        observed,
        MAX_RETAINED_DOCUMENT_BYTES,
    )?;
    Ok(observed)
}

fn validate_byte_limit(context: &str, observed: usize, limit: usize) -> Result<(), ShellError> {
    if observed > limit {
        return Err(resource_limit_error(context, "bytes", observed, limit));
    }
    Ok(())
}

fn validate_count_limit(context: &str, observed: usize, limit: usize) -> Result<(), ShellError> {
    if observed > limit {
        return Err(resource_limit_error(context, "count", observed, limit));
    }
    Ok(())
}

fn resource_limit_error(context: &str, unit: &str, observed: usize, limit: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{context} exceeds its configured limit"),
    )
    .with_context(format!(
        "observed_{unit}: {observed}; limit_{unit}: {limit}"
    ))
    .with_help("Reduce the request size or close documents before retrying")
}

fn count_overflow_error(context: &str, limit: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{context} overflowed while enforcing its configured limit"),
    )
    .with_context(format!("observed_count: overflow; limit_count: {limit}"))
    .with_help("Close documents before retrying")
}

fn retained_accounting_error() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "aggregate retained document accounting overflowed",
    )
    .with_context(format!(
        "observed_bytes: overflow; limit_bytes: {MAX_RETAINED_DOCUMENT_BYTES}"
    ))
    .with_help("Restart the language-service connection")
}

fn rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

fn bounded_request_id(message: &Value) -> Option<Value> {
    message.get("id").filter(|id| valid_request_id(id)).cloned()
}

fn valid_request_id(id: &Value) -> bool {
    let valid_scalar = id.is_null() || id.is_string() || id.is_number();
    valid_scalar && serde_json::to_vec(id).is_ok_and(|bytes| bytes.len() <= MAX_REQUEST_ID_BYTES)
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, ShellError> {
    let mut content_length = None;
    let mut header_bytes: usize = 0;
    let mut header_count: usize = 0;
    loop {
        let Some(line) = read_header_line(reader, header_bytes)? else {
            return if header_bytes == 0 {
                Ok(None)
            } else {
                Err(protocol_error(
                    "unexpected end of input while reading headers",
                ))
            };
        };
        header_bytes = header_bytes.checked_add(line.len()).ok_or_else(|| {
            resource_limit_error(
                "language-service headers",
                "bytes",
                usize::MAX,
                MAX_HEADER_BYTES,
            )
        })?;
        validate_byte_limit("language-service headers", header_bytes, MAX_HEADER_BYTES)?;
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        header_count = header_count.checked_add(1).ok_or_else(|| {
            count_overflow_error("language-service header count", MAX_HEADER_COUNT)
        })?;
        validate_count_limit(
            "language-service header count",
            header_count,
            MAX_HEADER_COUNT,
        )?;
        let line = std::str::from_utf8(&line)
            .map_err(|_| protocol_error("language-service headers must be valid UTF-8"))?;
        let Some((name, value)) = line.trim_end_matches(['\r', '\n']).split_once(':') else {
            return Err(protocol_error("language-service header is missing `:`"));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(protocol_error("duplicate Content-Length header"));
            }
            content_length = Some(parse_content_length(value.trim())?);
        }
    }
    let length = content_length.ok_or_else(|| protocol_error("missing Content-Length header"))?;
    validate_byte_limit("language-service message", length, MAX_MESSAGE_BYTES)?;
    let mut body = vec![0; length];
    let mut received = 0;
    while received < length {
        match reader.read(&mut body[received..]) {
            Ok(0) => {
                return Err(protocol_error(
                    "unexpected end of input while reading the language-service message body",
                )
                .with_context(format!(
                    "expected_bytes: {length}; received_bytes: {received}"
                )));
            }
            Ok(bytes) => received = received.saturating_add(bytes),
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(Some(body))
}

fn read_header_line<R: BufRead>(
    reader: &mut R,
    bytes_already_read: usize,
) -> Result<Option<Vec<u8>>, ShellError> {
    let remaining = MAX_HEADER_BYTES.saturating_sub(bytes_already_read);
    let mut line = Vec::new();
    let read_limit = remaining.saturating_add(1);
    let read = reader
        .take(read_limit as u64)
        .read_until(b'\n', &mut line)
        .map_err(io_error)?;
    if read == 0 {
        return Ok(None);
    }
    if line.len() > remaining {
        return Err(resource_limit_error(
            "language-service headers",
            "bytes",
            bytes_already_read.saturating_add(line.len()),
            MAX_HEADER_BYTES,
        ));
    }
    Ok(Some(line))
}

fn parse_content_length(value: &str) -> Result<usize, ShellError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(protocol_error(
            "Content-Length must be a non-negative decimal integer",
        ));
    }
    value.parse::<usize>().map_err(|_| {
        ShellError::new(
            ErrorCode::ResourceLimit,
            "language-service message length exceeds the platform limit",
        )
        .with_context(format!(
            "observed_bytes: greater than {}; limit_bytes: {MAX_MESSAGE_BYTES}",
            usize::MAX
        ))
        .with_help("Keep each language-service message at or below 4 MiB")
    })
}

fn write_message<W: Write>(writer: &mut W, message: &Value) -> Result<(), ShellError> {
    let body = serde_json::to_vec(message).map_err(|error| {
        protocol_error("could not serialize a language-service response")
            .with_context(error.to_string())
    })?;
    validate_byte_limit("language-service response", body.len(), MAX_MESSAGE_BYTES)?;
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

    fn open_params(uri: &str, language_id: &str, version: i64, text: &str) -> Value {
        json!({"textDocument": {
            "uri": uri, "languageId": language_id, "version": version, "text": text
        }})
    }

    fn change_params(uri: &str, version: i64, changes: Vec<Value>) -> Value {
        json!({
            "textDocument": {"uri": uri, "version": version},
            "contentChanges": changes,
        })
    }

    fn assert_accounting(service: &LanguageService) {
        let expected = service
            .documents
            .iter()
            .map(|(uri, document)| uri.len() + document.language_id.len() + document.text.len())
            .sum::<usize>();
        assert_eq!(service.retained_document_bytes, expected);
    }

    fn frame(body: &[u8]) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n", body.len())
            .into_bytes()
            .into_iter()
            .chain(body.iter().copied())
            .collect()
    }

    fn framed_json_values(bytes: Vec<u8>) -> Vec<Value> {
        let mut reader = BufReader::new(Cursor::new(bytes));
        let mut values = Vec::new();
        while let Some(body) = read_message(&mut reader).unwrap() {
            values.push(serde_json::from_slice(&body).unwrap());
        }
        values
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
        assert!(
            !messages[0]["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );
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
        assert!(
            completion["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["label"] == "quirl.cwd")
        );
        let signature = request(&mut service, 2, "textDocument/signatureHelp", params);
        assert!(
            signature["result"]["signatures"][0]["label"]
                .as_str()
                .unwrap()
                .starts_with("quirl.cwd(")
        );

        let hover = request(
            &mut service,
            3,
            "textDocument/hover",
            json!({
                "textDocument": {"uri": "file:///plugin.lua"},
                "position": {"line": 0, "character": 3}
            }),
        );
        assert!(
            hover["result"]["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("quirl.cwd(")
        );
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
        assert!(
            completion["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["label"] == "quirl check")
        );
    }

    #[test]
    fn injected_native_analyzer_reports_data_diagnostics_without_execution() {
        fn analyzer(source: &str) -> Vec<NativeDiagnostic> {
            let start = source.find("[1, 2").unwrap_or(0);
            vec![NativeDiagnostic {
                message: "data expression has an unclosed delimiter".to_owned(),
                start,
                end: start + 1,
                source: "quirl-data",
            }]
        }

        let source = "data [1, 2 | ^external touch /tmp/never-run";
        let mut service = LanguageService::new_with_native_analyzer(Catalog::builtin(), analyzer);
        let messages = open(&mut service, "file:///data.qrl", "quirl", source);
        let diagnostic = &messages[0]["params"]["diagnostics"][0];
        assert_eq!(diagnostic["source"], "quirl-data");
        assert_eq!(
            diagnostic["message"],
            "data expression has an unclosed delimiter"
        );
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
            assert!(
                completion["result"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["label"] == "quirl check")
            );
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
        assert!(
            messages[0]["params"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn quirl_diagnostics_share_native_unsupported_constructs_and_spans() {
        let source = "echo $HOME\nprintf ok |\n";
        let expected = check_script(source);
        let actual = quirl_diagnostics(source, None).unwrap();
        assert_eq!(actual.len(), expected.len());
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual["message"], expected.message);
            assert_eq!(
                actual["range"],
                range(source, expected.start, expected.end).unwrap()
            );
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
    fn validated_plugin_catalog_entries_reach_docs_and_completion_without_execution() {
        let mut catalog = Catalog::builtin();
        let mut plugin = catalog.find("quirl data ls").unwrap().clone();
        plugin.id = "plugin:demo/demo/run".to_owned();
        plugin.path = "demo run".to_owned();
        plugin.signature = "demo run".to_owned();
        plugin.parent = None;
        plugin.io.input = "Path".to_owned();
        plugin.io.output = "Values<String>".to_owned();
        plugin.io.streaming = false;
        plugin.provenance =
            quirl_catalog::ProvenanceInfo::builtin(quirl_catalog::Provenance::Plugin);
        for argument in &mut plugin.options {
            argument.provenance = plugin.provenance.clone();
        }
        catalog.merge(vec![plugin]);
        let mut service = LanguageService::new(catalog);

        let docs = request(&mut service, 1, "quirl/moduleDocs", json!({}));
        let docs = docs["result"]["value"].as_str().unwrap();
        assert!(docs.contains("demo run"));
        assert!(docs.contains("Input: `Path`"));
        assert!(docs.contains("Output: `Values<String>`"));
        let completions = quirl_completions(&service.catalog, "demo r", "demo r".len(), "r");
        let completion = completions
            .iter()
            .find(|completion| completion["label"] == "demo run")
            .unwrap();
        assert!(
            completion["documentation"]["value"]
                .as_str()
                .unwrap()
                .contains("Output: `Values<String>`")
        );
        let hover = quirl_hover(&service.catalog, "demo run", 4).unwrap();
        assert!(
            hover["contents"]["value"]
                .as_str()
                .unwrap()
                .contains("Input: `Path`")
        );
        let signature = quirl_signature(&service.catalog, "demo run", 8).unwrap();
        assert!(
            signature["signatures"][0]["documentation"]["value"]
                .as_str()
                .unwrap()
                .contains("Output: `Values<String>`")
        );
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

    #[test]
    fn malformed_complete_message_returns_parse_error_then_serves_next_frame() {
        let valid = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 7, "method": "initialize", "params": {}
        }))
        .unwrap();
        let input = frame(b"{")
            .into_iter()
            .chain(frame(&valid))
            .collect::<Vec<_>>();
        let mut output = Vec::new();

        serve(
            &mut BufReader::new(Cursor::new(input)),
            &mut output,
            Catalog::builtin(),
        )
        .unwrap();

        let responses = framed_json_values(output);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], Value::Null);
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[1]["id"], 7);
        assert_eq!(responses[1]["result"]["serverInfo"]["name"], "quirl-lsp");
    }

    #[test]
    fn invalid_envelope_uses_bounded_null_id_then_serves_next_frame() {
        let exact_id = Value::String("i".repeat(MAX_REQUEST_ID_BYTES - 2));
        let oversized_id = Value::String("i".repeat(MAX_REQUEST_ID_BYTES - 1));
        assert_eq!(
            serde_json::to_vec(&exact_id).unwrap().len(),
            MAX_REQUEST_ID_BYTES
        );
        assert_eq!(
            serde_json::to_vec(&oversized_id).unwrap().len(),
            MAX_REQUEST_ID_BYTES + 1
        );
        assert!(valid_request_id(&exact_id));
        assert!(!valid_request_id(&oversized_id));
        let invalid = serde_json::to_vec(&json!({
            "jsonrpc": "1.0", "id": oversized_id, "method": "initialize", "params": {}
        }))
        .unwrap();
        let valid = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 8, "method": "initialize", "params": {}
        }))
        .unwrap();
        let input = frame(&invalid)
            .into_iter()
            .chain(frame(&valid))
            .collect::<Vec<_>>();
        let mut output = Vec::new();

        serve(
            &mut BufReader::new(Cursor::new(input)),
            &mut output,
            Catalog::builtin(),
        )
        .unwrap();

        let responses = framed_json_values(output);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], Value::Null);
        assert_eq!(responses[0]["error"]["code"], -32600);
        assert!(serde_json::to_vec(&responses[0]).unwrap().len() < 256);
        assert_eq!(responses[1]["id"], 8);
    }

    #[test]
    fn positions_and_ranges_require_exact_unicode_boundaries() {
        let text = "a😀b\né";
        assert_eq!(position_to_offset(text, 0, 1).unwrap(), 1);
        let midpoint = position_to_offset(text, 0, 2).unwrap_err();
        assert!(midpoint.message.contains("surrogate pair"));
        assert_eq!(position_to_offset(text, 0, 3).unwrap(), 5);
        let past_line = position_to_offset(text, 0, 5).unwrap_err();
        assert!(past_line.message.contains("end of the line"));
        let past_document = position_to_offset(text, 2, 0).unwrap_err();
        assert!(past_document.message.contains("end of the document"));

        assert_eq!(offset_to_position(text, 1), Some((0, 1)));
        assert_eq!(offset_to_position(text, 2), None);
        assert_eq!(offset_to_position(text, text.len() + 1), None);
        assert_eq!(
            range(text, 1, 5).unwrap(),
            json!({
                "start": {"line": 0, "character": 1},
                "end": {"line": 0, "character": 3}
            })
        );
        for (start, end) in [(2, 5), (1, text.len() + 1), (5, 1)] {
            let error = range(text, start, end).unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation);
        }
    }

    #[test]
    fn invalid_analyzer_range_rejects_open_without_retaining_document() {
        fn invalid_analyzer(_: &str) -> Vec<NativeDiagnostic> {
            vec![NativeDiagnostic {
                message: "invalid test range".to_owned(),
                start: 1,
                end: 2,
                source: "test-analyzer",
            }]
        }

        let mut service =
            LanguageService::new_with_native_analyzer(Catalog::builtin(), invalid_analyzer);
        let error = service
            .did_open(&open_params("file:///invalid.qrl", "quirl", 1, "é"))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(service.documents.is_empty());
        assert_eq!(service.retained_document_bytes, 0);
    }

    #[test]
    fn duplicate_open_replaces_atomically_and_failed_replacement_preserves_state() {
        let mut service = LanguageService::default();
        let uri = "file:///replace.qrl";
        service
            .did_open(&open_params(uri, "quirl", 1, "echo old"))
            .unwrap();
        let before_bytes = service.retained_document_bytes;

        service
            .did_open(&open_params(uri, "quirl", 2, "echo replacement"))
            .unwrap();
        assert_eq!(service.documents.len(), 1);
        assert_eq!(service.documents[uri].version, 2);
        assert!(service.retained_document_bytes > before_bytes);
        assert_accounting(&service);

        let replacement_bytes = service.retained_document_bytes;
        let oversized = "x".repeat(MAX_DOCUMENT_BYTES + 1);
        let error = service
            .did_open(&open_params(uri, "quirl", 3, &oversized))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(service.documents[uri].version, 2);
        assert_eq!(service.documents[uri].text, "echo replacement");
        assert_eq!(service.retained_document_bytes, replacement_bytes);
        assert_accounting(&service);
    }

    #[test]
    fn change_limits_are_atomic_and_duplicate_changes_are_rejected() {
        let mut service = LanguageService::default();
        let uri = "file:///change.qrl";
        service
            .did_open(&open_params(uri, "quirl", 1, "echo old"))
            .unwrap();
        let before_bytes = service.retained_document_bytes;

        let count_error = service
            .did_change(&change_params(
                uri,
                2,
                vec![json!({"text": "echo one"}), json!({"text": "echo two"})],
            ))
            .unwrap_err();
        assert_eq!(count_error.code, ErrorCode::ResourceLimit);
        assert_eq!(service.documents[uri].version, 1);
        assert_eq!(service.retained_document_bytes, before_bytes);

        let oversized = "x".repeat(MAX_DOCUMENT_BYTES + 1);
        let size_error = service
            .did_change(&change_params(uri, 3, vec![json!({"text": oversized})]))
            .unwrap_err();
        assert_eq!(size_error.code, ErrorCode::ResourceLimit);
        assert_eq!(service.documents[uri].version, 1);
        assert_eq!(service.documents[uri].text, "echo old");

        service
            .did_change(&change_params(uri, 4, vec![json!({"text": "echo new"})]))
            .unwrap();
        assert_eq!(service.documents[uri].version, 4);
        assert_eq!(service.documents[uri].text, "echo new");
        assert_accounting(&service);
    }

    #[test]
    fn open_document_count_close_and_reopen_account_exactly() {
        let mut service = LanguageService::default();
        for index in 0..MAX_OPEN_DOCUMENTS {
            let uri = format!("file:///count-{index}.qrl");
            service
                .did_open(&open_params(&uri, "quirl", 1, ""))
                .unwrap();
        }
        assert_eq!(service.documents.len(), MAX_OPEN_DOCUMENTS);
        assert_accounting(&service);

        let error = service
            .did_open(&open_params("file:///overflow.qrl", "quirl", 1, ""))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(service.documents.len(), MAX_OPEN_DOCUMENTS);

        let closed_uri = "file:///count-0.qrl";
        service
            .did_close(&json!({"textDocument": {"uri": closed_uri}}))
            .unwrap();
        service
            .did_close(&json!({"textDocument": {"uri": closed_uri}}))
            .unwrap();
        service
            .did_open(&open_params("file:///reopened.qrl", "quirl", 1, ""))
            .unwrap();
        assert_eq!(service.documents.len(), MAX_OPEN_DOCUMENTS);
        assert_accounting(&service);
    }

    #[test]
    fn repeated_near_limit_documents_hit_the_aggregate_bound() {
        let mut service = LanguageService::default();
        let text = " ".repeat(MAX_DOCUMENT_BYTES);
        let mut rejected = false;
        for index in 0..=MAX_RETAINED_DOCUMENT_BYTES / MAX_DOCUMENT_BYTES {
            let uri = format!("file:///aggregate-{index}.qrl");
            match service.did_open(&open_params(&uri, "quirl", 1, &text)) {
                Ok(_) => assert!(!rejected),
                Err(error) => {
                    assert_eq!(error.code, ErrorCode::ResourceLimit);
                    rejected = true;
                    break;
                }
            }
        }
        assert!(rejected);
        assert!(service.retained_document_bytes <= MAX_RETAINED_DOCUMENT_BYTES);
        assert_accounting(&service);
    }

    #[test]
    fn uri_limits_use_utf8_bytes_and_apply_to_close() {
        let mut service = LanguageService::default();
        let exact_uri = "u".repeat(MAX_URI_BYTES);
        service
            .did_open(&open_params(&exact_uri, "quirl", 1, ""))
            .unwrap();
        let before_bytes = service.retained_document_bytes;
        let hostile_uri = "é".repeat(MAX_URI_BYTES / 2 + 1);
        let error = service
            .did_close(&json!({"textDocument": {"uri": hostile_uri}}))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(service.retained_document_bytes, before_bytes);
        assert!(service.documents.contains_key(&exact_uri));
    }

    #[test]
    fn accounting_overflow_and_shutdown_fail_closed_and_release_state() {
        let mut service = LanguageService {
            retained_document_bytes: usize::MAX,
            ..LanguageService::default()
        };
        let error = service
            .did_open(&open_params("file:///overflow.qrl", "quirl", 1, "x"))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(service.documents.is_empty());

        service.retained_document_bytes = 0;
        service
            .did_open(&open_params("file:///shutdown.qrl", "quirl", 1, "echo ok"))
            .unwrap();
        service.dispatch("shutdown", json!({})).unwrap();
        assert!(service.documents.is_empty());
        assert_eq!(service.retained_document_bytes, 0);
        assert_accounting(&service);
    }

    #[test]
    fn dispatch_rejects_updates_after_shutdown_without_retaining_state() {
        let mut service = LanguageService::default();
        service.dispatch("shutdown", json!({})).unwrap();
        let error = service
            .dispatch(
                "textDocument/didOpen",
                open_params("file:///late.qrl", "quirl", 1, ""),
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(service.documents.is_empty());
        assert_eq!(service.retained_document_bytes, 0);
    }

    #[test]
    fn framing_bounds_headers_lengths_and_partial_bodies() {
        let exact_body = vec![b' '; MAX_MESSAGE_BYTES];
        let exact_frame = frame(&exact_body);
        let exact = read_message(&mut BufReader::new(Cursor::new(exact_frame)))
            .unwrap()
            .unwrap();
        assert_eq!(exact.len(), MAX_MESSAGE_BYTES);

        let oversized_header = vec![b'x'; MAX_HEADER_BYTES + 1];
        let error = read_message(&mut BufReader::new(Cursor::new(oversized_header))).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);

        let mut many_headers = Vec::new();
        for _ in 0..=MAX_HEADER_COUNT {
            many_headers.extend_from_slice(b"X: y\r\n");
        }
        many_headers.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        let count_error = read_message(&mut BufReader::new(Cursor::new(many_headers))).unwrap_err();
        assert_eq!(count_error.code, ErrorCode::ResourceLimit);

        let oversized_body = format!("Content-Length: {}\r\n\r\n", MAX_MESSAGE_BYTES + 1);
        let body_error =
            read_message(&mut BufReader::new(Cursor::new(oversized_body))).unwrap_err();
        assert_eq!(body_error.code, ErrorCode::ResourceLimit);

        let partial = b"Content-Length: 5\r\n\r\n{}";
        let partial_error = read_message(&mut BufReader::new(Cursor::new(partial))).unwrap_err();
        assert_eq!(partial_error.code, ErrorCode::Validation);
        assert!(
            partial_error
                .details
                .context
                .iter()
                .any(|context| context.contains("received_bytes"))
        );

        let partial_headers = b"X-Test: incomplete\r\n";
        let header_error =
            read_message(&mut BufReader::new(Cursor::new(partial_headers))).unwrap_err();
        assert_eq!(header_error.code, ErrorCode::Validation);
    }

    #[test]
    fn framing_treats_boundary_eof_as_clean_and_truncated_frames_as_terminal() {
        assert!(
            read_message(&mut BufReader::new(Cursor::new(Vec::<u8>::new())))
                .unwrap()
                .is_none()
        );

        let complete = frame(b"{}");
        let mut complete_reader = BufReader::new(Cursor::new(complete));
        assert_eq!(read_message(&mut complete_reader).unwrap().unwrap(), b"{}");
        assert!(read_message(&mut complete_reader).unwrap().is_none());

        let truncated = frame(b"{}");
        let truncated = &truncated[..truncated.len() - 1];
        let error = read_message(&mut BufReader::new(Cursor::new(truncated))).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(
            error
                .details
                .context
                .iter()
                .any(|context| context.contains("received_bytes"))
        );
    }

    #[test]
    fn duplicate_and_overflowing_content_lengths_fail_closed() {
        let duplicate = b"Content-Length: 0\r\nContent-Length: 0\r\n\r\n";
        let duplicate_error =
            read_message(&mut BufReader::new(Cursor::new(duplicate))).unwrap_err();
        assert_eq!(duplicate_error.code, ErrorCode::Validation);

        let overflow = format!("Content-Length: {}0\r\n\r\n", usize::MAX);
        let overflow_error = read_message(&mut BufReader::new(Cursor::new(overflow))).unwrap_err();
        assert_eq!(overflow_error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn command_recognition_requires_an_exact_token_boundary() {
        let catalog = Catalog::builtin();
        assert_eq!(
            command_at(&catalog, "quirl check").map(|command| command.path.as_str()),
            Some("quirl check")
        );
        assert_eq!(
            command_at(&catalog, "  quirl check --format json")
                .map(|command| command.path.as_str()),
            Some("quirl check")
        );
        for line in [
            "quirl checkmate",
            "quirl check-more",
            "quirl check.more",
            "quirl checker",
        ] {
            assert_ne!(
                command_at(&catalog, line).map(|command| command.path.as_str()),
                Some("quirl check")
            );
        }
    }
}
