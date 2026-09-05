//! Terminal-delivered paste and committed Unicode text journeys.
//!
//! Failure model: pasted controls must remain source bytes, never terminal
//! commands or editing actions; oversized source must not become an executable
//! truncated prefix. Committed UTF-8 is edited by grapheme, including when bytes
//! arrive in separate writes. Each session owns a private profile and bounded
//! file oracle, submits once, and verifies exact bytes again after EOF. Inputs
//! are fixed fixtures, at most 262,157 bytes; output and waits use the PTY owner's
//! existing limits. These checks emulate terminal bytes, not OS clipboard or
//! IME integration. Multiline paste admission is covered in `usability`.

use super::{
    STARTUP_MARKER, Session, SessionOptions, TaskError, default_timeout, ensure_status,
    ensure_terminal_restored, key, read_bounded_fixture, wait_for_rich_input_since,
};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

const EDITOR_BYTES_MAX: usize = 64 * 1_024;
const PROOF_BYTES_MAX: usize = 1_024;

/// Admit ordinary Vi repeats and reject excessive expansion without executing input.
///
/// Numeric prefixes describe work rather than text. A small byte sequence must
/// not create an unlimited action vector; rejection must pass through Quirl's
/// edit-mode wrapper and restore the terminal before any queued Enter can run.
pub(super) fn check_vi_repeat_admission(binary: &Path) -> Result<(), TaskError> {
    let mut session = start_vi(binary)?;
    let proof = session.private.path.join("vi-proof");
    // One write keeps insertion and the mode switch in one terminal batch.
    // Applying earlier inserts under the final Normal mode reorders source.
    session.pty.send(b"abcXYZ\x1b")?;
    session.pty.wait_for_screen_text("normal normal >")?;
    session.pty.type_text("03x")?;
    session.pty.wait_for_screen_text("normal normal > XYZ")?;
    session.pty.type_text("i/usr/bin/printf '%s' '")?;
    session.pty.send(b"\x1b[F")?;
    session.pty.type_text("' > vi-proof")?;
    session.pty.wait_for_screen_text("vi-proof")?;
    assert_absent(&proof)?;
    submit_with_surface(&mut session, &proof, b"XYZ", "simple")?;
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, "valid Vi repeat")?;
    assert_simple_terminal_restored(&session)?;
    if read_bounded_fixture(&proof, PROOF_BYTES_MAX)? != b"XYZ" {
        return Err(io::Error::other("Vi repeat changed execution before EOF").into());
    }

    for excess in ["999999999w", "32d33w", "184467440737095516160w"] {
        let mut session = start_vi(binary)?;
        let proof = session.private.path.join("vi-reject-proof");
        session
            .pty
            .type_text("/usr/bin/printf UNEXPECTED > vi-reject-proof")?;
        enter_vi_normal(&mut session)?;
        // One burst queues Enter behind the rejected prefix. The latched mode
        // error must reach the host before that later event can submit source.
        session.pty.type_text(&format!("{excess}\r"))?;
        ensure_status(session.pty.wait_exit()?, 1, "excessive Vi repeat")?;
        assert_simple_terminal_restored(&session)?;
        assert_absent(&proof)?;
        let output = String::from_utf8_lossy(session.pty.output());
        if !output.contains("resource-limit") || !output.contains("1024") {
            return Err(io::Error::other(format!(
                "Vi repeat {excess:?} omitted its resource diagnostic"
            ))
            .into());
        }
    }
    Ok(())
}

fn start_vi(binary: &Path) -> Result<Session, TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            surface: Some("simple"),
            keymap: Some("vim"),
            semantic_hints: Some(false),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for_screen_text("insert normal >")?;
    Ok(session)
}

fn enter_vi_normal(session: &mut Session) -> Result<(), TaskError> {
    session.pty.send(key::ESCAPE)?;
    session.pty.wait_for_screen_text("normal normal >")?;
    Ok(())
}

fn assert_simple_terminal_restored(session: &Session) -> Result<(), TaskError> {
    let modes = session.pty.terminal_modes()?;
    if !modes.local_flags.contains(super::LocalFlags::ICANON)
        || !modes.local_flags.contains(super::LocalFlags::ECHO)
        || !session
            .pty
            .output()
            .windows(b"\x1b[?2004l".len())
            .any(|window| window == b"\x1b[?2004l")
    {
        return Err(io::Error::other("simple Vi session did not restore its terminal").into());
    }
    Ok(())
}

