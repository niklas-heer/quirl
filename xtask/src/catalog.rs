//! Build-time native catalog import, formatting, validation, and publication.

use clap::Subcommand;
use quirl_catalog::{
    NativeArgument, NativeCatalog, NativeCatalogDiagnostic, NativeCatalogLimits,
    NativeCatalogReader, NativeCommand, NativeCompletionAction, NativeFlag, NativePlatform,
    compile_native_catalog, parse_native_catalog,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::TaskError;

const CURATED_DIRECTORY: &str = "catalog/curated";
const DRAFT_DIRECTORY: &str = "catalog/draft";
const GENERATED_DIRECTORY: &str = "catalog/generated";
const IMPORT_CONFIGURATION: &str = "catalog/carapace-import.json";
const DATABASE_FILE: &str = "catalog/generated/catalog.sqlite3";
const CHECKSUM_FILE: &str = "catalog/generated/catalog.sqlite3.sha256";
const DRAFT_MANIFEST_FILE: &str = "catalog/draft/carapace.import.json";
const DRAFT_FILE_SUFFIX: &str = ".kdl";
const LEGACY_DRAFT_FILE: &str = "catalog/draft/carapace.kdl";
const LEGACY_DRAFT_FILE_PREFIX: &str = "carapace-";
const PROVENANCE_FILE: &str = "catalog/provenance/carapace.json";
const LICENSE_FILE: &str = "catalog/provenance/CARAPACE_LICENSE";
const SOURCE_FILE_BYTES_MAX: usize = 512 * 1024;
const SOURCE_TOTAL_BYTES_MAX: usize = 4 * 1024 * 1024;
const SOURCE_FILE_COUNT_MAX: usize = 128;
const IMPORT_COMMAND_COUNT_MAX: usize = 2_048;
const IMPORT_FLAG_COUNT_MAX: usize = 16_384;
const IMPORT_OUTPUT_BYTES_MAX: usize = 4 * 1024 * 1024;
const CATALOG_FILE_COUNT_MAX: usize = 256;
const CATALOG_TOTAL_BYTES_MAX: usize = 8 * 1024 * 1024;
const TEMPORARY_ATTEMPTS_MAX: usize = 32;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// One native-catalog development action.
#[derive(Debug, Subcommand)]
pub(crate) enum CatalogCommand {
    /// Import a static, review-only draft from a pinned local Carapace checkout.
    ImportCarapace {
        /// Local Carapace source checkout; its code is read but never executed.
        #[arg(long)]
        source: PathBuf,
        /// Exact 40-character Git revision expected in the detached checkout.
        #[arg(long)]
        revision: String,
    },
    /// Copy explicitly reviewed draft commands into command-named curated KDL.
    Promote {
        /// Root command to promote; repeat to promote multiple reviewed drafts.
        #[arg(long = "command", required = true)]
        commands: Vec<String>,
    },
    /// Canonically format every curated and draft KDL source.
    Fmt {
        /// Report non-canonical files without modifying them.
        #[arg(long)]
        check: bool,
    },
    /// Validate schema, formatting, provenance, separation, and derived artifacts.
    Check,
    /// Compile curated KDL and atomically publish SQLite plus its SHA-256 checksum.
    Build,
}

pub(crate) fn run(root: &Path, command: CatalogCommand) -> Result<(), TaskError> {
    match command {
        CatalogCommand::ImportCarapace { source, revision } => {
            import_carapace(root, &source, &revision)
        }
        CatalogCommand::Promote { commands } => promote_drafts(root, &commands),
        CatalogCommand::Fmt { check } => format_sources(root, check),
        CatalogCommand::Check => check_catalog(root),
        CatalogCommand::Build => build_catalog(root),
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "promotion requires exactly one matched draft before accessing it"
)]
fn promote_drafts(root: &Path, commands: &[String]) -> Result<(), TaskError> {
    if commands.is_empty() || commands.len() > SOURCE_FILE_COUNT_MAX {
        return Err(resource_error(
            "promoted command count",
            SOURCE_FILE_COUNT_MAX,
            commands.len(),
        ));
    }
    let (curated, _) = load_and_validate_sources(root, true)?;
    let curated_names = curated
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut requested = BTreeSet::new();
    let mut rendered = Vec::new();
    for command_name in commands {
        validate_catalog_identifier(command_name)?;
        if !requested.insert(command_name.as_str()) {
            return Err(input_error(format!(
                "duplicate promotion request for {command_name}"
            )));
        }
        if curated_names.contains(command_name.as_str()) {
            return Err(input_error(format!(
                "curated command {command_name} already exists; review and edit its source directly"
            )));
        }
        let draft_path = root
            .join(DRAFT_DIRECTORY)
            .join(format!("{command_name}{DRAFT_FILE_SUFFIX}"));
        let draft = load_one_catalog(root, &draft_path, true)?;
        if draft.commands.len() != 1 || draft.commands[0].name != *command_name {
            return Err(input_error(format!(
                "draft {} must contain exactly the root command {command_name}",
                draft_path.display()
            )));
        }
        if draft.provenance.license != curated.provenance.license
            || draft.provenance.revision != curated.provenance.revision
            || draft.provenance.source_url != curated.provenance.source_url
        {
            return Err(input_error(format!(
                "draft {command_name} provenance differs from the curated Carapace source"
            )));
        }
        let promoted = NativeCatalog {
            name: command_name.clone(),
            provenance: curated.provenance.clone(),
            commands: draft.commands,
        };
        let bytes = render_catalog(&promoted)?.into_bytes();
        let path = root
            .join(CURATED_DIRECTORY)
            .join(format!("{command_name}{DRAFT_FILE_SUFFIX}"));
        if path.exists() {
            return Err(input_error(format!(
                "promotion refuses to overwrite curated source {}",
                path.display()
            )));
        }
        rendered.push(RenderedDraft { path, bytes });
    }
    for promoted in &rendered {
        atomic_write(&promoted.path, &promoted.bytes)?;
    }
    println!("catalog promote: {} command(s)", rendered.len());
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportConfiguration {
    source_url: String,
    revision: String,
    license: String,
    license_file: String,
    license_sha256: String,
    author: String,
    coverage: ImportCoverage,
    roots: Vec<ImportRoot>,
    #[serde(default)]
    flat_roots: Vec<FlatImportGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportCoverage {
    root_commands_min: usize,
    command_paths_min: usize,
    flags_min: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportRoot {
    variable: String,
    platforms: Vec<String>,
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlatImportGroup {
    platforms: Vec<String>,
    files: Vec<String>,
}

fn expanded_import_roots(configuration: &ImportConfiguration) -> Vec<ImportRoot> {
    let mut roots = configuration.roots.clone();
    for group in &configuration.flat_roots {
        for file in &group.files {
            roots.push(ImportRoot {
                variable: "rootCmd".to_owned(),
                platforms: group.platforms.clone(),
                files: vec![file.clone()],
            });
        }
    }
    roots
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ImportManifest {
    source_url: String,
    revision: String,
    license: String,
    license_file: String,
    license_sha256: String,
    source_files: Vec<String>,
    #[serde(default)]
    draft_files: Vec<String>,
    command_paths: Vec<String>,
    command_count: usize,
    flag_count: usize,
    omitted_constructs: Vec<String>,
    semantic_diff: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceRecord {
    source_url: String,
    revision: String,
    license: String,
    license_file: String,
    license_sha256: String,
    import_policy: String,
    curation: String,
}

#[derive(Debug)]
struct ParsedGoCommand {
    variable: String,
    parent: Option<String>,
    command: NativeCommand,
    omitted_constructs: Vec<String>,
}

struct ParsedCheckout {
    catalog: NativeCatalog,
    source_files: Vec<String>,
    omitted_constructs: Vec<String>,
}

struct ParsedFlagActions {
    actions: BTreeMap<String, NativeCompletionAction>,
    omissions: Vec<String>,
}

struct ParsedFlags {
    flags: Vec<NativeFlag>,
    omissions: Vec<String>,
}

struct RenderedDraft {
    path: PathBuf,
    bytes: Vec<u8>,
}

fn import_carapace(root: &Path, source: &Path, revision: &str) -> Result<(), TaskError> {
    validate_revision(revision)?;
    let configuration_path = root.join(IMPORT_CONFIGURATION);
    let configuration: ImportConfiguration = read_json_bounded(&configuration_path)?;
    if configuration.revision != revision {
        return Err(input_error(format!(
            "requested Carapace revision {revision} does not match pinned revision {} in {}",
            configuration.revision,
            configuration_path.display()
        )));
    }
    let observed_revision = checkout_revision(source)?;
    if observed_revision != revision {
        return Err(input_error(format!(
            "Carapace checkout revision mismatch: expected {revision}, observed {observed_revision}"
        )));
    }
    validate_import_configuration(&configuration)?;
    verify_checkout_sources(source, revision, &configuration)?;
    validate_upstream_license(source, &configuration)?;
    let parsed = parse_carapace_checkout(source, &configuration)?;
    let catalog = parsed.catalog;
    let rendered = render_imported_drafts(root, &catalog)?;
    let previous = read_existing_carapace_drafts(root)?;
    let semantic_diff = semantic_diff(previous.as_ref(), &catalog);
    let (command_paths, command_count, flag_count) = catalog_statistics(&catalog);
    let draft_files = rendered
        .iter()
        .map(|draft| relative_display(root, &draft.path))
        .collect();
    let manifest = ImportManifest {
        source_url: configuration.source_url,
        revision: configuration.revision,
        license: configuration.license,
        license_file: configuration.license_file,
        license_sha256: configuration.license_sha256,
        source_files: parsed.source_files,
        draft_files,
        command_paths,
        command_count,
        flag_count,
        omitted_constructs: parsed.omitted_constructs,
        semantic_diff,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    publish_imported_drafts(root, &rendered)?;
    atomic_write(&root.join(DRAFT_MANIFEST_FILE), &manifest_bytes)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

fn render_imported_drafts(
    root: &Path,
    catalog: &NativeCatalog,
) -> Result<Vec<RenderedDraft>, TaskError> {
    let mut rendered = Vec::with_capacity(catalog.commands.len());
    let mut output_bytes = 0_usize;
    let mut root_names = BTreeSet::new();
    for command in &catalog.commands {
        if !root_names.insert(command.name.as_str()) {
            return Err(input_error(format!(
                "duplicate imported root command {}",
                command.name
            )));
        }
        let draft = NativeCatalog {
            name: format!("{}-draft", command.name),
            provenance: catalog.provenance.clone(),
            commands: vec![command.clone()],
        };
        let bytes = render_catalog(&draft)?.into_bytes();
        output_bytes = output_bytes.saturating_add(bytes.len());
        if output_bytes > IMPORT_OUTPUT_BYTES_MAX {
            return Err(resource_error(
                "generated draft bytes",
                IMPORT_OUTPUT_BYTES_MAX,
                output_bytes,
            ));
        }
        let relative = format!("{DRAFT_DIRECTORY}/{}{DRAFT_FILE_SUFFIX}", command.name);
        parse_native_catalog(
            std::str::from_utf8(&bytes)?,
            &relative,
            NativeCatalogLimits::default(),
        )
        .map_err(map_diagnostic)?;
        rendered.push(RenderedDraft {
            path: root.join(relative),
            bytes,
        });
    }
    rendered.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(rendered)
}

fn publish_imported_drafts(root: &Path, rendered: &[RenderedDraft]) -> Result<(), TaskError> {
    let expected = rendered
        .iter()
        .map(|draft| draft.path.clone())
        .collect::<BTreeSet<_>>();
    for draft in rendered {
        atomic_write(&draft.path, &draft.bytes)?;
    }
    for path in managed_carapace_draft_paths(root)? {
        if !expected.contains(&path) {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn managed_carapace_draft_paths(root: &Path) -> Result<Vec<PathBuf>, TaskError> {
    let manifest_path = root.join(DRAFT_MANIFEST_FILE);
    if manifest_path.is_file() {
        let manifest: ImportManifest = read_json_bounded(&manifest_path)?;
        if !manifest.draft_files.is_empty() {
            let mut paths = Vec::with_capacity(manifest.draft_files.len());
            for relative in manifest.draft_files {
                validate_normalized_relative_path(&relative, "Carapace draft path")?;
                if !relative.starts_with(&format!("{DRAFT_DIRECTORY}/"))
                    || !relative.ends_with(DRAFT_FILE_SUFFIX)
                {
                    return Err(input_error(format!(
                        "Carapace draft manifest path is outside the managed KDL area: {relative}"
                    )));
                }
                let path = root.join(relative);
                validate_input_metadata(
                    &path,
                    &fs::symlink_metadata(&path)?,
                    CATALOG_TOTAL_BYTES_MAX,
                )?;
                paths.push(path);
            }
            paths.sort();
            return Ok(paths);
        }
    }

    // Older manifests did not list owned output paths. Admit only the exact
    // legacy naming convention during the one-way migration.
    let draft_directory = root.join(DRAFT_DIRECTORY);
    let mut paths = Vec::new();
    for entry in fs::read_dir(&draft_directory)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(input_error(format!(
                "catalog source symlink is not allowed: {}",
                path.display()
            )));
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let managed = path == root.join(LEGACY_DRAFT_FILE)
            || (file_type.is_file()
                && name.starts_with(LEGACY_DRAFT_FILE_PREFIX)
                && name.ends_with(DRAFT_FILE_SUFFIX));
        if managed {
            validate_input_metadata(
                &path,
                &fs::symlink_metadata(&path)?,
                CATALOG_TOTAL_BYTES_MAX,
            )?;
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_existing_carapace_drafts(root: &Path) -> Result<Option<NativeCatalog>, TaskError> {
    let mut commands = Vec::new();
    let mut provenance = None;
    for path in managed_carapace_draft_paths(root)? {
        let draft = load_one_catalog(root, &path, false)?;
        if !is_carapace_draft_name(&draft.name) {
            return Err(input_error(format!(
                "managed Carapace draft has unexpected catalog identity {}",
                draft.name
            )));
        }
        if let Some(expected) = &provenance {
            if expected != &draft.provenance {
                return Err(input_error(
                    "managed Carapace drafts have inconsistent provenance",
                ));
            }
        } else {
            provenance = Some(draft.provenance.clone());
        }
        commands.extend(draft.commands);
    }
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(provenance.map(|provenance| NativeCatalog {
        name: "carapace-draft".to_owned(),
        provenance,
        commands,
    }))
}

fn is_carapace_draft_name(name: &str) -> bool {
    name == "carapace-draft" || name.ends_with("-draft")
}

fn validate_import_configuration(configuration: &ImportConfiguration) -> Result<(), TaskError> {
    if !configuration.source_url.starts_with("https://") {
        return Err(input_error("Carapace source_url must use https://"));
    }
    if configuration.license != "MIT" {
        return Err(input_error(
            "pinned Carapace input must retain its MIT license",
        ));
    }
    validate_normalized_relative_path(&configuration.license_file, "Carapace license path")?;
    if configuration.license_sha256.len() != 64
        || !configuration
            .license_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(input_error(
            "Carapace license_sha256 must be exactly 64 hexadecimal characters",
        ));
    }
    if configuration.coverage.root_commands_min == 0
        || configuration.coverage.command_paths_min < configuration.coverage.root_commands_min
        || configuration.coverage.flags_min < configuration.coverage.root_commands_min
    {
        return Err(input_error(
            "Carapace coverage floors must be nonzero and cannot contain fewer paths or flags than root commands",
        ));
    }
    for group in &configuration.flat_roots {
        if group.files.is_empty() {
            return Err(input_error(
                "flat Carapace import group has no source files",
            ));
        }
        parse_platforms(&group.platforms)?;
    }
    let roots = expanded_import_roots(configuration);
    if roots.is_empty() || roots.len() > SOURCE_FILE_COUNT_MAX {
        return Err(resource_error(
            "import root count",
            SOURCE_FILE_COUNT_MAX,
            roots.len(),
        ));
    }
    let mut files = BTreeSet::new();
    for root in &roots {
        validate_go_identifier(&root.variable)?;
        if root.files.is_empty() {
            return Err(input_error(format!(
                "import root {} has no source files",
                root.variable
            )));
        }
        for path in &root.files {
            validate_relative_source_path(path)?;
            if !files.insert(path) {
                return Err(input_error(format!("duplicate import source path {path}")));
            }
        }
        parse_platforms(&root.platforms)?;
    }
    if files.len() > SOURCE_FILE_COUNT_MAX {
        return Err(resource_error(
            "import source file count",
            SOURCE_FILE_COUNT_MAX,
            files.len(),
        ));
    }
    Ok(())
}

fn validate_upstream_license(
    source: &Path,
    configuration: &ImportConfiguration,
) -> Result<(), TaskError> {
    validate_no_symlink_components(source, &configuration.license_file)?;
    let path = source.join(&configuration.license_file);
    let bytes = read_regular_file_bounded(&path, 64 * 1024)?;
    let observed = sha256_hex(&bytes);
    if observed != configuration.license_sha256.to_ascii_lowercase() {
        return Err(input_error(format!(
            "Carapace license checksum mismatch: expected {}, observed {observed}",
            configuration.license_sha256
        )));
    }
    Ok(())
}

fn parse_carapace_checkout(
    source: &Path,
    configuration: &ImportConfiguration,
) -> Result<ParsedCheckout, TaskError> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_dir() {
        return Err(input_error("Carapace source must be a directory"));
    }
    let mut total_bytes = 0_usize;
    let mut source_files = Vec::new();
    let mut roots = Vec::new();
    let mut command_count = 0_usize;
    let mut flag_count = 0_usize;
    let mut omitted_constructs = Vec::new();
    for root in expanded_import_roots(configuration) {
        let platforms = parse_platforms(&root.platforms)?;
        let mut parsed = BTreeMap::<String, ParsedGoCommand>::new();
        for relative in &root.files {
            validate_no_symlink_components(source, relative)?;
            let path = source.join(relative);
            let bytes = read_regular_file_bounded(&path, SOURCE_FILE_BYTES_MAX)?;
            total_bytes = total_bytes.saturating_add(bytes.len());
            if total_bytes > SOURCE_TOTAL_BYTES_MAX {
                return Err(resource_error(
                    "Carapace source bytes",
                    SOURCE_TOTAL_BYTES_MAX,
                    total_bytes,
                ));
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                input_error(format!("Carapace source is not UTF-8: {}", path.display()))
            })?;
            for command in parse_go_file(text, relative)? {
                omitted_constructs.extend(command.omitted_constructs.iter().cloned());
                flag_count = flag_count.saturating_add(command.command.flags.len());
                command_count = command_count.saturating_add(1);
                if command_count > IMPORT_COMMAND_COUNT_MAX {
                    return Err(resource_error(
                        "import command count",
                        IMPORT_COMMAND_COUNT_MAX,
                        command_count,
                    ));
                }
                if flag_count > IMPORT_FLAG_COUNT_MAX {
                    return Err(resource_error(
                        "import flag count",
                        IMPORT_FLAG_COUNT_MAX,
                        flag_count,
                    ));
                }
                if parsed.insert(command.variable.clone(), command).is_some() {
                    return Err(input_error(format!(
                        "duplicate Cobra command variable in import root: {}",
                        root.variable
                    )));
                }
            }
            source_files.push(relative.clone());
        }
        roots.push(build_command_tree(&root.variable, &platforms, parsed)?);
    }
    roots.sort_by(|left, right| left.name.cmp(&right.name));
    source_files.sort();
    omitted_constructs.sort();
    Ok(ParsedCheckout {
        catalog: NativeCatalog {
            name: "carapace-draft".to_owned(),
            provenance: quirl_catalog::NativeProvenance {
                author: configuration.author.clone(),
                license: configuration.license.clone(),
                revision: configuration.revision.clone(),
                source_url: configuration.source_url.clone(),
            },
            commands: roots,
        },
        source_files,
        omitted_constructs,
    })
}

#[allow(
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "Go parser offsets come from ASCII delimiter searches and source byte limits bound accumulation"
)]
fn parse_go_file(source: &str, source_name: &str) -> Result<Vec<ParsedGoCommand>, TaskError> {
    if source.len() > SOURCE_FILE_BYTES_MAX {
        return Err(resource_error(
            "Carapace source file bytes",
            SOURCE_FILE_BYTES_MAX,
            source.len(),
        ));
    }
    let source = strip_go_comments(source)?;
    let source = source.as_str();
    let mut commands = Vec::new();
    let mut offset = 0_usize;
    while let Some(relative) = source[offset..].find("= &cobra.Command{") {
        let marker = offset + relative;
        let variable = variable_before_marker(source, marker)?;
        let body_start = marker
            + source[marker..]
                .find('{')
                .ok_or_else(|| input_error("Cobra command marker has no opening brace"))?;
        let body_end = matching_brace(source, body_start)?;
        let body = &source[body_start + 1..body_end];
        let use_value = keyed_go_string(body, "Use").ok_or_else(|| {
            input_error(format!(
                "{source_name}: Cobra command {variable} has no static Use string"
            ))
        })?;
        let name = use_value
            .split_ascii_whitespace()
            .next()
            .ok_or_else(|| input_error(format!("{source_name}: empty Cobra Use value")))?
            .to_owned();
        validate_catalog_identifier(&name)?;
        let summary = keyed_go_string(body, "Short").ok_or_else(|| {
            input_error(format!(
                "{source_name}: Cobra command {variable} has no static Short string"
            ))
        })?;
        let description = keyed_go_string(body, "Long")
            .filter(|value| !value.starts_with("http://") && !value.starts_with("https://"))
            .unwrap_or_else(|| summary.clone());
        let aliases = keyed_go_string_slice(body, "Aliases")?;
        let parsed_flags = parse_go_flags(source, source_name, &variable)?;
        let flags = parsed_flags.flags;
        let mut omitted_constructs = parsed_flags.omissions;
        let arguments = Vec::new();
        let positional_marker = format!("carapace.Gen({variable}).PositionalAnyCompletion(");
        if source.contains(&positional_marker) {
            omitted_constructs.push(format!(
                "{source_name}:{variable}: PositionalAnyCompletion omitted because upstream does not declare the names and documentation required by the native argument schema"
            ));
        }
        let parent = find_parent_variable(source, &variable);
        commands.push(ParsedGoCommand {
            variable,
            parent,
            omitted_constructs,
            command: NativeCommand {
                name,
                aliases,
                summary,
                description,
                intents: Vec::new(),
                platforms: vec![NativePlatform::Any],
                flags,
                arguments,
                subcommands: Vec::new(),
            },
        });
        offset = body_end.saturating_add(1);
    }
    if commands.is_empty() {
        return Err(input_error(format!(
            "{source_name}: no static Cobra command definitions found"
        )));
    }
    Ok(commands)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GoLexState {
    Code,
    LineComment,
    BlockComment,
    InterpretedString,
    RawString,
    Rune,
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "the scanner checks lookahead bounds and advances over ASCII Go comment delimiters"
)]
fn strip_go_comments(source: &str) -> Result<String, TaskError> {
    let mut bytes = source.as_bytes().to_vec();
    let mut state = GoLexState::Code;
    let mut escaped = false;
    let mut index = 0_usize;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index.saturating_add(1)).copied();
        match state {
            GoLexState::Code if byte == b'/' && next == Some(b'/') => {
                bytes[index] = b' ';
                bytes[index + 1] = b' ';
                state = GoLexState::LineComment;
                index = index.saturating_add(2);
                continue;
            }
            GoLexState::Code if byte == b'/' && next == Some(b'*') => {
                bytes[index] = b' ';
                bytes[index + 1] = b' ';
                state = GoLexState::BlockComment;
                index = index.saturating_add(2);
                continue;
            }
            GoLexState::Code if byte == b'"' => state = GoLexState::InterpretedString,
            GoLexState::Code if byte == b'`' => state = GoLexState::RawString,
            GoLexState::Code if byte == b'\'' => state = GoLexState::Rune,
            GoLexState::LineComment if byte == b'\n' => state = GoLexState::Code,
            GoLexState::LineComment => bytes[index] = b' ',
            GoLexState::BlockComment if byte == b'*' && next == Some(b'/') => {
                bytes[index] = b' ';
                bytes[index + 1] = b' ';
                state = GoLexState::Code;
                index = index.saturating_add(2);
                continue;
            }
            GoLexState::BlockComment if byte != b'\n' => bytes[index] = b' ',
            GoLexState::InterpretedString | GoLexState::Rune if escaped => escaped = false,
            GoLexState::InterpretedString | GoLexState::Rune if byte == b'\\' => escaped = true,
            GoLexState::InterpretedString if byte == b'"' => state = GoLexState::Code,
            GoLexState::Rune if byte == b'\'' => state = GoLexState::Code,
            GoLexState::RawString if byte == b'`' => state = GoLexState::Code,
            _ => {}
        }
        index = index.saturating_add(1);
    }
    if state == GoLexState::BlockComment {
        return Err(input_error("unterminated Go block comment"));
    }
    String::from_utf8(bytes).map_err(|_| input_error("comment filtering produced invalid UTF-8"))
}

#[allow(
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "marker and identifier offsets come from ASCII delimiter searches"
)]
fn variable_before_marker(source: &str, marker: usize) -> Result<String, TaskError> {
    let line_start = source[..marker].rfind('\n').map_or(0, |index| index + 1);
    let prefix = source[line_start..marker].trim();
    let variable = prefix
        .strip_prefix("var ")
        .map(str::trim)
        .ok_or_else(|| input_error("Cobra command definition must use `var name =`"))?;
    validate_go_identifier(variable)?;
    Ok(variable.to_owned())
}

fn matching_brace(source: &str, open: usize) -> Result<usize, TaskError> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return Err(input_error("Cobra command marker has no opening brace"));
    }
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        if let Some(delimiter) = quote {
            if delimiter == b'`' {
                if *byte == delimiter {
                    quote = None;
                }
                continue;
            }
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == delimiter {
                quote = None;
            }
            continue;
        }
        match byte {
            b'"' | b'\'' | b'`' => quote = Some(*byte),
            b'{' => depth = depth.saturating_add(1),
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(index);
                }
            }
            _ => {}
        }
    }
    Err(input_error("unterminated Cobra command definition"))
}

