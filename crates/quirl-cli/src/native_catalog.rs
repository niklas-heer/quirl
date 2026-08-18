//! Immutable native-command catalog admission and runtime projection.

use quirl_catalog::{
    Catalog, CommandSpec, NativeCatalogDiagnostic, NativeCatalogLimits, NativeCatalogReader,
    NativeDiagnosticKind, NativePlatform,
};
use quirl_core::{ErrorCode, ShellError};
use std::sync::OnceLock;

const EMBEDDED_NATIVE_DATABASE: &[u8] =
    include_bytes!("../../../catalog/generated/catalog.sqlite3");
const EMBEDDED_NATIVE_SOURCE: &str = "catalog/generated/catalog.sqlite3 (embedded)";
const RUNTIME_LIMITS: NativeCatalogLimits = NativeCatalogLimits::embedded();
static EMBEDDED_COMMANDS: OnceLock<Result<Vec<CommandSpec>, ShellError>> = OnceLock::new();

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows",
    target_os = "freebsd"
)))]
compile_error!("the embedded native catalog has no platform projection for this target");

/// Merge admitted native facts while preserving builtin-only operation on failure.
pub(crate) fn merge_embedded(catalog: &mut Catalog) {
    merge_loaded(catalog, embedded_commands());
}

/// Build the immutable builtin-plus-native catalog without local cache or plugins.
pub(crate) fn builtin_native_catalog() -> Catalog {
    let mut catalog = Catalog::builtin();
    merge_embedded(&mut catalog);
    catalog
}

fn merge_loaded(catalog: &mut Catalog, loaded: Result<Vec<CommandSpec>, ShellError>) {
    if let Ok(commands) = loaded {
        catalog.merge(commands);
    }
}

fn embedded_commands() -> Result<Vec<CommandSpec>, ShellError> {
    EMBEDDED_COMMANDS
        .get_or_init(|| load_commands(EMBEDDED_NATIVE_DATABASE, current_platform()))
        .clone()
}

fn load_commands(bytes: &[u8], platform: NativePlatform) -> Result<Vec<CommandSpec>, ShellError> {
    let reader =
        NativeCatalogReader::from_bytes(bytes, RUNTIME_LIMITS).map_err(native_catalog_error)?;
    Ok(reader.project_commands(platform))
}

fn current_platform() -> NativePlatform {
    #[cfg(target_os = "linux")]
    return NativePlatform::Linux;
    #[cfg(target_os = "macos")]
    return NativePlatform::Macos;
    #[cfg(target_os = "windows")]
    return NativePlatform::Windows;
    #[cfg(target_os = "freebsd")]
    return NativePlatform::Freebsd;
}

