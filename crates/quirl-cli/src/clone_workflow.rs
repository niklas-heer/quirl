//! Interactive clone admission and bounded, explicit project-layout preferences.
//!
//! Only one literal foreground command is eligible; no expansion is evaluated
//! here. The original command remains the default until an explicit UI choice.
//! Persistence may race, stall, contain malformed data, or fail after installation.
//! Reads are nonblocking and limited to 4 KiB; updates use compare-and-replace,
//! and first installation uses a create-new hard link so concurrent state cannot
//! be overwritten. Temporary failures preserve recoverable files, never user data.
//! Parent directories are a cooperative user-owned namespace, as for other local
//! Quirl state. Preference errors do not grant managed-clone authority.

use crate::{
    Cli, Command, SessionEditor,
    project_clone::{self, ClonePlan, ProjectsCommand},
};
use clap::{Parser, ValueEnum};
use quirl_core::{AtomicReplaceOptions, ErrorCode, ShellError, replace_file_atomically};
use quirl_process::NativeExecutor;
use quirl_syntax::{Quoting, parse_command_list};
use quirl_ui::{ProjectCloneChoice, QuirlPrompt};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const POLICY_BYTES_MAX: usize = 4096;
const PATH_BYTES_MAX: usize = 4096;
const PATH_COMPONENTS_MAX: usize = 64;

/// Saved authority for uncomplicated interactive `git clone URL` commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClonePolicy {
    /// Offer the managed destination once; Enter keeps the original command.
    Ask,
    /// Explicit opt-in to managed locations for eligible interactive commands.
    Managed,
    /// Keep all ordinary Git commands unchanged and suppress suggestions.
    Off,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preference {
    version: u32,
    policy: ClonePolicy,
}

/// Decision made before beginning the normal command execution transaction.
pub(crate) enum PreparedClone {
    /// Execute the user's exact command through the existing native path.
    Original,
    /// Execute the checked managed clone command after reserving its directory.
    Managed { plan: ClonePlan, source: String },
    /// An explicit open action can reuse a matching checkout without Git mutation.
    Existing(ClonePlan),
    /// The user cancelled before any clone was started.
    Cancelled,
}

enum Intent {
    Automatic(String),
    Explicit {
        repository: String,
        root: Option<PathBuf>,
    },
}

/// Recognize and authorize one clone without expanding or executing source.
pub(crate) fn prepare(
    source: &str,
    executor: &mut NativeExecutor,
    editor: &mut SessionEditor,
    prompt: &QuirlPrompt,
) -> Result<PreparedClone, ShellError> {
    let Some(intent) = recognize(source) else {
        return Ok(PreparedClone::Original);
    };
    // The editor has released raw input. Keep cancellation owned across metadata
    // probes, the chooser, and reservation so an interrupted suggestion cannot
    // fall back into executing the original clone.
    let signals = crate::InteractiveSignalCancellation::install()?;
    let result = prepare_intent(intent, executor, editor, prompt, &signals.cancellation);
    if signals.cancellation.is_cancelled()
        || result.as_ref().is_err_and(project_clone::was_cancelled)
    {
        Ok(PreparedClone::Cancelled)
    } else {
        result
    }
}

fn prepare_intent(
    intent: Intent,
    executor: &mut NativeExecutor,
    editor: &mut SessionEditor,
    prompt: &QuirlPrompt,
    cancellation: &quirl_core::ExecutionCancellation,
) -> Result<PreparedClone, ShellError> {
    let plan = match intent {
        Intent::Explicit { repository, root } => {
            project_clone::plan(&repository, root.as_deref(), executor)?
        }
        Intent::Automatic(repository) => {
            let SessionEditor::Rich(surface) = editor else {
                return Ok(PreparedClone::Original);
            };
            let path = preference_path(executor)?;
            let policy = load_policy(&path)?;
            if policy == ClonePolicy::Off {
                return Ok(PreparedClone::Original);
            }
            let plan = match project_clone::plan(&repository, None, executor) {
                Ok(plan) => plan,
                Err(error) if project_clone::was_cancelled(&error) => {
                    return Ok(PreparedClone::Cancelled);
                }
                // Unsupported repositories/configuration never hijack an ordinary
                // command when the user has not enabled managed cloning.
                Err(_) if policy == ClonePolicy::Ask => return Ok(PreparedClone::Original),
                Err(error) => return Err(error),
            };
            if policy == ClonePolicy::Ask {
                let destination = plan
                    .destination
                    .to_str()
                    .ok_or_else(|| invalid("managed destination is not UTF-8"))?;
                let choice = surface.choose_clone_location(destination, prompt, cancellation)?;
                if cancellation.is_cancelled() {
                    return Ok(PreparedClone::Cancelled);
                }
                match choice {
                    ProjectCloneChoice::Cancel => return Ok(PreparedClone::Cancelled),
                    ProjectCloneChoice::Original | ProjectCloneChoice::NeverSuggest => {
                        save_policy(&path, ClonePolicy::Off)?;
                        return Ok(PreparedClone::Original);
                    }
                    ProjectCloneChoice::ManagedOnce => save_policy(&path, ClonePolicy::Off)?,
                    ProjectCloneChoice::ManagedAlways => save_policy(&path, ClonePolicy::Managed)?,
                }
            }
            plan
        }
    };
    if cancellation.is_cancelled() {
        return Ok(PreparedClone::Cancelled);
    }
    if plan.existing {
        project_clone::reserve(&plan, executor)?;
        Ok(PreparedClone::Existing(plan))
    } else {
        let source = project_clone::clone_command(&plan)?;
        project_clone::reserve(&plan, executor)?;
        Ok(PreparedClone::Managed { plan, source })
    }
}

