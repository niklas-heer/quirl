use clap::{Parser, Subcommand, ValueEnum};
use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{CommandRunner, ErrorCode, ShellError};
use quirl_lua::{format_file, sdk_json, sdk_lua, sdk_markdown, LuaPolicy, LuaRuntime, QuirlConfig};
use quirl_steel::SteelRuntime;
use quirl_syntax::{classify, InteractiveLine, Mode};
use quirl_ui::{editor, render_error, QuirlPrompt};
use reedline::Signal;
use std::{
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    process::ExitCode,
};

#[derive(Debug, Parser)]
#[command(name = "quirl", version, about = "Everything you need, mixed in")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a Lua script, or a legacy Steel/Scheme prototype, in-process.
    Run {
        file: PathBuf,
        #[arg(trailing_var_arg = true)]
        arguments: Vec<String>,
    },
    /// Evaluate Lua and print the returned value.
    Eval { expression: String },
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
        Some(Command::Run { file, arguments }) if is_lua(&file) => {
            let lua = LuaRuntime::new(LuaPolicy::script())?;
            print_json_value(lua.run_file(&file, &arguments)?);
            Ok(0)
        }
        Some(Command::Run { file, .. }) => {
            let mut steel = SteelRuntime::new();
            print_values(steel.run_file(&file)?);
            Ok(0)
        }
        Some(Command::Eval { expression }) => {
            let lua = LuaRuntime::new(LuaPolicy::script())?;
            print_json_value(lua.eval(&expression)?);
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
    let mut line_editor = editor(catalog.clone());
    let mut mode = Mode::Command;
    let runner = CommandRunner::default();
    // Extension VMs remain lazy so they do not delay the first prompt.
    let mut lua = None;
    let mut steel = None;
    let mut last_status = 0;

    loop {
        let prompt = QuirlPrompt::new(mode);
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
                InteractiveLine::Command(command) => match runner.execute(command) {
                    Ok(outcome) => {
                        last_status = outcome.status;
                        print_outcome(&outcome);
                    }
                    Err(error) => {
                        last_status = 1;
                        eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
                    }
                },
                InteractiveLine::Lua(source) => match eval_lua(&mut lua, source) {
                    Ok(value) => {
                        last_status = 0;
                        print_json_value(value);
                    }
                    Err(error) => {
                        last_status = 1;
                        eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
                    }
                },
                InteractiveLine::Steel(source) => match eval_steel(&mut steel, source) {
                    Ok(values) => {
                        last_status = 0;
                        print_values(values);
                    }
                    Err(error) => {
                        last_status = 1;
                        eprintln!("{}", render_error(&error, io::stderr().is_terminal()));
                    }
                },
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

fn eval_steel(runtime: &mut Option<SteelRuntime>, source: &str) -> Result<Vec<String>, ShellError> {
    runtime.get_or_insert_with(SteelRuntime::new).eval(source)
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
    if is_lua(file) {
        validate_lua(file, format, "valid Lua")
    } else {
        match SteelRuntime::check_file(file) {
            Ok(()) => validation_success(file, format, "valid Steel"),
            Err(error) => validation_failure(error, format),
        }
    }
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

fn print_values(values: Vec<String>) {
    for value in values {
        println!("{value}");
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
