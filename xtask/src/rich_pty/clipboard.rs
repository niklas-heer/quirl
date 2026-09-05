//! Keyboard output selection and OSC 52 clipboard protocol evidence.
//!
//! Failure model: copying the wrong row, command echo, adjacent executable text,
//! terminal controls or a stale selection can appear successful without copying
//! the intended text. This journey checks exact single- and multiline UTF-8
//! payloads from captured PTY bytes, with fresh offsets for each copy. It never
//! forwards OSC 52 to a real terminal or touches the OS clipboard. Fixed input
//! is under 256 bytes, expected copy payloads under 64 bytes; PTY output, waits
//! and child/terminal cleanup retain the session owner's existing bounds.

use super::{
    STARTUP_MARKER, Session, SessionOptions, TaskError, ensure_status, ensure_terminal_restored,
    execute_and_resume, key,
};
use std::{io, path::Path};

const COPY_PREFIX: &[u8] = b"\x1b]52;";
// Standard base64 for UTF-8 COPY_SAFE_é and COPY_FIRST\nCOPY_SAFE_é respectively.
const SINGLE_COPY: &[u8] = b"\x1b]52;c;Q09QWV9TQUZFX8Op\x07";
const MULTILINE_COPY: &[u8] = b"\x1b]52;c;Q09QWV9GSVJTVApDT1BZX1NBRkVfw6k=\x07";

/// Select and copy exact Unicode output through keys, then recover and exit.
pub(super) fn check_clipboard_protocol(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    let output_start = session.pty.output().len();
    execute_and_resume(
        &mut session,
        "/usr/bin/printf 'COPY_FIRST\\nCOPY_SAFE_é\\nDO_NOT_COPY ; touch clipboard-proof\\n'",
    )?;
    session
        .pty
        .wait_for_screen("Unicode output available to copy", |screen| {
            screen.lines().iter().any(|line| line == "COPY_SAFE_é") && screen.has_completed_frame()
        })?;
    if session
        .pty
        .output()
        .get(output_start..)
        .unwrap_or_default()
        .windows(COPY_PREFIX.len())
        .any(|window| window == COPY_PREFIX)
    {
        return Err(
            io::Error::other("command output unexpectedly emitted clipboard protocol").into(),
        );
    }
    session.pty.send(key::ALT_Q)?;
    session.pty.wait_for_screen_text("Quirl")?;
    session.pty.send(b"o")?;
    session
        .pty
        .wait_for_screen("keyboard output focus", |screen| {
            screen.bottom_line().contains("OUTPUT") && screen.has_completed_frame()
        })?;
    // Focus starts on the final exit footer. Move past the executable guard
    // row to the intended output, then replace the anchor before copying.
    session.pty.send(b"\x1b[A\x1b[Av")?;
    copy_and_verify(&mut session, 12, SINGLE_COPY)?;
    session.pty.send(b"\x1b[A")?;
    copy_and_verify(&mut session, 23, MULTILINE_COPY)?;
    session.pty.send(key::ESCAPE)?;
    session
        .pty
        .wait_for_screen("clipboard focus dismissed", |screen| {
            screen.bottom_line().contains("NORMAL")
                && !screen.bottom_line().contains("OUTPUT")
                && !screen.bottom_line().contains("copied")
        })?;
    execute_and_resume(&mut session, "/usr/bin/printf CLIPBOARD_RECOVERED")?;
    let cleanup_start = session.pty.output().len();
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, "keyboard clipboard protocol")?;
    ensure_terminal_restored(&session, cleanup_start, "keyboard clipboard protocol")?;
    if session.private.path.join("clipboard-proof").try_exists()? {
        return Err(io::Error::other("copy selection executed adjacent command text").into());
    }
    Ok(())
}

fn copy_and_verify(session: &mut Session, bytes: usize, expected: &[u8]) -> Result<(), TaskError> {
    let start = session.pty.output().len();
    session.pty.send(b"y")?;
    let notice = format!("copied {bytes} bytes via terminal clipboard");
    session.pty.wait_for_screen(&notice, |screen| {
        screen.bottom_line().contains(&notice) && screen.has_completed_frame()
    })?;
    let emitted = session.pty.output().get(start..).unwrap_or_default();
    require_exact_copy(emitted, expected)
}

fn require_exact_copy(emitted: &[u8], expected: &[u8]) -> Result<(), TaskError> {
    let mut starts = emitted
        .windows(COPY_PREFIX.len())
        .enumerate()
        .filter_map(|(index, bytes)| (bytes == COPY_PREFIX).then_some(index));
    let Some(start) = starts.next() else {
        return Err(io::Error::other("copy notice appeared without an OSC 52 payload").into());
    };
    if starts.next().is_some()
        || emitted.get(start..start.saturating_add(expected.len())) != Some(expected)
    {
        return Err(
            io::Error::other("clipboard payload differed from the intended selection").into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_oracle_requires_one_complete_exact_fresh_payload() {
        assert!(require_exact_copy(SINGLE_COPY, SINGLE_COPY).is_ok());
        assert!(require_exact_copy(MULTILINE_COPY, MULTILINE_COPY).is_ok());
        assert!(
            require_exact_copy(b"copied 12 bytes via terminal clipboard", SINGLE_COPY).is_err()
        );
        assert!(require_exact_copy(MULTILINE_COPY, SINGLE_COPY).is_err());
        assert!(require_exact_copy(&SINGLE_COPY[..SINGLE_COPY.len() - 1], SINGLE_COPY).is_err());
        let duplicate = [SINGLE_COPY, SINGLE_COPY].concat();
        assert!(require_exact_copy(&duplicate, SINGLE_COPY).is_err());
        let extra_target = [SINGLE_COPY, b"\x1b]52;p;WA==\x07"].concat();
        assert!(require_exact_copy(&extra_target, SINGLE_COPY).is_err());
        let other_command = b"\x1b]52;c;dG91Y2ggY2xpcGJvYXJkLXByb29m\x07";
        assert!(require_exact_copy(other_command, SINGLE_COPY).is_err());
    }
}
