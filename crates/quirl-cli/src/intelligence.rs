//! Bounded local command intelligence backed by SQLite and Model2Vec.

use model2vec_rs::model::StaticModel;
use quirl_catalog::{ArgumentKind, Catalog, CompletionSource, Confidence, ProvenanceInfo, Trust};
use quirl_core::{ErrorCode, ShellError};
use rusqlite::{Connection, MAIN_DB, Transaction, limits::Limit, params, serialize::Data};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    env,
    io::Cursor,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
};

pub(crate) const DATABASE_BYTES_MAX: usize = 128 * 1024 * 1024;
pub(crate) const QUERY_BYTES_MAX: usize = 4 * 1024;
pub(crate) const SEARCH_RESULTS_MAX: usize = 100;
const DATABASE_SCHEMA_VERSION: i64 = 2;
const DATABASE_APPLICATION_ID: i64 = 0x5155_4952;
const DOCUMENTS_MAX: usize = 65_536;
const DOCUMENT_BYTES_MAX: usize = 16 * 1024;
const EMBEDDING_DIMENSIONS_MAX: usize = 2_048;
const EMBEDDING_BATCH_SIZE: usize = 256;
pub(crate) const AUTOMATIC_EMBEDDING_BATCH_SIZE: usize = 32;
const MODEL_ID: &str = "minishlab/potion-base-8M";
const MODEL_CONFIG_BYTES_MAX: u64 = 1024 * 1024;
const MODEL_TOKENIZER_BYTES_MAX: u64 = 4 * 1024 * 1024;
const MODEL_WEIGHTS_BYTES_MAX: u64 = 64 * 1024 * 1024;

const LOCAL_OVERLAY_SCHEMA_VERSION: u32 = 1;
const LOCAL_RECORDS_MAX: usize = 4_096;
const LOCAL_NEGATIVE_HITS_MAX: usize = 1_024;
const LOCAL_OVERLAY_QUERIES_MAX: usize = 8_192;
const LOCAL_PATH_DEPTH_MAX: usize = 16;
const LOCAL_SEGMENT_BYTES_MAX: usize = 256;
const LOCAL_TEXT_BYTES_MAX: usize = 4 * 1024;
const LOCAL_FINGERPRINT_BYTES_MAX: usize = 256;
const LOCAL_RETAINED_BYTES_MAX: usize = 4 * 1024 * 1024;
const LOCAL_NEGATIVE_BACKOFF_BASE_MS: u64 = 1_000;
const LOCAL_NEGATIVE_BACKOFF_MAX_MS: u64 = 5 * 60 * 1_000;
const LOCAL_NEGATIVE_EXPIRY_MS: u64 = 60 * 60 * 1_000;

const SCHEMA: &str = r#"
PRAGMA application_id = 1364543826;
PRAGMA user_version = 2;
CREATE TABLE metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) WITHOUT ROWID;
CREATE TABLE catalog_snapshot (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    catalog_json BLOB NOT NULL
);
CREATE TABLE commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    path TEXT NOT NULL,
    version TEXT,
    parent_id TEXT,
    signature TEXT NOT NULL,
    summary TEXT NOT NULL,
    details TEXT NOT NULL,
    input_type TEXT NOT NULL,
    output_type TEXT NOT NULL,
    streaming INTEGER NOT NULL CHECK (streaming IN (0, 1)),
    provenance_json TEXT NOT NULL
) WITHOUT ROWID;
CREATE UNIQUE INDEX commands_path ON commands(path);
CREATE TABLE aliases (
    command_id TEXT NOT NULL,
    alias TEXT NOT NULL,
    PRIMARY KEY (command_id, alias)
) WITHOUT ROWID;
CREATE TABLE arguments (
    argument_id TEXT PRIMARY KEY NOT NULL,
    command_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    kind TEXT NOT NULL,
    value_type TEXT NOT NULL,
    required INTEGER NOT NULL CHECK (required IN (0, 1)),
    repeatable INTEGER NOT NULL CHECK (repeatable IN (0, 1)),
    documentation TEXT NOT NULL,
    dynamic_provider TEXT,
    provenance_json TEXT NOT NULL,
    UNIQUE (command_id, ordinal)
) WITHOUT ROWID;
CREATE TABLE argument_names (
    argument_id TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (argument_id, name)
) WITHOUT ROWID;
CREATE TABLE argument_values (
    argument_id TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (argument_id, value)
) WITHOUT ROWID;
CREATE TABLE argument_conflicts (
    argument_id TEXT NOT NULL,
    conflicting_name TEXT NOT NULL,
    PRIMARY KEY (argument_id, conflicting_name)
) WITHOUT ROWID;
CREATE TABLE examples (
    owner_kind TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    example TEXT NOT NULL,
    PRIMARY KEY (owner_kind, owner_id, ordinal)
) WITHOUT ROWID;
CREATE TABLE effects (
    command_id TEXT NOT NULL,
    effect TEXT NOT NULL,
    PRIMARY KEY (command_id, effect)
) WITHOUT ROWID;
CREATE TABLE exit_codes (
    command_id TEXT NOT NULL,
    status INTEGER NOT NULL,
    meaning TEXT NOT NULL,
    PRIMARY KEY (command_id, status)
) WITHOUT ROWID;
CREATE TABLE semantic_documents (
    document_id TEXT PRIMARY KEY NOT NULL,
    document_kind TEXT NOT NULL,
    command_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    fingerprint TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX semantic_documents_command ON semantic_documents(command_id);
CREATE TABLE embeddings (
    document_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    dimensions INTEGER NOT NULL,
    vector_le_f32 BLOB NOT NULL,
    document_fingerprint TEXT NOT NULL,
    PRIMARY KEY (document_id, model_id)
) WITHOUT ROWID;
CREATE TABLE local_overlay_identity (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    schema_version INTEGER NOT NULL,
    native_catalog_fingerprint TEXT NOT NULL
);
CREATE TABLE local_command_paths (
    path_key TEXT PRIMARY KEY NOT NULL,
    segment_count INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE local_command_path_segments (
    path_key TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    segment TEXT NOT NULL,
    PRIMARY KEY (path_key, ordinal)
) WITHOUT ROWID;
CREATE TABLE local_completion_records (
    record_key TEXT PRIMARY KEY NOT NULL,
    path_key TEXT NOT NULL,
    candidate_kind TEXT NOT NULL,
    insertion_text TEXT NOT NULL,
    display_text TEXT NOT NULL,
    description TEXT,
    provider TEXT NOT NULL,
    confidence TEXT NOT NULL,
    trust TEXT NOT NULL,
    executable_fingerprint TEXT NOT NULL,
    provider_fingerprint TEXT NOT NULL,
    cwd_class TEXT NOT NULL,
    environment_fingerprint TEXT NOT NULL,
    observed_unix_ms INTEGER NOT NULL,
    refreshed_unix_ms INTEGER NOT NULL,
    refresh_state TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX local_completion_records_path ON local_completion_records(path_key);
CREATE TABLE local_negative_hits (
    negative_key TEXT PRIMARY KEY NOT NULL,
    path_key TEXT NOT NULL,
    provider TEXT NOT NULL,
    executable_fingerprint TEXT NOT NULL,
    provider_fingerprint TEXT NOT NULL,
    cwd_class TEXT NOT NULL,
    environment_fingerprint TEXT NOT NULL,
    failure_count INTEGER NOT NULL,
    observed_unix_ms INTEGER NOT NULL,
    retry_after_unix_ms INTEGER NOT NULL,
    expires_unix_ms INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX local_negative_hits_path ON local_negative_hits(path_key);
"#;

/// Normalized kind of one candidate retained from a local completion provider.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalCandidateKind {
    /// A child command appended to the owning command path.
    Subcommand,
    /// An option name beginning with `-`.
    Flag,
    /// A value inserted without changing the owning command path.
    Value,
}

/// Declarative shell format that produced one local completion observation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalCompletionProvider {
    /// Fish completion metadata.
    Fish,
    /// Bash completion metadata.
    Bash,
    /// Zsh completion metadata.
    Zsh,
}

/// Coarse working-directory scope used in a local completion cache key.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalCwdClass {
    /// Provider output is independent of the working directory.
    Any,
    /// Provider output applies to a directory outside a recognized repository.
    Directory,
    /// Provider output applies inside a recognized repository.
    Repository,
}

/// Explicit lifecycle state for retained local completion metadata.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LocalRefreshState {
    /// The provider completed and the record may be offered.
    Fresh,
    /// The record remains attributable but needs a bounded refresh before use.
    Stale,
}

/// One validated, flattened local completion fact.
///
/// The command path is stored relationally as ordered segments. Fingerprints
/// and scope fields form the cache identity; fixed-width timestamps are caller
/// supplied so persistence and tests never consult ambient wall-clock time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalCompletionRecord {
    pub(crate) command_path: Vec<String>,
    pub(crate) kind: LocalCandidateKind,
    pub(crate) insertion_text: String,
    pub(crate) display_text: String,
    pub(crate) description: Option<String>,
    pub(crate) provider: LocalCompletionProvider,
    pub(crate) confidence: Confidence,
    pub(crate) trust: Trust,
    pub(crate) executable_fingerprint: String,
    pub(crate) provider_fingerprint: String,
    pub(crate) cwd_class: LocalCwdClass,
    pub(crate) environment_fingerprint: String,
    pub(crate) observed_unix_ms: u64,
    pub(crate) refreshed_unix_ms: u64,
    pub(crate) refresh_state: LocalRefreshState,
}

/// Identity and deterministic time supplied when reading a local overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalOverlayQuery {
    pub(crate) native_catalog_fingerprint: String,
    pub(crate) executable_fingerprint: String,
    pub(crate) provider_fingerprint: String,
    pub(crate) cwd_class: LocalCwdClass,
    pub(crate) environment_fingerprint: String,
    pub(crate) now_unix_ms: u64,
}

/// One bounded negative provider observation with exponential retry state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalNegativeHit {
    pub(crate) command_path: Vec<String>,
    pub(crate) provider: LocalCompletionProvider,
    pub(crate) executable_fingerprint: String,
    pub(crate) provider_fingerprint: String,
    pub(crate) cwd_class: LocalCwdClass,
    pub(crate) environment_fingerprint: String,
    pub(crate) failure_count: u32,
    pub(crate) observed_unix_ms: u64,
    pub(crate) retry_after_unix_ms: u64,
    pub(crate) expires_unix_ms: u64,
}

/// One provider miss supplied to the deterministic negative-cache transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalNegativeObservation {
    pub(crate) command_path: Vec<String>,
    pub(crate) provider: LocalCompletionProvider,
    pub(crate) executable_fingerprint: String,
    pub(crate) provider_fingerprint: String,
    pub(crate) cwd_class: LocalCwdClass,
    pub(crate) environment_fingerprint: String,
    pub(crate) observed_unix_ms: u64,
}

/// Validated local facts selected for one exact executable/provider context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalOverlay {
    pub(crate) records: Vec<LocalCompletionRecord>,
    pub(crate) negative_hits: Vec<LocalNegativeHit>,
}

/// Ordered source tier for command-intelligence composition.
///
/// Declaration order is the required precedence: curated KDL/native and
/// authoritative builtin/plugin facts, central imports, local metadata, help,
/// manual pages, then PATH-only discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CompletionCompositionTier {
    /// Curated native KDL and current builtin/plugin contracts.
    Curated,
    /// Centrally imported declarative completion catalogs.
    CentralImported,
    /// Identity-validated local completion metadata.
    LocalCompletion,
    /// Parsed `--help` output.
    Help,
    /// Parsed manual-page output.
    Man,
    /// Executable presence observed on `PATH` without other metadata.
    PathOnly,
}

/// One description field with independent attribution and source precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionDescriptionFact {
    pub(crate) text: String,
    pub(crate) provenance: ProvenanceInfo,
    pub(crate) tier: CompletionCompositionTier,
}

/// One ranked command or option returned by local command intelligence.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct SearchResult {
    pub(crate) command: String,
    pub(crate) target: String,
    pub(crate) kind: String,
    pub(crate) summary: String,
    pub(crate) score: f32,
    pub(crate) semantic: bool,
}

/// Outcome of building the persistent embedding index.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct EmbeddingReport {
    pub(crate) model: String,
    pub(crate) documents: usize,
    pub(crate) dimensions: usize,
}

