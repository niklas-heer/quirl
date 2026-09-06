//! GHQ-compatible project destinations and explicit managed cloning.
//!
//! Failure model: remote/configuration strings, existing paths, Git failures,
//! interruption, and concurrent destination creation are operating failures.
//! Planning never mutates repositories. Reservation creates the final directory
//! atomically and never removes it: failed clones and concurrent user edits remain
//! available for inspection. Every existing ancestor is checked without following
//! symlinks below the selected root; the selected root is a trusted user location.
//! A same-user process replacing ancestors during Git execution is outside this
//! pathname-based API's security boundary. No operation overwrites or pulls an
//! existing checkout. Metadata probes retain at most 16 KiB per stream, last at
//! most two seconds combined per operation, and use the process crate's child containment. Roots,
//! remote bytes, components, and path depth are explicitly bounded. Interactive
//! clone output streams directly and cancellation belongs to the native executor.

use clap::Subcommand;
use quirl_core::{CommandOutcome, ErrorCode, ProcessRequest, ShellError};
use quirl_process::NativeExecutor;
use std::{
    ffi::OsStr,
    fs,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

const SOURCE_BYTES_MAX: usize = 4096;
const ROOTS_MAX: usize = 16;
const COMPONENTS_MAX: usize = 32;
const PROBE_BYTES_MAX: usize = 16 * 1024;

struct ProbeBudget {
    started: Instant,
    signals: crate::InteractiveSignalCancellation,
}

impl ProbeBudget {
    fn new() -> Result<Self, ShellError> {
        Ok(Self {
            started: Instant::now(),
            signals: crate::InteractiveSignalCancellation::install()?,
        })
    }

    fn remaining(&self) -> Result<Duration, ShellError> {
        if self.signals.cancellation.is_cancelled() {
            return Err(cancelled_error());
        }
        let elapsed = self.started.elapsed();
        Duration::from_secs(2)
            .checked_sub(elapsed)
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "Git project metadata exceeded its shared deadline",
                )
                .with_context(format!(
                    "limit: 2000 ms; observed: {} ms",
                    elapsed.as_millis()
                ))
                .with_help("Check Git configuration and filesystem responsiveness before retrying")
            })
    }
}

