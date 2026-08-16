use clap::{Subcommand, ValueEnum};
use quirl_core::{escape_json_terminal_controls, escape_terminal_controls, ErrorCode, ShellError};
use quirl_lua::{
    format_source, LuaPolicy, LuaRuntime, QuirlConfig, CONFIG_SCHEMA_VERSION, MAX_LUA_SOURCE_BYTES,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Parse, evaluate under config restrictions, and validate against Rust schemas.
    Check {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
    /// Open a loopback-only, schema-backed configuration form.
    Web {
        file: PathBuf,
        /// Loopback TCP port; 0 selects an available port.
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Print one evaluated, schema-backed configuration value.
    Get { file: PathBuf, key: String },
    /// Patch one recognized literal, validate the candidate, and retain a .bak.
    Set {
        file: PathBuf,
        key: String,
        value: String,
    },
    /// Show the current schema and values as an accessible line-oriented view.
    Tui { file: PathBuf },
    /// Format a configuration file with Quirl's deterministic Lua formatter.
    Fmt {
        file: PathBuf,
        /// Report formatting drift without writing the source file.
        #[arg(long)]
        check: bool,
    },
    /// Export the evaluated, schema-backed configuration without modifying its source.
    Export {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Json)]
        format: ConfigOutputFormat,
    },
    /// Compare two evaluated configuration files field by field.
    Diff {
        file: PathBuf,
        other: PathBuf,
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
    /// Preview the source migration required by the current configuration schema.
    Migrate {
        file: PathBuf,
        /// Migration is preview-only in 0.1.0; this flag is required to make that explicit.
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
    /// Diagnose schema validity and which settings can be safely patched as literals.
    Doctor {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ConfigOutputFormat {
    Text,
    Json,
}

pub fn wants_json(command: &ConfigCommand) -> bool {
    matches!(
        command,
        ConfigCommand::Check {
            format: ConfigOutputFormat::Json,
            ..
        } | ConfigCommand::Export {
            format: ConfigOutputFormat::Json,
            ..
        } | ConfigCommand::Diff {
            format: ConfigOutputFormat::Json,
            ..
        } | ConfigCommand::Migrate {
            format: ConfigOutputFormat::Json,
            ..
        } | ConfigCommand::Doctor {
            format: ConfigOutputFormat::Json,
            ..
        }
    )
}

pub fn execute(command: ConfigCommand) -> Result<i32, ShellError> {
    match command {
        ConfigCommand::Check { file, format } => check(&file, format),
        ConfigCommand::Get { file, key } => get(&file, &key),
        ConfigCommand::Set { file, key, value } => set(&file, &key, &value),
        ConfigCommand::Tui { file } => tui(&file),
        ConfigCommand::Web { file, port } => web(&file, port),
        ConfigCommand::Fmt { file, check } => format(&file, check),
        ConfigCommand::Export { file, format } => export(&file, format),
        ConfigCommand::Diff {
            file,
            other,
            format,
        } => diff(&file, &other, format),
        ConfigCommand::Migrate {
            file,
            dry_run,
            format,
        } => migrate(&file, dry_run, format),
        ConfigCommand::Doctor { file, format } => doctor(&file, format),
    }
}

fn check(file: &Path, format: ConfigOutputFormat) -> Result<i32, ShellError> {
    let runtime = LuaRuntime::new(LuaPolicy::config())?;
    match runtime.load_config_file(file) {
        Ok(config) => {
            match format {
                ConfigOutputFormat::Text => {
                    println!(
                        "✓ {} is valid Lua configuration",
                        escape_terminal_controls(&file.display().to_string())
                    );
                }
                ConfigOutputFormat::Json => print_json(&config)?,
            }
            Ok(0)
        }
        Err(error) if matches!(format, ConfigOutputFormat::Json) => {
            print_json(&error)?;
            Ok(1)
        }
        Err(error) => Err(error),
    }
}

fn get(file: &Path, key: &str) -> Result<i32, ShellError> {
    let field = ConfigField::parse(key)?;
    let config = load(file)?;
    println!("{}", escape_terminal_controls(&field.value(&config)));
    Ok(0)
}

fn tui(file: &Path) -> Result<i32, ShellError> {
    let config = load(file)?;
    println!(
        "Quirl configuration · {}",
        escape_terminal_controls(&file.display().to_string())
    );
    println!("read-only line view; use `quirl config set` to change a literal value\n");
    println!("[editor]");
    println!(
        "editor.keymap = {}  (helix | emacs | vim)",
        escape_terminal_controls(&config.editor.keymap)
    );
    println!(
        "editor.semantic_hints = {}  (true | false)",
        config.editor.semantic_hints
    );
    println!("\n[picker]");
    println!(
        "picker.layout = {}  (adaptive | bottom | full)",
        escape_terminal_controls(&config.picker.layout)
    );
    println!("picker.preview = {}  (true | false)", config.picker.preview);
    println!("\n[prompt]");
    println!(
        "prompt.left = {}",
        escape_terminal_controls(&ConfigField::PromptLeft.value(&config))
    );
    println!(
        "prompt.right = {}",
        escape_terminal_controls(&ConfigField::PromptRight.value(&config))
    );
    println!("\nOpen the synchronized form with `quirl config web <file>`.");
    Ok(0)
}

#[derive(Debug, Serialize)]
struct ConfigExportReport {
    document_type: &'static str,
    schema_version: u32,
    source: String,
    config: QuirlConfig,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct ConfigDifference {
    key: &'static str,
    before: String,
    after: String,
}

#[derive(Debug, Serialize)]
struct ConfigDiffReport {
    document_type: &'static str,
    schema_version: u32,
    left: String,
    right: String,
    differences: Vec<ConfigDifference>,
}

#[derive(Debug, Serialize)]
struct ConfigMigrationReport {
    document_type: &'static str,
    schema_version: u32,
    source: String,
    source_schema_version: Option<u32>,
    target_schema_version: u32,
    changed: bool,
    dry_run: bool,
    candidate_source: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConfigDoctorReport {
    document_type: &'static str,
    schema_version: u32,
    source: String,
    valid: bool,
    config_schema_version: u32,
    literal_fields: Vec<&'static str>,
    code_controlled_fields: Vec<&'static str>,
}

fn format(file: &Path, check: bool) -> Result<i32, ShellError> {
    let source = read_config_source(file)?;
    load(file)?;
    let formatted = format_source(&source);
    let changed = source != formatted;
    if changed && !check {
        let temporary = temporary_path(file)?;
        let result = install_candidate(file, &temporary, &formatted, &source);
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
    }
    let rendered_path = escape_terminal_controls(&file.display().to_string());
    if check && changed {
        println!("needs formatting {rendered_path}");
        return Ok(1);
    }
    println!(
        "{} {rendered_path}",
        if changed { "formatted" } else { "unchanged" }
    );
    Ok(0)
}

fn export(file: &Path, format: ConfigOutputFormat) -> Result<i32, ShellError> {
    let _source = read_config_source(file)?;
    let config = load(file)?;
    match format {
        ConfigOutputFormat::Json => print_json(&ConfigExportReport {
            document_type: "quirl.config.export",
            schema_version: 1,
            source: file.display().to_string(),
            config,
        })?,
        ConfigOutputFormat::Text => print_config_text(&config),
    }
    Ok(0)
}

fn diff(left: &Path, right: &Path, format: ConfigOutputFormat) -> Result<i32, ShellError> {
    let _left_source = read_config_source(left)?;
    let _right_source = read_config_source(right)?;
    let left_config = load(left)?;
    let right_config = load(right)?;
    let differences = config_differences(&left_config, &right_config);
    match format {
        ConfigOutputFormat::Json => print_json(&ConfigDiffReport {
            document_type: "quirl.config.diff",
            schema_version: 1,
            left: left.display().to_string(),
            right: right.display().to_string(),
            differences,
        })?,
        ConfigOutputFormat::Text if differences.is_empty() => println!(
            "no configuration differences: {} and {}",
            escape_terminal_controls(&left.display().to_string()),
            escape_terminal_controls(&right.display().to_string())
        ),
        ConfigOutputFormat::Text => {
            for difference in differences {
                println!(
                    "{}: {} -> {}",
                    difference.key,
                    escape_terminal_controls(&difference.before),
                    escape_terminal_controls(&difference.after)
                );
            }
        }
    }
    Ok(0)
}

fn migrate(file: &Path, dry_run: bool, format: ConfigOutputFormat) -> Result<i32, ShellError> {
    if !dry_run {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "configuration migration is preview-only",
        )
        .with_help("Pass `--dry-run`; Quirl 0.1.0 never rewrites configuration during migration"));
    }
    let source = read_config_source(file)?;
    load(file)?;
    let (source_schema_version, candidate) = migration_candidate(&source)?;
    let report = ConfigMigrationReport {
        document_type: "quirl.config.migration",
        schema_version: 1,
        source: file.display().to_string(),
        source_schema_version,
        target_schema_version: CONFIG_SCHEMA_VERSION,
        changed: candidate != source,
        dry_run: true,
        candidate_source: (candidate != source).then(|| candidate.clone()),
    };
    match format {
        ConfigOutputFormat::Json => print_json(&report)?,
        ConfigOutputFormat::Text if report.changed => {
            println!(
                "would migrate {} from unversioned config to schema_version {}; no files changed\n--- migration preview ---",
                escape_terminal_controls(&file.display().to_string()),
                CONFIG_SCHEMA_VERSION
            );
            if let Some(candidate) = &report.candidate_source {
                print!("{}", escape_terminal_controls(candidate));
                if !candidate.ends_with('\n') {
                    println!();
                }
            }
        }
        ConfigOutputFormat::Text => println!(
            "{} already declares schema_version {}; no files changed",
            escape_terminal_controls(&file.display().to_string()),
            CONFIG_SCHEMA_VERSION
        ),
    }
    Ok(0)
}

fn doctor(file: &Path, format: ConfigOutputFormat) -> Result<i32, ShellError> {
    let source = read_config_source(file)?;
    let config = load(file)?;
    let (literal_fields, code_controlled_fields) = patchable_fields(&source, &config);
    let report = ConfigDoctorReport {
        document_type: "quirl.config.doctor",
        schema_version: 1,
        source: file.display().to_string(),
        valid: true,
        config_schema_version: config.schema_version,
        literal_fields,
        code_controlled_fields,
    };
    match format {
        ConfigOutputFormat::Json => print_json(&report)?,
        ConfigOutputFormat::Text => {
            println!(
                "configuration is valid: {} (schema_version {})",
                escape_terminal_controls(&file.display().to_string()),
                report.config_schema_version
            );
            println!("literal fields: {}", report.literal_fields.join(", "));
            if report.code_controlled_fields.is_empty() {
                println!("code-controlled fields: none");
            } else {
                println!(
                    "code-controlled fields: {}",
                    report.code_controlled_fields.join(", ")
                );
            }
        }
    }
    Ok(0)
}

fn print_config_text(config: &QuirlConfig) {
    println!("schema_version = {}", config.schema_version);
    for field in ConfigField::ALL {
        println!(
            "{} = {}",
            field.key(),
            escape_terminal_controls(&field.value(config))
        );
    }
}

fn config_differences(left: &QuirlConfig, right: &QuirlConfig) -> Vec<ConfigDifference> {
    let mut differences = Vec::new();
    if left.schema_version != right.schema_version {
        differences.push(ConfigDifference {
            key: "schema_version",
            before: left.schema_version.to_string(),
            after: right.schema_version.to_string(),
        });
    }
    for field in ConfigField::ALL {
        let before = field.value(left);
        let after = field.value(right);
        if before != after {
            differences.push(ConfigDifference {
                key: field.key(),
                before,
                after,
            });
        }
    }
    differences
}

fn migration_candidate(source: &str) -> Result<(Option<u32>, String), ShellError> {
    let tokens = tokenize(source);
    let config_open = find_config_table(&tokens)
        .ok_or_else(|| patch_error("could not find a literal `quirl.config { ... }` table"))?;
    let values = field_values(&tokens, config_open, "schema_version");
    let Some(value) = values.first() else {
        let insert_at = tokens[config_open].end;
        let mut candidate = String::with_capacity(source.len() + 24);
        candidate.push_str(&source[..insert_at]);
        candidate.push_str("\n  schema_version = 1,");
        candidate.push_str(&source[insert_at..]);
        return Ok((None, candidate));
    };
    if values.len() != 1 {
        return Err(patch_error(
            "expected at most one literal `schema_version` field during migration",
        ));
    }
    let token = &tokens[*value];
    if !matches!(&token.kind, TokenKind::Other) {
        return Err(patch_error(
            "schema_version is code-controlled and cannot be migrated automatically",
        ));
    }
    let tail = &source[token.start..];
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0
        || !matches!(
            tail[digits..].trim_start().chars().next(),
            Some(',' | ';' | '}')
        )
    {
        return Err(patch_error(
            "schema_version must be an integer literal for migration preview",
        ));
    }
    let version = tail[..digits].parse::<u32>().map_err(|_| {
        patch_error("schema_version must be an integer literal for migration preview")
    })?;
    Ok((Some(version), source.to_owned()))
}

fn patchable_fields(source: &str, config: &QuirlConfig) -> (Vec<&'static str>, Vec<&'static str>) {
    let mut literal = Vec::new();
    let mut code_controlled = Vec::new();
    for field in ConfigField::ALL {
        let replacement = match field.lua_literal(&field.value(config)) {
            Ok(replacement) => replacement,
            Err(_) => {
                code_controlled.push(field.key());
                continue;
            }
        };
        if patch_literal(source, field, &replacement).is_ok() {
            literal.push(field.key());
        } else {
            code_controlled.push(field.key());
        }
    }
    (literal, code_controlled)
}

const WEB_MAX_REQUEST_BYTES: usize = 32 * 1024;
const WEB_MAX_HEADER_BYTES: usize = 8 * 1024;
const WEB_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
struct WebSession {
    token: String,
    source: String,
    config: QuirlConfig,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    query: String,
    headers: BTreeMap<String, String>,
    body: String,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: String,
}

/// Starts a deliberately small local-only server. It has no assets, cookies,
/// proxy support, or ambient network binding: the capability is the unguessable
/// URL token printed to the invoking terminal.
fn web(file: &Path, port: u16) -> Result<i32, ShellError> {
    let source = fs::read_to_string(file).map_err(|error| file_error("read", file, error))?;
    let config = load(file)?;
    let listener =
        TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                "could not bind the local configuration server",
            )
            .with_context(error.to_string())
            .with_help("Choose another --port, or omit it to select an available loopback port")
        })?;
    listener.set_nonblocking(true).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "could not configure the local configuration server",
        )
        .with_context(error.to_string())
        .with_help("Retry the command; if it persists, choose a different --port")
    })?;
    let token = session_token()?;
    let address = listener.local_addr().map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "could not determine the local configuration address",
        )
        .with_context(error.to_string())
        .with_help("Retry the command with an explicit loopback --port")
    })?;
    println!(
        "Quirl configuration form: http://{address}/?token={token}\nListening only on loopback; the form closes after ten idle minutes. Press Ctrl-C to stop."
    );
    let mut session = WebSession {
        token,
        source,
        config,
    };
    serve_web(&listener, file, &mut session)?;
    Ok(0)
}

