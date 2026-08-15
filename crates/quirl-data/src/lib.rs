//! Quirl's deliberately focused structured-data evaluator.

use quirl_core::{directory_entries, ErrorCode, ShellError};
use serde_json::{Map, Value};
use std::{cmp::Ordering, fs, path::PathBuf};

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
        Some("where") => filter_where(value, stage),
        Some("select") if words.len() >= 2 => select_fields(value, &words[1..], stage),
        Some("sort") => sort_rows(value, &words, stage),
        Some("take") => take_values(value, &words, stage),
        Some(command) => Err(data_error(
            stage,
            format!("unknown data transform `{command}`"),
        )),
        None => Err(data_error(stage, "empty pipeline stage")),
    }
}

fn get_field(value: Value, field: &str, stage: &str) -> Result<Value, ShellError> {
    match value {
        Value::Object(object) => get_path(&Value::Object(object), field)
            .cloned()
            .ok_or_else(|| data_error(stage, format!("object has no field `{field}`"))),
        Value::Array(values) => values
            .into_iter()
            .map(|value| {
                if !value.is_object() {
                    return Err(data_error(stage, "get over an array expects object rows"));
                }
                get_path(&value, field)
                    .cloned()
                    .ok_or_else(|| data_error(stage, format!("row has no field `{field}`")))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        _ => Err(data_error(
            stage,
            "get expects an object or array of objects",
        )),
    }
}

fn filter_where(value: Value, stage: &str) -> Result<Value, ShellError> {
    let expression = stage
        .strip_prefix("where")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .ok_or_else(|| {
            data_error(
                stage,
                "usage: where <field> <comparison> <value> [and|or ...]",
            )
        })?;
    let predicate = Predicate::parse(expression, stage)?;
    let Value::Array(values) = value else {
        return Err(data_error(stage, "where expects an array of objects"));
    };

    let mut filtered = Vec::new();
    for value in values {
        if !value.is_object() {
            return Err(data_error(stage, "where expects object rows"));
        }
        if predicate.matches(&value, stage)? {
            filtered.push(value);
        }
    }
    Ok(Value::Array(filtered))
}

#[derive(Debug, Clone, Copy)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug)]
struct Condition {
    field: String,
    comparison: Comparison,
    expected: Value,
}

#[derive(Debug, Clone, Copy)]
enum BooleanOperator {
    And,
    Or,
}

#[derive(Debug)]
struct Predicate {
    conditions: Vec<Condition>,
    operators: Vec<BooleanOperator>,
}

impl Predicate {
    fn parse(expression: &str, stage: &str) -> Result<Self, ShellError> {
        let tokens = predicate_tokens(expression, stage)?;
        if tokens.is_empty() {
            return Err(data_error(stage, "where requires a predicate"));
        }

        let mut conditions = Vec::new();
        let mut operators = Vec::new();
        let mut index = 0;
        loop {
            let Some(field) = tokens.get(index) else {
                return Err(data_error(stage, "expected a field after boolean operator"));
            };
            let Some(operator) = tokens.get(index + 1) else {
                return Err(data_error(stage, "expected a comparison after field"));
            };
            let Some(expected) = tokens.get(index + 2) else {
                return Err(data_error(stage, "expected a value after comparison"));
            };
            if field.quoted {
                return Err(data_error(stage, "predicate fields must be bare names"));
            }
            let comparison = match operator.text.as_str() {
                "==" => Comparison::Equal,
                "!=" => Comparison::NotEqual,
                "<" => Comparison::Less,
                "<=" => Comparison::LessOrEqual,
                ">" => Comparison::Greater,
                ">=" => Comparison::GreaterOrEqual,
                _ => {
                    return Err(data_error(
                        stage,
                        format!("unsupported comparison `{}`", operator.text),
                    ))
                }
            };
            conditions.push(Condition {
                field: field.text.clone(),
                comparison,
                expected: expected.as_value(),
            });
            index += 3;

            let Some(boolean) = tokens.get(index) else {
                break;
            };
            if boolean.quoted {
                return Err(data_error(
                    stage,
                    "expected `and` or `or` between comparisons",
                ));
            }
            operators.push(match boolean.text.as_str() {
                "and" => BooleanOperator::And,
                "or" => BooleanOperator::Or,
                _ => {
                    return Err(data_error(
                        stage,
                        "expected `and` or `or` between comparisons",
                    ))
                }
            });
            index += 1;
        }

        Ok(Self {
            conditions,
            operators,
        })
    }

    fn matches(&self, row: &Value, stage: &str) -> Result<bool, ShellError> {
        let mut group = evaluate_condition(&self.conditions[0], row, stage)?;
        let mut result = false;
        for (operator, condition) in self.operators.iter().zip(&self.conditions[1..]) {
            match operator {
                BooleanOperator::And => {
                    group = group && evaluate_condition(condition, row, stage)?;
                }
                BooleanOperator::Or => {
                    result = result || group;
                    group = evaluate_condition(condition, row, stage)?;
                }
            }
        }
        Ok(result || group)
    }
}

#[derive(Debug)]
struct PredicateToken {
    text: String,
    quoted: bool,
}

impl PredicateToken {
    fn as_value(&self) -> Value {
        if self.quoted {
            Value::String(self.text.clone())
        } else {
            serde_json::from_str(&self.text).unwrap_or_else(|_| Value::String(self.text.clone()))
        }
    }
}

fn predicate_tokens(expression: &str, stage: &str) -> Result<Vec<PredicateToken>, ShellError> {
    let mut tokens = Vec::new();
    let mut characters = expression.char_indices().peekable();
    while let Some((_, character)) = characters.peek().copied() {
        if character.is_whitespace() {
            characters.next();
            continue;
        }

        if character == '\'' || character == '"' {
            characters.next();
            let quote = character;
            let mut text = String::new();
            let mut closed = false;
            while let Some((_, character)) = characters.next() {
                if character == quote {
                    closed = true;
                    break;
                }
                if character == '\\' {
                    let Some((_, escaped)) = characters.next() else {
                        return Err(data_error(stage, "unfinished escape in quoted value"));
                    };
                    text.push(match escaped {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        other => other,
                    });
                } else {
                    text.push(character);
                }
            }
            if !closed {
                return Err(data_error(stage, "unclosed quote in predicate"));
            }
            tokens.push(PredicateToken { text, quoted: true });
            continue;
        }

        if matches!(character, '=' | '!' | '<' | '>') {
            let Some((_, first)) = characters.next() else {
                return Err(data_error(
                    stage,
                    "predicate operator is missing its first character",
                ));
            };
            let mut text = first.to_string();
            if characters.peek().is_some_and(|(_, next)| *next == '=') {
                text.push('=');
                characters.next();
            }
            tokens.push(PredicateToken {
                text,
                quoted: false,
            });
            continue;
        }

        let mut text = String::new();
        while let Some((_, character)) = characters.peek().copied() {
            if character.is_whitespace() || matches!(character, '=' | '!' | '<' | '>') {
                break;
            }
            text.push(character);
            characters.next();
        }
        tokens.push(PredicateToken {
            text,
            quoted: false,
        });
    }
    Ok(tokens)
}

fn evaluate_condition(condition: &Condition, row: &Value, stage: &str) -> Result<bool, ShellError> {
    let Some(actual) = get_path(row, &condition.field) else {
        return Ok(false);
    };
    match condition.comparison {
        Comparison::Equal => Ok(actual == &condition.expected),
        Comparison::NotEqual => Ok(actual != &condition.expected),
        Comparison::Less => {
            Ok(compare_values(actual, &condition.expected, stage)? == Ordering::Less)
        }
        Comparison::LessOrEqual => Ok(matches!(
            compare_values(actual, &condition.expected, stage)?,
            Ordering::Less | Ordering::Equal
        )),
        Comparison::Greater => {
            Ok(compare_values(actual, &condition.expected, stage)? == Ordering::Greater)
        }
        Comparison::GreaterOrEqual => Ok(matches!(
            compare_values(actual, &condition.expected, stage)?,
            Ordering::Greater | Ordering::Equal
        )),
    }
}

fn compare_values(left: &Value, right: &Value, stage: &str) -> Result<Ordering, ShellError> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => {
            if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
                return Ok(left.cmp(&right));
            }
            if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
                return Ok(left.cmp(&right));
            }
            if let (Some(left), Some(right)) = (left.as_i64(), right.as_u64()) {
                return Ok(if left < 0 {
                    Ordering::Less
                } else {
                    (left as u64).cmp(&right)
                });
            }
            if let (Some(left), Some(right)) = (left.as_u64(), right.as_i64()) {
                return Ok(if right < 0 {
                    Ordering::Greater
                } else {
                    left.cmp(&(right as u64))
                });
            }
            left.as_f64()
                .and_then(|left| right.as_f64().and_then(|right| left.partial_cmp(&right)))
                .ok_or_else(|| data_error(stage, "numbers cannot be ordered"))
        }
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Ok(left.cmp(right)),
        _ => Err(data_error(
            stage,
            format!(
                "cannot order {} and {} values",
                value_kind(left),
                value_kind(right)
            ),
        )),
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(value, |value, field| value.as_object()?.get(field))
}

