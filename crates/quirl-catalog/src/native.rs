//! Strict native command specifications and their bounded SQLite projection.

#![allow(
    clippy::result_large_err,
    reason = "the public inert diagnostic intentionally retains source identity, spans, help, and bounded context inline"
)]

use crate::{
    ArgumentKind, ArgumentSpec, CommandSpec, CompletionSource, Confidence, Effect, IoContract,
    Provenance, ProvenanceInfo, Trust,
};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use rusqlite::{Connection, MAIN_DB, Transaction, limits::Limit, params, serialize::Data};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// SQLite application identity for native Quirl command catalogs (`QCNC`).
pub const NATIVE_DATABASE_APPLICATION_ID: i64 = 0x5143_4e43;
/// Current SQLite schema version for native Quirl command catalogs.
pub const NATIVE_DATABASE_SCHEMA_VERSION: i64 = 3;

const SOURCE_BYTES_HARD_MAX: usize = 4 * 1024 * 1024;
const DATABASE_BYTES_HARD_MAX: usize = 128 * 1024 * 1024;
const COMMANDS_HARD_MAX: usize = 65_536;
const COMMAND_DEPTH_HARD_MAX: usize = 32;
const FLAGS_HARD_MAX: usize = 131_072;
const ARGUMENTS_HARD_MAX: usize = 131_072;
const VALUES_PER_COMMAND_HARD_MAX: usize = 1_024;
const STRING_BYTES_HARD_MAX: usize = 64 * 1024;
const DOCUMENTS_HARD_MAX: usize = 196_608;
const QUERY_BYTES_HARD_MAX: usize = 16 * 1024;
const RESULTS_HARD_MAX: usize = 1_024;
const TEMPORARY_ATTEMPTS_MAX: usize = 64;
const SQLITE_LENGTH_HARD_MAX: i32 = 4 * 1024 * 1024;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

const SCHEMA: &str = r#"
PRAGMA page_size = 4096;
PRAGMA journal_mode = OFF;
PRAGMA synchronous = OFF;
PRAGMA auto_vacuum = NONE;
CREATE TABLE catalog_snapshot (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    snapshot_json BLOB NOT NULL
);
CREATE TABLE provenance (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    catalog_name TEXT NOT NULL,
    author TEXT NOT NULL,
    license TEXT NOT NULL,
    revision TEXT NOT NULL,
    source_url TEXT NOT NULL
);
CREATE TABLE commands (
    command_id INTEGER PRIMARY KEY NOT NULL,
    parent_id INTEGER,
    name TEXT NOT NULL,
    full_path TEXT NOT NULL UNIQUE,
    depth INTEGER NOT NULL,
    summary TEXT NOT NULL,
    description TEXT NOT NULL
);
CREATE INDEX commands_parent_name ON commands(parent_id, name, command_id);
CREATE TABLE command_aliases (
    command_id INTEGER NOT NULL,
    alias TEXT NOT NULL,
    PRIMARY KEY (command_id, alias)
) WITHOUT ROWID;
CREATE TABLE command_platforms (
    command_id INTEGER NOT NULL,
    platform TEXT NOT NULL,
    PRIMARY KEY (command_id, platform)
) WITHOUT ROWID;
CREATE TABLE command_intents (
    command_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    phrase TEXT NOT NULL,
    PRIMARY KEY (command_id, ordinal)
) WITHOUT ROWID;
CREATE TABLE flags (
    flag_id INTEGER PRIMARY KEY NOT NULL,
    command_id INTEGER NOT NULL,
    name TEXT NOT NULL,
    short_name TEXT,
    summary TEXT NOT NULL,
    description TEXT NOT NULL,
    value_name TEXT,
    required INTEGER NOT NULL CHECK (required IN (0, 1)),
    repeatable INTEGER NOT NULL CHECK (repeatable IN (0, 1)),
    action TEXT
);
CREATE INDEX flags_command_name ON flags(command_id, name, flag_id);
CREATE TABLE flag_platforms (
    flag_id INTEGER NOT NULL,
    platform TEXT NOT NULL,
    PRIMARY KEY (flag_id, platform)
) WITHOUT ROWID;
CREATE TABLE arguments (
    argument_id INTEGER PRIMARY KEY NOT NULL,
    command_id INTEGER NOT NULL,
    ordinal INTEGER NOT NULL,
    name TEXT NOT NULL,
    summary TEXT NOT NULL,
    description TEXT NOT NULL,
    required INTEGER NOT NULL CHECK (required IN (0, 1)),
    repeatable INTEGER NOT NULL CHECK (repeatable IN (0, 1)),
    action TEXT,
    UNIQUE (command_id, ordinal),
    UNIQUE (command_id, name)
);
CREATE TABLE semantic_documents (
    document_id INTEGER PRIMARY KEY NOT NULL,
    document_kind TEXT NOT NULL,
    command_id INTEGER NOT NULL,
    target TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL
);
CREATE INDEX semantic_documents_command ON semantic_documents(command_id, document_id);
CREATE TABLE semantic_document_platforms (
    document_id INTEGER NOT NULL,
    platform TEXT NOT NULL,
    PRIMARY KEY (document_id, platform)
) WITHOUT ROWID;
"#;

/// Resource ceilings applied while parsing, compiling, publishing, and querying.
///
/// Callers may lower these defaults. Values above the crate's hard safety ceilings
/// are rejected instead of silently widening the untrusted-input boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeCatalogLimits {
    /// Maximum KDL source bytes admitted before parsing.
    pub source_bytes_max: usize,
    /// Maximum serialized SQLite image bytes.
    pub database_bytes_max: usize,
    /// Maximum commands across the complete tree.
    pub command_count_max: usize,
    /// Maximum root-inclusive command nesting depth.
    pub command_depth_max: usize,
    /// Maximum flags across all commands.
    pub flag_count_max: usize,
    /// Maximum positional arguments across all commands.
    pub argument_count_max: usize,
    /// Maximum aliases, intents, platforms, or local parameters on one command.
    pub values_per_command_max: usize,
    /// Maximum UTF-8 bytes retained in any single string field.
    pub string_bytes_max: usize,
    /// Maximum semantic documents retained or scanned.
    pub semantic_document_count_max: usize,
    /// Maximum UTF-8 bytes in a reader query.
    pub query_bytes_max: usize,
    /// Maximum rows returned by one reader query.
    pub query_results_max: usize,
}

impl Default for NativeCatalogLimits {
    fn default() -> Self {
        Self {
            source_bytes_max: 1024 * 1024,
            database_bytes_max: 128 * 1024 * 1024,
            command_count_max: 8_192,
            command_depth_max: 16,
            flag_count_max: 32_768,
            argument_count_max: 32_768,
            values_per_command_max: 256,
            string_bytes_max: 16 * 1024,
            semantic_document_count_max: 65_536,
            query_bytes_max: 4 * 1024,
            query_results_max: 256,
        }
    }
}

impl NativeCatalogLimits {
    /// Return the fixed limits used for Quirl's checked-in embedded catalog.
    ///
    /// Build tooling and runtime admission must use this same profile so a
    /// checked artifact cannot pass CI and then exceed the executable's bounds.
    pub const fn embedded() -> Self {
        Self {
            source_bytes_max: 1024 * 1024,
            database_bytes_max: 2 * 1024 * 1024,
            command_count_max: 2_048,
            command_depth_max: 16,
            flag_count_max: 8_192,
            argument_count_max: 8_192,
            values_per_command_max: 256,
            string_bytes_max: 16 * 1024,
            semantic_document_count_max: 24_576,
            query_bytes_max: 4 * 1024,
            query_results_max: 256,
        }
    }
}

/// Classification of an inert native-catalog diagnostic.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeDiagnosticKind {
    /// KDL grammar could not be parsed.
    Syntax,
    /// Parsed input or a database violated the closed contract.
    Validation,
    /// An explicit resource ceiling was exceeded.
    ResourceLimit,
    /// A filesystem operation failed.
    Io,
    /// SQLite construction or querying failed.
    Database,
}

/// Source-aware, effect-free diagnostic owned by the foundation catalog crate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeCatalogDiagnostic {
    /// Broad failure class for later mapping at an effect-owning boundary.
    pub kind: NativeDiagnosticKind,
    /// File path or logical source identity supplied by the caller.
    pub source_name: String,
    /// Human-readable failure description.
    pub message: String,
    /// UTF-8 byte offset in KDL source when the failure has a source location.
    pub byte_offset: Option<usize>,
    /// UTF-8 byte length of the associated source range.
    pub byte_length: Option<usize>,
    /// Actionable correction guidance.
    pub help: String,
    /// Bounded contextual facts such as configured and observed counts.
    pub context: Vec<String>,
}

impl fmt::Display for NativeCatalogDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.source_name, self.message)
    }
}

impl std::error::Error for NativeCatalogDiagnostic {}

/// Platform selector applied to commands, flags, and every child projection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum NativePlatform {
    /// Every supported platform.
    Any,
    /// Linux hosts.
    Linux,
    /// macOS hosts.
    Macos,
    /// Windows hosts.
    Windows,
    /// FreeBSD hosts.
    Freebsd,
}

impl NativePlatform {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "any" => Some(Self::Any),
            "linux" => Some(Self::Linux),
            "macos" => Some(Self::Macos),
            "windows" => Some(Self::Windows),
            "freebsd" => Some(Self::Freebsd),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
            Self::Freebsd => "freebsd",
        }
    }
}

/// Closed native actions a runtime may use to produce completion candidates.
///
/// The catalog stores declarations only; compiling or querying never executes an
/// action. Consumers remain responsible for their own bounded implementation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum NativeCompletionAction {
    /// Regular files and directories.
    Files,
    /// Directories only.
    Directories,
    /// Executables visible to the shell.
    Executables,
    /// Local user names.
    Users,
    /// Local group names.
    Groups,
    /// Host names from bounded native configuration.
    Hostnames,
    /// Environment-variable names.
    EnvironmentVariables,
}

impl NativeCompletionAction {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "files" => Some(Self::Files),
            "directories" => Some(Self::Directories),
            "executables" => Some(Self::Executables),
            "users" => Some(Self::Users),
            "groups" => Some(Self::Groups),
            "hostnames" => Some(Self::Hostnames),
            "environment_variables" => Some(Self::EnvironmentVariables),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Directories => "directories",
            Self::Executables => "executables",
            Self::Users => "users",
            Self::Groups => "groups",
            Self::Hostnames => "hostnames",
            Self::EnvironmentVariables => "environment_variables",
        }
    }

    fn provider_identity(self) -> &'static str {
        match self {
            Self::Files => "quirl.native.files",
            Self::Directories => "quirl.native.directories",
            Self::Executables => "quirl.native.executables",
            Self::Users => "quirl.native.users",
            Self::Groups => "quirl.native.groups",
            Self::Hostnames => "quirl.native.hostnames",
            Self::EnvironmentVariables => "quirl.native.environment_variables",
        }
    }

    fn value_type(self) -> &'static str {
        match self {
            Self::Files => "Path",
            Self::Directories => "Directory",
            Self::Executables => "Executable",
            Self::Users => "User",
            Self::Groups => "Group",
            Self::Hostnames => "Hostname",
            Self::EnvironmentVariables => "EnvironmentVariable",
        }
    }
}

/// Complete attribution required for a human-authored native catalog.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeProvenance {
    /// Person, project, or organization responsible for the source facts.
    pub author: String,
    /// SPDX license expression or explicit license name for the source facts.
    pub license: String,
    /// Immutable upstream revision, release, or content identity.
    pub revision: String,
    /// HTTPS or HTTP URL identifying the source material.
    pub source_url: String,
}

/// One named positional argument in a native command specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeArgument {
    /// Stable local identifier used in usage and semantic documents.
    pub name: String,
    /// Short completion-facing description.
    pub summary: String,
    /// Longer behavioral explanation.
    pub description: String,
    /// Whether omission is invalid.
    pub required: bool,
    /// Whether the argument consumes more than one value.
    pub repeatable: bool,
    /// Optional declarative native completion action.
    pub action: Option<NativeCompletionAction>,
}

/// One flag or valued option in a native command specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeFlag {
    /// Canonical spelling beginning with `--`, a short-only `-x`, or a Windows `/x`.
    pub name: String,
    /// Optional single-character short alias for a canonical long spelling.
    pub short: Option<String>,
    /// Short completion-facing description.
    pub summary: String,
    /// Longer behavioral explanation.
    pub description: String,
    /// Placeholder for a consumed value; absence means a boolean flag.
    pub value_name: Option<String>,
    /// Whether the option must be present.
    pub required: bool,
    /// Whether the option may occur more than once.
    pub repeatable: bool,
    /// Optional declarative native completion action for the consumed value.
    pub action: Option<NativeCompletionAction>,
    /// Effective platforms on which this flag spelling and behavior apply.
    pub platforms: Vec<NativePlatform>,
}

/// One command node with depth-bounded child commands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeCommand {
    /// One invocation token; parents provide the rest of the command path.
    pub name: String,
    /// Alternative invocation tokens in the same parent scope.
    pub aliases: Vec<String>,
    /// Short list- and completion-facing text.
    pub summary: String,
    /// Full behavioral description.
    pub description: String,
    /// Task-language phrases included in semantic lookup.
    pub intents: Vec<String>,
    /// Effective platforms on which this command and its metadata apply.
    pub platforms: Vec<NativePlatform>,
    /// Named options accepted by this command.
    pub flags: Vec<NativeFlag>,
    /// Ordered positional arguments accepted by this command.
    pub arguments: Vec<NativeArgument>,
    /// Child commands, recursively bounded by [`NativeCatalogLimits`].
    pub subcommands: Vec<NativeCommand>,
}

/// Typed snapshot compiled from one strict, human-authored KDL document.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeCatalog {
    /// Stable human-readable identity for this catalog source.
    pub name: String,
    /// Complete source attribution.
    pub provenance: NativeProvenance,
    /// Root commands in canonical lexical order.
    pub commands: Vec<NativeCommand>,
}

/// Kind of one structured completion projection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeCompletionKind {
    /// A child command token.
    Subcommand,
    /// A named option.
    Flag,
    /// An ordered positional argument placeholder.
    Argument,
}

