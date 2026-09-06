//! Real-Git managed-clone journeys under private profiles and local-only remotes.
//!
//! Failure model: a rendered proposal is not clone execution, navigation must
//! retain unfinished typing, and existing files must survive reuse/conflicts.
//! Each journey owns its PTY and a 120-second deadline. Private Git URL rewrites
//! map every public-looking fixture URL to an empty local bare repository; no
//! network, credentials, host Git configuration, or personal checkout is used.

use super::{
    STARTUP_MARKER, Session, SessionOptions, TaskError, default_timeout, ensure_status,
    ensure_terminal_restored, find_on_path, key, read_bounded_fixture, shell_quote,
    wait_for_rich_input_until, write_executable,
};
use std::{
    fs, io,
    io::Write,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const REMOTE_PREFIX: &str = "https://clone.example/team/";
const DIALOG: &str = "Keep your Git projects organized?";
const FIXTURE_BYTES_MAX: usize = 4096;

struct Journey {
    session: Session,
    deadline: Instant,
}

impl Journey {
    fn new(binary: &Path, repositories: &[&str]) -> Result<Self, TaskError> {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(120))
            .ok_or_else(|| io::Error::other("clone journey deadline overflow"))?;
        let mut session = Session::new(binary, SessionOptions::default())?;
        session.pty.wait_for(STARTUP_MARKER)?;
        let remotes = session.private.path.join("remotes");
        fs::create_dir(&remotes)?;
        for repository in repositories {
            symlink("source.git", remotes.join(repository))?;
        }
        // Git reads this private HOME after startup. The shell's native executor
        // inherited that same HOME, including its bounded root-resolution probes.
        let local_prefix = format!("file://{}/", remotes.display());
        let escaped = local_prefix.replace('\\', "\\\\").replace('"', "\\\"");
        fs::write(
            session.private.path.join(".gitconfig"),
            format!(
                "[ghq]\n root = ~/Managed\n[url \"{escaped}\"]\n insteadOf = {REMOTE_PREFIX}\n[protocol \"file\"]\n allow = always\n"
            ),
        )?;
        let mut journey = Self { session, deadline };
        journey.run("export GIT_ALLOW_PROTOCOL=file GIT_CONFIG_NOSYSTEM=1")?;
        journey.run("git init --bare remotes/source.git")?;
        journey.assert_file("remotes/source.git/HEAD", b"ref: refs/heads/")?;
        Ok(journey)
    }

    fn run(&mut self, command: &str) -> Result<(), TaskError> {
        let start = self.submit(command)?;
        self.wait_input(start)
    }

    fn submit(&mut self, command: &str) -> Result<usize, TaskError> {
        self.check_deadline()?;
        let start = self.session.pty.output().len();
        self.session.pty.type_text(command)?;
        self.session.pty.send(key::ENTER)?;
        Ok(start)
    }

    fn suggest(&mut self, repository: &str) -> Result<usize, TaskError> {
        let start = self.submit(&format!("git clone {REMOTE_PREFIX}{repository}"))?;
        self.wait_screen(DIALOG)?;
        let destination = format!("/Managed/clone.example/team/{repository}");
        let joined: String = self
            .session
            .pty
            .screen()
            .lines()
            .iter()
            .map(|line| line.trim())
            .collect();
        if !joined.contains(&destination) {
            return Err(
                io::Error::other("clone dialog omitted the exact managed destination").into(),
            );
        }
        Ok(start)
    }

    fn select(&mut self, start: usize, down_count: usize) -> Result<(), TaskError> {
        for _ in 0..down_count {
            self.session.pty.send(b"\x1b[B")?;
        }
        self.session.pty.send(key::ENTER)?;
        self.wait_input(start)
    }

    fn wait_input(&mut self, start: usize) -> Result<(), TaskError> {
        let deadline = self.step_deadline()?;
        wait_for_rich_input_until(&mut self.session, start, deadline)
    }

    fn step_deadline(&self) -> Result<Instant, TaskError> {
        Ok(Instant::now()
            .checked_add(default_timeout())
            .ok_or_else(|| io::Error::other("clone step deadline overflow"))?
            .min(self.deadline))
    }

    fn wait_screen(&mut self, expected: &str) -> Result<(), TaskError> {
        let deadline = self.step_deadline()?;
        loop {
            let screen = self.session.pty.screen();
            let text = screen.text();
            // The modal hides its cursor, so the editor's cursor-finalization
            // marker cannot complete it. Its final footer plus raw input proves
            // the entire dedicated choice frame is visible and ready instead.
            let ready = if expected == DIALOG {
                text.contains("Up/Down Tab Enter Esc PgUp/PgDn")
                    && !self.session.pty.terminal_modes()?.local_flags.intersects(
                        nix::sys::termios::LocalFlags::ICANON | nix::sys::termios::LocalFlags::ECHO,
                    )
            } else {
                screen.has_completed_frame()
            };
            if ready && text.contains(expected) {
                return Ok(());
            }
            self.check_deadline()?;
            if Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "clone journey did not display {expected:?}; screen: {text}"
                ))
                .into());
            }
            self.session.pty.drain_for(Duration::from_millis(16))?;
        }
    }

    fn check_deadline(&self) -> Result<(), TaskError> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::other(format!(
                "managed clone journey exceeded 120 seconds; screen: {}",
                self.session.pty.screen().text()
            ))
            .into());
        }
        Ok(())
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.session.private.path.join(relative)
    }

    fn managed(&self, repository: &str) -> PathBuf {
        self.path(&format!("Managed/clone.example/team/{repository}"))
    }

    fn assert_checkout(&self, path: &Path) -> Result<(), TaskError> {
        let config = read_bounded_fixture(&path.join(".git/config"), FIXTURE_BYTES_MAX)?;
        if !path.join(".git/HEAD").is_file()
            || !config
                .windows(REMOTE_PREFIX.len())
                .any(|window| window == REMOTE_PREFIX.as_bytes())
        {
            return Err(io::Error::other(format!(
                "missing real cloned repository at {}",
                path.display()
            ))
            .into());
        }
        Ok(())
    }

    fn assert_file(&self, relative: &str, expected_prefix: &[u8]) -> Result<(), TaskError> {
        let actual = read_bounded_fixture(&self.path(relative), FIXTURE_BYTES_MAX)?;
        if !actual.starts_with(expected_prefix) {
            return Err(
                io::Error::other(format!("clone fixture {relative} differed: {actual:?}")).into(),
            );
        }
        Ok(())
    }

    fn snapshot(&self, label: &str) -> Result<(), TaskError> {
        let Some(directory) = std::env::var_os("QUIRL_PROJECT_CLONE_ARTIFACT_DIR") else {
            return Ok(());
        };
        if directory.as_encoded_bytes().len() > 4096 {
            return Err(io::Error::other("clone artifact directory exceeds 4096 bytes").into());
        }
        let directory = PathBuf::from(directory);
        if !directory.is_absolute() || !directory.is_dir() {
            return Err(io::Error::other(
                "clone artifact directory must already exist and be absolute",
            )
            .into());
        }
        // Labels are fixed by this fixture. Create-new files prevent a requested
        // capture from overwriting earlier evidence or unrelated user files.
        for (extension, contents) in [
            ("svg", self.session.pty.screen().to_svg()?),
            ("screen.txt", self.session.pty.screen().text()),
        ] {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(format!("{label}.{extension}")))?;
            file.write_all(contents.as_bytes())?;
        }
        Ok(())
    }

    fn assert_policy(&self, expected: &str) -> Result<(), TaskError> {
        let bytes = read_bounded_fixture(
            &self.path("state/quirl/clone-policy.json"),
            FIXTURE_BYTES_MAX,
        )?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if value.get("policy").and_then(serde_json::Value::as_str) != Some(expected) {
            return Err(
                io::Error::other(format!("expected clone policy {expected}: {value}")).into(),
            );
        }
        Ok(())
    }

    fn finish(mut self, label: &str, expected_status: i32) -> Result<(), TaskError> {
        self.check_deadline()?;
        let start = self.session.pty.output().len();
        self.session.pty.send(key::CTRL_D)?;
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        ensure_status(
            self.session.pty.wait_exit_within(remaining)?,
            expected_status,
            label,
        )?;
        ensure_terminal_restored(&self.session, start, label)
    }
}

