//! One semantic catalog powers Quirl's completion, help, validation, docs, and AI API.

use serde::{Deserialize, Serialize};

mod import;

pub use import::{
    import_bash, import_fish, import_help, import_man, import_zsh, ImportDiagnostic, ImportReport,
};

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
    pub provenance: ProvenanceInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OptionSpec {
    pub names: Vec<String>,
    pub value: Option<String>,
    pub summary: String,
    pub provenance: ProvenanceInfo,
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
    Fish,
    Bash,
    Zsh,
    Help,
    Man,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Low,
    Medium,
    High,
    Exact,
}

/// Attribution for a catalog fact. Imported command options retain their own
/// provenance when multiple sources contribute to the same command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProvenanceInfo {
    pub source: Provenance,
    pub confidence: Confidence,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl ProvenanceInfo {
    pub fn builtin(source: Provenance) -> Self {
        let confidence = match source {
            Provenance::Builtin | Provenance::Lua => Confidence::Exact,
            Provenance::External => Confidence::Medium,
            Provenance::Fish | Provenance::Bash | Provenance::Zsh => Confidence::High,
            Provenance::Help | Provenance::Man => Confidence::Medium,
        };
        Self {
            source,
            confidence,
            origin: None,
            fingerprint: None,
        }
    }

    pub fn imported(
        source: Provenance,
        confidence: Confidence,
        origin: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            source,
            confidence,
            origin: Some(origin.into()),
            fingerprint: Some(fingerprint.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogExplanation {
    pub command: String,
    pub facts: Vec<FactExplanation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FactExplanation {
    pub fact: String,
    pub value: String,
    pub provenance: ProvenanceInfo,
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
            schema_version: 3,
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
                    "Command mode carries bytes and process status. Data mode evaluates Quirl's native structured values and pipelines.",
                    vec![],
                    &["mode data", "mode command", "mode toggle"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl data",
                    "quirl data <source> [| transform ...]",
                    "Evaluate a native structured-data pipeline",
                    "Sources are `pwd`, `ls [path]`, `open <path>`, or JSON. Transforms include typed `where` comparisons with `and`/`or`, dotted `get`, `select`, `sort`, `take`, `first`, and `length`.",
                    vec![],
                    &[
                        "mode data",
                        "ls . | select name kind size",
                        "ls . | where kind == file and size > 1024 | sort size desc | take 10",
                        "quirl data '[1,2,3] | length'",
                    ],
                    &[Effect::ReadFilesystem],
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
                    "quirl new",
                    "quirl new <name> [--lang lua] [--directory path]",
                    "Create a checked embedded-language script",
                    "Writes a deterministic annotated Lua template with create-new semantics, so an existing script is never overwritten.",
                    vec![
                        option(&["--lang"], Some("lua"), "Choose the generated embedded language"),
                        option(&["--directory"], Some("path"), "Choose the destination directory"),
                    ],
                    &["quirl new automation --lang lua"],
                    &[Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl describe",
                    "quirl describe <command> [--format text|json|markdown|html]",
                    "Describe one installed command",
                    "Renders one exact entry from the same semantic catalog used by completion, documentation, language services, and agents.",
                    vec![option(
                        &["--format"],
                        Some("text|json|markdown|html"),
                        "Choose a deterministic documentation view",
                    )],
                    &["quirl describe 'quirl run' --format markdown"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl doc",
                    "quirl doc [--format text|json|markdown|html] [--output path] [--open]",
                    "Generate installed command documentation",
                    "Generates deterministic human or machine documentation from the installed catalog, writes requested files atomically, and can open an explicit output in the platform viewer.",
                    vec![
                        option(
                            &["--format"],
                            Some("text|json|markdown|html"),
                            "Choose a deterministic documentation view",
                        ),
                        option(&["--output"], Some("path"), "Atomically write the generated view"),
                        option(&["--open"], None, "Open the explicit output in the default viewer"),
                    ],
                    &["quirl doc --format html --output target/quirl-docs/catalog.html --open"],
                    &[Effect::WriteFilesystem, Effect::SpawnProcess],
                    Provenance::Builtin,
                ),
                command(
                    "quirl run",
                    "quirl run <file|-> [--lang lua|quirl] [arguments...]",
                    "Run a Lua or Quirl script under explicit policy",
                    "Selects an embedded script language by explicit flag, shebang, or extension; Lua runs inside the restricted VM and line-oriented .quirl scripts use the native command/data executors.",
                    vec![option(
                        &["--lang"],
                        Some("lua|quirl"),
                        "Select the language explicitly, including for stdin",
                    )],
                    &[
                        "quirl run scripts/deploy.lua -- staging",
                        "quirl run --lang lua -",
                    ],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl check",
                    "quirl check <file|directory> [--format text|json]",
                    "Validate scripts without executing them",
                    "Deterministically discovers Lua and Quirl scripts, checks Lua syntax, annotations, modules, and restricted APIs plus Quirl statement structure, and aggregates structured diagnostics without executing source.",
                    vec![option(&["--format"], Some("text|json"), "Choose diagnostic output")],
                    &["quirl check scripts/deploy.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl fmt",
                    "quirl fmt <file|directory> [--check]",
                    "Format Lua scripts deterministically",
                    "Deterministically discovers scripts, applies Quirl's idempotent literal-safe Lua formatting contract, reports all CI drift, and leaves .quirl source unchanged.",
                    vec![option(&["--check"], None, "Report drift without writing")],
                    &["quirl fmt examples/config.lua --check"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl lint",
                    "quirl lint <file|directory> [--format text|json]",
                    "Lint scripts without executing them",
                    "Aggregates annotation and capability diagnostics for deterministically discovered scripts and rejects ambient APIs that bypass Quirl capabilities.",
                    vec![option(&["--format"], Some("text|json"), "Choose diagnostic output")],
                    &["quirl lint examples/plugin.lua --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl test",
                    "quirl test [file|directory]",
                    "Run a Lua test module under resource limits",
                    "Discovers conventional Lua test modules deterministically and runs every returned `test_*` function in an isolated restricted runtime.",
                    vec![],
                    &["quirl test", "quirl test examples/lua_tests.lua"],
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
                    "quirl config get",
                    "quirl config get <file> <key>",
                    "Read one evaluated configuration value",
                    "Evaluates the complete restricted Lua configuration, validates it through Rust schemas, and prints one recognized typed field.",
                    vec![],
                    &["quirl config get ~/.config/quirl/config.lua editor.keymap"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config set",
                    "quirl config set <file> <key> <value>",
                    "Safely patch one literal configuration value",
                    "Changes only a recognized literal field in `quirl.config`, validates the complete candidate before an atomic replacement, and retains the previous source as `.bak`.",
                    vec![],
                    &["quirl config set ~/.config/quirl/config.lua picker.preview false"],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl config tui",
                    "quirl config tui <file>",
                    "Inspect schema-backed configuration in the terminal",
                    "Shows current editor and picker values, allowed values, and textual editing guidance in an accessible line-oriented view.",
                    vec![],
                    &["quirl config tui ~/.config/quirl/config.lua"],
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
                    "quirl agent catalog",
                    "quirl agent catalog [--format text|json]",
                    "Export installed commands and Lua host capabilities",
                    "Emits a versioned deny-unknown schema with deterministic catalog and HOST_API content hashes, provenance, installed capabilities, and their versions.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl agent catalog --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl agent context",
                    "quirl agent context <query...> [--token-budget count] [--format markdown|json]",
                    "Build deterministic token-budgeted agent context",
                    "Ranks only installed command and HOST_API facts, selects the smallest relevant subtree within a documented deterministic token estimate, and records truncation and source hashes.",
                    vec![
                        option(
                            &["--token-budget"],
                            Some("count"),
                            "Bound the canonical context payload",
                        ),
                        option(
                            &["--format"],
                            Some("markdown|json"),
                            "Choose agent Markdown or stable machine JSON",
                        ),
                    ],
                    &["quirl agent context 'deploy the billing service' --format markdown --token-budget 6000"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl agent manifest",
                    "quirl agent manifest [--format text|json]",
                    "Export installed tools, versions, schemas, and validators",
                    "Lists only tools and capabilities installed in this Quirl composition, with schema/content hashes and validation commands grounded in the semantic catalog and generated Lua HOST_API.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose accessible text or stable machine JSON",
                    )],
                    &["quirl agent manifest --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl agent validate",
                    "quirl agent validate <file> --kind catalog|context|manifest [--format text|json]",
                    "Validate a versioned agent contract without execution",
                    "Rejects unknown fields, unsupported schema versions, tampered content hashes, nondeterministic ordering, and context payloads that exceed their declared token budget.",
                    vec![
                        option(
                            &["--kind"],
                            Some("catalog|context|manifest"),
                            "Select the deny-unknown document schema",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible diagnostics or stable JSON",
                        ),
                    ],
                    &["quirl agent validate agent-context.json --kind context --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl package manifest",
                    "quirl package manifest [--manifest path] [--format text|json]",
                    "Parse a versioned project package manifest",
                    "Reads a deny-unknown plugin.toml schema and shows normalized package identity, Quirl compatibility, requested capabilities, and contributions without loading its Lua entry.",
                    vec![
                        option(
                            &["--manifest"],
                            Some("path"),
                            "Read a manifest other than ./plugin.toml",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable machine JSON",
                        ),
                    ],
                    &["quirl package manifest --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl package build",
                    "quirl package build [--manifest path] [--format text|json]",
                    "Validate and build a deterministic package contract",
                    "Checks the entry path, Quirl version range, installed capabilities, and the public-command quality gate for summaries, argument docs and types, examples, effects, and error codes; it returns content hashes without executing Lua.",
                    vec![
                        option(
                            &["--manifest"],
                            Some("path"),
                            "Build a manifest other than ./plugin.toml",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible diagnostics or stable JSON",
                        ),
                    ],
                    &["quirl package build --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl package publish",
                    "quirl package publish --dry-run [--manifest path] [--format text|json]",
                    "Preview a deterministic network-free package publication",
                    "Runs the complete package build quality gate and emits the files, build hash, and requested permissions that would be published. Phase 2 performs no network publication.",
                    vec![
                        option(
                            &["--dry-run"],
                            None,
                            "Require a network-free publication plan",
                        ),
                        option(
                            &["--manifest"],
                            Some("path"),
                            "Read a manifest other than ./plugin.toml",
                        ),
                        option(
                            &["--format"],
                            Some("text|json"),
                            "Choose accessible text or stable JSON",
                        ),
                    ],
                    &["quirl package publish --dry-run --format json"],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl lsp",
                    "quirl lsp",
                    "Serve generated Lua and .quirl editor intelligence",
                    "Speaks a deterministic LSP subset over stdio, using the generated Lua HOST_API and semantic command catalog for diagnostics, completion, hover, signatures, and module docs without evaluating documents.",
                    vec![],
                    &["quirl lsp"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl eval",
                    "quirl eval <lua-expression>",
                    "Evaluate Lua and print the returned value",
                    "Runs one expression in the same restricted, budgeted Lua runtime used by scripts.",
                    vec![],
                    &["quirl eval 'return 20 + 22'"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl complete",
                    "quirl complete <input> [--format text|json]",
                    "Query the semantic completion engine",
                    "Returns the same attributed completion items used by the interactive editor.",
                    vec![option(
                        &["--format"],
                        Some("text|json"),
                        "Choose stable text or JSON output",
                    )],
                    &["quirl complete 'git commit --am' --format json"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl exec",
                    "quirl exec <command...>",
                    "Execute Quirl's native command graph",
                    "Runs quoted commands, byte pipes, redirects, boolean lists, and background jobs without a compatibility-shell round trip.",
                    vec![],
                    &["quirl exec ls '|' grep Cargo", "quirl exec sleep 1 '&'"],
                    &[Effect::SpawnProcess],
                    Provenance::Builtin,
                ),
                command(
                    "quirl pick",
                    "quirl pick [--source stdin|history|files|actions] [--query text] [--multi]",
                    "Select typed values with Quirl's shared fuzzy engine",
                    "The same deterministic exact/fuzzy/inverse query model ranks history, files, actions, jobs, completions, and data while returning the original value.",
                    vec![
                        option(
                            &["--source"],
                            Some("stdin|history|files|actions"),
                            "Choose the typed provider",
                        ),
                        option(&["--query"], Some("text"), "Set the initial fuzzy query"),
                        option(&["--multi"], None, "Return multiple selected values"),
                        option(&["--limit"], Some("count"), "Bound multi-selection output"),
                        option(&["--root"], Some("path"), "Set the file provider root"),
                        option(&["--format"], Some("text|json"), "Choose stable output"),
                    ],
                    &[
                        "quirl pick --source history --query cargo",
                        "quirl pick --source files --query src",
                        "quirl pick --source actions --query index",
                    ],
                    &[Effect::ReadFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "jobs",
                    "jobs",
                    "List structured background job state",
                    "Shows Quirl job ids, running/stopped/done state, and the original command.",
                    vec![],
                    &["jobs"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "fg",
                    "fg [%job]",
                    "Resume a job in the foreground",
                    "Transfers terminal ownership to the selected process group and waits until it exits or stops again.",
                    vec![],
                    &["fg", "fg %2"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "bg",
                    "bg [%job]",
                    "Resume a stopped job in the background",
                    "Sends SIGCONT to the selected process group without transferring terminal ownership.",
                    vec![],
                    &["bg", "bg %2"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "export",
                    "export NAME=value...",
                    "Set environment variables for later commands",
                    "The Preview grammar accepts explicit NAME=value assignments without shell expansion.",
                    vec![],
                    &["export RUST_LOG=debug"],
                    &[],
                    Provenance::Builtin,
                ),
                command(
                    "quirl index build",
                    "quirl index build [--fish path] [--bash path] [--zsh path] [--help path] [--man path] [--output path]",
                    "Build the attributed completion index",
                    "Imports declarative Fish, Bash, and Zsh completions plus bounded supplied help/man text without sourcing or executing providers, commands, or man, then atomically writes a versioned catalog.",
                    vec![
                        option(&["--fish"], Some("path"), "Import a Fish completion file or directory"),
                        option(&["--bash"], Some("path"), "Import a Bash completion file or directory"),
                        option(&["--zsh"], Some("path"), "Import a Zsh completion file or directory"),
                        option(&["--help"], Some("path"), "Parse supplied command-help text without executing its command"),
                        option(&["--man"], Some("path"), "Parse supplied rendered/raw man text without invoking man"),
                        option(&["--output"], Some("path"), "Write a specific index instead of the default cache"),
                        option(&["--format"], Some("text|json"), "Choose the build report format"),
                    ],
                    &[
                        "quirl index build",
                        "quirl index build --zsh completions/_tool",
                        "quirl index build --help captured/tool-help.txt --man docs/tool.man",
                    ],
                    &[Effect::ReadFilesystem, Effect::WriteFilesystem],
                    Provenance::Builtin,
                ),
                command(
                    "quirl index explain",
                    "quirl index explain <command> [--index path] [--format text|json]",
                    "Explain where indexed command facts came from",
                    "Shows source kind, confidence, origin, and fingerprint for command metadata and each retained option.",
                    vec![
                        option(&["--index"], Some("path"), "Read a specific catalog index"),
                        option(&["--format"], Some("text|json"), "Choose the explanation format"),
                    ],
                    &["quirl index explain git", "quirl index explain cargo --format json"],
                    &[Effect::ReadFilesystem],
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

    /// Merge imported commands without discarding the provenance of individual
    /// options. Existing higher-confidence facts win deterministic ties.
    pub fn merge(&mut self, imported: impl IntoIterator<Item = CommandSpec>) {
        for mut incoming in imported {
            if let Some(existing) = self
                .commands
                .iter_mut()
                .find(|command| command.path == incoming.path)
            {
                if incoming.provenance.confidence > existing.provenance.confidence {
                    existing.signature = incoming.signature;
                    existing.summary = incoming.summary;
                    existing.details = incoming.details;
                    existing.provenance = incoming.provenance;
                }
                for option in incoming.options.drain(..) {
                    merge_option(&mut existing.options, option);
                }
                existing.options.sort_by(|left, right| {
                    left.names
                        .first()
                        .cmp(&right.names.first())
                        .then_with(|| left.names.cmp(&right.names))
                });
            } else {
                incoming
                    .options
                    .sort_by(|left, right| left.names.cmp(&right.names));
                self.commands.push(incoming);
            }
        }
        self.commands
            .sort_by(|left, right| left.path.cmp(&right.path));
    }

    pub fn merge_report(&mut self, report: ImportReport) -> Vec<ImportDiagnostic> {
        self.merge(report.commands);
        report.diagnostics
    }

    /// Explain the source of every command-level and option-level fact currently
    /// retained in the catalog.
    pub fn explain(&self, path: &str) -> Option<CatalogExplanation> {
        let command = self.commands.iter().find(|command| command.path == path)?;
        let mut facts = vec![
            FactExplanation {
                fact: "command_path".to_owned(),
                value: command.path.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "signature".to_owned(),
                value: command.signature.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "summary".to_owned(),
                value: command.summary.clone(),
                provenance: command.provenance.clone(),
            },
            FactExplanation {
                fact: "details".to_owned(),
                value: command.details.clone(),
                provenance: command.provenance.clone(),
            },
        ];
        for example in &command.examples {
            facts.push(FactExplanation {
                fact: "example".to_owned(),
                value: example.clone(),
                provenance: command.provenance.clone(),
            });
        }
        for effect in &command.effects {
            facts.push(FactExplanation {
                fact: "effect".to_owned(),
                value: format!("{effect:?}"),
                provenance: command.provenance.clone(),
            });
        }
        for option in &command.options {
            facts.push(FactExplanation {
                fact: "option_names".to_owned(),
                value: option.names.join(", "),
                provenance: option.provenance.clone(),
            });
            if let Some(value) = &option.value {
                facts.push(FactExplanation {
                    fact: "option_value".to_owned(),
                    value: value.clone(),
                    provenance: option.provenance.clone(),
                });
            }
            facts.push(FactExplanation {
                fact: "option_summary".to_owned(),
                value: option.summary.clone(),
                provenance: option.provenance.clone(),
            });
        }
        Some(CatalogExplanation {
            command: command.path.clone(),
            facts,
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
        provenance: ProvenanceInfo::builtin(Provenance::Builtin),
    }
}

fn merge_option(options: &mut Vec<OptionSpec>, incoming: OptionSpec) {
    let duplicate = options.iter_mut().find(|existing| {
        existing
            .names
            .iter()
            .any(|name| incoming.names.iter().any(|candidate| candidate == name))
    });
    if let Some(existing) = duplicate {
        for name in incoming.names {
            if !existing.names.contains(&name) {
                existing.names.push(name);
            }
        }
        existing.names.sort();
        if incoming.provenance.confidence > existing.provenance.confidence {
            existing.value = incoming.value;
            existing.summary = incoming.summary;
            existing.provenance = incoming.provenance;
        }
    } else {
        options.push(incoming);
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
    let provenance = ProvenanceInfo::builtin(provenance);
    let options = options
        .into_iter()
        .map(|mut option| {
            option.provenance = provenance.clone();
            option
        })
        .collect();
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
        assert!(json.contains("confidence"));
        assert!(json.contains("git commit"));
    }

    #[test]
    fn language_service_is_discoverable_from_the_catalog() {
        let catalog = Catalog::builtin();
        let command = catalog.find("quirl lsp").unwrap();
        assert!(command.details.contains("without evaluating documents"));
        assert_eq!(command.provenance.source, Provenance::Builtin);
    }

    #[test]
    fn imported_options_merge_without_overwriting_exact_builtin_facts() {
        let mut catalog = Catalog::builtin();
        let diagnostics = catalog.merge_report(import_fish(
            "complete -c ls -l color -d 'Colorize output'",
            "ls.fish",
        ));
        assert!(diagnostics.is_empty());
        let command = catalog.find("ls").unwrap();
        assert_eq!(command.provenance.source, Provenance::Builtin);
        let color = command
            .options
            .iter()
            .find(|option| option.names.contains(&"--color".to_owned()))
            .unwrap();
        assert_eq!(color.provenance.source, Provenance::Fish);
    }

    #[test]
    fn explain_attributes_each_retained_fact() {
        let mut catalog = Catalog::builtin();
        catalog.merge_report(import_bash(
            "complete -W '--frozen --locked' cargo",
            "cargo.bash",
        ));
        let explanation = catalog.explain("cargo").unwrap();
        assert!(explanation
            .facts
            .iter()
            .any(|fact| fact.value == "--frozen" && fact.provenance.source == Provenance::Bash));
        assert!(explanation.facts.iter().all(|fact| !fact.value.is_empty()));
    }

    #[test]
    fn agent_and_package_surfaces_have_complete_catalog_metadata() {
        let catalog = Catalog::builtin();
        for path in [
            "quirl agent catalog",
            "quirl agent context",
            "quirl agent manifest",
            "quirl agent validate",
            "quirl package manifest",
            "quirl package build",
            "quirl package publish",
        ] {
            let command = catalog
                .commands
                .iter()
                .find(|command| command.path == path)
                .unwrap();
            assert!(!command.signature.is_empty(), "{path}");
            assert!(!command.summary.is_empty(), "{path}");
            assert!(!command.details.is_empty(), "{path}");
            assert!(!command.examples.is_empty(), "{path}");
            assert_eq!(command.provenance.confidence, Confidence::Exact);
        }
    }
}
