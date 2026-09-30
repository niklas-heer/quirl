//! Process-wide serialization of process spawning and descriptor creation.
//!
//! Failure model: on macOS, and wherever the platform lacks atomic
//! close-on-exec creation, a pipe or PTY descriptor exists briefly without
//! `FD_CLOEXEC`. If another thread forks and executes a program in that
//! window, the new program inherits the descriptor. When that program lives
//! long, such as a process-group anchor waiting on standard input or an
//! interactive editor, the leaked write end keeps a pipe open: the reader
//! never sees end-of-file, so a pipeline stage or even `Command::spawn`, which
//! reads its own status pipe until `exec`, can wait forever.
//!
//! Invariant: every product spawn and every non-atomic creation of an
//! inheritable descriptor holds [`spawn_guard`], so no fork can observe a
//! descriptor before it is marked close-on-exec. Holders keep the guard only
//! for the fork/exec or descriptor setup itself, never while waiting for a
//! child, and never acquire it recursively.

use std::{
    io,
    process::{Child, Command},
    sync::{Mutex, MutexGuard, PoisonError},
};

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Hold the process-wide spawn lock.
///
/// Take it around `fork`/`exec` and around creation of pipes or PTYs whose
/// close-on-exec flag is set after creation. A poisoned lock is still usable
/// because it protects no data, only ordering.
pub fn spawn_guard() -> MutexGuard<'static, ()> {
    SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Spawn `command` while holding [`spawn_guard`].
///
/// # Errors
///
/// Returns the operating-system error from [`Command::spawn`].
pub fn spawn_serialized(command: &mut Command) -> io::Result<Child> {
    let _guard = spawn_guard();
    command.spawn()
}

/// Run `create` while holding [`spawn_guard`], for descriptor constructors
/// such as `os_pipe::pipe` that mark close-on-exec after creation.
pub(crate) fn spawn_guard_scope<T>(create: impl FnOnce() -> T) -> T {
    let _guard = spawn_guard();
    create()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn spawns_wait_while_another_thread_holds_the_spawn_guard() {
        // Goal: a descriptor-creating section and a spawn never overlap.
        // Method: hold the guard, start a spawn on another thread, and prove
        // it only completes after the guard is released.
        let guard = spawn_guard();
        let (spawned, observed) = mpsc::channel();
        let spawner = thread::spawn(move || {
            let mut child = spawn_serialized(&mut Command::new("/usr/bin/true")).unwrap();
            spawned.send(Instant::now()).unwrap();
            child.wait().unwrap();
        });
        assert!(observed.recv_timeout(Duration::from_millis(200)).is_err());
        let released = Instant::now();
        drop(guard);
        let spawned_at = observed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(spawned_at >= released);
        spawner.join().unwrap();
    }
}