/// Commands for GHQ-compatible repositories without changing ordinary Git behavior.
#[derive(Debug, Subcommand)]
pub(crate) enum ProjectsCommand {
    /// Clone a remote under the configured host/owner/repository directory.
    Clone {
        /// HTTPS, HTTP, SSH, or SCP-style remote URL.
        repository: String,
        /// Override GHQ_ROOT, ghq.root, and the default ~/Projects root.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Show or change interactive managed-clone suggestions.
    Policy {
        /// Ask once, always use managed destinations, or leave Git commands alone.
        #[arg(value_enum)]
        mode: Option<crate::clone_workflow::ClonePolicy>,
    },
    /// Show the effective primary root without changing Git configuration.
    Root {
        /// Resolve this root instead of environment and Git configuration.
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

/// Validated clone target; planning performs bounded, read-only Git probes.
pub(crate) struct ClonePlan {
    /// Absolute destination with the GHQ host/path convention.
    pub(crate) destination: PathBuf,
    /// Whether the destination is already a checkout of the same remote identity.
    pub(crate) existing: bool,
    /// Validated original transport URL, retained only for the Git invocation.
    pub(crate) source: String,
    root: PathBuf,
}

/// Run an explicit projects command using a private native execution environment.
pub(crate) fn run(command: ProjectsCommand) -> Result<i32, ShellError> {
    match run_command(command) {
        Err(error) if was_cancelled(&error) => Ok(130),
        result => result,
    }
}

fn run_command(command: ProjectsCommand) -> Result<i32, ShellError> {
    let mut executor = NativeExecutor::default();
    match command {
        ProjectsCommand::Clone { repository, root } => {
            let planned = plan(&repository, root.as_deref(), &mut executor)?;
            let status = execute(&planned, &mut executor)?;
            if status == 0 {
                if crate::projects::record_clone_default(&planned.destination).is_err() {
                    eprintln!(
                        "Project cloned, but the project index could not be updated; use the picker to refresh it."
                    );
                }
                println!("{}", planned.destination.display());
            }
            Ok(status)
        }
        ProjectsCommand::Policy { mode } => crate::clone_workflow::run_policy(mode),
        ProjectsCommand::Root { root } => {
            let selected = roots(root.as_deref(), &mut executor, &ProbeBudget::new()?)?.remove(0);
            println!("{}", selected.display());
            Ok(0)
        }
    }
}

/// Resolve a safe destination and recognize existing matching checkouts.
///
/// The explicit root wins over private GHQ_ROOT and Git configuration. With
/// multiple ghq.root entries, matching existing checkouts win over a new clone in
/// the first root. Conflicting occupied destinations are never reused.
pub(crate) fn plan(
    repository: &str,
    root: Option<&Path>,
    executor: &mut NativeExecutor,
) -> Result<ClonePlan, ShellError> {
    let identity = remote_identity(repository)?;
    let budget = ProbeBudget::new()?;
    let roots = roots(root, executor, &budget)?;
    let mut first = None;
    for root in roots {
        let destination = root.join(&identity);
        validate_text(path_text(&destination)?)?;
        budget.remaining()?;
        let candidate =
            inspect_candidate(repository, root, destination, &identity, executor, &budget);
        match candidate {
            Ok(candidate) if candidate.existing => {
                budget.remaining()?;
                return Ok(candidate);
            }
            Err(error) if error.code == ErrorCode::ResourceLimit => return Err(error),
            candidate => {
                if first.is_none() {
                    first = Some(candidate);
                }
            }
        }
    }
    budget.remaining()?;
    first.unwrap_or_else(|| Err(invalid("no Git project root is configured")))
}

fn inspect_candidate(
    repository: &str,
    root: PathBuf,
    destination: PathBuf,
    identity: &Path,
    executor: &mut NativeExecutor,
    budget: &ProbeBudget,
) -> Result<ClonePlan, ShellError> {
    check_descendants(&root, &destination)?;
    let existing = match fs::symlink_metadata(&destination) {
        Ok(_) => matching_checkout(&destination, identity, executor, budget)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(io_error("cannot inspect the project destination", error)),
    };
    Ok(ClonePlan {
        destination,
        existing,
        source: repository.to_owned(),
        root,
    })
}

/// Reserve a fresh destination atomically, preserving every path on failure.
///
/// Call immediately before running [`clone_command`]. An existing matching plan
/// is a no-op; callers should offer navigation instead of spawning Git for it.
pub(crate) fn reserve(
    planned: &ClonePlan,
    executor: &mut NativeExecutor,
) -> Result<(), ShellError> {
    let budget = ProbeBudget::new()?;
    budget.remaining()?;
    if planned.existing {
        check_descendants(&planned.root, &planned.destination)?;
        matching_checkout(
            &planned.destination,
            &remote_identity(&planned.source)?,
            executor,
            &budget,
        )?;
        return Ok(());
    }
    check_descendants(&planned.root, &planned.destination)?;
    create_checked_directories(&planned.root, &budget)?;
    let parent = planned
        .destination
        .parent()
        .ok_or_else(|| invalid("the project destination has no parent"))?;
    create_checked_descendants(&planned.root, parent, &budget)?;
    budget.remaining()?;
    fs::create_dir(&planned.destination).map_err(|error| {
        io_error(
            "could not reserve the project destination; no clone was started",
            error,
        )
        .with_help("Inspect the destination and retry; Quirl never replaces an existing path")
    })?;
    check_descendants(&planned.root, &planned.destination)?;
    budget.remaining()?;
    Ok(())
}

/// Build an exactly quoted native Git command for a previously reserved target.
/// This does not execute, reserve, expand variables, or append shell operators.
pub(crate) fn clone_command(planned: &ClonePlan) -> Result<String, ShellError> {
    command_source(&[
        "git",
        "clone",
        "--",
        &planned.source,
        path_text(&planned.destination)?,
    ])
}

/// Clone with inherited terminal I/O, or leave an existing matching checkout alone.
/// Failed or interrupted clones retain their partial destination for inspection.
pub(crate) fn execute(
    planned: &ClonePlan,
    executor: &mut NativeExecutor,
) -> Result<i32, ShellError> {
    if planned.existing {
        verify_completed(planned, executor)?;
        return Ok(0);
    }
    reserve(planned, executor)?;
    let result = executor
        .execute_interactive(&clone_command(planned)?)
        .map_err(|error| {
            ShellError::new(
                error.code,
                "managed clone could not complete; its destination was preserved",
            )
            .with_help("Inspect the destination before retrying, or choose another project root")
        })?;
    if result.status == 0 {
        verify_completed(planned, executor)?;
    } else {
        eprintln!("Managed clone did not complete; its destination was preserved for inspection.");
    }
    Ok(result.status)
}

/// Verify a completed clone before publishing it in the project picker.
/// Rechecks descendants and the exact top-level checkout and remote identity;
/// exit status zero alone is insufficient evidence of a completed clone.
pub(crate) fn verify_completed(
    planned: &ClonePlan,
    executor: &mut NativeExecutor,
) -> Result<(), ShellError> {
    let budget = ProbeBudget::new()?;
    budget.remaining()?;
    check_descendants(&planned.root, &planned.destination)?;
    matching_checkout(
        &planned.destination,
        &remote_identity(&planned.source)?,
        executor,
        &budget,
    )?;
    budget.remaining()?;
    Ok(())
}

fn roots(
    root: Option<&Path>,
    executor: &mut NativeExecutor,
    budget: &ProbeBudget,
) -> Result<Vec<PathBuf>, ShellError> {
    if let Some(root) = root {
        return Ok(vec![absolute_root(root, executor)?]);
    }
    if let Some(root) = environment_value(executor, "GHQ_ROOT")?
        && !root.is_empty()
    {
        return Ok(vec![absolute_root(Path::new(&root), executor)?]);
    }
    let output = probe_with_budget(
        executor,
        &["config", "--null", "--path", "--get-all", "ghq.root"],
        budget,
    )?;
    match output.status {
        0 => {
            let value = output.stdout.unwrap_or_default();
            let mut roots = Vec::new();
            for root in value.split_terminator('\0') {
                if roots.len() == ROOTS_MAX {
                    return Err(limit(
                        "configured Git project roots",
                        ROOTS_MAX,
                        roots.len().saturating_add(1),
                    ));
                }
                roots.push(absolute_root(Path::new(root), executor)?);
            }
            if roots.is_empty() {
                return Err(invalid("Git returned an empty project root configuration"));
            }
            Ok(roots)
        }
        1 => Ok(vec![absolute_root(Path::new("~/Projects"), executor)?]),
        _ => Err(invalid("could not read ghq.root from Git configuration")),
    }
}

fn absolute_root(root: &Path, executor: &NativeExecutor) -> Result<PathBuf, ShellError> {
    let text = path_text(root)?;
    validate_text(text)?;
    if text.is_empty() {
        return Err(invalid("the Git project root cannot be empty"));
    }
    let expanded = if text == "~" || text.starts_with("~/") {
        let home = environment_value(executor, "HOME")?
            .ok_or_else(|| invalid("HOME is not configured for the project root"))?;
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err(invalid(
                "HOME must be an absolute directory for managed projects",
            ));
        }
        home.join(text.strip_prefix("~/").unwrap_or(""))
    } else {
        root.to_owned()
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map_err(|error| io_error("cannot resolve the project root", error))?
            .join(expanded)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::Normal(_) | Component::Prefix(_) => {
                normalized.push(component)
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid(
                    "project roots cannot contain parent-directory traversal",
                ));
            }
        }
    }
    validate_path_bounds(&normalized)?;
    canonical_root(&normalized)
}

