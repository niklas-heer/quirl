//! Live Zsh argument completion for explicit Tab requests.
//!
//! Quirl's catalog knows command structure, but many arguments are dynamic:
//! Git branches, Make targets, SSH hosts, package names. Zsh already ships
//! completion functions for them, so an explicit Tab asks the installed Zsh
//! through the bounded [`LocalCompletionProcess`] boundary. That boundary
//! starts no user startup files, clears the environment, contains the process
//! group, and enforces output, record, and wall-time limits.
//!
//! Only explicit requests reach this module; per-keystroke automatic
//! completion never starts a shell. A missing Zsh, a command without a Zsh
//! completion function, or any provider failure yields no suggestions, so the
//! catalog and filesystem sources still answer on their own.

use quirl_process::local_completion::{
    LocalCompletionLimits, LocalCompletionOutcome, LocalCompletionProcess, LocalCompletionProvider,
    LocalCompletionRequest,
};
use quirl_ui::{ExtensionSuggestion, SuggestionOrigin};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

/// Wall-time budget for one live Zsh request, including `compinit`.
const ZSH_COMPLETION_DEADLINE: Duration = Duration::from_millis(1_500);
/// Maximum candidates retained from one live request.
const ZSH_CANDIDATES_MAX: usize = 1_024;
/// Maximum completion function directories passed to Zsh.
const ZSH_ROOTS_MAX: usize = 64;
/// Maximum directory entries inspected while locating Zsh's own functions.
const ZSH_ROOT_SCAN_ENTRIES_MAX: usize = 256;
/// Maximum words sent to the provider, including the command and partial word.
const ZSH_WORDS_MAX: usize = 128;
/// Environment switch: `off`, `0`, `false`, or `no` disables live Zsh
/// completion for the session.
const ZSH_COMPLETION_VARIABLE: &str = "QUIRL_ZSH_COMPLETION";
/// Commands implemented by Quirl itself. Zsh's functions would describe a
/// different program; `cd` directories come from Quirl's filesystem source.
const QUIRL_OWNED_COMMANDS: &[&str] = &["cd", "quirl", "mode", "help", "exit"];

/// Lazily configured live Zsh completion source.
pub(crate) struct ZshArgumentCompleter {
    process: Option<LocalCompletionProcess>,
    setup: ZshAvailability,
}

/// Session-lifetime result of locating Zsh, resolved on the first request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ZshAvailability {
    Unresolved,
    Missing,
    Ready(ZshSetup),
}

/// Executable and function roots used for every request in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ZshSetup {
    shell_path: PathBuf,
    roots: Vec<PathBuf>,
}

/// One shell word of the command segment before the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    start: usize,
    decoded: String,
    quoted: bool,
}

impl ZshArgumentCompleter {
    pub(crate) fn new() -> Self {
        Self {
            process: None,
            setup: ZshAvailability::Unresolved,
        }
    }

    /// Complete the argument at `cursor` through the installed Zsh.
    pub(crate) fn complete(&mut self, line: &str, cursor: usize) -> Vec<ExtensionSuggestion> {
        let Some(query) = ArgumentQuery::parse(line, cursor) else {
            return Vec::new();
        };
        if self.setup == ZshAvailability::Unresolved {
            self.setup =
                ZshSetup::discover().map_or(ZshAvailability::Missing, ZshAvailability::Ready);
        }
        let ZshAvailability::Ready(setup) = &self.setup else {
            return Vec::new();
        };
        let process = match &self.process {
            Some(process) => process.clone(),
            None => match LocalCompletionProcess::new(1) {
                Ok(process) => self.process.insert(process).clone(),
                Err(_) => return Vec::new(),
            },
        };
        let request = setup.request(&query);
        let Ok(LocalCompletionOutcome::Completed(result)) = process.complete(request) else {
            return Vec::new();
        };
        query.suggestions(
            result
                .candidates
                .into_iter()
                .map(|candidate| (candidate.value, candidate.description)),
        )
    }
}

impl ZshSetup {
    fn discover() -> Option<Self> {
        if live_zsh_disabled(env::var_os(ZSH_COMPLETION_VARIABLE).as_deref()) {
            return None;
        }
        let shell_path = find_on_path("zsh")?;
        // Zsh's bundled functions come first so they win name collisions,
        // such as Homebrew Git's `_git` wrapper around Bash completion.
        // Site directories still add functions for commands Zsh lacks.
        let mut roots = zsh_function_roots(&shell_path);
        roots.extend(crate::index::default_zsh_roots());
        Some(Self::new(shell_path, roots))
    }

