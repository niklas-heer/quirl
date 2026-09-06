//! Application-independent foreground terminal journeys using private fixtures.
//!
//! Failure model: executable names, wrappers, or version launchers must not
//! decide whether a foreground child can use terminal descriptors. Relaying
//! input without queries/resizes, or failing to restore the editor after child
//! failure, is equally incorrect. Fixed scripts exercise ten fixture launches
//! and seven post-return marker commands;
//! every read, write, and child lifetime remains owned by the bounded Session.
//! No host application, network access, sleeps, or rescue input is required.

use super::{
    STARTUP_MARKER, Session, SessionOptions, TaskError, TempDirectory, contains,
    create_private_directory, ensure_status, ensure_terminal_restored, execute_and_resume,
    execute_and_resume_with_marker, find_on_path, key, read_bounded_fixture,
    send_ctrl_d_and_wait_for_exit, shell_quote, wait_for_rich_input_since, write_executable,
};
use std::{io, path::Path};

/// Prove unknown foreground applications receive a bidirectional terminal,
/// including query replies, resize delivery, interruption, and failure cleanup.
pub(super) fn check_generic_terminal_session(binary: &Path) -> Result<(), TaskError> {
    let fixtures = TempDirectory::new("quirl-generic-terminal")?;
    let bin = fixtures.path.join("bin");
    create_private_directory(&bin)?;
    let bash = find_on_path("bash").ok_or_else(|| io::Error::other("fixture requires Bash"))?;
    let stty = find_on_path("stty").ok_or_else(|| io::Error::other("fixture requires stty"))?;
    let name = format!("session-console-{}", std::process::id());
    write_executable(&bin.join(&name), &fixture_source(&bash, &stty))?;
    let wrapper = format!("session-wrapper-{}", std::process::id());
    write_executable(
        &bin.join(&wrapper),
        &format!("#!/bin/sh\nexec \"${{0%/*}}/{name}\" \"$@\"\n"),
    )?;
    let launcher = format!("session-toolchain-{}", std::process::id());
    write_executable(
        &bin.join(&launcher),
        &format!(
            "#!/bin/sh\n[ \"$1\" = run ] && [ \"$2\" = console@7 ] || exit 49\nshift 2\nexec \"${{0%/*}}/{name}\" \"$@\"\n"
        ),
    )?;
    let mut session = Session::new(
        binary,
        SessionOptions {
            path: Some(bin),
            rows: Some(24),
            columns: Some(100),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    for (command, label) in [
        (format!("{name} query direct"), "direct"),
        (format!("{wrapper} keys wrapper"), "wrapper"),
        (
            format!("{launcher} run console@7 keys launcher"),
            "launcher",
        ),
    ] {
        check_interactive_launch(&mut session, &command, label)?;
    }
    check_interrupt(&mut session, &name)?;
    check_failure(&mut session, &name)?;
    check_stopped(&mut session, &name, StoppedAction::Resume)?;
    check_stopped(&mut session, &name, StoppedAction::Cancel)?;
    check_retained_and_redirected_output(&mut session, &name)?;
    let cleanup_start = session.pty.output().len();
    ensure_status(
        send_ctrl_d_and_wait_for_exit(&mut session.pty)?,
        0,
        "generic terminal",
    )?;
    ensure_terminal_restored(&session, cleanup_start, "generic terminal")
}

fn fixture_source(bash: &Path, stty: &Path) -> String {
    format!(
        r#"#!{bash}
mode=$1
label=$2
case "$mode" in
  plain) printf 'GENERIC_RETAINED_OUTPUT\n'; exit 0 ;;
  capture)
    [ ! -t 1 ] || exit 48
    printf 'GENERIC_CAPTURED_OUTPUT\n'; exit 0 ;;
esac
if ! [ -t 0 ] || ! [ -t 1 ] || ! [ -t 2 ]; then
  printf 'GENERIC_NOT_A_TERMINAL:%s\n' "$label"; exit 41
fi
stty={stty}
saved=$("$stty" -g) || exit 42
cleanup() {{ "$stty" "$saved"; printf '\033[?1049l'; }}
trap cleanup EXIT
trap 'printf "interrupt\n" > generic-proof-interrupt; exit 130' INT
printf '\033[?1049h\033[2J\033[4;7HGENERIC_FRAME:%s\n' "$label"
if [ "$mode" = failure ]; then
  "$stty" -echo -icanon min 1 time 0
  printf 'GENERIC_READY:failure\n'
  IFS= read -r answer || exit 46
  [ "$answer" = fail ] || exit 47
  printf 'failure\n' > generic-proof-failure
  exit 7
fi
if [ "$mode" = query ]; then
  "$stty" -echo -icanon min 1 time 0
  printf '\033[4;7H\033[6n'
  IFS= read -r -d R reply || {{ printf 'GENERIC_QUERY_READ_FAILED\n'; exit 43; }}
  [ "$reply" = $'\033[4;7' ] || {{ printf 'GENERIC_QUERY_POSITION_FAILED\n'; exit 44; }}
  "$stty" "$saved"
  trap 'size=$("$stty" size); printf "GENERIC_RESIZED:%s\n" "$size"' WINCH
  printf 'GENERIC_QUERY_OK\n'
fi
printf '\033[4;7HGENERIC_FRAME:%s\033[K\033[8;1H' "$label"
printf 'GENERIC_READY:%s\n' "$label"
if [ "$mode" = stop ]; then
  printf 'stopped\n' > "generic-proof-$label"
  kill -STOP $$
  printf 'GENERIC_RESUMED:%s\n' "$label"
fi
if [ "$mode" = interrupt ]; then
  IFS= read -r answer
  exit 45
fi
# Bash 5.3 restarts an untimed read on WINCH and defers its trap until input.
# A timed read dispatches the signal immediately; its 30-second limit exceeds
# the owning PTY wait, so expiry cannot satisfy the resize oracle. A fixed
# second read admits an interrupted read without another user answer.
IFS= read -r -t 30 answer || IFS= read -r -t 30 answer || exit 46
[ "$answer" = "answer-$label" ] || exit 47
printf '%s\n' "$label" > "generic-proof-$label"
"#,
        bash = bash.display(),
        stty = shell_quote(stty),
    )
}