/// Bounded row counts used to report database readiness.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct DatabaseStats {
    pub(crate) commands: usize,
    pub(crate) arguments: usize,
    pub(crate) documents: usize,
    pub(crate) embeddings: usize,
}

#[derive(Debug)]
struct SemanticDocument {
    id: String,
    kind: String,
    command: String,
    target: String,
    title: String,
    body: String,
    fingerprint: String,
}

#[derive(Debug)]
struct StoredEmbedding {
    document: SemanticDocument,
    vector: Vec<f32>,
}

/// In-memory, bounded search state reused across interactive AI-mode edits.
///
/// Database rows and the optional local model are validated once when the
/// session is opened. Each query performs bounded CPU work without rereading
/// SQLite or reloading the roughly 30 MB model.
pub(crate) struct SearchSession {
    documents: Vec<SemanticDocument>,
    embeddings: Vec<StoredEmbedding>,
    model: Option<StaticModel>,
}

impl SearchSession {
    /// Validate and materialize one immutable database generation.
    pub(crate) fn open(
        bytes: &[u8],
        path: &Path,
        model_path: Option<&Path>,
    ) -> Result<Self, ShellError> {
        let connection = deserialize_database(bytes, path)?;
        validate_schema(&connection, path)?;
        let documents = read_documents(&connection, path)?;
        let embeddings = read_embeddings(&connection, path)?;
        let model = if embeddings.is_empty() {
            None
        } else if let Some(model_path) = model_path.filter(|path| model_is_installed(path)) {
            Some(load_model(model_path)?)
        } else {
            None
        };
        Ok(Self {
            documents,
            embeddings,
            model,
        })
    }

    /// Rank commands and options for one bounded natural-language query.
    pub(crate) fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, ShellError> {
        validate_query(query, limit)?;
        if let Some(model) = &self.model {
            let query_vector = catch_unwind(AssertUnwindSafe(|| model.encode_single(query)))
                .map_err(|_| {
                    ShellError::new(
                        ErrorCode::Validation,
                        "the local Model2Vec tokenizer failed",
                    )
                    .with_help("Replace the model files with an intact potion-base-8M release")
                })?;
            validate_dimensions(query_vector.len())?;
            let mut ranked = self
                .embeddings
                .iter()
                .filter(|embedding| embedding.vector.len() == query_vector.len())
                .map(|embedding| SearchResult {
                    command: embedding.document.command.clone(),
                    target: embedding.document.target.clone(),
                    kind: embedding.document.kind.clone(),
                    summary: embedding.document.title.clone(),
                    score: cosine_similarity(&query_vector, &embedding.vector),
                    semantic: true,
                })
                .collect::<Vec<_>>();
            sort_and_limit(&mut ranked, limit);
            if !ranked.is_empty() {
                return Ok(ranked);
            }
        }

        let query_terms = query
            .split(|character: char| !character.is_alphanumeric())
            .filter(|term| !term.is_empty())
            .map(str::to_lowercase)
            .take(64)
            .collect::<Vec<_>>();
        Ok(rank_lexical_documents(&self.documents, &query_terms, limit))
    }
}

pub(crate) fn default_model_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("QUIRL_MODEL_PATH") {
        return Some(PathBuf::from(path));
    }
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .map(|data| data.join("quirl/models/potion-base-8M"))
}

pub(crate) fn model_is_installed(path: &Path) -> bool {
    crate::ai_bootstrap::validate_pinned_model(path).is_ok()
}

pub(crate) fn database_stats(bytes: &[u8], path: &Path) -> Result<DatabaseStats, ShellError> {
    let connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    Ok(DatabaseStats {
        commands: count_rows(&connection, path, "commands")?,
        arguments: count_rows(&connection, path, "arguments")?,
        documents: count_rows(&connection, path, "semantic_documents")?,
        embeddings: count_rows(&connection, path, "embeddings")?,
    })
}

pub(crate) fn embeddings_are_current(bytes: &[u8], path: &Path) -> Result<bool, ShellError> {
    let connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let documents = count_rows(&connection, path, "semantic_documents")?;
    let current = connection
        .query_row(
            "SELECT count(*) FROM semantic_documents d JOIN embeddings e ON e.document_id = d.document_id AND e.model_id = ?1 AND e.document_fingerprint = d.fingerprint",
            params![MODEL_ID],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| invalid_database(path, error))?;
    let current = usize::try_from(current).map_err(|_| {
        ShellError::new(
            ErrorCode::Validation,
            "the command database contains an invalid current-embedding count",
        )
        .with_help("Rebuild the local command database")
    })?;
    Ok(current == documents)
}

#[cfg(debug_assertions)]
pub(crate) fn mark_embeddings_current_for_test(
    bytes: &[u8],
    path: &Path,
) -> Result<Vec<u8>, ShellError> {
    let connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    connection
        .execute("DELETE FROM embeddings", [])
        .map_err(database_error)?;
    connection
        .execute(
            "INSERT INTO embeddings(document_id, model_id, dimensions, vector_le_f32, document_fingerprint) SELECT document_id, ?1, 1, x'00000000', fingerprint FROM semantic_documents",
            params![MODEL_ID],
        )
        .map_err(database_error)?;
    serialize_database(&connection)
}

pub(crate) fn encode_database(
    catalog: &Catalog,
    discovery_state_json: Option<&str>,
) -> Result<Vec<u8>, ShellError> {
    let catalog_json = serde_json::to_vec(catalog).map_err(json_error)?;
    if catalog_json.len() > DATABASE_BYTES_MAX {
        return Err(resource_limit(
            "catalog snapshot bytes",
            DATABASE_BYTES_MAX,
            catalog_json.len(),
        ));
    }
    let mut connection = Connection::open_in_memory().map_err(database_error)?;
    configure_connection(&connection)?;
    connection.execute_batch(SCHEMA).map_err(database_error)?;
    let transaction = connection.transaction().map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO catalog_snapshot(singleton, catalog_json) VALUES (1, ?1)",
            params![catalog_json],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO metadata(key, value) VALUES ('catalog_schema_version', ?1)",
            params![catalog.schema_version.to_string()],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO local_overlay_identity(singleton, schema_version, native_catalog_fingerprint) VALUES (1, ?1, ?2)",
            params![
                i64::from(LOCAL_OVERLAY_SCHEMA_VERSION),
                crate::native_catalog::embedded_database_identity()
            ],
        )
        .map_err(database_error)?;
    if let Some(state) = discovery_state_json {
        transaction
            .execute(
                "INSERT INTO metadata(key, value) VALUES ('discovery_state', ?1)",
                params![state],
            )
            .map_err(database_error)?;
    }
    insert_catalog(&transaction, catalog)?;
    transaction.commit().map_err(database_error)?;
    serialize_database(&connection)
}

pub(crate) fn decode_database(
    bytes: &[u8],
    path: &Path,
) -> Result<(Catalog, Option<String>), ShellError> {
    let connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let catalog_json: Vec<u8> = connection
        .query_row(
            "SELECT catalog_json FROM catalog_snapshot WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| invalid_database(path, error))?;
    let catalog: Catalog = serde_json::from_slice(&catalog_json).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("{} contains an invalid catalog snapshot", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Rebuild it with `quirl index build`")
    })?;
    if catalog.schema_version != Catalog::builtin().schema_version {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "{} uses catalog schema {}, but this Quirl expects {}",
                path.display(),
                catalog.schema_version,
                Catalog::builtin().schema_version
            ),
        )
        .with_help("Rebuild it with `quirl index build`"));
    }
    let state = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'discovery_state'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| invalid_database(path, error))?;
    Ok((catalog, state))
}

/// Read local completion facts for one exact runtime identity.
///
/// A native-catalog mismatch invalidates the whole overlay. Executable,
/// provider, directory-class, and environment mismatches invalidate individual
/// rows. Expired negative hits and stale candidates are not returned.
pub(crate) fn read_local_overlay(
    bytes: &[u8],
    path: &Path,
    query: &LocalOverlayQuery,
) -> Result<LocalOverlay, ShellError> {
    validate_fingerprint(
        "native catalog fingerprint",
        &query.native_catalog_fingerprint,
    )?;
    validate_fingerprint("executable fingerprint", &query.executable_fingerprint)?;
    validate_fingerprint("provider fingerprint", &query.provider_fingerprint)?;
    validate_fingerprint("environment fingerprint", &query.environment_fingerprint)?;
    validate_persisted_u64("overlay query timestamp", query.now_unix_ms)?;
    let connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let stored = read_local_overlay_unfiltered(&connection, path)?;
    if stored.native_catalog_fingerprint != query.native_catalog_fingerprint {
        return Ok(LocalOverlay {
            records: Vec::new(),
            negative_hits: Vec::new(),
        });
    }
    let records = stored
        .records
        .into_iter()
        .filter(|record| {
            record.refresh_state == LocalRefreshState::Fresh
                && local_identity_matches(
                    &record.executable_fingerprint,
                    &record.provider_fingerprint,
                    record.cwd_class,
                    &record.environment_fingerprint,
                    query,
                )
        })
        .collect();
    let negative_hits = stored
        .negative_hits
        .into_iter()
        .filter(|hit| {
            hit.expires_unix_ms > query.now_unix_ms
                && local_identity_matches(
                    &hit.executable_fingerprint,
                    &hit.provider_fingerprint,
                    hit.cwd_class,
                    &hit.environment_fingerprint,
                    query,
                )
        })
        .collect();
    Ok(LocalOverlay {
        records,
        negative_hits,
    })
}

/// Read identity-valid overlay facts for a bounded set of runtime identities.
///
/// The single SQLite admission avoids reopening and deserializing the database
/// once per PATH executable. Duplicate identities and records are coalesced in
/// deterministic order before returning.
pub(crate) fn read_local_overlays(
    bytes: &[u8],
    path: &Path,
    queries: &[LocalOverlayQuery],
) -> Result<LocalOverlay, ShellError> {
    if queries.len() > LOCAL_OVERLAY_QUERIES_MAX {
        return Err(resource_limit(
            "local overlay identity queries",
            LOCAL_OVERLAY_QUERIES_MAX,
            queries.len(),
        ));
    }
    let mut identities = BTreeMap::new();
    for query in queries {
        validate_fingerprint(
            "native catalog fingerprint",
            &query.native_catalog_fingerprint,
        )?;
        validate_fingerprint("executable fingerprint", &query.executable_fingerprint)?;
        validate_fingerprint("provider fingerprint", &query.provider_fingerprint)?;
        validate_fingerprint("environment fingerprint", &query.environment_fingerprint)?;
        validate_persisted_u64("overlay query timestamp", query.now_unix_ms)?;
        identities.insert(
            (
                query.executable_fingerprint.clone(),
                query.provider_fingerprint.clone(),
                query.cwd_class,
                query.environment_fingerprint.clone(),
            ),
            query.now_unix_ms,
        );
    }
    let connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let stored = read_local_overlay_unfiltered(&connection, path)?;
    if queries
        .first()
        .is_none_or(|query| query.native_catalog_fingerprint != stored.native_catalog_fingerprint)
        || queries
            .iter()
            .any(|query| query.native_catalog_fingerprint != stored.native_catalog_fingerprint)
    {
        return Ok(LocalOverlay {
            records: Vec::new(),
            negative_hits: Vec::new(),
        });
    }
    let mut records = stored
        .records
        .into_iter()
        .filter(|record| {
            record.refresh_state == LocalRefreshState::Fresh
                && identities.contains_key(&(
                    record.executable_fingerprint.clone(),
                    record.provider_fingerprint.clone(),
                    record.cwd_class,
                    record.environment_fingerprint.clone(),
                ))
        })
        .collect::<Vec<_>>();
    sort_local_records(&mut records);
    records.dedup();
    let mut negative_hits = stored
        .negative_hits
        .into_iter()
        .filter(|hit| {
            identities
                .get(&(
                    hit.executable_fingerprint.clone(),
                    hit.provider_fingerprint.clone(),
                    hit.cwd_class,
                    hit.environment_fingerprint.clone(),
                ))
                .is_some_and(|now_unix_ms| hit.expires_unix_ms > *now_unix_ms)
        })
        .collect::<Vec<_>>();
    negative_hits.sort();
    negative_hits.dedup();
    Ok(LocalOverlay {
        records,
        negative_hits,
    })
}

