//! Crash-safe replacement of an existing bounded regular file.
//!
//! # Failure model and invariants
//!
//! - Temporary creation can collide or fail. Candidates use bounded retries,
//!   same-directory names, and create-new semantics, so an existing entry is
//!   never truncated. A guard removes a candidate on every returned failure.
//! - A write can stop after any byte, flushing or syncing can fail, and copying
//!   permissions can fail. The target is untouched until the complete candidate
//!   contents and intended permissions have both been synchronized.
//! - Replacement can succeed before the parent-directory sync fails. A verified
//!   hard link retains the original inode before replacement; returned failures
//!   roll it back, while a crash leaves either the old target or a complete new
//!   target plus the recoverable original sibling. Directory sync makes each
//!   committed namespace transition durable on Unix. Rust cannot portably sync
//!   a directory on other platforms, so those platforms guarantee synchronized
//!   file contents but only the operating system's rename durability.
//! - Inputs that are symbolic links or non-regular files are rejected. Unix also
//!   rejects targets that already have multiple hard links. Stable Rust does not
//!   expose the Windows link count, so Windows replaces only the named entry and
//!   leaves other hard-link aliases unchanged. A concurrent replacement or
//!   in-place modification observed before commit is rejected by comparing the
//!   expected bytes and stable file identity again after retaining the original.
//!   Portable Rust has no compare-and-swap rename, so an uncooperative writer in
//!   the final validation-to-rename window follows normal last-writer-wins rename
//!   semantics; it cannot cause a partial candidate to be installed.
//! - Cleanup failure never replaces the operation's originating [`ShellError`].
//!   It is appended as context. If restoring the target fails, the error names
//!   the retained recovery path instead of deleting the only original bytes.
//! - Output and validation reads are capped by the caller-supplied byte limit.
//!   Temporary-name attempts are capped, and no operation follows a target link
//!   intentionally.

