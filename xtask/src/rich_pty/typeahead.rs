//! Unread child-terminal type-ahead must survive the foreground-to-editor handoff.
//!
//! The child terminal echoes queued keys through the emulator while the outer
//! terminal stays raw. A gated child proves this ordering without relying on
//! delays: observe its output, send the next input, observe the echoed cells,
//! then release the child. Assert both rendered blanks
//! and exact executed argument bytes. Each fixture emits 45 short lines, polls
//! its private gate at most 1,000 times, and remains under the PTY owner's wait,
//! output and cleanup limits; failure drops the child owner before its files.

use super::{
    LocalFlags, STARTUP_MARKER, Session, SessionOptions, TaskError, ensure_status,
    ensure_terminal_restored, key, read_bounded_fixture, wait_for_file, wait_for_rich_input_since,
    write_fixture,
};
use std::{fs, io, path::Path};

pub(super) fn check_command_typeahead_redraw(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(40),
            symbols: Some("unicode"),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    write_fixture(
        &session.private.path.join("typeahead.sh"),
        "#!/bin/sh\nset -eu\ni=0\nwhile [ \"$i\" -lt 45 ]; do printf 'Native commands and byte pipelines\\n'; i=$((i+1)); done\nprintf '%s\\n' \"$3\"\nprintf ready > \"$2\"\ni=0\nwhile [ ! -f \"$1\" ]; do i=$((i+1)); [ \"$i\" -lt 1000 ] || exit 9; sleep 0.01; done\n",
    )?;
    for (label, caption) in [
        (
            "unicode",
            "# discover candidates · review suggestions before choosing",
        ),
        (
            "ascii",
            "# discover candidates - review suggestions before choosing",
        ),
    ] {
        check_caption(&mut session, label, caption)?;
    }
    let start = session.pty.output().len();
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, "type-ahead session")?;
    ensure_terminal_restored(&session, start, "type-ahead session")
}

fn check_caption(session: &mut Session, label: &str, caption: &str) -> Result<(), TaskError> {
    let marker = format!("TYPEAHEAD_{label}_READY");
    let command = format!("/bin/sh typeahead.sh {label}.gate {label}.ready {marker}");
    session.pty.type_text(&command)?;
    session.pty.send(key::ENTER)?;
    wait_for_file(session, session.private.path.join(format!("{label}.ready")))?;
    session
        .pty
        .wait_for_screen("gated foreground output", |screen| {
            screen.lines().iter().any(|line| line.starts_with(&marker))
        })?;
    let flags = session.pty.terminal_modes()?.local_flags;
    if flags.intersects(LocalFlags::ICANON | LocalFlags::ECHO) {
        return Err(io::Error::other("outer terminal did not retain raw input ownership").into());
    }
    let start = session.pty.output().len();
    session.pty.send(caption.as_bytes())?;
    session
        .pty
        .wait_for_screen("child terminal echoed the unread type-ahead", |screen| {
            screen.lines().iter().any(|line| line == caption)
        })?;
    fs::write(
        session.private.path.join(format!("{label}.gate")),
        b"release",
    )?;
    wait_for_rich_input_since(session, start)?;
    let expected_line = format!("❯ {caption}");
    session
        .pty
        .wait_for_screen("exact type-ahead input including blank cells", |screen| {
            screen.has_completed_frame() && screen.lines().iter().any(|line| line == &expected_line)
        })?;

    // Quote the existing editor buffer in place. The resulting argv proves the
    // redraw retained every original byte instead of merely fixing its picture.
    session.pty.send(b"\x01/usr/bin/printf '%s' '")?;
    session
        .pty
        .send(format!("\x05' > {label}.argv").as_bytes())?;
    let start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    wait_for_rich_input_since(session, start)?;
    let actual = read_bounded_fixture(&session.private.path.join(format!("{label}.argv")), 128)?;
    if actual != caption.as_bytes() {
        return Err(io::Error::other(format!("type-ahead source changed: {actual:?}")).into());
    }
    Ok(())
}