fn recognize(source: &str) -> Option<Intent> {
    let graph = parse_command_list(source).ok()?;
    let ([pipeline], []) = (graph.pipelines.as_slice(), graph.connectors.as_slice()) else {
        return None;
    };
    let [command] = pipeline.commands.as_slice() else {
        return None;
    };
    if pipeline.background || !command.redirects.is_empty() || !literal_words(&command.word_ir) {
        return None;
    }
    // Explicit executable paths and native-force syntax are deliberate escape
    // hatches. Never replace a selected Git/Quirl executable with the PATH one.
    let name = command.words.first()?.as_str();
    if name == "git" {
        let [_, operation, repository] = command.words.as_slice() else {
            return None;
        };
        return (operation == "clone" && !repository.starts_with('-'))
            .then(|| Intent::Automatic(repository.clone()));
    }
    if name != "quirl" {
        return None;
    }
    let cli = Cli::try_parse_from(&command.words).ok()?;
    if cli.build_info {
        return None;
    }
    match cli.command {
        Some(Command::Projects {
            command: ProjectsCommand::Clone { repository, root },
        }) => Some(Intent::Explicit { repository, root }),
        _ => None,
    }
}

fn literal_words(words: &[quirl_syntax::Word]) -> bool {
    !words.is_empty()
        && words.iter().all(|word| {
            word.parts.iter().all(|part| match part.quoting {
                Quoting::Single | Quoting::Escaped => true,
                Quoting::Double => !part.text.contains(['$', '`']),
                Quoting::Unquoted => !part.text.contains(['$', '`', '*', '?', '[', '{', '~']),
            })
        })
}

fn preference_path(executor: &NativeExecutor) -> Result<PathBuf, ShellError> {
    let environment = executor.environment_snapshot()?;
    let value = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
            .filter(|value| !value.is_empty())
    };
    let base = if let Some(state) = value("XDG_STATE_HOME") {
        PathBuf::from(state)
    } else if let Some(home) = value("HOME") {
        PathBuf::from(home).join(".local/state")
    } else {
        return Err(invalid(
            "HOME or XDG_STATE_HOME is required for the clone preference",
        ));
    };
    let path = base.join("quirl/clone-policy.json");
    validate_path(&path)?;
    Ok(path)
}

fn validate_path(path: &Path) -> Result<(), ShellError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(invalid(
            "clone preference path must be absolute without parent traversal",
        ));
    }
    let bytes = path.as_os_str().len();
    let components = path.components().count();
    if bytes > PATH_BYTES_MAX || components > PATH_COMPONENTS_MAX {
        return Err(limit("clone preference path", PATH_BYTES_MAX, bytes)
            .with_context(format!("components: {components}/{PATH_COMPONENTS_MAX}")));
    }
    Ok(())
}