use crate::{ErrorCode, ShellError};
use std::{
    ffi::OsString,
    fs::{self, File, Metadata, OpenOptions, Permissions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(unix)]
use nix::{
    fcntl::{open, OFlag},
    sys::stat::Mode,
};

const TEMPORARY_NAME_ATTEMPTS_MAX: usize = 64;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Explicit bounds for one atomic replacement transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtomicReplaceOptions {
    /// Maximum bytes permitted in both the expected source and replacement.
    ///
    /// The transaction reads at most this value plus one byte while checking
    /// for concurrent modification. Zero is invalid.
    pub bytes_max: usize,
}

/// Replace an existing regular file without exposing partial formatted output.
///
/// `expected` must contain the exact bytes from which `replacement` was
/// derived. Both are limited by [`AtomicReplaceOptions::bytes_max`]. The target
/// must be a regular file: symbolic links and special files fail closed, as do
/// pre-existing hard-link aliases on Unix. Platforms without a stable link-count
/// API replace only the named entry. The function preserves the target's
/// permissions, writes and synchronizes a create-new sibling, retains a recovery
/// hard link, atomically replaces the directory entry, and syncs the containing
/// directory on Unix.
///
/// A concurrent change observed before replacement returns
/// [`ErrorCode::Validation`]. I/O and durability failures return
/// [`ErrorCode::Io`], resource violations return [`ErrorCode::ResourceLimit`],
/// and unsupported target kinds return [`ErrorCode::InvalidArgument`]. Every
/// returned failure leaves the original bytes at the target or, if rollback
/// itself fails, at a recovery path named in the error context.
pub fn replace_file_atomically(
    path: &Path,
    expected: &[u8],
    replacement: &[u8],
    options: AtomicReplaceOptions,
) -> Result<(), ShellError> {
    replace_file_atomically_with_hook(path, expected, replacement, options, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionStage {
    TemporaryCreated,
    PartialWrite,
    Flushed,
    ContentSynced,
    PermissionsUpdated,
    MetadataSynced,
    OriginalRetained,
    CandidateRenamed,
    ParentSynced,
}

impl TransactionStage {
    fn description(self) -> &'static str {
        match self {
            Self::TemporaryCreated => "creating the temporary file",
            Self::PartialWrite => "writing the temporary file",
            Self::Flushed => "flushing the temporary file",
            Self::ContentSynced => "synchronizing temporary file contents",
            Self::PermissionsUpdated => "preserving target permissions",
            Self::MetadataSynced => "synchronizing temporary file metadata",
            Self::OriginalRetained => "retaining the original source",
            Self::CandidateRenamed => "replacing the source",
            Self::ParentSynced => "synchronizing the source directory",
        }
    }
}

#[derive(Debug)]
struct TargetSnapshot {
    identity: FileIdentity,
    permissions: Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    creation_time: u64,
    #[cfg(windows)]
    last_write_time: u64,
    #[cfg(windows)]
    file_size: u64,
    #[cfg(windows)]
    attributes: u32,
    #[cfg(not(any(unix, windows)))]
    length: u64,
}

#[derive(Debug)]
struct TransactionFiles {
    candidate: Option<PathBuf>,
    recovery_path: PathBuf,
    recovery_owned: bool,
}

impl TransactionFiles {
    fn new(candidate: PathBuf, recovery: PathBuf) -> Self {
        Self {
            candidate: Some(candidate),
            recovery_path: recovery,
            recovery_owned: false,
        }
    }

    fn candidate(&self) -> &Path {
        self.candidate
            .as_deref()
            .unwrap_or_else(|| Path::new("<installed-candidate>"))
    }

    fn recovery(&self) -> &Path {
        &self.recovery_path
    }

    fn candidate_was_installed(&mut self) {
        self.candidate = None;
    }

    fn recovery_was_created(&mut self) {
        self.recovery_owned = true;
    }

    fn recovery_was_consumed(&mut self) {
        self.recovery_owned = false;
    }

    fn preserve_recovery(&mut self) -> Option<PathBuf> {
        self.recovery_owned.then(|| {
            self.recovery_owned = false;
            self.recovery_path.clone()
        })
    }

    fn cleanup(&mut self, mut error: ShellError) -> ShellError {
        let mut namespace_changed = false;
        if let Some(candidate) = self.candidate.take() {
            match remove_if_present(&candidate) {
                Ok(()) => namespace_changed = true,
                Err(cleanup_error) => {
                    error = error.with_context(format!(
                        "temporary cleanup failed for {}: {cleanup_error}",
                        candidate.display()
                    ));
                }
            }
        }
        if self.recovery_owned {
            self.recovery_owned = false;
            match remove_if_present(&self.recovery_path) {
                Ok(()) => namespace_changed = true,
                Err(cleanup_error) => {
                    error = error.with_context(format!(
                        "recovery cleanup failed for {}: {cleanup_error}",
                        self.recovery_path.display()
                    ));
                }
            }
        }
        if namespace_changed {
            if let Err(cleanup_error) = sync_parent(&self.recovery_path) {
                error = error.with_context(format!(
                    "transaction cleanup directory sync failed: {cleanup_error}"
                ));
            }
        }
        error
    }
}

impl Drop for TransactionFiles {
    fn drop(&mut self) {
        if let Some(candidate) = self.candidate.take() {
            let _ = remove_if_present(&candidate);
        }
        if self.recovery_owned {
            self.recovery_owned = false;
            let _ = remove_if_present(&self.recovery_path);
        }
        let _ = sync_parent(&self.recovery_path);
    }
}

fn replace_file_atomically_with_hook(
    path: &Path,
    expected: &[u8],
    replacement: &[u8],
    options: AtomicReplaceOptions,
    mut after_stage: impl FnMut(TransactionStage) -> io::Result<()>,
) -> Result<(), ShellError> {
    validate_options(path, expected, replacement, options)?;
    let snapshot = inspect_expected_target(path, expected, options.bytes_max)?;
    let (candidate_path, candidate_file) = create_candidate(path)?;
    let recovery_path = recovery_path(&candidate_path);
    let mut files = TransactionFiles::new(candidate_path, recovery_path);

    let prepared = prepare_candidate(
        files.candidate(),
        candidate_file,
        replacement,
        snapshot.permissions.clone(),
        &mut after_stage,
    );
    if let Err(error) = prepared {
        return Err(files.cleanup(error));
    }

    let committed = commit_candidate(
        path,
        expected,
        options.bytes_max,
        &snapshot.identity,
        &mut files,
        &mut after_stage,
    );
    if let Err(error) = committed {
        return Err(files.cleanup(error));
    }
    Ok(())
}

fn validate_options(
    path: &Path,
    expected: &[u8],
    replacement: &[u8],
    options: AtomicReplaceOptions,
) -> Result<(), ShellError> {
    if options.bytes_max == 0 {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "atomic replacement byte limit must be greater than zero",
        )
        .with_help("Pass the formatter's positive source-size limit"));
    }
    for (description, observed) in [
        ("expected source", expected.len()),
        ("replacement source", replacement.len()),
    ] {
        if observed > options.bytes_max {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                format!(
                    "{description} for {} exceeds its replacement limit",
                    path.display()
                ),
            )
            .with_context(format!(
                "limit: {}; observed: {observed}",
                options.bytes_max
            ))
            .with_help("Reduce the source size before formatting"));
        }
    }
    Ok(())
}

