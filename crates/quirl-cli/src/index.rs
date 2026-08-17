use clap::{ArgAction, Subcommand, ValueEnum};
use quirl_catalog::{
    import_bash, import_fish, import_help, import_man, import_zsh, Catalog, ImportDiagnostic,
    ImportReport, Provenance,
};
use quirl_core::{
    escape_json_terminal_controls, escape_terminal_controls, replace_file_atomically,
    AtomicReplaceOptions, ErrorCode, ShellError,
};
use serde::Serialize;
use std::{
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

const INDEX_READ_LIMIT: usize = 4 * 1024 * 1024;
const COMPLETION_READ_LIMIT: usize = 4 * 1024 * 1024;
const DOCUMENTATION_READ_LIMIT: usize = 1024 * 1024;
const INDEX_ROOTS_MAX: usize = 128;
const INDEX_DIRECTORY_ENTRIES_MAX: usize = 8_192;
const INDEX_FILES_MAX: usize = 2_048;
const INDEX_PATH_BYTES_MAX: usize = 1024 * 1024;
const INDEX_SOURCE_BYTES_TOTAL_MAX: usize = 16 * 1024 * 1024;
const INDEX_RECORDS_MAX: usize = 65_536;
const INDEX_RETAINED_BYTES_MAX: usize = 16 * 1024 * 1024;
const INDEX_DIAGNOSTICS_MAX: usize = 4_096;
const INDEX_TEMPORARY_ATTEMPTS_MAX: usize = 64;
static NEXT_INDEX_TEMPORARY: AtomicU64 = AtomicU64::new(0);

// Failure cleanup preserves every armed name because identity validation plus
// pathname unlink is racy. Only the bounded post-commit path removes the hidden
// temporary, under the explicit assumption that the containing namespace is
// cooperative for that final success cleanup.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexWriteStage {
    ContentSynced,
    Installed,
}