/// Bounded structured candidate returned by [`NativeCatalogReader`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeCompletionCandidate {
    /// Projection kind.
    pub kind: NativeCompletionKind,
    /// Insertion text or positional placeholder.
    pub value: String,
    /// Short completion-facing explanation.
    pub summary: String,
    /// Longer behavioral explanation.
    pub description: String,
    /// Optional action for completing a consumed value.
    pub action: Option<NativeCompletionAction>,
}

/// One deterministic lexical semantic-search result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeSemanticHit {
    /// Owning command path.
    pub command_path: String,
    /// Command, flag, or argument target represented by the document.
    pub target: String,
    /// Human-readable document title.
    pub title: String,
    /// Deterministic lexical relevance score.
    pub score: u32,
}

/// Parse and validate one strict KDL native command catalog.
///
/// The document has exactly one `catalog "name"` root. Its children are one
/// required `provenance` node and one or more recursive `command` nodes:
///
/// ```text
/// catalog "tools" {
///   provenance author="Project" license="MIT" revision="v1" source="https://example.invalid/tools"
///   command "copy" summary="Copy files" description="Copy one path to another." {
///     alias "cp"
///     intent "duplicate a file"
///     platform "linux"
///     flag "--recursive" short="-r" summary="Recurse" description="Copy directory trees."
///     argument "source" summary="Input path" description="Path to read." required=#true action="files"
///     command "status" summary="Show status" description="Report bounded copy state."
///   }
/// }
/// ```
///
/// Commands accept only `alias`, `intent`, `platform`, `flag`, `argument`, and
/// nested `command` children. Flags accept `short`, `summary`, `description`,
/// `value`, `required`, `repeatable`, and `action` properties; arguments accept
/// the same properties except `short` and `value`. Every undeclared node,
/// property, positional value, duplicate, or type is rejected.
pub fn parse_native_catalog(
    source: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<NativeCatalog, NativeCatalogDiagnostic> {
    validate_limits(limits)?;
    if source.len() > limits.source_bytes_max {
        return Err(resource_diagnostic(
            source_name,
            "KDL source bytes",
            limits.source_bytes_max,
            source.len(),
        ));
    }
    if source_name.len() > limits.string_bytes_max {
        return Err(resource_diagnostic(
            "<source identity>",
            "source identity bytes",
            limits.string_bytes_max,
            source_name.len(),
        ));
    }
    let document = source.parse::<KdlDocument>().map_err(|error| {
        let first = error.diagnostics.first();
        NativeCatalogDiagnostic {
            kind: NativeDiagnosticKind::Syntax,
            source_name: source_name.to_owned(),
            message: first
                .and_then(|diagnostic| diagnostic.message.clone())
                .unwrap_or_else(|| "could not parse KDL document".to_owned()),
            byte_offset: first.map(|diagnostic| diagnostic.span.offset()),
            byte_length: first.map(|diagnostic| diagnostic.span.len()),
            help: first
                .and_then(|diagnostic| diagnostic.help.clone())
                .unwrap_or_else(|| "Correct the KDL syntax and retry".to_owned()),
            context: Vec::new(),
        }
    })?;
    parse_document(&document, source_name, limits)
}

#[derive(Default)]
struct ParseCounts {
    commands: usize,
    flags: usize,
    arguments: usize,
    documents: usize,
}

fn parse_document(
    document: &KdlDocument,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<NativeCatalog, NativeCatalogDiagnostic> {
    if document.nodes().len() != 1 || document.nodes()[0].name().value() != "catalog" {
        return Err(validation_diagnostic(
            source_name,
            None,
            "the document must contain exactly one `catalog` root node",
            "Wrap one provenance node and one or more command nodes in `catalog \"name\" { ... }`",
        ));
    }
    let root = &document.nodes()[0];
    validate_entries(root, 1, &[], source_name)?;
    let name = required_argument_string(root, 0, "catalog name", source_name, limits)?;
    validate_identifier(&name, "catalog name", root, source_name)?;
    let children = required_children(root, source_name)?;
    preflight_command_depth(children, source_name, limits)?;

    let provenance_nodes = children
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "provenance")
        .collect::<Vec<_>>();
    if provenance_nodes.len() != 1 {
        return Err(node_diagnostic(
            root,
            source_name,
            format!(
                "catalog requires exactly one `provenance` node; observed {}",
                provenance_nodes.len()
            ),
            "Add one provenance node with author, license, revision, and source properties",
        ));
    }
    for node in children.nodes() {
        if !matches!(node.name().value(), "provenance" | "command") {
            return Err(node_diagnostic(
                node,
                source_name,
                format!("unknown catalog child node `{}`", node.name().value()),
                "Use only provenance and command nodes at catalog scope",
            ));
        }
    }
    let provenance = parse_provenance(provenance_nodes[0], source_name, limits)?;
    let command_nodes = children
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "command")
        .collect::<Vec<_>>();
    if command_nodes.is_empty() {
        return Err(node_diagnostic(
            root,
            source_name,
            "catalog must contain at least one command",
            "Add a command node after provenance",
        ));
    }
    let mut counts = ParseCounts::default();
    let mut commands = Vec::with_capacity(command_nodes.len());
    for node in command_nodes {
        commands.push(parse_command(
            node,
            &[NativePlatform::Any],
            1,
            source_name,
            limits,
            &mut counts,
        )?);
    }
    validate_sibling_names(&commands, "<root>", root, source_name)?;
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    validate_unique_paths(&commands, source_name)?;
    Ok(NativeCatalog {
        name,
        provenance,
        commands,
    })
}

fn preflight_command_depth(
    document: &KdlDocument,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut stack = document
        .nodes()
        .iter()
        .filter(|node| node.name().value() == "command")
        .map(|node| (node, 1_usize))
        .collect::<Vec<_>>();
    let mut observed = 0_usize;
    while let Some((node, depth)) = stack.pop() {
        observed = observed.saturating_add(1);
        if observed > limits.command_count_max {
            return Err(resource_diagnostic(
                source_name,
                "command count",
                limits.command_count_max,
                observed,
            ));
        }
        if depth > limits.command_depth_max {
            return Err(node_resource_diagnostic(
                node,
                source_name,
                "command depth",
                limits.command_depth_max,
                depth,
            ));
        }
        if let Some(children) = node.children() {
            for child in children.nodes() {
                if child.name().value() == "command" {
                    stack.push((child, depth.saturating_add(1)));
                }
            }
        }
    }
    Ok(())
}

