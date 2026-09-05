//! End-to-end rich-terminal checks driven by the Rust PTY harness.

mod clipboard;
mod resize_input;
mod soak_gallery;
mod sustained;
mod text_input;
mod usability;

pub(crate) mod soak;
pub(crate) mod zero_poll;

use crate::{
    TaskError,
    pty::{PtySession, SpawnOptions, VirtualScreen, default_timeout, key},
};
use nix::{
    errno::Errno,
    sys::{
        signal::{Signal, kill},
        termios::LocalFlags,
    },
    unistd::Pid,
};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, DirBuilder, File},
    io::{self, BufWriter, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const STARTUP_MARKER: &[u8] = b"Tab complete";
const ALTERNATE_SCREEN_ENTER: &[u8] = b"\x1b[?1049h";
const ALTERNATE_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";
const DISCOVERY_ARTIFACT_BYTES_MAX: usize = 128 * 1024 * 1024;
const DISCOVERY_DIRECTORY_ENTRIES_MAX: usize = 8;
const DISCOVERY_FIXTURE_BYTES_MAX: usize = 4 * 1024;
const CHECK_NAMES: &[&str] = &[
    "rich-editing",
    "mode-switch-and-palette-screen",
    "automatic-command-intelligence",
    "codex-only-ai-mode",
    "durable-command-discovery",
    "deferred-catalog-admission",
    "catalog-failure-restores-terminal",
    "completion",
    "interactive-runtime",
    "cwd-history",
    "restart-history",
    "multiline-paste-admission",
    "paste-control-isolation",
    "oversized-paste-admission",
    "unicode-committed-text",
    "keyboard-clipboard-protocol",
    "terminal-input-limits",
    "vi-repeat-admission",
    "sustained-session",
    "resize-input",
    "retained-output-cycles",
    "first-session-help-and-data",
    "discovery-preserves-command-intent",
    "external-command-compatibility",
    "streamed-progress-without-newline",
    "spinner-animates-during-silent-command",
    "full-screen-program-takeover",
    "full-screen-program-spawn-failure-restores-terminal",
    "ctrl-l-forces-full-repaint",
    "local-completion-discovery",
    "rich-review-regressions",
    "native-job-control",
    "noninteractive-dialect-islands",
    "suspend-resume",
    "fallbacks",
    "no-color-preserves-semantic-hints",
];
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

type Check = fn(&Path) -> Result<(), TaskError>;

struct CheckCase {
    name: &'static str,
    run: Check,
}

#[derive(Default)]
struct SessionOptions {
    term: Option<&'static str>,
    surface: Option<&'static str>,
    shell: Option<PathBuf>,
    symbols: Option<&'static str>,
    keymap: Option<&'static str>,
    semantic_hints: Option<bool>,
    no_color: bool,
    catalog_gate: bool,
    catalog_failure: bool,
    redirect_stderr: bool,
    rows: Option<usize>,
    columns: Option<usize>,
    path: Option<PathBuf>,
    index_dir: Option<PathBuf>,
    help_path: Option<PathBuf>,
    ai_bootstrap_fake: bool,
    catalog_force_timeout: bool,
    catalog_refresh_enabled: bool,
    fish_completion: Option<String>,
}

struct Session {
    pty: PtySession,
    spawn: SpawnOptions,
    private: TempDirectory,
    catalog_gate: PathBuf,
    catalog_gate_reached: PathBuf,
}

impl Session {
    fn new(binary: &Path, options: SessionOptions) -> Result<Self, TaskError> {
        let private = TempDirectory::new("quirl-pty")?;
        let config_dir = private.path.join("config");
        create_private_directory(&config_dir)?;
        let xdg_config_dir = private.path.join("xdg-config");
        create_private_directory(&xdg_config_dir)?;
        let temporary_dir = private.path.join("tmp");
        create_private_directory(&temporary_dir)?;
        let semantic_hints = options.semantic_hints.unwrap_or(true);
        let symbols = options.symbols.unwrap_or("plain");
        let keymap = options.keymap.unwrap_or("emacs");
        let surface = options.surface.unwrap_or("rich");
        fs::write(
            config_dir.join("config.lua"),
            format!(
                r#"---@type quirl.Config
return quirl.config {{
  schema_version = 3,
  editor = {{ keymap = "{keymap}", semantic_hints = {semantic_hints}, banner = "none" }},
  picker = {{ layout = "adaptive", preview = true }},
  prompt = {{
    symbols = "{symbols}",
    left = {{ "directory" }},
    right = {{ "duration", "status" }},
    transient = false,
  }},
  ui = {{ theme = "tokyo-night", surface = "{surface}", statusline = {{ hints = true }} }},
  completion = {{ auto = false, min_chars = 2 }},
}}
"#
            ),
        )?;
        let completion_roots = if options.path.is_some()
            || options.catalog_refresh_enabled
            || options.fish_completion.is_some()
        {
            let roots = private.path.join("completion-roots");
            create_private_directory(&roots)?;
            let fish = roots.join("fish");
            let bash = roots.join("bash");
            let zsh = roots.join("zsh");
            create_private_directory(&fish)?;
            create_private_directory(&bash)?;
            create_private_directory(&zsh)?;
            if let Some(completion) = options.fish_completion.as_deref() {
                fs::write(fish.join("ghq.fish"), completion)?;
            }
            Some((fish, bash, zsh))
        } else {
            None
        };

        let path = options.path.clone().map_or_else(
            || env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
            PathBuf::into_os_string,
        );
        let index_dir = options
            .index_dir
            .clone()
            .unwrap_or_else(|| private.path.join("index"));
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
                index_dir.clone().into_os_string(),
            ),
            (
                OsString::from("QUIRL_INDEX_PATH"),
                index_dir.join("catalog.sqlite3").into_os_string(),
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
                xdg_config_dir.into_os_string(),
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
        if let Some((fish, bash, zsh)) = completion_roots {
            environment.insert(OsString::from("QUIRL_FISH_PATH"), fish.into_os_string());
            environment.insert(OsString::from("QUIRL_BASH_PATH"), bash.into_os_string());
            environment.insert(OsString::from("QUIRL_ZSH_PATH"), zsh.into_os_string());
        }
        // PTYs that do not exercise discovery must not inherit the host's PATH
        // catalog. Dedicated discovery sessions enable the one background
        // worker and retain the real bounded admission path.
        if options.catalog_force_timeout
            || (!options.catalog_refresh_enabled && options.path.is_none())
        {
            environment.insert(
                OsString::from("QUIRL_TEST_CATALOG_FORCE_TIMEOUT"),
                OsString::from("1"),
            );
        }
        if !options.catalog_refresh_enabled {
            environment.insert(
                OsString::from("QUIRL_TEST_CATALOG_REFRESH_DISABLED"),
                OsString::from("1"),
            );
        }
        environment.insert(
            OsString::from(if options.ai_bootstrap_fake {
                "QUIRL_TEST_AI_BOOTSTRAP_FAKE"
            } else {
                "QUIRL_TEST_AI_BOOTSTRAP_DISABLED"
            }),
            OsString::from("1"),
        );
        if options.no_color {
            environment.insert(OsString::from("NO_COLOR"), OsString::from("1"));
        }
        if let Some(help_path) = options.help_path {
            environment.insert(
                OsString::from("QUIRL_HELP_PATH"),
                help_path.into_os_string(),
            );
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
        let pty = PtySession::spawn(spawn.clone())?;
        Ok(Self {
            pty,
            spawn,
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
                Ok(()) => {
                    return fs::canonicalize(path).map(|path| Self { path });
                }
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

struct DirectoryPermissionsGuard {
    path: PathBuf,
    restore: fs::Permissions,
}

impl DirectoryPermissionsGuard {
    fn make_read_only(path: &Path) -> io::Result<Self> {
        let restore = fs::metadata(path)?.permissions();
        fs::set_permissions(path, fs::Permissions::from_mode(0o500))?;
        Ok(Self {
            path: path.to_path_buf(),
            restore,
        })
    }
}

impl Drop for DirectoryPermissionsGuard {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.path, self.restore.clone());
    }
}

/// Best-effort cleanup for the deliberately blocked construction child.
///
/// The child belongs to Quirl rather than xtask and may be in a process group
/// that is neither the PTY foreground group nor the PTY session leader. Reading
/// its fixture-recorded group on drop contains regressions without replacing
/// the originating harness error.
struct ObservedProcessGroupCleanup {
    pid_path: PathBuf,
    armed: bool,
}

impl ObservedProcessGroupCleanup {
    fn new(pid_path: PathBuf) -> Self {
        Self {
            pid_path,
            armed: true,
        }
    }

    fn observed_pid(&self) -> io::Result<Pid> {
        let contents = fs::read_to_string(&self.pid_path)?;
        let process_id = contents.trim().parse::<i32>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "construction child identifier in {} was invalid: {error}",
                    self.pid_path.display()
                ),
            )
        })?;
        if process_id <= 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("refusing to clean up unsafe process group {process_id}"),
            ));
        }
        Ok(Pid::from_raw(process_id))
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ObservedProcessGroupCleanup {
    #[allow(
        clippy::arithmetic_side_effects,
        reason = "negating a validated positive process group identifier is required by POSIX kill"
    )]
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(process) = self.observed_pid() {
            let _ = kill(Pid::from_raw(-process.as_raw()), Signal::SIGKILL);
        }
    }
}

pub(super) fn run(_root: &Path, binary: &Path, selected: &[String]) -> Result<(), TaskError> {
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

    // All fixed checks execute one admitted snapshot even if another worker
    // rebuilds the source binary while this suite is running.
    let pinned = crate::simulation::PinnedExecutable::create(binary, 0x5054_5943_4845_434b)?;
    println!(
        "rich PTY binary: sha256={} bytes={} source={}",
        pinned.sha256(),
        pinned.byte_size(),
        pinned.source().display()
    );
    let binary = pinned.path();
    for check in checks() {
        if !selected.is_empty() && !selected.iter().any(|name| name == check.name) {
            continue;
        }
        (check.run)(binary)?;
        println!("ok: check_{}", check.name.replace('-', "_"));
    }
    Ok(())
}

fn checks() -> [CheckCase; 36] {
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
            name: "automatic-command-intelligence",
            run: check_automatic_command_intelligence,
        },
        CheckCase {
            name: "codex-only-ai-mode",
            run: check_codex_only_ai_mode,
        },
        CheckCase {
            name: "durable-command-discovery",
            run: check_durable_command_discovery,
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
            name: "cwd-history",
            run: check_cwd_history,
        },
        CheckCase {
            name: "restart-history",
            run: usability::check_restart_history,
        },
        CheckCase {
            name: "multiline-paste-admission",
            run: usability::check_multiline_paste_admission,
        },
        CheckCase {
            name: "paste-control-isolation",
            run: text_input::check_paste_control_isolation,
        },
        CheckCase {
            name: "oversized-paste-admission",
            run: text_input::check_oversized_paste_admission,
        },
        CheckCase {
            name: "unicode-committed-text",
            run: text_input::check_unicode_committed_text,
        },
        CheckCase {
            name: "keyboard-clipboard-protocol",
            run: clipboard::check_clipboard_protocol,
        },
        CheckCase {
            name: "terminal-input-limits",
            run: text_input::check_terminal_input_limits,
        },
        CheckCase {
            name: "vi-repeat-admission",
            run: text_input::check_vi_repeat_admission,
        },
        CheckCase {
            name: "sustained-session",
            run: sustained::check_sustained_session,
        },
        CheckCase {
            name: "resize-input",
            run: resize_input::check_resize_input,
        },
        CheckCase {
            name: "retained-output-cycles",
            run: check_retained_output_cycles,
        },
        CheckCase {
            name: "first-session-help-and-data",
            run: check_first_session_help_and_data,
        },
        CheckCase {
            name: "discovery-preserves-command-intent",
            run: check_discovery_preserves_command_intent,
        },
        CheckCase {
            name: "external-command-compatibility",
            run: check_external_command_compatibility,
        },
        CheckCase {
            name: "streamed-progress-without-newline",
            run: check_streamed_progress_without_newline,
        },
        CheckCase {
            name: "spinner-animates-during-silent-command",
            run: check_spinner_animates_during_silent_command,
        },
        CheckCase {
            name: "full-screen-program-takeover",
            run: check_full_screen_program_takeover,
        },
        CheckCase {
            name: "full-screen-program-spawn-failure-restores-terminal",
            run: check_full_screen_program_spawn_failure_restores_terminal,
        },
        CheckCase {
            name: "ctrl-l-forces-full-repaint",
            run: check_ctrl_l_forces_full_repaint,
        },
        CheckCase {
            name: "local-completion-discovery",
            run: check_local_completion_discovery,
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
) -> Result<Vec<u8>, TaskError> {
    session.pty.type_text(command)?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(marker)
}