#[derive(Debug)]
struct IndexOwnedPath {
    path: PathBuf,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl IndexOwnedPath {
    fn from_file(path: PathBuf, file: &File) -> Result<Self, ShellError> {
        let metadata = file
            .metadata()
            .map_err(|error| index_io_error("inspect", &path, error))?;
        Ok(Self {
            path,
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    fn matches(&self, path: &Path) -> bool {
        fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.dev() == self.device && metadata.ino() == self.inode)
    }

    #[cfg(not(unix))]
    fn matches(&self, _path: &Path) -> bool {
        false
    }

    fn remove_committed(&self) -> io::Result<()> {
        // This is success-only cleanup of one bounded transaction name. The
        // containing namespace must remain cooperative during this final unlink.
        fs::remove_file(&self.path)
    }
}

struct IndexTemporary {
    temporary: Option<IndexOwnedPath>,
    destination: Option<PathBuf>,
}

impl IndexTemporary {
    fn new(path: PathBuf, file: &File) -> Result<Self, ShellError> {
        Ok(Self {
            temporary: Some(IndexOwnedPath::from_file(path, file)?),
            destination: None,
        })
    }

    fn path(&self) -> &Path {
        self.temporary
            .as_ref()
            .map(IndexOwnedPath::path)
            .unwrap_or_else(|| Path::new("<removed-index-temporary>"))
    }

    fn installed(&mut self, path: &Path) {
        self.destination = Some(path.to_path_buf());
    }

    fn owns(&self, path: &Path) -> bool {
        self.temporary
            .as_ref()
            .is_some_and(|temporary| temporary.matches(path))
    }

    fn cleanup(&mut self, mut error: ShellError) -> ShellError {
        if let Some(destination) = self.destination.take() {
            error = error.with_context(format!(
                "failure cleanup preserved installed index {}",
                destination.display()
            ));
        }
        if let Some(temporary) = self.temporary.take() {
            error = error.with_context(format!(
                "failure cleanup preserved index temporary {}",
                temporary.path().display()
            ));
        }
        error
    }

    fn disarm(&mut self) {
        self.temporary = None;
        self.destination = None;
    }
}

impl Drop for IndexTemporary {
    fn drop(&mut self) {
        self.destination = None;
        self.temporary = None;
    }
}

#[derive(Clone, Copy)]
struct IndexBounds {
    roots_max: usize,
    entries_max: usize,
    files_max: usize,
    path_bytes_max: usize,
    source_bytes_max: usize,
    records_max: usize,
    retained_bytes_max: usize,
    diagnostics_max: usize,
}

impl IndexBounds {
    const PRODUCTION: Self = Self {
        roots_max: INDEX_ROOTS_MAX,
        entries_max: INDEX_DIRECTORY_ENTRIES_MAX,
        files_max: INDEX_FILES_MAX,
        path_bytes_max: INDEX_PATH_BYTES_MAX,
        source_bytes_max: INDEX_SOURCE_BYTES_TOTAL_MAX,
        records_max: INDEX_RECORDS_MAX,
        retained_bytes_max: INDEX_RETAINED_BYTES_MAX,
        diagnostics_max: INDEX_DIAGNOSTICS_MAX,
    };
}

struct IndexBuildBudget {
    bounds: IndexBounds,
    roots: usize,
    entries: usize,
    files: usize,
    path_bytes: usize,
    source_bytes: usize,
    records: usize,
    retained_bytes: usize,
    diagnostics: usize,
}

impl IndexBuildBudget {
    fn new(bounds: IndexBounds) -> Self {
        Self {
            bounds,
            roots: 0,
            entries: 0,
            files: 0,
            path_bytes: 0,
            source_bytes: 0,
            records: 0,
            retained_bytes: 0,
            diagnostics: 0,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum IndexCommand {
    /// Import completion declarations and atomically write the catalog index.
    #[command(disable_help_flag = true)]
    Build {
        /// Print build command help. Long --help is reserved for help-text inputs.
        #[arg(short = 'h', action = ArgAction::Help)]
        usage_help: Option<bool>,
        /// Fish completion file or directory. Repeat to import several roots.
        #[arg(long)]
        fish: Vec<PathBuf>,
        /// Bash completion file or directory. Repeat to import several roots.
        #[arg(long)]
        bash: Vec<PathBuf>,
        /// Zsh completion file or directory. Repeat to import several roots.
        #[arg(long)]
        zsh: Vec<PathBuf>,
        /// Supplied command-help text file or directory. Never executes a command.
        #[arg(long = "help", value_name = "PATH")]
        help_sources: Vec<PathBuf>,
        /// Supplied rendered/raw man text file or directory. Never invokes man.
        #[arg(long, value_name = "PATH")]
        man: Vec<PathBuf>,
        /// Index destination. Defaults to Quirl's user cache directory.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Output representation for the build report.
        #[arg(long, value_enum, default_value_t = IndexOutputFormat::Text)]
        format: IndexOutputFormat,
    },
    /// Explain the provenance of a command and each retained option.
    Explain {
        /// Space-separated command path, for example `git commit`.
        #[arg(required = true, num_args = 1..)]
        command: Vec<String>,
        /// Read a specific index instead of Quirl's user cache.
        #[arg(long)]
        index: Option<PathBuf>,
        /// Output representation for the provenance explanation.
        #[arg(long, value_enum, default_value_t = IndexOutputFormat::Text)]
        format: IndexOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum IndexOutputFormat {
    Text,
    Json,
}

pub fn wants_json(command: &IndexCommand) -> bool {
    matches!(
        command,
        IndexCommand::Build {
            format: IndexOutputFormat::Json,
            ..
        } | IndexCommand::Explain {
            format: IndexOutputFormat::Json,
            ..
        }
    )
}

#[derive(Debug, Serialize)]
struct BuildReport {
    index: PathBuf,
    source_files: usize,
    commands: usize,
    options: usize,
    diagnostics: Vec<ImportDiagnostic>,
}

pub fn execute(command: IndexCommand) -> Result<i32, ShellError> {
    match command {
        IndexCommand::Build {
            usage_help: _,
            fish,
            bash,
            zsh,
            help_sources,
            man,
            output,
            format,
        } => build_index(fish, bash, zsh, help_sources, man, output, format),
        IndexCommand::Explain {
            command,
            index,
            format,
        } => explain_index(&command.join(" "), index, format),
    }
}

/// Load the default attributed index for completion/help consumers. Cached
/// imported facts augment, but can never replace, the builtins compiled into
/// this binary. A missing, unreadable, corrupt, or incompatible cache is
/// recoverable and falls back to those builtins.
pub fn load_default_catalog() -> Catalog {
    let Some(path) = default_index_path() else {
        return Catalog::builtin();
    };
    load_catalog_at(&path)
}

fn load_catalog_at(path: &Path) -> Catalog {
    match read_index(path) {
        Ok(source) => decode_catalog(&source, path)
            .map(merge_cached_catalog)
            .unwrap_or_else(|_| Catalog::builtin()),
        Err(_) => Catalog::builtin(),
    }
}

fn merge_cached_catalog(mut cached: Catalog) -> Catalog {
    // The index cache contains imported discovery facts, not authenticated
    // installation state. Only the validated plugin lock snapshot may confer
    // plugin provenance and make a command eligible for agent execution.
    cached
        .commands
        .retain(|command| command.provenance.source != Provenance::Plugin);
    let mut current = Catalog::builtin();
    current.merge(cached.commands);
    current
}

fn build_index(
    fish_roots: Vec<PathBuf>,
    bash_roots: Vec<PathBuf>,
    zsh_roots: Vec<PathBuf>,
    help_roots: Vec<PathBuf>,
    man_roots: Vec<PathBuf>,
    output: Option<PathBuf>,
    format: IndexOutputFormat,
) -> Result<i32, ShellError> {
    let fish_roots = if fish_roots.is_empty() {
        default_fish_roots()
    } else {
        fish_roots
    };
    let bash_roots = if bash_roots.is_empty() {
        default_bash_roots()
    } else {
        bash_roots
    };
    let zsh_roots = if zsh_roots.is_empty() {
        default_zsh_roots()
    } else {
        zsh_roots
    };
    let mut budget = IndexBuildBudget::new(IndexBounds::PRODUCTION);
    budget.roots = fish_roots
        .len()
        .saturating_add(bash_roots.len())
        .saturating_add(zsh_roots.len())
        .saturating_add(help_roots.len())
        .saturating_add(man_roots.len());
    ensure_index_limit("roots", budget.bounds.roots_max, budget.roots)?;
    let fish_files = completion_files(&fish_roots, Some("fish"), &mut budget)?;
    let bash_files = completion_files(&bash_roots, None, &mut budget)?;
    let zsh_files = completion_files(&zsh_roots, None, &mut budget)?;
    let help_files = completion_files(&help_roots, None, &mut budget)?;
    let man_files = completion_files(&man_roots, None, &mut budget)?;
    let (catalog, diagnostics) = catalog_from_files(
        &fish_files,
        &bash_files,
        &zsh_files,
        &help_files,
        &man_files,
        &mut budget,
    )?;
    let output = output.or_else(default_index_path).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "cannot determine a completion-index path",
        )
        .with_help("Pass an explicit destination with `quirl index build --output <path>`")
    })?;
    write_catalog_atomically(&output, &catalog)?;
    let report = BuildReport {
        index: output,
        source_files: fish_files.len()
            + bash_files.len()
            + zsh_files.len()
            + help_files.len()
            + man_files.len(),
        commands: catalog.commands.len(),
        options: catalog
            .commands
            .iter()
            .map(|command| command.options.len())
            .sum(),
        diagnostics,
    };
    match format {
        IndexOutputFormat::Text => {
            println!(
                "indexed {} commands and {} options from {} files into {}",
                report.commands,
                report.options,
                report.source_files,
                escape_terminal_controls(&report.index.display().to_string())
            );
            for diagnostic in &report.diagnostics {
                eprintln!(
                    "{}:{}: skipped completion declaration: {}",
                    escape_terminal_controls(&diagnostic.origin),
                    diagnostic.line,
                    escape_terminal_controls(&diagnostic.message)
                );
            }
        }
        IndexOutputFormat::Json => print_json(&report)?,
    }
    Ok(0)
}

fn explain_index(
    command: &str,
    index: Option<PathBuf>,
    format: IndexOutputFormat,
) -> Result<i32, ShellError> {
    let path = index.or_else(default_index_path).ok_or_else(|| {
        ShellError::new(ErrorCode::InvalidArgument, "cannot determine an index path")
            .with_help("Pass `--index <path>` or configure HOME/XDG_CACHE_HOME")
    })?;
    let source = read_index(&path)
        .map_err(|error| error.with_help("Build the index first with `quirl index build`"))?;
    let catalog = decode_catalog(&source, &path)?;
    let explanation = catalog.explain(command).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidCommand,
            format!("the completion index has no command `{command}`"),
        )
        .with_help("Run `quirl index build` to refresh installed completion metadata")
    })?;
    match format {
        IndexOutputFormat::Json => print_json(&explanation)?,
        IndexOutputFormat::Text => {
            println!("{}", escape_terminal_controls(&explanation.command));
            for fact in explanation.facts {
                let origin = fact
                    .provenance
                    .origin
                    .as_deref()
                    .unwrap_or("compiled into Quirl");
                let fingerprint = fact
                    .provenance
                    .fingerprint
                    .as_deref()
                    .map_or(String::new(), |value| format!(" · {value}"));
                println!(
                    "  {} `{}` ← {:?} / {:?} · {}{}",
                    escape_terminal_controls(&fact.fact),
                    escape_terminal_controls(&fact.value),
                    fact.provenance.source,
                    fact.provenance.confidence,
                    escape_terminal_controls(origin),
                    escape_terminal_controls(&fingerprint)
                );
            }
        }
    }
    Ok(0)
}

fn catalog_from_files(
    fish_files: &[PathBuf],
    bash_files: &[PathBuf],
    zsh_files: &[PathBuf],
    help_files: &[PathBuf],
    man_files: &[PathBuf],
    budget: &mut IndexBuildBudget,
) -> Result<(Catalog, Vec<ImportDiagnostic>), ShellError> {
    let mut catalog = Catalog::builtin();
    let mut diagnostics = Vec::new();
    for path in fish_files {
        let source = read_completion(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_fish(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in bash_files {
        let source = read_completion(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_bash(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in zsh_files {
        let source = read_completion(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_zsh(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in help_files {
        let source = read_documentation(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_help(&source, &path.display().to_string()),
            budget,
        )?;
    }
    for path in man_files {
        let source = read_documentation(path, budget)?;
        merge_bounded_report(
            &mut catalog,
            &mut diagnostics,
            import_man(&source, &path.display().to_string()),
            budget,
        )?;
    }
    Ok((catalog, diagnostics))
}

fn merge_bounded_report(
    catalog: &mut Catalog,
    diagnostics: &mut Vec<ImportDiagnostic>,
    report: ImportReport,
    budget: &mut IndexBuildBudget,
) -> Result<(), ShellError> {
    let report_records = report.commands.iter().fold(0_usize, |count, command| {
        count
            .saturating_add(1)
            .saturating_add(command.options.len())
    });
    let records = budget.records.saturating_add(report_records);
    ensure_index_limit("catalog records", budget.bounds.records_max, records)?;
    let diagnostic_count = budget.diagnostics.saturating_add(report.diagnostics.len());
    ensure_index_limit(
        "import diagnostics",
        budget.bounds.diagnostics_max,
        diagnostic_count,
    )?;
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, &report).map_err(json_error)?;
    let retained_bytes = budget.retained_bytes.saturating_add(counter.0);
    ensure_index_limit(
        "retained index text",
        budget.bounds.retained_bytes_max,
        retained_bytes,
    )?;

    budget.records = records;
    budget.diagnostics = diagnostic_count;
    budget.retained_bytes = retained_bytes;
    diagnostics.extend(catalog.merge_report(report));
    Ok(())
}

struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct BoundedBytesWriter {
    bytes: Vec<u8>,
    bytes_max: usize,
    exceeded: bool,
}

impl BoundedBytesWriter {
    fn new(bytes_max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            bytes_max,
            exceeded: false,
        }
    }
}

impl Write for BoundedBytesWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.bytes_max.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            self.bytes.extend_from_slice(&bytes[..remaining]);
            self.exceeded = true;
            return Err(io::Error::other("bounded index output exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn admit_index_path(path: &Path, budget: &mut IndexBuildBudget) -> Result<(), ShellError> {
    let files = budget.files.saturating_add(1);
    ensure_index_limit("source files", budget.bounds.files_max, files)?;
    let path_bytes = budget
        .path_bytes
        .saturating_add(path.as_os_str().as_encoded_bytes().len());
    ensure_index_limit(
        "retained path bytes",
        budget.bounds.path_bytes_max,
        path_bytes,
    )?;
    budget.files = files;
    budget.path_bytes = path_bytes;
    Ok(())
}

fn admit_source_bytes(bytes: usize, budget: &mut IndexBuildBudget) -> Result<(), ShellError> {
    let source_bytes = budget.source_bytes.saturating_add(bytes);
    ensure_index_limit("source bytes", budget.bounds.source_bytes_max, source_bytes)?;
    budget.source_bytes = source_bytes;
    Ok(())
}

fn ensure_index_limit(kind: &str, limit: usize, observed: usize) -> Result<(), ShellError> {
    if observed <= limit {
        Ok(())
    } else {
        Err(index_limit_error(kind, limit, observed))
    }
}

fn index_limit_error(kind: &str, limit: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("completion index exceeds its {kind} limit"),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help("Reduce the number or size of index sources and retry")
}

fn nonregular_index_input(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "completion index input {} is not a regular file or directory",
            path.display()
        ),
    )
    .with_help("Remove symlinks and special files from index input roots")
}

fn completion_files(
    roots: &[PathBuf],
    required_extension: Option<&str>,
    budget: &mut IndexBuildBudget,
) -> Result<Vec<PathBuf>, ShellError> {
    let mut files = Vec::new();
    for root in roots {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_file() => {
                admit_index_path(root, budget)?;
                files.push(root.clone());
            }
            Ok(metadata) if metadata.file_type().is_dir() => {
                let entries =
                    fs::read_dir(root).map_err(|error| index_io_error("enumerate", root, error))?;
                for entry in entries {
                    budget.entries = budget.entries.saturating_add(1);
                    ensure_index_limit(
                        "directory entries",
                        budget.bounds.entries_max,
                        budget.entries,
                    )?;
                    let entry = entry.map_err(|error| index_io_error("enumerate", root, error))?;
                    let path = entry.path();
                    let kind = entry
                        .file_type()
                        .map_err(|error| index_io_error("inspect", &path, error))?;
                    if !kind.is_file() {
                        if kind.is_symlink() {
                            return Err(nonregular_index_input(&path));
                        }
                        continue;
                    }
                    if required_extension.is_none_or(|extension| {
                        path.extension()
                            .is_some_and(|candidate| candidate == extension)
                    }) {
                        admit_index_path(&path, budget)?;
                        files.push(path);
                    }
                }
            }
            Ok(_) => return Err(nonregular_index_input(root)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(index_io_error("inspect", root, error)),
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn default_fish_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/usr/share/fish/completions"),
        PathBuf::from("/usr/share/fish/vendor_completions.d"),
        PathBuf::from("/opt/homebrew/share/fish/completions"),
        PathBuf::from("/opt/homebrew/share/fish/vendor_completions.d"),
    ];
    if let Some(config) = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from) {
        roots.push(config.join("fish/completions"));
    } else if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".config/fish/completions"));
    }
    roots
}

fn default_bash_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/bash-completion/completions"),
        PathBuf::from("/etc/bash_completion.d"),
        PathBuf::from("/opt/homebrew/etc/bash_completion.d"),
        PathBuf::from("/usr/local/etc/bash_completion.d"),
    ]
}

fn default_zsh_roots() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/usr/share/zsh/site-functions"),
        PathBuf::from("/usr/local/share/zsh/site-functions"),
        PathBuf::from("/opt/homebrew/share/zsh/site-functions"),
    ]
}

fn default_index_path() -> Option<PathBuf> {
    if let Some(path) = env::var_os("QUIRL_INDEX_PATH") {
        return Some(PathBuf::from(path));
    }
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|cache| cache.join("quirl/catalog.json"))
}