/// Reject pending sequences and filtered replies in both terminal interfaces.
pub(super) fn check_terminal_input_limits(binary: &Path) -> Result<(), TaskError> {
    for surface in ["rich", "simple"] {
        for input in [
            LimitInput::Paste,
            LimitInput::Escape,
            LimitInput::CursorReplies,
        ] {
            check_input_limit(binary, surface, input).map_err(|error| {
                io::Error::other(format!("{surface} {input:?} limit journey: {error}"))
            })?;
        }
    }
    check_input_limit(binary, "simple", LimitInput::Editor)
        .map_err(|error| io::Error::other(format!("simple editor admission journey: {error}")))?;
    // One real idle wait proves the timer fires without another input byte;
    // both interfaces use this reader, so avoid duplicating thirty seconds.
    check_input_limit(binary, "rich", LimitInput::Deadline)
        .map_err(|error| io::Error::other(format!("rich idle deadline journey: {error}")))?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum LimitInput {
    Paste,
    Escape,
    CursorReplies,
    Deadline,
    Editor,
}

fn limit_payload(input: LimitInput) -> Vec<u8> {
    let (mut payload, size) = match input {
        LimitInput::Paste => (
            b"\x1b[200~/usr/bin/printf UNEXPECTED > ingress-proof\n".to_vec(),
            262_157,
        ),
        LimitInput::Escape => (b"\x1b[".to_vec(), 4_097),
        LimitInput::CursorReplies => return b"\x1b[1;1R".repeat(1_025),
        LimitInput::Deadline => {
            return b"\x1b[200~/usr/bin/printf UNEXPECTED > ingress-proof\n".to_vec();
        }
        LimitInput::Editor => {
            let mut payload = b"\x1b[200~/usr/bin/printf UNEXPECTED > ingress-proof\n".to_vec();
            // Each completed paste is individually legal; their aggregate
            // crosses the owning editor limit without an explicit submission.
            payload.resize(32_775, b'1'); // Six marker bytes plus 32,769 source bytes.
            payload.extend_from_slice(b"\x1b[201~\x1b[200~");
            payload.extend(std::iter::repeat_n(b'1', 32_769));
            payload.extend_from_slice(b"\x1b[201~");
            return payload;
        }
    };
    payload.resize(size, b'1');
    payload
}

fn check_input_limit(
    binary: &Path,
    surface: &'static str,
    input: LimitInput,
) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            surface: Some(surface),
            semantic_hints: Some(false),
            ..SessionOptions::default()
        },
    )?;
    if surface == "rich" {
        session.pty.wait_for(STARTUP_MARKER)?;
    } else {
        session
            .pty
            .wait_for_screen("simple terminal input prompt", |screen| {
                screen.lines().iter().any(|line| line.trim() == "normal >")
            })?;
    }
    let proof = session.private.path.join("ingress-proof");
    let payload = limit_payload(input);
    let start = session.pty.output().len();
    // The final admitted byte triggers failure. If child exit races the
    // writer, the diagnostic and restored terminal remain the primary
    // oracle rather than treating an expected broken pipe as success.
    let send_result = session.pty.send(&payload);
    let status = if matches!(input, LimitInput::Deadline) {
        session.pty.wait_exit_within(Duration::from_secs(35))
    } else {
        session.pty.wait_exit()
    }
    .map_err(|error| {
        io::Error::other(format!("input write: {send_result:?}; child exit: {error}"))
    })?;
    if surface == "rich" {
        ensure_terminal_restored(&session, start, "terminal input limit")?;
    } else {
        let modes = session.pty.terminal_modes()?;
        if !modes.local_flags.contains(super::LocalFlags::ICANON)
            || !modes.local_flags.contains(super::LocalFlags::ECHO)
            || !session
                .pty
                .output()
                .get(start..)
                .unwrap_or_default()
                .windows(b"\x1b[?2004l".len())
                .any(|window| window == b"\x1b[?2004l")
        {
            return Err(io::Error::other(format!(
                "simple input failure did not restore terminal modes and bracketed paste; flags={:?}; tail={:?}",
                modes.local_flags,
                session.pty.output().get(session.pty.output().len().saturating_sub(1_024)..).unwrap_or_default(),
            ))
            .into());
        }
    }
    ensure_status(status, 1, "terminal input limit")?;
    assert_absent(&proof)?;
    let output = String::from_utf8_lossy(session.pty.output());
    if !output.contains("resource-limit") || !output.contains("terminal input") {
        return Err(io::Error::other(format!(
                "input limit did not report ResourceLimit; input write: {send_result:?}; output: {output}"
            ))
            .into());
    }
    if matches!(input, LimitInput::Deadline)
        && (!output.contains("deadline") || !output.contains("30000 ms"))
    {
        return Err(io::Error::other("idle input failure omitted its configured deadline").into());
    }
    Ok(())
}