fn canonical_root(root: &Path) -> Result<PathBuf, ShellError> {
    // Resolve the nearest existing trusted ancestor before adding missing root
    // components. macOS /tmp and /var aliases must not leave new checkout paths
    // inconsistent with Git's physical top-level path or the project index.
    let mut ancestor = root.to_owned();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&ancestor) {
            Ok(mut canonical) => {
                if !canonical.is_dir() {
                    return Err(invalid(
                        "the selected project root ancestor is not a directory",
                    ));
                }
                for component in missing.into_iter().rev() {
                    canonical.push(component);
                }
                validate_path_bounds(&canonical)?;
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if missing.len() == COMPONENTS_MAX {
                    return Err(limit(
                        "missing project root components",
                        COMPONENTS_MAX,
                        missing.len().saturating_add(1),
                    ));
                }
                let component = ancestor
                    .file_name()
                    .ok_or_else(|| invalid("the project root has no existing directory ancestor"))?
                    .to_owned();
                missing.push(component);
                if !ancestor.pop() {
                    return Err(invalid(
                        "the project root has no existing directory ancestor",
                    ));
                }
            }
            Err(error) => return Err(io_error("cannot resolve the project root", error)),
        }
    }
}

fn validate_path_bounds(path: &Path) -> Result<(), ShellError> {
    validate_text(path_text(path)?)?;
    let count = path.components().count();
    if count > COMPONENTS_MAX {
        return Err(limit("project root components", COMPONENTS_MAX, count));
    }
    Ok(())
}

fn environment_value(executor: &NativeExecutor, key: &str) -> Result<Option<String>, ShellError> {
    executor
        .environment_snapshot()?
        .into_iter()
        .find(|(name, _)| name == OsStr::new(key))
        .map(|(_, value)| {
            value
                .into_string()
                .map_err(|_| invalid("the project root environment is not valid UTF-8"))
        })
        .transpose()
}

fn remote_identity(source: &str) -> Result<PathBuf, ShellError> {
    validate_text(source)?;
    let (authority, remote_path, scheme) =
        if let Some((scheme, remainder)) = source.split_once("://") {
            if !matches!(scheme, "https" | "http" | "ssh") {
                return Err(invalid(
                    "managed projects require an HTTPS, HTTP, or SSH Git remote",
                ));
            }
            let (authority, path) = remainder
                .split_once('/')
                .ok_or_else(|| invalid("the Git remote must include a repository path"))?;
            (authority, path, scheme)
        } else {
            let (authority, path) = source
                .split_once(':')
                .ok_or_else(|| invalid("local paths are not managed Git remotes"))?;
            if authority.contains('/') || path.starts_with('/') {
                return Err(invalid(
                    "use an SSH URL or a host:owner/repository Git remote",
                ));
            }
            (authority, path, "scp")
        };
    let host = remote_host(authority, scheme)?;
    let path = remote_path.strip_suffix('/').unwrap_or(remote_path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut identity = PathBuf::from(host);
    let mut count = 0_usize;
    for component in path.split('/') {
        count = count.saturating_add(1);
        if count > COMPONENTS_MAX {
            return Err(limit("Git remote path components", COMPONENTS_MAX, count));
        }
        validate_component(component)?;
        identity.push(component);
    }
    Ok(identity)
}

fn remote_host(authority: &str, scheme: &str) -> Result<String, ShellError> {
    let host = match authority.rsplit_once('@') {
        Some((user, host)) if matches!(scheme, "ssh" | "scp") => {
            if user.is_empty()
                || !user
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            {
                return Err(invalid("the SSH remote has an unsupported user name"));
            }
            host
        }
        Some(_) => {
            return Err(invalid(
                "credentials in Git URLs are not supported; use Git's credential helper",
            ));
        }
        None => authority,
    };
    let (host, port) = match host.split_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| invalid("the Git remote port must be between 1 and 65535"))?;
            (host, Some(port))
        }
        None => (host, None),
    };
    if host.is_empty()
        || host.len() > 253
        || !host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(invalid(
            "the Git remote needs a DNS host or SSH alias; IPv6 literals are not supported",
        ));
    }
    let mut host = host.to_ascii_lowercase();
    let default_port = match scheme {
        "https" => 443,
        "http" => 80,
        _ => 22,
    };
    if let Some(port) = port.filter(|port| *port != default_port) {
        // DNS names cannot contain underscores, so this suffix cannot alias a
        // literal hostname and different non-default ports remain distinct.
        host.push_str(&format!("__port_{port}"));
    }
    validate_component(&host)?;
    Ok(host)
}