fn serve_web(
    listener: &TcpListener,
    file: &Path,
    session: &mut WebSession,
) -> Result<(), ShellError> {
    let mut deadline = Instant::now() + WEB_IDLE_TIMEOUT;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, peer)) => {
                // A listener bound to 127.0.0.1 should only yield loopback peers,
                // but retain the check if an OS or future binding changes that.
                if !peer.ip().is_loopback() {
                    let _ = write_http_response(&mut stream, &HttpResponse::forbidden());
                    continue;
                }
                let response = match read_http_request(&mut stream) {
                    Ok(request) => handle_web_request(file, session, request),
                    Err(error) => HttpResponse::error(400, &error.message),
                };
                write_http_response(&mut stream, &response)?;
                deadline = Instant::now() + WEB_IDLE_TIMEOUT;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(file_error("accept a local web connection for", file, error)),
        }
    }
    Ok(())
}

fn handle_web_request(file: &Path, session: &mut WebSession, request: HttpRequest) -> HttpResponse {
    if request.path != "/" {
        return HttpResponse::error(404, "Not found");
    }
    if !request
        .headers
        .get("host")
        .is_some_and(|host| loopback_authority(host))
    {
        return HttpResponse::forbidden();
    }
    if request
        .headers
        .get("origin")
        .is_some_and(|origin| !loopback_origin(origin))
    {
        return HttpResponse::forbidden();
    }
    match request.method.as_str() {
        "GET" => {
            let query = match parse_form(&request.query) {
                Ok(query) => query,
                Err(error) => return HttpResponse::error(400, &error.message),
            };
            if query.get("token") != Some(&session.token) {
                return HttpResponse::forbidden();
            }
            match refresh_session(file, session) {
                Ok(()) => HttpResponse::page(200, render_form(session, None)),
                Err(error) => HttpResponse::page(500, render_form(session, Some(&error.message))),
            }
        }
        "POST" => {
            if !request
                .headers
                .get("content-type")
                .is_some_and(|value| value.starts_with("application/x-www-form-urlencoded"))
            {
                return HttpResponse::error(
                    415,
                    "Expected an application/x-www-form-urlencoded form",
                );
            }
            let form = match parse_form(&request.body) {
                Ok(form) => form,
                Err(error) => {
                    return HttpResponse::page(400, render_form(session, Some(&error.message)))
                }
            };
            if form.get("csrf") != Some(&session.token) {
                return HttpResponse::forbidden();
            }
            if form.get("revision") != Some(&source_revision(&session.source)) {
                return HttpResponse::page(
                    409,
                    render_form(
                        session,
                        Some("This page is stale. Reload it before saving."),
                    ),
                );
            }
            match apply_web_form(file, session, &form) {
                Ok(message) => HttpResponse::page(200, render_form(session, Some(&message))),
                Err(error) => {
                    let status = if error.message.contains("changed after")
                        || error.message.contains("changed concurrently")
                    {
                        409
                    } else {
                        422
                    };
                    HttpResponse::page(status, render_form(session, Some(&error.message)))
                }
            }
        }
        _ => HttpResponse::error(405, "Only GET and POST are supported"),
    }
}

