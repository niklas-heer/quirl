//! Bounded process-owned discovery for prompt developer context.

use super::{ContainedChild, SessionEnvironment};
use std::{
    io::Read,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const PROJECT_ANCESTORS_MAX: usize = 32;
const PROBE_DEADLINE: Duration = Duration::from_millis(500);
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(2);
const PROBE_OUTPUT_BYTES_MAX: usize = 16 * 1024;
const VERSION_BYTES_MAX: usize = 64;

/// Git and toolchain values discovered for one exact working directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeveloperContextSnapshot {
    /// Current Git branch or abbreviated detached object identifier.
    pub git_branch: Option<String>,
    /// Compact bounded Git state counters, omitted for a clean worktree.
    pub git_state: Option<String>,
    /// Version reported by the active `rustc` selected by the session environment.
    pub rust_version: Option<String>,
}

/// A reusable probe carrying an exact snapshot of Quirl's private session environment.
///
/// Probes use fixed executables and arguments, never invoke a platform shell, retain at
/// most 16 KiB per stream, and share a 500 ms wall deadline. Each child is contained,
/// terminated, and reaped on every failure path.
#[derive(Clone)]
pub struct DeveloperContextProbe {
    environment: SessionEnvironment,
}

impl DeveloperContextProbe {
    pub(crate) fn new(environment: SessionEnvironment) -> Self {
        Self { environment }
    }

    /// Discover Git and active Rust context for `cwd` within one bounded refresh.
    pub fn probe(&self, cwd: &Path) -> DeveloperContextSnapshot {
        self.probe_with_deadline(cwd, PROBE_DEADLINE)
    }

    fn probe_with_deadline(
        &self,
        cwd: &Path,
        probe_deadline: Duration,
    ) -> DeveloperContextSnapshot {
        let Some(deadline) = Instant::now().checked_add(probe_deadline) else {
            return DeveloperContextSnapshot::default();
        };
        let rust_project = is_rust_project(cwd);
        let (git, rust_version) = thread::scope(|scope| {
            let git = scope.spawn(|| {
                self.run(
                    cwd,
                    "git",
                    &[
                        "status",
                        "--porcelain=v2",
                        "--branch",
                        "--show-stash",
                        "--untracked-files=normal",
                    ],
                    deadline,
                    &[("GIT_OPTIONAL_LOCKS", "0")],
                )
                .and_then(|output| parse_git_status(&output))
            });
            let rust = rust_project.then(|| {
                scope.spawn(|| {
                    self.run(
                        cwd,
                        "rustc",
                        &["--version"],
                        deadline,
                        &[("RUSTUP_AUTO_INSTALL", "0")],
                    )
                    .and_then(|output| parse_rust_version(&output))
                })
            });
            let git = git.join().ok().flatten();
            let rust = rust.and_then(|rust| rust.join().ok().flatten());
            (git, rust)
        });
        DeveloperContextSnapshot {
            git_branch: git.as_ref().and_then(|status| status.branch.clone()),
            git_state: git.and_then(|status| status.render_state()),
            rust_version,
        }
    }

    fn run(
        &self,
        cwd: &Path,
        program: &str,
        arguments: &[&str],
        deadline: Instant,
        extra_environment: &[(&str, &str)],
    ) -> Option<String> {
        if Instant::now() >= deadline {
            return None;
        }
        let executable = self.environment.resolve_executable(program)?;
        let mut command = Command::new(executable);
        self.environment.configure(&mut command).ok()?;
        command
            .args(arguments)
            .current_dir(cwd)
            .envs(extra_environment.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ContainedChild::spawn(&mut command).ok()?;
        let stdout = child.child_mut().stdout.take()?;
        let stderr = child.child_mut().stderr.take()?;
        let stdout_reader = thread::spawn(move || read_bounded(stdout));
        let stderr_reader = thread::spawn(move || read_bounded(stderr));
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // The direct executable may leave descendants holding the
                    // pipes. Terminate the complete contained tree before
                    // joining readers so refresh cannot wait forever on EOF.
                    let _ = child.terminate_and_reap();
                    break Some(status);
                }
                Ok(None) if Instant::now() < deadline => thread::sleep(PROBE_POLL_INTERVAL),
                Ok(None) | Err(_) => {
                    let _ = child.terminate_and_reap();
                    break None;
                }
            }
        };
        let stdout = stdout_reader.join().ok().and_then(Result::ok)?;
        let stderr = stderr_reader.join().ok().and_then(Result::ok)?;
        status
            .filter(ExitStatus::success)
            .and_then(|_| combine_output(stdout, stderr))
    }
}

