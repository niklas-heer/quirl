use crate::lua_worker::LuaWorkerRuntime as LuaRuntime;
use clap::{Subcommand, ValueEnum};
use quirl_core::{escape_json_terminal_controls, escape_terminal_controls, ErrorCode, ShellError};
use quirl_lua::{
    builtin_theme, builtin_theme_names, format_source, LuaPolicy, QuirlConfig, ThemeColors,
    CONFIG_SCHEMA_VERSION, MAX_LUA_SOURCE_BYTES, MAX_THEME_NAME_BYTES,
};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Parse, evaluate under config restrictions, and validate against Rust schemas.
    Check {
        /// Lua configuration file to validate.
        file: PathBuf,
        /// Output representation for the validation result.
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
    /// Open a loopback-only, schema-backed configuration form.
    Web {
        /// Lua configuration file edited by the local form.
        file: PathBuf,
        /// Loopback TCP port; 0 selects an available port.
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Print one evaluated, schema-backed configuration value.
    Get {
        /// Lua configuration file to evaluate.
        file: PathBuf,
        /// Recognized field such as editor.keymap or prompt.symbols.
        key: String,
    },
    /// Patch one recognized literal, validate the candidate, and retain a .bak.
    Set {
        /// Lua configuration file to update atomically.
        file: PathBuf,
        /// Recognized literal field; use prompt.symbols for prompt glyphs.
        key: String,
        /// Typed value; prompt symbols accept auto, plain, unicode, or nerd_font.
        value: String,
    },
    /// Show the current schema and values as an accessible line-oriented view.
    Tui {
        /// Lua configuration file to inspect.
        file: PathBuf,
    },
    /// Format a configuration file with Quirl's deterministic Lua formatter.
    Fmt {
        /// Lua configuration file to format.
        file: PathBuf,
        /// Report formatting drift without writing the source file.
        #[arg(long)]
        check: bool,
    },
    /// Export the evaluated, schema-backed configuration without modifying its source.
    Export {
        /// Lua configuration file to evaluate and export.
        file: PathBuf,
        /// Output representation for the evaluated configuration.
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Json)]
        format: ConfigOutputFormat,
    },
    /// Compare two evaluated configuration files field by field.
    Diff {
        /// Baseline Lua configuration file.
        file: PathBuf,
        /// Candidate Lua configuration file compared with the baseline.
        other: PathBuf,
        /// Output representation for the field-level differences.
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
    /// Preview the source migration required by the current configuration schema.
    Migrate {
        /// Lua configuration file to analyze for migration.
        file: PathBuf,
        /// Migration is preview-only in 0.1.0; this flag is required to make that explicit.
        #[arg(long)]
        dry_run: bool,
        /// Output representation for the migration preview.
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
    /// Diagnose schema validity and which settings can be safely patched as literals.
    Doctor {
        /// Lua configuration file to diagnose.
        file: PathBuf,
        /// Output representation for the diagnostic report.
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
        "editor.keymap = {}  (emacs | vim | helix [experimental])",
        escape_terminal_controls(&config.editor.keymap)
    );
    println!(
        "editor.semantic_hints = {}  (true | false)",
        config.editor.semantic_hints
    );
    println!(
        "editor.banner = {}  (full | compact | none)",
        escape_terminal_controls(&config.editor.banner)
    );
    println!("\n[picker]");
    println!(
        "picker.layout = {}  (adaptive | bottom | full)",
        escape_terminal_controls(&config.picker.layout)
    );
    println!("picker.preview = {}  (true | false)", config.picker.preview);
    println!("\n[prompt]");
    println!(
        "prompt.symbols = {}  (auto | plain | unicode | nerd_font)",
        escape_terminal_controls(&config.prompt.symbols)
    );
    println!(
        "prompt.left = {}",
        escape_terminal_controls(&ConfigField::PromptLeft.value(&config))
    );
    println!(
        "prompt.right = {}",
        escape_terminal_controls(&ConfigField::PromptRight.value(&config))
    );
    println!(
        "prompt.transient = {}  (true | false)",
        config.prompt.transient
    );
    println!("\n[ui]");
    println!(
        "ui.theme = {}  (built-ins: {})",
        escape_terminal_controls(&config.ui.theme),
        builtin_theme_names().collect::<Vec<_>>().join(" | ")
    );
    println!(
        "ui.themes = {}",
        escape_terminal_controls(&ConfigField::UiThemes.value(&config))
    );
    println!(
        "ui.surface = {}  (auto | rich | simple)",
        escape_terminal_controls(&config.ui.surface)
    );
    println!(
        "ui.statusline.hints = {}  (true | false)",
        config.ui.statusline.hints
    );
    println!("\n[completion]");
    println!(
        "completion.auto = {}  (true | false)",
        config.completion.auto
    );
    println!(
        "completion.min_chars = {}  (0..4096)",
        config.completion.min_chars
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
        result.map_err(CandidateInstallFailure::into_shell_error)?;
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
    let (source_schema_version, candidate) = migration_candidate(&source)?;
    // Inspect the declared version before evaluating it so a configuration from
    // a newer Quirl never gets described as an already-current schema.
    load(file)?;
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
        candidate.push_str(&format!("\n  schema_version = {CONFIG_SCHEMA_VERSION},"));
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
    if version > CONFIG_SCHEMA_VERSION {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "configuration schema_version {version} is newer than this Quirl supports"
            ),
        )
        .with_help(
            "Use a Quirl version that supports this configuration before requesting a migration preview",
        ));
    }
    if version == CONFIG_SCHEMA_VERSION {
        return Ok((Some(version), source.to_owned()));
    }
    let mut candidate = String::with_capacity(source.len());
    candidate.push_str(&source[..token.start]);
    candidate.push_str(&CONFIG_SCHEMA_VERSION.to_string());
    candidate.push_str(&source[token.start + digits..]);
    Ok((Some(version), candidate))
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

/// Classifies the two expected failures from an interactive save without
/// deriving an HTTP status from user-facing diagnostic text.
#[derive(Debug)]
enum WebFormFailure {
    Conflict(ShellError),
    Invalid(ShellError),
}

impl From<ShellError> for WebFormFailure {
    fn from(error: ShellError) -> Self {
        Self::Invalid(error)
    }
}

impl WebFormFailure {
    fn error(&self) -> &ShellError {
        match self {
            Self::Conflict(error) | Self::Invalid(error) => error,
        }
    }

    fn status(&self) -> u16 {
        match self {
            Self::Conflict(_) => 409,
            Self::Invalid(_) => 422,
        }
    }
}

#[derive(Debug)]
enum CandidateInstallFailure {
    Conflict(ShellError),
    Other(ShellError),
}

impl From<ShellError> for CandidateInstallFailure {
    fn from(error: ShellError) -> Self {
        Self::Other(error)
    }
}

impl CandidateInstallFailure {
    fn into_shell_error(self) -> ShellError {
        match self {
            Self::Conflict(error) | Self::Other(error) => error,
        }
    }
}