fn validate_component(component: &str) -> Result<(), ShellError> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with(['.', ' '])
        || component
            .chars()
            .any(|character| character.is_control() || "\\<>:\"|?*%#".contains(character))
    {
        return Err(invalid(
            "the Git remote contains an unsafe or ambiguous path component",
        ));
    }
    if component.len() > 255 {
        return Err(limit("Git remote component bytes", 255, component.len()));
    }
    let stem = component
        .split('.')
        .next()
        .unwrap_or(component)
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes().get(3), Some(b'1'..=b'9')));
    if reserved || component.eq_ignore_ascii_case(".git") {
        return Err(invalid(
            "the Git remote contains a reserved filesystem name",
        ));
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), ShellError> {
    if value.len() > SOURCE_BYTES_MAX {
        return Err(limit(
            "Git project path or URL bytes",
            SOURCE_BYTES_MAX,
            value.len(),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(
            "Git project paths and URLs cannot contain control characters",
        ));
    }
    Ok(())
}

fn matching_checkout(
    destination: &Path,
    identity: &Path,
    executor: &mut NativeExecutor,
    budget: &ProbeBudget,
) -> Result<bool, ShellError> {
    let conflict = || {
        ShellError::new(ErrorCode::Validation, "the managed project destination is occupied by a different or incomplete checkout")
        .with_help("Inspect that directory, use a different --root, or clone manually to an explicit destination")
    };
    let path = path_text(destination)?;
    let top = probe_with_budget(
        executor,
        &["-C", path, "rev-parse", "--show-toplevel"],
        budget,
    )?;
    if top.status != 0 {
        return Err(conflict());
    }
    let top = PathBuf::from(top.stdout.unwrap_or_default().trim_end_matches('\n'));
    let physical = fs::canonicalize(destination)
        .map_err(|error| io_error("cannot inspect the existing project", error))?;
    if top != physical {
        return Err(conflict());
    }
    let origin = probe_with_budget(
        executor,
        &["-C", path, "config", "--get", "remote.origin.url"],
        budget,
    )?;
    if origin.status != 0
        || remote_identity(origin.stdout.unwrap_or_default().trim_end_matches('\n'))
            .ok()
            .as_deref()
            != Some(identity)
    {
        return Err(conflict());
    }
    Ok(true)
}

fn check_descendants(root: &Path, destination: &Path) -> Result<(), ShellError> {
    check_directory(root)?;
    let relative = destination
        .strip_prefix(root)
        .map_err(|_| invalid("the project destination escaped its root"))?;
    let mut current = root.to_owned();
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(invalid("the project destination contains traversal"));
        }
        current.push(component);
        check_directory(&current)?;
    }
    Ok(())
}

fn check_directory(path: &Path) -> Result<(), ShellError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(invalid(
            "a managed project path is a symlink or is not a directory",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(
            "cannot inspect a managed project directory",
            error,
        )),
    }
}

fn create_checked_directories(path: &Path, budget: &ProbeBudget) -> Result<(), ShellError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        budget.remaining()?;
        current.push(component);
        if current.is_dir() {
            // Above the chosen root, platform aliases such as macOS /var are
            // legitimate. The user selects this trusted root explicitly.
            continue;
        }
        create_directory(&current)?;
    }
    Ok(())
}

fn create_checked_descendants(
    root: &Path,
    destination: &Path,
    budget: &ProbeBudget,
) -> Result<(), ShellError> {
    let mut current = root.to_owned();
    let relative = destination
        .strip_prefix(root)
        .map_err(|_| invalid("the project destination escaped its root"))?;
    for component in relative.components() {
        budget.remaining()?;
        current.push(component);
        check_directory(&current)?;
        create_directory(&current)?;
    }
    Ok(())
}

fn create_directory(path: &Path) -> Result<(), ShellError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => check_directory(path),
        Err(error) => Err(io_error("cannot create a managed project directory", error)),
    }
}

fn probe_with_budget(
    executor: &mut NativeExecutor,
    arguments: &[&str],
    budget: &ProbeBudget,
) -> Result<CommandOutcome, ShellError> {
    let mut words = vec!["git"];
    words.extend_from_slice(arguments);
    let result = executor.execute_capture_request(ProcessRequest {
        command: command_source(&words)?,
        deadline: budget.remaining()?,
        cancelled: budget.signals.cancellation.atomic(),
        max_output_bytes: PROBE_BYTES_MAX,
    }).map_err(|error| {
        // Native errors may retain complete source/config output. Expose only
        // the error class so a configured origin cannot leak credentials.
        ShellError::new(error.code, "a bounded Git project metadata operation failed")
            .with_context(format!("deadline: 2000 ms; retained bytes per stream: {PROBE_BYTES_MAX}"))
            .with_help("Check that Git is available and its configuration is readable; retry the operation")
    });
    if budget.signals.cancellation.is_cancelled() {
        return Err(cancelled_error());
    }
    let outcome = result?;
    // Captured native execution may temporarily give Git the terminal's
    // foreground group. Ctrl-C then reaches Git directly rather than the shell
    // signal flag; never treat that interrupted probe as permission to clone.
    if matches!(outcome.status, 130 | 143) {
        return Err(cancelled_error());
    }
    budget.remaining()?;
    Ok(outcome)
}

