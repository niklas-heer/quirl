use crate::agent::installed_agent_catalog;
use clap::{Subcommand, ValueEnum};
use quirl_catalog::Catalog;
use quirl_contract::{
    build_package, parse_package_manifest, DiagnosticSeverity, PackageBuild, PackageBuildOutcome,
    PackageManifest, PackagePublishPlan, PackageSourceAudit, ValidationDiagnostic,
};
use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{LuaRuntime, HOST_API};
use serde::Serialize;
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const DEFAULT_MANIFEST: &str = "plugin.toml";

#[derive(Debug, Subcommand)]
pub enum PackageCommand {
    /// Parse and normalize a project package manifest.
    Manifest {
        #[arg(long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,
        #[arg(long, value_enum, default_value_t = PackageOutputFormat::Text)]
        format: PackageOutputFormat,
    },
    /// Validate the entry, capabilities, and public command metadata quality gate.
    Build {
        #[arg(long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,
        #[arg(long, value_enum, default_value_t = PackageOutputFormat::Text)]
        format: PackageOutputFormat,
    },
    /// Produce a deterministic publish plan without network access.
    Publish {
        #[arg(long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, value_enum, default_value_t = PackageOutputFormat::Text)]
        format: PackageOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PackageOutputFormat {
    Text,
    Json,
}

pub fn wants_json(command: &PackageCommand) -> bool {
    matches!(
        command,
        PackageCommand::Manifest {
            format: PackageOutputFormat::Json,
            ..
        } | PackageCommand::Build {
            format: PackageOutputFormat::Json,
            ..
        } | PackageCommand::Publish {
            format: PackageOutputFormat::Json,
            ..
        }
    )
}

pub fn execute(command: PackageCommand, catalog: &Catalog) -> Result<i32, ShellError> {
    match command {
        PackageCommand::Manifest { manifest, format } => {
            let (source, parsed) = match read_manifest(&manifest) {
                Ok(value) => value,
                Err(error) => return format_error(error, format),
            };
            let _ = source;
            match format {
                PackageOutputFormat::Json => print_json(&parsed)?,
                PackageOutputFormat::Text => print_manifest(&manifest, &parsed),
            }
            Ok(0)
        }
        PackageCommand::Build { manifest, format } => {
            let outcome = match project_build(&manifest, catalog) {
                Ok(outcome) => outcome,
                Err(error) => return format_error(error, format),
            };
            print_build_outcome(&manifest, &outcome, format)?;
            Ok(i32::from(!outcome.valid))
        }
        PackageCommand::Publish {
            manifest,
            dry_run,
            format,
        } => {
            if !dry_run {
                return Err(ShellError::new(
                    ErrorCode::InvalidArgument,
                    "package publishing is network-disabled in Phase 2",
                )
                .with_help(
                    "Inspect the complete publish plan with `quirl package publish --dry-run`",
                ));
            }
            let outcome = match project_build(&manifest, catalog) {
                Ok(outcome) => outcome,
                Err(error) => return format_error(error, format),
            };
            let Some(build) = outcome.build.as_ref() else {
                print_build_outcome(&manifest, &outcome, format)?;
                return Ok(1);
            };
            let plan = PackagePublishPlan::dry_run(build)?;
            match format {
                PackageOutputFormat::Json => print_json(&plan)?,
                PackageOutputFormat::Text => print_publish_plan(&plan),
            }
            Ok(0)
        }
    }
}

fn project_build(path: &Path, catalog: &Catalog) -> Result<PackageBuildOutcome, ShellError> {
    let (manifest_source, manifest) = read_manifest(path)?;
    let entry_path = safe_entry_path(path, &manifest.package.entry);
    let entry_source = entry_path.as_ref().and_then(|entry| fs::read(entry).ok());
    let source_audit = audit_package_source(entry_path.as_deref(), entry_source.as_deref());
    let agent_catalog = installed_agent_catalog(catalog)?;
    let installed_capabilities = agent_catalog
        .capabilities
        .iter()
        .map(|capability| capability.name.clone())
        .collect::<Vec<_>>();
    Ok(build_package(
        &manifest,
        manifest_source.as_bytes(),
        entry_source.as_deref(),
        &installed_capabilities,
        &agent_catalog.host_api_hash,
        env!("CARGO_PKG_VERSION"),
        &path.display().to_string(),
        &source_audit,
    ))
}

fn audit_package_source(
    entry_path: Option<&Path>,
    entry_source: Option<&[u8]>,
) -> PackageSourceAudit {
    let Some(source_bytes) = entry_source else {
        return PackageSourceAudit::default();
    };
    let source_name = entry_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "package entry".to_owned());
    let source = match std::str::from_utf8(source_bytes) {
        Ok(source) => source,
        Err(error) => {
            return PackageSourceAudit {
                diagnostics: vec![ValidationDiagnostic {
                    code: "package.entry_utf8".to_owned(),
                    severity: DiagnosticSeverity::Error,
                    message: format!("package Lua entry is not valid UTF-8: {error}"),
                    path: "package.entry".to_owned(),
                    help: "Save the Lua entry as UTF-8 text".to_owned(),
                }],
                ..PackageSourceAudit::default()
            };
        }
    };

    let mut audit = PackageSourceAudit::default();
    if let Err(error) = LuaRuntime::check_source(source, &source_name) {
        audit.diagnostics.push(ValidationDiagnostic {
            code: "package.entry_lint".to_owned(),
            severity: DiagnosticSeverity::Error,
            message: error.message,
            path: "package.entry".to_owned(),
            help: error.details.help.first().cloned().unwrap_or_else(|| {
                "Fix the Lua parse or lint diagnostic before building".to_owned()
            }),
        });
    }
    for spec in HOST_API {
        if !contains_direct_lua_call(source, spec.path) {
            continue;
        }
        if let Some(capability) = spec.capability {
            audit.detected_capabilities.push(capability.to_owned());
        }
        if spec.path == "quirl.process.run" {
            audit
                .detected_effects
                .push(quirl_catalog::Effect::SpawnProcess);
        }
    }
    audit.detected_capabilities.sort();
    audit.detected_capabilities.dedup();
    audit.detected_effects.dedup();
    audit
}

/// Find a direct `path(...)` expression while ignoring Lua comments and string literals.
fn contains_direct_lua_call(source: &str, path: &str) -> bool {
    let bytes = source.as_bytes();
    let needle = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"--[[") {
            index = skip_until(bytes, index + 4, b"]]");
        } else if bytes[index..].starts_with(b"--") {
            index = skip_until(bytes, index + 2, b"\n");
        } else if bytes[index..].starts_with(b"[[") {
            index = skip_until(bytes, index + 2, b"]]");
        } else if matches!(bytes[index], b'\'' | b'"') {
            index = skip_quoted(bytes, index + 1, bytes[index]);
        } else if bytes[index..].starts_with(needle)
            && (index == 0
                || !matches!(bytes[index - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'.'))
        {
            let mut after = index + needle.len();
            while after < bytes.len() && bytes[after].is_ascii_whitespace() {
                after += 1;
            }
            if bytes.get(after) == Some(&b'(') {
                return true;
            }
            index += needle.len();
        } else {
            index += 1;
        }
    }
    false
}

