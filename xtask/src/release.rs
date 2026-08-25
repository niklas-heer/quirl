//! Bounded, deterministic release planning and artifact assembly.
//!
//! The candidate commit and its workspace version are the immutable identity.
//! Git and binary output are admitted through byte and time limits, archives
//! contain stable metadata, and publication outputs never replace different
//! bytes. A failed write leaves either the prior complete file or no file.

use clap::Subcommand;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use crate::assets::AssetManifest;
#[cfg(unix)]
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

const CONTRACT_VERSION: u32 = 1;
const COMMITS_MAX: usize = 1_024;
const GIT_OUTPUT_BYTES_MAX: usize = 4 * 1024 * 1024;
const JSON_BYTES_MAX: usize = 4 * 1024 * 1024;
const CHANGELOG_BYTES_MAX: usize = 4 * 1024 * 1024;
const ARTIFACT_BYTES_MAX: usize = 128 * 1024 * 1024;
const AGGREGATE_FILES_MAX: usize = 128;
const AGGREGATE_DEPTH_MAX: usize = 4;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);
const TEMPORARY_ATTEMPTS_MAX: usize = 32;
const RELEASE_NOTES_PATH: &str = "RELEASE_NOTES.md";
const RELEASE_PREPARATION_COMMIT: &str = "chore(release): prepare next version";
const PRODUCT_LICENSE_PATH: &str = "LICENSE";
const THIRD_PARTY_NOTICES_PATH: &str = "crates/quirl-process/THIRD_PARTY_NOTICES.md";
const ARCHIVE_THIRD_PARTY_NOTICES_PATH: &str = "THIRD_PARTY_NOTICES.md";
const THIRD_PARTY_INVENTORY_PATH: &str = "distribution/quirl-cli-third-party.tsv";
const THIRD_PARTY_LICENSE_FALLBACK_ROOT: &str = "distribution/licenses";
const ARCHIVE_THIRD_PARTY_LICENSES_PATH: &str = "THIRD_PARTY_LICENSES.txt";
const RELEASE_DEPENDENCIES_MAX: usize = 512;
const LICENSE_SCAN_ENTRIES_MAX: usize = 100_000;
const LICENSE_SCAN_DEPTH_MAX: usize = 8;
const LICENSE_DOCUMENTS_MAX: usize = 1_024;
const LICENSE_DOCUMENT_BYTES_MAX: usize = 512 * 1024;
const LICENSE_REPORT_BYTES_MAX: usize = 8 * 1024 * 1024;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// One release lifecycle action.
#[derive(Debug, Subcommand)]
pub(crate) enum ReleaseCommand {
    /// Print the version and deterministic notes implied by reachable history.
    Plan,
    /// Preview or atomically write the product version and changelog changes.
    Prepare {
        /// Apply the plan; without this flag no file is changed.
        #[arg(long)]
        write: bool,
    },
    /// Fail unless the clean candidate is internally consistent and releasable.
    Verify {
        /// Exact tag the caller intends to create or publish.
        #[arg(long)]
        expected_tag: Option<String>,
        /// Fail unless the intended tag is absent from the origin remote.
        #[arg(long, requires = "expected_tag")]
        require_remote_tag_absent: bool,
        /// Fail unless HEAD is the commit currently advertised by this origin branch.
        #[arg(long)]
        required_remote_branch: Option<String>,
    },
    /// Preview or atomically create and push the immutable lightweight release tag.
    Tag {
        /// Exact SemVer tag required by the release plan.
        #[arg(long)]
        expected_tag: String,
        /// Create and push the tag; without this flag no ref is changed.
        #[arg(long)]
        write: bool,
    },
    /// Validate and reproducibly package one already-built native binary.
    Package {
        /// Rust target triple used to build the native executable.
        #[arg(long)]
        target: String,
        /// Directory that receives the immutable package and provenance.
        #[arg(long)]
        output: PathBuf,
    },
    /// Combine native packages and runtime assets into publication metadata.
    Aggregate {
        /// Directory containing downloaded package and asset job outputs.
        #[arg(long)]
        input: PathBuf,
        /// Directory that receives the complete release upload set.
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct SemanticVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Bump {
    FirstRelease,
    None,
    Patch,
    Minor,
    Major,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleaseCommit {
    commit: String,
    subject: String,
    category: String,
    breaking: bool,
    releasing: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReleasePlan {
    schema_version: u32,
    candidate_commit: String,
    candidate_clean: bool,
    previous_tag: Option<String>,
    previous_version: Option<String>,
    workspace_version: String,
    next_version: String,
    bump: Bump,
    commit_count: usize,
    commits: Vec<ReleaseCommit>,
    release_notes: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PrepareResult {
    schema_version: u32,
    write: bool,
    next_version: String,
    changed_paths: Vec<String>,
    release_notes_path: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VerifyResult {
    schema_version: u32,
    version: String,
    candidate_commit: String,
    clean: bool,
    release_notes_sha256: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TagResult {
    schema_version: u32,
    write: bool,
    tag: String,
    candidate_commit: String,
    remote_state: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildInfo {
    schema_version: u32,
    version: String,
    build_profile: String,
    optimization_level: String,
    panic_strategy: String,
    operating_system: String,
    architecture: String,
    source_commit: String,
    build_timestamp: String,
    official_release: bool,
    source_dirty: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Clone, Debug, Deserialize)]
struct CargoPackage {
    name: String,
    version: String,
    license: Option<String>,
    license_file: Option<String>,
    repository: Option<String>,
    source: Option<String>,
    manifest_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DependencyContract {
    name: String,
    version: String,
    declared_license: String,
    source: String,
    repository: String,
}

#[derive(Clone, Debug)]
struct ReleaseDependency {
    contract: DependencyContract,
    package_root: PathBuf,
}

#[derive(Clone, Debug)]
struct InventoryRecord {
    contract: DependencyContract,
    platforms: BTreeSet<String>,
}

#[derive(Debug)]
struct LicenseDocument {
    bytes: Vec<u8>,
    origins: BTreeSet<String>,
}

#[derive(Debug)]
struct PackageLicenseFile {
    relative_path: String,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PackageProvenance {
    pub(crate) schema_version: u32,
    pub(crate) product: String,
    pub(crate) version: String,
    pub(crate) target: String,
    pub(crate) candidate_commit: String,
    pub(crate) source_date_epoch: u64,
    pub(crate) artifact: String,
    pub(crate) byte_size: u64,
    pub(crate) sha256: String,
    pub(crate) build_profile: String,
    pub(crate) optimization_level: String,
    pub(crate) panic_strategy: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseArtifact {
    pub(crate) logical_name: String,
    pub(crate) target: String,
    pub(crate) file: String,
    pub(crate) byte_size: u64,
    pub(crate) sha256: String,
    pub(crate) url: String,
    pub(crate) provenance: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseManifest {
    pub(crate) schema_version: u32,
    pub(crate) product: String,
    pub(crate) version: String,
    pub(crate) tag: String,
    pub(crate) candidate_commit: String,
    pub(crate) source_date_epoch: u64,
    pub(crate) artifacts: Vec<ReleaseArtifact>,
    pub(crate) asset_manifest: Option<String>,
    pub(crate) assets: Vec<crate::assets::AssetRecord>,
}

pub(crate) struct CandidateIdentity {
    pub(crate) version: String,
    pub(crate) commit: String,
    pub(crate) source_date_epoch: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PackageResult {
    schema_version: u32,
    artifact: String,
    provenance: String,
    sha256: String,
    byte_size: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AggregateResult {
    schema_version: u32,
    release_manifest: String,
    checksums: String,
    release_notes: String,
    upload_file_count: usize,
}

pub(crate) fn run(root: &Path, command: ReleaseCommand) -> Result<(), Box<dyn Error>> {
    match command {
        ReleaseCommand::Plan => print_json(&plan(root)?),
        ReleaseCommand::Prepare { write } => prepare(root, write),
        ReleaseCommand::Verify {
            expected_tag,
            require_remote_tag_absent,
            required_remote_branch,
        } => verify(
            root,
            expected_tag.as_deref(),
            require_remote_tag_absent,
            required_remote_branch.as_deref(),
        ),
        ReleaseCommand::Tag {
            expected_tag,
            write,
        } => tag(root, &expected_tag, write),
        ReleaseCommand::Package { target, output } => package(root, &target, &output),
        ReleaseCommand::Aggregate { input, output } => aggregate(root, &input, &output),
    }
}

fn plan(root: &Path) -> Result<ReleasePlan, Box<dyn Error>> {
    let workspace_version = workspace_version(root)?;
    let candidate_commit = git(root, &["rev-parse", "HEAD"], true)?;
    validate_commit(&candidate_commit)?;
    let candidate_clean = git(
        root,
        &["status", "--porcelain", "--untracked-files=normal"],
        true,
    )?
    .is_empty();
    let current_release_tag = format!("v{workspace_version}");
    let candidate_already_tagged = git(
        root,
        &[
            "describe",
            "--tags",
            "--exact-match",
            "--match",
            &current_release_tag,
            "HEAD",
        ],
        false,
    )? == current_release_tag;
    let tag_search_revision = if candidate_already_tagged {
        "HEAD^"
    } else {
        "HEAD"
    };
    let tag = git(
        root,
        &[
            "describe",
            "--tags",
            "--abbrev=0",
            "--match",
            "v[0-9]*",
            tag_search_revision,
        ],
        false,
    )?;
    let previous_tag = (!tag.is_empty()).then_some(tag);
    let previous_version = previous_tag
        .as_deref()
        .map(|value| parse_version(value.trim_start_matches('v')))
        .transpose()?;
    let range = previous_tag
        .as_ref()
        .map_or_else(|| "HEAD".to_owned(), |tag| format!("{tag}..HEAD"));
    let log = git(
        root,
        &[
            "log",
            "--topo-order",
            "--max-count=1025",
            "--format=%H%x1f%s%x1f%b%x1e",
            &range,
        ],
        true,
    )?;
    let commits = parse_commits(&log)?;
    let bump = if candidate_already_tagged {
        Bump::None
    } else {
        choose_bump(previous_version.as_ref(), &commits)
    };
    let next = match (&previous_version, bump, candidate_already_tagged) {
        (_, _, true) => parse_version(&workspace_version)?,
        (None, _, false) => parse_version(&workspace_version)?,
        (Some(version), Bump::Major, false) => SemanticVersion {
            major: version.major.checked_add(1).ok_or_else(version_overflow)?,
            minor: 0,
            patch: 0,
        },
        (Some(version), Bump::Minor, false) => SemanticVersion {
            major: version.major,
            minor: version.minor.checked_add(1).ok_or_else(version_overflow)?,
            patch: 0,
        },
        (Some(version), Bump::Patch, false) => SemanticVersion {
            major: version.major,
            minor: version.minor,
            patch: version.patch.checked_add(1).ok_or_else(version_overflow)?,
        },
        (Some(version), Bump::None | Bump::FirstRelease, false) => version.clone(),
    };
    let next_version = render_version(&next);
    let changelog = read_utf8_bounded(&root.join("CHANGELOG.md"), CHANGELOG_BYTES_MAX)?;
    let release_notes = render_release_notes(&next_version, &changelog, &commits);
    Ok(ReleasePlan {
        schema_version: CONTRACT_VERSION,
        candidate_commit,
        candidate_clean,
        previous_tag,
        previous_version: previous_version.as_ref().map(render_version),
        workspace_version,
        next_version,
        bump,
        commit_count: commits.len(),
        commits,
        release_notes,
    })
}

fn prepare(root: &Path, write: bool) -> Result<(), Box<dyn Error>> {
    let plan = plan(root)?;
    if plan.bump == Bump::None && plan.workspace_version == plan.next_version {
        return Err(input_error(
            "no releasing Conventional Commit exists since the latest tag",
        ));
    }
    let manifest_path = root.join("Cargo.toml");
    let manifest = read_utf8_bounded(&manifest_path, JSON_BYTES_MAX)?;
    let next_manifest = replace_workspace_version(&manifest, &plan.next_version)?;
    let lock_path = root.join("Cargo.lock");
    let lock = read_utf8_bounded(&lock_path, JSON_BYTES_MAX)?;
    let next_lock =
        replace_lock_workspace_versions(&lock, &plan.workspace_version, &plan.next_version)?;
    let changelog_path = root.join("CHANGELOG.md");
    let changelog = read_utf8_bounded(&changelog_path, CHANGELOG_BYTES_MAX)?;
    let release_date = git(root, &["show", "-s", "--format=%cs", "HEAD"], true)?;
    validate_release_date(&release_date)?;
    let next_changelog = update_changelog(&changelog, &plan.next_version, &release_date)?;
    let notes_path = root.join(RELEASE_NOTES_PATH);
    let mut changed_paths = Vec::new();
    if next_manifest != manifest {
        changed_paths.push("Cargo.toml".to_owned());
    }
    if next_lock != lock {
        changed_paths.push("Cargo.lock".to_owned());
    }
    if next_changelog != changelog {
        changed_paths.push("CHANGELOG.md".to_owned());
    }
    changed_paths.push(RELEASE_NOTES_PATH.to_owned());
    if write {
        atomic_write_if_changed(&manifest_path, next_manifest.as_bytes())?;
        atomic_write_if_changed(&lock_path, next_lock.as_bytes())?;
        atomic_write_if_changed(&changelog_path, next_changelog.as_bytes())?;
        atomic_write_if_changed(&notes_path, plan.release_notes.as_bytes())?;
    }
    print_json(&PrepareResult {
        schema_version: CONTRACT_VERSION,
        write,
        next_version: plan.next_version,
        changed_paths,
        release_notes_path: RELEASE_NOTES_PATH.to_owned(),
    })
}

fn verify(
    root: &Path,
    expected_tag: Option<&str>,
    require_remote_tag_absent: bool,
    required_remote_branch: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let plan = plan(root)?;
    if !plan.candidate_clean {
        return Err(input_error("release candidate worktree is not clean"));
    }
    if plan.workspace_version != plan.next_version {
        return Err(input_error(format!(
            "workspace version {} does not match planned version {}; run `cargo xtask release prepare --write`, review, and commit it",
            plan.workspace_version, plan.next_version
        )));
    }
    if let Some(branch) = required_remote_branch {
        verify_remote_branch_candidate(root, branch, &plan.candidate_commit)?;
    }
    if let Some(expected_tag) = expected_tag {
        let planned_tag = format!("v{}", plan.next_version);
        if expected_tag != planned_tag {
            return Err(input_error(format!(
                "requested tag {expected_tag:?} does not match planned tag {planned_tag:?}"
            )));
        }
        if require_remote_tag_absent {
            verify_remote_tag_absent(root, expected_tag)?;
        }
    }
    let changelog = read_utf8_bounded(&root.join("CHANGELOG.md"), CHANGELOG_BYTES_MAX)?;
    let heading = format!("## [{}]", plan.next_version);
    if !changelog.lines().any(|line| line.starts_with(&heading)) {
        return Err(input_error(format!(
            "CHANGELOG.md has no release heading for {}",
            plan.next_version
        )));
    }
    let notes = read_utf8_bounded(&root.join(RELEASE_NOTES_PATH), CHANGELOG_BYTES_MAX)?;
    if notes != plan.release_notes {
        return Err(input_error(format!(
            "{RELEASE_NOTES_PATH} does not match deterministic notes for the candidate; run `cargo xtask release prepare --write`, review, and commit it"
        )));
    }
    print_json(&VerifyResult {
        schema_version: CONTRACT_VERSION,
        version: plan.next_version,
        candidate_commit: plan.candidate_commit,
        clean: true,
        release_notes_sha256: sha256_hex(plan.release_notes.as_bytes()),
    })
}

fn verify_remote_branch_candidate(
    root: &Path,
    branch: &str,
    candidate_commit: &str,
) -> Result<(), Box<dyn Error>> {
    validate_remote_branch(branch)?;
    let reference = format!("refs/heads/{branch}");
    let arguments = [
        OsStr::new("ls-remote"),
        OsStr::new("--exit-code"),
        OsStr::new("--heads"),
        OsStr::new("origin"),
        OsStr::new(&reference),
    ];
    let output = command_output_with_directory(
        Path::new("git"),
        &arguments,
        root,
        PROCESS_TIMEOUT,
        GIT_OUTPUT_BYTES_MAX,
    )?;
    ensure_output_success("origin release branch lookup", &output)?;
    parse_remote_branch(&output.stdout, branch, &reference, candidate_commit)
}

fn parse_remote_branch(
    output: &[u8],
    branch: &str,
    expected_reference: &str,
    candidate_commit: &str,
) -> Result<(), Box<dyn Error>> {
    let line = std::str::from_utf8(output)?;
    let (commit, found_reference) = line
        .trim()
        .split_once('\t')
        .ok_or_else(|| input_error("remote branch response has an invalid shape"))?;
    validate_commit(commit)?;
    if commit != candidate_commit || found_reference != expected_reference {
        return Err(input_error(format!(
            "release candidate {candidate_commit} is not synchronized with origin branch {branch} at {commit}"
        )));
    }
    Ok(())
}

fn validate_remote_branch(branch: &str) -> Result<(), Box<dyn Error>> {
    if branch.is_empty()
        || branch.len() > 255
        || branch.starts_with('/')
        || branch.ends_with('/')
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains("//")
        || branch
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err(input_error("required remote branch name is invalid"));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemoteTagState {
    Absent,
    ExactCandidate,
}

fn tag(root: &Path, expected_tag: &str, write: bool) -> Result<(), Box<dyn Error>> {
    let plan = plan(root)?;
    validate_release_candidate(root, &plan, expected_tag)?;
    let initial_state = remote_tag_state(root, expected_tag, &plan.candidate_commit)?;
    if write && initial_state == RemoteTagState::Absent {
        ensure_local_tag(root, expected_tag, &plan.candidate_commit)?;
        push_release_tag(root, expected_tag)?;
    }
    let final_state = remote_tag_state(root, expected_tag, &plan.candidate_commit)?;
    if write && final_state != RemoteTagState::ExactCandidate {
        return Err(input_error(
            "release tag was not installed at the exact candidate",
        ));
    }
    print_json(&TagResult {
        schema_version: CONTRACT_VERSION,
        write,
        tag: expected_tag.to_owned(),
        candidate_commit: plan.candidate_commit,
        remote_state: match final_state {
            RemoteTagState::Absent => "absent",
            RemoteTagState::ExactCandidate => "exact_candidate",
        }
        .to_owned(),
    })
}

fn validate_release_candidate(
    root: &Path,
    plan: &ReleasePlan,
    expected_tag: &str,
) -> Result<(), Box<dyn Error>> {
    if !plan.candidate_clean {
        return Err(input_error("release candidate worktree is not clean"));
    }
    if plan.workspace_version != plan.next_version {
        return Err(input_error(
            "workspace version does not match the release plan",
        ));
    }
    if expected_tag != format!("v{}", plan.next_version) {
        return Err(input_error(
            "requested tag does not match the planned version",
        ));
    }
    let changelog = read_utf8_bounded(&root.join("CHANGELOG.md"), CHANGELOG_BYTES_MAX)?;
    if !changelog
        .lines()
        .any(|line| line.starts_with(&format!("## [{}]", plan.next_version)))
    {
        return Err(input_error(
            "CHANGELOG.md does not contain the planned release heading",
        ));
    }
    let notes = read_utf8_bounded(&root.join(RELEASE_NOTES_PATH), CHANGELOG_BYTES_MAX)?;
    if notes != plan.release_notes {
        return Err(input_error(
            "tracked release notes do not match the candidate",
        ));
    }
    Ok(())
}

fn remote_tag_state(
    root: &Path,
    tag: &str,
    candidate_commit: &str,
) -> Result<RemoteTagState, Box<dyn Error>> {
    let reference = format!("refs/tags/{tag}");
    let arguments = [
        OsStr::new("ls-remote"),
        OsStr::new("--exit-code"),
        OsStr::new("--refs"),
        OsStr::new("origin"),
        OsStr::new(&reference),
    ];
    let output = command_output_with_directory(
        Path::new("git"),
        &arguments,
        root,
        PROCESS_TIMEOUT,
        GIT_OUTPUT_BYTES_MAX,
    )?;
    match output.status.code() {
        Some(2) if output.stdout.is_empty() => Ok(RemoteTagState::Absent),
        Some(0) => parse_remote_tag(&output.stdout, tag, &reference, candidate_commit),
        _ => Err(io::Error::other(format!(
            "could not determine remote tag {tag}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into()),
    }
}

fn parse_remote_tag(
    output: &[u8],
    tag: &str,
    expected_reference: &str,
    candidate_commit: &str,
) -> Result<RemoteTagState, Box<dyn Error>> {
    let line = std::str::from_utf8(output)?;
    let (commit, found_reference) = line
        .trim()
        .split_once('\t')
        .ok_or_else(|| input_error("remote tag response has an invalid shape"))?;
    if commit == candidate_commit && found_reference == expected_reference {
        Ok(RemoteTagState::ExactCandidate)
    } else {
        Err(input_error(format!(
            "remote tag {tag} already exists but does not point directly to candidate {candidate_commit}"
        )))
    }
}

fn ensure_local_tag(root: &Path, tag: &str, candidate_commit: &str) -> Result<(), Box<dyn Error>> {
    let reference = format!("refs/tags/{tag}");
    let existing = git(root, &["rev-parse", "--verify", &reference], false)?;
    if !existing.is_empty() {
        if existing == candidate_commit {
            return Ok(());
        }
        return Err(input_error(format!(
            "local tag {tag} already exists at a different object"
        )));
    }
    let arguments = [
        OsStr::new("tag"),
        OsStr::new("--no-sign"),
        OsStr::new(tag),
        OsStr::new(candidate_commit),
    ];
    let output = command_output_with_directory(
        Path::new("git"),
        &arguments,
        root,
        PROCESS_TIMEOUT,
        GIT_OUTPUT_BYTES_MAX,
    )?;
    ensure_output_success("lightweight release tag creation", &output)
}

fn push_release_tag(root: &Path, tag: &str) -> Result<(), Box<dyn Error>> {
    let reference = format!("refs/tags/{tag}");
    let refspec = format!("{reference}:{reference}");
    let arguments = [
        OsStr::new("push"),
        OsStr::new("--porcelain"),
        OsStr::new("origin"),
        OsStr::new(&refspec),
    ];
    let output = command_output_with_directory(
        Path::new("git"),
        &arguments,
        root,
        PROCESS_TIMEOUT,
        GIT_OUTPUT_BYTES_MAX,
    )?;
    ensure_output_success("immutable release tag push", &output)
}

fn verify_remote_tag_absent(root: &Path, tag: &str) -> Result<(), Box<dyn Error>> {
    let candidate = git(root, &["rev-parse", "HEAD"], true)?;
    match remote_tag_state(root, tag, &candidate)? {
        RemoteTagState::Absent => Ok(()),
        RemoteTagState::ExactCandidate => Err(input_error(format!(
            "remote tag {tag} already exists; use `release tag --write` for idempotent recovery"
        ))),
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "target-specific package matches are validated before selection"
)]
fn release_dependencies_for_target(
    root: &Path,
    target: &str,
) -> Result<Vec<ReleaseDependency>, Box<dyn Error>> {
    let metadata_arguments = [
        OsStr::new("metadata"),
        OsStr::new("--format-version"),
        OsStr::new("1"),
        OsStr::new("--locked"),
        OsStr::new("--filter-platform"),
        OsStr::new(target),
    ];
    let metadata_output = command_output_with_directory(
        Path::new("cargo"),
        &metadata_arguments,
        root,
        PROCESS_TIMEOUT,
        JSON_BYTES_MAX,
    )?;
    ensure_output_success(
        "cargo metadata for release dependency audit",
        &metadata_output,
    )?;
    let metadata: CargoMetadata = serde_json::from_slice(&metadata_output.stdout)?;
    if metadata.packages.len() > RELEASE_DEPENDENCIES_MAX * 2 {
        return Err(resource_error("Cargo metadata package count"));
    }

    let tree_arguments = [
        OsStr::new("tree"),
        OsStr::new("-p"),
        OsStr::new("quirl-cli"),
        OsStr::new("--locked"),
        OsStr::new("--target"),
        OsStr::new(target),
        OsStr::new("--edges"),
        OsStr::new("normal,build"),
        OsStr::new("--prefix"),
        OsStr::new("none"),
        OsStr::new("--format"),
        OsStr::new("{p}"),
    ];
    let tree_output = command_output_with_directory(
        Path::new("cargo"),
        &tree_arguments,
        root,
        PROCESS_TIMEOUT,
        JSON_BYTES_MAX,
    )?;
    ensure_output_success("cargo tree for release dependency audit", &tree_output)?;
    let tree = std::str::from_utf8(&tree_output.stdout)?;
    let mut package_keys = BTreeSet::new();
    for line in tree.lines() {
        package_keys.insert(parse_cargo_tree_package(line)?);
        if package_keys.len() > RELEASE_DEPENDENCIES_MAX {
            return Err(resource_error("release dependency count"));
        }
    }

    let mut packages_by_key = BTreeMap::<(String, String), Vec<CargoPackage>>::new();
    for package in metadata.packages {
        packages_by_key
            .entry((package.name.clone(), package.version.clone()))
            .or_default()
            .push(package);
    }
    let mut dependencies = Vec::new();
    for key in package_keys {
        let candidates = packages_by_key.get(&key).ok_or_else(|| {
            input_error(format!(
                "cargo tree package {} v{} is absent from Cargo metadata",
                key.0, key.1
            ))
        })?;
        if candidates.len() != 1 {
            return Err(input_error(format!(
                "cargo tree package {} v{} resolves to {} metadata packages; the release audit requires an unambiguous source",
                key.0,
                key.1,
                candidates.len()
            )));
        }
        let package = &candidates[0];
        let Some(source) = package.source.as_deref() else {
            if !package.manifest_path.starts_with(root) {
                return Err(input_error(format!(
                    "path dependency {} v{} is outside the Quirl workspace and has no auditable source identity",
                    package.name, package.version
                )));
            }
            continue;
        };
        let declared_license = declared_package_license(package)?;
        let package_root = package
            .manifest_path
            .parent()
            .ok_or_else(|| input_error("Cargo package manifest has no parent directory"))?
            .to_path_buf();
        dependencies.push(ReleaseDependency {
            contract: DependencyContract {
                name: package.name.clone(),
                version: package.version.clone(),
                declared_license,
                source: source.to_owned(),
                repository: package.repository.clone().unwrap_or_else(|| "-".to_owned()),
            },
            package_root,
        });
    }
    dependencies.sort_by(|left, right| left.contract.cmp(&right.contract));
    let platform = target_platform(target)?.1;
    validate_dependency_inventory(root, platform, &dependencies)?;
    Ok(dependencies)
}

fn parse_cargo_tree_package(line: &str) -> Result<(String, String), Box<dyn Error>> {
    let line = line.trim();
    let (name, remainder) = line
        .split_once(" v")
        .ok_or_else(|| input_error(format!("invalid cargo tree package line {line:?}")))?;
    let version = remainder
        .split_whitespace()
        .next()
        .ok_or_else(|| input_error(format!("cargo tree package has no version: {line:?}")))?;
    if name.is_empty() || version.is_empty() || name.contains(char::is_whitespace) {
        return Err(input_error(format!(
            "invalid cargo tree package identity {line:?}"
        )));
    }
    Ok((name.to_owned(), version.to_owned()))
}

fn declared_package_license(package: &CargoPackage) -> Result<String, Box<dyn Error>> {
    if let Some(license) = package.license.as_deref().map(str::trim)
        && !license.is_empty()
    {
        return Ok(license.to_owned());
    }
    if let Some(license_file) = package.license_file.as_deref().map(str::trim)
        && !license_file.is_empty()
    {
        return Ok(format!("LicenseRef-file:{license_file}"));
    }
    Err(input_error(format!(
        "release dependency {} v{} declares neither a license nor a license file",
        package.name, package.version
    )))
}

fn validate_dependency_inventory(
    root: &Path,
    platform: &str,
    dependencies: &[ReleaseDependency],
) -> Result<(), Box<dyn Error>> {
    let inventory = parse_dependency_inventory(&read_utf8_bounded(
        &root.join(THIRD_PARTY_INVENTORY_PATH),
        JSON_BYTES_MAX,
    )?)?;
    let expected = inventory
        .iter()
        .filter(|record| record.platforms.contains(platform))
        .map(|record| record.contract.clone())
        .collect::<BTreeSet<_>>();
    let observed = dependencies
        .iter()
        .map(|dependency| dependency.contract.clone())
        .collect::<BTreeSet<_>>();
    if expected == observed {
        return Ok(());
    }
    let missing = expected.difference(&observed).take(8).collect::<Vec<_>>();
    let unexpected = observed.difference(&expected).take(8).collect::<Vec<_>>();
    Err(input_error(format!(
        "{THIRD_PARTY_INVENTORY_PATH} does not match the locked {platform} quirl-cli normal/build closure; missing {missing:?}; unexpected {unexpected:?}"
    )))
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "inventory records validate field counts and the total input is resource-bounded"
)]
fn parse_dependency_inventory(source: &str) -> Result<Vec<InventoryRecord>, Box<dyn Error>> {
    let mut records = Vec::new();
    let mut previous = None::<DependencyContract>;
    for (index, line) in source.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != 6 || fields.iter().any(|field| field.is_empty()) {
            return Err(input_error(format!(
                "invalid third-party inventory record on line {}",
                index + 1
            )));
        }
        let platforms = fields[2]
            .split(',')
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        if platforms.is_empty()
            || platforms
                .iter()
                .any(|platform| platform != "linux" && platform != "macos")
        {
            return Err(input_error(format!(
                "invalid third-party inventory platform on line {}",
                index + 1
            )));
        }
        let contract = DependencyContract {
            name: fields[0].to_owned(),
            version: fields[1].to_owned(),
            declared_license: fields[3].to_owned(),
            source: fields[4].to_owned(),
            repository: fields[5].to_owned(),
        };
        if previous.as_ref().is_some_and(|prior| prior >= &contract) {
            return Err(input_error(format!(
                "third-party inventory must be strictly sorted; line {} is out of order or duplicated",
                index + 1
            )));
        }
        previous = Some(contract.clone());
        records.push(InventoryRecord {
            contract,
            platforms,
        });
        if records.len() > RELEASE_DEPENDENCIES_MAX {
            return Err(resource_error("third-party inventory record count"));
        }
    }
    if records.is_empty() {
        return Err(input_error("third-party inventory is empty"));
    }
    Ok(records)
}

fn render_third_party_license_report(
    root: &Path,
    target: &str,
    dependencies: &[ReleaseDependency],
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut documents = BTreeMap::<String, LicenseDocument>::new();
    let mut dependency_documents = Vec::new();
    let mut unique_document_bytes = 0_usize;
    for dependency in dependencies {
        let mut files = collect_dependency_license_files(&dependency.package_root)?;
        if files.is_empty() {
            files.push(read_license_fallback(root, &dependency.contract)?);
        }
        let mut references = BTreeSet::new();
        for PackageLicenseFile {
            relative_path,
            bytes,
        } in files
        {
            std::str::from_utf8(&bytes).map_err(|error| {
                input_error(format!(
                    "license document {} v{}:{relative_path} is not UTF-8: {error}",
                    dependency.contract.name, dependency.contract.version
                ))
            })?;
            let sha256 = sha256_hex(&bytes);
            let origin = format!(
                "{} v{}:{relative_path}",
                dependency.contract.name, dependency.contract.version
            );
            if let Some(document) = documents.get_mut(&sha256) {
                if document.bytes != bytes {
                    return Err(input_error(
                        "SHA-256 collision in dependency license documents",
                    ));
                }
                document.origins.insert(origin);
            } else {
                unique_document_bytes = unique_document_bytes
                    .checked_add(bytes.len())
                    .ok_or_else(|| resource_error("dependency license byte count"))?;
                if unique_document_bytes > LICENSE_REPORT_BYTES_MAX {
                    return Err(resource_error("dependency license byte count"));
                }
                documents.insert(
                    sha256.clone(),
                    LicenseDocument {
                        bytes,
                        origins: BTreeSet::from([origin]),
                    },
                );
                if documents.len() > LICENSE_DOCUMENTS_MAX {
                    return Err(resource_error("dependency license document count"));
                }
            }
            references.insert((relative_path, sha256));
        }
        dependency_documents.push((dependency.contract.clone(), references));
    }

    let mut report = Vec::new();
    report_extend(
        &mut report,
        format!(
            "Quirl third-party dependency licenses\n\nTarget: {target}\nScope: locked quirl-cli normal and build dependency closure\n\nDependency inventory\n====================\n"
        )
        .as_bytes(),
    )?;
    for (contract, references) in dependency_documents {
        report_extend(
            &mut report,
            format!(
                "\n{} v{}\n  declared license: {}\n  source: {}\n  repository: {}\n",
                contract.name,
                contract.version,
                contract.declared_license,
                contract.source,
                contract.repository
            )
            .as_bytes(),
        )?;
        for (path, sha256) in references {
            report_extend(
                &mut report,
                format!("  document: {path} (sha256 {sha256})\n").as_bytes(),
            )?;
        }
    }
    report_extend(&mut report, b"\nLicense documents\n=================\n")?;
    for (sha256, document) in documents {
        report_extend(
            &mut report,
            format!("\nSHA-256: {sha256}\nOrigins:\n").as_bytes(),
        )?;
        for origin in document.origins {
            report_extend(&mut report, format!("  - {origin}\n").as_bytes())?;
        }
        report_extend(&mut report, b"--- BEGIN EXACT DOCUMENT ---\n")?;
        report_extend(&mut report, &document.bytes)?;
        if !document.bytes.ends_with(b"\n") {
            report_extend(&mut report, b"\n")?;
        }
        report_extend(&mut report, b"--- END EXACT DOCUMENT ---\n")?;
    }
    Ok(report)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "dependency and license file counts are bounded by release validation limits"
)]
fn collect_dependency_license_files(
    package_root: &Path,
) -> Result<Vec<PackageLicenseFile>, Box<dyn Error>> {
    let root_metadata = fs::symlink_metadata(package_root)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(input_error(format!(
            "dependency package root {} must be a non-symlink directory",
            package_root.display()
        )));
    }
    let mut stack = vec![(package_root.to_path_buf(), 0_usize)];
    let mut files = Vec::new();
    let mut entries_seen = 0_usize;
    while let Some((directory, depth)) = stack.pop() {
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries.into_iter().rev() {
            entries_seen = entries_seen
                .checked_add(1)
                .ok_or_else(|| resource_error("dependency package entry count"))?;
            if entries_seen > LICENSE_SCAN_ENTRIES_MAX {
                return Err(resource_error("dependency package entry count"));
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(input_error(format!(
                    "dependency package contains an unauditable symlink at {}",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                if depth >= LICENSE_SCAN_DEPTH_MAX {
                    return Err(resource_error("dependency package directory depth"));
                }
                stack.push((path, depth + 1));
                continue;
            }
            if !metadata.is_file() || !is_license_file_name(&entry.file_name()) {
                continue;
            }
            let relative = path
                .strip_prefix(package_root)?
                .to_string_lossy()
                .replace('\\', "/");
            files.push(PackageLicenseFile {
                relative_path: relative,
                bytes: read_bounded(&path, LICENSE_DOCUMENT_BYTES_MAX)?,
            });
            if files.len() > LICENSE_DOCUMENTS_MAX {
                return Err(resource_error("dependency package license file count"));
            }
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn is_license_file_name(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    ["license", "licence", "copying", "notice", "copyright"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn read_license_fallback(
    root: &Path,
    contract: &DependencyContract,
) -> Result<PackageLicenseFile, Box<dyn Error>> {
    let file_name = match (contract.name.as_str(), contract.version.as_str()) {
        ("keybindings", "0.0.2") => "keybindings-0.0.2-Apache-2.0.txt",
        ("mlua-sys", "0.11.0") => "mlua-sys-0.11.0-MIT.txt",
        ("number_prefix", "0.4.0") => "number_prefix-0.4.0-MIT.txt",
        _ => {
            return Err(input_error(format!(
                "release dependency {} v{} has no packaged license document and no reviewed fallback",
                contract.name, contract.version
            )));
        }
    };
    Ok(PackageLicenseFile {
        relative_path: format!("reviewed-fallback/{file_name}"),
        bytes: read_bounded(
            &root.join(THIRD_PARTY_LICENSE_FALLBACK_ROOT).join(file_name),
            LICENSE_DOCUMENT_BYTES_MAX,
        )?,
    })
}

fn report_extend(report: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let next_len = report
        .len()
        .checked_add(bytes.len())
        .ok_or_else(|| resource_error("third-party license report byte count"))?;
    if next_len > LICENSE_REPORT_BYTES_MAX {
        return Err(resource_error("third-party license report byte count"));
    }
    report.extend_from_slice(bytes);
    Ok(())
}

fn package(root: &Path, target: &str, output: &Path) -> Result<(), Box<dyn Error>> {
    validate_target(target)?;
    let plan = plan(root)?;
    if !plan.candidate_clean {
        return Err(input_error("refusing to package a dirty release candidate"));
    }
    if plan.workspace_version != plan.next_version {
        return Err(input_error(
            "workspace version does not match the release plan",
        ));
    }
    let binary = target_binary(root, target);
    let source_date_epoch = candidate_source_epoch(root)?;
    run_status_bounded(
        Command::new("cargo")
            .current_dir(root)
            .args([
                "build",
                "--locked",
                "--release",
                "--target",
                target,
                "-p",
                "quirl-cli",
            ])
            .env("SOURCE_DATE_EPOCH", source_date_epoch.to_string())
            .env("QUIRL_OFFICIAL_RELEASE", "1"),
        Duration::from_secs(30 * 60),
        "cargo release build",
    )?;
    let metadata = command_output(
        &binary,
        &[OsStr::new("--build-info")],
        PROCESS_TIMEOUT,
        JSON_BYTES_MAX,
    )?;
    ensure_output_success("quirl --build-info", &metadata)?;
    let build_info: BuildInfo = serde_json::from_slice(&metadata.stdout)?;
    validate_build_info(&build_info, &plan, target)?;
    let binary_bytes = read_bounded(&binary, ARTIFACT_BYTES_MAX)?;
    let product_license = read_bounded(&root.join(PRODUCT_LICENSE_PATH), ARTIFACT_BYTES_MAX)?;
    let third_party_notices =
        read_bounded(&root.join(THIRD_PARTY_NOTICES_PATH), ARTIFACT_BYTES_MAX)?;
    let release_dependencies = release_dependencies_for_target(root, target)?;
    let third_party_licenses =
        render_third_party_license_report(root, target, &release_dependencies)?;
    let extension = env::consts::EXE_SUFFIX;
    let archive_name = format!("quirl-v{}-{target}.tar", plan.next_version);
    let binary_name = format!("bin/quirl{extension}");
    let archive = render_tar_entries(
        &[
            (binary_name.as_str(), binary_bytes.as_slice(), 0o755),
            (PRODUCT_LICENSE_PATH, product_license.as_slice(), 0o644),
            (
                ARCHIVE_THIRD_PARTY_NOTICES_PATH,
                third_party_notices.as_slice(),
                0o644,
            ),
            (
                ARCHIVE_THIRD_PARTY_LICENSES_PATH,
                third_party_licenses.as_slice(),
                0o644,
            ),
        ],
        source_date_epoch,
    )?;
    let archive_sha256 = sha256_hex(&archive);
    let archive_bytes =
        u64::try_from(archive.len()).map_err(|_| resource_error("archive byte count"))?;
    let provenance_name = format!("{archive_name}.provenance.json");
    let provenance = PackageProvenance {
        schema_version: CONTRACT_VERSION,
        product: "quirl".to_owned(),
        version: plan.next_version,
        target: target.to_owned(),
        candidate_commit: plan.candidate_commit,
        source_date_epoch,
        artifact: archive_name.clone(),
        byte_size: archive_bytes,
        sha256: archive_sha256.clone(),
        build_profile: build_info.build_profile,
        optimization_level: build_info.optimization_level,
        panic_strategy: build_info.panic_strategy,
    };
    let output = absolute(root, output);
    fs::create_dir_all(&output)?;
    immutable_write(&output.join(&archive_name), &archive)?;
    immutable_write(&output.join(&provenance_name), &json_bytes(&provenance)?)?;
    print_json(&PackageResult {
        schema_version: CONTRACT_VERSION,
        artifact: archive_name,
        provenance: provenance_name,
        sha256: archive_sha256,
        byte_size: archive_bytes,
    })
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "archive sizes are checked against configured release limits before aggregation"
)]
fn aggregate(root: &Path, input: &Path, output: &Path) -> Result<(), Box<dyn Error>> {
    let plan = plan(root)?;
    validate_release_candidate(root, &plan, &format!("v{}", plan.next_version))?;
    let reviewed_release_notes = read_bounded(&root.join(RELEASE_NOTES_PATH), CHANGELOG_BYTES_MAX)?;
    let input = absolute(root, input);
    let output = absolute(root, output);
    if input == output || output.starts_with(&input) {
        return Err(input_error(
            "aggregate output must not be inside its input directory",
        ));
    }
    let files = collect_files(&input)?;
    let mut provenances = Vec::new();
    let mut asset_manifest = None;
    for path in &files {
        let name = file_name(path)?;
        if name.ends_with(".tar.provenance.json") {
            provenances.push(read_json_bounded::<PackageProvenance>(path)?);
        } else if name == "asset-manifest-v2.json" {
            if asset_manifest.is_some() {
                return Err(input_error(
                    "aggregate input contains multiple asset manifests",
                ));
            }
            asset_manifest = Some(read_json_bounded::<AssetManifest>(path)?);
        }
    }
    if provenances.len() != 4 {
        return Err(input_error(
            "aggregate input must contain exactly four native package provenances",
        ));
    }
    provenances.sort_by(|left, right| left.target.cmp(&right.target));
    let mut targets = BTreeSet::new();
    let mut artifacts = Vec::new();
    let mut selected = BTreeMap::<String, PathBuf>::new();
    for provenance in &provenances {
        validate_provenance(provenance, &plan)?;
        if !targets.insert(provenance.target.clone()) {
            return Err(input_error(format!(
                "duplicate package target {}",
                provenance.target
            )));
        }
        let archive_path = unique_file_named(&files, &provenance.artifact)?;
        let archive_bytes = read_bounded(archive_path, ARTIFACT_BYTES_MAX)?;
        if sha256_hex(&archive_bytes) != provenance.sha256
            || u64::try_from(archive_bytes.len()).ok() != Some(provenance.byte_size)
        {
            return Err(input_error(format!(
                "package {} differs from its provenance",
                provenance.artifact
            )));
        }
        let provenance_name = format!("{}.provenance.json", provenance.artifact);
        let provenance_path = unique_file_named(&files, &provenance_name)?;
        selected.insert(provenance.artifact.clone(), archive_path.to_path_buf());
        selected.insert(provenance_name.clone(), provenance_path.to_path_buf());
        artifacts.push(ReleaseArtifact {
            logical_name: "quirl".to_owned(),
            target: provenance.target.clone(),
            file: provenance.artifact.clone(),
            byte_size: provenance.byte_size,
            sha256: provenance.sha256.clone(),
            url: release_url(&plan.next_version, &provenance.artifact),
            provenance: provenance_name,
        });
    }
    let expected_targets = BTreeSet::from([
        "aarch64-apple-darwin".to_owned(),
        "aarch64-unknown-linux-gnu".to_owned(),
        "x86_64-apple-darwin".to_owned(),
        "x86_64-unknown-linux-gnu".to_owned(),
    ]);
    if targets != expected_targets {
        return Err(input_error(
            "aggregate input does not cover all four supported native targets",
        ));
    }
    let asset_manifest = asset_manifest.ok_or_else(|| {
        input_error("aggregate input must contain exactly one asset-manifest-v2.json")
    })?;
    asset_manifest.validate_for_release(&plan.next_version)?;
    let source_date_epoch = candidate_source_epoch(root)?;
    for asset in &asset_manifest.assets {
        if asset.source_revision != plan.candidate_commit
            || asset.source_date_epoch != source_date_epoch
        {
            return Err(input_error(format!(
                "asset {} source identity does not match aggregate HEAD",
                asset.logical_name
            )));
        }
    }
    let manifest_path = unique_file_named(&files, "asset-manifest-v2.json")?;
    selected.insert(
        "asset-manifest-v2.json".to_owned(),
        manifest_path.to_path_buf(),
    );
    for asset in &asset_manifest.assets {
        let path = unique_file_named(&files, &asset.file)?;
        let bytes = read_bounded(path, ARTIFACT_BYTES_MAX)?;
        if sha256_hex(&bytes) != asset.sha256
            || u64::try_from(bytes.len()).ok() != Some(asset.byte_size)
        {
            return Err(input_error(format!(
                "asset {} differs from its manifest",
                asset.file
            )));
        }
        selected.insert(asset.file.clone(), path.to_path_buf());
        for notice in &asset.notices {
            let notice_path = unique_file_named(&files, &notice.file)?;
            let notice_bytes = read_bounded(notice_path, ARTIFACT_BYTES_MAX)?;
            if sha256_hex(&notice_bytes) != notice.sha256
                || u64::try_from(notice_bytes.len()).ok() != Some(notice.byte_size)
                || notice_bytes != notice.text.as_bytes()
            {
                return Err(input_error(format!(
                    "asset notice {} differs from its manifest",
                    notice.file
                )));
            }
            selected.insert(notice.file.clone(), notice_path.to_path_buf());
        }
    }
    let manifest = ReleaseManifest {
        schema_version: CONTRACT_VERSION,
        product: "quirl".to_owned(),
        version: plan.next_version.clone(),
        tag: format!("v{}", plan.next_version),
        candidate_commit: plan.candidate_commit,
        source_date_epoch: candidate_source_epoch(root)?,
        artifacts,
        asset_manifest: Some("asset-manifest-v2.json".to_owned()),
        assets: asset_manifest.assets,
    };
    fs::create_dir_all(&output)?;
    for (name, source) in &selected {
        immutable_write(
            &output.join(name),
            &read_bounded(source, ARTIFACT_BYTES_MAX)?,
        )?;
    }
    let manifest_name = "release-manifest-v1.json";
    immutable_write(&output.join(manifest_name), &json_bytes(&manifest)?)?;
    let notes_name = "release-notes.md";
    immutable_write(&output.join(notes_name), &reviewed_release_notes)?;
    let mut checksum_entries = Vec::new();
    for name in selected
        .keys()
        .map(String::as_str)
        .chain([manifest_name, notes_name])
    {
        let bytes = read_bounded(&output.join(name), ARTIFACT_BYTES_MAX)?;
        checksum_entries.push((name.to_owned(), sha256_hex(&bytes)));
    }
    checksum_entries.sort();
    let mut checksums = String::new();
    for (name, digest) in &checksum_entries {
        checksums.push_str(digest);
        checksums.push_str("  ");
        checksums.push_str(name);
        checksums.push('\n');
    }
    immutable_write(&output.join("SHA256SUMS"), checksums.as_bytes())?;
    print_json(&AggregateResult {
        schema_version: CONTRACT_VERSION,
        release_manifest: manifest_name.to_owned(),
        checksums: "SHA256SUMS".to_owned(),
        release_notes: notes_name.to_owned(),
        upload_file_count: checksum_entries.len() + 1,
    })
}

fn parse_commits(log: &str) -> Result<Vec<ReleaseCommit>, Box<dyn Error>> {
    let mut commits = Vec::new();
    for record in log
        .split('\u{1e}')
        .filter(|record| !record.trim().is_empty())
    {
        if commits.len() == COMMITS_MAX {
            return Err(resource_error("release commit count"));
        }
        let mut fields = record.trim().splitn(3, '\u{1f}');
        let commit = fields.next().unwrap_or_default().trim();
        let subject = fields.next().unwrap_or_default().trim();
        let body = fields.next().unwrap_or_default();
        validate_commit(commit)?;
        if subject.is_empty() || subject.len() > 512 || body.len() > 64 * 1024 {
            return Err(resource_error("Conventional Commit message bytes"));
        }
        let prefix = subject.split(':').next().unwrap_or_default();
        let kind = prefix.split(['(', '!']).next().unwrap_or_default();
        let breaking = prefix.ends_with('!')
            || body.lines().any(|line| {
                line.starts_with("BREAKING CHANGE:") || line.starts_with("BREAKING-CHANGE:")
            });
        let is_release_preparation = subject == RELEASE_PREPARATION_COMMIT
            || (subject.starts_with("Merge pull request ")
                && body.lines().any(|line| line == RELEASE_PREPARATION_COMMIT));
        let category = match kind {
            _ if is_release_preparation => "release_preparation",
            "feat" => "features",
            "fix" => "fixes",
            "perf" => "performance",
            _ if breaking => "breaking",
            _ => "other",
        };
        commits.push(ReleaseCommit {
            commit: commit.to_owned(),
            subject: subject.to_owned(),
            category: category.to_owned(),
            breaking,
            releasing: !is_release_preparation
                && (breaking || matches!(kind, "feat" | "fix" | "perf")),
        });
    }
    Ok(commits)
}

fn choose_bump(previous: Option<&SemanticVersion>, commits: &[ReleaseCommit]) -> Bump {
    let Some(previous) = previous else {
        return Bump::FirstRelease;
    };
    if commits.iter().any(|commit| commit.breaking) {
        return if previous.major == 0 {
            Bump::Minor
        } else {
            Bump::Major
        };
    }
    if commits.iter().any(|commit| commit.category == "features") {
        Bump::Minor
    } else if commits
        .iter()
        .any(|commit| matches!(commit.category.as_str(), "fixes" | "performance"))
    {
        Bump::Patch
    } else {
        Bump::None
    }
}

fn render_release_notes(version: &str, changelog: &str, commits: &[ReleaseCommit]) -> String {
    let version_heading = format!("## [{version}]");
    let curated = changelog_section(changelog, &version_heading)
        .or_else(|| changelog_section(changelog, "## [Unreleased]"));
    if let Some(curated) = curated.filter(|section| !section.is_empty()) {
        return format!("# Quirl {version}\n\n{curated}\n");
    }
    render_commit_release_notes(version, commits)
}

fn changelog_section(source: &str, heading: &str) -> Option<String> {
    let mut in_section = false;
    let mut lines = Vec::new();
    for line in source.lines() {
        if !in_section {
            if line == heading || line.starts_with(&format!("{heading} - ")) {
                in_section = true;
            }
            continue;
        }
        if line.starts_with("## ") {
            break;
        }
        lines.push(line);
    }
    in_section.then(|| lines.join("\n").trim().to_owned())
}

#[allow(
    clippy::string_slice,
    reason = "the conventional-commit prefix ends at an ASCII delimiter returned by find"
)]
fn render_commit_release_notes(version: &str, commits: &[ReleaseCommit]) -> String {
    let mut output = format!("# Quirl {version}\n");
    for (category, heading) in [
        ("breaking", "Breaking changes"),
        ("features", "Features"),
        ("fixes", "Fixes"),
        ("performance", "Performance"),
        ("other", "Other changes"),
    ] {
        let matching = commits
            .iter()
            .filter(|commit| commit.category == category)
            .collect::<Vec<_>>();
        if matching.is_empty() {
            continue;
        }
        output.push_str("\n## ");
        output.push_str(heading);
        output.push('\n');
        for commit in matching {
            output.push_str("\n- ");
            output.push_str(&sanitize_markdown(&commit.subject));
            output.push_str(" (`");
            output.push_str(&commit.commit[..12]);
            output.push_str("`)\n");
        }
    }
    output
}

fn sanitize_markdown(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            '`' => '\'',
            character if character.is_control() => '\u{fffd}',
            character => character,
        })
        .collect()
}

#[allow(
    clippy::string_slice,
    clippy::arithmetic_side_effects,
    reason = "section offsets come from ASCII heading searches in the bounded changelog"
)]
fn update_changelog(source: &str, version: &str, date: &str) -> Result<String, Box<dyn Error>> {
    let release_heading = format!("## [{version}]");
    if source
        .lines()
        .any(|line| line.starts_with(&release_heading))
    {
        return Ok(source.to_owned());
    }
    let marker = "## [Unreleased]";
    let offset = source
        .find(marker)
        .ok_or_else(|| input_error("CHANGELOG.md has no [Unreleased] heading"))?;
    let insertion = offset + marker.len();
    let mut output = String::with_capacity(source.len() + version.len() + date.len() + 16);
    output.push_str(&source[..insertion]);
    output.push_str("\n\n");
    output.push_str(&format!("## [{version}] - {date}"));
    output.push_str(&source[insertion..]);
    Ok(output)
}

fn replace_workspace_version(source: &str, version: &str) -> Result<String, Box<dyn Error>> {
    let mut in_workspace_package = false;
    let mut replaced = false;
    let mut output = String::with_capacity(source.len());
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
        }
        if in_workspace_package && trimmed.starts_with("version = ") {
            if replaced {
                return Err(input_error(
                    "workspace package contains multiple version keys",
                ));
            }
            output.push_str(&format!("version = \"{version}\""));
            if line.ends_with('\n') {
                output.push('\n');
            }
            replaced = true;
        } else {
            output.push_str(line);
        }
    }
    if !replaced {
        return Err(input_error("Cargo.toml has no workspace.package version"));
    }
    Ok(output)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "lockfile line counts are bounded by the checked input size"
)]
fn replace_lock_workspace_versions(
    source: &str,
    current_version: &str,
    next_version: &str,
) -> Result<String, Box<dyn Error>> {
    if current_version == next_version {
        return Ok(source.to_owned());
    }
    let current_line = format!("version = \"{current_version}\"");
    let next_line = format!("version = \"{next_version}\"");
    let mut output = String::with_capacity(source.len());
    let mut replaced = 0usize;
    for block in source.split_inclusive("\n\n") {
        let is_local_package = block.trim_start().starts_with("[[package]]")
            && !block.lines().any(|line| line.starts_with("source = "));
        if is_local_package && block.lines().any(|line| line == current_line) {
            if replaced == 128 {
                return Err(resource_error("workspace package count in Cargo.lock"));
            }
            output.push_str(&block.replacen(&current_line, &next_line, 1));
            replaced += 1;
        } else {
            output.push_str(block);
        }
    }
    if replaced == 0 {
        return Err(input_error(format!(
            "Cargo.lock has no local workspace packages at version {current_version}"
        )));
    }
    Ok(output)
}

pub(crate) fn workspace_version(root: &Path) -> Result<String, Box<dyn Error>> {
    let manifest = read_utf8_bounded(&root.join("Cargo.toml"), JSON_BYTES_MAX)?;
    let mut in_workspace_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace_package = trimmed == "[workspace.package]";
        } else if in_workspace_package && let Some(value) = trimmed.strip_prefix("version = ") {
            let value = value.trim_matches('"');
            parse_version(value)?;
            return Ok(value.to_owned());
        }
    }
    Err(input_error("Cargo.toml has no workspace.package version"))
}

#[allow(
    clippy::indexing_slicing,
    reason = "semantic versions are validated as exactly three numeric components before access"
)]
fn parse_version(value: &str) -> Result<SemanticVersion, Box<dyn Error>> {
    if value.is_empty() || value.len() > 64 || value.contains(['-', '+']) {
        return Err(input_error(format!(
            "unsupported release version {value:?}"
        )));
    }
    let fields = value.split('.').collect::<Vec<_>>();
    if fields.len() != 3
        || fields.iter().any(|field| {
            field.is_empty()
                || (field.len() > 1 && field.starts_with('0'))
                || !field.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(input_error(format!(
            "invalid SemVer release version {value:?}"
        )));
    }
    Ok(SemanticVersion {
        major: fields[0].parse()?,
        minor: fields[1].parse()?,
        patch: fields[2].parse()?,
    })
}

fn render_version(version: &SemanticVersion) -> String {
    format!("{}.{}.{}", version.major, version.minor, version.patch)
}

fn validate_build_info(
    info: &BuildInfo,
    plan: &ReleasePlan,
    target: &str,
) -> Result<(), Box<dyn Error>> {
    if info.schema_version != 3
        || info.version != plan.next_version
        || info.source_commit != plan.candidate_commit
        || info.source_dirty != Some(false)
        || !info.official_release
        || info.build_profile != "release"
    {
        return Err(input_error(format!(
            "binary build identity does not match clean official candidate {} v{}",
            plan.candidate_commit, plan.next_version
        )));
    }
    let expected = target_platform(target)?;
    if info.architecture != expected.0 || info.operating_system != expected.1 {
        return Err(input_error(format!(
            "binary reports {}/{} but target {target} requires {}/{}",
            info.architecture, info.operating_system, expected.0, expected.1
        )));
    }
    if info.build_timestamp.parse::<u64>().is_err() {
        return Err(input_error(
            "binary build timestamp is not an unsigned integer",
        ));
    }
    Ok(())
}

fn validate_provenance(
    provenance: &PackageProvenance,
    plan: &ReleasePlan,
) -> Result<(), Box<dyn Error>> {
    if provenance.schema_version != CONTRACT_VERSION
        || provenance.product != "quirl"
        || provenance.version != plan.next_version
        || provenance.candidate_commit != plan.candidate_commit
        || provenance.artifact.contains(['/', '\\'])
        || provenance.sha256.len() != 64
    {
        return Err(input_error(format!(
            "package provenance for {} does not match the release candidate",
            provenance.target
        )));
    }
    validate_target(&provenance.target)
}

fn target_platform(target: &str) -> Result<(&'static str, &'static str), Box<dyn Error>> {
    match target {
        "aarch64-apple-darwin" => Ok(("aarch64", "macos")),
        "x86_64-apple-darwin" => Ok(("x86_64", "macos")),
        "aarch64-unknown-linux-gnu" => Ok(("aarch64", "linux")),
        "x86_64-unknown-linux-gnu" => Ok(("x86_64", "linux")),
        _ => Err(input_error(format!("unsupported release target {target}"))),
    }
}

fn validate_target(target: &str) -> Result<(), Box<dyn Error>> {
    target_platform(target).map(|_| ())
}

fn target_binary(root: &Path, target: &str) -> PathBuf {
    let target_root = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| absolute(root, &path))
        .unwrap_or_else(|| root.join("target"));
    target_root
        .join(target)
        .join("release")
        .join(format!("quirl{}", env::consts::EXE_SUFFIX))
}

#[allow(
    clippy::indexing_slicing,
    reason = "tar payload slices are bounded by each validated entry length"
)]
pub(crate) fn render_tar_entries(
    entries: &[(&str, &[u8], u64)],
    modified: u64,
) -> Result<Vec<u8>, Box<dyn Error>> {
    if entries.is_empty() || entries.len() > 32 {
        return Err(resource_error("tar entry count"));
    }
    let mut output = Vec::new();
    for (name, bytes, mode) in entries {
        if name.len() > 100 || bytes.len() > ARTIFACT_BYTES_MAX {
            return Err(resource_error("tar entry name or byte count"));
        }
        let padded = bytes
            .len()
            .checked_add(511)
            .and_then(|value| value.checked_div(512))
            .and_then(|value| value.checked_mul(512))
            .ok_or_else(|| resource_error("tar padded byte count"))?;
        let next_len = output
            .len()
            .checked_add(512)
            .and_then(|value| value.checked_add(padded))
            .ok_or_else(|| resource_error("tar byte count"))?;
        if next_len > ARTIFACT_BYTES_MAX {
            return Err(resource_error("tar byte count"));
        }
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        write_tar_octal(&mut header[100..108], *mode)?;
        write_tar_octal(&mut header[108..116], 0)?;
        write_tar_octal(&mut header[116..124], 0)?;
        write_tar_octal(&mut header[124..136], u64::try_from(bytes.len())?)?;
        write_tar_octal(&mut header[136..148], modified)?;
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        header[265..270].copy_from_slice(b"root\0");
        header[297..302].copy_from_slice(b"root\0");
        let checksum = header.iter().map(|byte| u64::from(*byte)).sum();
        write_tar_checksum(&mut header[148..156], checksum)?;
        output.extend_from_slice(&header);
        output.extend_from_slice(bytes);
        output.resize(next_len, 0);
    }
    output.resize(
        output
            .len()
            .checked_add(1_024)
            .ok_or_else(|| resource_error("tar byte count"))?,
        0,
    );
    Ok(output)
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "tar field length is validated before fixed trailer writes and octal width arithmetic"
)]
fn write_tar_octal(field: &mut [u8], value: u64) -> Result<(), Box<dyn Error>> {
    let digits = format!("{:0width$o}", value, width = field.len() - 1);
    if digits.len() + 1 != field.len() {
        return Err(resource_error("tar numeric field"));
    }
    field[..digits.len()].copy_from_slice(digits.as_bytes());
    field[field.len() - 1] = 0;
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    reason = "checksum field length is validated before fixed-format byte writes"
)]
fn write_tar_checksum(field: &mut [u8], value: u64) -> Result<(), Box<dyn Error>> {
    let digits = format!("{value:06o}");
    if digits.len() != 6 || field.len() != 8 {
        return Err(resource_error("tar checksum field"));
    }
    field[..6].copy_from_slice(digits.as_bytes());
    field[6] = 0;
    field[7] = b' ';
    Ok(())
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "visited file counts are bounded by the release asset limit"
)]
fn collect_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(input_error(
            "aggregate input must be a non-symlink directory",
        ));
    }
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut files = Vec::new();
    while let Some((directory, depth)) = pending.pop() {
        if depth > AGGREGATE_DEPTH_MAX {
            return Err(resource_error("aggregate directory depth"));
        }
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        if entries.len() > AGGREGATE_FILES_MAX {
            return Err(resource_error("aggregate directory entry count"));
        }
        for entry in entries {
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(input_error(format!(
                    "aggregate input contains symlink {}",
                    entry.path().display()
                )));
            }
            if file_type.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else if file_type.is_file() {
                files.push(entry.path());
                if files.len() > AGGREGATE_FILES_MAX {
                    return Err(resource_error("aggregate file count"));
                }
            } else {
                return Err(input_error(format!(
                    "aggregate input contains unsupported file {}",
                    entry.path().display()
                )));
            }
        }
    }
    Ok(files)
}