#[cfg(test)]
fn probe(executor: &mut NativeExecutor, arguments: &[&str]) -> Result<CommandOutcome, ShellError> {
    probe_with_budget(executor, arguments, &ProbeBudget::new()?)
}

fn command_source(words: &[&str]) -> Result<String, ShellError> {
    let mut source = String::new();
    for word in words {
        validate_text(word)?;
        if !source.is_empty() {
            source.push(' ');
        }
        source.push('\'');
        source.push_str(&word.replace('\'', "'\\''"));
        source.push('\'');
    }
    Ok(source)
}

fn path_text(path: &Path) -> Result<&str, ShellError> {
    path.to_str()
        .ok_or_else(|| invalid("managed project paths must be valid UTF-8"))
}

/// Identify cancellation without interpreting a diagnostic message or URL.
pub(crate) fn was_cancelled(error: &ShellError) -> bool {
    error.details.exit_status == Some(130)
}

fn cancelled_error() -> ShellError {
    let mut error = ShellError::new(
        ErrorCode::ResourceLimit,
        "Git project operation was cancelled",
    )
    .with_help("Inspect any reserved destination before retrying the command");
    error.details.exit_status = Some(130);
    error
}

fn invalid(message: &str) -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, message)
        .with_help("Use a supported remote such as https://github.com/owner/repository and a writable --root directory")
}

fn limit(subject: &str, maximum: usize, observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{subject} exceed the managed-project limit"),
    )
    .with_context(format!("limit: {maximum}; observed: {observed}"))
    .with_help("Shorten the remote or path, or reduce the number of ghq.root entries")
}