/// Copy a valid local overlay into a newly encoded catalog generation.
///
/// Catalog rebuilds create a fresh SQLite image. This transition preserves the
/// independently identity-validated overlay only when both images use the same
/// native-catalog identity; malformed prior bytes remain an operating error.
pub(crate) fn preserve_local_overlay(
    prior_bytes: &[u8],
    prior_path: &Path,
    new_bytes: &[u8],
    new_path: &Path,
) -> Result<Vec<u8>, ShellError> {
    let prior_connection = deserialize_database(prior_bytes, prior_path)?;
    validate_schema(&prior_connection, prior_path)?;
    let stored = read_local_overlay_unfiltered(&prior_connection, prior_path)?;
    let mut new_connection = deserialize_database(new_bytes, new_path)?;
    validate_schema(&new_connection, new_path)?;
    let new_identity: String = new_connection
        .query_row(
            "SELECT native_catalog_fingerprint FROM local_overlay_identity WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| invalid_database(new_path, error))?;
    if new_identity == stored.native_catalog_fingerprint {
        rewrite_local_overlay(
            &mut new_connection,
            &new_identity,
            &stored.records,
            &stored.negative_hits,
        )?;
    }
    serialize_database(&new_connection)
}

/// Select one primary description without separating it from its provenance.
///
/// Useful content follows the exact source-tier order. Generated discovery
/// phrases are considered only when no useful description exists at any tier,
/// preventing shell-provider boilerplate from masking help or manual content.
pub(crate) fn compose_primary_description(
    facts: &[CompletionDescriptionFact],
) -> Option<CompletionDescriptionFact> {
    let useful = facts
        .iter()
        .filter(|fact| !fact.text.trim().is_empty() && !is_generated_fallback(&fact.text))
        .min_by_key(|fact| fact.tier);
    useful
        .or_else(|| {
            facts
                .iter()
                .filter(|fact| !fact.text.trim().is_empty())
                .min_by_key(|fact| fact.tier)
        })
        .cloned()
}

/// Replace the addressed provider-result groups inside one SQLite generation.
///
/// The returned bytes retain the canonical catalog snapshot and update the
/// normalized overlay in the same SQLite transaction. Callers publish those
/// bytes with the existing atomic file replacement path.
pub(crate) fn merge_local_provider_result(
    bytes: &[u8],
    path: &Path,
    native_catalog_fingerprint: &str,
    records: &[LocalCompletionRecord],
) -> Result<Vec<u8>, ShellError> {
    validate_fingerprint("native catalog fingerprint", native_catalog_fingerprint)?;
    if records.is_empty() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "a local completion provider result contains no records",
        )
        .with_help("Record an empty provider result with the negative-cache API"));
    }
    if records.len() > LOCAL_RECORDS_MAX {
        return Err(resource_limit(
            "local completion records",
            LOCAL_RECORDS_MAX,
            records.len(),
        ));
    }
    for record in records {
        validate_local_record(record)?;
    }
    let mut connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let stored = read_local_overlay_unfiltered(&connection, path)?;
    let (mut retained, mut negative_hits) =
        if stored.native_catalog_fingerprint == native_catalog_fingerprint {
            (stored.records, stored.negative_hits)
        } else {
            (Vec::new(), Vec::new())
        };
    for incoming in records {
        retained.retain(|existing| !same_provider_result_group(existing, incoming));
        negative_hits.retain(|hit| !record_matches_negative(incoming, hit));
    }
    retained.extend_from_slice(records);
    sort_local_records(&mut retained);
    validate_overlay_bounds(&retained, &negative_hits)?;
    rewrite_local_overlay(
        &mut connection,
        native_catalog_fingerprint,
        &retained,
        &negative_hits,
    )?;
    serialize_database(&connection)
}

/// Record one deterministic provider miss and return the updated SQLite image.
///
/// Repeated misses for the same exact identity use capped exponential backoff.
/// Any executable, provider, native-catalog, cwd-class, or environment change
/// starts a fresh negative-cache identity.
pub(crate) fn record_local_negative_hit(
    bytes: &[u8],
    path: &Path,
    native_catalog_fingerprint: &str,
    observation: &LocalNegativeObservation,
) -> Result<Vec<u8>, ShellError> {
    validate_fingerprint("native catalog fingerprint", native_catalog_fingerprint)?;
    validate_negative_observation(observation)?;
    let mut connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let stored = read_local_overlay_unfiltered(&connection, path)?;
    let (mut records, mut negative_hits) =
        if stored.native_catalog_fingerprint == native_catalog_fingerprint {
            (stored.records, stored.negative_hits)
        } else {
            (Vec::new(), Vec::new())
        };
    records.retain(|record| !negative_matches_record(observation, record));
    let prior_failures = negative_hits
        .iter()
        .find(|hit| negative_identity_matches(observation, hit))
        .map_or(0, |hit| hit.failure_count);
    negative_hits.retain(|hit| !negative_identity_matches(observation, hit));
    let failure_count = prior_failures.checked_add(1).ok_or_else(|| {
        resource_limit(
            "negative-cache failures",
            usize::try_from(u32::MAX).unwrap_or(usize::MAX),
            usize::MAX,
        )
    })?;
    let shift = failure_count.saturating_sub(1).min(31);
    let backoff_ms = LOCAL_NEGATIVE_BACKOFF_BASE_MS
        .saturating_mul(1_u64 << shift)
        .min(LOCAL_NEGATIVE_BACKOFF_MAX_MS);
    let retry_after_unix_ms = checked_timestamp_add(
        "negative-cache retry timestamp",
        observation.observed_unix_ms,
        backoff_ms,
    )?;
    let expires_unix_ms = checked_timestamp_add(
        "negative-cache expiry timestamp",
        observation.observed_unix_ms,
        LOCAL_NEGATIVE_EXPIRY_MS,
    )?;
    negative_hits.push(LocalNegativeHit {
        command_path: observation.command_path.clone(),
        provider: observation.provider,
        executable_fingerprint: observation.executable_fingerprint.clone(),
        provider_fingerprint: observation.provider_fingerprint.clone(),
        cwd_class: observation.cwd_class,
        environment_fingerprint: observation.environment_fingerprint.clone(),
        failure_count,
        observed_unix_ms: observation.observed_unix_ms,
        retry_after_unix_ms,
        expires_unix_ms,
    });
    negative_hits.sort();
    validate_overlay_bounds(&records, &negative_hits)?;
    rewrite_local_overlay(
        &mut connection,
        native_catalog_fingerprint,
        &records,
        &negative_hits,
    )?;
    serialize_database(&connection)
}

// These function pointers keep the incremental contract lint-visible before
// the process-side provider task wires it into the interactive completion path.
// No provider is launched from this persistence owner.
const _: () = {
    let _ = read_local_overlay
        as fn(&[u8], &Path, &LocalOverlayQuery) -> Result<LocalOverlay, ShellError>;
    let _ = read_local_overlays
        as fn(&[u8], &Path, &[LocalOverlayQuery]) -> Result<LocalOverlay, ShellError>;
    let _ = merge_local_provider_result
        as fn(&[u8], &Path, &str, &[LocalCompletionRecord]) -> Result<Vec<u8>, ShellError>;
    let _ = record_local_negative_hit
        as fn(&[u8], &Path, &str, &LocalNegativeObservation) -> Result<Vec<u8>, ShellError>;
    let _ = compose_primary_description
        as fn(&[CompletionDescriptionFact]) -> Option<CompletionDescriptionFact>;
    let _ = [
        CompletionCompositionTier::Curated,
        CompletionCompositionTier::CentralImported,
        CompletionCompositionTier::LocalCompletion,
        CompletionCompositionTier::Help,
        CompletionCompositionTier::Man,
        CompletionCompositionTier::PathOnly,
    ];
};

pub(crate) fn build_embeddings(
    bytes: &[u8],
    path: &Path,
    model_path: &Path,
) -> Result<(Vec<u8>, EmbeddingReport), ShellError> {
    build_embeddings_cancellable(bytes, path, model_path, EMBEDDING_BATCH_SIZE, || Ok(()))
}

pub(crate) fn build_embeddings_cancellable(
    bytes: &[u8],
    path: &Path,
    model_path: &Path,
    batch_size: usize,
    mut check_cancelled: impl FnMut() -> Result<(), ShellError>,
) -> Result<(Vec<u8>, EmbeddingReport), ShellError> {
    let mut connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let documents = read_documents(&connection, path)?;
    let model = load_model(model_path)?;
    if batch_size == 0 || batch_size > EMBEDDING_BATCH_SIZE {
        return Err(resource_limit(
            "embedding batch size",
            EMBEDDING_BATCH_SIZE,
            batch_size,
        ));
    }
    let mut vectors = Vec::with_capacity(documents.len());
    for batch in documents.chunks(batch_size) {
        check_cancelled()?;
        let texts: Vec<String> = batch.iter().map(|document| document.body.clone()).collect();
        let mut encoded = catch_unwind(AssertUnwindSafe(|| {
            model.encode_with_args(&texts, Some(256), batch_size)
        }))
        .map_err(|_| {
            ShellError::new(
                ErrorCode::Validation,
                "the local Model2Vec tokenizer failed",
            )
            .with_help("Replace the model files with an intact potion-base-8M release")
        })?;
        if encoded.len() != batch.len() {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "the local Model2Vec model returned an incomplete embedding batch",
            )
            .with_context(format!(
                "expected vectors: {}; observed: {}",
                batch.len(),
                encoded.len()
            ))
            .with_help("Replace the model files and rebuild the semantic index"));
        }
        vectors.append(&mut encoded);
    }
    check_cancelled()?;
    if vectors.len() != documents.len() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "the local Model2Vec model returned an incomplete embedding batch",
        )
        .with_context(format!(
            "expected vectors: {}; observed: {}",
            documents.len(),
            vectors.len()
        ))
        .with_help("Replace the model files and rebuild the semantic index"));
    }
    let dimensions = vectors.first().map_or(0, Vec::len);
    validate_dimensions(dimensions)?;
    let transaction = connection.transaction().map_err(database_error)?;
    transaction
        .execute("DELETE FROM embeddings", [])
        .map_err(database_error)?;
    for (document, vector) in documents.iter().zip(vectors) {
        if vector.len() != dimensions || vector.iter().any(|value| !value.is_finite()) {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "the local Model2Vec model returned an invalid embedding",
            )
            .with_context(format!("document: {}", document.id))
            .with_help("Replace the model files and rebuild the semantic index"));
        }
        let blob = vector_to_bytes(&vector);
        transaction
            .execute(
                "INSERT INTO embeddings(document_id, model_id, dimensions, vector_le_f32, document_fingerprint) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![document.id, MODEL_ID, sqlite_integer(dimensions)?, blob, document.fingerprint],
            )
            .map_err(database_error)?;
    }
    transaction.commit().map_err(database_error)?;
    let encoded = serialize_database(&connection)?;
    Ok((
        encoded,
        EmbeddingReport {
            model: MODEL_ID.to_owned(),
            documents: documents.len(),
            dimensions,
        },
    ))
}

pub(crate) fn search(
    bytes: &[u8],
    path: &Path,
    query: &str,
    limit: usize,
    model_path: Option<&Path>,
) -> Result<Vec<SearchResult>, ShellError> {
    validate_query(query, limit)?;
    let connection = deserialize_database(bytes, path)?;
    validate_schema(&connection, path)?;
    let semantic = if let Some(model_path) = model_path.filter(|path| model_is_installed(path)) {
        semantic_search(&connection, path, query, limit, model_path)?
    } else {
        Vec::new()
    };
    if !semantic.is_empty() {
        return Ok(semantic);
    }
    lexical_search(&connection, path, query, limit)
}

struct StoredLocalOverlay {
    native_catalog_fingerprint: String,
    records: Vec<LocalCompletionRecord>,
    negative_hits: Vec<LocalNegativeHit>,
}

