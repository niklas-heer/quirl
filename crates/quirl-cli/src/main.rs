//! Quirl's command-line composition root and interactive shell executable.

mod agent;
mod ai;
mod ai_bootstrap;
mod assets;
mod author;
mod bounded_file;
mod config;
mod coordination;
mod extension_scheduler;
mod extensions;
mod history;
mod index;
mod intelligence;
mod lsp;
mod lua_worker;
mod mcp;
mod native_catalog;
mod package;
mod pick;
mod platform;
mod plugin;
mod protocol;
mod recovery;
mod script;

use agent::AgentCommand;
use ai::AiCommand;
use ai_bootstrap::{InteractiveAiBootstrap, LocalCompletionRequester};
use assets::AssetsCommand;
use author::{DescribeCommand, DocCommand, NewCommand};
use clap::{Parser, Subcommand, ValueEnum};
use config::ConfigCommand;
use extensions::{
    LuaCompletionAdapter, LuaExtensionHost, merge_installed_catalog_snapshot,
    resolve_installed_plugin_command,
};
use index::IndexCommand;
use lua_worker::LuaWorkerRuntime as LuaRuntime;
use mcp::ServeCommand;
use package::PackageCommand;
use pick::PickCommand;
use platform::{EventsCommand, ViewCommand, WatchCommand};
use plugin::PluginCommand;
use quirl_catalog::{Catalog, CommandSpec, Completion, Effect as CatalogEffect};
use quirl_contract::CommandProposal;
use quirl_core::{
    CommandOutcome, ErrorCode, ExecutionCancellation, ExecutionCleanupState, ExecutionEffect,
    ExecutionEffects, ExecutionInput, ExecutionMode, ExecutionOutcome, ExecutionOutput,
    ExecutionOutputTarget, ExecutionRequest, ExecutionSource, ExecutionStatus, ExtensionAction,
    ExtensionEventData, OutputStream, ProcessRequest, ShellError, StructuredValue,
    escape_json_terminal_controls, escape_terminal_controls, reject_terminal_controls,
};
use quirl_data::{DataEnvelope, DataOutput, DataRenderFormat, DataRuntime};
use quirl_lua::{LuaPolicy, MAX_LUA_SOURCE_BYTES, QuirlConfig, sdk_json, sdk_lua, sdk_markdown};
use quirl_picker::{ItemKind, MAX_PICKER_ITEMS, PickItem, Picker};
use quirl_process::{
    DEFAULT_CAPTURE_BYTES, JobStatus, NativeExecutor, ObservedActivity, OutputObserver,
    change_directory, sandboxed_process_host,
};
use quirl_syntax::{InteractiveLine, Mode, classify, parse_command_list};
use quirl_ui::{
    CatalogLoader, DATA_ITEMS_MAX, DATA_RETAINED_BYTES_MAX, ExtensionCompleter,
    ExtensionSuggestion, InteractiveDataSnapshot, InteractiveEnvironmentSnapshot,
    InteractiveHistoryEntry, InteractiveJobAction, InteractiveJobSnapshot, InteractiveJobStatus,
    InteractivePanelBatch, InteractivePanelProvider, InteractiveRuntimeSnapshot, InteractiveSignal,
    MODE_TOGGLE_HOST_COMMAND, NativeProjectContext, PROMPT_FIRST_PAINT_BUDGET, PickerItem,
    PickerItemKind, PickerMatch, PickerRanker, PromptContextScheduler, QuirlPrompt, RichSurface,
    SurfaceKind, editor_with_extensions_config_history_and_picker, history_path, render_error,
    select_surface, set_product_identity, terminal_supports_nerd_font, terminal_supports_unicode,
    terminal_width,
};
use recovery::RecoveryCommand;
use script::ScriptLanguage;
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex, RwLock, atomic::AtomicBool, mpsc},
    thread::{self, JoinHandle},
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
        /// Explicit script engine; required when standard input has no recognized shebang.
        #[arg(long, value_enum)]
        lang: Option<ScriptLanguage>,
        /// Arguments made available to the selected script engine.
        #[arg(trailing_var_arg = true)]
        arguments: Vec<String>,
    },
    /// Evaluate Lua and print the returned value.
    Eval {
        /// Lua expression or chunk to evaluate under Quirl's runtime policy.
        expression: String,
    },
    /// Evaluate a native structured-data expression or pipeline.
    Data {
        /// Native typed-data expression or pipeline to evaluate.
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
        /// Diagnostic renderer; JSON emits the stable machine envelope.
        #[arg(long, value_enum, default_value_t = DiagnosticFormat::Text)]
        format: DiagnosticFormat,
    },
    /// Deterministically format Lua or native Quirl files under a bounded path traversal.
    Fmt {
        /// Lua or native Quirl script or directory.
        #[arg(value_name = "PATH")]
        file: PathBuf,
        /// Report formatting drift without modifying any file.
        #[arg(long)]
        check: bool,
    },
    /// Lint Lua or native Quirl (.qrl, .quirl, .🌀) scripts without execution.
    Lint {
        /// Script file or recursively discovered directory.
        #[arg(value_name = "PATH")]
        file: PathBuf,
        /// Diagnostic renderer; JSON emits the stable machine envelope.
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
        /// Output representation for the generated host API contract.
        #[arg(long, value_enum, default_value_t = SdkFormat::Text)]
        format: SdkFormat,
    },
    /// Export the semantic command catalog used by completion, docs, and AI.
    Catalog {
        /// Output representation for the installed command catalog.
        #[arg(long, value_enum, default_value_t = CatalogFormat::Json)]
        format: CatalogFormat,
    },
    /// Export deterministic installed context and validation contracts for agents.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Search and index local command intelligence without network inference.
    Ai {
        #[command(subcommand)]
        command: AiCommand,
    },
    /// Inspect, retry, or update separately downloaded runtime assets.
    Assets {
        #[command(subcommand)]
        command: AssetsCommand,
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
        /// Partial command line to complete.
        input: String,
        /// Output representation for ranked completion candidates.
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
        /// Complete Quirl source, passed as one outer-shell argument.
        source: String,
        /// Error renderer; JSON emits the stable ShellError object.
        #[arg(long, value_enum, default_value_t = DiagnosticFormat::Text)]
        format: DiagnosticFormat,
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
    if lua_worker::worker_requested() {
        return match lua_worker::run_worker() {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        };
    }
    let cli = Cli::parse();
    if cli.build_info {
        print_json_value(serde_json::json!({
            "schema_version": 3,
            "version": env!("CARGO_PKG_VERSION"),
            "build_profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "optimization_level": env!("QUIRL_BUILD_OPT_LEVEL"),
            "panic_strategy": if cfg!(panic = "unwind") { "unwind" } else { "abort" },
            "operating_system": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "source_commit": env!("QUIRL_BUILD_COMMIT"),
            "build_timestamp": env!("QUIRL_BUILD_TIMESTAMP"),
            "official_release": env!("QUIRL_OFFICIAL_RELEASE") == "true",
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
        Ok(status) => ExitCode::from(u8::try_from(status.clamp(0, 255)).unwrap_or(u8::MAX)),
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

fn product_build_identity() -> String {
    if env!("QUIRL_OFFICIAL_RELEASE") == "true" {
        return format!("v{}", env!("CARGO_PKG_VERSION"));
    }
    let commit = env!("QUIRL_BUILD_COMMIT");
    let short_commit = commit.get(..7).unwrap_or(commit);
    let dirty = if env!("QUIRL_BUILD_DIRTY") == "true" {
        "*"
    } else {
        ""
    };
    format!(
        "dev@{}+{short_commit}{dirty}",
        env!("QUIRL_BUILD_TIMESTAMP")
    )
}

fn run(cli: Cli) -> Result<i32, ShellError> {
    match cli.command {
        Some(Command::New { command }) => author::create(command),
        Some(Command::Run {
            file,
            lang,
            arguments,
        }) => {
            let (request, _signals) = script::execution_request(&file, lang, &arguments)?;
            let outcome = execute_execution_request(&mut NativeExecutor::default(), request, None)?;
            print_execution_outcome(&outcome)?;
            Ok(outcome.status_code())
        }
        Some(Command::Eval { expression }) => {
            let request = execution_request(
                "<eval>",
                &expression,
                ExecutionMode::Lua,
                ExecutionOutputTarget::Value,
                ExecutionEffects::from_effects(&[ExecutionEffect::SpawnProcess]),
            )?;
            let outcome = execute_execution_request(&mut NativeExecutor::default(), request, None)?;
            print_execution_value(&outcome)?;
            Ok(outcome.status_code())
        }
        Some(Command::Data { expression, format }) => {
            let format = match format {
                DataOutputFormat::Json => DataRenderFormat::Json,
                DataOutputFormat::Plain => DataRenderFormat::Plain,
                DataOutputFormat::Table => DataRenderFormat::Table,
            };
            let request = execution_request(
                "<data>",
                &expression,
                ExecutionMode::Data,
                ExecutionOutputTarget::Value,
                ExecutionEffects::from_effects(&[
                    ExecutionEffect::ReadFilesystem,
                    ExecutionEffect::SpawnProcess,
                ]),
            )?;
            let outcome = execute_execution_request(&mut NativeExecutor::default(), request, None)?;
            render_execution_value(&outcome, format)?;
            Ok(outcome.status_code())
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
            let catalog = load_composed_catalog()?;
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
        Some(Command::Agent { command }) => agent::execute(command, &load_composed_catalog()?),
        Some(Command::Ai {
            command: AiCommand::Run { query },
        }) => execute_natural_command(&query),
        Some(Command::Ai { command }) => ai::execute(command),
        Some(Command::Assets { command }) => assets::execute(command),
        Some(Command::Package { command }) => package::execute(command, &load_composed_catalog()?),
        Some(Command::Describe { command }) => author::describe(command, &load_composed_catalog()?),
        Some(Command::Doc { command }) => author::doc(command, &load_composed_catalog()?),
        Some(Command::Lsp) => lsp::execute(load_composed_catalog()?),
        Some(Command::Serve { command }) => {
            mcp::execute(command, native_catalog::builtin_native_catalog())
        }
        Some(Command::Index { command }) => index::execute(command),
        Some(Command::Complete { input, format }) => {
            let catalog = load_composed_catalog()?;
            let mut extensions = LuaExtensionHost::discover();
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
                CompletionFormat::Text => {
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
        Some(Command::Pick { command }) => pick::execute(command, &load_composed_catalog()?),
        Some(Command::Events { command }) => platform::execute_events(command),
        Some(Command::View { command }) => platform::execute_view(command),
        Some(Command::Watch { command }) => platform::execute_watch(command),
        Some(Command::Recover { command }) => recovery::execute(command),
        Some(Command::Exec { source, .. }) => run_exec_with_recovery(&source),
        None if !io::stdin().is_terminal() => run_stdin(),
        None => {
            let host = LuaExtensionHost::discover();
            repl(Arc::new(Mutex::new(host)))
        }
    }
}

fn execute_natural_command(query: &[String]) -> Result<i32, ShellError> {
    let intent = join_natural_query(query)?;
    let results = index::search_default_database_kind(
        &intent,
        intelligence::SEARCH_RESULTS_MAX,
        intelligence::SearchDocumentKind::Command,
    )?;
    let candidate = natural_command_candidate(&results).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidCommand,
            "natural command retrieval found no catalog command",
        )
        .with_help("Refresh the local index or describe the task with more command-specific words")
    })?;
    let catalog = load_composed_catalog()?;
    let command = catalog
        .commands
        .iter()
        .find(|command| command.path == candidate.command)
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::Validation,
                "the ranked command is absent from the current catalog",
            )
            .with_context(format!("ranked path: {}", candidate.command))
            .with_help("Refresh the command index and retry the natural-language request")
        })?;
    let mut proposal = CommandProposal::retrieval_fallback(
        &catalog,
        command.id.clone(),
        format!("retrieved `{}` for the supplied task", command.path),
        "quirl-bounded-hybrid-retrieval-v1",
    )?;
    let mut input = io::stdin().lock();
    let mut output = io::stderr().lock();
    if !ai::resolve_natural_command_slots(&mut proposal, &catalog, &mut input, &mut output)? {
        writeln!(output, "natural command cancelled").map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                "could not report natural command cancellation",
            )
            .with_context(error.to_string())
            .with_help("Check that standard error is writable")
        })?;
        return Ok(1);
    }
    let validated = proposal.validate(&catalog)?;
    let preview = validated.render_trusted()?;
    let risk = validated.risk();
    let risk_reasons = validated.risk_reasons().to_vec();
    let effects = validated.effects().to_vec();
    if !ai::confirm_natural_command(&preview, risk, &risk_reasons, &mut input, &mut output)? {
        writeln!(output, "natural command cancelled").map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                "could not report natural command cancellation",
            )
            .with_context(error.to_string())
            .with_help("Check that standard error is writable")
        })?;
        return Ok(1);
    }
    drop(output);
    drop(input);

    let current_catalog = load_composed_catalog()?;
    let current = proposal.validate(&current_catalog)?;
    let current_preview = current.render_trusted()?;
    if current_preview != preview
        || current.risk() != risk
        || current.risk_reasons() != risk_reasons
        || current.effects() != effects
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "the catalog command changed after confirmation",
        )
        .with_help("Review the refreshed preview and confirm it again"));
    }
    let request = execution_request(
        "<natural-command>",
        &current_preview,
        ExecutionMode::NativeCommand,
        ExecutionOutputTarget::Inherit,
        natural_execution_effects(current.effects()),
    )?;
    let outcome = execute_execution_request(&mut NativeExecutor::default(), request, None)?;
    print_execution_outcome(&outcome)?;
    Ok(outcome.status_code())
}

fn natural_command_candidate(
    results: &[intelligence::SearchResult],
) -> Option<&intelligence::SearchResult> {
    results
        .iter()
        .find(|result| result.command != "quirl ai run")
}

fn join_natural_query(query: &[String]) -> Result<String, ShellError> {
    let mut bytes = 0_usize;
    for part in query {
        bytes = bytes
            .checked_add(part.len())
            .and_then(|value| value.checked_add(usize::from(bytes > 0)))
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "natural command query byte count overflowed",
                )
                .with_help("Use a shorter natural-language request")
            })?;
        if bytes > intelligence::QUERY_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "natural command query exceeded its byte limit",
            )
            .with_context(format!(
                "limit: {}; observed: {bytes}",
                intelligence::QUERY_BYTES_MAX
            ))
            .with_help("Use a shorter natural-language request"));
        }
    }
    Ok(query.join(" "))
}

fn natural_execution_effects(effects: &[CatalogEffect]) -> ExecutionEffects {
    let mut declared = vec![ExecutionEffect::SpawnProcess];
    declared.extend(effects.iter().map(|effect| match effect {
        CatalogEffect::ReadFilesystem => ExecutionEffect::ReadFilesystem,
        CatalogEffect::WriteFilesystem => ExecutionEffect::WriteFilesystem,
        CatalogEffect::SpawnProcess => ExecutionEffect::SpawnProcess,
        CatalogEffect::ChangeDirectory => ExecutionEffect::ChangeDirectory,
    }));
    ExecutionEffects::from_effects(&declared)
}

fn load_composed_catalog() -> Result<Catalog, ShellError> {
    let mut catalog = index::load_default_catalog();
    merge_installed_catalog_snapshot(&mut catalog)?;
    Ok(catalog)
}

fn load_rich_catalog() -> Result<Arc<Catalog>, ShellError> {
    #[cfg(debug_assertions)]
    catalog_admission_test_hook()?;
    // The rich surface invokes this on its owned worker after flushing the
    // first frame, so bounded discovery delays neither paint nor input.
    index::initialize_interactive_catalog();
    load_composed_catalog().map(Arc::new)
}

