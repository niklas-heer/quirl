use crate::{
    imported_argument, imported_command, Catalog, CommandSpec, Confidence, OptionSpec, Provenance,
    ProvenanceInfo, CATALOG_SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

const MAX_HELP_BYTES: usize = 1024 * 1024;
const MAX_HELP_LINES: usize = 20_000;
const MAX_HELP_OPTIONS: usize = 2_048;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
/// Non-fatal problem encountered while importing external command metadata.
///
/// Importers retain valid facts from the same source, allowing callers to report
/// degraded coverage without discarding the complete bounded import.
pub struct ImportDiagnostic {
    /// File path or logical provider identity supplied to the importer.
    pub origin: String,
    /// One-based source line associated with the diagnostic.
    pub line: usize,
    /// Human-readable explanation of the skipped, truncated, or dynamic construct.
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
/// Bounded result of parsing one external completion or documentation source.
pub struct ImportReport {
    /// Normalized command contracts recovered from supported static declarations.
    pub commands: Vec<CommandSpec>,
    /// Ordered non-fatal observations about unsupported or malformed source content.
    pub diagnostics: Vec<ImportDiagnostic>,
}

/// Import the declarative subset of Fish's `complete` builtin.
///
/// Static command, short/long/old-style option, description, argument, and
/// condition declarations are retained. Dynamic command substitutions remain
/// attributed in command details but are never executed by the importer.
pub fn import_fish(source: &str, origin: &str) -> ImportReport {
    let fingerprint = fingerprint(source);
    let mut report = ImportReport::default();
    for (line_number, line) in logical_lines(source) {
        if !line.contains("complete") {
            continue;
        }
        let tokens = match shell_words(&line) {
            Ok(tokens) => tokens,
            Err(message) => {
                report
                    .diagnostics
                    .push(diagnostic(origin, line_number, message));
                continue;
            }
        };
        let Some(complete_index) = tokens.iter().position(|token| token == "complete") else {
            continue;
        };
        let declaration = &tokens[complete_index + 1..];
        match parse_fish_declaration(declaration, origin, &fingerprint) {
            Ok(commands) => merge_report_commands(&mut report, commands),
            Err(message) => report
                .diagnostics
                .push(diagnostic(origin, line_number, message)),
        }
    }
    report
}

/// Import Bash `complete` declarations without sourcing or executing them.
///
/// Static `-W` word lists become option facts. `-F`, `-C`, and `-A` provider
/// declarations create attributable command entries so an index can explain
/// where dynamic completion would come from without granting it authority.
pub fn import_bash(source: &str, origin: &str) -> ImportReport {
    let fingerprint = fingerprint(source);
    let mut report = ImportReport::default();
    for (line_number, line) in logical_lines(source) {
        if !line.contains("complete") {
            continue;
        }
        let tokens = match shell_words(&line) {
            Ok(tokens) => tokens,
            Err(message) => {
                report
                    .diagnostics
                    .push(diagnostic(origin, line_number, message));
                continue;
            }
        };
        let Some(complete_index) = tokens.iter().position(|token| token == "complete") else {
            continue;
        };
        let declaration = &tokens[complete_index + 1..];
        match parse_bash_declaration(declaration, origin, &fingerprint) {
            Ok(commands) => merge_report_commands(&mut report, commands),
            Err(message) => report
                .diagnostics
                .push(diagnostic(origin, line_number, message)),
        }
    }
    report
}

/// Import common static Zsh completion declarations without loading Zsh.
///
/// `_arguments` option specs, literal/static-array `_describe` candidates, and
/// literal/static-array `_values` candidates are recognized. Functions,
/// substitutions, and dynamic providers are recorded but never executed.
pub fn import_zsh(source: &str, origin: &str) -> ImportReport {
    let fingerprint = fingerprint(source);
    let provenance =
        ProvenanceInfo::imported(Provenance::Zsh, Confidence::High, origin, &fingerprint);
    let commands = zsh_commands(source, origin);
    let arrays = zsh_static_arrays(source);
    let mut options = Vec::new();
    let mut described = Vec::new();
    let mut values = Vec::new();
    let mut diagnostics = Vec::new();
    let mut dynamic = Vec::new();

    for (line_number, line) in logical_lines(source) {
        let tokens = match shell_words(&line) {
            Ok(tokens) => tokens,
            Err(message) => {
                diagnostics.push(diagnostic(origin, line_number, message));
                continue;
            }
        };
        if let Some(call) = tokens.iter().position(|token| token == "_arguments") {
            let mut skip_next = false;
            for spec in &tokens[call + 1..] {
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if matches!(spec.as_str(), "-A" | "-M" | "-R") {
                    skip_next = true;
                    continue;
                }
                if matches!(spec.as_str(), "-s" | "-S" | "-w" | "-W" | "-C") {
                    continue;
                }
                if is_dynamic_zsh(spec) {
                    dynamic.push(spec.clone());
                    continue;
                }
                if let Some(option) = parse_zsh_argument(spec, &provenance) {
                    options.push(option);
                }
            }
        }
        if let Some(call) = tokens.iter().position(|token| token == "_describe") {
            let candidates = zsh_call_candidates(&tokens[call + 1..], &arrays);
            if candidates.dynamic {
                dynamic.push(format!("_describe {}", candidates.source));
            }
            described.extend(candidates.values);
        }
        if let Some(call) = tokens.iter().position(|token| token == "_values") {
            let candidates = zsh_call_candidates(&tokens[call + 1..], &arrays);
            if candidates.dynamic {
                dynamic.push(format!("_values {}", candidates.source));
            }
            values.extend(candidates.values);
        }
    }

    options.sort_by(|left, right| left.names.cmp(&right.names));
    options.dedup_by(|left, right| left.names == right.names);
    described.sort();
    described.dedup();
    values.sort();
    values.dedup();
    dynamic.sort();
    dynamic.dedup();

    let mut details = String::from(
        "Imported statically from Zsh `_arguments`, `_describe`, and `_values` declarations.",
    );
    if !values.is_empty() {
        details.push_str(" Static values: ");
        details.push_str(&values.join(", "));
        details.push('.');
    }
    if !dynamic.is_empty() {
        details.push_str(" Dynamic declarations recorded but not executed: ");
        details.push_str(&dynamic.join(", "));
        details.push('.');
    }
    let mut imported = commands
        .iter()
        .map(|path| {
            imported_command(
                path.clone(),
                format!("{path} [options]"),
                "Command discovered from Zsh completion metadata".to_owned(),
                details.clone(),
                options.clone(),
                provenance.clone(),
            )
        })
        .collect::<Vec<_>>();
    for command in &commands {
        for candidate in &described {
            let (name, summary) = split_zsh_description(candidate);
            if name.starts_with('-') || name.is_empty() {
                continue;
            }
            let path = format!("{command} {name}");
            imported.push(imported_command(
                path.clone(),
                format!("{path} [options]"),
                summary.unwrap_or("Subcommand imported from Zsh").to_owned(),
                "Imported from a static Zsh `_describe` candidate.".to_owned(),
                Vec::new(),
                provenance.clone(),
            ));
        }
    }
    let mut report = ImportReport {
        commands: Vec::new(),
        diagnostics,
    };
    if commands.is_empty() {
        report.diagnostics.push(diagnostic(
            origin,
            1,
            "Zsh completion has no `#compdef`, `compdef`, or inferable file name".to_owned(),
        ));
    } else {
        merge_report_commands(&mut report, imported);
    }
    report
}

/// Import bounded option metadata from supplied command-help text.
pub fn import_help(source: &str, origin: &str) -> ImportReport {
    import_documentation(source, origin, Provenance::Help)
}

/// Import bounded option metadata from supplied rendered or simple roff man text.
pub fn import_man(source: &str, origin: &str) -> ImportReport {
    import_documentation(source, origin, Provenance::Man)
}

fn import_documentation(source: &str, origin: &str, source_kind: Provenance) -> ImportReport {
    let fingerprint = fingerprint(source);
    let provenance = ProvenanceInfo::imported(source_kind, Confidence::Medium, origin, fingerprint);
    let (bounded, truncated_bytes) = bounded_prefix(source, MAX_HELP_BYTES);
    let mut diagnostics = Vec::new();
    if truncated_bytes {
        diagnostics.push(diagnostic(
            origin,
            1,
            format!("documentation input truncated to {MAX_HELP_BYTES} bytes"),
        ));
    }
    let lines = bounded.lines().take(MAX_HELP_LINES).collect::<Vec<_>>();
    if bounded.lines().count() > MAX_HELP_LINES {
        diagnostics.push(diagnostic(
            origin,
            MAX_HELP_LINES,
            format!("documentation input truncated to {MAX_HELP_LINES} lines"),
        ));
    }
    let command = documentation_command(&lines, origin);
    let mut options = Vec::new();
    for (line, text) in lines.iter().enumerate() {
        if options.len() == MAX_HELP_OPTIONS {
            diagnostics.push(diagnostic(
                origin,
                line + 1,
                format!("option ingestion stopped at {MAX_HELP_OPTIONS} entries"),
            ));
            break;
        }
        if let Some(option) = parse_documentation_option(text, &provenance) {
            options.push(option);
        }
    }
    options.sort_by(|left, right| left.names.cmp(&right.names));
    options.dedup_by(|left, right| left.names == right.names);
    let Some(command) = command else {
        diagnostics.push(diagnostic(
            origin,
            1,
            "could not infer a command name from Usage/NAME/SYNOPSIS or the file name".to_owned(),
        ));
        return ImportReport {
            commands: Vec::new(),
            diagnostics,
        };
    };
    let label = match source_kind {
        Provenance::Help => "help",
        Provenance::Man => "man page",
        _ => "documentation",
    };
    ImportReport {
        commands: vec![imported_command(
            command.clone(),
            format!("{command} [options]"),
            format!("Command discovered from supplied {label} text"),
            format!(
                "Options were heuristically parsed from bounded, supplied {label} text; no command was executed."
            ),
            options,
            provenance,
        )],
        diagnostics,
    }
}

fn zsh_commands(source: &str, origin: &str) -> Vec<String> {
    let mut commands = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(declaration) = trimmed.strip_prefix("#compdef") {
            commands.extend(
                declaration
                    .split_whitespace()
                    .filter(|value| !value.starts_with('-'))
                    .map(str::to_owned),
            );
        }
        if trimmed.starts_with("compdef ") {
            if let Ok(tokens) = shell_words(trimmed) {
                commands.extend(
                    tokens
                        .iter()
                        .skip(2)
                        .filter(|value| !value.starts_with('-'))
                        .cloned(),
                );
            }
        }
    }
    if commands.is_empty() {
        if let Some(name) = inferred_name_from_origin(origin) {
            commands.push(name);
        }
    }
    commands.sort();
    commands.dedup();
    commands
}

fn inferred_name_from_origin(origin: &str) -> Option<String> {
    let name = Path::new(origin).file_name()?.to_str()?;
    let name = name.trim_start_matches('_');
    let name = name
        .strip_suffix(".help.txt")
        .or_else(|| name.strip_suffix(".man.txt"))
        .or_else(|| name.strip_suffix(".help"))
        .or_else(|| name.strip_suffix(".man"))
        .or_else(|| name.strip_suffix(".txt"))
        .unwrap_or(name);
    (!name.is_empty()).then(|| name.to_owned())
}

fn zsh_static_arrays(source: &str) -> HashMap<String, Vec<String>> {
    let bytes = source.as_bytes();
    let mut arrays = HashMap::new();
    let mut index = 0usize;
    while index < bytes.len() {
        if !matches!(bytes[index], b'a'..=b'z' | b'A'..=b'Z' | b'_')
            || index > 0 && (bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_')
        {
            index += 1;
            continue;
        }
        let start = index;
        index += 1;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
        {
            index += 1;
        }
        let name = &source[start..index];
        let mut cursor = index;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'(') || cursor > 0 && bytes[cursor - 1] == b'$' {
            continue;
        }
        let Some(end) = matching_parenthesis(source, cursor) else {
            continue;
        };
        if let Ok(values) = shell_words(&source[cursor + 1..end]) {
            arrays.insert(name.to_owned(), values);
        }
        index = end + 1;
    }
    arrays
}

fn matching_parenthesis(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if *byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if *byte == active {
                quote = None;
            }
            continue;
        }
        if matches!(*byte, b'\'' | b'"') {
            quote = Some(*byte);
        } else if *byte == b'(' {
            depth += 1;
        } else if *byte == b')' {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(open + offset);
            }
        }
    }
    None
}