fn io_error(message: &str, error: std::io::Error) -> ShellError {
    ShellError::new(ErrorCode::Io, message)
        .with_context(error.to_string())
        .with_help("Check directory permissions and inspect the destination before retrying")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).unwrap();
            let directory = std::env::temp_dir().join(format!(
                "quirl-project-clone-{:016x}",
                u64::from_ne_bytes(random)
            ));
            fs::create_dir(&directory).unwrap();
            Self(fs::canonicalize(directory).unwrap())
        }

        fn executor(&self) -> NativeExecutor {
            let mut executor = NativeExecutor::default();
            executor
                .set_environment_variables(&[
                    ("HOME".into(), self.0.to_str().unwrap().into()),
                    ("GHQ_ROOT".into(), String::new()),
                    (
                        "GIT_CONFIG_GLOBAL".into(),
                        self.0.join("gitconfig").to_str().unwrap().into(),
                    ),
                    ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                    ("GIT_CONFIG_COUNT".into(), "0".into()),
                ])
                .unwrap();
            executor
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn common_git_transports_share_the_ghq_identity() {
        for source in [
            "https://GitHub.com/team/repository.git",
            "http://github.com/team/repository",
            "ssh://git@github.com/team/repository.git/",
            "git@github.com:team/repository.git",
            "ssh://github.com:22/team/repository",
            "https://github.com:443/team/repository",
        ] {
            assert_eq!(
                remote_identity(source).unwrap(),
                Path::new("github.com/team/repository")
            );
        }
    }

    #[test]
    fn nested_namespaces_and_ssh_aliases_are_preserved() {
        assert_eq!(
            remote_identity("work-git:group/subgroup/repo.git").unwrap(),
            Path::new("work-git/group/subgroup/repo")
        );
        assert_eq!(
            remote_identity("https://gitlab.com/team/deep/project.git").unwrap(),
            Path::new("gitlab.com/team/deep/project")
        );
    }

    #[test]
    fn nondefault_ports_have_collision_free_host_components() {
        assert_eq!(
            remote_identity("ssh://git@example.com:2222/team/repo").unwrap(),
            Path::new("example.com__port_2222/team/repo")
        );
        assert_ne!(
            remote_identity("ssh://example.com:2222/team/repo").unwrap(),
            remote_identity("ssh://example.com:2223/team/repo").unwrap()
        );
        assert!(remote_identity("https://example.com__port_2222/team/repo").is_err());
    }

    #[test]
    fn unsafe_remote_forms_fail_without_echoing_the_remote() {
        for source in [
            "/tmp/local",
            "../local",
            "file:///tmp/repo",
            "git://example.com/team/repo",
            "https://secret-token@example.com/team/repo",
            "https://name:secret@example.com/team/repo",
            "https://example.com/team/repo?token=secret",
            "https://example.com/team/../repo",
            "https://example.com/team//repo",
            "https://example.com/team/%2e%2e",
            "https://example.com/team/CON",
            "https://example.com/team/nul.txt",
            "https://example.com/team/LPT1",
            "https://example.com/team/.git",
            "https://example.com/team/repo.",
            "https://example.com/team/repo ",
            "https://example.com/team/rep\u{1b}o",
            "https://example.com/team/rep\0o",
            "https://example.com/team\\other/repo",
            "https://example.com/team/repo//",
            "https://example.com:0/team/repo",
            "https://example.com:65536/team/repo",
            "ssh://[::1]/team/repo",
            "host:/absolute/repo",
            "https://example.com/",
        ] {
            let error = remote_identity(source).unwrap_err();
            assert!(!error.message.contains("secret"));
            assert!(error.details.command.is_none());
        }
    }

    #[test]
    fn remote_limits_accept_the_boundary_and_reject_the_next_item() {
        let component = "a".repeat(255);
        assert!(remote_identity(&format!("https://host/{component}")).is_ok());
        assert_eq!(
            remote_identity(&format!("https://host/{component}a"))
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        let path = vec!["a"; COMPONENTS_MAX].join("/");
        assert!(remote_identity(&format!("https://host/{path}")).is_ok());
        assert_eq!(
            remote_identity(&format!("https://host/{path}/a"))
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            remote_identity(&"a".repeat(SOURCE_BYTES_MAX + 1))
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
    }

    #[test]
    fn quoted_clone_commands_keep_metacharacters_literal() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let planned = plan(
            "git@host:team/it's-$(touch-owned).git",
            Some(&fixture.0),
            &mut executor,
        )
        .unwrap();
        let source = clone_command(&planned).unwrap();
        let parsed = quirl_syntax::parse_command_list(&source).unwrap();
        let command = &parsed.pipelines[0].commands[0];
        assert_eq!(
            command.words,
            [
                "git",
                "clone",
                "--",
                &planned.source,
                planned.destination.to_str().unwrap()
            ]
        );
        assert_eq!(parsed.pipelines.len(), 1);
    }

    #[test]
    fn explicit_and_private_environment_roots_take_precedence() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let configured = fixture.0.join("configured");
        let environment = fixture.0.join("private");
        probe(
            &mut executor,
            &[
                "config",
                "--global",
                "ghq.root",
                configured.to_str().unwrap(),
            ],
        )
        .unwrap();
        executor
            .set_environment_variable("GHQ_ROOT".into(), environment.to_str().unwrap().into())
            .unwrap();
        let selected = plan("https://host/team/repo", None, &mut executor).unwrap();
        assert_eq!(selected.destination, environment.join("host/team/repo"));
        let selected = plan("https://host/team/repo", Some(&fixture.0), &mut executor).unwrap();
        assert_eq!(selected.destination, fixture.0.join("host/team/repo"));
    }

    #[test]
    fn absent_configuration_uses_the_private_home_projects_directory() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let selected = plan("https://host/team/repo", None, &mut executor).unwrap();
        assert_eq!(
            selected.destination,
            fixture.0.join("Projects/host/team/repo")
        );
        assert!(!selected.destination.exists());
    }

    #[test]
    fn git_config_roots_expand_tilde_using_private_home() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        probe(
            &mut executor,
            &["config", "--global", "ghq.root", "~/Managed"],
        )
        .unwrap();
        let selected = plan("https://host/team/repo", None, &mut executor).unwrap();
        assert_eq!(
            selected.destination,
            fixture.0.join("Managed/host/team/repo")
        );
    }

    fn initialize_checkout(executor: &mut NativeExecutor, destination: &Path, remote: &str) {
        fs::create_dir_all(destination).unwrap();
        assert_eq!(
            probe(executor, &["init", destination.to_str().unwrap()])
                .unwrap()
                .status,
            0
        );
        assert_eq!(
            probe(
                executor,
                &[
                    "-C",
                    destination.to_str().unwrap(),
                    "remote",
                    "add",
                    "origin",
                    remote
                ]
            )
            .unwrap()
            .status,
            0
        );
    }

    #[test]
    fn later_configured_roots_find_matching_existing_checkouts() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let first = fixture.0.join("first");
        let second = fixture.0.join("second");
        for root in [&first, &second] {
            probe(
                &mut executor,
                &[
                    "config",
                    "--global",
                    "--add",
                    "ghq.root",
                    root.to_str().unwrap(),
                ],
            )
            .unwrap();
        }
        let destination = second.join("host/team/repo");
        initialize_checkout(&mut executor, &destination, "git@host:team/repo.git");
        let selected = plan("https://host/team/repo", None, &mut executor).unwrap();
        assert!(selected.existing);
        assert_eq!(selected.destination, destination);
        assert!(!first.exists());
        assert_eq!(execute(&selected, &mut executor).unwrap(), 0);
    }

    #[test]
    fn foreign_repositories_and_incomplete_directories_are_never_reused() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let destination = fixture.0.join("host/team/repo");
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("keep"), "user data").unwrap();
        assert!(plan("https://host/team/repo", Some(&fixture.0), &mut executor).is_err());
        initialize_checkout(&mut executor, &destination, "https://host/other/repo");
        assert!(plan("https://host/team/repo", Some(&fixture.0), &mut executor).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("keep")).unwrap(),
            "user data"
        );
    }

    #[test]
    fn stale_existing_plans_revalidate_origin_before_success() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let destination = fixture.0.join("host/team/repo");
        initialize_checkout(&mut executor, &destination, "https://host/team/repo");
        let selected = plan("https://host/team/repo", Some(&fixture.0), &mut executor).unwrap();
        probe(
            &mut executor,
            &[
                "-C",
                destination.to_str().unwrap(),
                "remote",
                "set-url",
                "origin",
                "https://host/other/repo",
            ],
        )
        .unwrap();
        assert!(reserve(&selected, &mut executor).is_err());
        assert!(execute(&selected, &mut executor).is_err());
    }

    #[test]
    fn concurrent_destination_creation_preserves_the_winning_directory() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let selected = plan("https://host/team/repo", Some(&fixture.0), &mut executor).unwrap();
        fs::create_dir_all(&selected.destination).unwrap();
        fs::write(selected.destination.join("keep"), "user data").unwrap();
        assert!(reserve(&selected, &mut executor).is_err());
        assert_eq!(
            fs::read_to_string(selected.destination.join("keep")).unwrap(),
            "user data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_repository_ancestors_never_escape_the_root() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let mut executor = fixture.executor();
        symlink(&outside.0, fixture.0.join("host")).unwrap();
        assert!(create_directory(&fixture.0.join("host")).is_err());
        assert!(plan("https://host/team/repo", Some(&fixture.0), &mut executor).is_err());
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn an_ancestor_replaced_after_planning_is_rejected_without_writing_through_it() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let mut executor = fixture.executor();
        let selected = plan("https://host/team/repo", Some(&fixture.0), &mut executor).unwrap();
        symlink(&outside.0, fixture.0.join("host")).unwrap();
        assert!(reserve(&selected, &mut executor).is_err());
        assert_eq!(fs::read_dir(&outside.0).unwrap().count(), 0);
    }

    #[test]
    fn roots_reject_parent_traversal_and_empty_strings() {
        let fixture = Fixture::new();
        let executor = fixture.executor();
        assert!(absolute_root(Path::new(""), &executor).is_err());
        assert!(absolute_root(Path::new("~/../elsewhere"), &executor).is_err());
        assert!(absolute_root(Path::new("bad\nroot"), &executor).is_err());
    }

    #[test]
    fn configured_root_count_is_bounded_before_filesystem_mutation() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        for _ in 0..=ROOTS_MAX {
            probe(
                &mut executor,
                &["config", "--global", "--add", "ghq.root", "~/Projects"],
            )
            .unwrap();
        }
        let error = plan("https://host/team/repo", None, &mut executor)
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(!fixture.0.join("Projects").exists());
    }

    #[cfg(unix)]
    #[test]
    fn failed_git_clone_retains_its_partial_destination() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let binary_directory = fixture.0.join("bin");
        fs::create_dir(&binary_directory).unwrap();
        let git = binary_directory.join("git");
        fs::write(&git, "#!/bin/sh\nfor arg do destination=$arg; done\nprintf preserved > \"$destination/partial\"\nexit 19\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
        executor
            .set_environment_variable("PATH".into(), binary_directory.to_str().unwrap().into())
            .unwrap();
        let selected = plan(
            "https://host/team/repo",
            Some(&fixture.0.join("managed")),
            &mut executor,
        )
        .unwrap();
        assert_eq!(execute(&selected, &mut executor).unwrap(), 19);
        assert_eq!(
            fs::read_to_string(selected.destination.join("partial")).unwrap(),
            "preserved"
        );
    }

    #[test]
    fn successful_clone_uses_a_local_transport_fixture_without_network() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let source = fixture.0.join("source.git");
        assert_eq!(
            probe(&mut executor, &["init", "--bare", source.to_str().unwrap()])
                .unwrap()
                .status,
            0
        );
        // Git's normal insteadOf mechanism maps the public-looking remote to an
        // owned local bare repository; no network access or global config edit.
        executor
            .set_environment_variables(&[
                ("GIT_CONFIG_COUNT".into(), "1".into()),
                (
                    "GIT_CONFIG_KEY_0".into(),
                    format!("url.file://{}.insteadOf", source.display()),
                ),
                ("GIT_CONFIG_VALUE_0".into(), "https://host/team/repo".into()),
            ])
            .unwrap();
        let selected = plan(
            "https://host/team/repo",
            Some(&fixture.0.join("managed")),
            &mut executor,
        )
        .unwrap();
        assert_eq!(execute(&selected, &mut executor).unwrap(), 0);
        let repeated = plan(
            "git@host:team/repo.git",
            Some(&fixture.0.join("managed")),
            &mut executor,
        )
        .unwrap();
        assert!(repeated.existing);
        assert_eq!(repeated.destination, selected.destination);
    }
    #[test]
    fn a_conflicting_first_root_does_not_hide_a_matching_later_checkout() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let first = fixture.0.join("first");
        let second = fixture.0.join("second");
        for root in [&first, &second] {
            probe(
                &mut executor,
                &[
                    "config",
                    "--global",
                    "--add",
                    "ghq.root",
                    root.to_str().unwrap(),
                ],
            )
            .unwrap();
        }
        fs::create_dir_all(first.join("host/team/repo")).unwrap();
        let destination = second.join("host/team/repo");
        initialize_checkout(&mut executor, &destination, "git@host:team/repo.git");
        assert_eq!(
            plan("https://host/team/repo", None, &mut executor)
                .unwrap()
                .destination,
            destination
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_git_metadata_fails_with_the_capture_limit() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let git = fixture.0.join("git");
        fs::write(
            &git,
            "#!/bin/sh\ni=0; while [ \"$i\" -lt 17000 ]; do printf x; i=$((i+1)); done\n",
        )
        .unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
        executor
            .set_environment_variable("PATH".into(), fixture.0.to_str().unwrap().into())
            .unwrap();
        let error = plan("https://host/team/repo", None, &mut executor)
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.command.is_none());
        assert!(!fixture.0.join("Projects").exists());
    }

    #[test]
    fn zero_exit_without_a_matching_checkout_is_not_success() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let selected = plan("https://host/team/repo", Some(&fixture.0), &mut executor).unwrap();
        reserve(&selected, &mut executor).unwrap();
        assert!(verify_completed(&selected, &mut executor).is_err());
        assert!(selected.destination.is_dir());
    }
    #[test]
    fn a_newline_inside_one_configured_root_is_not_split_into_two_roots() {
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        fs::write(
            fixture.0.join("gitconfig"),
            "[ghq]\nroot = \"~/first\\n~/second\"\n",
        )
        .unwrap();
        assert!(plan("https://host/team/repo", None, &mut executor).is_err());
        assert!(!fixture.0.join("first").exists());
        assert!(!fixture.0.join("second").exists());
    }

    #[cfg(unix)]
    #[test]
    fn canonical_root_expansion_cannot_bypass_the_depth_limit() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let executor = fixture.executor();
        let deep = fixture.0.join(vec!["a"; COMPONENTS_MAX].join("/"));
        fs::create_dir_all(&deep).unwrap();
        let alias = fixture.0.join("short");
        symlink(&deep, &alias).unwrap();
        assert_eq!(
            absolute_root(&alias, &executor).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_checkout_probes_share_the_planning_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        let destination = fixture.0.join("host/team/repo");
        fs::create_dir_all(&destination).unwrap();
        let git = fixture.0.join("git");
        fs::write(&git, "#!/bin/sh\n/bin/sleep 1.1\ncase \"$3\" in rev-parse) printf '%s\\n' \"$2\";; config) printf 'https://host/team/repo\\n';; esac\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
        executor
            .set_environment_variable("PATH".into(), fixture.0.to_str().unwrap().into())
            .unwrap();
        let error = plan("https://host/team/repo", Some(&fixture.0), &mut executor)
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
    }

    #[cfg(unix)]
    #[test]
    fn missing_roots_resolve_trusted_ancestor_aliases_before_planning() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let mut executor = fixture.executor();
        let alias = fixture.0.join("alias");
        symlink(&outside.0, &alias).unwrap();
        let selected = plan(
            "https://host/team/repo",
            Some(&alias.join("new/root")),
            &mut executor,
        )
        .unwrap();
        assert_eq!(
            selected.destination,
            outside.0.join("new/root/host/team/repo")
        );
        reserve(&selected, &mut executor).unwrap();
        assert_eq!(
            fs::canonicalize(&selected.destination).unwrap(),
            selected.destination
        );
    }
    #[cfg(unix)]
    #[test]
    fn metadata_sigint_stops_probes_and_reaps_the_child() {
        use std::sync::{Arc, atomic::AtomicBool};
        if std::env::var_os("QUIRL_TEST_PROJECT_SIGINT").as_deref() == Some(OsStr::new("child")) {
            check_metadata_sigint_in_isolated_process(false);
            check_metadata_sigint_in_isolated_process(true);
            return;
        }
        // Actual signals must not cancel unrelated parallel tests. Run this
        // case in its own process group through the normal bounded process owner.
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        executor
            .set_environment_variable("QUIRL_TEST_PROJECT_SIGINT".into(), "child".into())
            .unwrap();
        let binary = std::env::current_exe().unwrap();
        let command = command_source(&[
            binary.to_str().unwrap(),
            "--exact",
            "project_clone::tests::metadata_sigint_stops_probes_and_reaps_the_child",
            "--nocapture",
        ])
        .unwrap();
        let outcome = executor
            .execute_capture_request(ProcessRequest {
                command,
                deadline: Duration::from_secs(8),
                cancelled: Arc::new(AtomicBool::new(false)),
                max_output_bytes: 8192,
            })
            .unwrap();
        assert_eq!(outcome.status, 0, "{outcome:?}");
    }

    #[cfg(unix)]
    fn check_metadata_sigint_in_isolated_process(signal_child: bool) {
        use nix::{
            errno::Errno,
            sys::signal::{Signal, kill},
            unistd::{Pid, getpid},
        };
        use std::{os::unix::fs::PermissionsExt, thread};
        let fixture = Fixture::new();
        let mut executor = fixture.executor();
        fs::create_dir_all(fixture.0.join("host/team/repo")).unwrap();
        let marker = fixture.0.join("metadata.pid");
        let git = fixture.0.join("git");
        fs::write(
            &git,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$QUIRL_METADATA_PID_FILE\"\nexec /bin/sleep 30\n",
        )
        .unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
        executor
            .set_environment_variables(&[
                ("PATH".into(), fixture.0.to_str().unwrap().into()),
                (
                    "QUIRL_METADATA_PID_FILE".into(),
                    marker.to_str().unwrap().into(),
                ),
            ])
            .unwrap();
        let observed_marker = marker.clone();
        let sender = thread::spawn(move || {
            let started = Instant::now();
            let child_pid = loop {
                if let Some(pid) = fs::read_to_string(&observed_marker)
                    .ok()
                    .and_then(|text| text.parse::<i32>().ok())
                {
                    break pid;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(1),
                    "the metadata child must start before cancellation"
                );
                thread::sleep(Duration::from_millis(5));
            };
            let target = if signal_child {
                Pid::from_raw(child_pid)
            } else {
                getpid()
            };
            kill(target, Signal::SIGINT).unwrap();
        });
        let error = plan("https://host/team/repo", Some(&fixture.0), &mut executor)
            .err()
            .unwrap();
        sender.join().unwrap();
        assert!(was_cancelled(&error), "{error:?}");
        let pid = fs::read_to_string(marker).unwrap().parse::<i32>().unwrap();
        assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
        assert_eq!(
            fs::read_dir(fixture.0.join("host/team/repo"))
                .unwrap()
                .count(),
            0
        );
    }
}
