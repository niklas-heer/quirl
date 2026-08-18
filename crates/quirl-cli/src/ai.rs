//! Local AI command search, option intent lookup, and related suggestions.

use crate::{index, intelligence};
use clap::{Subcommand, ValueEnum};
use quirl_catalog::Catalog;
use quirl_contract::{
    COMMAND_PROPOSAL_VALUE_BYTES_MAX, CommandProposal, CommandProposalRisk,
    CommandProposalRiskReason,
};
use quirl_core::{ErrorCode, ShellError, escape_json_terminal_controls, escape_terminal_controls};
use serde::Serialize;
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
};

const CONFIRMATION_BYTES_MAX: usize = 64;
pub(crate) const NATURAL_PREVIEW_BYTES_MAX: usize = 16 * 1024;

/// Resolve every required retrieval slot from bounded literal user input.
pub(crate) fn resolve_natural_command_slots(
    proposal: &mut CommandProposal,
    catalog: &Catalog,
    reader: &mut impl Read,
    writer: &mut impl Write,
) -> Result<bool, ShellError> {
    loop {
        let validated = proposal.validate(catalog)?;
        let Some(slot) = validated.unresolved_slots().first().cloned() else {
            return Ok(true);
        };
        write!(
            writer,
            "required `{}` ({}); enter a literal value or press Ctrl-D to cancel: ",
            escape_terminal_controls(slot.name()),
            slot.value_kind().name()
        )
        .and_then(|_| writer.flush())
        .map_err(slot_io_error)?;
        let Some(literal) = read_slot_literal(reader)? else {
            return Ok(false);
        };
        let value = slot.parse_value(&literal)?;
        proposal.resolve_slot(&slot, value)?;
    }
}

/// Local command-intelligence operations. None execute suggested commands.
#[derive(Debug, Subcommand)]
pub(crate) enum AiCommand {
    /// Show local database, model, and semantic-index readiness.
    Status {
        /// Output representation for readiness information.
        #[arg(long, value_enum, default_value_t = AiOutputFormat::Text)]
        format: AiOutputFormat,
    },
    /// Embed all current command and option documents with the pinned Quirl model.
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
    /// Propose, preview, confirm, and run one catalog-backed command.
    Run {
        /// Natural-language task description. Retrieval never invents shell text.
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
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
    model: String,
    model_identity: Option<intelligence::ModelIdentity>,
    embedding_index_identity: Option<intelligence::EmbeddingIndexIdentity>,
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
        AiCommand::Run { .. } => false,
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
            let kind = match kind {
                SearchKind::All => intelligence::SearchDocumentKind::All,
                SearchKind::Command => intelligence::SearchDocumentKind::Command,
                SearchKind::Option => intelligence::SearchDocumentKind::Option,
            };
            let results = index::search_default_database_kind(&query.join(" "), limit, kind)?;
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
        AiCommand::Run { .. } => Err(ShellError::new(
            ErrorCode::Validation,
            "natural command execution was not routed through the composition root",
        )
        .with_help("Run the command through the Quirl CLI dispatcher")),
    }
}

pub(crate) fn confirm_natural_command(
    preview: &str,
    risk: CommandProposalRisk,
    risk_reasons: &[CommandProposalRiskReason],
    reader: &mut impl Read,
    writer: &mut impl Write,
) -> Result<bool, ShellError> {
    if preview.len() > NATURAL_PREVIEW_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "natural command preview exceeded its byte limit",
        )
        .with_context(format!(
            "limit: {NATURAL_PREVIEW_BYTES_MAX}; observed: {}",
            preview.len()
        ))
        .with_help("Choose a command with fewer or shorter arguments"));
    }
    writeln!(writer, "exact command preview:")
        .and_then(|_| writeln!(writer, "{}", escape_terminal_controls(preview)))
        .and_then(|_| write!(writer, "type `yes` to accept this generated command: "))
        .and_then(|_| writer.flush())
        .map_err(confirmation_io_error)?;
    if read_confirmation(reader)? != "yes" {
        return Ok(false);
    }
    if risk == CommandProposalRisk::High {
        if risk_reasons.is_empty() {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "high-risk command proposal has no classified reason",
            )
            .with_help("Revalidate the proposal against the current catalog"));
        }
        writeln!(writer, "high-risk effects:").map_err(confirmation_io_error)?;
        for reason in risk_reasons {
            writeln!(writer, "- {}", reason.description()).map_err(confirmation_io_error)?;
        }
        write!(writer, "type `run high-risk` to confirm these effects: ")
            .and_then(|_| writer.flush())
            .map_err(confirmation_io_error)?;
        if read_confirmation(reader)? != "run high-risk" {
            return Ok(false);
        }
    }
    Ok(true)
}

