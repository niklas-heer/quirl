//! Quirl's deliberately focused structured-data evaluator.

use quirl_core::{directory_entries, ErrorCode, ShellError};
use serde_json::{Map, Value};
use std::{fs, path::PathBuf};

#[derive(Debug, Default)]
pub struct DataRuntime;

impl DataRuntime {
    pub const fn new() -> Self {
        Self
    }

    pub fn eval(&self, source: &str) -> Result<Value, ShellError> {
        let stages = split_pipeline(source)?;
        let Some((first, transforms)) = stages.split_first() else {
            return Ok(Value::Null);
        };
        let mut value = evaluate_source(first)?;
        for transform in transforms {
            value = apply_transform(value, transform)?;
        }
        Ok(value)
    }
}

fn evaluate_source(stage: &str) -> Result<Value, ShellError> {
    let words = shlex::split(stage).ok_or_else(|| data_error(stage, "unclosed quote"))?;
    match words.first().map(String::as_str) {
        Some("pwd") if words.len() == 1 => std::env::current_dir()
            .map(|path| Value::String(path.display().to_string()))
            .map_err(|error| {
                ShellError::new(ErrorCode::Io, "cannot read the current directory")
                    .with_context(error.to_string())
            }),
        Some("ls") if words.len() <= 2 => {
            let path = words
                .get(1)
                .map_or_else(|| PathBuf::from("."), PathBuf::from);
            serde_json::to_value(directory_entries(&path, false)?).map_err(|error| {
                ShellError::new(ErrorCode::Data, "cannot represent directory entries")
                    .with_context(error.to_string())
            })
        }
        Some("open") if words.len() == 2 => {
            let path = PathBuf::from(&words[1]);
            let contents = fs::read_to_string(&path).map_err(|error| {
                ShellError::new(ErrorCode::Io, format!("cannot open {}", path.display()))
                    .with_context(error.to_string())
            })?;
            Ok(serde_json::from_str(&contents).unwrap_or(Value::String(contents)))
        }
        Some("open") => Err(data_error(stage, "usage: open <path>")),
        Some("ls") => Err(data_error(stage, "usage: ls [path]")),
        _ => serde_json::from_str(stage).map_err(|error| {
            data_error(
                stage,
                format!("expected `pwd`, `ls`, `open`, or a JSON value: {error}"),
            )
        }),
    }
}

fn apply_transform(value: Value, stage: &str) -> Result<Value, ShellError> {
    let words = shlex::split(stage).ok_or_else(|| data_error(stage, "unclosed quote"))?;
    match words.first().map(String::as_str) {
        Some("length") if words.len() == 1 => match value {
            Value::Array(values) => Ok(Value::from(values.len())),
            Value::Object(values) => Ok(Value::from(values.len())),
            Value::String(value) => Ok(Value::from(value.chars().count())),
            _ => Err(data_error(
                stage,
                "length expects an array, object, or string",
            )),
        },
        Some("first") if words.len() == 1 => match value {
            Value::Array(values) => Ok(values.into_iter().next().unwrap_or(Value::Null)),
            _ => Err(data_error(stage, "first expects an array")),
        },
        Some("get") if words.len() == 2 => get_field(value, &words[1], stage),
        Some("where") => filter_where(value, &words, stage),
        Some("select") if words.len() >= 2 => select_fields(value, &words[1..], stage),
        Some(command) => Err(data_error(
            stage,
            format!("unknown data transform `{command}`"),
        )),
        None => Err(data_error(stage, "empty pipeline stage")),
    }
}

fn get_field(value: Value, field: &str, stage: &str) -> Result<Value, ShellError> {
    match value {
        Value::Object(mut object) => object
            .remove(field)
            .ok_or_else(|| data_error(stage, format!("object has no field `{field}`"))),
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::Object(mut object) => object
                    .remove(field)
                    .ok_or_else(|| data_error(stage, format!("row has no field `{field}`"))),
                _ => Err(data_error(stage, "get over an array expects object rows")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Err(data_error(
            stage,
            "get expects an object or array of objects",
        )),
    }
}

fn filter_where(value: Value, words: &[String], stage: &str) -> Result<Value, ShellError> {
    if words.len() < 4 || words[2] != "==" {
        return Err(data_error(stage, "usage: where <field> == <JSON value>"));
    }
    let expected_source = words[3..].join(" ");
    let expected = serde_json::from_str(&expected_source).unwrap_or(Value::String(expected_source));
    let Value::Array(values) = value else {
        return Err(data_error(stage, "where expects an array of objects"));
    };
    Ok(Value::Array(
        values
            .into_iter()
            .filter(|value| {
                value.as_object().and_then(|object| object.get(&words[1])) == Some(&expected)
            })
            .collect(),
    ))
}

fn select_fields(value: Value, fields: &[String], stage: &str) -> Result<Value, ShellError> {
    fn select(object: Map<String, Value>, fields: &[String]) -> Map<String, Value> {
        fields
            .iter()
            .filter_map(|field| {
                object
                    .get(field)
                    .cloned()
                    .map(|value| (field.clone(), value))
            })
            .collect()
    }

    match value {
        Value::Object(object) => Ok(Value::Object(select(object, fields))),
        Value::Array(values) => values
            .into_iter()
            .map(|value| match value {
                Value::Object(object) => Ok(Value::Object(select(object, fields))),
                _ => Err(data_error(stage, "select expects object rows")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Err(data_error(
            stage,
            "select expects an object or array of objects",
        )),
    }
}

fn split_pipeline(source: &str) -> Result<Vec<&str>, ShellError> {
    let mut stages = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0_u32;
    for (index, character) in source.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quote.is_some() {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '[' | '{' | '(' => depth += 1,
            ']' | '}' | ')' if depth > 0 => depth -= 1,
            '|' if depth == 0 => {
                let stage = source[start..index].trim();
                if stage.is_empty() {
                    return Err(data_error(source, "empty pipeline stage"));
                }
                stages.push(stage);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quote.is_some() {
        return Err(data_error(source, "unclosed quote"));
    }
    let final_stage = source[start..].trim();
    if !final_stage.is_empty() {
        stages.push(final_stage);
    }
    Ok(stages)
}

fn data_error(source: &str, message: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Data, "invalid data expression")
        .with_context(message)
        .with_label(None, 0, source.len(), "could not evaluate this stage")
        .with_help("Try `help data` for source and transform syntax")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transforms_structured_rows_without_stringification() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval(
                r#"[{"name":"api","status":"up"},{"name":"db","status":"down"}]
                   | where status == "down" | select name"#,
            )
            .unwrap();
        assert_eq!(value, serde_json::json!([{"name": "db"}]));
    }

    #[test]
    fn pipes_inside_json_strings_do_not_split_the_pipeline() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime.eval(r#"{"value":"a|b"} | get value"#).unwrap(),
            "a|b"
        );
    }

    #[test]
    fn length_preserves_a_numeric_value() {
        let runtime = DataRuntime::new();
        assert_eq!(runtime.eval("[1,2,3] | length").unwrap(), 3);
    }
}
