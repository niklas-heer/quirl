use crate::{stable_hash, DiagnosticSeverity, ValidationDiagnostic, ValidationReport};
use quirl_catalog::Effect;
use quirl_core::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const PACKAGE_SCHEMA_VERSION: u32 = 1;

const PACKAGE_METADATA_SCHEMA: &str = "PackageMetadata{deny_unknown;name:string;version:string;entry:string;quirl:string;summary:string(default-empty);license:string(default-empty)}";
const PACKAGE_CAPABILITY_SCHEMA: &str =
    "PackageCapabilitySection{deny_unknown;request:array<string>(default-empty)}";
const PACKAGE_CONTRIBUTION_SCHEMA: &str = "PackageContributions{deny_unknown;commands:array<string>(default-empty);panels:array<string>(default-empty);indexers:array<string>(default-empty)}";
const PACKAGE_ARGUMENT_SCHEMA: &str = "PackageArgument{deny_unknown;names:array<string>;kind:enum[positional,option,flag];value_type:string;required:bool;repeatable:bool(default-false);documentation:string}";
const PACKAGE_COMMAND_SCHEMA: &str = "PackageCommand{deny_unknown;path:string;signature:string;summary:string;details:string;input_type:string;output_type:string;arguments:array<PackageArgument>(default-empty);examples:array<string>;effects:array<enum[read_filesystem,write_filesystem,spawn_process,change_directory]>;error_codes:map<string,string>}";
pub const PACKAGE_SCHEMA_DESCRIPTOR: &str = "PackageManifest{deny_unknown;schema_version:u32;package:PackageMetadata;capabilities:PackageCapabilitySection(default);contributes:PackageContributions(default);public_commands:array<PackageCommand>(default-empty)}";
pub const PACKAGE_BUILD_SCHEMA_DESCRIPTOR: &str = "PackageBuild{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;manifest_schema_hash:string;package_name:string;package_version:string;resolved_quirl_version:string;manifest_hash:string;entry_hash:string;host_api_hash:string;capabilities:array<string>;public_commands:array<PackageCommand>;files:array<string>}";

pub fn package_manifest_schema_hash() -> String {
    package_structural_hash(&[
        PACKAGE_SCHEMA_DESCRIPTOR,
        PACKAGE_METADATA_SCHEMA,
        PACKAGE_CAPABILITY_SCHEMA,
        PACKAGE_CONTRIBUTION_SCHEMA,
        PACKAGE_COMMAND_SCHEMA,
        PACKAGE_ARGUMENT_SCHEMA,
    ])
}

pub fn package_build_schema_hash() -> String {
    package_structural_hash(&[
        PACKAGE_BUILD_SCHEMA_DESCRIPTOR,
        PACKAGE_COMMAND_SCHEMA,
        PACKAGE_ARGUMENT_SCHEMA,
    ])
}