fn begin_launch(session: &mut Session, source: &str, label: &str) -> Result<usize, TaskError> {
    let start = session.pty.output().len();
    session.pty.type_text(source)?;
    session.pty.send(key::ENTER)?;
    let ready = format!("GENERIC_READY:{label}");
    let rejected = format!("GENERIC_NOT_A_TERMINAL:{label}");
    session
        .pty
        .wait_for_screen("unknown application terminal admission", |screen| {
            let text = screen.text();
            (text.contains(&ready) && text.contains(&format!("GENERIC_FRAME:{label}")))
                || text.contains(&rejected)
        })?;
    if session.pty.screen().text().contains(&rejected) {
        return Err(io::Error::other(
            "unknown foreground application did not inherit three TTY descriptors",
        )
        .into());
    }
    Ok(start)
}

fn check_interactive_launch(
    session: &mut Session,
    source: &str,
    label: &str,
) -> Result<(), TaskError> {
    let start = begin_launch(session, source, label)?;
    if label == "direct" {
        if contains(
            session.pty.output().get(start..).unwrap_or_default(),
            b"\x1b[6n",
        ) {
            return Err(io::Error::other("child cursor query escaped the bounded emulator").into());
        }
        session.pty.resize(33, 111)?;
        session
            .pty
            .wait_for_screen("child reports resized terminal dimensions", |screen| {
                screen
                    .lines()
                    .iter()
                    .any(|line| line.trim() == "GENERIC_RESIZED:33 111")
            })?;
    }
    let return_start = session.pty.output().len();
    session.pty.type_text(&format!("answer-{label}"))?;
    session.pty.send(key::ENTER)?;
    finish_launch(session, return_start, label)
}

fn check_interrupt(session: &mut Session, name: &str) -> Result<(), TaskError> {
    begin_launch(session, &format!("{name} interrupt interrupt"), "interrupt")?;
    let start = session.pty.output().len();
    session.pty.send(key::CTRL_C)?;
    finish_launch(session, start, "interrupt")
}

fn check_failure(session: &mut Session, name: &str) -> Result<(), TaskError> {
    begin_launch(session, &format!("{name} failure failure"), "failure")?;
    let start = session.pty.output().len();
    session.pty.type_text("fail")?;
    session.pty.send(key::ENTER)?;
    finish_launch(session, start, "failure")
}

enum StoppedAction {
    Resume,
    Cancel,
}

fn check_stopped(
    session: &mut Session,
    name: &str,
    action: StoppedAction,
) -> Result<(), TaskError> {
    let label = match action {
        StoppedAction::Resume => "resume",
        StoppedAction::Cancel => "cancel",
    };
    begin_launch(session, &format!("{name} stop {label}"), label)?;
    session.pty.wait_for_screen(
        "stopped child remains owned with explicit choices",
        |screen| {
            let text = screen.text();
            text.contains("Program suspended")
                && text.contains("Enter resumes")
                && text.contains("Ctrl-C cancels")
        },
    )?;
    let mut return_start = session.pty.output().len();
    if matches!(action, StoppedAction::Cancel) {
        session.pty.send(key::CTRL_C)?;
    } else {
        session.pty.send(key::ENTER)?;
        session.pty.wait_for_screen_text("GENERIC_RESUMED:resume")?;
        return_start = session.pty.output().len();
        session.pty.type_text("answer-resume")?;
        session.pty.send(key::ENTER)?;
    }
    finish_launch(session, return_start, label)
}

fn finish_launch(session: &mut Session, start: usize, label: &str) -> Result<(), TaskError> {
    wait_for_rich_input_since(session, start)?;
    let expected = if label == "cancel" { "stopped" } else { label };
    if read_bounded_fixture(
        &session.private.path.join(format!("generic-proof-{label}")),
        256,
    )? != format!("{expected}\n").as_bytes()
    {
        return Err(io::Error::other("generic terminal key/signal proof differs").into());
    }
    if let Some(status) = match label {
        "interrupt" | "cancel" => Some("status:130"),
        "failure" => Some("status:7"),
        _ => None,
    } {
        session
            .pty
            .wait_for_screen("child failure status is retained", |screen| {
                screen.has_completed_frame()
                    && screen.bottom_line().contains("NORMAL")
                    && screen.text().contains(status)
            })?;
    }
    execute_and_resume_with_marker(
        session,
        &format!("/usr/bin/printf GENERIC_AFTER_%s {label}"),
        format!("GENERIC_AFTER_{label}").as_bytes(),
    )
}

fn check_retained_and_redirected_output(
    session: &mut Session,
    name: &str,
) -> Result<(), TaskError> {
    execute_and_resume_with_marker(
        session,
        &format!("{name} plain"),
        b"GENERIC_RETAINED_OUTPUT",
    )?;
    let output = session.private.path.join("redirected.txt");
    execute_and_resume(session, &format!("{name} capture > redirected.txt"))?;
    if read_bounded_fixture(&output, 256)? != b"GENERIC_CAPTURED_OUTPUT\n" {
        return Err(io::Error::other("explicit redirect changed fixture output").into());
    }
    execute_and_resume_with_marker(
        session,
        &format!("{name} capture | /bin/cat"),
        b"GENERIC_CAPTURED_OUTPUT",
    )
}
