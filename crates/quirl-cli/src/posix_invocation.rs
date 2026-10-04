//! How Quirl behaves when another program starts it as a shell.
//!
//! `sshd`, `scp`, `rsync`, `git`, editors resolving a login environment,
//! terminal emulators, and coding agents all start the user's shell with
//! POSIX conventions: `$SHELL -c 'code' [name [arg...]]`, `$SHELL -l -i -c
//! 'env'`, a script on standard input, or a login shell whose argv0 begins
//! with `-`. Those callers write POSIX `sh`, so Quirl hands such work to
//! `/bin/sh` unchanged instead of interpreting it. Quirl's own language
//! remains available interactively and through `quirl exec`, `quirl data`,
//! and `quirl run`.
//!
//! Parsing happens before Clap so that `-c` stays cheap for callers such as
//! `rsync` that start a shell per transfer. Only argument vectors that begin
//! with POSIX shell options take this path; every other invocation keeps the
//! Quirl command-line interface.
//!
//! An interactive login session first adopts the environment a POSIX login
//! shell (`/bin/sh -l`) establishes, such as `/etc/profile` and `path_helper`
//! entries, then re-executes Quirl with it. Capturing that environment is
//! bounded by a deadline and a byte limit; any failure keeps the inherited
//! environment and reports why, so a broken profile never prevents login.

use quirl_core::{ErrorCode, ShellError};
use std::{
    ffi::{OsStr, OsString},
    io::Read,
    time::{Duration, Instant},
};

/// Interpreter for every non-interactive invocation.
const POSIX_SHELL_PATH: &str = "/bin/sh";
/// Longest an interactive login waits for `/bin/sh -l` to report its
/// environment before continuing with the inherited one.
const LOGIN_ENVIRONMENT_DEADLINE: Duration = Duration::from_secs(5);
/// Largest login environment report accepted, matching the session bound.
const LOGIN_ENVIRONMENT_BYTES_MAX: usize = quirl_process::SESSION_ENVIRONMENT_BYTES_MAX;
/// Code that prints the login shell's environment as NUL-separated records.
const LOGIN_ENVIRONMENT_SCRIPT: &str = "exec /usr/bin/env -0";

/// A shell-style invocation recognized from POSIX options or a login argv0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PosixInvocation {
    /// `-l`, `--login`, or an argv0 beginning with `-`.
    pub(crate) login: bool,
    /// `-i`: an interactive session even when standard input is not a terminal.
    pub(crate) interactive: bool,
    /// `-s`: read commands from standard input.
    pub(crate) read_stdin: bool,
    /// The `-c` command string.
    pub(crate) command: Option<OsString>,
    /// Operands after the options: `$0` and positional parameters with `-c`,
    /// or positional parameters with `-s`.
    pub(crate) operands: Vec<OsString>,
    /// `-c` appeared without a command string after the options.
    missing_command: bool,
}

/// What a recognized invocation should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PosixAction {
    /// Run `/bin/sh` with the same command, operands, and login flag.
    DelegateToShell,
    /// Open Quirl's interactive shell, adopting a login environment first
    /// when `login` is set.
    Interactive {
        /// Whether to adopt the POSIX login environment before starting.
        login: bool,
    },
}

impl PosixInvocation {
    /// Recognize a shell-style invocation from `argv0` and the remaining
    /// arguments, or return `None` for Quirl's own command-line interface.
    pub(crate) fn parse(argv0: &OsStr, arguments: &[OsString]) -> Option<Self> {
        let login_argv0 = argv0
            .to_str()
            .and_then(|argv0| argv0.rsplit('/').next())
            .is_some_and(|name| name.starts_with('-'));
        let mut invocation = Self {
            login: login_argv0,
            interactive: false,
            read_stdin: false,
            command: None,
            operands: Vec::new(),
            missing_command: false,
        };
        let mut recognized = login_argv0;
        let mut wants_command = false;
        let mut index = 0;
        while let Some(argument) = arguments.get(index) {
            let Some(text) = argument.to_str() else {
                break;
            };
            match text {
                "--" => {
                    index = index.saturating_add(1);
                    break;
                }
                "--login" => invocation.login = true,
                _ if is_option_cluster(text) => {
                    for flag in text.chars().skip(1) {
                        match flag {
                            'c' => wants_command = true,
                            'l' => invocation.login = true,
                            'i' => invocation.interactive = true,
                            _ => invocation.read_stdin = true,
                        }
                    }
                }
                _ => break,
            }
            recognized = true;
            index = index.saturating_add(1);
        }
        if !recognized {
            return None;
        }
        let mut operands = arguments.get(index..).unwrap_or_default().iter().cloned();
        if wants_command {
            invocation.command = operands.next();
            invocation.missing_command = invocation.command.is_none();
        }
        invocation.operands = operands.collect();
        Some(invocation)
    }