fn keyed_go_string(body: &str, key: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(key)?.trim_start();
        let value = rest.strip_prefix(':')?.trim_start();
        if !value.starts_with('"') {
            return None;
        }
        first_go_string(value)
            .and_then(Result::ok)
            .map(|pair| pair.0)
    })
}

#[allow(
    clippy::indexing_slicing,
    reason = "the keyed literal parser validates the key and opening delimiter before fixed access"
)]
fn keyed_go_string_slice(body: &str, key: &str) -> Result<Vec<String>, TaskError> {
    let Some(line) = body.lines().find(|line| line.trim().starts_with(key)) else {
        return Ok(Vec::new());
    };
    let mut values = quoted_go_strings(line)?;
    values.sort();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(input_error(format!(
            "duplicate literal in Cobra {key} list"
        )));
    }
    for value in &values {
        validate_catalog_identifier(value)?;
    }
    Ok(values)
}

#[allow(
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "string offsets are produced by char_indices and bounded delimiter searches"
)]
fn first_go_string(input: &str) -> Option<Result<(String, usize), TaskError>> {
    let start = input.find('"')?;
    let mut escaped = false;
    for (relative, character) in input[start + 1..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
        } else if character == '"' {
            let end = start + 1 + relative;
            let literal = &input[start..=end];
            let decoded = serde_json::from_str::<String>(literal)
                .map_err(|error| input_error(format!("invalid quoted Go string: {error}")));
            return Some(decoded.map(|value| (value, end + 1)));
        }
    }
    Some(Err(input_error("unterminated quoted Go string")))
}