fn read_completion(path: &Path, budget: &mut IndexBuildBudget) -> Result<String, ShellError> {
    let source = read_index_utf8(
        path,
        COMPLETION_READ_LIMIT,
        "completion source",
        "Supply completion declarations in a readable UTF-8 regular file at or below 4 MiB",
    )?;
    admit_source_bytes(source.len(), budget)?;
    Ok(source)
}

fn read_documentation(path: &Path, budget: &mut IndexBuildBudget) -> Result<String, ShellError> {
    let source = read_index_utf8(
        path,
        DOCUMENTATION_READ_LIMIT,
        "documentation source",
        "Supply help or man text in a readable UTF-8 regular file at or below 1 MiB",
    )?;
    admit_source_bytes(source.len(), budget)?;
    Ok(source)
}

fn read_index(path: &Path) -> Result<String, ShellError> {
    read_index_utf8(
        path,
        INDEX_READ_LIMIT,
        "completion index",
        "Build a readable regular index file at or below 4 MiB with `quirl index build`",
    )
}

fn read_index_utf8(
    path: &Path,
    bytes_max: usize,
    context: &str,
    help: &str,
) -> Result<String, ShellError> {
    let path_metadata = fs::symlink_metadata(path).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not inspect {context} {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })?;
    validate_index_reader_metadata(path, &path_metadata, help)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    let file = options.open(path).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not open {context} {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })?;
    let metadata = file.metadata().map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            format!("could not inspect {context} {}", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })?;
    validate_index_reader_metadata(path, &metadata, help)?;
    #[cfg(unix)]
    if path_metadata.dev() != metadata.dev() || path_metadata.ino() != metadata.ino() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "completion index {} changed during admission",
                path.display()
            ),
        )
        .with_help(help));
    }
    let bytes_max_u64 = u64::try_from(bytes_max).unwrap_or(u64::MAX);
    if metadata.len() > bytes_max_u64 {
        return Err(index_read_limit_error(
            path,
            context,
            help,
            bytes_max,
            metadata.len(),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(bytes_max)
            .min(bytes_max),
    );
    file.take(bytes_max_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| index_io_error("read", path, error))?;
    if bytes.len() > bytes_max {
        return Err(index_read_limit_error(
            path,
            context,
            help,
            bytes_max,
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("{} is not UTF-8 {context} text", path.display()),
        )
        .with_context(error.to_string())
        .with_help(help)
    })
}

fn validate_index_reader_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    help: &str,
) -> Result<(), ShellError> {
    if !metadata.file_type().is_file() {
        return Err(nonregular_index_input(path).with_help(help));
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != 1 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("completion index {} has hard-link aliases", path.display()),
            )
            .with_context(format!("expected links: 1; observed: {}", metadata.nlink()))
            .with_help(help));
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "completion index {} has unsafe writable permissions",
                    path.display()
                ),
            )
            .with_context(format!("mode: {mode:#o}; forbidden write bits: 0o022"))
            .with_help(help));
        }
    }
    Ok(())
}

