//! Explicit, iterative conversions at typed-data serialization boundaries.

use super::{resource_limit_error, DataLimits, DataValue};
use crate::syntax::{SyntaxLiteral, SyntaxLiteralKind};
use quirl_core::{ErrorCode, ShellError};
use serde_json::{Number as JsonNumber, Value as JsonValue};
use std::mem::size_of;

/// Measured retained shape for one validated typed value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ValueUsage {
    /// Scalar and container nodes visited.
    pub(super) nodes: usize,
    /// UTF-8 bytes retained by scalar text and record keys.
    pub(super) text_bytes: usize,
    /// Conservative approximate in-memory bytes retained by the value.
    pub(super) retained_bytes: usize,
}

impl ValueUsage {
    fn observe_node(&mut self, depth: usize, limits: DataLimits) -> Result<(), ShellError> {
        if depth > limits.max_depth {
            return Err(resource_limit_error(
                "structured value nesting depth",
                limits.max_depth,
                depth,
                "Flatten the input or raise the configured data depth limit",
            ));
        }
        self.nodes = self.nodes.saturating_add(1);
        if self.nodes > limits.max_nodes {
            return Err(resource_limit_error(
                "structured value nodes",
                limits.max_nodes,
                self.nodes,
                "Reduce the input structure or raise the configured data node limit",
            ));
        }
        self.retained_bytes = self.retained_bytes.saturating_add(size_of::<DataValue>());
        self.check_materialized(limits)
    }

    fn observe_text(&mut self, bytes: usize, limits: DataLimits) -> Result<(), ShellError> {
        self.text_bytes = self.text_bytes.saturating_add(bytes);
        if self.text_bytes > limits.max_retained_text_bytes {
            return Err(resource_limit_error(
                "structured value retained text bytes",
                limits.max_retained_text_bytes,
                self.text_bytes,
                "Reduce string or field-name data, or raise the configured retained-text limit",
            ));
        }
        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.check_materialized(limits)
    }

    fn observe_list(&mut self, length: usize, limits: DataLimits) -> Result<(), ShellError> {
        if length > limits.max_rows {
            return Err(resource_limit_error(
                "structured value rows",
                limits.max_rows,
                length,
                "Use `take <count>` or raise the configured data row limit",
            ));
        }
        Ok(())
    }

    fn observe_record(
        &mut self,
        fields: usize,
        key_bytes: usize,
        limits: DataLimits,
    ) -> Result<(), ShellError> {
        if fields > limits.max_fields {
            return Err(resource_limit_error(
                "record fields",
                limits.max_fields,
                fields,
                "Select fewer fields or raise the configured data field limit",
            ));
        }
        let field_overhead = size_of::<String>().saturating_add(4 * size_of::<usize>());
        self.retained_bytes = self
            .retained_bytes
            .saturating_add(fields.saturating_mul(field_overhead));
        self.check_materialized(limits)?;
        self.observe_text(key_bytes, limits)
    }

    fn check_materialized(&self, limits: DataLimits) -> Result<(), ShellError> {
        if self.retained_bytes > limits.max_materialized_bytes {
            return Err(resource_limit_error(
                "structured value retained bytes",
                limits.max_materialized_bytes,
                self.retained_bytes,
                "Use a streaming transform or raise the configured materialization limit",
            ));
        }
        Ok(())
    }
}

/// Validate one typed value without recursion and return its measured usage.
pub(super) fn validate_data_value(
    value: &DataValue,
    limits: DataLimits,
) -> Result<ValueUsage, ShellError> {
    let mut usage = ValueUsage::default();
    let mut stack = vec![(value, 0_usize)];
    while let Some((value, depth)) = stack.pop() {
        usage.observe_node(depth, limits)?;
        match value {
            DataValue::Decimal(value)
            | DataValue::String(value)
            | DataValue::Path(value)
            | DataValue::DateTime(value)
            | DataValue::Pattern(value) => usage.observe_text(value.len(), limits)?,
            DataValue::List(values) => {
                usage.observe_list(values.len(), limits)?;
                stack.extend(values.iter().rev().map(|value| (value, depth + 1)));
            }
            DataValue::Record(values) => {
                let key_bytes = values
                    .keys()
                    .fold(0_usize, |total, key| total.saturating_add(key.len()));
                usage.observe_record(values.len(), key_bytes, limits)?;
                stack.extend(values.values().rev().map(|value| (value, depth + 1)));
            }
            DataValue::Nothing
            | DataValue::Bool(_)
            | DataValue::Int(_)
            | DataValue::UInt(_)
            | DataValue::Duration { .. }
            | DataValue::Size { .. } => {}
        }
    }
    Ok(usage)
}

