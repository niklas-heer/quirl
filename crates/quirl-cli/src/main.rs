mod agent;
mod author;
mod config;
mod extensions;
mod index;
mod lsp;
mod mcp;
mod package;
mod pick;
mod platform;
mod plugin;
mod protocol;
mod recovery;
mod script;

use agent::AgentCommand;
use author::{DescribeCommand, DocCommand, NewCommand};
use clap::{Parser, Subcommand, ValueEnum};
use config::ConfigCommand;
use extensions::{LuaCompletionAdapter, LuaExtensionHost};
use index::IndexCommand;
use mcp::ServeCommand;
use package::PackageCommand;
use pick::PickCommand;
use platform::{EventsCommand, ViewCommand, WatchCommand};
use plugin::PluginCommand;
use quirl_catalog::{Catalog, CommandSpec, Completion};
use quirl_core::{
    escape_json_terminal_controls, escape_terminal_controls, reject_terminal_controls,
    CommandOutcome, ErrorCode, ExtensionAction, ExtensionEventData, OutputStream, ShellError,
};
use quirl_data::{DataRenderFormat, DataRuntime};
use quirl_lua::{
    sdk_json, sdk_lua, sdk_markdown, LuaPolicy, LuaRuntime, QuirlConfig, MAX_LUA_SOURCE_BYTES,
};
use quirl_picker::{ItemKind, PickItem, Picker, MAX_PICKER_ITEMS};
use quirl_process::{sandboxed_process_host, JobStatus, NativeExecutor};
use quirl_syntax::{classify, InteractiveLine, Mode};
use quirl_ui::{
    editor_with_extensions_config_history_and_picker, history_path, render_error, select_surface,
    terminal_supports_unicode, terminal_width, InteractiveSignal, PickerItem, PickerItemKind,
    PickerMatch, PickerRanker, PromptContextScheduler, QuirlPrompt, RichSurface, SurfaceKind,
    MODE_TOGGLE_HOST_COMMAND,
};
use recovery::RecoveryCommand;
use script::ScriptLanguage;
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Parser)]
#[command(name = "quirl", version, about = "Everything you need, mixed in")]
struct Cli {
    /// Emit machine-readable metadata for release tooling.
    #[arg(long, hide = true)]
    build_info: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a checked embedded-language script from Quirl's template.
    New {
        #[command(flatten)]
        command: NewCommand,
    },
    /// Run a Lua, Quirl, Bash, or Zsh script through its explicit engine.
    Run {
        /// Script file, or - for standard input with --lang or a recognized shebang.
        file: PathBuf,
        #[arg(long, value_enum)]
        lang: Option<ScriptLanguage>,
        #[arg(trailing_var_arg = true)]
        arguments: Vec<String>,
    },
    /// Evaluate Lua and print the returned value.
    Eval { expression: String },
    /// Evaluate a native structured-data expression or pipeline.
    Data {
        expression: String,
        /// Select a human table/plain renderer or the explicit machine envelope.
        #[arg(long, value_enum, default_value_t = DataOutputFormat::Table)]
        format: DataOutputFormat,
    },
    /// Validate Lua or native Quirl (.qrl, .quirl, .🌀) scripts without executing source.
    Check {
        /// Script file or recursively discovered directory.
        #[arg(value_name = "PATH")]
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = DiagnosticFormat::Text)]
        format: DiagnosticFormat,
    },
    /// Deterministically format Lua files under a file or directory path.
    Fmt {
        /// Lua or native Quirl script/directory; native source is unchanged.
        #[arg(value_name = "PATH")]
        file: PathBuf,
        #[arg(long)]
        check: bool,
    },
    /// Lint Lua or native Quirl (.qrl, .quirl, .🌀) scripts without execution.
    Lint {
        /// Script file or recursively discovered directory.
        #[arg(value_name = "PATH")]
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = DiagnosticFormat::Text)]
        format: DiagnosticFormat,
    },
    /// Run discovered Lua test modules; defaults to the current directory.
    Test {
        /// Explicit test file or recursively discovered directory.
        #[arg(value_name = "PATH")]
        file: Option<PathBuf>,
    },
    /// Validate or inspect Lua configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Validate a trusted Lua plugin's registrations.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
    },
    /// Export the generated Lua SDK used by editors, docs, and AI.
    Sdk {
        #[arg(long, value_enum, default_value_t = SdkFormat::Text)]
        format: SdkFormat,
    },
    /// Export the semantic command catalog used by completion, docs, and AI.
    Catalog {
        #[arg(long, value_enum, default_value_t = CatalogFormat::Json)]
        format: CatalogFormat,
    },
    /// Export deterministic installed context and validation contracts for agents.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Inspect, validate, build, or dry-run publication of a Quirl package.
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
    /// Describe one installed command from the semantic catalog.
    Describe {
        #[command(flatten)]
        command: DescribeCommand,
    },
    /// Generate deterministic human or machine documentation.
    Doc {
        #[command(flatten)]
        command: DocCommand,
    },
    /// Serve deterministic Lua and native Quirl (.qrl, .quirl, .🌀) intelligence over stdio LSP.
    Lsp,
    /// Serve explicitly granted Quirl tooling to MCP clients over bounded stdio JSON-RPC.
    Serve {
        #[command(subcommand)]
        command: ServeCommand,
    },
    /// Build and inspect the attributed completion index.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    /// Ask the same completion engine used by the interactive IDE menu.
    Complete {
        input: String,
        #[arg(long, value_enum, default_value_t = CompletionFormat::Text)]
        format: CompletionFormat,
    },
    /// Select typed history, files, actions, or input with the shared picker.
    Pick {
        #[command(flatten)]
        command: PickCommand,
    },
    /// Inspect and validate the typed extension event protocol.
    Events {
        #[command(subcommand)]
        command: EventsCommand,
    },
    /// Render escape-safe directory and process panel models.
    View {
        #[command(subcommand)]
        command: ViewCommand,
    },
    /// Re-evaluate a typed data pipeline with bounded live samples.
    Watch {
        #[command(flatten)]
        command: WatchCommand,
    },
    /// Inspect recoverable snapshots from failed commands.
    Recover {
        #[command(subcommand)]
        command: RecoveryCommand,
    },
    /// Execute one command through Quirl's native pipeline and job graph.
    Exec {
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DiagnosticFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SdkFormat {
    Text,
    Json,
    Markdown,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CatalogFormat {
    Text,
    Json,
    Markdown,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompletionFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DataOutputFormat {
    Json,
    Plain,
    Table,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.build_info {
        print_json_value(serde_json::json!({
            "schema_version": 2,
            "version": env!("CARGO_PKG_VERSION"),
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "optimization_level": env!("QUIRL_BUILD_OPT_LEVEL"),
            "panic_strategy": if cfg!(panic = "unwind") { "unwind" } else { "abort" },
            "operating_system": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "source_commit": env!("QUIRL_BUILD_COMMIT"),
            "source_dirty": match env!("QUIRL_BUILD_DIRTY") {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
        }));
        return ExitCode::SUCCESS;
    }
    let wants_json = cli.wants_json();
    match run(cli) {
        Ok(status) => ExitCode::from(status.clamp(0, 255) as u8),
        Err(error) if wants_json => {
            match serde_json::to_string_pretty(&error) {
                Ok(json) => println!("{}", escape_json_terminal_controls(&json)),
                Err(_) => eprintln!("{}", render_stderr_error(&error)),
            }
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("{}", render_stderr_error(&error));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<i32, ShellError> {
    match cli.command {
        Some(Command::New { command }) => author::create(command),
        Some(Command::Run {
            file,
            lang,
            arguments,
        }) => {
            let output = script::run(&file, lang, &arguments)?;
            print_json_value(output.value);
            Ok(output.status)
        }
        Some(Command::Eval { expression }) => {
            let lua =
                LuaRuntime::new_with_process_host(LuaPolicy::script(), sandboxed_process_host())?;
            print_json_value(lua.eval(&expression)?);
            Ok(0)
        }
        Some(Command::Data { expression, format }) => {
            let format = match format {
                DataOutputFormat::Json => DataRenderFormat::Json,
                DataOutputFormat::Plain => DataRenderFormat::Plain,
                DataOutputFormat::Table => DataRenderFormat::Table,
            };
            let stdout = io::stdout();
            let mut output = stdout.lock();
            DataRuntime::with_process_host(sandboxed_process_host())
                .render_to_with_cancellation_handle(
                    &expression,
                    format,
                    Arc::new(AtomicBool::new(false)),
                    &mut output,
                )?;
            Ok(0)
        }
        Some(Command::Check { file, format }) => {
            script::analyze(&file, matches!(format, DiagnosticFormat::Json), false)
        }
        Some(Command::Fmt { file, check }) => script::format_paths(&file, check),
        Some(Command::Lint { file, format }) => {
            script::analyze(&file, matches!(format, DiagnosticFormat::Json), true)
        }
        Some(Command::Test { file }) => {
            script::test_paths(file.as_deref().unwrap_or_else(|| Path::new(".")))
        }
        Some(Command::Config { command }) => config::execute(command),
        Some(Command::Plugin { command }) => plugin::execute(command),
        Some(Command::Sdk { format }) => {
            match format {
                SdkFormat::Text => print!("{}", sdk_lua()),
                SdkFormat::Json => println!("{}", sdk_json()?),
                SdkFormat::Markdown => print!("{}", sdk_markdown()),
            }
            Ok(0)
        }
        Some(Command::Catalog { format }) => {
            let catalog = load_composed_catalog();
            match format {
                CatalogFormat::Json => {
                    let json = serde_json::to_string_pretty(&catalog).map_err(json_error)?;
                    println!("{}", escape_json_terminal_controls(&json));
                }
                CatalogFormat::Markdown => {
                    print!("{}", escape_terminal_controls(&catalog.to_markdown()));
                }
                CatalogFormat::Text => print_catalog(&catalog),
            }
            Ok(0)
        }
        Some(Command::Agent { command }) => agent::execute(command, &load_composed_catalog()),
        Some(Command::Package { command }) => package::execute(command, &load_composed_catalog()),
        Some(Command::Describe { command }) => author::describe(command, &load_composed_catalog()),
        Some(Command::Doc { command }) => author::doc(command, &load_composed_catalog()),
        Some(Command::Lsp) => lsp::execute(load_composed_catalog()),
        Some(Command::Serve { command }) => mcp::execute(command),
        Some(Command::Index { command }) => index::execute(command),
        Some(Command::Complete { input, format }) => {
            let mut catalog = index::load_default_catalog();
            let mut extensions = LuaExtensionHost::discover();
            extensions.merge_catalog_contributions(&mut catalog);
            let mut completions = catalog.complete(&input, input.len());
            completions.extend(
                extensions
                    .complete(&input, input.len())
                    .into_iter()
                    .map(extension_completion),
            );
            match format {
                CompletionFormat::Json => {
                    let json = serde_json::to_string_pretty(&completions).map_err(json_error)?;
                    println!("{}", escape_json_terminal_controls(&json));
                }
                _ => {
                    for completion in completions {
                        println!(
                            "{:<28} {}",
                            escape_terminal_controls(&completion.display),
                            escape_terminal_controls(&completion.summary)
                        );
                    }
                }
            }
            Ok(0)
        }
        Some(Command::Pick { command }) => pick::execute(command, &load_composed_catalog()),
        Some(Command::Events { command }) => platform::execute_events(command),
        Some(Command::View { command }) => platform::execute_view(command),
        Some(Command::Watch { command }) => platform::execute_watch(command),
        Some(Command::Recover { command }) => recovery::execute(command),
        Some(Command::Exec { command }) => run_exec_with_recovery(&command.join(" ")),
        None if !io::stdin().is_terminal() => run_stdin(),
        None => {
            let mut host = LuaExtensionHost::discover();
            let catalog = compose_catalog(&mut host);
            repl(catalog, Arc::new(Mutex::new(host)))
        }
    }
}

fn load_composed_catalog() -> Catalog {
    compose_catalog(&mut LuaExtensionHost::discover())
}

fn compose_catalog(extensions: &mut LuaExtensionHost) -> Catalog {
    let mut catalog = index::load_default_catalog();
    extensions.merge_catalog_contributions(&mut catalog);
    catalog
}

fn extension_completion(suggestion: quirl_ui::ExtensionSuggestion) -> Completion {
    Completion {
        value: suggestion.value,
        display: suggestion.display,
        summary: suggestion.summary,
        detail: suggestion.detail,
        replace_start: suggestion.replace_start,
        replace_end: suggestion.replace_end,
        match_indices: Vec::new(),
    }
}

impl Cli {
    fn wants_json(&self) -> bool {
        self.command.as_ref().is_some_and(Command::wants_json)
    }
}

impl Command {
    fn wants_json(&self) -> bool {
        match self {
            Self::Check {
                format: DiagnosticFormat::Json,
                ..
            }
            | Self::Lint {
                format: DiagnosticFormat::Json,
                ..
            }
            | Self::Sdk {
                format: SdkFormat::Json,
            }
            | Self::Catalog {
                format: CatalogFormat::Json,
            }
            | Self::Complete {
                format: CompletionFormat::Json,
                ..
            } => true,
            Self::Data {
                format: DataOutputFormat::Json,
                ..
            } => true,
            Self::Config { command } => config::wants_json(command),
            Self::Plugin { command } => plugin::wants_json(command),
            Self::Agent { command } => agent::wants_json(command),
            Self::Package { command } => package::wants_json(command),
            Self::Describe { command } => {
                matches!(command.format, author::DocumentationFormat::Json)
            }
            Self::Doc { command } => {
                matches!(command.format, author::DocumentationFormat::Json)
            }
            Self::Index { command } => index::wants_json(command),
            Self::Pick { command } => command.wants_json(),
            Self::Events { command } => platform::events_wants_json(command),
            Self::View { command } => platform::view_wants_json(command),
            Self::Watch { command } => {
                matches!(command.format, platform::PlatformOutputFormat::Json)
            }
            Self::Recover { command } => recovery::wants_json(command),
            _ => false,
        }
    }
}

fn run_exec_with_recovery(command: &str) -> Result<i32, ShellError> {
    let journal = recovery::RecoveryJournal::discover()?;
    let extensions = Arc::new(Mutex::new(LuaExtensionHost::discover()));
    begin_extension_session(&extensions);
    emit_directory_snapshot(&extensions);
    execute_with_recovery(
        &mut NativeExecutor::default(),
        &journal,
        command,
        Some(&extensions),
        ExecutionOutputMode::Capture,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionOutputMode {
    Capture,
    Interactive,
}

fn execute_with_recovery(
    executor: &mut NativeExecutor,
    journal: &recovery::RecoveryJournal,
    command: &str,
    extensions: Option<&Arc<Mutex<LuaExtensionHost>>>,
    output_mode: ExecutionOutputMode,
) -> Result<i32, ShellError> {
    let planned = match extensions {
        Some(extensions) => {
            match prepare_extension_plan(extensions, command, vec!["spawn_process".to_owned()]) {
                Ok(planned) => planned,
                Err(error) => {
                    let mut annotations = BTreeMap::new();
                    apply_observation_actions(
                        notify_extensions(
                            extensions,
                            ExtensionEventData::Error {
                                error: error.clone(),
                            },
                        ),
                        &mut annotations,
                    );
                    print_extension_annotations(&annotations);
                    return Err(error);
                }
            }
        }
        None => PlannedExecution::new(command),
    };
    let recovery_context = journal.capture_context(&planned.source)?;
    let PlannedExecution {
        source,
        mut annotations,
    } = planned;
    if let Some(extensions) = extensions {
        apply_observation_actions(
            notify_extensions(
                extensions,
                ExtensionEventData::ExecutionProgress {
                    completed: 0,
                    total: Some(1),
                    message: Some("execution started".to_owned()),
                },
            ),
            &mut annotations,
        );
    }
    let started = Instant::now();
    let renders_captured_output = output_mode == ExecutionOutputMode::Capture
        || interactive_dialect_island(&source).is_some();
    match execute_command_or_dialect_island(executor, &source, output_mode) {
        Ok(outcome) => {
            let duration = started.elapsed();
            if outcome.status != 0 {
                if let Err(error) =
                    journal.record_failure(&recovery_context, duration, Some(&outcome), None)
                {
                    eprintln!("warning: {}", render_stderr_error(&error));
                }
            }
            if let Some(extensions) = extensions {
                emit_outcome_events(extensions, &outcome, &mut annotations);
                apply_observation_actions(
                    notify_extensions(
                        extensions,
                        ExtensionEventData::ExecutionProgress {
                            completed: 1,
                            total: Some(1),
                            message: Some("execution finished".to_owned()),
                        },
                    ),
                    &mut annotations,
                );
                apply_observation_actions(
                    notify_extensions(
                        extensions,
                        ExtensionEventData::Result {
                            status: outcome.status,
                            duration_ms: duration_millis(duration),
                        },
                    ),
                    &mut annotations,
                );
            }
            if renders_captured_output {
                print_outcome(&outcome);
            }
            print_extension_annotations(&annotations);
            Ok(outcome.status)
        }
        Err(error) => {
            let duration = started.elapsed();
            if let Err(journal_error) =
                journal.record_failure(&recovery_context, duration, None, Some(&error))
            {
                eprintln!("warning: {}", render_stderr_error(&journal_error));
            }
            if let Some(extensions) = extensions {
                apply_observation_actions(
                    notify_extensions(
                        extensions,
                        ExtensionEventData::Error {
                            error: error.clone(),
                        },
                    ),
                    &mut annotations,
                );
                print_extension_annotations(&annotations);
            }
            Err(error)
        }
    }
}

fn execute_command_or_dialect_island(
    executor: &mut NativeExecutor,
    source: &str,
    output_mode: ExecutionOutputMode,
) -> Result<CommandOutcome, ShellError> {
    if let Some((language, body)) = interactive_dialect_island(source) {
        return script::run_interactive_island(
            language,
            body,
            &script::ScriptCancellation::default(),
        );
    }
    match output_mode {
        ExecutionOutputMode::Capture => executor.execute_capture(source),
        ExecutionOutputMode::Interactive => executor.execute_interactive(source),
    }
}

/// Recognize a deliberately tiny, explicit bridge form. The body is passed verbatim to the
/// selected interpreter; Quirl does not try to parse or reinterpret its dialect grammar.
fn interactive_dialect_island(source: &str) -> Option<(ScriptLanguage, &str)> {
    let source = source.trim();
    for (prefix, language) in [("bash", ScriptLanguage::Bash), ("zsh", ScriptLanguage::Zsh)] {
        let Some(rest) = source.strip_prefix(prefix) else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with('{') || !rest.ends_with('}') {
            continue;
        }
        return Some((language, rest[1..rest.len().saturating_sub(1)].trim()));
    }
    None
}

fn repl(catalog: Catalog, extensions: Arc<Mutex<LuaExtensionHost>>) -> Result<i32, ShellError> {
    begin_extension_session(&extensions);
    emit_directory_snapshot(&extensions);
    let mut observed_directory = std::env::current_dir().unwrap_or_default();
    let (mut active_config, mut applied_revision) = {
        let mut host = extensions.lock().map_err(|_| {
            ShellError::new(ErrorCode::Io, "the extension host lock was poisoned")
                .with_help("Restart Quirl to create a fresh extension host")
        })?;
        let config = host.active_config().clone();
        (config, host.config_revision())
    };
    let history_path = history_path()?;
    let mut line_editor =
        configured_editor(&catalog, &extensions, active_config.clone(), &history_path)?;
    print_banner(&active_config);
    let mut mode = Mode::Command;
    let mut executor = NativeExecutor::default();
    let recovery = recovery::RecoveryJournal::discover()?;
    let data = DataRuntime::with_process_host(sandboxed_process_host());
    // Script evaluation remains lazy; extension VMs load before the first editor view.
    let mut lua = None;
    let mut last_status = 0;
    let mut last_duration: Option<Duration> = None;
    let prompt_context = PromptContextScheduler::default();

    loop {
        let current_directory = std::env::current_dir().unwrap_or_default();
        if current_directory != observed_directory {
            let mut annotations = BTreeMap::new();
            apply_observation_actions(
                notify_extensions(
                    &extensions,
                    ExtensionEventData::DirectoryChanged {
                        previous: observed_directory.display().to_string(),
                        current: current_directory.display().to_string(),
                    },
                ),
                &mut annotations,
            );
            print_extension_annotations(&annotations);
            observed_directory = current_directory;
        }
        let (extension_segments, next_config) = extensions
            .lock()
            .map(|mut extensions| {
                extensions.reload_if_changed();
                let revision = extensions.config_revision();
                let next_config = (revision != applied_revision)
                    .then(|| (extensions.active_config().clone(), revision));
                let segments: Vec<_> = extensions
                    .named_prompt_segments(mode, last_status)
                    .into_iter()
                    .map(|segment| (segment.name, segment.value))
                    .collect();
                (segments, next_config)
            })
            .unwrap_or_default();
        if let Some((config, revision)) = next_config {
            applied_revision = revision;
            if config != active_config {
                sync_history(&mut line_editor, &history_path)?;
                active_config = config;
                line_editor =
                    configured_editor(&catalog, &extensions, active_config.clone(), &history_path)?;
            }
        }
        print_extension_errors(&extensions);
        let active_jobs = executor
            .jobs()
            .iter()
            .filter(|job| job.status != JobStatus::Done)
            .count();
        let native_context = prompt_context.sample_current_dir();
        let mut prompt = QuirlPrompt::with_config(mode, &active_config)
            .with_native_context(native_context.context)
            .with_status(last_status)
            .with_jobs(active_jobs)
            .with_named_extension_segments(extension_segments);
        if let Some(duration) = last_duration {
            prompt = prompt.with_duration(duration);
        }
        match line_editor.read_line(&prompt) {
            Ok(InteractiveSignal::Success(buffer)) => {
                sync_history(&mut line_editor, &history_path)?;
                match classify(mode, &buffer) {
                    InteractiveLine::Empty => {}
                    InteractiveLine::Exit => return Ok(last_status),
                    InteractiveLine::ChangeMode(next) => {
                        mode = next;
                        print_mode_feedback(mode, &active_config);
                    }
                    InteractiveLine::ToggleMode => {
                        mode = mode.toggled();
                        print_mode_feedback(mode, &active_config);
                    }
                    InteractiveLine::Help(topic) => print_help(&catalog, topic),
                    InteractiveLine::Command(command) => {
                        let started = Instant::now();
                        match execute_with_recovery(
                            &mut executor,
                            &recovery,
                            command,
                            Some(&extensions),
                            ExecutionOutputMode::Interactive,
                        ) {
                            Ok(status) => {
                                last_status = status;
                            }
                            Err(error) => {
                                last_status = 1;
                                eprintln!("{}", render_stderr_error(&error));
                            }
                        }
                        last_duration = Some(started.elapsed());
                    }
                    InteractiveLine::Data(source) => {
                        let planned = match prepare_extension_plan(&extensions, source, Vec::new())
                        {
                            Ok(planned) => planned,
                            Err(error) => {
                                last_status = 1;
                                eprintln!("{}", render_stderr_error(&error));
                                continue;
                            }
                        };
                        let mut annotations = planned.annotations;
                        apply_observation_actions(
                            notify_extensions(
                                &extensions,
                                ExtensionEventData::ExecutionProgress {
                                    completed: 0,
                                    total: Some(1),
                                    message: Some("data evaluation started".to_owned()),
                                },
                            ),
                            &mut annotations,
                        );
                        let started = Instant::now();
                        match data.eval(&planned.source) {
                            Ok(value) => {
                                last_status = 0;
                                emit_value_output(&extensions, &value, &mut annotations);
                                print_json_value(value);
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::ExecutionProgress {
                                            completed: 1,
                                            total: Some(1),
                                            message: Some("data evaluation finished".to_owned()),
                                        },
                                    ),
                                    &mut annotations,
                                );
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::Result {
                                            status: 0,
                                            duration_ms: duration_millis(started.elapsed()),
                                        },
                                    ),
                                    &mut annotations,
                                );
                                print_extension_annotations(&annotations);
                            }
                            Err(error) => {
                                last_status = 1;
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::Error {
                                            error: error.clone(),
                                        },
                                    ),
                                    &mut annotations,
                                );
                                print_extension_annotations(&annotations);
                                eprintln!("{}", render_stderr_error(&error));
                            }
                        }
                        last_duration = Some(started.elapsed());
                    }
                    InteractiveLine::Lua(source) => {
                        let planned = match prepare_extension_plan(&extensions, source, Vec::new())
                        {
                            Ok(planned) => planned,
                            Err(error) => {
                                last_status = 1;
                                eprintln!("{}", render_stderr_error(&error));
                                continue;
                            }
                        };
                        let mut annotations = planned.annotations;
                        apply_observation_actions(
                            notify_extensions(
                                &extensions,
                                ExtensionEventData::ExecutionProgress {
                                    completed: 0,
                                    total: Some(1),
                                    message: Some("Lua evaluation started".to_owned()),
                                },
                            ),
                            &mut annotations,
                        );
                        let started = Instant::now();
                        match eval_lua(&mut lua, &planned.source) {
                            Ok(value) => {
                                last_status = 0;
                                emit_value_output(&extensions, &value, &mut annotations);
                                print_json_value(value);
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::ExecutionProgress {
                                            completed: 1,
                                            total: Some(1),
                                            message: Some("Lua evaluation finished".to_owned()),
                                        },
                                    ),
                                    &mut annotations,
                                );
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::Result {
                                            status: 0,
                                            duration_ms: duration_millis(started.elapsed()),
                                        },
                                    ),
                                    &mut annotations,
                                );
                                print_extension_annotations(&annotations);
                            }
                            Err(error) => {
                                last_status = 1;
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::Error {
                                            error: error.clone(),
                                        },
                                    ),
                                    &mut annotations,
                                );
                                print_extension_annotations(&annotations);
                                eprintln!("{}", render_stderr_error(&error));
                            }
                        }
                        last_duration = Some(started.elapsed());
                    }
                }
            }
            Ok(InteractiveSignal::CtrlC) => {
                last_status = 130;
                let mut annotations = BTreeMap::new();
                apply_observation_actions(
                    notify_extensions(
                        &extensions,
                        ExtensionEventData::Cancellation {
                            reason: "interactive interrupt".to_owned(),
                        },
                    ),
                    &mut annotations,
                );
                println!("^C");
            }
            Ok(InteractiveSignal::CtrlD) => return Ok(last_status),
            Ok(InteractiveSignal::HostCommand(command)) if command == MODE_TOGGLE_HOST_COMMAND => {
                mode = mode.toggled();
                print_mode_feedback(mode, &active_config);
            }
            Ok(InteractiveSignal::Suspend) => suspend_self()?,
            Ok(_) => {}
            Err(error) => {
                return Err(
                    ShellError::new(ErrorCode::Io, "the interactive editor failed")
                        .with_context(error.to_string()),
                );
            }
        }
    }
}

