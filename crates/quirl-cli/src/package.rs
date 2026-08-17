use crate::agent::installed_agent_catalog;
use crate::lua_worker::LuaWorkerRuntime as LuaRuntime;
use clap::{Subcommand, ValueEnum};
use quirl_catalog::Catalog;
use quirl_contract::{
    build_package, parse_package_manifest, DiagnosticSeverity, PackageBuild, PackageBuildOutcome,
    PackageManifest, PackagePublishPlan, PackageSourceAudit, ValidationDiagnostic,
};
use quirl_core::{escape_json_terminal_controls, escape_terminal_controls, ErrorCode, ShellError};
use quirl_lua::HOST_API;
use serde::Serialize;
use std::{
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

const DEFAULT_MANIFEST: &str = "plugin.toml";
const MAX_PACKAGE_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_PACKAGE_ENTRY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug)]
struct PackageRoot {
    path: PathBuf,
    metadata: fs::Metadata,
}

#[derive(Debug, Subcommand)]
pub enum PackageCommand {
    /// Parse and normalize a project package manifest.
    Manifest {
        /// Package manifest to parse; defaults to `plugin.toml`.
        #[arg(long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,
        /// Output representation for the normalized manifest.
        #[arg(long, value_enum, default_value_t = PackageOutputFormat::Text)]
        format: PackageOutputFormat,
    },
    /// Validate the entry, capabilities, and public command metadata quality gate.
    Build {
        /// Package manifest whose project entry is validated.
        #[arg(long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,
        /// Output representation for the build outcome.
        #[arg(long, value_enum, default_value_t = PackageOutputFormat::Text)]
        format: PackageOutputFormat,
    },
    /// Produce a deterministic publish plan without network access.
    Publish {
        /// Package manifest used to construct the publish plan.
        #[arg(long, default_value = DEFAULT_MANIFEST)]
        manifest: PathBuf,
        /// Require the network-free preview path; publication is not yet enabled.
        #[arg(long)]
        dry_run: bool,
        /// Output representation for the publish plan.
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
            let (source, parsed, _) = match read_manifest(&manifest) {
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
    let (manifest_source, manifest, package_root) = read_manifest(path)?;
    let (entry_path, entry_source) = read_package_entry(&package_root, &manifest.package.entry)?;
    let source_audit = audit_package_source(Some(&entry_path), Some(&entry_source));
    let agent_catalog = installed_agent_catalog(catalog)?;
    let installed_capabilities = agent_catalog
        .capabilities
        .iter()
        .map(|capability| capability.name.clone())
        .collect::<Vec<_>>();
    Ok(build_package(
        &manifest,
        manifest_source.as_bytes(),
        Some(&entry_source),
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

fn read_manifest(path: &Path) -> Result<(String, PackageManifest, PackageRoot), ShellError> {
    let resolved_manifest = fs::canonicalize(path).map_err(|error| {
        package_io_error(
            path,
            "package manifest",
            error,
            &format!("Create {DEFAULT_MANIFEST} or pass --manifest <path>"),
        )
    })?;
    let package_path = resolved_manifest
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_owned();
    let package_metadata = fs::metadata(&package_path).map_err(|error| {
        package_io_error(
            &package_path,
            "package directory",
            error,
            "Keep the package directory available while authoring",
        )
    })?;
    let package_root = PackageRoot {
        path: package_path,
        metadata: package_metadata,
    };
    let (file, canonical_path, size) = open_verified_regular_file(
        &resolved_manifest,
        "package manifest",
        &format!("Create {DEFAULT_MANIFEST} or pass --manifest <path>"),
    )?;
    if canonical_path.parent() != Some(package_root.path.as_path()) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "package directory changed while the manifest was opened",
        )
        .with_help("Retry after the package directory stops changing"));
    }
    let bytes = read_bounded_file(
        file,
        size,
        MAX_PACKAGE_MANIFEST_BYTES,
        "package manifest",
        "Keep plugin.toml at or below 256 KiB",
    )?;
    let source = String::from_utf8(bytes).map_err(|error| {
        ShellError::new(ErrorCode::Validation, "package manifest is not valid UTF-8")
            .with_context(error.to_string())
            .with_help("Encode plugin.toml as UTF-8")
    })?;
    let manifest = parse_package_manifest(&source, &path.display().to_string())?;
    verify_package_root(&package_root)?;
    Ok((source, manifest, package_root))
}

fn read_package_entry(
    package_root: &PackageRoot,
    entry: &str,
) -> Result<(PathBuf, Vec<u8>), ShellError> {
    let entry = safe_entry_path(entry)?;
    verify_package_root(package_root)?;
    let candidate = package_root.path.join(entry);
    let resolved_candidate = fs::canonicalize(&candidate).map_err(|error| {
        package_io_error(
            &candidate,
            "package entry",
            error,
            "Correct the package entry and keep it inside the package directory",
        )
    })?;
    if !resolved_candidate.starts_with(&package_root.path) {
        return Err(package_entry_escape_error(
            &package_root.path,
            &resolved_candidate,
        ));
    }
    let (file, canonical_entry, size) = open_verified_regular_file(
        &resolved_candidate,
        "package entry",
        "Correct the package entry and keep it inside the package directory",
    )?;
    if !canonical_entry.starts_with(&package_root.path) {
        return Err(package_entry_escape_error(
            &package_root.path,
            &canonical_entry,
        ));
    }
    verify_package_root(package_root)?;
    let bytes = read_bounded_file(
        file,
        size,
        MAX_PACKAGE_ENTRY_BYTES,
        "package entry",
        "Keep the Lua entry at or below 4 MiB",
    )?;
    Ok((canonical_entry, bytes))
}

fn verify_package_root(package_root: &PackageRoot) -> Result<(), ShellError> {
    let current = fs::metadata(&package_root.path).map_err(|error| {
        package_io_error(
            &package_root.path,
            "package directory",
            error,
            "Retry after the package directory stops changing",
        )
    })?;
    if !same_file_identity(&package_root.metadata, &current) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "package directory changed while package files were read",
        )
        .with_help("Retry after the package directory stops changing"));
    }
    Ok(())
}