enum SyntaxWork<'a> {
    Visit(&'a SyntaxLiteral, usize),
    FinishList(usize),
    FinishRecord(Vec<String>),
}

/// Lower the already-bounded focused AST directly to the typed runtime value.
pub(super) fn data_value_from_syntax(
    literal: &SyntaxLiteral,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    let mut work = vec![SyntaxWork::Visit(literal, 0)];
    let mut output = Vec::new();
    let mut usage = ValueUsage::default();
    while let Some(task) = work.pop() {
        match task {
            SyntaxWork::Visit(literal, depth) => {
                usage.observe_node(depth, limits)?;
                match &literal.kind {
                    SyntaxLiteralKind::Nothing => output.push(DataValue::Nothing),
                    SyntaxLiteralKind::Bool(value) => output.push(DataValue::Bool(*value)),
                    SyntaxLiteralKind::Int(value) => output.push(DataValue::Int(*value)),
                    SyntaxLiteralKind::UInt(value) => output.push(DataValue::UInt(*value)),
                    SyntaxLiteralKind::Decimal(value) => {
                        usage.observe_text(value.len(), limits)?;
                        output.push(DataValue::Decimal(value.clone()));
                    }
                    SyntaxLiteralKind::String(value) => {
                        usage.observe_text(value.len(), limits)?;
                        output.push(DataValue::String(value.clone()));
                    }
                    SyntaxLiteralKind::List(values) => {
                        usage.observe_list(values.len(), limits)?;
                        work.push(SyntaxWork::FinishList(values.len()));
                        work.extend(
                            values
                                .iter()
                                .rev()
                                .map(|value| SyntaxWork::Visit(value, depth + 1)),
                        );
                    }
                    SyntaxLiteralKind::Record(fields) => {
                        let key_bytes = fields.iter().fold(0_usize, |total, field| {
                            total.saturating_add(field.name.value.len())
                        });
                        usage.observe_record(fields.len(), key_bytes, limits)?;
                        work.push(SyntaxWork::FinishRecord(
                            fields
                                .iter()
                                .map(|field| field.name.value.clone())
                                .collect(),
                        ));
                        work.extend(
                            fields
                                .iter()
                                .rev()
                                .map(|field| SyntaxWork::Visit(&field.value, depth + 1)),
                        );
                    }
                }
            }
            SyntaxWork::FinishList(length) => {
                let start = output.len().checked_sub(length).ok_or_else(builder_error)?;
                let values = output.split_off(start);
                output.push(DataValue::List(values));
            }
            SyntaxWork::FinishRecord(keys) => {
                let start = output
                    .len()
                    .checked_sub(keys.len())
                    .ok_or_else(builder_error)?;
                let values = output.split_off(start);
                output.push(DataValue::Record(keys.into_iter().zip(values).collect()));
            }
        }
    }
    finish_boundary(output, limits)
}

enum JsonWork {
    Visit(JsonValue, usize),
    FinishList(usize),
    FinishRecord(Vec<String>),
}