#[derive(Debug)]
struct PlannedExecution {
    source: String,
    annotations: BTreeMap<String, serde_json::Value>,
}

impl PlannedExecution {
    fn new(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            annotations: BTreeMap::new(),
        }
    }
}

fn notify_extensions(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    event: ExtensionEventData,
) -> Vec<ExtensionAction> {
    extensions
        .lock()
        .map(|mut extensions| extensions.dispatch_event(event))
        .unwrap_or_default()
}

fn begin_extension_session(extensions: &Arc<Mutex<LuaExtensionHost>>) {
    let restored_session = std::env::var("QUIRL_SESSION_ID")
        .ok()
        .filter(|session| !session.trim().is_empty());
    let mut annotations = BTreeMap::new();
    apply_observation_actions(
        notify_extensions(
            extensions,
            ExtensionEventData::SessionStart {
                restored: restored_session.is_some(),
            },
        ),
        &mut annotations,
    );
    if let Some(session_id) = restored_session {
        apply_observation_actions(
            notify_extensions(
                extensions,
                ExtensionEventData::SessionRestore { session_id },
            ),
            &mut annotations,
        );
    }
    print_extension_annotations(&annotations);
}

fn emit_directory_snapshot(extensions: &Arc<Mutex<LuaExtensionHost>>) {
    let current = std::env::current_dir()
        .unwrap_or_default()
        .display()
        .to_string();
    let mut annotations = BTreeMap::new();
    apply_observation_actions(
        notify_extensions(
            extensions,
            ExtensionEventData::DirectoryChanged {
                previous: current.clone(),
                current,
            },
        ),
        &mut annotations,
    );
    print_extension_annotations(&annotations);
}