fn package_entry_escape_error(package_root: &Path, resolved_entry: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "package entry resolves outside its package directory",
    )
    .with_context(format!(
        "package_root: {}; resolved_entry: {}",
        package_root.display(),
        resolved_entry.display()
    ))
    .with_help("Keep the entry inside the package; external symlink targets are rejected")
}

fn safe_entry_path(entry: &str) -> Result<PathBuf, ShellError> {
    let entry = Path::new(entry);
    if entry.is_absolute()
        || entry.as_os_str().is_empty()
        || entry.extension().is_none_or(|extension| extension != "lua")
        || !entry
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "package entry must be a relative .lua path inside its package directory",
        )
        .with_help("Use a relative .lua entry path without parent components"));
    }
    Ok(entry.to_owned())
}

fn open_verified_regular_file(
    path: &Path,
    context: &str,
    help: &str,
) -> Result<(fs::File, PathBuf, u64), ShellError> {
    let initial_metadata =
        fs::metadata(path).map_err(|error| package_io_error(path, context, error, help))?;
    if !initial_metadata.is_file() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("{context} {} is not a regular file", path.display()),
        )
        .with_help(help));
    }
    let file =
        open_nonblocking(path).map_err(|error| package_io_error(path, context, error, help))?;
    let metadata = file
        .metadata()
        .map_err(|error| package_io_error(path, context, error, help))?;
    if !metadata.is_file() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("{context} {} is not a regular file", path.display()),
        )
        .with_help(help));
    }
    let canonical_path =
        fs::canonicalize(path).map_err(|error| package_io_error(path, context, error, help))?;
    let resolved_metadata = fs::metadata(&canonical_path)
        .map_err(|error| package_io_error(&canonical_path, context, error, help))?;
    if !same_file_identity(&metadata, &resolved_metadata) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("{context} changed while it was being opened"),
        )
        .with_help("Retry after package files stop changing; symlink swaps are rejected"));
    }
    Ok((file, canonical_path, metadata.len()))
}