fn loopback_authority(authority: &str) -> bool {
    authority == "127.0.0.1"
        || authority
            .strip_prefix("127.0.0.1:")
            .is_some_and(|port| !port.is_empty() && port.parse::<u16>().is_ok())
}

fn loopback_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .and_then(|value| value.split('/').next())
        .is_some_and(loopback_authority)
}

fn refresh_session(file: &Path, session: &mut WebSession) -> Result<(), ShellError> {
    let source = fs::read_to_string(file).map_err(|error| file_error("read", file, error))?;
    let config = load(file)?;
    session.source = source;
    session.config = config;
    Ok(())
}

fn apply_web_form(
    file: &Path,
    session: &mut WebSession,
    form: &BTreeMap<String, String>,
) -> Result<String, ShellError> {
    let mut replacements = Vec::new();
    collect_web_change(
        &mut replacements,
        ConfigField::EditorKeymap,
        required_form_value(form, "editor_keymap")?,
        &session.config.editor.keymap,
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::EditorSemanticHints,
        required_form_value(form, "editor_semantic_hints")?,
        &session.config.editor.semantic_hints.to_string(),
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::PickerLayout,
        required_form_value(form, "picker_layout")?,
        &session.config.picker.layout,
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::PickerPreview,
        required_form_value(form, "picker_preview")?,
        &session.config.picker.preview.to_string(),
    )?;
    let prompt_left = parse_prompt_lines(required_form_value(form, "prompt_left")?)?;
    let prompt_right = parse_prompt_lines(required_form_value(form, "prompt_right")?)?;
    if prompt_left != session.config.prompt.left {
        replacements.push((ConfigField::PromptLeft, lua_string_array(&prompt_left)));
    }
    if prompt_right != session.config.prompt.right {
        replacements.push((ConfigField::PromptRight, lua_string_array(&prompt_right)));
    }
    if replacements.is_empty() {
        return Ok("No changes to save.".to_owned());
    }
    // Three-way merge: compare the form's base value with the current evaluated
    // configuration for every field the user changed. Non-overlapping source
    // edits are preserved; edits to the same field become a visible conflict.
    let current_source =
        fs::read_to_string(file).map_err(|error| file_error("re-read", file, error))?;
    let current_config = if current_source == session.source {
        session.config.clone()
    } else {
        load(file)?
    };
    for (field, _) in &replacements {
        if field.value(&current_config) != field.value(&session.config) {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("configuration field changed concurrently: {}", field.key()),
            )
            .with_help(
                "Reload the configuration form, review the external value, and submit again",
            ));
        }
    }
    let candidate = patch_literals(&current_source, &replacements)?;
    let temporary = temporary_path(file)?;
    let result = install_candidate(file, &temporary, &candidate, &current_source);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    session.source = candidate;
    session.config = load(file)?;
    Ok(format!(
        "Saved safely. Backup: {}",
        backup_path(file).display()
    ))
}

fn collect_web_change(
    replacements: &mut Vec<(ConfigField, String)>,
    field: ConfigField,
    submitted: &str,
    current: &str,
) -> Result<(), ShellError> {
    if submitted != current {
        replacements.push((field, field.lua_literal(submitted)?));
    }
    Ok(())
}

fn required_form_value<'a>(
    form: &'a BTreeMap<String, String>,
    key: &str,
) -> Result<&'a str, ShellError> {
    form.get(key).map(String::as_str).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("missing form field `{key}`"),
        )
        .with_help("Reload the local configuration form and submit it unchanged")
    })
}