fn prepare_extension_plan(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    source: &str,
    effects: Vec<String>,
) -> Result<PlannedExecution, ShellError> {
    let mut planned = PlannedExecution::new(source);
    apply_plan_actions(
        notify_extensions(
            extensions,
            ExtensionEventData::CommandPlan {
                source: source.to_owned(),
                effects,
            },
        ),
        &mut planned,
    )?;
    Ok(planned)
}

fn apply_plan_actions(
    actions: Vec<ExtensionAction>,
    planned: &mut PlannedExecution,
) -> Result<(), ShellError> {
    for action in actions {
        match action {
            ExtensionAction::Diagnose { message } => eprintln!("extension: {message}"),
            ExtensionAction::RewritePlan { source } => planned.source = source,
            ExtensionAction::SetEnvironment { name, value } => std::env::set_var(name, value),
            ExtensionAction::BlockExecution { reason } => {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    "an extension blocked execution",
                )
                .with_context(reason)
                .with_help("Review the extension policy or disable the blocking plugin"))
            }
            ExtensionAction::AnnotateResult { key, value } => {
                planned.annotations.insert(key, value);
            }
        }
    }
    Ok(())
}

fn apply_observation_actions(
    actions: Vec<ExtensionAction>,
    annotations: &mut BTreeMap<String, serde_json::Value>,
) {
    for action in actions {
        match action {
            ExtensionAction::Diagnose { message } => eprintln!("extension: {message}"),
            ExtensionAction::SetEnvironment { name, value } => std::env::set_var(name, value),
            ExtensionAction::AnnotateResult { key, value } => {
                annotations.insert(key, value);
            }
            ExtensionAction::RewritePlan { .. } | ExtensionAction::BlockExecution { .. } => {
                eprintln!("extension: plan mutation was ignored after execution began");
            }
        }
    }
}

