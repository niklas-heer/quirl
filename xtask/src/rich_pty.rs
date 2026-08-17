//! End-to-end rich-terminal checks driven by the Rust PTY harness.

use crate::pty::{key, PtySession, SpawnOptions, VirtualScreen, DEFAULT_TIMEOUT};
use nix::{
    errno::Errno,
    sys::{signal::kill, stat::Mode, termios::LocalFlags},
    unistd::{mkfifo, Pid},
};
use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fs::{self, DirBuilder, File},
    io::{self, BufWriter, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const STARTUP_MARKER: &[u8] = b"Tab complete";
const CHECK_NAMES: &[&str] = &[
    "rich-editing",
    "mode-switch-and-palette-screen",
    "deferred-catalog-admission",
    "catalog-failure-restores-terminal",
    "completion",
    "interactive-runtime",
    "rich-review-regressions",
    "native-job-control",
    "noninteractive-dialect-islands",
    "suspend-resume",
    "fallbacks",
    "no-color-preserves-semantic-hints",
];
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

type Check = fn(&Path) -> Result<(), Box<dyn Error>>;

struct CheckCase {
    name: &'static str,
    run: Check,
}

#[derive(Default)]
struct SessionOptions {
    term: Option<&'static str>,
    shell: Option<PathBuf>,
    symbols: Option<&'static str>,
    semantic_hints: Option<bool>,
    no_color: bool,
    catalog_gate: bool,
    catalog_failure: bool,
    redirect_stderr: bool,
    rows: Option<usize>,
    columns: Option<usize>,
}

struct Session {
    pty: PtySession,
    private: TempDirectory,
    catalog_gate: PathBuf,
    catalog_gate_reached: PathBuf,
}

impl Session {
    fn new(binary: &Path, options: SessionOptions) -> Result<Self, Box<dyn Error>> {
        let private = TempDirectory::new("quirl-pty")?;
        let config_dir = private.path.join("config");
        create_private_directory(&config_dir)?;
        let temporary_dir = private.path.join("tmp");
        create_private_directory(&temporary_dir)?;
        let semantic_hints = options.semantic_hints.unwrap_or(true);
        let symbols = options.symbols.unwrap_or("plain");
        fs::write(
            config_dir.join("config.lua"),
            format!(
                r#"---@type quirl.Config
return quirl.config {{
  schema_version = 3,
  editor = {{ keymap = "emacs", semantic_hints = {semantic_hints}, banner = "none" }},
  picker = {{ layout = "adaptive", preview = true }},
  prompt = {{
    symbols = "{symbols}",
    left = {{ "directory" }},
    right = {{ "duration", "status" }},
    transient = false,
  }},
  ui = {{ theme = "tokyo-night", surface = "rich", statusline = {{ hints = true }} }},
  completion = {{ auto = false, min_chars = 2 }},
}}
"#
            ),
        )?;

        let path = env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));
        let mut environment = BTreeMap::from([
            (
                OsString::from("HOME"),
                private.path.clone().into_os_string(),
            ),
            (OsString::from("PATH"), path),
            (
                OsString::from("TERM"),
                OsString::from(options.term.unwrap_or("xterm-256color")),
            ),
            (
                OsString::from("LC_ALL"),
                OsString::from(if cfg!(target_os = "macos") {
                    "en_US.UTF-8"
                } else {
                    "C.UTF-8"
                }),
            ),
            (
                OsString::from("TMPDIR"),
                temporary_dir.clone().into_os_string(),
            ),
            (
                OsString::from("QUIRL_CONFIG_DIR"),
                config_dir.clone().into_os_string(),
            ),
            (
                OsString::from("QUIRL_HISTORY"),
                private.path.join("history").into_os_string(),
            ),
            (
                OsString::from("QUIRL_PLUGIN_HOME"),
                private.path.join("plugins").into_os_string(),
            ),
            (
                OsString::from("QUIRL_INDEX_DIR"),
                private.path.join("index").into_os_string(),
            ),
            (
                OsString::from("QUIRL_RECOVERY_DIR"),
                private.path.join("recovery").into_os_string(),
            ),
            (
                OsString::from("XDG_CACHE_HOME"),
                private.path.join("cache").into_os_string(),
            ),
            (
                OsString::from("XDG_CONFIG_HOME"),
                private.path.join("xdg-config").into_os_string(),
            ),
            (
                OsString::from("XDG_DATA_HOME"),
                private.path.join("data").into_os_string(),
            ),
            (
                OsString::from("XDG_STATE_HOME"),
                private.path.join("state").into_os_string(),
            ),
        ]);
        let catalog_gate = private.path.join("catalog-admission.gate");
        let catalog_gate_reached = PathBuf::from(format!("{}.reached", catalog_gate.display()));
        if options.catalog_gate {
            environment.insert(
                OsString::from("QUIRL_TEST_CATALOG_GATE"),
                catalog_gate.clone().into_os_string(),
            );
        }
        if options.catalog_failure {
            environment.insert(
                OsString::from("QUIRL_TEST_CATALOG_FAILURE"),
                OsString::from("1"),
            );
        }
        if options.no_color {
            environment.insert(OsString::from("NO_COLOR"), OsString::from("1"));
        }

        let argv = if let Some(shell) = options.shell.as_ref() {
            if shell.file_name().is_some_and(|name| name == "zsh") {
                vec![shell.as_os_str().to_owned(), OsString::from("-f")]
            } else {
                vec![
                    shell.as_os_str().to_owned(),
                    OsString::from("--noprofile"),
                    OsString::from("--norc"),
                    OsString::from("-i"),
                ]
            }
        } else {
            vec![binary.as_os_str().to_owned()]
        };
        let mut spawn = SpawnOptions::new(argv, private.path.clone());
        spawn.environment = environment;
        spawn.rows = options.rows.unwrap_or(30);
        spawn.columns = options.columns.unwrap_or(120);
        spawn.stderr_path = options.redirect_stderr.then(|| private.path.join("stderr"));
        let pty = PtySession::spawn(spawn)?;
        Ok(Self {
            pty,
            private,
            catalog_gate,
            catalog_gate_reached,
        })
    }
}

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(label: &str) -> io::Result<Self> {
        for _ in 0..64 {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!("{label}-{}-{id}", std::process::id()));
            match create_private_directory(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique PTY test directory after 64 attempts",
        ))
    }
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.mode(0o700).create(path)
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(super) fn run(_root: &Path, binary: &Path, selected: &[String]) -> Result<(), Box<dyn Error>> {
    if !binary.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "missing Quirl binary: {}; run cargo build -p quirl-cli",
                binary.display()
            ),
        )
        .into());
    }
    for name in selected {
        if !CHECK_NAMES.contains(&name.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "unknown rich PTY check {name:?}; expected one of {}",
                    CHECK_NAMES.join(", ")
                ),
            )
            .into());
        }
    }

    for check in checks() {
        if !selected.is_empty() && !selected.iter().any(|name| name == check.name) {
            continue;
        }
        (check.run)(binary)?;
        println!("ok: check_{}", check.name.replace('-', "_"));
    }
    Ok(())
}

