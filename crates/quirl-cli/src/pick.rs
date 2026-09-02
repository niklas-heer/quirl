use crate::projects;
use clap::{Args, ValueEnum};
use quirl_catalog::Catalog;
use quirl_core::{ErrorCode, ShellError, escape_json_terminal_controls, escape_terminal_controls};
use quirl_lua::ProjectsConfig;
use quirl_picker::{ItemKind, PickItem, Picker};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_FILE_ITEMS: usize = 20_000;
const MAX_STDIN_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDIN_ITEMS: usize = 20_000;
const MAX_HISTORY_ITEMS: usize = 20_000;

#[derive(Debug, Args)]
pub struct PickCommand {
    /// Values to search. Defaults to lines from standard input.
    #[arg(long, value_enum, default_value_t = PickSource::Stdin)]
    source: PickSource,
    /// Fuzzy query. Prefix a term with `'` for exact or `!` to exclude it.
    #[arg(long, default_value = "")]
    query: String,
    /// Return more than one selected value.
    #[arg(long)]
    multi: bool,
    /// Maximum selected values when --multi is present.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Stable output representation.
    #[arg(long, value_enum, default_value_t = PickFormat::Text)]
    format: PickFormat,
    /// Root for the file source.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Refresh Git projects before selecting instead of reading the cache.
    #[arg(long)]
    refresh: bool,
}

impl PickCommand {
    pub fn wants_json(&self) -> bool {
        matches!(self.format, PickFormat::Json)
    }

    /// Whether this invocation needs the evaluated project configuration.
    pub fn wants_project_refresh(&self) -> bool {
        matches!(self.source, PickSource::Projects) && self.refresh
    }

    /// Whether this invocation needs the composed command catalog.
    pub fn wants_catalog(&self) -> bool {
        matches!(self.source, PickSource::Actions)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PickSource {
    Stdin,
    History,
    Files,
    Actions,
    Projects,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PickFormat {
    Text,
    Json,
}

pub fn execute(
    command: PickCommand,
    catalog: Option<&Catalog>,
    projects_config: Option<&ProjectsConfig>,
) -> Result<i32, ShellError> {
    let items = match command.source {
        PickSource::Stdin => stdin_items()?,
        PickSource::History => history_items()?,
        PickSource::Files => file_items(&command.root)?,
        PickSource::Actions => action_items(catalog.ok_or_else(|| {
            ShellError::new(
                ErrorCode::Validation,
                "the actions picker requires the composed command catalog",
            )
            .with_help("Retry after loading the installed command catalog")
        })?),
        PickSource::Projects => project_items(command.refresh, projects_config)?,
    };
    let limit = if command.multi {
        command.limit.max(1)
    } else {
        1
    };
    let selected = Picker.select(&items, &command.query, limit);
    match command.format {
        PickFormat::Text => {
            for item in selected {
                println!("{}", render_text_value(&item.value)?);
            }
        }
        PickFormat::Json => println!("{}", render_json(&selected)?),
    }
    Ok(0)
}

fn stdin_items() -> Result<Vec<PickItem>, ShellError> {
    stdin_items_from(io::stdin())
}

fn stdin_items_from(reader: impl Read) -> Result<Vec<PickItem>, ShellError> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(MAX_STDIN_BYTES.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read picker input")
                .with_context(error.to_string())
                .with_help("Pipe newline-delimited values into `quirl pick`")
        })?;
    if bytes.len() > MAX_STDIN_BYTES {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "picker standard input exceeds its read limit",
        )
        .with_context(format!("bytes: {}; limit: {MAX_STDIN_BYTES}", bytes.len()))
        .with_help("Pipe at most 4 MiB of newline-delimited values into `quirl pick`"));
    }
    let input = String::from_utf8(bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "picker standard input is not valid UTF-8",
        )
        .with_context(error.to_string())
        .with_help("Encode picker input as UTF-8 newline-delimited values")
    })?;
    let mut items = Vec::new();
    for (index, line) in input.lines().enumerate() {
        if index == MAX_STDIN_ITEMS {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "picker standard input contains too many values",
            )
            .with_context(format!("items: more than {MAX_STDIN_ITEMS}"))
            .with_help("Limit newline-delimited picker input to 20000 values"));
        }
        items.push(text_item(index, ItemKind::Data, line));
    }
    Ok(items)
}