fn parse_provenance(
    node: &KdlNode,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<NativeProvenance, NativeCatalogDiagnostic> {
    const PROPERTIES: &[&str] = &["author", "license", "revision", "source"];
    validate_entries(node, 0, PROPERTIES, source_name)?;
    reject_children(node, source_name)?;
    let author = required_property_string(node, "author", source_name, limits)?;
    let license = required_property_string(node, "license", source_name, limits)?;
    let revision = required_property_string(node, "revision", source_name, limits)?;
    let source_url = required_property_string(node, "source", source_name, limits)?;
    if !valid_source_url(&source_url) {
        return Err(node_diagnostic(
            node,
            source_name,
            "provenance source must be an absolute HTTP(S) URL without whitespace",
            "Set source to an https:// or http:// URL identifying the authoritative source",
        ));
    }
    Ok(NativeProvenance {
        author,
        license,
        revision,
        source_url,
    })
}

// Command parsing recurses only after an explicit stack has proven the tree is
// within the compile-time hard ceiling of 32 levels.
fn parse_command(
    node: &KdlNode,
    inherited_platforms: &[NativePlatform],
    depth: usize,
    source_name: &str,
    limits: NativeCatalogLimits,
    counts: &mut ParseCounts,
) -> Result<NativeCommand, NativeCatalogDiagnostic> {
    const PROPERTIES: &[&str] = &["description", "summary"];
    validate_entries(node, 1, PROPERTIES, source_name)?;
    counts.commands = counts.commands.saturating_add(1);
    if counts.commands > limits.command_count_max {
        return Err(node_resource_diagnostic(
            node,
            source_name,
            "command count",
            limits.command_count_max,
            counts.commands,
        ));
    }
    if depth > limits.command_depth_max {
        return Err(node_resource_diagnostic(
            node,
            source_name,
            "command depth",
            limits.command_depth_max,
            depth,
        ));
    }
    let name = required_argument_string(node, 0, "command name", source_name, limits)?;
    validate_identifier(&name, "command name", node, source_name)?;
    let summary = required_property_string(node, "summary", source_name, limits)?;
    let description = required_property_string(node, "description", source_name, limits)?;
    let children = node.children();

    let mut aliases = Vec::new();
    let mut intents = Vec::new();
    let mut declared_platforms = Vec::new();
    let mut flag_nodes = Vec::new();
    let mut arguments = Vec::new();
    let mut command_nodes = Vec::new();
    if let Some(children) = children {
        for child in children.nodes() {
            match child.name().value() {
                "alias" => aliases.push(parse_scalar_node(child, "alias", source_name, limits)?),
                "intent" => intents.push(parse_scalar_node(
                    child,
                    "intent phrase",
                    source_name,
                    limits,
                )?),
                "platform" => {
                    let value = parse_scalar_node(child, "platform", source_name, limits)?;
                    declared_platforms.push(NativePlatform::parse(&value).ok_or_else(|| {
                        node_diagnostic(
                            child,
                            source_name,
                            format!("unknown platform `{value}`"),
                            "Use any, linux, macos, windows, or freebsd",
                        )
                    })?);
                }
                "flag" => {
                    counts.flags = counts.flags.saturating_add(1);
                    if counts.flags > limits.flag_count_max {
                        return Err(node_resource_diagnostic(
                            child,
                            source_name,
                            "flag count",
                            limits.flag_count_max,
                            counts.flags,
                        ));
                    }
                    flag_nodes.push(child);
                }
                "argument" => {
                    counts.arguments = counts.arguments.saturating_add(1);
                    if counts.arguments > limits.argument_count_max {
                        return Err(node_resource_diagnostic(
                            child,
                            source_name,
                            "argument count",
                            limits.argument_count_max,
                            counts.arguments,
                        ));
                    }
                    arguments.push(parse_argument(child, source_name, limits)?);
                }
                "command" => command_nodes.push(child),
                other => {
                    return Err(node_diagnostic(
                        child,
                        source_name,
                        format!("unknown command child node `{other}`"),
                        "Use only alias, intent, platform, flag, argument, or command nodes",
                    ));
                }
            }
        }
    }
    for (label, observed) in [
        ("aliases", aliases.len()),
        ("intent phrases", intents.len()),
        ("platforms", declared_platforms.len()),
        ("flags", flag_nodes.len()),
        ("arguments", arguments.len()),
        ("subcommands", command_nodes.len()),
    ] {
        if observed > limits.values_per_command_max {
            return Err(node_resource_diagnostic(
                node,
                source_name,
                label,
                limits.values_per_command_max,
                observed,
            ));
        }
    }
    validate_unique_strings(&mut aliases, "alias", node, source_name)?;
    for alias in &aliases {
        validate_identifier(alias, "alias", node, source_name)?;
    }
    validate_unique_strings(&mut intents, "intent phrase", node, source_name)?;
    let platforms = effective_platforms(
        inherited_platforms,
        &mut declared_platforms,
        node,
        source_name,
    )?;
    let mut flags = Vec::with_capacity(flag_nodes.len());
    for flag_node in flag_nodes {
        flags.push(parse_flag(flag_node, &platforms, source_name, limits)?);
    }
    validate_flag_set(&mut flags, node, source_name)?;
    validate_argument_set(&arguments, node, source_name)?;
    counts.documents = counts
        .documents
        .saturating_add(1)
        .saturating_add(flags.len())
        .saturating_add(arguments.len());
    if counts.documents > limits.semantic_document_count_max {
        return Err(node_resource_diagnostic(
            node,
            source_name,
            "semantic document count",
            limits.semantic_document_count_max,
            counts.documents,
        ));
    }

    let mut subcommands = Vec::with_capacity(command_nodes.len());
    for child in command_nodes {
        subcommands.push(parse_command(
            child,
            &platforms,
            depth.saturating_add(1),
            source_name,
            limits,
            counts,
        )?);
    }
    validate_sibling_names(&subcommands, &name, node, source_name)?;
    subcommands.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(NativeCommand {
        name,
        aliases,
        summary,
        description,
        intents,
        platforms,
        flags,
        arguments,
        subcommands,
    })
}

fn parse_flag(
    node: &KdlNode,
    inherited_platforms: &[NativePlatform],
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<NativeFlag, NativeCatalogDiagnostic> {
    const PROPERTIES: &[&str] = &[
        "action",
        "description",
        "repeatable",
        "required",
        "short",
        "summary",
        "value",
    ];
    validate_entries(node, 1, PROPERTIES, source_name)?;
    let mut declared_platforms = Vec::new();
    if let Some(children) = node.children() {
        for child in children.nodes() {
            if child.name().value() != "platform" {
                return Err(node_diagnostic(
                    child,
                    source_name,
                    format!("unknown flag child node `{}`", child.name().value()),
                    "Use only platform child nodes on flags",
                ));
            }
            let value = parse_scalar_node(child, "platform", source_name, limits)?;
            declared_platforms.push(NativePlatform::parse(&value).ok_or_else(|| {
                node_diagnostic(
                    child,
                    source_name,
                    format!("unknown platform `{value}`"),
                    "Use any, linux, macos, windows, or freebsd",
                )
            })?);
        }
    }
    if declared_platforms.len() > limits.values_per_command_max {
        return Err(node_resource_diagnostic(
            node,
            source_name,
            "flag platforms",
            limits.values_per_command_max,
            declared_platforms.len(),
        ));
    }
    let platforms = effective_platforms(
        inherited_platforms,
        &mut declared_platforms,
        node,
        source_name,
    )?;
    let name = required_argument_string(node, 0, "flag name", source_name, limits)?;
    if !valid_flag_name(&name) {
        return Err(node_diagnostic(
            node,
            source_name,
            format!("invalid flag `{name}`"),
            "Use a lowercase long name such as --output-file, a short-only name such as -P, or a Windows name such as /q",
        ));
    }
    let short = optional_property_string(node, "short", source_name, limits)?;
    if short
        .as_deref()
        .is_some_and(|value| !valid_short_flag(value))
    {
        return Err(node_diagnostic(
            node,
            source_name,
            "short flag must be `-` followed by one ASCII letter or digit",
            "Use a short name such as -o or remove the short property",
        ));
    }
    if short.is_some() && valid_short_flag(&name) {
        return Err(node_diagnostic(
            node,
            source_name,
            "a short-only flag cannot declare another short alias",
            "Remove the short property or use the long spelling as the canonical flag name",
        ));
    }
    let summary = required_property_string(node, "summary", source_name, limits)?;
    let description = required_property_string(node, "description", source_name, limits)?;
    let value_name = optional_property_string(node, "value", source_name, limits)?;
    if let Some(value) = &value_name {
        validate_identifier(value, "flag value placeholder", node, source_name)?;
    }
    let required = optional_property_bool(node, "required", source_name)?.unwrap_or(false);
    let repeatable = optional_property_bool(node, "repeatable", source_name)?.unwrap_or(false);
    let action = optional_action(node, source_name, limits)?;
    if action.is_some() && value_name.is_none() {
        return Err(node_diagnostic(
            node,
            source_name,
            "a boolean flag cannot declare a completion action",
            "Add a value placeholder or remove the action property",
        ));
    }
    Ok(NativeFlag {
        name,
        short,
        summary,
        description,
        value_name,
        required,
        repeatable,
        action,
        platforms,
    })
}

fn parse_argument(
    node: &KdlNode,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<NativeArgument, NativeCatalogDiagnostic> {
    const PROPERTIES: &[&str] = &["action", "description", "repeatable", "required", "summary"];
    validate_entries(node, 1, PROPERTIES, source_name)?;
    reject_children(node, source_name)?;
    let name = required_argument_string(node, 0, "argument name", source_name, limits)?;
    validate_identifier(&name, "argument name", node, source_name)?;
    Ok(NativeArgument {
        name,
        summary: required_property_string(node, "summary", source_name, limits)?,
        description: required_property_string(node, "description", source_name, limits)?,
        required: optional_property_bool(node, "required", source_name)?.unwrap_or(false),
        repeatable: optional_property_bool(node, "repeatable", source_name)?.unwrap_or(false),
        action: optional_action(node, source_name, limits)?,
    })
}

fn parse_scalar_node(
    node: &KdlNode,
    label: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<String, NativeCatalogDiagnostic> {
    validate_entries(node, 1, &[], source_name)?;
    reject_children(node, source_name)?;
    required_argument_string(node, 0, label, source_name, limits)
}

fn optional_action(
    node: &KdlNode,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<Option<NativeCompletionAction>, NativeCatalogDiagnostic> {
    let value = optional_property_string(node, "action", source_name, limits)?;
    value
        .map(|value| {
            NativeCompletionAction::parse(&value).ok_or_else(|| {
                node_diagnostic(
                    node,
                    source_name,
                    format!("unknown native completion action `{value}`"),
                    "Use files, directories, executables, users, groups, hostnames, or environment_variables",
                )
            })
        })
        .transpose()
}

fn validate_entries(
    node: &KdlNode,
    positional_expected: usize,
    allowed_properties: &[&str],
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    if node.ty().is_some() {
        return Err(node_diagnostic(
            node,
            source_name,
            "typed KDL nodes are not part of the native catalog schema",
            "Remove the node type annotation",
        ));
    }
    let mut positional = 0_usize;
    let mut properties = BTreeSet::new();
    for entry in node.entries() {
        if entry.ty().is_some() {
            return Err(entry_diagnostic(
                entry,
                source_name,
                "typed KDL values are not part of the native catalog schema",
                "Remove the value type annotation",
            ));
        }
        if let Some(name) = entry.name() {
            let name = name.value();
            if !allowed_properties.contains(&name) {
                return Err(entry_diagnostic(
                    entry,
                    source_name,
                    format!("unknown property `{name}` on `{}`", node.name().value()),
                    "Remove the property or use a documented property for this node",
                ));
            }
            if !properties.insert(name) {
                return Err(entry_diagnostic(
                    entry,
                    source_name,
                    format!("duplicate property `{name}` on `{}`", node.name().value()),
                    "Keep exactly one value for each property",
                ));
            }
        } else {
            positional = positional.saturating_add(1);
        }
    }
    if positional != positional_expected {
        return Err(node_diagnostic(
            node,
            source_name,
            format!(
                "`{}` expects {positional_expected} positional argument(s); observed {positional}",
                node.name().value()
            ),
            "Remove extra arguments or add the documented string argument",
        ));
    }
    Ok(())
}

fn required_argument_string(
    node: &KdlNode,
    index: usize,
    label: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<String, NativeCatalogDiagnostic> {
    let entry = node.entry(index).ok_or_else(|| {
        node_diagnostic(
            node,
            source_name,
            format!("missing {label}"),
            "Provide the required string argument",
        )
    })?;
    checked_string(entry, label, source_name, limits)
}

fn required_property_string(
    node: &KdlNode,
    property: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<String, NativeCatalogDiagnostic> {
    let entry = node.entry(property).ok_or_else(|| {
        node_diagnostic(
            node,
            source_name,
            format!("`{}` requires property `{property}`", node.name().value()),
            "Add the required non-empty string property",
        )
    })?;
    checked_string(entry, property, source_name, limits)
}

fn optional_property_string(
    node: &KdlNode,
    property: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<Option<String>, NativeCatalogDiagnostic> {
    node.entry(property)
        .map(|entry| checked_string(entry, property, source_name, limits))
        .transpose()
}

fn optional_property_bool(
    node: &KdlNode,
    property: &str,
    source_name: &str,
) -> Result<Option<bool>, NativeCatalogDiagnostic> {
    node.entry(property)
        .map(|entry| {
            entry.value().as_bool().ok_or_else(|| {
                entry_diagnostic(
                    entry,
                    source_name,
                    format!("property `{property}` must be a KDL boolean"),
                    "Use #true or #false",
                )
            })
        })
        .transpose()
}

fn checked_string(
    entry: &KdlEntry,
    label: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<String, NativeCatalogDiagnostic> {
    let value = entry.value().as_string().ok_or_else(|| {
        entry_diagnostic(
            entry,
            source_name,
            format!("{label} must be a KDL string"),
            "Quote the value as a non-empty string",
        )
    })?;
    if value.trim().is_empty() {
        return Err(entry_diagnostic(
            entry,
            source_name,
            format!("{label} must not be empty or whitespace-only"),
            "Provide a meaningful non-empty value",
        ));
    }
    if value.len() > limits.string_bytes_max {
        let mut diagnostic = entry_diagnostic(
            entry,
            source_name,
            format!("{label} exceeds its UTF-8 byte limit"),
            "Shorten this value and retry",
        );
        diagnostic.kind = NativeDiagnosticKind::ResourceLimit;
        diagnostic.context = vec![
            format!("limit: {}", limits.string_bytes_max),
            format!("observed: {}", value.len()),
        ];
        return Err(diagnostic);
    }
    if value.chars().any(char::is_control) {
        return Err(entry_diagnostic(
            entry,
            source_name,
            format!("{label} contains a control character"),
            "Remove terminal and non-printing control characters",
        ));
    }
    Ok(value.to_owned())
}

fn required_children<'a>(
    node: &'a KdlNode,
    source_name: &str,
) -> Result<&'a KdlDocument, NativeCatalogDiagnostic> {
    node.children().ok_or_else(|| {
        node_diagnostic(
            node,
            source_name,
            format!("`{}` requires a child block", node.name().value()),
            "Add the documented child nodes inside braces",
        )
    })
}

fn reject_children(node: &KdlNode, source_name: &str) -> Result<(), NativeCatalogDiagnostic> {
    if node.children().is_some() {
        return Err(node_diagnostic(
            node,
            source_name,
            format!("`{}` does not accept a child block", node.name().value()),
            "Remove the child block",
        ));
    }
    Ok(())
}

fn effective_platforms(
    inherited: &[NativePlatform],
    declared: &mut Vec<NativePlatform>,
    node: &KdlNode,
    source_name: &str,
) -> Result<Vec<NativePlatform>, NativeCatalogDiagnostic> {
    declared.sort_unstable();
    if declared.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(node_diagnostic(
            node,
            source_name,
            "platform declaration contains a duplicate platform",
            "Keep each platform at most once",
        ));
    }
    if declared.contains(&NativePlatform::Any) && declared.len() != 1 {
        return Err(node_diagnostic(
            node,
            source_name,
            "platform `any` cannot be combined with specific platforms",
            "Use `any` alone or list only specific platforms",
        ));
    }
    if declared.is_empty() {
        return Ok(inherited.to_vec());
    }
    if inherited == [NativePlatform::Any] {
        return Ok(declared.clone());
    }
    if declared.as_slice() == [NativePlatform::Any] {
        return Ok(inherited.to_vec());
    }
    let result = inherited
        .iter()
        .filter(|platform| declared.contains(platform))
        .copied()
        .collect::<Vec<_>>();
    if result.is_empty() {
        return Err(node_diagnostic(
            node,
            source_name,
            "declared platforms do not overlap inherited platforms",
            "Remove the platform declaration or select an inherited platform",
        ));
    }
    Ok(result)
}

fn validate_unique_strings(
    values: &mut [String],
    label: &str,
    node: &KdlNode,
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    values.sort();
    if let Some(duplicate) = values
        .windows(2)
        .find(|pair| pair[0] == pair[1])
        .map(|pair| pair[0].clone())
    {
        return Err(node_diagnostic(
            node,
            source_name,
            format!("duplicate {label} `{duplicate}`"),
            format!("Keep each {label} at most once"),
        ));
    }
    Ok(())
}

fn validate_flag_set(
    flags: &mut [NativeFlag],
    node: &KdlNode,
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut names = BTreeMap::<&str, Vec<&[NativePlatform]>>::new();
    for flag in flags.iter() {
        for name in std::iter::once(&flag.name).chain(flag.short.iter()) {
            let scopes = names.entry(name).or_default();
            if scopes
                .iter()
                .any(|existing| platform_sets_overlap(existing, &flag.platforms))
            {
                return Err(node_diagnostic(
                    node,
                    source_name,
                    format!("duplicate flag name `{name}`"),
                    "Use each long and short flag name at most once on overlapping platforms",
                ));
            }
            scopes.push(&flag.platforms);
        }
    }
    flags.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.platforms.cmp(&right.platforms))
    });
    Ok(())
}

fn platform_sets_overlap(left: &[NativePlatform], right: &[NativePlatform]) -> bool {
    left.contains(&NativePlatform::Any)
        || right.contains(&NativePlatform::Any)
        || left.iter().any(|platform| right.contains(platform))
}

fn validate_argument_set(
    arguments: &[NativeArgument],
    node: &KdlNode,
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut names = BTreeSet::new();
    let mut optional_seen = false;
    for (index, argument) in arguments.iter().enumerate() {
        if !names.insert(&argument.name) {
            return Err(node_diagnostic(
                node,
                source_name,
                format!("duplicate argument `{}`", argument.name),
                "Use each positional argument name at most once",
            ));
        }
        if optional_seen && argument.required {
            return Err(node_diagnostic(
                node,
                source_name,
                format!(
                    "required argument `{}` follows an optional argument",
                    argument.name
                ),
                "Place every required positional argument before optional arguments",
            ));
        }
        optional_seen |= !argument.required;
        if argument.repeatable && index + 1 != arguments.len() {
            return Err(node_diagnostic(
                node,
                source_name,
                format!("repeatable argument `{}` is not last", argument.name),
                "Move the repeatable positional argument to the end",
            ));
        }
    }
    Ok(())
}

fn validate_sibling_names(
    commands: &[NativeCommand],
    parent: &str,
    node: &KdlNode,
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut names = BTreeMap::<&str, &str>::new();
    for command in commands {
        for name in std::iter::once(&command.name).chain(command.aliases.iter()) {
            if let Some(previous) = names.insert(name, &command.name) {
                return Err(node_diagnostic(
                    node,
                    source_name,
                    format!(
                        "command name or alias `{name}` under `{parent}` conflicts between `{previous}` and `{}`",
                        command.name
                    ),
                    "Use unique command names and aliases within each parent",
                ));
            }
        }
    }
    Ok(())
}

fn validate_unique_paths(
    commands: &[NativeCommand],
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut paths = BTreeSet::new();
    let mut stack = commands
        .iter()
        .map(|command| (command, command.name.clone()))
        .collect::<Vec<_>>();
    while let Some((command, path)) = stack.pop() {
        if !paths.insert(path.clone()) {
            return Err(validation_diagnostic(
                source_name,
                None,
                format!("duplicate normalized command path `{path}`"),
                "Use unique names at every command-tree level",
            ));
        }
        for child in &command.subcommands {
            stack.push((child, format!("{path} {}", child.name)));
        }
    }
    Ok(())
}

fn validate_identifier(
    value: &str,
    label: &str,
    node: &KdlNode,
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    if !valid_identifier(value) {
        return Err(node_diagnostic(
            node,
            source_name,
            format!("invalid {label} `{value}`"),
            "Use lowercase ASCII letters, digits, hyphens, or underscores",
        ));
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn valid_long_flag(value: &str) -> bool {
    value.strip_prefix("--").is_some_and(|name| {
        !name.is_empty() && {
            let mut bytes = name.bytes();
            bytes
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                && bytes
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        }
    })
}

fn valid_short_flag(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 2 && bytes[0] == b'-' && bytes[1].is_ascii_alphanumeric()
}

fn valid_flag_name(value: &str) -> bool {
    valid_long_flag(value) || valid_short_flag(value) || valid_windows_flag(value)
}

fn valid_windows_flag(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphanumeric()
        && bytes[2..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

fn valid_source_url(value: &str) -> bool {
    (value.starts_with("https://") || value.starts_with("http://"))
        && value.len() > "https://".len()
        && !value.chars().any(char::is_whitespace)
}

fn validate_limits(limits: NativeCatalogLimits) -> Result<(), NativeCatalogDiagnostic> {
    for (label, configured, hard) in [
        (
            "source bytes",
            limits.source_bytes_max,
            SOURCE_BYTES_HARD_MAX,
        ),
        (
            "database bytes",
            limits.database_bytes_max,
            DATABASE_BYTES_HARD_MAX,
        ),
        ("command count", limits.command_count_max, COMMANDS_HARD_MAX),
        (
            "command depth",
            limits.command_depth_max,
            COMMAND_DEPTH_HARD_MAX,
        ),
        ("flag count", limits.flag_count_max, FLAGS_HARD_MAX),
        (
            "argument count",
            limits.argument_count_max,
            ARGUMENTS_HARD_MAX,
        ),
        (
            "values per command",
            limits.values_per_command_max,
            VALUES_PER_COMMAND_HARD_MAX,
        ),
        (
            "string bytes",
            limits.string_bytes_max,
            STRING_BYTES_HARD_MAX,
        ),
        (
            "semantic document count",
            limits.semantic_document_count_max,
            DOCUMENTS_HARD_MAX,
        ),
        ("query bytes", limits.query_bytes_max, QUERY_BYTES_HARD_MAX),
        ("query results", limits.query_results_max, RESULTS_HARD_MAX),
    ] {
        if configured == 0 || configured > hard {
            return Err(NativeCatalogDiagnostic {
                kind: NativeDiagnosticKind::Validation,
                source_name: "<native catalog limits>".to_owned(),
                message: format!("invalid {label} limit"),
                byte_offset: None,
                byte_length: None,
                help: format!("Choose a value from 1 through {hard}"),
                context: vec![
                    format!("configured: {configured}"),
                    format!("hard maximum: {hard}"),
                ],
            });
        }
    }
    Ok(())
}

fn validation_diagnostic(
    source_name: &str,
    span: Option<(usize, usize)>,
    message: impl Into<String>,
    help: impl Into<String>,
) -> NativeCatalogDiagnostic {
    NativeCatalogDiagnostic {
        kind: NativeDiagnosticKind::Validation,
        source_name: source_name.to_owned(),
        message: message.into(),
        byte_offset: span.map(|span| span.0),
        byte_length: span.map(|span| span.1),
        help: help.into(),
        context: Vec::new(),
    }
}

fn node_diagnostic(
    node: &KdlNode,
    source_name: &str,
    message: impl Into<String>,
    help: impl Into<String>,
) -> NativeCatalogDiagnostic {
    let span = node.span();
    validation_diagnostic(
        source_name,
        Some((span.offset(), span.len())),
        message,
        help,
    )
}

fn entry_diagnostic(
    entry: &KdlEntry,
    source_name: &str,
    message: impl Into<String>,
    help: impl Into<String>,
) -> NativeCatalogDiagnostic {
    let span = entry.span();
    validation_diagnostic(
        source_name,
        Some((span.offset(), span.len())),
        message,
        help,
    )
}

fn resource_diagnostic(
    source_name: &str,
    label: &str,
    limit: usize,
    observed: usize,
) -> NativeCatalogDiagnostic {
    NativeCatalogDiagnostic {
        kind: NativeDiagnosticKind::ResourceLimit,
        source_name: source_name.to_owned(),
        message: format!("{label} exceeds its configured limit"),
        byte_offset: None,
        byte_length: None,
        help: format!("Reduce {label} and retry"),
        context: vec![format!("limit: {limit}"), format!("observed: {observed}")],
    }
}

fn node_resource_diagnostic(
    node: &KdlNode,
    source_name: &str,
    label: &str,
    limit: usize,
    observed: usize,
) -> NativeCatalogDiagnostic {
    let span = node.span();
    let mut diagnostic = resource_diagnostic(source_name, label, limit, observed);
    diagnostic.byte_offset = Some(span.offset());
    diagnostic.byte_length = Some(span.len());
    diagnostic
}

fn io_diagnostic(action: &str, path: &Path, error: io::Error) -> NativeCatalogDiagnostic {
    NativeCatalogDiagnostic {
        kind: NativeDiagnosticKind::Io,
        source_name: path.display().to_string(),
        message: format!("could not {action} native catalog file"),
        byte_offset: None,
        byte_length: None,
        help: "Use a writable private directory and an unlinked regular destination file"
            .to_owned(),
        context: vec![error.to_string()],
    }
}

fn database_diagnostic(
    source_name: &str,
    action: &str,
    error: impl fmt::Display,
) -> NativeCatalogDiagnostic {
    NativeCatalogDiagnostic {
        kind: NativeDiagnosticKind::Database,
        source_name: source_name.to_owned(),
        message: format!("could not {action} native catalog database"),
        byte_offset: None,
        byte_length: None,
        help: "Recompile the catalog from a valid bounded KDL source".to_owned(),
        context: vec![error.to_string()],
    }
}

#[derive(Clone)]
struct FlatCommand<'a> {
    command: &'a NativeCommand,
    path: String,
    parent_path: Option<String>,
    depth: usize,
}

struct SemanticDocumentInsert<'a> {
    kind: &'a str,
    command_id: i64,
    target: &'a str,
    title: &'a str,
    body: &'a str,
    platforms: &'a [NativePlatform],
}

/// Compile a validated typed catalog into a deterministic, bounded SQLite image.
///
/// The exact typed JSON snapshot and every normalized projection are committed in
/// one SQLite transaction. No filesystem state is touched.
pub fn compile_native_catalog(
    catalog: &NativeCatalog,
    limits: NativeCatalogLimits,
) -> Result<Vec<u8>, NativeCatalogDiagnostic> {
    validate_limits(limits)?;
    validate_typed_catalog(catalog, limits)?;
    let snapshot = serde_json::to_vec(catalog).map_err(|error| {
        database_diagnostic("<native catalog>", "serialize typed snapshot", error)
    })?;
    if snapshot.len() > limits.database_bytes_max {
        return Err(resource_diagnostic(
            "<native catalog>",
            "typed snapshot bytes",
            limits.database_bytes_max,
            snapshot.len(),
        ));
    }
    let mut connection = Connection::open_in_memory()
        .map_err(|error| database_diagnostic("<native catalog>", "open in-memory", error))?;
    set_sqlite_limits(&connection, limits)?;
    connection
        .execute_batch(SCHEMA)
        .map_err(|error| database_diagnostic("<native catalog>", "create schema", error))?;
    connection
        .pragma_update(None, "application_id", NATIVE_DATABASE_APPLICATION_ID)
        .and_then(|()| {
            connection.pragma_update(None, "user_version", NATIVE_DATABASE_SCHEMA_VERSION)
        })
        .map_err(|error| database_diagnostic("<native catalog>", "set schema identity", error))?;
    let transaction = connection.transaction().map_err(|error| {
        database_diagnostic("<native catalog>", "begin compilation transaction", error)
    })?;
    insert_catalog(&transaction, catalog, &snapshot)?;
    transaction.commit().map_err(|error| {
        database_diagnostic("<native catalog>", "commit compilation transaction", error)
    })?;
    let data: Data<'_> = connection.serialize(MAIN_DB).map_err(|error| {
        database_diagnostic("<native catalog>", "serialize SQLite image", error)
    })?;
    if data.len() > limits.database_bytes_max {
        return Err(resource_diagnostic(
            "<native catalog>",
            "SQLite image bytes",
            limits.database_bytes_max,
            data.len(),
        ));
    }
    Ok(data.to_vec())
}

/// Parse strict KDL and compile it into one bounded deterministic SQLite image.
pub fn compile_native_catalog_source(
    source: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<Vec<u8>, NativeCatalogDiagnostic> {
    let catalog = parse_native_catalog(source, source_name, limits)?;
    compile_native_catalog(&catalog, limits)
}

fn set_sqlite_limits(
    connection: &Connection,
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    let length_limit = i32::try_from(limits.database_bytes_max)
        .unwrap_or(i32::MAX)
        .min(SQLITE_LENGTH_HARD_MAX);
    for (limit, value) in [
        (Limit::SQLITE_LIMIT_LENGTH, length_limit),
        (Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024),
        (Limit::SQLITE_LIMIT_COLUMN, 64),
        (Limit::SQLITE_LIMIT_EXPR_DEPTH, 32),
        (Limit::SQLITE_LIMIT_COMPOUND_SELECT, 16),
        (Limit::SQLITE_LIMIT_VDBE_OP, 100_000),
        (Limit::SQLITE_LIMIT_FUNCTION_ARG, 32),
        (Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH, 4 * 1024),
        (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 64),
        (Limit::SQLITE_LIMIT_TRIGGER_DEPTH, 0),
        (Limit::SQLITE_LIMIT_ATTACHED, 0),
        (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
    ] {
        connection.set_limit(limit, value).map_err(|error| {
            database_diagnostic("<native catalog>", "set SQLite resource limits", error)
        })?;
    }
    Ok(())
}

fn flatten_commands(catalog: &NativeCatalog) -> Vec<FlatCommand<'_>> {
    let mut stack = catalog
        .commands
        .iter()
        .map(|command| FlatCommand {
            command,
            path: command.name.clone(),
            parent_path: None,
            depth: 1,
        })
        .collect::<Vec<_>>();
    let mut flattened = Vec::new();
    while let Some(flat) = stack.pop() {
        let parent_path = flat.path.clone();
        for child in &flat.command.subcommands {
            stack.push(FlatCommand {
                command: child,
                path: format!("{parent_path} {}", child.name),
                parent_path: Some(parent_path.clone()),
                depth: flat.depth.saturating_add(1),
            });
        }
        flattened.push(flat);
    }
    flattened.sort_by(|left, right| left.path.cmp(&right.path));
    flattened
}

fn insert_catalog(
    transaction: &Transaction<'_>,
    catalog: &NativeCatalog,
    snapshot: &[u8],
) -> Result<(), NativeCatalogDiagnostic> {
    transaction
        .execute(
            "INSERT INTO catalog_snapshot(singleton, snapshot_json) VALUES (1, ?1)",
            params![snapshot],
        )
        .map_err(|error| database_diagnostic("<native catalog>", "insert snapshot", error))?;
    transaction
        .execute(
            "INSERT INTO provenance(singleton, catalog_name, author, license, revision, source_url) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
            params![
                catalog.name,
                catalog.provenance.author,
                catalog.provenance.license,
                catalog.provenance.revision,
                catalog.provenance.source_url,
            ],
        )
        .map_err(|error| database_diagnostic("<native catalog>", "insert provenance", error))?;

    let flattened = flatten_commands(catalog);
    let mut ids = BTreeMap::new();
    for (index, flat) in flattened.iter().enumerate() {
        ids.insert(flat.path.as_str(), sqlite_id(index, "command id")?);
    }
    let mut next_flag_id = 1_i64;
    let mut next_argument_id = 1_i64;
    let mut next_document_id = 1_i64;
    for flat in &flattened {
        let command_id = ids[flat.path.as_str()];
        let parent_id = flat
            .parent_path
            .as_deref()
            .and_then(|parent| ids.get(parent))
            .copied();
        transaction
            .execute(
                "INSERT INTO commands(command_id, parent_id, name, full_path, depth, summary, description) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    command_id,
                    parent_id,
                    flat.command.name,
                    flat.path,
                    sqlite_usize(flat.depth, "command depth")?,
                    flat.command.summary,
                    flat.command.description,
                ],
            )
            .map_err(|error| database_diagnostic("<native catalog>", "insert command", error))?;
        for alias in &flat.command.aliases {
            transaction
                .execute(
                    "INSERT INTO command_aliases(command_id, alias) VALUES (?1, ?2)",
                    params![command_id, alias],
                )
                .map_err(|error| database_diagnostic("<native catalog>", "insert alias", error))?;
        }
        for platform in &flat.command.platforms {
            transaction
                .execute(
                    "INSERT INTO command_platforms(command_id, platform) VALUES (?1, ?2)",
                    params![command_id, platform.as_str()],
                )
                .map_err(|error| {
                    database_diagnostic("<native catalog>", "insert platform", error)
                })?;
        }
        for (ordinal, intent) in flat.command.intents.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO command_intents(command_id, ordinal, phrase) VALUES (?1, ?2, ?3)",
                    params![command_id, sqlite_usize(ordinal, "intent ordinal")?, intent],
                )
                .map_err(|error| database_diagnostic("<native catalog>", "insert intent", error))?;
        }
        let body = command_document_body(flat.command, &flat.path);
        insert_document(
            transaction,
            next_document_id,
            SemanticDocumentInsert {
                kind: "command",
                command_id,
                target: &flat.path,
                title: &flat.path,
                body: &body,
                platforms: &flat.command.platforms,
            },
        )?;
        next_document_id = next_document_id.saturating_add(1);
        for flag in &flat.command.flags {
            transaction
                .execute(
                    "INSERT INTO flags(flag_id, command_id, name, short_name, summary, description, value_name, required, repeatable, action) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        next_flag_id,
                        command_id,
                        flag.name,
                        flag.short,
                        flag.summary,
                        flag.description,
                        flag.value_name,
                        flag.required,
                        flag.repeatable,
                        flag.action.map(NativeCompletionAction::as_str),
                    ],
                )
                .map_err(|error| database_diagnostic("<native catalog>", "insert flag", error))?;
            for platform in &flag.platforms {
                transaction
                    .execute(
                        "INSERT INTO flag_platforms(flag_id, platform) VALUES (?1, ?2)",
                        params![next_flag_id, platform.as_str()],
                    )
                    .map_err(|error| {
                        database_diagnostic("<native catalog>", "insert flag platform", error)
                    })?;
            }
            let title = flag.short.as_ref().map_or_else(
                || flag.name.clone(),
                |short| format!("{} {short}", flag.name),
            );
            let body = format!(
                "{} {} {} {} {}",
                flat.path,
                title,
                flag.summary,
                flag.description,
                flag.value_name.as_deref().unwrap_or("")
            );
            insert_document(
                transaction,
                next_document_id,
                SemanticDocumentInsert {
                    kind: "flag",
                    command_id,
                    target: &flag.name,
                    title: &title,
                    body: &body,
                    platforms: &flag.platforms,
                },
            )?;
            next_flag_id = next_flag_id.saturating_add(1);
            next_document_id = next_document_id.saturating_add(1);
        }
        for (ordinal, argument) in flat.command.arguments.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO arguments(argument_id, command_id, ordinal, name, summary, description, required, repeatable, action) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        next_argument_id,
                        command_id,
                        sqlite_usize(ordinal, "argument ordinal")?,
                        argument.name,
                        argument.summary,
                        argument.description,
                        argument.required,
                        argument.repeatable,
                        argument.action.map(NativeCompletionAction::as_str),
                    ],
                )
                .map_err(|error| database_diagnostic("<native catalog>", "insert argument", error))?;
            let body = format!(
                "{} {} {} {}",
                flat.path, argument.name, argument.summary, argument.description
            );
            insert_document(
                transaction,
                next_document_id,
                SemanticDocumentInsert {
                    kind: "argument",
                    command_id,
                    target: &argument.name,
                    title: &argument.name,
                    body: &body,
                    platforms: &flat.command.platforms,
                },
            )?;
            next_argument_id = next_argument_id.saturating_add(1);
            next_document_id = next_document_id.saturating_add(1);
        }
    }
    Ok(())
}