fn index_read_limit_error(
    path: &Path,
    context: &str,
    help: &str,
    limit: usize,
    observed: u64,
) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{context} {} exceeds its read limit", path.display()),
    )
    .with_context(format!("limit: {limit}; observed: {observed}"))
    .with_help(help)
}

fn decode_catalog(source: &str, path: &Path) -> Result<Catalog, ShellError> {
    let catalog = Catalog::from_json(source).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("{} is not a valid Quirl completion index", path.display()),
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
    Ok(catalog)
}

fn write_catalog_atomically(path: &Path, catalog: &Catalog) -> Result<(), ShellError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        create_index_directories(parent)?;
    }
    let mut writer = BoundedBytesWriter::new(INDEX_READ_LIMIT);
    if let Err(error) = serde_json::to_writer_pretty(&mut writer, catalog) {
        if writer.exceeded {
            return Err(index_limit_error(
                "serialized bytes",
                INDEX_READ_LIMIT,
                INDEX_READ_LIMIT.saturating_add(1),
            ));
        }
        return Err(json_error(error));
    }
    let encoded = writer.bytes;
    if encoded.len() > INDEX_READ_LIMIT {
        return Err(index_limit_error(
            "serialized bytes",
            INDEX_READ_LIMIT,
            encoded.len(),
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_index_reader_metadata(
                path,
                &metadata,
                "Use an unlinked regular index file with no group/other write access",
            )?;
            let expected = read_index_utf8(
                path,
                INDEX_READ_LIMIT,
                "completion index",
                "Use an unlinked regular index file at or below 4 MiB",
            )?;
            replace_file_atomically(
                path,
                expected.as_bytes(),
                &encoded,
                AtomicReplaceOptions {
                    bytes_max: INDEX_READ_LIMIT,
                },
            )
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            install_new_index(path, &encoded, parent.unwrap_or_else(|| Path::new(".")))
        }
        Err(error) => Err(index_io_error("inspect", path, error)),
    }
}