/// Keep pasted control bytes literal through editing, cancellation, and execution.
pub(super) fn check_paste_control_isolation(binary: &Path) -> Result<(), TaskError> {
    let mut session = start(binary)?;
    let proof = session.private.path.join("control-proof");
    // No bracketed-paste terminator occurs inside the fixture: that delimiter
    // belongs to the terminal protocol and cannot represent literal paste data.
    let literal = "CONTROL_START\x1b]52;c;SE9TVElMRQ==\x07\x1b[2J\x03\x04\x15\t\rCONTROL_END";
    let source = format!("/usr/bin/printf '%s' '{literal}' > control-proof");
    let start = session.pty.output().len();
    paste(&mut session, &source)?;
    session.pty.wait_for_screen_text("CONTROL_END")?;
    assert_absent(&proof)?;
    let emitted = session.pty.output().get(start..).unwrap_or_default();
    if emitted
        .windows(b"\x1b]52;c;SE9TVElMRQ==".len())
        .any(|window| window == b"\x1b]52;c;SE9TVElMRQ==")
    {
        return Err(
            io::Error::other("pasted OSC clipboard payload escaped into terminal output").into(),
        );
    }
    cancel(&mut session)?;
    assert_absent(&proof)?;
    paste(&mut session, &source)?;
    session.pty.wait_for_screen_text("CONTROL_END")?;
    assert_absent(&proof)?;
    submit(&mut session, &proof, literal.as_bytes())?;
    finish(
        &mut session,
        &proof,
        literal.as_bytes(),
        "paste control isolation",
    )?;
    for no_color in [false, true] {
        check_simple_multiline_paste(binary, no_color)?;
    }
    Ok(())
}

fn check_simple_multiline_paste(binary: &Path, no_color: bool) -> Result<(), TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            surface: Some("simple"),
            semantic_hints: Some(false),
            no_color,
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(b"\x1b[?2004h")?;
    session
        .pty
        .wait_for_screen("simple paste prompt", |screen| {
            screen.lines().iter().any(|line| line.trim() == "normal >")
        })?;
    let proof = session.private.path.join("simple-paste-proof");
    let control_start = session.pty.output().len();
    paste(
        &mut session,
        "SIMPLE_OSC_BEGIN\x1b]52;c;SE9TVElMRQ==\x07SIMPLE_OSC_END",
    )?;
    session.pty.wait_for_screen_text("SIMPLE_OSC_END")?;
    if session
        .pty
        .output()
        .get(control_start..)
        .unwrap_or_default()
        .windows(b"\x1b]52;c;SE9TVElMRQ==".len())
        .any(|window| window == b"\x1b]52;c;SE9TVElMRQ==")
    {
        return Err(io::Error::other(
            "simple pasted OSC clipboard payload escaped into terminal output",
        )
        .into());
    }
    let cancelled = session.pty.output().len();
    session.pty.send(key::CTRL_C)?;
    session
        .pty
        .wait_for_since(b"interactive input cancelled", cancelled, default_timeout())?;
    session
        .pty
        .wait_for_since(b"\x1b[?2004h", cancelled, default_timeout())?;
    let source = "/usr/bin/printf '%s\\n' 'SIMPLE one' >> simple-paste-proof\n/usr/bin/printf '%s\\n' 'SIMPLE two' >> simple-paste-proof";
    paste(&mut session, source)?;
    session.pty.wait_for_screen_text("SIMPLE two")?;
    session.pty.drain_for(Duration::from_millis(100))?;
    assert_absent(&proof)?;
    let start = session.pty.output().len();
    session.pty.send(key::CTRL_C)?;
    session
        .pty
        .wait_for_since(b"interactive input cancelled", start, default_timeout())?;
    session
        .pty
        .wait_for_since(b"\x1b[?2004h", start, default_timeout())?;
    assert_absent(&proof)?;
    paste(&mut session, source)?;
    session.pty.wait_for_screen_text("SIMPLE two")?;
    assert_absent(&proof)?;
    let expected = b"SIMPLE one\nSIMPLE two\n";
    submit_with_surface(&mut session, &proof, expected, "simple")?;
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, "simple multiline paste")?;
    let modes = session.pty.terminal_modes()?;
    if !modes.local_flags.contains(super::LocalFlags::ICANON)
        || !modes.local_flags.contains(super::LocalFlags::ECHO)
        || read_bounded_fixture(&proof, PROOF_BYTES_MAX)? != expected
    {
        return Err(
            io::Error::other("simple paste changed output or terminal state before EOF").into(),
        );
    }
    Ok(())
}

/// Reject an oversized paste as a whole, retaining an existing complete command.
pub(super) fn check_oversized_paste_admission(binary: &Path) -> Result<(), TaskError> {
    let mut session = start(binary)?;
    let proof = session.private.path.join("oversize-proof");
    session
        .pty
        .type_text("/usr/bin/printf '%s' ORIGINAL > oversize-proof")?;
    session.pty.wait_for_screen_text("ORIGINAL")?;
    let source = "x".repeat(EDITOR_BYTES_MAX.saturating_add(1));
    paste(&mut session, &source)?;
    session.pty.wait_for_screen_text("paste rejected")?;
    assert_absent(&proof)?;
    submit(&mut session, &proof, b"ORIGINAL")?;
    finish(
        &mut session,
        &proof,
        b"ORIGINAL",
        "oversized paste admission",
    )
}