fn emit_outcome_events(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    outcome: &CommandOutcome,
    annotations: &mut BTreeMap<String, serde_json::Value>,
) {
    for (stream, text) in [
        (OutputStream::Stdout, outcome.stdout.as_deref()),
        (OutputStream::Stderr, outcome.stderr.as_deref()),
    ] {
        if let Some(text) = text {
            apply_observation_actions(
                notify_extensions(
                    extensions,
                    ExtensionEventData::Output {
                        stream,
                        bytes: text.len(),
                        text: safe_extension_output_text(text),
                    },
                ),
                annotations,
            );
        }
    }
}

fn emit_value_output(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    value: &serde_json::Value,
    annotations: &mut BTreeMap<String, serde_json::Value>,
) {
    let text = serde_json::to_string(value).unwrap_or_default();
    apply_observation_actions(
        notify_extensions(
            extensions,
            ExtensionEventData::Output {
                stream: OutputStream::Stdout,
                bytes: text.len(),
                text: safe_extension_output_text(&text),
            },
        ),
        annotations,
    );
}

fn safe_extension_output_text(text: &str) -> Option<String> {
    reject_terminal_controls("extension output", text)
        .is_ok()
        .then(|| text.to_owned())
}

fn print_extension_annotations(annotations: &BTreeMap<String, serde_json::Value>) {
    for (key, value) in annotations {
        let value = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
        eprintln!(
            "extension annotation {}: {}",
            escape_terminal_controls(key),
            escape_terminal_controls(&value)
        );
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

enum SessionEditor {
    Rich(Box<RichSurface>),
    Simple(Box<reedline::Reedline>),
}

impl SessionEditor {
    fn read_line(&mut self, prompt: &QuirlPrompt) -> Result<InteractiveSignal, ShellError> {
        match self {
            Self::Rich(editor) => editor.read_line(prompt),
            Self::Simple(editor) => editor
                .read_line(prompt)
                .map(|signal| match signal {
                    reedline::Signal::Success(buffer) => InteractiveSignal::Success(buffer),
                    reedline::Signal::CtrlC => InteractiveSignal::CtrlC,
                    reedline::Signal::CtrlD => InteractiveSignal::CtrlD,
                    reedline::Signal::HostCommand(command) => {
                        InteractiveSignal::HostCommand(command)
                    }
                    _ => InteractiveSignal::CtrlC,
                })
                .map_err(|error| {
                    ShellError::new(ErrorCode::Io, "the interactive editor failed")
                        .with_context(error.to_string())
                        .with_help("Retry with ui.surface = \"simple\"")
                }),
        }
    }

    fn sync_history(&mut self, history_path: &Path) -> Result<(), ShellError> {
        match self {
            Self::Rich(editor) => editor.sync_history(),
            Self::Simple(editor) => sync_reedline_history(editor, history_path),
        }
    }
}

fn configured_editor(
    catalog: &Catalog,
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    config: QuirlConfig,
    history_path: &Path,
) -> Result<SessionEditor, ShellError> {
    let completion_adapter = LuaCompletionAdapter::new(Arc::clone(extensions));
    let picker_ranker: Arc<dyn PickerRanker> = Arc::new(SharedPickerRanker);
    if select_surface(&config.ui.surface) == SurfaceKind::Rich {
        return RichSurface::new(
            catalog.clone(),
            Some(Box::new(completion_adapter)),
            Arc::clone(&picker_ranker),
            &config,
            history_path.to_path_buf(),
        )
        .map(|editor| SessionEditor::Rich(Box::new(editor)));
    }
    editor_with_extensions_config_history_and_picker(
        catalog.clone(),
        Some(Box::new(completion_adapter)),
        config,
        history_path.to_path_buf(),
        picker_ranker,
    )
    .map(|editor| SessionEditor::Simple(Box::new(editor)))
}

#[derive(Debug)]
struct SharedPickerRanker;

impl PickerRanker for SharedPickerRanker {
    fn rank(&self, items: &[PickerItem], query: &str, limit: usize) -> Vec<PickerMatch> {
        let shared = items
            .iter()
            .take(MAX_PICKER_ITEMS)
            .map(|item| PickItem {
                id: item.id.clone(),
                kind: shared_picker_kind(item.kind),
                label: item.label.clone(),
                description: item.description.clone(),
                preview: item.preview.clone(),
                value: serde_json::Value::String(item.value.clone()),
            })
            .collect::<Vec<_>>();
        Picker
            .rank(&shared, query)
            .into_iter()
            .take(limit.min(MAX_PICKER_ITEMS))
            .map(|matched| PickerMatch {
                index: matched.index,
                match_indices: matched.match_indices,
            })
            .collect()
    }
}

const fn shared_picker_kind(kind: PickerItemKind) -> ItemKind {
    match kind {
        PickerItemKind::History => ItemKind::History,
        PickerItemKind::File => ItemKind::File,
        PickerItemKind::Directory => ItemKind::Directory,
        PickerItemKind::Action => ItemKind::Action,
        PickerItemKind::Completion => ItemKind::Completion,
        PickerItemKind::Job => ItemKind::Job,
        PickerItemKind::Data => ItemKind::Data,
    }
}

fn sync_history(editor: &mut SessionEditor, history_path: &Path) -> Result<(), ShellError> {
    editor.sync_history(history_path)
}

fn sync_reedline_history(
    editor: &mut reedline::Reedline,
    history_path: &Path,
) -> Result<(), ShellError> {
    editor.sync_history().map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not save history to {}", history_path.display()),
        )
        .with_context(error.to_string())
        .with_help("Set QUIRL_HISTORY to a writable file path")
    })
}