fn checks() -> [CheckCase; 12] {
    [
        CheckCase {
            name: "rich-editing",
            run: check_rich_editing,
        },
        CheckCase {
            name: "mode-switch-and-palette-screen",
            run: check_mode_switch_and_palette_screen,
        },
        CheckCase {
            name: "deferred-catalog-admission",
            run: check_deferred_catalog_admission,
        },
        CheckCase {
            name: "catalog-failure-restores-terminal",
            run: check_catalog_failure_restores_terminal,
        },
        CheckCase {
            name: "completion",
            run: check_completion,
        },
        CheckCase {
            name: "interactive-runtime",
            run: check_interactive_runtime,
        },
        CheckCase {
            name: "rich-review-regressions",
            run: check_rich_review_regressions,
        },
        CheckCase {
            name: "native-job-control",
            run: check_native_job_control,
        },
        CheckCase {
            name: "noninteractive-dialect-islands",
            run: check_noninteractive_dialect_islands,
        },
        CheckCase {
            name: "suspend-resume",
            run: check_suspend_resume,
        },
        CheckCase {
            name: "fallbacks",
            run: check_fallbacks,
        },
        CheckCase {
            name: "no-color-preserves-semantic-hints",
            run: check_no_color_preserves_semantic_hints,
        },
    ]
}