fn parse_prompt_lines(value: &str) -> Result<Vec<String>, ShellError> {
    if value.len() > 4096 {
        return Err(
            ShellError::new(ErrorCode::ResourceLimit, "prompt segment list is too large")
                .with_help("Keep each prompt list below 4096 bytes"),
        );
    }
    let values = value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned())
        .collect::<Vec<_>>();
    if values.iter().all(|value| valid_prompt_item(value)) {
        Ok(values)
    } else {
        Err(
            ShellError::new(ErrorCode::InvalidArgument, "invalid prompt segment name")
                .with_help("Use non-empty names up to 128 characters without control characters"),
        )
    }
}

impl HttpResponse {
    fn page(status: u16, body: String) -> Self {
        Self { status, body }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::page(
            status,
            format!(
                "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>Quirl configuration</title><main><h1>Quirl configuration</h1><p role=\"alert\">{}</p></main></html>",
                html_escape(message)
            ),
        )
    }

    fn forbidden() -> Self {
        Self::error(
            403,
            "This local configuration session requires its private URL token.",
        )
    }
}

fn render_form(session: &WebSession, notice: Option<&str>) -> String {
    let config = &session.config;
    let selected = |value: &str, option: &str| if value == option { " selected" } else { "" };
    let notice = notice.map_or_else(String::new, |message| {
        format!(
            "<p role=\"status\" aria-live=\"polite\">{}</p>",
            html_escape(message)
        )
    });
    format!(
        "<!doctype html>
<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">
<meta name=\"referrer\" content=\"no-referrer\"><title>Quirl configuration</title>
<style>body{{font:1rem system-ui,sans-serif;max-width:48rem;margin:2rem auto;padding:0 1rem}}fieldset{{margin:1rem 0;padding:1rem}}label{{display:block;margin:.75rem 0}}select,textarea{{font:inherit;max-width:100%;width:24rem}}textarea{{height:7rem}}button{{font:inherit;padding:.5rem 1rem}}[role=status]{{padding:.75rem;background:#eef}}</style></head>
<body><main><h1>Quirl configuration</h1><p><code>config.lua</code> is the source of truth. Saves validate Lua, retain a <code>.bak</code>, and refuse concurrent edits.</p>{notice}
<form method=\"post\" action=\"/\"><input type=\"hidden\" name=\"csrf\" value=\"{token}\"><input type=\"hidden\" name=\"revision\" value=\"{revision}\">
<fieldset><legend>Schema</legend><p>Version <output>{schema}</output> (managed by Quirl; not edited by this form).</p></fieldset>
<fieldset><legend>Editor</legend><label for=\"editor-keymap\">Keymap <select id=\"editor-keymap\" name=\"editor_keymap\"><option value=\"helix\"{key_helix}>helix</option><option value=\"emacs\"{key_emacs}>emacs</option><option value=\"vim\"{key_vim}>vim</option></select></label><label for=\"editor-semantic-hints\">Semantic hints <select id=\"editor-semantic-hints\" name=\"editor_semantic_hints\"><option value=\"true\"{semantic_true}>true</option><option value=\"false\"{semantic_false}>false</option></select></label></fieldset>
<fieldset><legend>Picker</legend><label for=\"picker-layout\">Layout <select id=\"picker-layout\" name=\"picker_layout\"><option value=\"adaptive\"{layout_adaptive}>adaptive</option><option value=\"bottom\"{layout_bottom}>bottom</option><option value=\"full\"{layout_full}>full</option></select></label><label for=\"picker-preview\">Preview <select id=\"picker-preview\" name=\"picker_preview\"><option value=\"true\"{preview_true}>true</option><option value=\"false\"{preview_false}>false</option></select></label></fieldset>
<fieldset><legend>Prompt</legend><p>One segment name per line.</p><label for=\"prompt-left\">Left <textarea id=\"prompt-left\" name=\"prompt_left\">{prompt_left}</textarea></label><label for=\"prompt-right\">Right <textarea id=\"prompt-right\" name=\"prompt_right\">{prompt_right}</textarea></label></fieldset>
<button type=\"submit\">Save configuration</button></form></main></body></html>",
        token = html_escape(&session.token),
        revision = html_escape(&source_revision(&session.source)),
        schema = config.schema_version,
        key_helix = selected(&config.editor.keymap, "helix"),
        key_emacs = selected(&config.editor.keymap, "emacs"),
        key_vim = selected(&config.editor.keymap, "vim"),
        semantic_true = selected(&config.editor.semantic_hints.to_string(), "true"),
        semantic_false = selected(&config.editor.semantic_hints.to_string(), "false"),
        layout_adaptive = selected(&config.picker.layout, "adaptive"),
        layout_bottom = selected(&config.picker.layout, "bottom"),
        layout_full = selected(&config.picker.layout, "full"),
        preview_true = selected(&config.picker.preview.to_string(), "true"),
        preview_false = selected(&config.picker.preview.to_string(), "false"),
        prompt_left = html_escape(&config.prompt.left.join("\n")),
        prompt_right = html_escape(&config.prompt.right.join("\n")),
    )
}

fn source_revision(source: &str) -> String {
    // This is not a secret: the CSRF token authenticates the session. A stable
    // revision detects stale tabs without placing the Lua source in an HTML form.
    let hash = source
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{:016x}-{}", hash, source.len())
}

fn html_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect::<Vec<_>>(),
            '>' => "&gt;".chars().collect::<Vec<_>>(),
            '"' => "&quot;".chars().collect::<Vec<_>>(),
            '\'' => "&#39;".chars().collect::<Vec<_>>(),
            _ => std::iter::once(character).collect(),
        })
        .collect()
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, ShellError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not limit a local web request")
                .with_context(error.to_string())
                .with_help("Reload the local form; Quirl closes requests that cannot be bounded")
        })?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 2048];
    let header_end = loop {
        if bytes.len() > WEB_MAX_REQUEST_BYTES {
            return Err(request_error("request is too large"));
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err(request_error("request ended before its headers")),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    if end > WEB_MAX_HEADER_BYTES {
                        return Err(request_error("request headers are too large"));
                    }
                    break end + 4;
                }
            }
            Err(error) => return Err(request_error(&format!("could not read request: {error}"))),
        }
    };
    let head = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| request_error("request headers must be UTF-8"))?;
    let (method, target, headers) = parse_http_head(head)?;
    let target = target.to_owned();
    let content_length = headers.get("content-length").map_or(Ok(0_usize), |value| {
        value
            .parse::<usize>()
            .map_err(|_| request_error("invalid Content-Length"))
    })?;
    if !request_size_is_allowed(header_end, content_length) {
        return Err(request_error("request body is too large"));
    }
    while bytes.len() < header_end + content_length {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| request_error(&format!("could not read request body: {error}")))?;
        if count == 0 {
            return Err(request_error("request ended before its body"));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = std::str::from_utf8(&bytes[header_end..header_end + content_length])
        .map_err(|_| request_error("request body must be UTF-8"))?
        .to_owned();
    let (path, query) = target
        .split_once('?')
        .map_or((target.as_str(), ""), |(path, query)| (path, query));
    if !path.starts_with('/') || path.contains('\0') {
        return Err(request_error("invalid request target"));
    }
    Ok(HttpRequest {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body,
    })
}