fn native_catalog_error(diagnostic: NativeCatalogDiagnostic) -> ShellError {
    let code = match diagnostic.kind {
        NativeDiagnosticKind::ResourceLimit => ErrorCode::ResourceLimit,
        NativeDiagnosticKind::Io => ErrorCode::Io,
        NativeDiagnosticKind::Syntax
        | NativeDiagnosticKind::Validation
        | NativeDiagnosticKind::Database => ErrorCode::Validation,
    };
    let mut error = ShellError::new(code, diagnostic.message)
        .with_context(format!("native catalog source: {}", diagnostic.source_name))
        .with_context(format!("embedded artifact: {EMBEDDED_NATIVE_SOURCE}"))
        .with_help(diagnostic.help);
    for context in diagnostic.context {
        error = error.with_context(context);
    }
    if let Some(start) = diagnostic.byte_offset {
        let end = start.saturating_add(diagnostic.byte_length.unwrap_or_default());
        error = error.with_label(None, start, end, "native catalog source location");
    } else {
        error = error.with_label(
            None,
            0,
            0,
            format!("native catalog source: {}", diagnostic.source_name),
        );
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use quirl_catalog::{
        ArgumentKind, CompletionSource, Confidence, Provenance, ProvenanceInfo, Trust,
    };

    #[test]
    fn embedded_catalog_is_deterministic_platform_filtered_and_deeply_projected() {
        let first = load_commands(EMBEDDED_NATIVE_DATABASE, NativePlatform::Macos).unwrap();
        let second = load_commands(EMBEDDED_NATIVE_DATABASE, NativePlatform::Macos).unwrap();
        assert_eq!(first, second);
        assert!(first.iter().any(|command| command.path == "open"));
        assert!(!first.iter().any(|command| command.path == "where"));
        assert!(
            first
                .iter()
                .any(|command| command.path == "docker compose alpha dry-run")
        );
        assert!(first.iter().any(|command| command.aliases == ["g"]));
    }

    #[test]
    fn native_facts_reach_completion_help_and_agent_surfaces() {
        let catalog = builtin_native_catalog();
        let line = "docker compose alpha d";
        let completions = catalog.complete(line, line.len());
        assert!(
            completions
                .iter()
                .any(|item| item.value == "docker compose alpha dry-run")
        );
        assert_eq!(
            catalog.find("g").map(|command| command.path.as_str()),
            Some("git")
        );
        assert!(
            catalog
                .to_markdown()
                .contains("docker compose alpha dry-run")
        );
        assert!(
            catalog
                .find("git")
                .unwrap()
                .details
                .contains("manage source history")
        );
        let agent = crate::agent::installed_agent_catalog(&catalog).unwrap();
        assert!(
            agent
                .commands
                .iter()
                .any(|command| command.path == "docker compose alpha dry-run")
        );
    }

    #[test]
    fn native_facts_reach_the_lsp_catalog_surface() {
        let catalog = builtin_native_catalog();
        let mut service = quirl_lsp::LanguageService::new(catalog);
        let line = "docker compose alpha d";
        service.handle(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {"textDocument": {
                "uri": "file:///native.qrl",
                "languageId": "quirl",
                "version": 1,
                "text": line
            }}
        }));
        let response = service.handle(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "textDocument/completion",
            "params": {
                "textDocument": {"uri": "file:///native.qrl"},
                "position": {"line": 0, "character": line.len()}
            }
        }));
        assert!(
            response[0]["result"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["label"] == "docker compose alpha dry-run")
        );
    }

    #[test]
    fn actions_are_closed_inert_provider_metadata() {
        let commands = load_commands(EMBEDDED_NATIVE_DATABASE, NativePlatform::Linux).unwrap();
        let xdg_open = commands
            .iter()
            .find(|command| command.path == "xdg-open")
            .unwrap();
        let target = xdg_open
            .options
            .iter()
            .find(|argument| argument.kind == ArgumentKind::Positional)
            .unwrap();
        assert_eq!(
            target.values,
            Some(CompletionSource::Dynamic {
                provider: "quirl.native.files".to_owned()
            })
        );
        assert_eq!(target.provenance.confidence, Confidence::High);
        assert_eq!(target.provenance.source, Provenance::External);
    }

    #[test]
    fn native_diagnostic_kinds_map_to_actionable_shell_errors() {
        for (kind, expected) in [
            (NativeDiagnosticKind::Syntax, ErrorCode::Validation),
            (NativeDiagnosticKind::Validation, ErrorCode::Validation),
            (
                NativeDiagnosticKind::ResourceLimit,
                ErrorCode::ResourceLimit,
            ),
            (NativeDiagnosticKind::Io, ErrorCode::Io),
            (NativeDiagnosticKind::Database, ErrorCode::Validation),
        ] {
            let error = native_catalog_error(NativeCatalogDiagnostic {
                kind,
                source_name: "fixture".to_owned(),
                message: "broken catalog".to_owned(),
                byte_offset: Some(2),
                byte_length: Some(3),
                help: "repair the fixture".to_owned(),
                context: vec!["observed: invalid".to_owned()],
            });
            assert_eq!(error.code, expected);
            assert!(!error.details.help.is_empty());
            assert!(!error.details.labels.is_empty());
            assert!(error.details.context.len() >= 3);
        }
    }

    #[test]
    fn corrupt_embedded_shape_falls_back_to_builtins() {
        let loaded = load_commands(b"not sqlite", NativePlatform::Linux);
        let error = loaded.clone().unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("SQLite") || error.message.contains("database"));

        let mut catalog = Catalog::builtin();
        let expected = catalog.clone();
        merge_loaded(&mut catalog, loaded);
        assert_eq!(catalog, expected);
    }

    #[test]
    fn exact_builtins_keep_precedence_over_curated_external_facts() {
        let mut catalog = Catalog::builtin();
        let builtin = catalog.find("git commit").unwrap().clone();
        merge_embedded(&mut catalog);
        assert_eq!(catalog.find("git commit"), Some(&builtin));
    }

    #[test]
    fn cache_ties_and_trusted_plugins_keep_precedence() {
        let mut cached = builtin_native_catalog().find("docker").unwrap().clone();
        cached.summary = "Locally admitted Docker metadata".to_owned();
        cached.provenance = ProvenanceInfo {
            source: Provenance::Fish,
            confidence: Confidence::High,
            trust: Trust::Declared,
            origin: Some("local cache".to_owned()),
            fingerprint: Some("local-v1".to_owned()),
            generated_at: None,
        };
        let mut catalog = Catalog::builtin();
        catalog.merge([cached]);
        merge_embedded(&mut catalog);
        assert_eq!(
            catalog.find("docker").unwrap().summary,
            "Locally admitted Docker metadata"
        );

        let mut plugin = catalog.find("docker").unwrap().clone();
        plugin.id = "plugin:docker".to_owned();
        plugin.version = Some("1.0.0".to_owned());
        plugin.summary = "Trusted plugin Docker contract".to_owned();
        plugin.provenance = ProvenanceInfo::builtin(Provenance::Plugin);
        catalog.merge([plugin]);
        assert_eq!(
            catalog.find("docker").unwrap().summary,
            "Trusted plugin Docker contract"
        );
    }
}