fn enter_and_wait(
    session: &mut Session,
    command: &str,
    marker: &[u8],
) -> Result<Vec<u8>, Box<dyn Error>> {
    session.pty.type_text(command)?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(marker)
}

fn wait_for_prompt(session: &mut Session) -> Result<(), Box<dyn Error>> {
    let child = session
        .pty
        .child_pid()
        .ok_or_else(|| io::Error::other("Quirl exited before returning to the prompt"))?;
    let tail_start = session.pty.output().len().saturating_sub(2_000);
    let owns_prompt = session
        .pty
        .foreground_group()
        .is_ok_and(|group| group == child);
    if !owns_prompt || !contains(&session.pty.output()[tail_start..], STARTUP_MARKER) {
        session.pty.wait_for(STARTUP_MARKER)?;
    }
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while session.pty.foreground_group()? != child && Instant::now() < deadline {
        session.pty.drain_for(Duration::from_millis(10))?;
    }
    if session.pty.foreground_group()? != child {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Quirl did not recover terminal ownership at the prompt",
        )
        .into());
    }
    Ok(())
}

fn check_rich_editing(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.type_text("/usr/bin/printf BACKSPACE_BAD")?;
    session.pty.send(b"\x7f\x7f\x7f")?;
    enter_and_wait(&mut session, "OK", b"BACKSPACE_OK")?;
    session.pty.type_text("/usr/bin/printf DELETE_XOK")?;
    session.pty.send(b"\x1b[D\x1b[D\x1b[D\x1b[3~\r")?;
    session.pty.wait_for(b"DELETE_OK")?;
    session.pty.type_text("/usr/bin/printf UNICODE_e\u{301}")?;
    session.pty.send(b"\x7f")?;
    enter_and_wait(&mut session, "OK", b"UNICODE_OK")?;
    session.pty.type_text("/usr/bin/printf CTRLD_XOK")?;
    session.pty.send(b"\x1b[D\x1b[D\x1b[D\x04\r")?;
    session.pty.wait_for(b"CTRLD_OK")?;
    session.pty.type_text("/usr/bin/printf SHOULD_NOT_RUN")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    enter_and_wait(&mut session, "/usr/bin/printf AFTER_CTRLC", b"AFTER_CTRLC")?;
    session.pty.send(key::ALT_M)?;
    session
        .pty
        .wait_for_screen("rich data-mode status", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.starts_with("data | Alt-M mode"))
        })?;
    session.pty.send(key::ALT_M)?;
    session
        .pty
        .wait_for_screen("rich command-mode status", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.starts_with("command | Alt-M mode"))
        })?;
    session.pty.type_text("/usr/bin/printf 'MULTI_ONE")?;
    session.pty.send(key::ENTER)?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.type_text("_TWO'")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"MULTI_ONE\r\n_TWO")?;
    enter_and_wait(
        &mut session,
        "/bin/sh -c 'printf STDOUT_OK; printf STDERR_OK >&2'",
        b"STDERR_OK",
    )?;
    if !contains(session.pty.output(), b"STDOUT_OK") {
        return Err(
            io::Error::other("interactive command stdout was not handed back to the PTY").into(),
        );
    }
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, "Ctrl-D")
}