struct ZshCandidates {
    values: Vec<String>,
    dynamic: bool,
    source: String,
}

fn zsh_call_candidates(tokens: &[String], arrays: &HashMap<String, Vec<String>>) -> ZshCandidates {
    let mut arguments = tokens.iter().filter(|token| !token.starts_with('-'));
    let _label = arguments.next();
    let remaining = arguments.cloned().collect::<Vec<_>>();
    if remaining.len() == 1 {
        if let Some(values) = arrays.get(&remaining[0]) {
            return ZshCandidates {
                values: values.clone(),
                dynamic: false,
                source: remaining[0].clone(),
            };
        }
    }
    let dynamic = remaining.iter().any(|value| is_dynamic_zsh(value))
        || remaining.len() == 1 && remaining.first().is_some_and(|value| is_identifier(value));
    let values = if dynamic {
        Vec::new()
    } else {
        remaining
            .iter()
            .flat_map(|value| shell_words(value).unwrap_or_else(|_| vec![value.clone()]))
            .map(|value| value.trim_matches(['(', ')']).to_owned())
            .filter(|value| !value.is_empty())
            .collect()
    };
    ZshCandidates {
        values,
        dynamic,
        source: remaining.join(" "),
    }
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn is_dynamic_zsh(value: &str) -> bool {
    value.contains("$(") || value.contains('`') || value.starts_with('$') || value.starts_with("->")
}

fn parse_zsh_argument(spec: &str, provenance: &ProvenanceInfo) -> Option<OptionSpec> {
    let spec = strip_zsh_exclusion(spec).trim_start_matches(['*', '!']);
    let mut names = if let Some(open) = spec.find('{') {
        let close = spec[open + 1..].find('}')? + open + 1;
        spec[open + 1..close]
            .split(',')
            .filter_map(normalize_zsh_option)
            .collect::<Vec<_>>()
    } else {
        let end = spec.find(['[', ':', '=', '+']).unwrap_or(spec.len());
        spec[..end]
            .split(',')
            .filter_map(normalize_zsh_option)
            .collect::<Vec<_>>()
    };
    if names.is_empty() {
        return None;
    }
    names.sort();
    names.dedup();
    let summary = bracketed(spec)
        .filter(|value| !value.is_empty())
        .unwrap_or("Imported Zsh completion option")
        .to_owned();
    let after_description = spec
        .rfind(']')
        .map_or(spec, |index| &spec[index.saturating_add(1)..]);
    let value =
        (spec.contains('=') || spec.contains('+') || after_description.contains(':')).then(|| {
            after_description
                .split(':')
                .find(|part| !part.is_empty())
                .unwrap_or("value")
                .trim_matches(['[', ']'])
                .to_owned()
        });
    Some(imported_argument(names, value, summary, provenance.clone()))
}

fn strip_zsh_exclusion(spec: &str) -> &str {
    if spec.starts_with('(') {
        spec.find(')').map_or(spec, |end| &spec[end + 1..])
    } else {
        spec
    }
}

fn normalize_zsh_option(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_start_matches(['*', '!'])
        .trim_end_matches(['=', '+']);
    (value.starts_with('-') && value.len() > 1).then(|| value.to_owned())
}

fn bracketed(value: &str) -> Option<&str> {
    let open = value.find('[')?;
    let close = value[open + 1..].find(']')? + open + 1;
    Some(&value[open + 1..close])
}

fn split_zsh_description(value: &str) -> (&str, Option<&str>) {
    value
        .split_once(':')
        .map_or((value, None), |(name, summary)| (name, Some(summary)))
}

fn bounded_prefix(source: &str, limit: usize) -> (&str, bool) {
    if source.len() <= limit {
        return (source, false);
    }
    let mut end = limit;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    (&source[..end], true)
}

fn documentation_command(lines: &[&str], origin: &str) -> Option<String> {
    for line in lines {
        let normalized = normalize_roff(line);
        let trimmed = normalized.trim();
        let usage = trimmed
            .strip_prefix("Usage:")
            .or_else(|| trimmed.strip_prefix("usage:"));
        if let Some(usage) = usage {
            if let Some(command) = usage.split_whitespace().next() {
                return Some(command.rsplit('/').next().unwrap_or(command).to_owned());
            }
        }
    }
    inferred_name_from_origin(origin)
}

fn parse_documentation_option(line: &str, provenance: &ProvenanceInfo) -> Option<OptionSpec> {
    let normalized = normalize_roff(line);
    let trimmed = normalized
        .trim()
        .trim_start_matches(|character: char| character == '.' || character.is_ascii_uppercase())
        .trim_start();
    if !trimmed.starts_with('-') {
        return None;
    }
    let (head, description) = split_at_spacing(trimmed);
    let mut names = Vec::new();
    let mut value = None;
    for word in head.split_whitespace() {
        let word =
            word.trim_matches(|character| matches!(character, ',' | ';' | '[' | ']' | '"' | '\''));
        if word.starts_with('-') && word.len() > 1 {
            let (name, attached) = word
                .split_once("[=")
                .or_else(|| word.split_once('='))
                .map_or((word, None), |(name, value)| (name, Some(value)));
            names.push(name.to_owned());
            if attached.is_some() {
                value = Some(
                    attached
                        .unwrap_or("value")
                        .trim_matches(['<', '>'])
                        .to_owned(),
                );
            }
        } else if !names.is_empty()
            && (word.starts_with('<')
                || word.chars().all(|character| {
                    character.is_ascii_uppercase() || matches!(character, '_' | '-')
                }))
        {
            value = Some(word.trim_matches(['<', '>', '[', ']']).to_owned());
        }
    }
    names.sort();
    names.dedup();
    (!names.is_empty()).then(|| {
        imported_argument(
            names,
            value,
            if description.is_empty() {
                "Imported documentation option".to_owned()
            } else {
                description.to_owned()
            },
            provenance.clone(),
        )
    })
}

fn normalize_roff(line: &str) -> String {
    line.replace("\\fB", "")
        .replace("\\fI", "")
        .replace("\\fR", "")
        .replace("\\-", "-")
        .replace("\\&", "")
}

fn split_at_spacing(line: &str) -> (&str, &str) {
    let bytes = line.as_bytes();
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index].is_ascii_whitespace() && bytes[index + 1].is_ascii_whitespace() {
            let mut description = index + 2;
            while bytes.get(description).is_some_and(u8::is_ascii_whitespace) {
                description += 1;
            }
            return (&line[..index], &line[description..]);
        }
        index += 1;
    }
    (line, "")
}