/// Starts a deliberately small local-only server. It has no assets, cookies,
/// proxy support, or ambient network binding: the capability is the unguessable
/// URL token printed to the invoking terminal.
fn web(file: &Path, port: u16) -> Result<i32, ShellError> {
    let source = read_config_source(file)?;
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
                match write_http_response(&mut stream, &response) {
                    Ok(()) => deadline = Instant::now() + WEB_IDLE_TIMEOUT,
                    // Browsers are free to abandon a navigation while the form
                    // response is being produced. That peer-local failure must
                    // not terminate the loopback server for every other tab.
                    Err(error) if client_disconnected(&error) => continue,
                    Err(error) => return Err(web_response_error(error)),
                }
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
                .is_some_and(|value| is_urlencoded_form_content_type(value))
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
                Err(error) => HttpResponse::page(
                    error.status(),
                    render_form(session, Some(&error.error().message)),
                ),
            }
        }
        _ => HttpResponse::error(405, "Only GET and POST are supported"),
    }
}

fn loopback_authority(authority: &str) -> bool {
    authority == "127.0.0.1"
        || authority
            .strip_prefix("127.0.0.1:")
            .is_some_and(|port| port.parse::<u16>().is_ok_and(|port| port != 0))
}

fn loopback_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(loopback_authority)
}

fn is_urlencoded_form_content_type(value: &str) -> bool {
    value
        .split_once(';')
        .map_or(value, |(media_type, _)| media_type)
        .trim()
        .eq_ignore_ascii_case("application/x-www-form-urlencoded")
}

fn refresh_session(file: &Path, session: &mut WebSession) -> Result<(), ShellError> {
    let source = read_config_source(file)?;
    let config = load(file)?;
    session.source = source;
    session.config = config;
    Ok(())
}