#[allow(
    clippy::indexing_slicing,
    reason = "the candidate count is validated as exactly one before access"
)]
fn unique_file_named<'a>(files: &'a [PathBuf], name: &str) -> Result<&'a Path, Box<dyn Error>> {
    let matching = files
        .iter()
        .filter(|path| path.file_name() == Some(OsStr::new(name)))
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(input_error(format!(
            "expected exactly one aggregate input named {name}"
        )));
    }
    Ok(matching[0])
}

pub(crate) fn release_url(version: &str, file: &str) -> String {
    format!("https://github.com/niklas-heer/quirl/releases/download/v{version}/{file}")
}

pub(crate) fn candidate_source_epoch(root: &Path) -> Result<u64, Box<dyn Error>> {
    git(root, &["show", "-s", "--format=%ct", "HEAD"], true)?
        .parse::<u64>()
        .map_err(|_| input_error("candidate commit timestamp is not an unsigned integer"))
}

pub(crate) fn clean_candidate_identity(root: &Path) -> Result<CandidateIdentity, Box<dyn Error>> {
    let plan = plan(root)?;
    let tag = format!("v{}", plan.next_version);
    validate_release_candidate(root, &plan, &tag)?;
    Ok(CandidateIdentity {
        version: plan.next_version,
        commit: plan.candidate_commit,
        source_date_epoch: candidate_source_epoch(root)?,
    })
}

