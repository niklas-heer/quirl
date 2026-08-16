//! Bounded admission for user-controlled regular files at the CLI boundary.
//!
//! Paths are mutable namespace references: they may identify special files or
//! be replaced while admission is in progress. Unix therefore opens with
//! `O_NONBLOCK`, and every platform validates type and size from the exact open
//! handle. Reads retain at most the configured limit plus one sentinel byte so
//! post-metadata growth fails closed without unbounded allocation.

use quirl_core::{ErrorCode, ShellError};
use std::{
    fs::{File, OpenOptions},
    io::Read,
    path::Path,
};

#[derive(Clone, Copy)]
pub(crate) struct ReadFileOptions<'a> {
    pub(crate) path: &'a Path,
    pub(crate) bytes_max: usize,
    pub(crate) context: &'a str,
    pub(crate) help: &'a str,
    pub(crate) io_error_code: ErrorCode,
}

pub(crate) fn read_regular_file(options: ReadFileOptions<'_>) -> Result<Vec<u8>, ShellError> {
    let Some(bytes) = read_regular_file_with_hook(options, false, || {})? else {
        return Err(ShellError::new(
            options.io_error_code,
            format!(
                "could not open {} {}",
                options.context,
                options.path.display()
            ),
        )
        .with_context("required file disappeared during admission")
        .with_help(options.help));
    };
    Ok(bytes)
}

pub(crate) fn read_optional_regular_file(
    options: ReadFileOptions<'_>,
) -> Result<Option<Vec<u8>>, ShellError> {
    read_regular_file_with_hook(options, true, || {})
}

fn read_regular_file_with_hook(
    options: ReadFileOptions<'_>,
    missing_is_none: bool,
    after_admission: impl FnOnce(),
) -> Result<Option<Vec<u8>>, ShellError> {
    let file = match open_nonblocking(options.path) {
        Ok(file) => file,
        Err(error) if missing_is_none && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) if open_error_identifies_special_file(&error) => {
            return Err(nonregular_file_error(options).with_context(error.to_string()));
        }
        Err(error) => return Err(io_error(options, "open", error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| io_error(options, "inspect", error))?;
    if !metadata.file_type().is_file() {
        return Err(nonregular_file_error(options));
    }
    let bytes_max_u64 = u64::try_from(options.bytes_max).unwrap_or(u64::MAX);
    if metadata.len() > bytes_max_u64 {
        return Err(limit_error(options, metadata.len(), false));
    }

    after_admission();

    let capacity = usize::try_from(metadata.len())
        .unwrap_or(options.bytes_max)
        .min(options.bytes_max);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(bytes_max_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(options, "read", error))?;
    if bytes.len() > options.bytes_max {
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        return Err(limit_error(options, observed, true));
    }
    Ok(Some(bytes))
}

fn io_error(options: ReadFileOptions<'_>, action: &str, error: std::io::Error) -> ShellError {
    ShellError::new(
        options.io_error_code,
        format!(
            "could not {action} {} {}",
            options.context,
            options.path.display()
        ),
    )
    .with_context(error.to_string())
    .with_help(options.help)
}

fn nonregular_file_error(options: ReadFileOptions<'_>) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "{} {} is not a regular file",
            options.context,
            options.path.display()
        ),
    )
    .with_context("directories, FIFOs, sockets, and device nodes are rejected")
    .with_help(options.help)
}

#[cfg(unix)]
fn open_error_identifies_special_file(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == nix::libc::EOPNOTSUPP
                || code == nix::libc::ENXIO
                || code == nix::libc::ENODEV
    )
}

#[cfg(not(unix))]
fn open_error_identifies_special_file(_error: &std::io::Error) -> bool {
    false
}

fn limit_error(options: ReadFileOptions<'_>, observed: u64, is_lower_bound: bool) -> ShellError {
    let observed = if is_lower_bound {
        format!("at least {observed}")
    } else {
        observed.to_string()
    };
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!(
            "{} {} exceeds its read limit",
            options.context,
            options.path.display()
        ),
    )
    .with_context(format!(
        "limit: {}; observed: {observed}",
        options.bytes_max
    ))
    .with_help(options.help)
}

fn open_nonblocking(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(nix::libc::O_NONBLOCK);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Write,
        path::PathBuf,
        sync::{
            atomic::{AtomicU64, Ordering},
            mpsc,
        },
        time::Duration,
    };

    static TEST_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestPath(PathBuf);

    impl TestPath {
        fn new(label: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "quirl-bounded-file-{label}-{}-{}",
                std::process::id(),
                TEST_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )))
        }

        fn options(&self, bytes_max: usize) -> ReadFileOptions<'_> {
            ReadFileOptions {
                path: &self.0,
                bytes_max,
                context: "test input",
                help: "Pass a bounded regular test file",
                io_error_code: ErrorCode::Io,
            }
        }
    }

    impl Drop for TestPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn exact_limit_is_accepted_and_limit_plus_one_is_rejected() {
        let path = TestPath::new("limits");
        fs::write(&path.0, b"1234").unwrap();
        assert_eq!(read_regular_file(path.options(4)).unwrap(), b"1234");

        fs::write(&path.0, b"12345").unwrap();
        let error = read_regular_file(path.options(4)).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 4"));
        assert!(error.details.context[0].contains("observed: 5"));
    }

    #[test]
    fn growth_after_handle_admission_is_rejected_by_the_limit_plus_one_read() {
        let path = TestPath::new("growth");
        fs::write(&path.0, b"1234").unwrap();
        let error = read_regular_file_with_hook(path.options(4), false, || {
            let mut file = OpenOptions::new().append(true).open(&path.0).unwrap();
            file.write_all(b"5").unwrap();
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("observed: at least 5"));
    }

    #[cfg(unix)]
    #[test]
    fn path_replacement_after_open_does_not_change_the_admitted_handle() {
        let path = TestPath::new("replacement");
        let replacement = TestPath::new("replacement-source");
        fs::write(&path.0, b"opened").unwrap();
        fs::write(&replacement.0, b"replacement").unwrap();

        let bytes = read_regular_file_with_hook(path.options(32), false, || {
            fs::rename(&replacement.0, &path.0).unwrap();
        })
        .unwrap()
        .unwrap();
        assert_eq!(bytes, b"opened");
        assert_eq!(fs::read(&path.0).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn fifo_without_a_writer_is_rejected_without_blocking() {
        use nix::{sys::stat::Mode, unistd::mkfifo};

        let path = TestPath::new("fifo");
        mkfifo(&path.0, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let worker_path = path.0.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let options = ReadFileOptions {
                path: &worker_path,
                bytes_max: 16,
                context: "test input",
                help: "Pass a bounded regular test file",
                io_error_code: ErrorCode::Io,
            };
            let _ = sender.send(read_regular_file(options));
        });
        let error = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("FIFO admission must not block")
            .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("not a regular file"));
    }
}