/// Cancel safely, retain default clone behavior, and persist the one-time dismissal.
pub(super) fn check_project_clone_default(binary: &Path) -> Result<(), TaskError> {
    let mut journey = Journey::new(binary, &["cancelled", "original", "subsequent"])?;
    check_metadata_cancellation(&mut journey)?;
    let start = journey.suggest("cancelled")?;
    journey.session.pty.send(key::CTRL_C)?;
    journey.wait_input(start)?;
    for path in [
        journey.path("cancelled"),
        journey.managed("cancelled"),
        journey.path("state/quirl/clone-policy.json"),
    ] {
        if path.exists() {
            return Err(io::Error::other("cancelled clone changed files or preference").into());
        }
    }
    let start = journey.suggest("original")?;
    journey.select(start, 0)?;
    journey.assert_checkout(&journey.path("original"))?;
    journey.assert_policy("off")?;
    journey.run(&format!("git clone {REMOTE_PREFIX}subsequent"))?;
    journey.assert_checkout(&journey.path("subsequent"))?;
    if journey.managed("original").exists() || journey.managed("subsequent").exists() {
        return Err(io::Error::other("default clone was silently redirected").into());
    }
    journey.finish("managed clone default and cancellation", 0)
}

fn check_metadata_cancellation(journey: &mut Journey) -> Result<(), TaskError> {
    let real_git =
        find_on_path("git").ok_or_else(|| io::Error::other("clone fixture requires Git"))?;
    let original_path = journey
        .session
        .spawn
        .environment
        .get(std::ffi::OsStr::new("PATH"))
        .ok_or_else(|| io::Error::other("private clone fixture PATH is missing"))?
        .clone();
    let bin = journey.path("metadata-bin");
    fs::create_dir(&bin)?;
    let ready = journey.path("metadata-ready");
    let forbidden = journey.path("forbidden-clone");
    write_executable(
        &bin.join("git"),
        &format!(
            r#"#!/bin/sh
if [ "$1" = config ] && [ "$2" = --null ] && [ "$3" = --path ] && [ "$4" = --get-all ] && [ "$5" = ghq.root ]; then
  printf '%s' METADATA_READY > {ready}
  /bin/sleep 30
  exit 74
fi
if [ "$1" = clone ]; then
  printf '%s' UNAUTHORIZED_FALLBACK > {forbidden}
  exit 73
fi
exec {git} "$@"
"#,
            ready = shell_quote(&ready),
            forbidden = shell_quote(&forbidden),
            git = shell_quote(&real_git)
        ),
    )?;
    journey.run(&format!("export PATH={}:$PATH", shell_quote(&bin)))?;
    let start = journey.submit(&format!("git clone {REMOTE_PREFIX}metadata"))?;
    let deadline = journey.step_deadline()?;
    while !ready.exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "clone metadata probe did not start: {}",
                journey.session.pty.screen().text()
            ))
            .into());
        }
        journey.session.pty.drain_for(Duration::from_millis(16))?;
    }
    journey.assert_file("metadata-ready", b"METADATA_READY")?;
    journey.session.pty.send(key::CTRL_C)?;
    journey.wait_input(start)?;
    for path in [
        forbidden,
        journey.path("metadata"),
        journey.managed("metadata"),
        journey.path("state/quirl/clone-policy.json"),
    ] {
        if path.exists() {
            return Err(io::Error::other(format!(
                "cancelled metadata probe mutated {}; contents={:?}; screen={}",
                path.display(),
                read_bounded_fixture(&path, FIXTURE_BYTES_MAX).ok(),
                journey.session.pty.screen().text()
            ))
            .into());
        }
    }
    journey.run("printf '%s' $? > metadata-status")?;
    journey.assert_file("metadata-status", b"130")?;
    journey.run(&format!(
        "export PATH={}",
        shell_quote(&PathBuf::from(original_path))
    ))?;
    journey.run("printf '%s' METADATA_CANCELLED_OK > metadata-cancel-proof")?;
    journey.assert_file("metadata-cancel-proof", b"METADATA_CANCELLED_OK")
}