fn read_bounded(mut reader: impl Read) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut overflowed = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok((retained, overflowed));
        }
        let remaining = PROBE_OUTPUT_BYTES_MAX.saturating_sub(retained.len());
        let retained_now = remaining.min(read);
        retained.extend_from_slice(&buffer[..retained_now]);
        overflowed |= retained_now < read;
        if overflowed {
            // Closing the pipe lets the child observe overflow immediately.
            // The owner still terminates and reaps the complete tree.
            return Ok((retained, true));
        }
    }
}

fn combine_output(stdout: (Vec<u8>, bool), stderr: (Vec<u8>, bool)) -> Option<String> {
    if stdout.1 || stderr.1 {
        return None;
    }
    let bytes = if stdout.0.is_empty() {
        stderr.0
    } else {
        stdout.0
    };
    String::from_utf8(bytes).ok()
}

fn is_rust_project(cwd: &Path) -> bool {
    const MARKERS: [&str; 5] = [
        "rust-toolchain.toml",
        "rust-toolchain",
        "Cargo.toml",
        "Cargo.lock",
        "rust-project.json",
    ];
    cwd.ancestors()
        .take(PROJECT_ANCESTORS_MAX)
        .any(|directory| {
            MARKERS
                .iter()
                .any(|marker| directory.join(marker).is_file())
        })
}

fn parse_rust_version(output: &str) -> Option<String> {
    let mut fields = output.split_whitespace();
    if fields.next()? != "rustc" {
        return None;
    }
    let version = fields.next()?;
    (version.len() <= VERSION_BYTES_MAX && version.chars().all(is_version_character))
        .then(|| version.to_owned())
}

fn is_version_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '+' | '_')
}

#[derive(Default)]
struct GitStatus {
    branch: Option<String>,
    object_id: Option<String>,
    staged: u16,
    modified: u16,
    conflicted: u16,
    untracked: u16,
    ahead: u16,
    behind: u16,
    stash: u16,
}

impl GitStatus {
    fn render_state(self) -> Option<String> {
        let mut parts = Vec::new();
        push_count(&mut parts, "+", self.staged);
        push_count(&mut parts, "~", self.modified);
        push_count(&mut parts, "!", self.conflicted);
        push_count(&mut parts, "?", self.untracked);
        push_count(&mut parts, "^", self.ahead);
        push_count(&mut parts, "v", self.behind);
        push_count(&mut parts, "*", self.stash);
        (!parts.is_empty()).then(|| parts.join(" "))
    }
}

fn push_count(parts: &mut Vec<String>, symbol: &str, count: u16) {
    if count > 0 {
        parts.push(format!("{symbol}{count}"));
    }
}

fn parse_git_status(output: &str) -> Option<GitStatus> {
    let mut status = GitStatus::default();
    for line in output.lines() {
        if line.len() > PROBE_OUTPUT_BYTES_MAX {
            return None;
        }
        if let Some(value) = line.strip_prefix("# branch.oid ") {
            status.object_id = valid_git_token(value).then(|| value.to_owned());
        } else if let Some(value) = line.strip_prefix("# branch.head ") {
            if value != "(detached)" && valid_git_token(value) {
                status.branch = Some(value.to_owned());
            }
        } else if let Some(value) = line.strip_prefix("# branch.ab ") {
            let mut fields = value.split_whitespace();
            status.ahead = parse_signed_count(fields.next()?, '+')?;
            status.behind = parse_signed_count(fields.next()?, '-')?;
        } else if let Some(value) = line.strip_prefix("# stash ") {
            status.stash = parse_count(value)?;
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let state = line.as_bytes().get(2..4)?;
            status.staged = status.staged.saturating_add(u16::from(state[0] != b'.'));
            status.modified = status.modified.saturating_add(u16::from(state[1] != b'.'));
        } else if line.starts_with("u ") {
            status.conflicted = status.conflicted.saturating_add(1);
        } else if line.starts_with("? ") {
            status.untracked = status.untracked.saturating_add(1);
        } else if !line.starts_with("! ") && !line.is_empty() {
            return None;
        }
    }
    if status.branch.is_none() {
        status.branch = status.object_id.as_deref().and_then(|object_id| {
            (object_id != "(initial)").then(|| object_id.chars().take(8).collect::<String>())
        });
    }
    Some(status)
}