/// Identity for asset builds that aren't cutting a release — just refreshing
/// downloadable content (the completion database, the command model) against
/// whatever version is *currently* shipped. Unlike
/// [`clean_candidate_identity`], this never requires a matching CHANGELOG
/// heading, release notes, or tag: those only make sense when preparing an
/// actual version bump. It still requires a clean worktree, since publishing
/// an asset built from uncommitted changes would misattribute its identity.
pub(crate) fn current_candidate_identity(root: &Path) -> Result<CandidateIdentity, Box<dyn Error>> {
    let version = workspace_version(root)?;
    let commit = current_commit(root)?;
    let clean = git(
        root,
        &["status", "--porcelain", "--untracked-files=normal"],
        true,
    )?
    .is_empty();
    if !clean {
        return Err(input_error("candidate worktree is not clean"));
    }
    Ok(CandidateIdentity {
        version,
        commit,
        source_date_epoch: candidate_source_epoch(root)?,
    })
}

pub(crate) fn current_commit(root: &Path) -> Result<String, Box<dyn Error>> {
    let commit = git(root, &["rev-parse", "HEAD"], true)?;
    validate_commit(&commit)?;
    Ok(commit)
}

pub(crate) fn local_tag_target(root: &Path, tag: &str) -> Result<String, Box<dyn Error>> {
    let reference = format!("refs/tags/{tag}");
    let commit = git(root, &["rev-parse", "--verify", &reference], true)?;
    validate_commit(&commit)?;
    Ok(commit)
}

