//! Real fd and parser-queue checks for a zero-duration Crossterm poll.
//!
//! A private file gate orders the test: the child enters raw mode and announces
//! readiness, the PTY owner queues exactly XY, then creates the gate. Thus the
//! first zero-duration poll must inspect the kernel and the next must inspect
//! already parsed events. The idle third poll must return false. No rescue input
//! or timing delay substitutes for readiness. The gate wait is capped at five
//! seconds, input is two bytes, output at 16 KiB; RAII restores raw mode and
//! the PTY owner kills/reaps the child on any failure.

use super::{PtySession, SpawnOptions, TaskError, TempDirectory, ensure_status};
use crossterm::{
    event::{self, Event, KeyCode},
    terminal,
};
use nix::sys::termios::LocalFlags;
use std::{
    fs::OpenOptions,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{Duration, Instant},
};

const READY: &[u8] = b"ZERO_POLL_READY";
const PASSED: &[u8] = b"ZERO_POLL_PASS";

/// Spawn this xtask under a PTY and prove nonblocking readiness and cleanup.
pub(crate) fn check() -> Result<(), TaskError> {
    let private = TempDirectory::new("zero-poll")?;
    let gate = private.path.join("input-queued");
    let mut options = SpawnOptions::new(
        vec![
            std::env::current_exe()?.into_os_string(),
            "zero-poll-probe".into(),
            "--gate".into(),
            gate.clone().into_os_string(),
        ],
        private.path.clone(),
    );
    options.timeout = Duration::from_secs(8);
    options.output_bytes_max = 16 * 1024;
    let mut pty = PtySession::spawn(options)?;
    pty.wait_for(READY)?;
    pty.send(b"XY")?;
    drop(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&gate)?,
    );
    ensure_status(pty.wait_exit()?, 0, "zero-duration terminal poll")?;
    if !pty
        .output()
        .windows(PASSED.len())
        .any(|bytes| bytes == PASSED)
    {
        return Err(io::Error::other("zero-poll child omitted its success marker").into());
    }
    let modes = pty.terminal_modes()?;
    if !modes.local_flags.contains(LocalFlags::ICANON)
        || !modes.local_flags.contains(LocalFlags::ECHO)
    {
        return Err(io::Error::other("zero-poll child did not restore terminal modes").into());
    }
    pty.close()?;
    println!("ok: xtask-linked Crossterm zero-duration poll (separate from pinned Quirl checks)");
    Ok(())
}

/// Internal child endpoint; the parent owns its private gate and PTY lifetime.
pub(crate) fn run_probe(gate: &Path) -> Result<(), TaskError> {
    terminal::enable_raw_mode()?;
    let _raw_mode = RawMode;
    println!("ZERO_POLL_READY");
    io::stdout().flush()?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .ok_or_else(|| io::Error::other("zero-poll gate deadline overflowed"))?;
    while !gate.try_exists()? {
        if Instant::now() >= deadline {
            return Err(io::Error::other("zero-poll input gate timed out").into());
        }
        // This is only the explicit parent/child barrier, not input readiness.
        std::thread::sleep(Duration::from_millis(1));
    }
    for character in ['X', 'Y'] {
        if !event::poll(Duration::ZERO)? {
            return Err(io::Error::other(format!("zero poll missed queued {character}")).into());
        }
        match event::read()? {
            Event::Key(key) if key.code == KeyCode::Char(character) => {}
            other => {
                return Err(
                    io::Error::other(format!("unexpected zero-poll event: {other:?}")).into(),
                );
            }
        }
    }
    if event::poll(Duration::ZERO)? {
        return Err(io::Error::other("idle zero poll reported an event").into());
    }
    println!("ZERO_POLL_PASS");
    Ok(())
}

struct RawMode;
impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}