fn wait_for_rich_input_since(session: &mut Session, start: usize) -> Result<(), TaskError> {
    // A rendered output/footer can precede the next editor's terminal lease.
    // Require its fresh mouse-mode enable before sending more input; screen
    // echo or an earlier prompt must not make a command look completed.
    session
        .pty
        .wait_for_since(b"\x1b[?1000h", start, default_timeout())?;
    Ok(())
}

fn execute_and_resume(session: &mut Session, command: &str) -> Result<(), TaskError> {
    let output_start = session.pty.output().len();
    session.pty.type_text(command)?;
    session.pty.send(key::ENTER)?;
    let command_record = format!("❯ {command}");
    session
        .pty
        .wait_for_screen("completed command in persistent viewport", |screen| {
            let text = screen.text();
            text.contains(&command_record)
                && text.contains("── exit ")
                && screen.bottom_line().contains("NORMAL")
        })?;
    wait_for_rich_input_since(session, output_start)?;
    ensure_alternate_screen_unchanged(session, output_start, "completed command")
}

fn execute_and_resume_with_marker(
    session: &mut Session,
    command: &str,
    marker: &[u8],
) -> Result<(), TaskError> {
    let output_start = session.pty.output().len();
    session.pty.type_text(command)?;
    session.pty.send(key::ENTER)?;
    let marker = std::str::from_utf8(marker).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("viewport marker is not UTF-8: {error}"),
        )
    })?;
    session.pty.wait_for_screen(
        &format!("command marker {marker:?} in persistent viewport"),
        |screen| {
            screen.lines().iter().any(|line| {
                let line = line.trim_start();
                !line.starts_with(['❯', '>', '∙']) && line.contains(marker)
            }) && screen.bottom_line().contains("NORMAL")
        },
    )?;
    session
        .pty
        .wait_for_since(b"\x1b[?1000h", output_start, default_timeout())?;
    ensure_alternate_screen_unchanged(session, output_start, "marked command")
}

#[allow(
    clippy::indexing_slicing,
    reason = "the comparison suffix begins at an offset clamped to the captured screen bytes"
)]
fn ensure_alternate_screen_unchanged(
    session: &Session,
    output_start: usize,
    stage: &str,
) -> Result<(), TaskError> {
    let emitted = &session.pty.output()[output_start..];
    if contains(emitted, ALTERNATE_SCREEN_LEAVE) || contains(emitted, ALTERNATE_SCREEN_ENTER) {
        return Err(io::Error::other(format!(
            "{stage} changed persistent alternate-screen ownership"
        ))
        .into());
    }
    Ok(())
}

fn execute_simple_with_marker(
    session: &mut Session,
    command: &str,
    marker: &[u8],
) -> Result<(), TaskError> {
    session.pty.type_text(command)?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(marker)?;
    wait_for_terminal_owner(session)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "poll counts are bounded by the terminal ownership deadline"
)]
fn wait_for_terminal_owner(session: &mut Session) -> Result<(), TaskError> {
    let child = session
        .pty
        .child_pid()
        .ok_or_else(|| io::Error::other("Quirl exited before recovering terminal ownership"))?;
    let deadline = Instant::now() + default_timeout();
    while session.pty.foreground_group()? != child && Instant::now() < deadline {
        session.pty.drain_for(Duration::from_millis(10))?;
    }
    if session.pty.foreground_group()? != child {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Quirl did not recover terminal ownership",
        )
        .into());
    }
    Ok(())
}

fn check_discovery_preserves_command_intent(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    fs::write(
        session.private.path.join("quarterly report;$notes.txt"),
        b"PICKED_EXACT_FILE\n",
    )?;
    fs::write(
        session.private.path.join("-n"),
        b"PICKED_OPTION_LIKE_FILE\n",
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.type_text("git st")?;
    session.pty.send(b"\t")?;
    session.pty.wait_for_screen_text("git status [--short]")?;
    session.pty.send(b"\x1b[B\x1b[A")?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session
        .pty
        .wait_for_screen("Up stays in explicit completion", |screen| {
            screen.text().contains("git status [--short]")
                && !screen.text().contains("history picker")
        })?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    session.pty.type_text("git status | quirl data")?;
    session.pty.send(b"\x1bOP")?;
    session
        .pty
        .wait_for_screen("help follows the pipeline stage at the cursor", |screen| {
            let text = screen.text();
            text.contains("> quirl data") && text.contains("Sources are")
        })?;
    session.pty.send(key::ESCAPE)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session
        .pty
        .wait_for_screen_text("git status | quirl data")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    pick_file_and_read(
        &mut session,
        "quarterly",
        "quarterly report;$notes.txt",
        r"> cat quarterly\ report\;\$notes.txt",
        "PICKED_EXACT_FILE",
    )?;
    pick_file_and_read(
        &mut session,
        "-n",
        "-n",
        "> cat ./-n",
        "PICKED_OPTION_LIKE_FILE",
    )?;
    let cleanup_start = session.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "command discovery",
    )?;
    ensure_terminal_restored(&session, cleanup_start, "command discovery")
}

fn pick_file_and_read(
    session: &mut Session,
    query: &str,
    filename: &str,
    expected_editor: &str,
    marker: &str,
) -> Result<(), TaskError> {
    session.pty.type_text("cat ")?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"f")?;
    session.pty.wait_for_screen_text("picker")?;
    session.pty.type_text(query)?;
    session.pty.wait_for_screen_text(filename)?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("picked file inserted into editor", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.trim() == expected_editor)
                && !screen.text().contains("picker")
        })?;
    let execution_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session.pty.wait_for_screen_text(marker)?;
    wait_for_rich_input_since(session, execution_start)?;
    Ok(())
}

