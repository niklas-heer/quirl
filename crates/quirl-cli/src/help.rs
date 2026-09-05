//! Bounded interactive discovery rendered through the active terminal surface.
//!
//! Catalog text may come from local imports. Help never executes it: it scans at
//! most 4,096 commands, bounds every displayed field, and escapes terminal controls.
//! A failed query leaves the editor and session usable. Large results are visibly
//! shortened instead of flooding the transcript or picking an arbitrary prefix.

use quirl_catalog::{Catalog, CommandSpec};
use quirl_core::{ErrorCode, ShellError, escape_terminal_controls};
use std::fmt::{self, Write};
use unicode_width::UnicodeWidthChar;

const QUERY_BYTES_MAX: usize = 256;
const COMMANDS_MAX: usize = 4_096;
const RESULTS_MAX: usize = 12;
const OUTPUT_BYTES_MAX: usize = 64 * 1024;
const UNWRAPPED_BYTES_MAX: usize = OUTPUT_BYTES_MAX / 2;
const FIELD_BYTES_MAX: usize = 4_096;
const TRUNCATION: &str =
    "\n[Help shortened; refine the topic or use `quirl describe <command>`.]\n";

pub(super) fn render(
    catalog: &Catalog,
    topic: Option<&str>,
    width: u16,
) -> Result<String, ShellError> {
    let query = topic.unwrap_or("").trim();
    if query.len() > QUERY_BYTES_MAX {
        return Err(
            ShellError::new(ErrorCode::ResourceLimit, "help topic is too long")
                .with_context(format!(
                    "limit {QUERY_BYTES_MAX} bytes; observed {} bytes",
                    query.len()
                ))
                .with_help("Use a command name or a few words, such as `help data`"),
        );
    }
    let mut output = HelpText::default();
    if query.is_empty() {
        let _ = writeln!(output, "Getting started with Quirl\n");
        // The entry point must remain available even when imported commands
        // sort ahead of every builtin or attempt to replace its description.
        let builtins = Catalog::builtin();
        if let Some(command) = builtins
            .commands
            .iter()
            .find(|command| command.path == "help")
        {
            let _ = writeln!(output, "{}", display(&command.details));
            examples(&mut output, command);
        }
    } else {
        topic_help(&mut output, catalog, query);
    }
    if catalog.commands.len() > COMMANDS_MAX {
        let _ = writeln!(
            output,
            "Search covers the first {COMMANDS_MAX} catalog commands; use Tab to explore more."
        );
    }
    Ok(wrap(
        &output.finish(),
        usize::from(width.saturating_sub(2)).clamp(2, 120),
    ))
}

fn topic_help(output: &mut HelpText, catalog: &Catalog, query: &str) {
    let commands = catalog.commands.iter().take(COMMANDS_MAX);
    let qualified = format!("quirl {query}");
    let exact = commands
        .clone()
        .find(|command| command.path == query)
        .or_else(|| commands.clone().find(|command| command.path == qualified));
    let mut aliases = commands
        .clone()
        .filter(|command| command.aliases.iter().take(32).any(|alias| alias == query));
    let first_alias = aliases.next();
    if exact.is_none() && aliases.next().is_some() {
        matches(
            output,
            commands
                .clone()
                .filter(|command| command.aliases.iter().take(32).any(|alias| alias == query)),
            "Commands sharing this alias",
        );
    } else if let Some(command) = exact.or(first_alias) {
        command_help(output, command);
        let child_prefix = format!("{} ", prefix(&command.path, FIELD_BYTES_MAX));
        matches(
            output,
            commands.filter(|item| item.path.starts_with(&child_prefix)),
            "Subcommands",
        );
    } else {
        let normalized = query.to_ascii_lowercase();
        let found =
            commands.filter(|command| query == "all" || search_matches(command, &normalized));
        let _ = writeln!(output, "Commands matching `{}`", display(query));
        if !matches(output, found, "") {
            let _ = writeln!(
                output,
                "No matching command. Try a shorter topic, `help`, or `help all`.\nTab and F1 also explore the command at your cursor."
            );
        }
    }
}