/// Convert parsed JSON to generic typed values. JSON cannot create domain types.
pub(super) fn data_value_from_json(
    value: JsonValue,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    let mut work = vec![JsonWork::Visit(value, 0)];
    let mut output = Vec::new();
    let mut usage = ValueUsage::default();
    while let Some(task) = work.pop() {
        match task {
            JsonWork::Visit(value, depth) => {
                usage.observe_node(depth, limits)?;
                match value {
                    JsonValue::Null => output.push(DataValue::Nothing),
                    JsonValue::Bool(value) => output.push(DataValue::Bool(value)),
                    JsonValue::Number(value) => {
                        let value = json_number(value);
                        if let DataValue::Decimal(decimal) = &value {
                            usage.observe_text(decimal.len(), limits)?;
                        }
                        output.push(value);
                    }
                    JsonValue::String(value) => {
                        usage.observe_text(value.len(), limits)?;
                        output.push(DataValue::String(value));
                    }
                    JsonValue::Array(values) => {
                        let length = values.len();
                        usage.observe_list(length, limits)?;
                        work.push(JsonWork::FinishList(length));
                        work.extend(
                            values
                                .into_iter()
                                .rev()
                                .map(|value| JsonWork::Visit(value, depth + 1)),
                        );
                    }
                    JsonValue::Object(values) => {
                        let key_bytes = values
                            .keys()
                            .fold(0_usize, |total, key| total.saturating_add(key.len()));
                        usage.observe_record(values.len(), key_bytes, limits)?;
                        let (keys, values): (Vec<_>, Vec<_>) = values.into_iter().unzip();
                        work.push(JsonWork::FinishRecord(keys));
                        work.extend(
                            values
                                .into_iter()
                                .rev()
                                .map(|value| JsonWork::Visit(value, depth + 1)),
                        );
                    }
                }
            }
            JsonWork::FinishList(length) => finish_list(&mut output, length)?,
            JsonWork::FinishRecord(keys) => finish_record(&mut output, keys)?,
        }
    }
    finish_boundary(output, limits)
}

enum TomlWork {
    Visit(toml::Value, usize),
    FinishList(usize),
    FinishRecord(Vec<String>),
}

/// Convert TOML without a JSON round trip; TOML datetimes retain their domain type.
pub(super) fn data_value_from_toml(
    value: toml::Value,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    let mut work = vec![TomlWork::Visit(value, 0)];
    let mut output = Vec::new();
    let mut usage = ValueUsage::default();
    while let Some(task) = work.pop() {
        match task {
            TomlWork::Visit(value, depth) => {
                usage.observe_node(depth, limits)?;
                match value {
                    toml::Value::String(value) => {
                        usage.observe_text(value.len(), limits)?;
                        output.push(DataValue::String(value));
                    }
                    toml::Value::Integer(value) => output.push(DataValue::Int(value)),
                    toml::Value::Float(value) => {
                        let value = value.to_string();
                        usage.observe_text(value.len(), limits)?;
                        output.push(DataValue::Decimal(value));
                    }
                    toml::Value::Boolean(value) => output.push(DataValue::Bool(value)),
                    toml::Value::Datetime(value) => {
                        let value = value.to_string();
                        usage.observe_text(value.len(), limits)?;
                        output.push(DataValue::DateTime(value));
                    }
                    toml::Value::Array(values) => {
                        let length = values.len();
                        usage.observe_list(length, limits)?;
                        work.push(TomlWork::FinishList(length));
                        work.extend(
                            values
                                .into_iter()
                                .rev()
                                .map(|value| TomlWork::Visit(value, depth + 1)),
                        );
                    }
                    toml::Value::Table(values) => {
                        let key_bytes = values
                            .keys()
                            .fold(0_usize, |total, key| total.saturating_add(key.len()));
                        usage.observe_record(values.len(), key_bytes, limits)?;
                        let (keys, values): (Vec<_>, Vec<_>) = values.into_iter().unzip();
                        work.push(TomlWork::FinishRecord(keys));
                        work.extend(
                            values
                                .into_iter()
                                .rev()
                                .map(|value| TomlWork::Visit(value, depth + 1)),
                        );
                    }
                }
            }
            TomlWork::FinishList(length) => finish_list(&mut output, length)?,
            TomlWork::FinishRecord(keys) => finish_record(&mut output, keys)?,
        }
    }
    finish_boundary(output, limits)
}