fn insert_document(
    transaction: &Transaction<'_>,
    document_id: i64,
    insert: SemanticDocumentInsert<'_>,
) -> Result<(), NativeCatalogDiagnostic> {
    transaction
        .execute(
            "INSERT INTO semantic_documents(document_id, document_kind, command_id, target, title, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                document_id,
                insert.kind,
                insert.command_id,
                insert.target,
                insert.title,
                insert.body
            ],
        )
        .map_err(|error| database_diagnostic("<native catalog>", "insert semantic document", error))?;
    for platform in insert.platforms {
        transaction
            .execute(
                "INSERT INTO semantic_document_platforms(document_id, platform) VALUES (?1, ?2)",
                params![document_id, platform.as_str()],
            )
            .map_err(|error| {
                database_diagnostic(
                    "<native catalog>",
                    "insert semantic document platform",
                    error,
                )
            })?;
    }
    Ok(())
}

fn command_document_body(command: &NativeCommand, path: &str) -> String {
    format!(
        "{} {} {} {} {}",
        path,
        command.aliases.join(" "),
        command.summary,
        command.description,
        command.intents.join(" ")
    )
}

fn sqlite_id(index: usize, label: &str) -> Result<i64, NativeCatalogDiagnostic> {
    let one_based = index.saturating_add(1);
    sqlite_usize(one_based, label)
}