fn check_mode_switch_and_palette_screen(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(18),
            columns: Some(78),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    if !session
        .pty
        .screen()
        .bottom_line()
        .starts_with("command | Alt-M mode")
    {
        return Err(screen_error(
            "initial rich status was not anchored at the terminal bottom",
            session.pty.screen(),
        ));
    }
    session.pty.type_text("echo MODE_BUFFER_RETAINED")?;
    session.pty.wait_for_screen_text("MODE_BUFFER_RETAINED")?;
    for (expected_mode, expected_indicator) in [
        ("data", "D echo MODE_BUFFER_RETAINED"),
        ("command", "> echo MODE_BUFFER_RETAINED"),
        ("data", "D echo MODE_BUFFER_RETAINED"),
    ] {
        let output_start = session.pty.output().len();
        session.pty.send(key::ALT_M)?;
        session.pty.wait_for_screen(
            &format!("bottom-anchored {expected_mode} mode frame"),
            |screen| {
                screen
                    .bottom_line()
                    .starts_with(&format!("{expected_mode} | Alt-M mode"))
            },
        )?;
        let emitted = &session.pty.output()[output_start..];
        if contains(emitted, b"mode:") || contains(emitted, b"mode ->") {
            return Err(io::Error::other(format!(
                "Alt-M emitted feedback into scrollback; output={emitted:?}"
            ))
            .into());
        }
        if !session.pty.screen().text().contains(expected_indicator) {
            return Err(screen_error(
                &format!("Alt-M discarded active editor buffer; expected {expected_indicator:?}"),
                session.pty.screen(),
            ));
        }
    }
    session.pty.send(key::CTRL_K)?;
    session
        .pty
        .wait_for_screen("bottom-anchored Ctrl-K palette", |screen| {
            let bottom = screen.bottom_line();
            bottom.starts_with("data |")
                && bottom.contains("results (picker)")
                && screen.text().contains("picker")
        })?;
    if !session.pty.screen().text().contains("git status") {
        return Err(screen_error(
            "Ctrl-K palette did not render catalog commands",
            session.pty.screen(),
        ));
    }
    session.pty.send(key::ESCAPE)?;
    session
        .pty
        .wait_for_screen("compact prompt after palette dismissal", |screen| {
            let text = screen.text();
            screen.bottom_line().starts_with("data | Alt-M mode")
                && !text.contains("picker")
                && !text.contains("results (picker)")
        })?;
    let lines = session.pty.screen().lines();
    let compact = &lines[lines.len().saturating_sub(3)..];
    if compact
        .get(1)
        .is_none_or(|line| !line.contains("MODE_BUFFER_RETAINED"))
    {
        return Err(io::Error::other(format!(
            "palette dismissal did not restore bottom editor; bottom_rows={compact:?}"
        ))
        .into());
    }
    session.pty.send(key::CTRL_U)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, "screen-state session")
}

fn check_completion(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.type_text("git st")?;
    session.pty.send(b"\t")?;
    session.pty.wait_for(b"git status [--short]")?;
    session.pty.send(key::ESCAPE)?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    session.pty.type_text("git st")?;
    session.pty.send(b"\t")?;
    session.pty.wait_for(b"git status [--short]")?;
    session.pty.send(key::ENTER)?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"not a git repository")?;
    session.pty.type_text("git")?;
    session.pty.send(b"\x1b[Z")?;
    session.pty.wait_for(b"picker")?;
    session.pty.send(b"\x1b[200~zzzz-no-match\x1b[201~")?;
    session.pty.wait_for(b"zzzz-no-match")?;
    session.pty.send(key::ESCAPE)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    session.pty.send(key::CTRL_D)?;
    session.pty.wait_exit()?;
    Ok(())
}