fn valid_git_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|character| !character.is_control() && !character.is_whitespace())
}

fn parse_signed_count(value: &str, sign: char) -> Option<u16> {
    parse_count(value.strip_prefix(sign)?)
}

fn parse_count(value: &str) -> Option<u16> {
    value
        .parse::<u64>()
        .ok()
        .map(|value| u16::try_from(value).unwrap_or(u16::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{ffi::OsString, fs, os::unix::fs::PermissionsExt, path::PathBuf};

    #[test]
    fn rust_version_accepts_only_bounded_rustc_output() {
        assert_eq!(
            parse_rust_version("rustc 1.97.1 (abc 2026-01-01)\n"),
            Some("1.97.1".to_owned())
        );
        assert_eq!(parse_rust_version("cargo 1.97.1\n"), None);
        assert_eq!(parse_rust_version("rustc 1.97.1\u{1b}[31m\n"), None);
    }

    #[test]
    fn porcelain_v2_renders_branch_and_bounded_state_counts() {
        let parsed = parse_git_status(
            "# branch.oid 0123456789abcdef\n# branch.head feature/context\n# branch.ab +2 -1\n# stash 3\n1 M. N... 0 0 0 a b file\n1 .M N... 0 0 0 a b file2\nu UU N... 0 0 0 a b c file3\n? new\n",
        )
        .expect("valid fixture");
        assert_eq!(parsed.branch.as_deref(), Some("feature/context"));
        assert_eq!(
            parsed.render_state().as_deref(),
            Some("+1 ~1 !1 ?1 ^2 v1 *3")
        );
    }

    #[test]
    fn detached_git_status_uses_abbreviated_object_id() {
        let parsed = parse_git_status("# branch.oid 0123456789abcdef\n# branch.head (detached)\n")
            .expect("valid fixture");
        assert_eq!(parsed.branch.as_deref(), Some("01234567"));
        assert_eq!(parsed.render_state(), None);
    }

    #[cfg(unix)]
    fn executable_fixture(root: &Path, name: &str, body: &str) {
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fixture");
        let mut permissions = fs::metadata(&path).expect("fixture metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("make fixture executable");
    }

    #[cfg(unix)]
    fn temporary_fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "quirl-developer-context-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("bin")).expect("create fixture");
        root
    }

    #[cfg(unix)]
    #[test]
    fn probe_uses_the_session_path_and_exact_working_directory() {
        let root = temporary_fixture("session-path");
        let bin = root.join("bin");
        fs::write(root.join("Cargo.toml"), "[package]\nname = \"fixture\"\n")
            .expect("write Rust marker");
        executable_fixture(
            &bin,
            "git",
            "printf '# branch.oid 0123456789abcdef\\n# branch.head main\\n? file\\n'",
        );
        executable_fixture(
            &bin,
            "rustc",
            &format!(
                "test \"$PWD\" = '{}' || exit 9\nprintf 'rustc 9.8.7 (fixture)\\n'",
                fs::canonicalize(&root)
                    .expect("canonical fixture")
                    .display()
            ),
        );
        let environment =
            SessionEnvironment::capture([(OsString::from("PATH"), bin.as_os_str().to_os_string())]);
        // The production probe remains bounded at 500 ms. Give this positive-path
        // fixture extra scheduler headroom because the full workspace suite runs
        // many process-lifecycle tests concurrently on loaded CI hosts.
        let snapshot = DeveloperContextProbe::new(environment)
            .probe_with_deadline(&root, Duration::from_secs(3));

        assert_eq!(snapshot.git_branch.as_deref(), Some("main"));
        assert_eq!(snapshot.git_state.as_deref(), Some("?1"));
        assert_eq!(snapshot.rust_version.as_deref(), Some("9.8.7"));
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[cfg(unix)]
    #[test]
    fn hanging_probe_is_terminated_at_the_shared_deadline() {
        let root = temporary_fixture("deadline");
        let bin = root.join("bin");
        executable_fixture(&bin, "git", "while :; do :; done");
        let environment =
            SessionEnvironment::capture([(OsString::from("PATH"), bin.as_os_str().to_os_string())]);
        let started = Instant::now();
        let snapshot = DeveloperContextProbe::new(environment).probe(&root);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(snapshot, DeveloperContextSnapshot::default());
        fs::remove_dir_all(root).expect("remove fixture");
    }
}