fn sqlite_usize(value: usize, label: &str) -> Result<i64, NativeCatalogDiagnostic> {
    i64::try_from(value)
        .map_err(|_| resource_diagnostic("<native catalog>", label, i64::MAX as usize, value))
}

fn validate_typed_catalog(
    catalog: &NativeCatalog,
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    typed_identifier(&catalog.name, "catalog name")?;
    typed_string(&catalog.provenance.author, "provenance author", limits)?;
    typed_string(&catalog.provenance.license, "provenance license", limits)?;
    typed_string(&catalog.provenance.revision, "provenance revision", limits)?;
    typed_string(&catalog.provenance.source_url, "provenance source", limits)?;
    if !valid_source_url(&catalog.provenance.source_url) {
        return Err(validation_diagnostic(
            "<native catalog>",
            None,
            "provenance source must be an absolute HTTP(S) URL without whitespace",
            "Use an https:// or http:// source URL",
        ));
    }
    if catalog.commands.is_empty() {
        return Err(validation_diagnostic(
            "<native catalog>",
            None,
            "catalog must contain at least one command",
            "Add a root command",
        ));
    }
    validate_typed_siblings(&catalog.commands, "<root>")?;
    let mut stack = catalog
        .commands
        .iter()
        .map(|command| (command, 1_usize, &[NativePlatform::Any][..]))
        .collect::<Vec<_>>();
    let mut counts = ParseCounts::default();
    let mut paths = BTreeSet::new();
    let mut path_stack = catalog
        .commands
        .iter()
        .map(|command| (command, command.name.clone()))
        .collect::<Vec<_>>();
    while let Some((command, path)) = path_stack.pop() {
        if !paths.insert(path.clone()) {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!("duplicate command path `{path}`"),
                "Use unique command names in each parent scope",
            ));
        }
        for child in &command.subcommands {
            path_stack.push((child, format!("{path} {}", child.name)));
        }
    }
    while let Some((command, depth, inherited_platforms)) = stack.pop() {
        counts.commands = counts.commands.saturating_add(1);
        counts.flags = counts.flags.saturating_add(command.flags.len());
        counts.arguments = counts.arguments.saturating_add(command.arguments.len());
        counts.documents = counts
            .documents
            .saturating_add(1)
            .saturating_add(command.flags.len())
            .saturating_add(command.arguments.len());
        for (label, observed, limit) in [
            ("command count", counts.commands, limits.command_count_max),
            ("command depth", depth, limits.command_depth_max),
            ("flag count", counts.flags, limits.flag_count_max),
            (
                "argument count",
                counts.arguments,
                limits.argument_count_max,
            ),
            (
                "semantic document count",
                counts.documents,
                limits.semantic_document_count_max,
            ),
        ] {
            if observed > limit {
                return Err(resource_diagnostic(
                    "<native catalog>",
                    label,
                    limit,
                    observed,
                ));
            }
        }
        for (label, observed) in [
            ("aliases", command.aliases.len()),
            ("intent phrases", command.intents.len()),
            ("platforms", command.platforms.len()),
            ("flags", command.flags.len()),
            ("arguments", command.arguments.len()),
            ("subcommands", command.subcommands.len()),
        ] {
            if observed > limits.values_per_command_max {
                return Err(resource_diagnostic(
                    "<native catalog>",
                    label,
                    limits.values_per_command_max,
                    observed,
                ));
            }
        }
        typed_identifier(&command.name, "command name")?;
        typed_string(&command.summary, "command summary", limits)?;
        typed_string(&command.description, "command description", limits)?;
        validate_typed_unique_strings(&command.aliases, "alias", limits)?;
        validate_typed_unique_strings(&command.intents, "intent phrase", limits)?;
        for alias in &command.aliases {
            typed_identifier(alias, "alias")?;
        }
        validate_typed_platforms(&command.platforms, inherited_platforms)?;
        validate_typed_flags(&command.flags, &command.platforms, limits)?;
        validate_typed_arguments(&command.arguments, limits)?;
        validate_typed_siblings(&command.subcommands, &command.name)?;
        for child in &command.subcommands {
            stack.push((child, depth.saturating_add(1), command.platforms.as_slice()));
        }
    }
    Ok(())
}

fn typed_string(
    value: &str,
    label: &str,
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    if value.trim().is_empty() {
        return Err(validation_diagnostic(
            "<native catalog>",
            None,
            format!("{label} must not be empty"),
            "Provide a meaningful non-empty value",
        ));
    }
    if value.len() > limits.string_bytes_max {
        return Err(resource_diagnostic(
            "<native catalog>",
            label,
            limits.string_bytes_max,
            value.len(),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(validation_diagnostic(
            "<native catalog>",
            None,
            format!("{label} contains a control character"),
            "Remove terminal and non-printing control characters",
        ));
    }
    Ok(())
}

fn typed_identifier(value: &str, label: &str) -> Result<(), NativeCatalogDiagnostic> {
    if valid_identifier(value) {
        return Ok(());
    }
    Err(validation_diagnostic(
        "<native catalog>",
        None,
        format!("invalid {label} `{value}`"),
        "Use lowercase ASCII letters, digits, hyphens, or underscores",
    ))
}

fn validate_typed_unique_strings(
    values: &[String],
    label: &str,
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut seen = BTreeSet::new();
    for value in values {
        typed_string(value, label, limits)?;
        if !seen.insert(value) {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!("duplicate {label} `{value}`"),
                format!("Keep each {label} at most once"),
            ));
        }
    }
    Ok(())
}

fn validate_typed_platforms(
    platforms: &[NativePlatform],
    inherited: &[NativePlatform],
) -> Result<(), NativeCatalogDiagnostic> {
    if platforms.is_empty() {
        return Err(validation_diagnostic(
            "<native catalog>",
            None,
            "catalog item has no effective platform",
            "Use at least one platform or NativePlatform::Any",
        ));
    }
    let set = platforms.iter().collect::<BTreeSet<_>>();
    if set.len() != platforms.len()
        || (platforms.contains(&NativePlatform::Any) && platforms.len() != 1)
    {
        return Err(validation_diagnostic(
            "<native catalog>",
            None,
            "catalog item contains duplicate or contradictory platforms",
            "Use unique specific platforms, or use any by itself",
        ));
    }
    if inherited != [NativePlatform::Any]
        && !platforms
            .iter()
            .all(|platform| inherited.contains(platform))
    {
        return Err(validation_diagnostic(
            "<native catalog>",
            None,
            "platform is not supported by its inherited scope",
            "Restrict platforms to the inherited effective platform set",
        ));
    }
    Ok(())
}

fn validate_typed_flags(
    flags: &[NativeFlag],
    command_platforms: &[NativePlatform],
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut names = BTreeMap::<&str, Vec<&[NativePlatform]>>::new();
    for flag in flags {
        if !valid_flag_name(&flag.name) {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!("invalid flag `{}`", flag.name),
                "Use a lowercase long name such as --output-file, a short-only name such as -P, or a Windows name such as /q",
            ));
        }
        validate_typed_platforms(&flag.platforms, command_platforms)?;
        let mut spellings = vec![flag.name.as_str()];
        if flag.short.is_some() && valid_short_flag(&flag.name) {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!(
                    "short-only flag `{}` declares another short alias",
                    flag.name
                ),
                "Remove the short alias or use a long canonical spelling",
            ));
        }
        if let Some(short) = &flag.short {
            if !valid_short_flag(short) {
                return Err(validation_diagnostic(
                    "<native catalog>",
                    None,
                    format!("invalid short flag `{short}`"),
                    "Use one ASCII short flag such as -o",
                ));
            }
            spellings.push(short);
        }
        for spelling in spellings {
            let scopes = names.entry(spelling).or_default();
            if scopes
                .iter()
                .any(|existing| platform_sets_overlap(existing, &flag.platforms))
            {
                return Err(validation_diagnostic(
                    "<native catalog>",
                    None,
                    format!("duplicate flag `{spelling}` on overlapping platforms"),
                    "Use each flag spelling at most once on overlapping platforms",
                ));
            }
            scopes.push(&flag.platforms);
        }
        typed_string(&flag.summary, "flag summary", limits)?;
        typed_string(&flag.description, "flag description", limits)?;
        if let Some(value) = &flag.value_name {
            typed_identifier(value, "flag value placeholder")?;
        }
        if flag.action.is_some() && flag.value_name.is_none() {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!("boolean flag `{}` declares a completion action", flag.name),
                "Add a value placeholder or remove the action",
            ));
        }
    }
    Ok(())
}

fn validate_typed_arguments(
    arguments: &[NativeArgument],
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut names = BTreeSet::new();
    let mut optional_seen = false;
    for (index, argument) in arguments.iter().enumerate() {
        typed_identifier(&argument.name, "argument name")?;
        if !names.insert(&argument.name) {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!("duplicate argument `{}`", argument.name),
                "Use unique positional argument names",
            ));
        }
        typed_string(&argument.summary, "argument summary", limits)?;
        typed_string(&argument.description, "argument description", limits)?;
        if optional_seen && argument.required {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!(
                    "required argument `{}` follows an optional argument",
                    argument.name
                ),
                "Place required positional arguments first",
            ));
        }
        optional_seen |= !argument.required;
        if argument.repeatable && index + 1 != arguments.len() {
            return Err(validation_diagnostic(
                "<native catalog>",
                None,
                format!("repeatable argument `{}` is not last", argument.name),
                "Move the repeatable positional argument to the end",
            ));
        }
    }
    Ok(())
}

fn validate_typed_siblings(
    commands: &[NativeCommand],
    parent: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    let mut names = BTreeMap::<&str, &str>::new();
    for command in commands {
        for name in std::iter::once(&command.name).chain(command.aliases.iter()) {
            if let Some(previous) = names.insert(name, &command.name) {
                return Err(validation_diagnostic(
                    "<native catalog>",
                    None,
                    format!(
                        "command token `{name}` under `{parent}` conflicts between `{previous}` and `{}`",
                        command.name
                    ),
                    "Use unique sibling command names and aliases",
                ));
            }
        }
    }
    Ok(())
}

/// Read-only, fully validated view over one exact native SQLite catalog image.
pub struct NativeCatalogReader {
    connection: Connection,
    snapshot: NativeCatalog,
    limits: NativeCatalogLimits,
}