fn prepare_candidate(
    candidate_path: &Path,
    mut candidate: File,
    replacement: &[u8],
    permissions: Permissions,
    after_stage: &mut impl FnMut(TransactionStage) -> io::Result<()>,
) -> Result<(), ShellError> {
    observe_stage(
        candidate_path,
        TransactionStage::TemporaryCreated,
        after_stage,
    )?;
    let split = replacement.len().div_ceil(2);
    candidate
        .write_all(&replacement[..split])
        .map_err(|error| transaction_io_error("write candidate", candidate_path, error))?;
    observe_stage(candidate_path, TransactionStage::PartialWrite, after_stage)?;
    candidate
        .write_all(&replacement[split..])
        .map_err(|error| transaction_io_error("write candidate", candidate_path, error))?;
    candidate
        .flush()
        .map_err(|error| transaction_io_error("flush candidate", candidate_path, error))?;
    observe_stage(candidate_path, TransactionStage::Flushed, after_stage)?;
    candidate
        .sync_all()
        .map_err(|error| transaction_io_error("sync candidate contents", candidate_path, error))?;
    observe_stage(candidate_path, TransactionStage::ContentSynced, after_stage)?;
    candidate.set_permissions(permissions).map_err(|error| {
        transaction_io_error("preserve candidate permissions", candidate_path, error)
    })?;
    observe_stage(
        candidate_path,
        TransactionStage::PermissionsUpdated,
        after_stage,
    )?;
    candidate
        .sync_all()
        .map_err(|error| transaction_io_error("sync candidate metadata", candidate_path, error))?;
    observe_stage(
        candidate_path,
        TransactionStage::MetadataSynced,
        after_stage,
    )
}

fn commit_candidate(
    path: &Path,
    expected: &[u8],
    bytes_max: usize,
    identity: &FileIdentity,
    files: &mut TransactionFiles,
    after_stage: &mut impl FnMut(TransactionStage) -> io::Result<()>,
) -> Result<(), ShellError> {
    retain_original(path, files, identity)?;
    sync_parent(path)?;
    observe_stage(path, TransactionStage::OriginalRetained, after_stage)?;
    validate_retained_target(path, files.recovery(), expected, bytes_max, identity)?;

    fs::rename(files.candidate(), path)
        .map_err(|error| transaction_io_error("replace target", path, error))?;
    files.candidate_was_installed();
    if let Err(error) = observe_stage(path, TransactionStage::CandidateRenamed, after_stage) {
        return Err(rollback_original(path, files, error));
    }
    if let Err(error) = sync_parent(path) {
        return Err(rollback_original(path, files, error));
    }
    if let Err(error) = observe_stage(path, TransactionStage::ParentSynced, after_stage) {
        return Err(rollback_original(path, files, error));
    }

    if let Err(error) = fs::remove_file(files.recovery()) {
        let error = transaction_io_error("remove retained original", files.recovery(), error);
        return Err(rollback_original(path, files, error));
    }
    files.recovery_was_consumed();
    // The candidate and its directory entry are already durable. A failed
    // cleanup sync may leave the recovery link visible after a crash, but must
    // not turn a committed complete replacement into an ambiguous failure.
    let _ = sync_parent(path);
    Ok(())
}

fn retain_original(
    path: &Path,
    files: &mut TransactionFiles,
    expected_identity: &FileIdentity,
) -> Result<(), ShellError> {
    let recovery = files.recovery().to_path_buf();
    match fs::hard_link(path, &recovery) {
        Ok(()) => {}
        Err(error) => {
            return Err(
                transaction_io_error("retain original source", &recovery, error).with_help(
                    "Move the source to a filesystem that supports same-directory hard links",
                ),
            );
        }
    }
    files.recovery_was_created();
    let metadata = fs::symlink_metadata(&recovery)
        .map_err(|error| transaction_io_error("inspect retained original", &recovery, error))?;
    validate_regular_metadata(&recovery, &metadata, Some(2))?;
    if &file_identity(&recovery, &metadata)? != expected_identity {
        return Err(concurrent_change_error(path));
    }
    Ok(())
}