fn parse_fish_declaration(
    tokens: &[String],
    origin: &str,
    fingerprint: &str,
) -> Result<Vec<CommandSpec>, String> {
    let mut commands = Vec::new();
    let mut names = Vec::new();
    let mut description = None;
    let mut value = None;
    let mut conditions = Vec::new();
    let mut arguments = Vec::new();
    let mut erase = false;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        match token.as_str() {
            "-c" | "--command" => commands.push(required_value(tokens, &mut index, token)?),
            "-s" | "--short-option" => {
                let name = required_value(tokens, &mut index, token)?;
                names.push(format!("-{name}"));
            }
            "-l" | "--long-option" => {
                let name = required_value(tokens, &mut index, token)?;
                names.push(format!("--{name}"));
            }
            "-o" | "--old-option" => {
                let name = required_value(tokens, &mut index, token)?;
                names.push(format!("-{name}"));
            }
            "-d" | "--description" => {
                description = Some(required_value(tokens, &mut index, token)?)
            }
            "-a" | "--arguments" => arguments.push(required_value(tokens, &mut index, token)?),
            "-n" | "--condition" => conditions.push(required_value(tokens, &mut index, token)?),
            "-r" | "--require-parameter" | "-x" | "--exclusive" => value = Some("value".to_owned()),
            "-e" | "--erase" => erase = true,
            _ => {
                if let Some(command) = long_assignment(token, "--command") {
                    commands.push(command);
                } else if let Some(name) = long_assignment(token, "--short-option") {
                    names.push(format!("-{name}"));
                } else if let Some(name) = long_assignment(token, "--long-option") {
                    names.push(format!("--{name}"));
                } else if let Some(name) = long_assignment(token, "--old-option") {
                    names.push(format!("-{name}"));
                } else if let Some(summary) = long_assignment(token, "--description") {
                    description = Some(summary);
                } else if let Some(argument) = long_assignment(token, "--arguments") {
                    arguments.push(argument);
                } else if let Some(condition) = long_assignment(token, "--condition") {
                    conditions.push(condition);
                } else if let Some(command) = short_attached(token, "-c") {
                    commands.push(command);
                } else if let Some(name) = short_attached(token, "-s") {
                    names.push(format!("-{name}"));
                } else if let Some(name) = short_attached(token, "-l") {
                    names.push(format!("--{name}"));
                }
            }
        }
        index += 1;
    }
    if erase {
        return Ok(Vec::new());
    }
    if commands.is_empty() {
        return Err("Fish completion declaration has no command (`-c`)".to_owned());
    }
    names.sort();
    names.dedup();
    let provenance =
        ProvenanceInfo::imported(Provenance::Fish, Confidence::High, origin, fingerprint);
    let summary = description
        .clone()
        .unwrap_or_else(|| "Imported Fish completion".to_owned());
    let option = (!names.is_empty())
        .then(|| imported_argument(names, value, summary.clone(), provenance.clone()));
    let mut details = String::from("Imported from a declarative Fish `complete` definition.");
    if !conditions.is_empty() {
        details.push_str(" Conditions: ");
        details.push_str(&conditions.join("; "));
        details.push('.');
    }
    if !arguments.is_empty() {
        details.push_str(" Static or dynamic argument declaration: ");
        details.push_str(&arguments.join(" "));
        details.push('.');
    }
    Ok(commands
        .into_iter()
        .map(|path| {
            imported_command(
                path.clone(),
                format!("{path} [options]"),
                "Command discovered from Fish completion metadata".to_owned(),
                details.clone(),
                option.clone().into_iter().collect(),
                provenance.clone(),
            )
        })
        .collect())
}