fn skip_until(bytes: &[u8], mut index: usize, terminator: &[u8]) -> usize {
    while index < bytes.len() && !bytes[index..].starts_with(terminator) {
        index += 1;
    }
    (index + terminator.len()).min(bytes.len())
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    index
}

fn read_manifest(path: &Path) -> Result<(String, PackageManifest), ShellError> {
    let source = fs::read_to_string(path).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not read package manifest {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help(format!(
            "Create {DEFAULT_MANIFEST} or pass --manifest <path>"
        ))
    })?;
    let manifest = parse_package_manifest(&source, &path.display().to_string())?;
    Ok((source, manifest))
}

fn safe_entry_path(manifest: &Path, entry: &str) -> Option<PathBuf> {
    let entry = Path::new(entry);
    if entry.is_absolute()
        || entry.extension().is_none_or(|extension| extension != "lua")
        || !entry
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    Some(
        manifest
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(entry),
    )
}

fn print_manifest(path: &Path, manifest: &PackageManifest) {
    println!(
        "{} {} ({})",
        manifest.package.name,
        manifest.package.version,
        path.display()
    );
    println!("  {}", manifest.package.summary);
    println!("  entry: {}", manifest.package.entry);
    println!("  Quirl: {}", manifest.package.quirl);
    println!(
        "  contributions: {} commands, {} panels, {} indexers",
        manifest.contributes.commands.len(),
        manifest.contributes.panels.len(),
        manifest.contributes.indexers.len()
    );
    println!(
        "  requested capabilities: {}",
        if manifest.capabilities.request.is_empty() {
            "none".to_owned()
        } else {
            manifest.capabilities.request.join(", ")
        }
    );
}