fn install_new_index(path: &Path, encoded: &[u8], parent: &Path) -> Result<(), ShellError> {
    install_new_index_with_hook(path, encoded, parent, |_| Ok(()))
}

fn install_new_index_with_hook(
    path: &Path,
    encoded: &[u8],
    parent: &Path,
    mut after_stage: impl FnMut(IndexWriteStage) -> io::Result<()>,
) -> Result<(), ShellError> {
    let (temporary, mut file) = create_index_temporary(path)?;
    let mut guard = IndexTemporary::new(temporary.clone(), &file).map_err(|error| {
        error.with_context(format!(
            "failure cleanup preserved index temporary {}",
            temporary.display()
        ))
    })?;
    let split = encoded.len().div_ceil(2);
    file.write_all(&encoded[..split])
        .and_then(|()| file.write_all(&encoded[split..]))
        .and_then(|()| file.sync_all())
        .and_then(|()| after_stage(IndexWriteStage::ContentSynced))
        .map_err(|error| guard.cleanup(index_io_error("write", guard.path(), error)))?;
    validate_index_temporary(guard.path(), &file).map_err(|error| guard.cleanup(error))?;
    drop(file);
    fs::hard_link(guard.path(), path)
        .map_err(|error| guard.cleanup(index_io_error("install", path, error)))?;
    guard.installed(path);
    if let Err(error) = after_stage(IndexWriteStage::Installed) {
        return Err(guard.cleanup(index_io_error("install", path, error)));
    }
    if !guard.owns(path) {
        let error = ShellError::new(
            ErrorCode::Validation,
            format!(
                "index destination {} changed during installation",
                path.display()
            ),
        )
        .with_help("Remove the conflicting index entry and retry");
        return Err(guard.cleanup(error));
    }
    validate_index_installed(path).map_err(|error| guard.cleanup(error))?;
    if let Err(error) = sync_index_directory(parent) {
        return Err(guard.cleanup(error));
    }
    guard
        .temporary
        .as_ref()
        .map(IndexOwnedPath::remove_committed)
        .transpose()
        .map_err(|error| guard.cleanup(index_io_error("clean", guard.path(), error)))?;
    guard.disarm();
    let _ = sync_index_directory(parent);
    Ok(())
}

