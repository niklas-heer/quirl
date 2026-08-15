use clap::{Subcommand, ValueEnum};
use quirl_core::{ErrorCode, ShellError};
use quirl_lua::{LuaPolicy, LuaRuntime, QuirlConfig};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Parse, evaluate under config restrictions, and validate against Rust schemas.
    Check {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = ConfigOutputFormat::Text)]
        format: ConfigOutputFormat,
    },
    /// Print one evaluated, schema-backed configuration value.
    Get { file: PathBuf, key: String },
    /// Patch one recognized literal, validate the candidate, and retain a .bak.
    Set {
        file: PathBuf,
        key: String,
        value: String,
    },
    /// Show the current schema and values as an accessible line-oriented view.
    Tui { file: PathBuf },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ConfigOutputFormat {
    Text,
    Json,
}

pub fn execute(command: ConfigCommand) -> Result<i32, ShellError> {
    match command {
        ConfigCommand::Check { file, format } => check(&file, format),
        ConfigCommand::Get { file, key } => get(&file, &key),
        ConfigCommand::Set { file, key, value } => set(&file, &key, &value),
        ConfigCommand::Tui { file } => tui(&file),
    }
}

fn check(file: &Path, format: ConfigOutputFormat) -> Result<i32, ShellError> {
    let runtime = LuaRuntime::new(LuaPolicy::config())?;
    match runtime.load_config_file(file) {
        Ok(config) => {
            match format {
                ConfigOutputFormat::Text => {
                    println!("✓ {} is valid Lua configuration", file.display());
                }
                ConfigOutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&config).map_err(json_error)?
                ),
            }
            Ok(0)
        }
        Err(error) if matches!(format, ConfigOutputFormat::Json) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&error).map_err(json_error)?
            );
            Ok(1)
        }
        Err(error) => Err(error),
    }
}

fn get(file: &Path, key: &str) -> Result<i32, ShellError> {
    let field = ConfigField::parse(key)?;
    let config = load(file)?;
    println!("{}", field.value(&config));
    Ok(0)
}

fn tui(file: &Path) -> Result<i32, ShellError> {
    let config = load(file)?;
    println!("Quirl configuration · {}", file.display());
    println!("read-only line view; use `quirl config set` to change a literal value\n");
    println!("[editor]");
    println!(
        "editor.keymap = {}  (helix | emacs | vim)",
        config.editor.keymap
    );
    println!(
        "editor.semantic_hints = {}  (true | false)",
        config.editor.semantic_hints
    );
    println!("\n[picker]");
    println!(
        "picker.layout = {}  (adaptive | bottom | full)",
        config.picker.layout
    );
    println!("picker.preview = {}  (true | false)", config.picker.preview);
    println!("\nThe synchronized local web configuration view remains future work.");
    Ok(0)
}

fn load(file: &Path) -> Result<QuirlConfig, ShellError> {
    LuaRuntime::new(LuaPolicy::config())?.load_config_file(file)
}

fn set(file: &Path, key: &str, value: &str) -> Result<i32, ShellError> {
    let field = ConfigField::parse(key)?;
    let replacement = field.lua_literal(value)?;
    let source = fs::read_to_string(file).map_err(|error| file_error("read", file, error))?;
    let candidate = patch_literal(&source, field, &replacement)?;
    let temporary = temporary_path(file)?;
    let result = install_candidate(file, &temporary, &candidate);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    println!(
        "updated {key} in {} (backup: {})",
        file.display(),
        backup_path(file).display()
    );
    Ok(0)
}