fn read_preference(path: &Path) -> Result<Option<Vec<u8>>, ShellError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(invalid("clone preference is not a regular file"));
    }
    if metadata.len() > u64::try_from(POLICY_BYTES_MAX).unwrap_or(u64::MAX) {
        return Err(limit(
            "clone preference bytes",
            POLICY_BYTES_MAX,
            usize::try_from(metadata.len()).unwrap_or(usize::MAX),
        ));
    }
    let mut bytes = Vec::new();
    file.take(u64::try_from(POLICY_BYTES_MAX.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > POLICY_BYTES_MAX {
        return Err(limit(
            "clone preference bytes",
            POLICY_BYTES_MAX,
            bytes.len(),
        ));
    }
    Ok(Some(bytes))
}

fn load_policy(path: &Path) -> Result<ClonePolicy, ShellError> {
    let Some(bytes) = read_preference(path)? else {
        return Ok(ClonePolicy::Ask);
    };
    let preference: Preference =
        serde_json::from_slice(&bytes).map_err(|_| invalid("clone preference is malformed"))?;
    if preference.version != 1 {
        return Err(invalid("clone preference has an unsupported version"));
    }
    Ok(preference.policy)
}

fn save_policy(path: &Path, policy: ClonePolicy) -> Result<(), ShellError> {
    validate_path(path)?;
    let bytes = serde_json::to_vec(&Preference { version: 1, policy }).map_err(io_error)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("clone preference has no parent"))?;
    // Path admission bounds recursion in the standard library to 64 components.
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(parent).map_err(io_error)?;
    if let Some(expected) = read_preference(path)? {
        return replace_file_atomically(
            path,
            &expected,
            &bytes,
            AtomicReplaceOptions {
                bytes_max: POLICY_BYTES_MAX,
            },
        );
    }
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(io_error)?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let temporary = parent.join(format!(".clone-policy-{suffix}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(io_error)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(io_error)?;
    // Unlike rename, hard-link installation cannot overwrite a concurrently
    // created policy. Failed candidates remain private and recoverable.
    fs::hard_link(&temporary, path).map_err(io_error)?;
    fs::remove_file(&temporary).map_err(io_error)?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(io_error)?;
    Ok(())
}

/// Show or explicitly replace the saved policy; never start discovery or clone.
pub(crate) fn run_policy(mode: Option<ClonePolicy>) -> Result<i32, ShellError> {
    let path = preference_path(&NativeExecutor::default())?;
    let policy = if let Some(mode) = mode {
        save_policy(&path, mode)?;
        mode
    } else {
        load_policy(&path)?
    };
    println!(
        "{}",
        match policy {
            ClonePolicy::Ask => "ask",
            ClonePolicy::Managed => "managed",
            ClonePolicy::Off => "off",
        }
    );
    Ok(0)
}

fn invalid(message: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help("Use `quirl projects policy ask` to reset the preference; inspect clone-policy.json if resetting fails")
}
fn io_error(error: impl std::fmt::Display) -> ShellError {
    ShellError::new(ErrorCode::Io, "could not access the clone preference").with_context(error.to_string())
        .with_help("Check permissions in the Quirl state directory and retry; existing clone data was not removed")
}
fn limit(subject: &str, configured: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{subject} exceeded its limit"),
    )
    .with_context(format!("limit: {configured}; observed: {observed}"))
    .with_help("Use a smaller, valid clone preference and an absolute state path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_literal_clone_without_options_or_destination_is_eligible() {
        for source in [
            "git clone https://github.com/a/b",
            "git clone 'git@github.com:a/b.git'",
            "git clone \"https://github.com/a/b\"",
        ] {
            assert!(
                matches!(recognize(source), Some(Intent::Automatic(_))),
                "{source}"
            );
        }
        for source in [
            "git clone url scratch",
            "git clone --depth 1 url",
            "git clone -- url",
            "git -C /tmp clone url",
            "git clone $URL",
            "git clone $(touch nope)",
            "git clone `touch nope`",
            "git clone ~/local",
            "git clone url &",
            "git clone url > output",
            "git clone url; pwd",
            "printf git clone url",
            "git clone url | cat",
            "/usr/bin/git clone https://github.com/a/b",
            "^git clone https://github.com/a/b",
            "/other/quirl projects clone https://github.com/a/b",
            "quirl --build-info projects clone https://github.com/a/b",
        ] {
            assert!(recognize(source).is_none(), "{source}");
        }
        assert!(matches!(
            recognize("quirl projects clone 'https://github.com/a/b' --root '/tmp/my projects'"),
            Some(Intent::Explicit { .. })
        ));
    }

    #[test]
    fn preference_versions_fields_and_limits_fail_closed_and_can_be_reset() {
        let directory = tempfile_directory();
        let path = directory.join("clone-policy.json");
        assert_eq!(load_policy(&path).unwrap(), ClonePolicy::Ask);
        save_policy(&path, ClonePolicy::Managed).unwrap();
        assert_eq!(load_policy(&path).unwrap(), ClonePolicy::Managed);
        for bytes in [
            br#"{"version":2,"policy":"managed"}"#.as_slice(),
            br#"{"version":1,"policy":"managed","surprise":true}"#,
            b"invalid",
        ] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(load_policy(&path).unwrap_err().code, ErrorCode::Validation);
        }
        save_policy(&path, ClonePolicy::Off).unwrap();
        assert_eq!(load_policy(&path).unwrap(), ClonePolicy::Off);
        fs::write(&path, vec![b'x'; POLICY_BYTES_MAX + 1]).unwrap();
        assert_eq!(
            load_policy(&path).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn preference_symlinks_and_special_files_are_rejected_without_modifying_targets() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile_directory();
        let target = directory.join("keep");
        fs::write(&target, b"user data").unwrap();
        let path = directory.join("clone-policy.json");
        symlink(&target, &path).unwrap();
        assert!(load_policy(&path).is_err());
        assert!(save_policy(&path, ClonePolicy::Managed).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"user data");
        fs::remove_file(&path).unwrap();
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRUSR).unwrap();
        assert!(load_policy(&path).is_err());
        fs::remove_file(&path).unwrap();
        save_policy(&path, ClonePolicy::Ask).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(directory).unwrap();
    }

    fn tempfile_directory() -> PathBuf {
        let mut random = [0; 8];
        getrandom::fill(&mut random).unwrap();
        let path =
            std::env::temp_dir().join(format!("quirl-clone-policy-{}", u64::from_ne_bytes(random)));
        fs::create_dir(&path).unwrap();
        path
    }
}