fn check_deferred_catalog_admission(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            catalog_gate: true,
            ..SessionOptions::default()
        },
    )?;
    let gate_reached = session.catalog_gate_reached.clone();
    wait_for_file(&mut session, gate_reached)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    if !contains(session.pty.output(), STARTUP_MARKER) {
        return Err(io::Error::other("catalog loader ran before first frame flush").into());
    }
    if session
        .pty
        .terminal_modes()?
        .local_flags
        .intersects(LocalFlags::ICANON | LocalFlags::ECHO)
    {
        return Err(io::Error::other("catalog gate did not run inside owned raw mode").into());
    }
    session.pty.resize(4, 40)?;
    session
        .pty
        .send(b"\x1b[200~/usr/bin/printf QUEUED_AFTER_ADMISSION\x1b[201~\r")?;
    session.pty.drain_for(Duration::from_millis(150))?;
    if contains(session.pty.output(), b"QUEUED_AFTER_ADMISSION") {
        return Err(
            io::Error::other("terminal input was consumed before catalog publication").into(),
        );
    }
    fs::write(&session.catalog_gate, b"release\n")?;
    session.pty.wait_for(b"QUEUED_AFTER_ADMISSION")?;
    session.pty.resize(30, 120)?;
    wait_for_prompt(&mut session)?;
    session.pty.type_text("git st")?;
    session.pty.send(b"\t")?;
    session.pty.wait_for(b"git status [--short]")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    session.pty.send(key::CTRL_D)?;
    session.pty.wait_exit()?;
    Ok(())
}

fn check_catalog_failure_restores_terminal(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            catalog_gate: true,
            catalog_failure: true,
            ..SessionOptions::default()
        },
    )?;
    let gate_reached = session.catalog_gate_reached.clone();
    wait_for_file(&mut session, gate_reached)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    if !contains(session.pty.output(), STARTUP_MARKER) {
        return Err(io::Error::other("catalog failure was injected before first frame").into());
    }
    fs::write(&session.catalog_gate, b"fail\n")?;
    if session.pty.wait_exit()? == 0 {
        return Err(io::Error::other("injected catalog failure exited successfully").into());
    }
    let observed = session.pty.output();
    if !contains(observed, b"injected catalog admission failure") {
        return Err(io::Error::other("catalog error was replaced during cleanup").into());
    }
    for (marker, state) in [
        (b"\x1b[?2004l".as_slice(), "bracketed paste"),
        (b"\x1b[?25h".as_slice(), "cursor visibility"),
        (b"\x1b[0 q".as_slice(), "cursor shape"),
        (b"\x1b[J".as_slice(), "inline viewport"),
    ] {
        if !contains(observed, marker) {
            return Err(
                io::Error::other(format!("catalog failure did not restore {state}")).into(),
            );
        }
    }
    let modes = session.pty.terminal_modes()?;
    if !modes.local_flags.contains(LocalFlags::ICANON)
        || !modes.local_flags.contains(LocalFlags::ECHO)
    {
        return Err(io::Error::other("catalog failure did not restore cooked modes").into());
    }
    Ok(())
}

fn check_interactive_runtime(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.resize(4, 40)?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.send(b"\x1b[200~resize-safe\x1b[201~")?;
    session.pty.wait_for(b"resize-safe")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    session.pty.resize(30, 120)?;
    session.pty.wait_for(STARTUP_MARKER)?;
    enter_and_wait(&mut session, "/bin/sleep 30 &", STARTUP_MARKER)?;
    session.pty.send(b"\x07")?;
    session.pty.wait_for(b"fg job 1")?;
    session.pty.send(key::ENTER)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    session.pty.send(key::ALT_M)?;
    session
        .pty
        .wait_for_screen("data mode before typed runtime checks", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.starts_with("data | Alt-M mode"))
        })?;
    enter_and_wait(&mut session, "[1,2]", STARTUP_MARKER)?;
    session.pty.send(b"\x1bd")?;
    session.pty.wait_for(b"cached typed result")?;
    session.pty.send(key::ESCAPE)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    let csv_path = session.private.path.join("stream.csv");
    let mut stream = BufWriter::new(File::create(&csv_path)?);
    writeln!(stream, "name")?;
    for index in 0..100_000 {
        writeln!(stream, "row-{index:05}")?;
    }
    stream.flush()?;
    session
        .pty
        .type_text(&format!("open {}", shell_quote(&csv_path)))?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"row-00000")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"cancelled")?;
    wait_for_prompt(&mut session)?;
    session.pty.send(key::ALT_M)?;
    session
        .pty
        .wait_for_screen("command mode after typed runtime checks", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.starts_with("command | Alt-M mode"))
        })?;
    session
        .pty
        .type_text("lua return quirl.process.run('/bin/sleep 30')")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"exceeded its deadline")?;
    wait_for_prompt(&mut session)?;
    enter_and_wait(
        &mut session,
        "/usr/bin/printf AFTER_%s DATA_CANCEL_RESTORED",
        b"AFTER_DATA_CANCEL_RESTORED",
    )?;
    wait_for_prompt(&mut session)?;
    session.pty.send(key::CTRL_D)?;
    session.pty.wait_exit()?;
    Ok(())
}