pub(crate) fn verify_binary_version(
    binary: &Path,
    expected_version: &str,
) -> Result<(), Box<dyn Error>> {
    let output = command_output(
        binary,
        &[OsStr::new("--version")],
        PROCESS_TIMEOUT,
        4 * 1024,
    )?;
    ensure_output_success("offline packaged quirl --version", &output)?;
    let expected = format!("quirl {expected_version}\n");
    if output.stdout != expected.as_bytes() {
        return Err(input_error(
            "offline packaged binary reported an unexpected version",
        ));
    }
    Ok(())
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "poll counts are bounded by the subprocess deadline"
)]
pub(crate) fn run_status_bounded(
    command: &mut Command,
    timeout: Duration,
    label: &str,
) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return ensure_success(label, status);
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{label} exceeded {} seconds", timeout.as_secs()),
            )
            .into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn git(root: &Path, arguments: &[&str], require_success: bool) -> Result<String, Box<dyn Error>> {
    let argument_os = arguments.iter().map(OsStr::new).collect::<Vec<_>>();
    let output = command_output_with_directory(
        Path::new("git"),
        &argument_os,
        root,
        PROCESS_TIMEOUT,
        GIT_OUTPUT_BYTES_MAX,
    )?;
    if require_success {
        ensure_output_success("git", &output)?;
    } else if !output.status.success() {
        return Ok(String::new());
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(Into::into)
}

struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn command_output(
    program: &Path,
    arguments: &[&OsStr],
    timeout: Duration,
    bytes_max: usize,
) -> Result<BoundedOutput, Box<dyn Error>> {
    let directory = program.parent().unwrap_or_else(|| Path::new("."));
    command_output_with_directory(program, arguments, directory, timeout, bytes_max)
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "poll counts are bounded by the subprocess deadline"
)]
fn command_output_with_directory(
    program: &Path,
    arguments: &[&OsStr],
    directory: &Path,
    timeout: Duration,
    bytes_max: usize,
) -> Result<BoundedOutput, Box<dyn Error>> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("child stderr pipe missing"))?;
    let stdout_reader = thread::spawn(move || read_stream_bounded(stdout, bytes_max));
    let stderr_reader = thread::spawn(move || read_stream_bounded(stderr, bytes_max));
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if Instant::now() >= deadline {
            terminate_process_group(&mut child);
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))??;
    if timed_out {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{} exceeded {} seconds",
                program.display(),
                timeout.as_secs()
            ),
        )
        .into());
    }
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn terminate_process_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(process_group) = i32::try_from(child.id()) {
        let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
    }
    let _ = child.kill();
}