fn history_items() -> Result<Vec<PickItem>, ShellError> {
    let path = quirl_ui::history_path()?;
    history_items_from(&path)
}

fn history_items_from(path: &Path) -> Result<Vec<PickItem>, ShellError> {
    Ok(quirl_ui::read_history(path)?
        .into_iter()
        .rev()
        .take(MAX_HISTORY_ITEMS)
        .enumerate()
        .map(|(index, entry)| text_item(index, ItemKind::History, &entry))
        .collect())
}

fn file_items(root: &Path) -> Result<Vec<PickItem>, ShellError> {
    let mut items = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                format!("could not read picker root {}", directory.display()),
            )
            .with_context(error.to_string())
            .with_help("Choose a readable directory with --root")
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                ShellError::new(ErrorCode::Io, "could not read a picker file entry")
                    .with_context(error.to_string())
                    .with_help("Retry with a narrower --root")
            })?;
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let label = relative.to_string_lossy().into_owned();
            let is_directory = entry
                .file_type()
                .map_err(|error| {
                    ShellError::new(
                        ErrorCode::Io,
                        format!("could not inspect {}", path.display()),
                    )
                    .with_context(error.to_string())
                    .with_help("Retry with a narrower --root")
                })?
                .is_dir();
            items.push(PickItem {
                id: path.to_string_lossy().into_owned(),
                kind: if is_directory {
                    ItemKind::Directory
                } else {
                    ItemKind::File
                },
                label,
                description: if is_directory {
                    "directory".to_owned()
                } else {
                    "file".to_owned()
                },
                preview: None,
                value: serde_json::Value::String(path.to_string_lossy().into_owned()),
            });
            if items.len() >= MAX_FILE_ITEMS {
                return Ok(items);
            }
            if is_directory && !entry.file_name().to_string_lossy().starts_with('.') {
                pending.push(path);
            }
        }
    }
    Ok(items)
}

fn action_items(catalog: &Catalog) -> Vec<PickItem> {
    catalog
        .commands
        .iter()
        .enumerate()
        .map(|(index, command)| PickItem {
            id: format!("action-{index}"),
            kind: ItemKind::Action,
            label: command.path.clone(),
            description: command.summary.clone(),
            preview: Some(command.details.clone()),
            value: serde_json::Value::String(command.path.clone()),
        })
        .collect()
}

fn project_items(
    refresh: bool,
    projects_config: Option<&ProjectsConfig>,
) -> Result<Vec<PickItem>, ShellError> {
    let snapshot = if refresh {
        let projects_config = projects_config.ok_or_else(|| {
            ShellError::new(
                ErrorCode::Validation,
                "project refresh requires the active project configuration",
            )
            .with_help("Retry after loading the active config.lua project settings")
        })?;
        let config = projects::ProjectDiscoveryConfig::from_config(projects_config)?.ok_or_else(
            || {
                ShellError::new(
                    ErrorCode::Validation,
                    "project discovery is disabled by the active configuration",
                )
                .with_help(
                    "Set projects.discovery to `auto`, or retry without --refresh to read the cache",
                )
            },
        )?;
        projects::refresh_default(&config)?
    } else {
        projects::cached_default()?
    };
    Ok(snapshot
        .repositories
        .into_iter()
        .enumerate()
        .map(|(index, repository)| {
            let path = repository.path.to_string_lossy().into_owned();
            PickItem {
                id: format!("project-{index}"),
                kind: ItemKind::Directory,
                label: repository.name.to_string_lossy().into_owned(),
                description: repository.inferred_root.to_string_lossy().into_owned(),
                preview: Some(path.clone()),
                value: serde_json::Value::String(path),
            }
        })
        .collect())
}