#[cfg(unix)]
fn open_nonblocking(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_nonblocking(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    match (
        left.volume_serial_number(),
        left.file_index(),
        right.volume_serial_number(),
        right.file_index(),
    ) {
        (Some(left_volume), Some(left_index), Some(right_volume), Some(right_index)) => {
            left_volume == right_volume && left_index == right_index
        }
        _ => false,
    }
}

#[cfg(not(any(unix, windows)))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    let _ = (left, right);
    false
}

fn read_bounded_file<R: Read>(
    file: R,
    size: u64,
    limit: usize,
    context: &str,
    help: &str,
) -> Result<Vec<u8>, ShellError> {
    if size > limit as u64 {
        return Err(package_limit_error(context, size, limit, help));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| package_read_error(context, error, help))?;
    if bytes.len() > limit {
        return Err(package_limit_error(
            context,
            bytes.len() as u64,
            limit,
            help,
        ));
    }
    Ok(bytes)
}

fn package_io_error(path: &Path, context: &str, error: std::io::Error, help: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not read {context} {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help(help)
}

fn package_read_error(context: &str, error: std::io::Error, help: &str) -> ShellError {
    ShellError::new(ErrorCode::Io, format!("could not read {context}"))
        .with_context(error.to_string())
        .with_help(help)
}

fn package_limit_error(context: &str, observed: u64, limit: usize, help: &str) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{context} exceeds its read limit"),
    )
    .with_context(format!("observed_bytes: {observed}; limit_bytes: {limit}"))
    .with_help(help)
}

fn print_manifest(path: &Path, manifest: &PackageManifest) {
    println!(
        "{} {} ({})",
        escape_terminal_controls(&manifest.package.name),
        escape_terminal_controls(&manifest.package.version),
        escape_terminal_controls(&path.display().to_string())
    );
    println!("  {}", escape_terminal_controls(&manifest.package.summary));
    println!(
        "  entry: {}",
        escape_terminal_controls(&manifest.package.entry)
    );
    println!(
        "  Quirl: {}",
        escape_terminal_controls(&manifest.package.quirl)
    );
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
            escape_terminal_controls(&manifest.capabilities.request.join(", "))
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
            escape_terminal_controls(&path.display().to_string()),
            outcome.diagnostics.len()
        );
        for diagnostic in &outcome.diagnostics {
            println!(
                "  {} at {}: {}\n    help: {}",
                escape_terminal_controls(&diagnostic.code),
                escape_terminal_controls(&diagnostic.path),
                escape_terminal_controls(&diagnostic.message),
                escape_terminal_controls(&diagnostic.help)
            );
        }
    }
    Ok(())
}

fn print_build(path: &Path, build: &PackageBuild) {
    println!(
        "✓ built {} {} from {}",
        escape_terminal_controls(&build.package_name),
        escape_terminal_controls(&build.package_version),
        escape_terminal_controls(&path.display().to_string())
    );
    println!(
        "  manifest: {}",
        escape_terminal_controls(&build.manifest_hash)
    );
    println!("  entry: {}", escape_terminal_controls(&build.entry_hash));
    println!(
        "  host API: {}",
        escape_terminal_controls(&build.host_api_hash)
    );
    println!(
        "  {} public commands passed metadata quality gates",
        build.public_commands.len()
    );
}