fn check_first_session_help_and_data(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(40),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    execute_and_resume_with_marker(
        &mut session,
        &format!("help {}", "x".repeat(257)),
        b"help topic is too long",
    )?;
    execute_and_resume_with_marker(
        &mut session,
        "help mode",
        b"Switch the visible interactive grammar",
    )?;
    // Repainting must recover help from retained state, not stale terminal bytes.
    session.pty.send(key::CTRL_L)?;
    session
        .pty
        .wait_for_screen_text("Switch the visible interactive grammar")?;
    execute_and_resume_with_marker(&mut session, "help", b"Getting started with Quirl")?;
    session.pty.type_text("mode data")?;
    let data_mode_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("textual Data-mode entry", |screen| {
            screen.bottom_line().contains("DATA")
        })?;
    wait_for_rich_input_since(&mut session, data_mode_start)?;
    session.pty.type_text(r#"[{"service":"api","status":"failed"},{"service":"web","status":"healthy"}] | where status == "failed" | select service"#)?;
    let result_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("first typed result", |screen| {
            let text = screen.text();
            screen.bottom_line().contains("DATA")
                && text
                    .lines()
                    .any(|line| line.trim().starts_with(r#"{"service":"api"}"#))
        })?;
    wait_for_rich_input_since(&mut session, result_start)?;
    session.pty.type_text("mode normal")?;
    let normal_mode_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("textual Normal-mode return", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    wait_for_rich_input_since(&mut session, normal_mode_start)?;
    let cleanup_start = session.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "first session",
    )?;
    ensure_terminal_restored(&session, cleanup_start, "first session")
}

fn check_rich_editing(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.type_text("/usr/bin/printf BACKSPACE_BAD")?;
    session.pty.send(b"\x7f\x7f\x7f")?;
    let backspace_start = session.pty.output().len();
    enter_and_wait(&mut session, "OK", b"BACKSPACE_OK")?;
    wait_for_rich_input_since(&mut session, backspace_start)?;
    session.pty.type_text("/usr/bin/printf DELETE_XOK")?;
    session.pty.send(b"\x1b[D\x1b[D\x1b[D\x1b[3~")?;
    let delete_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session.pty.wait_for_screen_text("DELETE_OK")?;
    wait_for_rich_input_since(&mut session, delete_start)?;
    session.pty.type_text("/usr/bin/printf UNICODE_e\u{301}")?;
    session.pty.drain_for(Duration::from_millis(50))?;
    session.pty.send(b"\x7f")?;
    let unicode_start = session.pty.output().len();
    enter_and_wait(&mut session, "OK", b"UNICODE_OK")?;
    wait_for_rich_input_since(&mut session, unicode_start)?;
    session.pty.type_text("/usr/bin/printf CTRLD_XOK")?;
    let ctrl_d_start = session.pty.output().len();
    session.pty.send(b"\x1b[D\x1b[D\x1b[D\x04\r")?;
    session.pty.wait_for(b"CTRLD_OK")?;
    wait_for_rich_input_since(&mut session, ctrl_d_start)?;
    session.pty.type_text("/usr/bin/printf SHOULD_NOT_RUN")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    execute_and_resume_with_marker(&mut session, "/usr/bin/printf AFTER_CTRLC", b"AFTER_CTRLC")?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"d")?;
    session
        .pty
        .wait_for_screen("rich data-mode status", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("DATA") && line.contains("Alt-Q Quirl"))
        })?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"i")?;
    session
        .pty
        .wait_for_screen("rich AI-mode status", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("AI") && line.contains("Alt-Q Quirl"))
        })?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"n")?;
    session
        .pty
        .wait_for_screen("rich command-mode status", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("NORMAL") && line.contains("Alt-Q Quirl"))
        })?;
    session.pty.type_text("/usr/bin/printf 'MULTI_ONE")?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("incomplete quote continues editing", |screen| {
            screen.text().contains("MULTI_ONE")
                && screen.lines().iter().any(|line| line.trim() == ".")
        })?;
    session.pty.type_text("_TWO'")?;
    let multiline_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("captured multiline output", |screen| {
            screen.lines().iter().any(|line| line.trim() == "MULTI_ONE")
                && screen.lines().iter().any(|line| line.trim() == "_TWO")
        })?;
    wait_for_rich_input_since(&mut session, multiline_start)?;
    execute_and_resume_with_marker(
        &mut session,
        "/bin/sh -c 'printf STDOUT_OK; printf STDERR_OK >&2'",
        b"STDERR_OK",
    )?;
    if !session.pty.screen().lines().iter().any(|line| {
        let line = line.trim_start();
        !line.starts_with(['❯', '>', '∙', '.']) && line.contains("STDOUT_OK")
    }) {
        return Err(screen_error(
            "interactive command stdout was not handed back to the PTY",
            session.pty.screen(),
        ));
    }
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "Ctrl-D",
    )
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn check_mode_switch_and_palette_screen(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(18),
            columns: Some(78),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    if !session.pty.screen().bottom_line().contains("NORMAL") {
        return Err(screen_error(
            "initial rich status was not anchored at the terminal bottom",
            session.pty.screen(),
        ));
    }
    session.pty.type_text("echo MODE_BUFFER_RETAINED")?;
    session.pty.wait_for_screen_text("MODE_BUFFER_RETAINED")?;
    for (leader_key, expected_mode, expected_indicator) in [
        (b"d", "DATA", "D echo MODE_BUFFER_RETAINED"),
        (b"i", "AI", "AI echo MODE_BUFFER_RETAINED"),
        (b"n", "NORMAL", "echo MODE_BUFFER_RETAINED"),
        (b"d", "DATA", "D echo MODE_BUFFER_RETAINED"),
    ] {
        let output_start = session.pty.output().len();
        session.pty.send(key::ALT_Q)?;
        session.pty.send(leader_key)?;
        session.pty.wait_for_screen(
            &format!("bottom-status {expected_mode} mode frame"),
            |screen| {
                screen
                    .bottom_line()
                    .split('|')
                    .next()
                    .is_some_and(|mode_segment| mode_segment.contains(expected_mode))
            },
        )?;
        let emitted = &session.pty.output()[output_start..];
        if contains(emitted, b"mode:") || contains(emitted, b"mode ->") {
            return Err(io::Error::other(format!(
                "Alt-Q mode selection emitted feedback into scrollback; output={emitted:?}"
            ))
            .into());
        }
        if !session.pty.screen().text().contains(expected_indicator) {
            return Err(screen_error(
                &format!("Alt-Q discarded active editor buffer; expected {expected_indicator:?}"),
                session.pty.screen(),
            ));
        }
    }
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"p")?;
    session
        .pty
        .wait_for_screen("top-anchored Alt-Q palette", |screen| {
            let bottom = screen.bottom_line();
            bottom.contains("DATA")
                && bottom.contains("results (commands)")
                && screen.text().contains("picker")
        })?;
    session.pty.type_text("git status")?;
    session
        .pty
        .wait_for_screen("filtered Alt-Q palette", |screen| {
            screen.text().contains("git status")
        })?;
    session.pty.send(key::ESCAPE)?;
    session
        .pty
        .wait_for_screen("top editor after palette dismissal", |screen| {
            let text = screen.text();
            screen.bottom_line().contains("DATA")
                && !text.contains("picker")
                && !text.contains("results (commands)")
        })?;
    let lines = session.pty.screen().lines();
    let top = &lines[..lines.len().min(3)];
    if top
        .get(1)
        .is_none_or(|line| !line.contains("MODE_BUFFER_RETAINED"))
    {
        return Err(io::Error::other(format!(
            "palette dismissal did not restore the top editor; top_rows={top:?}"
        ))
        .into());
    }
    session.pty.send(key::CTRL_U)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "screen-state session",
    )
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn check_automatic_command_intelligence(binary: &Path) -> Result<(), TaskError> {
    // Failure model: redraws may lag resize/input, commands may retain the PTY
    // foreground group, and any return path may strand raw or alternate-screen
    // state. Keep this ordered end-to-end transaction together so every phase
    // proves the prior phase returned to an owned prompt before continuing.
    let fixtures = TempDirectory::new("quirl-command-intelligence")?;
    let binary_dir = fixtures.path.join("bin");
    let index_dir = fixtures.path.join("index");
    create_private_directory(&binary_dir)?;
    create_private_directory(&index_dir)?;
    write_executable(
        &binary_dir.join("fixture-tool"),
        "#!/bin/sh\nprintf 'FIXTURE_TOOL_EXECUTED\\n'\n",
    )?;
    write_executable(
        &binary_dir.join("handoff-tool"),
        "#!/bin/sh\nprintf 'HANDOFF_STARTED\\n'\n/bin/sleep 2\nprintf 'HANDOFF_DONE\\n'\n",
    )?;
    write_executable(
        &binary_dir.join("ls"),
        "#!/bin/sh\nprintf 'SYSTEM_LS:%s\\n' \"$*\"\n",
    )?;

    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(18),
            columns: Some(120),
            path: Some(binary_dir),
            index_dir: Some(index_dir.clone()),
            catalog_refresh_enabled: true,
            ..SessionOptions::default()
        },
    )?;
    let startup = session.pty.wait_for(STARTUP_MARKER)?;
    if !contains(&startup, ALTERNATE_SCREEN_ENTER) {
        return Err(io::Error::other("rich startup did not enter the alternate screen").into());
    }
    ensure_bottom_status(&session, "startup")?;

    let resize_start = session.pty.output().len();
    session.pty.resize(14, 96)?;
    session
        .pty
        .wait_for_screen("bottom status after resize", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    if contains(
        &session.pty.output()[resize_start..],
        ALTERNATE_SCREEN_LEAVE,
    ) {
        return Err(io::Error::other("resize released alternate-screen ownership").into());
    }
    let restored_startup_width_start = session.pty.output().len();
    session.pty.resize(18, 120)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", restored_startup_width_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("bottom status after resize restoration", |screen| {
            screen.bottom_line().contains("NORMAL")
                && screen
                    .lines()
                    .iter()
                    .filter(|line| line.contains("NORMAL"))
                    .count()
                    == 1
        })?;

    // This check exercises discovered commands, so admit its isolated PATH
    // catalog explicitly instead of relying on time spent resizing the frame.
    wait_for_file_contents(
        &mut session,
        &index_dir.join("catalog.sqlite3"),
        b"fixture-tool",
    )?;
    wait_for_command_information(&mut session, "ls", &["Enter run", "source:"])?;
    let normal_ls_information = session.pty.screen().text();
    if normal_ls_information.contains("List a directory as typed entries in Data mode")
        || normal_ls_information.contains("source: quirl · built-in")
        || !normal_ls_information.contains("source:")
    {
        return Err(screen_error(
            "Normal mode exposed Quirl's typed ls instead of the PATH command",
            session.pty.screen(),
        ));
    }
    session.pty.send(b"\x1b[B")?;
    session
        .pty
        .wait_for_screen("navigated automatic results", |screen| {
            screen.bottom_line().contains("Enter accept")
        })?;
    session.pty.send(key::ESCAPE)?;
    wait_for_standard_status(&mut session)?;
    clear_editor(&mut session)?;
    let wide_provenance_resize_start = session.pty.output().len();
    session.pty.resize(18, 400)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", wide_provenance_resize_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("wide provenance frame", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;

    for (command, summary, provenance) in [
        ("cd", "Change the shell working", "source: builtin"),
        (
            "git status",
            "Show repository and working-tree",
            "source: external",
        ),
        (
            "fixture-tool",
            "Installed command discovered",
            "source: external",
        ),
    ] {
        if let Err(error) = wait_for_command_information(
            &mut session,
            command,
            &[summary, "Capabilities:", provenance],
        ) {
            let database = read_bounded_fixture(
                &index_dir.join("catalog.sqlite3"),
                DISCOVERY_ARTIFACT_BYTES_MAX,
            )
            .unwrap_or_default();
            return Err(io::Error::other(format!(
                "{error}; command={command:?}; database_has_command={}",
                contains(&database, command.as_bytes())
            ))
            .into());
        }
        session.pty.send(key::ESCAPE)?;
        wait_for_standard_status(&mut session)?;
        clear_editor(&mut session)?;
    }
    let normal_width_resize_start = session.pty.output().len();
    session.pty.resize(18, 120)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", normal_width_resize_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("normal-width flag frame", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;

    session.pty.type_text("ls -al")?;
    session
        .pty
        .wait_for_screen("complete Normal ls command in editor", |screen| {
            screen.lines().iter().any(|line| line == "> ls -al")
        })?;
    let normal_ls_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("Normal ls dispatched to PATH", |screen| {
            transcript_tail_flows_into_prompt(screen, "ls -al", "SYSTEM_LS:-al")
                && screen.bottom_line().contains("NORMAL")
        })?;
    session
        .pty
        .wait_for_since(b"\x1b[?1000h", normal_ls_start, default_timeout())?;
    if session.pty.screen().text().contains("unknown flag")
        || session
            .pty
            .screen()
            .text()
            .contains("List a directory as typed entries in Data mode")
    {
        return Err(screen_error(
            "Normal ls was parsed as Quirl's typed-data override",
            session.pty.screen(),
        ));
    }
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.send(key::CTRL_U)?;
    session
        .pty
        .wait_for_screen("fresh blank Normal prompt after ls", |screen| {
            !screen.lines().iter().any(|line| line.trim() == "ls -al")
                && screen.bottom_line().contains("NORMAL")
        })?;
    write_fixture(
        &session.private.path.join("DATA_MODE_LS_SENTINEL"),
        "visible to typed ls\n",
    )?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"d")?;
    session
        .pty
        .wait_for_screen("Data mode before typed ls checks", |screen| {
            screen.bottom_line().contains("DATA")
        })?;
    let wide_data_resize_start = session.pty.output().len();
    session.pty.resize(18, 400)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", wide_data_resize_start, default_timeout())?;
    wait_for_command_information(
        &mut session,
        "ls",
        &[
            "List a directory as typed entries in Data mode",
            "Enter run",
        ],
    )?;
    let data_ls_information = session.pty.screen().text();
    if data_ls_information.contains("Installed command discovered")
        || data_ls_information.contains("--all")
        || data_ls_information.contains("--long")
    {
        return Err(screen_error(
            "Data mode selected the PATH ls instead of its typed override",
            session.pty.screen(),
        ));
    }
    session.pty.send(key::ESCAPE)?;
    wait_for_mode_status(&mut session, "DATA")?;
    clear_editor_in_mode(&mut session, "DATA")?;
    let data_width_resize_start = session.pty.output().len();
    session.pty.resize(18, 120)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", data_width_resize_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("normal-width typed ls frame", |screen| {
            screen.bottom_line().contains("DATA")
        })?;
    session.pty.type_text("ls")?;
    session
        .pty
        .wait_for_screen("complete Data ls command in editor", |screen| {
            // The retained Normal-mode `ls -al` history entry may appear as
            // dim suggestion text after the exact `ls` editor buffer.
            screen.lines().iter().any(|line| line.starts_with("> D ls"))
        })?;
    let execution_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("Data ls used Quirl's typed override", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("file") && line.contains("DATA_MODE_LS_SENTINEL"))
                && screen.bottom_line().contains("DATA")
        })?;
    let execution = &session.pty.output()[execution_start..];
    if !contains(execution, b"DATA_MODE_LS_SENTINEL")
        || contains(execution, ALTERNATE_SCREEN_LEAVE)
        || contains(execution, ALTERNATE_SCREEN_ENTER)
    {
        return Err(io::Error::other(
            "typed ls output did not remain inside the persistent viewport",
        )
        .into());
    }
    session
        .pty
        .wait_for_since(b"\x1b[?1000h", execution_start, default_timeout())?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"n")?;
    wait_for_standard_status(&mut session)?;
    session.pty.type_text("x")?;
    session.pty.wait_for_screen_text("x")?;
    clear_editor(&mut session)?;
    ensure_bottom_status(&session, "after first-Enter execution")?;

    wait_for_command_information(
        &mut session,
        "handoff-tool",
        &["Installed command discovered", "Enter run"],
    )?;
    let quirl_process = session
        .pty
        .child_pid()
        .ok_or_else(|| io::Error::other("Quirl exited before foreground handoff"))?;
    let handoff_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session.pty.wait_for_screen_text("HANDOFF_STARTED")?;
    if session.pty.foreground_group()? == quirl_process {
        return Err(
            io::Error::other("foreground command did not receive terminal ownership").into(),
        );
    }
    session
        .pty
        .wait_for_screen("captured foreground command completion", |screen| {
            let text = screen.text();
            text.contains("HANDOFF_STARTED")
                && text.contains("HANDOFF_DONE")
                && screen.bottom_line().contains("NORMAL")
        })?;
    wait_for_rich_input_since(&mut session, handoff_start)?;
    if session.pty.foreground_group()? != quirl_process {
        return Err(
            io::Error::other("Quirl did not recover terminal ownership after handoff").into(),
        );
    }
    ensure_alternate_screen_unchanged(&session, execution_start, "captured foreground command")?;
    ensure_bottom_status(&session, "after captured foreground command")?;

    session.pty.resize(4, 48)?;
    session
        .pty
        .wait_for_screen("compact bottom status", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    let restored_cleanup_width_start = session.pty.output().len();
    session.pty.resize(18, 120)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", restored_cleanup_width_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("restored bottom status", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    let cleanup_start = session.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "command-intelligence EOF",
    )?;
    ensure_terminal_restored(&session, cleanup_start, "command-intelligence EOF")
}