fn read_local_overlay_unfiltered(
    connection: &Connection,
    path: &Path,
) -> Result<StoredLocalOverlay, ShellError> {
    let (version, native_catalog_fingerprint): (i64, String) = connection
        .query_row(
            "SELECT schema_version, native_catalog_fingerprint FROM local_overlay_identity WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| invalid_database(path, error))?;
    if version != i64::from(LOCAL_OVERLAY_SCHEMA_VERSION) {
        return Err(invalid_overlay(
            path,
            format!(
                "local overlay schema version {version} is not {}",
                LOCAL_OVERLAY_SCHEMA_VERSION
            ),
        ));
    }
    validate_fingerprint("native catalog fingerprint", &native_catalog_fingerprint)?;
    let paths = read_local_paths(connection, path)?;
    let records = read_local_records(connection, path, &paths)?;
    let negative_hits = read_local_negative_hits(connection, path, &paths)?;
    validate_overlay_bounds(&records, &negative_hits)?;
    Ok(StoredLocalOverlay {
        native_catalog_fingerprint,
        records,
        negative_hits,
    })
}

fn read_local_paths(
    connection: &Connection,
    path: &Path,
) -> Result<BTreeMap<String, Vec<String>>, ShellError> {
    let rows_max = LOCAL_RECORDS_MAX.saturating_add(LOCAL_NEGATIVE_HITS_MAX);
    let mut statement = connection
        .prepare(
            "SELECT p.path_key, p.segment_count, s.ordinal, s.segment
             FROM local_command_paths p JOIN local_command_path_segments s ON s.path_key = p.path_key
             ORDER BY p.path_key, s.ordinal LIMIT ?1",
        )
        .map_err(|error| invalid_database(path, error))?;
    let rows = statement
        .query_map(
            params![sqlite_integer(
                rows_max
                    .saturating_mul(LOCAL_PATH_DEPTH_MAX)
                    .saturating_add(1)
            )?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .map_err(|error| invalid_database(path, error))?;
    let mut paths = BTreeMap::<String, (usize, Vec<String>)>::new();
    let mut row_count = 0_usize;
    for row in rows {
        let (path_key, segment_count, ordinal, segment) =
            row.map_err(|error| invalid_database(path, error))?;
        row_count = row_count.saturating_add(1);
        if row_count > rows_max.saturating_mul(LOCAL_PATH_DEPTH_MAX) {
            return Err(resource_limit(
                "local path segment rows",
                rows_max.saturating_mul(LOCAL_PATH_DEPTH_MAX),
                row_count,
            ));
        }
        let segment_count = persisted_usize("local path segment count", segment_count)?;
        let ordinal = persisted_usize("local path segment ordinal", ordinal)?;
        let (expected_count, segments) = paths
            .entry(path_key)
            .or_insert_with(|| (segment_count, Vec::new()));
        if *expected_count != segment_count
            || ordinal != segments.len()
            || segment_count > LOCAL_PATH_DEPTH_MAX
        {
            return Err(invalid_overlay(
                path,
                "local command path ordinals are not contiguous",
            ));
        }
        validate_segment(&segment)?;
        segments.push(segment);
    }
    let mut normalized = BTreeMap::new();
    for (path_key, (expected_count, segments)) in paths {
        if expected_count != segments.len() {
            return Err(invalid_overlay(
                path,
                "a local command path segment count does not match its rows",
            ));
        }
        validate_command_path(&segments)?;
        if path_key != local_path_key(&segments)? {
            return Err(invalid_overlay(
                path,
                "local command path identity does not match its segments",
            ));
        }
        normalized.insert(path_key, segments);
    }
    Ok(normalized)
}

fn read_local_records(
    connection: &Connection,
    path: &Path,
    paths: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<LocalCompletionRecord>, ShellError> {
    let mut statement = connection
        .prepare(
            "SELECT record_key, path_key, candidate_kind, insertion_text, display_text, description,
                    provider, confidence, trust, executable_fingerprint, provider_fingerprint,
                    cwd_class, environment_fingerprint, observed_unix_ms, refreshed_unix_ms, refresh_state
             FROM local_completion_records ORDER BY record_key LIMIT ?1",
        )
        .map_err(|error| invalid_database(path, error))?;
    let rows = statement
        .query_map(
            params![sqlite_integer(LOCAL_RECORDS_MAX.saturating_add(1))?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, String>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                    row.get::<_, String>(15)?,
                ))
            },
        )
        .map_err(|error| invalid_database(path, error))?;
    let mut records = Vec::new();
    for row in rows {
        let (
            record_key,
            path_key,
            kind,
            insertion_text,
            display_text,
            description,
            provider,
            confidence,
            trust,
            executable_fingerprint,
            provider_fingerprint,
            cwd_class,
            environment_fingerprint,
            observed_unix_ms,
            refreshed_unix_ms,
            refresh_state,
        ) = row.map_err(|error| invalid_database(path, error))?;
        let command_path = paths.get(&path_key).cloned().ok_or_else(|| {
            invalid_overlay(
                path,
                "a local completion row references an unknown command path",
            )
        })?;
        let record = LocalCompletionRecord {
            command_path,
            kind: parse_candidate_kind(path, &kind)?,
            insertion_text,
            display_text,
            description,
            provider: parse_provider(path, &provider)?,
            confidence: parse_confidence(path, &confidence)?,
            trust: parse_trust(path, &trust)?,
            executable_fingerprint,
            provider_fingerprint,
            cwd_class: parse_cwd_class(path, &cwd_class)?,
            environment_fingerprint,
            observed_unix_ms: persisted_u64("local observation timestamp", observed_unix_ms)?,
            refreshed_unix_ms: persisted_u64("local refresh timestamp", refreshed_unix_ms)?,
            refresh_state: parse_refresh_state(path, &refresh_state)?,
        };
        validate_local_record(&record)?;
        if record_key != local_record_key(&record)? {
            return Err(invalid_overlay(
                path,
                "local completion record identity does not match its fields",
            ));
        }
        records.push(record);
        if records.len() > LOCAL_RECORDS_MAX {
            return Err(resource_limit(
                "local completion records",
                LOCAL_RECORDS_MAX,
                records.len(),
            ));
        }
    }
    sort_local_records(&mut records);
    Ok(records)
}

fn read_local_negative_hits(
    connection: &Connection,
    path: &Path,
    paths: &BTreeMap<String, Vec<String>>,
) -> Result<Vec<LocalNegativeHit>, ShellError> {
    let mut statement = connection
        .prepare(
            "SELECT negative_key, path_key, provider, executable_fingerprint, provider_fingerprint,
                    cwd_class, environment_fingerprint, failure_count, observed_unix_ms,
                    retry_after_unix_ms, expires_unix_ms
             FROM local_negative_hits ORDER BY negative_key LIMIT ?1",
        )
        .map_err(|error| invalid_database(path, error))?;
    let rows = statement
        .query_map(
            params![sqlite_integer(LOCAL_NEGATIVE_HITS_MAX.saturating_add(1))?],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                ))
            },
        )
        .map_err(|error| invalid_database(path, error))?;
    let mut hits = Vec::new();
    for row in rows {
        let (
            negative_key,
            path_key,
            provider,
            executable_fingerprint,
            provider_fingerprint,
            cwd_class,
            environment_fingerprint,
            failure_count,
            observed_unix_ms,
            retry_after_unix_ms,
            expires_unix_ms,
        ) = row.map_err(|error| invalid_database(path, error))?;
        let command_path = paths.get(&path_key).cloned().ok_or_else(|| {
            invalid_overlay(
                path,
                "a local negative hit references an unknown command path",
            )
        })?;
        let failure_count = u32::try_from(failure_count).map_err(|_| {
            invalid_overlay(path, "a local negative hit has an invalid failure count")
        })?;
        let hit = LocalNegativeHit {
            command_path,
            provider: parse_provider(path, &provider)?,
            executable_fingerprint,
            provider_fingerprint,
            cwd_class: parse_cwd_class(path, &cwd_class)?,
            environment_fingerprint,
            failure_count,
            observed_unix_ms: persisted_u64("negative observation timestamp", observed_unix_ms)?,
            retry_after_unix_ms: persisted_u64("negative retry timestamp", retry_after_unix_ms)?,
            expires_unix_ms: persisted_u64("negative expiry timestamp", expires_unix_ms)?,
        };
        validate_negative_hit(&hit)?;
        if negative_key != local_negative_key(&hit)? {
            return Err(invalid_overlay(
                path,
                "local negative-hit identity does not match its fields",
            ));
        }
        hits.push(hit);
        if hits.len() > LOCAL_NEGATIVE_HITS_MAX {
            return Err(resource_limit(
                "local negative hits",
                LOCAL_NEGATIVE_HITS_MAX,
                hits.len(),
            ));
        }
    }
    hits.sort();
    Ok(hits)
}

fn rewrite_local_overlay(
    connection: &mut Connection,
    native_catalog_fingerprint: &str,
    records: &[LocalCompletionRecord],
    negative_hits: &[LocalNegativeHit],
) -> Result<(), ShellError> {
    let transaction = connection.transaction().map_err(database_error)?;
    transaction
        .execute("DELETE FROM local_completion_records", [])
        .map_err(database_error)?;
    transaction
        .execute("DELETE FROM local_negative_hits", [])
        .map_err(database_error)?;
    transaction
        .execute("DELETE FROM local_command_path_segments", [])
        .map_err(database_error)?;
    transaction
        .execute("DELETE FROM local_command_paths", [])
        .map_err(database_error)?;
    transaction
        .execute(
            "UPDATE local_overlay_identity SET schema_version = ?1, native_catalog_fingerprint = ?2 WHERE singleton = 1",
            params![i64::from(LOCAL_OVERLAY_SCHEMA_VERSION), native_catalog_fingerprint],
        )
        .map_err(database_error)?;
    let mut paths = BTreeSet::<Vec<String>>::new();
    paths.extend(records.iter().map(|record| record.command_path.clone()));
    paths.extend(negative_hits.iter().map(|hit| hit.command_path.clone()));
    for command_path in paths {
        let path_key = local_path_key(&command_path)?;
        transaction
            .execute(
                "INSERT INTO local_command_paths(path_key, segment_count) VALUES (?1, ?2)",
                params![path_key, sqlite_integer(command_path.len())?],
            )
            .map_err(database_error)?;
        for (ordinal, segment) in command_path.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO local_command_path_segments(path_key, ordinal, segment) VALUES (?1, ?2, ?3)",
                    params![path_key, sqlite_integer(ordinal)?, segment],
                )
                .map_err(database_error)?;
        }
    }
    for record in records {
        insert_local_record(&transaction, record)?;
    }
    for hit in negative_hits {
        insert_local_negative_hit(&transaction, hit)?;
    }
    transaction.commit().map_err(database_error)
}

