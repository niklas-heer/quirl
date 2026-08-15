use clap::{Args, ValueEnum};
use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{ErrorCode, ShellError};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Args)]
pub(crate) struct NewCommand {
    /// Name of the script to create, without an extension.
    pub(crate) name: String,
    /// Embedded language used for the generated script.
    #[arg(long, value_enum, default_value_t = NewLanguage::Lua)]
    pub(crate) lang: NewLanguage,
    /// Directory in which the script is created.
    #[arg(long, default_value = ".")]
    pub(crate) directory: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum NewLanguage {
    Lua,
}

#[derive(Debug, Args)]
pub(crate) struct DescribeCommand {
    /// Exact catalog path, such as `quirl run`.
    pub(crate) topic: String,
    #[arg(long, value_enum, default_value_t = DocumentationFormat::Text)]
    pub(crate) format: DocumentationFormat,
}

#[derive(Debug, Args)]
pub(crate) struct DocCommand {
    /// Write generated documentation to this file instead of stdout.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = DocumentationFormat::Markdown)]
    pub(crate) format: DocumentationFormat,
    /// Open the generated file with the platform's default viewer.
    #[arg(long)]
    pub(crate) open: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum DocumentationFormat {
    Text,
    Json,
    Markdown,
    Html,
}

pub(crate) fn create(command: NewCommand) -> Result<i32, ShellError> {
    validate_script_name(&command.name)?;
    let extension = match command.lang {
        NewLanguage::Lua => "lua",
    };
    let path = command
        .directory
        .join(format!("{}.{}", command.name, extension));
    if path.exists() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("{} already exists", path.display()),
        )
        .with_help("Choose a new script name or move the existing file first"));
    }
    fs::create_dir_all(&command.directory).map_err(|error| io_error("create", &path, error))?;
    let source = lua_script_template(&command.name);
    write_new_file(&path, source.as_bytes())?;
    println!("created {}", path.display());
    Ok(0)
}

pub(crate) fn describe(command: DescribeCommand, catalog: &Catalog) -> Result<i32, ShellError> {
    let specification = catalog.find(&command.topic).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("catalog command `{}` was not found", command.topic),
        )
        .with_help("Run `quirl catalog --format text` to list installed command paths")
    })?;
    print!("{}", render_command(specification, command.format)?);
    Ok(0)
}

pub(crate) fn doc(command: DocCommand, catalog: &Catalog) -> Result<i32, ShellError> {
    let rendered = render_catalog(catalog, command.format)?;
    match command.output {
        Some(path) => {
            atomic_write(&path, rendered.as_bytes())?;
            println!("generated {}", path.display());
            if command.open {
                open_document(&path)?;
            }
        }
        None if command.open => {
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                "--open requires --output so the viewer has a stable file",
            )
            .with_help("Pass `--output target/quirl-docs/catalog.html --format html --open`"));
        }
        None => print!("{rendered}"),
    }
    Ok(0)
}

fn validate_script_name(name: &str) -> Result<(), ShellError> {
    if !name.is_empty()
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::InvalidArgument,
        format!("`{name}` is not a portable script name"),
    )
    .with_help("Use ASCII letters, digits, hyphens, and underscores"))
}

fn lua_script_template(name: &str) -> String {
    format!(
        "---@module {name}\n---@param ctx table\n---@return table result\nlocal function main(ctx)\n  return {{ script = \"{name}\", arguments = ctx.args }}\nend\n\nreturn {{ main = main }}\n"
    )
}

fn render_catalog(catalog: &Catalog, format: DocumentationFormat) -> Result<String, ShellError> {
    match format {
        DocumentationFormat::Text => Ok(render_catalog_text(catalog)),
        DocumentationFormat::Json => serde_json::to_string_pretty(catalog)
            .map(|json| format!("{json}\n"))
            .map_err(json_error),
        DocumentationFormat::Markdown => Ok(catalog.to_markdown()),
        DocumentationFormat::Html => Ok(render_catalog_html(catalog)),
    }
}

fn render_command(
    command: &CommandSpec,
    format: DocumentationFormat,
) -> Result<String, ShellError> {
    match format {
        DocumentationFormat::Text => Ok(render_command_text(command)),
        DocumentationFormat::Json => serde_json::to_string_pretty(command)
            .map(|json| format!("{json}\n"))
            .map_err(json_error),
        DocumentationFormat::Markdown => Ok(render_command_markdown(command)),
        DocumentationFormat::Html => Ok(render_command_html(command)),
    }
}

fn render_catalog_text(catalog: &Catalog) -> String {
    let mut output = format!("Quirl catalog schema {}\n\n", catalog.schema_version);
    for command in &catalog.commands {
        output.push_str(&format!("{:<32} {}\n", command.signature, command.summary));
    }
    output
}

fn render_command_text(command: &CommandSpec) -> String {
    let mut output = format!(
        "{}\n  {}\n\n{}\n",
        command.signature, command.summary, command.details
    );
    if !command.options.is_empty() {
        output.push_str("\nOptions:\n");
        for option in &command.options {
            output.push_str(&format!(
                "  {:<24} {}\n",
                option.names.join(", "),
                option.documentation
            ));
        }
    }
    if !command.examples.is_empty() {
        output.push_str("\nExamples:\n");
        for example in &command.examples {
            output.push_str(&format!("  {example}\n"));
        }
    }
    output
}

