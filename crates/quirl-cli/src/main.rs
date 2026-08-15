mod agent;
mod author;
mod config;
mod extensions;
mod index;
mod lsp;
mod package;
mod pick;
mod script;

use agent::AgentCommand;
use author::{DescribeCommand, DocCommand, NewCommand};
use clap::{Parser, Subcommand, ValueEnum};
use config::ConfigCommand;
use extensions::{LuaCompletionAdapter, LuaExtensionHost};
use index::IndexCommand;
use package::PackageCommand;
use pick::PickCommand;
use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{ErrorCode, ShellError};
use quirl_data::DataRuntime;
use quirl_lua::{sdk_json, sdk_lua, sdk_markdown, LuaPolicy, LuaRuntime, QuirlConfig};
use quirl_process::{JobStatus, NativeExecutor};
use quirl_syntax::{classify, InteractiveLine, Mode};
use quirl_ui::{
    editor_with_extensions_config_and_history, history_path, render_error, PromptContextScheduler,
    QuirlPrompt, MODE_TOGGLE_HOST_COMMAND,
};
use reedline::Signal;
use script::ScriptLanguage;
use std::{
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Parser)]
#[command(name = "quirl", version, about = "Everything you need, mixed in")]
struct Cli {
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
    /// Run a Lua or .quirl script under its explicit restricted policy.
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
    Data { expression: String },
    /// Validate a Lua/.quirl file or directory without executing source.
    Check {
        /// Script file or recursively discovered directory.
        #[arg(value_name = "PATH")]
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = DiagnosticFormat::Text)]
        format: DiagnosticFormat,
    },
    /// Deterministically format Lua files under a file or directory path.
    Fmt {
        /// Lua/.quirl file or recursively discovered directory; .quirl is unchanged.
        #[arg(value_name = "PATH")]
        file: PathBuf,
        #[arg(long)]
        check: bool,
    },
    /// Lint Lua/.quirl files under a file or directory without execution.
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
    /// Serve deterministic Lua and .quirl editor intelligence over stdio LSP.
    Lsp,
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
    /// Execute one command through Quirl's native pipeline and job graph.
    Exec {
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// Load a plugin under restrictions and validate its registrations.
    Check {
        file: PathBuf,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let wants_json = cli.wants_json();
    match run(cli) {
        Ok(status) => ExitCode::from(status.clamp(0, 255) as u8),
        Err(error) if wants_json => {
            match serde_json::to_string_pretty(&error) {
                Ok(json) => println!("{json}"),
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
            let lua = LuaRuntime::new(LuaPolicy::script())?;
            print_json_value(lua.eval(&expression)?);
            Ok(0)
        }
        Some(Command::Data { expression }) => {
            print_json_value(DataRuntime::new().eval(&expression)?);
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
        Some(Command::Plugin {
            command: PluginCommand::Check { file, format },
        }) => {
            let lua = LuaRuntime::new(LuaPolicy::config())?;
            match lua.load_plugin_file(&file) {
                Ok(registrations) => {
                    match format {
                        DiagnosticFormat::Json => println!(
                            "{}",
                            serde_json::to_string_pretty(&registrations).map_err(json_error)?
                        ),
                        _ => println!(
                            "✓ {} registered {} prompt segments and {} completion providers",
                            file.display(),
                            registrations.prompt_segments.len(),
                            registrations.completion_providers.len()
                        ),
                    }
                    Ok(0)
                }
                Err(error) => validation_failure(error, format),
            }
        }
        Some(Command::Sdk { format }) => {
            match format {
                SdkFormat::Text => print!("{}", sdk_lua()),
                SdkFormat::Json => println!("{}", sdk_json()?),
                SdkFormat::Markdown => print!("{}", sdk_markdown()),
            }
            Ok(0)
        }
        Some(Command::Catalog { format }) => {
            let catalog = index::load_default_catalog();
            match format {
                CatalogFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&catalog).map_err(json_error)?
                ),
                CatalogFormat::Markdown => print!("{}", catalog.to_markdown()),
                CatalogFormat::Text => print_catalog(&catalog),
            }
            Ok(0)
        }
        Some(Command::Agent { command }) => agent::execute(command, &index::load_default_catalog()),
        Some(Command::Package { command }) => {
            package::execute(command, &index::load_default_catalog())
        }
        Some(Command::Describe { command }) => {
            author::describe(command, &index::load_default_catalog())
        }
        Some(Command::Doc { command }) => author::doc(command, &index::load_default_catalog()),
        Some(Command::Lsp) => lsp::execute(index::load_default_catalog()),
        Some(Command::Index { command }) => index::execute(command),
        Some(Command::Complete { input, format }) => {
            let catalog = index::load_default_catalog();
            let completions = catalog.complete(&input, input.len());
            match format {
                CompletionFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&completions).map_err(json_error)?
                ),
                _ => {
                    for completion in completions {
                        println!("{:<28} {}", completion.display, completion.summary);
                    }
                }
            }
            Ok(0)
        }
        Some(Command::Pick { command }) => pick::execute(command, &index::load_default_catalog()),
        Some(Command::Exec { command }) => {
            let outcome = NativeExecutor::default().execute(&command.join(" "))?;
            print_outcome(&outcome);
            Ok(outcome.status)
        }
        None if !io::stdin().is_terminal() => run_stdin(),
        None => repl(index::load_default_catalog()),
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
            | Self::Plugin {
                command:
                    PluginCommand::Check {
                        format: DiagnosticFormat::Json,
                        ..
                    },
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
            Self::Config { command } => config::wants_json(command),
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
            _ => false,
        }
    }
}