fn insert_local_record(
    transaction: &Transaction<'_>,
    record: &LocalCompletionRecord,
) -> Result<(), ShellError> {
    transaction
        .execute(
            "INSERT INTO local_completion_records(record_key, path_key, candidate_kind, insertion_text, display_text, description, provider, confidence, trust, executable_fingerprint, provider_fingerprint, cwd_class, environment_fingerprint, observed_unix_ms, refreshed_unix_ms, refresh_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                local_record_key(record)?, local_path_key(&record.command_path)?,
                candidate_kind_name(record.kind), record.insertion_text, record.display_text,
                record.description, provider_name(record.provider), confidence_name(record.confidence),
                trust_name(record.trust), record.executable_fingerprint, record.provider_fingerprint,
                cwd_class_name(record.cwd_class), record.environment_fingerprint,
                sqlite_u64(record.observed_unix_ms)?, sqlite_u64(record.refreshed_unix_ms)?,
                refresh_state_name(record.refresh_state),
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn insert_local_negative_hit(
    transaction: &Transaction<'_>,
    hit: &LocalNegativeHit,
) -> Result<(), ShellError> {
    transaction
        .execute(
            "INSERT INTO local_negative_hits(negative_key, path_key, provider, executable_fingerprint, provider_fingerprint, cwd_class, environment_fingerprint, failure_count, observed_unix_ms, retry_after_unix_ms, expires_unix_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                local_negative_key(hit)?, local_path_key(&hit.command_path)?, provider_name(hit.provider),
                hit.executable_fingerprint, hit.provider_fingerprint, cwd_class_name(hit.cwd_class),
                hit.environment_fingerprint, i64::from(hit.failure_count), sqlite_u64(hit.observed_unix_ms)?,
                sqlite_u64(hit.retry_after_unix_ms)?, sqlite_u64(hit.expires_unix_ms)?,
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn validate_local_record(record: &LocalCompletionRecord) -> Result<(), ShellError> {
    validate_command_path(&record.command_path)?;
    validate_local_text("candidate insertion text", &record.insertion_text, false)?;
    validate_local_text("candidate display text", &record.display_text, false)?;
    if let Some(description) = &record.description {
        validate_local_text("candidate description", description, true)?;
    }
    if record.kind == LocalCandidateKind::Flag && !record.insertion_text.starts_with('-') {
        return Err(local_validation(
            "a local flag candidate must begin with `-`",
            "Discard the provider result and refresh its completion metadata",
        ));
    }
    if record.kind == LocalCandidateKind::Subcommand
        && record.insertion_text.chars().any(char::is_whitespace)
    {
        return Err(local_validation(
            "a local subcommand candidate must be one path segment",
            "Split nested subcommands into normalized command-path segments",
        ));
    }
    if record.confidence == Confidence::Exact
        || matches!(record.trust, Trust::Builtin | Trust::Trusted)
    {
        return Err(local_validation(
            "local completion metadata cannot claim authoritative provenance",
            "Use low, medium, or high confidence with declared, imported, or heuristic trust",
        ));
    }
    validate_fingerprint("executable fingerprint", &record.executable_fingerprint)?;
    validate_fingerprint("provider fingerprint", &record.provider_fingerprint)?;
    validate_fingerprint("environment fingerprint", &record.environment_fingerprint)?;
    validate_persisted_u64("observation timestamp", record.observed_unix_ms)?;
    validate_persisted_u64("refresh timestamp", record.refreshed_unix_ms)?;
    if record.refreshed_unix_ms < record.observed_unix_ms {
        return Err(local_validation(
            "a local completion refresh predates its observation",
            "Supply monotonic fixed-width provider timestamps",
        ));
    }
    Ok(())
}

fn validate_negative_observation(observation: &LocalNegativeObservation) -> Result<(), ShellError> {
    validate_command_path(&observation.command_path)?;
    validate_fingerprint(
        "executable fingerprint",
        &observation.executable_fingerprint,
    )?;
    validate_fingerprint("provider fingerprint", &observation.provider_fingerprint)?;
    validate_fingerprint(
        "environment fingerprint",
        &observation.environment_fingerprint,
    )?;
    validate_persisted_u64(
        "negative observation timestamp",
        observation.observed_unix_ms,
    )
}

fn validate_negative_hit(hit: &LocalNegativeHit) -> Result<(), ShellError> {
    let observation = LocalNegativeObservation {
        command_path: hit.command_path.clone(),
        provider: hit.provider,
        executable_fingerprint: hit.executable_fingerprint.clone(),
        provider_fingerprint: hit.provider_fingerprint.clone(),
        cwd_class: hit.cwd_class,
        environment_fingerprint: hit.environment_fingerprint.clone(),
        observed_unix_ms: hit.observed_unix_ms,
    };
    validate_negative_observation(&observation)?;
    if hit.failure_count == 0
        || hit.retry_after_unix_ms <= hit.observed_unix_ms
        || hit.expires_unix_ms <= hit.retry_after_unix_ms
    {
        return Err(local_validation(
            "a local negative-cache row has invalid bounded retry state",
            "Discard the local overlay and retry provider discovery",
        ));
    }
    validate_persisted_u64("negative retry timestamp", hit.retry_after_unix_ms)?;
    validate_persisted_u64("negative expiry timestamp", hit.expires_unix_ms)
}

fn validate_overlay_bounds(
    records: &[LocalCompletionRecord],
    negative_hits: &[LocalNegativeHit],
) -> Result<(), ShellError> {
    if records.len() > LOCAL_RECORDS_MAX {
        return Err(resource_limit(
            "local completion records",
            LOCAL_RECORDS_MAX,
            records.len(),
        ));
    }
    if negative_hits.len() > LOCAL_NEGATIVE_HITS_MAX {
        return Err(resource_limit(
            "local negative hits",
            LOCAL_NEGATIVE_HITS_MAX,
            negative_hits.len(),
        ));
    }
    let retained_bytes = serde_json::to_vec(&(records, negative_hits))
        .map_err(json_error)?
        .len();
    if retained_bytes > LOCAL_RETAINED_BYTES_MAX {
        return Err(resource_limit(
            "local overlay retained bytes",
            LOCAL_RETAINED_BYTES_MAX,
            retained_bytes,
        ));
    }
    Ok(())
}

fn validate_command_path(command_path: &[String]) -> Result<(), ShellError> {
    if command_path.is_empty() || command_path.len() > LOCAL_PATH_DEPTH_MAX {
        return Err(resource_limit(
            "local command path depth",
            LOCAL_PATH_DEPTH_MAX,
            command_path.len(),
        ));
    }
    for segment in command_path {
        validate_segment(segment)?;
    }
    Ok(())
}

fn validate_segment(segment: &str) -> Result<(), ShellError> {
    validate_local_text("local command path segment", segment, false)?;
    if segment.len() > LOCAL_SEGMENT_BYTES_MAX {
        return Err(resource_limit(
            "local command path segment bytes",
            LOCAL_SEGMENT_BYTES_MAX,
            segment.len(),
        ));
    }
    if segment.chars().any(char::is_whitespace) {
        return Err(local_validation(
            "a local command path segment contains whitespace",
            "Split the command into normalized path segments",
        ));
    }
    Ok(())
}

fn validate_local_text(label: &str, value: &str, allow_empty: bool) -> Result<(), ShellError> {
    if !allow_empty && value.is_empty() {
        return Err(local_validation(
            format!("{label} is empty"),
            "Discard the malformed provider result and refresh it",
        ));
    }
    if value.len() > LOCAL_TEXT_BYTES_MAX {
        return Err(resource_limit(label, LOCAL_TEXT_BYTES_MAX, value.len()));
    }
    if value.chars().any(char::is_control) {
        return Err(local_validation(
            format!("{label} contains control characters"),
            "Discard the malformed provider result and refresh it",
        ));
    }
    Ok(())
}

fn validate_fingerprint(label: &str, value: &str) -> Result<(), ShellError> {
    if value.is_empty() || value.len() > LOCAL_FINGERPRINT_BYTES_MAX {
        return Err(resource_limit(
            label,
            LOCAL_FINGERPRINT_BYTES_MAX,
            value.len(),
        ));
    }
    if value
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(local_validation(
            format!("{label} contains whitespace or control characters"),
            "Use a bounded stable digest or version identifier",
        ));
    }
    Ok(())
}

fn validate_persisted_u64(label: &str, value: u64) -> Result<(), ShellError> {
    if value > u64::try_from(i64::MAX).unwrap_or(u64::MAX) {
        return Err(local_validation(
            format!("{label} exceeds SQLite's fixed-width integer range"),
            "Supply a nonnegative signed 64-bit millisecond value",
        ));
    }
    Ok(())
}

fn checked_timestamp_add(label: &str, timestamp: u64, delta: u64) -> Result<u64, ShellError> {
    let result = timestamp.checked_add(delta).ok_or_else(|| {
        local_validation(
            format!("{label} overflowed"),
            "Supply a smaller nonnegative signed 64-bit millisecond value",
        )
    })?;
    validate_persisted_u64(label, result)?;
    Ok(result)
}

fn local_path_key(command_path: &[String]) -> Result<String, ShellError> {
    let encoded = serde_json::to_vec(command_path).map_err(json_error)?;
    Ok(fingerprint(&encoded))
}

fn local_record_key(record: &LocalCompletionRecord) -> Result<String, ShellError> {
    let encoded = serde_json::to_vec(record).map_err(json_error)?;
    Ok(fingerprint(&encoded))
}

fn local_negative_key(hit: &LocalNegativeHit) -> Result<String, ShellError> {
    let encoded = serde_json::to_vec(hit).map_err(json_error)?;
    Ok(fingerprint(&encoded))
}

fn sort_local_records(records: &mut [LocalCompletionRecord]) {
    records.sort_by(|left, right| {
        left.command_path
            .cmp(&right.command_path)
            .then_with(|| left.provider.cmp(&right.provider))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.insertion_text.cmp(&right.insertion_text))
            .then_with(|| left.display_text.cmp(&right.display_text))
            .then_with(|| {
                left.executable_fingerprint
                    .cmp(&right.executable_fingerprint)
            })
            .then_with(|| left.provider_fingerprint.cmp(&right.provider_fingerprint))
    });
}

fn same_provider_result_group(
    existing: &LocalCompletionRecord,
    incoming: &LocalCompletionRecord,
) -> bool {
    existing.command_path == incoming.command_path
        && existing.provider == incoming.provider
        && existing.cwd_class == incoming.cwd_class
        && existing.environment_fingerprint == incoming.environment_fingerprint
}

fn record_matches_negative(record: &LocalCompletionRecord, hit: &LocalNegativeHit) -> bool {
    record.command_path == hit.command_path
        && record.provider == hit.provider
        && record.executable_fingerprint == hit.executable_fingerprint
        && record.provider_fingerprint == hit.provider_fingerprint
        && record.cwd_class == hit.cwd_class
        && record.environment_fingerprint == hit.environment_fingerprint
}

fn negative_matches_record(
    observation: &LocalNegativeObservation,
    record: &LocalCompletionRecord,
) -> bool {
    observation.command_path == record.command_path
        && observation.provider == record.provider
        && observation.cwd_class == record.cwd_class
        && observation.environment_fingerprint == record.environment_fingerprint
}

fn negative_identity_matches(
    observation: &LocalNegativeObservation,
    hit: &LocalNegativeHit,
) -> bool {
    observation.command_path == hit.command_path
        && observation.provider == hit.provider
        && observation.executable_fingerprint == hit.executable_fingerprint
        && observation.provider_fingerprint == hit.provider_fingerprint
        && observation.cwd_class == hit.cwd_class
        && observation.environment_fingerprint == hit.environment_fingerprint
}

fn local_identity_matches(
    executable_fingerprint: &str,
    provider_fingerprint: &str,
    cwd_class: LocalCwdClass,
    environment_fingerprint: &str,
    query: &LocalOverlayQuery,
) -> bool {
    executable_fingerprint == query.executable_fingerprint
        && provider_fingerprint == query.provider_fingerprint
        && cwd_class == query.cwd_class
        && environment_fingerprint == query.environment_fingerprint
}

fn is_generated_fallback(text: &str) -> bool {
    let normalized = text.trim().to_ascii_lowercase();
    normalized == "installed command discovered on path"
        || normalized.starts_with("command discovered from fish completion metadata")
        || normalized.starts_with("command discovered from bash completion metadata")
        || normalized.starts_with("command discovered from zsh completion metadata")
}

fn insert_catalog(transaction: &Transaction<'_>, catalog: &Catalog) -> Result<(), ShellError> {
    let mut document_count = 0_usize;
    for command in &catalog.commands {
        transaction
            .execute(
                "INSERT INTO commands(command_id, path, version, parent_id, signature, summary, details, input_type, output_type, streaming, provenance_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    command.id,
                    command.path,
                    command.version,
                    command.parent,
                    command.signature,
                    command.summary,
                    command.details,
                    command.io.input,
                    command.io.output,
                    command.io.streaming,
                    serde_json::to_string(&command.provenance).map_err(json_error)?,
                ],
            )
            .map_err(database_error)?;
        for alias in &command.aliases {
            transaction
                .execute(
                    "INSERT INTO aliases(command_id, alias) VALUES (?1, ?2)",
                    params![command.id, alias],
                )
                .map_err(database_error)?;
        }
        insert_indexed_strings(transaction, "command", &command.id, &command.examples)?;
        for effect in &command.effects {
            transaction
                .execute(
                    "INSERT INTO effects(command_id, effect) VALUES (?1, ?2)",
                    params![
                        command.id,
                        serde_json::to_string(effect).map_err(json_error)?
                    ],
                )
                .map_err(database_error)?;
        }
        for (status, meaning) in &command.exit_codes {
            transaction
                .execute(
                    "INSERT INTO exit_codes(command_id, status, meaning) VALUES (?1, ?2, ?3)",
                    params![command.id, status, meaning],
                )
                .map_err(database_error)?;
        }
        let command_document = command_document(command);
        insert_document(transaction, &command_document)?;
        document_count = document_count.saturating_add(1);
        for (ordinal, argument) in command.options.iter().enumerate() {
            let argument_id = format!("{}:{ordinal}", command.id);
            let (dynamic_provider, static_values) = match &argument.values {
                Some(CompletionSource::Dynamic { provider }) => (Some(provider.as_str()), &[][..]),
                Some(CompletionSource::Static { values }) => (None, values.as_slice()),
                None => (None, &[][..]),
            };
            transaction
                .execute(
                    "INSERT INTO arguments(argument_id, command_id, ordinal, kind, value_type, required, repeatable, documentation, dynamic_provider, provenance_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        argument_id,
                        command.id,
                        sqlite_integer(ordinal)?,
                        argument_kind(argument.kind),
                        argument.value_type,
                        argument.required,
                        argument.repeatable,
                        argument.documentation,
                        dynamic_provider,
                        serde_json::to_string(&argument.provenance).map_err(json_error)?,
                    ],
                )
                .map_err(database_error)?;
            for name in &argument.names {
                transaction
                    .execute(
                        "INSERT INTO argument_names(argument_id, name) VALUES (?1, ?2)",
                        params![argument_id, name],
                    )
                    .map_err(database_error)?;
            }
            for value in static_values {
                transaction
                    .execute(
                        "INSERT INTO argument_values(argument_id, value) VALUES (?1, ?2)",
                        params![argument_id, value],
                    )
                    .map_err(database_error)?;
            }
            for conflict in &argument.conflicts {
                transaction
                    .execute(
                        "INSERT INTO argument_conflicts(argument_id, conflicting_name) VALUES (?1, ?2)",
                        params![argument_id, conflict],
                    )
                    .map_err(database_error)?;
            }
            insert_indexed_strings(transaction, "argument", &argument_id, &argument.examples)?;
            let document = argument_document(command, ordinal);
            insert_document(transaction, &document)?;
            document_count = document_count.saturating_add(1);
            if document_count > DOCUMENTS_MAX {
                return Err(resource_limit(
                    "semantic documents",
                    DOCUMENTS_MAX,
                    document_count,
                ));
            }
        }
    }
    Ok(())
}