#[allow(
    clippy::string_slice,
    reason = "consumed offsets returned by the Go string parser are UTF-8 boundaries"
)]
fn quoted_go_strings(input: &str) -> Result<Vec<String>, TaskError> {
    let mut values = Vec::new();
    let mut rest = input;
    while let Some(result) = first_go_string(rest) {
        let (value, consumed) = result?;
        values.push(value);
        rest = &rest[consumed..];
    }
    Ok(values)
}

#[allow(
    clippy::indexing_slicing,
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "flag parser offsets come from validated ASCII Go syntax and bounded search results"
)]
fn parse_go_flags(
    source: &str,
    source_name: &str,
    variable: &str,
) -> Result<ParsedFlags, TaskError> {
    let ParsedFlagActions {
        actions,
        mut omissions,
    } = parse_go_flag_actions(source, source_name, variable)?;
    let prefixes = [
        format!("{variable}.Flags()."),
        format!("{variable}.PersistentFlags()."),
    ];
    let mut flags = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        let Some(rest) = prefixes
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix))
        else {
            continue;
        };
        let Some(open) = rest.find('(') else {
            return Err(input_error(format!(
                "static flag declaration for {variable} has no argument list"
            )));
        };
        let method = &rest[..open];
        if method == "SetInterspersed" {
            omissions.push(format!(
                "{source_name}:{variable}: Cobra SetInterspersed parser policy omitted because the native catalog describes completion facts, not argument-parser execution"
            ));
            continue;
        }
        let is_bool = matches!(
            method,
            "Bool" | "BoolP" | "BoolS" | "BoolSliceS" | "Count" | "CountP" | "CountS"
        );
        let is_string = matches!(
            method,
            "String"
                | "StringP"
                | "StringS"
                | "StringArray"
                | "StringArrayP"
                | "StringArrayS"
                | "StringSlice"
                | "StringSliceP"
                | "StringSliceS"
                | "Int"
                | "IntP"
                | "IntS"
        );
        if !is_bool && !is_string {
            return Err(input_error(format!(
                "{source_name}: unsupported Cobra flag method {method} for {variable}; extend the bounded importer before accepting this upstream shape"
            )));
        }
        let values = quoted_go_strings(&rest[open + 1..])?;
        if values.len() < 2 {
            return Err(input_error(format!(
                "static flag declaration for {variable} is incomplete"
            )));
        }
        let name = values[0].clone();
        let canonical_name = if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() {
            format!("-{name}")
        } else if validate_long_name(&name).is_ok() {
            format!("--{name}")
        } else {
            omissions.push(format!(
                "{source_name}:{variable}: flag {name} omitted because its spelling is outside the strict native catalog grammar"
            ));
            continue;
        };
        let has_short = method.ends_with('P') || method.ends_with('S');
        let short = if has_short && values.get(1).is_some_and(|value| !value.is_empty()) {
            let value = format!("-{}", values[1]);
            if value == canonical_name {
                None
            } else if canonical_name.starts_with("--")
                && value.len() == 2
                && value.as_bytes()[1].is_ascii_alphanumeric()
            {
                Some(value)
            } else {
                omissions.push(format!(
                    "{source_name}:{variable}: short flag {value} on {name} omitted because it is outside the strict native short-name grammar"
                ));
                None
            }
        } else {
            None
        };
        let summary = values
            .last()
            .cloned()
            .ok_or_else(|| input_error("flag description is missing"))?;
        if summary.trim().is_empty() || summary.chars().any(char::is_control) {
            omissions.push(format!(
                "{source_name}:{variable}: flag {name} omitted because upstream does not provide the non-empty single-line description required by the native schema"
            ));
            continue;
        }
        let action = actions.get(&name).copied();
        flags.push(NativeFlag {
            name: canonical_name,
            short,
            summary: summary.clone(),
            description: summary,
            value_name: (!is_bool).then(|| "value".to_owned()),
            required: false,
            repeatable: method.contains("Array")
                || method.contains("Slice")
                || method.starts_with("Count"),
            action: (!is_bool).then_some(action).flatten(),
            platforms: vec![NativePlatform::Any],
        });
    }
    flags.sort_by(|left, right| left.name.cmp(&right.name));
    let mut names = BTreeMap::<String, String>::new();
    for flag in &flags {
        for name in std::iter::once(&flag.name).chain(flag.short.iter()) {
            if let Some(previous) = names.insert(name.clone(), flag.name.clone()) {
                return Err(input_error(format!(
                    "duplicate imported flag name {name} on {variable}: {previous} and {}",
                    flag.name
                )));
            }
        }
    }
    Ok(ParsedFlags { flags, omissions })
}

#[allow(
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "action parser offsets come from ASCII delimiter searches in the bounded Go source"
)]
fn parse_go_flag_actions(
    source: &str,
    source_name: &str,
    variable: &str,
) -> Result<ParsedFlagActions, TaskError> {
    let marker = format!("carapace.Gen({variable}).FlagCompletion(");
    let mut actions = BTreeMap::new();
    let mut omissions = Vec::new();
    let mut offset = 0_usize;
    while let Some(relative) = source[offset..].find(&marker) {
        let call_start = offset + relative;
        let tail = &source[call_start + marker.len()..];
        let action_map_relative = tail.find("carapace.ActionMap{").ok_or_else(|| {
            input_error(format!(
                "{source_name}: unsupported non-literal FlagCompletion for {variable}"
            ))
        })?;
        if tail
            .find("carapace.Gen(")
            .is_some_and(|next_call| next_call < action_map_relative)
        {
            return Err(input_error(format!(
                "{source_name}: unsupported non-literal FlagCompletion for {variable}"
            )));
        }
        let open = call_start + marker.len() + action_map_relative + "carapace.ActionMap".len();
        let close = matching_brace(source, open)?;
        for line in source[open + 1..close].lines() {
            let trimmed = line.trim();
            let Some((name, consumed)) = first_go_string(trimmed).transpose()? else {
                continue;
            };
            let action_source = &trimmed[consumed..];
            if let Some(action) = action_from_source(action_source) {
                if actions.insert(name.clone(), action).is_some() {
                    return Err(input_error(format!(
                        "duplicate completion action for flag {name} on {variable}"
                    )));
                }
            } else if action_source.contains(':') {
                omissions.push(format!(
                    "{source_name}:{variable}: completion action for {name} omitted because it is outside the closed native action set"
                ));
            }
        }
        offset = close.saturating_add(1);
    }
    Ok(ParsedFlagActions { actions, omissions })
}

