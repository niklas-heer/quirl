//! Persistent-profile and paste journeys with filesystem execution oracles.
//!
//! Failure model: rendered command text is not evidence that it executed. Each
//! journey owns a private profile and checks a bounded sentinel file instead.
//! Bracketed paste and history selection must not execute commands; one explicit
//! submission must execute each intended command exactly once. All waits use the
//! bounded PTY owner, and errors drop that owner before its private profile.

use super::{
    PtySession, STARTUP_MARKER, Session, SessionOptions, TaskError, default_timeout, ensure_status,
    ensure_terminal_restored, key, read_bounded_fixture, shell_quote, wait_for_rich_input_since,
    wait_for_standard_status,
};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

const SENTINEL_BYTES_MAX: usize = 1_024;

/// Reopen the same profile and recall a command without executing on selection.
pub(super) fn check_restart_history(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let sentinel = session.private.path.join("history-proof");
    let command = format!(
        "/usr/bin/printf '%s\\n' RESTART_HISTORY_PROOF >> {}",
        shell_quote(Path::new("history-proof"))
    );
    let start = session.pty.output().len();
    session.pty.type_text(&command)?;
    session.pty.send(key::ENTER)?;
    wait_for_sentinel(&mut session, &sentinel, b"RESTART_HISTORY_PROOF\n")?;
    wait_for_rich_input_since(&mut session, start)?;
    assert_sentinel(&sentinel, b"RESTART_HISTORY_PROOF\n")?;
    exit_once(&mut session, "first history session")?;
    assert_sentinel(&sentinel, b"RESTART_HISTORY_PROOF\n")?;

    // Keep the exact HOME, history path, configuration, and cwd while replacing
    // the reaped PTY owner. This proves persistence rather than an editor cache.
    session.pty = PtySession::spawn(session.spawn.clone())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    session.pty.send(b"\x1b[A")?;
    session
        .pty
        .wait_for_screen("reopened durable history", |screen| {
            let text = screen.text();
            text.contains("history") && text.contains("RESTART_HISTORY_PROOF")
        })?;
    session.pty.send(key::ENTER)?;
    wait_for_standard_status(&mut session)?;
    session.pty.wait_for_screen_text("RESTART_HISTORY_PROOF")?;
    assert_sentinel(&sentinel, b"RESTART_HISTORY_PROOF\n")?;
    let start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    wait_for_sentinel(
        &mut session,
        &sentinel,
        b"RESTART_HISTORY_PROOF\nRESTART_HISTORY_PROOF\n",
    )?;
    wait_for_rich_input_since(&mut session, start)?;
    assert_sentinel(&sentinel, b"RESTART_HISTORY_PROOF\nRESTART_HISTORY_PROOF\n")?;
    exit_once(&mut session, "reopened history session")?;
    assert_sentinel(&sentinel, b"RESTART_HISTORY_PROOF\nRESTART_HISTORY_PROOF\n")
}

/// Paste two quoted commands, cancel them, then submit the paste exactly once.
pub(super) fn check_multiline_paste_admission(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(40),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let sentinel = session.private.path.join("paste-proof");
    // The PTY starts in its private profile. Relative targets keep both pasted
    // commands fully visible even when the host's temporary directory is long.
    let target = shell_quote(Path::new("paste-proof"));
    let source = format!(
        "/usr/bin/printf '%s\\n' 'PASTE \"quoted\" one' >> {target}\n\
         /usr/bin/printf '%s\\n' 'PASTE two' >> {target}"
    );
    let paste = format!("\x1b[200~{source}\x1b[201~");
    session.pty.send(paste.as_bytes())?;
    session.pty.wait_for_screen_text("PASTE two")?;
    session.pty.drain_for(Duration::from_millis(200))?;
    assert_absent(&sentinel)?;
    session.pty.send(key::CTRL_C)?;
    session
        .pty
        .wait_for_screen("cancelled multiline paste", |screen| {
            screen.text().contains("interactive input cancelled")
                && screen.bottom_line().contains("NORMAL")
        })?;
    assert_absent(&sentinel)?;

    session.pty.send(paste.as_bytes())?;
    session.pty.wait_for_screen_text("PASTE two")?;
    assert_absent(&sentinel)?;
    let start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    let expected = b"PASTE \"quoted\" one\nPASTE two\n";
    wait_for_sentinel(&mut session, &sentinel, expected)?;
    wait_for_rich_input_since(&mut session, start)?;
    assert_sentinel(&sentinel, expected)?;
    exit_once(&mut session, "multiline paste")?;
    assert_sentinel(&sentinel, expected)
}

fn wait_for_sentinel(session: &mut Session, path: &Path, expected: &[u8]) -> Result<(), TaskError> {
    let deadline = Instant::now()
        .checked_add(default_timeout())
        .ok_or_else(|| io::Error::other("sentinel deadline overflowed"))?;
    while Instant::now() < deadline {
        match read_bounded_fixture(path, SENTINEL_BYTES_MAX) {
            Ok(actual) if actual == expected => return Ok(()),
            Ok(actual) if expected.starts_with(&actual) => {}
            Ok(_) => return assert_sentinel(path, expected),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        session.pty.drain_for(Duration::from_millis(20))?;
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "submitted command did not produce its expected sentinel",
    )
    .into())
}

fn assert_absent(path: &Path) -> Result<(), TaskError> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(
            io::Error::other("editing or selection executed a command before submission").into(),
        ),
    }
}

fn assert_sentinel(path: &Path, expected: &[u8]) -> Result<(), TaskError> {
    let actual = read_bounded_fixture(path, SENTINEL_BYTES_MAX)?;
    if actual != expected {
        return Err(io::Error::other(format!(
            "submitted commands did not execute exactly once: expected {expected:?}, observed {actual:?}"
        ))
        .into());
    }
    Ok(())
}

fn exit_once(session: &mut Session, label: &str) -> Result<(), TaskError> {
    let start = session.pty.output().len();
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, label)?;
    ensure_terminal_restored(session, start, label)
}
