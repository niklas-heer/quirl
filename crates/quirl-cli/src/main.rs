mod extensions;

use clap::{Parser, Subcommand, ValueEnum};
use extensions::{LuaCompletionAdapter, LuaExtensionHost};
use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{CommandRunner, ErrorCode, ShellError};
use quirl_data::DataRuntime;
use quirl_lua::{format_file, sdk_json, sdk_lua, sdk_markdown, LuaPolicy, LuaRuntime, QuirlConfig};
use quirl_syntax::{classify, InteractiveLine, Mode};
use quirl_ui::{editor_with_extensions_and_config, render_error, QuirlPrompt};
use reedline::Signal;
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
    /// Run a Lua script in-process.
    Run {
        file: PathBuf,
        #[arg(trailing_var_arg = true)]
        arguments: Vec<String>,
    },
    /// Evaluate Lua and print the returned value.
    Eval { expression: String },
    /// Evaluate a native structured-data expression or pipeline.
    Data { expression: String },
    /// Parse a script without executing it.
    Check {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Conservatively format a Lua file.
    Fmt {
        file: PathBuf,
        #[arg(long)]
        check: bool,
    },
    /// Parse and lint a Lua file without executing it.
    Lint {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Run test_* functions returned by a Lua test module.
    Test { file: PathBuf },
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
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Export the semantic command catalog used by completion, docs, and AI.
    Catalog {
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    /// Ask the same completion engine used by the interactive IDE menu.
    Complete {
        input: String,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Execute one Bash/Zsh-style command through the configured compatibility shell.
    Exec {
        #[arg(required = true, trailing_var_arg = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Parse, evaluate under config restrictions, and validate against Rust schemas.
    Check {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
}

#[derive(Debug, Subcommand)]
enum PluginCommand {
    /// Load a plugin under restrictions and validate its registrations.
    Check {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
    Markdown,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(status) => ExitCode::from(status.clamp(0, 255) as u8),
        Err(error) => {
            eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<i32, ShellError> {
    let catalog = Catalog::builtin();
    match cli.command {
        Some(Command::Run { file, arguments }) => {
            require_lua_file(&file)?;
            let lua = LuaRuntime::new(LuaPolicy::script())?;
            print_json_value(lua.run_file(&file, &arguments)?);
            Ok(0)
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
        Some(Command::Check { file, format }) => validate_file(&file, format),
        Some(Command::Fmt { file, check }) => {
            let changed = format_file(&file, check)?;
            if check && changed {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!("{} needs formatting", file.display()),
                )
                .with_help(format!("Run `quirl fmt {}`", file.display())));
            }
            println!(
                "{} {}",
                if changed { "formatted" } else { "unchanged" },
                file.display()
            );
            Ok(0)
        }
        Some(Command::Lint { file, format }) => validate_lua(&file, format, "lint clean"),
        Some(Command::Test { file }) => {
            let lua = LuaRuntime::new(LuaPolicy::script())?;
            let count = lua.test_file(&file)?;
            println!("✓ {count} Lua tests passed in {}", file.display());
            Ok(0)
        }
        Some(Command::Config {
            command: ConfigCommand::Check { file, format },
        }) => {
            let lua = LuaRuntime::new(LuaPolicy::config())?;
            match lua.load_config_file(&file) {
                Ok(config) => print_config_result(&file, &config, format),
                Err(error) => validation_failure(error, format),
            }
        }
        Some(Command::Plugin {
            command: PluginCommand::Check { file, format },
        }) => {
            let lua = LuaRuntime::new(LuaPolicy::config())?;
            match lua.load_plugin_file(&file) {
                Ok(registrations) => {
                    match format {
                        OutputFormat::Json => println!(
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
                OutputFormat::Text => print!("{}", sdk_lua()),
                OutputFormat::Json => println!("{}", sdk_json()?),
                OutputFormat::Markdown => print!("{}", sdk_markdown()),
            }
            Ok(0)
        }
        Some(Command::Catalog { format }) => {
            match format {
                OutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&catalog).map_err(json_error)?
                ),
                OutputFormat::Markdown => print!("{}", catalog.to_markdown()),
                OutputFormat::Text => print_catalog(&catalog),
            }
            Ok(0)
        }
        Some(Command::Complete { input, format }) => {
            let completions = catalog.complete(&input, input.len());
            match format {
                OutputFormat::Json => println!(
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
        Some(Command::Exec { command }) => {
            let outcome = CommandRunner::default().execute(&command.join(" "))?;
            print_outcome(&outcome);
            Ok(outcome.status)
        }
        None if !io::stdin().is_terminal() => run_stdin(),
        None => repl(catalog),
    }
}

fn repl(catalog: Catalog) -> Result<i32, ShellError> {
    println!(
        "\x1b[1;32mQuirl\x1b[0m 0.1 · command mode · Tab explores · `mode data` switches grammar"
    );
    let extensions = Arc::new(Mutex::new(LuaExtensionHost::discover()));
    let (mut active_config, mut applied_revision) = {
        let mut host = extensions.lock().map_err(|_| {
            ShellError::new(ErrorCode::Io, "the extension host lock was poisoned")
                .with_help("Restart Quirl to create a fresh extension host")
        })?;
        let config = host.active_config().clone();
        (config, host.config_revision())
    };
    let mut line_editor = configured_editor(&catalog, &extensions, active_config.clone());
    let mut mode = Mode::Command;
    let runner = CommandRunner::default();
    let data = DataRuntime::new();
    // Script evaluation remains lazy; extension VMs load before the first editor view.
    let mut lua = None;
    let mut last_status = 0;
    let mut last_duration: Option<Duration> = None;

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
                active_config = config;
                line_editor = configured_editor(&catalog, &extensions, active_config.clone());
            }
        }
        print_extension_errors(&extensions);
        let mut prompt = QuirlPrompt::with_config(mode, &active_config)
            .with_status(last_status)
            .with_named_extension_segments(extension_segments);
        if let Some(duration) = last_duration {
            prompt = prompt.with_duration(duration);
        }
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(buffer)) => match classify(mode, &buffer) {
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
                    match runner.execute(command) {
                        Ok(outcome) => {
                            last_status = outcome.status;
                            print_outcome(&outcome);
                        }
                        Err(error) => {
                            last_status = 1;
                            eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
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
                            eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
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
                            eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
                        }
                    }
                    last_duration = Some(started.elapsed());
                }
            },
            Ok(Signal::CtrlC) => {
                last_status = 130;
                println!("^C");
            }
            Ok(Signal::CtrlD) => return Ok(last_status),
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
    config: QuirlConfig,
) -> reedline::Reedline {
    let completion_adapter = LuaCompletionAdapter::new(Arc::clone(extensions));
    editor_with_extensions_and_config(catalog.clone(), Some(Box::new(completion_adapter)), config)
}

fn print_extension_errors(extensions: &Arc<Mutex<LuaExtensionHost>>) {
    let errors = extensions
        .lock()
        .map(|mut extensions| extensions.take_errors())
        .unwrap_or_default();
    for error in errors {
        eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
    }
}

fn eval_lua(
    runtime: &mut Option<LuaRuntime>,
    source: &str,
) -> Result<serde_json::Value, ShellError> {
    if runtime.is_none() {
        *runtime = Some(LuaRuntime::new(LuaPolicy::script())?);
    }
    runtime
        .as_ref()
        .expect("Lua runtime initialized")
        .eval(source)
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

fn validate_file(file: &Path, format: OutputFormat) -> Result<i32, ShellError> {
    if let Err(error) = require_lua_file(file) {
        return validation_failure(error, format);
    }
    validate_lua(file, format, "valid Lua")
}

fn validate_lua(file: &Path, format: OutputFormat, message: &str) -> Result<i32, ShellError> {
    match LuaRuntime::check_file(file) {
        Ok(()) => validation_success(file, format, message),
        Err(error) => validation_failure(error, format),
    }
}

fn validation_success(file: &Path, format: OutputFormat, message: &str) -> Result<i32, ShellError> {
    match format {
        OutputFormat::Json => println!(
            "{{\"valid\":true,\"file\":{}}}",
            json_string(&file.display().to_string())?
        ),
        _ => println!("✓ {} is {message}", file.display()),
    }
    Ok(0)
}

fn validation_failure(error: ShellError, format: OutputFormat) -> Result<i32, ShellError> {
    if matches!(format, OutputFormat::Json) {
        println!(
            "{}",
            serde_json::to_string_pretty(&error).map_err(json_error)?
        );
        Ok(1)
    } else {
        Err(error)
    }
}

fn print_config_result(
    file: &Path,
    config: &QuirlConfig,
    format: OutputFormat,
) -> Result<i32, ShellError> {
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(config).map_err(json_error)?
        ),
        _ => println!("✓ {} is valid Lua configuration", file.display()),
    }
    Ok(0)
}

fn is_lua(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "lua")
}

fn require_lua_file(path: &Path) -> Result<(), ShellError> {
    if is_lua(path) {
        Ok(())
    } else {
        Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} is not a Lua script", path.display()),
        )
        .with_help("Quirl currently runs embedded scripts with the .lua extension"))
    }
}

fn print_help(catalog: &Catalog, topic: Option<&str>) {
    match topic.and_then(|topic| catalog.find(topic)) {
        Some(command) => print_command_help(command),
        None if topic.is_some() => println!(
            "No exact catalog entry for `{}`. Press Tab to explore related commands.",
            topic.unwrap()
        ),
        None => print_catalog(catalog),
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

fn json_string(value: &str) -> Result<String, ShellError> {
    serde_json::to_string(value).map_err(json_error)
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not produce JSON").with_context(error.to_string())
}