fn check_codex_only_ai_mode(binary: &Path) -> Result<(), TaskError> {
    let fixtures = TempDirectory::new("quirl-idle-ai-bootstrap")?;
    let binary_dir = fixtures.path.join("bin");
    let index_dir = fixtures.path.join("index");
    create_private_directory(&binary_dir)?;
    create_private_directory(&index_dir)?;
    write_executable(
        &binary_dir.join("idle-background-tool"),
        "#!/bin/sh\nexit 0\n",
    )?;
    let catalog_path = index_dir.join("catalog.sqlite3");
    let mut session = Session::new(
        binary,
        SessionOptions {
            ai_bootstrap_fake: true,
            catalog_force_timeout: true,
            catalog_refresh_enabled: true,
            path: Some(binary_dir),
            index_dir: Some(index_dir),
            rows: Some(12),
            columns: Some(120),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session
        .pty
        .wait_for_screen("live catalog discovery status", |screen| {
            screen
                .bottom_line()
                .contains("Catalog: discovering installed commands")
        })?;
    wait_for_file_contents(&mut session, &catalog_path, b"idle-background-tool")?;
    let catalog = fs::read(&catalog_path)?;
    if contains(&catalog, b"niklas-heer/quirl-command-v3-int8") {
        return Err(io::Error::other("Codex-only mode built a local model index").into());
    }
    let cleanup_start = session.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "Codex-only AI mode EOF",
    )?;
    ensure_terminal_restored(&session, cleanup_start, "Codex-only AI mode EOF")
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn check_durable_command_discovery(binary: &Path) -> Result<(), TaskError> {
    // Failure model: a cold write may be partial, a warm read may rewrite state,
    // refresh may publish inside an editor turn, hostile declarations may run,
    // and corrupt state may prevent prompt or terminal cleanup. This sequence
    // retains one isolated namespace so byte-for-byte transitions stay causal.
    let fixtures = TempDirectory::new("quirl-durable-discovery")?;
    let binary_dir = fixtures.path.join("bin");
    let help_dir = fixtures.path.join("help");
    let index_dir = fixtures.path.join("index");
    create_private_directory(&binary_dir)?;
    create_private_directory(&help_dir)?;
    create_private_directory(&index_dir)?;
    let hostile_marker = fixtures.path.join("HOSTILE_CONTENT_EXECUTED");
    write_executable(
        &binary_dir.join("cold-tool"),
        &format!(
            "#!/bin/sh\n/usr/bin/touch {}\n",
            shell_quote(&hostile_marker)
        ),
    )?;
    write_fixture(
        &help_dir.join("declarative-cold.help"),
        &format!(
            "Usage: declarative-cold [options]\n  --safe  Safe fixture option\n/usr/bin/touch {}\n",
            shell_quote(&hostile_marker)
        ),
    )?;
    let catalog_path = index_dir.join("catalog.sqlite3");

    let mut cold = discovery_session(binary, &binary_dir, &index_dir, &help_dir)?;
    cold.pty.wait_for(STARTUP_MARKER)?;
    wait_for_file(&mut cold, catalog_path.clone())?;
    assert_discovery_artifacts_bounded(&index_dir)?;
    wait_for_command_information(
        &mut cold,
        "cold-tool",
        &["Installed command discovered", "source: external"],
    )?;
    clear_editor(&mut cold)?;
    wait_for_command_information(
        &mut cold,
        "declarative-cold",
        &["Command discovered from supplied", "source: help-import"],
    )?;
    if hostile_marker.exists() {
        return Err(io::Error::other("cold discovery executed hostile fixture content").into());
    }
    cold.pty.send(key::ESCAPE)?;
    wait_for_standard_status(&mut cold)?;
    clear_editor(&mut cold)?;
    cold.pty.send(key::ALT_Q)?;
    cold.pty.send(b"d")?;
    cold.pty
        .wait_for_screen("data mode before natural search", |screen| {
            screen.bottom_line().contains("DATA")
        })?;
    cold.pty.send(key::ALT_Q)?;
    cold.pty.send(b"i")?;
    cold.pty
        .wait_for_screen("AI mode before command search", |screen| {
            screen.bottom_line().contains("AI")
        })?;
    let natural_output_start = cold.pty.output().len();
    cold.pty.type_text("installed command discovered")?;
    cold.pty.wait_for_screen("Codex-only AI intent", |screen| {
        screen.text().contains("installed command discovered")
            && screen.bottom_line().contains("Enter send")
    })?;
    let natural_output = &cold.pty.output()[natural_output_start..];
    if contains(natural_output, ALTERNATE_SCREEN_LEAVE) {
        return Err(io::Error::other(
            "AI input released the rich session instead of updating it in place",
        )
        .into());
    }
    cold.pty.send(key::TAB)?;
    cold.pty
        .wait_for_screen("Tab does not accept a local AI suggestion", |screen| {
            screen.bottom_line().contains("Enter send")
                && screen.text().contains("installed command discovered")
        })?;
    if contains(
        &cold.pty.output()[natural_output_start..],
        ALTERNATE_SCREEN_LEAVE,
    ) {
        return Err(io::Error::other("AI-mode Tab released the rich session").into());
    }
    clear_editor_in_mode(&mut cold, "AI")?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut cold.pty)?,
        0,
        "cold discovery session",
    )?;
    let cold_catalog = read_bounded_fixture(&catalog_path, DISCOVERY_ARTIFACT_BYTES_MAX)?;
    drop(cold);

    let mut warm = discovery_session(binary, &binary_dir, &index_dir, &help_dir)?;
    warm.pty.wait_for(STARTUP_MARKER)?;
    if read_bounded_fixture(&catalog_path, DISCOVERY_ARTIFACT_BYTES_MAX)? != cold_catalog {
        return Err(io::Error::other("warm discovery rewrote matching durable state").into());
    }
    wait_for_command_information(
        &mut warm,
        "cold-tool",
        &["Installed command discovered", "source: external"],
    )?;
    clear_editor(&mut warm)?;

    write_executable(
        &binary_dir.join("changed-path-tool"),
        &format!(
            "#!/bin/sh\n/usr/bin/touch {}\n",
            shell_quote(&hostile_marker)
        ),
    )?;
    write_fixture(
        &help_dir.join("changed-declaration.help"),
        &format!(
            "Usage: changed-declaration [options]\n  --safe  Changed fixture option\n/usr/bin/touch {}\n",
            shell_quote(&hostile_marker)
        ),
    )?;
    execute_and_resume(&mut warm, "cd .")?;
    wait_for_file_contents(&mut warm, &catalog_path, b"changed-path-tool")?;
    wait_for_file_contents(&mut warm, &catalog_path, b"changed-declaration")?;
    wait_for_file_contents(&mut warm, &catalog_path, b"changed-declaration.help")?;
    execute_and_resume(&mut warm, "cd .")?;
    wait_for_command_information(
        &mut warm,
        "changed-path-tool",
        &["Installed command discovered", "source: external"],
    )?;
    clear_editor(&mut warm)?;
    wait_for_command_information(
        &mut warm,
        "changed-declaration",
        &["Command discovered from supplied", "source: help-import"],
    )?;
    if hostile_marker.exists() {
        return Err(io::Error::other("refresh executed hostile fixture content").into());
    }
    warm.pty.send(key::ESCAPE)?;
    wait_for_standard_status(&mut warm)?;
    clear_editor(&mut warm)?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut warm.pty)?,
        0,
        "warm discovery session",
    )?;
    if read_bounded_fixture(&catalog_path, DISCOVERY_ARTIFACT_BYTES_MAX)? == cold_catalog {
        return Err(
            io::Error::other("changed discovery sources did not replace durable state").into(),
        );
    }
    assert_discovery_artifacts_bounded(&index_dir)?;

    let corrupt = TempDirectory::new("quirl-corrupt-discovery")?;
    let corrupt_index = corrupt.path.join("index");
    create_private_directory(&corrupt_index)?;
    write_fixture(
        &corrupt_index.join("catalog.sqlite3"),
        "not a sqlite command database",
    )?;
    let _permissions = DirectoryPermissionsGuard::make_read_only(&corrupt_index)?;
    let mut degraded = discovery_session(binary, &binary_dir, &corrupt_index, &help_dir)?;
    degraded
        .pty
        .wait_for_screen("corrupt-cache fallback frame", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    degraded.pty.send(key::ALT_Q)?;
    degraded.pty.send(b"d")?;
    degraded
        .pty
        .wait_for_screen("corrupt-cache Data fallback frame", |screen| {
            screen.bottom_line().contains("DATA")
        })?;
    wait_for_command_information(
        &mut degraded,
        "ls",
        &[
            "List a directory as typed entries in Data mode",
            "Capabilities:",
        ],
    )?;
    write_fixture(
        &degraded.private.path.join("FALLBACK_BUILTIN_VISIBLE"),
        "visible to fallback ls\n",
    )?;
    let fallback_start = degraded.pty.output().len();
    degraded.pty.send(key::ENTER)?;
    degraded.pty.wait_for(b"FALLBACK_BUILTIN_VISIBLE")?;
    degraded
        .pty
        .wait_for_screen("corrupt-cache fallback returned", |screen| {
            screen.bottom_line().contains("DATA")
        })?;
    wait_for_rich_input_since(&mut degraded, fallback_start)?;
    let cleanup_start = degraded.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut degraded.pty)?,
        0,
        "corrupt-cache fallback",
    )?;
    ensure_terminal_restored(&degraded, cleanup_start, "corrupt-cache fallback")
}

fn check_completion(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    let path_target = session.private.path.join("path-target");
    create_private_directory(&path_target)?;
    create_private_directory(&session.private.path.join("path with space"))?;
    fs::write(
        session.private.path.join("path-target.txt"),
        b"not a directory\n",
    )?;
    fs::write(path_target.join("notes.txt"), b"PATH_FILE_OK\n")?;
    session.pty.wait_for(STARTUP_MARKER)?;

    session
        .pty
        .type_text(&format!("cd {}/path-t", session.private.path.display()))?;
    session.pty.send(b"\t")?;
    session
        .pty
        .wait_for_screen("absolute directory completion", |screen| {
            let text = screen.text();
            text.contains("path-target/") && !text.contains("path-target.txt")
        })?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("accepted directory completion", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.starts_with("> cd ") && line.ends_with("/path-target/"))
        })?;
    let cd_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("silent cd transcript", |screen| {
            let text = screen.text();
            text.contains("❯ cd ")
                && text.contains("path-target/")
                && text.contains("── exit 0")
                && !text.contains("(no output)")
        })?;
    wait_for_rich_input_since(&mut session, cd_start)?;

    session.pty.type_text("cat no")?;
    session.pty.send(b"\t")?;
    session.pty.wait_for_screen_text("notes.txt")?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("accepted file completion", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.trim() == "> cat notes.txt")
        })?;
    let cat_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session.pty.wait_for_screen_text("PATH_FILE_OK")?;
    wait_for_rich_input_since(&mut session, cat_start)?;

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
    session
        .pty
        .wait_for_screen("accepted git completion", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.trim() == "> git status")
        })?;
    let git_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session.pty.wait_for_screen_text("not a git repository")?;
    wait_for_rich_input_since(&mut session, git_start)?;
    session.pty.type_text("git")?;
    session.pty.send(b"\x1b[Z")?;
    session.pty.wait_for_screen_text("picker")?;
    session.pty.send(b"\x1b[200~zzzz-no-match\x1b[201~")?;
    session.pty.wait_for(b"zzzz-no-match")?;
    session.pty.send(key::ESCAPE)?;
    session.pty.drain_for(Duration::from_millis(100))?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    send_ctrl_d_and_wait_for_exit(&mut session.pty)?;
    Ok(())
}

fn check_deferred_catalog_admission(binary: &Path) -> Result<(), TaskError> {
    check_cold_context_help(binary)?;
    check_cold_catalog_intents(binary)?;
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
    session
        .pty
        .send(b"\x1b[200~/usr/bin/printf QUEUED_AFTER_ADMISSION\x1b[201~\r")?;
    session
        .pty
        .wait_for_screen("input while catalog loading", |screen| {
            screen.text().contains("QUEUED_AFTER_ADMISSION")
        })?;
    fs::write(&session.catalog_gate, b"release\n")?;
    let catalog_published = PathBuf::from(format!("{}.published", session.catalog_gate.display()));
    wait_for_file(&mut session, catalog_published)?;
    session.pty.resize(30, 120)?;
    session
        .pty
        .wait_for_screen("queued command transcript while catalog loads", |screen| {
            let text = screen.text();
            text.contains("❯ /usr/bin/printf QUEUED_AFTER_ADMISSION")
                && text.contains("QUEUED_AFTER_ADMISSION")
                && screen.bottom_line().contains("result kept in viewport")
        })?;
    session.pty.type_text("git st")?;
    session.pty.send(b"\t")?;
    session.pty.wait_for(b"git status [--short]")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    send_ctrl_d_and_wait_for_exit(&mut session.pty)?;
    Ok(())
}