fn check_rich_review_regressions(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    let startup = session.pty.wait_for(STARTUP_MARKER)?;
    for marker in ["❯", "◆", "·"] {
        if contains(&startup, marker.as_bytes()) {
            return Err(io::Error::other("plain symbols emitted Unicode chrome").into());
        }
    }
    let mut paste = b"\x1b[200~".to_vec();
    paste.extend(std::iter::repeat_n(b'x', 5_000));
    paste.extend_from_slice(b"\x1b[201~");
    session.pty.send(&paste)?;
    session.pty.drain_for(Duration::from_millis(500))?;
    session.pty.send(b"\t")?;
    if !contains(
        &session.pty.drain_for(Duration::from_secs(1))?,
        b"completion limited",
    ) {
        return Err(io::Error::other("oversized completion notice missing").into());
    }
    session.pty.send(key::CTRL_U)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.type_text("git st")?;
    session.pty.send(b"\x1bOP")?;
    session.pty.wait_for(b"documentation")?;
    session.pty.send(b"\x1bOP")?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.send(key::ENTER)?;
    session.pty.type_text("atus")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"not a git repository")?;
    let long_line = format!("echo {}VIEWPORT-END", "x".repeat(180));
    session.pty.send(b"\x1b[200~")?;
    session.pty.type_text(&long_line)?;
    session.pty.send(b"\x1b[201~")?;
    session.pty.wait_for(b"VIEWPORT-END")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    let cleanup_start = session.pty.output().len();
    session.pty.send(key::CTRL_D)?;
    session.pty.wait_exit()?;
    if !contains(&session.pty.output()[cleanup_start..], b"\x1b[?25h") {
        return Err(io::Error::other("cleanup did not show cursor").into());
    }
    let mut no_hints = Session::new(
        binary,
        SessionOptions {
            semantic_hints: Some(false),
            ..SessionOptions::default()
        },
    )?;
    no_hints.pty.wait_for(STARTUP_MARKER)?;
    no_hints.pty.type_text("definitely-not-a-command")?;
    if contains(
        &no_hints.pty.drain_for(Duration::from_millis(300))?,
        b"unknown command",
    ) {
        return Err(io::Error::other("semantic_hints=false rendered diagnostic").into());
    }
    no_hints.pty.send(key::CTRL_C)?;
    no_hints.pty.wait_for(b"^C")?;
    no_hints.pty.send(key::CTRL_D)?;
    no_hints.pty.wait_exit()?;
    Ok(())
}

fn check_suspend_resume(binary: &Path) -> Result<(), Box<dyn Error>> {
    let Some(shell) = find_on_path("zsh").or_else(|| find_on_path("bash")) else {
        println!("skip: check_suspend_resume (zsh/bash unavailable)");
        return Ok(());
    };
    let mut session = Session::new(
        binary,
        SessionOptions {
            shell: Some(shell),
            ..SessionOptions::default()
        },
    )?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.type_text(&shell_quote(binary))?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.send(b"\x1a")?;
    session.pty.wait_for(b"suspended")?;
    session.pty.type_text("fg")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.send(key::CTRL_D)?;
    session.pty.drain_for(Duration::from_millis(300))?;
    session.pty.type_text("exit")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_exit()?;
    Ok(())
}