/// Clone once, immediately navigate with retained typing, and reuse without Git mutation.
pub(super) fn check_project_clone_navigation(binary: &Path) -> Result<(), TaskError> {
    let mut journey = Journey::new(binary, &["once", "ordinary", "conflict"])?;
    let start = journey.suggest("once")?;
    journey.snapshot("clone-chooser")?;
    journey.select(start, 1)?;
    let checkout = journey.managed("once");
    journey.assert_checkout(&checkout)?;
    journey.assert_policy("off")?;
    journey.wait_screen("Alt-Q u open project")?;
    journey.snapshot("clone-open-offer")?;
    // The Open action must return the current editor text, rather than replacing
    // it with a cd command or executing it during the directory transition.
    journey
        .session
        .pty
        .type_text("printf '%s' preserved > open-proof")?;
    journey.session.pty.send(key::ALT_Q)?;
    journey.wait_screen("Alt-Q · Quirl")?;
    let start = journey.session.pty.output().len();
    journey.session.pty.send(b"u")?;
    journey.wait_input(start)?;
    journey.wait_screen("printf '%s' preserved > open-proof")?;
    journey.snapshot("clone-open-restored")?;
    if checkout.join("open-proof").exists() || journey.path("open-proof").exists() {
        return Err(io::Error::other("project navigation executed unfinished input").into());
    }
    let start = journey.session.pty.output().len();
    journey.session.pty.send(key::ENTER)?;
    journey.wait_input(start)?;
    let actual = read_bounded_fixture(&checkout.join("open-proof"), FIXTURE_BYTES_MAX)?;
    if actual != b"preserved" || journey.path("open-proof").exists() {
        return Err(io::Error::other("Open project lost typing or failed to change cwd").into());
    }
    journey.run(&format!("git clone {REMOTE_PREFIX}ordinary"))?;
    journey.assert_checkout(&checkout.join("ordinary"))?;
    if journey.managed("ordinary").exists() {
        return Err(io::Error::other("clone-once enabled future managed clones").into());
    }
    fs::write(checkout.join(".git/FETCH_HEAD"), b"DO_NOT_FETCH\n")?;
    let config = read_bounded_fixture(&checkout.join(".git/config"), FIXTURE_BYTES_MAX)?;
    journey.run(&format!("quirl projects clone {REMOTE_PREFIX}once"))?;
    journey.wait_screen("Alt-Q u open project")?;
    if read_bounded_fixture(&checkout.join(".git/FETCH_HEAD"), FIXTURE_BYTES_MAX)?
        != b"DO_NOT_FETCH\n"
        || read_bounded_fixture(&checkout.join(".git/config"), FIXTURE_BYTES_MAX)? != config
    {
        return Err(
            io::Error::other("existing checkout reuse fetched or changed its origin").into(),
        );
    }
    let conflict = journey.managed("conflict");
    fs::create_dir(&conflict)?;
    fs::write(conflict.join("keep"), b"USER_DATA\n")?;
    journey.run(&format!("quirl projects clone {REMOTE_PREFIX}conflict"))?;
    if read_bounded_fixture(&conflict.join("keep"), FIXTURE_BYTES_MAX)? != b"USER_DATA\n"
        || conflict.join(".git").exists()
    {
        return Err(io::Error::other("conflicting project destination was modified").into());
    }
    journey.finish("managed clone navigation and reuse", 1)
}