fn sort_rows(value: Value, words: &[String], stage: &str) -> Result<Value, ShellError> {
    if !(words.len() == 2 || words.len() == 3) {
        return Err(data_error(stage, "usage: sort <field> [asc|desc]"));
    }
    let descending = match words.get(2).map(String::as_str) {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(data_error(stage, "sort direction must be `asc` or `desc`")),
    };
    let Value::Array(mut values) = value else {
        return Err(data_error(stage, "sort expects an array of objects"));
    };
    for value in &values {
        if !value.is_object() {
            return Err(data_error(stage, "sort expects object rows"));
        }
        if get_path(value, &words[1]).is_none() {
            return Err(data_error(
                stage,
                format!("row has no field `{}`", words[1]),
            ));
        }
    }

    let mut comparison_error = None;
    values.sort_by(|left, right| {
        if comparison_error.is_some() {
            return Ordering::Equal;
        }
        let (Some(left), Some(right)) = (get_path(left, &words[1]), get_path(right, &words[1]))
        else {
            comparison_error = Some(data_error(
                stage,
                format!("row has no field `{}`", words[1]),
            ));
            return Ordering::Equal;
        };
        match compare_values(left, right, stage) {
            Ok(ordering) if descending => ordering.reverse(),
            Ok(ordering) => ordering,
            Err(error) => {
                comparison_error = Some(error);
                Ordering::Equal
            }
        }
    });
    if let Some(error) = comparison_error {
        return Err(error);
    }
    Ok(Value::Array(values))
}