enum YamlWork {
    Visit(serde_yaml_ng::Value, usize),
    FinishList(usize),
    FinishRecord(Vec<String>),
}

/// Convert YAML without JSON erasure. Tagged values and non-string mapping keys fail closed.
pub(super) fn data_value_from_yaml(
    value: serde_yaml_ng::Value,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    let mut work = vec![YamlWork::Visit(value, 0)];
    let mut output = Vec::new();
    let mut usage = ValueUsage::default();
    while let Some(task) = work.pop() {
        match task {
            YamlWork::Visit(value, depth) => {
                usage.observe_node(depth, limits)?;
                match value {
                    serde_yaml_ng::Value::Null => output.push(DataValue::Nothing),
                    serde_yaml_ng::Value::Bool(value) => output.push(DataValue::Bool(value)),
                    serde_yaml_ng::Value::Number(value) => {
                        let value = yaml_number(&value)?;
                        if let DataValue::Decimal(decimal) = &value {
                            usage.observe_text(decimal.len(), limits)?;
                        }
                        output.push(value);
                    }
                    serde_yaml_ng::Value::String(value) => {
                        usage.observe_text(value.len(), limits)?;
                        output.push(DataValue::String(value));
                    }
                    serde_yaml_ng::Value::Sequence(values) => {
                        let length = values.len();
                        usage.observe_list(length, limits)?;
                        work.push(YamlWork::FinishList(length));
                        work.extend(
                            values
                                .into_iter()
                                .rev()
                                .map(|value| YamlWork::Visit(value, depth + 1)),
                        );
                    }
                    serde_yaml_ng::Value::Mapping(values) => {
                        usage.observe_record(values.len(), 0, limits)?;
                        let mut keys = Vec::with_capacity(values.len());
                        let mut children = Vec::with_capacity(values.len());
                        for (key, value) in values {
                            let serde_yaml_ng::Value::String(key) = key else {
                                return Err(ShellError::new(
                                    ErrorCode::Data,
                                    "YAML mapping keys must be strings",
                                )
                                .with_help(
                                    "Quote every YAML mapping key before opening the document",
                                ));
                            };
                            usage.observe_text(key.len(), limits)?;
                            keys.push(key);
                            children.push(value);
                        }
                        work.push(YamlWork::FinishRecord(keys));
                        work.extend(
                            children
                                .into_iter()
                                .rev()
                                .map(|value| YamlWork::Visit(value, depth + 1)),
                        );
                    }
                    serde_yaml_ng::Value::Tagged(_) => {
                        return Err(ShellError::new(
                            ErrorCode::Data,
                            "YAML tags are not supported by the typed data boundary",
                        )
                        .with_help(
                            "Remove the YAML tag or convert it to an explicit record field",
                        ));
                    }
                }
            }
            YamlWork::FinishList(length) => finish_list(&mut output, length)?,
            YamlWork::FinishRecord(keys) => finish_record(&mut output, keys)?,
        }
    }
    finish_boundary(output, limits)
}

enum JsonOutputWork<'a> {
    Visit(&'a DataValue),
    FinishList(usize),
    FinishRecord(Vec<String>),
}