fn check_cold_context_help(binary: &Path) -> Result<(), TaskError> {
    // Failure model: F1 before catalog publication used to disappear silently.
    // Hold the real admission worker until builtin context help is visible,
    // then prove publication preserves both that context and the original input.
    // Every failure still drops the owning Session and restores/reaps its PTY.
    let mut session = Session::new(
        binary,
        SessionOptions {
            catalog_gate: true,
            ..SessionOptions::default()
        },
    )?;
    let reached = session.catalog_gate_reached.clone();
    wait_for_file(&mut session, reached)?;
    let command = "git status | quirl data COLD_F1_PRESERVED";
    let editor_line = format!("> {command}");
    session.pty.type_text(command)?;
    session
        .pty
        .wait_for_screen("cold F1 input is complete", |screen| {
            screen.lines().iter().any(|line| line == &editor_line)
        })?;
    session.pty.send(b"\x1bOP")?;
    let context_help = |screen: &VirtualScreen| {
        let text = screen.text();
        text.contains("catalog help")
            && screen.lines().iter().any(|line| {
                line.split('│')
                    .nth(1)
                    .is_some_and(|cell| cell.trim() == "> quirl data")
                    && line
                        .split('│')
                        .nth(3)
                        .is_some_and(|cell| cell.trim() == "quirl data")
            })
            && text.contains("Sources are")
    };
    session.pty.wait_for_screen(
        "cold F1 builtin context help before publication",
        context_help,
    )?;
    fs::write(&session.catalog_gate, b"release\n")?;
    let published = PathBuf::from(format!("{}.published", session.catalog_gate.display()));
    wait_for_file(&mut session, published)?;
    session
        .pty
        .wait_for_screen("cold F1 context survives catalog publication", context_help)?;
    session.pty.send(key::ESCAPE)?;
    session
        .pty
        .wait_for_screen("cold F1 preserves original editor", |screen| {
            screen.lines().iter().any(|line| line == &editor_line)
                && !screen.text().contains("catalog help")
        })?;
    session.pty.send(key::CTRL_C)?;
    session
        .pty
        .wait_for_screen("cold F1 cancellation restores empty input", |screen| {
            screen.lines().iter().any(|line| line.trim() == ">")
                && screen.text().contains("interactive input cancelled")
                && screen.bottom_line().contains("NORMAL")
        })?;
    execute_and_resume_with_marker(
        &mut session,
        "/usr/bin/printf COLD_F1_RECOVERED",
        b"COLD_F1_RECOVERED",
    )?;
    let cleanup_start = session.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "cold F1 help",
    )?;
    ensure_terminal_restored(&session, cleanup_start, "cold F1 help")
}

fn check_cold_catalog_intents(binary: &Path) -> Result<(), TaskError> {
    for palette in [false, true] {
        let mut session = Session::new(
            binary,
            SessionOptions {
                catalog_gate: true,
                ..SessionOptions::default()
            },
        )?;
        let reached = session.catalog_gate_reached.clone();
        wait_for_file(&mut session, reached)?;
        if palette {
            session
                .pty
                .type_text("/usr/bin/printf COLD_PALETTE_PRESERVED")?;
            session.pty.send(key::ALT_Q)?;
            session.pty.send(b"p")?;
            session.pty.wait_for_screen_text("picker")?;
            session.pty.type_text("doctor")?;
            session.pty.wait_for_screen_text("doctor")?;
        } else {
            session.pty.type_text("git st")?;
            session.pty.send(key::TAB)?;
            session.pty.wait_for_screen_text("loading catalog")?;
        }
        fs::write(&session.catalog_gate, b"release\n")?;
        let published = PathBuf::from(format!("{}.published", session.catalog_gate.display()));
        wait_for_file(&mut session, published)?;
        if palette {
            session.pty.wait_for_screen_text("quirl config doctor")?;
            session.pty.send(key::ESCAPE)?;
            session.pty.wait_for_screen_text("COLD_PALETTE_PRESERVED")?;
        } else {
            session.pty.wait_for_screen_text("git status [--short]")?;
        }
        session.pty.send(key::CTRL_C)?;
        session
            .pty
            .wait_for_screen("cold intent cancellation restores input", |screen| {
                screen.text().contains("interactive input cancelled")
                    && screen.bottom_line().contains("NORMAL")
            })?;
        execute_and_resume_with_marker(
            &mut session,
            "/usr/bin/printf COLD_INTENT_RECOVERED",
            b"COLD_INTENT_RECOVERED",
        )?;
        let start = session.pty.output().len();
        session.pty.send(key::CTRL_D)?;
        ensure_status(session.pty.wait_exit()?, 0, "cold catalog intent")?;
        ensure_terminal_restored(&session, start, "cold catalog intent")?;
    }
    Ok(())
}

fn check_catalog_failure_restores_terminal(binary: &Path) -> Result<(), TaskError> {
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
    // `reset_best_effort` is the last-resort cleanup Drop runs when unwinding
    // through exactly this kind of fault, possibly with the terminal already
    // in a bad state. It must never query the terminal for its cursor
    // position (`ESC[6n`, answered synchronously off stdin) to repaint: a
    // terminal that doesn't answer promptly would hang the exit this test
    // just proved happens (`wait_exit` above already bounds that, but assert
    // the cause directly rather than only its bounded symptom).
    if contains(observed, b"\x1b[6n") {
        return Err(io::Error::other(
            "catalog failure cleanup queried the terminal's cursor position instead of \
             repainting from already-known state",
        )
        .into());
    }
    let modes = session.pty.terminal_modes()?;
    if !modes.local_flags.contains(LocalFlags::ICANON)
        || !modes.local_flags.contains(LocalFlags::ECHO)
    {
        return Err(io::Error::other("catalog failure did not restore cooked modes").into());
    }
    Ok(())
}

fn check_cwd_history(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    let local_directory = session.private.path.join("local-project");
    let other_directory = session.private.path.join("other-project");
    create_private_directory(&local_directory)?;
    create_private_directory(&other_directory)?;
    session.pty.wait_for(STARTUP_MARKER)?;

    execute_cwd_history_command(
        &mut session,
        &format!("cd {}", shell_quote(&local_directory)),
    )?;
    execute_cwd_history_command(&mut session, "/usr/bin/printf LOCAL_HISTORY_CHOICE")?;

    execute_cwd_history_command(
        &mut session,
        &format!("cd {}", shell_quote(&other_directory)),
    )?;
    execute_cwd_history_command(&mut session, "/usr/bin/printf OTHER_HISTORY_CHOICE")?;

    execute_cwd_history_command(
        &mut session,
        &format!("cd {}", shell_quote(&local_directory)),
    )?;
    session.pty.send(b"\x1b[A")?;
    session
        .pty
        .wait_for_screen("cwd-aware fuzzy history", |screen| {
            let text = screen.text();
            text.contains("history")
                && text.contains("LOCAL_HISTORY_CHOICE")
                && text.contains("OTHER_HISTORY_CHOICE")
        })?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen_text("/usr/bin/printf LOCAL_HISTORY_CHOICE")?;
    clear_editor(&mut session)?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "cwd history",
    )?;
    if !session.private.path.join("history.sqlite3").is_file() {
        return Err(io::Error::other("cwd-aware SQLite history was not created").into());
    }
    Ok(())
}

fn execute_cwd_history_command(session: &mut Session, command: &str) -> Result<(), TaskError> {
    let output_start = session.pty.output().len();
    session.pty.type_text(command)?;
    let editor_line = format!("> {command}");
    session
        .pty
        .wait_for_screen("complete cwd-history command in editor", |screen| {
            screen.lines().iter().any(|line| line == &editor_line)
        })?;
    session.pty.send(key::ENTER)?;
    let command_record = format!("❯ {command}");
    session
        .pty
        .wait_for_screen("completed cwd-history command", |screen| {
            let text = screen.text();
            text.contains(&command_record)
                && text.contains("── exit ")
                && screen.bottom_line().contains("NORMAL")
        })?;
    wait_for_rich_input_since(session, output_start)?;
    ensure_alternate_screen_unchanged(session, output_start, "cwd-history command")
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "cycle counts are fixed and captured output offsets come from successful marker searches"
)]
fn check_retained_output_cycles(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(30),
            columns: Some(120),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    for cycle in 0..12 {
        let command = format!("/usr/bin/printf RETAINED_CYCLE_{cycle:02}");
        let output_start = session.pty.output().len();
        session.pty.type_text(&command)?;
        session.pty.send(key::ENTER)?;
        let marker = format!("RETAINED_CYCLE_{cycle:02}");
        session.pty.wait_for_screen(
            &format!("persistent viewport command cycle {cycle}"),
            |screen| {
                transcript_tail_flows_into_prompt(screen, &command, &marker)
                    && screen.bottom_line().contains("NORMAL")
            },
        )?;
        wait_for_rich_input_since(&mut session, output_start)?;
        let emitted = &session.pty.output()[output_start..];
        if contains(emitted, ALTERNATE_SCREEN_LEAVE) || contains(emitted, ALTERNATE_SCREEN_ENTER) {
            return Err(io::Error::other(format!(
                "persistent viewport cycle {cycle} changed alternate-screen ownership"
            ))
            .into());
        }
        let screen = session.pty.screen();
        if !screen.text().contains(&command) {
            return Err(screen_error(
                &format!("persistent viewport cycle {cycle} lost its command record"),
                screen,
            ));
        }
        if !screen.text().contains(&marker) {
            return Err(screen_error(
                &format!("persistent viewport cycle {cycle} lost command output"),
                screen,
            ));
        }
        ensure_bottom_status(&session, "persistent command viewport")?;
    }
    let tail_snapshot = session.pty.screen().text();
    session.pty.send(b"\x1b[5~")?;
    session
        .pty
        .wait_for_screen("PageUp changed the transcript viewport", |screen| {
            let text = screen.text();
            text != tail_snapshot
                && text.contains("RETAINED_CYCLE_00")
                && !text.contains("RETAINED_CYCLE_11")
                && screen.bottom_line().contains("NORMAL")
        })?;
    ensure_bottom_status(&session, "scrolled persistent viewport")?;
    session.pty.send(b"\x1b[6~")?;
    session
        .pty
        .wait_for_screen("PageDown returned to transcript tail", |screen| {
            transcript_tail_flows_into_prompt(
                screen,
                "/usr/bin/printf RETAINED_CYCLE_11",
                "RETAINED_CYCLE_11",
            ) && screen.bottom_line().contains("NORMAL")
        })?;

    session.pty.send(b"\x1b[<64;1;1M")?;
    session
        .pty
        .wait_for_screen("mouse wheel up changed the transcript viewport", |screen| {
            let text = screen.text();
            text != tail_snapshot
                && screen.bottom_line().contains("SCROLL")
                && screen.bottom_line().contains("NORMAL")
        })?;
    session.pty.send(b"\x1b[<65;1;1M")?;
    session
        .pty
        .wait_for_screen("mouse wheel down returned to transcript tail", |screen| {
            transcript_tail_flows_into_prompt(
                screen,
                "/usr/bin/printf RETAINED_CYCLE_11",
                "RETAINED_CYCLE_11",
            ) && screen.bottom_line().contains("NORMAL")
        })?;

    let (selection_row, selection_column) = session
        .pty
        .screen()
        .lines()
        .iter()
        .enumerate()
        .rev()
        .find_map(|(row, line)| {
            line.find("RETAINED_CYCLE_11")
                .filter(|_| line.trim_start().starts_with("RETAINED_CYCLE_11"))
                .map(|column| (row, column))
        })
        .ok_or_else(|| io::Error::other("retained output marker was not visible for selection"))?;
    let selection_end = selection_column.saturating_add("RETAINED_CYCLE_11".len() - 1);
    let mouse_down = format!(
        "\x1b[<0;{};{}M",
        selection_column.saturating_add(1),
        selection_row.saturating_add(1)
    );
    let mouse_drag = format!(
        "\x1b[<32;{};{}M",
        selection_end.saturating_add(1),
        selection_row.saturating_add(1)
    );
    let mouse_up = format!(
        "\x1b[<0;{};{}m",
        selection_end.saturating_add(1),
        selection_row.saturating_add(1)
    );
    let copy_start = session.pty.output().len();
    session.pty.send(mouse_down.as_bytes())?;
    session.pty.send(mouse_drag.as_bytes())?;
    session.pty.send(mouse_up.as_bytes())?;
    session
        .pty
        .wait_for_screen("mouse output selection copied", |screen| {
            screen.bottom_line().contains("copied 17 bytes")
        })?;
    if !contains(
        &session.pty.output()[copy_start..],
        b"\x1b]52;c;UkVUQUlORURfQ1lDTEVfMTE=\x07",
    ) {
        return Err(io::Error::other(
            "mouse-selected output did not emit the exact bounded OSC 52 payload",
        )
        .into());
    }
    session.pty.send(key::ESCAPE)?;
    session
        .pty
        .wait_for_screen("mouse selection dismissed", |screen| {
            screen.bottom_line().contains("NORMAL")
                && !screen.bottom_line().contains("OUTPUT")
                && !screen.bottom_line().contains("copied")
        })?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "retained-output cycles",
    )
}