fn action_from_source(source: &str) -> Option<NativeCompletionAction> {
    if source.contains("ActionDirectories") {
        Some(NativeCompletionAction::Directories)
    } else if source.contains("ActionFiles") || source.contains("ActionFilename") {
        Some(NativeCompletionAction::Files)
    } else if source.contains("ActionExecutables") {
        Some(NativeCompletionAction::Executables)
    } else if source.contains("ActionEnvironment") {
        Some(NativeCompletionAction::EnvironmentVariables)
    } else if source.contains("ActionUsers") {
        Some(NativeCompletionAction::Users)
    } else if source.contains("ActionGroups") {
        Some(NativeCompletionAction::Groups)
    } else if source.contains("ActionHosts") {
        Some(NativeCompletionAction::Hostnames)
    } else {
        None
    }
}

#[allow(
    clippy::string_slice,
    reason = "the marker offset comes from an ASCII substring search"
)]
fn find_parent_variable(source: &str, child: &str) -> Option<String> {
    let needle = format!(".AddCommand({child})");
    source.lines().find_map(|line| {
        let trimmed = line.trim();
        let index = trimmed.find(&needle)?;
        let parent = &trimmed[..index];
        validate_go_identifier(parent).ok()?;
        Some(parent.to_owned())
    })
}

fn build_command_tree(
    root_variable: &str,
    platforms: &[NativePlatform],
    mut parsed: BTreeMap<String, ParsedGoCommand>,
) -> Result<NativeCommand, TaskError> {
    if !parsed.contains_key(root_variable) {
        return Err(input_error(format!(
            "configured root variable {root_variable} was not found"
        )));
    }
    let parent_map = parsed
        .iter()
        .filter_map(|(name, command)| {
            command
                .parent
                .as_ref()
                .map(|parent| (name.clone(), parent.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    for (child, parent) in &parent_map {
        if !parsed.contains_key(parent) {
            return Err(input_error(format!(
                "command variable {child} refers to missing parent {parent}"
            )));
        }
    }
    let mut children = BTreeMap::<String, Vec<String>>::new();
    for (child, parent) in parent_map {
        children.entry(parent).or_default().push(child);
    }
    let mut stack = vec![(root_variable.to_owned(), false)];
    let mut built = BTreeMap::<String, NativeCommand>::new();
    let mut visited = BTreeSet::new();
    while let Some((variable, expanded)) = stack.pop() {
        if expanded {
            let mut entry = parsed
                .remove(&variable)
                .ok_or_else(|| input_error(format!("duplicate or cyclic command {variable}")))?;
            let mut subcommands = Vec::new();
            for child in children.get(&variable).into_iter().flatten() {
                subcommands.push(
                    built
                        .remove(child)
                        .ok_or_else(|| input_error(format!("could not build child {child}")))?,
                );
            }
            subcommands.sort_by(|left, right| left.name.cmp(&right.name));
            entry.command.subcommands = subcommands;
            entry.command.platforms = platforms.to_vec();
            for flag in &mut entry.command.flags {
                flag.platforms = platforms.to_vec();
            }
            built.insert(variable, entry.command);
            continue;
        }
        if !visited.insert(variable.clone()) {
            return Err(input_error(format!("cycle in command graph at {variable}")));
        }
        stack.push((variable.clone(), true));
        for child in children.get(&variable).into_iter().flatten().rev() {
            stack.push((child.clone(), false));
        }
    }
    if !parsed.is_empty() {
        let names = parsed.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(input_error(format!(
            "configured import contains commands not reachable from {root_variable}: {names}"
        )));
    }
    built
        .remove(root_variable)
        .ok_or_else(|| input_error(format!("could not build root {root_variable}")))
}

fn format_sources(root: &Path, check: bool) -> Result<(), TaskError> {
    let sources = catalog_source_paths(root)?;
    let mut changed = Vec::new();
    for path in sources {
        let source = read_catalog_source(&path)?;
        let catalog = parse_native_catalog(
            &source,
            &relative_display(root, &path),
            NativeCatalogLimits::default(),
        )
        .map_err(map_diagnostic)?;
        let canonical = render_catalog(&catalog)?;
        if canonical != source {
            changed.push(relative_display(root, &path));
            if !check {
                atomic_write(&path, canonical.as_bytes())?;
            }
        }
    }
    if check && !changed.is_empty() {
        return Err(input_error(format!(
            "non-canonical catalog KDL: {}; run `cargo xtask catalog fmt`",
            changed.join(", ")
        )));
    }
    if !check {
        println!("catalog fmt: {} file(s) changed", changed.len());
    }
    Ok(())
}

fn check_catalog(root: &Path) -> Result<(), TaskError> {
    let (curated, drafts) = load_and_validate_sources(root, true)?;
    validate_separation(&curated, &drafts)?;
    validate_import_artifacts(root, &drafts)?;
    let limits = NativeCatalogLimits::embedded();
    let bytes = compile_native_catalog(&curated, limits).map_err(map_diagnostic)?;
    let repeated = compile_native_catalog(&curated, limits).map_err(map_diagnostic)?;
    if bytes != repeated {
        return Err(input_error(
            "native catalog compiler output is not deterministic",
        ));
    }
    let checksum = checksum_line(&bytes);
    let database_path = root.join(DATABASE_FILE);
    let checksum_path = root.join(CHECKSUM_FILE);
    let observed_database = read_regular_file_bounded(&database_path, limits.database_bytes_max)?;
    let observed_checksum = read_regular_file_bounded(&checksum_path, 256)?;
    if observed_database != bytes || observed_checksum != checksum.as_bytes() {
        return Err(input_error(
            "compiled catalog artifacts drifted; run `cargo xtask catalog build`",
        ));
    }
    NativeCatalogReader::from_bytes(&observed_database, limits).map_err(map_diagnostic)?;
    println!(
        "catalog check: {} curated command(s), {} draft catalog(s), checksum {}",
        curated.commands.len(),
        drafts.len(),
        checksum
            .split_ascii_whitespace()
            .next()
            .unwrap_or("<missing>")
    );
    Ok(())
}

fn build_catalog(root: &Path) -> Result<(), TaskError> {
    let (curated, drafts) = load_and_validate_sources(root, true)?;
    validate_separation(&curated, &drafts)?;
    validate_import_artifacts(root, &drafts)?;
    let limits = NativeCatalogLimits::embedded();
    let bytes = compile_native_catalog(&curated, limits).map_err(map_diagnostic)?;
    NativeCatalogReader::from_bytes(&bytes, limits).map_err(map_diagnostic)?;
    let checksum = checksum_line(&bytes);
    let generated = root.join(GENERATED_DIRECTORY);
    fs::create_dir_all(&generated)?;
    atomic_write(&root.join(DATABASE_FILE), &bytes)?;
    atomic_write(&root.join(CHECKSUM_FILE), checksum.as_bytes())?;
    println!(
        "catalog build: {} bytes, {}",
        bytes.len(),
        checksum.trim_end()
    );
    Ok(())
}

fn load_and_validate_sources(
    root: &Path,
    require_canonical: bool,
) -> Result<(NativeCatalog, Vec<NativeCatalog>), TaskError> {
    let _ = catalog_source_paths(root)?;
    let curated_paths = kdl_files(&root.join(CURATED_DIRECTORY))?;
    if curated_paths.is_empty() {
        return Err(input_error("catalog/curated must contain KDL sources"));
    }
    let mut curated_sources = Vec::with_capacity(curated_paths.len());
    for path in curated_paths {
        let catalog = load_one_catalog(root, &path, require_canonical)?;
        validate_provenance(&catalog, false)?;
        curated_sources.push(catalog);
    }
    let curated = merge_catalogs("native-tools", curated_sources)?;
    let mut drafts = Vec::new();
    for path in kdl_files(&root.join(DRAFT_DIRECTORY))? {
        let catalog = load_one_catalog(root, &path, require_canonical)?;
        validate_provenance(&catalog, true)?;
        drafts.push(catalog);
    }
    Ok((curated, drafts))
}

fn merge_catalogs(name: &str, catalogs: Vec<NativeCatalog>) -> Result<NativeCatalog, TaskError> {
    let mut catalogs = catalogs.into_iter();
    let first = catalogs
        .next()
        .ok_or_else(|| input_error("cannot merge an empty catalog set"))?;
    let provenance = first.provenance;
    let mut commands = first.commands;
    for catalog in catalogs {
        if catalog.provenance != provenance {
            return Err(input_error(format!(
                "catalog {} has provenance inconsistent with the curated source set",
                catalog.name
            )));
        }
        commands.extend(catalog.commands);
    }
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    let mut names = BTreeSet::new();
    for command in &commands {
        if !names.insert(command.name.as_str()) {
            return Err(input_error(format!(
                "duplicate curated root command `{}`",
                command.name
            )));
        }
    }
    Ok(NativeCatalog {
        name: name.to_owned(),
        provenance,
        commands,
    })
}

fn load_one_catalog(
    root: &Path,
    path: &Path,
    require_canonical: bool,
) -> Result<NativeCatalog, TaskError> {
    let source = read_catalog_source(path)?;
    let name = relative_display(root, path);
    let catalog = parse_native_catalog(&source, &name, NativeCatalogLimits::default())
        .map_err(map_diagnostic)?;
    if require_canonical && render_catalog(&catalog)? != source {
        return Err(input_error(format!(
            "{name} is not canonical; run `cargo xtask catalog fmt`"
        )));
    }
    Ok(catalog)
}

fn validate_provenance(catalog: &NativeCatalog, draft: bool) -> Result<(), TaskError> {
    if catalog.provenance.author.trim().is_empty()
        || catalog.provenance.license.trim().is_empty()
        || catalog.provenance.source_url.trim().is_empty()
    {
        return Err(input_error(format!(
            "catalog {} has incomplete provenance",
            catalog.name
        )));
    }
    if catalog.provenance.revision.len() != 40
        || !catalog
            .provenance
            .revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(input_error(format!(
            "catalog {} provenance revision must be a full 40-character Git commit",
            catalog.name
        )));
    }
    if draft && !catalog.name.ends_with("-draft") {
        return Err(input_error(format!(
            "draft catalog {} must have a name ending in -draft",
            catalog.name
        )));
    }
    if !draft && catalog.name.ends_with("-draft") {
        return Err(input_error(format!(
            "curated catalog {} cannot use a draft identity",
            catalog.name
        )));
    }
    Ok(())
}

fn validate_separation(
    _curated: &NativeCatalog,
    drafts: &[NativeCatalog],
) -> Result<(), TaskError> {
    let mut draft_names = BTreeSet::new();
    for draft in drafts {
        for command in &draft.commands {
            if !draft_names.insert(command.name.as_str()) {
                return Err(input_error(format!(
                    "draft root command `{}` appears in multiple draft catalogs",
                    command.name
                )));
            }
        }
    }
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    reason = "artifact comparison validates equal bounded collections before paired access"
)]
fn validate_import_artifacts(root: &Path, drafts: &[NativeCatalog]) -> Result<(), TaskError> {
    let configuration: ImportConfiguration = read_json_bounded(&root.join(IMPORT_CONFIGURATION))?;
    validate_import_configuration(&configuration)?;
    let mut imported_commands = Vec::new();
    let mut imported_catalog_count = 0_usize;
    for draft in drafts
        .iter()
        .filter(|catalog| is_carapace_draft_name(&catalog.name))
    {
        imported_catalog_count = imported_catalog_count.saturating_add(1);
        if draft.provenance.author != configuration.author
            || draft.provenance.license != configuration.license
            || draft.provenance.revision != configuration.revision
            || draft.provenance.source_url != configuration.source_url
        {
            return Err(input_error(
                "Carapace draft provenance does not match catalog/carapace-import.json",
            ));
        }
        if draft.commands.len() != 1 {
            return Err(input_error(format!(
                "Carapace draft {} must contain exactly one root command",
                draft.name
            )));
        }
        let command = &draft.commands[0];
        let expected_catalog_name = format!("{}-draft", command.name);
        if draft.name != expected_catalog_name {
            return Err(input_error(format!(
                "Carapace draft {} must use catalog identity {expected_catalog_name}",
                draft.name
            )));
        }
        let expected_path = root
            .join(DRAFT_DIRECTORY)
            .join(format!("{}{DRAFT_FILE_SUFFIX}", command.name));
        if !expected_path.is_file() {
            return Err(input_error(format!(
                "Carapace draft {} is not stored at {}",
                command.name,
                expected_path.display()
            )));
        }
        imported_commands.push(command.clone());
    }
    if imported_catalog_count == 0 {
        return Err(input_error(
            "catalog/draft must contain per-command Carapace drafts",
        ));
    }
    imported_commands.sort_by(|left, right| left.name.cmp(&right.name));
    let expected_draft_files = imported_commands
        .iter()
        .map(|command| format!("{DRAFT_DIRECTORY}/{}{DRAFT_FILE_SUFFIX}", command.name))
        .collect::<Vec<_>>();
    let draft = NativeCatalog {
        name: "carapace-draft".to_owned(),
        provenance: quirl_catalog::NativeProvenance {
            author: configuration.author.clone(),
            license: configuration.license.clone(),
            revision: configuration.revision.clone(),
            source_url: configuration.source_url.clone(),
        },
        commands: imported_commands,
    };
    let manifest: ImportManifest = read_json_bounded(&root.join(DRAFT_MANIFEST_FILE))?;
    let configured_roots = expanded_import_roots(&configuration);
    let mut configured_files = configured_roots
        .iter()
        .flat_map(|import_root| import_root.files.iter().cloned())
        .collect::<Vec<_>>();
    configured_files.sort();
    let (command_paths, command_count, flag_count) = catalog_statistics(&draft);
    if imported_catalog_count < configuration.coverage.root_commands_min
        || command_count < configuration.coverage.command_paths_min
        || flag_count < configuration.coverage.flags_min
    {
        return Err(input_error(format!(
            "Carapace import coverage regressed: roots {imported_catalog_count}/{}, paths {command_count}/{}, flags {flag_count}/{}",
            configuration.coverage.root_commands_min,
            configuration.coverage.command_paths_min,
            configuration.coverage.flags_min,
        )));
    }
    if manifest.source_url != configuration.source_url
        || manifest.revision != configuration.revision
        || manifest.license != configuration.license
        || manifest.license_file != configuration.license_file
        || manifest.license_sha256 != configuration.license_sha256
        || manifest.source_files != configured_files
        || manifest.draft_files != expected_draft_files
        || manifest.command_paths != command_paths
        || manifest.command_count != command_count
        || manifest.flag_count != flag_count
        || !manifest.semantic_diff.is_empty()
    {
        return Err(input_error(
            "Carapace import manifest drifted from the pinned configuration or draft; rerun the importer and review its semantic diff",
        ));
    }
    let provenance: ProvenanceRecord = read_json_bounded(&root.join(PROVENANCE_FILE))?;
    if provenance.source_url != configuration.source_url
        || provenance.revision != configuration.revision
        || provenance.license != configuration.license
        || provenance.license_file != LICENSE_FILE
        || provenance.license_sha256 != configuration.license_sha256
        || provenance.import_policy.trim().is_empty()
        || provenance.curation.trim().is_empty()
    {
        return Err(input_error(
            "catalog/provenance/carapace.json does not match the pinned import configuration",
        ));
    }
    let license = read_regular_file_bounded(&root.join(LICENSE_FILE), 64 * 1024)?;
    let observed_license = sha256_hex(&license);
    if observed_license != configuration.license_sha256.to_ascii_lowercase() {
        return Err(input_error(format!(
            "retained Carapace license checksum mismatch: expected {}, observed {observed_license}",
            configuration.license_sha256
        )));
    }
    Ok(())
}

fn render_catalog(catalog: &NativeCatalog) -> Result<String, TaskError> {
    let mut output = String::new();
    writeln!(output, "catalog {} {{", quote(&catalog.name)?)?;
    writeln!(
        output,
        "    provenance author={} license={} revision={} source={}",
        quote(&catalog.provenance.author)?,
        quote(&catalog.provenance.license)?,
        quote(&catalog.provenance.revision)?,
        quote(&catalog.provenance.source_url)?
    )?;
    for command in &catalog.commands {
        render_command(&mut output, command, 1)?;
    }
    output.push_str("}\n");
    Ok(output)
}

fn render_command(
    output: &mut String,
    command: &NativeCommand,
    depth: usize,
) -> Result<(), TaskError> {
    let indent = "    ".repeat(depth);
    write!(
        output,
        "{indent}command {} summary={} description={}",
        quote(&command.name)?,
        quote(&command.summary)?,
        quote(&command.description)?
    )?;
    let has_children = !command.aliases.is_empty()
        || !command.intents.is_empty()
        || !command.platforms.is_empty()
        || !command.flags.is_empty()
        || !command.arguments.is_empty()
        || !command.subcommands.is_empty();
    if !has_children {
        output.push('\n');
        return Ok(());
    }
    output.push_str(" {\n");
    for alias in &command.aliases {
        writeln!(output, "{indent}    alias {}", quote(alias)?)?;
    }
    for intent in &command.intents {
        writeln!(output, "{indent}    intent {}", quote(intent)?)?;
    }
    for platform in &command.platforms {
        writeln!(
            output,
            "{indent}    platform {}",
            quote(platform_name(*platform))?
        )?;
    }
    for flag in &command.flags {
        render_flag(output, flag, &command.platforms, &format!("{indent}    "))?;
    }
    for argument in &command.arguments {
        render_argument(output, argument, &format!("{indent}    "))?;
    }
    for child in &command.subcommands {
        render_command(output, child, depth.saturating_add(1))?;
    }
    writeln!(output, "{indent}}}")?;
    Ok(())
}

fn render_flag(
    output: &mut String,
    flag: &NativeFlag,
    inherited_platforms: &[NativePlatform],
    indent: &str,
) -> Result<(), TaskError> {
    write!(
        output,
        "{indent}flag {} summary={} description={}",
        quote(&flag.name)?,
        quote(&flag.summary)?,
        quote(&flag.description)?
    )?;
    if let Some(short) = &flag.short {
        write!(output, " short={}", quote(short)?)?;
    }
    if let Some(value) = &flag.value_name {
        write!(output, " value={}", quote(value)?)?;
    }
    if flag.required {
        output.push_str(" required=#true");
    }
    if flag.repeatable {
        output.push_str(" repeatable=#true");
    }
    if let Some(action) = flag.action {
        write!(output, " action={}", quote(action_name(action))?)?;
    }
    if flag.platforms == inherited_platforms {
        output.push('\n');
        return Ok(());
    }
    output.push_str(" {\n");
    for platform in &flag.platforms {
        writeln!(
            output,
            "{indent}    platform {}",
            quote(platform_name(*platform))?
        )?;
    }
    writeln!(output, "{indent}}}")?;
    Ok(())
}

fn render_argument(
    output: &mut String,
    argument: &NativeArgument,
    indent: &str,
) -> Result<(), TaskError> {
    write!(
        output,
        "{indent}argument {} summary={} description={}",
        quote(&argument.name)?,
        quote(&argument.summary)?,
        quote(&argument.description)?
    )?;
    if argument.required {
        output.push_str(" required=#true");
    }
    if argument.repeatable {
        output.push_str(" repeatable=#true");
    }
    if let Some(action) = argument.action {
        write!(output, " action={}", quote(action_name(action))?)?;
    }
    output.push('\n');
    Ok(())
}

fn quote(value: &str) -> Result<String, TaskError> {
    Ok(serde_json::to_string(value)?)
}

fn platform_name(platform: NativePlatform) -> &'static str {
    match platform {
        NativePlatform::Any => "any",
        NativePlatform::Linux => "linux",
        NativePlatform::Macos => "macos",
        NativePlatform::Windows => "windows",
        NativePlatform::Freebsd => "freebsd",
    }
}

fn action_name(action: NativeCompletionAction) -> &'static str {
    match action {
        NativeCompletionAction::Files => "files",
        NativeCompletionAction::Directories => "directories",
        NativeCompletionAction::Executables => "executables",
        NativeCompletionAction::Users => "users",
        NativeCompletionAction::Groups => "groups",
        NativeCompletionAction::Hostnames => "hostnames",
        NativeCompletionAction::EnvironmentVariables => "environment_variables",
    }
}