fn check_native_job_control(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let prompt_modes = session.pty.terminal_modes()?;
    let child = session
        .pty
        .child_pid()
        .ok_or_else(|| io::Error::other("Quirl exited before job checks"))?;
    if session.pty.foreground_group()? != child {
        return Err(io::Error::other("Quirl did not own terminal").into());
    }
    enter_and_wait(
        &mut session,
        "/bin/sh -c 'test \"$(ps -o tpgid= -p $$)\" -eq $$ && printf TTY_%s OWNED'",
        b"TTY_OWNED",
    )?;
    wait_for_prompt(&mut session)?;
    let race = std::iter::repeat_n("/usr/bin/true | /bin/cat", 8)
        .collect::<Vec<_>>()
        .join("; ");
    enter_and_wait(
        &mut session,
        &format!("{race}; /usr/bin/printf LEADER_%s RACE_OK"),
        b"LEADER_RACE_OK",
    )?;
    wait_for_prompt(&mut session)?;
    let pid_path = session.private.path.join("construction.pid");
    let gate_path = session.private.path.join("construction.gate");
    mkfifo(&gate_path, Mode::S_IRUSR | Mode::S_IWUSR)?;
    let script = format!(
        "printf %s $$ > {}; printf x > {}; sleep 30",
        shell_quote(&pid_path),
        shell_quote(&gate_path)
    );
    let construction = format!(
        "/bin/sh -c {} | /bin/cat < {} > /definitely/missing/quirl-construction-output",
        shell_quote_text(&script),
        shell_quote(&gate_path)
    );
    session.pty.type_text(&construction)?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"cannot write redirected output")?;
    wait_for_prompt(&mut session)?;
    let observed_child: i32 = fs::read_to_string(&pid_path)?.trim().parse()?;
    match kill(Pid::from_raw(observed_child), None) {
        Err(Errno::ESRCH) => {}
        Err(error) => return Err(error.into()),
        Ok(()) => {
            return Err(io::Error::other(format!(
                "partial construction leaked child {observed_child}"
            ))
            .into())
        }
    }
    enter_and_wait(
        &mut session,
        "/usr/bin/printf AFTER_%s CONSTRUCTION_CLEANUP",
        b"AFTER_CONSTRUCTION_CLEANUP",
    )?;
    wait_for_prompt(&mut session)?;
    session.pty.type_text("/bin/sleep 30")?;
    session.pty.send(key::ENTER)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut child_group = child;
    while child_group == child && Instant::now() < deadline {
        session.pty.drain_for(Duration::from_millis(20))?;
        child_group = session.pty.foreground_group()?;
    }
    if child_group.as_raw() <= 0 || child_group == child {
        return Err(io::Error::other("foreground child did not receive terminal").into());
    }
    session.pty.send(b"\x1a")?;
    wait_for_prompt(&mut session)?;
    if session.pty.foreground_group()? != child {
        return Err(io::Error::other("Quirl did not recover terminal after Ctrl-Z").into());
    }
    enter_and_wait(&mut session, "jobs", b"stopped")?;
    wait_for_prompt(&mut session)?;
    session.pty.type_text("bg %1")?;
    session.pty.send(key::ENTER)?;
    wait_for_prompt(&mut session)?;
    enter_and_wait(&mut session, "jobs", b"running")?;
    wait_for_prompt(&mut session)?;
    session.pty.type_text("fg %1")?;
    session.pty.send(key::ENTER)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.pty.foreground_group()? == child && Instant::now() < deadline {
        session.pty.drain_for(Duration::from_millis(20))?;
    }
    if session.pty.foreground_group()? == child {
        return Err(io::Error::other("fg did not return terminal to job").into());
    }
    session.pty.send(key::CTRL_C)?;
    wait_for_prompt(&mut session)?;
    enter_and_wait(
        &mut session,
        "/usr/bin/printf AFTER_%s JOB_CTRLC",
        b"AFTER_JOB_CTRLC",
    )?;
    wait_for_prompt(&mut session)?;
    session.pty.type_text("/bin/sh -c 'stty -echo; kill -STOP $$; stty -a | grep -q -- \"-echo\" && printf JOB_%s MODES_OK'")?;
    session.pty.send(key::ENTER)?;
    wait_for_prompt(&mut session)?;
    if session.pty.terminal_modes()? != prompt_modes {
        return Err(io::Error::other("stopped child modes leaked").into());
    }
    session.pty.type_text("fg %2")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"JOB_MODES_OK")?;
    wait_for_prompt(&mut session)?;
    if session.pty.terminal_modes()? != prompt_modes {
        return Err(io::Error::other("termios not restored after fg").into());
    }
    session.pty.send(key::CTRL_D)?;
    session.pty.wait_exit()?;
    Ok(())
}

