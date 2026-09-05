//! Handle-based admission for bounded UI file readers.
//!
//! Directory listings and path metadata can become stale before open. On Unix,
//! nonblocking open prevents a replacement FIFO from waiting for a writer; the
//! exact opened handle must identify a regular file before any read or seek.
//! Symlinks to regular files remain supported. Callers own byte limits and map
//! I/O errors into their existing diagnostic or optional-context presentation.
//! This cannot bound latency imposed by a stalled filesystem itself.

use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
};

/// Open a readable regular file, following symlinks to regular targets.
///
/// Reject special files before reading. Unix opens without FIFO rendezvous;
/// callers must still enforce their own read-size and filesystem-latency policy.
pub(crate) fn open_regular_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(nix::libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "UI input must be a regular file",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_path() -> std::path::PathBuf {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "quirl-ui-file-admission-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn regular_files_are_admitted_and_directories_are_rejected() {
        let directory = test_path();
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("input");
        std::fs::write(&path, "content").unwrap();
        assert!(open_regular_file(&path).is_ok());
        assert!(open_regular_file(&directory).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fifo_admission_returns_without_waiting_for_a_writer() {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        use std::{sync::mpsc, time::Duration};

        let path = test_path();
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let worker_path = path.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = sender.send(open_regular_file(&worker_path));
        });
        let result = receiver.recv_timeout(Duration::from_secs(1));
        std::fs::remove_file(path).unwrap();
        assert!(result.expect("FIFO admission must not block").is_err());
        worker.join().unwrap();
    }
}