fn render_command_markdown(command: &CommandSpec) -> String {
    let mut output = format!(
        "# `{}`\n\n{}\n\n{}\n",
        command.signature, command.summary, command.details
    );
    if !command.options.is_empty() {
        output.push_str("\n## Options\n\n");
        for option in &command.options {
            output.push_str(&format!(
                "- `{}` — {}\n",
                option.names.join("`, `"),
                option.documentation
            ));
        }
    }
    if !command.examples.is_empty() {
        output.push_str("\n## Examples\n\n");
        for example in &command.examples {
            output.push_str(&format!("```sh\n{example}\n```\n\n"));
        }
    }
    output
}

fn render_catalog_html(catalog: &Catalog) -> String {
    let mut body = format!(
        "<h1>Quirl command catalog</h1><p>Schema version {}</p>",
        catalog.schema_version
    );
    for command in &catalog.commands {
        body.push_str(&render_command_html(command));
    }
    html_document(&body)
}

fn render_command_html(command: &CommandSpec) -> String {
    let mut output = format!(
        "<section><h2><code>{}</code></h2><p>{}</p><p>{}</p>",
        escape_html(&command.signature),
        escape_html(&command.summary),
        escape_html(&command.details)
    );
    if !command.options.is_empty() {
        output.push_str("<h3>Options</h3><ul>");
        for option in &command.options {
            output.push_str(&format!(
                "<li><code>{}</code> — {}</li>",
                escape_html(&option.names.join(", ")),
                escape_html(&option.documentation)
            ));
        }
        output.push_str("</ul>");
    }
    output.push_str("</section>");
    output
}

fn html_document(body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Quirl command catalog</title></head><body>{body}</body></html>\n"
    )
}

fn escape_html(source: &str) -> String {
    source
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn write_new_file(path: &Path, contents: &[u8]) -> Result<(), ShellError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(path)
        .map_err(|error| io_error("create", path, error))?;
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| io_error("write", path, error))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), ShellError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(|error| io_error("create", parent, error))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::InvalidArgument,
                "documentation output has no file name",
            )
            .with_help("Choose a regular output file path")
        })?;
    let temporary = parent.join(format!(".{file_name}.quirl.tmp"));
    let result = (|| {
        let mut file =
            fs::File::create(&temporary).map_err(|error| io_error("create", &temporary, error))?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error("write", &temporary, error))?;
        fs::rename(&temporary, path).map_err(|error| io_error("replace", path, error))?;
        let directory = fs::File::open(parent).map_err(|error| io_error("open", parent, error))?;
        directory
            .sync_all()
            .map_err(|error| io_error("synchronize", parent, error))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn open_document(path: &Path) -> Result<(), ShellError> {
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = Command::new("xdg-open");

    let status = command
        .arg(path)
        .status()
        .map_err(|error| io_error("open", path, error))?;
    if status.success() {
        Ok(())
    } else {
        Err(ShellError::new(
            ErrorCode::Io,
            format!("the document viewer exited with {status}"),
        )
        .with_help(format!("Open {} manually", path.display())))
    }
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("failed to {action} {}: {error}", path.display()),
    )
    .with_help("Check the path and its permissions, then retry")
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Validation, error.to_string())
        .with_help("Report this catalog serialization failure")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "quirl-author-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn new_script_creates_checks_and_runs_with_arguments_before_refusing_overwrite() {
        let directory = temporary_directory("new");
        let command = NewCommand {
            name: "automation".to_owned(),
            lang: NewLanguage::Lua,
            directory: directory.clone(),
        };
        create(command).unwrap();
        let path = directory.join("automation.lua");
        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains("---@module automation"));
        assert!(source.contains("return { main = main }"));
        quirl_lua::LuaRuntime::check_file(&path).unwrap();
        let output = crate::script::run(&path, None, &["staging".to_owned()]).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.value["script"], "automation");
        assert_eq!(output.value["arguments"], serde_json::json!(["staging"]));
        let duplicate = create(NewCommand {
            name: "automation".to_owned(),
            lang: NewLanguage::Lua,
            directory: directory.clone(),
        });
        assert!(duplicate.is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn documentation_views_are_deterministic_and_escape_html() {
        let catalog = Catalog::builtin();
        let first = render_catalog(&catalog, DocumentationFormat::Html).unwrap();
        let second = render_catalog(&catalog, DocumentationFormat::Html).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("<!doctype html>"));
        assert!(!first.contains("<source>"));
        assert!(render_catalog(&catalog, DocumentationFormat::Json)
            .unwrap()
            .contains("schema_version"));
    }

    #[test]
    fn atomic_document_write_replaces_complete_content() {
        let directory = temporary_directory("doc");
        let path = directory.join("catalog.md");
        atomic_write(&path, b"first\n").unwrap();
        atomic_write(&path, b"second\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "second\n");
        assert!(!directory.join(".catalog.md.quirl.tmp").exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