fn search_matches(command: &CommandSpec, query: &str) -> bool {
    let path = prefix(&command.path, FIELD_BYTES_MAX).to_ascii_lowercase();
    let summary = prefix(&command.summary, FIELD_BYTES_MAX).to_ascii_lowercase();
    query
        .split_whitespace()
        .all(|word| path.contains(word) || summary.contains(word))
}

fn command_help(output: &mut HelpText, command: &CommandSpec) {
    let _ = writeln!(
        output,
        "{}\n  {}\n\n{}",
        display(&command.signature),
        display(&command.summary),
        display(&command.details)
    );
    if !command.options.is_empty() {
        let _ = writeln!(output, "\nArguments and options:");
        for option in command.options.iter().take(16) {
            for (index, name) in option.names.iter().take(8).enumerate() {
                if index > 0 {
                    let _ = write!(output, ", ");
                }
                let _ = write!(output, "{}", display(name));
            }
            let _ = writeln!(output, "  {}", display(&option.documentation));
        }
        if command.options.len() > 16 {
            output.truncated = true;
        }
    }
    examples(output, command);
}

fn examples(output: &mut HelpText, command: &CommandSpec) {
    if !command.examples.is_empty() {
        let _ = writeln!(output, "\nTry:");
        for example in command.examples.iter().take(8) {
            let _ = writeln!(output, "  {}", display(example));
        }
        if command.examples.len() > 8 {
            output.truncated = true;
        }
    }
}

fn matches<'a>(
    output: &mut HelpText,
    mut commands: impl Iterator<Item = &'a CommandSpec>,
    heading: &str,
) -> bool {
    let Some(first) = commands.next() else {
        return false;
    };
    if !heading.is_empty() {
        let _ = writeln!(output, "\n{heading}:");
    }
    for command in
        std::iter::once(first).chain(commands.by_ref().take(RESULTS_MAX.saturating_sub(1)))
    {
        let _ = writeln!(
            output,
            "  {} — {}",
            display(&command.path),
            display(&command.summary)
        );
    }
    if commands.next().is_some() {
        let _ = writeln!(
            output,
            "  More matches; add a command name or another search word."
        );
    }
    let _ = writeln!(output, "\nUse `help <command>` to read its examples.");
    true
}

fn prefix(value: &str, bytes: usize) -> &str {
    value
        .get(..value.floor_char_boundary(bytes.min(value.len())))
        .unwrap_or_default()
}

fn display(value: &str) -> String {
    let shown = prefix(value, FIELD_BYTES_MAX);
    let mut rendered = escape_terminal_controls(shown).replace('\t', "\\t");
    if shown.len() < value.len() {
        rendered.push_str("… [shortened]");
    }
    rendered
}

// At most one newline is added per input character, so the wrapped result
// fits twice the bounded writer's byte budget. Each turn scans one bounded
// display line; long catalog paragraphs remain readable in narrow terminals.
fn wrap(text: &str, width: usize) -> String {
    let mut output = String::with_capacity(text.len());
    for line in text.lines() {
        let mut remaining = line;
        while !remaining.is_empty() {
            let mut columns = 0_usize;
            let mut end = 0_usize;
            let mut last_space = None;
            for (index, character) in remaining.char_indices() {
                let next = columns.saturating_add(character.width().unwrap_or(0));
                if next > width {
                    break;
                }
                if character == ' ' && index > 0 {
                    last_space = Some(index);
                }
                columns = next;
                end = index.saturating_add(character.len_utf8());
            }
            if end < remaining.len() {
                end = last_space.unwrap_or(end);
            }
            let (shown, rest) = remaining.split_at(end);
            output.push_str(shown);
            remaining = rest.trim_start_matches(' ');
            if !remaining.is_empty() {
                output.push('\n');
            }
        }
        output.push('\n');
    }
    output
}

#[derive(Default)]
struct HelpText {
    text: String,
    truncated: bool,
}

impl Write for HelpText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let available = UNWRAPPED_BYTES_MAX
            .saturating_sub(TRUNCATION.len())
            .saturating_sub(self.text.len());
        let shown = prefix(text, available);
        self.text.push_str(shown);
        self.truncated |= shown.len() < text.len();
        Ok(())
    }
}