#[allow(
    clippy::indexing_slicing,
    reason = "the read size is clamped to the fixed buffer length before slicing"
)]
fn read_stream_bounded(mut stream: impl Read, bytes_max: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > bytes_max {
            return Err(io::Error::other(format!(
                "process output exceeded {bytes_max} bytes"
            )));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn ensure_success(label: &str, status: ExitStatus) -> Result<(), Box<dyn Error>> {
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{label} failed with {status}")).into())
    }
}

fn ensure_output_success(label: &str, output: &BoundedOutput) -> Result<(), Box<dyn Error>> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(io::Error::other(format!(
        "{label} failed with {}: {}",
        output.status,
        stderr.trim()
    ))
    .into())
}

pub(crate) fn read_json_bounded<T: DeserializeOwned>(path: &Path) -> Result<T, Box<dyn Error>> {
    serde_json::from_slice(&read_bounded(path, JSON_BYTES_MAX)?).map_err(Into::into)
}

pub(crate) fn read_bounded(path: &Path, bytes_max: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(input_error(format!(
            "{} must be a non-symlink regular file",
            path.display()
        )));
    }
    if metadata.len() > u64::try_from(bytes_max)? {
        return Err(resource_error("input file byte count"));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len())?);
    File::open(path)?
        .take(u64::try_from(bytes_max)?.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > bytes_max {
        return Err(resource_error("input file byte count"));
    }
    Ok(bytes)
}