impl fmt::Debug for NativeCatalogReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeCatalogReader")
            .field("snapshot", &self.snapshot)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl NativeCatalogReader {
    /// Validate and open a bounded in-memory copy of an encoded catalog image.
    pub fn from_bytes(
        bytes: &[u8],
        limits: NativeCatalogLimits,
    ) -> Result<Self, NativeCatalogDiagnostic> {
        Self::from_bytes_named(bytes, "<native catalog database>", limits)
    }

    /// Read an admitted unlinked regular file and validate its complete image.
    pub fn open(path: &Path, limits: NativeCatalogLimits) -> Result<Self, NativeCatalogDiagnostic> {
        validate_limits(limits)?;
        let bytes = read_admitted_file(path, limits.database_bytes_max)?;
        Self::from_bytes_named(&bytes, &path.display().to_string(), limits)
    }

    /// Return the exact typed snapshot stored with the normalized projections.
    pub fn snapshot(&self) -> &NativeCatalog {
        &self.snapshot
    }

    /// Project the validated native tree into Quirl's shared semantic catalog model.
    ///
    /// Commands outside `platform` are omitted together with their descendants.
    /// Curated facts use high-confidence declared provenance, so exact builtin and
    /// trusted-plugin records retain precedence when callers merge this result.
    /// Completion actions become inert provider identities; projection and ordinary
    /// catalog lookup never execute a provider or catalog-supplied code.
    pub fn project_commands(&self, platform: NativePlatform) -> Vec<CommandSpec> {
        let provenance = native_provenance(&self.snapshot);
        let mut projected = Vec::new();
        let mut stack = self
            .snapshot
            .commands
            .iter()
            .rev()
            .map(|command| (command, None::<String>, None::<String>))
            .collect::<Vec<_>>();
        while let Some((command, parent_path, parent_id)) = stack.pop() {
            if !native_command_supports(command, platform) {
                continue;
            }
            let path = parent_path.as_ref().map_or_else(
                || command.name.clone(),
                |parent| format!("{parent} {}", command.name),
            );
            let id = format!("native:{}:{path}", self.snapshot.name);
            let options = native_arguments(command, platform, &provenance);
            projected.push(CommandSpec {
                id: id.clone(),
                version: Some(self.snapshot.provenance.revision.clone()),
                path: path.clone(),
                aliases: command
                    .aliases
                    .iter()
                    .map(|alias| {
                        parent_path
                            .as_ref()
                            .map_or_else(|| alias.clone(), |parent| format!("{parent} {alias}"))
                    })
                    .collect(),
                parent: parent_id,
                signature: native_signature(&path, &options),
                summary: command.summary.clone(),
                details: native_details(command),
                options,
                examples: Vec::new(),
                io: IoContract::default(),
                effects: Vec::<Effect>::new(),
                exit_codes: BTreeMap::new(),
                provenance: provenance.clone(),
            });
            stack.extend(
                command
                    .subcommands
                    .iter()
                    .rev()
                    .map(|child| (child, Some(path.clone()), Some(id.clone()))),
            );
        }
        projected
    }

    /// Query root or nested child-command tokens for one platform.
    pub fn subcommands(
        &self,
        parent_path: &str,
        platform: NativePlatform,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<NativeCompletionCandidate>, NativeCatalogDiagnostic> {
        self.validate_query(parent_path, prefix, limit)?;
        let limit = sqlite_usize(limit, "query result limit")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT n.token, c.summary, c.description
                 FROM (
                    SELECT command_id, name AS token FROM commands
                    UNION ALL
                    SELECT command_id, alias AS token FROM command_aliases
                 ) n
                 JOIN commands c ON c.command_id = n.command_id
                 WHERE ((?1 = '' AND c.parent_id IS NULL) OR c.parent_id = (SELECT command_id FROM commands WHERE full_path = ?1))
                   AND (?2 = 'any' OR EXISTS (SELECT 1 FROM command_platforms p WHERE p.command_id = c.command_id AND (p.platform = 'any' OR p.platform = ?2)))
                   AND substr(n.token, 1, length(?3)) = ?3
                 ORDER BY n.token, c.full_path
                 LIMIT ?4",
            )
            .map_err(|error| self.query_error("prepare subcommand query", error))?;
        let rows = statement
            .query_map(
                params![parent_path, platform.as_str(), prefix, limit],
                |row| {
                    Ok(NativeCompletionCandidate {
                        kind: NativeCompletionKind::Subcommand,
                        value: row.get(0)?,
                        summary: row.get(1)?,
                        description: row.get(2)?,
                        action: None,
                    })
                },
            )
            .map_err(|error| self.query_error("execute subcommand query", error))?;
        collect_rows(rows, self.limits.query_results_max, "subcommand query")
    }

    /// Query long and short flags for an exact command path and platform.
    pub fn flags(
        &self,
        command_path: &str,
        platform: NativePlatform,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<NativeCompletionCandidate>, NativeCatalogDiagnostic> {
        self.validate_query(command_path, prefix, limit)?;
        let limit = sqlite_usize(limit, "query result limit")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT n.token, f.summary, f.description, f.action
                 FROM (
                    SELECT flag_id, name AS token FROM flags
                    UNION ALL
                    SELECT flag_id, short_name AS token FROM flags WHERE short_name IS NOT NULL
                 ) n
                 JOIN flags f ON f.flag_id = n.flag_id
                 JOIN commands c ON c.command_id = f.command_id
                 WHERE c.full_path = ?1
                   AND (?2 = 'any' OR EXISTS (SELECT 1 FROM command_platforms p WHERE p.command_id = c.command_id AND (p.platform = 'any' OR p.platform = ?2)))
                   AND (?2 = 'any' OR EXISTS (SELECT 1 FROM flag_platforms p WHERE p.flag_id = f.flag_id AND (p.platform = 'any' OR p.platform = ?2)))
                   AND substr(n.token, 1, length(?3)) = ?3
                 ORDER BY n.token, f.flag_id
                 LIMIT ?4",
            )
            .map_err(|error| self.query_error("prepare flag query", error))?;
        let rows = statement
            .query_map(
                params![command_path, platform.as_str(), prefix, limit],
                |row| {
                    let action: Option<String> = row.get(3)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        action,
                    ))
                },
            )
            .map_err(|error| self.query_error("execute flag query", error))?;
        let rows = collect_rows(rows, self.limits.query_results_max, "flag query")?;
        rows.into_iter()
            .map(|(value, summary, description, action)| {
                Ok(NativeCompletionCandidate {
                    kind: NativeCompletionKind::Flag,
                    value,
                    summary,
                    description,
                    action: parse_stored_action(action.as_deref())?,
                })
            })
            .collect()
    }

    /// Query ordered positional argument placeholders for an exact command path.
    pub fn arguments(
        &self,
        command_path: &str,
        platform: NativePlatform,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<NativeCompletionCandidate>, NativeCatalogDiagnostic> {
        self.validate_query(command_path, prefix, limit)?;
        let limit = sqlite_usize(limit, "query result limit")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT a.name, a.summary, a.description, a.action
                 FROM arguments a JOIN commands c ON c.command_id = a.command_id
                 WHERE c.full_path = ?1
                   AND (?2 = 'any' OR EXISTS (SELECT 1 FROM command_platforms p WHERE p.command_id = c.command_id AND (p.platform = 'any' OR p.platform = ?2)))
                   AND substr(a.name, 1, length(?3)) = ?3
                 ORDER BY a.ordinal
                 LIMIT ?4",
            )
            .map_err(|error| self.query_error("prepare argument query", error))?;
        let rows = statement
            .query_map(
                params![command_path, platform.as_str(), prefix, limit],
                |row| {
                    let action: Option<String> = row.get(3)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        action,
                    ))
                },
            )
            .map_err(|error| self.query_error("execute argument query", error))?;
        let rows = collect_rows(rows, self.limits.query_results_max, "argument query")?;
        rows.into_iter()
            .map(|(value, summary, description, action)| {
                Ok(NativeCompletionCandidate {
                    kind: NativeCompletionKind::Argument,
                    value,
                    summary,
                    description,
                    action: parse_stored_action(action.as_deref())?,
                })
            })
            .collect()
    }

    /// Perform bounded deterministic lexical lookup over semantic documents.
    pub fn semantic_lookup(
        &self,
        query: &str,
        platform: NativePlatform,
        limit: usize,
    ) -> Result<Vec<NativeSemanticHit>, NativeCatalogDiagnostic> {
        self.validate_query(query, "", limit)?;
        let terms = query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let read_limit = sqlite_usize(
            self.limits.semantic_document_count_max.saturating_add(1),
            "semantic document read limit",
        )?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT c.full_path, d.target, d.title, d.body
                 FROM semantic_documents d JOIN commands c ON c.command_id = d.command_id
                 WHERE (?1 = 'any' OR EXISTS (SELECT 1 FROM semantic_document_platforms p WHERE p.document_id = d.document_id AND (p.platform = 'any' OR p.platform = ?1)))
                 ORDER BY d.document_id LIMIT ?2",
            )
            .map_err(|error| self.query_error("prepare semantic query", error))?;
        let rows = statement
            .query_map(params![platform.as_str(), read_limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|error| self.query_error("execute semantic query", error))?;
        let documents = collect_rows(
            rows,
            self.limits.semantic_document_count_max,
            "semantic document scan",
        )?;
        let mut hits = documents
            .into_iter()
            .filter_map(|(command_path, target, title, body)| {
                let body = body.to_lowercase();
                let score = terms.iter().fold(0_u32, |score, term| {
                    score.saturating_add(if body.contains(term) { 1 } else { 0 })
                });
                (score > 0).then_some(NativeSemanticHit {
                    command_path,
                    target,
                    title,
                    score,
                })
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.command_path.cmp(&right.command_path))
                .then_with(|| left.target.cmp(&right.target))
                .then_with(|| left.title.cmp(&right.title))
        });
        hits.truncate(limit);
        Ok(hits)
    }

    fn from_bytes_named(
        bytes: &[u8],
        source_name: &str,
        limits: NativeCatalogLimits,
    ) -> Result<Self, NativeCatalogDiagnostic> {
        validate_limits(limits)?;
        if bytes.len() > limits.database_bytes_max {
            return Err(resource_diagnostic(
                source_name,
                "SQLite image bytes",
                limits.database_bytes_max,
                bytes.len(),
            ));
        }
        let mut connection = Connection::open_in_memory()
            .map_err(|error| database_diagnostic(source_name, "open reader", error))?;
        set_sqlite_limits(&connection, limits)?;
        connection
            .deserialize_read_exact(MAIN_DB, Cursor::new(bytes), bytes.len(), true)
            .map_err(|error| database_diagnostic(source_name, "deserialize image", error))?;
        connection
            .execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF;")
            .map_err(|error| database_diagnostic(source_name, "harden reader", error))?;
        validate_database_identity(&connection, source_name)?;
        let snapshot_bytes: Vec<u8> = connection
            .query_row(
                "SELECT snapshot_json FROM catalog_snapshot WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| database_diagnostic(source_name, "read typed snapshot", error))?;
        let snapshot =
            serde_json::from_slice::<NativeCatalog>(&snapshot_bytes).map_err(|error| {
                validation_diagnostic(
                    source_name,
                    None,
                    format!("typed catalog snapshot is invalid: {error}"),
                    "Recompile the database from valid KDL source",
                )
            })?;
        validate_typed_catalog(&snapshot, limits)?;
        let expected = compile_native_catalog(&snapshot, limits)?;
        if expected != bytes {
            return Err(validation_diagnostic(
                source_name,
                None,
                "normalized database rows do not exactly match the typed snapshot",
                "Recompile the complete database from its authoritative KDL source",
            ));
        }
        Ok(Self {
            connection,
            snapshot,
            limits,
        })
    }

    fn validate_query(
        &self,
        primary: &str,
        secondary: &str,
        limit: usize,
    ) -> Result<(), NativeCatalogDiagnostic> {
        let observed = primary.len().saturating_add(secondary.len());
        if observed > self.limits.query_bytes_max {
            return Err(resource_diagnostic(
                "<native catalog query>",
                "query bytes",
                self.limits.query_bytes_max,
                observed,
            ));
        }
        if limit == 0 || limit > self.limits.query_results_max {
            return Err(resource_diagnostic(
                "<native catalog query>",
                "query results",
                self.limits.query_results_max,
                limit,
            ));
        }
        Ok(())
    }

    fn query_error(&self, action: &str, error: impl fmt::Display) -> NativeCatalogDiagnostic {
        database_diagnostic("<native catalog database>", action, error)
    }
}

fn native_provenance(catalog: &NativeCatalog) -> ProvenanceInfo {
    ProvenanceInfo {
        source: Provenance::External,
        confidence: Confidence::High,
        trust: Trust::Declared,
        origin: Some(catalog.provenance.source_url.clone()),
        fingerprint: Some(catalog.provenance.revision.clone()),
        generated_at: None,
    }
}

fn native_command_supports(command: &NativeCommand, platform: NativePlatform) -> bool {
    native_platforms_support(&command.platforms, platform)
}

fn native_platforms_support(platforms: &[NativePlatform], platform: NativePlatform) -> bool {
    platform == NativePlatform::Any
        || platforms.contains(&NativePlatform::Any)
        || platforms.contains(&platform)
}

fn native_arguments(
    command: &NativeCommand,
    platform: NativePlatform,
    provenance: &ProvenanceInfo,
) -> Vec<ArgumentSpec> {
    let flags = command
        .flags
        .iter()
        .filter(|flag| native_platforms_support(&flag.platforms, platform))
        .map(|flag| {
            let action = flag.action;
            ArgumentSpec {
                names: flag
                    .short
                    .iter()
                    .cloned()
                    .chain(std::iter::once(flag.name.clone()))
                    .collect(),
                kind: if flag.value_name.is_some() {
                    ArgumentKind::Option
                } else {
                    ArgumentKind::Flag
                },
                value_type: action.map_or_else(
                    || flag.value_name.clone().unwrap_or_else(|| "Bool".to_owned()),
                    |action| action.value_type().to_owned(),
                ),
                required: flag.required,
                repeatable: flag.repeatable,
                values: action.map(|action| CompletionSource::Dynamic {
                    provider: action.provider_identity().to_owned(),
                }),
                conflicts: Vec::new(),
                documentation: native_documentation(&flag.summary, &flag.description),
                examples: Vec::new(),
                provenance: provenance.clone(),
            }
        });
    let positional = command.arguments.iter().map(|argument| {
        let action = argument.action;
        ArgumentSpec {
            names: vec![argument.name.clone()],
            kind: ArgumentKind::Positional,
            value_type: action
                .map_or("String", NativeCompletionAction::value_type)
                .to_owned(),
            required: argument.required,
            repeatable: argument.repeatable,
            values: action.map(|action| CompletionSource::Dynamic {
                provider: action.provider_identity().to_owned(),
            }),
            conflicts: Vec::new(),
            documentation: native_documentation(&argument.summary, &argument.description),
            examples: Vec::new(),
            provenance: provenance.clone(),
        }
    });
    flags.chain(positional).collect()
}

fn native_signature(path: &str, arguments: &[ArgumentSpec]) -> String {
    let mut signature = path.to_owned();
    for argument in arguments {
        let name = argument.names.last().map_or("value", String::as_str);
        let value = match argument.kind {
            ArgumentKind::Flag => name.to_owned(),
            ArgumentKind::Option => format!("{name} <{}>", argument.value_type),
            ArgumentKind::Positional => format!("<{}>", name),
        };
        let value = if argument.repeatable {
            format!("{value}...")
        } else {
            value
        };
        if argument.required {
            signature.push_str(&format!(" {value}"));
        } else {
            signature.push_str(&format!(" [{value}]"));
        }
    }
    signature
}