    /// A plain `quirl` whose standard input is not a terminal: a script for
    /// `/bin/sh -s`.
    pub(crate) fn standard_input() -> Self {
        Self {
            login: false,
            interactive: false,
            read_stdin: true,
            command: None,
            operands: Vec::new(),
            missing_command: false,
        }
    }

    /// Decide what to do, given whether standard input is a terminal.
    pub(crate) fn action(&self, stdin_is_terminal: bool) -> Result<PosixAction, ShellError> {
        if self.missing_command {
            return Err(missing_command_error());
        }
        let wants_command = self.command.is_some();
        let interactive = !wants_command
            && !self.read_stdin
            && self.operands.is_empty()
            && (self.interactive || stdin_is_terminal);
        if interactive {
            return Ok(PosixAction::Interactive { login: self.login });
        }
        Ok(PosixAction::DelegateToShell)
    }

    /// Arguments for `/bin/sh` that reproduce this invocation.
    pub(crate) fn shell_arguments(&self) -> Vec<OsString> {
        let mut arguments = Vec::with_capacity(self.operands.len().saturating_add(3));
        if self.login {
            arguments.push(OsString::from("-l"));
        }
        match &self.command {
            Some(command) => {
                arguments.push(OsString::from("-c"));
                arguments.push(command.clone());
            }
            None if self.operands.is_empty() || self.read_stdin => {
                arguments.push(OsString::from("-s"));
            }
            None => {}
        }
        arguments.extend(self.operands.iter().cloned());
        arguments
    }
}

/// `-c`, `-l`, `-i`, `-s` and their clusters such as `-lc` or `-ilc`.
fn is_option_cluster(text: &str) -> bool {
    text.len() > 1
        && text.starts_with('-')
        && text
            .chars()
            .skip(1)
            .all(|flag| matches!(flag, 'c' | 'l' | 'i' | 's'))
}

fn missing_command_error() -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, "-c needs a command string")
        .with_help("Usage: quirl -c 'command' [name [argument...]]")
}

/// Replace this process with `/bin/sh` running the same invocation, so
/// signals, standard streams, and the exit status belong to the shell.
#[cfg(unix)]
pub(crate) fn delegate_to_shell(invocation: &PosixInvocation) -> ShellError {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(POSIX_SHELL_PATH)
        .args(invocation.shell_arguments())
        .exec();
    ShellError::new(ErrorCode::ProcessSpawn, "could not start the POSIX shell")
        .with_context(format!("{POSIX_SHELL_PATH}: {error}"))
        .with_help("Quirl runs `-c` commands and scripts through /bin/sh; check that it exists")
}

/// Windows has no POSIX shell to delegate to.
#[cfg(not(unix))]
pub(crate) fn delegate_to_shell(_invocation: &PosixInvocation) -> ShellError {
    ShellError::new(
        ErrorCode::InvalidCommand,
        "`-c` and standard-input scripts need a POSIX shell, which Windows does not provide",
    )
    .with_help("Use `quirl exec 'command'` for Quirl's native command language")
}

/// Re-execute Quirl as a plain interactive shell inside the environment a
/// POSIX login shell establishes. Returns only when that is impossible; the
/// caller then continues with the inherited environment.
#[cfg(unix)]
pub(crate) fn reexecute_with_login_environment() -> ShellError {
    use std::os::unix::process::CommandExt;
    let environment = match capture_login_environment() {
        Ok(environment) => environment,
        Err(error) => return error,
    };
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            return ShellError::new(ErrorCode::Io, "could not locate the Quirl executable")
                .with_context(error.to_string())
                .with_help("Quirl continues with the environment it was started with");
        }
    };
    let error = std::process::Command::new(executable)
        .arg0("quirl")
        .env_clear()
        .envs(environment)
        .exec();
    ShellError::new(
        ErrorCode::ProcessSpawn,
        "could not restart Quirl for the login session",
    )
    .with_context(error.to_string())
    .with_help("Quirl continues with the environment it was started with")
}

