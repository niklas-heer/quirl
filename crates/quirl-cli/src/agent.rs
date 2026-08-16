use clap::{Subcommand, ValueEnum};
use quirl_catalog::Catalog;
use quirl_contract::{
    build_agent_catalog, build_agent_context, build_agent_manifest, render_context_markdown,
    validate_agent_document_with_anchors, AgentCatalog, AgentDocumentKind, AgentManifest,
    AgentValidationAnchors, HostCapability, HostParameter, ValidationReport, DEFAULT_TOKEN_BUDGET,
};
use quirl_core::{escape_json_terminal_controls, escape_terminal_controls, ErrorCode, ShellError};
use quirl_lua::HOST_API;
use serde::Serialize;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

const MAX_AGENT_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Export the installed command and Lua host capability catalog.
    Catalog {
        /// Output representation for the complete installed catalog.
        #[arg(long, value_enum, default_value_t = AgentOutputFormat::Json)]
        format: AgentOutputFormat,
    },
    /// Select the smallest relevant installed subtree within a deterministic budget.
    Context {
        /// Search terms used to rank installed commands and host capabilities.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Maximum estimated tokens retained in the selected context.
        #[arg(long, default_value_t = DEFAULT_TOKEN_BUDGET)]
        token_budget: usize,
        /// Output representation for the bounded context document.
        #[arg(long, value_enum, default_value_t = ContextOutputFormat::Markdown)]
        format: ContextOutputFormat,
    },
    /// Export installed versions, schema hashes, capabilities, tools, and validators.
    Manifest {
        /// Output representation for the installed agent manifest.
        #[arg(long, value_enum, default_value_t = AgentOutputFormat::Json)]
        format: AgentOutputFormat,
    },
    /// Validate a versioned agent document without executing code.
    Validate {
        /// JSON agent catalog, context, or manifest to validate.
        file: PathBuf,
        /// Schema contract expected for the input document.
        #[arg(long, value_enum)]
        kind: AgentKind,
        /// Output representation for the validation report.
        #[arg(long, value_enum, default_value_t = AgentOutputFormat::Text)]
        format: AgentOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AgentOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ContextOutputFormat {
    Markdown,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AgentKind {
    Catalog,
    Context,
    Manifest,
}

pub fn wants_json(command: &AgentCommand) -> bool {
    matches!(
        command,
        AgentCommand::Catalog {
            format: AgentOutputFormat::Json
        } | AgentCommand::Context {
            format: ContextOutputFormat::Json,
            ..
        } | AgentCommand::Manifest {
            format: AgentOutputFormat::Json
        } | AgentCommand::Validate {
            format: AgentOutputFormat::Json,
            ..
        }
    )
}

pub fn execute(command: AgentCommand, catalog: &Catalog) -> Result<i32, ShellError> {
    match command {
        AgentCommand::Catalog { format } => {
            let catalog = installed_agent_catalog(catalog)?;
            match format {
                AgentOutputFormat::Json => print_json(&catalog)?,
                AgentOutputFormat::Text => print_agent_catalog(&catalog),
            }
            Ok(0)
        }
        AgentCommand::Context {
            query,
            token_budget,
            format,
        } => {
            let catalog = installed_agent_catalog(catalog)?;
            let context = build_agent_context(&catalog, &query.join(" "), token_budget)?;
            match format {
                ContextOutputFormat::Json => print_json(&context)?,
                ContextOutputFormat::Markdown => {
                    print!(
                        "{}",
                        escape_terminal_controls(&render_context_markdown(&context))
                    )
                }
            }
            Ok(0)
        }
        AgentCommand::Manifest { format } => {
            let catalog = installed_agent_catalog(catalog)?;
            let manifest = build_agent_manifest(&catalog)?;
            match format {
                AgentOutputFormat::Json => print_json(&manifest)?,
                AgentOutputFormat::Text => print_agent_manifest(&manifest),
            }
            Ok(0)
        }
        AgentCommand::Validate { file, kind, format } => {
            let source = read_agent_document(&file)?;
            let catalog = installed_agent_catalog(catalog)?;
            let anchors = AgentValidationAnchors::from(&catalog);
            let report = validate_agent_document_with_anchors(&source, kind.into(), Some(&anchors));
            match format {
                AgentOutputFormat::Json => print_json(&report)?,
                AgentOutputFormat::Text => print_validation(&file, &report),
            }
            Ok(i32::from(!report.valid))
        }
    }
}

fn read_agent_document(path: &Path) -> Result<Vec<u8>, ShellError> {
    let initial_metadata =
        fs::metadata(path).map_err(|error| agent_document_io_error(path, error))?;
    if !initial_metadata.is_file() {
        return Err(agent_document_file_kind_error(path));
    }
    let file = open_agent_document(path).map_err(|error| agent_document_io_error(path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| agent_document_io_error(path, error))?;
    if !metadata.is_file() {
        return Err(agent_document_file_kind_error(path));
    }
    if metadata.len() > MAX_AGENT_DOCUMENT_BYTES as u64 {
        return Err(agent_document_limit_error(metadata.len()));
    }

    let mut source = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_AGENT_DOCUMENT_BYTES.saturating_add(1) as u64)
        .read_to_end(&mut source)
        .map_err(|error| agent_document_io_error(path, error))?;
    if source.len() > MAX_AGENT_DOCUMENT_BYTES {
        return Err(agent_document_limit_error(source.len() as u64));
    }
    Ok(source)
}

fn agent_document_file_kind_error(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("agent document {} is not a regular file", path.display()),
    )
    .with_help("Pass a regular JSON file generated by `quirl agent`")
}

#[cfg(unix)]
fn open_agent_document(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_agent_document(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

fn agent_document_io_error(path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not read agent document {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Pass a readable JSON document generated by `quirl agent`")
}

fn agent_document_limit_error(observed_bytes: u64) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "agent validation document exceeds its read limit",
    )
    .with_context(format!(
        "observed_bytes: {observed_bytes}; limit_bytes: {MAX_AGENT_DOCUMENT_BYTES}"
    ))
    .with_help("Keep the JSON agent document at or below 4 MiB")
}

pub fn installed_agent_catalog(catalog: &Catalog) -> Result<AgentCatalog, ShellError> {
    build_agent_catalog(catalog, &installed_host_api(), env!("CARGO_PKG_VERSION"))
}

fn installed_host_api() -> Vec<HostCapability> {
    HOST_API
        .iter()
        .map(|spec| HostCapability {
            path: spec.path.to_owned(),
            summary: spec.summary.to_owned(),
            parameters: spec
                .parameters
                .iter()
                .map(|parameter| HostParameter {
                    name: parameter.name.to_owned(),
                    value_type: parameter.lua_type.to_owned(),
                })
                .collect(),
            returns: spec.returns.to_owned(),
            capability: spec.capability.map(str::to_owned),
        })
        .collect()
}

fn print_agent_catalog(catalog: &AgentCatalog) {
    println!(
        "Quirl {} agent catalog",
        escape_terminal_controls(&catalog.quirl_version)
    );
    println!(
        "catalog schema: {}",
        escape_terminal_controls(&catalog.schema_hash)
    );
    println!(
        "catalog content: {}",
        escape_terminal_controls(&catalog.catalog_hash)
    );
    println!(
        "host API: {}",
        escape_terminal_controls(&catalog.host_api_hash)
    );
    println!("\nCommands:");
    for command in &catalog.commands {
        println!(
            "  {:<32} {}",
            escape_terminal_controls(&command.signature),
            escape_terminal_controls(&command.summary)
        );
    }
    println!("\nCapabilities:");
    for capability in &catalog.capabilities {
        println!(
            "  {:<24} v{} {}",
            escape_terminal_controls(&capability.name),
            capability.version,
            escape_terminal_controls(&capability.schema_hash)
        );
    }
}

fn print_agent_manifest(manifest: &AgentManifest) {
    println!(
        "Quirl {} installed agent interface",
        escape_terminal_controls(&manifest.quirl_version)
    );
    println!(
        "schema: {}",
        escape_terminal_controls(&manifest.schema_hash)
    );
    println!("{} tools", manifest.tools.len());
    for tool in &manifest.tools {
        println!(
            "  {:<32} {}",
            escape_terminal_controls(&tool.name),
            escape_terminal_controls(&tool.summary)
        );
    }
    println!("{} declared capabilities", manifest.capabilities.len());
    for capability in &manifest.capabilities {
        println!(
            "  {} ({})",
            escape_terminal_controls(&capability.name),
            escape_terminal_controls(&capability.schema_hash)
        );
    }
}

fn print_validation(file: &Path, report: &ValidationReport) {
    if report.valid {
        println!(
            "✓ {} is a valid versioned agent document",
            escape_terminal_controls(&file.display().to_string())
        );
        return;
    }
    println!(
        "✗ {} has {} diagnostics",
        escape_terminal_controls(&file.display().to_string()),
        report.diagnostics.len()
    );
    for diagnostic in &report.diagnostics {
        println!(
            "  {} at {}: {}\n    help: {}",
            escape_terminal_controls(&diagnostic.code),
            escape_terminal_controls(&diagnostic.path),
            escape_terminal_controls(&diagnostic.message),
            escape_terminal_controls(&diagnostic.help)
        );
    }
}

fn print_json(value: &impl Serialize) -> Result<(), ShellError> {
    let json = serde_json::to_string_pretty(value).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not serialize agent output")
            .with_context(error.to_string())
            .with_help("Report this as a Quirl agent schema defect")
    })?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}