fn read_slot_literal(reader: &mut impl Read) -> Result<Option<String>, ShellError> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let count = reader.read(&mut byte).map_err(slot_io_error)?;
        if count == 0 {
            if bytes.is_empty() {
                return Ok(None);
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() == COMMAND_PROPOSAL_VALUE_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "natural command slot value exceeded its byte limit",
            )
            .with_context(format!(
                "limit: {COMMAND_PROPOSAL_VALUE_BYTES_MAX}; observed: at least {}",
                bytes.len() + 1
            ))
            .with_help("Enter a shorter literal value"));
        }
        bytes.push(byte[0]);
    }
    let value = String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "natural command slot value is not valid UTF-8",
        )
        .with_context(error.to_string())
        .with_help("Enter the literal as UTF-8 text")
    })?;
    Ok(Some(value.trim_end_matches('\r').to_owned()))
}

fn read_confirmation(reader: &mut impl Read) -> Result<String, ShellError> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0_u8; 1];
        let count = reader.read(&mut byte).map_err(confirmation_io_error)?;
        if count == 0 || byte[0] == b'\n' {
            break;
        }
        if bytes.len() == CONFIRMATION_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "natural command confirmation exceeded its byte limit",
            )
            .with_context(format!(
                "limit: {CONFIRMATION_BYTES_MAX}; observed: at least {}",
                bytes.len() + 1
            ))
            .with_help("Type exactly the short confirmation phrase shown in the prompt"));
        }
        bytes.push(byte[0]);
    }
    let value = String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "natural command confirmation is not valid UTF-8",
        )
        .with_context(error.to_string())
        .with_help("Type the confirmation phrase as UTF-8 text")
    })?;
    Ok(value.trim_end_matches('\r').to_owned())
}

fn confirmation_io_error(error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        "could not read or display natural command confirmation",
    )
    .with_context(error.to_string())
    .with_help("Check terminal input and output, then retry")
}

fn slot_io_error(error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        "could not read or display a natural command slot",
    )
    .with_context(error.to_string())
    .with_help("Check terminal input and output, then retry")
}