    fn new(shell_path: PathBuf, roots: Vec<PathBuf>) -> Self {
        let mut admitted = Vec::new();
        for root in roots {
            if admitted.len() == ZSH_ROOTS_MAX {
                break;
            }
            if root.is_dir() && !admitted.contains(&root) {
                admitted.push(root);
            }
        }
        Self {
            shell_path,
            roots: admitted,
        }
    }

    fn request(&self, query: &ArgumentQuery) -> LocalCompletionRequest {
        // Only variables completion functions commonly consult are passed;
        // the provider never inherits the full session environment.
        let environment = ["PATH", "HOME", "USER", "LOGNAME"]
            .into_iter()
            .filter_map(|name| Some((name.to_owned(), env::var(name).ok()?)))
            .collect();
        LocalCompletionRequest {
            provider: LocalCompletionProvider::Zsh,
            shell_path: self.shell_path.clone(),
            command_path: vec![query.command.clone()],
            arguments: query.arguments.clone(),
            completion_roots: self.roots.clone(),
            completion_scripts: Vec::new(),
            environment,
            deadline: ZSH_COMPLETION_DEADLINE,
            cancelled: Arc::new(AtomicBool::new(false)),
            limits: LocalCompletionLimits {
                candidate_count_max: ZSH_CANDIDATES_MAX,
                completion_root_count_max: ZSH_ROOTS_MAX,
                argument_count_max: ZSH_WORDS_MAX,
                ..LocalCompletionLimits::default()
            },
        }
    }
}

/// The command and arguments Zsh should complete, with the replaced span.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgumentQuery {
    command: String,
    /// Arguments after the command; the last one is the partial word.
    arguments: Vec<String>,
    replace_start: usize,
    replace_end: usize,
}

impl ArgumentQuery {
    /// Parse the command segment before `cursor`, or `None` when the cursor
    /// is not in an unquoted argument of a completable command.
    fn parse(line: &str, cursor: usize) -> Option<Self> {
        let before = line.get(..cursor)?;
        let (mut words, partial) = segment_words(before)?;
        // Leading `NAME=value` assignments only change the environment.
        let command_index = words
            .iter()
            .position(|word| !is_assignment(&word.decoded))?;
        words.drain(..command_index);
        let command = words.first()?.decoded.clone();
        if QUIRL_OWNED_COMMANDS.contains(&command.as_str()) || command.contains('/') {
            return None;
        }
        let (replace_start, partial_text) = match partial {
            Some(word) if word.quoted => return None,
            Some(word) => (word.start, word.decoded),
            None => (cursor, String::new()),
        };
        let mut arguments = words
            .iter()
            .skip(1)
            .map(|word| word.decoded.clone())
            .collect::<Vec<_>>();
        arguments.push(partial_text);
        if arguments.len() >= ZSH_WORDS_MAX {
            return None;
        }
        Some(Self {
            command,
            arguments,
            replace_start,
            replace_end: cursor,
        })
    }

    fn partial(&self) -> &str {
        self.arguments.last().map_or("", String::as_str)
    }

    fn suggestions(
        &self,
        candidates: impl Iterator<Item = (String, Option<String>)>,
    ) -> Vec<ExtensionSuggestion> {
        let mut suggestions = Vec::new();
        for (candidate, description) in candidates {
            // A bare suffix such as `=` (an assignment with no name) is an
            // artifact of how some functions add matches, not a candidate.
            let only_punctuation = candidate.chars().all(|character| "=:,".contains(character));
            if candidate.is_empty()
                || only_punctuation
                || !candidate.starts_with(self.partial())
                || candidate.chars().any(char::is_control)
            {
                continue;
            }
            // `_path_files` adds a directory's `/` as a removable suffix that
            // the capture does not see; restore it so Zsh and filesystem
            // matches agree and Tab keeps completing inside the directory.
            let candidate = if !candidate.ends_with('/') && names_directory(&candidate) {
                format!("{candidate}/")
            } else {
                candidate
            };
            let value = escape_candidate(&candidate);
            if suggestions
                .iter()
                .any(|existing: &ExtensionSuggestion| existing.value == value)
            {
                continue;
            }
            suggestions.push(ExtensionSuggestion {
                display: candidate,
                value,
                summary: description.unwrap_or_default(),
                detail: format!("Zsh completion for `{}`", self.command),
                replace_start: self.replace_start,
                replace_end: self.replace_end,
                origin: SuggestionOrigin::Zsh,
            });
        }
        suggestions
    }
}