fn create_index_temporary(path: &Path) -> Result<(PathBuf, File), ShellError> {
    let name = path.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "completion index has no file name",
        )
        .with_help("Choose a regular index destination file")
    })?;
    for _ in 0..INDEX_TEMPORARY_ATTEMPTS_MAX {
        let sequence = NEXT_INDEX_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".quirl-{}-{sequence}.tmp", std::process::id()));
        let temporary = path.with_file_name(temporary_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary) {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                    return Err(
                        index_io_error("secure", &temporary, error).with_context(format!(
                            "failure cleanup preserved index temporary {}",
                            temporary.display()
                        )),
                    );
                }
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(index_io_error("create", &temporary, error)),
        }
    }
    Err(index_limit_error(
        "temporary-name attempts",
        INDEX_TEMPORARY_ATTEMPTS_MAX,
        INDEX_TEMPORARY_ATTEMPTS_MAX,
    ))
}

fn validate_index_temporary(path: &Path, file: &File) -> Result<(), ShellError> {
    let path_metadata =
        fs::symlink_metadata(path).map_err(|error| index_io_error("inspect", path, error))?;
    let file_metadata = file
        .metadata()
        .map_err(|error| index_io_error("inspect", path, error))?;
    validate_index_reader_metadata(
        path,
        &path_metadata,
        "Remove the conflicting index temporary and retry",
    )?;
    #[cfg(unix)]
    {
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "index temporary {} changed before installation",
                    path.display()
                ),
            )
            .with_help("Remove the conflicting index temporary and retry"));
        }
        let mode = file_metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!("index temporary {} has unsafe permissions", path.display()),
            )
            .with_context(format!("expected mode: 0o600; observed mode: {mode:#o}"))
            .with_help("Remove the conflicting index temporary and retry"));
        }
    }
    Ok(())
}

fn validate_index_installed(path: &Path) -> Result<(), ShellError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| index_io_error("inspect", path, error))?;
    if !metadata.file_type().is_file() {
        return Err(nonregular_index_input(path));
    }
    #[cfg(unix)]
    {
        if metadata.nlink() != 2 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "index destination {} changed during installation",
                    path.display()
                ),
            )
            .with_context(format!("expected links: 2; observed: {}", metadata.nlink()))
            .with_help("Remove the conflicting index entry and retry"));
        }
        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "index destination {} has unsafe permissions",
                    path.display()
                ),
            )
            .with_context(format!("expected mode: 0o600; observed mode: {mode:#o}"))
            .with_help("Remove the conflicting index entry and retry"));
        }
    }
    Ok(())
}

fn create_index_directories(directory: &Path) -> Result<(), ShellError> {
    const DEPTH_MAX: usize = 64;
    let mut missing = Vec::new();
    let mut cursor = directory;
    loop {
        match fs::symlink_metadata(cursor) {
            Ok(metadata) if metadata.file_type().is_dir() => break,
            Ok(_) => return Err(nonregular_index_input(cursor)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if missing.len() >= DEPTH_MAX {
                    return Err(index_limit_error(
                        "output directory depth",
                        DEPTH_MAX,
                        missing.len() + 1,
                    ));
                }
                missing.push(cursor.to_path_buf());
                cursor = cursor
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
            }
            Err(error) => return Err(index_io_error("inspect", cursor, error)),
        }
    }
    let mut created = Vec::<PathBuf>::new();
    for path in missing.into_iter().rev() {
        if let Err(error) = fs::create_dir(&path) {
            let mut shell_error = index_io_error("create", &path, error);
            while let Some(created_path) = created.pop() {
                shell_error = shell_error.with_context(format!(
                    "index directory {} was preserved because cleanup cannot atomically prove path ownership",
                    created_path.display()
                ));
            }
            return Err(shell_error);
        }
        created.push(path);
    }
    Ok(())
}

#[cfg(unix)]
fn sync_index_directory(path: &Path) -> Result<(), ShellError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| index_io_error("synchronize", path, error))
}

#[cfg(not(unix))]
fn sync_index_directory(_path: &Path) -> Result<(), ShellError> {
    Ok(())
}

fn index_io_error(action: &str, path: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("cannot {action} completion index source {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Check that the path exists and is readable by the current user")
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not serialize completion index data")
        .with_context(error.to_string())
}

