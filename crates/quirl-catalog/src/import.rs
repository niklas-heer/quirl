use crate::{
    CATALOG_SCHEMA_VERSION, Catalog, CommandSpec, CompletionSource, Confidence, OptionSpec,
    Provenance, ProvenanceInfo, imported_argument, imported_command,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::Path};

const MAX_HELP_BYTES: usize = 1024 * 1024;
const MAX_HELP_LINES: usize = 20_000;
const MAX_HELP_OPTIONS: usize = 2_048;
const MAX_MAN_OPTION_DESCRIPTION_BYTES: usize = 4 * 1024;
const MAX_COMPLETION_IMPORT_BYTES: usize = 4 * 1024 * 1024;
const MAX_COMPLETION_IMPORT_ORIGIN_BYTES: usize = 4 * 1024;
const MAX_COMPLETION_IMPORT_LINES: usize = 20_000;
const MAX_IMPORT_TOKENS_PER_DECLARATION: usize = 16_384;
const MAX_COMMANDS_PER_DECLARATION: usize = 256;
const MAX_RETAINED_COMMANDS: usize = 2_048;
const MAX_IMPORT_CANDIDATES: usize = 4_096;
const MAX_RETAINED_IMPORT_BYTES: usize = 4 * 1024 * 1024;
const MAX_IMPORT_DIAGNOSTICS: usize = 256;

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
    let (source, source_truncated) = bounded_prefix(source, MAX_COMPLETION_IMPORT_BYTES);
    let (origin, origin_truncated) = bounded_prefix(origin, MAX_COMPLETION_IMPORT_ORIGIN_BYTES);
    let fingerprint = fingerprint(source);
    let mut report = ImportReport::default();
    if origin_truncated {
        push_diagnostic(
            &mut report.diagnostics,
            diagnostic(
                origin,
                1,
                format!(
                    "Fish completion origin truncated at {MAX_COMPLETION_IMPORT_ORIGIN_BYTES} UTF-8 bytes"
                ),
            ),
        );
    }
    if source_truncated {
        push_diagnostic(
            &mut report.diagnostics,
            diagnostic(
                origin,
                1,
                format!(
                    "Fish completion source truncated at {MAX_COMPLETION_IMPORT_BYTES} UTF-8 bytes"
                ),
            ),
        );
    }
    let (lines, lines_truncated) = logical_lines(source);
    if lines_truncated {
        push_diagnostic(
            &mut report.diagnostics,
            diagnostic(
                origin,
                MAX_COMPLETION_IMPORT_LINES,
                format!(
                    "Fish completion source stopped at {MAX_COMPLETION_IMPORT_LINES} logical lines"
                ),
            ),
        );
    }
    let mut staged = Vec::new();
    let mut candidate_count = 0;
    let mut retained_bytes = 0;
    for (line_number, line) in lines {
        if !line.contains("complete") {
            continue;
        }
        let tokens = match shell_words(&line) {
            Ok(tokens) => tokens,
            Err(message) => {
                push_diagnostic(
                    &mut report.diagnostics,
                    diagnostic(origin, line_number, message),
                );
                continue;
            }
        };
        let Some(complete_index) = tokens.iter().position(|token| token == "complete") else {
            continue;
        };
        let declaration = &tokens[complete_index + 1..];
        match parse_fish_declaration(declaration, origin, &fingerprint, &mut candidate_count) {
            Ok(commands) => {
                if !retain_commands(
                    &mut staged,
                    commands,
                    &mut retained_bytes,
                    &mut report.diagnostics,
                    origin,
                    line_number,
                ) {
                    break;
                }
            }
            Err(message) => push_diagnostic(
                &mut report.diagnostics,
                diagnostic(origin, line_number, message),
            ),
        }
    }
    merge_report_commands(&mut report, staged);
    report
}

/// Import Bash `complete` declarations without sourcing or executing them.
///
/// Static `-W` word lists become option facts. `-F`, `-C`, and `-A` provider
/// declarations create attributable command entries so an index can explain
/// where dynamic completion would come from without granting it authority.
pub fn import_bash(source: &str, origin: &str) -> ImportReport {
    let (source, source_truncated) = bounded_prefix(source, MAX_COMPLETION_IMPORT_BYTES);
    let (origin, origin_truncated) = bounded_prefix(origin, MAX_COMPLETION_IMPORT_ORIGIN_BYTES);
    let fingerprint = fingerprint(source);
    let mut report = ImportReport::default();
    if origin_truncated {
        push_diagnostic(
            &mut report.diagnostics,
            diagnostic(
                origin,
                1,
                format!(
                    "Bash completion origin truncated at {MAX_COMPLETION_IMPORT_ORIGIN_BYTES} UTF-8 bytes"
                ),
            ),
        );
    }
    if source_truncated {
        push_diagnostic(
            &mut report.diagnostics,
            diagnostic(
                origin,
                1,
                format!(
                    "Bash completion source truncated at {MAX_COMPLETION_IMPORT_BYTES} UTF-8 bytes"
                ),
            ),
        );
    }
    let (lines, lines_truncated) = logical_lines(source);
    if lines_truncated {
        push_diagnostic(
            &mut report.diagnostics,
            diagnostic(
                origin,
                MAX_COMPLETION_IMPORT_LINES,
                format!(
                    "Bash completion source stopped at {MAX_COMPLETION_IMPORT_LINES} logical lines"
                ),
            ),
        );
    }
    let mut staged = Vec::new();
    let mut candidate_count = 0;
    let mut retained_bytes = 0;
    for (line_number, line) in lines {
        if !line.contains("complete") {
            continue;
        }
        let tokens = match shell_words(&line) {
            Ok(tokens) => tokens,
            Err(message) => {
                push_diagnostic(
                    &mut report.diagnostics,
                    diagnostic(origin, line_number, message),
                );
                continue;
            }
        };
        let Some(complete_index) = tokens.iter().position(|token| token == "complete") else {
            continue;
        };
        let declaration = &tokens[complete_index + 1..];
        match parse_bash_declaration(declaration, origin, &fingerprint, &mut candidate_count) {
            Ok(commands) => {
                if !retain_commands(
                    &mut staged,
                    commands,
                    &mut retained_bytes,
                    &mut report.diagnostics,
                    origin,
                    line_number,
                ) {
                    break;
                }
            }
            Err(message) => push_diagnostic(
                &mut report.diagnostics,
                diagnostic(origin, line_number, message),
            ),
        }
    }
    merge_report_commands(&mut report, staged);
    report
}

