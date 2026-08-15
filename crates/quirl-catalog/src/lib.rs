//! One semantic catalog powers Quirl's completion, help, validation, docs, and AI API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Catalog {
    pub schema_version: u32,
    pub commands: Vec<CommandSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    /// Space-separated command path, such as `git commit`.
    pub path: String,
    pub signature: String,
    pub summary: String,
    pub details: String,
    pub options: Vec<OptionSpec>,
    pub examples: Vec<String>,
    pub effects: Vec<Effect>,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptionSpec {
    pub names: Vec<String>,
    pub value: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    ReadFilesystem,
    WriteFilesystem,
    SpawnProcess,
    ChangeDirectory,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Builtin,
    External,
    Lua,
    Steel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Completion {
    pub value: String,
    pub display: String,
    pub summary: String,
    pub detail: String,
    pub replace_start: usize,
    pub replace_end: usize,
    pub match_indices: Vec<usize>,
}

impl Catalog {
    pub fn builtin() -> Self {
        Self {
            schema_version: 1,
            commands: vec![
                command(
                    "help",
                    "help [command]",
                    "Explore commands and their contracts",
                    "Reads this same catalog used by completion and AI discovery.",
                    vec![],
                    &["help git commit"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "mode",
                    "mode <command|data|toggle>",
                    "Switch the visible interactive grammar",
                    "Command mode carries bytes and process status. Data mode evaluates Steel values in this prototype.",
                    vec![],
                    &["mode data", "mode command", "mode toggle"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "ls",
                    "ls [path] [options]",
                    "List a directory as structured entries",
                    "Quirl's native ls renders a table for humans and stable JSON for tools.",
                    vec![
                        option(&["-a", "--all"], None, "Include hidden entries"),
                        option(&["-l", "--long"], None, "Show size, kind, and modified time"),
                        option(&["--json"], None, "Emit stable structured JSON"),
                    ],
                    &["ls", "ls --long src", "ls --json | jq"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "cd",
                    "cd [path]",
                    "Change the shell working directory",
                    "Changes Quirl's process directory so later commands and prompt context follow it.",
                    vec![],
                    &["cd .."],
                    &[Effect::ChangeDirectory],
                    Provenance::Builtin,
                ),
                command(
                    "lua",
                    "lua <expression>",
                    "Evaluate Lua without leaving command mode",
                    "Runs an expression in the persistent restricted Lua 5.4 VM.",
                    vec![],
                    &["lua return 20 + 22"],
                    &[],
                    Provenance::Lua,
                ),
                command(
                    "steel",
                    "steel <expression>",
                    "Evaluate Steel without leaving command mode",
                    "Runs an expression in the persistent embedded Steel VM.",
                    vec![],
                    &["steel (+ 1 2)"],
                    &[],
                    Provenance::Steel,
                ),
                command(
                    "quirl run",
                    "quirl run <file>",
                    "Run a Lua, Steel, Scheme, or prototype .quirl script",
                    "Lua is the first-class extension language; legacy prototype files continue through Steel during migration.",
                    vec![],
                    &["quirl run scripts/deploy.lua -- staging"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl check",
                    "quirl check <file> [--format text|json]",
                    "Validate a script without executing it",
                    "Parses and lints Lua, validates restricted APIs, and returns structured diagnostics without running it.",
                    vec![option(&["--format"], Some("text|json"), "Choose diagnostic output")],
                    &["quirl check scripts/deploy.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl fmt",
                    "quirl fmt <file> [--check]",
                    "Format a Lua extension file",
                    "Applies Quirl's deterministic Lua formatting contract or checks it in CI.",
                    vec![option(&["--check"], None, "Report drift without writing")],
                    &["quirl fmt examples/config.lua --check"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl lint",
                    "quirl lint <file> [--format text|json]",
                    "Lint Lua without executing it",
                    "Checks syntax and rejects ambient APIs that bypass Quirl capabilities.",
                    vec![option(&["--format"], Some("text|json"), "Choose diagnostic output")],
                    &["quirl lint examples/plugin.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl test",
                    "quirl test <file>",
                    "Run a Lua test module under resource limits",
                    "Runs every returned `test_*` function in the same restricted runtime used by extensions.",
                    vec![],
                    &["quirl test examples/lua_tests.lua"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config check",
                    "quirl config check <file> [--format text|json]",
                    "Validate Lua configuration through Rust schemas",
                    "Evaluates under config restrictions and preserves the active last-known-good value on failure.",
                    vec![option(&["--format"], Some("text|json"), "Choose output format")],
                    &["quirl config check examples/config.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl plugin check",
                    "quirl plugin check <file> [--format text|json]",
                    "Validate Lua plugin registrations",
                    "Loads a trusted plugin with process access denied and validates prompt and completion callbacks.",
                    vec![option(&["--format"], Some("text|json"), "Choose output format")],
                    &["quirl plugin check examples/plugin.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl sdk",
                    "quirl sdk [--format text|json|markdown]",
                    "Export the generated Lua extension SDK",
                    "LuaLS stubs, AI JSON, and human documentation are generated from the same Rust host API definitions.",
                    vec![option(
                        &["--format"],
                        Some("text|json|markdown"),
                        "Choose the generated SDK view",
                    )],
                    &["quirl sdk --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl catalog",
                    "quirl catalog [--format json|markdown]",
                    "Export installed command knowledge for humans or AI",
                    "Emits the versioned semantic catalog bundled with this binary.",
                    vec![option(&["--format"], Some("json|markdown"), "Choose a stable output format")],
                    &["quirl catalog --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "git commit",
                    "git commit [options]",
                    "Record changes to the repository",
                    "External command metadata demonstrates imported completion knowledge.",
                    vec![
                        option(&["-m", "--message"], Some("message"), "Use the given commit message"),
                        option(&["-a", "--all"], None, "Stage modified and deleted tracked files"),
                        option(&["--amend"], None, "Replace the tip of the current branch"),
                        option(&["--no-verify"], None, "Bypass pre-commit and commit-msg hooks"),
                    ],
                    &["git commit -m \"Explain the change\""],
                    &[Effect::WriteFilesystem, Effect::SpawnProcess],
                    Provenance::External,
                ),
                command(
                    "git status",
                    "git status [--short]",
                    "Show repository and working-tree status",
                    "External command metadata can eventually be imported from generated specs and help output.",
                    vec![option(&["-s", "--short"], None, "Use the compact status format")],
                    &["git status --short"],
                    &[Effect::ReadFilesystem, Effect::SpawnProcess],
                    Provenance::External,
                ),
            ],
        }
    }

    /// Complete command paths or options using a deterministic fuzzy subsequence score.
    pub fn complete(&self, input: &str, cursor: usize) -> Vec<Completion> {
        let cursor = cursor.min(input.len());
        let before = &input[..cursor];
        let trimmed_start = before.len() - before.trim_start().len();
        let query = before.trim_start();

        if let Some((command, token_start, token)) = self.option_context(query, trimmed_start) {
            let mut choices = command
                .options
                .iter()
                .flat_map(|option| option.names.iter().map(move |name| (name, option)))
                .filter_map(|(name, option)| {
                    fuzzy_match(token, name).map(|(score, indices)| {
                        (
                            score,
                            Completion {
                                value: name.clone(),
                                display: match &option.value {
                                    Some(value) => format!("{name} <{value}>"),
                                    None => name.clone(),
                                },
                                summary: option.summary.clone(),
                                detail: command.signature.clone(),
                                replace_start: token_start,
                                replace_end: cursor,
                                match_indices: indices,
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();
            choices.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then_with(|| left.1.value.cmp(&right.1.value))
            });
            return choices.into_iter().map(|(_, item)| item).collect();
        }

        let mut choices = self
            .commands
            .iter()
            .filter_map(|command| {
                fuzzy_match(query, &command.path).map(|(score, indices)| {
                    (
                        score,
                        Completion {
                            value: command.path.clone(),
                            display: command.signature.clone(),
                            summary: command.summary.clone(),
                            detail: format!("{} · {:?}", command.details, command.provenance),
                            replace_start: trimmed_start,
                            replace_end: cursor,
                            match_indices: indices,
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        choices.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.value.cmp(&right.1.value))
        });
        choices.into_iter().map(|(_, item)| item).collect()
    }

    pub fn find(&self, topic: &str) -> Option<&CommandSpec> {
        let topic = topic.trim();
        self.commands
            .iter()
            .find(|command| command.path == topic)
            .or_else(|| {
                self.commands
                    .iter()
                    .find(|command| command.path.starts_with(topic))
            })
    }

    pub fn to_markdown(&self) -> String {
        let mut output = String::from("# Quirl command catalog\n\n");
        for command in &self.commands {
            output.push_str(&format!(
                "## `{}`\n\n{}\n\n",
                command.signature, command.summary
            ));
            output.push_str(&format!("{}\n\n", command.details));
            if !command.options.is_empty() {
                output.push_str("Options:\n\n");
                for option in &command.options {
                    output.push_str(&format!(
                        "- `{}` — {}\n",
                        option.names.join("`, `"),
                        option.summary
                    ));
                }
                output.push('\n');
            }
        }
        output
    }

    fn option_context<'catalog, 'query>(
        &'catalog self,
        query: &'query str,
        leading_whitespace: usize,
    ) -> Option<(&'catalog CommandSpec, usize, &'query str)> {
        let token_start = query
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        let token = &query[token_start..];
        if !token.starts_with('-') {
            return None;
        }
        let command_text = query[..token_start].trim_end();
        let command = self
            .commands
            .iter()
            .filter(|command| {
                command_text == command.path
                    || command_text.starts_with(&format!("{} ", command.path))
            })
            .max_by_key(|command| command.path.len())?;
        Some((command, leading_whitespace + token_start, token))
    }
}

fn fuzzy_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    let query = query.to_lowercase();
    let candidate_lower = candidate.to_lowercase();
    if query.is_empty() {
        return Some((0, vec![]));
    }
    if candidate_lower.starts_with(&query) {
        return Some((
            10_000 - candidate.len() as i32,
            (0..query.chars().count()).collect(),
        ));
    }

    let mut indices = Vec::new();
    let mut candidate_chars = candidate_lower.char_indices();
    for wanted in query.chars() {
        let (byte_index, _) = candidate_chars.find(|(_, actual)| *actual == wanted)?;
        indices.push(candidate_lower[..byte_index].chars().count());
    }
    let spread = indices.last().copied().unwrap_or_default() as i32;
    Some((1_000 - spread - candidate.len() as i32, indices))
}

fn option(names: &[&str], value: Option<&str>, summary: &str) -> OptionSpec {
    OptionSpec {
        names: names.iter().map(|name| (*name).to_owned()).collect(),
        value: value.map(str::to_owned),
        summary: summary.to_owned(),
    }
}

#[allow(clippy::too_many_arguments)]
fn command(
    path: &str,
    signature: &str,
    summary: &str,
    details: &str,
    options: Vec<OptionSpec>,
    examples: &[&str],
    effects: &[Effect],
    provenance: Provenance,
) -> CommandSpec {
    CommandSpec {
        path: path.to_owned(),
        signature: signature.to_owned(),
        summary: summary.to_owned(),
        details: details.to_owned(),
        options,
        examples: examples
            .iter()
            .map(|example| (*example).to_owned())
            .collect(),
        effects: effects.to_vec(),
        provenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_command_completion_discovers_subcommands() {
        let completions = Catalog::builtin().complete("git c", 5);
        assert_eq!(completions[0].value, "git commit");
        assert!(completions[0].summary.contains("Record"));
    }

    #[test]
    fn option_completion_uses_command_context() {
        let completions = Catalog::builtin().complete("git commit --am", 15);
        assert_eq!(completions[0].value, "--amend");
        assert_eq!(completions[0].replace_start, 11);
    }

    #[test]
    fn catalog_is_machine_readable() {
        let json = serde_json::to_string(&Catalog::builtin()).unwrap();
        assert!(json.contains("schema_version"));
        assert!(json.contains("git commit"));
    }
}