#[cfg(unix)]
fn suspend_self() -> Result<(), ShellError> {
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not suspend the interactive shell")
            .with_context(error.to_string())
            .with_help(
                "Use the terminal's job-control shortcut or run Quirl from a job-control shell",
            )
    })
}

#[cfg(not(unix))]
fn suspend_self() -> Result<(), ShellError> {
    Ok(())
}

fn print_extension_errors(extensions: &Arc<Mutex<LuaExtensionHost>>) {
    let errors = extensions
        .lock()
        .map(|mut extensions| extensions.take_errors())
        .unwrap_or_default();
    for error in errors {
        eprintln!("{}", render_stderr_error(&error));
    }
}

fn print_banner(config: &QuirlConfig) {
    let terminal = io::stdout().is_terminal();
    if !show_welcome(&config.editor.banner, terminal) {
        return;
    }
    let unicode = unicode_chrome(&config.prompt.symbols, terminal_supports_unicode());
    let banner = if config.editor.banner == "compact" {
        compact_welcome(terminal_width().unwrap_or(80), unicode)
    } else {
        onboarding_banner(terminal_width().unwrap_or(80), unicode)
    };
    if color_enabled(
        terminal,
        std::env::var_os("NO_COLOR").is_some(),
        terminal_is_dumb(),
    ) {
        if let Some(rest) = banner.strip_prefix("Quirl") {
            println!("\x1b[1;32mQuirl\x1b[0m{rest}");
        } else {
            println!("{banner}");
        }
    } else {
        println!("{banner}");
    }
}

fn show_welcome(banner: &str, terminal: bool) -> bool {
    banner != "none" && terminal
}

fn onboarding_banner(width: u16, unicode: bool) -> String {
    let width = usize::from(width.max(1));
    if width < 32 {
        return minimal_onboarding(width);
    }
    let separator = if unicode { " · " } else { " | " };
    let heading_separator = if unicode { " · " } else { " - " };
    let heading = if width >= 96 {
        format!(
            "Quirl {}{heading_separator}a modern shell for processes and typed data",
            env!("CARGO_PKG_VERSION")
        )
    } else if width >= 64 {
        format!(
            "Quirl {}{heading_separator}processes + typed data",
            env!("CARGO_PKG_VERSION")
        )
    } else {
        format!(
            "Quirl {}{heading_separator}command + data",
            env!("CARGO_PKG_VERSION")
        )
    };
    let mut lines = vec![heading];
    if width >= 96 {
        lines.push(
            [
                "COMMAND: processes/bytes",
                "DATA: typed values/tables",
                "Alt-M mode",
            ]
            .join(separator),
        );
        lines.push(
            [
                "Tab semantic completion",
                "Ctrl-R history",
                "Ctrl-T files",
                "Ctrl-K actions",
                "F1 help",
            ]
            .join(separator),
        );
        lines.push("Nerd Font prompt: set prompt.symbols = \"nerd_font\" in config.lua".to_owned());
        lines.push("Type help to explore commands.".to_owned());
    } else if width >= 64 {
        lines.push(["COMMAND: processes/bytes", "DATA: typed values"].join(separator));
        lines.push(["Alt-M mode", "Tab semantic completion", "Ctrl-R history"].join(separator));
        lines.push(["Ctrl-T files", "Ctrl-K actions", "F1 help"].join(separator));
        lines.push("Nerd Font prompt: prompt.symbols = \"nerd_font\"".to_owned());
    } else {
        lines.push("Processes + typed data".to_owned());
        lines.push(["Alt-M mode", "Tab complete"].join(separator));
        lines.push(["Ctrl-R history", "Ctrl-T files"].join(separator));
        lines.push(["Ctrl-K actions", "F1 help"].join(separator));
        lines.push("Nerd Font prompt:".to_owned());
        lines.push("  prompt.symbols = \"nerd_font\"".to_owned());
    }
    lines.join("\n")
}

fn compact_welcome(width: u16, unicode: bool) -> String {
    let separator = if unicode { " · " } else { " | " };
    let line = [
        format!("Quirl {}", env!("CARGO_PKG_VERSION")),
        "COMMAND/DATA".to_owned(),
        "Tab complete".to_owned(),
        "Alt-M mode".to_owned(),
        "help".to_owned(),
    ]
    .join(separator);
    truncate_display_width(&line, usize::from(width.max(1)))
}

fn minimal_onboarding(width: usize) -> String {
    let mut lines = vec![if width >= UnicodeWidthStr::width("Quirl") {
        "Quirl".to_owned()
    } else {
        truncate_display_width("Quirl", width)
    }];
    for hint in ["Alt-M mode", "Tab complete", "Ctrl-R/T/K pickers", "help"] {
        if UnicodeWidthStr::width(hint) <= width {
            lines.push(hint.to_owned());
        }
    }
    lines.join("\n")
}

fn truncate_display_width(value: &str, width: usize) -> String {
    let mut rendered = String::new();
    let mut used = 0_usize;
    for character in value.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used.saturating_add(character_width) > width {
            break;
        }
        rendered.push(character);
        used = used.saturating_add(character_width);
    }
    rendered
}

fn print_mode_feedback(mode: Mode, config: &QuirlConfig) {
    let unicode = unicode_chrome(&config.prompt.symbols, terminal_supports_unicode());
    println!("{}", mode_feedback(mode, unicode));
}

fn unicode_chrome(prompt_symbols: &str, terminal_unicode: bool) -> bool {
    prompt_symbols != "plain" && terminal_unicode
}

fn mode_feedback(mode: Mode, unicode: bool) -> String {
    let (separator, description) = match (unicode, mode) {
        (true, Mode::Command) => (" → ", "processes and byte pipelines"),
        (true, Mode::Data) => (" → ", "typed values and data pipelines"),
        (false, Mode::Command) => (": ", "processes and byte pipelines"),
        (false, Mode::Data) => (": ", "typed values and data pipelines"),
    };
    let detail_separator = if unicode { " · " } else { " - " };
    format!("mode{separator}{mode}{detail_separator}{description}")
}

fn render_stderr_error(error: &ShellError) -> String {
    render_error(
        error,
        color_enabled(
            io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
            terminal_is_dumb(),
        ),
    )
}

fn color_enabled(terminal: bool, no_color_is_set: bool, dumb_terminal: bool) -> bool {
    terminal && !no_color_is_set && !dumb_terminal
}

fn terminal_is_dumb() -> bool {
    is_dumb_terminal(std::env::var("TERM").ok().as_deref())
}

fn is_dumb_terminal(term: Option<&str>) -> bool {
    term.is_some_and(|term| term.eq_ignore_ascii_case("dumb"))
}