fn semantic_diff(previous: Option<&NativeCatalog>, next: &NativeCatalog) -> Vec<String> {
    let before = previous.map(command_fingerprints).unwrap_or_default();
    let after = command_fingerprints(next);
    let mut changes = Vec::new();
    for path in before.keys() {
        if !after.contains_key(path) {
            changes.push(format!("removed {path}"));
        }
    }
    for (path, fingerprint) in &after {
        match before.get(path) {
            None => changes.push(format!("added {path}")),
            Some(previous) if previous != fingerprint => changes.push(format!("changed {path}")),
            Some(_) => {}
        }
    }
    changes
}

fn command_fingerprints(catalog: &NativeCatalog) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let mut stack = catalog
        .commands
        .iter()
        .map(|command| (command, command.name.clone()))
        .collect::<Vec<_>>();
    while let Some((command, path)) = stack.pop() {
        let fingerprint = format!(
            "{}|{}|{:?}|{:?}|{:?}|{:?}",
            command.summary,
            command.description,
            command.aliases,
            command.platforms,
            command.flags,
            command.arguments
        );
        result.insert(path.clone(), fingerprint);
        for child in command.subcommands.iter().rev() {
            stack.push((child, format!("{path} {}", child.name)));
        }
    }
    result
}

fn catalog_statistics(catalog: &NativeCatalog) -> (Vec<String>, usize, usize) {
    let mut paths = Vec::new();
    let mut flags = 0_usize;
    let mut stack = catalog
        .commands
        .iter()
        .map(|command| (command, command.name.clone()))
        .collect::<Vec<_>>();
    while let Some((command, path)) = stack.pop() {
        paths.push(path.clone());
        flags = flags.saturating_add(command.flags.len());
        for child in command.subcommands.iter().rev() {
            stack.push((child, format!("{path} {}", child.name)));
        }
    }
    paths.sort();
    let count = paths.len();
    (paths, count, flags)
}