fn print_publish_plan(plan: &PackagePublishPlan) {
    println!(
        "✓ dry-run publish plan for {} {}",
        escape_terminal_controls(&plan.package_name),
        escape_terminal_controls(&plan.package_version)
    );
    println!("  build: {}", escape_terminal_controls(&plan.build_hash));
    println!(
        "  files: {}",
        escape_terminal_controls(&plan.files.join(", "))
    );
    println!(
        "  capabilities: {}",
        if plan.requested_capabilities.is_empty() {
            "none".to_owned()
        } else {
            escape_terminal_controls(&plan.requested_capabilities.join(", "))
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
    let json = serde_json::to_string_pretty(value).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not serialize package output")
            .with_context(error.to_string())
            .with_help("Report this as a Quirl package schema defect")
    })?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn test_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "quirl-package-{name}-{}-{}",
            std::process::id(),
            TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn entry_resolution_stays_inside_manifest_directory() {
        assert_eq!(
            safe_entry_path("src/plugin.lua").unwrap(),
            PathBuf::from("src/plugin.lua")
        );
        assert!(safe_entry_path("../plugin.lua").is_err());
        assert!(safe_entry_path("/tmp/plugin.lua").is_err());
        assert!(safe_entry_path("plugin.txt").is_err());
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

    #[test]
    fn package_reads_enforce_manifest_and_entry_limits() {
        let directory = test_directory("limits");
        let manifest = directory.join(DEFAULT_MANIFEST);
        fs::write(
            &manifest,
            vec![b'x'; MAX_PACKAGE_MANIFEST_BYTES.saturating_add(1)],
        )
        .unwrap();
        let manifest_error = read_manifest(&manifest).unwrap_err();
        assert_eq!(manifest_error.code, ErrorCode::ResourceLimit);
        assert!(manifest_error.details.context.iter().any(|context| {
            context.contains("observed_bytes") && context.contains("limit_bytes")
        }));

        fs::write(&manifest, "manifest placeholder").unwrap();
        fs::write(
            directory.join("entry.lua"),
            vec![b'x'; MAX_PACKAGE_ENTRY_BYTES.saturating_add(1)],
        )
        .unwrap();
        let canonical_package = fs::canonicalize(&directory).unwrap();
        let package_root = PackageRoot {
            metadata: fs::metadata(&canonical_package).unwrap(),
            path: canonical_package,
        };
        let entry_error = read_package_entry(&package_root, "entry.lua").unwrap_err();
        assert_eq!(entry_error.code, ErrorCode::ResourceLimit);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bounded_package_read_accepts_exact_limit_across_partial_reads() {
        struct OneByteReader(Cursor<Vec<u8>>);

        impl Read for OneByteReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let bytes_max = buffer.len().min(1);
                self.0.read(&mut buffer[..bytes_max])
            }
        }

        let source = vec![b'x'; MAX_PACKAGE_MANIFEST_BYTES];
        let bytes = read_bounded_file(
            OneByteReader(Cursor::new(source)),
            MAX_PACKAGE_MANIFEST_BYTES as u64,
            MAX_PACKAGE_MANIFEST_BYTES,
            "package manifest",
            "shrink the manifest",
        )
        .unwrap();
        assert_eq!(bytes.len(), MAX_PACKAGE_MANIFEST_BYTES);
    }

    #[test]
    fn bounded_package_read_detects_growth_after_metadata() {
        let directory = test_directory("growth");
        let path = directory.join("entry.lua");
        fs::write(&path, b"return {}\n").unwrap();
        let file = fs::File::open(&path).unwrap();
        let observed_before_growth = file.metadata().unwrap().len();
        fs::write(&path, vec![b'x'; MAX_PACKAGE_ENTRY_BYTES.saturating_add(1)]).unwrap();

        let error = read_bounded_file(
            file,
            observed_before_growth,
            MAX_PACKAGE_ENTRY_BYTES,
            "package entry",
            "shrink the entry",
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bounded_package_read_uses_the_opened_file_after_path_replacement() {
        let directory = test_directory("opened-handle");
        let path = directory.join("entry.lua");
        let moved = directory.join("opened.lua");
        fs::write(&path, b"return 'reviewed'\n").unwrap();
        let file = fs::File::open(&path).unwrap();
        let size = file.metadata().unwrap().len();
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, b"return 'replacement'\n").unwrap();

        let bytes = read_bounded_file(
            file,
            size,
            MAX_PACKAGE_ENTRY_BYTES,
            "package entry",
            "shrink the entry",
        )
        .unwrap();
        assert_eq!(bytes, b"return 'reviewed'\n");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn package_files_must_be_regular_files_and_manifests_must_be_utf8() {
        let directory = test_directory("file-kind");
        let directory_error = read_manifest(&directory).unwrap_err();
        assert_eq!(directory_error.code, ErrorCode::Validation);

        let manifest = directory.join(DEFAULT_MANIFEST);
        fs::write(&manifest, [0xff]).unwrap();
        let encoding_error = read_manifest(&manifest).unwrap_err();
        assert_eq!(encoding_error.code, ErrorCode::Validation);
        assert!(encoding_error.message.contains("UTF-8"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn package_entry_directory_is_rejected_as_a_special_file() {
        let directory = test_directory("entry-directory");
        let canonical_package = fs::canonicalize(&directory).unwrap();
        let package_root = PackageRoot {
            metadata: fs::metadata(&canonical_package).unwrap(),
            path: canonical_package,
        };
        fs::create_dir_all(directory.join("entry.lua")).unwrap();

        let error = read_package_entry(&package_root, "entry.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("regular file"));

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn package_entry_socket_is_rejected_before_opening() {
        use std::os::unix::net::UnixListener;

        let directory = test_directory("entry-socket");
        let canonical_package = fs::canonicalize(&directory).unwrap();
        let package_root = PackageRoot {
            metadata: fs::metadata(&canonical_package).unwrap(),
            path: canonical_package,
        };
        let socket_path = directory.join("entry.lua");
        let listener = UnixListener::bind(&socket_path).unwrap();

        let error = read_package_entry(&package_root, "entry.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("regular file"));

        drop(listener);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn package_entry_symlink_cannot_escape_the_canonical_package_root() {
        use std::os::unix::fs::symlink;

        let root = test_directory("symlink-escape");
        let package = root.join("package");
        fs::create_dir_all(&package).unwrap();
        let manifest = package.join(DEFAULT_MANIFEST);
        fs::write(&manifest, "manifest placeholder").unwrap();
        let outside = root.join("outside.lua");
        fs::write(&outside, "return {}\n").unwrap();
        symlink(&outside, package.join("entry.lua")).unwrap();

        let canonical_package = fs::canonicalize(&package).unwrap();
        let package_root = PackageRoot {
            metadata: fs::metadata(&canonical_package).unwrap(),
            path: canonical_package,
        };
        let error = read_package_entry(&package_root, "entry.lua").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("outside"));

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn package_entry_symlink_inside_the_package_uses_verified_target_bytes() {
        use std::os::unix::fs::symlink;

        let package = test_directory("contained-symlink");
        let target = package.join("target.lua");
        fs::write(&target, "return 'contained'\n").unwrap();
        symlink(&target, package.join("entry.lua")).unwrap();
        let canonical_package = fs::canonicalize(&package).unwrap();
        let package_root = PackageRoot {
            metadata: fs::metadata(&canonical_package).unwrap(),
            path: canonical_package,
        };

        let (path, bytes) = read_package_entry(&package_root, "entry.lua").unwrap();
        assert_eq!(path, fs::canonicalize(target).unwrap());
        assert_eq!(bytes, b"return 'contained'\n");

        fs::remove_dir_all(package).unwrap();
    }

    #[test]
    fn package_root_replacement_is_detected() {
        let root = test_directory("root-replacement");
        let package = root.join("package");
        let moved = root.join("moved");
        fs::create_dir_all(&package).unwrap();
        let canonical_package = fs::canonicalize(&package).unwrap();
        let package_root = PackageRoot {
            metadata: fs::metadata(&canonical_package).unwrap(),
            path: canonical_package,
        };
        fs::rename(&package, &moved).unwrap();
        fs::create_dir(&package).unwrap();

        let error = verify_package_root(&package_root).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("changed"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn package_entry_rejects_invalid_utf8_without_execution() {
        let audit = audit_package_source(Some(Path::new("entry.lua")), Some(&[0xff]));
        assert!(audit
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.entry_utf8"));
    }
}