fn insert_indexed_strings(
    transaction: &Transaction<'_>,
    owner_kind: &str,
    owner_id: &str,
    values: &[String],
) -> Result<(), ShellError> {
    for (ordinal, value) in values.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO examples(owner_kind, owner_id, ordinal, example) VALUES (?1, ?2, ?3, ?4)",
                params![owner_kind, owner_id, sqlite_integer(ordinal)?, value],
            )
            .map_err(database_error)?;
    }
    Ok(())
}

fn insert_document(
    transaction: &Transaction<'_>,
    document: &SemanticDocument,
) -> Result<(), ShellError> {
    if document.body.len() > DOCUMENT_BYTES_MAX {
        return Err(resource_limit(
            "semantic document bytes",
            DOCUMENT_BYTES_MAX,
            document.body.len(),
        ));
    }
    transaction
        .execute(
            "INSERT INTO semantic_documents(document_id, document_kind, command_id, target_id, title, body, fingerprint) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![document.id, document.kind, document.command, document.target, document.title, document.body, document.fingerprint],
        )
        .map_err(database_error)?;
    Ok(())
}

fn command_document(command: &quirl_catalog::CommandSpec) -> SemanticDocument {
    let aliases = command.aliases.join(" ");
    let examples = command.examples.join(" ");
    let body = format!(
        "{} {} {} {} {} {}",
        command.path, aliases, command.signature, command.summary, command.details, examples
    );
    SemanticDocument {
        id: format!("command:{}", command.id),
        kind: "command".to_owned(),
        command: command.path.clone(),
        target: command.path.clone(),
        title: command.path.clone(),
        fingerprint: fingerprint(body.as_bytes()),
        body,
    }
}

fn argument_document(command: &quirl_catalog::CommandSpec, ordinal: usize) -> SemanticDocument {
    let argument = &command.options[ordinal];
    let names = argument.names.join(" ");
    let examples = argument.examples.join(" ");
    let values = match &argument.values {
        Some(CompletionSource::Static { values }) => values.join(" "),
        Some(CompletionSource::Dynamic { provider }) => provider.clone(),
        None => String::new(),
    };
    let body = format!(
        "{} {} {} {} {} {}",
        command.path, names, argument.value_type, argument.documentation, values, examples
    );
    SemanticDocument {
        id: format!("argument:{}:{ordinal}", command.id),
        kind: "option".to_owned(),
        command: command.path.clone(),
        target: format!("{} {}", command.path, names),
        title: names,
        fingerprint: fingerprint(body.as_bytes()),
        body,
    }
}

fn read_documents(
    connection: &Connection,
    path: &Path,
) -> Result<Vec<SemanticDocument>, ShellError> {
    let mut statement = connection
        .prepare("SELECT document_id, document_kind, command_id, target_id, title, body, fingerprint FROM semantic_documents ORDER BY document_id LIMIT ?1")
        .map_err(|error| invalid_database(path, error))?;
    let rows = statement
        .query_map(
            params![sqlite_integer(DOCUMENTS_MAX.saturating_add(1))?],
            |row| {
                Ok(SemanticDocument {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    command: row.get(2)?,
                    target: row.get(3)?,
                    title: row.get(4)?,
                    body: row.get(5)?,
                    fingerprint: row.get(6)?,
                })
            },
        )
        .map_err(|error| invalid_database(path, error))?;
    let mut documents = Vec::new();
    for row in rows {
        documents.push(row.map_err(|error| invalid_database(path, error))?);
        if documents.len() > DOCUMENTS_MAX {
            return Err(resource_limit(
                "semantic documents",
                DOCUMENTS_MAX,
                documents.len(),
            ));
        }
    }
    Ok(documents)
}

fn semantic_search(
    connection: &Connection,
    path: &Path,
    query: &str,
    limit: usize,
    model_path: &Path,
) -> Result<Vec<SearchResult>, ShellError> {
    let embeddings = read_embeddings(connection, path)?;
    if embeddings.is_empty() {
        return Ok(Vec::new());
    }
    let model = load_model(model_path)?;
    let query_vector =
        catch_unwind(AssertUnwindSafe(|| model.encode_single(query))).map_err(|_| {
            ShellError::new(
                ErrorCode::Validation,
                "the local Model2Vec tokenizer failed",
            )
            .with_help("Replace the model files with an intact potion-base-8M release")
        })?;
    validate_dimensions(query_vector.len())?;
    let mut ranked: Vec<_> = embeddings
        .into_iter()
        .filter(|embedding| embedding.vector.len() == query_vector.len())
        .map(|embedding| SearchResult {
            command: embedding.document.command,
            target: embedding.document.target,
            kind: embedding.document.kind,
            summary: embedding.document.title,
            score: cosine_similarity(&query_vector, &embedding.vector),
            semantic: true,
        })
        .collect();
    sort_and_limit(&mut ranked, limit);
    Ok(ranked)
}

fn lexical_search(
    connection: &Connection,
    path: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, ShellError> {
    let query_terms: Vec<String> = query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(str::to_lowercase)
        .take(64)
        .collect();
    Ok(rank_lexical_documents(
        &read_documents(connection, path)?,
        &query_terms,
        limit,
    ))
}

fn rank_lexical_documents(
    documents: &[SemanticDocument],
    query_terms: &[String],
    limit: usize,
) -> Vec<SearchResult> {
    let query_phrase = query_terms.join(" ");
    let mut document_matches = Vec::with_capacity(documents.len());
    let mut phrase_matches = Vec::with_capacity(documents.len());
    for document in documents {
        let haystack = document.body.to_lowercase();
        let matched = query_terms
            .iter()
            .enumerate()
            .filter_map(|(index, term)| haystack.contains(term.as_str()).then_some(index))
            .collect::<BTreeSet<_>>();
        document_matches.push(matched);
        phrase_matches.push(!query_phrase.is_empty() && haystack.contains(&query_phrase));
    }

    let mut command_matches = BTreeMap::<&str, BTreeSet<usize>>::new();
    for (document, matched) in documents.iter().zip(&document_matches) {
        if document.kind == "command" {
            command_matches.insert(&document.command, matched.clone());
        }
    }
    for (document, matched) in documents.iter().zip(&document_matches) {
        if document.kind == "command" {
            continue;
        }
        let base = command_matches
            .get(document.command.as_str())
            .cloned()
            .unwrap_or_default();
        let mut candidate = base;
        candidate.extend(matched);
        let aggregate = command_matches.entry(&document.command).or_default();
        if candidate.len() > aggregate.len() {
            *aggregate = candidate;
        }
    }

    let denominator = query_terms.len().max(1) as f32;
    let mut results = documents
        .iter()
        .zip(document_matches)
        .zip(phrase_matches)
        .filter_map(|((document, local_matches), phrase_match)| {
            let matches = if document.kind == "command" {
                command_matches
                    .get(document.command.as_str())
                    .map_or(0, BTreeSet::len)
            } else {
                local_matches.len()
            };
            (matches > 0).then(|| SearchResult {
                command: document.command.clone(),
                target: document.target.clone(),
                kind: document.kind.clone(),
                summary: document.title.clone(),
                score: matches as f32 / denominator + if phrase_match { 1.0 } else { 0.0 },
                semantic: false,
            })
        })
        .collect::<Vec<_>>();
    sort_and_limit(&mut results, limit);
    results
}

fn read_embeddings(
    connection: &Connection,
    path: &Path,
) -> Result<Vec<StoredEmbedding>, ShellError> {
    let mut statement = connection
        .prepare(
            "SELECT d.document_id, d.document_kind, d.command_id, d.target_id, d.title, d.body, d.fingerprint, e.dimensions, e.vector_le_f32
             FROM semantic_documents d JOIN embeddings e ON e.document_id = d.document_id
             WHERE e.model_id = ?1 AND e.document_fingerprint = d.fingerprint
             ORDER BY d.document_id LIMIT ?2",
        )
        .map_err(|error| invalid_database(path, error))?;
    let rows = statement
        .query_map(
            params![MODEL_ID, sqlite_integer(DOCUMENTS_MAX.saturating_add(1))?],
            |row| {
                let dimensions: i64 = row.get(7)?;
                let bytes: Vec<u8> = row.get(8)?;
                Ok((
                    SemanticDocument {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        command: row.get(2)?,
                        target: row.get(3)?,
                        title: row.get(4)?,
                        body: row.get(5)?,
                        fingerprint: row.get(6)?,
                    },
                    dimensions,
                    bytes,
                ))
            },
        )
        .map_err(|error| invalid_database(path, error))?;
    let mut embeddings = Vec::new();
    for row in rows {
        let (document, dimensions, bytes) = row.map_err(|error| invalid_database(path, error))?;
        let dimensions = usize::try_from(dimensions).map_err(|_| {
            ShellError::new(
                ErrorCode::Validation,
                "the command database contains an invalid embedding dimension",
            )
            .with_context(format!("observed: {dimensions}"))
            .with_help("Run `quirl ai index` to rebuild semantic embeddings")
        })?;
        validate_dimensions(dimensions)?;
        let vector = bytes_to_vector(&bytes, dimensions)?;
        embeddings.push(StoredEmbedding { document, vector });
        if embeddings.len() > DOCUMENTS_MAX {
            return Err(resource_limit(
                "stored embeddings",
                DOCUMENTS_MAX,
                embeddings.len(),
            ));
        }
    }
    Ok(embeddings)
}

fn deserialize_database(bytes: &[u8], path: &Path) -> Result<Connection, ShellError> {
    if bytes.len() > DATABASE_BYTES_MAX {
        return Err(resource_limit(
            "database bytes",
            DATABASE_BYTES_MAX,
            bytes.len(),
        ));
    }
    let mut connection = Connection::open_in_memory().map_err(database_error)?;
    configure_connection(&connection)?;
    connection
        .deserialize_read_exact(MAIN_DB, Cursor::new(bytes), bytes.len(), false)
        .map_err(|error| invalid_database(path, error))?;
    Ok(connection)
}

fn configure_connection(connection: &Connection) -> Result<(), ShellError> {
    let length_limit = i32::try_from(DATABASE_BYTES_MAX).unwrap_or(i32::MAX);
    connection
        .set_limit(Limit::SQLITE_LIMIT_LENGTH, length_limit)
        .map_err(database_error)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)
        .map_err(database_error)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_COLUMN, 64)
        .map_err(database_error)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 32)
        .map_err(database_error)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_COMPOUND_SELECT, 16)
        .map_err(database_error)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 64)
        .map_err(database_error)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)
        .map_err(database_error)?;
    connection
        .set_limit(Limit::SQLITE_LIMIT_WORKER_THREADS, 0)
        .map_err(database_error)?;
    Ok(())
}