fn apply_web_form(
    file: &Path,
    session: &mut WebSession,
    form: &BTreeMap<String, String>,
) -> Result<String, WebFormFailure> {
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
        ConfigField::EditorBanner,
        required_form_value(form, "editor_banner")?,
        &session.config.editor.banner,
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
    collect_web_change(
        &mut replacements,
        ConfigField::PromptSymbols,
        required_form_value(form, "prompt_symbols")?,
        &session.config.prompt.symbols,
    )?;
    let prompt_left = parse_prompt_lines(required_form_value(form, "prompt_left")?)?;
    let prompt_right = parse_prompt_lines(required_form_value(form, "prompt_right")?)?;
    if prompt_left != session.config.prompt.left {
        replacements.push((ConfigField::PromptLeft, lua_string_array(&prompt_left)));
    }
    if prompt_right != session.config.prompt.right {
        replacements.push((ConfigField::PromptRight, lua_string_array(&prompt_right)));
    }
    collect_web_change(
        &mut replacements,
        ConfigField::PromptTransient,
        required_form_value(form, "prompt_transient")?,
        &session.config.prompt.transient.to_string(),
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::UiTheme,
        required_form_value(form, "ui_theme")?,
        &session.config.ui.theme,
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::UiSurface,
        required_form_value(form, "ui_surface")?,
        &session.config.ui.surface,
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::UiStatuslineHints,
        required_form_value(form, "ui_statusline_hints")?,
        &session.config.ui.statusline.hints.to_string(),
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::CompletionAuto,
        required_form_value(form, "completion_auto")?,
        &session.config.completion.auto.to_string(),
    )?;
    collect_web_change(
        &mut replacements,
        ConfigField::CompletionMinChars,
        required_form_value(form, "completion_min_chars")?,
        &session.config.completion.min_chars.to_string(),
    )?;
    if replacements.is_empty() {
        return Ok("No changes to save.".to_owned());
    }
    // Three-way merge: compare the form's base value with the current evaluated
    // configuration for every field the user changed. Non-overlapping source
    // edits are preserved; edits to the same field become a visible conflict.
    let current_source = read_config_source(file)?;
    let current_config = if current_source == session.source {
        session.config.clone()
    } else {
        load(file)?
    };
    for (field, _) in &replacements {
        if field.value(&current_config) != field.value(&session.config) {
            return Err(WebFormFailure::Conflict(
                ShellError::new(
                    ErrorCode::Validation,
                    format!("configuration field changed concurrently: {}", field.key()),
                )
                .with_help(
                    "Reload the configuration form, review the external value, and submit again",
                ),
            ));
        }
    }
    let candidate = patch_literals(&current_source, &replacements)?;
    let temporary = temporary_path(file)?;
    let result = install_candidate(file, &temporary, &candidate, &current_source);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    match result {
        Ok(()) => {}
        Err(CandidateInstallFailure::Conflict(error)) => {
            return Err(WebFormFailure::Conflict(error));
        }
        Err(CandidateInstallFailure::Other(error)) => return Err(WebFormFailure::Invalid(error)),
    }
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

fn render_theme_card(name: &str, colors: &ThemeColors, selected_theme: &str) -> String {
    let checked = if name == selected_theme {
        " checked"
    } else {
        ""
    };
    format!(
        "<label class=\"theme-card\" style=\"--bg:{background};--command:{command};--data:{data};--context:{context};--secondary:{secondary};--string:{string};--dim:{dim};--error:{error};--warning:{warning}\"><input type=\"radio\" name=\"ui_theme\" value=\"{id}\"{checked}><span class=\"theme-name\">{display_name}</span><span class=\"terminal\"><span class=\"term-context\">~/src/quirl</span> <span class=\"term-secondary\">on main dirty</span><br><span class=\"term-command\">$ git</span> <span class=\"term-context\">status</span> <span class=\"term-string\">--short</span><br><span class=\"term-dim\">Alt-M mode | Ctrl-R history</span> <span class=\"term-error\">status:1</span></span></label>",
        background = html_escape(&colors.status_background),
        command = html_escape(&colors.accent_command),
        data = html_escape(&colors.accent_data),
        context = html_escape(&colors.context_primary),
        secondary = html_escape(&colors.context_secondary),
        string = html_escape(&colors.string),
        dim = html_escape(&colors.muted),
        error = html_escape(&colors.error),
        warning = html_escape(&colors.warning),
        id = html_escape(name),
        display_name = html_escape(name),
    )
}

fn render_theme_cards(config: &QuirlConfig) -> String {
    let builtins = builtin_theme_names()
        .filter_map(|name| builtin_theme(name).map(|colors| (name.to_owned(), colors)));
    let custom = config
        .ui
        .themes
        .iter()
        .map(|(name, colors)| (name.clone(), colors.clone()));
    let cards = builtins
        .chain(custom)
        .map(|(name, colors)| render_theme_card(&name, &colors, &config.ui.theme))
        .collect::<String>();
    format!(
        "<fieldset><legend>Theme</legend><p>Preview the 31 bounded built-in palettes and any custom palettes already defined in <code>config.lua</code>. <code>NO_COLOR</code> remains authoritative.</p><div class=\"theme-grid\">{cards}</div></fieldset>"
    )
}

fn render_form(session: &WebSession, notice: Option<&str>) -> String {
    let config = &session.config;
    let selected = |value: &str, option: &str| if value == option { " selected" } else { "" };
    let theme_cards = render_theme_cards(config);
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
<style>:root{{color-scheme:dark}}*{{box-sizing:border-box}}body{{font:1rem system-ui,sans-serif;max-width:72rem;margin:0 auto;padding:2rem 1rem 5rem;background:#101014;color:#ededf2}}main>p{{color:#a9a9b3}}code{{color:#b9a8ff}}fieldset{{margin:1.25rem 0;padding:1.25rem;border:1px solid #383842;border-radius:.8rem;background:#18181e}}legend{{font-weight:700;padding:0 .4rem}}label{{display:block;margin:.75rem 0}}select,input,textarea,button{{font:inherit}}select,input,textarea{{max-width:100%;width:26rem;padding:.45rem;border:1px solid #555563;border-radius:.4rem;background:#101014;color:#ededf2}}textarea{{height:7rem}}button{{padding:.7rem 1.2rem;border:0;border-radius:.45rem;background:#8f7cf7;color:#fff;font-weight:700;cursor:pointer}}[role=status]{{padding:.75rem;border-radius:.5rem;background:#29304a}}.theme-grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(17rem,1fr));gap:.9rem}}.theme-card{{position:relative;margin:0;padding:.85rem;border:2px solid #3c3c45;border-radius:.7rem;cursor:pointer}}.theme-card:has(input:checked){{border-color:#b9a8ff;box-shadow:0 0 0 2px #6d58db}}.theme-card input{{position:absolute;width:1rem;right:.7rem;top:.3rem}}.theme-name{{display:block;margin:0 1.5rem .65rem 0;font-weight:700}}.terminal{{display:block;padding:.8rem;border-radius:.45rem;background:var(--bg);color:#ededf2;font:500 .82rem/1.6 ui-monospace,SFMono-Regular,Consolas,monospace;overflow:hidden}}.terminal span{{white-space:nowrap}}.term-context{{color:var(--context)}}.term-secondary{{color:var(--secondary)}}.term-command{{color:var(--command);font-weight:700}}.term-data{{color:var(--data)}}.term-string{{color:var(--string)}}.term-dim{{color:var(--dim)}}.term-error{{color:var(--error)}}.term-warning{{color:var(--warning)}}@media(max-width:36rem){{body{{padding-top:1rem}}fieldset{{padding:.9rem}}}}</style></head>
<body><main><h1>Make Quirl yours</h1><p><code>config.lua</code> is the source of truth. Preview a palette, then save through the bounded transaction that validates Lua, retains a <code>.bak</code>, and refuses concurrent edits.</p>{notice}
<form method=\"post\" action=\"/\"><input type=\"hidden\" name=\"csrf\" value=\"{token}\"><input type=\"hidden\" name=\"revision\" value=\"{revision}\">{theme_cards}
<fieldset><legend>Schema</legend><p>Version <output>{schema}</output> (managed by Quirl; not edited by this form).</p></fieldset>
<fieldset><legend>Editor</legend><label for=\"editor-keymap\">Keymap <select id=\"editor-keymap\" name=\"editor_keymap\"><option value=\"emacs\"{key_emacs}>emacs — complete default</option><option value=\"vim\"{key_vim}>vim</option><option value=\"helix\"{key_helix}>helix — experimental</option></select></label><label for=\"editor-semantic-hints\">Semantic hints <select id=\"editor-semantic-hints\" name=\"editor_semantic_hints\"><option value=\"true\"{semantic_true}>true</option><option value=\"false\"{semantic_false}>false</option></select></label><label for=\"editor-banner\">Welcome <select id=\"editor-banner\" name=\"editor_banner\"><option value=\"full\"{banner_full}>full</option><option value=\"compact\"{banner_compact}>compact</option><option value=\"none\"{banner_none}>none</option></select></label></fieldset>
<fieldset><legend>Picker</legend><label for=\"picker-layout\">Layout <select id=\"picker-layout\" name=\"picker_layout\"><option value=\"adaptive\"{layout_adaptive}>adaptive</option><option value=\"bottom\"{layout_bottom}>bottom</option><option value=\"full\"{layout_full}>full</option></select></label><label for=\"picker-preview\">Preview <select id=\"picker-preview\" name=\"picker_preview\"><option value=\"true\"{preview_true}>true</option><option value=\"false\"{preview_false}>false</option></select></label></fieldset>
<fieldset><legend>Prompt</legend><label for=\"prompt-symbols\">Symbols <select id=\"prompt-symbols\" name=\"prompt_symbols\"><option value=\"auto\"{symbols_auto}>auto — safe Unicode</option><option value=\"plain\"{symbols_plain}>plain — ASCII only</option><option value=\"unicode\"{symbols_unicode}>unicode</option><option value=\"nerd_font\"{symbols_nerd_font}>Nerd Font / Powerline</option></select></label><p><strong>Nerd Font</strong> is an explicit opt-in and requires a patched terminal font. Auto never assumes one. Segment lists use one name per line.</p><label for=\"prompt-left\">Left <textarea id=\"prompt-left\" name=\"prompt_left\">{prompt_left}</textarea></label><label for=\"prompt-right\">Right <textarea id=\"prompt-right\" name=\"prompt_right\">{prompt_right}</textarea></label><label for=\"prompt-transient\">Transient prompt <select id=\"prompt-transient\" name=\"prompt_transient\"><option value=\"true\"{transient_true}>true</option><option value=\"false\"{transient_false}>false</option></select></label></fieldset>
<fieldset><legend>Interactive surface</legend><label for=\"ui-surface\">Surface <select id=\"ui-surface\" name=\"ui_surface\"><option value=\"auto\"{surface_auto}>auto</option><option value=\"rich\"{surface_rich}>rich</option><option value=\"simple\"{surface_simple}>simple</option></select></label><label for=\"ui-statusline-hints\">Status-line hints <select id=\"ui-statusline-hints\" name=\"ui_statusline_hints\"><option value=\"true\"{statusline_true}>true</option><option value=\"false\"{statusline_false}>false</option></select></label></fieldset>
<fieldset><legend>Completion</legend><label for=\"completion-auto\">Open automatically <select id=\"completion-auto\" name=\"completion_auto\"><option value=\"true\"{completion_auto_true}>true</option><option value=\"false\"{completion_auto_false}>false</option></select></label><label for=\"completion-min-chars\">Minimum characters <input id=\"completion-min-chars\" name=\"completion_min_chars\" type=\"number\" min=\"0\" max=\"4096\" value=\"{completion_min_chars}\"></label></fieldset>
<button type=\"submit\">Save configuration</button></form></main></body></html>",
        token = html_escape(&session.token),
        revision = html_escape(&source_revision(&session.source)),
        theme_cards = theme_cards,
        schema = config.schema_version,
        key_helix = selected(&config.editor.keymap, "helix"),
        key_emacs = selected(&config.editor.keymap, "emacs"),
        key_vim = selected(&config.editor.keymap, "vim"),
        semantic_true = selected(&config.editor.semantic_hints.to_string(), "true"),
        semantic_false = selected(&config.editor.semantic_hints.to_string(), "false"),
        banner_full = selected(&config.editor.banner, "full"),
        banner_compact = selected(&config.editor.banner, "compact"),
        banner_none = selected(&config.editor.banner, "none"),
        layout_adaptive = selected(&config.picker.layout, "adaptive"),
        layout_bottom = selected(&config.picker.layout, "bottom"),
        layout_full = selected(&config.picker.layout, "full"),
        preview_true = selected(&config.picker.preview.to_string(), "true"),
        preview_false = selected(&config.picker.preview.to_string(), "false"),
        symbols_auto = selected(&config.prompt.symbols, "auto"),
        symbols_plain = selected(&config.prompt.symbols, "plain"),
        symbols_unicode = selected(&config.prompt.symbols, "unicode"),
        symbols_nerd_font = selected(&config.prompt.symbols, "nerd_font"),
        prompt_left = html_escape(&config.prompt.left.join("\n")),
        prompt_right = html_escape(&config.prompt.right.join("\n")),
        transient_true = selected(&config.prompt.transient.to_string(), "true"),
        transient_false = selected(&config.prompt.transient.to_string(), "false"),
        surface_auto = selected(&config.ui.surface, "auto"),
        surface_rich = selected(&config.ui.surface, "rich"),
        surface_simple = selected(&config.ui.surface, "simple"),
        statusline_true = selected(&config.ui.statusline.hints.to_string(), "true"),
        statusline_false = selected(&config.ui.statusline.hints.to_string(), "false"),
        completion_auto_true = selected(&config.completion.auto.to_string(), "true"),
        completion_auto_false = selected(&config.completion.auto.to_string(), "false"),
        completion_min_chars = config.completion.min_chars,
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
    let header_end = loop {
        if bytes.len() == WEB_MAX_REQUEST_BYTES {
            return Err(request_error("request is too large"));
        }
        let count = read_http_bytes(stream, &mut bytes, WEB_MAX_REQUEST_BYTES)
            .map_err(|error| request_error(&format!("could not read request: {error}")))?;
        if count == 0 {
            return Err(request_error("request ended before its headers"));
        }
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            if end > WEB_MAX_HEADER_BYTES {
                return Err(request_error("request headers are too large"));
            }
            break end + 4;
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
    let body_end = header_end + content_length;
    if bytes.len() > body_end {
        return Err(request_error(
            "request contains bytes past its declared body",
        ));
    }
    while bytes.len() < body_end {
        let count = read_http_bytes(stream, &mut bytes, body_end)
            .map_err(|error| request_error(&format!("could not read request body: {error}")))?;
        if count == 0 {
            return Err(request_error("request ended before its body"));
        }
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

/// Append at most `limit` total bytes. Passing a shortened mutable slice to
/// `read` matters: TCP may have more bytes queued than the declared body.
fn read_http_bytes(stream: &mut TcpStream, bytes: &mut Vec<u8>, limit: usize) -> io::Result<usize> {
    let remaining = limit.saturating_sub(bytes.len());
    let mut buffer = [0_u8; 2048];
    let buffer_limit = remaining.min(buffer.len());
    let count = stream.read(&mut buffer[..buffer_limit])?;
    bytes.extend_from_slice(&buffer[..count]);
    Ok(count)
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

fn write_http_response(stream: &mut TcpStream, response: &HttpResponse) -> io::Result<()> {
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
}

fn client_disconnected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    )
}

fn web_response_error(error: io::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not respond to a local web request")
        .with_context(error.to_string())
        .with_help("Reload the private loopback URL and submit the form again")
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
    let source = read_config_source(file)?;
    let candidate = patch_literals(&source, &[(field, replacement)])?;
    let temporary = temporary_path(file)?;
    let result = install_candidate(file, &temporary, &candidate, &source);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(CandidateInstallFailure::into_shell_error)?;
    println!(
        "updated {key} in {} (backup: {})",
        escape_terminal_controls(&file.display().to_string()),
        escape_terminal_controls(&backup_path(file).display().to_string())
    );
    Ok(0)
}

/// Validate and install `candidate` with no-replace semantics only if `file`
/// still has the source the editor originally loaded. This is optimistic
/// concurrency: a configuration app must never turn an external edit into an
/// invisible loss. The prior entry is made durable before the candidate link,
/// so a crash or concurrent save always leaves an explicit recovery copy.
fn install_candidate(
    file: &Path,
    temporary: &Path,
    candidate: &str,
    expected_source: &str,
) -> Result<(), CandidateInstallFailure> {
    install_candidate_with_hook(file, temporary, candidate, expected_source, || {})
}

fn install_candidate_with_hook(
    file: &Path,
    temporary: &Path,
    candidate: &str,
    expected_source: &str,
    before_candidate_link: impl FnOnce(),
) -> Result<(), CandidateInstallFailure> {
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temporary)
        .map_err(|error| {
            CandidateInstallFailure::from(file_error("create candidate for", file, error))
        })?;
    output
        .write_all(candidate.as_bytes())
        .and_then(|()| output.sync_all())
        .map_err(|error| {
            CandidateInstallFailure::from(file_error("write candidate for", file, error))
        })?;

    // Candidate evaluation happens before either the source or its backup changes.
    LuaRuntime::new(LuaPolicy::config())
        .map_err(CandidateInstallFailure::from)?
        .load_config_file(temporary)
        .map_err(CandidateInstallFailure::from)?;
    drop(output);

    let current = read_config_source(file).map_err(CandidateInstallFailure::from)?;
    if current != expected_source {
        return Err(CandidateInstallFailure::Conflict(conflict_error()));
    }

    // A hard link gives this transaction the portable no-replace primitive
    // that `rename` does not: creating the destination fails if an external
    // editor has recreated it. Prove the filesystem supports that primitive
    // before moving the user's source entry.
    let link_probe = transaction_path(temporary, "link-probe");
    fs::hard_link(temporary, &link_probe).map_err(|error| {
        CandidateInstallFailure::from(
            file_error(
                "prepare a no-replace configuration install for",
                file,
                error,
            )
            .with_help(
                "Move config.lua to a filesystem that supports hard links, or edit it directly",
            ),
        )
    })?;
    fs::remove_file(&link_probe).map_err(|error| {
        CandidateInstallFailure::from(file_error(
            "clean up the no-replace configuration probe for",
            file,
            error,
        ))
    })?;

    // Capture the directory entry before comparing it. From this point on an
    // atomic external save can only recreate `file`; the no-replace link below
    // observes that race instead of overwriting it. In-place writers retain the
    // captured inode, which becomes the backup on success.
    let retained = transaction_path(temporary, "original");
    match fs::symlink_metadata(&retained) {
        Ok(_) => {
            return Err(CandidateInstallFailure::from(
                ShellError::new(
                    ErrorCode::Io,
                    "could not reserve a configuration recovery path",
                )
                .with_context(format!("path already exists: {}", retained.display()))
                .with_help("Retry the configuration update"),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(CandidateInstallFailure::from(file_error(
                "inspect the configuration recovery path for",
                file,
                error,
            )));
        }
    }
    fs::rename(file, &retained).map_err(|error| {
        CandidateInstallFailure::from(file_error("capture the current source for", file, error))
    })?;
    if let Err(error) = sync_parent(&retained) {
        return Err(CandidateInstallFailure::Other(recover_missing_source(
            file, &retained, error,
        )));
    }

    let captured = match read_config_source(&retained) {
        Ok(captured) => captured,
        Err(error) => {
            return Err(recover_missing_source(file, &retained, error).into());
        }
    };
    if captured != expected_source {
        return Err(CandidateInstallFailure::Conflict(recover_missing_source(
            file,
            &retained,
            conflict_error(),
        )));
    }
    let source_permissions = match fs::metadata(&retained) {
        Ok(metadata) => metadata.permissions(),
        Err(error) => {
            let error = file_error("inspect permissions for", file, error);
            return Err(CandidateInstallFailure::Other(recover_missing_source(
                file, &retained, error,
            )));
        }
    };
    if let Err(error) = fs::set_permissions(temporary, source_permissions) {
        let error = file_error("preserve permissions for", file, error);
        return Err(CandidateInstallFailure::Other(recover_missing_source(
            file, &retained, error,
        )));
    }

    let backup = backup_path(file);
    let retained_is_symlink = match fs::symlink_metadata(&retained) {
        Ok(metadata) => metadata.file_type().is_symlink(),
        Err(error) => {
            let error = file_error("inspect captured source for", file, error);
            return Err(CandidateInstallFailure::Other(recover_missing_source(
                file, &retained, error,
            )));
        }
    };
    let recovery = if retained_is_symlink {
        // Backups are always regular validated text, never a link to an
        // unrelated target. Keep the captured link until candidate install
        // succeeds so a failure can still be recovered explicitly.
        if let Err(error) = install_backup(&retained, &backup, expected_source) {
            return Err(CandidateInstallFailure::Other(recover_missing_source(
                file,
                &retained,
                error.into_shell_error(),
            )));
        }
        retained.clone()
    } else {
        if let Err(error) = replace_backup_candidate(&retained, &backup) {
            return Err(CandidateInstallFailure::Other(recover_missing_source(
                file, &retained, error,
            )));
        }
        if let Err(error) = sync_parent(&backup) {
            return Err(CandidateInstallFailure::Other(recover_missing_source(
                file, &backup, error,
            )));
        }
        backup.clone()
    };

    before_candidate_link();

    if let Err(error) = fs::hard_link(temporary, file) {
        let source_was_recreated = error.kind() == io::ErrorKind::AlreadyExists;
        let shell_error = if source_was_recreated {
            conflict_error()
        } else {
            file_error("install the validated candidate for", file, error)
        };
        return Err(if source_was_recreated {
            CandidateInstallFailure::Conflict(recover_missing_source(file, &recovery, shell_error))
        } else {
            CandidateInstallFailure::Other(recover_missing_source(file, &recovery, shell_error))
        });
    }

    if retained_is_symlink {
        fs::remove_file(&retained).map_err(|error| {
            CandidateInstallFailure::from(
                file_error("clean up the captured configuration link for", file, error).with_help(
                    format!("The captured link remains at {}", retained.display()),
                ),
            )
        })?;
    }
    sync_parent(file).map_err(|error| {
        CandidateInstallFailure::from(error.with_help(format!(
            "The candidate may be installed; the original remains available at {}",
            backup.display()
        )))
    })?;
    fs::remove_file(temporary).map_err(|error| {
        CandidateInstallFailure::from(
            file_error("clean up the installed candidate for", file, error).with_help(format!(
                "The candidate is installed; remove the extra link at {}",
                temporary.display()
            )),
        )
    })?;
    sync_parent(file).map_err(CandidateInstallFailure::from)?;
    Ok(())
}

fn recover_missing_source(file: &Path, recovery: &Path, error: ShellError) -> ShellError {
    if fs::symlink_metadata(file).is_ok() {
        return error.with_context(format!(
            "the concurrently created source was preserved; the prior source remains at {}",
            recovery.display()
        ));
    }
    match fs::hard_link(recovery, file) {
        Ok(()) => {
            if recovery != backup_path(file) {
                if let Err(cleanup_error) = fs::remove_file(recovery) {
                    return error
                        .with_context(format!("recovery source: {}", recovery.display()))
                        .with_context(format!("recovery cleanup failed: {cleanup_error}"))
                        .with_help("Review and remove the retained recovery link after reloading");
                }
            }
            if let Err(sync_error) = sync_parent(file) {
                return error
                    .with_context(format!("automatic restore sync failed: {sync_error}"))
                    .with_help("Verify the restored configuration after the next filesystem sync");
            }
            error
        }
        Err(restore_error) => error
            .with_context(format!("recovery source: {}", recovery.display()))
            .with_context(format!("automatic restore failed: {restore_error}"))
            .with_help("Do not retry until the retained source has been restored or reviewed"),
    }
}

fn transaction_path(temporary: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(temporary.as_os_str());
    value.push(format!(".{suffix}"));
    PathBuf::from(value)
}

/// Install the backup through a fresh sibling and an atomic rename. Replacing
/// the directory entry avoids following a pre-existing `.bak` symlink into an
/// unrelated user file.
fn install_backup(
    source_file: &Path,
    backup: &Path,
    source: &str,
) -> Result<(), CandidateInstallFailure> {
    let temporary = temporary_path(backup).map_err(CandidateInstallFailure::from)?;
    let install = (|| -> Result<(), CandidateInstallFailure> {
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| {
                CandidateInstallFailure::from(file_error(
                    "create backup candidate for",
                    backup,
                    error,
                ))
            })?;
        output
            .write_all(source.as_bytes())
            .and_then(|()| output.sync_all())
            .map_err(|error| {
                CandidateInstallFailure::from(file_error("write backup for", backup, error))
            })?;
        if let Ok(metadata) = fs::metadata(source_file) {
            fs::set_permissions(&temporary, metadata.permissions()).map_err(|error| {
                CandidateInstallFailure::from(file_error(
                    "preserve backup permissions for",
                    backup,
                    error,
                ))
            })?;
        }
        replace_backup_candidate(&temporary, backup).map_err(CandidateInstallFailure::from)?;
        sync_parent(backup).map_err(CandidateInstallFailure::from)
    })();
    if install.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    install
}

#[cfg(unix)]
fn replace_backup_candidate(temporary: &Path, backup: &Path) -> Result<(), ShellError> {
    // POSIX rename replaces the directory entry itself, including a symlink,
    // without following its target.
    fs::rename(temporary, backup).map_err(|error| file_error("replace backup", backup, error))
}

#[cfg(not(unix))]
fn replace_backup_candidate(temporary: &Path, backup: &Path) -> Result<(), ShellError> {
    // Windows rename does not replace an existing file. Removing a symlink
    // removes the link itself; a concurrent recreation makes rename fail
    // closed instead of following the new entry.
    match fs::symlink_metadata(backup) {
        Ok(_) => fs::remove_file(backup)
            .map_err(|error| file_error("remove prior backup", backup, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(file_error("inspect prior backup", backup, error)),
    }
    fs::rename(temporary, backup).map_err(|error| file_error("replace backup", backup, error))
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
    EditorBanner,
    PickerLayout,
    PickerPreview,
    PromptSymbols,
    PromptLeft,
    PromptRight,
    PromptTransient,
    UiTheme,
    UiThemes,
    UiSurface,
    UiStatuslineHints,
    CompletionAuto,
    CompletionMinChars,
}

impl ConfigField {
    const ALL: [Self; 15] = [
        Self::EditorKeymap,
        Self::EditorSemanticHints,
        Self::EditorBanner,
        Self::PickerLayout,
        Self::PickerPreview,
        Self::PromptSymbols,
        Self::PromptLeft,
        Self::PromptRight,
        Self::PromptTransient,
        Self::UiTheme,
        Self::UiThemes,
        Self::UiSurface,
        Self::UiStatuslineHints,
        Self::CompletionAuto,
        Self::CompletionMinChars,
    ];

    const fn key(self) -> &'static str {
        match self {
            Self::EditorKeymap => "editor.keymap",
            Self::EditorSemanticHints => "editor.semantic_hints",
            Self::EditorBanner => "editor.banner",
            Self::PickerLayout => "picker.layout",
            Self::PickerPreview => "picker.preview",
            Self::PromptSymbols => "prompt.symbols",
            Self::PromptLeft => "prompt.left",
            Self::PromptRight => "prompt.right",
            Self::PromptTransient => "prompt.transient",
            Self::UiTheme => "ui.theme",
            Self::UiThemes => "ui.themes",
            Self::UiSurface => "ui.surface",
            Self::UiStatuslineHints => "ui.statusline.hints",
            Self::CompletionAuto => "completion.auto",
            Self::CompletionMinChars => "completion.min_chars",
        }
    }

    fn parse(key: &str) -> Result<Self, ShellError> {
        match key {
            "editor.keymap" => Ok(Self::EditorKeymap),
            "editor.semantic_hints" => Ok(Self::EditorSemanticHints),
            "editor.banner" => Ok(Self::EditorBanner),
            "picker.layout" => Ok(Self::PickerLayout),
            "picker.preview" => Ok(Self::PickerPreview),
            "prompt.symbols" => Ok(Self::PromptSymbols),
            "prompt.left" => Ok(Self::PromptLeft),
            "prompt.right" => Ok(Self::PromptRight),
            "prompt.transient" => Ok(Self::PromptTransient),
            "ui.theme" => Ok(Self::UiTheme),
            "ui.themes" => Ok(Self::UiThemes),
            "ui.surface" => Ok(Self::UiSurface),
            "ui.statusline.hints" => Ok(Self::UiStatuslineHints),
            "completion.auto" => Ok(Self::CompletionAuto),
            "completion.min_chars" => Ok(Self::CompletionMinChars),
            _ => Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("`{key}` is not a recognized configuration field"),
            )
            .with_help(format!("Recognized fields: {}", Self::KEYS.join(", ")))),
        }
    }

    const KEYS: [&'static str; 15] = [
        "editor.keymap",
        "editor.semantic_hints",
        "editor.banner",
        "picker.layout",
        "picker.preview",
        "prompt.symbols",
        "prompt.left",
        "prompt.right",
        "prompt.transient",
        "ui.theme",
        "ui.themes",
        "ui.surface",
        "ui.statusline.hints",
        "completion.auto",
        "completion.min_chars",
    ];

    const fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::EditorKeymap => ("editor", "keymap"),
            Self::EditorSemanticHints => ("editor", "semantic_hints"),
            Self::EditorBanner => ("editor", "banner"),
            Self::PickerLayout => ("picker", "layout"),
            Self::PickerPreview => ("picker", "preview"),
            Self::PromptSymbols => ("prompt", "symbols"),
            Self::PromptLeft => ("prompt", "left"),
            Self::PromptRight => ("prompt", "right"),
            Self::PromptTransient => ("prompt", "transient"),
            Self::UiTheme => ("ui", "theme"),
            Self::UiThemes => ("ui", "themes"),
            Self::UiSurface => ("ui", "surface"),
            Self::UiStatuslineHints => ("ui.statusline", "hints"),
            Self::CompletionAuto => ("completion", "auto"),
            Self::CompletionMinChars => ("completion", "min_chars"),
        }
    }

    fn lua_literal(self, value: &str) -> Result<String, ShellError> {
        if self == Self::UiTheme && value.len() > MAX_THEME_NAME_BYTES {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "theme name exceeds its byte limit",
            )
            .with_context(format!(
                "bytes: {}; limit: {MAX_THEME_NAME_BYTES}",
                value.len()
            ))
            .with_help("Use a shorter stable ASCII theme name"));
        }
        let valid = match self {
            Self::EditorKeymap => matches!(value, "helix" | "emacs" | "vim"),
            Self::EditorBanner => matches!(value, "full" | "compact" | "none"),
            Self::PickerLayout => matches!(value, "adaptive" | "bottom" | "full"),
            Self::PromptSymbols => {
                matches!(value, "auto" | "plain" | "unicode" | "nerd_font")
            }
            Self::UiTheme => valid_theme_name(value),
            Self::UiThemes => false,
            Self::UiSurface => matches!(value, "auto" | "rich" | "simple"),
            Self::CompletionMinChars => value.parse::<u16>().is_ok_and(|value| value <= 4096),
            Self::EditorSemanticHints
            | Self::PickerPreview
            | Self::PromptTransient
            | Self::UiStatuslineHints
            | Self::CompletionAuto => matches!(value, "true" | "false"),
            Self::PromptLeft | Self::PromptRight => serde_json::from_str::<Vec<String>>(value)
                .map(|values| values.iter().all(|item| valid_prompt_item(item)))
                .unwrap_or(false),
        };
        if !valid {
            let expected = match self {
                Self::EditorKeymap => "helix, emacs, or vim",
                Self::EditorBanner => "full, compact, or none",
                Self::PickerLayout => "adaptive, bottom, or full",
                Self::PromptSymbols => "auto, plain, unicode, or nerd_font",
                Self::UiTheme => {
                    "a non-empty theme name using lowercase letters, digits, or hyphens"
                }
                Self::UiThemes => "custom theme definitions edited directly in Lua",
                Self::UiSurface => "auto, rich, or simple",
                Self::CompletionMinChars => "an integer from 0 through 4096",
                Self::EditorSemanticHints
                | Self::PickerPreview
                | Self::PromptTransient
                | Self::UiStatuslineHints
                | Self::CompletionAuto => "true or false",
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
            Self::EditorKeymap
            | Self::EditorBanner
            | Self::PickerLayout
            | Self::PromptSymbols
            | Self::UiTheme
            | Self::UiSurface => {
                format!("\"{value}\"")
            }
            Self::UiThemes => {
                return Err(ShellError::new(
                    ErrorCode::InvalidArgument,
                    "ui.themes is code-controlled",
                )
                .with_help("Edit custom theme definitions directly in config.lua"));
            }
            Self::EditorSemanticHints
            | Self::PickerPreview
            | Self::PromptTransient
            | Self::UiStatuslineHints
            | Self::CompletionAuto
            | Self::CompletionMinChars => value.to_owned(),
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
            Self::EditorBanner => config.editor.banner.clone(),
            Self::PickerLayout => config.picker.layout.clone(),
            Self::PickerPreview => config.picker.preview.to_string(),
            Self::PromptSymbols => config.prompt.symbols.clone(),
            Self::PromptLeft => serde_json::to_string(&config.prompt.left).unwrap_or_default(),
            Self::PromptRight => serde_json::to_string(&config.prompt.right).unwrap_or_default(),
            Self::PromptTransient => config.prompt.transient.to_string(),
            Self::UiTheme => config.ui.theme.clone(),
            Self::UiThemes => serde_json::to_string(&config.ui.themes).unwrap_or_default(),
            Self::UiSurface => config.ui.surface.clone(),
            Self::UiStatuslineHints => config.ui.statusline.hints.to_string(),
            Self::CompletionAuto => config.completion.auto.to_string(),
            Self::CompletionMinChars => config.completion.min_chars.to_string(),
        }
    }
}

fn valid_theme_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_THEME_NAME_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
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
    let mut section_open = config_open;
    for section_part in section.split('.') {
        let section_values = field_values(&tokens, section_open, section_part);
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
        section_open = *section_value;
    }
    let values = field_values(&tokens, section_open, name);
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
        ConfigField::EditorKeymap
        | ConfigField::EditorBanner
        | ConfigField::PickerLayout
        | ConfigField::PromptSymbols
        | ConfigField::UiTheme
        | ConfigField::UiSurface => {
            matches!(&tokens[*value].kind, TokenKind::String)
        }
        ConfigField::UiThemes => false,
        ConfigField::EditorSemanticHints
        | ConfigField::PickerPreview
        | ConfigField::PromptTransient
        | ConfigField::UiStatuslineHints
        | ConfigField::CompletionAuto => matches!(
            &tokens[*value].kind,
            TokenKind::Identifier(value) if value == "true" || value == "false"
        ),
        ConfigField::CompletionMinChars => matches!(&tokens[*value].kind, TokenKind::Other),
        ConfigField::PromptLeft | ConfigField::PromptRight => literal_end_index.is_some(),
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
  editor = { keymap = "helix", semantic_hints = true, banner = "full" }, -- editor note
  picker = { layout = "adaptive", preview = true },
  prompt = { symbols = "auto", left = { "directory" }, right = {} },
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
    fn external_edit_at_final_install_boundary_is_preserved_as_a_conflict() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        let source = example_source();
        let external = source.replace("keymap = \"helix\"", "keymap = \"vim\"");
        let candidate = source.replace("preview = true", "preview = false");
        fs::write(&file, source).unwrap();
        let temporary = temporary_path(&file).unwrap();

        let error = install_candidate_with_hook(&file, &temporary, &candidate, source, || {
            fs::write(&file, &external).unwrap()
        })
        .unwrap_err();

        assert!(matches!(error, CandidateInstallFailure::Conflict(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), external);
        assert_eq!(fs::read_to_string(backup_path(&file)).unwrap(), source);
        assert!(!transaction_path(&temporary, "original").exists());
        fs::remove_file(temporary).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn backup_install_replaces_symlink_without_overwriting_its_target() {
        use std::os::unix::fs::symlink;

        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        let target = directory.join("unrelated.txt");
        fs::write(&file, example_source()).unwrap();
        fs::write(&target, "do not overwrite").unwrap();
        symlink(&target, backup_path(&file)).unwrap();

        set(&file, "picker.preview", "false").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "do not overwrite");
        assert_eq!(
            fs::read_to_string(backup_path(&file)).unwrap(),
            example_source()
        );
        assert!(!fs::symlink_metadata(backup_path(&file))
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn source_symlink_update_preserves_target_and_writes_a_regular_backup() {
        use std::os::unix::fs::symlink;

        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let target = directory.join("shared-config.lua");
        let file = directory.join("config.lua");
        fs::write(&target, example_source()).unwrap();
        symlink(&target, &file).unwrap();

        set(&file, "picker.preview", "false").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), example_source());
        assert!(!fs::symlink_metadata(&file)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_to_string(backup_path(&file)).unwrap(),
            example_source()
        );
        assert!(!fs::symlink_metadata(backup_path(&file))
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn prompt_symbol_profile_is_discoverable_and_patchable() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();

        assert_eq!(set(&file, "prompt.symbols", "nerd_font").unwrap(), 0);
        let config = load(&file).unwrap();
        assert_eq!(config.prompt.symbols, "nerd_font");
        assert!(fs::read_to_string(&file)
            .unwrap()
            .contains("symbols = \"nerd_font\""));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn theme_name_is_a_bounded_patchable_literal() {
        assert_eq!(
            ConfigField::UiTheme.lua_literal("tokyo-night").unwrap(),
            "\"tokyo-night\""
        );
        assert!(ConfigField::UiTheme.lua_literal("").is_err());
        assert!(ConfigField::UiTheme.lua_literal("Tokyo Night").is_err());
        let oversized = ConfigField::UiTheme
            .lua_literal(&"a".repeat(MAX_THEME_NAME_BYTES + 1))
            .unwrap_err();
        assert_eq!(oversized.code, ErrorCode::ResourceLimit);
        assert!(oversized.details.context[0].contains(&format!("limit: {MAX_THEME_NAME_BYTES}")));

        let source = r#"return quirl.config {
  ui = { theme = "tokyo-night", surface = "auto" },
}"#;
        let patched = patch_literal(source, ConfigField::UiTheme, "\"gruvbox-dark\"").unwrap();
        assert!(patched.contains("theme = \"gruvbox-dark\""));
        assert!(patched.contains("surface = \"auto\""));
    }

    #[test]
    fn rich_surface_fields_are_nested_patchable_and_validated() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, include_str!("../../../examples/config.lua")).unwrap();

        assert_eq!(set(&file, "ui.theme", "ansi").unwrap(), 0);
        assert_eq!(set(&file, "ui.surface", "rich").unwrap(), 0);
        assert_eq!(set(&file, "ui.statusline.hints", "false").unwrap(), 0);
        assert_eq!(set(&file, "completion.min_chars", "4").unwrap(), 0);

        let config = load(&file).unwrap();
        assert_eq!(config.schema_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(config.ui.theme, "ansi");
        assert_eq!(config.ui.surface, "rich");
        assert!(!config.ui.statusline.hints);
        assert_eq!(config.completion.min_chars, 4);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_complete_candidate_never_replaces_source() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        let invalid = example_source().replace(
            "prompt = { symbols = \"auto\", left = { \"directory\" }, right = {} }",
            "prompt = { symbols = \"auto\", left = { 42 }, right = {} }",
        );
        fs::write(&file, &invalid).unwrap();

        let error = set(&file, "picker.preview", "false").unwrap_err();

        assert_eq!(fs::read_to_string(&file).unwrap(), invalid);
        assert!(!backup_path(&file).exists());
        assert_eq!(error.code, ErrorCode::Validation);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_preview_inserts_the_current_schema_without_mutating_the_source() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();
        let source = read_config_source(&file).unwrap();

        let (version, candidate) = migration_candidate(&source).unwrap();

        assert_eq!(version, None);
        assert!(candidate.contains(&format!("schema_version = {CONFIG_SCHEMA_VERSION}")));
        assert_eq!(read_config_source(&file).unwrap(), source);
        assert!(!backup_path(&file).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migration_preview_rejects_a_future_schema_instead_of_calling_it_current() {
        let source = example_source().replace(
            "editor = { keymap = \"helix\", semantic_hints = true, banner = \"full\" },",
            &format!(
                "schema_version = {},\n  editor = {{ keymap = \"helix\", semantic_hints = true, banner = \"full\" }},",
                CONFIG_SCHEMA_VERSION + 1
            ),
        );

        let error = migration_candidate(&source).unwrap_err();

        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("newer"));
        assert!(error.details.help[0].contains("Quirl version"));
    }

    #[test]
    fn migration_preview_projects_v1_to_the_current_schema_without_rewriting_other_source() {
        let source = example_source().replace(
            "editor = { keymap = \"helix\", semantic_hints = true, banner = \"full\" },",
            "schema_version = 1,\n  editor = { keymap = \"helix\", semantic_hints = true, banner = \"full\" },",
        );

        let (version, candidate) = migration_candidate(&source).unwrap();

        assert_eq!(version, Some(1));
        assert!(candidate.contains(&format!("schema_version = {CONFIG_SCHEMA_VERSION}")));
        assert!(candidate.contains("-- Keep this comment"));
        assert!(candidate.contains("render = function() return \"ok\" end"));
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
    fn diff_reports_the_selected_theme() {
        let left = load_test_config();
        let mut right = left.clone();
        right.ui.theme = "gruvbox-dark".to_owned();

        assert_eq!(
            config_differences(&left, &right),
            vec![ConfigDifference {
                key: "ui.theme",
                before: left.ui.theme,
                after: "gruvbox-dark".to_owned(),
            }]
        );
    }

    #[test]
    fn custom_theme_map_diff_is_deterministic() {
        let left = load_test_config();
        let mut right = left.clone();
        let colors = right.active_theme().unwrap();
        right.ui.themes.insert("z-last".to_owned(), colors.clone());
        right.ui.themes.insert("a-first".to_owned(), colors);

        let differences = config_differences(&left, &right);
        assert_eq!(differences.len(), 1);
        assert_eq!(differences[0].key, "ui.themes");
        assert_eq!(differences[0].before, "{}");
        let first = differences[0].after.find("a-first").unwrap();
        let last = differences[0].after.find("z-last").unwrap();
        assert!(first < last);
    }

    #[test]
    fn doctor_distinguishes_literal_and_code_controlled_fields_without_writing() {
        let source = example_source().replace("keymap = \"helix\"", "keymap = \"helix\" .. \"\"");
        let config = load_test_config();
        let (literal, code_controlled) = patchable_fields(&source, &config);

        assert!(!literal.contains(&"editor.keymap"));
        assert!(code_controlled.contains(&"editor.keymap"));
        assert!(code_controlled.contains(&"ui.themes"));
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
            ("editor_banner".to_owned(), "full".to_owned()),
            ("picker_layout".to_owned(), "adaptive".to_owned()),
            ("picker_preview".to_owned(), "true".to_owned()),
            ("prompt_symbols".to_owned(), "auto".to_owned()),
            ("prompt_left".to_owned(), "directory".to_owned()),
            ("prompt_right".to_owned(), String::new()),
            ("prompt_transient".to_owned(), "true".to_owned()),
            ("ui_theme".to_owned(), "tokyo-night".to_owned()),
            ("ui_surface".to_owned(), "auto".to_owned()),
            ("ui_statusline_hints".to_owned(), "true".to_owned()),
            ("completion_auto".to_owned(), "false".to_owned()),
            ("completion_min_chars".to_owned(), "2".to_owned()),
        ])
    }

    #[test]
    fn web_form_renders_the_complete_schema_without_a_second_store() {
        let mut config = load_test_config();
        let custom_colors = config.active_theme().unwrap();
        config
            .ui
            .themes
            .insert("custom-preview".to_owned(), custom_colors);
        let session = WebSession {
            token: "private".to_owned(),
            source: example_source().to_owned(),
            config,
        };
        let page = render_form(&session, None);
        assert!(page.contains("Schema"));
        assert!(page.contains("editor_keymap"));
        assert!(page.contains("editor_banner"));
        assert!(page.contains("picker_layout"));
        assert!(page.contains("prompt_left"));
        assert!(page.contains("ui_theme"));
        assert!(page.contains("value=\"tokyo-night\" checked"));
        assert!(page.contains("class=\"theme-grid\""));
        assert!(page.contains("class=\"terminal\""));
        assert!(page.contains("value=\"dracula\""));
        assert!(page.contains("value=\"solarized-dark\""));
        assert!(page.contains("value=\"custom-preview\""));
        assert!(page.contains("ui_surface"));
        assert!(page.contains("completion_min_chars"));
        assert!(!page.contains("Cache-Control"));
        assert!(!page.contains("<script"));
    }

    #[test]
    fn web_form_patches_a_builtin_theme_selection() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        let source = example_source().replace(
            "  prompt = { symbols = \"auto\", left = { \"directory\" }, right = {} },\n}",
            "  prompt = { symbols = \"auto\", left = { \"directory\" }, right = {} },\n  ui = { theme = \"tokyo-night\", surface = \"auto\", statusline = { hints = true } },\n}",
        );
        fs::write(&file, &source).unwrap();
        let mut session = WebSession {
            token: "private".to_owned(),
            source: source.clone(),
            config: load(&file).unwrap(),
        };
        let mut form = web_form("private", &source, "helix");
        form.insert("ui_theme".to_owned(), "ansi".to_owned());

        apply_web_form(&file, &mut session, &form).unwrap();

        assert_eq!(load(&file).unwrap().ui.theme, "ansi");
        assert!(fs::read_to_string(&file)
            .unwrap()
            .contains("theme = \"ansi\""));
        fs::remove_dir_all(directory).unwrap();
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

        assert!(matches!(error, WebFormFailure::Conflict(_)));
        assert_eq!(error.status(), 409);
        assert!(error
            .error()
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
        assert!(!loopback_authority("127.0.0.1:0"));
        assert!(!loopback_authority("127.0.0.1:"));
        assert!(!loopback_origin("http://127.0.0.1:1234/not-an-origin"));
        assert!(loopback_origin("http://127.0.0.1:1234"));
        assert!(is_urlencoded_form_content_type(
            "application/x-www-form-urlencoded; charset=utf-8"
        ));
        assert!(!is_urlencoded_form_content_type(
            "application/x-www-form-urlencoded-evil"
        ));
        assert!(request_size_is_allowed(128, WEB_MAX_REQUEST_BYTES - 128));
        assert!(!request_size_is_allowed(128, WEB_MAX_REQUEST_BYTES));
        assert!(!request_size_is_allowed(usize::MAX, 1));
    }

    #[test]
    fn disconnected_browser_writes_are_nonfatal_but_other_write_errors_are_reported() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::NotConnected,
        ] {
            assert!(client_disconnected(&io::Error::from(kind)), "{kind:?}");
        }
        let error = io::Error::from(io::ErrorKind::WriteZero);
        assert!(!client_disconnected(&error));
        let reported = web_response_error(error);
        assert_eq!(reported.code, ErrorCode::Io);
        assert!(!reported.details.help.is_empty());
    }

    #[test]
    fn http_reader_rejects_an_oversized_declared_body_before_reading_it() {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(
                    format!(
                        "POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {WEB_MAX_REQUEST_BYTES}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();

        let error = read_http_request(&mut stream).unwrap_err();

        assert!(error.message.contains("body is too large"));
        client.join().unwrap();
    }

    #[test]
    fn http_reader_rejects_eof_before_a_declared_body() {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .write_all(b"POST / HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 8\r\n\r\nshort")
                .unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();

        let error = read_http_request(&mut stream).unwrap_err();

        assert!(error.message.contains("ended before its body"));
        client.join().unwrap();
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