fn read_utf8_bounded(path: &Path, bytes_max: usize) -> Result<String, Box<dyn Error>> {
    String::from_utf8(read_bounded(path, bytes_max)?).map_err(Into::into)
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

pub(crate) fn json_bytes(value: &impl Serialize) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    if bytes.len() > JSON_BYTES_MAX {
        return Err(resource_error("JSON contract byte count"));
    }
    Ok(bytes)
}

fn print_json(value: &impl Serialize) -> Result<(), Box<dyn Error>> {
    io::stdout().write_all(&json_bytes(value)?)?;
    Ok(())
}

pub(crate) fn immutable_write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        if read_bounded(path, bytes.len().max(JSON_BYTES_MAX))? == bytes {
            return Ok(());
        }
        return Err(input_error(format!(
            "refusing to replace different published bytes at {}",
            path.display()
        )));
    }
    atomic_write(path, bytes)
}

fn atomic_write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    if path.exists() && read_bounded(path, bytes.len().max(JSON_BYTES_MAX))? == bytes {
        return Ok(());
    }
    atomic_write(path, bytes)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn Error>> {
    let parent = path
        .parent()
        .ok_or_else(|| input_error("output path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = None;
    for _ in 0..TEMPORARY_ATTEMPTS_MAX {
        let suffix = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".quirl-release-{}-{suffix}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let (temporary_path, mut file) =
        temporary.ok_or_else(|| resource_error("temporary file attempts"))?;
    let install = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary_path, path)?;
        File::open(parent)?.sync_all()
    })();
    if install.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    install.map_err(Into::into)
}