fn take_values(value: Value, words: &[String], stage: &str) -> Result<Value, ShellError> {
    if words.len() != 2 {
        return Err(data_error(stage, "usage: take <count>"));
    }
    let count = words[1]
        .parse::<usize>()
        .map_err(|_| data_error(stage, "take count must be a non-negative integer"))?;
    let Value::Array(mut values) = value else {
        return Err(data_error(stage, "take expects an array"));
    };
    values.truncate(count);
    Ok(Value::Array(values))
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
    let mut delimiters = Vec::new();
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
            '[' | '{' | '(' => delimiters.push(character),
            ']' | '}' | ')' => {
                let expected = match character {
                    ']' => '[',
                    '}' => '{',
                    ')' => '(',
                    _ => unreachable!(),
                };
                if delimiters.pop() != Some(expected) {
                    return Err(data_error(
                        source,
                        "unmatched or mismatched closing delimiter",
                    ));
                }
            }
            '|' if delimiters.is_empty() => {
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
    if !delimiters.is_empty() {
        return Err(data_error(source, "unclosed delimiter"));
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

    #[test]
    fn filters_sorts_and_limits_rows_with_the_documented_grammar() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval(
                r#"[
                    {"name":"old-small","kind":"file","size":100,"meta":{"age":40}},
                    {"name":"new-large","kind":"file","size":900,"meta":{"age":2}},
                    {"name":"old-large","kind":"file","size":700,"meta":{"age":35}},
                    {"name":"directory","kind":"dir","size":1200,"meta":{"age":90}},
                    {"name":"old-largest","kind":"file","size":1100,"meta":{"age":60}}
                ]
                | where kind == file and meta.age > 30
                | select name size
                | sort size desc
                | take 2"#,
            )
            .unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {"name": "old-largest", "size": 1100},
                {"name": "old-large", "size": 700}
            ])
        );
    }

    #[test]
    fn where_supports_all_comparisons_and_and_before_or_precedence() {
        let runtime = DataRuntime::new();
        let value = runtime
            .eval(
                r#"[
                    {"name":"a","score":1,"enabled":true},
                    {"name":"b","score":2,"enabled":false},
                    {"name":"c","score":3,"enabled":true},
                    {"name":"d","score":4,"enabled":true}
                ] | where score >= 2 and score < 4 or name != "d" and enabled == true
                  | get name"#,
            )
            .unwrap();
        assert_eq!(value, serde_json::json!(["a", "b", "c"]));

        assert_eq!(
            runtime
                .eval(r#"[{"n":1},{"n":2},{"n":3}] | where n <= 2 and n > 1"#)
                .unwrap(),
            serde_json::json!([{"n": 2}])
        );
    }

    #[test]
    fn quoted_predicate_values_remain_strings() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval(r#"[{"value":"42"},{"value":42}] | where value == "42""#)
                .unwrap(),
            serde_json::json!([{"value": "42"}])
        );
        assert_eq!(
            runtime
                .eval(r#"[{"value":"a and b"},{"value":"a"}] | where value == 'a and b'"#)
                .unwrap(),
            serde_json::json!([{"value": "a and b"}])
        );
    }

    #[test]
    fn nested_fields_work_for_get_where_and_sort() {
        let runtime = DataRuntime::new();
        assert_eq!(
            runtime
                .eval(
                    r#"[{"user":{"name":"Ada","rank":2}},{"user":{"name":"Lin","rank":1}}]
                       | where user.rank != 3 | sort user.rank | get user.name"#,
                )
                .unwrap(),
            serde_json::json!(["Lin", "Ada"])
        );
    }

    #[test]
    fn malformed_predicates_and_incomparable_sorts_are_errors() {
        let runtime = DataRuntime::new();
        assert!(runtime.eval(r#"[{"value":1}] | where value = 1"#).is_err());
        assert!(runtime
            .eval(r#"[{"value":1},{"value":"one"}] | sort value"#)
            .is_err());
        assert!(runtime.eval("[1,2 | length").is_err());
    }
}
