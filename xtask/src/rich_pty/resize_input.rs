//! Coalesced resize and input regression with real PTY execution evidence.
//!
//! A backend can lose readable input when SIGWINCH and terminal readiness arrive
//! in one batch. Stop the child only after its command text is visibly settled,
//! queue the resize and sole Enter, and resume it. Each of 32 cycles per profile must produce
//! a unique output line and fresh input-ready sequence; no rescue keys or delays
//! are sent. The existing PTY owner kills and reaps even a stopped child on error.

use super::{
    STARTUP_MARKER, Session, SessionOptions, TaskError, VirtualScreen, ensure_status,
    ensure_terminal_restored, key, wait_for_rich_input_since,
};
use nix::{
    sys::{
        signal::{Signal, kill},
        wait::{WaitPidFlag, WaitStatus, waitpid},
    },
    unistd::Pid,
};
use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

/// Probe 32 stopped-child and 32 immediate SIGWINCH/input deliveries.
/// Stops are confirmed within two seconds; all input and output waits retain the
/// PTY owner's existing deadlines and byte limits. No terminal input follows
/// Enter until that command has executed and the editor is ready again.
pub(super) fn check_resize_input(binary: &Path) -> Result<(), TaskError> {
    let mut session = Session::new(binary, SessionOptions::default())?;
    session.pty.wait_for(STARTUP_MARKER)?;
    for coalesced in [true, false] {
        for cycle in 0..32 {
            session.pty.resize(24, 72)?;
            let profile = if coalesced { "STOPPED" } else { "LIVE" };
            let token = format!("RESIZE_{profile}_{cycle:02}");
            let command = format!("/usr/bin/printf '%s\\n' {token}");
            session.pty.type_text(&command)?;
            session
                .pty
                .wait_for_screen("command settled before coalesced resize", |screen| {
                    screen
                        .lines()
                        .iter()
                        .any(|line| line.trim() == format!("> {command}"))
                })?;
            let child = session
                .pty
                .child_pid()
                .ok_or_else(|| io::Error::other("resize regression lost its child"))?;
            if coalesced {
                stop_child(child)?;
            }
            let output_start = session.pty.output().len();
            session.pty.resize(40, 120)?;
            session.pty.send(key::ENTER)?;
            if coalesced {
                kill(child, Signal::SIGCONT)?;
            }
            session.pty.wait_for_screen(
                &format!("sole Enter executes {token} after resize"),
                |screen| completed(screen, &token),
            )?;
            wait_for_rich_input_since(&mut session, output_start)?;
        }
    }
    let output_start = session.pty.output().len();
    session.pty.send(key::CTRL_D)?;
    ensure_status(session.pty.wait_exit()?, 0, "coalesced resize input")?;
    ensure_terminal_restored(&session, output_start, "coalesced resize input")
}

fn completed(screen: &VirtualScreen, token: &str) -> bool {
    screen.lines().iter().any(|line| {
        // Retained output adds a scrollbar in the final terminal column once
        // the viewport fills; it is decoration, not part of command output.
        line.trim_end() == token
            || (unicode_width::UnicodeWidthStr::width(line.as_str()) == screen.columns()
                && line
                    .strip_prefix(token)
                    .is_some_and(|suffix| matches!(suffix.trim(), "#" | "|")))
    })
}

fn stop_child(child: Pid) -> Result<(), TaskError> {
    kill(child, Signal::SIGSTOP)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(2))
        .ok_or_else(|| io::Error::other("stop confirmation deadline overflowed"))?;
    loop {
        match waitpid(child, Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WNOHANG))? {
            WaitStatus::Stopped(_, Signal::SIGSTOP) => return Ok(()),
            WaitStatus::StillAlive if Instant::now() < deadline => {
                // This wait confirms suspension before either event is queued;
                // it never gives the backend extra time to consume input.
                std::thread::sleep(Duration::from_millis(1));
            }
            status => {
                return Err(io::Error::other(format!(
                    "could not confirm stopped resize-test child: {status:?}"
                ))
                .into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PtySession, SpawnOptions, TempDirectory};
    use super::*;

    #[test]
    fn command_echo_and_other_cycle_output_cannot_satisfy_execution_oracle() {
        let mut screen = VirtualScreen::new(4, 80, 0).unwrap();
        screen.feed(b"> /usr/bin/printf RESIZE_INPUT_01\r\nRESIZE_INPUT_00\x1b[4;1HNORMAL");
        assert!(!completed(&screen, "RESIZE_INPUT_01"));
        screen.feed(b"\x1b[3;1HRESIZE_INPUT_01");
        assert!(completed(&screen, "RESIZE_INPUT_01"));
        screen.feed(b"\x1b[3;80H#");
        assert!(completed(&screen, "RESIZE_INPUT_01"));
    }

    #[test]
    fn dropping_the_owner_kills_and_reaps_a_stopped_resize_test_child() {
        let directory = TempDirectory::new("resize-stop-cleanup").unwrap();
        let mut pty = PtySession::spawn(SpawnOptions::new(
            ["/bin/sh", "-c", "printf STOP_READY; exec sleep 30"]
                .into_iter()
                .map(Into::into)
                .collect(),
            directory.path.clone(),
        ))
        .unwrap();
        pty.wait_for(b"STOP_READY").unwrap();
        let child = pty.child_pid().unwrap();
        stop_child(child).unwrap();
        drop(pty);
        assert_eq!(
            waitpid(child, Some(WaitPidFlag::WNOHANG)),
            Err(nix::errno::Errno::ECHILD)
        );
    }
}
