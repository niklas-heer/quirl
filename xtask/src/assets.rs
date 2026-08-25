//! Deterministic construction of separately downloadable runtime assets.
//!
//! Asset bytes and descriptor input are bounded, symlinks are rejected, and
//! manifests are assembled only from descriptors whose neighboring bytes match
//! their declared size and digest.

use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use crate::release::{
    CandidateIdentity, absolute, clean_candidate_identity, current_candidate_identity,
    immutable_write, input_error, json_bytes, read_bounded, read_json_bounded, release_url,
    render_tar_entries, resource_error, run_status_bounded, sha256_hex,
    validate_relative_file_name,
};

const CONTRACT_VERSION: u32 = 2;
const ASSET_BYTES_MAX: usize = 128 * 1024 * 1024;
const INPUT_FILES_MAX: usize = 64;
const INPUT_DEPTH_MAX: usize = 4;
const NOTICES_MAX: usize = 4;
const NOTICE_BYTES_MAX: usize = 16 * 1024;
const MODEL_ROOT: &str = "models/quirl-command-v3-int8/quirl-command-v3-9bc5efbd14096b54";

/// One downloadable-asset construction action.
#[derive(Debug, Subcommand)]
pub(crate) enum AssetsCommand {
    /// Build one runtime asset and its bounded machine descriptor.
    Build {
        /// Logical asset to construct.
        #[arg(long)]
        kind: AssetKind,
        /// Directory receiving the immutable asset and descriptor.
        #[arg(long)]
        output: PathBuf,
        /// Candidate identity source: `next` (default, cutting a release —
        /// requires a matching CHANGELOG/tag/notes) or `current` (refreshing
        /// content against whatever version is already shipped).
        #[arg(long, value_enum, default_value_t = IdentitySource::Next)]
        identity: IdentitySource,
    },
    /// Combine asset descriptors into the release asset manifest.
    Manifest {
        /// Directory containing one descriptor and file per logical asset.
        #[arg(long)]
        input: PathBuf,
        /// Exact manifest path to write.
        #[arg(long)]
        output: PathBuf,
        /// Previously published version-scoped manifest whose unchanged
        /// records are retained. New descriptors replace records with the
        /// same logical name after both inputs pass their full contracts.
        #[arg(long)]
        previous_manifest: Option<PathBuf>,
        /// Candidate identity source, matching whichever the descriptors
        /// were built with.
        #[arg(long, value_enum, default_value_t = IdentitySource::Next)]
        identity: IdentitySource,
    },
    /// Rewrite an already-built manifest's asset URLs to a different host.
    ///
    /// The manifest must already be fully built and validated; only the
    /// `url` field of each asset changes here, never `sha256`/`byte_size`,
    /// so this can't silently change what a client actually installs.
    RebaseManifest {
        /// Already-built manifest to rebase.
        #[arg(long)]
        input: PathBuf,
        /// New base URL; each asset's URL becomes `{base_url}/{file}`.
        #[arg(long)]
        base_url: String,
        /// Exact manifest path to write.
        #[arg(long)]
        output: PathBuf,
    },
}

/// Which identity an asset build/manifest carries.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum IdentitySource {
    /// The next planned release version — requires a matching CHANGELOG
    /// heading, release notes, and an as-yet-unpublished tag. Use when
    /// actually cutting a quirl release.
    Next,
    /// The current, already-shipped workspace version. Use for refreshing
    /// downloadable content (like the completion database) independently of
    /// cutting a new quirl release.
    Current,
}

impl IdentitySource {
    fn resolve(self, root: &Path) -> Result<CandidateIdentity, Box<dyn Error>> {
        match self {
            Self::Next => clean_candidate_identity(root),
            Self::Current => current_candidate_identity(root),
        }
    }
}