fn request_size_is_allowed(header_end: usize, content_length: usize) -> bool {
    content_length <= WEB_MAX_REQUEST_BYTES
        && header_end
            .checked_add(content_length)
            .is_some_and(|total| total <= WEB_MAX_REQUEST_BYTES)
}

fn parse_http_head(head: &str) -> Result<(String, &str, BTreeMap<String, String>), ShellError> {
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| request_error("missing request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| request_error("missing request method"))?;
    let target = parts
        .next()
        .ok_or_else(|| request_error("missing request target"))?;
    let version = parts
        .next()
        .ok_or_else(|| request_error("missing HTTP version"))?;
    if parts.next().is_some()
        || !matches!(method, "GET" | "POST")
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
    {
        return Err(request_error("unsupported HTTP request"));
    }
    let mut headers = BTreeMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| request_error("malformed HTTP header"))?;
        if name.is_empty()
            || name
                .bytes()
                .any(|byte| !byte.is_ascii_alphanumeric() && byte != b'-')
        {
            return Err(request_error("malformed HTTP header name"));
        }
        let value = value.trim();
        if value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(request_error("malformed HTTP header value"));
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.to_owned())
            .is_some()
        {
            return Err(request_error("duplicate HTTP header"));
        }
    }
    Ok((method.to_owned(), target, headers))
}

fn write_http_response(stream: &mut TcpStream, response: &HttpResponse) -> Result<(), ShellError> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        _ => "Internal Server Error",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store, max-age=0\r\nPragma: no-cache\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        response.status, reason, response.body.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(response.body.as_bytes()))
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not respond to a local web request")
                .with_context(error.to_string())
                .with_help("Reload the private loopback URL and submit the form again")
        })
}

fn parse_form(input: &str) -> Result<BTreeMap<String, String>, ShellError> {
    let mut values = BTreeMap::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| request_error("malformed form field"))?;
        let key = percent_decode(key)?;
        let value = percent_decode(value)?;
        if key.len() > 128
            || value.len() > 4096
            || key.bytes().any(|byte| byte.is_ascii_control())
            || value.bytes().any(|byte| byte == 0)
        {
            return Err(request_error("form field exceeds local editor limits"));
        }
        if values.insert(key, value).is_some() {
            return Err(request_error("duplicate form field"));
        }
    }
    Ok(values)
}

fn percent_decode(value: &str) -> Result<String, ShellError> {
    let mut output = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' => {
                let high = *bytes
                    .get(index + 1)
                    .ok_or_else(|| request_error("incomplete percent escape"))?;
                let low = *bytes
                    .get(index + 2)
                    .ok_or_else(|| request_error("incomplete percent escape"))?;
                output.push((hex_value(high)? << 4) | hex_value(low)?);
                index += 2;
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8(output).map_err(|_| request_error("form fields must be UTF-8"))
}

fn hex_value(byte: u8) -> Result<u8, ShellError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(request_error("invalid percent escape")),
    }
}

fn request_error(message: &str) -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, message)
        .with_help("Reload the local configuration form and try again")
}

fn session_token() -> Result<String, ShellError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "could not obtain secure local session entropy",
        )
        .with_context(error.to_string())
        .with_help("Retry after the operating system random source is available")
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn load(file: &Path) -> Result<QuirlConfig, ShellError> {
    LuaRuntime::new(LuaPolicy::config())?.load_config_file(file)
}

fn read_config_source(file: &Path) -> Result<String, ShellError> {
    let input = File::open(file).map_err(|error| file_error("read", file, error))?;
    let size = input
        .metadata()
        .map_err(|error| file_error("inspect", file, error))?
        .len();
    if size > MAX_LUA_SOURCE_BYTES as u64 {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!(
                "configuration source {} exceeds its read limit",
                file.display()
            ),
        )
        .with_context(format!("bytes: {size}; limit: {MAX_LUA_SOURCE_BYTES}"))
        .with_help("Keep config.lua below 4 MiB and load large data through bounded adapters"));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    input
        .take(MAX_LUA_SOURCE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| file_error("read", file, error))?;
    if bytes.len() > MAX_LUA_SOURCE_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!(
                "configuration source {} exceeds its read limit",
                file.display()
            ),
        )
        .with_context(format!(
            "bytes: at least {}; limit: {MAX_LUA_SOURCE_BYTES}",
            bytes.len()
        ))
        .with_help("Keep config.lua below 4 MiB and load large data through bounded adapters"));
    }
    String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::ScriptRead,
            format!("configuration source {} is not valid UTF-8", file.display()),
        )
        .with_context(error.to_string())
        .with_help("Encode config.lua as UTF-8")
    })
}

fn set(file: &Path, key: &str, value: &str) -> Result<i32, ShellError> {
    let field = ConfigField::parse(key)?;
    let replacement = field.lua_literal(value)?;
    let source = fs::read_to_string(file).map_err(|error| file_error("read", file, error))?;
    let candidate = patch_literals(&source, &[(field, replacement)])?;
    let temporary = temporary_path(file)?;
    let result = install_candidate(file, &temporary, &candidate, &source);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    println!(
        "updated {key} in {} (backup: {})",
        escape_terminal_controls(&file.display().to_string()),
        escape_terminal_controls(&backup_path(file).display().to_string())
    );
    Ok(0)
}

/// Validate and atomically install `candidate` only if `file` still has the
/// source the editor originally loaded. This is optimistic concurrency: a
/// configuration app must never turn an external edit into an invisible loss.
fn install_candidate(
    file: &Path,
    temporary: &Path,
    candidate: &str,
    expected_source: &str,
) -> Result<(), ShellError> {
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temporary)
        .map_err(|error| file_error("create candidate for", file, error))?;
    output
        .write_all(candidate.as_bytes())
        .and_then(|()| output.sync_all())
        .map_err(|error| file_error("write candidate for", file, error))?;

    // Candidate evaluation happens before either the source or its backup changes.
    LuaRuntime::new(LuaPolicy::config())?.load_config_file(temporary)?;

    let current = fs::read_to_string(file).map_err(|error| file_error("re-read", file, error))?;
    if current != expected_source {
        return Err(conflict_error());
    }

    if let Ok(metadata) = fs::metadata(file) {
        fs::set_permissions(temporary, metadata.permissions())
            .map_err(|error| file_error("preserve permissions for", file, error))?;
    }
    let backup = backup_path(file);
    fs::copy(file, &backup).map_err(|error| file_error("back up", file, error))?;
    // Copying the backup can be slow on networked filesystems. Re-check after
    // it so an edit that raced the transaction is reported instead of replaced.
    let current = fs::read_to_string(file).map_err(|error| file_error("re-read", file, error))?;
    if current != expected_source {
        return Err(conflict_error());
    }
    fs::rename(temporary, file).map_err(|error| {
        file_error("atomically replace", file, error).with_help(format!(
            "The original remains available at {}",
            backup.display()
        ))
    })?;
    sync_parent(file)?;
    Ok(())
}

fn conflict_error() -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "configuration changed after this editor loaded it",
    )
    .with_help("Reload the configuration form, review the external changes, and submit again")
}

#[cfg(unix)]
fn sync_parent(file: &Path) -> Result<(), ShellError> {
    let parent = file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| file_error("sync the directory containing", file, error))
}