fn repl(catalog: Catalog) -> Result<i32, ShellError> {
    let extensions = Arc::new(Mutex::new(LuaExtensionHost::discover()));
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
    print_banner();
    let mut mode = Mode::Command;
    let mut executor = NativeExecutor::default();
    let data = DataRuntime::new();
    // Script evaluation remains lazy; extension VMs load before the first editor view.
    let mut lua = None;
    let mut last_status = 0;
    let mut last_duration: Option<Duration> = None;
    let prompt_context = PromptContextScheduler::default();

    loop {
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
            Ok(Signal::Success(buffer)) => {
                sync_history(&mut line_editor, &history_path)?;
                match classify(mode, &buffer) {
                    InteractiveLine::Empty => {}
                    InteractiveLine::Exit => return Ok(last_status),
                    InteractiveLine::ChangeMode(next) => {
                        mode = next;
                        println!("mode → {mode}");
                    }
                    InteractiveLine::ToggleMode => {
                        mode = mode.toggled();
                        println!("mode → {mode}");
                    }
                    InteractiveLine::Help(topic) => print_help(&catalog, topic),
                    InteractiveLine::Command(command) => {
                        let started = Instant::now();
                        match executor.execute(command) {
                            Ok(outcome) => {
                                last_status = outcome.status;
                                print_outcome(&outcome);
                            }
                            Err(error) => {
                                last_status = 1;
                                eprintln!("{}", render_stderr_error(&error));
                            }
                        }
                        last_duration = Some(started.elapsed());
                    }
                    InteractiveLine::Data(source) => {
                        let started = Instant::now();
                        match data.eval(source) {
                            Ok(value) => {
                                last_status = 0;
                                print_json_value(value);
                            }
                            Err(error) => {
                                last_status = 1;
                                eprintln!("{}", render_stderr_error(&error));
                            }
                        }
                        last_duration = Some(started.elapsed());
                    }
                    InteractiveLine::Lua(source) => {
                        let started = Instant::now();
                        match eval_lua(&mut lua, source) {
                            Ok(value) => {
                                last_status = 0;
                                print_json_value(value);
                            }
                            Err(error) => {
                                last_status = 1;
                                eprintln!("{}", render_stderr_error(&error));
                            }
                        }
                        last_duration = Some(started.elapsed());
                    }
                }
            }
            Ok(Signal::CtrlC) => {
                last_status = 130;
                println!("^C");
            }
            Ok(Signal::CtrlD) => return Ok(last_status),
            Ok(Signal::HostCommand(command)) if command == MODE_TOGGLE_HOST_COMMAND => {
                mode = mode.toggled();
                println!("mode → {mode}");
            }
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

fn configured_editor(
    catalog: &Catalog,
    extensions: &Arc<Mutex<LuaExtensionHost>>,
    mut config: QuirlConfig,
    history_path: &Path,
) -> Result<reedline::Reedline, ShellError> {
    if std::env::var_os("NO_COLOR").is_some() {
        config.editor.semantic_hints = false;
    }
    let completion_adapter = LuaCompletionAdapter::new(Arc::clone(extensions));
    editor_with_extensions_config_and_history(
        catalog.clone(),
        Some(Box::new(completion_adapter)),
        config,
        history_path.to_path_buf(),
    )
}

fn sync_history(editor: &mut reedline::Reedline, history_path: &Path) -> Result<(), ShellError> {
    editor.sync_history().map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not save history to {}", history_path.display()),
        )
        .with_context(error.to_string())
        .with_help("Set QUIRL_HISTORY to a writable file path")
    })
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