pub(crate) fn absolute(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

pub(crate) fn validate_relative_file_name(value: &str) -> Result<(), Box<dyn Error>> {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 255
        || path.components().count() != 1
        || !matches!(path.components().next(), Some(Component::Normal(_)))
    {
        return Err(input_error(format!("invalid artifact file name {value:?}")));
    }
    Ok(())
}

fn file_name(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| input_error(format!("artifact path is not UTF-8: {}", path.display())))
}

fn validate_commit(value: &str) -> Result<(), Box<dyn Error>> {
    if value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(input_error(
            "Git candidate identity must be a full 40-digit hexadecimal commit",
        ))
    }
}

fn validate_release_date(value: &str) -> Result<(), Box<dyn Error>> {
    if value.len() == 10
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 4 | 7) {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
    {
        Ok(())
    } else {
        Err(input_error("Git commit date must use YYYY-MM-DD"))
    }
}

fn version_overflow() -> Box<dyn Error> {
    resource_error("semantic version component")
}

pub(crate) fn input_error(message: impl Into<String>) -> Box<dyn Error> {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

pub(crate) fn resource_error(resource: &str) -> Box<dyn Error> {
    io::Error::other(format!(
        "{resource} exceeded its configured release-tooling limit"
    ))
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(subject: &str, breaking: bool) -> ReleaseCommit {
        ReleaseCommit {
            commit: "a".repeat(40),
            subject: subject.to_owned(),
            category: if subject.starts_with("feat") {
                "features"
            } else {
                "other"
            }
            .to_owned(),
            breaking,
            releasing: true,
        }
    }

    #[test]
    fn breaking_change_before_one_defaults_to_minor() {
        let version = parse_version("0.4.2").unwrap();
        assert_eq!(
            choose_bump(Some(&version), &[commit("feat!: break", true)]),
            Bump::Minor
        );
    }

    #[test]
    fn conventional_fix_and_perf_are_patch_changes() {
        let parsed = parse_commits(&format!(
            "{}\u{1f}fix(cli): repair\u{1f}\u{1e}{}\u{1f}perf: faster\u{1f}\u{1e}",
            "a".repeat(40),
            "b".repeat(40)
        ))
        .unwrap();
        assert!(parsed.iter().all(|entry| entry.releasing));
        assert_eq!(
            choose_bump(Some(&parse_version("1.2.3").unwrap()), &parsed),
            Bump::Patch
        );
    }

    #[test]
    fn release_history_fails_instead_of_truncating_at_the_commit_limit() {
        let mut log = String::new();
        for index in 0..=COMMITS_MAX {
            log.push_str(&format!(
                "{:040x}\u{1f}docs: bounded {index}\u{1f}\u{1e}",
                index
            ));
        }
        assert!(parse_commits(&log).is_err());
    }

    #[test]
    fn remote_tag_must_point_directly_to_the_exact_candidate() {
        let candidate = "a".repeat(40);
        let reference = "refs/tags/v0.1.0";
        let exact = format!("{candidate}\t{reference}\n");
        assert_eq!(
            parse_remote_tag(exact.as_bytes(), "v0.1.0", reference, &candidate).unwrap(),
            RemoteTagState::ExactCandidate
        );
        let different = format!("{}\t{reference}\n", "b".repeat(40));
        assert!(parse_remote_tag(different.as_bytes(), "v0.1.0", reference, &candidate).is_err());
    }

    #[test]
    fn remote_release_branch_must_point_to_the_exact_candidate() {
        let candidate = "a".repeat(40);
        let reference = "refs/heads/main";
        let exact = format!("{candidate}\t{reference}\n");
        assert!(parse_remote_branch(exact.as_bytes(), "main", reference, &candidate).is_ok());
        let different = format!("{}\t{reference}\n", "b".repeat(40));
        assert!(parse_remote_branch(different.as_bytes(), "main", reference, &candidate).is_err());
        assert!(validate_remote_branch("release/0.1").is_ok());
        assert!(validate_remote_branch("../main").is_err());
        assert!(validate_remote_branch("main;git push").is_err());
    }

    #[test]
    fn changelog_update_preserves_curated_unreleased_content() {
        let source = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Curated.\n";
        let updated = update_changelog(source, "0.1.0", "2026-08-18").unwrap();
        assert!(updated.contains("## [0.1.0] - 2026-08-18\n\n### Added"));
        assert_eq!(
            update_changelog(&updated, "0.1.0", "2026-08-18").unwrap(),
            updated
        );
    }

    #[test]
    fn release_notes_use_curated_changelog_before_and_after_preparation() {
        let source = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- A user-facing summary.\n";
        let prepared = update_changelog(source, "0.1.0", "2026-08-18").unwrap();
        let commits = vec![ReleaseCommit {
            commit: "a".repeat(40),
            subject: "feat: implementation detail".to_owned(),
            category: "features".to_owned(),
            breaking: false,
            releasing: true,
        }];
        let expected = "# Quirl 0.1.0\n\n### Added\n\n- A user-facing summary.\n";
        assert_eq!(render_release_notes("0.1.0", source, &commits), expected);
        assert_eq!(render_release_notes("0.1.0", &prepared, &commits), expected);
    }

    #[test]
    fn workspace_version_rewrite_does_not_touch_other_versions() {
        let source = "[workspace.package]\nversion = \"0.1.0\"\n\n[fixture]\nversion = \"9\"\n";
        let updated = replace_workspace_version(source, "0.2.0").unwrap();
        assert!(updated.contains("version = \"0.2.0\""));
        assert!(updated.contains("[fixture]\nversion = \"9\""));
    }

    #[test]
    fn lock_rewrite_changes_only_local_workspace_packages() {
        let source = "version = 4\n\n[[package]]\nname = \"quirl-cli\"\nversion = \"0.1.0\"\n\n[[package]]\nname = \"external\"\nversion = \"0.1.0\"\nsource = \"registry+https://example.invalid\"\n\n";
        let updated = replace_lock_workspace_versions(source, "0.1.0", "0.2.0").unwrap();
        assert!(updated.contains("name = \"quirl-cli\"\nversion = \"0.2.0\""));
        assert!(updated.contains("name = \"external\"\nversion = \"0.1.0\""));
    }

    #[test]
    fn release_preparation_commit_is_excluded_from_reviewed_notes() {
        let log = format!(
            "{}\u{1f}{RELEASE_PREPARATION_COMMIT}\u{1f}\u{1e}",
            "a".repeat(40)
        );
        let commits = parse_commits(&log).unwrap();
        assert_eq!(
            render_release_notes("0.2.0", "## [Unreleased]\n", &commits),
            "# Quirl 0.2.0\n"
        );
    }

    #[test]
    fn release_preparation_merge_commit_is_excluded_from_reviewed_notes() {
        let log = format!(
            "{}\u{1f}Merge pull request #42 from niklas-heer/codex/release-prepare\u{1f}{RELEASE_PREPARATION_COMMIT}\u{1e}",
            "c".repeat(40)
        );
        let commits = parse_commits(&log).unwrap();
        assert_eq!(commits[0].category, "release_preparation");
        assert_eq!(
            render_release_notes("0.2.0", "## [Unreleased]\n", &commits),
            "# Quirl 0.2.0\n"
        );
    }

    #[test]
    fn tar_bytes_are_reproducible_and_end_with_two_zero_blocks() {
        let entries = [("bin/quirl", b"binary".as_slice(), 0o755)];
        let first = render_tar_entries(&entries, 123).unwrap();
        let second = render_tar_entries(&entries, 123).unwrap();
        assert_eq!(first, second);
        assert!(first[first.len() - 1024..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn native_tar_entries_have_stable_paths_and_modes() {
        let archive = render_tar_entries(
            &[
                ("bin/quirl", b"binary", 0o755),
                ("LICENSE", b"license", 0o644),
                ("THIRD_PARTY_NOTICES.md", b"notices", 0o644),
                ("THIRD_PARTY_LICENSES.txt", b"dependency licenses", 0o644),
            ],
            123,
        )
        .unwrap();
        for (offset, name, mode) in [
            (0, b"bin/quirl".as_slice(), b"0000755\0".as_slice()),
            (1_024, b"LICENSE".as_slice(), b"0000644\0".as_slice()),
            (
                2_048,
                b"THIRD_PARTY_NOTICES.md".as_slice(),
                b"0000644\0".as_slice(),
            ),
            (
                3_072,
                b"THIRD_PARTY_LICENSES.txt".as_slice(),
                b"0000644\0".as_slice(),
            ),
        ] {
            assert_eq!(&archive[offset..offset + name.len()], name);
            assert_eq!(&archive[offset + 100..offset + 108], mode);
        }
    }

    #[test]
    fn release_dependency_inventory_matches_every_supported_target() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        for target in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ] {
            let dependencies = release_dependencies_for_target(root, target).unwrap();
            assert!(dependencies.len() > 200);
        }
    }

    #[test]
    fn third_party_report_includes_declared_and_fallback_license_texts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let target = "x86_64-unknown-linux-gnu";
        let dependencies = release_dependencies_for_target(root, target).unwrap();
        let report = render_third_party_license_report(root, target, &dependencies).unwrap();
        let report = String::from_utf8(report).unwrap();
        assert!(
            report.contains("model2vec-rs v0.2.1\n  declared license: LicenseRef-file:LICENSE")
        );
        assert!(
            report
                .contains("keybindings v0.0.2:reviewed-fallback/keybindings-0.0.2-Apache-2.0.txt")
        );
        assert!(report.contains("mlua-sys v0.11.0:reviewed-fallback/mlua-sys-0.11.0-MIT.txt"));
        assert!(
            report.contains("number_prefix v0.4.0:reviewed-fallback/number_prefix-0.4.0-MIT.txt")
        );
        assert!(report.contains("onig_sys v69.9.3:oniguruma/COPYING"));
    }

    #[test]
    fn artifact_names_cannot_escape_the_output_directory() {
        assert!(validate_relative_file_name("quirl.tar").is_ok());
        assert!(validate_relative_file_name("../quirl.tar").is_err());
        assert!(validate_relative_file_name("nested/quirl.tar").is_err());
    }
}