/// Supported separately downloadable runtime payloads.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum AssetKind {
    /// The compiled native completion database.
    CompletionDatabase,
    /// The pinned local command-model file bundle.
    CommandModel,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetCompatibility {
    pub(crate) quirl_version_requirement: String,
    pub(crate) operating_systems: Vec<String>,
    pub(crate) architectures: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetRecord {
    pub(crate) logical_name: String,
    pub(crate) file: String,
    pub(crate) format: String,
    pub(crate) format_version: u32,
    pub(crate) byte_size: u64,
    pub(crate) sha256: String,
    pub(crate) url: String,
    pub(crate) compatibility: AssetCompatibility,
    pub(crate) source_revision: String,
    pub(crate) source_date_epoch: u64,
    pub(crate) notices: Vec<AssetNotice>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetNotice {
    pub(crate) name: String,
    pub(crate) spdx_license: String,
    pub(crate) file: String,
    pub(crate) byte_size: u64,
    pub(crate) sha256: String,
    pub(crate) url: String,
    pub(crate) text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetManifest {
    pub(crate) schema_version: u32,
    pub(crate) quirl_version: String,
    pub(crate) assets: Vec<AssetRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetDescriptor {
    schema_version: u32,
    quirl_version: String,
    asset: AssetRecord,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetBuildResult {
    schema_version: u32,
    asset: String,
    descriptor: String,
    sha256: String,
    byte_size: u64,
    notices: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetManifestResult {
    schema_version: u32,
    manifest: String,
    asset_count: usize,
}

pub(crate) fn run(root: &Path, command: AssetsCommand) -> Result<(), Box<dyn Error>> {
    match command {
        AssetsCommand::Build {
            kind,
            output,
            identity,
        } => build(root, kind, &output, identity),
        AssetsCommand::Manifest {
            input,
            output,
            previous_manifest,
            identity,
        } => manifest(
            root,
            &input,
            &output,
            previous_manifest.as_deref(),
            identity,
        ),
        AssetsCommand::RebaseManifest {
            input,
            base_url,
            output,
        } => rebase_manifest(&input, &base_url, &output),
    }
}

impl AssetManifest {
    pub(crate) fn validate_for_release(&self, version: &str) -> Result<(), Box<dyn Error>> {
        self.validate(version, true)
    }

    pub(crate) fn validate_for_channel(&self, version: &str) -> Result<(), Box<dyn Error>> {
        self.validate(version, false)
    }

    fn validate(&self, version: &str, require_release_url: bool) -> Result<(), Box<dyn Error>> {
        if self.schema_version != CONTRACT_VERSION || self.quirl_version != version {
            return Err(input_error(
                "asset manifest does not target the requested Quirl version",
            ));
        }
        if self.assets.len() != 2 {
            return Err(input_error(
                "asset manifest must contain exactly two runtime assets",
            ));
        }
        let mut names = BTreeSet::new();
        for asset in &self.assets {
            validate_record(asset, version, require_release_url)?;
            if !names.insert(asset.logical_name.as_str()) {
                return Err(input_error(format!(
                    "duplicate runtime asset {}",
                    asset.logical_name
                )));
            }
        }
        if names != BTreeSet::from(["command-model", "completion-database"]) {
            return Err(input_error(
                "asset manifest is missing a required logical asset",
            ));
        }
        Ok(())
    }
}

fn build(
    root: &Path,
    kind: AssetKind,
    output: &Path,
    identity: IdentitySource,
) -> Result<(), Box<dyn Error>> {
    let identity = identity.resolve(root)?;
    let version = identity.version;
    let output = absolute(root, output);
    fs::create_dir_all(&output)?;
    let (logical_name, format, bytes) = match kind {
        AssetKind::CompletionDatabase => {
            run_status_bounded(
                Command::new(std::env::current_exe()?)
                    .current_dir(root)
                    .args(["catalog", "build"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null()),
                Duration::from_secs(10 * 60),
                "native completion database build",
            )?;
            let bytes = read_bounded(
                &root.join("catalog/generated/catalog.sqlite3"),
                ASSET_BYTES_MAX,
            )?;
            ("completion-database", "sqlite3", bytes)
        }
        AssetKind::CommandModel => {
            let model_root = root.join(MODEL_ROOT);
            let names = [
                "LICENSE",
                "README.md",
                "config.json",
                "model.safetensors",
                "quirl-model.json",
                "tokenizer.json",
            ];
            let mut retained = Vec::with_capacity(names.len());
            let mut total = 0usize;
            for name in names {
                let bytes = read_bounded(&model_root.join(name), ASSET_BYTES_MAX)?;
                total = total
                    .checked_add(bytes.len())
                    .ok_or_else(|| resource_error("model bundle byte count"))?;
                if total > ASSET_BYTES_MAX {
                    return Err(resource_error("model bundle byte count"));
                }
                retained.push((format!("command-model/{name}"), bytes));
            }
            let entries = retained
                .iter()
                .map(|(name, bytes)| (name.as_str(), bytes.as_slice(), 0o644))
                .collect::<Vec<_>>();
            let bytes = render_tar_entries(&entries, identity.source_date_epoch)?;
            ("command-model", "tar", bytes)
        }
    };
    let byte_size = u64::try_from(bytes.len()).map_err(|_| resource_error("asset byte count"))?;
    let sha256 = sha256_hex(&bytes);
    let file = content_addressed_file(logical_name, &version, &sha256)?;
    let notices = if logical_name == "completion-database" {
        let notice_bytes = read_bounded(
            &root.join("catalog/provenance/CARAPACE_LICENSE"),
            NOTICE_BYTES_MAX,
        )?;
        let notice_sha256 = sha256_hex(&notice_bytes);
        let notice_file = format!("quirl-carapace-license-{notice_sha256}.txt");
        let text = String::from_utf8(notice_bytes.clone())
            .map_err(|_| input_error("Carapace license notice is not UTF-8"))?;
        immutable_write(&output.join(&notice_file), &notice_bytes)?;
        vec![AssetNotice {
            name: "Carapace".to_owned(),
            spdx_license: "MIT".to_owned(),
            file: notice_file.clone(),
            byte_size: u64::try_from(notice_bytes.len())
                .map_err(|_| resource_error("asset notice byte count"))?,
            sha256: notice_sha256,
            url: release_url(&version, &notice_file),
            text,
        }]
    } else {
        Vec::new()
    };
    let notice_files = notices.iter().map(|notice| notice.file.clone()).collect();
    let record = AssetRecord {
        logical_name: logical_name.to_owned(),
        file: file.clone(),
        format: format.to_owned(),
        format_version: 1,
        byte_size,
        sha256: sha256.clone(),
        url: release_url(&version, &file),
        compatibility: AssetCompatibility {
            quirl_version_requirement: format!("={version}"),
            operating_systems: vec!["linux".to_owned(), "macos".to_owned()],
            architectures: vec!["aarch64".to_owned(), "x86_64".to_owned()],
        },
        source_revision: identity.commit.clone(),
        source_date_epoch: identity.source_date_epoch,
        notices,
    };
    let descriptor_name = format!("{logical_name}.asset-v2.json");
    immutable_write(&output.join(&file), &bytes)?;
    immutable_write(
        &output.join(&descriptor_name),
        &json_bytes(&AssetDescriptor {
            schema_version: CONTRACT_VERSION,
            quirl_version: version,
            asset: record,
        })?,
    )?;
    print_json(&AssetBuildResult {
        schema_version: CONTRACT_VERSION,
        asset: file,
        descriptor: descriptor_name,
        sha256,
        byte_size,
        notices: notice_files,
    })
}

fn manifest(
    root: &Path,
    input: &Path,
    output: &Path,
    previous_manifest: Option<&Path>,
    identity: IdentitySource,
) -> Result<(), Box<dyn Error>> {
    let identity = identity.resolve(root)?;
    manifest_with_identity(root, input, output, previous_manifest, identity)
}

fn manifest_with_identity(
    root: &Path,
    input: &Path,
    output: &Path,
    previous_manifest: Option<&Path>,
    identity: CandidateIdentity,
) -> Result<(), Box<dyn Error>> {
    let version = identity.version;
    let input = absolute(root, input);
    let output = absolute(root, output);
    if output.file_name() != Some(OsStr::new("asset-manifest-v2.json")) {
        return Err(input_error(
            "asset manifest output file must be named asset-manifest-v2.json",
        ));
    }
    let files = collect_files(&input)?;
    let mut descriptors = Vec::new();
    for path in &files {
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".asset-v2.json"))
        {
            descriptors.push(read_json_bounded::<AssetDescriptor>(path)?);
        }
    }
    if descriptors.is_empty() || descriptors.len() > 2 {
        return Err(input_error(
            "asset manifest input must contain one or two descriptors",
        ));
    }
    descriptors.sort_by(|left, right| left.asset.logical_name.cmp(&right.asset.logical_name));
    let replaced_names = descriptors
        .iter()
        .map(|descriptor| descriptor.asset.logical_name.as_str())
        .collect::<BTreeSet<_>>();
    let mut assets = BTreeMap::new();
    if let Some(previous_manifest) = previous_manifest {
        let previous_path = absolute(root, previous_manifest);
        let previous: AssetManifest = read_json_bounded(&previous_path)?;
        previous.validate_for_channel(&version)?;
        for asset in previous.assets {
            if !replaced_names.contains(asset.logical_name.as_str()) {
                verify_retained_record(&previous_path, &asset)?;
            }
            assets.insert(asset.logical_name.clone(), asset);
        }
    } else if descriptors.len() != 2 {
        return Err(input_error(
            "an initial asset manifest requires both runtime asset descriptors",
        ));
    }
    for descriptor in descriptors {
        if descriptor.schema_version != CONTRACT_VERSION
            || descriptor.quirl_version != version
            || descriptor.asset.source_revision != identity.commit
            || descriptor.asset.source_date_epoch != identity.source_date_epoch
        {
            return Err(input_error(
                "asset descriptor version does not match the workspace release",
            ));
        }
        validate_record(&descriptor.asset, &version, true)?;
        let path = unique_file_named(&files, &descriptor.asset.file)?;
        let bytes = read_bounded(path, ASSET_BYTES_MAX)?;
        if sha256_hex(&bytes) != descriptor.asset.sha256
            || u64::try_from(bytes.len()).ok() != Some(descriptor.asset.byte_size)
        {
            return Err(input_error(format!(
                "asset {} differs from its descriptor",
                descriptor.asset.file
            )));
        }
        for notice in &descriptor.asset.notices {
            let notice_path = unique_file_named(&files, &notice.file)?;
            let notice_bytes = read_bounded(notice_path, NOTICE_BYTES_MAX)?;
            if sha256_hex(&notice_bytes) != notice.sha256
                || u64::try_from(notice_bytes.len()).ok() != Some(notice.byte_size)
                || notice_bytes != notice.text.as_bytes()
            {
                return Err(input_error(format!(
                    "asset notice {} differs from its descriptor",
                    notice.file
                )));
            }
        }
        assets.insert(descriptor.asset.logical_name.clone(), descriptor.asset);
    }
    let manifest = AssetManifest {
        schema_version: CONTRACT_VERSION,
        quirl_version: version,
        assets: assets.into_values().collect(),
    };
    manifest.validate_for_channel(&manifest.quirl_version)?;
    immutable_write(&output, &json_bytes(&manifest)?)?;
    print_json(&AssetManifestResult {
        schema_version: CONTRACT_VERSION,
        manifest: output.display().to_string(),
        asset_count: manifest.assets.len(),
    })
}

fn verify_retained_record(manifest_path: &Path, asset: &AssetRecord) -> Result<(), Box<dyn Error>> {
    let directory = manifest_path
        .parent()
        .ok_or_else(|| input_error("previous asset manifest has no parent directory"))?;
    verify_sibling_bytes(
        directory,
        &asset.file,
        asset.byte_size,
        &asset.sha256,
        ASSET_BYTES_MAX,
        "retained asset",
    )?;
    for notice in &asset.notices {
        let bytes = verify_sibling_bytes(
            directory,
            &notice.file,
            notice.byte_size,
            &notice.sha256,
            NOTICE_BYTES_MAX,
            "retained asset notice",
        )?;
        if bytes != notice.text.as_bytes() {
            return Err(input_error(format!(
                "retained asset notice {} differs from its manifest text",
                notice.file
            )));
        }
    }
    Ok(())
}

fn verify_sibling_bytes(
    directory: &Path,
    file: &str,
    expected_size: u64,
    expected_sha256: &str,
    bytes_max: usize,
    label: &str,
) -> Result<Vec<u8>, Box<dyn Error>> {
    validate_relative_file_name(file)?;
    let path = directory.join(file);
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        input_error(format!(
            "could not inspect {label} sibling {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.file_type().is_file() || metadata.len() != expected_size {
        return Err(input_error(format!(
            "{label} sibling {} is not the declared regular file",
            path.display()
        )));
    }
    let bytes = read_bounded(&path, bytes_max)?;
    if sha256_hex(&bytes) != expected_sha256 {
        return Err(input_error(format!(
            "{label} sibling {} differs from its declared SHA-256",
            path.display()
        )));
    }
    Ok(bytes)
}

fn rebase_manifest(input: &Path, base_url: &str, output: &Path) -> Result<(), Box<dyn Error>> {
    // `file://` is accepted too, purely for local end-to-end testing: the
    // runtime side (`crates/quirl-cli/src/assets.rs`) only trusts `file://`
    // asset payloads when the manifest itself was loaded from a local file
    // via `--manifest`/`QUIRL_ASSET_MANIFEST_FILE`, never from a fetched
    // HTTPS manifest, so this can't be used to smuggle a local path into a
    // published manifest that real installs would ever see.
    let scheme_ok = base_url.starts_with("https://") || base_url.starts_with("file://");
    if base_url.is_empty() || !scheme_ok || base_url.ends_with('/') {
        return Err(input_error(
            "rebase base URL must be a bounded absolute HTTPS or file:// URL with no trailing slash",
        ));
    }
    let mut manifest: AssetManifest = read_json_bounded(input)?;
    let quirl_version = manifest.quirl_version.clone();
    // A refresh may merge an unchanged record from the currently published
    // website manifest with a newly built GitHub-URL descriptor. Validate the
    // complete provider-neutral contract before rewriting every URL together.
    manifest.validate_for_channel(&quirl_version)?;
    for asset in &mut manifest.assets {
        validate_relative_file_name(&asset.file)?;
        asset.url = format!("{base_url}/{}", asset.file);
        for notice in &mut asset.notices {
            validate_relative_file_name(&notice.file)?;
            notice.url = format!("{base_url}/{}", notice.file);
        }
    }
    immutable_write(output, &json_bytes(&manifest)?)?;
    print_json(&AssetManifestResult {
        schema_version: CONTRACT_VERSION,
        manifest: output.display().to_string(),
        asset_count: manifest.assets.len(),
    })
}

fn validate_record(
    record: &AssetRecord,
    version: &str,
    require_release_url: bool,
) -> Result<(), Box<dyn Error>> {
    validate_relative_file_name(&record.file)?;
    if !matches!(
        record.logical_name.as_str(),
        "completion-database" | "command-model"
    ) || !matches!(record.format.as_str(), "sqlite3" | "tar")
        || record.format_version != 1
        || record.byte_size == 0
        || record.byte_size > u64::try_from(ASSET_BYTES_MAX)?
        || record.sha256.len() != 64
        || !record
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || record.file != content_addressed_file(&record.logical_name, version, &record.sha256)?
        || (require_release_url && record.url != release_url(version, &record.file))
        || (!require_release_url
            && (!record.url.starts_with("https://")
                || !record.url.ends_with(&format!("/{}", record.file))))
        || record.compatibility.quirl_version_requirement != format!("={version}")
        || record.compatibility.operating_systems != ["linux", "macos"]
        || record.compatibility.architectures != ["aarch64", "x86_64"]
        || record.source_revision.len() != 40
        || !record
            .source_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || record.source_date_epoch == 0
        || !validate_notices(record, version, require_release_url)?
    {
        return Err(input_error(format!(
            "invalid runtime asset record {}",
            record.logical_name
        )));
    }
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    reason = "notice lookup returns exactly one candidate before fixed-slot access"
)]
fn validate_notices(
    record: &AssetRecord,
    version: &str,
    require_release_url: bool,
) -> Result<bool, Box<dyn Error>> {
    if record.notices.len() > NOTICES_MAX {
        return Ok(false);
    }
    if record.logical_name == "command-model" {
        return Ok(record.notices.is_empty());
    }
    if record.logical_name != "completion-database" || record.notices.len() != 1 {
        return Ok(false);
    }
    let notice = &record.notices[0];
    validate_relative_file_name(&notice.file)?;
    let text_bytes = notice.text.as_bytes();
    let expected_file = format!("quirl-carapace-license-{}.txt", notice.sha256);
    Ok(notice.name == "Carapace"
        && notice.spdx_license == "MIT"
        && !text_bytes.is_empty()
        && text_bytes.len() <= NOTICE_BYTES_MAX
        && u64::try_from(text_bytes.len()).ok() == Some(notice.byte_size)
        && sha256_hex(text_bytes) == notice.sha256
        && notice.file == expected_file
        && if require_release_url {
            notice.url == release_url(version, &notice.file)
        } else {
            notice.url.starts_with("https://") && notice.url.ends_with(&format!("/{}", notice.file))
        })
}

fn content_addressed_file(
    logical_name: &str,
    quirl_version: &str,
    sha256: &str,
) -> Result<String, Box<dyn Error>> {
    let (stem, extension) = match logical_name {
        "completion-database" => ("quirl-completion-database", "sqlite3"),
        "command-model" => ("quirl-command-model", "tar"),
        _ => return Err(input_error("unknown runtime asset logical name")),
    };
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(input_error("runtime asset digest is not lowercase SHA-256"));
    }
    Ok(format!("{stem}-v{quirl_version}-{sha256}.{extension}"))
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "visited file counts are bounded by the release asset limit"
)]
fn collect_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    if !fs::symlink_metadata(root)?.file_type().is_dir() {
        return Err(input_error("asset manifest input must be a directory"));
    }
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    let mut files = Vec::new();
    while let Some((directory, depth)) = pending.pop() {
        if depth > INPUT_DEPTH_MAX {
            return Err(resource_error("asset input directory depth"));
        }
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        if entries.len() > INPUT_FILES_MAX {
            return Err(resource_error("asset input entry count"));
        }
        for entry in entries {
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                return Err(input_error(format!(
                    "asset input contains symlink {}",
                    entry.path().display()
                )));
            }
            if file_type.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else if file_type.is_file() {
                files.push(entry.path());
                if files.len() > INPUT_FILES_MAX {
                    return Err(resource_error("asset input file count"));
                }
            } else {
                return Err(input_error(format!(
                    "asset input contains unsupported file {}",
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
            "expected exactly one asset file named {name}"
        )));
    }
    Ok(matching[0])
}

fn print_json(value: &impl Serialize) -> Result<(), Box<dyn Error>> {
    io::stdout().write_all(&json_bytes(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> CandidateIdentity {
        CandidateIdentity {
            version: "0.1.0".to_owned(),
            commit: "d".repeat(40),
            source_date_epoch: 2,
        }
    }

    fn record(name: &str, _file: &str, format: &str) -> AssetRecord {
        let sha256 = sha256_hex(b"x");
        let file = content_addressed_file(name, "0.1.0", &sha256).unwrap();
        let notices = if name == "completion-database" {
            let text = "MIT License\n\nCopyright (c) Carapace contributors\n".to_owned();
            let notice_sha256 = sha256_hex(text.as_bytes());
            let notice_file = format!("quirl-carapace-license-{notice_sha256}.txt");
            vec![AssetNotice {
                name: "Carapace".to_owned(),
                spdx_license: "MIT".to_owned(),
                file: notice_file.clone(),
                byte_size: u64::try_from(text.len()).unwrap(),
                sha256: notice_sha256,
                url: release_url("0.1.0", &notice_file),
                text,
            }]
        } else {
            Vec::new()
        };
        AssetRecord {
            logical_name: name.to_owned(),
            file: file.clone(),
            format: format.to_owned(),
            format_version: 1,
            byte_size: 1,
            sha256,
            url: release_url("0.1.0", &file),
            compatibility: AssetCompatibility {
                quirl_version_requirement: "=0.1.0".to_owned(),
                operating_systems: vec!["linux".to_owned(), "macos".to_owned()],
                architectures: vec!["aarch64".to_owned(), "x86_64".to_owned()],
            },
            source_revision: "b".repeat(40),
            source_date_epoch: 1,
            notices,
        }
    }

    #[test]
    fn manifest_requires_both_unique_logical_assets() {
        let valid = AssetManifest {
            schema_version: 2,
            quirl_version: "0.1.0".to_owned(),
            assets: vec![
                record("command-model", "model.tar", "tar"),
                record("completion-database", "completion.sqlite3", "sqlite3"),
            ],
        };
        assert!(valid.validate_for_release("0.1.0").is_ok());
        let duplicate = AssetManifest {
            assets: vec![valid.assets[0].clone(), valid.assets[0].clone()],
            ..valid
        };
        assert!(duplicate.validate_for_release("0.1.0").is_err());
    }

    #[test]
    fn manifest_contract_rejects_unknown_fields() {
        let json = r#"{"schema_version":2,"quirl_version":"0.1.0","assets":[],"extra":true}"#;
        assert!(serde_json::from_str::<AssetManifest>(json).is_err());
    }

    fn temp_path(name: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "quirl-xtask-assets-test-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn rebase_manifest_only_rewrites_urls() {
        let manifest = AssetManifest {
            schema_version: 2,
            quirl_version: "0.1.0".to_owned(),
            assets: vec![
                record("command-model", "model.tar", "tar"),
                record("completion-database", "completion.sqlite3", "sqlite3"),
            ],
        };
        assert!(manifest.validate_for_release("0.1.0").is_ok());
        let input = temp_path("input.json");
        let output = temp_path("output.json");
        fs::write(&input, json_bytes(&manifest).unwrap()).unwrap();

        rebase_manifest(&input, "https://quirl.vercel.app/reference", &output).unwrap();

        let rebased: AssetManifest = read_json_bounded(&output).unwrap();
        for (original, rewritten) in manifest.assets.iter().zip(&rebased.assets) {
            assert_eq!(
                rewritten.url,
                format!("https://quirl.vercel.app/reference/{}", original.file)
            );
            assert_eq!(rewritten.sha256, original.sha256);
            assert_eq!(rewritten.byte_size, original.byte_size);
            assert_eq!(rewritten.logical_name, original.logical_name);
            for (original_notice, rewritten_notice) in
                original.notices.iter().zip(&rewritten.notices)
            {
                assert_eq!(
                    rewritten_notice.url,
                    format!(
                        "https://quirl.vercel.app/reference/{}",
                        original_notice.file
                    )
                );
                assert_eq!(rewritten_notice.text, original_notice.text);
                assert_eq!(rewritten_notice.sha256, original_notice.sha256);
            }
        }
        assert_eq!(rebased.quirl_version, manifest.quirl_version);

        fs::remove_file(&input).unwrap();
        fs::remove_file(&output).unwrap();
    }

    #[test]
    fn rebase_manifest_rejects_a_non_https_base_url() {
        let manifest = AssetManifest {
            schema_version: 2,
            quirl_version: "0.1.0".to_owned(),
            assets: vec![
                record("command-model", "model.tar", "tar"),
                record("completion-database", "completion.sqlite3", "sqlite3"),
            ],
        };
        let input = temp_path("insecure-input.json");
        let output = temp_path("insecure-output.json");
        fs::write(&input, json_bytes(&manifest).unwrap()).unwrap();

        assert!(rebase_manifest(&input, "http://insecure.invalid", &output).is_err());
        assert!(rebase_manifest(&input, "https://trailing.invalid/", &output).is_err());

        fs::remove_file(&input).unwrap();
    }

    #[test]
    fn manifest_refresh_retains_only_an_exact_previous_payload() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let identity = test_identity();
        let directory = temp_path("merge");
        let input = directory.join("input");
        fs::create_dir_all(&input).unwrap();

        let mut retained = record("command-model", "ignored", "tar");
        retained.source_revision = "c".repeat(40);
        retained.url = format!("https://quirl.dev/reference/v0.1.0/{}", retained.file);
        let previous = AssetManifest {
            schema_version: CONTRACT_VERSION,
            quirl_version: identity.version.clone(),
            assets: vec![
                retained.clone(),
                record("completion-database", "ignored", "sqlite3"),
            ],
        };
        let previous_path = directory.join("previous.json");
        fs::write(&previous_path, json_bytes(&previous).unwrap()).unwrap();
        fs::write(directory.join(&retained.file), b"x").unwrap();

        let bytes = b"new completion database";
        let sha256 = sha256_hex(bytes);
        let file =
            content_addressed_file("completion-database", &identity.version, &sha256).unwrap();
        let mut replacement = record("completion-database", "ignored", "sqlite3");
        replacement.file = file.clone();
        replacement.byte_size = u64::try_from(bytes.len()).unwrap();
        replacement.sha256 = sha256;
        replacement.url = release_url(&identity.version, &file);
        replacement.source_revision = identity.commit.clone();
        replacement.source_date_epoch = identity.source_date_epoch;
        fs::write(input.join(&file), bytes).unwrap();
        for notice in &replacement.notices {
            fs::write(input.join(&notice.file), notice.text.as_bytes()).unwrap();
        }
        fs::write(
            input.join("completion-database.asset-v2.json"),
            json_bytes(&AssetDescriptor {
                schema_version: CONTRACT_VERSION,
                quirl_version: identity.version.clone(),
                asset: replacement.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let output = directory.join("asset-manifest-v2.json");
        manifest_with_identity(root, &input, &output, Some(&previous_path), test_identity())
            .unwrap();

        let merged: AssetManifest = read_json_bounded(&output).unwrap();
        assert_eq!(merged.assets.len(), 2);
        assert!(merged.assets.contains(&retained));
        assert!(merged.assets.contains(&replacement));

        fs::remove_file(&output).unwrap();
        fs::write(directory.join(&retained.file), b"y").unwrap();
        assert!(
            manifest_with_identity(root, &input, &output, Some(&previous_path), test_identity(),)
                .is_err()
        );
        assert!(!output.exists());

        fs::remove_file(directory.join(&retained.file)).unwrap();
        assert!(
            manifest_with_identity(root, &input, &output, Some(&previous_path), test_identity())
                .is_err()
        );
        assert!(!output.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn retained_record_requires_exact_sibling_payload_and_notice_bytes() {
        let directory = temp_path("retained-files");
        fs::create_dir_all(&directory).unwrap();
        let record = record("completion-database", "ignored", "sqlite3");
        let manifest_path = directory.join("asset-manifest-v2.json");
        fs::write(&manifest_path, b"{}").unwrap();
        fs::write(directory.join(&record.file), b"x").unwrap();
        for notice in &record.notices {
            fs::write(directory.join(&notice.file), notice.text.as_bytes()).unwrap();
        }
        assert!(verify_retained_record(&manifest_path, &record).is_ok());

        fs::write(directory.join(&record.file), b"y").unwrap();
        assert!(verify_retained_record(&manifest_path, &record).is_err());
        fs::write(directory.join(&record.file), b"x").unwrap();

        let notice = &record.notices[0];
        let mut corrupt_notice = notice.text.as_bytes().to_vec();
        corrupt_notice[0] ^= 1;
        fs::write(directory.join(&notice.file), corrupt_notice).unwrap();
        assert!(verify_retained_record(&manifest_path, &record).is_err());
        fs::remove_file(directory.join(&notice.file)).unwrap();
        assert!(verify_retained_record(&manifest_path, &record).is_err());
        fs::remove_file(directory.join(&record.file)).unwrap();
        assert!(verify_retained_record(&manifest_path, &record).is_err());

        fs::remove_dir_all(directory).unwrap();
    }
}