fn print_banner() {
    let banner = "Quirl 0.1 · command mode · Ctrl-Space switches · Ctrl-R/T/K pick · Tab completes";
    if color_enabled(
        io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
    ) {
        println!("\x1b[1;32mQuirl\x1b[0m{}", &banner["Quirl".len()..]);
    } else {
        println!("{banner}");
    }
}

fn render_stderr_error(error: &ShellError) -> String {
    render_error(
        error,
        color_enabled(
            io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").is_some(),
        ),
    )
}

fn color_enabled(terminal: bool, no_color_is_set: bool) -> bool {
    terminal && !no_color_is_set
}

fn eval_lua(
    runtime: &mut Option<LuaRuntime>,
    source: &str,
) -> Result<serde_json::Value, ShellError> {
    if runtime.is_none() {
        *runtime = Some(LuaRuntime::new(LuaPolicy::script())?);
    }
    let runtime = runtime.as_ref().ok_or_else(|| {
        ShellError::new(ErrorCode::Lua, "could not initialize the Lua runtime")
            .with_help("Run the command again; if this persists, report the configuration used.")
    })?;
    runtime.eval(source)
}

fn run_stdin() -> Result<i32, ShellError> {
    let mut source = String::new();
    io::stdin().read_to_string(&mut source).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not read standard input")
            .with_context(error.to_string())
    })?;
    let lua = LuaRuntime::new(LuaPolicy::script())?;
    print_json_value(lua.eval(&source)?);
    Ok(0)
}

fn validation_failure(error: ShellError, format: DiagnosticFormat) -> Result<i32, ShellError> {
    if matches!(format, DiagnosticFormat::Json) {
        println!(
            "{}",
            serde_json::to_string_pretty(&error).map_err(json_error)?
        );
        Ok(1)
    } else {
        Err(error)
    }
}

fn print_help(catalog: &Catalog, topic: Option<&str>) {
    if let Some(topic) = topic {
        if let Some(command) = catalog.find(topic) {
            print_command_help(command);
        } else {
            println!(
                "No exact catalog entry for `{topic}`. Press Tab to explore related commands."
            );
        }
    } else {
        print_catalog(catalog);
    }
}

fn print_catalog(catalog: &Catalog) {
    println!("Quirl commands\n");
    for command in &catalog.commands {
        println!("  {:<24} {}", command.signature, command.summary);
    }
    println!(
        "\nTab opens the IDE completion menu; `quirl catalog --format json` is the AI interface."
    );
}

fn print_command_help(command: &CommandSpec) {
    println!(
        "{}\n  {}\n\n{}",
        command.signature, command.summary, command.details
    );
    if !command.options.is_empty() {
        println!("\nOptions:");
        for option in &command.options {
            println!("  {:<20} {}", option.names.join(", "), option.summary);
        }
    }
    if !command.examples.is_empty() {
        println!("\nExamples:");
        for example in &command.examples {
            println!("  {example}");
        }
    }
}

fn print_json_value(value: serde_json::Value) {
    match value {
        serde_json::Value::Null => {}
        serde_json::Value::String(value) => println!("{value}"),
        value if value.is_object() || value.is_array() => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| "<unprintable Lua value>".to_owned())
            );
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

    #[test]
    fn color_requires_a_terminal_and_no_color_must_be_absent() {
        assert!(color_enabled(true, false));
        assert!(!color_enabled(false, false));
        assert!(!color_enabled(true, true));
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
                command: Some(Command::Lsp)
            })
        ));
        assert!(Cli::try_parse_from(["quirl", "lsp", "--port", "9000"]).is_err());
    }

    #[test]
    fn every_cli_leaf_has_one_exact_catalog_contract() {
        fn leaves(command: &clap::Command, prefix: &str, output: &mut Vec<String>) {
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
                    output.push(path);
                }
            }
        }

        let mut cli_leaves = Vec::new();
        leaves(&Cli::command(), "quirl", &mut cli_leaves);
        let catalog = Catalog::builtin();
        for path in cli_leaves {
            let count = catalog
                .commands
                .iter()
                .filter(|command| command.path == path)
                .count();
            assert_eq!(
                count, 1,
                "CLI leaf `{path}` must have one exact catalog entry"
            );
        }
    }
}