#[cfg(debug_assertions)]
fn catalog_admission_test_hook() -> Result<(), ShellError> {
    const TEST_GATE_TIMEOUT: Duration = Duration::from_secs(5);
    if let Some(gate) = std::env::var_os("QUIRL_TEST_CATALOG_GATE") {
        let gate = PathBuf::from(gate);
        let reached = PathBuf::from(format!("{}.reached", gate.display()));
        std::fs::write(&reached, b"first frame flushed").map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not publish catalog test gate marker")
                .with_context(error.to_string())
                .with_help("Use a writable QUIRL_TEST_CATALOG_GATE path")
        })?;
        let started = Instant::now();
        while !gate.exists() {
            if started.elapsed() >= TEST_GATE_TIMEOUT {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "catalog test gate exceeded its bounded wait",
                )
                .with_context(format!(
                    "limit: {} ms; observed: at least {} ms",
                    TEST_GATE_TIMEOUT.as_millis(),
                    started.elapsed().as_millis()
                ))
                .with_help("Release the test gate before its five-second deadline"));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    if std::env::var_os("QUIRL_TEST_CATALOG_FAILURE").is_some() {
        return Err(
            ShellError::new(ErrorCode::Validation, "injected catalog admission failure")
                .with_help("Remove QUIRL_TEST_CATALOG_FAILURE outside restoration tests"),
        );
    }
    Ok(())
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
            Self::Ai { command } => ai::wants_json(command),
            Self::Assets { command } => assets::wants_json(command),
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
            Self::Exec {
                format: DiagnosticFormat::Json,
                ..
            } => true,
            _ => false,
        }
    }
}

// `quirl exec` source-boundary failure model:
//
// - The outer shell owns argv construction and removes only its own quoting. Exec accepts one
//   UTF-8 argv element as complete Quirl source; it never joins, escapes, or reparses multiple
//   argv elements.
// - Quirl quoting, operators, redirects, and empty command arguments are syntax inside that one
//   source element. A second argv element, including `;`, is a Clap error and cannot acquire Quirl
//   syntax during dispatch.
// - The accepted source is passed byte-for-byte to the plan event. Without a rewrite, the same
//   source reaches parsing, diagnostics, and recovery capture; persisted recovery data remains
//   subject to its existing bounds and secret redaction.
// - An explicit extension `RewritePlan` action is the only later source transition. Its replacement
//   becomes the source used by execution, error events, and recovery without another argv boundary.
// - Parse, execution, extension, and recovery failures remain `ShellError` values. The selected
//   renderer changes presentation only and cannot change the accepted or recorded source.
fn run_exec_with_recovery(source: &str) -> Result<i32, ShellError> {
    let journal = recovery::RecoveryJournal::discover()?;
    let extensions = Arc::new(Mutex::new(LuaExtensionHost::discover()));
    let mut executor = NativeExecutor::default();
    begin_extension_session(&extensions, &mut executor);
    emit_directory_snapshot(&extensions, &mut executor);
    execute_with_recovery(
        &mut executor,
        &journal,
        source,
        Some(&extensions),
        ExecutionOutputMode::Capture,
        None,
    )
    .map(|report| report.status)
}

fn execution_request(
    source_name: &str,
    source: &str,
    mode: ExecutionMode,
    output: ExecutionOutputTarget,
    declared_effects: ExecutionEffects,
) -> Result<ExecutionRequest, ShellError> {
    Ok(
        ExecutionRequest::new(ExecutionSource::new(source_name, source)?, mode)
            .with_output(output)
            .with_effects(declared_effects, ExecutionEffects::all()),
    )
}

/// Scoped deadline observer for engines whose public pull API accepts only the
/// shared cancellation flag. The worker is always stopped and joined before
/// outcome publication, so no deadline task can outlive its execution plan.
struct DeadlineCancellationGuard {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    expired: Arc<AtomicBool>,
}

impl DeadlineCancellationGuard {
    fn arm(plan: &quirl_core::ExecutionPlan, stage: &str) -> Result<Self, ShellError> {
        plan.ensure_active(stage)?;
        let remaining = plan.deadline().ensure_remaining(stage)?;
        let cancelled = plan.cancellation().atomic();
        let expired = Arc::new(AtomicBool::new(false));
        let worker_expired = Arc::clone(&expired);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("quirl-execution-deadline".to_owned())
            .spawn(move || {
                if matches!(
                    receiver.recv_timeout(remaining),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) && cancelled
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    worker_expired.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            })
            .map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not start the execution deadline observer",
                )
                .with_context(error.to_string())
                .with_help("Retry the operation; report repeated deadline observer failures")
            })?;
        Ok(Self {
            stop: Some(sender),
            worker: Some(worker),
            expired,
        })
    }

    fn finish<T>(
        mut self,
        result: Result<T, ShellError>,
        plan: &quirl_core::ExecutionPlan,
        stage: &str,
    ) -> Result<T, ShellError> {
        if let Err(cleanup_error) = self.stop_and_join() {
            return match result {
                Err(error) => Err(error.with_context(format!(
                    "deadline observer cleanup also failed: {}",
                    cleanup_error.message
                ))),
                Ok(_) => Err(cleanup_error),
            };
        }
        if self.expired.load(std::sync::atomic::Ordering::Relaxed) {
            return match plan.deadline().ensure_remaining(stage) {
                Err(error) => Err(error),
                Ok(_) => Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "execution deadline observer stopped the operation",
                )
                .with_context(format!("deadline observed {stage}"))
                .with_help("Use a shorter-running operation or increase the execution deadline")),
            };
        }
        if plan.cancellation().is_cancelled() {
            return plan.cancellation().ensure_active(stage).and(result);
        }
        result
    }

    fn stop_and_join(&mut self) -> Result<(), ShellError> {
        self.stop.take();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| {
            ShellError::new(ErrorCode::Io, "execution deadline observer failed")
                .with_help("Retry the operation; report repeated deadline observer failures")
        })
    }
}

impl Drop for DeadlineCancellationGuard {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The sole CLI mode-selection point for executable command, data, Lua, and
/// explicit reference-shell requests. Each branch delegates resource ownership
/// to its existing engine and returns only bounded passive values.
fn execute_execution_request(
    executor: &mut NativeExecutor,
    request: ExecutionRequest,
    extensions: Option<&Arc<Mutex<LuaExtensionHost>>>,
) -> Result<ExecutionOutcome, ShellError> {
    execute_execution_request_streaming(executor, request, extensions, None)
}

fn execute_execution_request_streaming(
    executor: &mut NativeExecutor,
    request: ExecutionRequest,
    extensions: Option<&Arc<Mutex<LuaExtensionHost>>>,
    observer: Option<&mut OutputObserver<'_>>,
) -> Result<ExecutionOutcome, ShellError> {
    let plan = request.plan()?;
    plan.ensure_active("before engine initialization")?;
    if !matches!(plan.input(), ExecutionInput::None) && plan.mode() != ExecutionMode::Plugin {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "this command does not yet accept piped input",
        )
        .with_command(plan.source().text())
        .with_help(
            "Run it without piped input, or use a data/plugin command that supports streaming",
        ));
    }
    let deadline_guard = DeadlineCancellationGuard::arm(&plan, "before mode dispatch")?;
    let result = match plan.mode() {
        ExecutionMode::NativeCommand => execute_native_plan(executor, &plan, observer),
        ExecutionMode::QuirlScript | ExecutionMode::LuaScript => {
            script::execute_plan(executor, &plan)
        }
        ExecutionMode::Data => execute_data_plan(&plan),
        ExecutionMode::Lua => execute_lua_plan(&plan),
        ExecutionMode::Bash | ExecutionMode::Zsh => execute_reference_plan(executor, &plan),
        ExecutionMode::Plugin => {
            let extensions = extensions.ok_or_else(|| {
                ShellError::new(
                    ErrorCode::InvalidCommand,
                    "plugin execution requires the installed extension host",
                )
                .with_command(plan.source().text())
                .with_help("Run the command through `quirl exec` or the interactive shell")
            })?;
            let mut extensions = extensions.lock().map_err(|_| {
                ShellError::new(ErrorCode::Lua, "the extension host lock was poisoned")
                    .with_help("Restart Quirl before executing another plugin command")
            })?;
            extensions.dispatch_plugin_plan(&plan).map_err(|mut error| {
                error.details.command = Some(plan.source().text().to_owned());
                error.with_context(format!("plugin command id: {}", plan.source().name()))
            })
        }
        ExecutionMode::Protocol => Err(ShellError::new(
            ErrorCode::InvalidCommand,
            "the selected execution adapter is not installed",
        )
        .with_command(plan.source().text())
        .with_help("Use a native, data, Lua, Bash, or Zsh front door in this release")),
    };
    let result = deadline_guard.finish(result, &plan, "during engine execution");
    match result {
        Ok(outcome) => {
            plan.ensure_active("before outcome commit")
                .map_err(|error| preserve_execution_source(error, plan.source()))?;
            Ok(outcome)
        }
        Err(error) => Err(preserve_execution_source(error, plan.source())),
    }
}

fn execute_native_plan(
    executor: &mut NativeExecutor,
    plan: &quirl_core::ExecutionPlan,
    observer: Option<&mut OutputObserver<'_>>,
) -> Result<ExecutionOutcome, ShellError> {
    require_declared_effect(plan, ExecutionEffect::SpawnProcess)?;
    let deadline = plan
        .deadline()
        .ensure_remaining("before native process startup")?;
    let request = ProcessRequest {
        command: plan.source().text().to_owned(),
        deadline,
        cancelled: plan.cancellation().atomic(),
        max_output_bytes: match plan.output() {
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream,
            } => max_bytes_per_stream,
            ExecutionOutputTarget::Inherit => 1,
            ExecutionOutputTarget::Value => {
                return Err(representation_error(plan, "native commands return bytes"));
            }
        },
    };
    let outcome = match plan.output() {
        ExecutionOutputTarget::Capture {
            max_bytes_per_stream: _,
        } => match observer {
            Some(observer) => executor.execute_capture_request_streaming(request, observer)?,
            None => executor.execute_capture_request(request)?,
        },
        ExecutionOutputTarget::Inherit => executor.execute_interactive_request(request)?,
        ExecutionOutputTarget::Value => {
            return Err(representation_error(
                plan,
                "native commands cannot produce structured values",
            ));
        }
    };
    let mut normalized = ExecutionOutcome::from_command(outcome, plan.output())?;
    if executor
        .jobs()
        .iter()
        .any(|job| job.status != JobStatus::Done)
    {
        normalized.cleanup = ExecutionCleanupState::RetainedByEngine;
    }
    Ok(normalized)
}

fn execute_data_plan(plan: &quirl_core::ExecutionPlan) -> Result<ExecutionOutcome, ShellError> {
    if plan.output() != ExecutionOutputTarget::Value {
        return Err(representation_error(
            plan,
            "data execution returns structured values",
        ));
    }
    let cancelled = plan.cancellation().atomic();
    let runtime = if plan
        .declared_effects()
        .contains(ExecutionEffect::SpawnProcess)
    {
        DataRuntime::with_process_host(sandboxed_process_host())
    } else {
        DataRuntime::new()
    };
    let value = runtime
        .eval_output_with_cancellation_handle(plan.source().text(), Arc::clone(&cancelled))?
        .into_envelope(&cancelled)?;
    let output = data_envelope_output(value)?;
    plan.ensure_active("after data execution")?;
    ExecutionOutcome::new(
        ExecutionStatus::Exited(0),
        output,
        Vec::new(),
        ExecutionCleanupState::Complete,
    )
}

fn data_envelope_output(envelope: DataEnvelope) -> Result<ExecutionOutput, ShellError> {
    match envelope {
        DataEnvelope::Value { value } => Ok(ExecutionOutput::Value { value }),
        DataEnvelope::Stream { items } => Ok(ExecutionOutput::Values { values: items }),
        DataEnvelope::Option { .. } | DataEnvelope::Result { .. } | DataEnvelope::Task { .. } => {
            Err(ShellError::new(
                ErrorCode::Data,
                "data execution returned a control-flow envelope where a value was required",
            )
            .with_help("Handle Option, Result, or Task before crossing this execution boundary"))
        }
    }
}

fn execute_lua_plan(plan: &quirl_core::ExecutionPlan) -> Result<ExecutionOutcome, ShellError> {
    if plan.output() != ExecutionOutputTarget::Value {
        return Err(representation_error(
            plan,
            "Lua execution returns structured values",
        ));
    }
    let mut policy = LuaPolicy::script();
    policy.wall_time = policy.wall_time.min(
        plan.deadline()
            .ensure_remaining("before Lua initialization")?,
    );
    let runtime = if plan
        .declared_effects()
        .contains(ExecutionEffect::SpawnProcess)
    {
        LuaRuntime::new_with_process_host_and_cancellation(
            policy,
            sandboxed_process_host(),
            plan.cancellation().atomic(),
        )?
    } else {
        LuaRuntime::new_with_cancellation(policy, plan.cancellation().atomic())?
    };
    plan.ensure_active("after Lua initialization")?;
    let value = runtime.eval(plan.source().text())?;
    plan.ensure_active("after Lua execution")?;
    ExecutionOutcome::new(
        ExecutionStatus::Exited(0),
        ExecutionOutput::Value {
            value: StructuredValue::from_json(value),
        },
        Vec::new(),
        ExecutionCleanupState::Complete,
    )
}

fn execute_reference_plan(
    executor: &NativeExecutor,
    plan: &quirl_core::ExecutionPlan,
) -> Result<ExecutionOutcome, ShellError> {
    require_declared_effect(plan, ExecutionEffect::SpawnProcess)?;
    if !matches!(plan.output(), ExecutionOutputTarget::Capture { .. }) {
        return Err(representation_error(
            plan,
            "explicit Bash/Zsh execution requires bounded byte capture",
        ));
    }
    let language = match plan.mode() {
        ExecutionMode::Bash => ScriptLanguage::Bash,
        ExecutionMode::Zsh => ScriptLanguage::Zsh,
        _ => {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "reference adapter received a non-reference execution mode",
            )
            .with_help("Report this execution-plan mismatch as a Quirl defect"));
        }
    };
    let cancellation = script::ScriptCancellation::from_atomic(plan.cancellation().atomic());
    let outcome = script::run_interactive_island(script::InteractiveIslandRequest {
        language,
        source: plan.source().text(),
        source_name: plan.source().name(),
        arguments: plan.arguments(),
        deadline: plan.deadline(),
        max_bytes_per_stream: match plan.output() {
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream,
            } => max_bytes_per_stream,
            ExecutionOutputTarget::Inherit | ExecutionOutputTarget::Value => {
                return Err(representation_error(
                    plan,
                    "explicit Bash/Zsh execution requires bounded byte capture",
                ));
            }
        },
        cancellation: &cancellation,
        executor,
    })?;
    ExecutionOutcome::from_command(outcome, plan.output())
}

fn require_declared_effect(
    plan: &quirl_core::ExecutionPlan,
    effect: ExecutionEffect,
) -> Result<(), ShellError> {
    if plan.declared_effects().contains(effect) {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::Validation,
        "execution plan does not declare required engine authority",
    )
    .with_command(plan.source().text())
    .with_context(format!("required effect: {effect:?}"))
    .with_help("Declare and allow every effect before selecting this execution engine"))
}

fn representation_error(plan: &quirl_core::ExecutionPlan, expected: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "execution output representation is incompatible",
    )
    .with_command(plan.source().text())
    .with_context(expected)
    .with_help("Choose the output representation declared by the selected execution mode")
}