fn text_item(index: usize, kind: ItemKind, value: &str) -> PickItem {
    PickItem {
        id: index.to_string(),
        kind,
        label: value.to_owned(),
        description: String::new(),
        preview: None,
        value: serde_json::Value::String(value.to_owned()),
    }
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not produce picker JSON")
        .with_context(error.to_string())
        .with_help("Retry with --format text")
}

fn render_text_value(value: &serde_json::Value) -> Result<String, ShellError> {
    match value {
        serde_json::Value::String(value) => Ok(escape_terminal_controls(value)),
        value => Ok(escape_json_terminal_controls(
            &serde_json::to_string(value).map_err(json_error)?,
        )),
    }
}

fn render_json(value: &impl serde::Serialize) -> Result<String, ShellError> {
    Ok(escape_json_terminal_controls(
        &serde_json::to_string_pretty(value).map_err(json_error)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_keep_the_original_command_path() {
        let items = action_items(&Catalog::builtin());
        let selected = Picker.select(&items, "'index 'explain", 1);
        assert_eq!(selected[0].value, "quirl index explain");
    }

    #[test]
    fn only_explicit_project_refreshes_load_project_configuration() {
        let command = PickCommand {
            source: PickSource::Projects,
            query: String::new(),
            multi: false,
            limit: 20,
            format: PickFormat::Text,
            root: PathBuf::from("."),
            refresh: true,
        };
        assert!(command.wants_project_refresh());

        let cached = PickCommand {
            refresh: false,
            ..command
        };
        assert!(!cached.wants_project_refresh());
        assert!(!cached.wants_catalog());

        let actions = PickCommand {
            source: PickSource::Actions,
            ..cached
        };
        assert!(actions.wants_catalog());
    }

    #[test]
    fn picker_output_neutralizes_c0_and_c1_controls_without_changing_json_semantics() {
        let hostile = serde_json::json!("safe\u{1b}[31m\u{9b}2J\r");
        let text = render_text_value(&hostile).unwrap();
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{009b}'));
        assert!(!text.contains('\r'));
        assert!(text.contains("\\u{1b}[31m"));

        let selected = vec![text_item(0, ItemKind::Data, hostile.as_str().unwrap())];
        let json = render_json(&selected).unwrap();
        assert!(!json.contains('\u{009b}'));
        let parsed: Vec<PickItem> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, selected);
    }

    #[test]
    fn picker_stdin_rejects_oversized_input_and_item_counts_before_selection() {
        let oversized = vec![b'x'; MAX_STDIN_BYTES + 1];
        let error = stdin_items_from(std::io::Cursor::new(oversized)).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.help[0].contains("4 MiB"));

        let many_items = "value\n".repeat(MAX_STDIN_ITEMS + 1);
        let error = stdin_items_from(std::io::Cursor::new(many_items)).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.help[0].contains("20000"));
    }

    #[test]
    fn history_source_reuses_bounded_tail_reader_and_decodes_multiline_entries() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "quirl-pick-history-{}-{unique}",
            std::process::id()
        ));
        let mut source = "old\n".repeat(MAX_HISTORY_ITEMS + 10);
        source.push_str(&"x".repeat(64 * 1024 + 1));
        source.push('\n');
        source.push_str("printf one<\\n>printf two\n");
        fs::write(&path, source).unwrap();

        let items = history_items_from(&path).unwrap();
        assert_eq!(items.len(), MAX_HISTORY_ITEMS);
        assert_eq!(items[0].label, "printf one\nprintf two");
        assert_eq!(items[0].value, "printf one\nprintf two");
        assert!(items.iter().all(|item| item.label.len() <= 64 * 1024));

        fs::remove_file(path).unwrap();
    }
}