fn parse_bash_declaration(
    tokens: &[String],
    origin: &str,
    fingerprint: &str,
) -> Result<Vec<CommandSpec>, String> {
    let mut commands = Vec::new();
    let mut word_lists = Vec::new();
    let mut providers = Vec::new();
    let mut remove = false;
    let mut command_arguments = false;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if command_arguments {
            commands.push(token.clone());
            index += 1;
            continue;
        }
        match token.as_str() {
            "--" => command_arguments = true,
            "-W" => word_lists.push(required_value(tokens, &mut index, token)?),
            "-F" | "-C" | "-A" => {
                let provider = required_value(tokens, &mut index, token)?;
                providers.push(format!("{token} {provider}"));
            }
            "-o" | "-X" | "-P" | "-S" => {
                let _ = required_value(tokens, &mut index, token)?;
            }
            "-r" => remove = true,
            "-D" | "-E" | "-I" | "-p" => {}
            value if value.starts_with("-W") && value.len() > 2 => {
                word_lists.push(value[2..].to_owned())
            }
            value
                if (value.starts_with("-F")
                    || value.starts_with("-C")
                    || value.starts_with("-A"))
                    && value.len() > 2 =>
            {
                providers.push(format!("{} {}", &value[..2], &value[2..]));
            }
            value if value.starts_with('-') => {}
            value => commands.push(value.to_owned()),
        }
        index += 1;
    }
    if remove {
        return Ok(Vec::new());
    }
    if commands.is_empty() {
        return Err("Bash completion declaration has no command".to_owned());
    }
    let command_confidence = if word_lists.is_empty() {
        Confidence::Medium
    } else {
        Confidence::High
    };
    let command_provenance =
        ProvenanceInfo::imported(Provenance::Bash, command_confidence, origin, fingerprint);
    let option_provenance =
        ProvenanceInfo::imported(Provenance::Bash, Confidence::High, origin, fingerprint);
    let mut option_names = word_lists
        .iter()
        .flat_map(|words| shell_words(words).unwrap_or_default())
        .filter_map(|candidate| normalize_bash_option(&candidate))
        .collect::<Vec<_>>();
    option_names.sort();
    option_names.dedup();
    let options = option_names
        .into_iter()
        .map(|name| {
            let value = name
                .strip_suffix('=')
                .filter(|_| name.ends_with('='))
                .map(|_| "value".to_owned());
            imported_argument(
                vec![name.trim_end_matches('=').to_owned()],
                value,
                "Imported Bash completion candidate".to_owned(),
                option_provenance.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut details = String::from("Imported from a Bash `complete` declaration.");
    if !providers.is_empty() {
        details.push_str(" Dynamic provider declared but not executed: ");
        details.push_str(&providers.join(", "));
        details.push('.');
    }
    Ok(commands
        .into_iter()
        .map(|path| {
            imported_command(
                path.clone(),
                format!("{path} [options]"),
                "Command discovered from Bash completion metadata".to_owned(),
                details.clone(),
                options.clone(),
                command_provenance.clone(),
            )
        })
        .collect())
}

fn merge_report_commands(report: &mut ImportReport, commands: Vec<CommandSpec>) {
    let mut catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        commands: std::mem::take(&mut report.commands),
    };
    catalog.merge(commands);
    report.commands = catalog.commands;
}

fn normalize_bash_option(candidate: &str) -> Option<String> {
    let candidate = candidate
        .trim_matches(|character: char| matches!(character, ',' | ';'))
        .to_owned();
    (candidate.starts_with('-') && candidate.len() > 1).then_some(candidate)
}

fn required_value(tokens: &[String], index: &mut usize, option: &str) -> Result<String, String> {
    *index += 1;
    tokens
        .get(*index)
        .cloned()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn long_assignment(token: &str, option: &str) -> Option<String> {
    token
        .strip_prefix(option)
        .and_then(|value| value.strip_prefix('='))
        .map(str::to_owned)
}

fn short_attached(token: &str, option: &str) -> Option<String> {
    token
        .strip_prefix(option)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn diagnostic(origin: &str, line: usize, message: String) -> ImportDiagnostic {
    ImportDiagnostic {
        origin: origin.to_owned(),
        line,
        message,
    }
}

fn fingerprint(source: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn logical_lines(source: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut buffer = String::new();
    let mut start = 1;
    for (offset, line) in source.lines().enumerate() {
        let number = offset + 1;
        if buffer.is_empty() {
            start = number;
        }
        let continuation = line.trim_end().ends_with('\\');
        let part = if continuation {
            line.trim_end().trim_end_matches('\\')
        } else {
            line
        };
        buffer.push_str(part);
        if continuation {
            buffer.push(' ');
        } else {
            lines.push((start, std::mem::take(&mut buffer)));
        }
    }
    if !buffer.is_empty() {
        lines.push((start, buffer));
    }
    lines
}

fn shell_words(source: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in source.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            started = true;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                word.push(character);
            }
            started = true;
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                started = true;
            }
            '#' if !started => break,
            ';' if !started => break,
            character if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(character);
                started = true;
            }
        }
    }
    if escaped {
        return Err("trailing escape in completion declaration".to_owned());
    }
    if quote.is_some() {
        return Err("unclosed quote in completion declaration".to_owned());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fish_declarations_preserve_options_descriptions_values_and_conditions() {
        let source = "complete -c deploy -s e -l environment -r \\\n+            -d 'Target environment' -n '__fish_use_subcommand'";
        let report = import_fish(source, "deploy.fish");
        assert!(report.diagnostics.is_empty());
        let command = &report.commands[0];
        assert_eq!(command.path, "deploy");
        assert_eq!(command.options[0].names, vec!["--environment", "-e"]);
        assert_eq!(command.options[0].value_type, "value");
        assert_eq!(command.options[0].documentation, "Target environment");
        assert!(command.details.contains("__fish_use_subcommand"));
        assert_eq!(command.options[0].provenance.source, Provenance::Fish);
    }

    #[test]
    fn fish_declarations_for_one_command_are_merged_deterministically() {
        let report = import_fish(
            "complete -c demo -l verbose -d Verbose\ncomplete -c demo -l output -r",
            "demo.fish",
        );
        assert_eq!(report.commands.len(), 1);
        assert_eq!(report.commands[0].options[0].names, vec!["--output"]);
        assert_eq!(report.commands[0].options[1].names, vec!["--verbose"]);
    }

    #[test]
    fn bash_word_lists_and_dynamic_providers_are_attributed_without_execution() {
        let report = import_bash(
            "complete -o filenames -W '--all --output= --verbose' -F _demo demo",
            "demo.bash",
        );
        assert!(report.diagnostics.is_empty());
        let command = &report.commands[0];
        assert_eq!(command.path, "demo");
        assert!(command.details.contains("_demo"));
        assert_eq!(command.options.len(), 3);
        let output = command
            .options
            .iter()
            .find(|option| option.names == ["--output"])
            .expect("output option imported");
        assert_eq!(output.value_type, "value");
        assert_eq!(output.provenance.source, Provenance::Bash);
    }

    #[test]
    fn malformed_declarations_are_diagnostics_not_partial_facts() {
        let report = import_fish("complete -c 'broken", "broken.fish");
        assert!(report.commands.is_empty());
        assert_eq!(report.diagnostics[0].line, 1);
        assert!(report.diagnostics[0].message.contains("unclosed quote"));
    }

    #[test]
    fn fingerprints_are_stable_and_change_with_the_source() {
        let first = import_fish("complete -c a -l all", "a.fish");
        let second = import_fish("complete -c a -l all", "a.fish");
        let changed = import_fish("complete -c a -l almost", "a.fish");
        assert_eq!(
            first.commands[0].provenance.fingerprint,
            second.commands[0].provenance.fingerprint
        );
        assert_ne!(
            first.commands[0].provenance.fingerprint,
            changed.commands[0].provenance.fingerprint
        );
    }

    #[test]
    fn zsh_arguments_describe_and_values_are_imported_without_execution() {
        let source = r#"#compdef deploy
actions=(
  'start:Start services'
  'stop:Stop services'
)
_arguments \
  '(-v --verbose)'{-v,--verbose}'[Show details]' \
  '--output=[Write result]:file:_files' \
  '--dynamic[Dynamic]:value:$(dangerous-provider)'
_describe 'action' actions
_values 'environment' staging production
"#;
        let report = import_zsh(source, "_deploy");
        assert!(report.diagnostics.is_empty());
        let command = report
            .commands
            .iter()
            .find(|command| command.path == "deploy")
            .unwrap();
        assert_eq!(command.provenance.source, Provenance::Zsh);
        assert!(command.details.contains("staging"));
        assert!(command.details.contains("not executed"));
        let verbose = command
            .options
            .iter()
            .find(|option| option.names.contains(&"--verbose".to_owned()))
            .unwrap();
        assert_eq!(verbose.names, ["--verbose", "-v"]);
        assert_eq!(verbose.documentation, "Show details");
        let output = command
            .options
            .iter()
            .find(|option| option.names == ["--output"])
            .unwrap();
        assert_eq!(output.value_type, "file");
        assert!(report
            .commands
            .iter()
            .any(|command| command.path == "deploy start" && command.summary == "Start services"));
    }

    #[test]
    fn supplied_help_text_yields_bounded_attributed_options() {
        let source = "Usage: demo [OPTIONS]\n\nOptions:\n  -a, --all          Show all entries\n  -o, --output FILE  Write output\n      --color=<WHEN> Color mode\n";
        let report = import_help(source, "demo.help.txt");
        assert!(report.diagnostics.is_empty());
        let command = &report.commands[0];
        assert_eq!(command.path, "demo");
        assert_eq!(command.provenance.source, Provenance::Help);
        let output = command
            .options
            .iter()
            .find(|option| option.names.contains(&"--output".to_owned()))
            .unwrap();
        assert_eq!(output.names, ["--output", "-o"]);
        assert_eq!(output.value_type, "FILE");
        assert_eq!(output.documentation, "Write output");
        let color = command
            .options
            .iter()
            .find(|option| option.names == ["--color"])
            .unwrap();
        assert_eq!(color.value_type, "WHEN");
    }

    #[test]
    fn supplied_man_text_handles_simple_roff_options() {
        let source = ".TH DEMO 1\n.SH SYNOPSIS\ndemo [OPTIONS]\n.SH OPTIONS\n.BR \\-q , \\--quiet\n  Suppress output\n.B \\--format=FORMAT\n";
        let report = import_man(source, "demo.man");
        let command = &report.commands[0];
        assert_eq!(command.provenance.source, Provenance::Man);
        assert!(command
            .options
            .iter()
            .any(|option| option.names.contains(&"--quiet".to_owned())));
        assert!(command
            .options
            .iter()
            .any(|option| option.names == ["--format"]));
    }

    #[test]
    fn oversized_help_input_is_truncated_deterministically() {
        let mut source = String::from("Usage: huge [OPTIONS]\n");
        source.push_str(&"x".repeat(MAX_HELP_BYTES + 32));
        let report = import_help(&source, "huge.help");
        assert_eq!(report.commands[0].path, "huge");
        assert!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("truncated")));
    }
}