fn preserve_execution_source(mut error: ShellError, source: &ExecutionSource) -> ShellError {
    if error.details.command.is_none() {
        error.details.command = Some(source.text().to_owned());
    }
    for label in &mut error.details.labels {
        if matches!(label.source.as_deref(), Some("command" | "eval")) {
            label.source = Some(source.text().to_owned());
        }
    }
    error
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionOutputMode {
    Capture,
    Interactive,
    RichViewport,
}

#[derive(Debug)]
struct ExecutionReport {
    status: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ExecutionReport {
    fn from_outcome(outcome: ExecutionOutcome) -> Self {
        let status = outcome.status_code();
        let (stdout, stderr) = match outcome.output {
            ExecutionOutput::Bytes { stdout, stderr } => (stdout, stderr),
            ExecutionOutput::Inherited
            | ExecutionOutput::Value { .. }
            | ExecutionOutput::Values { .. } => (Vec::new(), Vec::new()),
        };
        Self {
            status,
            stdout,
            stderr,
        }
    }
}

fn execute_with_recovery(
    executor: &mut NativeExecutor,
    journal: &recovery::RecoveryJournal,
    source: &str,
    extensions: Option<&Arc<Mutex<LuaExtensionHost>>>,
    output_mode: ExecutionOutputMode,
    observer: Option<&mut OutputObserver<'_>>,
) -> Result<ExecutionReport, ShellError> {
    let planned = match extensions {
        Some(extensions) => {
            let plan: Result<PlannedExecution, ShellError> = (|| {
                let plugin_command = resolve_installed_plugin_command(source)?;
                let effects = plugin_command
                    .as_ref()
                    .map(extensions::InstalledPluginCommand::effect_names)
                    .unwrap_or_else(|| vec!["spawn_process".to_owned()]);
                let mut planned = prepare_extension_plan(extensions, source, effects, executor)?;
                planned.plugin_command = if planned.source == source {
                    plugin_command
                } else {
                    resolve_installed_plugin_command(&planned.source)?
                };
                Ok(planned)
            })();
            match plan {
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
                        executor,
                    );
                    print_extension_annotations(&annotations);
                    return Err(error);
                }
            }
        }
        None => PlannedExecution::new(source),
    };
    let recovery_context = journal.capture_context(&planned.source)?;
    let PlannedExecution {
        source,
        plugin_command,
        mut annotations,
    } = planned;
    if let Some(extensions) = extensions {
        quiesce_extension_callbacks(extensions)?;
    }
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
            executor,
        );
    }
    let started = Instant::now();
    let renders_captured_output = output_mode == ExecutionOutputMode::Capture
        || (output_mode == ExecutionOutputMode::Interactive
            && interactive_dialect_island(&source).is_some());
    match execute_command_or_dialect_island_with_extensions(
        executor,
        &source,
        output_mode,
        extensions,
        plugin_command.as_ref(),
        observer,
    ) {
        Ok(outcome) => {
            let duration = started.elapsed();
            let recovery_outcome = command_outcome_projection(&outcome);
            if outcome.status_code() != 0
                && let Err(error) = journal.record_failure(
                    &recovery_context,
                    duration,
                    Some(&recovery_outcome),
                    None,
                )
            {
                eprintln!("warning: {}", render_stderr_error(&error));
            }
            if let Some(extensions) = extensions {
                emit_execution_outcome_events(extensions, &outcome, &mut annotations, executor);
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
                    executor,
                );
                apply_observation_actions(
                    notify_extensions(
                        extensions,
                        ExtensionEventData::Result {
                            status: outcome.status_code(),
                            duration_ms: duration_millis(duration),
                        },
                    ),
                    &mut annotations,
                    executor,
                );
            }
            if renders_captured_output
                || matches!(
                    &outcome.output,
                    ExecutionOutput::Value { .. } | ExecutionOutput::Values { .. }
                )
            {
                print_execution_outcome(&outcome)?;
            }
            print_extension_annotations(&annotations);
            Ok(ExecutionReport::from_outcome(outcome))
        }
        Err(error) => {
            let duration = started.elapsed();
            if let Err(journal_error) =
                journal.record_failure(&recovery_context, duration, None, Some(&error))
            {
                eprintln!("warning: {}", render_stderr_error(&journal_error));
            }
            if let Some(extensions) = extensions {
                notify_execution_error(extensions, &error, &mut annotations, executor);
                print_extension_annotations(&annotations);
            }
            Err(error)
        }
    }
}

fn notify_execution_error(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    error: &ShellError,
    annotations: &mut BTreeMap<String, serde_json::Value>,
    executor: &mut NativeExecutor,
) {
    if let Some(reason) = execution_interruption_reason(error) {
        apply_observation_actions(
            notify_extensions(
                extensions,
                ExtensionEventData::Cancellation {
                    reason: reason.to_owned(),
                },
            ),
            annotations,
            executor,
        );
    }
    apply_observation_actions(
        notify_extensions(
            extensions,
            ExtensionEventData::Error {
                error: error.clone(),
            },
        ),
        annotations,
        executor,
    );
}

fn execution_interruption_reason(error: &ShellError) -> Option<&'static str> {
    if error.code != ErrorCode::ResourceLimit {
        return None;
    }
    let message = error.message.to_ascii_lowercase();
    if message.contains("cancel") {
        Some("execution cancelled")
    } else if message.contains("deadline") || message.contains("wall_time") {
        Some("execution deadline expired")
    } else {
        None
    }
}

#[cfg(test)]
fn execute_command_or_dialect_island(
    executor: &mut NativeExecutor,
    source: &str,
    output_mode: ExecutionOutputMode,
) -> Result<CommandOutcome, ShellError> {
    execute_command_or_dialect_island_with_extensions(
        executor,
        source,
        output_mode,
        None,
        None,
        None,
    )
    .map(|outcome| command_outcome_projection(&outcome))
}

/// Curated executables that always take over the whole terminal — full-screen
/// editors, pagers, and other TUI programs — rather than write a plain
/// stream of output.
///
/// The rich viewport normally captures a foreground command's output and
/// replays it inside its own transcript block, which works well for
/// programs that print a stream of text. A program in this list instead
/// needs direct control of the real terminal: cursor addressing, its own
/// alternate screen, and live keystrokes as the user types them. A captured,
/// replayed transcript cannot provide any of that — the program either
/// blocks on input it never receives, or (like Vim) detects that its output
/// is not a terminal and refuses to draw at all.
const FULL_SCREEN_PROGRAMS: &[&str] = &[
    "vim", "vi", "nvim", "view", "nvi", "emacs", "nano", "pico", "less", "more", "most", "man",
    "top", "htop", "btop", "gotop", "tmux", "screen", "watch", "mc", "ncdu", "fzf", "tig",
    "lazygit", "k9s",
];

/// Return whether `source` is a single foreground external command whose
/// executable is a known [`FULL_SCREEN_PROGRAMS`] entry.
///
/// Deliberately conservative: multi-stage pipelines, boolean or sequential
/// lists, background commands, and commands with redirects all return
/// `false` and fall back to the rich viewport's captured, replayed
/// rendering. A pipeline stage still needs its output captured by the other
/// side of the pipe, and a redirect target is a request the takeover path
/// has no way to honor, so neither is safe to reinterpret as "give this
/// command the real terminal."
fn needs_real_terminal(source: &str) -> bool {
    let Ok(list) = parse_command_list(source) else {
        return false;
    };
    let ([pipeline], []) = (list.pipelines.as_slice(), list.connectors.as_slice()) else {
        return false;
    };
    if pipeline.background {
        return false;
    }
    let [command] = pipeline.commands.as_slice() else {
        return false;
    };
    if !command.redirects.is_empty() {
        return false;
    }
    let Some(executable) = command.words.first() else {
        return false;
    };
    let name = executable.rsplit('/').next().unwrap_or(executable.as_str());
    FULL_SCREEN_PROGRAMS.contains(&name)
}

fn execute_command_or_dialect_island_with_extensions(
    executor: &mut NativeExecutor,
    source: &str,
    output_mode: ExecutionOutputMode,
    extensions: Option<&Arc<Mutex<LuaExtensionHost>>>,
    installed: Option<&extensions::InstalledPluginCommand>,
    observer: Option<&mut OutputObserver<'_>>,
) -> Result<ExecutionOutcome, ShellError> {
    if output_mode == ExecutionOutputMode::RichViewport
        && installed.is_none()
        && interactive_dialect_island(source).is_none()
        && parse_command_list(source)
            .map(|list| list.pipelines.iter().any(|pipeline| pipeline.background))
            .unwrap_or(false)
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "background commands are not available in the rich viewport",
        )
        .with_command(source)
        .with_help(
            "Set ui.surface = \"simple\" for background jobs until rich PTY-backed jobs are available",
        ));
    }
    let request = if let Some((language, body)) = interactive_dialect_island(source) {
        let mode = match language {
            ScriptLanguage::Bash => ExecutionMode::Bash,
            ScriptLanguage::Zsh => ExecutionMode::Zsh,
            ScriptLanguage::Lua | ScriptLanguage::Quirl => {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    "interactive dialect island selected a non-shell language",
                )
                .with_command(source)
                .with_help("Use an explicit Bash or Zsh island"));
            }
        };
        let (mode, engine_source, source_name, target) = (
            mode,
            body,
            "<dialect-island>",
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream: DEFAULT_CAPTURE_BYTES,
            },
        );
        execution_request(
            source_name,
            engine_source,
            mode,
            target,
            ExecutionEffects::all(),
        )?
    } else if let Some(installed) = installed {
        let extensions = extensions.ok_or_else(|| {
            ShellError::new(
                ErrorCode::InvalidCommand,
                "installed plugin command requires an extension host",
            )
            .with_command(source)
            .with_help("Run the command through `quirl exec` or the interactive shell")
        })?;
        let mut extensions = extensions.lock().map_err(|_| {
            ShellError::new(ErrorCode::Lua, "the extension host lock was poisoned")
                .with_help("Restart Quirl before executing another plugin command")
        })?;
        extensions.plugin_execution_request(installed, source, ExecutionInput::None)?
    } else {
        let target = match output_mode {
            ExecutionOutputMode::Capture | ExecutionOutputMode::RichViewport => {
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: DEFAULT_CAPTURE_BYTES,
                }
            }
            ExecutionOutputMode::Interactive => ExecutionOutputTarget::Inherit,
        };
        execution_request(
            "<command>",
            source,
            ExecutionMode::NativeCommand,
            target,
            ExecutionEffects::all(),
        )?
    };
    execute_execution_request_streaming(executor, request, extensions, observer).map_err(
        |mut error| {
            error.details.command = Some(source.to_owned());
            error
        },
    )
}

fn recovery_journal(
    slot: &mut Option<recovery::RecoveryJournal>,
) -> Result<&recovery::RecoveryJournal, ShellError> {
    if slot.is_none() {
        *slot = Some(recovery::RecoveryJournal::discover()?);
    }
    slot.as_ref().ok_or_else(|| {
        ShellError::new(
            ErrorCode::Io,
            "recovery journal initialization did not retain its owner",
        )
        .with_help("Restart Quirl and retry the recovery operation")
    })
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
        let Some(body) = rest
            .strip_prefix('{')
            .and_then(|body| body.strip_suffix('}'))
        else {
            continue;
        };
        return Some((language, body.trim()));
    }
    None
}

const INTERACTIVE_DATA_PULLS_PER_TURN_MAX: usize = 16;
const INTERACTIVE_DATA_OPTION_DEPTH_MAX: usize = 64;

struct InteractiveSignalCancellation {
    cancellation: ExecutionCancellation,
    #[cfg(unix)]
    signal_ids: Vec<signal_hook::SigId>,
}

impl InteractiveSignalCancellation {
    fn install() -> Result<Self, ShellError> {
        let flag = Arc::new(AtomicBool::new(false));
        let cancellation = ExecutionCancellation::from_atomic(Arc::clone(&flag));
        #[cfg(unix)]
        {
            let mut signal_ids = Vec::with_capacity(2);
            for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                match signal_hook::flag::register(signal, Arc::clone(&flag)) {
                    Ok(signal_id) => signal_ids.push(signal_id),
                    Err(error) => {
                        for signal_id in signal_ids {
                            signal_hook::low_level::unregister(signal_id);
                        }
                        return Err(ShellError::new(
                            ErrorCode::Io,
                            "could not install interactive data cancellation handlers",
                        )
                        .with_context(error.to_string())
                        .with_help("Retry after restoring the process signal state"));
                    }
                }
            }
            Ok(Self {
                cancellation,
                signal_ids,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self { cancellation })
        }
    }
}

impl Drop for InteractiveSignalCancellation {
    fn drop(&mut self) {
        #[cfg(unix)]
        for signal_id in self.signal_ids.drain(..) {
            signal_hook::low_level::unregister(signal_id);
        }
    }
}

#[derive(Default)]
struct InteractiveDataCache {
    items: VecDeque<InteractiveDataSnapshot>,
    retained_bytes: usize,
    next_id: u64,
}

struct InteractiveDataStage {
    items: VecDeque<(InteractiveDataSnapshot, usize)>,
    retained_bytes: usize,
    next_id: u64,
}

impl InteractiveDataCache {
    fn stage(&self) -> InteractiveDataStage {
        InteractiveDataStage {
            items: VecDeque::with_capacity(DATA_ITEMS_MAX),
            retained_bytes: 0,
            next_id: self.next_id,
        }
    }

    fn commit(&mut self, stage: InteractiveDataStage) {
        self.next_id = stage.next_id;
        for (item, bytes) in stage.items {
            while self.items.len() == DATA_ITEMS_MAX
                || self.retained_bytes.saturating_add(bytes) > DATA_RETAINED_BYTES_MAX
            {
                let Some(removed) = self.items.pop_front() else {
                    break;
                };
                self.retained_bytes = self
                    .retained_bytes
                    .saturating_sub(interactive_data_snapshot_bytes(&removed));
            }
            if bytes <= DATA_RETAINED_BYTES_MAX {
                self.retained_bytes = self.retained_bytes.saturating_add(bytes);
                self.items.push_back(item);
            }
        }
    }

    fn snapshot(&self) -> Vec<InteractiveDataSnapshot> {
        self.items.iter().cloned().collect()
    }
}

impl InteractiveDataStage {
    fn observe(&mut self, value: &StructuredValue) -> Result<(), ShellError> {
        let id = self.next_id.checked_add(1).ok_or_else(|| {
            ShellError::new(
                ErrorCode::ResourceLimit,
                "interactive data result identity counter was exhausted",
            )
            .with_help("Restart Quirl before retaining another typed picker result")
        })?;
        self.next_id = id;
        let plain = DataEnvelope::value(value.clone()).render(DataRenderFormat::Plain)?;
        let label = plain.trim_end().chars().take(256).collect::<String>();
        let preview = serde_json::to_string_pretty(value).map_err(|error| {
            ShellError::new(
                ErrorCode::Data,
                "could not retain a typed data picker preview",
            )
            .with_context(error.to_string())
            .with_help("Use the scrollback output without retaining this picker item")
        })?;
        let insertion = serde_json::to_string(&value.json_value()).map_err(|error| {
            ShellError::new(ErrorCode::Data, "could not create a data picker insertion")
                .with_context(error.to_string())
                .with_help("Use the original data expression instead of this cached value")
        })?;
        let item = InteractiveDataSnapshot {
            id,
            label,
            preview: Some(truncate_utf8_owned(preview, 4 * 1024)),
            value: value.clone(),
            insertion: truncate_utf8_owned(insertion, 4 * 1024),
        };
        let bytes = interactive_data_snapshot_bytes(&item);
        while self.items.len() == DATA_ITEMS_MAX
            || self.retained_bytes.saturating_add(bytes) > DATA_RETAINED_BYTES_MAX
        {
            let Some((_, removed_bytes)) = self.items.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(removed_bytes);
        }
        if bytes <= DATA_RETAINED_BYTES_MAX {
            self.retained_bytes = self.retained_bytes.saturating_add(bytes);
            self.items.push_back((item, bytes));
        }
        Ok(())
    }
}

fn interactive_data_snapshot_bytes(item: &InteractiveDataSnapshot) -> usize {
    serde_json::to_vec(&item.value)
        .map(|value| value.len())
        .unwrap_or(DATA_RETAINED_BYTES_MAX.saturating_add(1))
        .saturating_add(item.label.len())
        .saturating_add(item.preview.as_ref().map_or(0, String::len))
        .saturating_add(item.insertion.len())
}

fn truncate_utf8_owned(mut value: String, bytes_max: usize) -> String {
    if value.len() <= bytes_max {
        return value;
    }
    let mut end = bytes_max;
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
    value
}

fn execute_interactive_data(
    source: &str,
    cache: &mut InteractiveDataCache,
    writer: &mut impl Write,
) -> Result<(ExecutionOutcome, u64), ShellError> {
    let signals = InteractiveSignalCancellation::install()?;
    let request = execution_request(
        "<interactive-data>",
        source,
        ExecutionMode::Data,
        ExecutionOutputTarget::Inherit,
        ExecutionEffects::all(),
    )?
    .with_cancellation(signals.cancellation.clone());
    let plan = request.plan()?;
    let deadline_guard =
        DeadlineCancellationGuard::arm(&plan, "before interactive data initialization")?;
    let result = (|| {
        let cancelled = plan.cancellation().atomic();
        let output = DataRuntime::with_process_host(sandboxed_process_host())
            .eval_output_with_cancellation_handle(plan.source().text(), Arc::clone(&cancelled))?;
        let mut stage = cache.stage();
        let bytes = render_interactive_data_output(output, &cancelled, writer, &mut stage)?;
        let outcome = ExecutionOutcome::new(
            ExecutionStatus::Exited(0),
            ExecutionOutput::Inherited,
            Vec::new(),
            ExecutionCleanupState::Complete,
        )?;
        Ok((outcome, bytes, stage))
    })();
    let (outcome, bytes, stage) =
        deadline_guard.finish(result, &plan, "during interactive data execution")?;
    plan.ensure_active("before interactive data cache commit")?;
    cache.commit(stage);
    Ok((outcome, bytes))
}

