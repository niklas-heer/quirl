use crate::bounded_file::{read_regular_file, ReadFileOptions};
use clap::{Args, ValueEnum};
use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{
    escape_json_terminal_controls, escape_terminal_controls, replace_file_atomically,
    AtomicReplaceOptions, ErrorCode, ShellError,
};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const AUTHOR_PATH_BYTES_MAX: usize = 4 * 1024;
const DOCUMENT_OUTPUT_BYTES_MAX: usize = 8 * 1024 * 1024;
const DIRECTORY_DEPTH_MAX: usize = 64;
const TEMPORARY_NAME_ATTEMPTS_MAX: usize = 64;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateStage {
    DirectoriesReady,
    PartialWrite,
}

struct CreationTransaction {
    created_file: Option<PathBuf>,
    created_directories: Vec<PathBuf>,
}

impl CreationTransaction {
    fn new() -> Self {
        Self {
            created_file: None,
            created_directories: Vec::new(),
        }
    }

    fn cleanup(&mut self, mut error: ShellError) -> ShellError {
        if let Some(path) = self.created_file.take() {
            if let Err(cleanup_error) = fs::remove_file(&path) {
                if cleanup_error.kind() != io::ErrorKind::NotFound {
                    error = error.with_context(format!(
                        "created-file rollback failed for {}: {cleanup_error}",
                        path.display()
                    ));
                }
            }
        }
        while let Some(path) = self.created_directories.pop() {
            if let Err(cleanup_error) = fs::remove_dir(&path) {
                if cleanup_error.kind() != io::ErrorKind::NotFound {
                    error = error.with_context(format!(
                        "created-directory rollback failed for {}: {cleanup_error}",
                        path.display()
                    ));
                }
            }
        }
        error
    }

    fn commit(&mut self) {
        self.created_file = None;
        self.created_directories.clear();
    }
}

impl Drop for CreationTransaction {
    fn drop(&mut self) {
        if let Some(path) = self.created_file.take() {
            let _ = fs::remove_file(path);
        }
        while let Some(path) = self.created_directories.pop() {
            let _ = fs::remove_dir(path);
        }
    }
}

struct TemporaryOutput(Option<PathBuf>);

impl TemporaryOutput {
    fn path(&self) -> &Path {
        self.0
            .as_deref()
            .unwrap_or_else(|| Path::new("<installed-temporary>"))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }

    fn cleanup(&mut self, mut error: ShellError) -> ShellError {
        if let Some(path) = self.0.take() {
            if let Err(cleanup_error) = fs::remove_file(&path) {
                if cleanup_error.kind() != io::ErrorKind::NotFound {
                    error = error.with_context(format!(
                        "documentation temporary cleanup failed for {}: {cleanup_error}",
                        path.display()
                    ));
                }
            }
        }
        error
    }
}

impl Drop for TemporaryOutput {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

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
    Quirl,
}

#[derive(Debug, Args)]
pub(crate) struct DescribeCommand {
    /// Exact catalog path, such as `quirl run`.
    pub(crate) topic: String,
    /// Output representation for the selected command contract.
    #[arg(long, value_enum, default_value_t = DocumentationFormat::Text)]
    pub(crate) format: DocumentationFormat,
}

#[derive(Debug, Args)]
pub(crate) struct DocCommand {
    /// Write generated documentation to this file instead of stdout.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
    /// Output representation for the generated catalog documentation.
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
    create_with_hook(command, |_| Ok(()))
}

fn create_with_hook(
    command: NewCommand,
    mut after_stage: impl FnMut(CreateStage) -> io::Result<()>,
) -> Result<i32, ShellError> {
    validate_script_name(&command.name)?;
    let extension = match command.lang {
        NewLanguage::Lua => "lua",
        NewLanguage::Quirl => "qrl",
    };
    let path = command
        .directory
        .join(format!("{}.{}", command.name, extension));
    validate_author_path(&path)?;
    match fs::symlink_metadata(&path) {
        Ok(_) => {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("{} already exists", path.display()),
            )
            .with_help("Choose a new script name or move the existing file first"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("inspect", &path, error)),
    }
    let mut transaction = CreationTransaction::new();
    if let Err(error) = create_directories(&command.directory, &mut transaction) {
        return Err(transaction.cleanup(error));
    }
    if let Err(error) = after_stage(CreateStage::DirectoriesReady) {
        return Err(transaction.cleanup(io_error("prepare", &path, error)));
    }
    let source = match command.lang {
        NewLanguage::Lua => lua_script_template(&command.name),
        NewLanguage::Quirl => quirl_script_template(&command.name),
    };
    if let Err(error) = write_new_file(&path, source.as_bytes(), &mut transaction, &mut after_stage)
    {
        return Err(transaction.cleanup(error));
    }
    transaction.commit();
    println!(
        "created {}",
        escape_terminal_controls(&path.display().to_string())
    );
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
    let rendered = render_command(specification, command.format)?;
    print!("{}", terminal_safe_stdout(&rendered, command.format));
    Ok(0)
}