fn package_structural_hash(parts: &[&str]) -> String {
    let mut descriptor = Vec::new();
    for part in parts {
        descriptor.extend_from_slice(part.as_bytes());
        descriptor.push(0x1f);
    }
    stable_hash(&descriptor)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub schema_version: u32,
    pub package: PackageMetadata,
    #[serde(default)]
    pub capabilities: PackageCapabilitySection,
    #[serde(default)]
    pub contributes: PackageContributions,
    #[serde(default)]
    pub public_commands: Vec<PackageCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    pub entry: String,
    pub quirl: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub license: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageCapabilitySection {
    #[serde(default)]
    pub request: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageContributions {
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub panels: Vec<String>,
    #[serde(default)]
    pub indexers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentKind {
    Positional,
    Option,
    Flag,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageArgument {
    pub names: Vec<String>,
    pub kind: ArgumentKind,
    pub value_type: String,
    pub required: bool,
    #[serde(default)]
    pub repeatable: bool,
    pub documentation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageCommand {
    pub path: String,
    pub signature: String,
    pub summary: String,
    pub details: String,
    pub input_type: String,
    pub output_type: String,
    #[serde(default)]
    pub arguments: Vec<PackageArgument>,
    pub examples: Vec<String>,
    pub effects: Vec<Effect>,
    pub error_codes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageBuild {
    pub document_type: String,
    pub schema_version: u32,
    pub schema_hash: String,
    pub manifest_schema_hash: String,
    pub package_name: String,
    pub package_version: String,
    pub resolved_quirl_version: String,
    pub manifest_hash: String,
    pub entry_hash: String,
    pub host_api_hash: String,
    pub capabilities: Vec<String>,
    pub public_commands: Vec<PackageCommand>,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageBuildOutcome {
    pub document_type: String,
    pub schema_version: u32,
    pub valid: bool,
    pub diagnostics: Vec<ValidationDiagnostic>,
    pub build: Option<PackageBuild>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageSourceAudit {
    pub diagnostics: Vec<ValidationDiagnostic>,
    pub detected_capabilities: Vec<String>,
    pub detected_effects: Vec<Effect>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackagePublishPlan {
    pub document_type: String,
    pub schema_version: u32,
    pub dry_run: bool,
    pub package_name: String,
    pub package_version: String,
    pub build_hash: String,
    pub files: Vec<String>,
    pub requested_capabilities: Vec<String>,
    pub network_performed: bool,
}

pub fn parse_package_manifest(source: &str, origin: &str) -> Result<PackageManifest, ShellError> {
    toml::from_str(source).map_err(|error| {
        let mut diagnostic = ShellError::new(
            ErrorCode::Validation,
            format!("invalid Quirl package manifest {origin}"),
        )
        .with_context(error.to_string())
        .with_help("Use schema_version = 1 and remove unknown manifest fields");
        if let Some(span) = error.span() {
            diagnostic = diagnostic.with_label(
                Some(origin.to_owned()),
                span.start,
                span.end,
                "manifest schema mismatch",
            );
        }
        diagnostic
    })
}

pub fn validate_package_manifest(
    manifest: &PackageManifest,
    entry_available: bool,
    installed_capabilities: &[String],
    quirl_version: &str,
) -> ValidationReport {
    let mut diagnostics = Vec::new();
    if manifest.schema_version != PACKAGE_SCHEMA_VERSION {
        error(
            &mut diagnostics,
            "package.schema_version",
            &format!(
                "unsupported package schema version {}",
                manifest.schema_version
            ),
            "schema_version",
            &format!("Set schema_version = {PACKAGE_SCHEMA_VERSION}"),
        );
    }
    validate_package_metadata(&manifest.package, entry_available, &mut diagnostics);
    if !supports_version(&manifest.package.quirl, quirl_version) {
        error(
            &mut diagnostics,
            "package.quirl_version",
            &format!(
                "installed Quirl {quirl_version} does not satisfy `{}`",
                manifest.package.quirl
            ),
            "package.quirl",
            "Update the declared range or build with a compatible Quirl version",
        );
    }
    validate_capabilities(
        &manifest.capabilities.request,
        installed_capabilities,
        &mut diagnostics,
    );
    validate_contributions(manifest, &mut diagnostics);
    ValidationReport {
        document_type: "quirl.package.validation".to_owned(),
        schema_version: PACKAGE_SCHEMA_VERSION,
        valid: diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != DiagnosticSeverity::Error),
        diagnostics,
    }
}

// These inputs deliberately cross the dependency boundary as plain data: the
// CLI owns filesystem/Lua inspection while this crate owns deterministic build validation.
#[allow(clippy::too_many_arguments)]
pub fn build_package(
    manifest: &PackageManifest,
    manifest_source: &[u8],
    entry_source: Option<&[u8]>,
    installed_capabilities: &[String],
    host_api_hash: &str,
    quirl_version: &str,
    manifest_file: &str,
    source_audit: &PackageSourceAudit,
) -> PackageBuildOutcome {
    let mut validation = validate_package_manifest(
        manifest,
        entry_source.is_some(),
        installed_capabilities,
        quirl_version,
    );
    validation
        .diagnostics
        .extend(source_audit.diagnostics.iter().cloned());
    reconcile_source_contract(manifest, source_audit, &mut validation.diagnostics);
    validation.valid = validation
        .diagnostics
        .iter()
        .all(|diagnostic| diagnostic.severity != DiagnosticSeverity::Error);
    if !validation.valid {
        return PackageBuildOutcome {
            document_type: "quirl.package.build_outcome".to_owned(),
            schema_version: PACKAGE_SCHEMA_VERSION,
            valid: false,
            diagnostics: validation.diagnostics,
            build: None,
        };
    }

    let mut capabilities = manifest.capabilities.request.clone();
    capabilities.sort();
    let mut public_commands = manifest.public_commands.clone();
    public_commands.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest_file =
        package_file_name(manifest_file).unwrap_or_else(|| "plugin.toml".to_owned());
    let entry_file = normalize_package_path(&manifest.package.entry)
        .unwrap_or_else(|| manifest.package.entry.clone());
    let mut files = vec![manifest_file, entry_file];
    files.sort();
    files.dedup();
    let entry_source = entry_source.unwrap_or_default();
    PackageBuildOutcome {
        document_type: "quirl.package.build_outcome".to_owned(),
        schema_version: PACKAGE_SCHEMA_VERSION,
        valid: true,
        diagnostics: Vec::new(),
        build: Some(PackageBuild {
            document_type: "quirl.package.build".to_owned(),
            schema_version: PACKAGE_SCHEMA_VERSION,
            schema_hash: package_build_schema_hash(),
            manifest_schema_hash: package_manifest_schema_hash(),
            package_name: manifest.package.name.clone(),
            package_version: manifest.package.version.clone(),
            resolved_quirl_version: quirl_version.to_owned(),
            manifest_hash: stable_hash(manifest_source),
            entry_hash: stable_hash(entry_source),
            host_api_hash: host_api_hash.to_owned(),
            capabilities,
            public_commands,
            files,
        }),
    }
}

impl PackagePublishPlan {
    pub fn dry_run(build: &PackageBuild) -> Result<Self, ShellError> {
        let build_bytes = serde_json::to_vec(build).map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not serialize package build plan")
                .with_context(error.to_string())
                .with_help("Report this as a Quirl package schema defect")
        })?;
        Ok(Self {
            document_type: "quirl.package.publish_plan".to_owned(),
            schema_version: PACKAGE_SCHEMA_VERSION,
            dry_run: true,
            package_name: build.package_name.clone(),
            package_version: build.package_version.clone(),
            build_hash: stable_hash(&build_bytes),
            files: build.files.clone(),
            requested_capabilities: build.capabilities.clone(),
            network_performed: false,
        })
    }
}

fn validate_package_metadata(
    package: &PackageMetadata,
    entry_available: bool,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    if !valid_package_name(&package.name) {
        error(
            diagnostics,
            "package.name",
            "package name must use lowercase ASCII letters, digits, and single hyphens",
            "package.name",
            "Use a name such as `kubernetes-workbench`",
        );
    }
    if !valid_semver(&package.version) {
        error(
            diagnostics,
            "package.version",
            "package version must be a three-component semantic version",
            "package.version",
            "Use a version such as `0.1.0`",
        );
    }
    if package.summary.trim().is_empty() {
        error(
            diagnostics,
            "package.summary",
            "public packages require a summary",
            "package.summary",
            "Add a concise sentence describing the package",
        );
    }
    if package.quirl.trim().is_empty() {
        error(
            diagnostics,
            "package.quirl",
            "the supported Quirl version range must be explicit",
            "package.quirl",
            "Declare a range such as `>=0.1, <0.2`",
        );
    }
    if !valid_entry(&package.entry) {
        error(
            diagnostics,
            "package.entry",
            "entry must be a relative .lua path contained in the package",
            "package.entry",
            "Use a path such as `plugin.lua` without `..` components",
        );
    } else if !entry_available {
        error(
            diagnostics,
            "package.entry_missing",
            &format!("package entry `{}` is missing", package.entry),
            "package.entry",
            "Create the entry file or correct the manifest path",
        );
    }
}

fn validate_capabilities(
    requested: &[String],
    installed: &[String],
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    validate_sorted_unique(requested, "capabilities.request", diagnostics);
    let installed = installed
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for capability in requested {
        if !installed.contains(capability.as_str()) {
            error(
                diagnostics,
                "package.capability_unavailable",
                &format!("requested capability `{capability}` is not installed"),
                "capabilities.request",
                "Request only capabilities listed by `quirl agent manifest --format json`",
            );
        }
    }
}

fn reconcile_source_contract(
    manifest: &PackageManifest,
    audit: &PackageSourceAudit,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    let requested = manifest
        .capabilities
        .request
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for capability in &audit.detected_capabilities {
        if !requested.contains(capability.as_str()) {
            error(
                diagnostics,
                "package.source_capability_undeclared",
                &format!("Lua entry uses `{capability}` without requesting it"),
                "capabilities.request",
                "Add the detected capability to capabilities.request and review its authority",
            );
        }
    }
    let detected = audit
        .detected_capabilities
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for capability in &manifest.capabilities.request {
        if !detected.contains(capability.as_str()) {
            warning(
                diagnostics,
                "package.source_capability_not_statically_seen",
                &format!("requested capability `{capability}` was not statically observed in the Lua entry"),
                "capabilities.request",
                "Review whether the capability is used indirectly; static source auditing is conservative",
            );
        }
    }
    let declared_effects = manifest
        .public_commands
        .iter()
        .flat_map(|command| command.effects.iter())
        .map(effect_key)
        .collect::<BTreeSet<_>>();
    for effect in &audit.detected_effects {
        if !declared_effects.contains(&effect_key(effect)) {
            error(
                diagnostics,
                "package.source_effect_undeclared",
                &format!("Lua entry has detected `{effect:?}` behavior absent from public command effects"),
                "public_commands.effects",
                "Declare the detected effect on the public command that performs it",
            );
        }
    }
}

fn validate_contributions(manifest: &PackageManifest, diagnostics: &mut Vec<ValidationDiagnostic>) {
    validate_sorted_unique(
        &manifest.contributes.commands,
        "contributes.commands",
        diagnostics,
    );
    if !manifest.contributes.panels.is_empty() {
        error(
            diagnostics,
            "package.phase3_panels",
            "panel contributions are not supported by the Phase 2 package runtime",
            "contributes.panels",
            "Remove panel contributions until the Phase 3 UI extension contract is available",
        );
    }
    if !manifest.contributes.indexers.is_empty() {
        error(
            diagnostics,
            "package.phase3_indexers",
            "indexer contributions are not supported by the Phase 2 package runtime",
            "contributes.indexers",
            "Remove indexer contributions until the Phase 3 extension contract is available",
        );
    }
    validate_sorted_unique(
        &manifest.contributes.panels,
        "contributes.panels",
        diagnostics,
    );
    validate_sorted_unique(
        &manifest.contributes.indexers,
        "contributes.indexers",
        diagnostics,
    );
    let declared = manifest
        .contributes
        .commands
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let documented = manifest
        .public_commands
        .iter()
        .map(|command| command.path.as_str())
        .collect::<BTreeSet<_>>();
    if declared != documented {
        error(
            diagnostics,
            "package.command_metadata",
            "every contributed command must have exactly one public_commands metadata record",
            "public_commands",
            "Add complete metadata for each contributes.commands entry and remove undeclared records",
        );
    }
    if documented.len() != manifest.public_commands.len() {
        error(
            diagnostics,
            "package.command_duplicate",
            "public command paths must be unique",
            "public_commands",
            "Keep one metadata record for each command path",
        );
    }
    for (index, command) in manifest.public_commands.iter().enumerate() {
        validate_public_command(command, index, diagnostics);
    }
}

fn validate_public_command(
    command: &PackageCommand,
    index: usize,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    let base = format!("public_commands[{index}]");
    for (field, value) in [
        ("path", command.path.as_str()),
        ("signature", command.signature.as_str()),
        ("summary", command.summary.as_str()),
        ("details", command.details.as_str()),
        ("input_type", command.input_type.as_str()),
        ("output_type", command.output_type.as_str()),
    ] {
        if value.trim().is_empty() {
            error(
                diagnostics,
                "package.command_quality",
                &format!("public command field `{field}` must not be empty"),
                &format!("{base}.{field}"),
                "Document summaries, signatures, types, and detailed behavior before building",
            );
        }
    }
    if command.examples.is_empty() || command.examples.iter().any(|item| item.trim().is_empty()) {
        error(
            diagnostics,
            "package.command_examples",
            "public commands require at least one non-empty example",
            &format!("{base}.examples"),
            "Add a copyable invocation demonstrating normal use",
        );
    }
    if command.effects.is_empty() {
        error(
            diagnostics,
            "package.command_effects",
            "public commands require at least one explicit effect in Phase 2",
            &format!("{base}.effects"),
            "Declare the command's observable effect before building",
        );
    }
    if command.error_codes.is_empty()
        || command
            .error_codes
            .iter()
            .any(|(code, summary)| code.parse::<i32>().is_err() || summary.trim().is_empty())
    {
        error(
            diagnostics,
            "package.command_error_codes",
            "public commands require numeric error codes with non-empty explanations",
            &format!("{base}.error_codes"),
            "Document at least success and expected failure statuses",
        );
    }
    if (command.signature.contains('<') || command.signature.contains('['))
        && command.arguments.is_empty()
    {
        error(
            diagnostics,
            "package.command_arguments",
            "a signature with arguments requires typed argument metadata",
            &format!("{base}.arguments"),
            "Add names, kind, value_type, required, and documentation for each argument",
        );
    }
    for (argument_index, argument) in command.arguments.iter().enumerate() {
        let path = format!("{base}.arguments[{argument_index}]");
        if argument.names.is_empty()
            || argument.names.iter().any(|name| name.trim().is_empty())
            || argument.value_type.trim().is_empty()
            || argument.documentation.trim().is_empty()
        {
            error(
                diagnostics,
                "package.argument_quality",
                "argument names, value type, and documentation must be complete",
                &path,
                "Complete the public argument contract before building",
            );
        }
    }
}

fn validate_sorted_unique(
    values: &[String],
    path: &str,
    diagnostics: &mut Vec<ValidationDiagnostic>,
) {
    let mut normalized = values.to_vec();
    normalized.sort();
    normalized.dedup();
    if normalized != values {
        error(
            diagnostics,
            "package.order",
            "values must be sorted and unique for deterministic builds",
            path,
            "Sort the values lexicographically and remove duplicates",
        );
    }
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_semver(version: &str) -> bool {
    let (core, prerelease) = version
        .split_once('-')
        .map_or((version, None), |(core, suffix)| (core, Some(suffix)));
    let parts = core.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        && prerelease.is_none_or(|suffix| !suffix.is_empty())
}

fn supports_version(requirement: &str, installed: &str) -> bool {
    let Some(installed) = parse_version(installed) else {
        return false;
    };
    requirement.split(',').all(|constraint| {
        let constraint = constraint.trim();
        let (operator, version) = [">=", "<=", ">", "<", "="]
            .into_iter()
            .find_map(|operator| {
                constraint
                    .strip_prefix(operator)
                    .map(|version| (operator, version.trim()))
            })
            .unwrap_or(("=", constraint));
        let Some(required) = parse_version(version) else {
            return false;
        };
        match operator {
            ">=" => installed >= required,
            "<=" => installed <= required,
            ">" => installed > required,
            "<" => installed < required,
            _ => installed == required,
        }
    })
}

fn parse_version(version: &str) -> Option<[u64; 3]> {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut parsed = [0_u64; 3];
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (index, part) in parts.into_iter().enumerate() {
        parsed[index] = part.parse().ok()?;
    }
    Some(parsed)
}

fn valid_entry(entry: &str) -> bool {
    normalize_package_path(entry).is_some_and(|path| path.ends_with(".lua"))
}

fn normalize_package_path(path: &str) -> Option<String> {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(value) => {
                parts.push(value.to_string_lossy().into_owned());
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn package_file_name(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
}

fn effect_key(effect: &Effect) -> u8 {
    match effect {
        Effect::ReadFilesystem => 0,
        Effect::WriteFilesystem => 1,
        Effect::SpawnProcess => 2,
        Effect::ChangeDirectory => 3,
    }
}

fn error(
    diagnostics: &mut Vec<ValidationDiagnostic>,
    code: &str,
    message: &str,
    path: &str,
    help: &str,
) {
    diagnostics.push(ValidationDiagnostic {
        code: code.to_owned(),
        severity: DiagnosticSeverity::Error,
        message: message.to_owned(),
        path: path.to_owned(),
        help: help.to_owned(),
    });
}

fn warning(
    diagnostics: &mut Vec<ValidationDiagnostic>,
    code: &str,
    message: &str,
    path: &str,
    help: &str,
) {
    diagnostics.push(ValidationDiagnostic {
        code: code.to_owned(),
        severity: DiagnosticSeverity::Warning,
        message: message.to_owned(),
        path: path.to_owned(),
        help: help.to_owned(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
schema_version = 1

[package]
name = "deploy-tools"
version = "0.1.0"
entry = "plugin.lua"
quirl = ">=0.1, <0.2"
summary = "Deploy services with explicit safeguards"
license = "MIT"

[capabilities]
request = ["process.spawn"]

[contributes]
commands = ["deploy"]
panels = []
indexers = []

[[public_commands]]
path = "deploy"
signature = "deploy <environment>"
summary = "Deploy a service"
details = "Deploys one service after validation."
input_type = "Nothing"
output_type = "Result<Deployment>"
examples = ["deploy staging"]
effects = ["spawn_process"]
error_codes = { "0" = "deployed", "1" = "deployment failed" }

[[public_commands.arguments]]
names = ["environment"]
kind = "positional"
value_type = "Environment"
required = true
documentation = "Target deployment environment"
"#;

    #[test]
    fn manifest_schema_denies_unknown_fields() {
        let source = format!("{VALID}\nunknown = true\n");
        let error = parse_package_manifest(&source, "plugin.toml").unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(!error.details.labels.is_empty());
    }

    #[test]
    fn complete_public_metadata_passes_quality_gate() {
        let manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        let report =
            validate_package_manifest(&manifest, true, &["process.spawn".to_owned()], "0.1.0");
        assert!(report.valid, "{:?}", report.diagnostics);
    }

    #[test]
    fn undocumented_public_command_fails_quality_gate() {
        let mut manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        manifest.public_commands[0].examples.clear();
        manifest.public_commands[0].error_codes.clear();
        let report =
            validate_package_manifest(&manifest, true, &["process.spawn".to_owned()], "0.1.0");
        assert!(!report.valid);
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.command_examples"));
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.command_error_codes"));
    }

    #[test]
    fn public_commands_require_effect_and_error_metadata() {
        let mut manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        manifest.public_commands[0].effects.clear();
        manifest.public_commands[0].error_codes.clear();
        let report =
            validate_package_manifest(&manifest, true, &["process.spawn".to_owned()], "0.1.0");
        assert!(!report.valid);
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.command_effects"));
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.command_error_codes"));
    }

    #[test]
    fn phase_three_contributions_are_rejected_in_phase_two() {
        let mut manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        manifest.contributes.panels.push("deploy.status".to_owned());
        manifest
            .contributes
            .indexers
            .push("deploy.targets".to_owned());
        let report =
            validate_package_manifest(&manifest, true, &["process.spawn".to_owned()], "0.1.0");
        assert!(!report.valid);
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.phase3_panels"));
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.phase3_indexers"));
    }

    #[test]
    fn unavailable_capability_fails_validation() {
        let manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        let report = validate_package_manifest(&manifest, true, &[], "0.1.0");
        assert!(!report.valid);
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.capability_unavailable"));
    }

    #[test]
    fn incompatible_quirl_version_fails_validation() {
        let manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        let report =
            validate_package_manifest(&manifest, true, &["process.spawn".to_owned()], "0.2.0");
        assert!(!report.valid);
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.quirl_version"));
    }

    #[test]
    fn build_and_publish_dry_run_are_deterministic_and_network_free() {
        let manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        let source_audit = PackageSourceAudit {
            diagnostics: Vec::new(),
            detected_capabilities: vec!["process.spawn".to_owned()],
            detected_effects: vec![Effect::SpawnProcess],
        };
        let first = build_package(
            &manifest,
            VALID.as_bytes(),
            Some(b"return {}"),
            &["process.spawn".to_owned()],
            "fnv1a64:host",
            "0.1.0",
            "plugin.toml",
            &source_audit,
        );
        let second = build_package(
            &manifest,
            VALID.as_bytes(),
            Some(b"return {}"),
            &["process.spawn".to_owned()],
            "fnv1a64:host",
            "0.1.0",
            "plugin.toml",
            &source_audit,
        );
        assert_eq!(first, second);
        let build = first.build.unwrap();
        let plan = PackagePublishPlan::dry_run(&build).unwrap();
        assert!(plan.dry_run);
        assert!(!plan.network_performed);
    }

    #[test]
    fn source_contract_mismatches_and_lint_diagnostics_block_builds() {
        let manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        let audit = PackageSourceAudit {
            diagnostics: vec![ValidationDiagnostic {
                code: "package.entry_lint".to_owned(),
                severity: DiagnosticSeverity::Error,
                message: "invalid Lua".to_owned(),
                path: "package.entry".to_owned(),
                help: "Fix the Lua entry".to_owned(),
            }],
            detected_capabilities: vec!["filesystem.write".to_owned()],
            detected_effects: vec![Effect::WriteFilesystem],
        };
        let outcome = build_package(
            &manifest,
            VALID.as_bytes(),
            Some(b"invalid"),
            &["process.spawn".to_owned()],
            "fnv1a64:host",
            "0.1.0",
            "plugin.toml",
            &audit,
        );
        assert!(!outcome.valid);
        assert!(outcome.build.is_none());
        assert!(outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.entry_lint"));
        assert!(outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.source_capability_undeclared"));
        assert!(outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "package.source_effect_undeclared"));
    }

    #[test]
    fn package_file_paths_are_invocation_independent() {
        let manifest = parse_package_manifest(VALID, "plugin.toml").unwrap();
        let audit = PackageSourceAudit {
            diagnostics: Vec::new(),
            detected_capabilities: vec!["process.spawn".to_owned()],
            detected_effects: vec![Effect::SpawnProcess],
        };
        let relative = build_package(
            &manifest,
            VALID.as_bytes(),
            Some(b"return {}"),
            &["process.spawn".to_owned()],
            "fnv1a64:host",
            "0.1.0",
            "plugin.toml",
            &audit,
        );
        let absolute = build_package(
            &manifest,
            VALID.as_bytes(),
            Some(b"return {}"),
            &["process.spawn".to_owned()],
            "fnv1a64:host",
            "0.1.0",
            "/tmp/example/plugin.toml",
            &audit,
        );
        assert_eq!(relative, absolute);
        assert_eq!(
            relative.build.unwrap().files,
            vec!["plugin.lua", "plugin.toml"]
        );
    }
}