impl From<AgentKind> for AgentDocumentKind {
    fn from(kind: AgentKind) -> Self {
        match kind {
            AgentKind::Catalog => Self::Catalog,
            AgentKind::Context => Self::Context,
            AgentKind::Manifest => Self::Manifest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "quirl-agent-{name}-{}-{}",
            std::process::id(),
            TEST_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn host_api_adapter_preserves_installed_capabilities() {
        let api = installed_host_api();
        assert!(api.iter().any(|spec| {
            spec.path == "quirl.process.run" && spec.capability.as_deref() == Some("process.spawn")
        }));
    }

    #[test]
    fn clap_requires_agent_validation_kind() {
        use clap::Parser;

        assert!(
            crate::Cli::try_parse_from(["quirl", "agent", "validate", "catalog.json"]).is_err()
        );
        assert!(crate::Cli::try_parse_from([
            "quirl",
            "agent",
            "validate",
            "catalog.json",
            "--kind",
            "catalog"
        ])
        .is_ok());
    }

    #[test]
    fn agent_validation_read_rejects_oversized_input_before_parsing() {
        let path = test_file("oversized.json");
        fs::write(&path, vec![b' '; MAX_AGENT_DOCUMENT_BYTES + 1]).unwrap();

        let error = read_agent_document(&path).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("observed_bytes") && context.contains("limit_bytes")));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn agent_validation_read_accepts_input_at_the_limit() {
        let path = test_file("at-limit.json");
        fs::write(&path, vec![b' '; MAX_AGENT_DOCUMENT_BYTES]).unwrap();

        assert_eq!(
            read_agent_document(&path).unwrap().len(),
            MAX_AGENT_DOCUMENT_BYTES
        );

        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn agent_validation_rejects_special_files_without_blocking() {
        use std::os::unix::net::UnixListener;

        let path = test_file("socket.json");
        let listener = UnixListener::bind(&path).unwrap();

        let error = read_agent_document(&path).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("regular file"));

        drop(listener);
        fs::remove_file(path).unwrap();
    }
}