fn status(format: AiOutputFormat) -> Result<i32, ShellError> {
    let database_path = index::default_database_path()?;
    let database_metadata = fs::metadata(&database_path).ok();
    let stats = index::default_database_stats().ok();
    let embedding_index_identity = if stats.is_some() {
        index::default_embedding_index_identity()?
    } else {
        None
    };
    let model_path = intelligence::default_model_path();
    let model_identity = model_path.as_deref().and_then(intelligence::model_identity);
    let semantic_ready = index::default_embeddings_are_current().unwrap_or(false);
    let status = AiStatus {
        database_path,
        database_ready: stats.is_some(),
        database_bytes: database_metadata.map(|metadata| metadata.len()),
        model: model_identity
            .as_ref()
            .map(|identity| identity.repository.clone())
            .unwrap_or_else(|| crate::ai_bootstrap::MODEL_REPOSITORY.to_owned()),
        model_identity: model_identity.clone(),
        embedding_index_identity,
        model_ready: model_identity.is_some(),
        commands: stats.as_ref().map(|stats| stats.commands),
        options: stats.as_ref().map(|stats| stats.arguments),
        semantic_documents: stats.as_ref().map(|stats| stats.documents),
        embeddings: stats.as_ref().map(|stats| stats.embeddings),
        semantic_ready,
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
                escape_terminal_controls(&status.model),
                escape_terminal_controls(&model_path),
                if status.model_ready {
                    "ready"
                } else {
                    "missing"
                }
            );
            println!("network loading: disabled");
            if let Some(identity) = &status.model_identity {
                println!(
                    "model identity: {}",
                    escape_terminal_controls(&identity.identity)
                );
            }
            if let Some(identity) = &status.embedding_index_identity {
                println!(
                    "embedding index: {} documents, generation {}, fingerprint {}",
                    identity.document_count,
                    identity.document_generation_version,
                    escape_terminal_controls(&identity.index_fingerprint)
                );
                println!(
                    "embedding model identity: {}",
                    escape_terminal_controls(&identity.model_identity)
                );
            }
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
            match result.mode {
                intelligence::RetrievalMode::Lexical => "lexical · ",
                intelligence::RetrievalMode::Hybrid => "hybrid · ",
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

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_catalog::Catalog;
    use std::path::Path;

    #[test]
    fn text_results_name_the_actual_retrieval_mode() {
        let bytes = intelligence::encode_database(&Catalog::builtin(), None).unwrap();
        let results = intelligence::search(
            &bytes,
            Path::new("catalog.sqlite3"),
            "change directory",
            8,
            None,
        )
        .unwrap();
        let rendered = render_results_text(&results);
        assert!(rendered.contains("lexical · command"));
        assert!(!rendered.contains("semantic ·"));
    }

    #[test]
    fn ordinary_generated_command_requires_exact_acceptance() {
        let mut output = Vec::new();
        assert!(
            confirm_natural_command(
                "'pwd'",
                CommandProposalRisk::Ordinary,
                &[],
                &mut &b"yes\n"[..],
                &mut output,
            )
            .unwrap()
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("exact command preview")
        );

        assert!(
            !confirm_natural_command(
                "'pwd'",
                CommandProposalRisk::Ordinary,
                &[],
                &mut &b"no\n"[..],
                &mut Vec::new(),
            )
            .unwrap()
        );
    }

    #[test]
    fn high_risk_generated_command_requires_a_distinct_second_confirmation() {
        let mut output = Vec::new();
        assert!(
            confirm_natural_command(
                "'rm'",
                CommandProposalRisk::High,
                &[CommandProposalRiskReason::WriteFilesystem],
                &mut &b"yes\nrun high-risk\n"[..],
                &mut output,
            )
            .unwrap()
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("filesystem writes or deletion")
        );
        assert!(
            !confirm_natural_command(
                "'rm'",
                CommandProposalRisk::High,
                &[CommandProposalRiskReason::WriteFilesystem],
                &mut &b"yes\nyes\n"[..],
                &mut Vec::new(),
            )
            .unwrap()
        );
    }

    #[test]
    fn confirmation_input_and_preview_are_bounded() {
        let oversized = "x".repeat(NATURAL_PREVIEW_BYTES_MAX + 1);
        assert_eq!(
            confirm_natural_command(
                &oversized,
                CommandProposalRisk::Ordinary,
                &[],
                &mut &b"yes\n"[..],
                &mut Vec::new(),
            )
            .unwrap_err()
            .code,
            ErrorCode::ResourceLimit
        );
        let input = vec![b'x'; CONFIRMATION_BYTES_MAX + 1];
        assert_eq!(
            confirm_natural_command(
                "'pwd'",
                CommandProposalRisk::Ordinary,
                &[],
                &mut input.as_slice(),
                &mut Vec::new(),
            )
            .unwrap_err()
            .code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn retrieval_slots_are_resolved_as_catalog_declared_literals() {
        let catalog = Catalog::builtin();
        let command = catalog
            .commands
            .iter()
            .find(|command| command.path == "quirl describe")
            .unwrap();
        let mut proposal = CommandProposal::retrieval_fallback(
            &catalog,
            command.id.clone(),
            "Retrieval selected describe.",
            "fixture-retriever-v1",
        )
        .unwrap();
        let mut output = Vec::new();
        assert!(
            resolve_natural_command_slots(
                &mut proposal,
                &catalog,
                &mut &b"quirl run; touch nope\n"[..],
                &mut output,
            )
            .unwrap()
        );
        assert_eq!(
            proposal
                .validate(&catalog)
                .unwrap()
                .render_trusted()
                .unwrap(),
            "'quirl' 'describe' 'quirl run; touch nope'"
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("required `command`")
        );
    }

    #[test]
    fn retrieval_slot_cancellation_and_input_limits_fail_without_rendering() {
        let catalog = Catalog::builtin();
        let command = catalog
            .commands
            .iter()
            .find(|command| command.path == "quirl describe")
            .unwrap();
        let proposal = CommandProposal::retrieval_fallback(
            &catalog,
            command.id.clone(),
            "Retrieval selected describe.",
            "fixture-retriever-v1",
        )
        .unwrap();
        let mut cancelled = proposal.clone();
        assert!(
            !resolve_natural_command_slots(
                &mut cancelled,
                &catalog,
                &mut &b""[..],
                &mut Vec::new(),
            )
            .unwrap()
        );
        assert!(cancelled.validate(&catalog).unwrap().has_unresolved_slots());

        let mut oversized = proposal;
        let input = vec![b'x'; COMMAND_PROPOSAL_VALUE_BYTES_MAX + 1];
        assert_eq!(
            resolve_natural_command_slots(
                &mut oversized,
                &catalog,
                &mut input.as_slice(),
                &mut Vec::new(),
            )
            .unwrap_err()
            .code,
            ErrorCode::ResourceLimit
        );
        assert!(oversized.validate(&catalog).unwrap().has_unresolved_slots());
    }
}