fn validate_retained_target(
    path: &Path,
    recovery: &Path,
    expected: &[u8],
    bytes_max: usize,
    expected_identity: &FileIdentity,
) -> Result<(), ShellError> {
    let target = inspect_retained_entry(path, path, expected, bytes_max)?;
    let retained = inspect_retained_entry(path, recovery, expected, bytes_max)?;
    if &target.identity != expected_identity || retained.identity != target.identity {
        return Err(concurrent_change_error(path));
    }
    Ok(())
}

fn inspect_retained_entry(
    source_path: &Path,
    entry_path: &Path,
    expected: &[u8],
    bytes_max: usize,
) -> Result<TargetSnapshot, ShellError> {
    inspect_target(entry_path, expected, bytes_max, Some(2)).map_err(|error| {
        if error.code == ErrorCode::InvalidArgument {
            concurrent_change_error(source_path).with_context(error.message)
        } else {
            error
        }
    })
}

fn rollback_original(
    path: &Path,
    files: &mut TransactionFiles,
    mut originating_error: ShellError,
) -> ShellError {
    let recovery = files.recovery().to_path_buf();
    match fs::rename(&recovery, path) {
        Ok(()) => {
            files.recovery_was_consumed();
            if let Err(sync_error) = sync_parent(path) {
                originating_error = originating_error.with_context(format!(
                    "original source was restored but its directory sync failed: {sync_error}"
                ));
            }
        }
        Err(restore_error) => {
            let retained = files.preserve_recovery();
            originating_error = originating_error
                .with_context(format!("automatic restore failed: {restore_error}"))
                .with_context(format!(
                    "original source remains at {}",
                    retained.as_deref().unwrap_or(&recovery).display()
                ))
                .with_help("Restore the retained original before retrying the formatter");
        }
    }
    originating_error
}

fn inspect_expected_target(
    path: &Path,
    expected: &[u8],
    bytes_max: usize,
) -> Result<TargetSnapshot, ShellError> {
    inspect_target(path, expected, bytes_max, Some(1))
}

fn inspect_target(
    path: &Path,
    expected: &[u8],
    bytes_max: usize,
    links_expected: Option<u64>,
) -> Result<TargetSnapshot, ShellError> {
    let link_metadata = fs::symlink_metadata(path)
        .map_err(|error| transaction_io_error("inspect target", path, error))?;
    validate_regular_metadata(path, &link_metadata, links_expected)?;
    let mut file = open_regular_no_follow(path)
        .map_err(|error| transaction_io_error("open target", path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| transaction_io_error("inspect open target", path, error))?;
    validate_regular_metadata(path, &metadata, links_expected)?;
    let identity = file_identity(path, &metadata)?;
    if identity != file_identity(path, &link_metadata)? {
        return Err(concurrent_change_error(path));
    }
    let mut bytes = Vec::with_capacity(expected.len().min(bytes_max));
    Read::by_ref(&mut file)
        .take(bytes_max.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| transaction_io_error("read target", path, error))?;
    if bytes.len() > bytes_max {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            format!("source {} exceeds its replacement limit", path.display()),
        )
        .with_context(format!("limit: {bytes_max}; observed: {}", bytes.len()))
        .with_help("Reduce the source size before formatting"));
    }
    if bytes != expected {
        return Err(concurrent_change_error(path));
    }
    Ok(TargetSnapshot {
        identity,
        permissions: metadata.permissions(),
    })
}