fn eval_lua(
    runtime: &mut Option<LuaRuntime>,
    source: &str,
) -> Result<serde_json::Value, ShellError> {
    if runtime.is_none() {
        *runtime = Some(LuaRuntime::new_with_process_host(
            LuaPolicy::script(),
            sandboxed_process_host(),
        )?);
    }
    let runtime = runtime.as_ref().ok_or_else(|| {
        ShellError::new(ErrorCode::Lua, "could not initialize the Lua runtime")
            .with_help("Run the command again; if this persists, report the configuration used.")
    })?;
    runtime.eval(source)
}

fn run_stdin() -> Result<i32, ShellError> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_LUA_SOURCE_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read standard input")
                .with_context(error.to_string())
        })?;
    if bytes.len() > MAX_LUA_SOURCE_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "standard-input Lua source exceeds its read limit",
        )
        .with_context(format!(
            "bytes: {}; limit: {MAX_LUA_SOURCE_BYTES}",
            bytes.len()
        ))
        .with_help("Keep executable source below 4 MiB and load data through bounded inputs"));
    }
    let source = String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::ScriptRead,
            "standard-input Lua source is not valid UTF-8",
        )
        .with_context(error.to_string())
        .with_help("Encode Lua source as UTF-8")
    })?;
    let lua = LuaRuntime::new_with_process_host(LuaPolicy::script(), sandboxed_process_host())?;
    print_json_value(lua.eval(&source)?);
    Ok(0)
}

fn print_help(catalog: &Catalog, topic: Option<&str>) {
    if let Some(topic) = topic {
        if let Some(command) = catalog.find(topic) {
            print_command_help(command);
        } else {
            println!(
                "No exact catalog entry for `{}`. Press Tab to explore related commands.",
                escape_terminal_controls(topic)
            );
        }
    } else {
        print_catalog(catalog);
    }
}

fn print_catalog(catalog: &Catalog) {
    println!("Quirl commands\n");
    for command in &catalog.commands {
        println!(
            "  {:<24} {}",
            escape_terminal_controls(&command.signature),
            escape_terminal_controls(&command.summary)
        );
    }
    println!(
        "\nTab opens the IDE completion menu; `quirl catalog --format json` is the AI interface."
    );
}

fn print_command_help(command: &CommandSpec) {
    println!(
        "{}\n  {}\n\n{}",
        escape_terminal_controls(&command.signature),
        escape_terminal_controls(&command.summary),
        escape_terminal_controls(&command.details)
    );
    if !command.options.is_empty() {
        println!("\nOptions:");
        for option in &command.options {
            println!(
                "  {:<20} {}",
                escape_terminal_controls(&option.names.join(", ")),
                escape_terminal_controls(&option.documentation)
            );
        }
    }
    if !command.examples.is_empty() {
        println!("\nExamples:");
        for example in &command.examples {
            println!("  {}", escape_terminal_controls(example));
        }
    }
}

fn print_json_value(value: serde_json::Value) {
    match value {
        serde_json::Value::Null => {}
        serde_json::Value::String(value) => println!("{}", escape_terminal_controls(&value)),
        value if value.is_object() || value.is_array() => {
            let json = serde_json::to_string_pretty(&value)
                .unwrap_or_else(|_| "<unprintable Lua value>".to_owned());
            println!("{}", escape_json_terminal_controls(&json));
        }
        value => println!("{value}"),
    }
}

