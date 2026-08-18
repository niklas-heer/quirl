//! Build-time native catalog import, formatting, validation, and publication.

use clap::Subcommand;
use quirl_catalog::{
    NativeArgument, NativeCatalog, NativeCatalogDiagnostic, NativeCatalogLimits, NativeCommand,
    NativeCompletionAction, NativeFlag, NativePlatform, compile_native_catalog,
    parse_native_catalog,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsStr,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const CURATED_DIRECTORY: &str = "catalog/curated";
const DRAFT_DIRECTORY: &str = "catalog/draft";
const GENERATED_DIRECTORY: &str = "catalog/generated";
const IMPORT_CONFIGURATION: &str = "catalog/carapace-import.json";
const DATABASE_FILE: &str = "catalog/generated/catalog.sqlite3";
const CHECKSUM_FILE: &str = "catalog/generated/catalog.sqlite3.sha256";
const DRAFT_FILE: &str = "catalog/draft/carapace.kdl";
const DRAFT_MANIFEST_FILE: &str = "catalog/draft/carapace.import.json";
const PROVENANCE_FILE: &str = "catalog/provenance/carapace.json";
const LICENSE_FILE: &str = "catalog/provenance/CARAPACE_LICENSE";
const SOURCE_FILE_BYTES_MAX: usize = 512 * 1024;
const SOURCE_TOTAL_BYTES_MAX: usize = 4 * 1024 * 1024;
const SOURCE_FILE_COUNT_MAX: usize = 128;
const IMPORT_COMMAND_COUNT_MAX: usize = 2_048;
const IMPORT_FLAG_COUNT_MAX: usize = 16_384;
const IMPORT_OUTPUT_BYTES_MAX: usize = 4 * 1024 * 1024;
const CATALOG_FILE_COUNT_MAX: usize = 128;
const CATALOG_TOTAL_BYTES_MAX: usize = 8 * 1024 * 1024;
const TEMPORARY_ATTEMPTS_MAX: usize = 32;
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

pub(crate) fn run(root: &Path, command: CatalogCommand) -> Result<(), Box<dyn Error>> {
    match command {
        CatalogCommand::ImportCarapace { source, revision } => {
            import_carapace(root, &source, &revision)
        }
        CatalogCommand::Fmt { check } => format_sources(root, check),
        CatalogCommand::Check => check_catalog(root),
        CatalogCommand::Build => build_catalog(root),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportConfiguration {
    source_url: String,
    revision: String,
    license: String,
    author: String,
    roots: Vec<ImportRoot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportRoot {
    variable: String,
    platforms: Vec<String>,
    files: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
struct ImportManifest {
    source_url: String,
    revision: String,
    license: String,
    source_files: Vec<String>,
    command_paths: Vec<String>,
    command_count: usize,
    flag_count: usize,
    semantic_diff: Vec<String>,
}

#[derive(Debug)]
struct ParsedGoCommand {
    variable: String,
    parent: Option<String>,
    command: NativeCommand,
}

fn import_carapace(root: &Path, source: &Path, revision: &str) -> Result<(), Box<dyn Error>> {
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
    let (catalog, source_files) = parse_carapace_checkout(source, &configuration)?;
    reject_curated_collisions(root, &catalog)?;
    let rendered = render_catalog(&catalog)?;
    if rendered.len() > IMPORT_OUTPUT_BYTES_MAX {
        return Err(resource_error(
            "generated draft bytes",
            IMPORT_OUTPUT_BYTES_MAX,
            rendered.len(),
        ));
    }
    parse_native_catalog(&rendered, DRAFT_FILE, NativeCatalogLimits::default())
        .map_err(map_diagnostic)?;

    let draft_path = root.join(DRAFT_FILE);
    let previous = read_optional_catalog(&draft_path)?;
    let semantic_diff = semantic_diff(previous.as_ref(), &catalog);
    let (command_paths, command_count, flag_count) = catalog_statistics(&catalog);
    let manifest = ImportManifest {
        source_url: configuration.source_url,
        revision: configuration.revision,
        license: configuration.license,
        source_files,
        command_paths,
        command_count,
        flag_count,
        semantic_diff,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    atomic_write(&draft_path, rendered.as_bytes())?;
    atomic_write(&root.join(DRAFT_MANIFEST_FILE), &manifest_bytes)?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    Ok(())
}

fn validate_import_configuration(
    configuration: &ImportConfiguration,
) -> Result<(), Box<dyn Error>> {
    if !configuration.source_url.starts_with("https://") {
        return Err(input_error("Carapace source_url must use https://"));
    }
    if configuration.license != "MIT" {
        return Err(input_error(
            "pinned Carapace input must retain its MIT license",
        ));
    }
    if configuration.roots.is_empty() || configuration.roots.len() > SOURCE_FILE_COUNT_MAX {
        return Err(resource_error(
            "import root count",
            SOURCE_FILE_COUNT_MAX,
            configuration.roots.len(),
        ));
    }
    let mut files = BTreeSet::new();
    for root in &configuration.roots {
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

fn parse_carapace_checkout(
    source: &Path,
    configuration: &ImportConfiguration,
) -> Result<(NativeCatalog, Vec<String>), Box<dyn Error>> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_dir() {
        return Err(input_error("Carapace source must be a directory"));
    }
    let mut total_bytes = 0_usize;
    let mut source_files = Vec::new();
    let mut roots = Vec::new();
    let mut command_count = 0_usize;
    let mut flag_count = 0_usize;
    for root in &configuration.roots {
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
    Ok((
        NativeCatalog {
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
    ))
}

fn parse_go_file(source: &str, source_name: &str) -> Result<Vec<ParsedGoCommand>, Box<dyn Error>> {
    if source.len() > SOURCE_FILE_BYTES_MAX {
        return Err(resource_error(
            "Carapace source file bytes",
            SOURCE_FILE_BYTES_MAX,
            source.len(),
        ));
    }
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
        let flags = parse_go_flags(source, &variable)?;
        let arguments = parse_go_arguments(source, &variable);
        let parent = find_parent_variable(source, &variable);
        commands.push(ParsedGoCommand {
            variable,
            parent,
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

fn variable_before_marker(source: &str, marker: usize) -> Result<String, Box<dyn Error>> {
    let line_start = source[..marker].rfind('\n').map_or(0, |index| index + 1);
    let prefix = source[line_start..marker].trim();
    let variable = prefix
        .strip_prefix("var ")
        .map(str::trim)
        .ok_or_else(|| input_error("Cobra command definition must use `var name =`"))?;
    validate_go_identifier(variable)?;
    Ok(variable.to_owned())
}

fn matching_brace(source: &str, open: usize) -> Result<usize, Box<dyn Error>> {
    let bytes = source.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return Err(input_error("Cobra command marker has no opening brace"));
    }
    let mut depth = 0_usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
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

fn keyed_go_string_slice(body: &str, key: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let Some(line) = body.lines().find(|line| line.trim().starts_with(key)) else {
        return Ok(Vec::new());
    };
    let mut values = quoted_go_strings(line)?;
    values.sort();
    values.dedup();
    for value in &values {
        validate_catalog_identifier(value)?;
    }
    Ok(values)
}

fn first_go_string(input: &str) -> Option<Result<(String, usize), Box<dyn Error>>> {
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

fn quoted_go_strings(input: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let mut values = Vec::new();
    let mut rest = input;
    while let Some(result) = first_go_string(rest) {
        let (value, consumed) = result?;
        values.push(value);
        rest = &rest[consumed..];
    }
    Ok(values)
}

fn parse_go_flags(source: &str, variable: &str) -> Result<Vec<NativeFlag>, Box<dyn Error>> {
    let mut actions = BTreeMap::<String, NativeCompletionAction>::new();
    for line in source.lines() {
        let trimmed = line.trim();
        let Some((name, consumed)) = first_go_string(trimmed).transpose()? else {
            continue;
        };
        let action_source = &trimmed[consumed..];
        if let Some(action) = action_from_source(action_source) {
            actions.insert(name, action);
        }
    }
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
        if !method.starts_with("Bool") && !method.starts_with("String") {
            return Err(input_error(format!(
                "unsupported Cobra flag method {method} for {variable}; extend the bounded importer before accepting this upstream shape"
            )));
        }
        let values = quoted_go_strings(&rest[open + 1..])?;
        if values.len() < 2 {
            return Err(input_error(format!(
                "static flag declaration for {variable} is incomplete"
            )));
        }
        let name = values[0].clone();
        validate_long_name(&name)?;
        let has_short = method.ends_with('P') || method.ends_with('S');
        let short = if has_short && values.get(1).is_some_and(|value| !value.is_empty()) {
            let value = format!("-{}", values[1]);
            if value.len() != 2 {
                return Err(input_error(format!("invalid short flag {value}")));
            }
            Some(value)
        } else {
            None
        };
        let summary = values
            .last()
            .cloned()
            .ok_or_else(|| input_error("flag description is missing"))?;
        let is_bool = method.starts_with("Bool");
        let action = actions.get(&name).copied();
        flags.push(NativeFlag {
            long: format!("--{name}"),
            short,
            summary: summary.clone(),
            description: summary,
            value_name: (!is_bool).then(|| "value".to_owned()),
            required: false,
            repeatable: method.contains("Array") || method.contains("Slice"),
            action: (!is_bool).then_some(action).flatten(),
        });
    }
    flags.sort_by(|left, right| left.long.cmp(&right.long));
    let mut names = BTreeSet::new();
    flags.retain(|flag| names.insert(flag.long.clone()));
    Ok(flags)
}

fn parse_go_arguments(source: &str, variable: &str) -> Vec<NativeArgument> {
    let marker = format!("carapace.Gen({variable}).PositionalAnyCompletion(");
    let Some(start) = source.find(&marker) else {
        return Vec::new();
    };
    let tail = &source[start + marker.len()..];
    let action = action_from_source(tail.lines().take(4).collect::<String>().as_str());
    vec![NativeArgument {
        name: "values".to_owned(),
        summary: "Additional values".to_owned(),
        description: "Additional positional values accepted by the command.".to_owned(),
        required: false,
        repeatable: true,
        action,
    }]
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
) -> Result<NativeCommand, Box<dyn Error>> {
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

fn format_sources(root: &Path, check: bool) -> Result<(), Box<dyn Error>> {
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

fn check_catalog(root: &Path) -> Result<(), Box<dyn Error>> {
    let (curated, drafts) = load_and_validate_sources(root, true)?;
    validate_separation(&curated, &drafts)?;
    validate_import_artifacts(root, &drafts)?;
    let bytes =
        compile_native_catalog(&curated, NativeCatalogLimits::default()).map_err(map_diagnostic)?;
    let repeated =
        compile_native_catalog(&curated, NativeCatalogLimits::default()).map_err(map_diagnostic)?;
    if bytes != repeated {
        return Err(input_error(
            "native catalog compiler output is not deterministic",
        ));
    }
    let checksum = checksum_line(&bytes);
    let database_path = root.join(DATABASE_FILE);
    let checksum_path = root.join(CHECKSUM_FILE);
    let observed_database = read_regular_file_bounded(
        &database_path,
        NativeCatalogLimits::default().database_bytes_max,
    )?;
    let observed_checksum = read_regular_file_bounded(&checksum_path, 256)?;
    if observed_database != bytes || observed_checksum != checksum.as_bytes() {
        return Err(input_error(
            "compiled catalog artifacts drifted; run `cargo xtask catalog build`",
        ));
    }
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

fn build_catalog(root: &Path) -> Result<(), Box<dyn Error>> {
    let (curated, drafts) = load_and_validate_sources(root, true)?;
    validate_separation(&curated, &drafts)?;
    validate_import_artifacts(root, &drafts)?;
    let bytes =
        compile_native_catalog(&curated, NativeCatalogLimits::default()).map_err(map_diagnostic)?;
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
) -> Result<(NativeCatalog, Vec<NativeCatalog>), Box<dyn Error>> {
    let _ = catalog_source_paths(root)?;
    let curated_paths = kdl_files(&root.join(CURATED_DIRECTORY))?;
    if curated_paths.len() != 1 {
        return Err(input_error(format!(
            "catalog/curated must contain exactly one KDL source; observed {}",
            curated_paths.len()
        )));
    }
    let curated = load_one_catalog(root, &curated_paths[0], require_canonical)?;
    validate_provenance(&curated, false)?;
    let mut drafts = Vec::new();
    for path in kdl_files(&root.join(DRAFT_DIRECTORY))? {
        let catalog = load_one_catalog(root, &path, require_canonical)?;
        validate_provenance(&catalog, true)?;
        drafts.push(catalog);
    }
    Ok((curated, drafts))
}

fn load_one_catalog(
    root: &Path,
    path: &Path,
    require_canonical: bool,
) -> Result<NativeCatalog, Box<dyn Error>> {
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

fn validate_provenance(catalog: &NativeCatalog, draft: bool) -> Result<(), Box<dyn Error>> {
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
    curated: &NativeCatalog,
    drafts: &[NativeCatalog],
) -> Result<(), Box<dyn Error>> {
    let curated_names = curated
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut draft_names = BTreeSet::new();
    for draft in drafts {
        for command in &draft.commands {
            if curated_names.contains(command.name.as_str()) {
                return Err(input_error(format!(
                    "draft root command `{}` collides with a curated definition",
                    command.name
                )));
            }
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

fn validate_import_artifacts(root: &Path, drafts: &[NativeCatalog]) -> Result<(), Box<dyn Error>> {
    let configuration: ImportConfiguration = read_json_bounded(&root.join(IMPORT_CONFIGURATION))?;
    validate_import_configuration(&configuration)?;
    let draft = drafts
        .iter()
        .find(|catalog| catalog.name == "carapace-draft")
        .ok_or_else(|| input_error("catalog/draft must contain carapace-draft"))?;
    if draft.provenance.author != configuration.author
        || draft.provenance.license != configuration.license
        || draft.provenance.revision != configuration.revision
        || draft.provenance.source_url != configuration.source_url
    {
        return Err(input_error(
            "Carapace draft provenance does not match catalog/carapace-import.json",
        ));
    }
    let manifest: ImportManifest = read_json_bounded(&root.join(DRAFT_MANIFEST_FILE))?;
    let mut configured_files = configuration
        .roots
        .iter()
        .flat_map(|import_root| import_root.files.iter().cloned())
        .collect::<Vec<_>>();
    configured_files.sort();
    let (command_paths, command_count, flag_count) = catalog_statistics(draft);
    if manifest.source_url != configuration.source_url
        || manifest.revision != configuration.revision
        || manifest.license != configuration.license
        || manifest.source_files != configured_files
        || manifest.command_paths != command_paths
        || manifest.command_count != command_count
        || manifest.flag_count != flag_count
        || !manifest.semantic_diff.is_empty()
    {
        return Err(input_error(
            "Carapace import manifest drifted from the pinned configuration or draft; rerun the importer and review its semantic diff",
        ));
    }
    let provenance: serde_json::Value = read_json_bounded(&root.join(PROVENANCE_FILE))?;
    if provenance
        .get("source_url")
        .and_then(serde_json::Value::as_str)
        != Some(configuration.source_url.as_str())
        || provenance
            .get("revision")
            .and_then(serde_json::Value::as_str)
            != Some(configuration.revision.as_str())
        || provenance
            .get("license")
            .and_then(serde_json::Value::as_str)
            != Some(configuration.license.as_str())
    {
        return Err(input_error(
            "catalog/provenance/carapace.json does not match the pinned import configuration",
        ));
    }
    let license = read_regular_file_bounded(&root.join(LICENSE_FILE), 64 * 1024)?;
    if !license.starts_with(b"MIT License\n")
        || !license.windows(14).any(|part| part == b"Copyright (c) ")
    {
        return Err(input_error("retained Carapace MIT license is incomplete"));
    }
    Ok(())
}

fn reject_curated_collisions(root: &Path, imported: &NativeCatalog) -> Result<(), Box<dyn Error>> {
    let curated_paths = kdl_files(&root.join(CURATED_DIRECTORY))?;
    for path in curated_paths {
        let curated = load_one_catalog(root, &path, false)?;
        validate_separation(&curated, std::slice::from_ref(imported))?;
    }
    Ok(())
}

fn render_catalog(catalog: &NativeCatalog) -> Result<String, Box<dyn Error>> {
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
) -> Result<(), Box<dyn Error>> {
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
        render_flag(output, flag, &format!("{indent}    "))?;
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

fn render_flag(output: &mut String, flag: &NativeFlag, indent: &str) -> Result<(), Box<dyn Error>> {
    write!(
        output,
        "{indent}flag {} summary={} description={}",
        quote(&flag.long)?,
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
    output.push('\n');
    Ok(())
}

fn render_argument(
    output: &mut String,
    argument: &NativeArgument,
    indent: &str,
) -> Result<(), Box<dyn Error>> {
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

fn quote(value: &str) -> Result<String, Box<dyn Error>> {
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
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64 + "  catalog.sqlite3\n".len());
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output.push_str("  catalog.sqlite3\n");
    output
}

fn catalog_source_paths(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
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

fn kdl_files(directory: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
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

fn read_catalog_source(path: &Path) -> Result<String, Box<dyn Error>> {
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

fn read_optional_catalog(path: &Path) -> Result<Option<NativeCatalog>, Box<dyn Error>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            let source = read_catalog_source(path)?;
            Ok(Some(
                parse_native_catalog(
                    &source,
                    &path.display().to_string(),
                    NativeCatalogLimits::default(),
                )
                .map_err(map_diagnostic)?,
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_json_bounded<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, Box<dyn Error>> {
    let bytes = read_regular_file_bounded(path, SOURCE_FILE_BYTES_MAX)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn read_regular_file_bounded(path: &Path, bytes_max: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let metadata = fs::symlink_metadata(path)?;
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
    let mut file = File::open(path)?;
    let handle_metadata = file.metadata()?;
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
    if !same_file_metadata(&metadata, &final_metadata) {
        return Err(input_error(format!(
            "input path changed while reading: {}",
            path.display()
        )));
    }
    Ok(bytes)
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

fn checkout_revision(source: &Path) -> Result<String, Box<dyn Error>> {
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

fn validate_revision(revision: &str) -> Result<(), Box<dyn Error>> {
    if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(input_error(
            "Carapace revision must be exactly 40 hexadecimal characters",
        ))
    }
}

fn parse_platforms(values: &[String]) -> Result<Vec<NativePlatform>, Box<dyn Error>> {
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
    platforms.dedup();
    if platforms.contains(&NativePlatform::Any) && platforms.len() != 1 {
        return Err(input_error(
            "platform any cannot be combined with specific platforms",
        ));
    }
    Ok(platforms)
}

fn validate_relative_source_path(value: &str) -> Result<(), Box<dyn Error>> {
    let path = Path::new(value);
    if path.extension() != Some(OsStr::new("go"))
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(input_error(format!(
            "Carapace source path must be a normalized relative .go path: {value}"
        )));
    }
    Ok(())
}

fn validate_no_symlink_components(root: &Path, relative: &str) -> Result<(), Box<dyn Error>> {
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

fn validate_go_identifier(value: &str) -> Result<(), Box<dyn Error>> {
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

fn validate_catalog_identifier(value: &str) -> Result<(), Box<dyn Error>> {
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

fn validate_long_name(value: &str) -> Result<(), Box<dyn Error>> {
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

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
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

fn map_diagnostic(diagnostic: NativeCatalogDiagnostic) -> Box<dyn Error> {
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

fn input_error(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn resource_error(label: &str, limit: usize, observed: usize) -> Box<dyn Error> {
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
                    long: "--path".to_owned(),
                    short: Some("-p".to_owned()),
                    summary: "Select path".to_owned(),
                    description: "Select one filesystem path.".to_owned(),
                    value_name: Some("path".to_owned()),
                    required: false,
                    repeatable: false,
                    action: Some(NativeCompletionAction::Files),
                }],
                arguments: Vec::new(),
                subcommands: Vec::new(),
            }],
        }
    }

    fn prepare_import_workspace() -> (TestDirectory, PathBuf, Vec<u8>) {
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
        let configuration = serde_json::json!({
            "source_url": "https://github.com/carapace-sh/carapace-bin",
            "revision": REVISION,
            "license": "MIT",
            "author": "Carapace contributors",
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

        let source = directory.0.join("carapace");
        let source_file = source.join("completers/common/fd_completer/cmd/root.go");
        fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        fs::copy(fixture.join("fd-root.go"), source_file).unwrap();
        fs::create_dir(source.join(".git")).unwrap();
        fs::write(source.join(".git/HEAD"), format!("{REVISION}\n")).unwrap();
        (directory, source, curated_before)
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
        let source = r#"
var rootCmd = &cobra.Command{
    Use: "tool",
    Short: "Tool",
}
func init() {
    rootCmd.Flags().Uint("count", 0, "Count")
}
"#;
        let error = parse_go_file(source, "unsupported.go").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported Cobra flag method Uint")
        );
    }

    #[test]
    fn imported_draft_cannot_collide_with_curated_roots() {
        let curated = fixture_catalog();
        let mut draft = fixture_catalog();
        draft.name = "fixture-draft".to_owned();
        let error = validate_separation(&curated, &[draft]).unwrap_err();
        assert!(error.to_string().contains("collides with a curated"));
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
    fn importer_e2e_is_deterministic_and_preserves_curated_source() {
        let (directory, source, curated_before) = prepare_import_workspace();
        import_carapace(&directory.0, &source, REVISION).unwrap();
        import_carapace(&directory.0, &source, REVISION).unwrap();
        let draft_second = fs::read(directory.0.join(DRAFT_FILE)).unwrap();
        let manifest_second = fs::read(directory.0.join(DRAFT_MANIFEST_FILE)).unwrap();
        import_carapace(&directory.0, &source, REVISION).unwrap();
        assert_eq!(
            fs::read(directory.0.join(DRAFT_FILE)).unwrap(),
            draft_second
        );
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
    }

    #[test]
    fn importer_rejects_a_revision_other_than_the_explicit_pin() {
        let (directory, source, curated_before) = prepare_import_workspace();
        let wrong_revision = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let error = import_carapace(&directory.0, &source, wrong_revision).unwrap_err();
        assert!(error.to_string().contains("does not match pinned revision"));
        assert!(!directory.0.join(DRAFT_FILE).exists());
        assert_eq!(
            fs::read(directory.0.join("catalog/curated/native.kdl")).unwrap(),
            curated_before
        );
    }
}