/// Remember explicit opt-in while leaving destinations, flags, redirects, and lists alone.
pub(super) fn check_project_clone_always(binary: &Path) -> Result<(), TaskError> {
    let mut journey = Journey::new(
        binary,
        &[
            "first", "second", "explicit", "options", "compound", "redirect",
        ],
    )?;
    let start = journey.suggest("first")?;
    journey.select(start, 2)?;
    journey.assert_checkout(&journey.managed("first"))?;
    journey.assert_policy("managed")?;
    journey.run(&format!("git clone {REMOTE_PREFIX}second"))?;
    journey.assert_checkout(&journey.managed("second"))?;
    for command in [
        format!("git clone {REMOTE_PREFIX}explicit scratch"),
        format!("git clone --depth 1 {REMOTE_PREFIX}options"),
        format!("git clone {REMOTE_PREFIX}compound && printf '%s' CLONE_LIST_OK > list-proof"),
        format!("git clone {REMOTE_PREFIX}redirect > clone-output"),
    ] {
        journey.run(&command)?;
    }
    for directory in ["scratch", "options", "compound", "redirect"] {
        journey.assert_checkout(&journey.path(directory))?;
    }
    journey.assert_file("list-proof", b"CLONE_LIST_OK")?;
    for repository in ["explicit", "options", "compound", "redirect"] {
        if journey.managed(repository).exists() {
            return Err(
                io::Error::other("managed clone intercepted an explicit Git operation").into(),
            );
        }
    }
    journey.assert_policy("managed")?;
    journey.finish("managed clone remembered preference and bypasses", 0)
}