fn checksum_line(bytes: &[u8]) -> String {
    let mut output = sha256_hex(bytes);
    output.push_str("  catalog.sqlite3\n");
    output
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "SHA-256 is specified in terms of wrapping fixed-width arithmetic"
)]
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64 + "  catalog.sqlite3\n".len());
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn catalog_source_paths(root: &Path) -> Result<Vec<PathBuf>, TaskError> {
    let mut paths = kdl_files(&root.join(CURATED_DIRECTORY))?;
    paths.extend(kdl_files(&root.join(DRAFT_DIRECTORY))?);
    paths.sort();
    if paths.len() > CATALOG_FILE_COUNT_MAX {
        return Err(resource_error(
            "catalog KDL file count",
            CATALOG_FILE_COUNT_MAX,
            paths.len(),
        ));
    }
    let mut total_bytes = 0_usize;
    for path in &paths {
        let metadata = fs::symlink_metadata(path)?;
        total_bytes =
            total_bytes.saturating_add(usize::try_from(metadata.len()).unwrap_or(usize::MAX));
        if total_bytes > CATALOG_TOTAL_BYTES_MAX {
            return Err(resource_error(
                "catalog source bytes",
                CATALOG_TOTAL_BYTES_MAX,
                total_bytes,
            ));
        }
    }
    Ok(paths)
}