const INTERACTIVE_TRANSCRIPT_OUTPUT_BYTES_MAX: usize = DEFAULT_CAPTURE_BYTES;

struct BoundedTranscriptWriter {
    bytes: Vec<u8>,
    discarded_bytes: u64,
}

impl BoundedTranscriptWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(INTERACTIVE_TRANSCRIPT_OUTPUT_BYTES_MAX),
            discarded_bytes: 0,
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.discarded_bytes > 0 {
            let marker = format!(
                "\n… output truncated after {INTERACTIVE_TRANSCRIPT_OUTPUT_BYTES_MAX} bytes; discarded {} bytes …\n",
                self.discarded_bytes
            );
            let marker = marker.as_bytes();
            let retained_bytes =
                INTERACTIVE_TRANSCRIPT_OUTPUT_BYTES_MAX.saturating_sub(marker.len());
            self.bytes.truncate(retained_bytes);
            self.bytes.extend_from_slice(marker);
        }
        self.bytes
    }
}

impl Write for BoundedTranscriptWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = INTERACTIVE_TRANSCRIPT_OUTPUT_BYTES_MAX.saturating_sub(self.bytes.len());
        let retained = remaining.min(buffer.len());
        let retained_bytes = buffer
            .get(..retained)
            .ok_or_else(|| io::Error::other("bounded transcript retained an invalid byte count"))?;
        self.bytes.extend_from_slice(retained_bytes);
        self.discarded_bytes = self.discarded_bytes.saturating_add(
            u64::try_from(buffer.len().saturating_sub(retained)).unwrap_or(u64::MAX),
        );
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn render_interactive_data_output(
    mut output: DataOutput,
    cancelled: &AtomicBool,
    writer: &mut impl Write,
    stage: &mut InteractiveDataStage,
) -> Result<u64, ShellError> {
    let mut option_depth = 0_usize;
    while let DataOutput::Option(Some(inner)) = output {
        option_depth = option_depth
            .checked_add(1)
            .ok_or_else(|| interactive_data_option_depth_error(usize::MAX))?;
        if option_depth > INTERACTIVE_DATA_OPTION_DEPTH_MAX {
            return Err(interactive_data_option_depth_error(option_depth));
        }
        output = *inner;
    }

    let mut bytes = 0_u64;
    for _ in 0..option_depth {
        bytes = bytes.saturating_add(write_interactive_data_bytes(b"some(\n", cancelled, writer)?);
    }
    bytes = bytes.saturating_add(match output {
        DataOutput::Value(value) => write_interactive_data_value(&value, cancelled, writer, stage),
        DataOutput::Stream(mut stream) => {
            let mut stream_bytes = 0_u64;
            let mut pulls = 0_usize;
            while let Some(value) = stream.next(cancelled)? {
                stream_bytes = stream_bytes.saturating_add(write_interactive_data_value(
                    &value, cancelled, writer, stage,
                )?);
                pulls = pulls.saturating_add(1);
                if pulls == INTERACTIVE_DATA_PULLS_PER_TURN_MAX {
                    pulls = 0;
                    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err(ShellError::new(
                            ErrorCode::ResourceLimit,
                            "interactive data rendering was cancelled",
                        )
                        .with_help("Retry the expression after the cancellation is clear"));
                    }
                    std::thread::yield_now();
                }
            }
            Ok(stream_bytes)
        }
        DataOutput::Option(None) => write_interactive_data_bytes(b"none\n", cancelled, writer),
        DataOutput::Option(Some(_)) => {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "interactive data output retained an unflattened nested option",
            )
            .with_help("Report this data-output state as a Quirl defect"));
        }
    }?);
    for _ in 0..option_depth {
        bytes = bytes.saturating_add(write_interactive_data_bytes(b")\n", cancelled, writer)?);
    }
    Ok(bytes)
}

fn interactive_data_option_depth_error(observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "interactive data option nesting exceeded its configured limit",
    )
    .with_context(format!(
        "limit: {INTERACTIVE_DATA_OPTION_DEPTH_MAX}; observed: {observed}"
    ))
    .with_help("Reduce nested optional transforms before rendering this expression")
}

fn write_interactive_data_value(
    value: &StructuredValue,
    cancelled: &AtomicBool,
    writer: &mut impl Write,
    stage: &mut InteractiveDataStage,
) -> Result<u64, ShellError> {
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "interactive data rendering was cancelled",
        )
        .with_help("Retry the expression after the cancellation is clear"));
    }
    let rendered = DataEnvelope::value(value.clone()).render(DataRenderFormat::Plain)?;
    writer
        .write_all(rendered.as_bytes())
        .map_err(data_output_write_error)?;
    writer.flush().map_err(data_output_write_error)?;
    stage.observe(value)?;
    Ok(u64::try_from(rendered.len()).unwrap_or(u64::MAX))
}

fn write_interactive_data_bytes(
    bytes: &[u8],
    cancelled: &AtomicBool,
    writer: &mut impl Write,
) -> Result<u64, ShellError> {
    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "interactive data rendering was cancelled",
        )
        .with_help("Retry the expression after the cancellation is clear"));
    }
    writer.write_all(bytes).map_err(data_output_write_error)?;
    writer.flush().map_err(data_output_write_error)?;
    Ok(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
}

fn data_output_write_error(error: io::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "cannot write interactive data output")
        .with_context(error.to_string())
        .with_help("Check that standard output is writable before retrying the expression")
}

fn interactive_job_snapshots(jobs: &[quirl_process::JobState]) -> Vec<InteractiveJobSnapshot> {
    jobs.iter()
        .filter_map(|job| {
            let status = match job.status {
                JobStatus::Running => InteractiveJobStatus::Running,
                JobStatus::Stopped => InteractiveJobStatus::Stopped,
                JobStatus::Done => return None,
            };
            let actions = match status {
                InteractiveJobStatus::Running => vec![InteractiveJobAction::Foreground],
                InteractiveJobStatus::Stopped => vec![
                    InteractiveJobAction::Foreground,
                    InteractiveJobAction::Background,
                ],
            };
            Some(InteractiveJobSnapshot {
                id: job.id,
                status,
                command: job.command.clone(),
                actions,
            })
        })
        .collect()
}

fn interactive_environment_snapshot(
    executor: &NativeExecutor,
) -> Result<Vec<InteractiveEnvironmentSnapshot>, ShellError> {
    Ok(executor
        .environment_snapshot()?
        .into_iter()
        .map(|(name, value)| InteractiveEnvironmentSnapshot {
            name: name.to_string_lossy().into_owned(),
            value: value.to_string_lossy().into_owned(),
        })
        .collect())
}