#[cfg(not(unix))]
fn sync_parent(_file: &Path) -> Result<(), ShellError> {
    // Rust's portable File API cannot open directories on Windows. The
    // replacement already succeeded and the file contents were flushed.
    Ok(())
}

fn backup_path(file: &Path) -> PathBuf {
    let mut value = OsString::from(file.as_os_str());
    value.push(".bak");
    PathBuf::from(value)
}

fn temporary_path(file: &Path) -> Result<PathBuf, ShellError> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                "could not create a configuration transaction",
            )
            .with_context(error.to_string())
            .with_help("Retry after the system clock is available")
        })?
        .as_nanos();
    let name = file.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} has no configuration file name", file.display()),
        )
        .with_help("Pass a path to config.lua")
    })?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".quirl-tmp-{}-{nonce}", std::process::id()));
    Ok(file.with_file_name(temporary_name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigField {
    EditorKeymap,
    EditorSemanticHints,
    PickerLayout,
    PickerPreview,
    PromptLeft,
    PromptRight,
}

impl ConfigField {
    const ALL: [Self; 6] = [
        Self::EditorKeymap,
        Self::EditorSemanticHints,
        Self::PickerLayout,
        Self::PickerPreview,
        Self::PromptLeft,
        Self::PromptRight,
    ];

    const fn key(self) -> &'static str {
        match self {
            Self::EditorKeymap => "editor.keymap",
            Self::EditorSemanticHints => "editor.semantic_hints",
            Self::PickerLayout => "picker.layout",
            Self::PickerPreview => "picker.preview",
            Self::PromptLeft => "prompt.left",
            Self::PromptRight => "prompt.right",
        }
    }

    fn parse(key: &str) -> Result<Self, ShellError> {
        match key {
            "editor.keymap" => Ok(Self::EditorKeymap),
            "editor.semantic_hints" => Ok(Self::EditorSemanticHints),
            "picker.layout" => Ok(Self::PickerLayout),
            "picker.preview" => Ok(Self::PickerPreview),
            "prompt.left" => Ok(Self::PromptLeft),
            "prompt.right" => Ok(Self::PromptRight),
            _ => Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("`{key}` is not an editable literal configuration field"),
            )
            .with_help(format!("Editable fields: {}", Self::KEYS.join(", ")))),
        }
    }

    const KEYS: [&'static str; 6] = [
        "editor.keymap",
        "editor.semantic_hints",
        "picker.layout",
        "picker.preview",
        "prompt.left",
        "prompt.right",
    ];

    const fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::EditorKeymap => ("editor", "keymap"),
            Self::EditorSemanticHints => ("editor", "semantic_hints"),
            Self::PickerLayout => ("picker", "layout"),
            Self::PickerPreview => ("picker", "preview"),
            Self::PromptLeft => ("prompt", "left"),
            Self::PromptRight => ("prompt", "right"),
        }
    }

    fn lua_literal(self, value: &str) -> Result<String, ShellError> {
        let valid = match self {
            Self::EditorKeymap => matches!(value, "helix" | "emacs" | "vim"),
            Self::PickerLayout => matches!(value, "adaptive" | "bottom" | "full"),
            Self::EditorSemanticHints | Self::PickerPreview => matches!(value, "true" | "false"),
            Self::PromptLeft | Self::PromptRight => serde_json::from_str::<Vec<String>>(value)
                .map(|values| values.iter().all(|item| valid_prompt_item(item)))
                .unwrap_or(false),
        };
        if !valid {
            let expected = match self {
                Self::EditorKeymap => "helix, emacs, or vim",
                Self::PickerLayout => "adaptive, bottom, or full",
                Self::EditorSemanticHints | Self::PickerPreview => "true or false",
                Self::PromptLeft | Self::PromptRight => {
                    "a JSON array of non-empty prompt segment names"
                }
            };
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("invalid value `{value}`"),
            )
            .with_help(format!("Expected {expected}")));
        }
        Ok(match self {
            Self::EditorKeymap | Self::PickerLayout => format!("\"{value}\""),
            Self::EditorSemanticHints | Self::PickerPreview => value.to_owned(),
            Self::PromptLeft | Self::PromptRight => {
                let values = serde_json::from_str::<Vec<String>>(value).map_err(|_| {
                    ShellError::new(ErrorCode::InvalidArgument, "invalid prompt segment list")
                        .with_help("Pass a JSON array of non-empty prompt segment names")
                })?;
                lua_string_array(&values)
            }
        })
    }

    fn value(self, config: &QuirlConfig) -> String {
        match self {
            Self::EditorKeymap => config.editor.keymap.clone(),
            Self::EditorSemanticHints => config.editor.semantic_hints.to_string(),
            Self::PickerLayout => config.picker.layout.clone(),
            Self::PickerPreview => config.picker.preview.to_string(),
            Self::PromptLeft => serde_json::to_string(&config.prompt.left).unwrap_or_default(),
            Self::PromptRight => serde_json::to_string(&config.prompt.right).unwrap_or_default(),
        }
    }
}

fn valid_prompt_item(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.bytes().any(|byte| byte.is_ascii_control())
}

