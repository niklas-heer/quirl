use clap::{Args, ValueEnum};
use quirl_catalog::Catalog;
use quirl_core::{ErrorCode, ShellError};
use quirl_picker::{ItemKind, PickItem, Picker};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

const MAX_FILE_ITEMS: usize = 20_000;

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
}

impl PickCommand {
    pub fn wants_json(&self) -> bool {
        matches!(self.format, PickFormat::Json)
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PickSource {
    Stdin,
    History,
    Files,
    Actions,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PickFormat {
    Text,
    Json,
}

pub fn execute(command: PickCommand, catalog: &Catalog) -> Result<i32, ShellError> {
    let items = match command.source {
        PickSource::Stdin => stdin_items()?,
        PickSource::History => history_items()?,
        PickSource::Files => file_items(&command.root)?,
        PickSource::Actions => action_items(catalog),
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
                match &item.value {
                    serde_json::Value::String(value) => println!("{value}"),
                    value => println!("{}", serde_json::to_string(value).map_err(json_error)?),
                }
            }
        }
        PickFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&selected).map_err(json_error)?
        ),
    }
    Ok(0)
}

fn stdin_items() -> Result<Vec<PickItem>, ShellError> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).map_err(|error| {
        ShellError::new(ErrorCode::Io, "could not read picker input")
            .with_context(error.to_string())
            .with_help("Pipe newline-delimited values into `quirl pick`")
    })?;
    Ok(input
        .lines()
        .enumerate()
        .map(|(index, line)| text_item(index, ItemKind::Data, line))
        .collect())
}

fn history_items() -> Result<Vec<PickItem>, ShellError> {
    let path = quirl_ui::history_path()?;
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(ShellError::new(
                ErrorCode::Io,
                format!("could not read history at {}", path.display()),
            )
            .with_context(error.to_string())
            .with_help("Set QUIRL_HISTORY to a readable history file"));
        }
    };
    Ok(source
        .lines()
        .rev()
        .enumerate()
        .map(|(index, line)| text_item(index, ItemKind::History, line))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_keep_the_original_command_path() {
        let items = action_items(&Catalog::builtin());
        let selected = Picker.select(&items, "'index 'explain", 1);
        assert_eq!(selected[0].value, "quirl index explain");
    }
}