fn repl(extensions: Arc<Mutex<LuaExtensionHost>>) -> Result<i32, ShellError> {
    set_product_identity(&product_build_identity())?;
    let mut executor = NativeExecutor::default();
    let runtime_extensions_present = extensions
        .lock()
        .map(|extensions| extensions.has_runtime_extensions())
        .unwrap_or(false);
    if runtime_extensions_present {
        begin_extension_session(&extensions, &mut executor);
        emit_directory_snapshot(&extensions, &mut executor);
    }
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
    let mut history_database = history::HistoryDatabase::open_default(&history_path)?;
    let ai_bootstrap = InteractiveAiBootstrap::new();
    let (mut line_editor, mut catalog) = configured_initial_editor(
        &extensions,
        &ai_bootstrap,
        active_config.clone(),
        &history_path,
    )?;
    print_banner(&active_config);
    // Scheduling returns immediately; the guard only joins after cancellation
    // during shell shutdown, so no download can delay the first prompt.
    let _asset_refresh = assets::schedule_background_update();
    let _periodic_asset_refresh =
        assets::schedule_periodic_update(ai_bootstrap.asset_update_signal());
    let mut mode = Mode::Command;
    // Recovery snapshots are only needed once a native command is accepted.
    // Environment capture can be deferred past the first interactive paint.
    let mut recovery = None;
    let mut data_cache = InteractiveDataCache::default();
    let mut runtime_snapshot_generation = 0_u64;
    let mut installed_environment_generation = None;
    // Script evaluation remains lazy. Extension VMs load before the first editor
    // view, but first paint reads only their bounded prompt cache; Lua refreshes
    // on the fixed worker pool after the snapshot is returned.
    let mut lua = None;
    let mut last_status = 0;
    let mut last_duration: Option<Duration> = None;
    // The welcome text is the cold first paint. Start developer discovery only
    // after it is visible, then let the rich editor adopt the complete result
    // on a later bounded poll without requiring the user to submit a line.
    let prompt_environment_generation = executor.environment_generation();
    let prompt_probe = Arc::new(RwLock::new(executor.developer_context_probe()));
    let worker_probe = Arc::clone(&prompt_probe);
    let prompt_scheduler = Arc::new(PromptContextScheduler::with_project_context_loader(
        PROMPT_FIRST_PAINT_BUDGET,
        move |cwd| {
            let probe = worker_probe
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let snapshot = probe.probe(cwd);
            NativeProjectContext {
                git_branch: snapshot.git_branch,
                git_state: snapshot.git_state,
                rust_version: snapshot.rust_version,
            }
        },
    ));
    let _ = prompt_scheduler.sample_current_dir();
    let mut prompt_context = (
        prompt_scheduler,
        prompt_probe,
        prompt_environment_generation,
    );
    let mut first_prompt = true;
    loop {
        if !first_prompt {
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
                    &mut executor,
                );
                print_extension_annotations(&annotations);
                observed_directory = current_directory;
            }
        }
        let (extension_segments, next_config) = extensions
            .lock()
            .map(|mut extensions| {
                // `active_config` already loaded and fingerprinted the first
                // extension generation. Rich catalog admission happens inside
                // the first editor session after its initial flush, so avoid a
                // second bounded extension snapshot before that first paint.
                if !first_prompt {
                    extensions.reload_if_changed();
                }
                let revision = extensions.config_revision();
                let next_config = (revision != applied_revision)
                    .then(|| (extensions.active_config().clone(), revision));
                let segments: Vec<_> = if extensions.has_runtime_extensions() {
                    extensions
                        .named_prompt_segments(mode, last_status)
                        .into_iter()
                        .map(|segment| (segment.name, segment.value))
                        .collect()
                } else {
                    Vec::new()
                };
                (segments, next_config)
            })
            .unwrap_or_default();
        if let Some((config, revision)) = next_config {
            applied_revision = revision;
            if config != active_config {
                sync_history(&mut line_editor, &history_path)?;
                active_config = config;
                let published_catalog = catalog.as_ref().ok_or_else(|| {
                    ShellError::new(
                        ErrorCode::Io,
                        "interactive catalog was not published before reconfiguration",
                    )
                    .with_help("Restart Quirl to create a fresh interactive session")
                })?;
                line_editor = configured_editor(
                    published_catalog,
                    &extensions,
                    &ai_bootstrap,
                    active_config.clone(),
                    &history_path,
                )?;
            }
        }
        if ai_bootstrap.take_catalog_changed() {
            // Cache publication is complete before this flag is set. Adopt it
            // only between editor turns, when no terminal handoff, callback,
            // or input buffer is partially committed.
            sync_history(&mut line_editor, &history_path)?;
            let refreshed_catalog = Arc::new(load_composed_catalog()?);
            if !line_editor.replace_catalog(Arc::clone(&refreshed_catalog)) {
                line_editor = configured_editor(
                    &refreshed_catalog,
                    &extensions,
                    &ai_bootstrap,
                    active_config.clone(),
                    &history_path,
                )?;
            }
            catalog = Some(refreshed_catalog);
            ai_bootstrap.request_reindex();
        }
        print_extension_errors(&extensions);
        let job_states = executor.jobs();
        let active_jobs = job_states
            .iter()
            .filter(|job| job.status != JobStatus::Done)
            .count();
        let mut prompt = QuirlPrompt::with_config(mode, &active_config)
            .with_status(last_status)
            .with_jobs(active_jobs)
            .with_named_extension_segments(extension_segments);
        {
            let (scheduler, probe, environment_generation) = &mut prompt_context;
            let current_generation = executor.environment_generation();
            if current_generation != *environment_generation {
                let mut probe = probe
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *probe = executor.developer_context_probe();
                *environment_generation = current_generation;
            }
            let prompt_directory = std::env::current_dir().unwrap_or_default();
            let sample = scheduler.sample(&prompt_directory);
            let cached_scheduler = Arc::clone(scheduler);
            prompt = prompt
                .with_native_context(sample.context)
                .with_native_context_provider(move || {
                    cached_scheduler.cached_context(&prompt_directory)
                });
        }
        if let Some(duration) = last_duration {
            prompt = prompt.with_duration(duration);
        }
        runtime_snapshot_generation =
            runtime_snapshot_generation.checked_add(1).ok_or_else(|| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "interactive runtime snapshot generation counter was exhausted",
                )
                .with_help("Restart Quirl before preparing another interactive prompt")
            })?;
        let environment_generation = executor.environment_generation();
        let environment = if installed_environment_generation == Some(environment_generation) {
            None
        } else {
            Some(interactive_environment_snapshot(&executor)?)
        };
        line_editor.install_runtime_snapshot(InteractiveRuntimeSnapshot {
            generation: runtime_snapshot_generation,
            jobs: interactive_job_snapshots(&job_states),
            data: data_cache.snapshot(),
            environment,
        });
        installed_environment_generation = Some(environment_generation);
        let history_directory = std::env::current_dir().unwrap_or_default();
        line_editor.install_history_snapshot(history_database.snapshot(&history_directory, mode)?);
        let signal = line_editor.read_line(&mut prompt);
        // Rich Alt-Q transitions happen while the surface retains terminal
        // ownership. Adopt the mode rendered by that session before classifying
        // accepted input; degraded editors leave the prompt mode unchanged and
        // continue to report their host command below.
        mode = prompt.mode();
        if signal.is_ok() && catalog.is_none() {
            catalog = line_editor.published_catalog();
        }
        first_prompt = false;
        // Admission already owns the single catalog worker. Accepted input is
        // only an additional rescan hint; idle discovery began after first
        // paint and does not depend on reaching this prompt boundary.
        ai_bootstrap.request_catalog_refresh();
        match signal {
            Ok(InteractiveSignal::Success(buffer)) => {
                sync_history(&mut line_editor, &history_path)?;
                quiesce_extension_callbacks(&extensions)?;
                let interactive_line = classify(mode, &buffer);
                match interactive_line {
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
                    InteractiveLine::Help(topic) => {
                        let published_catalog = catalog.as_deref().ok_or_else(|| {
                            ShellError::new(
                                ErrorCode::Io,
                                "interactive catalog publication is incomplete",
                            )
                            .with_help("Restart Quirl before requesting help again")
                        })?;
                        print_help(published_catalog, topic);
                    }
                    InteractiveLine::Command(command) => {
                        let started = Instant::now();
                        let journal = recovery_journal(&mut recovery)?;
                        let output_mode = line_editor.command_output_mode();
                        // A known full-screen program (an editor, pager, or
                        // similar) cannot work through the rich viewport's
                        // captured, replayed rendering: it needs the real
                        // terminal for its own alternate screen, cursor
                        // addressing, and live keystrokes. Route it through
                        // the same inherited-stdio path the simple surface
                        // always uses, and hand it the real terminal for the
                        // duration of the call.
                        let takeover = output_mode == ExecutionOutputMode::RichViewport
                            && needs_real_terminal(command);
                        let execution_output_mode = if takeover {
                            ExecutionOutputMode::Interactive
                        } else {
                            output_mode
                        };
                        let streaming = line_editor.begin_command_stream(command, &prompt)?;
                        if takeover {
                            line_editor.release_terminal_for_takeover()?;
                        }
                        let mut streamed_any = false;
                        let execution = if streaming {
                            let mut observer = |activity: ObservedActivity<'_>| match activity {
                                ObservedActivity::Output { stream, bytes } => {
                                    streamed_any |= !bytes.is_empty();
                                    line_editor.append_command_stream(stream, bytes, &prompt)
                                }
                                ObservedActivity::Tick => {
                                    line_editor.tick_command_stream(started, &prompt)
                                }
                            };
                            execute_with_recovery(
                                &mut executor,
                                journal,
                                command,
                                Some(&extensions),
                                execution_output_mode,
                                Some(&mut observer),
                            )
                        } else {
                            execute_with_recovery(
                                &mut executor,
                                journal,
                                command,
                                Some(&extensions),
                                execution_output_mode,
                                None,
                            )
                        };
                        if takeover {
                            // Reacquire the rich viewport even if the child
                            // failed to start; the terminal must never stay
                            // stranded on the takeover's (possibly partial)
                            // frame.
                            line_editor.resume_after_terminal_takeover(&prompt)?;
                        }
                        let transcript_result = match execution {
                            Ok(report) => {
                                last_status = report.status;
                                if streaming {
                                    if !streamed_any {
                                        for chunk in report.stdout.chunks(8 * 1024) {
                                            line_editor.append_command_stream(
                                                OutputStream::Stdout,
                                                chunk,
                                                &prompt,
                                            )?;
                                        }
                                        for chunk in report.stderr.chunks(8 * 1024) {
                                            line_editor.append_command_stream(
                                                OutputStream::Stderr,
                                                chunk,
                                                &prompt,
                                            )?;
                                        }
                                    }
                                    line_editor.finish_command_stream(
                                        report.status,
                                        started.elapsed(),
                                        &prompt,
                                    )
                                } else {
                                    line_editor.append_command_transcript(
                                        command,
                                        &report.stdout,
                                        &report.stderr,
                                        report.status,
                                        started.elapsed(),
                                    )
                                }
                            }
                            Err(error) => {
                                last_status = 1;
                                if streaming {
                                    let rendered = render_error(&error, false);
                                    for chunk in rendered.as_bytes().chunks(8 * 1024) {
                                        line_editor.append_command_stream(
                                            OutputStream::Stderr,
                                            chunk,
                                            &prompt,
                                        )?;
                                    }
                                    line_editor.finish_command_stream(
                                        last_status,
                                        started.elapsed(),
                                        &prompt,
                                    )
                                } else {
                                    line_editor.append_command_error(
                                        command,
                                        &error,
                                        started.elapsed(),
                                    )
                                }
                            }
                        };
                        let elapsed = started.elapsed();
                        last_duration = Some(elapsed);
                        history_database.record(
                            command,
                            &history_directory,
                            mode,
                            last_status,
                            Some(elapsed),
                        )?;
                        transcript_result?;
                    }
                    InteractiveLine::Data(source) => {
                        let planned = match prepare_extension_plan(
                            &extensions,
                            source,
                            Vec::new(),
                            &mut executor,
                        ) {
                            Ok(planned) => planned,
                            Err(error) => {
                                last_status = 1;
                                line_editor.append_command_error(source, &error, Duration::ZERO)?;
                                history_database.record(
                                    source,
                                    &history_directory,
                                    mode,
                                    last_status,
                                    None,
                                )?;
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
                            &mut executor,
                        );
                        let started = Instant::now();
                        let rich_output = line_editor.is_rich();
                        let mut captured_output = BoundedTranscriptWriter::new();
                        let data_result = if rich_output {
                            execute_interactive_data(
                                &planned.source,
                                &mut data_cache,
                                &mut captured_output,
                            )
                        } else {
                            let mut stdout = io::stdout().lock();
                            execute_interactive_data(&planned.source, &mut data_cache, &mut stdout)
                        };
                        match data_result {
                            Ok((outcome, output_bytes)) => {
                                last_status = 0;
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::Output {
                                            stream: OutputStream::Stdout,
                                            bytes: usize::try_from(output_bytes)
                                                .unwrap_or(usize::MAX),
                                            text: None,
                                        },
                                    ),
                                    &mut annotations,
                                    &mut executor,
                                );
                                debug_assert_eq!(outcome.status_code(), 0);
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
                                    &mut executor,
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
                                    &mut executor,
                                );
                                print_extension_annotations(&annotations);
                                if rich_output {
                                    line_editor.append_command_transcript(
                                        source,
                                        &captured_output.finish(),
                                        &[],
                                        0,
                                        started.elapsed(),
                                    )?;
                                }
                            }
                            Err(error) => {
                                last_status = 1;
                                notify_execution_error(
                                    &extensions,
                                    &error,
                                    &mut annotations,
                                    &mut executor,
                                );
                                print_extension_annotations(&annotations);
                                line_editor.append_command_error(
                                    source,
                                    &error,
                                    started.elapsed(),
                                )?;
                            }
                        }
                        let elapsed = started.elapsed();
                        last_duration = Some(elapsed);
                        history_database.record(
                            source,
                            &history_directory,
                            mode,
                            last_status,
                            Some(elapsed),
                        )?;
                    }
                    InteractiveLine::Natural(query) => {
                        let started = Instant::now();
                        match index::search_default_database(query, 8) {
                            Ok(results) => {
                                last_status = i32::from(results.is_empty());
                                let stdout = ai::render_results_text(&results).into_bytes();
                                let mut stderr = Vec::new();
                                if results.is_empty() {
                                    stderr.extend_from_slice(
                                        b"no matching commands; refresh with `quirl index build`\n",
                                    );
                                }
                                line_editor.emit_output(
                                    query,
                                    &stdout,
                                    &stderr,
                                    last_status,
                                    started.elapsed(),
                                )?;
                            }
                            Err(error) => {
                                last_status = 1;
                                line_editor.append_command_error(
                                    query,
                                    &error,
                                    started.elapsed(),
                                )?;
                            }
                        }
                        let elapsed = started.elapsed();
                        last_duration = Some(elapsed);
                        history_database.record(
                            query,
                            &history_directory,
                            mode,
                            last_status,
                            Some(elapsed),
                        )?;
                    }
                    InteractiveLine::Lua(source) => {
                        let planned = match prepare_extension_plan(
                            &extensions,
                            source,
                            Vec::new(),
                            &mut executor,
                        ) {
                            Ok(planned) => planned,
                            Err(error) => {
                                last_status = 1;
                                line_editor.append_command_error(source, &error, Duration::ZERO)?;
                                history_database.record(
                                    source,
                                    &history_directory,
                                    mode,
                                    last_status,
                                    None,
                                )?;
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
                            &mut executor,
                        );
                        let started = Instant::now();
                        match eval_lua(&mut lua, &planned.source) {
                            Ok(outcome) => {
                                last_status = outcome.status_code();
                                if let Some(value) = execution_value_json(&outcome) {
                                    emit_value_output(
                                        &extensions,
                                        &value,
                                        &mut annotations,
                                        &mut executor,
                                    );
                                }
                                let (stdout, stderr) = execution_outcome_bytes(&outcome)?;
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
                                    &mut executor,
                                );
                                apply_observation_actions(
                                    notify_extensions(
                                        &extensions,
                                        ExtensionEventData::Result {
                                            status: outcome.status_code(),
                                            duration_ms: duration_millis(started.elapsed()),
                                        },
                                    ),
                                    &mut annotations,
                                    &mut executor,
                                );
                                print_extension_annotations(&annotations);
                                line_editor.emit_output(
                                    source,
                                    &stdout,
                                    &stderr,
                                    last_status,
                                    started.elapsed(),
                                )?;
                            }
                            Err(error) => {
                                last_status = 1;
                                notify_execution_error(
                                    &extensions,
                                    &error,
                                    &mut annotations,
                                    &mut executor,
                                );
                                print_extension_annotations(&annotations);
                                line_editor.append_command_error(
                                    source,
                                    &error,
                                    started.elapsed(),
                                )?;
                            }
                        }
                        let elapsed = started.elapsed();
                        last_duration = Some(elapsed);
                        history_database.record(
                            source,
                            &history_directory,
                            mode,
                            last_status,
                            Some(elapsed),
                        )?;
                    }
                }
            }
            Ok(InteractiveSignal::ChangeDirectory {
                path,
                buffer,
                cursor,
            }) => {
                line_editor.restore_input(buffer, cursor)?;
                if let Err(error) = change_directory(&path) {
                    last_status = 1;
                    line_editor.append_command_error(
                        "directory explorer",
                        &error,
                        Duration::ZERO,
                    )?;
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
                    &mut executor,
                );
                line_editor.emit_output(
                    "^C",
                    &[],
                    b"interactive input cancelled\n",
                    last_status,
                    Duration::ZERO,
                )?;
            }
            Ok(InteractiveSignal::CtrlD) => return Ok(last_status),
            Ok(InteractiveSignal::HostCommand(command)) if command == MODE_TOGGLE_HOST_COMMAND => {
                mode = mode.toggled();
                print_mode_feedback(mode, &active_config);
            }
            Ok(InteractiveSignal::Suspend) => suspend_self()?,
            Ok(_) => {}
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug)]
struct PlannedExecution {
    source: String,
    plugin_command: Option<extensions::InstalledPluginCommand>,
    annotations: BTreeMap<String, serde_json::Value>,
}

impl PlannedExecution {
    fn new(source: &str) -> Self {
        Self {
            source: source.to_owned(),
            plugin_command: None,
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
        .map(|mut extensions| extensions.dispatch_event(event).unwrap_or_default())
        .unwrap_or_default()
}

fn notify_extensions_required(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    event: ExtensionEventData,
) -> Result<Vec<ExtensionAction>, ShellError> {
    let mut extensions = extensions.lock().map_err(|_| {
        ShellError::new(ErrorCode::Lua, "the extension host lock was poisoned")
            .with_help("Restart Quirl before executing another command")
    })?;
    extensions.dispatch_event(event)
}

fn quiesce_extension_callbacks(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
) -> Result<(), ShellError> {
    let quiescence = {
        let mut extensions = extensions.lock().map_err(|_| {
            ShellError::new(ErrorCode::Lua, "the extension host lock was poisoned")
                .with_help("Restart Quirl before executing another command")
        })?;
        extensions.begin_callback_quiescence()
    };
    quiescence.wait()
}

fn begin_extension_session(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    executor: &mut NativeExecutor,
) {
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
        executor,
    );
    if let Some(session_id) = restored_session {
        apply_observation_actions(
            notify_extensions(
                extensions,
                ExtensionEventData::SessionRestore { session_id },
            ),
            &mut annotations,
            executor,
        );
    }
    print_extension_annotations(&annotations);
}

fn emit_directory_snapshot(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    executor: &mut NativeExecutor,
) {
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
        executor,
    );
    print_extension_annotations(&annotations);
}

fn prepare_extension_plan(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    source: &str,
    effects: Vec<String>,
    executor: &mut NativeExecutor,
) -> Result<PlannedExecution, ShellError> {
    let mut planned = PlannedExecution::new(source);
    apply_plan_actions(
        notify_extensions_required(
            extensions,
            ExtensionEventData::CommandPlan {
                source: source.to_owned(),
                effects,
            },
        )?,
        &mut planned,
        executor,
    )?;
    Ok(planned)
}

fn apply_plan_actions(
    actions: Vec<ExtensionAction>,
    planned: &mut PlannedExecution,
    executor: &mut NativeExecutor,
) -> Result<(), ShellError> {
    let mut environment_updates = Vec::new();
    for action in actions {
        match action {
            ExtensionAction::Diagnose { message } => {
                eprintln!("extension: {}", terminal_safe_extension_text(&message));
            }
            ExtensionAction::RewritePlan { source } => planned.source = source,
            ExtensionAction::SetEnvironment { name, value } => {
                environment_updates.push((name, value));
            }
            ExtensionAction::BlockExecution { reason } => {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    "an extension blocked execution",
                )
                .with_context(reason)
                .with_help("Review the extension policy or disable the blocking plugin"));
            }
            ExtensionAction::AnnotateResult { key, value } => {
                planned.annotations.insert(key, value);
            }
        }
    }
    executor.set_environment_variables(&environment_updates)?;
    Ok(())
}

fn apply_observation_actions(
    actions: Vec<ExtensionAction>,
    annotations: &mut BTreeMap<String, serde_json::Value>,
    executor: &mut NativeExecutor,
) {
    let mut environment_updates = Vec::new();
    for action in actions {
        match action {
            ExtensionAction::Diagnose { message } => {
                eprintln!("extension: {}", terminal_safe_extension_text(&message));
            }
            ExtensionAction::SetEnvironment { name, value } => {
                environment_updates.push((name, value));
            }
            ExtensionAction::AnnotateResult { key, value } => {
                annotations.insert(key, value);
            }
            ExtensionAction::RewritePlan { .. } | ExtensionAction::BlockExecution { .. } => {
                eprintln!("extension: plan mutation was ignored after execution began");
            }
        }
    }
    if let Err(error) = executor.set_environment_variables(&environment_updates) {
        eprintln!("extension: {}", render_stderr_error(&error));
    }
}

fn emit_outcome_events(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    outcome: &CommandOutcome,
    annotations: &mut BTreeMap<String, serde_json::Value>,
    executor: &mut NativeExecutor,
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
                executor,
            );
        }
    }
}

fn emit_execution_outcome_events(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    outcome: &ExecutionOutcome,
    annotations: &mut BTreeMap<String, serde_json::Value>,
    executor: &mut NativeExecutor,
) {
    match &outcome.output {
        ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => {
            emit_outcome_events(
                extensions,
                &command_outcome_projection(outcome),
                annotations,
                executor,
            );
        }
        ExecutionOutput::Value { value } => {
            emit_value_output(extensions, &value.json_value(), annotations, executor);
        }
        ExecutionOutput::Values { values } => {
            let values =
                serde_json::Value::Array(values.iter().map(StructuredValue::json_value).collect());
            emit_value_output(extensions, &values, annotations, executor);
        }
    }
}

fn command_outcome_projection(outcome: &ExecutionOutcome) -> CommandOutcome {
    let (stdout, stderr) = match &outcome.output {
        ExecutionOutput::Bytes { stdout, stderr } => (
            Some(String::from_utf8_lossy(stdout).into_owned()),
            Some(String::from_utf8_lossy(stderr).into_owned()),
        ),
        ExecutionOutput::Inherited
        | ExecutionOutput::Value { .. }
        | ExecutionOutput::Values { .. } => (None, None),
    };
    CommandOutcome {
        status: outcome.status_code(),
        stdout,
        stderr,
    }
}

fn print_execution_outcome(outcome: &ExecutionOutcome) -> Result<(), ShellError> {
    match &outcome.output {
        ExecutionOutput::Inherited => Ok(()),
        ExecutionOutput::Bytes { .. } => {
            print_outcome(&command_outcome_projection(outcome));
            Ok(())
        }
        ExecutionOutput::Value { .. } | ExecutionOutput::Values { .. } => {
            print_execution_value(outcome)
        }
    }
}

fn execution_outcome_bytes(outcome: &ExecutionOutcome) -> Result<(Vec<u8>, Vec<u8>), ShellError> {
    match &outcome.output {
        ExecutionOutput::Inherited => Ok((Vec::new(), Vec::new())),
        ExecutionOutput::Bytes { stdout, stderr } => Ok((stdout.clone(), stderr.clone())),
        ExecutionOutput::Value { value } => Ok((json_value_bytes(value.json_value()), Vec::new())),
        ExecutionOutput::Values { values } => Ok((
            json_value_bytes(serde_json::Value::Array(
                values.iter().map(StructuredValue::json_value).collect(),
            )),
            Vec::new(),
        )),
    }
}

fn json_value_bytes(value: serde_json::Value) -> Vec<u8> {
    let rendered = match value {
        serde_json::Value::Null => return Vec::new(),
        serde_json::Value::String(value) => escape_terminal_controls(&value),
        value if value.is_object() || value.is_array() => serde_json::to_string_pretty(&value)
            .map(|json| escape_json_terminal_controls(&json))
            .unwrap_or_else(|_| "<unprintable Lua value>".to_owned()),
        value => value.to_string(),
    };
    let mut bytes = rendered.into_bytes();
    bytes.push(b'\n');
    bytes
}