fn validate_regular_metadata(
    path: &Path,
    metadata: &Metadata,
    links_expected: Option<u64>,
) -> Result<(), ShellError> {
    if !metadata.file_type().is_file() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} is not an unlinked regular file", path.display()),
        )
        .with_help("Pass a real regular source file, not a symlink or special file"));
    }
    if let Some(expected) = links_expected {
        if let Some(observed) = hard_link_count(metadata) {
            if observed == expected {
                return Ok(());
            }
            return Err(ShellError::new(
                ErrorCode::InvalidArgument,
                format!("{} has hard-link aliases", path.display()),
            )
            .with_context(format!("expected links: {expected}; observed: {observed}"))
            .with_help(
                "Format a copied regular file so replacing it cannot split hard-link aliases",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_no_follow(path: &Path) -> io::Result<File> {
    let descriptor = open(
        path,
        OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    Ok(File::from(descriptor))
}

#[cfg(not(unix))]
fn open_regular_no_follow(path: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn file_identity(_path: &Path, metadata: &Metadata) -> Result<FileIdentity, ShellError> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(_path: &Path, metadata: &Metadata) -> Result<FileIdentity, ShellError> {
    use std::os::windows::fs::MetadataExt;
    Ok(FileIdentity {
        creation_time: metadata.creation_time(),
        last_write_time: metadata.last_write_time(),
        file_size: metadata.file_size(),
        attributes: metadata.file_attributes(),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_path: &Path, metadata: &Metadata) -> Result<FileIdentity, ShellError> {
    Ok(FileIdentity {
        length: metadata.len(),
    })
}

#[cfg(unix)]
fn hard_link_count(metadata: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.nlink())
}

#[cfg(not(unix))]
fn hard_link_count(_metadata: &Metadata) -> Option<u64> {
    None
}

fn create_candidate(path: &Path) -> Result<(PathBuf, File), ShellError> {
    for _ in 0..TEMPORARY_NAME_ATTEMPTS_MAX {
        let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = temporary_path(path, sequence)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(transaction_io_error(
                    "create temporary candidate",
                    &candidate,
                    error,
                ));
            }
        }
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        format!(
            "could not reserve a temporary source beside {}",
            path.display()
        ),
    )
    .with_context(format!(
        "limit: {TEMPORARY_NAME_ATTEMPTS_MAX}; observed: {TEMPORARY_NAME_ATTEMPTS_MAX}"
    ))
    .with_help("Remove stale .quirl-format temporary files and retry"))
}

fn temporary_path(path: &Path, sequence: u64) -> Result<PathBuf, ShellError> {
    let name = path.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{} has no source file name", path.display()),
        )
        .with_help("Pass a path to an existing source file")
    })?;
    let mut temporary_name = OsString::from(".");
    temporary_name.push(name);
    temporary_name.push(format!(".quirl-format-{}-{sequence}", std::process::id()));
    Ok(path.with_file_name(temporary_name))
}

fn recovery_path(candidate: &Path) -> PathBuf {
    let mut value = OsString::from(candidate.as_os_str());
    value.push(".original");
    PathBuf::from(value)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), ShellError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| transaction_io_error("sync parent directory", parent, error))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), ShellError> {
    Ok(())
}

fn observe_stage(
    path: &Path,
    stage: TransactionStage,
    after_stage: &mut impl FnMut(TransactionStage) -> io::Result<()>,
) -> Result<(), ShellError> {
    after_stage(stage).map_err(|error| {
        transaction_io_error(stage.description(), path, error)
            .with_context(format!("transaction stage: {stage:?}"))
    })
}

fn concurrent_change_error(path: &Path) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!(
            "source {} changed while it was being formatted",
            path.display()
        ),
    )
    .with_help("Reload the source, review the concurrent changes, and run the formatter again")
}