/// Whether `candidate` is an existing directory relative to the session's
/// working directory, with a leading `~/` expanded from `HOME`.
fn names_directory(candidate: &str) -> bool {
    let path = match candidate.strip_prefix("~/") {
        Some(rest) => match env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => return false,
        },
        None => PathBuf::from(candidate),
    };
    fs::metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

/// Shell-escape a candidate while keeping a leading `~/` expandable.
fn escape_candidate(candidate: &str) -> String {
    match candidate.strip_prefix("~/") {
        Some(rest) => format!("~/{}", quirl_ui::escape_shell_word(rest)),
        None => quirl_ui::escape_shell_word(candidate),
    }
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        let mut characters = name.chars();
        characters
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
            && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

/// Split the last command segment of `before` into complete words and the
/// partial word at its end (`None` after trailing whitespace).
///
/// Unquoted `|`, `&`, `;`, and newlines start a new command. Returns `None`
/// for an unfinished quote that spans the cursor into a previous word.
fn segment_words(before: &str) -> Option<(Vec<Word>, Option<Word>)> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut words = Vec::new();
    let mut current: Option<Word> = None;
    let mut quote = Quote::None;
    let mut escaped = false;
    for (index, character) in before.char_indices() {
        if escaped {
            escaped = false;
            if let Some(word) = current.as_mut() {
                word.decoded.push(character);
            }
            continue;
        }
        match (quote, character) {
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::Double, '\\') => escaped = true,
            (Quote::Single | Quote::Double, _) => {
                if let Some(word) = current.as_mut() {
                    word.decoded.push(character);
                }
            }
            (Quote::None, '|' | '&' | ';' | '\n') => {
                words.clear();
                current = None;
            }
            (Quote::None, whitespace) if whitespace.is_whitespace() => {
                if let Some(word) = current.take() {
                    words.push(word);
                }
            }
            (Quote::None, _) => {
                let word = current.get_or_insert_with(|| Word {
                    start: index,
                    decoded: String::new(),
                    quoted: false,
                });
                match character {
                    '\\' => escaped = true,
                    '\'' => {
                        word.quoted = true;
                        quote = Quote::Single;
                    }
                    '"' => {
                        word.quoted = true;
                        quote = Quote::Double;
                    }
                    _ => word.decoded.push(character),
                }
            }
        }
    }
    Some((words, current))
}

fn live_zsh_disabled(value: Option<&std::ffi::OsStr>) -> bool {
    value
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            )
        })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
    }
}

/// Locate the completion functions shipped with the Zsh at `shell_path`.
///
/// Zsh installs them below `<prefix>/share/zsh`, either flat
/// (`functions/`, macOS `5.9/functions/`) or split into category
/// subdirectories (Debian's `functions/Completion/Unix`). The scan is
/// bounded in entries and depth and follows only the installation prefix.
fn zsh_function_roots(shell_path: &Path) -> Vec<PathBuf> {
    let resolved = fs::canonicalize(shell_path).unwrap_or_else(|_| shell_path.to_path_buf());
    let mut prefixes = Vec::new();
    if let Some(prefix) = resolved.parent().and_then(Path::parent) {
        prefixes.push(prefix.to_path_buf());
        // `/bin/zsh` keeps its data under `/usr/share/zsh`.
        if prefix == Path::new("/") {
            prefixes.push(PathBuf::from("/usr"));
        }
    }
    let mut roots = Vec::new();
    let mut scanned = 0_usize;
    for prefix in prefixes {
        let share = prefix.join("share/zsh");
        // Debian and Ubuntu also load packaged completions from these.
        for vendor in ["vendor-functions", "vendor-completions"] {
            let directory = share.join(vendor);
            if directory.is_dir() {
                roots.push(directory);
            }
        }
        let mut function_directories = vec![share.join("functions")];
        for entry in bounded_directories(&share, &mut scanned) {
            function_directories.push(entry.join("functions"));
        }
        for directory in function_directories {
            if !directory.is_dir() {
                continue;
            }
            roots.push(directory.clone());
            for child in bounded_directories(&directory, &mut scanned) {
                roots.push(child.clone());
                roots.extend(bounded_directories(&child, &mut scanned));
            }
        }
    }
    roots.truncate(ZSH_ROOTS_MAX);
    roots
}

