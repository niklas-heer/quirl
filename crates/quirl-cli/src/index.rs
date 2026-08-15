use clap::{ArgAction, Subcommand, ValueEnum};
use quirl_catalog::{
    import_bash, import_fish, import_help, import_man, import_zsh, Catalog, ImportDiagnostic,
};
use quirl_core::{ErrorCode, ShellError};
use serde::Serialize;
use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

const DOCUMENTATION_READ_LIMIT: u64 = 1024 * 1024 + 1;

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
    match fs::read_to_string(path) {
        Ok(source) => decode_catalog(&source, path)
            .map(merge_cached_catalog)
            .unwrap_or_else(|_| Catalog::builtin()),
        Err(_) => Catalog::builtin(),
    }
}

fn merge_cached_catalog(cached: Catalog) -> Catalog {
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
    let fish_files = completion_files(&fish_roots, Some("fish"))?;
    let bash_files = completion_files(&bash_roots, None)?;
    let zsh_files = completion_files(&zsh_roots, None)?;
    let help_files = completion_files(&help_roots, None)?;
    let man_files = completion_files(&man_roots, None)?;
    let (catalog, diagnostics) = catalog_from_files(
        &fish_files,
        &bash_files,
        &zsh_files,
        &help_files,
        &man_files,
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
                report.index.display()
            );
            for diagnostic in &report.diagnostics {
                eprintln!(
                    "{}:{}: skipped completion declaration: {}",
                    diagnostic.origin, diagnostic.line, diagnostic.message
                );
            }
        }
        IndexOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(json_error)?
        ),
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
    let source = fs::read_to_string(&path).map_err(|error| {
        index_io_error("read", &path, error)
            .with_help("Build the index first with `quirl index build`")
    })?;
    let catalog = decode_catalog(&source, &path)?;
    let explanation = catalog.explain(command).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidCommand,
            format!("the completion index has no command `{command}`"),
        )
        .with_help("Run `quirl index build` to refresh installed completion metadata")
    })?;
    match format {
        IndexOutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&explanation).map_err(json_error)?
        ),
        IndexOutputFormat::Text => {
            println!("{}", explanation.command);
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
                    fact.fact,
                    fact.value,
                    fact.provenance.source,
                    fact.provenance.confidence,
                    origin,
                    fingerprint
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
) -> Result<(Catalog, Vec<ImportDiagnostic>), ShellError> {
    let mut catalog = Catalog::builtin();
    let mut diagnostics = Vec::new();
    for path in fish_files {
        let source = read_completion(path)?;
        diagnostics.extend(catalog.merge_report(import_fish(&source, &path.display().to_string())));
    }
    for path in bash_files {
        let source = read_completion(path)?;
        diagnostics.extend(catalog.merge_report(import_bash(&source, &path.display().to_string())));
    }
    for path in zsh_files {
        let source = read_completion(path)?;
        diagnostics.extend(catalog.merge_report(import_zsh(&source, &path.display().to_string())));
    }
    for path in help_files {
        let source = read_documentation(path)?;
        diagnostics.extend(catalog.merge_report(import_help(&source, &path.display().to_string())));
    }
    for path in man_files {
        let source = read_documentation(path)?;
        diagnostics.extend(catalog.merge_report(import_man(&source, &path.display().to_string())));
    }
    Ok((catalog, diagnostics))
}

fn completion_files(
    roots: &[PathBuf],
    required_extension: Option<&str>,
) -> Result<Vec<PathBuf>, ShellError> {
    let mut files = Vec::new();
    for root in roots {
        match fs::metadata(root) {
            Ok(metadata) if metadata.is_file() => files.push(root.clone()),
            Ok(metadata) if metadata.is_dir() => {
                let entries =
                    fs::read_dir(root).map_err(|error| index_io_error("enumerate", root, error))?;
                for entry in entries {
                    let entry = entry.map_err(|error| index_io_error("enumerate", root, error))?;
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    if required_extension.is_none_or(|extension| {
                        path.extension()
                            .is_some_and(|candidate| candidate == extension)
                    }) {
                        files.push(path);
                    }
                }
            }
            Ok(_) => {}
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

fn read_completion(path: &Path) -> Result<String, ShellError> {
    fs::read_to_string(path).map_err(|error| index_io_error("read", path, error))
}

fn read_documentation(path: &Path) -> Result<String, ShellError> {
    let file = fs::File::open(path).map_err(|error| index_io_error("read", path, error))?;
    let mut bytes = Vec::new();
    file.take(DOCUMENTATION_READ_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|error| index_io_error("read", path, error))?;
    String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("{} is not UTF-8 documentation text", path.display()),
        )
        .with_context(error.to_string())
        .with_help("Supply rendered help or man text encoded as UTF-8")
    })
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
        fs::create_dir_all(parent).map_err(|error| index_io_error("create", parent, error))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("catalog.json");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let encoded = serde_json::to_string_pretty(catalog).map_err(json_error)?;
    fs::write(&temporary, encoded).map_err(|error| index_io_error("write", &temporary, error))?;
    fs::rename(&temporary, path).map_err(|error| index_io_error("install", path, error))
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
        assert!(!directory
            .join(format!(".catalog.json.{}.tmp", std::process::id()))
            .exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_or_incompatible_default_cache_recovers_to_current_builtins() {
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