fn print_build_outcome(
    path: &Path,
    outcome: &PackageBuildOutcome,
    format: PackageOutputFormat,
) -> Result<(), ShellError> {
    if matches!(format, PackageOutputFormat::Json) {
        return print_json(outcome);
    }
    if let Some(build) = &outcome.build {
        print_build(path, build);
    } else {
        println!(
            "✗ {} failed package validation with {} diagnostics",
            path.display(),
            outcome.diagnostics.len()
        );
        for diagnostic in &outcome.diagnostics {
            println!(
                "  {} at {}: {}\n    help: {}",
                diagnostic.code, diagnostic.path, diagnostic.message, diagnostic.help
            );
        }
    }
    Ok(())
}

fn print_build(path: &Path, build: &PackageBuild) {
    println!(
        "✓ built {} {} from {}",
        build.package_name,
        build.package_version,
        path.display()
    );
    println!("  manifest: {}", build.manifest_hash);
    println!("  entry: {}", build.entry_hash);
    println!("  host API: {}", build.host_api_hash);
    println!(
        "  {} public commands passed metadata quality gates",
        build.public_commands.len()
    );
}

fn print_publish_plan(plan: &PackagePublishPlan) {
    println!(
        "✓ dry-run publish plan for {} {}",
        plan.package_name, plan.package_version
    );
    println!("  build: {}", plan.build_hash);
    println!("  files: {}", plan.files.join(", "));
    println!(
        "  capabilities: {}",
        if plan.requested_capabilities.is_empty() {
            "none".to_owned()
        } else {
            plan.requested_capabilities.join(", ")
        }
    );
    println!("  network performed: no");
}

fn format_error(error: ShellError, format: PackageOutputFormat) -> Result<i32, ShellError> {
    if matches!(format, PackageOutputFormat::Json) {
        print_json(&error)?;
        Ok(1)
    } else {
        Err(error)
    }
}

fn print_json(value: &impl Serialize) -> Result<(), ShellError> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not serialize package output")
                .with_context(error.to_string())
                .with_help("Report this as a Quirl package schema defect")
        })?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_resolution_stays_inside_manifest_directory() {
        assert_eq!(
            safe_entry_path(Path::new("project/plugin.toml"), "src/plugin.lua"),
            Some(PathBuf::from("project/src/plugin.lua"))
        );
        assert_eq!(
            safe_entry_path(Path::new("project/plugin.toml"), "../plugin.lua"),
            None
        );
        assert_eq!(
            safe_entry_path(Path::new("project/plugin.toml"), "/tmp/plugin.lua"),
            None
        );
    }

    #[test]
    fn publish_requires_explicit_dry_run() {
        use clap::Parser;

        assert!(crate::Cli::try_parse_from(["quirl", "package", "publish"]).is_ok());
        assert!(crate::Cli::try_parse_from(["quirl", "package", "publish", "--dry-run"]).is_ok());
    }

    #[test]
    fn source_audit_lints_without_execution_and_detects_direct_host_calls() {
        let valid = audit_package_source(
            Some(Path::new("plugin.lua")),
            Some(b"return quirl.process.run('echo safe')"),
        );
        assert!(valid.diagnostics.is_empty(), "{:?}", valid.diagnostics);
        assert_eq!(valid.detected_capabilities, vec!["process.spawn"]);
        assert_eq!(
            valid.detected_effects,
            vec![quirl_catalog::Effect::SpawnProcess]
        );

        let invalid = audit_package_source(Some(Path::new("plugin.lua")), Some(b"return ("));
        assert!(invalid
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.entry_lint"));
    }

    #[test]
    fn source_audit_ignores_host_paths_in_comments_and_strings() {
        let audit = audit_package_source(
            Some(Path::new("plugin.lua")),
            Some(b"-- quirl.process.run('no')\nreturn \"quirl.process.run('no')\""),
        );
        assert!(audit.detected_capabilities.is_empty());
        assert!(audit.detected_effects.is_empty());
    }
}