/// Import common static Zsh completion declarations without loading Zsh.
///
/// `_arguments` option specs, literal/static-array `_describe` candidates, and
/// literal/static-array `_values` candidates are recognized. Functions,
/// substitutions, and dynamic providers are recorded but never executed.
pub fn import_zsh(source: &str, origin: &str) -> ImportReport {
    let (source, source_truncated) = bounded_prefix(source, MAX_COMPLETION_IMPORT_BYTES);
    let (origin, origin_truncated) = bounded_prefix(origin, MAX_COMPLETION_IMPORT_ORIGIN_BYTES);
    let fingerprint = fingerprint(source);
    let provenance =
        ProvenanceInfo::imported(Provenance::Zsh, Confidence::High, origin, &fingerprint);
    let mut diagnostics = Vec::new();
    if origin_truncated {
        push_diagnostic(
            &mut diagnostics,
            diagnostic(
                origin,
                1,
                format!(
                    "Zsh completion origin truncated at {MAX_COMPLETION_IMPORT_ORIGIN_BYTES} UTF-8 bytes"
                ),
            ),
        );
    }
    if source_truncated {
        push_diagnostic(
            &mut diagnostics,
            diagnostic(
                origin,
                1,
                format!(
                    "Zsh completion source truncated at {MAX_COMPLETION_IMPORT_BYTES} UTF-8 bytes"
                ),
            ),
        );
    }
    let (lines, lines_truncated) = logical_lines(source);
    if lines_truncated {
        push_diagnostic(
            &mut diagnostics,
            diagnostic(
                origin,
                MAX_COMPLETION_IMPORT_LINES,
                format!(
                    "Zsh completion source stopped at {MAX_COMPLETION_IMPORT_LINES} logical lines"
                ),
            ),
        );
    }
    let commands = zsh_commands(&lines, origin, &mut diagnostics);
    let mut candidate_count = 0;
    let arrays = zsh_static_arrays(source, origin, &mut candidate_count, &mut diagnostics);
    let mut options = Vec::new();
    let mut described = Vec::new();
    let mut values = Vec::new();
    let mut dynamic = Vec::new();

    for (line_number, line) in lines {
        let tokens = match shell_words(&line) {
            Ok(tokens) => tokens,
            Err(message) => {
                push_diagnostic(&mut diagnostics, diagnostic(origin, line_number, message));
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
                    if let Err(message) =
                        admit_candidates(&mut candidate_count, 1, "Zsh dynamic declarations")
                    {
                        push_diagnostic(&mut diagnostics, diagnostic(origin, line_number, message));
                        break;
                    }
                    dynamic.push(spec.clone());
                    continue;
                }
                if let Some(option) = parse_zsh_argument(spec, &provenance) {
                    if let Err(message) =
                        admit_candidates(&mut candidate_count, 1, "Zsh argument candidates")
                    {
                        push_diagnostic(&mut diagnostics, diagnostic(origin, line_number, message));
                        break;
                    }
                    options.push(option);
                }
            }
        }
        if let Some(call) = tokens.iter().position(|token| token == "_describe") {
            let candidates = zsh_call_candidates(&tokens[call + 1..], &arrays);
            if candidates.dynamic {
                if let Err(message) =
                    admit_candidates(&mut candidate_count, 1, "Zsh `_describe` candidates")
                {
                    push_diagnostic(&mut diagnostics, diagnostic(origin, line_number, message));
                    continue;
                }
                dynamic.push(format!("_describe {}", candidates.source));
            }
            if !candidates.from_array
                && let Err(message) = admit_candidates(
                    &mut candidate_count,
                    candidates.values.len(),
                    "Zsh `_describe` candidates",
                )
            {
                push_diagnostic(&mut diagnostics, diagnostic(origin, line_number, message));
                continue;
            }
            described.extend(candidates.values);
        }
        if let Some(call) = tokens.iter().position(|token| token == "_values") {
            let candidates = zsh_call_candidates(&tokens[call + 1..], &arrays);
            if candidates.dynamic {
                if let Err(message) =
                    admit_candidates(&mut candidate_count, 1, "Zsh `_values` candidates")
                {
                    push_diagnostic(&mut diagnostics, diagnostic(origin, line_number, message));
                    continue;
                }
                dynamic.push(format!("_values {}", candidates.source));
            }
            if !candidates.from_array
                && let Err(message) = admit_candidates(
                    &mut candidate_count,
                    candidates.values.len(),
                    "Zsh `_values` candidates",
                )
            {
                push_diagnostic(&mut diagnostics, diagnostic(origin, line_number, message));
                continue;
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
    let mut imported = Vec::new();
    let mut retained_bytes = 0;
    for path in &commands {
        if !retain_commands(
            &mut imported,
            vec![imported_command(
                path.clone(),
                format!("{path} [options]"),
                "Command discovered from Zsh completion metadata".to_owned(),
                details.clone(),
                options.clone(),
                provenance.clone(),
            )],
            &mut retained_bytes,
            &mut diagnostics,
            origin,
            1,
        ) {
            break;
        }
    }
    let mut retention_exhausted = imported.len() < commands.len();
    for command in &commands {
        if retention_exhausted {
            break;
        }
        for candidate in &described {
            let (name, summary) = split_zsh_description(candidate);
            if name.starts_with('-') || name.is_empty() {
                continue;
            }
            let path = format!("{command} {name}");
            let summary = summary.unwrap_or("External subcommand").to_owned();
            retention_exhausted = !retain_commands(
                &mut imported,
                vec![imported_command(
                    path.clone(),
                    format!("{path} [options]"),
                    summary.clone(),
                    format!(
                        "{} Run `{path} --help` for authoritative runtime usage.",
                        sentence(&summary)
                    ),
                    Vec::new(),
                    provenance.clone(),
                )],
                &mut retained_bytes,
                &mut diagnostics,
                origin,
                1,
            );
            if retention_exhausted {
                break;
            }
        }
    }
    let mut report = ImportReport {
        commands: Vec::new(),
        diagnostics,
    };
    if commands.is_empty() {
        push_diagnostic(
            &mut report.diagnostics,
            diagnostic(
                origin,
                1,
                "Zsh completion has no `#compdef`, `compdef`, or inferable file name".to_owned(),
            ),
        );
    } else {
        merge_report_commands(&mut report, imported);
    }
    report
}

/// Import bounded option metadata from supplied command-help text.
pub fn import_help(source: &str, origin: &str) -> ImportReport {
    import_documentation(source, origin, Provenance::Help)
}

/// Import bounded option metadata from supplied rendered, simple roff, or BSD
/// mdoc man text without interpreting macros or executing a formatter.
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
    let mut options = match source_kind {
        Provenance::Man => parse_mdoc_options(
            &lines,
            command.as_deref(),
            origin,
            &provenance,
            &mut diagnostics,
        ),
        _ => Vec::new(),
    };
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
    let summary = match source_kind {
        Provenance::Man => documentation_summary(&lines)
            .unwrap_or_else(|| format!("Command discovered from supplied {label} text")),
        _ => format!("Command discovered from supplied {label} text"),
    };
    ImportReport {
        commands: vec![imported_command(
            command.clone(),
            format!("{command} [options]"),
            summary,
            format!(
                "Options were heuristically parsed from bounded, supplied {label} text; no command was executed."
            ),
            options,
            provenance,
        )],
        diagnostics,
    }
}

fn zsh_commands(
    lines: &[(usize, String)],
    origin: &str,
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> Vec<String> {
    let mut commands = Vec::new();
    for (line_number, line) in lines {
        let trimmed = line.trim();
        if let Some(declaration) = trimmed.strip_prefix("#compdef") {
            let declared = declaration
                .split_whitespace()
                .filter(|value| !value.starts_with('-'))
                .collect::<Vec<_>>();
            if declared.len() > MAX_COMMANDS_PER_DECLARATION {
                push_diagnostic(
                    diagnostics,
                    diagnostic(
                        origin,
                        *line_number,
                        format!(
                            "Zsh `#compdef` declaration has {} commands; limit is {MAX_COMMANDS_PER_DECLARATION}",
                            declared.len()
                        ),
                    ),
                );
            } else {
                commands.extend(declared.into_iter().map(str::to_owned));
            }
        }
        if trimmed.starts_with("compdef ")
            && let Ok(tokens) = shell_words(trimmed)
        {
            let declared = tokens
                .iter()
                .skip(2)
                .filter(|value| !value.starts_with('-'))
                .collect::<Vec<_>>();
            if declared.len() > MAX_COMMANDS_PER_DECLARATION {
                push_diagnostic(
                    diagnostics,
                    diagnostic(
                        origin,
                        *line_number,
                        format!(
                            "Zsh `compdef` declaration has {} commands; limit is {MAX_COMMANDS_PER_DECLARATION}",
                            declared.len()
                        ),
                    ),
                );
            } else {
                commands.extend(declared.into_iter().map(|value| (*value).clone()));
            }
        }
        if commands.len() > MAX_RETAINED_COMMANDS {
            push_diagnostic(
                diagnostics,
                diagnostic(
                    origin,
                    *line_number,
                    format!(
                        "Zsh completion would retain {} command names; limit is {MAX_RETAINED_COMMANDS}",
                        commands.len()
                    ),
                ),
            );
            commands.truncate(MAX_RETAINED_COMMANDS);
            break;
        }
    }
    if commands.is_empty()
        && let Some(name) = inferred_name_from_origin(origin)
    {
        commands.push(name);
    }
    commands.sort();
    commands.dedup();
    commands
}

fn inferred_name_from_origin(origin: &str) -> Option<String> {
    let name = Path::new(origin).file_name()?.to_str()?;
    let name = name.trim_start_matches('_');
    let name = name.strip_suffix(".gz").unwrap_or(name);
    let name = name
        .rsplit_once('.')
        .filter(|(_, section)| is_man_section(section))
        .map_or(name, |(stem, _)| stem);
    let name = name
        .strip_suffix(".help.txt")
        .or_else(|| name.strip_suffix(".man.txt"))
        .or_else(|| name.strip_suffix(".help"))
        .or_else(|| name.strip_suffix(".man"))
        .or_else(|| name.strip_suffix(".txt"))
        .unwrap_or(name);
    (!name.is_empty()).then(|| name.to_owned())
}

fn is_man_section(section: &str) -> bool {
    section.starts_with(|character: char| character.is_ascii_digit())
        && section
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn zsh_static_arrays(
    source: &str,
    origin: &str,
    candidate_count: &mut usize,
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> HashMap<String, Vec<String>> {
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
            if values.iter().any(|value| is_dynamic_zsh(value)) {
                index = end + 1;
                continue;
            }
            match admit_candidates(candidate_count, values.len(), "Zsh static array candidates") {
                Ok(()) => {
                    arrays.insert(name.to_owned(), values);
                }
                Err(message) => {
                    let line = source[..start]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        + 1;
                    push_diagnostic(diagnostics, diagnostic(origin, line, message));
                }
            }
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
    from_array: bool,
}

fn zsh_call_candidates(tokens: &[String], arrays: &HashMap<String, Vec<String>>) -> ZshCandidates {
    let mut positional = Vec::new();
    let mut index = 0_usize;
    while index < tokens.len() {
        let token = &tokens[index];
        if matches!(token.as_str(), "-t" | "-J" | "-V" | "-M" | "-o" | "-O") {
            index = index.saturating_add(2);
            continue;
        }
        if token.starts_with('-') {
            index = index.saturating_add(1);
            continue;
        }
        positional.push(token);
        index = index.saturating_add(1);
    }
    let mut arguments = positional.into_iter();
    let _label = arguments.next();
    let remaining = arguments.cloned().collect::<Vec<_>>();
    if remaining.len() == 1
        && let Some(values) = arrays.get(&remaining[0])
    {
        return ZshCandidates {
            values: values.clone(),
            dynamic: false,
            source: remaining[0].clone(),
            from_array: true,
        };
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
        from_array: false,
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
        if let Some(usage) = usage
            && let Some(command) = usage.split_whitespace().next()
        {
            return Some(command.rsplit('/').next().unwrap_or(command).to_owned());
        }
        if let Some(name) = trimmed.strip_prefix(".Nm ")
            && let Some(command) = name.split_whitespace().next()
        {
            return Some(command.to_owned());
        }
    }
    inferred_name_from_origin(origin)
}

fn documentation_summary(lines: &[&str]) -> Option<String> {
    let mut in_name_section = false;
    for line in lines {
        let trimmed = line.trim();
        if let Some(section) = trimmed.strip_prefix(".Sh ") {
            in_name_section = section.trim_matches('"').eq_ignore_ascii_case("NAME");
            continue;
        }
        if in_name_section && let Some(summary) = trimmed.strip_prefix(".Nd ") {
            let summary = normalize_mdoc_text(summary, None);
            if !summary.is_empty() {
                return Some(summary);
            }
        }
    }
    None
}

fn parse_mdoc_options(
    lines: &[&str],
    command: Option<&str>,
    origin: &str,
    provenance: &ProvenanceInfo,
    diagnostics: &mut Vec<ImportDiagnostic>,
) -> Vec<OptionSpec> {
    let mut options = Vec::new();
    let mut line_index = 0usize;
    while line_index < lines.len() && options.len() < MAX_HELP_OPTIONS {
        let Some(names) = parse_mdoc_option_header(lines[line_index]) else {
            line_index += 1;
            continue;
        };
        let option_line = line_index + 1;
        line_index += 1;
        let mut description = String::new();
        let mut description_truncated = false;
        while line_index < lines.len() {
            let trimmed = lines[line_index].trim();
            if trimmed.starts_with(".It ") || trimmed == ".El" || trimmed.starts_with(".Sh ") {
                break;
            }
            let text = normalize_mdoc_text(trimmed, command);
            if !text.is_empty() {
                let separator = usize::from(!description.is_empty());
                let available = MAX_MAN_OPTION_DESCRIPTION_BYTES
                    .saturating_sub(description.len())
                    .saturating_sub(separator);
                if available == 0 {
                    description_truncated = true;
                } else {
                    if separator == 1 {
                        description.push(' ');
                    }
                    let (bounded, truncated) = bounded_prefix(&text, available);
                    description.push_str(bounded);
                    description_truncated |= truncated;
                }
            }
            line_index += 1;
        }
        if description_truncated {
            push_diagnostic(
                diagnostics,
                diagnostic(
                    origin,
                    option_line,
                    format!(
                        "man option description truncated to {MAX_MAN_OPTION_DESCRIPTION_BYTES} bytes"
                    ),
                ),
            );
        }
        options.push(imported_argument(
            names,
            None,
            if description.is_empty() {
                "Imported man-page option".to_owned()
            } else {
                description
            },
            provenance.clone(),
        ));
    }
    if options.len() == MAX_HELP_OPTIONS && line_index < lines.len() {
        push_diagnostic(
            diagnostics,
            diagnostic(
                origin,
                line_index + 1,
                format!("option ingestion stopped at {MAX_HELP_OPTIONS} entries"),
            ),
        );
    }
    options
}

fn parse_mdoc_option_header(line: &str) -> Option<Vec<String>> {
    let declaration = line.trim().strip_prefix(".It ")?;
    let tokens: Vec<_> = declaration.split_whitespace().collect();
    let mut names = Vec::new();
    let mut index = 0usize;
    while index < tokens.len() {
        if tokens[index] == "Fl" {
            let flag = tokens.get(index + 1)?.trim_matches(|character: char| {
                matches!(character, ',' | ';' | '|' | '[' | ']' | '(' | ')' | '"')
            });
            if !flag.is_empty() {
                names.push(if flag.starts_with('-') {
                    flag.to_owned()
                } else {
                    format!("-{flag}")
                });
            }
            index += 2;
        } else {
            index += 1;
        }
    }
    names.sort();
    names.dedup();
    (!names.is_empty()).then_some(names)
}

fn normalize_mdoc_text(line: &str, command: Option<&str>) -> String {
    let normalized = normalize_roff(line);
    let trimmed = normalized.trim();
    if trimmed.is_empty()
        || trimmed.starts_with(".\\\"")
        || matches!(trimmed, ".Pp" | ".Bl" | ".El")
        || trimmed.starts_with(".Bl ")
    {
        return String::new();
    }
    let mut tokens = trimmed.split_whitespace();
    let mut words = Vec::new();
    while let Some(token) = tokens.next() {
        let token = token.strip_prefix('.').unwrap_or(token);
        match token {
            "Fl" => {
                if let Some(flag) = tokens.next() {
                    words.push(format!("-{}", trim_mdoc_token(flag)));
                }
            }
            "Nm" => {
                let name = tokens
                    .next()
                    .map(trim_mdoc_token)
                    .filter(|name| !name.is_empty())
                    .or(command);
                if let Some(name) = name {
                    words.push(name.to_owned());
                }
            }
            "Xr" => {
                if let Some(name) = tokens.next() {
                    let name = trim_mdoc_token(name);
                    if let Some(section) = tokens.next() {
                        words.push(format!("{name}({})", trim_mdoc_token(section)));
                    } else {
                        words.push(name.to_owned());
                    }
                }
            }
            "Ar" | "Pa" | "Dv" | "Ev" | "Cm" | "Ic" | "Li" | "Em" | "Sy" | "Ql" | "Dq" | "Sq"
            | "Pf" | "Nd" | "Pp" => {}
            _ => words.push(token.to_owned()),
        }
    }
    words.join(" ")
}

fn trim_mdoc_token(token: &str) -> &str {
    token.trim_matches(|character: char| {
        matches!(character, ',' | ';' | '|' | '[' | ']' | '(' | ')' | '"')
    })
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
            && !word.is_empty()
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
    candidate_count: &mut usize,
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
    if commands.len() > MAX_COMMANDS_PER_DECLARATION {
        return Err(format!(
            "Fish completion declaration has {} commands; limit is {MAX_COMMANDS_PER_DECLARATION}",
            commands.len()
        ));
    }
    admit_candidates(
        candidate_count,
        names
            .len()
            .saturating_add(arguments.len())
            .saturating_add(conditions.len()),
        "Fish completion declaration",
    )?;
    names.sort();
    names.dedup();
    let provenance =
        ProvenanceInfo::imported(Provenance::Fish, Confidence::High, origin, fingerprint);
    let summary = description
        .clone()
        .unwrap_or_else(|| "External command".to_owned());
    let option = (!names.is_empty())
        .then(|| imported_argument(names, value, summary.clone(), provenance.clone()));
    let scoped_subcommands = fish_seen_subcommands(&conditions);
    let declares_subcommands = option.is_none()
        && conditions
            .iter()
            .any(|condition| condition.contains("needs_subcommand"));
    let static_arguments = fish_static_arguments(&arguments);
    let mut imported = Vec::new();
    for command in commands {
        if declares_subcommands && !static_arguments.is_empty() {
            for subcommand in &static_arguments {
                let path = format!("{command} {subcommand}");
                imported.push(imported_command(
                    path.clone(),
                    format!("{path} [options]"),
                    summary.clone(),
                    format!(
                        "{} Run `{path} --help` for authoritative runtime usage.",
                        sentence(&summary)
                    ),
                    Vec::new(),
                    provenance.clone(),
                ));
            }
            continue;
        }
        if let Some(option) = &option
            && !scoped_subcommands.is_empty()
        {
            for subcommand in &scoped_subcommands {
                let path = format!("{command} {subcommand}");
                imported.push(imported_command(
                    path.clone(),
                    format!("{path} [options]"),
                    "External subcommand".to_owned(),
                    format!("Options discovered for `{path}`."),
                    vec![option.clone()],
                    provenance.clone(),
                ));
            }
            continue;
        }
        imported.push(imported_command(
            command.clone(),
            format!("{command} [options]"),
            "External command".to_owned(),
            format!("Completion metadata discovered for `{command}`."),
            option.clone().into_iter().collect(),
            provenance.clone(),
        ));
    }
    Ok(imported)
}

fn fish_seen_subcommands(conditions: &[String]) -> Vec<String> {
    let mut subcommands = Vec::new();
    for condition in conditions {
        let Some((_, declared)) = condition.split_once("__fish_seen_subcommand_from") else {
            continue;
        };
        subcommands.extend(
            shell_words(declared)
                .unwrap_or_default()
                .into_iter()
                .filter(|value| is_static_completion_word(value)),
        );
    }
    subcommands.sort();
    subcommands.dedup();
    subcommands
}

fn fish_static_arguments(arguments: &[String]) -> Vec<String> {
    let mut values = arguments
        .iter()
        .filter(|argument| !argument.contains('(') && !argument.contains('$'))
        .flat_map(|argument| shell_words(argument).unwrap_or_default())
        .filter(|value| is_static_completion_word(value))
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

fn is_static_completion_word(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn sentence(text: &str) -> String {
    let text = text.trim();
    if text.ends_with(['.', '!', '?']) {
        text.to_owned()
    } else {
        format!("{text}.")
    }
}

fn parse_bash_declaration(
    tokens: &[String],
    origin: &str,
    fingerprint: &str,
    candidate_count: &mut usize,
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
    if commands.len() > MAX_COMMANDS_PER_DECLARATION {
        return Err(format!(
            "Bash completion declaration has {} commands; limit is {MAX_COMMANDS_PER_DECLARATION}",
            commands.len()
        ));
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
    let mut option_names = Vec::new();
    for words in &word_lists {
        let candidates = shell_words(words)?;
        admit_candidates(
            candidate_count,
            candidates.len(),
            "Bash completion candidates",
        )?;
        option_names.extend(
            candidates
                .iter()
                .filter_map(|candidate| normalize_bash_option(candidate)),
        );
    }
    admit_candidates(
        candidate_count,
        providers.len(),
        "Bash completion providers",
    )?;
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
    enrich_imported_parent_commands(&mut catalog.commands);
    report.commands = catalog.commands;
}

fn enrich_imported_parent_commands(commands: &mut [CommandSpec]) {
    let child_facts = commands
        .iter()
        .filter_map(|child| {
            let (parent, name) = child.path.rsplit_once(' ')?;
            Some((parent.to_owned(), name.to_owned(), child.summary.clone()))
        })
        .collect::<Vec<_>>();
    for command in commands {
        let children = child_facts
            .iter()
            .filter(|(parent, _, _)| parent == &command.path)
            .collect::<Vec<_>>();
        if children.is_empty() {
            continue;
        }
        if matches!(
            command.summary.as_str(),
            "External command"
                | "Imported Fish completion"
                | "Command discovered from Fish completion metadata"
                | "Command discovered from Zsh completion metadata"
        ) {
            command.summary = format!("{} subcommands available", children.len());
        }
        let retained_facts = command
            .details
            .find(" Static values:")
            .or_else(|| command.details.find(" Dynamic declarations"))
            .map_or_else(String::new, |start| command.details[start..].to_owned());
        command.details = format!(
            "Available subcommands: {}.",
            children
                .iter()
                .map(|(_, name, summary)| format!("{name} — {summary}"))
                .collect::<Vec<_>>()
                .join("; ")
        );
        command.details.push_str(&retained_facts);
    }
}

fn admit_candidates(
    candidate_count: &mut usize,
    additional: usize,
    context: &str,
) -> Result<(), String> {
    let observed = candidate_count.saturating_add(additional);
    if observed > MAX_IMPORT_CANDIDATES {
        return Err(format!(
            "{context} would retain {observed} candidates; limit is {MAX_IMPORT_CANDIDATES}"
        ));
    }
    *candidate_count = observed;
    Ok(())
}

fn retain_commands(
    retained: &mut Vec<CommandSpec>,
    commands: Vec<CommandSpec>,
    retained_bytes: &mut usize,
    diagnostics: &mut Vec<ImportDiagnostic>,
    origin: &str,
    line: usize,
) -> bool {
    for command in commands {
        let observed_commands = retained.len().saturating_add(1);
        if observed_commands > MAX_RETAINED_COMMANDS {
            push_diagnostic(
                diagnostics,
                diagnostic(
                    origin,
                    line,
                    format!(
                        "completion import would retain {observed_commands} commands; limit is {MAX_RETAINED_COMMANDS}"
                    ),
                ),
            );
            return false;
        }
        let command_bytes = command_retained_bytes(&command);
        let observed_bytes = retained_bytes.saturating_add(command_bytes);
        if observed_bytes > MAX_RETAINED_IMPORT_BYTES {
            push_diagnostic(
                diagnostics,
                diagnostic(
                    origin,
                    line,
                    format!(
                        "completion import would retain {observed_bytes} UTF-8 bytes; limit is {MAX_RETAINED_IMPORT_BYTES}"
                    ),
                ),
            );
            return false;
        }
        *retained_bytes = observed_bytes;
        retained.push(command);
    }
    true
}

fn command_retained_bytes(command: &CommandSpec) -> usize {
    let mut bytes = command
        .id
        .len()
        .saturating_add(command.path.len())
        .saturating_add(command.signature.len())
        .saturating_add(command.summary.len())
        .saturating_add(command.details.len());
    bytes = command
        .aliases
        .iter()
        .chain(command.examples.iter())
        .fold(bytes, |total, value| total.saturating_add(value.len()));
    if let Some(parent) = &command.parent {
        bytes = bytes.saturating_add(parent.len());
    }
    if let Some(version) = &command.version {
        bytes = bytes.saturating_add(version.len());
    }
    bytes = bytes
        .saturating_add(command.io.input.len())
        .saturating_add(command.io.output.len())
        .saturating_add(provenance_retained_bytes(&command.provenance));
    bytes = command
        .exit_codes
        .values()
        .fold(bytes, |total, value| total.saturating_add(value.len()));
    for option in &command.options {
        bytes = option
            .names
            .iter()
            .chain(option.conflicts.iter())
            .chain(option.examples.iter())
            .fold(bytes, |total, value| total.saturating_add(value.len()));
        bytes = bytes
            .saturating_add(option.value_type.len())
            .saturating_add(option.documentation.len())
            .saturating_add(provenance_retained_bytes(&option.provenance));
        if let Some(values) = &option.values {
            bytes = match values {
                CompletionSource::Static { values } => values
                    .iter()
                    .fold(bytes, |total, value| total.saturating_add(value.len())),
                CompletionSource::Dynamic { provider } => bytes.saturating_add(provider.len()),
            };
        }
    }
    bytes
}

fn provenance_retained_bytes(provenance: &ProvenanceInfo) -> usize {
    [
        &provenance.origin,
        &provenance.fingerprint,
        &provenance.generated_at,
    ]
    .into_iter()
    .flatten()
    .fold(0, |total, value| total.saturating_add(value.len()))
}

fn push_diagnostic(diagnostics: &mut Vec<ImportDiagnostic>, diagnostic: ImportDiagnostic) {
    if diagnostics.len() < MAX_IMPORT_DIAGNOSTICS {
        diagnostics.push(diagnostic);
    }
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

fn logical_lines(source: &str) -> (Vec<(usize, String)>, bool) {
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
            if lines.len() == MAX_COMPLETION_IMPORT_LINES {
                return (lines, source.lines().count() > number);
            }
        }
    }
    if !buffer.is_empty() {
        lines.push((start, buffer));
    }
    (lines, false)
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
                    if words.len() > MAX_IMPORT_TOKENS_PER_DECLARATION {
                        return Err(format!(
                            "completion declaration has more than {MAX_IMPORT_TOKENS_PER_DECLARATION} tokens"
                        ));
                    }
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
    if words.len() > MAX_IMPORT_TOKENS_PER_DECLARATION {
        return Err(format!(
            "completion declaration has more than {MAX_IMPORT_TOKENS_PER_DECLARATION} tokens"
        ));
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ArgumentKind;

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
        assert_eq!(command.summary, "External command");
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
    fn fish_subcommands_and_scoped_options_form_command_paths() {
        let source = r#"
complete -c ghq -f
complete -c ghq -n __fish_ghq_needs_subcommand -a get -d 'Clone/sync with a remote repository'
complete -c ghq -n __fish_ghq_needs_subcommand -a list -d 'List local repositories'
complete -c ghq -n '__fish_seen_subcommand_from get' -s u -l update -d 'Update an existing clone'
complete -c ghq -n '__fish_seen_subcommand_from list' -s p -l full-path -d 'Print full paths'
"#;
        let report = import_fish(source, "ghq.fish");
        assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
        let root = report
            .commands
            .iter()
            .find(|command| command.path == "ghq")
            .unwrap();
        assert_eq!(root.summary, "2 subcommands available");
        assert!(root.details.contains("get — Clone/sync"));
        assert!(root.details.contains("list — List local"));
        let get = report
            .commands
            .iter()
            .find(|command| command.path == "ghq get")
            .unwrap();
        assert_eq!(get.summary, "Clone/sync with a remote repository");
        assert!(
            get.options
                .iter()
                .any(|option| option.names == ["--update", "-u"])
        );
        let list = report
            .commands
            .iter()
            .find(|command| command.path == "ghq list")
            .unwrap();
        assert_eq!(list.summary, "List local repositories");
        assert!(
            list.options
                .iter()
                .any(|option| option.names == ["--full-path", "-p"])
        );
        assert!(
            !report
                .commands
                .iter()
                .any(|command| command.path.contains("__fish"))
        );
    }

    #[test]
    fn zsh_describe_skips_tag_arguments_and_resolves_static_array() {
        let source = r#"#compdef ghq
local -a _c
_c=(
  'get:Clone/sync with a remote repository'
  'list:List local repositories'
)
_describe -t commands Commands _c
"#;
        let report = import_zsh(source, "_ghq");
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.path == "ghq get")
        );
        assert!(
            report
                .commands
                .iter()
                .any(|command| command.path == "ghq list")
        );
        assert!(
            !report
                .commands
                .iter()
                .any(|command| { matches!(command.path.as_str(), "ghq Commands" | "ghq _c") })
        );
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
        assert!(
            report.commands.iter().any(
                |command| command.path == "deploy start" && command.summary == "Start services"
            )
        );
    }

    #[test]
    fn completion_import_command_declarations_stop_at_exact_and_plus_one_bounds() {
        let fish_commands = (0..MAX_COMMANDS_PER_DECLARATION)
            .map(|index| format!("-c fish{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let fish_exact = import_fish(&format!("complete {fish_commands}"), "commands.fish");
        assert_eq!(fish_exact.commands.len(), MAX_COMMANDS_PER_DECLARATION);
        assert!(fish_exact.diagnostics.is_empty());
        let fish_plus_one = import_fish(
            &format!("complete {fish_commands} -c overflow"),
            "commands.fish",
        );
        assert!(fish_plus_one.commands.is_empty());
        assert!(
            fish_plus_one.diagnostics[0]
                .message
                .contains(&format!("limit is {MAX_COMMANDS_PER_DECLARATION}"))
        );

        let bash_commands = (0..MAX_COMMANDS_PER_DECLARATION)
            .map(|index| format!("bash{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let bash_exact = import_bash(&format!("complete {bash_commands}"), "commands.bash");
        assert_eq!(bash_exact.commands.len(), MAX_COMMANDS_PER_DECLARATION);
        assert!(bash_exact.diagnostics.is_empty());
        let bash_plus_one = import_bash(
            &format!("complete {bash_commands} overflow"),
            "commands.bash",
        );
        assert!(bash_plus_one.commands.is_empty());
        assert!(
            bash_plus_one.diagnostics[0]
                .message
                .contains(&format!("limit is {MAX_COMMANDS_PER_DECLARATION}"))
        );

        let zsh_commands = (0..MAX_COMMANDS_PER_DECLARATION)
            .map(|index| format!("zsh{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let zsh_exact = import_zsh(&format!("#compdef {zsh_commands}"), "_commands");
        assert_eq!(zsh_exact.commands.len(), MAX_COMMANDS_PER_DECLARATION);
        assert!(zsh_exact.diagnostics.is_empty());
        let zsh_plus_one = import_zsh(&format!("#compdef {zsh_commands} overflow"), "_commands");
        assert!(zsh_plus_one.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains(&format!("limit is {MAX_COMMANDS_PER_DECLARATION}"))
        }));
    }

    #[test]
    fn completion_import_candidates_stop_at_exact_and_plus_one_bounds() {
        let candidates = (0..MAX_IMPORT_CANDIDATES)
            .map(|index| format!("--value{index}"))
            .collect::<Vec<_>>();

        let fish_arguments = candidates
            .iter()
            .map(|candidate| format!("-l {}", candidate.trim_start_matches('-')))
            .collect::<Vec<_>>()
            .join(" ");
        let fish_exact = import_fish(
            &format!("complete -c demo {fish_arguments}"),
            "candidates.fish",
        );
        assert_eq!(
            fish_exact.commands[0].options[0].names.len(),
            MAX_IMPORT_CANDIDATES
        );
        assert!(fish_exact.diagnostics.is_empty());
        let fish_plus_one = import_fish(
            &format!("complete -c demo {fish_arguments} -l overflow"),
            "candidates.fish",
        );
        assert!(fish_plus_one.commands.is_empty());
        assert!(
            fish_plus_one.diagnostics[0]
                .message
                .contains(&format!("limit is {MAX_IMPORT_CANDIDATES}"))
        );

        let joined = candidates.join(" ");
        let bash_exact = import_bash(&format!("complete -W '{joined}' demo"), "candidates.bash");
        assert_eq!(bash_exact.commands[0].options.len(), MAX_IMPORT_CANDIDATES);
        assert!(bash_exact.diagnostics.is_empty());
        let bash_plus_one = import_bash(
            &format!("complete -W '{joined} --overflow' demo"),
            "candidates.bash",
        );
        assert!(bash_plus_one.commands.is_empty());
        assert!(
            bash_plus_one.diagnostics[0]
                .message
                .contains(&format!("limit is {MAX_IMPORT_CANDIDATES}"))
        );

        let zsh_candidates = (0..MAX_IMPORT_CANDIDATES)
            .map(|index| format!("value{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let zsh_exact = import_zsh(
            &format!("#compdef demo\n_values values {zsh_candidates}"),
            "_candidates",
        );
        assert!(zsh_exact.commands[0].details.contains("value4095"));
        assert!(zsh_exact.diagnostics.is_empty());
        let zsh_plus_one = import_zsh(
            &format!("#compdef demo\n_values values {zsh_candidates} overflow"),
            "_candidates",
        );
        assert!(zsh_plus_one.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains(&format!("limit is {MAX_IMPORT_CANDIDATES}"))
        }));
    }

    #[test]
    fn zsh_command_candidate_cross_product_has_a_retained_command_bound() {
        let exact_candidates = (0..MAX_RETAINED_COMMANDS - 1)
            .map(|index| format!("candidate{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let exact = import_zsh(
            &format!("#compdef demo\n_describe actions {exact_candidates}"),
            "_amplification",
        );
        assert_eq!(exact.commands.len(), MAX_RETAINED_COMMANDS);
        assert!(exact.diagnostics.is_empty());

        let plus_one = import_zsh(
            &format!("#compdef demo\n_describe actions {exact_candidates} overflow"),
            "_amplification",
        );
        assert_eq!(plus_one.commands.len(), MAX_RETAINED_COMMANDS);
        assert!(plus_one.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains(&format!("limit is {MAX_RETAINED_COMMANDS}"))
        }));
    }

    #[test]
    fn completion_import_sources_stop_at_exact_and_plus_one_byte_bounds() {
        for (declaration, origin, importer) in [
            (
                "complete -c fish\n#",
                "source.fish",
                import_fish as fn(&str, &str) -> ImportReport,
            ),
            ("complete bash\n#", "source.bash", import_bash),
            ("#compdef zsh\n#", "_source", import_zsh),
        ] {
            let exact = format!(
                "{declaration}{}",
                "x".repeat(MAX_COMPLETION_IMPORT_BYTES - declaration.len())
            );
            assert_eq!(exact.len(), MAX_COMPLETION_IMPORT_BYTES);
            assert!(importer(&exact, origin).diagnostics.is_empty());

            let plus_one = format!("{exact}x");
            let report = importer(&plus_one, origin);
            assert!(report.diagnostics.iter().any(|diagnostic| {
                diagnostic
                    .message
                    .contains(&format!("{MAX_COMPLETION_IMPORT_BYTES} UTF-8 bytes"))
            }));
        }
    }

    #[test]
    fn retained_output_bytes_accept_the_exact_bound_and_reject_plus_one() {
        let provenance =
            ProvenanceInfo::imported(Provenance::Fish, Confidence::High, "exact", "hash");
        let mut exact = imported_command(
            "exact".to_owned(),
            "exact".to_owned(),
            "exact".to_owned(),
            String::new(),
            Vec::new(),
            provenance.clone(),
        );
        let fixed_bytes = command_retained_bytes(&exact);
        exact.details = "x".repeat(MAX_RETAINED_IMPORT_BYTES - fixed_bytes);
        assert_eq!(command_retained_bytes(&exact), MAX_RETAINED_IMPORT_BYTES);
        let mut retained = Vec::new();
        let mut retained_bytes = 0;
        let mut diagnostics = Vec::new();
        assert!(retain_commands(
            &mut retained,
            vec![exact.clone()],
            &mut retained_bytes,
            &mut diagnostics,
            "exact",
            1,
        ));
        assert!(diagnostics.is_empty());

        exact.details.push('x');
        assert!(!retain_commands(
            &mut Vec::new(),
            vec![exact],
            &mut 0,
            &mut diagnostics,
            "plus-one",
            1,
        ));
        assert!(
            diagnostics[0]
                .message
                .contains(&format!("limit is {MAX_RETAINED_IMPORT_BYTES}"))
        );
    }

    #[test]
    fn completion_import_diagnostics_are_bounded_for_every_shell() {
        let malformed = "complete -c 'broken\n".repeat(MAX_IMPORT_DIAGNOSTICS + 1);
        for importer in [
            import_fish as fn(&str, &str) -> ImportReport,
            import_bash,
            import_zsh,
        ] {
            let report = importer(&malformed, "malformed");
            assert_eq!(report.diagnostics.len(), MAX_IMPORT_DIAGNOSTICS);
        }
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
        assert!(
            command
                .options
                .iter()
                .any(|option| option.names.contains(&"--quiet".to_owned()))
        );
        assert!(
            command
                .options
                .iter()
                .any(|option| option.names == ["--format"])
        );
    }

    #[test]
    fn bsd_cp_mdoc_normalizes_section_name_summary_and_option_semantics() {
        let source = ".Dd March 28, 2024\n.Dt CP 1\n.Os\n.Sh NAME\n.Nm cp\n.Nd copy files\n.Sh DESCRIPTION\n.Bl -tag -width flag\n.It Fl R\nIf the\n.Ar source_file\ndesignates a directory,\n.Nm\ncopies the directory and the entire subtree.\n.It Fl a\nArchive mode. Preserves structure and attributes of files.\n.It Fl p\nCause\n.Nm\nto preserve modification time, access time, file flags, file mode, user ID, and group ID, as allowed by permissions.\n.El\n";

        let report = import_man(source, "/usr/share/man/man1/cp.1");

        assert!(report.diagnostics.is_empty());
        let command = &report.commands[0];
        assert_eq!(command.path, "cp");
        assert_eq!(command.summary, "copy files");
        let recursive = command
            .options
            .iter()
            .find(|option| option.names == ["-R"])
            .unwrap();
        assert!(recursive.documentation.contains("copies the directory"));
        let preserve = command
            .options
            .iter()
            .find(|option| option.names == ["-p"])
            .unwrap();
        assert!(preserve.documentation.contains("file mode"));
        assert!(preserve.documentation.contains("permissions"));
    }

    #[test]
    fn man_origin_strips_section_and_compression_suffixes() {
        assert_eq!(
            inferred_name_from_origin("/man/cp.1").as_deref(),
            Some("cp")
        );
        assert_eq!(
            inferred_name_from_origin("/man/printf.1p.gz").as_deref(),
            Some("printf")
        );
    }

    #[test]
    fn real_world_sources_preserve_short_flags_and_value_consumption() {
        let reports = [
            import_fish(
                "complete -c sample -s a -d All\ncomplete -c sample -s l -d Long\ncomplete -c sample -s o -l output -r",
                "sample.fish",
            ),
            import_bash(
                "complete -o filenames -W '-a -l --all --long -o= --output=' sample",
                "sample.bash",
            ),
            import_zsh(
                "#compdef sample\n_arguments '-a[All]' '-l[Long]' '*-o[Output]:file:_files'",
                "_sample",
            ),
            import_help(
                "Usage: sample [OPTIONS]\n  -a, --all          All entries\n  -l, --long         Long rows\n  -o, --output FILE  Output file",
                "sample.help",
            ),
            import_man(
                ".TH SAMPLE 1\n.SH SYNOPSIS\nsample [OPTIONS]\n.SH OPTIONS\n.BR \\-a , \\--all\n.BR \\-l , \\--long\n.BR \\-o , \\--output=FILE",
                "sample.man",
            ),
        ];

        for report in reports {
            assert!(report.diagnostics.is_empty(), "{:#?}", report.diagnostics);
            let command = report
                .commands
                .iter()
                .find(|command| command.path == "sample")
                .unwrap();
            for short in ["-a", "-l"] {
                let argument = command
                    .options
                    .iter()
                    .find(|argument| argument.names.iter().any(|name| name == short))
                    .unwrap();
                assert_eq!(
                    argument.kind,
                    ArgumentKind::Flag,
                    "source: {:?}",
                    command.provenance.source
                );
                assert_eq!(argument.provenance.source, command.provenance.source);
            }
            let output = command
                .options
                .iter()
                .find(|argument| argument.names.iter().any(|name| name == "-o"))
                .unwrap();
            assert_eq!(output.kind, ArgumentKind::Option);
            assert_eq!(output.provenance.source, command.provenance.source);
        }
    }

    #[test]
    fn oversized_help_input_is_truncated_deterministically() {
        let mut source = String::from("Usage: huge [OPTIONS]\n");
        source.push_str(&"x".repeat(MAX_HELP_BYTES + 32));
        let report = import_help(&source, "huge.help");
        assert_eq!(report.commands[0].path, "huge");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("truncated"))
        );
    }
}