fn transaction_io_error(operation: &str, path: &Path, error: io::Error) -> ShellError {
    ShellError::new(
        ErrorCode::Io,
        format!("cannot {operation} for {}", path.display()),
    )
    .with_context(error.to_string())
    .with_help("Check filesystem permissions and capacity, then retry formatting")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quirl-atomic-file-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options() -> AtomicReplaceOptions {
        AtomicReplaceOptions { bytes_max: 1024 }
    }

    fn assert_only_source_remains(directory: &Path, source: &Path) {
        let entries = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![source.to_path_buf()]);
    }

    #[test]
    fn complete_replacement_preserves_permissions_and_cleans_transaction_files() {
        let directory = TestDirectory::new("success");
        let source = directory.0.join("script.qrl");
        fs::write(&source, b"old source\n").unwrap();
        let mut permissions = fs::metadata(&source).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&source, permissions).unwrap();

        replace_file_atomically(&source, b"old source\n", b"new source\n", options()).unwrap();

        assert_eq!(fs::read(&source).unwrap(), b"new source\n");
        assert!(fs::metadata(&source).unwrap().permissions().readonly());
        assert_only_source_remains(&directory.0, &source);
    }

    #[test]
    fn every_transaction_stage_failure_restores_original_and_cleans_temporary_files() {
        let stages = [
            TransactionStage::TemporaryCreated,
            TransactionStage::PartialWrite,
            TransactionStage::Flushed,
            TransactionStage::ContentSynced,
            TransactionStage::PermissionsUpdated,
            TransactionStage::MetadataSynced,
            TransactionStage::OriginalRetained,
            TransactionStage::CandidateRenamed,
            TransactionStage::ParentSynced,
        ];
        for failed_stage in stages {
            let directory = TestDirectory::new(failed_stage.description());
            let source = directory.0.join("script.lua");
            fs::write(&source, b"original bytes\n").unwrap();
            let error = replace_file_atomically_with_hook(
                &source,
                b"original bytes\n",
                b"completely formatted bytes\n",
                options(),
                |stage| {
                    if stage == failed_stage {
                        Err(io::Error::other("injected transaction failure"))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();

            assert_eq!(error.code, ErrorCode::Io, "stage: {failed_stage:?}");
            assert!(error
                .details
                .context
                .iter()
                .any(|context| context.contains("injected transaction failure")));
            assert_eq!(fs::read(&source).unwrap(), b"original bytes\n");
            assert_only_source_remains(&directory.0, &source);
        }
    }

    #[test]
    fn concurrent_change_is_rejected_before_replacement() {
        let directory = TestDirectory::new("concurrent");
        let source = directory.0.join("script.qrl");
        fs::write(&source, b"changed elsewhere\n").unwrap();

        let error = replace_file_atomically(
            &source,
            b"source formatter read\n",
            b"formatted source\n",
            options(),
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::Validation);
        assert_eq!(fs::read(&source).unwrap(), b"changed elsewhere\n");
        assert_only_source_remains(&directory.0, &source);
    }

    #[test]
    fn recovery_name_collision_preserves_the_foreign_entry_and_original_source() {
        let directory = TestDirectory::new("recovery-collision");
        let source = directory.0.join("script.qrl");
        fs::write(&source, b"old source\n").unwrap();
        let mut collision = None;

        let error = replace_file_atomically_with_hook(
            &source,
            b"old source\n",
            b"new source\n",
            options(),
            |stage| {
                if stage == TransactionStage::TemporaryCreated {
                    let candidate = fs::read_dir(&directory.0)?
                        .filter_map(Result::ok)
                        .map(|entry| entry.path())
                        .find(|path| {
                            path != &source
                                && !path.as_os_str().as_encoded_bytes().ends_with(b".original")
                        })
                        .ok_or_else(|| io::Error::other("candidate was not visible"))?;
                    let recovery = recovery_path(&candidate);
                    fs::write(&recovery, b"foreign entry\n")?;
                    collision = Some(recovery);
                }
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::Io);
        assert_eq!(fs::read(&source).unwrap(), b"old source\n");
        let collision = collision.unwrap();
        assert_eq!(fs::read(&collision).unwrap(), b"foreign entry\n");
        let entries = fs::read_dir(&directory.0).unwrap().count();
        assert_eq!(entries, 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_hardlinks_and_special_files_fail_closed() {
        use std::os::unix::{fs::symlink, net::UnixListener};

        let directory = TestDirectory::new("target-kinds");
        let source = directory.0.join("source.lua");
        fs::write(&source, b"source\n").unwrap();
        let link = directory.0.join("link.lua");
        symlink(&source, &link).unwrap();
        let error =
            replace_file_atomically(&link, b"source\n", b"formatted\n", options()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(fs::read(&source).unwrap(), b"source\n");

        let alias = directory.0.join("alias.lua");
        fs::hard_link(&source, &alias).unwrap();
        let error =
            replace_file_atomically(&source, b"source\n", b"formatted\n", options()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
        assert_eq!(fs::read(&alias).unwrap(), b"source\n");

        let socket = directory.0.join("socket.lua");
        let _listener = UnixListener::bind(&socket).unwrap();
        let error = replace_file_atomically(&socket, b"", b"formatted\n", options()).unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn replacement_limit_reports_configured_and_observed_bytes() {
        let directory = TestDirectory::new("limit");
        let source = directory.0.join("script.lua");
        fs::write(&source, b"old\n").unwrap();

        let error = replace_file_atomically(
            &source,
            b"old\n",
            b"replacement",
            AtomicReplaceOptions { bytes_max: 4 },
        )
        .unwrap_err();

        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.details.context[0].contains("limit: 4; observed: 11"));
        assert_eq!(fs::read(&source).unwrap(), b"old\n");
    }
}