fn bounded_directories(directory: &Path, scanned: &mut usize) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut directories = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        if *scanned >= ZSH_ROOT_SCAN_ENTRIES_MAX {
            break;
        }
        *scanned = scanned.saturating_add(1);
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            directories.push(entry.path());
        }
    }
    directories.sort();
    directories
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(label: &str) -> PathBuf {
        let directory = env::temp_dir().join(format!(
            "quirl-shell-completion-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn argument_query_splits_the_current_command_segment() {
        let query = ArgumentQuery::parse("ls | git checkout fe", 20).unwrap();
        assert_eq!(query.command, "git");
        assert_eq!(query.arguments, ["checkout", "fe"]);
        assert_eq!(query.replace_start, 18);

        let query = ArgumentQuery::parse("FOO=1 make ", 11).unwrap();
        assert_eq!(query.command, "make");
        assert_eq!(query.arguments, [""]);
        assert_eq!(query.replace_start, 11);

        let query = ArgumentQuery::parse(r"cat my\ fi", 10).unwrap();
        assert_eq!(query.arguments, ["my fi"]);
        assert_eq!(query.replace_start, 4);
    }

    #[test]
    fn command_position_quoted_words_and_quirl_commands_are_not_sent_to_zsh() {
        assert!(ArgumentQuery::parse("gi", 2).is_none());
        assert!(ArgumentQuery::parse("ls && gi", 8).is_none());
        assert!(ArgumentQuery::parse("cat \"my", 7).is_none());
        assert!(ArgumentQuery::parse("quirl pl", 8).is_none());
        assert!(ArgumentQuery::parse("./run.sh ar", 11).is_none());
    }

    #[test]
    fn suggestions_escape_values_and_keep_only_prefix_matches() {
        let query = ArgumentQuery::parse("cat my", 6).unwrap();
        let suggestions = query.suggestions(
            [
                ("my file.txt".to_owned(), None),
                ("other".to_owned(), None),
                ("my file.txt".to_owned(), Some("duplicate".to_owned())),
                ("my\u{1b}bad".to_owned(), None),
            ]
            .into_iter(),
        );
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].value, r"my\ file.txt");
        assert_eq!(suggestions[0].display, "my file.txt");
        assert_eq!(suggestions[0].replace_start, 4);
        assert_eq!(suggestions[0].origin, SuggestionOrigin::Zsh);
        assert_eq!(escape_candidate("~/a b"), r"~/a\ b");

        let directory = temporary_directory("directory-suffix");
        let query = ArgumentQuery::parse("make ", 5).unwrap();
        let name = directory.to_string_lossy().into_owned();
        let suggestions = query.suggestions(std::iter::once((name.clone(), None)));
        assert_eq!(suggestions[0].display, format!("{name}/"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn live_zsh_completion_can_be_switched_off() {
        use std::ffi::OsStr;
        for value in ["off", "0", "FALSE", " no "] {
            assert!(live_zsh_disabled(Some(OsStr::new(value))), "{value}");
        }
        for value in [None, Some(OsStr::new("on")), Some(OsStr::new(""))] {
            assert!(!live_zsh_disabled(value));
        }
    }

    #[cfg(unix)]
    #[test]
    fn live_zsh_completion_uses_installed_completion_functions() {
        // Goal: prove the whole path through a real Zsh, using a private
        // completion function so the result does not depend on the host.
        let Some(shell_path) = find_on_path("zsh") else {
            return;
        };
        let root = temporary_directory("functions");
        fs::write(
            root.join("_quirltest"),
            "#compdef quirltest\n_arguments '1:flavour:((alpha\\:first beta\\:second bravo\\:third))' '2:file:_files'\n",
        )
        .unwrap();
        fs::write(root.join("my file.txt"), b"").unwrap();
        let mut roots = vec![root.clone()];
        roots.extend(zsh_function_roots(&shell_path));
        let mut completer = ZshArgumentCompleter {
            process: None,
            setup: ZshAvailability::Ready(ZshSetup::new(shell_path, roots)),
        };

        let suggestions = completer.complete("quirltest b", 11);
        let values = suggestions
            .iter()
            .map(|suggestion| suggestion.value.as_str())
            .collect::<Vec<_>>();

        assert_eq!(values, ["beta", "bravo"]);
        assert_eq!(suggestions[0].summary, "second");
        assert_eq!(suggestions[0].replace_start, 10);
        assert!(completer.complete("unknown-command b", 17).is_empty());

        // `_path_files` adds pre-quoted matches; they are escaped only once.
        let line = format!("quirltest alpha {}/my", root.display());
        let files = completer.complete(&line, line.len());
        let expected = quirl_ui::escape_shell_word(&format!("{}/my file.txt", root.display()));
        assert_eq!(
            files
                .iter()
                .map(|suggestion| suggestion.value.as_str())
                .collect::<Vec<_>>(),
            [expected.as_str()]
        );
        fs::remove_dir_all(root).unwrap();
    }
}