fn native_details(command: &NativeCommand) -> String {
    if command.intents.is_empty() {
        return command.description.clone();
    }
    format!(
        "{}\n\nRelated intents: {}.",
        command.description,
        command.intents.join("; ")
    )
}

fn native_documentation(summary: &str, description: &str) -> String {
    if summary == description {
        summary.to_owned()
    } else {
        format!("{summary} {description}")
    }
}

fn validate_database_identity(
    connection: &Connection,
    source_name: &str,
) -> Result<(), NativeCatalogDiagnostic> {
    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|error| database_diagnostic(source_name, "read application id", error))?;
    let schema_version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| database_diagnostic(source_name, "read schema version", error))?;
    if application_id != NATIVE_DATABASE_APPLICATION_ID
        || schema_version != NATIVE_DATABASE_SCHEMA_VERSION
    {
        return Err(validation_diagnostic(
            source_name,
            None,
            "database has an incompatible application id or schema version",
            "Recompile it with this version of quirl-catalog",
        ));
    }
    let integrity: String = connection
        .query_row("PRAGMA quick_check(1)", [], |row| row.get(0))
        .map_err(|error| database_diagnostic(source_name, "check database integrity", error))?;
    if integrity != "ok" {
        return Err(validation_diagnostic(
            source_name,
            None,
            format!("database integrity check failed: {integrity}"),
            "Recompile the database from its authoritative KDL source",
        ));
    }
    Ok(())
}

fn parse_stored_action(
    value: Option<&str>,
) -> Result<Option<NativeCompletionAction>, NativeCatalogDiagnostic> {
    value
        .map(|value| {
            NativeCompletionAction::parse(value).ok_or_else(|| {
                validation_diagnostic(
                    "<native catalog database>",
                    None,
                    format!("database contains unknown completion action `{value}`"),
                    "Recompile the database from valid KDL source",
                )
            })
        })
        .transpose()
}

fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>>,
    limit: usize,
    label: &str,
) -> Result<Vec<T>, NativeCatalogDiagnostic> {
    let mut values = Vec::new();
    for row in rows {
        values.push(
            row.map_err(|error| database_diagnostic("<native catalog database>", label, error))?,
        );
        if values.len() > limit {
            return Err(resource_diagnostic(
                "<native catalog database>",
                label,
                limit,
                values.len(),
            ));
        }
    }
    Ok(values)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishStage {
    ContentSynced,
    BeforeRename,
}

struct CatalogTemporary {
    path: PathBuf,
    armed: bool,
}

impl CatalogTemporary {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CatalogTemporary {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Compile and atomically publish one native catalog image.
///
/// Parsing and validation should be completed before this function is called.
/// Compilation and staging complete before the destination rename, so every
/// reported pre-publication failure preserves a prior valid image. Staging files
/// are uniquely named, mode `0600` on Unix, and removed by an RAII owner.
pub fn publish_native_catalog(
    path: &Path,
    catalog: &NativeCatalog,
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    publish_native_catalog_with_hook(path, catalog, limits, |_| Ok(()))
}

/// Parse strict KDL, compile it in memory, and atomically publish the image.
///
/// Parse, validation, and SQLite failures occur before any staging file is
/// created, preserving an existing valid destination.
pub fn publish_native_catalog_source(
    path: &Path,
    source: &str,
    source_name: &str,
    limits: NativeCatalogLimits,
) -> Result<(), NativeCatalogDiagnostic> {
    let catalog = parse_native_catalog(source, source_name, limits)?;
    publish_native_catalog(path, &catalog, limits)
}

fn publish_native_catalog_with_hook(
    path: &Path,
    catalog: &NativeCatalog,
    limits: NativeCatalogLimits,
    mut hook: impl FnMut(PublishStage) -> io::Result<()>,
) -> Result<(), NativeCatalogDiagnostic> {
    let encoded = compile_native_catalog(catalog, limits)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|error| io_diagnostic("inspect parent directory", parent, error))?;
    if !parent_metadata.file_type().is_dir() {
        return Err(validation_diagnostic(
            &parent.display().to_string(),
            None,
            "native catalog parent is not a directory",
            "Choose an existing private directory",
        ));
    }
    validate_parent_permissions(parent, &parent_metadata)?;
    let previous = match fs::symlink_metadata(path) {
        Ok(_) => {
            let bytes = read_admitted_file(path, limits.database_bytes_max)?;
            NativeCatalogReader::from_bytes_named(&bytes, &path.display().to_string(), limits)?;
            Some(bytes)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(io_diagnostic("inspect destination", path, error)),
    };
    let (mut temporary, mut file) = create_temporary(path)?;
    file.write_all(&encoded)
        .and_then(|()| file.sync_all())
        .map_err(|error| io_diagnostic("write staging", &temporary.path, error))?;
    hook(PublishStage::ContentSynced)
        .map_err(|error| io_diagnostic("complete staged write", &temporary.path, error))?;
    let staged = read_admitted_file(&temporary.path, limits.database_bytes_max)?;
    if staged != encoded {
        return Err(validation_diagnostic(
            &temporary.path.display().to_string(),
            None,
            "staged native catalog bytes changed before publication",
            "Retry in a private directory without concurrent writers",
        ));
    }
    match &previous {
        Some(expected) => {
            let observed = read_admitted_file(path, limits.database_bytes_max)?;
            if &observed != expected {
                return Err(validation_diagnostic(
                    &path.display().to_string(),
                    None,
                    "native catalog destination changed during publication",
                    "Retry after the other writer has completed",
                ));
            }
        }
        None => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(validation_diagnostic(
                    &path.display().to_string(),
                    None,
                    "native catalog destination appeared during publication",
                    "Retry after the other writer has completed",
                ));
            }
            Err(error) => return Err(io_diagnostic("reinspect destination", path, error)),
        },
    }
    hook(PublishStage::BeforeRename)
        .map_err(|error| io_diagnostic("prepare atomic rename", &temporary.path, error))?;
    drop(file);
    fs::rename(&temporary.path, path)
        .map_err(|error| io_diagnostic("atomically replace destination", path, error))?;
    temporary.disarm();
    // Once rename succeeds the complete valid image is visible. Directory sync is
    // durability hardening; it cannot be made part of rollback without risking a
    // second failure that destroys the newly published valid image.
    let _ = File::open(parent).and_then(|directory| directory.sync_all());
    Ok(())
}

fn create_temporary(path: &Path) -> Result<(CatalogTemporary, File), NativeCatalogDiagnostic> {
    let file_name = path.file_name().ok_or_else(|| {
        validation_diagnostic(
            &path.display().to_string(),
            None,
            "native catalog destination has no file name",
            "Choose a concrete database file path",
        )
    })?;
    for _ in 0..TEMPORARY_ATTEMPTS_MAX {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_file_name(format!(
            ".{}.quirl-native-{}-{sequence}.tmp",
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
        match options.open(&temporary) {
            Ok(file) => return Ok((CatalogTemporary::new(temporary), file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_diagnostic("create staging", &temporary, error)),
        }
    }
    Err(resource_diagnostic(
        &path.display().to_string(),
        "temporary-name attempts",
        TEMPORARY_ATTEMPTS_MAX,
        TEMPORARY_ATTEMPTS_MAX,
    ))
}

fn read_admitted_file(path: &Path, bytes_max: usize) -> Result<Vec<u8>, NativeCatalogDiagnostic> {
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| io_diagnostic("inspect", path, error))?;
    validate_file_metadata(path, &path_metadata, bytes_max)?;
    let mut file = File::open(path).map_err(|error| io_diagnostic("open", path, error))?;
    let handle_metadata = file
        .metadata()
        .map_err(|error| io_diagnostic("inspect open handle", path, error))?;
    validate_file_metadata(path, &handle_metadata, bytes_max)?;
    validate_same_file(path, &path_metadata, &handle_metadata)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(
            u64::try_from(bytes_max)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)
        .map_err(|error| io_diagnostic("read", path, error))?;
    if bytes.len() > bytes_max {
        return Err(resource_diagnostic(
            &path.display().to_string(),
            "file bytes",
            bytes_max,
            bytes.len(),
        ));
    }
    let final_metadata =
        fs::symlink_metadata(path).map_err(|error| io_diagnostic("reinspect", path, error))?;
    validate_file_metadata(path, &final_metadata, bytes_max)?;
    validate_same_file(path, &path_metadata, &final_metadata)?;
    Ok(bytes)
}

fn validate_parent_permissions(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), NativeCatalogDiagnostic> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(validation_diagnostic(
                &path.display().to_string(),
                None,
                format!("native catalog parent has unsafe mode {mode:#o}"),
                "Remove group and other write permissions or choose a private directory",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (path, metadata);
    Ok(())
}

fn validate_file_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    bytes_max: usize,
) -> Result<(), NativeCatalogDiagnostic> {
    if !metadata.file_type().is_file() {
        return Err(validation_diagnostic(
            &path.display().to_string(),
            None,
            "native catalog input is not a regular file",
            "Use an unlinked regular file",
        ));
    }
    let observed = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if observed > bytes_max {
        return Err(resource_diagnostic(
            &path.display().to_string(),
            "file bytes",
            bytes_max,
            observed,
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(validation_diagnostic(
                &path.display().to_string(),
                None,
                format!("native catalog file has {} hard links", metadata.nlink()),
                "Use an unlinked regular file",
            ));
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(validation_diagnostic(
                &path.display().to_string(),
                None,
                format!("native catalog file has unsafe mode {mode:#o}"),
                "Remove group and other write permissions",
            ));
        }
    }
    Ok(())
}

fn validate_same_file(
    path: &Path,
    expected: &fs::Metadata,
    observed: &fs::Metadata,
) -> Result<(), NativeCatalogDiagnostic> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if expected.dev() != observed.dev()
            || expected.ino() != observed.ino()
            || expected.len() != observed.len()
            || expected.mtime() != observed.mtime()
            || expected.mtime_nsec() != observed.mtime_nsec()
        {
            return Err(validation_diagnostic(
                &path.display().to_string(),
                None,
                "native catalog path changed while it was being read",
                "Retry in a private directory without concurrent writers",
            ));
        }
    }
    #[cfg(not(unix))]
    {
        if expected.len() != observed.len()
            || expected.file_type() != observed.file_type()
            || expected.modified().ok() != observed.modified().ok()
        {
            return Err(validation_diagnostic(
                &path.display().to_string(),
                None,
                "native catalog path changed while it was being read",
                "Retry in a private directory without concurrent writers",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
catalog "native-tools" {
    provenance author="Quirl contributors" license="MIT" revision="release-2026-08" source="https://example.invalid/native-tools"
    command "git" summary="Manage repositories" description="Inspect and update distributed version-control repositories." {
        alias "g"
        intent "work with source history"
        platform "linux"
        platform "macos"
        flag "-C" summary="Select a working directory" description="Run as if started in this directory." value="directory" action="directories"
        flag "--platform-mode" summary="Use Linux mode" description="Use behavior defined for Linux hosts." {
            platform "linux"
        }
        flag "--platform-mode" summary="Use macOS mode" description="Use behavior defined for macOS hosts." {
            platform "macos"
        }
        flag "--verbose" short="-v" summary="Show detail" description="Print additional operation detail."
        argument "repository" summary="Repository directory" description="Select the repository to inspect." required=#true action="directories"
        command "commit" summary="Record changes" description="Create a new commit from staged changes." {
            intent "record staged changes"
            platform "macos"
            flag "--message" short="-m" summary="Commit message" description="Use the supplied commit message." value="message"
            argument "paths" summary="Paths to commit" description="Limit the commit to selected paths." repeatable=#true action="files"
        }
    }
    command "winutil" summary="Inspect Windows" description="Inspect platform-specific Windows state." {
        platform "windows"
        flag "/users" summary="List users" description="List bounded local user names."
    }
}
"#;

    fn fixture() -> NativeCatalog {
        parse_native_catalog(FIXTURE, "fixture.kdl", NativeCatalogLimits::default()).unwrap()
    }

    #[test]
    fn strict_kdl_builds_canonical_typed_tree() {
        let catalog = fixture();
        assert_eq!(catalog.name, "native-tools");
        assert_eq!(catalog.provenance.license, "MIT");
        assert_eq!(catalog.commands[0].name, "git");
        assert_eq!(
            catalog.commands[0].platforms,
            vec![NativePlatform::Linux, NativePlatform::Macos]
        );
        assert_eq!(
            catalog.commands[0].subcommands[0].platforms,
            vec![NativePlatform::Macos]
        );
        assert_eq!(catalog.commands[1].name, "winutil");
        assert_eq!(catalog.commands[0].flags[0].name, "--platform-mode");
        assert_eq!(
            catalog.commands[0].flags[0].platforms,
            [NativePlatform::Linux]
        );
        assert_eq!(catalog.commands[0].flags[1].name, "--platform-mode");
        assert_eq!(
            catalog.commands[0].flags[1].platforms,
            [NativePlatform::Macos]
        );
        assert_eq!(catalog.commands[0].flags[2].name, "--verbose");
        assert_eq!(catalog.commands[0].flags[3].name, "-C");
        assert_eq!(catalog.commands[1].flags[0].name, "/users");
        assert_eq!(
            catalog.commands[0].arguments[0].action,
            Some(NativeCompletionAction::Directories)
        );
    }

    #[test]
    fn syntax_diagnostic_retains_source_span_and_help() {
        let error = parse_native_catalog(
            "catalog \"broken\" {",
            "broken.kdl",
            NativeCatalogLimits::default(),
        )
        .unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::Syntax);
        assert_eq!(error.source_name, "broken.kdl");
        assert!(error.byte_offset.is_some());
        assert!(error.byte_length.is_some());
        assert!(!error.help.is_empty());
    }

    #[test]
    fn schema_rejects_unknown_duplicate_and_extra_input() {
        let cases = [
            (
                FIXTURE.replace("alias \"g\"", "mystery \"g\""),
                "unknown command child node",
            ),
            (
                FIXTURE.replace(
                    "summary=\"Manage repositories\"",
                    "summary=\"Manage repositories\" summary=\"again\"",
                ),
                "duplicate property `summary`",
            ),
            (
                FIXTURE.replace("alias \"g\"", "alias \"g\" \"extra\""),
                "expects 1 positional argument",
            ),
            (
                FIXTURE.replace("action=\"directories\"", "action=\"network_magic\""),
                "unknown native completion action",
            ),
            (
                FIXTURE.replace("platform \"windows\"", "platform \"plan9\""),
                "unknown platform",
            ),
        ];
        for (source, expected) in cases {
            let error = parse_native_catalog(&source, "strict.kdl", NativeCatalogLimits::default())
                .unwrap_err();
            assert!(
                error.message.contains(expected),
                "expected {expected:?}, observed {:?}",
                error.message
            );
            assert!(error.byte_offset.is_some());
        }
    }

    #[test]
    fn schema_rejects_duplicates_and_invalid_combinations() {
        let duplicate = FIXTURE.replace("alias \"g\"", "alias \"g\"\n        alias \"g\"");
        let error =
            parse_native_catalog(&duplicate, "duplicate.kdl", NativeCatalogLimits::default())
                .unwrap_err();
        assert!(error.message.contains("duplicate alias"));

        let action_on_boolean = FIXTURE.replace(
            "flag \"--verbose\" short=\"-v\" summary=\"Show detail\" description=\"Print additional operation detail.\"",
            "flag \"--verbose\" short=\"-v\" summary=\"Show detail\" description=\"Print additional operation detail.\" action=\"files\"",
        );
        let error = parse_native_catalog(
            &action_on_boolean,
            "combination.kdl",
            NativeCatalogLimits::default(),
        )
        .unwrap_err();
        assert!(error.message.contains("boolean flag"));

        let aliased_short_only =
            FIXTURE.replace("flag \"-C\" summary=", "flag \"-C\" short=\"-c\" summary=");
        let error = parse_native_catalog(
            &aliased_short_only,
            "short-only.kdl",
            NativeCatalogLimits::default(),
        )
        .unwrap_err();
        assert!(error.message.contains("short-only flag"));

        let impossible_platform = FIXTURE.replace(
            "platform \"macos\"\n            flag \"--message\"",
            "platform \"windows\"\n            flag \"--message\"",
        );
        let error = parse_native_catalog(
            &impossible_platform,
            "platform.kdl",
            NativeCatalogLimits::default(),
        )
        .unwrap_err();
        assert!(error.message.contains("do not overlap"));

        let overlapping_flags = FIXTURE.replace(
            "flag \"--platform-mode\" summary=\"Use macOS mode\" description=\"Use behavior defined for macOS hosts.\" {\n            platform \"macos\"",
            "flag \"--platform-mode\" summary=\"Use macOS mode\" description=\"Use behavior defined for macOS hosts.\" {\n            platform \"linux\"",
        );
        let error = parse_native_catalog(
            &overlapping_flags,
            "overlapping-flags.kdl",
            NativeCatalogLimits::default(),
        )
        .unwrap_err();
        assert!(
            error
                .message
                .contains("duplicate flag name `--platform-mode`")
        );
    }

    #[test]
    fn source_count_and_depth_limits_fail_closed() {
        let mut limits = NativeCatalogLimits {
            source_bytes_max: FIXTURE.len() - 1,
            ..NativeCatalogLimits::default()
        };
        let error = parse_native_catalog(FIXTURE, "large.kdl", limits).unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::ResourceLimit);
        assert_eq!(error.context[0], format!("limit: {}", FIXTURE.len() - 1));

        limits = NativeCatalogLimits {
            command_count_max: 2,
            ..NativeCatalogLimits::default()
        };
        let error = parse_native_catalog(FIXTURE, "commands.kdl", limits).unwrap_err();
        assert!(error.message.contains("command count"));

        limits = NativeCatalogLimits {
            command_depth_max: 1,
            ..NativeCatalogLimits::default()
        };
        let error = parse_native_catalog(FIXTURE, "depth.kdl", limits).unwrap_err();
        assert!(error.message.contains("command depth"));
        assert!(error.byte_offset.is_some());
    }

    #[test]
    fn embedded_profile_rejects_one_command_beyond_its_runtime_boundary() {
        let mut catalog = fixture();
        let template = catalog.commands[1].clone();
        catalog.commands = (0..=NativeCatalogLimits::embedded().command_count_max)
            .map(|index| {
                let mut command = template.clone();
                command.name = format!("command-{index}");
                command
            })
            .collect();
        let error = compile_native_catalog(&catalog, NativeCatalogLimits::embedded()).unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::ResourceLimit);
        assert!(error.message.contains("command count"));
        assert_eq!(error.context[0], "limit: 2048");
        assert_eq!(error.context[1], "observed: 2049");
    }

    #[test]
    fn compiler_bytes_and_normalized_rows_are_deterministic() {
        let catalog = fixture();
        let limits = NativeCatalogLimits::default();
        let first = compile_native_catalog(&catalog, limits).unwrap();
        let second = compile_native_catalog(&catalog, limits).unwrap();
        assert_eq!(first, second);
        let reader = NativeCatalogReader::from_bytes(&first, limits).unwrap();
        assert_eq!(reader.snapshot(), &catalog);
        let rows = reader
            .connection
            .prepare(
                "SELECT command_id, parent_id, full_path, depth FROM commands ORDER BY command_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, None, "git".to_owned(), 1),
                (2, Some(1), "git commit".to_owned(), 2),
                (3, None, "winutil".to_owned(), 1),
            ]
        );
        let application_id: i64 = reader
            .connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .unwrap();
        let version: i64 = reader
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(application_id, NATIVE_DATABASE_APPLICATION_ID);
        assert_eq!(version, NATIVE_DATABASE_SCHEMA_VERSION);
    }

    #[test]
    fn reader_separates_platforms_and_resolves_nested_projections() {
        let catalog = fixture();
        let limits = NativeCatalogLimits::default();
        let bytes = compile_native_catalog(&catalog, limits).unwrap();
        let reader = NativeCatalogReader::from_bytes(&bytes, limits).unwrap();

        let linux = reader
            .subcommands("", NativePlatform::Linux, "", 10)
            .unwrap();
        assert_eq!(
            linux
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>(),
            vec!["g", "git"]
        );
        let windows = reader
            .subcommands("", NativePlatform::Windows, "", 10)
            .unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].value, "winutil");

        assert!(
            reader
                .subcommands("git", NativePlatform::Linux, "", 10)
                .unwrap()
                .is_empty()
        );
        let nested = reader
            .subcommands("git", NativePlatform::Macos, "co", 10)
            .unwrap();
        assert_eq!(nested[0].value, "commit");
        let flags = reader
            .flags("git commit", NativePlatform::Macos, "--m", 10)
            .unwrap();
        assert_eq!(flags[0].value, "--message");
        let short_only = reader
            .flags("git", NativePlatform::Linux, "-C", 10)
            .unwrap();
        assert_eq!(short_only[0].value, "-C");
        let linux_mode = reader
            .flags("git", NativePlatform::Linux, "--platform", 10)
            .unwrap();
        assert_eq!(linux_mode.len(), 1);
        assert_eq!(linux_mode[0].summary, "Use Linux mode");
        let macos_mode = reader
            .flags("git", NativePlatform::Macos, "--platform", 10)
            .unwrap();
        assert_eq!(macos_mode.len(), 1);
        assert_eq!(macos_mode[0].summary, "Use macOS mode");
        let windows_flag = reader
            .flags("winutil", NativePlatform::Windows, "/", 10)
            .unwrap();
        assert_eq!(windows_flag[0].value, "/users");
        assert!(
            reader
                .flags("winutil", NativePlatform::Linux, "/", 10)
                .unwrap()
                .is_empty()
        );
        let arguments = reader
            .arguments("git", NativePlatform::Linux, "repo", 10)
            .unwrap();
        assert_eq!(arguments[0].value, "repository");
        assert_eq!(
            arguments[0].action,
            Some(NativeCompletionAction::Directories)
        );

        let all_platforms = reader.project_commands(NativePlatform::Any);
        assert!(
            all_platforms
                .iter()
                .any(|command| command.path == "git commit")
        );
        assert!(
            all_platforms
                .iter()
                .any(|command| command.path == "winutil")
        );
    }

    #[test]
    fn semantic_lookup_is_bounded_platform_filtered_and_deterministic() {
        let limits = NativeCatalogLimits::default();
        let bytes = compile_native_catalog(&fixture(), limits).unwrap();
        let reader = NativeCatalogReader::from_bytes(&bytes, limits).unwrap();
        let macos = reader
            .semantic_lookup("record staged changes", NativePlatform::Macos, 10)
            .unwrap();
        assert_eq!(macos[0].command_path, "git commit");
        assert_eq!(macos[0].score, 3);
        let linux = reader
            .semantic_lookup("record staged changes", NativePlatform::Linux, 10)
            .unwrap();
        assert!(linux.iter().all(|hit| hit.command_path != "git commit"));
        let linux_flag = reader
            .semantic_lookup("Linux", NativePlatform::Linux, 10)
            .unwrap();
        assert!(linux_flag.iter().any(|hit| hit.target == "--platform-mode"));
        let macos_flag = reader
            .semantic_lookup("Linux", NativePlatform::Macos, 10)
            .unwrap();
        assert!(macos_flag.iter().all(|hit| hit.target != "--platform-mode"));
        let error = reader
            .semantic_lookup("query", NativePlatform::Any, limits.query_results_max + 1)
            .unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::ResourceLimit);
    }

    #[test]
    fn reader_rejects_projection_tampering_against_exact_snapshot() {
        let limits = NativeCatalogLimits::default();
        let bytes = compile_native_catalog(&fixture(), limits).unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .deserialize_read_exact(MAIN_DB, Cursor::new(&bytes), bytes.len(), false)
            .unwrap();
        connection
            .execute(
                "UPDATE commands SET summary = 'tampered' WHERE full_path = 'git'",
                [],
            )
            .unwrap();
        let tampered = connection.serialize(MAIN_DB).unwrap().to_vec();
        let error = NativeCatalogReader::from_bytes(&tampered, limits).unwrap_err();
        assert!(error.message.contains("do not exactly match"));
    }

    #[test]
    fn atomic_failure_preserves_previous_image_and_cleans_staging() {
        let directory = TestDirectory::new("atomic-failure");
        let path = directory.path.join("catalog.sqlite3");
        let limits = NativeCatalogLimits::default();
        let original = fixture();
        publish_native_catalog(&path, &original, limits).unwrap();
        let original_bytes = fs::read(&path).unwrap();

        let mut replacement = original.clone();
        replacement.provenance.revision = "replacement".to_owned();
        let error = publish_native_catalog_with_hook(&path, &replacement, limits, |stage| {
            if stage == PublishStage::BeforeRename {
                Err(io::Error::other("injected publication failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::Io);
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert_eq!(
            NativeCatalogReader::open(&path, limits).unwrap().snapshot(),
            &original
        );
        let entries = fs::read_dir(&directory.path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from("catalog.sqlite3")]);
    }

    #[test]
    fn validation_failure_never_stages_or_replaces_a_valid_image() {
        let directory = TestDirectory::new("validation-failure");
        let path = directory.path.join("catalog.sqlite3");
        let limits = NativeCatalogLimits::default();
        publish_native_catalog(&path, &fixture(), limits).unwrap();
        let original_bytes = fs::read(&path).unwrap();
        let mut invalid = fixture();
        invalid.commands[0].flags[0].action = Some(NativeCompletionAction::Files);
        let error = publish_native_catalog(&path, &invalid, limits).unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::Validation);
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert_eq!(fs::read_dir(&directory.path).unwrap().count(), 1);
    }

    #[test]
    fn source_and_compilation_failures_precede_filesystem_staging() {
        let directory = TestDirectory::new("prepublication-failure");
        let path = directory.path.join("catalog.sqlite3");
        let limits = NativeCatalogLimits::default();
        publish_native_catalog_source(&path, FIXTURE, "fixture.kdl", limits).unwrap();
        let original_bytes = fs::read(&path).unwrap();

        let error =
            publish_native_catalog_source(&path, "catalog \"broken\" {", "broken.kdl", limits)
                .unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::Syntax);
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert_eq!(fs::read_dir(&directory.path).unwrap().count(), 1);

        let tiny_database = NativeCatalogLimits {
            database_bytes_max: 1,
            ..limits
        };
        let error = publish_native_catalog_source(&path, FIXTURE, "fixture.kdl", tiny_database)
            .unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::ResourceLimit);
        assert_eq!(fs::read(&path).unwrap(), original_bytes);
        assert_eq!(fs::read_dir(&directory.path).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn publication_rejects_a_shared_writable_parent() {
        use std::os::unix::fs::PermissionsExt;

        let directory = TestDirectory::new("unsafe-parent");
        fs::set_permissions(&directory.path, fs::Permissions::from_mode(0o777)).unwrap();
        let path = directory.path.join("catalog.sqlite3");
        let error =
            publish_native_catalog(&path, &fixture(), NativeCatalogLimits::default()).unwrap_err();
        assert_eq!(error.kind, NativeDiagnosticKind::Validation);
        assert!(error.message.contains("unsafe mode"));
        assert!(fs::read_dir(&directory.path).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn file_identity_detects_in_place_metadata_changes_and_hard_links() {
        let directory = TestDirectory::new("metadata-change");
        let path = directory.path.join("catalog.sqlite3");
        fs::write(&path, b"before").unwrap();
        let before = fs::metadata(&path).unwrap();
        fs::write(&path, b"after-change").unwrap();
        let after = fs::metadata(&path).unwrap();
        let error = validate_same_file(&path, &before, &after).unwrap_err();
        assert!(error.message.contains("changed while it was being read"));

        let alias = directory.path.join("alias.sqlite3");
        fs::hard_link(&path, &alias).unwrap();
        let linked = fs::metadata(&path).unwrap();
        let error = validate_file_metadata(&path, &linked, 1024).unwrap_err();
        assert!(error.message.contains("hard links"));
    }

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quirl-catalog-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }
}