fn install_candidate(file: &Path, temporary: &Path, candidate: &str) -> Result<(), ShellError> {
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temporary)
        .map_err(|error| file_error("create candidate for", file, error))?;
    output
        .write_all(candidate.as_bytes())
        .and_then(|()| output.sync_all())
        .map_err(|error| file_error("write candidate for", file, error))?;

    // Candidate evaluation happens before either the source or its backup changes.
    LuaRuntime::new(LuaPolicy::config())?.load_config_file(temporary)?;

    if let Ok(metadata) = fs::metadata(file) {
        fs::set_permissions(temporary, metadata.permissions())
            .map_err(|error| file_error("preserve permissions for", file, error))?;
    }
    let backup = backup_path(file);
    fs::copy(file, &backup).map_err(|error| file_error("back up", file, error))?;
    fs::rename(temporary, file).map_err(|error| {
        file_error("atomically replace", file, error).with_help(format!(
            "The original remains available at {}",
            backup.display()
        ))
    })?;
    sync_parent(file)?;
    Ok(())
}

fn sync_parent(file: &Path) -> Result<(), ShellError> {
    let parent = file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| file_error("sync the directory containing", file, error))
}

fn backup_path(file: &Path) -> PathBuf {
    let mut value = OsString::from(file.as_os_str());
    value.push(".bak");
    PathBuf::from(value)
}