pub(crate) fn doc(command: DocCommand, catalog: &Catalog) -> Result<i32, ShellError> {
    let rendered = render_catalog(catalog, command.format)?;
    match command.output {
        Some(path) => {
            atomic_write(&path, rendered.as_bytes())?;
            println!(
                "generated {}",
                escape_terminal_controls(&path.display().to_string())
            );
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
        None => print!("{}", terminal_safe_stdout(&rendered, command.format)),
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

fn quirl_script_template(name: &str) -> String {
    format!("# Native Quirl script: {name}\ndata [\"{name}\"] | length\n")
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

/// Sanitize only bytes written directly to a terminal. Explicit `--output`
/// files retain their selected format exactly, including valid JSON/HTML bytes.
fn terminal_safe_stdout(rendered: &str, format: DocumentationFormat) -> String {
    if matches!(format, DocumentationFormat::Json) {
        escape_json_terminal_controls(rendered)
    } else {
        escape_terminal_controls(rendered)
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
        "{}\n  {}\n\n{}\n\nInput: {}\nOutput: {}\nLive streaming: {}\n",
        command.signature,
        command.summary,
        command.details,
        command.io.input,
        command.io.output,
        command.io.streaming
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
        "# `{}`\n\n{}\n\n{}\n\n## I/O contract\n\n- Input: `{}`\n- Output: `{}`\n- Live streaming: `{}`\n",
        command.signature,
        command.summary,
        command.details,
        command.io.input,
        command.io.output,
        command.io.streaming
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
        "<section><h2><code>{}</code></h2><p>{}</p><p>{}</p><p>Input: <code>{}</code><br>Output: <code>{}</code><br>Live streaming: <code>{}</code></p>",
        escape_html(&command.signature),
        escape_html(&command.summary),
        escape_html(&command.details),
        escape_html(&command.io.input),
        escape_html(&command.io.output),
        command.io.streaming
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

fn write_new_file(
    path: &Path,
    contents: &[u8],
    transaction: &mut CreationTransaction,
    after_stage: &mut impl FnMut(CreateStage) -> io::Result<()>,
) -> Result<(), ShellError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|error| io_error("create", path, error))?;
    transaction.created_file = Some(path.to_path_buf());
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error("secure", path, error))?;
    let split = contents.len().div_ceil(2);
    file.write_all(&contents[..split])
        .and_then(|()| after_stage(CreateStage::PartialWrite))
        .and_then(|()| file.write_all(&contents[split..]))
        .and_then(|()| file.sync_all())
        .map_err(|error| io_error("write", path, error))?;
    validate_regular_output(path, Some(&file), 1)
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), ShellError> {
    validate_author_path(path)?;
    if contents.len() > DOCUMENT_OUTPUT_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!(
                "documentation output {} exceeds its write limit",
                path.display()
            ),
        )
        .with_context(format!(
            "limit: {DOCUMENT_OUTPUT_BYTES_MAX}; observed: {}",
            contents.len()
        ))
        .with_help("Choose a smaller catalog or write documentation in narrower sections"));
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut directories = CreationTransaction::new();
    if let Err(error) = create_directories(parent, &mut directories) {
        return Err(directories.cleanup(error));
    }
    match fs::symlink_metadata(path) {
        Ok(_) => {
            validate_regular_output(path, None, 1)?;
            let expected = read_regular_file(ReadFileOptions {
                path,
                bytes_max: DOCUMENT_OUTPUT_BYTES_MAX,
                context: "documentation output",
                help: "Use an unlinked regular documentation output at or below 8 MiB",
                io_error_code: ErrorCode::Io,
            })?;
            replace_file_atomically(
                path,
                &expected,
                contents,
                AtomicReplaceOptions {
                    bytes_max: DOCUMENT_OUTPUT_BYTES_MAX,
                },
            )?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            install_new_output(path, contents, parent)?;
        }
        Err(error) => return Err(io_error("inspect", path, error)),
    }
    directories.commit();
    Ok(())
}

fn install_new_output(path: &Path, contents: &[u8], parent: &Path) -> Result<(), ShellError> {
    let (temporary, mut file) = create_output_temporary(path)?;
    let mut guard = TemporaryOutput(Some(temporary));
    file.write_all(contents)
        .and_then(|()| file.sync_all())
        .map_err(|error| guard.cleanup(io_error("write", guard.path(), error)))?;
    validate_regular_output(guard.path(), Some(&file), 1).map_err(|error| guard.cleanup(error))?;
    drop(file);
    fs::hard_link(guard.path(), path)
        .map_err(|error| guard.cleanup(io_error("install", path, error)))?;
    if !same_output_file(guard.path(), path) {
        return Err(guard.cleanup(
            ShellError::new(
                ErrorCode::Validation,
                format!(
                    "{} changed during documentation installation",
                    path.display()
                ),
            )
            .with_help("Remove the conflicting output entry and retry"),
        ));
    }
    if let Err(error) = validate_regular_output(path, None, 2) {
        if same_output_file(guard.path(), path) {
            let _ = fs::remove_file(path);
        }
        return Err(guard.cleanup(error));
    }
    if let Err(error) = sync_parent_directory(parent) {
        if same_output_file(guard.path(), path) {
            let _ = fs::remove_file(path);
        }
        return Err(guard.cleanup(error));
    }
    fs::remove_file(guard.path())
        .map_err(|error| guard.cleanup(io_error("clean", guard.path(), error)))?;
    guard.disarm();
    let _ = sync_parent_directory(parent);
    Ok(())
}

fn create_output_temporary(path: &Path) -> Result<(PathBuf, File), ShellError> {
    let name = path.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "documentation output has no file name",
        )
        .with_help("Choose a regular output file path")
    })?;
    for _ in 0..TEMPORARY_NAME_ATTEMPTS_MAX {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".quirl-{}-{sequence}.tmp", std::process::id()));
        let temporary = path.with_file_name(temporary_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary) {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                    let mut shell_error = io_error("secure", &temporary, error);
                    if let Err(cleanup_error) = fs::remove_file(&temporary) {
                        shell_error = shell_error.with_context(format!(
                            "documentation temporary cleanup failed for {}: {cleanup_error}",
                            temporary.display()
                        ));
                    }
                    return Err(shell_error);
                }
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error("create", &temporary, error)),
        }
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        "documentation temporary-name attempts exhausted",
    )
    .with_context(format!(
        "limit: {TEMPORARY_NAME_ATTEMPTS_MAX}; observed: {TEMPORARY_NAME_ATTEMPTS_MAX}"
    ))
    .with_help("Remove stale hidden documentation temporary files and retry"))
}