/// Windows login sessions keep the inherited environment.
#[cfg(not(unix))]
pub(crate) fn reexecute_with_login_environment() -> ShellError {
    ShellError::new(
        ErrorCode::InvalidCommand,
        "login environments need a POSIX shell, which Windows does not provide",
    )
    .with_help("Quirl continues with the environment it was started with")
}

/// Run `/bin/sh -l` once and return the environment it exports.
#[cfg(unix)]
fn capture_login_environment() -> Result<Vec<(OsString, OsString)>, ShellError> {
    let mut command = std::process::Command::new(POSIX_SHELL_PATH);
    command
        .args(["-l", "-c", LOGIN_ENVIRONMENT_SCRIPT])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit());
    let mut child = quirl_process::ContainedChild::spawn(&mut command)?;
    let stdout = child.child_mut().stdout.take().ok_or_else(|| {
        ShellError::new(ErrorCode::Io, "the login shell's output was not captured")
            .with_help("Quirl continues with the environment it was started with")
    })?;
    // The reader ends when the contained process group exits or is killed,
    // because every holder of the pipe belongs to that group.
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let limit =
            u64::try_from(LOGIN_ENVIRONMENT_BYTES_MAX.saturating_add(1)).unwrap_or(u64::MAX);
        stdout.take(limit).read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= LOGIN_ENVIRONMENT_DEADLINE {
            drop(child);
            let _ = reader.join();
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "the login profile did not finish in time",
            )
            .with_context(format!(
                "deadline {} ms",
                LOGIN_ENVIRONMENT_DEADLINE.as_millis()
            ))
            .with_help("Check /etc/profile and ~/.profile for commands that wait for input"));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let bytes = reader
        .join()
        .map_err(|_| {
            ShellError::new(
                ErrorCode::Io,
                "the login environment reader stopped unexpectedly",
            )
            .with_help("Quirl continues with the environment it was started with")
        })?
        .map_err(|error| {
            ShellError::new(ErrorCode::Io, "could not read the login environment")
                .with_context(error.to_string())
                .with_help("Quirl continues with the environment it was started with")
        })?;
    if !status.success() {
        return Err(ShellError::new(ErrorCode::Io, "the login profile failed")
            .with_context(status.to_string())
            .with_help("Fix the error /bin/sh reported in /etc/profile or ~/.profile"));
    }
    parse_environment(&bytes)
}