fn emit_value_output(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    value: &serde_json::Value,
    annotations: &mut BTreeMap<String, serde_json::Value>,
    executor: &mut NativeExecutor,
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
        executor,
    );
}

fn safe_extension_output_text(text: &str) -> Option<String> {
    (text.len() <= extensions::MAX_EXTENSION_EVENT_BYTES
        && reject_terminal_controls("extension output", text).is_ok())
    .then(|| text.to_owned())
}

fn terminal_safe_extension_text(text: &str) -> String {
    escape_terminal_controls(text)
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

struct CachedPanelAdapter {
    extensions: Arc<Mutex<LuaExtensionHost>>,
    observed_generation: Option<u64>,
}

impl CachedPanelAdapter {
    fn new(extensions: Arc<Mutex<LuaExtensionHost>>) -> Self {
        Self {
            extensions,
            observed_generation: None,
        }
    }
}

impl InteractivePanelProvider for CachedPanelAdapter {
    fn poll_cached(&mut self) -> Result<Option<InteractivePanelBatch>, ShellError> {
        let mut extensions = self.extensions.lock().map_err(|_| {
            ShellError::new(
                ErrorCode::Lua,
                "the extension panel cache lock was poisoned",
            )
            .with_help("Restart Quirl before displaying extension panels again")
        })?;
        let snapshot = extensions.cached_panel_snapshot();
        if self.observed_generation == Some(snapshot.generation) {
            return Ok(None);
        }
        self.observed_generation = Some(snapshot.generation);
        Ok(Some(snapshot))
    }
}

enum SessionEditor {
    Rich(Box<RichSurface>),
    Simple(Box<reedline::Reedline>),
}

impl SessionEditor {
    fn read_line(&mut self, prompt: &mut QuirlPrompt) -> Result<InteractiveSignal, ShellError> {
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

    fn install_runtime_snapshot(&mut self, snapshot: InteractiveRuntimeSnapshot) {
        if let Self::Rich(editor) = self {
            editor.install_runtime_snapshot(snapshot);
        }
    }

    fn restore_input(&mut self, buffer: String, cursor: usize) -> Result<(), ShellError> {
        match self {
            Self::Rich(editor) => editor.restore_input(buffer, cursor),
            Self::Simple(_) => Err(ShellError::new(
                ErrorCode::Validation,
                "the simple editor cannot restore a rich modal buffer",
            )
            .with_help("Retry the directory change from the rich surface")),
        }
    }

    fn install_history_snapshot(&mut self, history: Vec<InteractiveHistoryEntry>) {
        if let Self::Rich(editor) = self {
            editor.install_history_snapshot(history);
        }
    }

    fn command_output_mode(&self) -> ExecutionOutputMode {
        match self {
            Self::Rich(_) => ExecutionOutputMode::RichViewport,
            Self::Simple(_) => ExecutionOutputMode::Interactive,
        }
    }

    fn is_rich(&self) -> bool {
        matches!(self, Self::Rich(_))
    }

    fn emit_output(
        &mut self,
        command: &str,
        stdout: &[u8],
        stderr: &[u8],
        status: i32,
        duration: Duration,
    ) -> Result<(), ShellError> {
        match self {
            Self::Rich(editor) => {
                editor.append_transcript(command, stdout, stderr, status, duration)
            }
            Self::Simple(_) => {
                io::stdout().lock().write_all(stdout).map_err(|error| {
                    ShellError::new(ErrorCode::Io, "could not write interactive output")
                        .with_context(error.to_string())
                        .with_help("Check that standard output is still available")
                })?;
                io::stderr().lock().write_all(stderr).map_err(|error| {
                    ShellError::new(ErrorCode::Io, "could not write interactive error output")
                        .with_context(error.to_string())
                        .with_help("Check that standard error is still available")
                })?;
                Ok(())
            }
        }
    }

    fn append_command_transcript(
        &mut self,
        command: &str,
        stdout: &[u8],
        stderr: &[u8],
        status: i32,
        duration: Duration,
    ) -> Result<(), ShellError> {
        if let Self::Rich(editor) = self {
            return editor.append_transcript(command, stdout, stderr, status, duration);
        }
        Ok(())
    }

    fn begin_command_stream(
        &mut self,
        command: &str,
        prompt: &QuirlPrompt,
    ) -> Result<bool, ShellError> {
        let Self::Rich(editor) = self else {
            return Ok(false);
        };
        editor.begin_command_stream(command, prompt)?;
        Ok(true)
    }

    /// Release the terminal for a full-screen foreground child.
    ///
    /// A no-op for the simple surface, which never captures a foreground
    /// child's terminal in the first place.
    fn release_terminal_for_takeover(&mut self) -> Result<(), ShellError> {
        if let Self::Rich(editor) = self {
            editor.release_terminal_for_takeover()?;
        }
        Ok(())
    }

    /// Reacquire the terminal after [`Self::release_terminal_for_takeover`].
    fn resume_after_terminal_takeover(&mut self, prompt: &QuirlPrompt) -> Result<(), ShellError> {
        if let Self::Rich(editor) = self {
            editor.resume_after_terminal_takeover(prompt)?;
        }
        Ok(())
    }

    fn append_command_stream(
        &mut self,
        stream: OutputStream,
        bytes: &[u8],
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        if let Self::Rich(editor) = self {
            editor.append_command_stream(stream, bytes, prompt)?;
        }
        Ok(())
    }

    /// Refresh the running-command spinner and elapsed time.
    ///
    /// A no-op for the simple surface, which inherits the real terminal
    /// directly and has no viewport of its own to animate.
    fn tick_command_stream(
        &mut self,
        started: Instant,
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        if let Self::Rich(editor) = self {
            editor.tick_command_stream(started.elapsed(), prompt)?;
        }
        Ok(())
    }

    fn finish_command_stream(
        &mut self,
        status: i32,
        duration: Duration,
        prompt: &QuirlPrompt,
    ) -> Result<(), ShellError> {
        if let Self::Rich(editor) = self {
            editor.finish_command_stream(status, duration, prompt)?;
        }
        Ok(())
    }

    fn append_command_error(
        &mut self,
        command: &str,
        error: &ShellError,
        duration: Duration,
    ) -> Result<(), ShellError> {
        match self {
            Self::Rich(editor) => editor.append_transcript(
                command,
                &[],
                render_error(error, false).as_bytes(),
                1,
                duration,
            ),
            Self::Simple(_) => {
                eprintln!("{}", render_stderr_error(error));
                Ok(())
            }
        }
    }

    fn published_catalog(&self) -> Option<Arc<Catalog>> {
        match self {
            Self::Rich(editor) => editor.published_catalog(),
            Self::Simple(_) => None,
        }
    }

    fn replace_catalog(&mut self, catalog: Arc<Catalog>) -> bool {
        if let Self::Rich(editor) = self {
            editor.replace_catalog(catalog);
            return true;
        }
        false
    }
}

fn configured_initial_editor(
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    ai_bootstrap: &InteractiveAiBootstrap,
    config: QuirlConfig,
    history_path: &Path,
) -> Result<(SessionEditor, Option<Arc<Catalog>>), ShellError> {
    if select_surface(&config.ui.surface) == SurfaceKind::Rich {
        let completion_adapter = LocalAwareCompletionAdapter::new(
            Arc::clone(extensions),
            ai_bootstrap.local_completion_requester(),
        );
        let picker_ranker: Arc<dyn PickerRanker> = Arc::new(SharedPickerRanker);
        let loader: CatalogLoader = Box::new(load_rich_catalog);
        let mut editor = RichSurface::new_deferred(
            loader,
            Some(Box::new(completion_adapter)),
            picker_ranker,
            &config,
            history_path.to_path_buf(),
        )?;
        editor.set_panel_provider(Box::new(CachedPanelAdapter::new(Arc::clone(extensions))));
        editor.set_activity_provider(Box::new(ai_bootstrap.activity_provider()));
        editor.set_intent_completer(Box::new(AiIntentCompleter::default()));
        return Ok((SessionEditor::Rich(Box::new(editor)), None));
    }

    // The simple/degraded editor requires its catalog during construction and
    // remains intentionally eager; only the rich first-frame path is deferred.
    index::initialize_interactive_catalog();
    let catalog = Arc::new(load_composed_catalog()?);
    ai_bootstrap.catalog_admitted();
    let editor = configured_editor(&catalog, extensions, ai_bootstrap, config, history_path)?;
    Ok((editor, Some(catalog)))
}

fn configured_editor(
    catalog: &Arc<Catalog>,
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    ai_bootstrap: &InteractiveAiBootstrap,
    config: QuirlConfig,
    history_path: &Path,
) -> Result<SessionEditor, ShellError> {
    let completion_adapter = LocalAwareCompletionAdapter::new(
        Arc::clone(extensions),
        ai_bootstrap.local_completion_requester(),
    );
    let picker_ranker: Arc<dyn PickerRanker> = Arc::new(SharedPickerRanker);
    if select_surface(&config.ui.surface) == SurfaceKind::Rich {
        return RichSurface::new(
            Arc::clone(catalog),
            Some(Box::new(completion_adapter)),
            Arc::clone(&picker_ranker),
            &config,
            history_path.to_path_buf(),
        )
        .map(|mut editor| {
            editor.set_panel_provider(Box::new(CachedPanelAdapter::new(Arc::clone(extensions))));
            editor.set_activity_provider(Box::new(ai_bootstrap.activity_provider()));
            editor.set_intent_completer(Box::new(AiIntentCompleter::default()));
            SessionEditor::Rich(Box::new(editor))
        });
    }
    editor_with_extensions_config_history_and_picker(
        catalog.as_ref().clone(),
        Some(Box::new(completion_adapter)),
        config,
        history_path.to_path_buf(),
        picker_ranker,
    )
    .map(|editor| SessionEditor::Simple(Box::new(editor)))
}

#[derive(Debug)]
struct SharedPickerRanker;

struct LocalAwareCompletionAdapter {
    lua: LuaCompletionAdapter,
    local: LocalCompletionRequester,
}

impl LocalAwareCompletionAdapter {
    fn new(extensions: Arc<Mutex<LuaExtensionHost>>, local: LocalCompletionRequester) -> Self {
        Self {
            lua: LuaCompletionAdapter::new(extensions),
            local,
        }
    }
}

impl ExtensionCompleter for LocalAwareCompletionAdapter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<ExtensionSuggestion> {
        self.local.request(line, pos);
        self.lua.complete(line, pos)
    }
}

#[derive(Default)]
struct AiIntentCompleter {
    search: Option<intelligence::SearchSession>,
}

impl ExtensionCompleter for AiIntentCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<ExtensionSuggestion> {
        let Some(query) = line
            .get(..pos)
            .map(str::trim)
            .filter(|query| !query.is_empty())
        else {
            return Vec::new();
        };
        if self.search.is_none() {
            let Ok(search) = index::open_default_search_session() else {
                return Vec::new();
            };
            self.search = Some(search);
        }
        let Some(search) = &self.search else {
            return Vec::new();
        };
        search
            .search(query, 8)
            .unwrap_or_default()
            .into_iter()
            .map(|result| ExtensionSuggestion {
                value: result.target.clone(),
                display: result.target,
                summary: result.summary.clone(),
                detail: format!(
                    "{} · {} · score {:.3}: {}",
                    if result.semantic {
                        "semantic"
                    } else {
                        "lexical"
                    },
                    result.kind,
                    result.score,
                    result.summary
                ),
                replace_start: 0,
                replace_end: pos,
            })
            .collect()
    }
}

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
        let mut ranked = Picker.rank(&shared, query);
        ranked.sort_by(|left, right| {
            let left_score = left
                .score
                .saturating_add(items.get(left.index).map_or(0, |item| item.rank_bias));
            let right_score = right
                .score
                .saturating_add(items.get(right.index).map_or(0, |item| item.rank_bias));
            right_score
                .cmp(&left_score)
                .then_with(|| left.index.cmp(&right.index))
        });
        ranked
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
    let nerd_font = config.prompt.symbols == "nerd_font"
        || (config.prompt.symbols == "auto" && terminal_supports_nerd_font());
    let banner = if config.editor.banner == "compact" {
        compact_welcome(terminal_width().unwrap_or(80), unicode)
    } else {
        onboarding_banner(terminal_width().unwrap_or(80), unicode, nerd_font)
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

fn onboarding_banner(width: u16, unicode: bool, nerd_font: bool) -> String {
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
                "NORMAL: processes/bytes",
                "DATA: typed values/tables",
                "Alt-Q Quirl",
            ]
            .join(separator),
        );
        lines.push(
            [
                "Tab semantic completion",
                "Ctrl-R history",
                "Alt-Q f files",
                "Alt-Q p actions",
                "F1 help",
            ]
            .join(separator),
        );
        lines.push(if nerd_font {
            "Nerd Font icons: enabled".to_owned()
        } else {
            "Nerd Font prompt: set prompt.symbols = \"nerd_font\" in config.lua".to_owned()
        });
        lines.push("Type help to explore commands.".to_owned());
    } else if width >= 64 {
        lines.push(["NORMAL: processes/bytes", "DATA: typed values"].join(separator));
        lines.push(
            [
                "Alt-Q Quirl",
                "Tab semantic completion",
                "Up/Ctrl-R history",
            ]
            .join(separator),
        );
        lines.push(["Alt-Q f files", "Alt-Q p actions", "F1 help"].join(separator));
        lines.push(if nerd_font {
            "Nerd Font icons: enabled".to_owned()
        } else {
            "Nerd Font prompt: prompt.symbols = \"nerd_font\"".to_owned()
        });
    } else {
        lines.push("Processes + typed data".to_owned());
        lines.push(["Alt-Q Quirl", "Tab complete"].join(separator));
        lines.push(["Ctrl-R history", "Alt-Q f files"].join(separator));
        lines.push(["Alt-Q p actions", "F1 help"].join(separator));
        if nerd_font {
            lines.push("Nerd Font icons: enabled".to_owned());
        } else {
            lines.push("Nerd Font prompt:".to_owned());
            lines.push("  prompt.symbols = \"nerd_font\"".to_owned());
        }
    }
    lines.join("\n")
}