fn kdl_files(directory: &Path) -> Result<Vec<PathBuf>, TaskError> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.file_type().is_dir() {
        return Err(input_error(format!(
            "catalog source path is not a directory: {}",
            directory.display()
        )));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(input_error(format!(
                "catalog source symlink is not allowed: {}",
                entry.path().display()
            )));
        }
        if file_type.is_file() && entry.path().extension() == Some(OsStr::new("kdl")) {
            paths.push(entry.path());
        } else if file_type.is_file()
            && entry.path().file_name() == Some(OsStr::new("carapace.import.json"))
            && directory.ends_with(DRAFT_DIRECTORY)
        {
            continue;
        } else {
            return Err(input_error(format!(
                "unexpected entry in catalog source directory: {}",
                entry.path().display()
            )));
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_catalog_source(path: &Path) -> Result<String, TaskError> {
    let bytes = read_regular_file_bounded(path, NativeCatalogLimits::default().source_bytes_max)?;
    let source = String::from_utf8(bytes)
        .map_err(|_| input_error(format!("catalog KDL is not UTF-8: {}", path.display())))?;
    if source.len() > CATALOG_TOTAL_BYTES_MAX {
        return Err(resource_error(
            "catalog source bytes",
            CATALOG_TOTAL_BYTES_MAX,
            source.len(),
        ));
    }
    Ok(source)
}

fn read_json_bounded<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, TaskError> {
    let bytes = read_regular_file_bounded(path, SOURCE_FILE_BYTES_MAX)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_regular_file_bounded(path: &Path, bytes_max: usize) -> Result<Vec<u8>, TaskError> {
    let metadata = fs::symlink_metadata(path)?;
    validate_input_metadata(path, &metadata, bytes_max)?;
    let observed = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    let mut file = File::open(path)?;
    let handle_metadata = file.metadata()?;
    validate_input_metadata(path, &handle_metadata, bytes_max)?;
    if !same_file_metadata(&metadata, &handle_metadata) {
        return Err(input_error(format!(
            "input path changed while opening: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(observed.min(bytes_max));
    Read::by_ref(&mut file)
        .take(
            u64::try_from(bytes_max)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
    if bytes.len() > bytes_max {
        return Err(resource_error("input file bytes", bytes_max, bytes.len()));
    }
    let final_metadata = fs::symlink_metadata(path)?;
    validate_input_metadata(path, &final_metadata, bytes_max)?;
    if !same_file_metadata(&metadata, &final_metadata) {
        return Err(input_error(format!(
            "input path changed while reading: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn validate_input_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    bytes_max: usize,
) -> Result<(), TaskError> {
    if !metadata.file_type().is_file() {
        return Err(input_error(format!(
            "input is not a regular file: {}",
            path.display()
        )));
    }
    let observed = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if observed > bytes_max {
        return Err(resource_error("input file bytes", bytes_max, observed));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(input_error(format!(
                "input has {} hard links: {}",
                metadata.nlink(),
                path.display()
            )));
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(input_error(format!(
                "input has unsafe mode {mode:#o}: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn same_file_metadata(expected: &fs::Metadata, observed: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    expected.dev() == observed.dev()
        && expected.ino() == observed.ino()
        && expected.len() == observed.len()
        && expected.mtime() == observed.mtime()
        && expected.mtime_nsec() == observed.mtime_nsec()
}

#[cfg(not(unix))]
fn same_file_metadata(expected: &fs::Metadata, observed: &fs::Metadata) -> bool {
    expected.len() == observed.len()
        && expected.file_type() == observed.file_type()
        && expected.modified().ok() == observed.modified().ok()
}

fn checkout_revision(source: &Path) -> Result<String, TaskError> {
    let dot_git = source.join(".git");
    let metadata = fs::symlink_metadata(&dot_git)?;
    let git_directory = if metadata.file_type().is_dir() {
        dot_git
    } else if metadata.file_type().is_file() {
        let pointer = String::from_utf8(read_regular_file_bounded(&dot_git, 4096)?)?;
        let path = pointer
            .trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| input_error("invalid .git indirection file"))?;
        let candidate = PathBuf::from(path);
        if candidate.is_absolute() {
            candidate
        } else {
            source.join(candidate)
        }
    } else {
        return Err(input_error(
            "Carapace checkout .git is not a file or directory",
        ));
    };
    let head = String::from_utf8(read_regular_file_bounded(
        &git_directory.join("HEAD"),
        4096,
    )?)?;
    let revision = head.trim();
    if revision.starts_with("ref: ") {
        return Err(input_error(
            "Carapace checkout must be detached at the explicitly pinned revision",
        ));
    }
    validate_revision(revision)?;
    Ok(revision.to_ascii_lowercase())
}

fn verify_checkout_sources(
    source: &Path,
    revision: &str,
    configuration: &ImportConfiguration,
) -> Result<(), TaskError> {
    let roots = expanded_import_roots(configuration);
    let mut files = roots
        .iter()
        .flat_map(|root| root.files.iter().map(String::as_str))
        .chain(std::iter::once(configuration.license_file.as_str()))
        .collect::<Vec<_>>();
    files.sort_unstable();
    for relative in &files {
        let object = format!("{revision}:{relative}");
        let arguments = vec!["cat-file".to_owned(), "-e".to_owned(), object];
        let status = run_bounded_git(source, &arguments)?;
        if !status.success() {
            return Err(input_error(format!(
                "manifest-listed Carapace source is absent from pinned revision: {relative}"
            )));
        }
    }

    let mut arguments = vec![
        "diff".to_owned(),
        "--no-ext-diff".to_owned(),
        "--no-textconv".to_owned(),
        "--quiet".to_owned(),
        revision.to_owned(),
        "--".to_owned(),
    ];
    arguments.extend(files.into_iter().map(str::to_owned));
    let status = run_bounded_git(source, &arguments)?;
    if status.success() {
        return Ok(());
    }
    if status.code() == Some(1) {
        return Err(input_error(
            "manifest-listed Carapace sources differ from the pinned revision",
        ));
    }
    Err(input_error(format!(
        "could not verify manifest-listed Carapace sources at the pinned revision: {status}"
    )))
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "poll counts are bounded by the subprocess deadline"
)]
fn run_bounded_git(source: &Path, arguments: &[String]) -> Result<ExitStatus, TaskError> {
    let mut child = Command::new("git")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .arg("-C")
        .arg(source)
        .args(arguments)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(input_error(format!(
                "Git checkout verification exceeded {} seconds",
                GIT_TIMEOUT.as_secs()
            )));
        }
        // Git receives no stdin and cannot invoke external diff, hooks, or an
        // fsmonitor; this short poll makes cancellation and reaping explicit.
        thread::sleep(Duration::from_millis(10));
    }
}

fn validate_revision(revision: &str) -> Result<(), TaskError> {
    if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(input_error(
            "Carapace revision must be exactly 40 hexadecimal characters",
        ))
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "platform tuples are validated as exactly two fields before access"
)]
fn parse_platforms(values: &[String]) -> Result<Vec<NativePlatform>, TaskError> {
    if values.is_empty() {
        return Ok(vec![NativePlatform::Any]);
    }
    let mut platforms = Vec::new();
    for value in values {
        let platform = match value.as_str() {
            "any" => NativePlatform::Any,
            "linux" => NativePlatform::Linux,
            "macos" => NativePlatform::Macos,
            "windows" => NativePlatform::Windows,
            "freebsd" => NativePlatform::Freebsd,
            _ => return Err(input_error(format!("unknown import platform {value}"))),
        };
        platforms.push(platform);
    }
    platforms.sort_unstable();
    if platforms.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(input_error("duplicate import platform"));
    }
    if platforms.contains(&NativePlatform::Any) && platforms.len() != 1 {
        return Err(input_error(
            "platform any cannot be combined with specific platforms",
        ));
    }
    Ok(platforms)
}

fn validate_relative_source_path(value: &str) -> Result<(), TaskError> {
    let path = Path::new(value);
    validate_normalized_relative_path(value, "Carapace source path")?;
    if path.extension() != Some(OsStr::new("go")) {
        return Err(input_error(format!(
            "Carapace source path must be a normalized relative .go path: {value}"
        )));
    }
    Ok(())
}

fn validate_normalized_relative_path(value: &str, label: &str) -> Result<(), TaskError> {
    let path = Path::new(value);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(input_error(format!(
            "{label} must be a normalized relative path: {value}"
        )));
    }
    Ok(())
}

fn validate_no_symlink_components(root: &Path, relative: &str) -> Result<(), TaskError> {
    let mut path = root.to_path_buf();
    for component in Path::new(relative).components() {
        let Component::Normal(component) = component else {
            return Err(input_error(format!(
                "invalid source component in {relative}"
            )));
        };
        path.push(component);
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(input_error(format!(
                "Carapace source path contains a symlink: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn validate_go_identifier(value: &str) -> Result<(), TaskError> {
    let mut bytes = value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if valid {
        Ok(())
    } else {
        Err(input_error(format!("invalid Go identifier {value}")))
    }
}

fn validate_catalog_identifier(value: &str) -> Result<(), TaskError> {
    let mut bytes = value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(input_error(format!(
            "invalid command or alias identifier {value}"
        )))
    }
}

fn validate_long_name(value: &str) -> Result<(), TaskError> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Ok(())
    } else {
        Err(input_error(format!("invalid imported long flag {value}")))
    }
}

struct TemporaryFile {
    path: PathBuf,
    armed: bool,
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), TaskError> {
    let parent = path
        .parent()
        .ok_or_else(|| input_error("output has no parent directory"))?;
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() {
        return Err(input_error(format!(
            "output parent is not a directory: {}",
            parent.display()
        )));
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| input_error("output has no file name"))?;
    for _ in 0..TEMPORARY_ATTEMPTS_MAX {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary_path = path.with_file_name(format!(
            ".{}.xtask-{}-{sequence}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary_path) {
            Ok(mut file) => {
                let mut temporary = TemporaryFile {
                    path: temporary_path,
                    armed: true,
                };
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
                fs::rename(&temporary.path, path)?;
                temporary.armed = false;
                let _ = File::open(parent).and_then(|directory| directory.sync_all());
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(resource_error(
        "temporary output attempts",
        TEMPORARY_ATTEMPTS_MAX,
        TEMPORARY_ATTEMPTS_MAX,
    ))
}

fn map_diagnostic(diagnostic: NativeCatalogDiagnostic) -> TaskError {
    let mut message = format!(
        "native catalog {:?}: {}: {}",
        diagnostic.kind, diagnostic.source_name, diagnostic.message
    );
    if let Some(offset) = diagnostic.byte_offset {
        let _ = write!(message, " (byte {offset}");
        if let Some(length) = diagnostic.byte_length {
            let _ = write!(message, ", length {length}");
        }
        message.push(')');
    }
    for context in diagnostic.context {
        let _ = write!(message, "; {context}");
    }
    let _ = write!(message, ". Help: {}", diagnostic.help);
    input_error(message)
}

fn input_error(message: impl Into<String>) -> TaskError {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn resource_error(label: &str, limit: usize, observed: usize) -> TaskError {
    input_error(format!(
        "resource limit exceeded for {label}: limit {limit}, observed {observed}"
    ))
}

fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVISION: &str = "ebfb9beda84fdf057dc9a89b59527158d23d323c";

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "quirl-xtask-catalog-{label}-{}-{}",
                std::process::id(),
                NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_catalog() -> NativeCatalog {
        NativeCatalog {
            name: "fixture".to_owned(),
            provenance: quirl_catalog::NativeProvenance {
                author: "Fixture authors".to_owned(),
                license: "MIT".to_owned(),
                revision: REVISION.to_owned(),
                source_url: "https://example.invalid/fixture".to_owned(),
            },
            commands: vec![NativeCommand {
                name: "tool".to_owned(),
                aliases: vec!["t".to_owned()],
                summary: "Use tool".to_owned(),
                description: "Use the fixture tool.".to_owned(),
                intents: vec!["operate fixture".to_owned()],
                platforms: vec![NativePlatform::Linux, NativePlatform::Macos],
                flags: vec![NativeFlag {
                    name: "--path".to_owned(),
                    short: Some("-p".to_owned()),
                    summary: "Select path".to_owned(),
                    description: "Select one filesystem path.".to_owned(),
                    value_name: Some("path".to_owned()),
                    required: false,
                    repeatable: false,
                    action: Some(NativeCompletionAction::Files),
                    platforms: vec![NativePlatform::Linux, NativePlatform::Macos],
                }],
                arguments: Vec::new(),
                subcommands: Vec::new(),
            }],
        }
    }

    fn prepare_import_workspace() -> (TestDirectory, PathBuf, Vec<u8>, String) {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let fixture = workspace.join("catalog/fixtures/carapace");
        let directory = TestDirectory::new("import-e2e");
        let catalog_root = directory.0.join("catalog");
        fs::create_dir_all(catalog_root.join("curated")).unwrap();
        fs::create_dir(catalog_root.join("draft")).unwrap();
        let curated = render_catalog(&fixture_catalog()).unwrap();
        let curated_path = catalog_root.join("curated/native.kdl");
        fs::write(&curated_path, curated).unwrap();
        let curated_before = fs::read(&curated_path).unwrap();
        let source = directory.0.join("carapace");
        let source_file = source.join("completers/common/fd_completer/cmd/root.go");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::copy(fixture.join("fd-root.go"), source_file).unwrap();
        fs::copy(fixture.join("LICENSE"), source.join("LICENSE")).unwrap();
        for arguments in [
            vec!["init".to_owned(), "--quiet".to_owned()],
            vec!["add".to_owned(), ".".to_owned()],
            vec![
                "-c".to_owned(),
                "user.name=Quirl test".to_owned(),
                "-c".to_owned(),
                "user.email=quirl@example.invalid".to_owned(),
                "commit".to_owned(),
                "--quiet".to_owned(),
                "-m".to_owned(),
                "fixture".to_owned(),
            ],
        ] {
            assert!(run_bounded_git(&source, &arguments).unwrap().success());
        }
        let head = fs::read_to_string(source.join(".git/HEAD")).unwrap();
        let reference = head.trim().strip_prefix("ref: ").unwrap();
        let revision = fs::read_to_string(source.join(".git").join(reference))
            .unwrap()
            .trim()
            .to_owned();
        let detach = vec![
            "switch".to_owned(),
            "--detach".to_owned(),
            "--quiet".to_owned(),
            revision.clone(),
        ];
        assert!(run_bounded_git(&source, &detach).unwrap().success());
        let configuration = serde_json::json!({
            "source_url": "https://github.com/carapace-sh/carapace-bin",
            "revision": revision,
            "license": "MIT",
            "license_file": "LICENSE",
            "license_sha256": "25dbdc5c4265ee2983e2b2cd0b5073df0a24aecc0de14a49d01ec9c702f7f22c",
            "author": "Carapace contributors",
            "coverage": {
                "root_commands_min": 1,
                "command_paths_min": 1,
                "flags_min": 1
            },
            "roots": [{
                "variable": "rootCmd",
                "platforms": ["linux", "macos"],
                "files": ["completers/common/fd_completer/cmd/root.go"]
            }]
        });
        fs::write(
            directory.0.join(IMPORT_CONFIGURATION),
            serde_json::to_vec_pretty(&configuration).unwrap(),
        )
        .unwrap();
        (directory, source, curated_before, revision)
    }

    #[test]
    fn canonical_format_is_idempotent() {
        let first = render_catalog(&fixture_catalog()).unwrap();
        let parsed =
            parse_native_catalog(&first, "fixture.kdl", NativeCatalogLimits::default()).unwrap();
        let second = render_catalog(&parsed).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn deterministic_build_has_stable_checksum() {
        let catalog = fixture_catalog();
        let first = compile_native_catalog(&catalog, NativeCatalogLimits::default()).unwrap();
        let second = compile_native_catalog(&catalog, NativeCatalogLimits::default()).unwrap();
        assert_eq!(first, second);
        assert_eq!(checksum_line(&first), checksum_line(&second));
    }

    #[test]
    fn semantic_diff_is_sorted_and_meaningful() {
        let previous = fixture_catalog();
        let mut next = previous.clone();
        next.commands[0].summary = "Changed".to_owned();
        next.commands.push(NativeCommand {
            name: "added".to_owned(),
            aliases: Vec::new(),
            summary: "Added".to_owned(),
            description: "Added command.".to_owned(),
            intents: Vec::new(),
            platforms: vec![NativePlatform::Any],
            flags: Vec::new(),
            arguments: Vec::new(),
            subcommands: Vec::new(),
        });
        assert_eq!(
            semantic_diff(Some(&previous), &next),
            vec!["added added", "changed tool"]
        );
    }

    #[test]
    fn parser_rejects_oversized_upstream_source() {
        let source = "x".repeat(SOURCE_FILE_BYTES_MAX + 1);
        let error = parse_go_file(&source, "oversized.go").unwrap_err();
        assert!(error.to_string().contains("resource limit exceeded"));
    }

    #[test]
    fn parser_rejects_dynamic_or_incomplete_upstream_definitions() {
        let source = "var rootCmd = &cobra.Command{Use: commandName, Short: \"dynamic\"}";
        let error = parse_go_file(source, "dynamic.go").unwrap_err();
        assert!(error.to_string().contains("no static"));
    }

    #[test]
    fn parser_rejects_unreviewed_cobra_flag_methods() {
        let source = r##"
var rootCmd = &cobra.Command{
    Use: "tool",
    Short: "Tool",
}
func init() {
    rootCmd.Flags().Uint("count", 0, "Count")
}
"##;
        let error = parse_go_file(source, "unsupported.go").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported Cobra flag method Uint")
        );
    }

    #[test]
    fn parser_rejects_duplicate_flags_instead_of_silently_dropping_one() {
        let source = r#"
var rootCmd = &cobra.Command{
    Use: "tool",
    Short: "Tool",
}
func init() {
    rootCmd.Flags().Bool("verbose", false, "First")
    rootCmd.Flags().Bool("verbose", false, "Second")
}
"#;
        let error = parse_go_file(source, "duplicate.go").unwrap_err();
        assert!(error.to_string().contains("duplicate imported flag"));
    }

    #[test]
    fn incompatible_upstream_flag_spellings_are_recorded_as_omissions() {
        let source = r##"
var rootCmd = &cobra.Command{
    Use: "tool",
    Short: "Tool",
}
func init() {
    rootCmd.Flags().BoolP("valid", "#", false, "Valid flag")
    rootCmd.Flags().Bool("UPPER", false, "Uppercase flag")
    rootCmd.Flags().Bool("empty", false, "")
}
"##;
        let parsed = parse_go_file(source, "flags.go").unwrap();
        assert_eq!(parsed[0].command.flags.len(), 1);
        assert_eq!(parsed[0].command.flags[0].name, "--valid");
        assert_eq!(parsed[0].command.flags[0].short, None);
        assert_eq!(parsed[0].omitted_constructs.len(), 3);
        assert!(
            parsed[0]
                .omitted_constructs
                .iter()
                .all(|omission| omission.starts_with("flags.go:rootCmd:"))
        );
    }

    #[test]
    fn parser_ignores_commented_out_go_constructs() {
        let source = r#"
/*
var fakeCmd = &cobra.Command{
    Use: "fake",
    Short: "Fake",
}
fakeCmd.Flags().Bool("fabricated", false, "Fabricated")
*/
var rootCmd = &cobra.Command{
    Use: "tool",
    Short: "Tool",
}
// rootCmd.Flags().Bool("commented", false, "Commented")
func init() {
    rootCmd.Flags().Bool("real", false, "Real")
}
"#;
        let parsed = parse_go_file(source, "comments.go").unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].command.name, "tool");
        assert_eq!(parsed[0].command.flags[0].name, "--real");
    }

    #[test]
    fn completion_actions_are_scoped_to_their_cobra_command() {
        let source = r#"
var firstCmd = &cobra.Command{
    Use: "first",
    Short: "First",
}
var secondCmd = &cobra.Command{
    Use: "second",
    Short: "Second",
}
func init() {
    firstCmd.Flags().String("path", "", "First path")
    secondCmd.Flags().String("path", "", "Second path")
    carapace.Gen(firstCmd).FlagCompletion(carapace.ActionMap{
        "path": carapace.ActionFiles(),
    })
}
"#;
        let parsed = parse_go_file(source, "scoped.go").unwrap();
        let first = parsed
            .iter()
            .find(|command| command.variable == "firstCmd")
            .unwrap();
        let second = parsed
            .iter()
            .find(|command| command.variable == "secondCmd")
            .unwrap();
        assert_eq!(
            first.command.flags[0].action,
            Some(NativeCompletionAction::Files)
        );
        assert_eq!(second.command.flags[0].action, None);
    }

    #[test]
    fn positional_completion_is_recorded_as_an_omission_not_invented_metadata() {
        let source = r#"
var rootCmd = &cobra.Command{
    Use: "tool",
    Short: "Tool",
}
func init() {
    carapace.Gen(rootCmd).PositionalAnyCompletion(carapace.ActionFiles())
}
"#;
        let parsed = parse_go_file(source, "positionals.go").unwrap();
        assert!(parsed[0].command.arguments.is_empty());
        assert_eq!(parsed[0].omitted_constructs.len(), 1);
        assert!(parsed[0].omitted_constructs[0].contains("PositionalAnyCompletion"));
    }

    #[test]
    fn imported_draft_may_propose_updates_to_curated_roots() {
        let curated = fixture_catalog();
        let mut draft = fixture_catalog();
        draft.name = "fixture-draft".to_owned();
        validate_separation(&curated, &[draft]).unwrap();
    }

    #[test]
    fn failed_atomic_write_removes_staging_file() {
        let directory = TestDirectory::new("cleanup");
        let destination = directory.0.join("destination");
        fs::create_dir(&destination).unwrap();
        let error = atomic_write(&destination, b"cannot replace directory").unwrap_err();
        assert!(!error.to_string().is_empty());
        let entries = fs::read_dir(&directory.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![OsStr::new("destination")]);
    }

    #[test]
    fn failed_build_preserves_existing_artifacts_and_leaves_no_staging_files() {
        let directory = TestDirectory::new("build-cleanup");
        fs::create_dir_all(directory.0.join(CURATED_DIRECTORY)).unwrap();
        fs::create_dir(directory.0.join(DRAFT_DIRECTORY)).unwrap();
        fs::create_dir(directory.0.join(GENERATED_DIRECTORY)).unwrap();
        fs::write(
            directory.0.join("catalog/curated/broken.kdl"),
            "catalog \"broken\" {",
        )
        .unwrap();
        fs::write(directory.0.join(DATABASE_FILE), b"old database").unwrap();
        fs::write(directory.0.join(CHECKSUM_FILE), b"old checksum").unwrap();
        assert!(build_catalog(&directory.0).is_err());
        assert_eq!(
            fs::read(directory.0.join(DATABASE_FILE)).unwrap(),
            b"old database"
        );
        assert_eq!(
            fs::read(directory.0.join(CHECKSUM_FILE)).unwrap(),
            b"old checksum"
        );
        let staging = fs::read_dir(directory.0.join(GENERATED_DIRECTORY))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with('.'))
            .collect::<Vec<_>>();
        assert!(staging.is_empty());
    }

    #[test]
    fn vendored_carapace_excerpt_import_is_deterministic() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let fixture = workspace.join("catalog/fixtures/carapace");
        let source = fs::read_to_string(fixture.join("fd-root.go")).unwrap();
        let first = parse_go_file(&source, "fd-root.go").unwrap();
        let second = parse_go_file(&source, "fd-root.go").unwrap();
        assert_eq!(first.len(), 1);
        let first_render = render_catalog(&NativeCatalog {
            name: "carapace-draft".to_owned(),
            provenance: fixture_catalog().provenance,
            commands: vec![first.into_iter().next().unwrap().command],
        })
        .unwrap();
        let second_render = render_catalog(&NativeCatalog {
            name: "carapace-draft".to_owned(),
            provenance: fixture_catalog().provenance,
            commands: vec![second.into_iter().next().unwrap().command],
        })
        .unwrap();
        assert_eq!(first_render, second_render);
        assert_eq!(
            first_render,
            fs::read_to_string(fixture.join("fd.expected.kdl")).unwrap()
        );
        assert!(fixture.join("LICENSE").is_file());
        assert!(fixture.join("ATTRIBUTION.md").is_file());
    }

    #[test]
    fn imported_roots_render_as_separate_review_files() {
        let directory = TestDirectory::new("per-command-drafts");
        fs::create_dir_all(directory.0.join(DRAFT_DIRECTORY)).unwrap();
        let mut catalog = fixture_catalog();
        catalog.commands.push(NativeCommand {
            name: "second".to_owned(),
            aliases: Vec::new(),
            summary: "Second tool".to_owned(),
            description: "Use the second fixture tool.".to_owned(),
            intents: Vec::new(),
            platforms: vec![NativePlatform::Any],
            flags: Vec::new(),
            arguments: Vec::new(),
            subcommands: Vec::new(),
        });
        let rendered = render_imported_drafts(&directory.0, &catalog).unwrap();
        let names = rendered
            .iter()
            .map(|draft| {
                draft
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(names, ["second.kdl", "tool.kdl"]);
        for draft in rendered {
            let source = std::str::from_utf8(&draft.bytes).unwrap();
            let parsed = parse_native_catalog(
                source,
                &draft.path.display().to_string(),
                NativeCatalogLimits::default(),
            )
            .unwrap();
            assert_eq!(parsed.commands.len(), 1);
        }
    }

    #[test]
    fn publishing_per_command_drafts_removes_the_legacy_aggregate() {
        let directory = TestDirectory::new("legacy-draft");
        fs::create_dir_all(directory.0.join(DRAFT_DIRECTORY)).unwrap();
        let legacy = directory.0.join(LEGACY_DRAFT_FILE);
        fs::write(&legacy, b"legacy aggregate").unwrap();
        let rendered = render_imported_drafts(&directory.0, &fixture_catalog()).unwrap();
        publish_imported_drafts(&directory.0, &rendered).unwrap();
        assert!(!legacy.exists());
        assert!(directory.0.join("catalog/draft/tool.kdl").is_file());
    }

    #[test]
    fn promotion_is_explicit_and_never_overwrites_curated_source() {
        let directory = TestDirectory::new("promote");
        fs::create_dir_all(directory.0.join(CURATED_DIRECTORY)).unwrap();
        fs::create_dir_all(directory.0.join(DRAFT_DIRECTORY)).unwrap();

        let mut curated = fixture_catalog();
        curated.name = "base".to_owned();
        curated.commands[0].name = "base".to_owned();
        curated.commands[0].aliases.clear();
        fs::write(
            directory.0.join("catalog/curated/base.kdl"),
            render_catalog(&curated).unwrap(),
        )
        .unwrap();

        let mut draft = fixture_catalog();
        draft.name = "new-draft".to_owned();
        draft.commands[0].name = "new".to_owned();
        draft.commands[0].aliases.clear();
        fs::write(
            directory.0.join("catalog/draft/new.kdl"),
            render_catalog(&draft).unwrap(),
        )
        .unwrap();

        promote_drafts(&directory.0, &["new".to_owned()]).unwrap();
        let promoted_path = directory.0.join("catalog/curated/new.kdl");
        let promoted = fs::read(&promoted_path).unwrap();
        let error = promote_drafts(&directory.0, &["new".to_owned()]).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert_eq!(fs::read(promoted_path).unwrap(), promoted);
    }

    #[test]
    fn checked_in_carapace_drafts_cover_core_platforms_and_nested_commands() {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let mut catalogs = kdl_files(&workspace.join(DRAFT_DIRECTORY))
            .unwrap()
            .into_iter()
            .map(|path| load_one_catalog(&workspace, &path, true))
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        catalogs.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(catalogs.len(), 90);
        let roots = catalogs
            .iter()
            .map(|catalog| catalog.commands[0].name.as_str())
            .collect::<BTreeSet<_>>();
        assert!(roots.is_superset(&BTreeSet::from([
            "age",
            "cat",
            "cd",
            "code",
            "cp",
            "fd",
            "ffmpeg",
            "fzf",
            "grep",
            "head",
            "jq",
            "just",
            "ls",
            "mkdir",
            "mv",
            "node",
            "pwd",
            "python",
            "rg",
            "rm",
            "rmdir",
            "rsync",
            "rustc",
            "ssh",
            "tail",
            "tar",
            "task",
            "tree",
            "vim",
            "watchexec",
            "wget",
            "zip",
        ])));

        let ls = catalogs
            .iter()
            .find(|catalog| catalog.commands[0].name == "ls")
            .unwrap();
        assert_eq!(
            ls.commands[0].platforms,
            [NativePlatform::Linux, NativePlatform::Freebsd]
        );
        let mkdir = catalogs
            .iter()
            .find(|catalog| catalog.commands[0].name == "mkdir")
            .unwrap();
        assert_eq!(mkdir.commands[0].platforms, [NativePlatform::Windows]);

        let task = catalogs
            .iter()
            .find(|catalog| catalog.commands[0].name == "task")
            .unwrap();
        let completion = task.commands[0]
            .subcommands
            .iter()
            .find(|command| command.name == "completion")
            .unwrap();
        assert_eq!(
            completion
                .subcommands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["bash", "fish", "powershell", "zsh"]
        );
    }

    #[test]
    fn importer_e2e_is_deterministic_and_preserves_curated_source() {
        let (directory, source, curated_before, revision) = prepare_import_workspace();
        import_carapace(&directory.0, &source, &revision).unwrap();
        import_carapace(&directory.0, &source, &revision).unwrap();
        let draft_path = directory.0.join("catalog/draft/fd.kdl");
        let draft_second = fs::read(&draft_path).unwrap();
        let manifest_second = fs::read(directory.0.join(DRAFT_MANIFEST_FILE)).unwrap();
        import_carapace(&directory.0, &source, &revision).unwrap();
        assert_eq!(fs::read(&draft_path).unwrap(), draft_second);
        assert_eq!(
            fs::read(directory.0.join(DRAFT_MANIFEST_FILE)).unwrap(),
            manifest_second
        );
        assert_eq!(
            fs::read(directory.0.join("catalog/curated/native.kdl")).unwrap(),
            curated_before
        );
        let manifest: ImportManifest = serde_json::from_slice(&manifest_second).unwrap();
        assert!(manifest.semantic_diff.is_empty());
        assert_eq!(manifest.omitted_constructs.len(), 1);
    }

    #[test]
    fn importer_rejects_a_revision_other_than_the_explicit_pin() {
        let (directory, source, curated_before, _revision) = prepare_import_workspace();
        let wrong_revision = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let error = import_carapace(&directory.0, &source, wrong_revision).unwrap_err();
        assert!(error.to_string().contains("does not match pinned revision"));
        assert!(
            managed_carapace_draft_paths(&directory.0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fs::read(directory.0.join("catalog/curated/native.kdl")).unwrap(),
            curated_before
        );
    }

    #[test]
    fn importer_rejects_dirty_manifest_listed_sources() {
        let (directory, source, curated_before, revision) = prepare_import_workspace();
        let source_file = source.join("completers/common/fd_completer/cmd/root.go");
        let mut modified = fs::read(&source_file).unwrap();
        modified.extend_from_slice(b"\n// modified after checkout\n");
        fs::write(&source_file, modified).unwrap();
        let error = import_carapace(&directory.0, &source, &revision).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differ from the pinned revision")
        );
        assert!(
            managed_carapace_draft_paths(&directory.0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fs::read(directory.0.join("catalog/curated/native.kdl")).unwrap(),
            curated_before
        );
    }

    #[test]
    fn importer_rejects_a_license_outside_the_reviewed_digest() {
        let (directory, source, _curated_before, _revision) = prepare_import_workspace();
        let configuration: ImportConfiguration =
            read_json_bounded(&directory.0.join(IMPORT_CONFIGURATION)).unwrap();
        fs::write(source.join("LICENSE"), b"MIT License\ntruncated\n").unwrap();
        let error = validate_upstream_license(&source, &configuration).unwrap_err();
        assert!(error.to_string().contains("license checksum mismatch"));
    }

    #[cfg(unix)]
    #[test]
    fn importer_file_admission_rejects_hard_links() {
        let directory = TestDirectory::new("hard-link");
        let source = directory.0.join("source.go");
        let alias = directory.0.join("alias.go");
        fs::write(&source, b"package cmd\n").unwrap();
        fs::hard_link(&source, &alias).unwrap();
        let error = read_regular_file_bounded(&source, 1024).unwrap_err();
        assert!(error.to_string().contains("hard links"));
    }
}