fn validate_author_path(path: &Path) -> Result<(), ShellError> {
    let bytes = path.as_os_str().as_encoded_bytes().len();
    if bytes <= AUTHOR_PATH_BYTES_MAX && path.file_name().is_some() {
        return Ok(());
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        "authoring path exceeds its bounded portable representation",
    )
    .with_context(format!("limit: {AUTHOR_PATH_BYTES_MAX}; observed: {bytes}"))
    .with_help("Choose a shorter path with a regular file name"))
}

fn create_directories(
    directory: &Path,
    transaction: &mut CreationTransaction,
) -> Result<(), ShellError> {
    let mut missing = Vec::new();
    let mut cursor = directory;
    loop {
        if missing.len() >= DIRECTORY_DEPTH_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!(
                    "directory path {} exceeds its depth limit",
                    directory.display()
                ),
            )
            .with_context(format!(
                "limit: {DIRECTORY_DEPTH_MAX}; observed: at least {}",
                missing.len() + 1
            ))
            .with_help("Choose a shallower authoring directory"));
        }
        match fs::symlink_metadata(cursor) {
            Ok(metadata) if metadata.file_type().is_dir() => break,
            Ok(_) => {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!("{} is not a real directory", cursor.display()),
                )
                .with_help("Replace links and special files in the output path"));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(cursor.to_path_buf());
                cursor = cursor
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
            }
            Err(error) => return Err(io_error("inspect", cursor, error)),
        }
    }
    for path in missing.into_iter().rev() {
        match fs::create_dir(&path) {
            Ok(()) => transaction.created_directories.push(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|inspect_error| io_error("inspect", &path, inspect_error))?;
                if !metadata.file_type().is_dir() {
                    return Err(ShellError::new(
                        ErrorCode::Validation,
                        format!("{} was replaced while creating directories", path.display()),
                    )
                    .with_help("Remove the conflicting entry and retry"));
                }
            }
            Err(error) => return Err(io_error("create", &path, error)),
        }
    }
    Ok(())
}

