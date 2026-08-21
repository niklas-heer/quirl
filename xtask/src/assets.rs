//! Deterministic construction of separately downloadable runtime assets.
//!
//! Asset bytes and descriptor input are bounded, symlinks are rejected, and
//! manifests are assembled only from descriptors whose neighboring bytes match
//! their declared size and digest.

use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
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

const CONTRACT_VERSION: u32 = 1;
const ASSET_BYTES_MAX: usize = 128 * 1024 * 1024;
const INPUT_FILES_MAX: usize = 64;
const INPUT_DEPTH_MAX: usize = 4;
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetManifest {
    pub(crate) schema_version: u32,
    pub(crate) release_version: String,
    pub(crate) candidate_commit: String,
    pub(crate) source_date_epoch: u64,
    pub(crate) assets: Vec<AssetRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetDescriptor {
    schema_version: u32,
    release_version: String,
    candidate_commit: String,
    source_date_epoch: u64,
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
            identity,
        } => manifest(root, &input, &output, identity),
        AssetsCommand::RebaseManifest {
            input,
            base_url,
            output,
        } => rebase_manifest(&input, &base_url, &output),
    }
}

impl AssetManifest {
    pub(crate) fn validate_for_release(&self, version: &str) -> Result<(), Box<dyn Error>> {
        if self.schema_version != CONTRACT_VERSION || self.release_version != version {
            return Err(input_error(
                "asset manifest version does not match the release",
            ));
        }
        if self.candidate_commit.len() != 40
            || !self
                .candidate_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || self.source_date_epoch == 0
        {
            return Err(input_error("asset manifest candidate identity is invalid"));
        }
        if self.assets.len() != 2 {
            return Err(input_error(
                "asset manifest must contain exactly two runtime assets",
            ));
        }
        let mut names = BTreeSet::new();
        for asset in &self.assets {
            validate_record(asset, version)?;
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
    let (logical_name, file, format, bytes) = match kind {
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
            let file = format!("quirl-completion-database-v{version}.sqlite3");
            let bytes = read_bounded(
                &root.join("catalog/generated/catalog.sqlite3"),
                ASSET_BYTES_MAX,
            )?;
            ("completion-database", file, "sqlite3", bytes)
        }
        AssetKind::CommandModel => {
            let file = format!("quirl-command-model-v{version}.tar");
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
            ("command-model", file, "tar", bytes)
        }
    };
    let byte_size = u64::try_from(bytes.len()).map_err(|_| resource_error("asset byte count"))?;
    let sha256 = sha256_hex(&bytes);
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
    };
    let descriptor_name = format!("{logical_name}.asset-v1.json");
    immutable_write(&output.join(&file), &bytes)?;
    immutable_write(
        &output.join(&descriptor_name),
        &json_bytes(&AssetDescriptor {
            schema_version: CONTRACT_VERSION,
            release_version: version,
            candidate_commit: identity.commit,
            source_date_epoch: identity.source_date_epoch,
            asset: record,
        })?,
    )?;
    print_json(&AssetBuildResult {
        schema_version: CONTRACT_VERSION,
        asset: file,
        descriptor: descriptor_name,
        sha256,
        byte_size,
    })
}

fn manifest(
    root: &Path,
    input: &Path,
    output: &Path,
    identity: IdentitySource,
) -> Result<(), Box<dyn Error>> {
    let identity = identity.resolve(root)?;
    let version = identity.version;
    let input = absolute(root, input);
    let output = absolute(root, output);
    if output.file_name() != Some(OsStr::new("asset-manifest-v1.json")) {
        return Err(input_error(
            "asset manifest output file must be named asset-manifest-v1.json",
        ));
    }
    let files = collect_files(&input)?;
    let mut descriptors = Vec::new();
    for path in &files {
        if path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".asset-v1.json"))
        {
            descriptors.push(read_json_bounded::<AssetDescriptor>(path)?);
        }
    }
    if descriptors.len() != 2 {
        return Err(input_error(
            "asset manifest input must contain exactly two descriptors",
        ));
    }
    descriptors.sort_by(|left, right| left.asset.logical_name.cmp(&right.asset.logical_name));
    let mut assets = Vec::with_capacity(descriptors.len());
    for descriptor in descriptors {
        if descriptor.schema_version != CONTRACT_VERSION
            || descriptor.release_version != version
            || descriptor.candidate_commit != identity.commit
            || descriptor.source_date_epoch != identity.source_date_epoch
        {
            return Err(input_error(
                "asset descriptor version does not match the workspace release",
            ));
        }
        validate_record(&descriptor.asset, &version)?;
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
        assets.push(descriptor.asset);
    }
    let manifest = AssetManifest {
        schema_version: CONTRACT_VERSION,
        release_version: version,
        candidate_commit: identity.commit,
        source_date_epoch: identity.source_date_epoch,
        assets,
    };
    manifest.validate_for_release(&manifest.release_version)?;
    immutable_write(&output, &json_bytes(&manifest)?)?;
    print_json(&AssetManifestResult {
        schema_version: CONTRACT_VERSION,
        manifest: output.display().to_string(),
        asset_count: manifest.assets.len(),
    })
}