/// Encode a typed value into ordinary JSON-compatible data.
///
/// JSON has no domain types: paths, datetimes, and patterns become strings;
/// durations and sizes use suffixed strings. Decimal text must be a valid JSON
/// number. This is the only lossy value-to-JSON bridge used by the evaluator.
pub(super) fn json_from_data_value(
    value: &DataValue,
    limits: DataLimits,
) -> Result<JsonValue, ShellError> {
    validate_data_value(value, limits)?;
    let mut work = vec![JsonOutputWork::Visit(value)];
    let mut output = Vec::new();
    while let Some(task) = work.pop() {
        match task {
            JsonOutputWork::Visit(value) => match value {
                DataValue::Nothing => output.push(JsonValue::Null),
                DataValue::Bool(value) => output.push(JsonValue::Bool(*value)),
                DataValue::Int(value) => output.push(JsonValue::from(*value)),
                DataValue::UInt(value) => output.push(JsonValue::from(*value)),
                DataValue::Decimal(value) => output.push(JsonValue::Number(
                    value.parse::<JsonNumber>().map_err(|_| {
                        ShellError::new(
                            ErrorCode::Data,
                            "decimal cannot be represented as a JSON number",
                        )
                        .with_context(format!("decimal source: {value}"))
                        .with_help("Select or convert non-finite decimals before `to json`")
                    })?,
                )),
                DataValue::String(value)
                | DataValue::Path(value)
                | DataValue::DateTime(value)
                | DataValue::Pattern(value) => output.push(JsonValue::String(value.clone())),
                DataValue::Duration { nanoseconds } => {
                    output.push(JsonValue::String(format!("{nanoseconds}ns")))
                }
                DataValue::Size { bytes } => output.push(JsonValue::String(format!("{bytes}B"))),
                DataValue::List(values) => {
                    work.push(JsonOutputWork::FinishList(values.len()));
                    work.extend(values.iter().rev().map(JsonOutputWork::Visit));
                }
                DataValue::Record(values) => {
                    work.push(JsonOutputWork::FinishRecord(
                        values.keys().cloned().collect(),
                    ));
                    work.extend(values.values().rev().map(JsonOutputWork::Visit));
                }
            },
            JsonOutputWork::FinishList(length) => {
                let start = output.len().checked_sub(length).ok_or_else(builder_error)?;
                let values = output.split_off(start);
                output.push(JsonValue::Array(values));
            }
            JsonOutputWork::FinishRecord(keys) => {
                let start = output
                    .len()
                    .checked_sub(keys.len())
                    .ok_or_else(builder_error)?;
                let values = output.split_off(start);
                output.push(JsonValue::Object(keys.into_iter().zip(values).collect()));
            }
        }
    }
    if output.len() == 1 {
        output.pop().ok_or_else(builder_error)
    } else {
        Err(builder_error())
    }
}

fn json_number(value: JsonNumber) -> DataValue {
    match (value.as_i64(), value.as_u64()) {
        (Some(value), _) => DataValue::Int(value),
        (_, Some(value)) => DataValue::UInt(value),
        _ => DataValue::Decimal(value.to_string()),
    }
}

fn yaml_number(value: &serde_yaml_ng::Number) -> Result<DataValue, ShellError> {
    if let Some(value) = value.as_i64() {
        return Ok(DataValue::Int(value));
    }
    if let Some(value) = value.as_u64() {
        return Ok(DataValue::UInt(value));
    }
    let decimal = value.to_string();
    if decimal.parse::<f64>().is_ok() {
        Ok(DataValue::Decimal(decimal))
    } else {
        Err(ShellError::new(
            ErrorCode::Data,
            "YAML number is outside the supported typed numeric domain",
        )
        .with_context(format!("number source: {decimal}"))
        .with_help("Use a signed integer, unsigned integer, or finite decimal"))
    }
}

fn finish_list(output: &mut Vec<DataValue>, length: usize) -> Result<(), ShellError> {
    let start = output.len().checked_sub(length).ok_or_else(builder_error)?;
    let values = output.split_off(start);
    output.push(DataValue::List(values));
    Ok(())
}

fn finish_record(output: &mut Vec<DataValue>, keys: Vec<String>) -> Result<(), ShellError> {
    let start = output
        .len()
        .checked_sub(keys.len())
        .ok_or_else(builder_error)?;
    let values = output.split_off(start);
    output.push(DataValue::Record(keys.into_iter().zip(values).collect()));
    Ok(())
}

fn finish_boundary(
    mut output: Vec<DataValue>,
    limits: DataLimits,
) -> Result<DataValue, ShellError> {
    if output.len() != 1 {
        return Err(builder_error());
    }
    let value = output.pop().ok_or_else(builder_error)?;
    validate_data_value(&value, limits)?;
    Ok(value)
}

fn builder_error() -> ShellError {
    ShellError::new(
        ErrorCode::Data,
        "typed value boundary reached an inconsistent builder state",
    )
    .with_help("Report this internal typed-data conversion invariant failure")
}