fn validate_schema(connection: &Connection, path: &Path) -> Result<(), ShellError> {
    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(|error| invalid_database(path, error))?;
    let schema_version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| invalid_database(path, error))?;
    if application_id != DATABASE_APPLICATION_ID || schema_version != DATABASE_SCHEMA_VERSION {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!("{} is not a compatible Quirl command database", path.display()),
        )
        .with_context(format!(
            "application id: {application_id}; schema version: {schema_version}; expected: {DATABASE_APPLICATION_ID}/{DATABASE_SCHEMA_VERSION}"
        ))
        .with_help("Rebuild it with `quirl index build`"));
    }
    Ok(())
}

fn serialize_database(connection: &Connection) -> Result<Vec<u8>, ShellError> {
    let data: Data<'_> = connection.serialize(MAIN_DB).map_err(database_error)?;
    if data.len() > DATABASE_BYTES_MAX {
        return Err(resource_limit(
            "database bytes",
            DATABASE_BYTES_MAX,
            data.len(),
        ));
    }
    Ok(data.to_vec())
}

fn load_model(path: &Path) -> Result<StaticModel, ShellError> {
    validate_model_files(path)?;
    catch_unwind(AssertUnwindSafe(|| {
        StaticModel::from_pretrained(path, None, Some(true), None)
    }))
    .map_err(|_| {
        ShellError::new(ErrorCode::Validation, "the local Model2Vec loader failed")
            .with_help("Replace the model files with an intact potion-base-8M release")
    })?
    .map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("could not load potion-base-8M from {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Replace the model files or set QUIRL_MODEL_PATH to an intact local model")
    })
}

fn validate_model_files(path: &Path) -> Result<(), ShellError> {
    crate::ai_bootstrap::validate_pinned_model(path)?;
    for (name, bytes_max) in [
        ("config.json", MODEL_CONFIG_BYTES_MAX),
        ("tokenizer.json", MODEL_TOKENIZER_BYTES_MAX),
        ("model.safetensors", MODEL_WEIGHTS_BYTES_MAX),
    ] {
        let file = path.join(name);
        let metadata = std::fs::symlink_metadata(&file).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                format!("potion-base-8M is not installed at {}", path.display()),
            )
            .with_context(error.to_string())
            .with_help("Place config.json, tokenizer.json, and model.safetensors in that directory or set QUIRL_MODEL_PATH")
        })?;
        if !metadata.file_type().is_file() {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("local model input {} is not a regular file", file.display()),
            )
            .with_help("Install potion-base-8M as three unlinked regular files"));
        }
        if metadata.len() > bytes_max {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!(
                    "local model input {} exceeds its size limit",
                    file.display()
                ),
            )
            .with_context(format!("limit: {bytes_max}; observed: {}", metadata.len()))
            .with_help("Replace the model file with the official potion-base-8M artifact"));
        }
    }
    Ok(())
}

fn count_rows(connection: &Connection, path: &Path, table: &str) -> Result<usize, ShellError> {
    let sql = match table {
        "commands" => "SELECT count(*) FROM commands",
        "arguments" => "SELECT count(*) FROM arguments",
        "semantic_documents" => "SELECT count(*) FROM semantic_documents",
        "embeddings" => "SELECT count(*) FROM embeddings",
        _ => unreachable!("table names are fixed by the command database schema"),
    };
    let count: i64 = connection
        .query_row(sql, [], |row| row.get(0))
        .map_err(|error| invalid_database(path, error))?;
    usize::try_from(count).map_err(|_| {
        ShellError::new(
            ErrorCode::Validation,
            "the command database contains an invalid row count",
        )
        .with_context(format!("table: {table}; observed: {count}"))
        .with_help("Rebuild it with `quirl index build`")
    })
}

fn validate_query(query: &str, limit: usize) -> Result<(), ShellError> {
    if query.trim().is_empty() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "natural-language command search requires a query",
        )
        .with_help("Describe the task you want a command to perform"));
    }
    if query.len() > QUERY_BYTES_MAX {
        return Err(resource_limit("query bytes", QUERY_BYTES_MAX, query.len()));
    }
    if limit == 0 || limit > SEARCH_RESULTS_MAX {
        return Err(resource_limit("search results", SEARCH_RESULTS_MAX, limit));
    }
    Ok(())
}

fn validate_dimensions(dimensions: usize) -> Result<(), ShellError> {
    if dimensions == 0 || dimensions > EMBEDDING_DIMENSIONS_MAX {
        return Err(resource_limit(
            "embedding dimensions",
            EMBEDDING_DIMENSIONS_MAX,
            dimensions,
        ));
    }
    Ok(())
}

fn vector_to_bytes(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len().saturating_mul(4));
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn bytes_to_vector(bytes: &[u8], dimensions: usize) -> Result<Vec<f32>, ShellError> {
    let expected = dimensions.checked_mul(4).ok_or_else(|| {
        resource_limit("embedding bytes", EMBEDDING_DIMENSIONS_MAX * 4, usize::MAX)
    })?;
    if bytes.len() != expected {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "the command database contains a malformed embedding",
        )
        .with_context(format!(
            "expected bytes: {expected}; observed: {}",
            bytes.len()
        ))
        .with_help("Run `quirl ai index` to rebuild semantic embeddings"));
    }
    let vector = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect::<Vec<_>>();
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "the command database contains a non-finite embedding",
        )
        .with_help("Run `quirl ai index` to rebuild semantic embeddings"));
    }
    Ok(vector)
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (left, right) in left.iter().zip(right) {
        dot += left * right;
        left_norm += left * left;
        right_norm += right * right;
    }
    let denominator = left_norm.sqrt() * right_norm.sqrt();
    if denominator > 0.0 {
        dot / denominator
    } else {
        0.0
    }
}

fn sort_and_limit(results: &mut Vec<SearchResult>, limit: usize) {
    results.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.target.cmp(&right.target))
    });
    results.truncate(limit);
}

fn argument_kind(kind: ArgumentKind) -> &'static str {
    match kind {
        ArgumentKind::Positional => "positional",
        ArgumentKind::Option => "option",
        ArgumentKind::Flag => "flag",
    }
}

fn candidate_kind_name(kind: LocalCandidateKind) -> &'static str {
    match kind {
        LocalCandidateKind::Subcommand => "subcommand",
        LocalCandidateKind::Flag => "flag",
        LocalCandidateKind::Value => "value",
    }
}

fn parse_candidate_kind(path: &Path, value: &str) -> Result<LocalCandidateKind, ShellError> {
    match value {
        "subcommand" => Ok(LocalCandidateKind::Subcommand),
        "flag" => Ok(LocalCandidateKind::Flag),
        "value" => Ok(LocalCandidateKind::Value),
        _ => Err(invalid_overlay(
            path,
            format!("unknown local candidate kind `{value}`"),
        )),
    }
}

fn provider_name(provider: LocalCompletionProvider) -> &'static str {
    match provider {
        LocalCompletionProvider::Fish => "fish",
        LocalCompletionProvider::Bash => "bash",
        LocalCompletionProvider::Zsh => "zsh",
    }
}

fn parse_provider(path: &Path, value: &str) -> Result<LocalCompletionProvider, ShellError> {
    match value {
        "fish" => Ok(LocalCompletionProvider::Fish),
        "bash" => Ok(LocalCompletionProvider::Bash),
        "zsh" => Ok(LocalCompletionProvider::Zsh),
        _ => Err(invalid_overlay(
            path,
            format!("unknown local provider `{value}`"),
        )),
    }
}

fn cwd_class_name(class: LocalCwdClass) -> &'static str {
    match class {
        LocalCwdClass::Any => "any",
        LocalCwdClass::Directory => "directory",
        LocalCwdClass::Repository => "repository",
    }
}

fn parse_cwd_class(path: &Path, value: &str) -> Result<LocalCwdClass, ShellError> {
    match value {
        "any" => Ok(LocalCwdClass::Any),
        "directory" => Ok(LocalCwdClass::Directory),
        "repository" => Ok(LocalCwdClass::Repository),
        _ => Err(invalid_overlay(
            path,
            format!("unknown local cwd class `{value}`"),
        )),
    }
}

fn refresh_state_name(state: LocalRefreshState) -> &'static str {
    match state {
        LocalRefreshState::Fresh => "fresh",
        LocalRefreshState::Stale => "stale",
    }
}

fn parse_refresh_state(path: &Path, value: &str) -> Result<LocalRefreshState, ShellError> {
    match value {
        "fresh" => Ok(LocalRefreshState::Fresh),
        "stale" => Ok(LocalRefreshState::Stale),
        _ => Err(invalid_overlay(
            path,
            format!("unknown local refresh state `{value}`"),
        )),
    }
}

fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
        Confidence::Exact => "exact",
    }
}

fn parse_confidence(path: &Path, value: &str) -> Result<Confidence, ShellError> {
    match value {
        "low" => Ok(Confidence::Low),
        "medium" => Ok(Confidence::Medium),
        "high" => Ok(Confidence::High),
        "exact" => Ok(Confidence::Exact),
        _ => Err(invalid_overlay(
            path,
            format!("unknown local confidence `{value}`"),
        )),
    }
}

fn trust_name(trust: Trust) -> &'static str {
    match trust {
        Trust::Builtin => "builtin",
        Trust::Trusted => "trusted",
        Trust::Declared => "declared",
        Trust::Imported => "imported",
        Trust::Heuristic => "heuristic",
    }
}

fn parse_trust(path: &Path, value: &str) -> Result<Trust, ShellError> {
    match value {
        "builtin" => Ok(Trust::Builtin),
        "trusted" => Ok(Trust::Trusted),
        "declared" => Ok(Trust::Declared),
        "imported" => Ok(Trust::Imported),
        "heuristic" => Ok(Trust::Heuristic),
        _ => Err(invalid_overlay(
            path,
            format!("unknown local trust `{value}`"),
        )),
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{:x}", hasher.finalize())
}

fn sqlite_integer(value: usize) -> Result<i64, ShellError> {
    i64::try_from(value).map_err(|_| {
        resource_limit(
            "SQLite integer",
            usize::try_from(i64::MAX).unwrap_or(usize::MAX),
            value,
        )
    })
}

fn sqlite_u64(value: u64) -> Result<i64, ShellError> {
    i64::try_from(value).map_err(|_| {
        local_validation(
            "a fixed-width local-overlay integer exceeds SQLite's signed range",
            "Supply a nonnegative signed 64-bit value",
        )
    })
}

fn persisted_usize(label: &str, value: i64) -> Result<usize, ShellError> {
    usize::try_from(value).map_err(|_| {
        local_validation(
            format!("the persisted {label} is negative or too large"),
            "Discard the malformed local overlay and refresh it",
        )
    })
}

fn persisted_u64(label: &str, value: i64) -> Result<u64, ShellError> {
    u64::try_from(value).map_err(|_| {
        local_validation(
            format!("the persisted {label} is negative"),
            "Discard the malformed local overlay and refresh it",
        )
    })
}

fn resource_limit(resource: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("command intelligence exceeded its {resource} limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Narrow the indexed command sources or query and retry")
}

fn database_error(error: rusqlite::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not update the local command database")
        .with_context(error.to_string())
        .with_help("Check the Quirl cache directory and retry")
}

fn invalid_database(path: &Path, error: rusqlite::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("{} is not a valid Quirl command database", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Rebuild it with `quirl index build`")
}

fn invalid_overlay(path: &Path, context: impl Into<String>) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "{} contains an invalid local completion overlay",
            path.display()
        ),
    )
    .with_context(context.into())
    .with_help("Rebuild the command index and refresh local completion metadata")
}

fn local_validation(message: impl Into<String>, help: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Validation, message.into()).with_help(help.into())
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        "could not encode command intelligence data",
    )
    .with_context(error.to_string())
    .with_help("Correct invalid command metadata and retry")
}

trait OptionalRow<T> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalRow<T> for Result<T, rusqlite::Error> {
    fn optional(self) -> Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_round_trips_catalog_and_discovery_state() {
        let catalog = Catalog::builtin();
        let bytes = encode_database(&catalog, Some("{\"version\":1}")).unwrap();
        assert!(bytes.starts_with(b"SQLite format 3\0"));
        let (decoded, state) = decode_database(&bytes, Path::new("catalog.sqlite3")).unwrap();
        assert_eq!(decoded, catalog);
        assert_eq!(state.as_deref(), Some("{\"version\":1}"));
    }