fn temporary_path(file: &Path) -> Result<PathBuf, ShellError> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            ShellError::new(
                ErrorCode::Io,
                "could not create a configuration transaction",
            )
            .with_context(error.to_string())
        })?
        .as_nanos();
    let name = file.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} has no configuration file name", file.display()),
        )
        .with_help("Pass a path to config.lua")
    })?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".quirl-tmp-{}-{nonce}", std::process::id()));
    Ok(file.with_file_name(temporary_name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigField {
    EditorKeymap,
    EditorSemanticHints,
    PickerLayout,
    PickerPreview,
}

impl ConfigField {
    fn parse(key: &str) -> Result<Self, ShellError> {
        match key {
            "editor.keymap" => Ok(Self::EditorKeymap),
            "editor.semantic_hints" => Ok(Self::EditorSemanticHints),
            "picker.layout" => Ok(Self::PickerLayout),
            "picker.preview" => Ok(Self::PickerPreview),
            _ => Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("`{key}` is not an editable literal configuration field"),
            )
            .with_help(format!("Editable fields: {}", Self::KEYS.join(", ")))),
        }
    }

    const KEYS: [&'static str; 4] = [
        "editor.keymap",
        "editor.semantic_hints",
        "picker.layout",
        "picker.preview",
    ];

    const fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::EditorKeymap => ("editor", "keymap"),
            Self::EditorSemanticHints => ("editor", "semantic_hints"),
            Self::PickerLayout => ("picker", "layout"),
            Self::PickerPreview => ("picker", "preview"),
        }
    }

    fn lua_literal(self, value: &str) -> Result<String, ShellError> {
        let valid = match self {
            Self::EditorKeymap => matches!(value, "helix" | "emacs" | "vim"),
            Self::PickerLayout => matches!(value, "adaptive" | "bottom" | "full"),
            Self::EditorSemanticHints | Self::PickerPreview => matches!(value, "true" | "false"),
        };
        if !valid {
            let expected = match self {
                Self::EditorKeymap => "helix, emacs, or vim",
                Self::PickerLayout => "adaptive, bottom, or full",
                Self::EditorSemanticHints | Self::PickerPreview => "true or false",
            };
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("invalid value `{value}`"),
            )
            .with_help(format!("Expected {expected}")));
        }
        Ok(match self {
            Self::EditorKeymap | Self::PickerLayout => format!("\"{value}\""),
            Self::EditorSemanticHints | Self::PickerPreview => value.to_owned(),
        })
    }

    fn value(self, config: &QuirlConfig) -> String {
        match self {
            Self::EditorKeymap => config.editor.keymap.clone(),
            Self::EditorSemanticHints => config.editor.semantic_hints.to_string(),
            Self::PickerLayout => config.picker.layout.clone(),
            Self::PickerPreview => config.picker.preview.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Identifier(String),
    String,
    Symbol(u8),
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

fn patch_literal(
    source: &str,
    field: ConfigField,
    replacement: &str,
) -> Result<String, ShellError> {
    let tokens = tokenize(source);
    let config_open = find_config_table(&tokens)
        .ok_or_else(|| patch_error("could not find a literal `quirl.config { ... }` table"))?;
    let (section, name) = field.parts();
    let section_values = field_values(&tokens, config_open, section);
    let [section_value] = section_values.as_slice() else {
        return Err(patch_error(&format!(
            "expected exactly one literal `{section} = {{ ... }}` section"
        )));
    };
    if !matches!(&tokens[*section_value].kind, TokenKind::Symbol(b'{')) {
        return Err(patch_error(&format!(
            "`{section}` is code-controlled instead of a literal table"
        )));
    }
    let values = field_values(&tokens, *section_value, name);
    let [value] = values.as_slice() else {
        return Err(patch_error(&format!(
            "expected exactly one literal `{section}.{name}` field"
        )));
    };
    let expected_literal = match field {
        ConfigField::EditorKeymap | ConfigField::PickerLayout => {
            matches!(&tokens[*value].kind, TokenKind::String)
        }
        ConfigField::EditorSemanticHints | ConfigField::PickerPreview => matches!(
            &tokens[*value].kind,
            TokenKind::Identifier(value) if value == "true" || value == "false"
        ),
    };
    if !expected_literal {
        return Err(patch_error(&format!(
            "`{section}.{name}` is code-controlled instead of a recognized literal"
        )));
    }
    let token = &tokens[*value];
    let mut patched = String::with_capacity(source.len() + replacement.len());
    patched.push_str(&source[..token.start]);
    patched.push_str(replacement);
    patched.push_str(&source[token.end..]);
    Ok(patched)
}

fn find_config_table(tokens: &[Token]) -> Option<usize> {
    tokens
        .windows(4)
        .position(|tokens| {
            identifier_is(&tokens[0], "quirl")
                && matches!(&tokens[1].kind, TokenKind::Symbol(b'.'))
                && identifier_is(&tokens[2], "config")
                && matches!(&tokens[3].kind, TokenKind::Symbol(b'{'))
        })
        .map(|index| index + 3)
}

fn field_values(tokens: &[Token], open: usize, field: &str) -> Vec<usize> {
    let mut values = Vec::new();
    let mut depth = 0usize;
    let mut index = open + 1;
    while index < tokens.len() {
        match &tokens[index].kind {
            TokenKind::Symbol(b'{') | TokenKind::Symbol(b'(') | TokenKind::Symbol(b'[') => {
                depth += 1;
            }
            TokenKind::Symbol(b'}') if depth == 0 => break,
            TokenKind::Symbol(b'}') | TokenKind::Symbol(b')') | TokenKind::Symbol(b']') => {
                depth = depth.saturating_sub(1);
            }
            _ if depth == 0 && identifier_is(&tokens[index], field) => {
                if matches!(
                    tokens.get(index + 1).map(|token| &token.kind),
                    Some(TokenKind::Symbol(b'='))
                ) && tokens.get(index + 2).is_some()
                {
                    values.push(index + 2);
                }
            }
            _ => {}
        }
        index += 1;
    }
    values
}

fn identifier_is(token: &Token, expected: &str) -> bool {
    matches!(&token.kind, TokenKind::Identifier(value) if value == expected)
}

fn tokenize(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"--") {
            if let Some(end) = long_bracket_end(bytes, index + 2) {
                index = end;
            } else {
                index = bytes[index..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |offset| index + offset + 1);
            }
            continue;
        }
        if let Some(end) = long_bracket_end(bytes, index) {
            tokens.push(Token {
                kind: TokenKind::Other,
                start: index,
                end,
            });
            index = end;
            continue;
        }
        let start = index;
        let kind = match bytes[index] {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                index += 1;
                while index < bytes.len()
                    && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
                {
                    index += 1;
                }
                TokenKind::Identifier(source[start..index].to_owned())
            }
            quote @ (b'\'' | b'"') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else if bytes[index] == quote {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
                TokenKind::String
            }
            symbol @ (b'.' | b'=' | b'{' | b'}' | b'(' | b')' | b'[' | b']' | b',' | b';') => {
                index += 1;
                TokenKind::Symbol(symbol)
            }
            _ => {
                index += 1;
                TokenKind::Other
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    tokens
}

fn long_bracket_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'[') {
        return None;
    }
    let mut equals = 0usize;
    while bytes.get(start + 1 + equals) == Some(&b'=') {
        equals += 1;
    }
    if bytes.get(start + 1 + equals) != Some(&b'[') {
        return None;
    }
    let mut index = start + 2 + equals;
    while index < bytes.len() {
        if bytes[index] == b']'
            && bytes.get(index + 1..index + 1 + equals)
                == Some(&bytes[start + 1..start + 1 + equals])
            && bytes.get(index + 1 + equals) == Some(&b']')
        {
            return Some(index + 2 + equals);
        }
        index += 1;
    }
    Some(bytes.len())
}

