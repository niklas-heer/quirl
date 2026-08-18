//! Bounded local command intelligence backed by SQLite and Model2Vec.

use model2vec_rs::model::StaticModel;
use quirl_catalog::{ArgumentKind, Catalog, CompletionSource};
use quirl_core::{ErrorCode, ShellError};
use rusqlite::{Connection, MAIN_DB, Transaction, limits::Limit, params, serialize::Data};
use serde::Serialize;
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
const DATABASE_SCHEMA_VERSION: i64 = 1;
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

const SCHEMA: &str = r#"
PRAGMA application_id = 1364543826;
PRAGMA user_version = 1;
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
"#;

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
}