fn check_noninteractive_dialect_islands(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    enter_and_wait(
        &mut session,
        "bash { read value || printf ISLAND_%s STDIN_CLOSED; }",
        b"ISLAND_STDIN_CLOSED",
    )?;
    wait_for_prompt(&mut session)?;
    session.pty.type_text("bash { sleep 30; }")?;
    session.pty.send(key::ENTER)?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.send(b"\x1a")?;
    session.pty.wait_for(b"cancelled")?;
    wait_for_prompt(&mut session)?;
    enter_and_wait(
        &mut session,
        "/usr/bin/printf AFTER_%s ISLAND_CTRLZ",
        b"AFTER_ISLAND_CTRLZ",
    )?;
    wait_for_prompt(&mut session)?;
    session.pty.send(key::CTRL_D)?;
    session.pty.wait_exit()?;
    Ok(())
}

fn check_fallbacks(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut dumb = Session::new(
        binary,
        SessionOptions {
            term: Some("dumb"),
            catalog_failure: true,
            ..SessionOptions::default()
        },
    )?;
    dumb.pty.drain_for(Duration::from_millis(500))?;
    if contains(dumb.pty.output(), STARTUP_MARKER) {
        return Err(io::Error::other("TERM=dumb rendered rich status").into());
    }
    enter_and_wait(&mut dumb, "/usr/bin/printf DUMB_OK", b"DUMB_OK")?;
    dumb.pty.send(key::CTRL_D)?;
    dumb.pty.wait_exit()?;
    let mut redirected = Session::new(
        binary,
        SessionOptions {
            redirect_stderr: true,
            ..SessionOptions::default()
        },
    )?;
    redirected.pty.drain_for(Duration::from_millis(500))?;
    if contains(redirected.pty.output(), STARTUP_MARKER) {
        return Err(io::Error::other("redirected stderr rendered rich status").into());
    }
    enter_and_wait(
        &mut redirected,
        "/usr/bin/printf REDIRECT_OK",
        b"REDIRECT_OK",
    )?;
    redirected.pty.send(key::CTRL_D)?;
    redirected.pty.wait_exit()?;
    Ok(())
}

fn check_no_color_preserves_semantic_hints(binary: &Path) -> Result<(), Box<dyn Error>> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            no_color: true,
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.type_text("quirl describe --unknown")?;
    session.pty.wait_for(b"unknown flag")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    session.pty.send(key::CTRL_D)?;
    session.pty.wait_exit()?;
    Ok(())
}

fn wait_for_file(session: &mut Session, path: PathBuf) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    while !path.is_file() && Instant::now() < deadline {
        session.pty.drain_for(Duration::from_millis(20))?;
    }
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out waiting for {}", path.display()),
        )
        .into());
    }
    Ok(())
}

fn ensure_status(status: i32, expected: i32, label: &str) -> Result<(), Box<dyn Error>> {
    if status != expected {
        return Err(
            io::Error::other(format!("{label} exited {status}; expected {expected}")).into(),
        );
    }
    Ok(())
}

fn screen_error(message: &str, screen: &VirtualScreen) -> Box<dyn Error> {
    io::Error::other(format!("{message}; screen=\n{}", screen.text())).into()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn shell_quote(path: &Path) -> String {
    shell_quote_text(&path.to_string_lossy())
}

fn shell_quote_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn temporary_session_directory_is_private() {
        let directory = TempDirectory::new("quirl-private-test").unwrap();
        let mode = fs::metadata(&directory.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