fn rebase_manifest(input: &Path, base_url: &str, output: &Path) -> Result<(), Box<dyn Error>> {
    if base_url.is_empty() || !base_url.starts_with("https://") || base_url.ends_with('/') {
        return Err(input_error(
            "rebase base URL must be a bounded absolute HTTPS URL with no trailing slash",
        ));
    }
    let mut manifest: AssetManifest = read_json_bounded(input)?;
    let release_version = manifest.release_version.clone();
    // `validate_for_release` requires each asset's URL to be the exact
    // GitHub Releases form (`validate_record`), so it only runs here, on the
    // as-built manifest, before any URL rewriting — confirming the input is
    // a genuine, unmodified build output. It can't run again afterward: a
    // rebased manifest fails that same GitHub-specific check by design.
    manifest.validate_for_release(&release_version)?;
    for asset in &mut manifest.assets {
        validate_relative_file_name(&asset.file)?;
        asset.url = format!("{base_url}/{}", asset.file);
    }
    immutable_write(output, &json_bytes(&manifest)?)?;
    print_json(&AssetManifestResult {
        schema_version: CONTRACT_VERSION,
        manifest: output.display().to_string(),
        asset_count: manifest.assets.len(),
    })
}

fn validate_record(record: &AssetRecord, version: &str) -> Result<(), Box<dyn Error>> {
    validate_relative_file_name(&record.file)?;
    if !matches!(
        record.logical_name.as_str(),
        "completion-database" | "command-model"
    ) || !matches!(record.format.as_str(), "sqlite3" | "tar")
        || record.format_version != 1
        || record.byte_size == 0
        || record.byte_size > u64::try_from(ASSET_BYTES_MAX)?
        || record.sha256.len() != 64
        || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || record.url != release_url(version, &record.file)
        || record.compatibility.quirl_version_requirement != format!("={version}")
        || record.compatibility.operating_systems != ["linux", "macos"]
        || record.compatibility.architectures != ["aarch64", "x86_64"]
    {
        return Err(input_error(format!(
            "invalid runtime asset record {}",
            record.logical_name
        )));
    }
    Ok(())
}

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

    fn record(name: &str, file: &str, format: &str) -> AssetRecord {
        AssetRecord {
            logical_name: name.to_owned(),
            file: file.to_owned(),
            format: format.to_owned(),
            format_version: 1,
            byte_size: 1,
            sha256: "a".repeat(64),
            url: release_url("0.1.0", file),
            compatibility: AssetCompatibility {
                quirl_version_requirement: "=0.1.0".to_owned(),
                operating_systems: vec!["linux".to_owned(), "macos".to_owned()],
                architectures: vec!["aarch64".to_owned(), "x86_64".to_owned()],
            },
        }
    }

    #[test]
    fn manifest_requires_both_unique_logical_assets() {
        let valid = AssetManifest {
            schema_version: 1,
            release_version: "0.1.0".to_owned(),
            candidate_commit: "b".repeat(40),
            source_date_epoch: 1,
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
        let json = r#"{"schema_version":1,"release_version":"0.1.0","candidate_commit":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","source_date_epoch":1,"assets":[],"extra":true}"#;
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
            schema_version: 1,
            release_version: "0.1.0".to_owned(),
            candidate_commit: "b".repeat(40),
            source_date_epoch: 1,
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
        }
        assert_eq!(rebased.release_version, manifest.release_version);
        assert_eq!(rebased.candidate_commit, manifest.candidate_commit);

        fs::remove_file(&input).unwrap();
        fs::remove_file(&output).unwrap();
    }

    #[test]
    fn rebase_manifest_rejects_a_non_https_base_url() {
        let manifest = AssetManifest {
            schema_version: 1,
            release_version: "0.1.0".to_owned(),
            candidate_commit: "b".repeat(40),
            source_date_epoch: 1,
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
}