fn print_outcome(outcome: &quirl_core::CommandOutcome) {
    if let Some(stdout) = &outcome.stdout {
        if !stdout.is_empty() {
            println!("{stdout}");
        }
    }
    if let Some(stderr) = &outcome.stderr {
        if !stderr.is_empty() {
            eprintln!("{stderr}");
        }
    }
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not produce JSON").with_context(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use quirl_catalog::{ArgumentKind, CompletionSource};
    use quirl_core::CommandOutcome;
    use std::collections::BTreeSet;
    use std::{
        fs,
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static NEXT_DIFFERENTIAL_FIXTURE: AtomicUsize = AtomicUsize::new(0);
    const DEFAULT_DIFFERENTIAL_CASES: usize = 128;
    const DEFAULT_DIFFERENTIAL_SEED: u64 = 7_640_891_576_956_012_809;

    struct DifferentialGenerator {
        state: u64,
    }

    impl DifferentialGenerator {
        fn new(seed: u64) -> Self {
            Self { state: seed.max(1) }
        }

        fn next(&mut self) -> u64 {
            let mut value = self.state;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.state = value;
            value
        }

        fn bounded(&mut self, upper_exclusive: usize) -> usize {
            usize::try_from(self.next() % u64::try_from(upper_exclusive).unwrap()).unwrap()
        }

        fn word(&mut self) -> String {
            let len = 1_usize.saturating_add(self.bounded(12));
            (0..len)
                .map(|_| {
                    let offset = u8::try_from(self.bounded(26)).unwrap();
                    char::from(b'a'.saturating_add(offset))
                })
                .collect()
        }

        fn source(&mut self) -> String {
            let left = self.word();
            let right = self.word();
            match self.bounded(8) {
                0 => format!("printf '%s:%s' '{left}' '{right}'"),
                1 => format!("printf '%s' '{left}' | tr a-z A-Z"),
                2 => format!("printf '%s' '{left}'; printf '%s' '{right}'"),
                3 => format!("true && printf '%s' '{left}' || printf '%s' '{right}'"),
                4 => format!("false && printf '%s' '{left}' || printf '%s' '{right}'"),
                5 => {
                    let a = self.bounded(100);
                    let b = self.bounded(100);
                    format!("printf '%s' $(({a} + {b}))")
                }
                6 => format!("printf '[%s]' $(printf '%s' '{left}')"),
                _ => format!("printf '%s' '{left}' | cat"),
            }
        }
    }

    fn differential_setting(name: &str, default: usize) -> usize {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (1..=10_000).contains(value))
            .unwrap_or(default)
    }

    fn differential_seed() -> u64 {
        std::env::var("QUIRL_TEST_SEED")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_DIFFERENTIAL_SEED)
    }

    #[test]
    fn welcome_respects_the_configured_visibility_and_terminal_boundary() {
        assert!(show_welcome("full", true));
        assert!(show_welcome("compact", true));
        assert!(!show_welcome("none", true));
        assert!(!show_welcome("full", false));
        assert!(unicode_chrome("auto", true));
        assert!(!unicode_chrome("auto", false));
        assert!(!unicode_chrome("plain", true));
    }

    #[test]
    fn onboarding_adapts_without_hiding_daily_driver_shortcuts() {
        for (width, unicode) in [(32, false), (64, true), (96, true)] {
            let banner = onboarding_banner(width, unicode);
            assert!(banner.contains("Alt-M mode"));
            assert!(banner.contains("semantic completion") || banner.contains("Tab complete"));
            assert!(banner.contains("Ctrl-R history"));
            assert!(banner.contains("Ctrl-T files"));
            assert!(banner.contains("Ctrl-K actions"));
            assert!(banner.contains("prompt.symbols = \"nerd_font\""));
            assert!(!banner.contains('\u{1b}'));
            assert!(banner
                .lines()
                .all(|line| UnicodeWidthStr::width(line) <= usize::from(width)));
            if !unicode {
                assert!(banner.is_ascii());
            }
        }
    }

    #[test]
    fn onboarding_has_a_display_width_safe_minimal_layout() {
        for width in [1, 2, 4, 8, 16, 31] {
            let banner = onboarding_banner(width, true);
            assert!(!banner.is_empty());
            assert!(banner
                .lines()
                .all(|line| UnicodeWidthStr::width(line) <= usize::from(width)));
            assert!(!banner.contains('\u{1b}'));
        }
    }

    #[test]
    fn compact_welcome_is_width_safe_and_font_safe() {
        for width in [1, 16, 32, 64, 96] {
            let plain = compact_welcome(width, false);
            assert!(plain.is_ascii());
            assert!(UnicodeWidthStr::width(plain.as_str()) <= usize::from(width));
            let unicode = compact_welcome(width, true);
            assert!(UnicodeWidthStr::width(unicode.as_str()) <= usize::from(width));
        }
    }

    #[test]
    fn onboarding_color_is_independent_from_plain_glyphs() {
        assert!(!unicode_chrome("plain", true));
        assert!(color_enabled(true, false, false));
        assert!(!color_enabled(true, true, false));
        assert!(!color_enabled(true, false, true));
    }

    #[test]
    fn mode_feedback_names_the_active_execution_model() {
        assert_eq!(
            mode_feedback(Mode::Command, false),
            "mode: command - processes and byte pipelines"
        );
        assert_eq!(
            mode_feedback(Mode::Data, true),
            "mode → data · typed values and data pipelines"
        );
    }

    struct DifferentialFixture {
        root: PathBuf,
    }

    impl DifferentialFixture {
        fn create(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "quirl-cli-differential-{label}-{}-{}",
                std::process::id(),
                NEXT_DIFFERENTIAL_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("input"), "from input\\n").unwrap();
            Self { root }
        }

        fn path(&self, name: &str) -> String {
            shell_word(&self.root.join(name))
        }

        fn redirected_output(&self) -> Option<Vec<u8>> {
            fs::read(self.root.join("output")).ok()
        }
    }

    impl Drop for DifferentialFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn shell_word(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\\"'\\\"'"))
    }

    fn shell_is_available(shell: &str) -> bool {
        std::process::Command::new(shell)
            .arg("-c")
            .arg(":")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn reference_outcome(shell: &str, source: &str) -> CommandOutcome {
        let mut command = std::process::Command::new(shell);
        if shell == "bash" {
            command.args(["--noprofile", "--norc", "-c", source]);
            command.env_remove("BASH_ENV").env_remove("ENV");
        } else {
            command.args(["-f", "-c", source]);
            command.env_remove("ZDOTDIR").env_remove("ENV");
        }
        let output = command.env("LC_ALL", "C").output().unwrap();
        CommandOutcome {
            status: output.status.code().unwrap_or(1),
            stdout: Some(String::from_utf8(output.stdout).unwrap()),
            stderr: Some(String::from_utf8(output.stderr).unwrap()),
        }
    }

    fn assert_same_outcome(label: &str, native: &CommandOutcome, reference: &CommandOutcome) {
        assert_eq!(native.status, reference.status, "{label}: status");
        assert_eq!(
            native.stdout.as_deref().unwrap_or_default(),
            reference.stdout.as_deref().unwrap_or_default(),
            "{label}: stdout"
        );
        assert_eq!(
            native.stderr.as_deref().unwrap_or_default(),
            reference.stderr.as_deref().unwrap_or_default(),
            "{label}: stderr"
        );
    }

    fn assert_native_and_reference_case(
        label: &str,
        source: impl Fn(&DifferentialFixture) -> String,
        expected_redirected_output: Option<&[u8]>,
    ) {
        let native_fixture = DifferentialFixture::create(label);
        let native_source = source(&native_fixture);
        let native = NativeExecutor::default()
            .execute_capture(&native_source)
            .unwrap();
        assert_eq!(
            native_fixture.redirected_output().as_deref(),
            expected_redirected_output,
            "{label}: native redirected filesystem effect"
        );

        for shell in ["bash", "zsh"] {
            if !shell_is_available(shell) {
                eprintln!("skipping {shell} composition differential: executable is unavailable");
                continue;
            }
            let reference_fixture = DifferentialFixture::create(label);
            let reference_source = source(&reference_fixture);
            let reference = reference_outcome(shell, &reference_source);
            assert_same_outcome(label, &native, &reference);
            assert_eq!(
                reference_fixture.redirected_output().as_deref(),
                expected_redirected_output,
                "{label}: {shell} redirected filesystem effect"
            );
        }
    }

    #[test]
    fn color_requires_a_terminal_and_no_color_must_be_absent() {
        assert!(color_enabled(true, false, false));
        assert!(!color_enabled(false, false, false));
        assert!(!color_enabled(true, true, false));
        assert!(!color_enabled(true, false, true));
    }

    #[test]
    fn term_dumb_disables_color_case_insensitively() {
        assert!(is_dumb_terminal(Some("dumb")));
        assert!(is_dumb_terminal(Some("DUMB")));
        assert!(!is_dumb_terminal(Some("xterm-256color")));
        assert!(!is_dumb_terminal(None));
    }

    #[test]
    fn commands_reject_output_formats_they_do_not_implement() {
        for arguments in [
            vec!["quirl", "check", "example.lua", "--format", "markdown"],
            vec!["quirl", "lint", "example.lua", "--format", "markdown"],
            vec![
                "quirl",
                "config",
                "check",
                "config.lua",
                "--format",
                "markdown",
            ],
            vec![
                "quirl",
                "plugin",
                "check",
                "plugin.lua",
                "--format",
                "markdown",
            ],
            vec!["quirl", "complete", "git", "--format", "markdown"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        assert!(Cli::try_parse_from(["quirl", "sdk", "--format", "markdown"]).is_ok());
        assert!(Cli::try_parse_from(["quirl", "catalog", "--format", "markdown"]).is_ok());
    }

    #[test]
    fn machine_formats_own_failures_after_argument_parsing() {
        for arguments in [
            vec!["quirl", "check", "missing.lua", "--format", "json"],
            vec!["quirl", "agent", "context", "deploy", "--format", "json"],
            vec!["quirl", "package", "publish", "--format", "json"],
            vec!["quirl", "describe", "missing", "--format", "json"],
            vec!["quirl", "doc", "--open", "--format", "json"],
            vec!["quirl", "index", "explain", "missing", "--format", "json"],
            vec![
                "quirl", "pick", "--source", "files", "--root", "missing", "--format", "json",
            ],
            vec!["quirl", "events", "validate", "missing", "--format", "json"],
            vec!["quirl", "view", "directory", "missing", "--format", "json"],
            vec![
                "quirl",
                "watch",
                "pwd",
                "--capacity",
                "0",
                "--format",
                "json",
            ],
        ] {
            let cli = Cli::try_parse_from(arguments).unwrap();
            assert!(cli.wants_json());
        }
    }

    #[test]
    fn lsp_is_a_leaf_stdio_command() {
        assert!(matches!(
            Cli::try_parse_from(["quirl", "lsp"]),
            Ok(Cli {
                build_info: false,
                command: Some(Command::Lsp)
            })
        ));
        assert!(Cli::try_parse_from(["quirl", "lsp", "--port", "9000"]).is_err());
    }

    #[test]
    fn release_tooling_build_info_is_hidden_and_machine_selected() {
        let cli = Cli::try_parse_from(["quirl", "--build-info"]).unwrap();
        assert!(cli.build_info);
        assert!(cli.command.is_none());
        assert!(!<Cli as clap::CommandFactory>::command()
            .render_long_help()
            .to_string()
            .contains("build-info"));
    }

    #[test]
    fn extension_plan_actions_rewrite_annotate_and_block_before_execution() {
        let mut planned = PlannedExecution::new("echo original");
        apply_plan_actions(
            vec![
                ExtensionAction::RewritePlan {
                    source: "echo rewritten".to_owned(),
                },
                ExtensionAction::AnnotateResult {
                    key: "policy".to_owned(),
                    value: serde_json::json!("checked"),
                },
            ],
            &mut planned,
        )
        .unwrap();
        assert_eq!(planned.source, "echo rewritten");
        assert_eq!(planned.annotations["policy"], "checked");

        let error = apply_plan_actions(
            vec![ExtensionAction::BlockExecution {
                reason: "denied by policy".to_owned(),
            }],
            &mut planned,
        )
        .unwrap_err();
        assert!(error.message.contains("blocked"));
        assert!(safe_extension_output_text("\u{1b}[31mraw").is_none());
    }

    #[test]
    fn catalog_contracts_match_visible_clap_arguments_and_value_domains() {
        fn leaves<'command>(
            command: &'command clap::Command,
            prefix: &str,
            output: &mut Vec<(String, &'command clap::Command)>,
        ) {
            for child in command
                .get_subcommands()
                .filter(|child| child.get_name() != "help")
            {
                let path = format!("{prefix} {}", child.get_name());
                if child
                    .get_subcommands()
                    .any(|grandchild| grandchild.get_name() != "help")
                {
                    leaves(child, &path, output);
                } else {
                    output.push((path, child));
                }
            }
        }

        fn visible_option_names(argument: &clap::Arg) -> BTreeSet<String> {
            argument
                .get_long_and_visible_aliases()
                .into_iter()
                .flatten()
                .map(|name| format!("--{name}"))
                .chain(
                    argument
                        .get_short_and_visible_aliases()
                        .into_iter()
                        .flatten()
                        .map(|name| format!("-{name}")),
                )
                .collect()
        }

        let catalog = Catalog::builtin();
        let mut cli_leaves = Vec::new();
        let cli = Cli::command();
        leaves(&cli, "quirl", &mut cli_leaves);
        for (path, cli) in cli_leaves {
            let contract = catalog
                .commands
                .iter()
                .find(|command| command.path == path)
                .unwrap_or_else(|| panic!("CLI leaf `{path}` has no exact catalog contract"));
            let visible_arguments = cli
                .get_arguments()
                .filter(|argument| {
                    !argument.is_hide_set()
                        && !matches!(argument.get_action(), clap::ArgAction::Help)
                })
                .collect::<Vec<_>>();

            let documented_options = contract
                .options
                .iter()
                .filter(|argument| argument.kind != ArgumentKind::Positional)
                .flat_map(|argument| argument.names.iter().cloned())
                .collect::<BTreeSet<_>>();
            let clap_options = visible_arguments
                .iter()
                .filter(|argument| !argument.is_positional())
                .flat_map(|argument| visible_option_names(argument))
                .collect::<BTreeSet<_>>();
            assert_eq!(
                documented_options, clap_options,
                "catalog option names diverge from Clap for `{path}`"
            );

            for argument in visible_arguments {
                if argument.is_positional() {
                    let id = argument.get_id().as_str();
                    let mut matching = contract
                        .options
                        .iter()
                        .filter(|documented| {
                            documented.kind == ArgumentKind::Positional
                                && documented.names.iter().any(|name| {
                                    name.split('|')
                                        .any(|candidate| candidate.eq_ignore_ascii_case(id))
                                })
                        })
                        .collect::<Vec<_>>();
                    if matching.is_empty() {
                        let positionals = contract
                            .options
                            .iter()
                            .filter(|documented| documented.kind == ArgumentKind::Positional)
                            .collect::<Vec<_>>();
                        if positionals.len() == 1 {
                            matching = positionals;
                        }
                    }
                    assert_eq!(
                        matching.len(),
                        1,
                        "catalog must describe positional `{id}` for `{path}` exactly once"
                    );
                    let documented = matching[0];
                    assert_eq!(
                        documented.required,
                        argument.is_required_set(),
                        "catalog requiredness diverges for positional `{id}` on `{path}`"
                    );
                    assert_eq!(
                        documented.repeatable,
                        matches!(argument.get_action(), clap::ArgAction::Append),
                        "catalog repeatability diverges for positional `{id}` on `{path}`"
                    );
                    assert!(
                                contract.signature.contains(&documented.names[0]),
                                "catalog signature `{}` omits its positional contract for `{id}` on `{path}`",
                                contract.signature
                            );
                    continue;
                }

                let names = visible_option_names(argument);
                let matching = contract
                    .options
                    .iter()
                    .filter(|documented| {
                        documented.kind != ArgumentKind::Positional
                            && documented.names.iter().cloned().collect::<BTreeSet<_>>() == names
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    matching.len(),
                    1,
                    "catalog must describe visible option(s) {names:?} for `{path}` exactly once"
                );
                let documented = matching[0];
                let possible_values = matches!(
                    argument.get_action(),
                    clap::ArgAction::Set | clap::ArgAction::Append
                )
                .then(|| {
                    argument
                        .get_possible_values()
                        .into_iter()
                        .map(|value| value.get_name().to_owned())
                        .collect()
                })
                .unwrap_or_default();
                let catalog_values = match &documented.values {
                    Some(CompletionSource::Static { values }) => values.iter().cloned().collect(),
                    Some(CompletionSource::Dynamic { provider }) => panic!(
                        "catalog option(s) {names:?} for `{path}` uses dynamic values `{provider}` but Clap exposes a fixed domain"
                    ),
                    None => BTreeSet::new(),
                };
                assert_eq!(
                    catalog_values, possible_values,
                    "catalog value domain diverges for option(s) {names:?} on `{path}`"
                );
                assert_eq!(
                    documented.required,
                    argument.is_required_set(),
                    "catalog requiredness diverges for option(s) {names:?} on `{path}`"
                );
                assert_eq!(
                    documented.repeatable,
                    matches!(argument.get_action(), clap::ArgAction::Append),
                    "catalog repeatability diverges for option(s) {names:?} on `{path}`"
                );
                assert!(
                    names.iter().all(|name| contract.signature.contains(name)),
                    "catalog signature `{}` omits option(s) {names:?} for `{path}`",
                    contract.signature
                );
            }
        }
    }

    #[test]
    fn native_executor_matches_bash_and_zsh_for_frozen_c1_composition_fixtures() {
        assert_native_and_reference_case(
            "quoting-and-byte-pipe",
            |_| "printf '%s' 'hello world' | tr a-z A-Z".to_owned(),
            None,
        );
        assert_native_and_reference_case(
            "boolean-short-circuit",
            |_| {
                "sh -c 'printf left; printf left-error >&2; exit 7' && printf no || sh -c 'printf recovered; printf recovered-error >&2'"
                    .to_owned()
            },
            None,
        );
        assert_native_and_reference_case(
            "stderr-and-status",
            |_| "sh -c 'printf error >&2; exit 7'".to_owned(),
            None,
        );
        assert_native_and_reference_case(
            "output-and-append-redirect",
            |fixture| {
                format!(
                    "printf first > {} && printf second >> {}",
                    fixture.path("output"),
                    fixture.path("output")
                )
            },
            Some(b"firstsecond"),
        );
        assert_native_and_reference_case(
            "input-redirect",
            |fixture| format!("cat < {}", fixture.path("input")),
            None,
        );

        let variable = format!(
            "QUIRL_C1_DIFFERENTIAL_{}",
            NEXT_DIFFERENTIAL_FIXTURE.fetch_add(1, Ordering::Relaxed)
        );
        assert_native_and_reference_case(
            "export-assignment",
            |_| format!("export {variable}=value && printenv {variable}"),
            None,
        );
        std::env::remove_var(variable);

        assert_native_and_reference_case(
            "semicolon-list-and-here-string",
            |_| "printf first; cat <<< second".to_owned(),
            None,
        );
        assert_native_and_reference_case(
            "parameter-and-arithmetic-expansion",
            |_| {
                "export QUIRL_C1_EXPANSION=value; printf '%s:%s' $QUIRL_C1_EXPANSION $((1 + 2))"
                    .to_owned()
            },
            None,
        );
        assert_native_and_reference_case(
            "bounded-command-substitution",
            |_| "printf '%s' $(printf nested)".to_owned(),
            None,
        );
        assert_native_and_reference_case(
            "ordered-standard-descriptor-duplication",
            |_| "sh -c 'printf output; printf error >&2' 2>&1".to_owned(),
            None,
        );
    }

    #[test]
    fn seeded_c1_differential_cases_match_available_reference_shells() {
        let seed = differential_seed();
        let cases = differential_setting("QUIRL_TEST_CASES", DEFAULT_DIFFERENTIAL_CASES);
        let mut generator = DifferentialGenerator::new(seed);
        let sources = (0..cases).map(|_| generator.source()).collect::<Vec<_>>();

        for shell in ["bash", "zsh"] {
            if !shell_is_available(shell) {
                eprintln!("skipping {shell} generated differential: executable is unavailable");
                continue;
            }
            for (index, source) in sources.iter().enumerate() {
                let native = NativeExecutor::default().execute_capture(source).unwrap();
                let reference = reference_outcome(shell, source);
                assert_same_outcome(
                    &format!("seed={seed} case={index} shell={shell} source={source}"),
                    &native,
                    &reference,
                );
            }
        }
    }

    #[test]
    fn explicit_dialect_islands_are_routed_without_reparsing_the_body() {
        let (language, body) = interactive_dialect_island("bash { if true; then printf yes; fi; }")
            .expect("bash island is recognized");
        assert_eq!(language, ScriptLanguage::Bash);
        assert_eq!(body, "if true; then printf yes; fi;");
        assert!(matches!(
            interactive_dialect_island("zsh { print value; }"),
            Some((ScriptLanguage::Zsh, "print value;"))
        ));
        assert!(interactive_dialect_island("bash echo value").is_none());
    }

    #[test]
    fn interactive_native_dispatch_inherits_streams_instead_of_capturing() {
        let outcome = execute_command_or_dialect_island(
            &mut NativeExecutor::default(),
            "true",
            ExecutionOutputMode::Interactive,
        )
        .unwrap();
        assert_eq!(outcome.status, 0);
        assert_eq!(outcome.stdout, None);
        assert_eq!(outcome.stderr, None);

        let captured = execute_command_or_dialect_island(
            &mut NativeExecutor::default(),
            "true",
            ExecutionOutputMode::Capture,
        )
        .unwrap();
        assert_eq!(captured.stdout.as_deref(), Some(""));
        assert_eq!(captured.stderr.as_deref(), Some(""));
    }

    #[test]
    fn bash_island_preserves_reference_observable_output_when_available() {
        if !shell_is_available("bash") {
            return;
        }
        let native = execute_command_or_dialect_island(
            &mut NativeExecutor::default(),
            "bash { printf value; printf warning >&2; exit 7; }",
            ExecutionOutputMode::Capture,
        )
        .unwrap();
        let reference = reference_outcome("bash", "printf value; printf warning >&2; exit 7;");
        assert_same_outcome("bash interactive island", &native, &reference);
    }
}