fn check_external_command_compatibility(binary: &Path) -> Result<(), TaskError> {
    let fixtures = TempDirectory::new("quirl-external-compatibility")?;
    let binary_dir = fixtures.path.join("bin");
    create_private_directory(&binary_dir)?;
    let finished = fixtures.path.join("GHQ_FINISHED");
    write_executable(
        &binary_dir.join("ghq"),
        &format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
               get)\n\
                 printf '\\033[0;32mclone\\033[0m %s\\n' \"$2\"\n\
                 printf 'progress 10%%\\rprogress 20%%\\n'\n\
                 /bin/sleep 5\n\
                 printf 'clone complete\\n'\n\
                 : > {}\n\
                 ;;\n\
               list) printf 'github.com/example/repository\\n' ;;\n\
               *) printf 'unexpected ghq arguments\\n' >&2; exit 64 ;;\n\
             esac\n",
            shell_quote(&finished)
        ),
    )?;
    let fish = r#"
complete -c ghq -f
complete -c ghq -s h -l help -d 'Show help'
complete -c ghq -n __fish_ghq_needs_subcommand -a get -d 'Clone/sync with a remote repository'
complete -c ghq -n __fish_ghq_needs_subcommand -a list -d 'List local repositories'
complete -c ghq -n __fish_ghq_needs_subcommand -a root -d 'Show repositories root'
complete -c ghq -n '__fish_seen_subcommand_from get' -s u -l update -d 'Update local repository if cloned already'
complete -c ghq -n '__fish_seen_subcommand_from list' -s p -l full-path -d 'Print full paths'
"#;
    let index_dir = fixtures.path.join("index");
    create_private_directory(&index_dir)?;
    let mut session = Session::new(
        binary,
        SessionOptions {
            path: Some(binary_dir),
            index_dir: Some(index_dir.clone()),
            catalog_refresh_enabled: true,
            fish_completion: Some(fish.to_owned()),
            symbols: Some("unicode"),
            rows: Some(20),
            columns: Some(160),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session
        .pty
        .wait_for_screen("development build identity in status bar", |screen| {
            let bottom = screen.bottom_line();
            bottom.contains("🌀") && bottom.contains("dev@")
        })?;
    wait_for_file_contents(&mut session, &index_dir.join("catalog.sqlite3"), b"ghq get")?;
    let wide_resize_start = session.pty.output().len();
    session.pty.resize(20, 400)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", wide_resize_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("wide external-command frame", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    wait_for_command_information(
        &mut session,
        "ghq",
        &[
            "subcommands available",
            "Clone/sync with a remote repository",
            "List local repositories",
            "source: fish-import",
        ],
    )?;
    let information = session.pty.screen().text();
    if information.contains("ghq _c")
        || information.contains("ghq Commands")
        || information.contains("Subcommand imported from Zsh")
        || information.contains("Command discovered from Fish completion metadata")
    {
        return Err(screen_error(
            "GHQ completion exposed importer internals instead of command documentation",
            session.pty.screen(),
        ));
    }
    session.pty.send(key::ESCAPE)?;
    clear_editor(&mut session)?;
    let narrow_resize_start = session.pty.output().len();
    session.pty.resize(20, 160)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", narrow_resize_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("narrow external-command frame", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    let command = "ghq get git@github.com:niklas-heer/homebrew-tap.git";
    let output_start = session.pty.output().len();
    session.pty.type_text(command)?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("GHQ progress streamed before process exit", |screen| {
            let text = screen.text();
            text.contains("clone git@github.com:niklas-heer/homebrew-tap.git")
                && text.contains("progress 20%")
                && !text.contains("clone complete")
                && screen.bottom_line().contains("running")
        })?;
    if finished.exists() {
        return Err(io::Error::other(
            "GHQ fixture completed before its first progress frame was observable",
        )
        .into());
    }
    if session.pty.screen().text().contains("\\u{1b}")
        || session.pty.screen().text().contains("progress 10%")
    {
        return Err(screen_error(
            "streamed ANSI or carriage-return progress was rendered as literal escape text",
            session.pty.screen(),
        ));
    }
    session.pty.wait_for_screen(
        "GHQ command completed inside persistent viewport",
        |screen| {
            let text = screen.text();
            text.contains("clone complete")
                && text.contains("── exit 0")
                && screen.bottom_line().contains("NORMAL")
                && screen.bottom_line().contains("dev@")
        },
    )?;
    if !finished.is_file() {
        return Err(io::Error::other("GHQ fixture did not reach process completion").into());
    }
    wait_for_rich_input_since(&mut session, output_start)?;
    ensure_alternate_screen_unchanged(&session, output_start, "streamed GHQ fixture")?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "external command compatibility",
    )
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn check_streamed_progress_without_newline(binary: &Path) -> Result<(), TaskError> {
    // Failure model: a child that reports progress with bare `\r` overwrites
    // (no trailing `\n`) — `git push`, `curl`, package-manager progress bars,
    // and similar — must still be visible while it runs. A transcript that
    // only commits complete lines would sit frozen for the whole operation
    // and dump every update at once on exit, which is indistinguishable from
    // a hang. Each `printf` below is its own write separated by a real
    // `sleep`, so a passing check proves each `\r` update reached the screen
    // as it happened rather than being coalesced into the final flush.
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let output_start = session.pty.output().len();
    // Quote printf formats inside the fixture shell too: unquoted backslashes
    // are consumed by that shell and would print literal `r` instead of CR.
    let command = r#"/bin/sh -c 'printf "start\n" >&2; printf "33%%\r" >&2; sleep 1; printf "66%%\r" >&2; sleep 1; printf "100%%done\n" >&2'"#;
    session.pty.type_text(command)?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("first progress frame live", |screen| {
            screen.lines().iter().any(|line| line.trim() == "33%")
                && screen.bottom_line().contains("running")
        })?;
    if session.pty.screen().text().contains("100%done") {
        return Err(io::Error::other(
            "progress fixture completed before its first `\\r` frame was observable",
        )
        .into());
    }
    session
        .pty
        .wait_for_screen("second progress frame live", |screen| {
            screen.lines().iter().any(|line| line.trim() == "66%")
                && screen.bottom_line().contains("running")
        })?;
    session.pty.wait_for_screen(
        "progress fixture completed inside persistent viewport",
        |screen| {
            let text = screen.text();
            text.contains("100%done")
                && text.contains("── exit 0")
                && screen.bottom_line().contains("NORMAL")
        },
    )?;
    wait_for_rich_input_since(&mut session, output_start)?;
    if contains(&session.pty.output()[output_start..], b"\\u{1b}") {
        return Err(screen_error(
            "streamed carriage-return progress leaked a literal escape sequence",
            session.pty.screen(),
        ));
    }
    ensure_alternate_screen_unchanged(&session, output_start, "streamed carriage-return progress")?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "streamed progress without newline",
    )
}

fn check_spinner_animates_during_silent_command(binary: &Path) -> Result<(), TaskError> {
    // Failure model: a command that produces no output of its own (a bare
    // `sleep`) must still show Quirl is alive and waiting on it. Without a
    // liveness tick independent of child output, the viewport would sit
    // frozen on the very first frame for the command's whole duration,
    // indistinguishable from a hang. This is distinct from
    // `check_streamed_progress_without_newline`, which proves a child's own
    // `\r` progress reaches the screen live; this proves Quirl's own
    // heartbeat does, with zero bytes from the child at all.
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.type_text("/bin/sleep 2")?;
    let execution_start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("spinner shows the command is running", |screen| {
            screen.bottom_line().contains("running")
        })?;
    let first = session.pty.screen().bottom_line();
    session
        .pty
        .wait_for_screen("spinner advances without any child output", |screen| {
            screen.bottom_line().contains("running") && screen.bottom_line() != first
        })?;
    let second = session.pty.screen().bottom_line();
    if second == first {
        return Err(io::Error::other(
            "the running-command status line never changed while the silent child was still \
             executing",
        )
        .into());
    }
    session.pty.wait_for_screen(
        "silent command completed inside persistent viewport",
        |screen| screen.text().contains("── exit 0") && screen.bottom_line().contains("NORMAL"),
    )?;
    wait_for_rich_input_since(&mut session, execution_start)?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "spinner animates during silent command",
    )
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn check_full_screen_program_takeover(binary: &Path) -> Result<(), TaskError> {
    // Failure model: the rich viewport normally captures a foreground
    // command's stdout and stderr through a pipe and replays it inside its
    // own transcript block. A full-screen program (an editor, pager, or
    // similar) instead needs the real terminal: its own alternate screen,
    // absolute cursor addressing, and live keystrokes. A fixture named
    // `vim` proves the fix reaches the real terminal rather than a
    // transcript-safe imitation of it: raw `\x1b[?1049h`/`\x1b[?1049l`
    // bytes on the wire can only come from a real inherited terminal, since
    // captured output is escaped before it ever reaches the transcript (see
    // `check_streamed_progress_without_newline`'s literal-escape assertion).
    let fixtures = TempDirectory::new("quirl-full-screen-takeover")?;
    let binary_dir = fixtures.path.join("bin");
    create_private_directory(&binary_dir)?;
    write_executable(
        &binary_dir.join("vim"),
        "#!/bin/sh\n\
         printf '\\033[?1049h'\n\
         printf 'FIXTURE_READY\\n'\n\
         read line\n\
         printf 'GOT:%s\\n' \"$line\"\n\
         printf '\\033[?1049l'\n",
    )?;
    let mut session = Session::new(
        binary,
        SessionOptions {
            path: Some(binary_dir),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let output_start = session.pty.output().len();
    session.pty.type_text("vim")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"FIXTURE_READY")?;
    if !contains(&session.pty.output()[output_start..], b"\x1b[?1049h") {
        return Err(io::Error::other(
            "full-screen fixture never entered a real alternate screen; \
             its output is still being captured instead of inherited",
        )
        .into());
    }
    session.pty.type_text("hello")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"GOT:hello")?;
    session
        .pty
        .wait_for_since(b"\x1b[?1049l", output_start, default_timeout())?;
    session
        .pty
        .wait_for_screen("rich viewport reacquired after takeover", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    execute_and_resume(&mut session, "/usr/bin/printf AFTER_%s TAKEOVER")?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "full-screen program takeover",
    )
}