fn lua_string_array(values: &[String]) -> String {
    let quoted = values
        .iter()
        .map(|value| serde_json::to_string(value).unwrap_or_default())
        .collect::<Vec<_>>();
    format!("{{ {} }}", quoted.join(", "))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Identifier(String),
    String,
    Symbol(u8),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

fn patch_literals(
    source: &str,
    replacements: &[(ConfigField, String)],
) -> Result<String, ShellError> {
    let mut candidate = source.to_owned();
    for (field, replacement) in replacements {
        candidate = patch_literal(&candidate, *field, replacement)?;
    }
    Ok(candidate)
}

fn patch_literal(
    source: &str,
    field: ConfigField,
    replacement: &str,
) -> Result<String, ShellError> {
    let tokens = tokenize(source);
    let config_open = find_config_table(&tokens)
        .ok_or_else(|| patch_error("could not find a literal `quirl.config { ... }` table"))?;
    let (section, name) = field.parts();
    let section_values = field_values(&tokens, config_open, section);
    let [section_value] = section_values.as_slice() else {
        return Err(patch_error(&format!(
            "expected exactly one literal `{section} = {{ ... }}` section"
        )));
    };
    if !matches!(&tokens[*section_value].kind, TokenKind::Symbol(b'{')) {
        return Err(patch_error(&format!(
            "`{section}` is code-controlled instead of a literal table"
        )));
    }
    let values = field_values(&tokens, *section_value, name);
    let [value] = values.as_slice() else {
        return Err(patch_error(&format!(
            "expected exactly one literal `{section}.{name}` field"
        )));
    };
    let literal_end_index = match field {
        ConfigField::PromptLeft | ConfigField::PromptRight => {
            literal_string_array_end(&tokens, *value)
        }
        _ => Some(*value),
    };
    let expected_literal = match field {
        ConfigField::EditorKeymap | ConfigField::PickerLayout => {
            matches!(&tokens[*value].kind, TokenKind::String)
        }
        ConfigField::EditorSemanticHints | ConfigField::PickerPreview => matches!(
            &tokens[*value].kind,
            TokenKind::Identifier(value) if value == "true" || value == "false"
        ),
        ConfigField::PromptLeft | ConfigField::PromptRight => {
            literal_string_array_end(&tokens, *value).is_some()
        }
    } && literal_end_index
        .is_some_and(|index| literal_value_ends(&tokens, index));
    if !expected_literal {
        return Err(patch_error(&format!(
            "`{section}.{name}` is code-controlled instead of a recognized literal"
        )));
    }
    let token = &tokens[*value];
    let end = literal_end_index
        .map(|index| tokens[index].end)
        .ok_or_else(|| patch_error("literal configuration value is incomplete"))?;
    let mut patched = String::with_capacity(source.len() + replacement.len());
    patched.push_str(&source[..token.start]);
    patched.push_str(replacement);
    patched.push_str(&source[end..]);
    Ok(patched)
}

fn literal_value_ends(tokens: &[Token], end: usize) -> bool {
    matches!(
        tokens.get(end + 1).map(|token| &token.kind),
        None | Some(TokenKind::Symbol(b',' | b';' | b'}'))
    )
}

fn literal_string_array_end(tokens: &[Token], start: usize) -> Option<usize> {
    if !matches!(
        tokens.get(start).map(|token| &token.kind),
        Some(TokenKind::Symbol(b'{'))
    ) {
        return None;
    }
    let mut index = start + 1;
    let mut expects_value = true;
    while let Some(token) = tokens.get(index) {
        match &token.kind {
            TokenKind::Symbol(b'}') if expects_value => return Some(index),
            TokenKind::String if expects_value => expects_value = false,
            TokenKind::Symbol(b',') | TokenKind::Symbol(b';') if !expects_value => {
                expects_value = true;
            }
            TokenKind::Symbol(b'}') if !expects_value => return Some(index),
            _ => return None,
        }
        index += 1;
    }
    None
}

fn find_config_table(tokens: &[Token]) -> Option<usize> {
    tokens
        .windows(4)
        .position(|tokens| {
            identifier_is(&tokens[0], "quirl")
                && matches!(&tokens[1].kind, TokenKind::Symbol(b'.'))
                && identifier_is(&tokens[2], "config")
                && matches!(&tokens[3].kind, TokenKind::Symbol(b'{'))
        })
        .map(|index| index + 3)
}

fn field_values(tokens: &[Token], open: usize, field: &str) -> Vec<usize> {
    let mut values = Vec::new();
    let mut depth = 0usize;
    let mut index = open + 1;
    while index < tokens.len() {
        match &tokens[index].kind {
            TokenKind::Symbol(b'{') | TokenKind::Symbol(b'(') | TokenKind::Symbol(b'[') => {
                depth += 1;
            }
            TokenKind::Symbol(b'}') if depth == 0 => break,
            TokenKind::Symbol(b'}') | TokenKind::Symbol(b')') | TokenKind::Symbol(b']') => {
                depth = depth.saturating_sub(1);
            }
            _ if depth == 0 && identifier_is(&tokens[index], field) => {
                if matches!(
                    tokens.get(index + 1).map(|token| &token.kind),
                    Some(TokenKind::Symbol(b'='))
                ) && tokens.get(index + 2).is_some()
                {
                    values.push(index + 2);
                }
            }
            _ => {}
        }
        index += 1;
    }
    values
}

fn identifier_is(token: &Token, expected: &str) -> bool {
    matches!(&token.kind, TokenKind::Identifier(value) if value == expected)
}

fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"--") {
            if let Some(end) = long_bracket_end(bytes, index + 2) {
                index = end;
            } else {
                index = bytes[index..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |offset| index + offset + 1);
            }
            continue;
        }
        if let Some(end) = long_bracket_end(bytes, index) {
            tokens.push(Token {
                kind: TokenKind::Other,
                start: index,
                end,
            });
            index = end;
            continue;
        }
        let start = index;
        let kind = match bytes[index] {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                TokenKind::Identifier(source[start..index].to_owned())
            }
            quote @ (b'\'' | b'"') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else if bytes[index] == quote {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
                TokenKind::String
            }
            symbol @ (b'.' | b'=' | b'{' | b'}' | b'(' | b')' | b'[' | b']' | b',' | b';') => {
                index += 1;
                TokenKind::Symbol(symbol)
            }
            _ => {
                index += 1;
                TokenKind::Other
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    tokens
}

fn long_bracket_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut equals = 0usize;
    while bytes.get(start + 1 + equals) == Some(&b'=') {
        equals += 1;
    }
    if bytes.get(start + 1 + equals) != Some(&b'[') {
        return None;
    }
    let mut index = start + 2 + equals;
    while index < bytes.len() {
        if bytes[index] == b']'
            && bytes.get(index + 1..index + 1 + equals)
                == Some(&bytes[start + 1..start + 1 + equals])
            && bytes.get(index + 1 + equals) == Some(&b']')
        {
            return Some(index + 2 + equals);
        }
        index += 1;
    }
    Some(bytes.len())
}

fn patch_error(message: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help(
        "Only recognized literal fields inside `quirl.config { ... }` can be patched; edit dynamic values in code",
    )
}

fn file_error(action: &str, file: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not {action} {}", file.display()),
    )
    .with_context(error.to_string())
    .with_help("Check that the configuration path and its parent directory are writable")
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not produce JSON")
        .with_context(error.to_string())
        .with_help("Use the text output format, or report this serialization failure")
}