fn validate_regular_output(
    path: &Path,
    open_file: Option<&File>,
    links_expected: u64,
) -> Result<(), ShellError> {
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| io_error("inspect", path, error))?;
    if !path_metadata.file_type().is_file() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("{} is not a regular output file", path.display()),
        )
        .with_help("Use an unlinked regular file, not a symlink or special file"));
    }
    #[cfg(unix)]
    {
        if path_metadata.nlink() != links_expected {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("{} has hard-link aliases", path.display()),
            )
            .with_context(format!(
                "expected links: {links_expected}; observed: {}",
                path_metadata.nlink()
            ))
            .with_help("Use an unlinked regular output file"));
        }
        if let Some(file) = open_file {
            let file_metadata = file
                .metadata()
                .map_err(|error| io_error("inspect", path, error))?;
            if path_metadata.dev() != file_metadata.dev()
                || path_metadata.ino() != file_metadata.ino()
            {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!("{} changed during output preparation", path.display()),
                )
                .with_help("Remove the conflicting entry and retry"));
            }
            let mode = file_metadata.mode() & 0o777;
            if mode != 0o600 {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!("{} has unexpected temporary permissions", path.display()),
                )
                .with_context(format!("expected mode: 0o600; observed mode: {mode:#o}"))
                .with_help("Remove the conflicting entry and retry"));
            }
        }
        if links_expected == 2 {
            let mode = path_metadata.mode() & 0o777;
            if mode != 0o600 {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!("{} has unexpected installed permissions", path.display()),
                )
                .with_context(format!("expected mode: 0o600; observed mode: {mode:#o}"))
                .with_help("Remove the conflicting entry and retry"));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn same_output_file(left: &Path, right: &Path) -> bool {
    let Ok(left) = fs::symlink_metadata(left) else {
        return false;
    };
    let Ok(right) = fs::symlink_metadata(right) else {
        return false;
    };
    left.file_type().is_file()
        && right.file_type().is_file()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_output_file(_left: &Path, _right: &Path) -> bool {
    false
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<(), ShellError> {
    let directory = fs::File::open(parent).map_err(|error| io_error("open", parent, error))?;
    directory
        .sync_all()
        .map_err(|error| io_error("synchronize", parent, error))
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<(), ShellError> {
    // Rust's portable File API cannot open directories on Windows. The
    // replacement already succeeded and the file contents were flushed.
    Ok(())
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
    fn new_lua_script_creates_checks_and_runs_with_arguments_before_refusing_overwrite() {
        let directory = temporary_directory("new");
        let command = NewCommand {
            name: "script".to_owned(),
            lang: NewLanguage::Lua,
            directory: directory.clone(),
        };
        create(command).unwrap();
        let path = directory.join("script.lua");
        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains("---@module script"));
        assert!(source.contains("return { main = main }"));
        crate::lua_worker::LuaWorkerRuntime::check_file(&path).unwrap();
        let output = crate::script::run(&path, None, &["staging".to_owned()]).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.value["script"], "script");
        assert_eq!(output.value["arguments"], serde_json::json!(["staging"]));
        let duplicate = create(NewCommand {
            name: "script".to_owned(),
            lang: NewLanguage::Lua,
            directory: directory.clone(),
        });
        assert!(duplicate.is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn new_quirl_script_uses_the_canonical_qrl_extension() {
        let directory = temporary_directory("native-script");
        create(NewCommand {
            name: "script".to_owned(),
            lang: NewLanguage::Quirl,
            directory: directory.clone(),
        })
        .unwrap();
        let path = directory.join("script.qrl");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# Native Quirl script: script\ndata [\"script\"] | length\n"
        );
        let output = crate::script::run(&path, None, &[]).unwrap();
        assert_eq!(output.status, 0);
        assert_eq!(output.value, serde_json::json!(1));
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_new_script_rolls_back_only_entries_created_by_this_invocation() {
        let root = temporary_directory("rollback");
        let preexisting = root.join("preexisting");
        fs::create_dir_all(&preexisting).unwrap();
        let marker = preexisting.join("keep.txt");
        fs::write(&marker, b"keep").unwrap();
        let created_directory = preexisting.join("new/deep");

        let error = create_with_hook(
            NewCommand {
                name: "script".to_owned(),
                lang: NewLanguage::Quirl,
                directory: created_directory.clone(),
            },
            |stage| {
                if stage == CreateStage::PartialWrite {
                    Err(io::Error::other("injected script write failure"))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::Io);
        assert_eq!(fs::read(&marker).unwrap(), b"keep");
        assert!(!created_directory.exists());
        assert!(preexisting.exists());
        fs::remove_dir_all(root).unwrap();
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
        let command = catalog.find("quirl doc").unwrap();
        for format in [
            DocumentationFormat::Text,
            DocumentationFormat::Markdown,
            DocumentationFormat::Html,
            DocumentationFormat::Json,
        ] {
            let rendered = render_command(command, format).unwrap();
            assert!(rendered.contains(&command.io.input));
            assert!(rendered.contains(&command.io.output));
        }
    }

    #[test]
    fn stdout_documentation_neutralizes_controls_without_changing_json_values() {
        let mut catalog = Catalog::builtin();
        let command = catalog.commands.first_mut().unwrap();
        command.signature = "quirl hostile\u{1b}[31m\u{9b}2J\r".to_owned();
        command.summary = "summary\u{1b}]8;;https://example.invalid\u{7}".to_owned();
        command.details = "details\u{9b}2J\r".to_owned();
        command.examples = vec!["echo \u{1b}[2J".to_owned()];

        for format in [DocumentationFormat::Text, DocumentationFormat::Markdown] {
            let rendered = render_command(command, format).unwrap();
            let safe = terminal_safe_stdout(&rendered, format);
            assert!(!safe.contains('\u{1b}'));
            assert!(!safe.contains('\u{009b}'));
            assert!(!safe.contains('\r'));
            assert!(safe.contains("\\u{1b}[31m"));
        }

        let rendered = render_command(command, DocumentationFormat::Json).unwrap();
        let safe = terminal_safe_stdout(&rendered, DocumentationFormat::Json);
        assert!(!safe.contains('\u{1b}'));
        assert!(!safe.contains('\u{009b}'));
        let decoded: CommandSpec = serde_json::from_str(&safe).unwrap();
        assert_eq!(decoded, *command);
    }

    #[test]
    fn output_files_keep_the_requested_documentation_bytes_unmodified() {
        let directory = temporary_directory("raw-output");
        let path = directory.join("catalog.md");
        let rendered = "heading\u{1b}[31m\u{9b}2J\r\n";
        atomic_write(&path, rendered.as_bytes()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), rendered);
        fs::remove_dir_all(directory).unwrap();
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

    #[test]
    fn documentation_output_accepts_exact_limit_and_rejects_limit_plus_one() {
        let directory = temporary_directory("output-limit");
        let path = directory.join("catalog.txt");
        atomic_write(&path, &vec![b'x'; DOCUMENT_OUTPUT_BYTES_MAX]).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            DOCUMENT_OUTPUT_BYTES_MAX as u64
        );

        let error = atomic_write(&path, &vec![b'x'; DOCUMENT_OUTPUT_BYTES_MAX + 1]).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            DOCUMENT_OUTPUT_BYTES_MAX as u64
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn documentation_output_rejects_symlinks_hardlinks_and_special_files() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        use std::os::unix::fs::symlink;

        let directory = temporary_directory("output-kinds");
        fs::create_dir(&directory).unwrap();
        let outside = directory.join("outside");
        fs::write(&outside, b"outside").unwrap();

        let symlink_path = directory.join("symlink");
        symlink(&outside, &symlink_path).unwrap();
        assert_eq!(
            atomic_write(&symlink_path, b"replacement")
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        assert_eq!(fs::read(&outside).unwrap(), b"outside");

        let hardlink_path = directory.join("hardlink");
        fs::hard_link(&outside, &hardlink_path).unwrap();
        assert_eq!(
            atomic_write(&hardlink_path, b"replacement")
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );

        let socket_path = directory.join("socket");
        mkfifo(&socket_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert_eq!(
            atomic_write(&socket_path, b"replacement").unwrap_err().code,
            ErrorCode::Validation
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