fn check_full_screen_program_spawn_failure_restores_terminal(
    binary: &Path,
) -> Result<(), TaskError> {
    // Failure model: `needs_real_terminal` decides to hand a command the
    // real terminal from its parsed source text alone, before the
    // executable is known to exist. When the recognized full-screen program
    // is missing from PATH, the spawn fails after the alternate screen has
    // already been released for it. `resume_after_terminal_takeover` must
    // still run — the rich viewport has to be reacquired and repainted
    // before the spawn error is shown, never left stranded on the
    // takeover's half-cleared real-terminal frame.
    let fixtures = TempDirectory::new("quirl-full-screen-spawn-failure")?;
    let binary_dir = fixtures.path.join("bin");
    create_private_directory(&binary_dir)?;
    let mut session = Session::new(
        binary,
        SessionOptions {
            path: Some(binary_dir),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.type_text("lazygit")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for_screen(
        "spawn failure surfaced after reacquiring the rich viewport",
        |screen| {
            let text = screen.text();
            text.contains("could not start") && screen.bottom_line().contains("NORMAL")
        },
    )?;
    execute_and_resume(&mut session, "/usr/bin/printf AFTER_SPAWN_FAILURE")?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "full-screen program spawn failure",
    )
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn check_ctrl_l_forces_full_repaint(binary: &Path) -> Result<(), TaskError> {
    // Failure model: a raw ANSI clear wipes the real screen but does not by
    // itself invalidate ratatui's internal diff buffer, so the next draw
    // would re-emit only cells whose modeled content changed, leaving the
    // freshly wiped screen blank wherever nothing changed. Ctrl-L must force
    // a full repaint of the current frame instead. It must also do so
    // without `Terminal::clear`'s blocking cursor-position query
    // (`ESC[6n`, answered synchronously off stdin): this fixture's fake PTY
    // never answers device-status reports, so a Ctrl-L implementation that
    // depends on one would hang here rather than repaint.
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let since = session.pty.output().len();
    session.pty.send(key::CTRL_L)?;
    session
        .pty
        .wait_for_since(b"\x1b[J", since, default_timeout())?;
    if contains(&session.pty.output()[since..], b"\x1b[6n") {
        return Err(io::Error::other(
            "Ctrl-L queried the terminal's cursor position instead of \
             repainting from already-known state",
        )
        .into());
    }
    session
        .pty
        .wait_for_screen("status bar repainted after Ctrl-L", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "Ctrl-L full repaint",
    )
}

fn check_local_completion_discovery(binary: &Path) -> Result<(), TaskError> {
    // Failure model: provider code is untrusted and editor revisions can repeat
    // while one background generation is running. Fake shells make invocation,
    // framing, and persistence deterministic without host rc files or binaries.
    let fixtures = TempDirectory::new("quirl-local-completion")?;
    let binary_dir = fixtures.path.join("bin");
    let index_dir = fixtures.path.join("index");
    create_private_directory(&binary_dir)?;
    create_private_directory(&index_dir)?;
    let calls = fixtures.path.join("provider-calls");
    let provider = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nprintf 'QLB10000000400000013repomanage repositories0000000600000009--jsonemit JSON'\n",
        shell_quote(&calls)
    );
    for shell in ["fish", "zsh"] {
        write_executable(&binary_dir.join(shell), &provider)?;
    }
    write_executable(&binary_dir.join("ghq"), "#!/bin/sh\nexit 0\n")?;
    let mut session = Session::new(
        binary,
        SessionOptions {
            path: Some(binary_dir),
            index_dir: Some(index_dir.clone()),
            catalog_refresh_enabled: true,
            fish_completion: Some("# dynamic provider fixture\n".to_owned()),
            rows: Some(20),
            columns: Some(200),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let catalog_path = index_dir.join("catalog.sqlite3");
    if let Err(error) = wait_for_file_contents(&mut session, &catalog_path, b"manage repositories")
    {
        let calls = fs::read_to_string(&calls).unwrap_or_else(|_| "<no calls>".to_owned());
        let database =
            read_bounded_fixture(&catalog_path, DISCOVERY_ARTIFACT_BYTES_MAX).unwrap_or_default();
        let provider_state = database
            .windows(b"\"local_providers\":".len())
            .position(|window| window == b"\"local_providers\":")
            .and_then(|start| database.get(start..start.saturating_add(2_048).min(database.len())))
            .map(String::from_utf8_lossy);
        return Err(io::Error::other(format!(
            "{error}; provider calls={calls:?}; database_has_ghq={} database_has_fish={} provider_state={provider_state:?}; screen=\n{}",
            contains(&database, b"ghq"),
            contains(&database, b"ghq.fish"),
            session.pty.screen().text()
        ))
        .into());
    }
    wait_for_file_contents(&mut session, &catalog_path, b"emit JSON")?;
    execute_and_resume(&mut session, "cd .")?;

    session.pty.type_text("ghq ")?;
    session.pty.send(b"\t")?;
    session
        .pty
        .wait_for_screen("local provider root completion", |screen| {
            let text = screen.text();
            text.contains("repo") && text.contains("manage repositories")
        })?;
    session.pty.send(key::ESCAPE)?;
    clear_editor(&mut session)?;

    session.pty.type_text("ghq repo ")?;
    session.pty.send(b"\t")?;
    wait_for_file_contents(&mut session, &calls, b"repo")?;
    session.pty.send(key::ESCAPE)?;
    clear_editor(&mut session)?;
    execute_and_resume(&mut session, "cd .")?;
    session.pty.type_text("ghq repo --")?;
    session.pty.send(b"\t")?;
    session
        .pty
        .wait_for_screen("incremental nested provider completion", |screen| {
            let text = screen.text();
            text.contains("--json") && text.contains("emit JSON")
        })?;

    session.pty.send(key::ESCAPE)?;
    clear_editor(&mut session)?;
    let cleanup_start = session.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "local completion discovery EOF",
    )?;
    ensure_terminal_restored(&session, cleanup_start, "local completion discovery EOF")?;
    assert_discovery_artifacts_bounded(&index_dir)
}

fn transcript_tail_flows_into_prompt(screen: &VirtualScreen, command: &str, output: &str) -> bool {
    let command_record = format!("❯ {command}");
    let lines = screen.lines();
    let command_row = lines
        .iter()
        .rposition(|line| line.contains(&command_record));
    let output_row = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with(output));
    let exit_row = lines.iter().rposition(|line| line.contains("── exit "));
    let status_row = lines
        .iter()
        .rposition(|line| line.contains("NORMAL") || line.contains("DATA") || line.contains("AI"));
    matches!(
        (command_row, output_row, exit_row, status_row),
        (Some(command_row), Some(output_row), Some(exit_row), Some(status_row))
            if command_row < output_row
                && output_row < exit_row
                && exit_row.saturating_add(2) <= status_row
    )
}

fn check_interactive_runtime(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.resize(4, 40)?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.send(b"\x1b[200~resize-safe\x1b[201~")?;
    session.pty.wait_for(b"resize-safe")?;
    session.pty.send(key::CTRL_C)?;
    session
        .pty
        .wait_for_screen("tiny viewport cancellation status", |screen| {
            screen.text().contains("status:130") && screen.bottom_line().contains("NORMAL")
        })?;
    session.pty.resize(30, 120)?;
    session
        .pty
        .wait_for_screen("restored persistent viewport after resize", |screen| {
            screen.bottom_line().contains("NORMAL")
        })?;
    let background_start = session.pty.output().len();
    session.pty.type_text("/bin/sleep 30 &")?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("rich background-command rejection", |screen| {
            let text = screen.text();
            text.contains("background commands are not available in the rich viewport")
                && text.contains("ui.surface")
                && text.contains("simple")
                && !text.contains("fg job 1")
                && screen.bottom_line().contains("NORMAL")
        })?;
    ensure_alternate_screen_unchanged(
        &session,
        background_start,
        "rich background-command rejection",
    )?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"d")?;
    session
        .pty
        .wait_for_screen("data mode before typed runtime checks", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("DATA") && line.contains("Alt-Q Quirl"))
        })?;
    session.pty.type_text("[1,2]")?;
    session.pty.send(key::ENTER)?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"r")?;
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
    session.pty.wait_for_screen(
        "bounded large data output returned to rich editor",
        |screen| {
            let text = screen.text();
            text.contains("output truncated after 1048576 bytes")
                && text.contains("discarded")
                && screen.bottom_line().contains("DATA")
        },
    )?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"i")?;
    session
        .pty
        .wait_for_screen("AI mode after typed runtime checks", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("AI") && line.contains("Alt-Q Quirl"))
        })?;
    session.pty.send(key::ALT_Q)?;
    session.pty.send(b"n")?;
    session
        .pty
        .wait_for_screen("command mode after typed runtime checks", |screen| {
            screen
                .lines()
                .iter()
                .any(|line| line.contains("NORMAL") && line.contains("Alt-Q Quirl"))
        })?;
    session
        .pty
        .type_text("lua return quirl.process.run('/bin/sleep 30')")?;
    session.pty.send(key::ENTER)?;
    session
        .pty
        .wait_for_screen("Lua deadline error in persistent viewport", |screen| {
            screen.text().contains("exceeded its deadline")
                && screen.bottom_line().contains("NORMAL")
        })?;
    execute_and_resume(
        &mut session,
        "/usr/bin/printf AFTER_%s DATA_CANCEL_RESTORED",
    )?;
    send_ctrl_d_and_wait_for_exit(&mut session.pty)?;
    Ok(())
}

/// Send exactly one EOF after the caller has observed an empty, ready editor.
/// Retrying would hide a missing readiness oracle or a dropped input event.
fn send_ctrl_d_and_wait_for_exit(pty: &mut PtySession) -> Result<i32, TaskError> {
    pty.send(key::CTRL_D)?;
    pty.wait_exit()
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn check_rich_review_regressions(binary: &Path) -> Result<(), TaskError> {
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
    session.pty.wait_for_screen_text("not a git repository")?;
    let long_line = format!("echo {}VIEWPORT-END", "x".repeat(180));
    session.pty.send(b"\x1b[200~")?;
    session.pty.type_text(&long_line)?;
    session.pty.send(b"\x1b[201~")?;
    session.pty.wait_for(b"VIEWPORT-END")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    // The cancellation marker is emitted before the rich surface finishes
    // its replacement frame. Wait for that complete idle frame so Ctrl-D
    // cannot race the cancellation repaint and make this cleanup assertion
    // depend on scheduler timing.
    session
        .pty
        .wait_for_screen("idle prompt after cancellation", |screen| {
            screen.text().contains("interactive input cancelled")
                && screen.bottom_line().contains("NORMAL")
        })?;
    let cleanup_start = session.pty.output().len();
    send_ctrl_d_and_wait_for_exit(&mut session.pty)?;
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
    send_ctrl_d_and_wait_for_exit(&mut no_hints.pty)?;
    Ok(())
}

fn check_suspend_resume(binary: &Path) -> Result<(), TaskError> {
    let Some(shell) = find_on_path("zsh").or_else(|| find_on_path("bash")) else {
        println!("skip: check_suspend_resume (zsh/bash unavailable)");
        return Ok(());
    };
    // zsh's own job-control message says "suspended"; bash's says "Stopped".
    // The standard GitHub-hosted Ubuntu runner does not ship zsh, so this
    // check silently exercised bash there while asserting zsh's wording,
    // which can never match: not flaky, just wrong for whichever shell
    // `find_on_path` actually resolved.
    let is_zsh = shell.file_name().is_some_and(|name| name == "zsh");
    let suspend_marker: &[u8] = if is_zsh { b"suspended" } else { b"Stopped" };
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
    session.pty.wait_for(suspend_marker)?;
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

fn construction_group_from_output(output: &[u8]) -> io::Result<Pid> {
    let marker = b"owned pipeline process group: ";
    let start = output
        .windows(marker.len())
        .rposition(|window| window == marker)
        .ok_or_else(|| io::Error::other("construction failure omitted its owned process group"))?;
    let tail = output
        .get(start.saturating_add(marker.len())..)
        .unwrap_or_default();
    let end = tail
        .iter()
        .take(11)
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .ok_or_else(|| {
            io::Error::other("construction process group line is incomplete or too long")
        })?;
    let digits = tail.get(..end).unwrap_or_default();
    let process_group = std::str::from_utf8(digits)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        .filter(|process_group| *process_group > 1)
        .ok_or_else(|| {
            io::Error::other("construction failure reported an invalid process group")
        })?;
    Ok(Pid::from_raw(process_group))
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "job counts and process identifiers are bounded by the fixed PTY scenario"
)]
fn check_native_job_control(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            surface: Some("simple"),
            ..SessionOptions::default()
        },
    )?;
    session
        .pty
        .wait_for_screen("simple-surface job-control prompt", |screen| {
            screen.lines().iter().any(|line| line.trim() == "normal >")
        })?;
    let prompt_modes = session.pty.terminal_modes()?;
    let child = session
        .pty
        .child_pid()
        .ok_or_else(|| io::Error::other("Quirl exited before job checks"))?;
    if session.pty.foreground_group()? != child {
        return Err(io::Error::other("Quirl did not own terminal").into());
    }
    execute_simple_with_marker(
        &mut session,
        "/bin/sh -c 'test \"$(ps -o tpgid= -p $$)\" -eq $$ && printf TTY_%s OWNED'",
        b"TTY_OWNED",
    )?;
    let race = std::iter::repeat_n("/usr/bin/true | /bin/cat", 8)
        .collect::<Vec<_>>()
        .join("; ");
    execute_simple_with_marker(
        &mut session,
        &format!("{race}; /usr/bin/printf LEADER_%s RACE_OK"),
        b"LEADER_RACE_OK",
    )?;
    let pid_path = session.private.path.join("construction.pid");
    let mut construction_cleanup = ObservedProcessGroupCleanup::new(pid_path.clone());
    // The executor reports its verified owned group before construction unwinds.
    // This proves partial child ownership without requiring the guest to publish
    // its PID before the next stage's redirection fails.
    session
        .pty
        .type_text("/bin/sleep 30 | /bin/cat > /definitely/missing/quirl-construction-output")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"cannot write redirected output")?;
    let group_deadline = Instant::now() + default_timeout();
    let observed_group = loop {
        match construction_group_from_output(session.pty.output()) {
            Ok(group) => break group,
            Err(error) if Instant::now() >= group_deadline => return Err(error.into()),
            Err(_) => {
                session.pty.drain_for(Duration::from_millis(20))?;
            }
        }
    };
    fs::write(&pid_path, observed_group.as_raw().to_string())?;
    wait_for_terminal_owner(&mut session)?;
    let observed_child = construction_cleanup.observed_pid()?;
    match kill(observed_child, None) {
        Err(Errno::ESRCH) => {}
        Err(error) => return Err(error.into()),
        Ok(()) => {
            return Err(io::Error::other(format!(
                "partial construction leaked child {}",
                observed_child.as_raw()
            ))
            .into());
        }
    }
    match nix::sys::signal::killpg(observed_child, None) {
        Err(Errno::ESRCH) => construction_cleanup.disarm(),
        Err(error) => return Err(error.into()),
        Ok(()) => {
            return Err(
                io::Error::other("partial construction leaked its owned process group").into(),
            );
        }
    }
    execute_simple_with_marker(
        &mut session,
        "/usr/bin/printf AFTER_%s CONSTRUCTION_CLEANUP",
        b"AFTER_CONSTRUCTION_CLEANUP",
    )?;
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
    session.pty.wait_for(b"stopped")?;
    wait_for_terminal_owner(&mut session)?;
    if session.pty.foreground_group()? != child {
        return Err(io::Error::other("Quirl did not recover terminal after Ctrl-Z").into());
    }
    execute_simple_with_marker(&mut session, "jobs", b"stopped")?;
    execute_simple_with_marker(
        &mut session,
        "bg %1; /usr/bin/printf BG_%s RETURNED",
        b"BG_RETURNED",
    )?;
    execute_simple_with_marker(&mut session, "jobs", b"running")?;
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
    wait_for_terminal_owner(&mut session)?;
    execute_simple_with_marker(
        &mut session,
        "/usr/bin/printf AFTER_%s JOB_CTRLC",
        b"AFTER_JOB_CTRLC",
    )?;
    session.pty.type_text("/bin/sh -c 'stty -echo; kill -STOP $$; stty -a | grep -q -- \"-echo\" && printf JOB_%s MODES_OK'")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"stopped")?;
    wait_for_terminal_owner(&mut session)?;
    if session.pty.terminal_modes()? != prompt_modes {
        return Err(io::Error::other("stopped child modes leaked").into());
    }
    session.pty.type_text("fg %2")?;
    session.pty.send(key::ENTER)?;
    session.pty.wait_for(b"JOB_MODES_OK")?;
    wait_for_terminal_owner(&mut session)?;
    if session.pty.terminal_modes()? != prompt_modes {
        return Err(io::Error::other("termios not restored after fg").into());
    }
    send_ctrl_d_and_wait_for_exit(&mut session.pty)?;
    Ok(())
}