fn compact_welcome(width: u16, unicode: bool) -> String {
    let separator = if unicode { " · " } else { " | " };
    let line = [
        format!("Quirl {}", env!("CARGO_PKG_VERSION")),
        "COMMAND/DATA".to_owned(),
        "Tab complete".to_owned(),
        "Alt-Q Quirl".to_owned(),
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
    for hint in ["Alt-Q Quirl", "Tab complete", "Up/Ctrl-R history", "help"] {
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
        (true, Mode::Natural) => (" → ", "local command and option suggestions"),
        (false, Mode::Command) => (": ", "processes and byte pipelines"),
        (false, Mode::Data) => (": ", "typed values and data pipelines"),
        (false, Mode::Natural) => (": ", "local command and option suggestions"),
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
) -> Result<ExecutionOutcome, ShellError> {
    if let Some(runtime) = runtime.as_ref() {
        runtime.clear_cancellation();
    }
    let cancellation = runtime.as_ref().map_or_else(
        ExecutionCancellation::default,
        LuaRuntime::execution_cancellation,
    );
    let request = execution_request(
        "<interactive-lua>",
        source,
        ExecutionMode::Lua,
        ExecutionOutputTarget::Value,
        ExecutionEffects::from_effects(&[ExecutionEffect::SpawnProcess]),
    )?
    .with_cancellation(cancellation);
    let plan = request.plan()?;
    let deadline_guard =
        DeadlineCancellationGuard::arm(&plan, "before interactive Lua initialization")?;
    let result = (|| {
        if runtime.is_none() {
            let mut policy = LuaPolicy::script();
            policy.wall_time = policy.wall_time.min(
                plan.deadline()
                    .ensure_remaining("before interactive Lua VM construction")?,
            );
            *runtime = Some(LuaRuntime::new_with_process_host_and_cancellation(
                policy,
                sandboxed_process_host(),
                plan.cancellation().atomic(),
            )?);
        }
        let runtime = runtime.as_ref().ok_or_else(|| {
            ShellError::new(ErrorCode::Lua, "could not initialize the Lua runtime").with_help(
                "Run the command again; if this persists, report the configuration used.",
            )
        })?;
        plan.ensure_active("before interactive Lua evaluation")?;
        let value = runtime.eval(plan.source().text())?;
        plan.ensure_active("after interactive Lua evaluation")?;
        ExecutionOutcome::new(
            ExecutionStatus::Exited(0),
            ExecutionOutput::Value {
                value: StructuredValue::from_json(value),
            },
            Vec::new(),
            ExecutionCleanupState::Complete,
        )
    })();
    let outcome = deadline_guard.finish(result, &plan, "during interactive Lua execution")?;
    plan.ensure_active("before interactive Lua outcome commit")?;
    Ok(outcome)
}

fn run_stdin() -> Result<i32, ShellError> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(u64::try_from(MAX_LUA_SOURCE_BYTES.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read standard input")
                .with_context(error.to_string())
                .with_help(
                    "Check that standard input is connected and not closed by the calling shell",
                )
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
    let request = execution_request(
        "<stdin>",
        &source,
        ExecutionMode::Lua,
        ExecutionOutputTarget::Value,
        ExecutionEffects::from_effects(&[ExecutionEffect::SpawnProcess]),
    )?;
    let outcome = execute_execution_request(&mut NativeExecutor::default(), request, None)?;
    print_execution_value(&outcome)?;
    Ok(outcome.status_code())
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

fn print_execution_value(outcome: &ExecutionOutcome) -> Result<(), ShellError> {
    match &outcome.output {
        ExecutionOutput::Value { value } => {
            print_json_value(value.json_value());
            Ok(())
        }
        ExecutionOutput::Inherited
        | ExecutionOutput::Bytes { .. }
        | ExecutionOutput::Values { .. } => Err(ShellError::new(
            ErrorCode::Validation,
            "execution outcome did not contain a structured value",
        )
        .with_help("Report this as an execution adapter representation defect")),
    }
}

fn execution_value_json(outcome: &ExecutionOutcome) -> Option<serde_json::Value> {
    match &outcome.output {
        ExecutionOutput::Value { value } => Some(value.json_value()),
        ExecutionOutput::Values { values } => Some(serde_json::Value::Array(
            values.iter().map(StructuredValue::json_value).collect(),
        )),
        ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => None,
    }
}

fn render_execution_value(
    outcome: &ExecutionOutcome,
    format: DataRenderFormat,
) -> Result<(), ShellError> {
    match &outcome.output {
        ExecutionOutput::Value { value } => {
            print!(
                "{}",
                DataEnvelope::Value {
                    value: value.clone()
                }
                .render(format)?
            );
            Ok(())
        }
        ExecutionOutput::Values { values } => {
            print!(
                "{}",
                DataEnvelope::Stream {
                    items: values.clone()
                }
                .render(format)?
            );
            Ok(())
        }
        ExecutionOutput::Inherited | ExecutionOutput::Bytes { .. } => Err(ShellError::new(
            ErrorCode::Validation,
            "data execution outcome did not contain a structured value",
        )
        .with_help("Report this as an execution adapter representation defect")),
    }
}

fn print_outcome(outcome: &quirl_core::CommandOutcome) {
    if let Some(stdout) = &outcome.stdout
        && !stdout.is_empty()
    {
        println!("{stdout}");
    }
    if let Some(stderr) = &outcome.stderr
        && !stderr.is_empty()
    {
        eprintln!("{stderr}");
    }
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not produce JSON")
        .with_context(error.to_string())
        .with_help("Retry without --format json, or report this if it persists")
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

    #[test]
    fn natural_command_never_selects_its_own_composition_entrypoint() {
        let bytes = intelligence::encode_database(&Catalog::builtin(), None).unwrap();
        let results = intelligence::search_kind(
            &bytes,
            Path::new("catalog.sqlite3"),
            "show the current working directory",
            intelligence::SEARCH_RESULTS_MAX,
            None,
            intelligence::SearchDocumentKind::Command,
        )
        .unwrap();
        assert_eq!(results[0].command, "quirl ai run");
        assert_ne!(
            natural_command_candidate(&results).unwrap().command,
            "quirl ai run"
        );
    }

    #[test]
    fn needs_real_terminal_matches_only_plain_full_screen_invocations() {
        assert!(needs_real_terminal("vim"));
        assert!(needs_real_terminal("vim notes.txt"));
        assert!(needs_real_terminal("less README.md"));
        assert!(needs_real_terminal("/usr/bin/vim notes.txt"));

        // Not a full-screen program at all.
        assert!(!needs_real_terminal("git push"));
        assert!(!needs_real_terminal("ls -la"));

        // A pipeline or redirect still needs its side of the byte stream
        // captured, so neither is safe to reinterpret as a terminal handoff.
        assert!(!needs_real_terminal("git log | less"));
        assert!(!needs_real_terminal("vim > out.txt"));

        // A backgrounded full-screen program cannot own the terminal either.
        assert!(!needs_real_terminal("vim &"));

        // A boolean or sequential list is not a single foreground command.
        assert!(!needs_real_terminal("true; vim"));

        // Invalid native syntax must not panic the heuristic.
        assert!(!needs_real_terminal("vim '"));
    }

    #[test]
    fn natural_native_execution_always_declares_process_authority() {
        let unknown = natural_execution_effects(&[]);
        assert!(unknown.contains(ExecutionEffect::SpawnProcess));
        assert!(!unknown.contains(ExecutionEffect::WriteFilesystem));

        let write = natural_execution_effects(&[CatalogEffect::WriteFilesystem]);
        assert!(write.contains(ExecutionEffect::SpawnProcess));
        assert!(write.contains(ExecutionEffect::WriteFilesystem));
    }

    #[test]
    fn shared_picker_prefers_same_directory_history() {
        let item = |id: &str, rank_bias| PickerItem {
            id: id.to_owned(),
            kind: PickerItemKind::History,
            label: "git status".to_owned(),
            description: "history".to_owned(),
            preview: None,
            value: id.to_owned(),
            rank_bias,
        };
        let matches = SharedPickerRanker.rank(&[item("remote", 0), item("local", 4_000)], "git", 2);
        assert_eq!(matches[0].index, 1);
    }

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
            usize::try_from(
                self.next()
                    .checked_rem(u64::try_from(upper_exclusive).unwrap())
                    .expect("generator bounds must be nonzero"),
            )
            .unwrap()
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
            let banner = onboarding_banner(width, unicode, false);
            assert!(banner.contains("Alt-Q Quirl"));
            assert!(banner.contains("semantic completion") || banner.contains("Tab complete"));
            assert!(banner.contains("Ctrl-R history"));
            assert!(banner.contains("Alt-Q f files"));
            assert!(banner.contains("Alt-Q p actions"));
            assert!(banner.contains("prompt.symbols = \"nerd_font\""));
            assert!(!banner.contains('\u{1b}'));
            assert!(
                banner
                    .lines()
                    .all(|line| UnicodeWidthStr::width(line) <= usize::from(width))
            );
            if !unicode {
                assert!(banner.is_ascii());
            }
        }
    }

    #[test]
    fn onboarding_has_a_display_width_safe_minimal_layout() {
        for width in [1, 2, 4, 8, 16, 31] {
            let banner = onboarding_banner(width, true, false);
            assert!(!banner.is_empty());
            assert!(
                banner
                    .lines()
                    .all(|line| UnicodeWidthStr::width(line) <= usize::from(width))
            );
            assert!(!banner.contains('\u{1b}'));
        }
    }

    #[test]
    fn onboarding_reports_an_enabled_nerd_font_profile_without_setup_advice() {
        let banner = onboarding_banner(96, true, true);
        assert!(banner.contains("Nerd Font icons: enabled"));
        assert!(!banner.contains("prompt.symbols"));
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
            "mode: normal - processes and byte pipelines"
        );
        assert_eq!(
            mode_feedback(Mode::Data, true),
            "mode → data · typed values and data pipelines"
        );
        assert_eq!(
            mode_feedback(Mode::Natural, true),
            "mode → ai · local command and option suggestions"
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
    fn exec_accepts_exactly_one_complete_source_operand() {
        let source = r#"printf '<%s>|<%s>' 'hello world' ''"#;
        let cli = Cli::try_parse_from(["quirl", "exec", source]).unwrap();
        assert!(!cli.wants_json());
        assert!(matches!(
            cli.command,
            Some(Command::Exec {
                source: accepted,
                format: DiagnosticFormat::Text,
            }) if accepted == source
        ));

        let json = Cli::try_parse_from(["quirl", "exec", source, "--format", "json"]).unwrap();
        assert!(json.wants_json());
        assert!(matches!(
            json.command,
            Some(Command::Exec {
                source: accepted,
                format: DiagnosticFormat::Json,
            }) if accepted == source
        ));

        assert!(Cli::try_parse_from(["quirl", "exec", source, ";"]).is_err());
        assert!(Cli::try_parse_from(["quirl", "exec", "printf", "hello world"]).is_err());
    }

    #[test]
    fn exec_source_preserves_spaces_empty_arguments_quotes_and_backslashes() {
        let source = r#"printf '<%s>|<%s>|<%s>' 'hello world' '' 'quote\"\slash'"#;
        let outcome = NativeExecutor::default().execute_capture(source).unwrap();
        assert_eq!(outcome.status, 0);
        assert_eq!(
            outcome.stdout.as_deref(),
            Some(r#"<hello world>|<>|<quote\"\slash>"#)
        );
    }

    #[test]
    fn exec_source_intentionally_parses_operators_and_redirects() {
        let fixture = DifferentialFixture::create("exec-source-operators");
        let source = format!(
            "printf '%s\\n' first second | grep second > {}; cat {}",
            fixture.path("output"),
            fixture.path("output")
        );
        let cli = Cli::try_parse_from(["quirl", "exec", source.as_str()]).unwrap();
        let accepted = match cli.command {
            Some(Command::Exec { source, .. }) => source,
            _ => panic!("exec arguments must parse as an exec command"),
        };
        assert_eq!(accepted, source);

        let outcome = NativeExecutor::default()
            .execute_capture(&accepted)
            .unwrap();
        assert_eq!(outcome.status, 0);
        assert_eq!(outcome.stdout.as_deref(), Some("second\n"));
        assert_eq!(
            fixture.redirected_output().as_deref(),
            Some(&b"second\n"[..])
        );
    }

    #[test]
    fn exec_text_and_json_errors_retain_the_exact_source() {
        let source = "printf 'unterminated\\path";
        let error = execute_command_or_dialect_island(
            &mut NativeExecutor::default(),
            source,
            ExecutionOutputMode::Capture,
        )
        .unwrap_err();
        assert_eq!(error.details.command.as_deref(), Some(source));
        assert_eq!(error.details.labels[0].source.as_deref(), Some(source));
        assert!(!error.details.help.is_empty());

        let text = render_error(&error, false);
        assert!(text.contains(source));
        assert!(text.contains("help"));

        let json = serde_json::to_string(&error).unwrap();
        let decoded: ShellError = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.details.command.as_deref(), Some(source));
        assert_eq!(decoded.details.help, error.details.help);
    }

    #[test]
    fn rich_viewport_execution_returns_bounded_stdout_and_stderr() {
        let outcome = execute_command_or_dialect_island_with_extensions(
            &mut NativeExecutor::default(),
            "sh -c 'printf viewport-out; printf viewport-error >&2; exit 7'",
            ExecutionOutputMode::RichViewport,
            None,
            None,
            None,
        )
        .unwrap();
        let report = ExecutionReport::from_outcome(outcome);

        assert_eq!(report.status, 7);
        assert_eq!(report.stdout, b"viewport-out");
        assert_eq!(report.stderr, b"viewport-error");
    }

    #[test]
    fn inherited_execution_report_never_claims_to_have_captured_bytes() {
        let request = execution_request(
            "<inherit-report>",
            "true",
            ExecutionMode::NativeCommand,
            ExecutionOutputTarget::Inherit,
            ExecutionEffects::all(),
        )
        .unwrap();
        let outcome =
            execute_execution_request(&mut NativeExecutor::default(), request, None).unwrap();
        let report = ExecutionReport::from_outcome(outcome);

        assert_eq!(report.status, 0);
        assert!(report.stdout.is_empty());
        assert!(report.stderr.is_empty());
    }

    #[test]
    fn shared_execution_contract_preserves_mode_output_and_status() {
        let cases = [
            (
                ExecutionMode::NativeCommand,
                "false",
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: DEFAULT_CAPTURE_BYTES,
                },
                1,
            ),
            (
                ExecutionMode::Data,
                "[1,2,3] | length",
                ExecutionOutputTarget::Value,
                0,
            ),
            (
                ExecutionMode::Lua,
                "return 42",
                ExecutionOutputTarget::Value,
                0,
            ),
        ];
        for (mode, source, output, status) in cases {
            let request = execution_request(
                "<contract-test>",
                source,
                mode,
                output,
                ExecutionEffects::all(),
            )
            .unwrap();
            let outcome =
                execute_execution_request(&mut NativeExecutor::default(), request, None).unwrap();
            assert_eq!(outcome.status_code(), status, "mode {mode:?}");
            assert_eq!(outcome.cleanup, ExecutionCleanupState::Complete);
            assert!(matches!(
                (mode, outcome.output),
                (ExecutionMode::NativeCommand, ExecutionOutput::Bytes { .. })
                    | (
                        ExecutionMode::Data | ExecutionMode::Lua,
                        ExecutionOutput::Value { .. }
                    )
            ));
        }

        for mode in [ExecutionMode::Bash, ExecutionMode::Zsh] {
            let executable = match mode {
                ExecutionMode::Bash => "bash",
                ExecutionMode::Zsh => "zsh",
                _ => panic!("test iterates only reference-shell modes"),
            };
            if !shell_is_available(executable) {
                continue;
            }
            let request = execution_request(
                "<contract-test>",
                "printf value; exit 7",
                mode,
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: DEFAULT_CAPTURE_BYTES,
                },
                ExecutionEffects::all(),
            )
            .unwrap();
            let outcome =
                execute_execution_request(&mut NativeExecutor::default(), request, None).unwrap();
            assert_eq!(outcome.status_code(), 7);
            assert!(matches!(
                outcome.output,
                ExecutionOutput::Bytes { ref stdout, .. } if stdout == b"value"
            ));
        }
    }

    #[test]
    fn shared_execution_contract_denies_effects_and_cancellation_before_every_mode() {
        for mode in [
            ExecutionMode::NativeCommand,
            ExecutionMode::Data,
            ExecutionMode::Lua,
            ExecutionMode::Bash,
            ExecutionMode::Zsh,
        ] {
            let source = format!("source for {mode:?}");
            let denied = ExecutionRequest::new(
                ExecutionSource::new("<contract-test>", &source).unwrap(),
                mode,
            )
            .with_effects(
                ExecutionEffects::from_effects(&[ExecutionEffect::SpawnProcess]),
                ExecutionEffects::none(),
            );
            let error = execute_execution_request(&mut NativeExecutor::default(), denied, None)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation);
            assert_eq!(error.details.command.as_deref(), Some(source.as_str()));

            let cancellation = quirl_core::ExecutionCancellation::default();
            cancellation.cancel();
            let cancelled = ExecutionRequest::new(
                ExecutionSource::new("<contract-test>", &source).unwrap(),
                mode,
            )
            .with_cancellation(cancellation)
            .with_effects(ExecutionEffects::none(), ExecutionEffects::all());
            let error = execute_execution_request(&mut NativeExecutor::default(), cancelled, None)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
        }

        let missing_authority = ExecutionRequest::new(
            ExecutionSource::new("<contract-test>", "true").unwrap(),
            ExecutionMode::NativeCommand,
        )
        .with_output(ExecutionOutputTarget::Inherit)
        .with_effects(ExecutionEffects::none(), ExecutionEffects::all());
        let error =
            execute_execution_request(&mut NativeExecutor::default(), missing_authority, None)
                .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("required engine authority"));
    }

    #[test]
    fn shared_execution_contract_attaches_exact_source_to_engine_errors() {
        let cases = [
            (ExecutionMode::NativeCommand, "printf 'unterminated"),
            (ExecutionMode::Data, "[1,2 | length"),
            (ExecutionMode::Lua, "error('contract failure')"),
        ];
        for (mode, source) in cases {
            let output = if mode == ExecutionMode::NativeCommand {
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: DEFAULT_CAPTURE_BYTES,
                }
            } else {
                ExecutionOutputTarget::Value
            };
            let request = execution_request(
                "<contract-test>",
                source,
                mode,
                output,
                ExecutionEffects::all(),
            )
            .unwrap();
            let error = execute_execution_request(&mut NativeExecutor::default(), request, None)
                .unwrap_err();
            assert_eq!(
                error.details.command.as_deref(),
                Some(source),
                "mode {mode:?}"
            );
        }

        for mode in [ExecutionMode::Bash, ExecutionMode::Zsh] {
            let executable = if mode == ExecutionMode::Bash {
                "bash"
            } else {
                "zsh"
            };
            if !shell_is_available(executable) {
                continue;
            }
            let source = "if true; then";
            let request = execution_request(
                "<contract-test>",
                source,
                mode,
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: DEFAULT_CAPTURE_BYTES,
                },
                ExecutionEffects::all(),
            )
            .unwrap();
            let error = execute_execution_request(&mut NativeExecutor::default(), request, None)
                .unwrap_err();
            assert_eq!(error.details.command.as_deref(), Some(source));
        }
    }

    #[test]
    fn shared_plan_deadline_stops_every_reachable_engine() {
        let mut cases = vec![
            (
                ExecutionMode::NativeCommand,
                "sleep 10",
                ExecutionOutputTarget::Inherit,
            ),
            (
                ExecutionMode::NativeCommand,
                "sleep 10",
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: 64,
                },
            ),
            (
                ExecutionMode::QuirlScript,
                "sleep 10",
                ExecutionOutputTarget::Value,
            ),
            (
                ExecutionMode::Data,
                "^external sleep 10",
                ExecutionOutputTarget::Value,
            ),
            (
                ExecutionMode::Lua,
                "return quirl.process.run('sleep 10')",
                ExecutionOutputTarget::Value,
            ),
            (
                ExecutionMode::LuaScript,
                "return { abi_version = 1, main = function() quirl.process.run('sleep 10'); return { abi_version = 1, ok = true, status = 0, output = { kind = 'value', value = { type = 'nothing' } } } end }",
                ExecutionOutputTarget::Value,
            ),
        ];
        for (mode, executable) in [(ExecutionMode::Bash, "bash"), (ExecutionMode::Zsh, "zsh")] {
            if shell_is_available(executable) {
                cases.push((
                    mode,
                    "sleep 10",
                    ExecutionOutputTarget::Capture {
                        max_bytes_per_stream: 64,
                    },
                ));
            } else {
                eprintln!("skipping {executable} deadline contract: executable is unavailable");
            }
        }

        for (mode, source, output) in cases {
            let request = execution_request(
                "<deadline-contract>",
                source,
                mode,
                output,
                ExecutionEffects::all(),
            )
            .unwrap()
            .with_deadline(Duration::from_millis(20));
            let started = Instant::now();
            let error = execute_execution_request(&mut NativeExecutor::default(), request, None)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit, "mode {mode:?}");
            assert!(started.elapsed() < Duration::from_secs(1), "mode {mode:?}");
        }
    }

    #[test]
    fn shared_plan_cancellation_reaches_every_reachable_engine() {
        let mut cases = vec![
            (
                ExecutionMode::NativeCommand,
                "sleep 10",
                ExecutionOutputTarget::Inherit,
            ),
            (
                ExecutionMode::NativeCommand,
                "sleep 10",
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: 64,
                },
            ),
            (
                ExecutionMode::QuirlScript,
                "sleep 10",
                ExecutionOutputTarget::Value,
            ),
            (
                ExecutionMode::Data,
                "^external sleep 10",
                ExecutionOutputTarget::Value,
            ),
            (
                ExecutionMode::Lua,
                "return quirl.process.run('sleep 10')",
                ExecutionOutputTarget::Value,
            ),
            (
                ExecutionMode::LuaScript,
                "return { abi_version = 1, main = function() quirl.process.run('sleep 10'); return { abi_version = 1, ok = true, status = 0, output = { kind = 'value', value = { type = 'nothing' } } } end }",
                ExecutionOutputTarget::Value,
            ),
        ];
        for (mode, executable) in [(ExecutionMode::Bash, "bash"), (ExecutionMode::Zsh, "zsh")] {
            if shell_is_available(executable) {
                cases.push((
                    mode,
                    "sleep 10",
                    ExecutionOutputTarget::Capture {
                        max_bytes_per_stream: 64,
                    },
                ));
            } else {
                eprintln!("skipping {executable} cancellation contract: executable is unavailable");
            }
        }

        for (mode, source, output) in cases {
            let cancellation = ExecutionCancellation::default();
            let worker_cancellation = cancellation.clone();
            let request = execution_request(
                "<cancellation-contract>",
                source,
                mode,
                output,
                ExecutionEffects::all(),
            )
            .unwrap()
            .with_cancellation(worker_cancellation)
            .with_deadline(Duration::from_secs(1));
            let worker = thread::spawn(move || {
                execute_execution_request(&mut NativeExecutor::default(), request, None)
            });
            thread::sleep(Duration::from_millis(20));
            cancellation.cancel();
            let error = worker.join().unwrap().unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit, "mode {mode:?}");
            assert!(
                error.message.contains("cancel"),
                "mode {mode:?}: {}",
                error.message
            );
        }
    }

    #[test]
    fn plan_capture_limit_is_exact_for_native_and_reference_shells() {
        let mut modes = vec![ExecutionMode::NativeCommand];
        for (mode, executable) in [(ExecutionMode::Bash, "bash"), (ExecutionMode::Zsh, "zsh")] {
            if shell_is_available(executable) {
                modes.push(mode);
            } else {
                eprintln!("skipping {executable} capture contract: executable is unavailable");
            }
        }
        for mode in modes {
            let exact = execution_request(
                "<capture-contract>",
                "printf 1234",
                mode,
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: 4,
                },
                ExecutionEffects::all(),
            )
            .unwrap();
            let outcome =
                execute_execution_request(&mut NativeExecutor::default(), exact, None).unwrap();
            assert!(matches!(
                outcome.output,
                ExecutionOutput::Bytes { ref stdout, .. } if stdout == b"1234"
            ));

            let overflow = execution_request(
                "<capture-contract>",
                "printf 12345",
                mode,
                ExecutionOutputTarget::Capture {
                    max_bytes_per_stream: 4,
                },
                ExecutionEffects::all(),
            )
            .unwrap();
            let error = execute_execution_request(&mut NativeExecutor::default(), overflow, None)
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit, "mode {mode:?}");
        }
    }

    #[test]
    fn inherited_native_output_has_no_capture_but_keeps_plan_control() {
        let request = execution_request(
            "<inherit-contract>",
            "true",
            ExecutionMode::NativeCommand,
            ExecutionOutputTarget::Inherit,
            ExecutionEffects::all(),
        )
        .unwrap();
        let outcome =
            execute_execution_request(&mut NativeExecutor::default(), request, None).unwrap();
        assert_eq!(outcome.status_code(), 0);
        assert_eq!(outcome.output, ExecutionOutput::Inherited);
    }

    #[test]
    fn native_background_outcome_reports_retained_engine_cleanup() {
        let request = execution_request(
            "<cleanup-contract>",
            "sleep 1 &",
            ExecutionMode::NativeCommand,
            ExecutionOutputTarget::Capture {
                max_bytes_per_stream: 64,
            },
            ExecutionEffects::all(),
        )
        .unwrap();
        let mut executor = NativeExecutor::default();
        let outcome = execute_execution_request(&mut executor, request, None).unwrap();
        assert_eq!(outcome.cleanup, ExecutionCleanupState::RetainedByEngine);
        let job = executor.jobs()[0].clone();
        executor.cancel_job(job.id).unwrap();
    }

    #[test]
    fn interruption_events_distinguish_cancellation_and_deadline_from_other_limits() {
        let cancelled = ShellError::new(ErrorCode::ResourceLimit, "execution was cancelled");
        assert_eq!(
            execution_interruption_reason(&cancelled),
            Some("execution cancelled")
        );
        let deadline = ShellError::new(
            ErrorCode::ResourceLimit,
            "execution exceeded its absolute deadline",
        );
        assert_eq!(
            execution_interruption_reason(&deadline),
            Some("execution deadline expired")
        );
        let capture = ShellError::new(
            ErrorCode::ResourceLimit,
            "captured output exceeded its limit",
        );
        assert_eq!(execution_interruption_reason(&capture), None);
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
        assert!(
            !<Cli as clap::CommandFactory>::command()
                .render_long_help()
                .to_string()
                .contains("build-info")
        );
    }

    #[test]
    fn extension_plan_actions_rewrite_annotate_and_block_before_execution() {
        let mut planned = PlannedExecution::new("echo original");
        let mut executor = NativeExecutor::default();
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
            &mut executor,
        )
        .unwrap();
        assert_eq!(planned.source, "echo rewritten");
        assert_eq!(planned.annotations["policy"], "checked");

        let error = apply_plan_actions(
            vec![ExtensionAction::BlockExecution {
                reason: "denied by policy".to_owned(),
            }],
            &mut planned,
            &mut executor,
        )
        .unwrap_err();
        assert!(error.message.contains("blocked"));
        assert!(safe_extension_output_text("\u{1b}[31mraw").is_none());
        assert_eq!(
            terminal_safe_extension_text("raw\u{1b}[31m\rtext"),
            "raw\\u{1b}[31m\\rtext"
        );
    }

    #[cfg(unix)]
    #[test]
    fn extension_actions_update_only_the_owning_executor_environment() {
        let name = "QUIRL_EXTENSION_SESSION_ENVIRONMENT";
        assert!(std::env::var_os(name).is_none());
        let mut owner = NativeExecutor::default();
        let mut independent = NativeExecutor::default();
        let mut planned = PlannedExecution::new("true");

        apply_plan_actions(
            vec![ExtensionAction::SetEnvironment {
                name: name.to_owned(),
                value: "owner".to_owned(),
            }],
            &mut planned,
            &mut owner,
        )
        .unwrap();

        let command = format!("sh -c 'printf %s \"${name}\"'");
        assert_eq!(
            owner.execute_capture(&command).unwrap().stdout.as_deref(),
            Some("owner")
        );
        assert_eq!(
            independent
                .execute_capture(&command)
                .unwrap()
                .stdout
                .as_deref(),
            Some("")
        );
        assert!(std::env::var_os(name).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn blocked_extension_actions_do_not_partially_commit_environment() {
        let name = "QUIRL_BLOCKED_EXTENSION_ENVIRONMENT";
        let mut executor = NativeExecutor::default();
        let mut planned = PlannedExecution::new("true");

        let error = apply_plan_actions(
            vec![
                ExtensionAction::SetEnvironment {
                    name: name.to_owned(),
                    value: "must-not-commit".to_owned(),
                },
                ExtensionAction::BlockExecution {
                    reason: "blocked after staging".to_owned(),
                },
            ],
            &mut planned,
            &mut executor,
        )
        .unwrap_err();

        assert!(error.message.contains("blocked"));
        let command = format!("sh -c 'printf %s \"${name}\"'");
        assert_eq!(
            executor
                .execute_capture(&command)
                .unwrap()
                .stdout
                .as_deref(),
            Some("")
        );
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
            assert!(
                cli.get_about()
                    .is_some_and(|about| !about.to_string().trim().is_empty()),
                "CLI leaf `{path}` has no parser-facing summary"
            );
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
                assert!(
                    argument
                        .get_help()
                        .is_some_and(|help| !help.to_string().trim().is_empty()),
                    "visible Clap argument `{}` for `{path}` has no parser-facing documentation",
                    argument.get_id()
                );
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
            "QUIRL_C1_DIFFERENTIAL_{}_{}",
            std::process::id(),
            NEXT_DIFFERENTIAL_FIXTURE.fetch_add(1, Ordering::Relaxed)
        );
        assert!(std::env::var_os(&variable).is_none());
        assert_native_and_reference_case(
            "export-assignment",
            |_| format!("export {variable}=value && printenv {variable}"),
            None,
        );

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
    fn rich_viewport_rejects_background_commands_before_spawn() {
        let error = execute_command_or_dialect_island(
            &mut NativeExecutor::default(),
            "sleep 30 &",
            ExecutionOutputMode::RichViewport,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("background"));
        assert!(
            error
                .details
                .help
                .iter()
                .any(|help| help.contains("simple"))
        );
    }

    #[test]
    fn bounded_transcript_writer_discards_excess_and_marks_output() {
        let mut writer = BoundedTranscriptWriter::new();
        let oversized = vec![b'x'; INTERACTIVE_TRANSCRIPT_OUTPUT_BYTES_MAX + 128];
        writer.write_all(&oversized).unwrap();
        let output = writer.finish();
        assert_eq!(output.len(), INTERACTIVE_TRANSCRIPT_OUTPUT_BYTES_MAX);
        assert!(output.ends_with("bytes …\n".as_bytes()));
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("discarded 128 bytes")
        );
    }

    #[test]
    fn structured_outcome_bytes_are_terminal_safe_and_newline_terminated() {
        let outcome = ExecutionOutcome::new(
            ExecutionStatus::Exited(0),
            ExecutionOutput::Value {
                value: StructuredValue::String("hello\u{1b}[2J".to_owned()),
            },
            Vec::new(),
            ExecutionCleanupState::Complete,
        )
        .unwrap();
        let (stdout, stderr) = execution_outcome_bytes(&outcome).unwrap();
        assert_eq!(stdout, b"hello\\u{1b}[2J\n");
        assert!(stderr.is_empty());
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

    #[test]
    fn interactive_data_rows_write_before_a_later_pull_failure() {
        let path = std::env::temp_dir().join(format!(
            "quirl-interactive-data-{}-{}.csv",
            std::process::id(),
            NEXT_DIFFERENTIAL_FIXTURE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, "name,kind\napi,service\nbroken\n").unwrap();
        let output = DataRuntime::new()
            .eval_output(&format!("open {}", path.display()))
            .unwrap();
        let mut rendered = Vec::new();
        let mut stage = InteractiveDataCache::default().stage();
        let error = render_interactive_data_output(
            output,
            &AtomicBool::new(false),
            &mut rendered,
            &mut stage,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Data);
        assert!(String::from_utf8(rendered).unwrap().contains("api"));
        assert_eq!(stage.items.len(), 1);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn interactive_data_cancellation_stops_before_the_next_write() {
        let output = DataRuntime::new()
            .eval_output(r#""one\ntwo" | lines"#)
            .unwrap();
        let mut rendered = Vec::new();
        let mut stage = InteractiveDataCache::default().stage();
        let error = render_interactive_data_output(
            output,
            &AtomicBool::new(true),
            &mut rendered,
            &mut stage,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(rendered.is_empty());
        assert!(stage.items.is_empty());
    }

    #[test]
    fn interactive_data_option_depth_fails_before_rendering() {
        let mut output = DataOutput::Value(StructuredValue::Int(1));
        for _ in 0..=INTERACTIVE_DATA_OPTION_DEPTH_MAX {
            output = DataOutput::Option(Some(Box::new(output)));
        }
        let mut rendered = Vec::new();
        let mut stage = InteractiveDataCache::default().stage();
        let error = render_interactive_data_output(
            output,
            &AtomicBool::new(false),
            &mut rendered,
            &mut stage,
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("observed: 65"));
        assert!(rendered.is_empty());
        assert!(stage.items.is_empty());
    }

    struct FailingWriter {
        remaining: usize,
        bytes: Vec<u8>,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected failure",
                ));
            }
            let written = buffer.len().min(self.remaining);
            self.bytes.extend_from_slice(&buffer[..written]);
            self.remaining = self.remaining.saturating_sub(written);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn interactive_data_write_failure_is_not_a_successful_partial_result() {
        let output = DataRuntime::new()
            .eval_output(r#""one\ntwo" | lines"#)
            .unwrap();
        let mut writer = FailingWriter {
            remaining: 2,
            bytes: Vec::new(),
        };
        let mut stage = InteractiveDataCache::default().stage();
        let error = render_interactive_data_output(
            output,
            &AtomicBool::new(false),
            &mut writer,
            &mut stage,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert!(stage.items.is_empty());
    }
}