/// Parse `NAME=VALUE\0` records, rejecting oversized or malformed output.
fn parse_environment(bytes: &[u8]) -> Result<Vec<(OsString, OsString)>, ShellError> {
    let malformed = || {
        ShellError::new(ErrorCode::Io, "the login environment was malformed")
            .with_help("Quirl continues with the environment it was started with")
    };
    if bytes.len() > LOGIN_ENVIRONMENT_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "the login environment exceeds its limit",
        )
        .with_context(format!("limit {LOGIN_ENVIRONMENT_BYTES_MAX} bytes"))
        .with_help("Export fewer or shorter variables from your login profile"));
    }
    if bytes.last().is_some_and(|byte| *byte != 0) {
        return Err(malformed());
    }
    let mut environment = Vec::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let separator = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(malformed)?;
        let (name, value) = record.split_at(separator);
        let value = value.get(1..).ok_or_else(malformed)?;
        if matches!(name, b"_" | b"SHLVL") {
            continue;
        }
        environment.push((os_string_from_bytes(name), os_string_from_bytes(value)));
    }
    Ok(environment)
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse(argv0: &str, values: &[&str]) -> Option<PosixInvocation> {
        PosixInvocation::parse(OsStr::new(argv0), &arguments(values))
    }

    #[test]
    fn quirl_subcommands_and_help_keep_the_quirl_interface() {
        assert_eq!(parse("quirl", &[]), None);
        assert_eq!(parse("quirl", &["data", "ls"]), None);
        assert_eq!(parse("/usr/bin/quirl", &["--help"]), None);
        assert_eq!(parse("quirl", &["-V"]), None);
        assert_eq!(parse("quirl", &["-cx"]), None);
    }

    #[test]
    fn ssh_style_commands_keep_their_name_and_arguments() {
        let invocation = parse("quirl", &["-c", "scp -t /tmp", "name", "a b"]).unwrap();
        assert_eq!(invocation.command, Some(OsString::from("scp -t /tmp")));
        assert_eq!(invocation.operands, arguments(&["name", "a b"]));
        assert_eq!(
            invocation.action(false).unwrap(),
            PosixAction::DelegateToShell
        );
        assert_eq!(
            invocation.shell_arguments(),
            arguments(&["-c", "scp -t /tmp", "name", "a b"])
        );
    }

    #[test]
    fn editor_environment_probes_pass_login_through_and_ignore_interactive() {
        for flags in [
            &["-ilc"][..],
            &["-i", "-l", "-c"],
            &["-l", "-i", "-c"],
            &["--login", "-ic"],
        ] {
            let mut values = flags.to_vec();
            values.push("env");
            let invocation = parse("quirl", &values).unwrap();
            assert!(invocation.login, "{flags:?}");
            assert_eq!(
                invocation.action(true).unwrap(),
                PosixAction::DelegateToShell,
                "{flags:?}"
            );
            assert_eq!(
                invocation.shell_arguments(),
                arguments(&["-l", "-c", "env"])
            );
        }
    }

    #[test]
    fn login_argv0_starts_an_interactive_login_session_at_a_terminal() {
        let invocation = parse("-quirl", &[]).unwrap();
        assert_eq!(
            invocation.action(true).unwrap(),
            PosixAction::Interactive { login: true }
        );
        let invocation = parse("quirl", &["-l"]).unwrap();
        assert_eq!(
            invocation.action(true).unwrap(),
            PosixAction::Interactive { login: true }
        );
        let invocation = parse("quirl", &["-i"]).unwrap();
        assert_eq!(
            invocation.action(false).unwrap(),
            PosixAction::Interactive { login: false }
        );
    }

    #[test]
    fn piped_scripts_run_in_the_posix_shell() {
        let invocation = parse("-quirl", &[]).unwrap();
        assert_eq!(
            invocation.action(false).unwrap(),
            PosixAction::DelegateToShell
        );
        assert_eq!(invocation.shell_arguments(), arguments(&["-l", "-s"]));
        let invocation = parse("quirl", &["-s", "one", "two"]).unwrap();
        assert_eq!(
            invocation.shell_arguments(),
            arguments(&["-s", "one", "two"])
        );
    }

    #[test]
    fn double_dash_ends_options_before_operands() {
        let invocation = parse("quirl", &["-c", "--", "echo $0", "-name"]).unwrap();
        assert_eq!(invocation.command, Some(OsString::from("echo $0")));
        assert_eq!(invocation.operands, arguments(&["-name"]));
    }

    #[test]
    fn a_command_flag_without_a_command_explains_its_usage() {
        for values in [&["-c"][..], &["-lc"], &["-c", "--"]] {
            let error = parse("quirl", values).unwrap().action(false).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidArgument, "{values:?}");
        }
        assert!(
            parse("quirl", &["-c", "true"])
                .unwrap()
                .action(false)
                .is_ok()
        );
    }

    #[test]
    fn login_environments_parse_and_drop_shell_bookkeeping() {
        let parsed =
            parse_environment(b"PATH=/usr/bin:/bin\0SHLVL=1\0_=/usr/bin/env\0A=x=y\0").unwrap();
        assert_eq!(
            parsed,
            vec![
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (OsString::from("A"), OsString::from("x=y")),
            ]
        );
        assert!(parse_environment(b"PATH=/bin").is_err());
        assert!(parse_environment(b"NOEQUALS\0").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn delegated_commands_behave_exactly_like_sh() {
        let invocation = parse(
            "quirl",
            &["-c", "printf '%s|' \"$0\" \"$@\"; exit 7", "n", "a b"],
        )
        .unwrap();
        let output = std::process::Command::new(POSIX_SHELL_PATH)
            .args(invocation.shell_arguments())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"n|a b|");
    }

    #[cfg(unix)]
    #[test]
    fn the_login_environment_is_captured_from_a_login_shell() {
        let environment = capture_login_environment().unwrap();
        assert!(environment.iter().any(|(name, _)| name == "PATH"));
    }
}