fn print_json(value: &impl serde::Serialize) -> Result<(), ShellError> {
    let json = serde_json::to_string_pretty(value).map_err(json_error)?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_source() -> &'static str {
        r#"-- Keep this comment and the unrelated plugin setup.
local plugin_value = { preview = false, render = function() return "ok" end }
local config = quirl.config {
  editor = { keymap = "helix", semantic_hints = true }, -- editor note
  picker = { layout = "adaptive", preview = true },
  prompt = { left = { "directory" }, right = {} },
}
return config
"#
    }

    fn test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("quirl-config-test-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn patch_changes_only_the_recognized_literal() {
        let patched =
            patch_literal(example_source(), ConfigField::EditorKeymap, "\"vim\"").unwrap();
        assert!(patched.contains("keymap = \"vim\""));
        assert!(patched.contains("render = function() return \"ok\" end"));
        assert!(patched.contains("-- editor note"));
        assert_eq!(patched.matches("keymap =").count(), 1);
    }

    #[test]
    fn patch_rejects_code_controlled_values() {
        let source = example_source().replace("keymap = \"helix\"", "keymap = choose_keymap() ");
        let error = patch_literal(&source, ConfigField::EditorKeymap, "\"vim\"").unwrap_err();
        assert!(error.message.contains("code-controlled"));
    }

    #[test]
    fn set_validates_then_atomically_installs_and_keeps_backup() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();

        set(&file, "picker.preview", "false").unwrap();

        let installed = fs::read_to_string(&file).unwrap();
        assert!(installed.contains("picker = { layout = \"adaptive\", preview = false }"));
        assert_eq!(
            fs::read_to_string(backup_path(&file)).unwrap(),
            example_source()
        );
        assert!(!load(&file).unwrap().picker.preview);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_complete_candidate_never_replaces_source() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        let invalid = example_source().replace(
            "prompt = { left = { \"directory\" }, right = {} }",
            "prompt = { left = { 42 }, right = {} }",
        );
        fs::write(&file, &invalid).unwrap();

        let error = set(&file, "picker.preview", "false").unwrap_err();

        assert_eq!(fs::read_to_string(&file).unwrap(), invalid);
        assert!(!backup_path(&file).exists());
        assert_eq!(error.code, ErrorCode::Validation);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_preview_inserts_v1_without_mutating_the_source() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();
        let source = read_config_source(&file).unwrap();

        let (version, candidate) = migration_candidate(&source).unwrap();

        assert_eq!(version, None);
        assert!(candidate.contains("schema_version = 1"));
        assert_eq!(read_config_source(&file).unwrap(), source);
        assert!(!backup_path(&file).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_preview_requires_an_explicit_dry_run_flag() {
        let error = migrate(Path::new("config.lua"), false, ConfigOutputFormat::Text).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert!(error.details.help[0].contains("--dry-run"));
    }

    #[test]
    fn diff_is_stable_and_reports_only_changed_schema_fields() {
        let left = load_test_config();
        let mut right = left.clone();
        right.editor.keymap = "vim".to_owned();
        right.picker.preview = false;

        assert_eq!(
            config_differences(&left, &right),
            vec![
                ConfigDifference {
                    key: "editor.keymap",
                    before: "helix".to_owned(),
                    after: "vim".to_owned(),
                },
                ConfigDifference {
                    key: "picker.preview",
                    before: "true".to_owned(),
                    after: "false".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn doctor_distinguishes_literal_and_code_controlled_fields_without_writing() {
        let source = example_source().replace("keymap = \"helix\"", "keymap = \"helix\" .. \"\"");
        let config = load_test_config();
        let (literal, code_controlled) = patchable_fields(&source, &config);

        assert!(!literal.contains(&"editor.keymap"));
        assert!(code_controlled.contains(&"editor.keymap"));
    }

    #[test]
    fn config_formatter_uses_the_validated_atomic_backup_transaction() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        let source = example_source().replace("local config", "local   config");
        let expected = format_source(&source);
        assert_ne!(source, expected);
        fs::write(&file, &source).unwrap();

        assert_eq!(format(&file, true).unwrap(), 1);
        assert_eq!(fs::read_to_string(&file).unwrap(), source);
        assert_eq!(format(&file, false).unwrap(), 0);
        assert_eq!(fs::read_to_string(&file).unwrap(), expected);
        assert_eq!(fs::read_to_string(backup_path(&file)).unwrap(), source);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_json_commands_request_json_error_rendering() {
        assert!(wants_json(&ConfigCommand::Export {
            file: PathBuf::from("config.lua"),
            format: ConfigOutputFormat::Json,
        }));
        assert!(wants_json(&ConfigCommand::Doctor {
            file: PathBuf::from("config.lua"),
            format: ConfigOutputFormat::Json,
        }));
    }

    #[test]
    fn portable_config_surfaces_reject_oversized_source_before_evaluation() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, vec![b'x'; MAX_LUA_SOURCE_BYTES + 1]).unwrap();

        let error = read_config_source(&file).unwrap_err();

        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.help[0].contains("4 MiB"));
        fs::remove_dir_all(directory).unwrap();
    }

    fn web_form(token: &str, source: &str, keymap: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("csrf".to_owned(), token.to_owned()),
            ("revision".to_owned(), source_revision(source)),
            ("editor_keymap".to_owned(), keymap.to_owned()),
            ("editor_semantic_hints".to_owned(), "true".to_owned()),
            ("picker_layout".to_owned(), "adaptive".to_owned()),
            ("picker_preview".to_owned(), "true".to_owned()),
            ("prompt_left".to_owned(), "directory".to_owned()),
            ("prompt_right".to_owned(), String::new()),
        ])
    }

    #[test]
    fn web_form_renders_the_complete_schema_without_a_second_store() {
        let config = load_test_config();
        let session = WebSession {
            token: "private".to_owned(),
            source: example_source().to_owned(),
            config,
        };
        let page = render_form(&session, None);
        assert!(page.contains("Schema"));
        assert!(page.contains("editor_keymap"));
        assert!(page.contains("picker_layout"));
        assert!(page.contains("prompt_left"));
        assert!(!page.contains("Cache-Control"));
        assert!(!page.contains("<script"));
    }

    #[test]
    fn web_form_merges_non_overlapping_external_source_edits() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();
        let original = fs::read_to_string(&file).unwrap();
        let mut session = WebSession {
            token: "private".to_owned(),
            source: original.clone(),
            config: load(&file).unwrap(),
        };
        let external = original.replace("-- editor note", "-- external note");
        fs::write(&file, &external).unwrap();
        assert_eq!(load(&file).unwrap(), session.config);

        apply_web_form(&file, &mut session, &web_form("private", &original, "vim")).unwrap();

        let installed = fs::read_to_string(&file).unwrap();
        assert!(installed.contains("keymap = \"vim\""));
        assert!(installed.contains("-- external note"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn web_form_rejects_same_field_concurrent_edits() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();
        let original = fs::read_to_string(&file).unwrap();
        let mut session = WebSession {
            token: "private".to_owned(),
            source: original.clone(),
            config: load(&file).unwrap(),
        };
        let external = original.replace("keymap = \"helix\"", "keymap = \"emacs\"");
        fs::write(&file, &external).unwrap();

        let error = apply_web_form(&file, &mut session, &web_form("private", &original, "vim"))
            .unwrap_err();

        assert!(error
            .message
            .contains("changed concurrently: editor.keymap"));
        assert_eq!(fs::read_to_string(&file).unwrap(), external);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn web_handler_rejects_non_loopback_hosts_origins_and_tokens() {
        let mut session = WebSession {
            token: "private".to_owned(),
            source: example_source().to_owned(),
            config: load_test_config(),
        };
        let request = HttpRequest {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            query: "token=private".to_owned(),
            headers: BTreeMap::from([("host".to_owned(), "attacker.invalid".to_owned())]),
            body: String::new(),
        };
        assert_eq!(
            handle_web_request(Path::new("missing"), &mut session, request).status,
            403
        );

        let request = HttpRequest {
            method: "POST".to_owned(),
            path: "/".to_owned(),
            query: String::new(),
            headers: BTreeMap::from([
                ("host".to_owned(), "127.0.0.1:1234".to_owned()),
                ("origin".to_owned(), "https://attacker.invalid".to_owned()),
                (
                    "content-type".to_owned(),
                    "application/x-www-form-urlencoded".to_owned(),
                ),
            ]),
            body: "csrf=private".to_owned(),
        };
        assert_eq!(
            handle_web_request(Path::new("missing"), &mut session, request).status,
            403
        );
    }

    #[test]
    fn hostile_form_input_and_limits_are_rejected() {
        assert!(parse_form("field=%zz").is_err());
        assert!(parse_form("field=%00").is_err());
        assert!(parse_form("field=a&field=b").is_err());
        assert!(parse_form(&format!("field={}", "a".repeat(4097))).is_err());
        assert!(parse_prompt_lines(&"a".repeat(4097)).is_err());
        assert!(!loopback_origin("http://attacker.invalid"));
        assert!(!loopback_origin("https://127.0.0.1:1234"));
        assert!(request_size_is_allowed(128, WEB_MAX_REQUEST_BYTES - 128));
        assert!(!request_size_is_allowed(128, WEB_MAX_REQUEST_BYTES));
        assert!(!request_size_is_allowed(usize::MAX, 1));
    }

    fn load_test_config() -> QuirlConfig {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();
        let config = load(&file).unwrap();
        fs::remove_dir_all(directory).unwrap();
        config
    }
}