fn patch_error(message: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help(
        "Only recognized literal fields inside `quirl.config { ... }` can be patched; edit dynamic values in code",
    )
}

fn file_error(action: &str, file: &Path, error: std::io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("could not {action} {}", file.display()),
    )
    .with_context(error.to_string())
    .with_help("Check that the configuration path and its parent directory are writable")
}

fn json_error(error: serde_json::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not produce JSON").with_context(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_source() -> &'static str {
        r#"-- Keep this comment and the unrelated plugin setup.
local plugin_value = { preview = false, render = function() return "ok" end }
local config = quirl.config {
  editor = { keymap = "helix", semantic_hints = true }, -- editor note
  picker = { layout = "adaptive", preview = true },
  prompt = { left = { "directory" }, right = {} },
}
return config
"#
    }

    fn test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("quirl-config-test-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn patch_changes_only_the_recognized_literal() {
        let patched =
            patch_literal(example_source(), ConfigField::EditorKeymap, "\"vim\"").unwrap();
        assert!(patched.contains("keymap = \"vim\""));
        assert!(patched.contains("render = function() return \"ok\" end"));
        assert!(patched.contains("-- editor note"));
        assert_eq!(patched.matches("keymap =").count(), 1);
    }

    #[test]
    fn patch_rejects_code_controlled_values() {
        let source = example_source().replace("keymap = \"helix\"", "keymap = choose_keymap() ");
        let error = patch_literal(&source, ConfigField::EditorKeymap, "\"vim\"").unwrap_err();
        assert!(error.message.contains("code-controlled"));
    }

    #[test]
    fn set_validates_then_atomically_installs_and_keeps_backup() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        fs::write(&file, example_source()).unwrap();

        set(&file, "picker.preview", "false").unwrap();

        let installed = fs::read_to_string(&file).unwrap();
        assert!(installed.contains("picker = { layout = \"adaptive\", preview = false }"));
        assert_eq!(
            fs::read_to_string(backup_path(&file)).unwrap(),
            example_source()
        );
        assert!(!load(&file).unwrap().picker.preview);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_complete_candidate_never_replaces_source() {
        let directory = test_directory();
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("config.lua");
        let invalid = example_source().replace(
            "prompt = { left = { \"directory\" }, right = {} }",
            "prompt = { left = { 42 }, right = {} }",
        );
        fs::write(&file, &invalid).unwrap();

        let error = set(&file, "picker.preview", "false").unwrap_err();

        assert_eq!(fs::read_to_string(&file).unwrap(), invalid);
        assert!(!backup_path(&file).exists());
        assert_eq!(error.code, ErrorCode::Validation);
        fs::remove_dir_all(directory).unwrap();
    }
}
