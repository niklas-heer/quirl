//! Local AI command search, option intent lookup, and related suggestions.

use crate::{index, intelligence};
use clap::{Subcommand, ValueEnum};
use quirl_core::{escape_json_terminal_controls, escape_terminal_controls, ErrorCode, ShellError};
use serde::Serialize;
use std::{fs, path::PathBuf};

/// Local command-intelligence operations. None execute suggested commands.
#[derive(Debug, Subcommand)]
pub(crate) enum AiCommand {
    /// Show local database, model, and semantic-index readiness.
    Status {
        /// Output representation for readiness information.
        #[arg(long, value_enum, default_value_t = AiOutputFormat::Text)]
        format: AiOutputFormat,
    },
    /// Embed all current command and option documents with potion-base-8M.
    Index {
        /// Output representation for the build report.
        #[arg(long, value_enum, default_value_t = AiOutputFormat::Text)]
        format: AiOutputFormat,
    },
    /// Find commands and options by task intent.
    Search {
        /// Natural-language description, for example "copy a directory preserving permissions".
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Maximum number of results, bounded to 100.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Restrict results to commands, options, or both.
        #[arg(long, value_enum, default_value_t = SearchKind::All)]
        kind: SearchKind,
        /// Output representation for ranked results.
        #[arg(long, value_enum, default_value_t = AiOutputFormat::Text)]
        format: AiOutputFormat,
    },
    /// Suggest commands and options related to an installed command.
    Related {
        /// Installed command path, for example `git commit`.
        #[arg(required = true, num_args = 1..)]
        command: Vec<String>,
        /// Maximum number of results, bounded to 100.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Output representation for ranked results.
        #[arg(long, value_enum, default_value_t = AiOutputFormat::Text)]
        format: AiOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum AiOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum SearchKind {
    All,
    Command,
    Option,
}

#[derive(Debug, Serialize)]
struct AiStatus {
    database_path: PathBuf,
    database_ready: bool,
    database_bytes: Option<u64>,
    model: &'static str,
    model_path: Option<PathBuf>,
    model_ready: bool,
    commands: Option<usize>,
    options: Option<usize>,
    semantic_documents: Option<usize>,
    embeddings: Option<usize>,
    semantic_ready: bool,
    network_loading: bool,
}

pub(crate) fn wants_json(command: &AiCommand) -> bool {
    match command {
        AiCommand::Status { format }
        | AiCommand::Index { format }
        | AiCommand::Search { format, .. }
        | AiCommand::Related { format, .. } => matches!(format, AiOutputFormat::Json),
    }
}

pub(crate) fn execute(command: AiCommand) -> Result<i32, ShellError> {
    match command {
        AiCommand::Status { format } => status(format),
        AiCommand::Index { format } => {
            let report = index::build_default_embeddings()?;
            match format {
                AiOutputFormat::Json => print_json(&report)?,
                AiOutputFormat::Text => println!(
                    "indexed {} command and option documents as {}-dimension vectors with {}",
                    report.documents,
                    report.dimensions,
                    escape_terminal_controls(&report.model)
                ),
            }
            Ok(0)
        }
        AiCommand::Search {
            query,
            limit,
            kind,
            format,
        } => {
            let mut results = index::search_default_database(&query.join(" "), limit)?;
            results.retain(|result| match kind {
                SearchKind::All => true,
                SearchKind::Command => result.kind == "command",
                SearchKind::Option => result.kind == "option",
            });
            present_results(&results, format)?;
            Ok(i32::from(results.is_empty()))
        }
        AiCommand::Related {
            command,
            limit,
            format,
        } => {
            if limit == 0 || limit > intelligence::SEARCH_RESULTS_MAX {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "related suggestions exceeded their result limit",
                )
                .with_context(format!(
                    "limit: {}; observed: {limit}",
                    intelligence::SEARCH_RESULTS_MAX
                ))
                .with_help("Use a result limit between 1 and 100"));
            }
            let command = command.join(" ");
            let requested = limit.checked_add(4).ok_or_else(|| {
                ShellError::new(ErrorCode::ResourceLimit, "related-result limit overflowed")
                    .with_help("Use a result limit between 1 and 100")
            })?;
            let requested = requested.min(intelligence::SEARCH_RESULTS_MAX);
            let mut results = index::search_default_database(&command, requested)?;
            results.retain(|result| result.command != command);
            results.truncate(limit);
            present_results(&results, format)?;
            Ok(i32::from(results.is_empty()))
        }
    }
}

fn status(format: AiOutputFormat) -> Result<i32, ShellError> {
    let database_path = index::default_database_path()?;
    let database_metadata = fs::metadata(&database_path).ok();
    let stats = index::default_database_stats().ok();
    let model_path = intelligence::default_model_path();
    let status = AiStatus {
        database_path,
        database_ready: stats.is_some(),
        database_bytes: database_metadata.map(|metadata| metadata.len()),
        model: "minishlab/potion-base-8M",
        model_ready: model_path
            .as_deref()
            .is_some_and(intelligence::model_is_installed),
        commands: stats.as_ref().map(|stats| stats.commands),
        options: stats.as_ref().map(|stats| stats.arguments),
        semantic_documents: stats.as_ref().map(|stats| stats.documents),
        embeddings: stats.as_ref().map(|stats| stats.embeddings),
        semantic_ready: stats.as_ref().is_some_and(|stats| stats.embeddings > 0),
        model_path,
        network_loading: false,
    };
    match format {
        AiOutputFormat::Json => print_json(&status)?,
        AiOutputFormat::Text => {
            println!(
                "database: {} ({})",
                escape_terminal_controls(&status.database_path.display().to_string()),
                if status.database_ready {
                    "ready"
                } else {
                    "missing"
                }
            );
            let model_path = status
                .model_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "unconfigured".to_owned());
            println!(
                "model: {} at {} ({})",
                status.model,
                escape_terminal_controls(&model_path),
                if status.model_ready {
                    "ready"
                } else {
                    "missing"
                }
            );
            println!("network loading: disabled");
            if let Some(stats) = stats {
                println!(
                    "knowledge: {} commands, {} options, {} documents, {} embeddings",
                    stats.commands, stats.arguments, stats.documents, stats.embeddings
                );
            }
        }
    }
    Ok(i32::from(!status.database_ready))
}

pub(crate) fn present_results(
    results: &[intelligence::SearchResult],
    format: AiOutputFormat,
) -> Result<(), ShellError> {
    match format {
        AiOutputFormat::Json => print_json(results),
        AiOutputFormat::Text => {
            print!("{}", render_results_text(results));
            Ok(())
        }
    }
}

pub(crate) fn render_results_text(results: &[intelligence::SearchResult]) -> String {
    let mut rendered = String::new();
    for result in results {
        use std::fmt::Write as _;
        let _ = writeln!(
            rendered,
            "{:<36} {:>6.3}  {}{}",
            escape_terminal_controls(&result.target),
            result.score,
            if result.semantic {
                "semantic · "
            } else {
                "lexical · "
            },
            escape_terminal_controls(&result.kind)
        );
    }
    rendered
}

fn print_json<T: Serialize + ?Sized>(value: &T) -> Result<(), ShellError> {
    let json = serde_json::to_string_pretty(value).map_err(|error| {
        ShellError::new(ErrorCode::Validation, "could not encode AI command output")
            .with_context(error.to_string())
            .with_help("Retry with text output")
    })?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}