impl HelpText {
    fn finish(mut self) -> String {
        if self.truncated {
            self.text.push_str(TRUNCATION);
        }
        self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(catalog: &Catalog, topic: Option<&str>) -> Result<String, ShellError> {
        super::render(catalog, topic, 100)
    }

    #[test]
    fn overview_teaches_the_first_session_without_dumping_the_catalog() {
        let catalog = Catalog::builtin();
        let output = render(&catalog, None).unwrap();
        assert!(output.contains("Getting started"));
        assert!(output.contains("mode data"));
        assert!(output.contains("Ctrl-D"));
        assert!(output.contains("help data"));
        assert!(!output.contains("quirl serve mcp"));
    }

    #[test]
    fn exact_and_shorthand_topics_show_the_same_catalog_contract() {
        let catalog = Catalog::builtin();
        assert_eq!(
            render(&catalog, Some("data")).unwrap(),
            render(&catalog, Some("quirl data")).unwrap()
        );
        let output = render(&catalog, Some("mode")).unwrap();
        assert!(output.contains("Switch the visible interactive grammar"));
        assert!(output.contains("mode normal"));
    }

    #[test]
    fn partial_and_keyword_queries_list_choices_without_claiming_an_exact_match() {
        let catalog = Catalog::builtin();
        let partial = render(&catalog, Some("quirl config")).unwrap();
        assert!(partial.contains("Commands matching"));
        assert!(partial.contains("quirl config check"));
        assert!(partial.contains("quirl config web"));
        let keyword = render(&catalog, Some("structured pipeline")).unwrap();
        assert!(keyword.contains("quirl data"));
        let missing = render(&catalog, Some("not-a-quirl-command")).unwrap();
        assert!(missing.contains("No matching command"));
        assert!(missing.contains("help all"));
    }

    #[test]
    fn exact_paths_win_over_aliases_and_ambiguous_aliases_offer_choices() {
        let mut catalog = Catalog::builtin();
        let mut imported = catalog.commands[0].clone();
        imported.path = "a-imported".to_owned();
        imported.signature = "a-imported".to_owned();
        imported.aliases = vec!["mode".to_owned(), "shared".to_owned()];
        catalog.commands.insert(0, imported.clone());
        assert!(render(&catalog, Some("mode")).unwrap().starts_with("mode "));
        imported.path = "b-imported".to_owned();
        imported.signature = "b-imported".to_owned();
        catalog.commands.insert(1, imported);
        let choices = render(&catalog, Some("shared")).unwrap();
        assert!(choices.contains("Commands sharing this alias"));
        assert!(choices.contains("a-imported"));
        assert!(choices.contains("b-imported"));
    }

    #[test]
    fn onboarding_survives_large_catalogs_and_help_wraps_to_terminal_columns() {
        let mut catalog = Catalog::builtin();
        let mut imported = catalog.commands[0].clone();
        imported.path = "a-imported".to_owned();
        catalog
            .commands
            .splice(..0, std::iter::repeat_n(imported, COMMANDS_MAX));
        assert!(render(&catalog, None).unwrap().contains("mode data"));
        let output = super::render(&catalog, None, 40).unwrap();
        assert!(
            output
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 38)
        );
        assert!(
            wrap("界界界界", 3)
                .lines()
                .all(|line| unicode_width::UnicodeWidthStr::width(line) <= 3)
        );
    }

    #[test]
    fn untrusted_help_is_bounded_escaped_and_a_later_valid_query_recovers() {
        let mut catalog = Catalog::builtin();
        let item = catalog
            .commands
            .iter_mut()
            .find(|command| command.path == "mode")
            .unwrap();
        item.details = "\x1b[2J".repeat(2_000);
        item.examples = vec!["é".repeat(8_000); 100];
        let output = render(&catalog, Some("mode")).unwrap();
        assert!(output.len() <= OUTPUT_BYTES_MAX);
        assert!(!output.contains('\x1b'));
        assert!(output.contains("shortened"));
        let error = render(&catalog, Some(&"é".repeat(QUERY_BYTES_MAX))).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(render(&catalog, Some("cd")).unwrap().contains("cd .."));
        assert!(render(&catalog, Some(&"x".repeat(QUERY_BYTES_MAX))).is_ok());
    }
}