fn check_noninteractive_dialect_islands(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    execute_and_resume_with_marker(
        &mut session,
        "bash { read value || printf ISLAND_%s STDIN_CLOSED; }",
        b"ISLAND_STDIN_CLOSED",
    )?;
    session.pty.type_text("bash { sleep 30; }")?;
    session.pty.send(key::ENTER)?;
    session.pty.drain_for(Duration::from_millis(200))?;
    session.pty.send(b"\x1a")?;
    session.pty.wait_for_screen(
        "cancelled dialect island in persistent viewport",
        |screen| screen.text().contains("cancelled") && screen.bottom_line().contains("NORMAL"),
    )?;
    execute_and_resume_with_marker(
        &mut session,
        "/usr/bin/printf AFTER_%s ISLAND_CTRLZ",
        b"AFTER_ISLAND_CTRLZ",
    )?;
    send_ctrl_d_and_wait_for_exit(&mut session.pty)?;
    Ok(())
}

fn check_fallbacks(binary: &Path) -> Result<(), TaskError> {
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
    send_ctrl_d_and_wait_for_exit(&mut dumb.pty)?;
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
    send_ctrl_d_and_wait_for_exit(&mut redirected.pty)?;
    Ok(())
}

fn check_no_color_preserves_semantic_hints(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            no_color: true,
            ..SessionOptions::default()
        },
    )?;
    // A renderer may position across unchanged blank cells instead of writing
    // spaces. Visible labels must be checked in the terminal model, not as a
    // contiguous raw byte substring whose encoding changes with text styles.
    wait_for_standard_status(&mut session)?;
    wait_for_rich_input_since(&mut session, 0)?;
    session.pty.type_text("quirl describe --unknown")?;
    session.pty.wait_for_screen_text("unknown flag")?;
    session.pty.send(key::CTRL_C)?;
    session.pty.wait_for(b"^C")?;
    send_ctrl_d_and_wait_for_exit(&mut session.pty)?;
    Ok(())
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "poll counts are bounded by the file wait deadline"
)]
fn wait_for_file(session: &mut Session, path: PathBuf) -> Result<(), TaskError> {
    let deadline = Instant::now() + default_timeout();
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

fn discovery_session(
    binary: &Path,
    path: &Path,
    index_dir: &Path,
    help_path: &Path,
) -> Result<Session, TaskError> {
    Session::new(
        binary,
        SessionOptions {
            path: Some(path.to_path_buf()),
            index_dir: Some(index_dir.to_path_buf()),
            help_path: Some(help_path.to_path_buf()),
            catalog_refresh_enabled: true,
            rows: Some(18),
            columns: Some(400),
            ..SessionOptions::default()
        },
    )
}

fn wait_for_command_information(
    session: &mut Session,
    command: &str,
    markers: &[&str],
) -> Result<(), TaskError> {
    session.pty.type_text(command)?;
    session.pty.wait_for_screen(
        &format!("automatic information for {command:?}"),
        |screen| {
            let text = screen.text();
            // Provenance can arrive after the menu and footer. Inspect a
            // completed frame for the current edit before making negative
            // assertions about which command owns the visible documentation.
            screen.has_completed_frame()
                && screen.lines().iter().any(|line| {
                    ["> ", "❯ ", "> D ", "▦ "]
                        .iter()
                        .any(|prefix| line.starts_with(&format!("{prefix}{command}")))
                })
                && markers.iter().all(|marker| text.contains(marker))
        },
    )?;
    Ok(())
}

fn ensure_bottom_status(session: &Session, stage: &str) -> Result<(), TaskError> {
    if !session.pty.screen().bottom_line().contains("NORMAL") {
        return Err(screen_error(
            &format!("rich status was not on the physical bottom at {stage}"),
            session.pty.screen(),
        ));
    }
    Ok(())
}

fn wait_for_standard_status(session: &mut Session) -> Result<(), TaskError> {
    wait_for_mode_status(session, "NORMAL")
}

fn wait_for_mode_status(session: &mut Session, mode: &str) -> Result<(), TaskError> {
    session
        .pty
        .wait_for_screen(&format!("standard {mode} bottom status"), |screen| {
            let bottom = screen.bottom_line();
            bottom.contains(mode) && bottom.contains("Tab complete")
        })?;
    Ok(())
}

fn clear_editor(session: &mut Session) -> Result<(), TaskError> {
    clear_editor_in_mode(session, "NORMAL")
}

fn clear_editor_in_mode(session: &mut Session, mode: &str) -> Result<(), TaskError> {
    session.pty.send(key::CTRL_U)?;
    let prompts = match mode {
        "DATA" => ["> D", "▦"],
        "AI" => ["> AI", "✧"],
        _ => [">", "❯"],
    };
    session
        .pty
        .wait_for_screen("empty editor after clear", |screen| {
            let bottom = screen.bottom_line();
            bottom.contains(mode)
                && bottom.contains("Tab complete")
                && screen
                    .lines()
                    .iter()
                    .any(|line| prompts.contains(&line.trim()))
        })?;
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    reason = "captured output offsets come from successful marker searches"
)]
fn ensure_terminal_restored(
    session: &Session,
    output_start: usize,
    stage: &str,
) -> Result<(), TaskError> {
    if !contains(
        &session.pty.output()[output_start..],
        ALTERNATE_SCREEN_LEAVE,
    ) {
        return Err(io::Error::other(format!("{stage} did not leave the alternate screen")).into());
    }
    let modes = session.pty.terminal_modes()?;
    if !modes.local_flags.contains(LocalFlags::ICANON)
        || !modes.local_flags.contains(LocalFlags::ECHO)
    {
        return Err(
            io::Error::other(format!("{stage} did not restore cooked terminal modes")).into(),
        );
    }
    Ok(())
}

fn write_fixture(path: &Path, contents: &str) -> io::Result<()> {
    let bytes = contents.as_bytes();
    if bytes.len() > DISCOVERY_FIXTURE_BYTES_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "fixture {} exceeds its byte limit; observed={} limit={DISCOVERY_FIXTURE_BYTES_MAX}",
                path.display(),
                bytes.len()
            ),
        ));
    }
    fs::write(path, bytes)
}

fn write_executable(path: &Path, contents: &str) -> io::Result<()> {
    write_fixture(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn read_bounded_fixture(path: &Path, bytes_max: usize) -> io::Result<Vec<u8>> {
    let declared = fs::metadata(path)?.len();
    let bytes_max_u64 = u64::try_from(bytes_max).unwrap_or(u64::MAX);
    if declared > bytes_max_u64 {
        return Err(io::Error::other(format!(
            "{} exceeds its byte limit; observed={declared} limit={bytes_max}",
            path.display()
        )));
    }
    let bytes = fs::read(path)?;
    if bytes.len() > bytes_max {
        return Err(io::Error::other(format!(
            "{} grew past its byte limit while reading; observed={} limit={bytes_max}",
            path.display(),
            bytes.len()
        )));
    }
    Ok(bytes)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "poll counts are bounded by the file-content deadline"
)]
fn wait_for_file_contents(
    session: &mut Session,
    path: &Path,
    marker: &[u8],
) -> Result<(), TaskError> {
    let deadline = Instant::now() + default_timeout();
    while Instant::now() < deadline {
        if read_bounded_fixture(path, DISCOVERY_ARTIFACT_BYTES_MAX)
            .is_ok_and(|bytes| contains(&bytes, marker))
        {
            return Ok(());
        }
        session.pty.drain_for(Duration::from_millis(20))?;
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "timed out waiting for {marker:?} in bounded artifact {}",
            path.display()
        ),
    )
    .into())
}

fn assert_discovery_artifacts_bounded(index_dir: &Path) -> Result<(), TaskError> {
    let mut entries = 0_usize;
    for entry in fs::read_dir(index_dir)? {
        let entry = entry?;
        entries = entries.saturating_add(1);
        if entries > DISCOVERY_DIRECTORY_ENTRIES_MAX {
            return Err(io::Error::other(format!(
                "discovery directory exceeded its entry limit; observed={entries} limit={DISCOVERY_DIRECTORY_ENTRIES_MAX}"
            ))
            .into());
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_file() {
            return Err(io::Error::other(format!(
                "discovery artifact {} was not a regular file",
                path.display()
            ))
            .into());
        }
        let _ = read_bounded_fixture(&path, DISCOVERY_ARTIFACT_BYTES_MAX)?;
    }
    Ok(())
}

fn ensure_status(status: i32, expected: i32, label: &str) -> Result<(), TaskError> {
    if status != expected {
        return Err(
            io::Error::other(format!("{label} exited {status}; expected {expected}")).into(),
        );
    }
    Ok(())
}

fn screen_error(message: &str, screen: &VirtualScreen) -> TaskError {
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
    use std::{
        os::unix::{fs::PermissionsExt, process::CommandExt},
        process::{Command, Stdio},
        thread,
    };

    #[test]
    fn construction_group_requires_a_complete_safe_identifier() {
        assert_eq!(
            construction_group_from_output(b"owned pipeline process group: 1234\r\n").unwrap(),
            Pid::from_raw(1234)
        );
        for value in ["1234", "0\n", "1\n", "-12\n", "2147483648\n", "12oops\n"] {
            assert!(
                construction_group_from_output(
                    format!("owned pipeline process group: {value}").as_bytes()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn temporary_session_directory_is_private() {
        let directory = TempDirectory::new("quirl-private-test").unwrap();
        let mode = fs::metadata(&directory.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn construction_failure_cleanup_kills_the_observed_process_group() {
        let directory = TempDirectory::new("quirl-construction-cleanup-test").unwrap();
        let pid_path = directory.path.join("child.pid");
        let cleanup = ObservedProcessGroupCleanup::new(pid_path.clone());
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf '%s' $$ > \"$1\"; sleep 30", "fixture"])
            .arg(&pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pid_path.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !pid_path.is_file() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("construction fixture did not record its process group");
        }
        let observed = cleanup.observed_pid().unwrap();

        drop(cleanup);
        let status = child.wait().unwrap();

        assert!(!status.success());
        assert!(matches!(kill(observed, None), Err(Errno::ESRCH)));
    }
}