fn print_json(value: &impl Serialize) -> Result<(), ShellError> {
    let json = serde_json::to_string_pretty(value).map_err(json_error)?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use quirl_catalog::Provenance;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, Parser)]
    struct IndexCli {
        #[command(subcommand)]
        command: IndexCommand,
    }

    fn temporary_directory() -> PathBuf {
        let path = env::temp_dir().join(format!(
            "quirl-index-test-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_budget() -> IndexBuildBudget {
        IndexBuildBudget::new(IndexBounds::PRODUCTION)
    }

    #[test]
    fn file_imports_merge_fish_and_bash_into_one_catalog() {
        let directory = temporary_directory();
        let fish = directory.join("demo.fish");
        let bash = directory.join("demo.bash");
        fs::write(&fish, "complete -c demo -l fish-option").unwrap();
        fs::write(&bash, "complete -W '--bash-option' demo").unwrap();
        let (catalog, diagnostics) = catalog_from_files(
            std::slice::from_ref(&fish),
            std::slice::from_ref(&bash),
            &[],
            &[],
            &[],
            &mut test_budget(),
        )
        .unwrap();
        assert!(diagnostics.is_empty());
        let explanation = catalog.explain("demo").unwrap();
        assert!(explanation
            .facts
            .iter()
            .any(|fact| fact.provenance.source == Provenance::Fish));
        assert!(explanation
            .facts
            .iter()
            .any(|fact| fact.provenance.source == Provenance::Bash));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn zsh_help_and_man_files_merge_with_attributed_facts() {
        let directory = temporary_directory();
        let zsh = directory.join("_ship");
        let help = directory.join("serve.help");
        let man = directory.join("inspect.man");
        fs::write(&zsh, "#compdef ship\n_arguments '--port=[Port]:port:'\n").unwrap();
        fs::write(&help, "Usage: serve [OPTIONS]\n  --listen ADDR  Address\n").unwrap();
        fs::write(&man, ".SH SYNOPSIS\ninspect [OPTIONS]\n.B \\--json\n").unwrap();
        let (catalog, diagnostics) = catalog_from_files(
            &[],
            &[],
            std::slice::from_ref(&zsh),
            std::slice::from_ref(&help),
            std::slice::from_ref(&man),
            &mut test_budget(),
        )
        .unwrap();
        assert!(diagnostics.is_empty());
        for (command, provenance) in [
            ("ship", Provenance::Zsh),
            ("serve", Provenance::Help),
            ("inspect", Provenance::Man),
        ] {
            assert!(catalog
                .explain(command)
                .unwrap()
                .facts
                .iter()
                .any(|fact| fact.provenance.source == provenance));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn build_accepts_repeatable_help_man_and_zsh_inputs() {
        let cli = IndexCli::try_parse_from([
            "index", "build", "--zsh", "_one", "--zsh", "_two", "--help", "one.help", "--help",
            "two.help", "--man", "one.man",
        ])
        .unwrap();
        let IndexCommand::Build {
            zsh,
            help_sources,
            man,
            ..
        } = cli.command
        else {
            panic!("expected build command");
        };
        assert_eq!(zsh, [PathBuf::from("_one"), PathBuf::from("_two")]);
        assert_eq!(
            help_sources,
            [PathBuf::from("one.help"), PathBuf::from("two.help")]
        );
        assert_eq!(man, [PathBuf::from("one.man")]);
    }

    #[test]
    fn atomic_index_round_trips_and_checks_the_schema() {
        let directory = temporary_directory();
        let path = directory.join("catalog.json");
        let catalog = Catalog::builtin();
        write_catalog_atomically(&path, &catalog).unwrap();
        let source = fs::read_to_string(&path).unwrap();
        assert_eq!(decode_catalog(&source, &path).unwrap(), catalog);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn index_budget_accepts_exact_limits_and_rejects_limit_plus_one() {
        let bounds = IndexBounds {
            roots_max: 2,
            entries_max: 2,
            files_max: 2,
            path_bytes_max: 8,
            source_bytes_max: 4,
            records_max: 2,
            retained_bytes_max: 128,
            diagnostics_max: 2,
        };
        let mut budget = IndexBuildBudget::new(bounds);
        budget.roots = 2;
        ensure_index_limit("roots", bounds.roots_max, budget.roots).unwrap();
        admit_index_path(Path::new("one"), &mut budget).unwrap();
        admit_index_path(Path::new("two"), &mut budget).unwrap();
        admit_source_bytes(4, &mut budget).unwrap();

        assert_eq!(
            admit_index_path(Path::new("x"), &mut budget)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            admit_source_bytes(1, &mut budget).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            ensure_index_limit("roots", bounds.roots_max, 3)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn failed_index_install_preserves_collision_and_temporary() {
        let directory = temporary_directory();
        let path = directory.join("catalog.json");
        fs::write(&path, b"foreign").unwrap();

        let error = install_new_index(&path, b"new", &directory).unwrap_err();

        assert_eq!(error.code, ErrorCode::Io);
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_cleanup_preserves_a_concurrent_temporary_replacement() {
        let directory = temporary_directory();
        let path = directory.join("catalog.json");
        let moved = directory.join("moved-owned-temporary");

        let error = install_new_index_with_hook(&path, b"new", &directory, |stage| {
            if stage == IndexWriteStage::ContentSynced {
                let temporary = fs::read_dir(&directory)?
                    .next()
                    .ok_or_else(|| io::Error::other("temporary was not visible"))??
                    .path();
                fs::rename(&temporary, &moved)?;
                fs::write(&temporary, b"foreign")?;
                return Err(io::Error::other("injected temporary replacement"));
            }
            Ok(())
        })
        .unwrap_err();

        let replacement = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| entry != &moved)
            .unwrap();
        assert_eq!(fs::read(replacement).unwrap(), b"foreign");
        assert!(moved.exists());
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("failure cleanup preserved index temporary")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_cleanup_preserves_colliding_temporary_and_destination_entries() {
        let directory = temporary_directory();
        let path = directory.join("catalog.json");
        let moved_temporary = directory.join("moved-owned-temporary");
        let moved_destination = directory.join("moved-owned-destination");

        let error = install_new_index_with_hook(&path, b"new", &directory, |stage| {
            if stage == IndexWriteStage::Installed {
                let temporary = fs::read_dir(&directory)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .find(|entry| entry.as_ref().is_ok_and(|entry| entry != &path))
                    .ok_or_else(|| io::Error::other("temporary was not visible"))??;
                fs::rename(&temporary, &moved_temporary)?;
                fs::rename(&path, &moved_destination)?;
                fs::write(&temporary, b"foreign")?;
                fs::hard_link(&temporary, &path)?;
                return Err(io::Error::other("injected installed replacement"));
            }
            Ok(())
        })
        .unwrap_err();

        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        let replacement_temporary = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|entry| {
                entry != &path && entry != &moved_temporary && entry != &moved_destination
            })
            .unwrap();
        assert_eq!(fs::read(replacement_temporary).unwrap(), b"foreign");
        assert!(moved_temporary.exists());
        assert!(moved_destination.exists());
        assert!(
            error
                .details
                .context
                .iter()
                .filter(|context| context.contains("failure cleanup preserved"))
                .count()
                >= 2
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_reader_rejects_symlinks_hardlinks_and_special_files() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        use std::os::unix::fs::symlink;

        let directory = temporary_directory();
        let source = directory.join("source");
        fs::write(&source, b"{}").unwrap();
        let link = directory.join("link");
        symlink(&source, &link).unwrap();
        assert_eq!(read_index(&link).unwrap_err().code, ErrorCode::Validation);

        let alias = directory.join("alias");
        fs::hard_link(&source, &alias).unwrap();
        assert_eq!(read_index(&source).unwrap_err().code, ErrorCode::Validation);

        let socket = directory.join("socket");
        mkfifo(&socket, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert_eq!(read_index(&socket).unwrap_err().code, ErrorCode::Validation);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn index_cleanup_failure_retains_the_originating_error() {
        let directory = temporary_directory();
        let path = directory.join("temporary");
        let file = File::create(&path).unwrap();
        let mut guard = IndexTemporary::new(path.clone(), &file).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        let error = guard.cleanup(
            ShellError::new(ErrorCode::Io, "originating index failure")
                .with_context("injected primary failure"),
        );

        assert_eq!(error.message, "originating index failure");
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("injected primary failure")));
        assert!(error
            .details
            .context
            .iter()
            .any(|context| context.contains("failure cleanup preserved index temporary")));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_or_incompatible_default_cache_recovers_to_current_builtins() {
        let missing = load_catalog_at(Path::new("/definitely/missing/quirl-index.json"));
        assert!(missing.find("quirl run").is_some());

        let corrupt = load_catalog_at(Path::new("/dev/null"));
        assert!(corrupt.find("quirl run").is_some());

        let directory = temporary_directory();
        let path = directory.join("old-schema.json");
        let mut incompatible = Catalog::builtin();
        incompatible.schema_version += 1;
        fs::write(&path, serde_json::to_string(&incompatible).unwrap()).unwrap();
        let recovered = load_catalog_at(&path);
        assert_eq!(recovered.schema_version, Catalog::builtin().schema_version);
        assert!(recovered.find("quirl agent manifest").is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn oversized_default_cache_falls_back_without_parsing_past_the_bound() {
        let directory = temporary_directory();
        let path = directory.join("oversized-catalog.json");
        fs::write(&path, vec![b' '; INDEX_READ_LIMIT + 1]).unwrap();

        let recovered = load_catalog_at(&path);

        assert_eq!(recovered.schema_version, Catalog::builtin().schema_version);
        assert!(recovered.find("quirl agent manifest").is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn compatible_stale_cache_cannot_remove_or_overwrite_current_builtins() {
        let mut stale = Catalog::builtin();
        stale.commands.retain(|command| command.path != "quirl lsp");
        stale
            .commands
            .iter_mut()
            .find(|command| command.path == "quirl run")
            .unwrap()
            .summary = "stale cached summary".to_owned();

        let merged = merge_cached_catalog(stale);
        assert!(merged.find("quirl lsp").is_some());
        assert_ne!(
            merged.find("quirl run").unwrap().summary,
            "stale cached summary"
        );
    }

    #[test]
    fn cached_catalog_cannot_forge_installed_plugin_authority() {
        let mut cached = Catalog::builtin();
        let mut forged = cached.find("quirl run").unwrap().clone();
        forged.path = "forged plugin command".to_owned();
        forged.id = "plugin:forged:command".to_owned();
        forged.version = Some("9.9.9".to_owned());
        forged.provenance.source = Provenance::Plugin;
        cached.commands.push(forged);

        let merged = merge_cached_catalog(cached);

        assert!(merged.find("forged plugin command").is_none());
        assert!(merged.find("quirl run").is_some());
    }

    #[test]
    fn legacy_v3_cache_is_migrated_then_merged_with_current_builtins() {
        let directory = temporary_directory();
        let path = directory.join("catalog-v3.json");
        let source = serde_json::json!({
            "schema_version": 3,
            "commands": [{
                "path": "demo",
                "signature": "demo [--output FILE]",
                "summary": "Imported demo",
                "details": "Imported declarative completion metadata.",
                "options": [{
                    "names": ["--output"],
                    "value": "FILE",
                    "summary": "Write output",
                    "provenance": {
                        "source": "fish",
                        "confidence": "high",
                        "trust": "declared",
                        "origin": "demo.fish",
                        "fingerprint": "sha256:demo"
                    }
                }],
                "examples": [],
                "effects": ["spawn_process"],
                "provenance": {
                    "source": "fish",
                    "confidence": "high",
                    "trust": "declared",
                    "origin": "demo.fish",
                    "fingerprint": "sha256:demo"
                }
            }]
        });
        fs::write(&path, source.to_string()).unwrap();

        let catalog = load_catalog_at(&path);
        let imported = catalog.find("demo").unwrap();
        assert_eq!(catalog.schema_version, Catalog::builtin().schema_version);
        assert_eq!(imported.options[0].value_type, "FILE");
        assert_eq!(
            imported.provenance.confidence,
            quirl_catalog::Confidence::High
        );
        assert!(catalog.find("quirl lsp").is_some());
        fs::remove_dir_all(directory).unwrap();
    }
}