    #[test]
    fn lexical_search_finds_command_and_option_intent() {
        let catalog = Catalog::builtin();
        let bytes = encode_database(&catalog, None).unwrap();
        let results = search(
            &bytes,
            Path::new("catalog.sqlite3"),
            "change directory",
            8,
            None,
        )
        .unwrap();
        assert!(results.iter().any(|result| result.command == "cd"));
    }

    #[test]
    fn interactive_search_session_reuses_one_validated_database_generation() {
        let bytes = encode_database(&Catalog::builtin(), None).unwrap();
        let session = SearchSession::open(&bytes, Path::new("catalog.sqlite3"), None).unwrap();
        let directory = session.search("change directory", 8).unwrap();
        let listing = session
            .search("list a directory as typed entries", 8)
            .unwrap();
        assert!(directory.iter().any(|result| result.command == "cd"));
        assert!(
            listing
                .iter()
                .any(|result| result.command == "quirl data ls")
        );
    }

    #[test]
    fn malformed_database_is_rejected_without_panicking() {
        let error = decode_database(b"not sqlite", Path::new("catalog.sqlite3")).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
    }

    #[test]
    fn oversized_query_is_rejected_before_database_work() {
        let query = "x".repeat(QUERY_BYTES_MAX + 1);
        let error = validate_query(&query, 8).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn current_embedding_readiness_requires_every_document_fingerprint() {
        let path = Path::new("catalog.sqlite3");
        let bytes = encode_database(&Catalog::builtin(), None).unwrap();
        assert!(!embeddings_are_current(&bytes, path).unwrap());
        let connection = deserialize_database(&bytes, path).unwrap();
        connection
            .execute(
                "INSERT INTO embeddings(document_id, model_id, dimensions, vector_le_f32, document_fingerprint) SELECT document_id, ?1, 1, x'00000000', fingerprint FROM semantic_documents",
                params![MODEL_ID],
            )
            .unwrap();
        let current = serialize_database(&connection).unwrap();
        assert!(embeddings_are_current(&current, path).unwrap());
        connection
            .execute(
                "UPDATE embeddings SET document_fingerprint = 'stale' WHERE document_id = (SELECT min(document_id) FROM embeddings)",
                [],
            )
            .unwrap();
        let stale = serialize_database(&connection).unwrap();
        assert!(!embeddings_are_current(&stale, path).unwrap());
    }

    fn local_record(insertion_text: &str) -> LocalCompletionRecord {
        LocalCompletionRecord {
            command_path: vec!["tool".to_owned()],
            kind: LocalCandidateKind::Subcommand,
            insertion_text: insertion_text.to_owned(),
            display_text: insertion_text.to_owned(),
            description: Some(format!("Use {insertion_text}")),
            provider: LocalCompletionProvider::Zsh,
            confidence: Confidence::High,
            trust: Trust::Declared,
            executable_fingerprint: "exe:v1".to_owned(),
            provider_fingerprint: "provider:v1".to_owned(),
            cwd_class: LocalCwdClass::Repository,
            environment_fingerprint: "env:v1".to_owned(),
            observed_unix_ms: 1_000,
            refreshed_unix_ms: 2_000,
            refresh_state: LocalRefreshState::Fresh,
        }
    }

    fn local_query(now_unix_ms: u64) -> LocalOverlayQuery {
        LocalOverlayQuery {
            native_catalog_fingerprint: crate::native_catalog::embedded_database_identity()
                .to_owned(),
            executable_fingerprint: "exe:v1".to_owned(),
            provider_fingerprint: "provider:v1".to_owned(),
            cwd_class: LocalCwdClass::Repository,
            environment_fingerprint: "env:v1".to_owned(),
            now_unix_ms,
        }
    }

    fn local_negative(observed_unix_ms: u64) -> LocalNegativeObservation {
        LocalNegativeObservation {
            command_path: vec!["missing".to_owned()],
            provider: LocalCompletionProvider::Fish,
            executable_fingerprint: "exe:v1".to_owned(),
            provider_fingerprint: "provider:v1".to_owned(),
            cwd_class: LocalCwdClass::Repository,
            environment_fingerprint: "env:v1".to_owned(),
            observed_unix_ms,
        }
    }

    #[test]
    fn local_overlay_round_trip_is_byte_stable_and_sorted() {
        let path = Path::new("catalog.sqlite3");
        let base = encode_database(&Catalog::builtin(), None).unwrap();
        let native = crate::native_catalog::embedded_database_identity();
        let left = merge_local_provider_result(
            &base,
            path,
            native,
            &[local_record("zebra"), local_record("alpha")],
        )
        .unwrap();
        let right = merge_local_provider_result(
            &base,
            path,
            native,
            &[local_record("alpha"), local_record("zebra")],
        )
        .unwrap();
        assert_eq!(left, right);
        let overlay = read_local_overlay(&left, path, &local_query(3_000)).unwrap();
        assert_eq!(overlay.records.len(), 2);
        assert_eq!(overlay.records[0].insertion_text, "alpha");
        assert_eq!(overlay.records[1].insertion_text, "zebra");
        assert_eq!(
            decode_database(&left, path).unwrap().0,
            Catalog::builtin(),
            "overlay updates must retain the canonical catalog generation",
        );
    }

    #[test]
    fn batched_overlay_reads_deduplicate_identities_and_enforce_the_query_bound() {
        let path = Path::new("catalog.sqlite3");
        let base = encode_database(&Catalog::builtin(), None).unwrap();
        let bytes = merge_local_provider_result(
            &base,
            path,
            crate::native_catalog::embedded_database_identity(),
            &[local_record("alpha")],
        )
        .unwrap();
        let query = local_query(3_000);
        let overlay = read_local_overlays(&bytes, path, &[query.clone(), query]).unwrap();
        assert_eq!(overlay.records.len(), 1);

        let excessive = vec![local_query(3_000); LOCAL_OVERLAY_QUERIES_MAX + 1];
        let error = read_local_overlays(&bytes, path, &excessive).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[test]
    fn catalog_reencoding_preserves_positive_and_negative_overlay_bytes() {
        let path = Path::new("catalog.sqlite3");
        let base = encode_database(&Catalog::builtin(), Some("{\"generation\":1}")).unwrap();
        let native = crate::native_catalog::embedded_database_identity();
        let positive =
            merge_local_provider_result(&base, path, native, &[local_record("alpha")]).unwrap();
        let prior =
            record_local_negative_hit(&positive, path, native, &local_negative(2_000)).unwrap();
        let fresh = encode_database(&Catalog::builtin(), Some("{\"generation\":2}")).unwrap();
        let preserved = preserve_local_overlay(&prior, path, &fresh, path).unwrap();
        let overlay = read_local_overlays(&preserved, path, &[local_query(3_000)]).unwrap();
        assert_eq!(overlay.records.len(), 1);
        assert_eq!(overlay.negative_hits.len(), 1);
        assert_eq!(
            decode_database(&preserved, path).unwrap().1.as_deref(),
            Some("{\"generation\":2}")
        );
    }

    #[test]
    fn local_overlay_rejects_malformed_rows_and_versions() {
        let path = Path::new("catalog.sqlite3");
        let base = encode_database(&Catalog::builtin(), None).unwrap();
        let bytes = merge_local_provider_result(
            &base,
            path,
            crate::native_catalog::embedded_database_identity(),
            &[local_record("alpha")],
        )
        .unwrap();
        let connection = deserialize_database(&bytes, path).unwrap();
        connection
            .execute(
                "UPDATE local_completion_records SET candidate_kind = 'unknown'",
                [],
            )
            .unwrap();
        let malformed = serialize_database(&connection).unwrap();
        assert_eq!(
            read_local_overlay(&malformed, path, &local_query(3_000))
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );

        let connection = deserialize_database(&base, path).unwrap();
        connection
            .execute("UPDATE local_overlay_identity SET schema_version = 999", [])
            .unwrap();
        let wrong_version = serialize_database(&connection).unwrap();
        assert_eq!(
            read_local_overlay(&wrong_version, path, &local_query(3_000))
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
    }

    #[test]
    fn local_overlay_invalidates_every_fingerprint_boundary() {
        let path = Path::new("catalog.sqlite3");
        let base = encode_database(&Catalog::builtin(), None).unwrap();
        let bytes = merge_local_provider_result(
            &base,
            path,
            crate::native_catalog::embedded_database_identity(),
            &[local_record("alpha")],
        )
        .unwrap();
        let mut executable = local_query(3_000);
        executable.executable_fingerprint = "exe:v2".to_owned();
        assert!(
            read_local_overlay(&bytes, path, &executable)
                .unwrap()
                .records
                .is_empty()
        );
        let mut provider = local_query(3_000);
        provider.provider_fingerprint = "provider:v2".to_owned();
        assert!(
            read_local_overlay(&bytes, path, &provider)
                .unwrap()
                .records
                .is_empty()
        );
        let mut native = local_query(3_000);
        native.native_catalog_fingerprint = "native:v2".to_owned();
        assert!(
            read_local_overlay(&bytes, path, &native)
                .unwrap()
                .records
                .is_empty()
        );
        let mut environment = local_query(3_000);
        environment.environment_fingerprint = "env:v2".to_owned();
        assert!(
            read_local_overlay(&bytes, path, &environment)
                .unwrap()
                .records
                .is_empty()
        );
    }

    #[test]
    fn negative_cache_backoff_and_expiry_use_supplied_time() {
        let path = Path::new("catalog.sqlite3");
        let native = crate::native_catalog::embedded_database_identity();
        let base = encode_database(&Catalog::builtin(), None).unwrap();
        let once = record_local_negative_hit(&base, path, native, &local_negative(10_000)).unwrap();
        let twice =
            record_local_negative_hit(&once, path, native, &local_negative(20_000)).unwrap();
        let current = read_local_overlay(&twice, path, &local_query(21_000)).unwrap();
        assert_eq!(current.negative_hits[0].failure_count, 2);
        assert_eq!(current.negative_hits[0].retry_after_unix_ms, 22_000);
        let expired = read_local_overlay(
            &twice,
            path,
            &local_query(20_000 + LOCAL_NEGATIVE_EXPIRY_MS),
        )
        .unwrap();
        assert!(expired.negative_hits.is_empty());
    }

    #[test]
    fn local_overlay_rejects_record_and_depth_overflow() {
        let path = Path::new("catalog.sqlite3");
        let base = encode_database(&Catalog::builtin(), None).unwrap();
        let native = crate::native_catalog::embedded_database_identity();
        let mut deep = local_record("alpha");
        deep.command_path = (0..=LOCAL_PATH_DEPTH_MAX)
            .map(|index| format!("s{index}"))
            .collect();
        assert_eq!(
            merge_local_provider_result(&base, path, native, &[deep])
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );

        let records = (0..=LOCAL_RECORDS_MAX)
            .map(|index| local_record(&format!("candidate-{index}")))
            .collect::<Vec<_>>();
        assert_eq!(
            merge_local_provider_result(&base, path, native, &records)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn description_composition_uses_exact_tiers_and_skips_generated_fallbacks() {
        use quirl_catalog::Provenance;

        let fact = |tier, source, text: &str| CompletionDescriptionFact {
            text: text.to_owned(),
            provenance: ProvenanceInfo::builtin(source),
            tier,
        };
        let facts = vec![
            fact(
                CompletionCompositionTier::PathOnly,
                Provenance::External,
                "PATH fallback",
            ),
            fact(
                CompletionCompositionTier::Man,
                Provenance::Man,
                "Manual description",
            ),
            fact(
                CompletionCompositionTier::Help,
                Provenance::Help,
                "Help description",
            ),
            fact(
                CompletionCompositionTier::LocalCompletion,
                Provenance::Zsh,
                "Local description",
            ),
            fact(
                CompletionCompositionTier::CentralImported,
                Provenance::Fish,
                "Central description",
            ),
            fact(
                CompletionCompositionTier::Curated,
                Provenance::External,
                "Curated description",
            ),
        ];
        let selected = compose_primary_description(&facts).unwrap();
        assert_eq!(selected.text, "Curated description");
        assert_eq!(selected.provenance.source, Provenance::External);

        let fallback = fact(
            CompletionCompositionTier::LocalCompletion,
            Provenance::Zsh,
            "Command discovered from Zsh completion metadata",
        );
        let useful = fact(
            CompletionCompositionTier::Help,
            Provenance::Help,
            "Useful help text",
        );
        let selected = compose_primary_description(&[fallback, useful.clone()]).unwrap();
        assert_eq!(selected, useful);
    }
}