/// Edit combining text, emoji sequences, and flags as committed Unicode graphemes.
pub(super) fn check_unicode_committed_text(binary: &Path) -> Result<(), TaskError> {
    let mut session = start(binary)?;
    let proof = session.private.path.join("unicode-proof");
    session.pty.type_text("/usr/bin/printf '%s' '")?;
    // Byte-fragmented committed text exercises the terminal decoder. It does
    // not model candidate windows, composition events, or an OS input method.
    for byte in "e\u{301}👩‍💻🇩🇪界".as_bytes() {
        session.pty.send(std::slice::from_ref(byte))?;
    }
    session.pty.send(b"\x7f")?; // Remove the CJK grapheme.
    session.pty.send(b"\x1b[D")?; // Move before the flag grapheme.
    session.pty.send(b"\x1b[3~")?; // Delete both flag regional indicators.
    session.pty.send(b"\x7f")?; // Remove the joined emoji as one grapheme.
    session.pty.type_text("X' > unicode-proof")?;
    session.pty.wait_for_screen_text("unicode-proof")?;
    assert_absent(&proof)?;
    submit(&mut session, &proof, "e\u{301}X".as_bytes())?;

    session
        .pty
        .type_text("/usr/bin/printf '%s' 'cafe\u{301} 👩‍💻")?;
    session.pty.send(b"\x17")?; // Kill the emoji word into the editor kill ring.
    session.pty.send(b"\x19")?; // Yank exactly those bytes back.
    session.pty.type_text("' >> unicode-proof")?;
    session.pty.wait_for_screen_text("unicode-proof")?;
    let expected = "e\u{301}Xcafe\u{301} 👩‍💻".as_bytes();
    submit(&mut session, &proof, expected)?;
    finish(&mut session, &proof, expected, "committed Unicode editing")
}

fn start(binary: &Path) -> Result<Session, TaskError> {
    let mut session = Session::new(
        binary,
        SessionOptions {
            rows: Some(40),
            semantic_hints: Some(false),
            ..SessionOptions::default()
        },
    )?;
    session.pty.wait_for(STARTUP_MARKER)?;
    Ok(session)
}

fn paste(session: &mut Session, source: &str) -> Result<(), TaskError> {
    session.pty.send(b"\x1b[200~")?;
    session.pty.send(source.as_bytes())?;
    session.pty.send(b"\x1b[201~")
}

fn cancel(session: &mut Session) -> Result<(), TaskError> {
    session.pty.send(key::CTRL_C)?;
    session
        .pty
        .wait_for_screen("cancelled terminal paste", |screen| {
            screen.text().contains("interactive input cancelled")
                && screen.bottom_line().contains("NORMAL")
        })?;
    Ok(())
}

fn submit(session: &mut Session, path: &Path, expected: &[u8]) -> Result<(), TaskError> {
    submit_with_surface(session, path, expected, "rich")
}

fn submit_with_surface(
    session: &mut Session,
    path: &Path,
    expected: &[u8],
    surface: &str,
) -> Result<(), TaskError> {
    let start = session.pty.output().len();
    session.pty.send(key::ENTER)?;
    let deadline = Instant::now()
        .checked_add(default_timeout())
        .ok_or_else(|| io::Error::other("input proof deadline overflowed"))?;
    loop {
        match read_bounded_fixture(path, PROOF_BYTES_MAX) {
            Ok(actual) if actual == expected => break,
            Ok(actual) if expected.starts_with(&actual) => {}
            Ok(actual) => return Err(io::Error::other(format!(
                "terminal text changed before execution: expected {expected:?}, observed {actual:?}"
            ))
            .into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal input did not produce its exact file proof",
            )
            .into());
        }
        session.pty.drain_for(Duration::from_millis(20))?;
    }
    if surface == "rich" {
        wait_for_rich_input_since(session, start)
    } else {
        session
            .pty
            .wait_for_since(b"\x1b[?2004h", start, default_timeout())?;
        Ok(())
    }
}

fn assert_absent(path: &Path) -> Result<(), TaskError> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(io::Error::other("terminal text executed before explicit submission").into()),
    }
}

fn finish(
    session: &mut Session,
    path: &Path,
    expected: &[u8],
    label: &str,
) -> Result<(), TaskError> {
    let start = session.pty.output().len();
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, label)?;
    ensure_terminal_restored(session, start, label)?;
    let actual = read_bounded_fixture(path, PROOF_BYTES_MAX)?;
    if actual != expected {
        return Err(io::Error::other("terminal input executed extra bytes before EOF").into());
    }
    Ok(())
}
