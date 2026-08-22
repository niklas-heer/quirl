//! Deterministic Homebrew formula generation and bounded offline validation.

use clap::Subcommand;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use crate::release::{
    ReleaseArtifact, ReleaseManifest, absolute, atomic_write, current_commit, input_error,
    json_bytes, local_tag_target, read_bounded, read_json_bounded, release_url, run_status_bounded,
    sha256_hex, verify_binary_version,
};

const FORMULA_BYTES_MAX: usize = 128 * 1024;
const PACKAGE_BYTES_MAX: usize = 128 * 1024 * 1024;
const HOMEBREW_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// One Homebrew formula action.
#[derive(Debug, Subcommand)]
pub(crate) enum HomebrewCommand {
    /// Preview or atomically write Formula/quirl.rb from a release manifest.
    Render {
        /// Versioned release manifest produced by `release aggregate`.
        #[arg(long)]
        release_manifest: PathBuf,
        /// Exact published tag whose commit and manifest must agree.
        #[arg(long)]
        expected_tag: String,
        /// Root of a checkout of the separate Homebrew tap.
        #[arg(long)]
        tap_root: PathBuf,
        /// Write Formula/quirl.rb; without this flag the formula is only printed.
        #[arg(long)]
        write: bool,
    },
    /// Validate the generated formula and run Homebrew's bounded offline checks.
    Check {
        /// Root of a checkout of the separate Homebrew tap.
        #[arg(long)]
        tap_root: PathBuf,
        /// Release manifest used to verify the local package-install test.
        #[arg(long, requires = "package_root")]
        release_manifest: Option<PathBuf>,
        /// Directory containing the already-downloaded native package.
        #[arg(long, requires = "release_manifest")]
        package_root: Option<PathBuf>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RenderResult {
    schema_version: u32,
    write: bool,
    formula: String,
    version: String,
    sha256: String,
    formula_content: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CheckResult {
    schema_version: u32,
    formula: String,
    byte_size: usize,
    brew_checks_run: bool,
    offline_package_test_run: bool,
}

pub(crate) fn run(root: &Path, command: HomebrewCommand) -> Result<(), Box<dyn Error>> {
    match command {
        HomebrewCommand::Render {
            release_manifest,
            expected_tag,
            tap_root,
            write,
        } => render(root, &release_manifest, &expected_tag, &tap_root, write),
        HomebrewCommand::Check {
            tap_root,
            release_manifest,
            package_root,
        } => check(
            root,
            &tap_root,
            release_manifest.as_deref(),
            package_root.as_deref(),
        ),
    }
}

fn render(
    root: &Path,
    release_manifest: &Path,
    expected_tag: &str,
    tap_root: &Path,
    write: bool,
) -> Result<(), Box<dyn Error>> {
    let manifest: ReleaseManifest = read_json_bounded(&absolute(root, release_manifest))?;
    validate_manifest(root, &manifest)?;
    if expected_tag != manifest.tag {
        return Err(input_error(
            "requested Homebrew tag does not match the release manifest tag",
        ));
    }
    let formula = render_formula(&manifest)?;
    let formula_path = absolute(root, tap_root).join("Formula/quirl.rb");
    if write {
        atomic_write(&formula_path, formula.as_bytes())?;
    }
    print_json(&RenderResult {
        schema_version: 1,
        write,
        formula: formula_path.display().to_string(),
        version: manifest.version,
        sha256: crate::release::sha256_hex(formula.as_bytes()),
        formula_content: (!write).then_some(formula),
    })?;
    Ok(())
}

fn check(
    root: &Path,
    tap_root: &Path,
    release_manifest: Option<&Path>,
    package_root: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let tap_root = absolute(root, tap_root);
    let formula_path = tap_root.join("Formula/quirl.rb");
    let formula = String::from_utf8(read_bounded(&formula_path, FORMULA_BYTES_MAX)?)?;
    validate_formula_shape(&formula)?;
    let brew_checks_run = executable_on_path("brew")?;
    if brew_checks_run {
        run_status_bounded(
            Command::new("brew")
                .current_dir(&tap_root)
                .args(["style", "Formula/quirl.rb"]),
            HOMEBREW_TIMEOUT,
            "brew style",
        )?;
        run_status_bounded(
            Command::new("brew")
                .current_dir(&tap_root)
                .args(["audit", "--strict", "Formula/quirl.rb"])
                .env("HOMEBREW_NO_AUTO_UPDATE", "1")
                .env("HOMEBREW_NO_ANALYTICS", "1"),
            HOMEBREW_TIMEOUT,
            "brew audit",
        )?;
    }
    let offline_package_test_run = match (release_manifest, package_root) {
        (Some(release_manifest), Some(package_root)) => {
            offline_package_test(root, release_manifest, package_root)?;
            true
        }
        (None, None) => false,
        _ => {
            return Err(input_error(
                "offline package test requires both manifest and package root",
            ));
        }
    };
    print_json(&CheckResult {
        schema_version: 1,
        formula: formula_path.display().to_string(),
        byte_size: formula.len(),
        brew_checks_run,
        offline_package_test_run,
    })
}

fn validate_manifest(root: &Path, manifest: &ReleaseManifest) -> Result<(), Box<dyn Error>> {
    validate_release_version(&manifest.version)?;
    if manifest.schema_version != 1
        || manifest.product != "quirl"
        || manifest.tag != format!("v{}", manifest.version)
        || manifest.candidate_commit.len() != 40
        || !manifest
            .candidate_commit
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || manifest.source_date_epoch == 0
        || current_commit(root)? != manifest.candidate_commit
        || local_tag_target(root, &manifest.tag)? != manifest.candidate_commit
        || manifest.artifacts.len() != 4
        || manifest.asset_manifest.as_deref() != Some("asset-manifest-v2.json")
    {
        return Err(input_error(
            "release manifest is not a complete Quirl v1 release",
        ));
    }
    crate::assets::AssetManifest {
        schema_version: 2,
        quirl_version: manifest.version.clone(),
        assets: manifest.assets.clone(),
    }
    .validate_for_release(&manifest.version)?;
    if manifest.assets.iter().any(|asset| {
        asset.source_revision != manifest.candidate_commit
            || asset.source_date_epoch != manifest.source_date_epoch
    }) {
        return Err(input_error(
            "release asset source identity does not match the native candidate",
        ));
    }
    let expected = [
        "aarch64-apple-darwin",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "x86_64-unknown-linux-gnu",
    ];
    let mut targets = manifest
        .artifacts
        .iter()
        .map(|artifact| artifact.target.as_str())
        .collect::<Vec<_>>();
    targets.sort_unstable();
    if targets != expected {
        return Err(input_error(
            "release manifest does not contain each supported Homebrew target exactly once",
        ));
    }
    for artifact in &manifest.artifacts {
        validate_artifact_for_manifest(&manifest.version, artifact)?;
    }
    Ok(())
}

fn validate_artifact_for_manifest(
    version: &str,
    artifact: &ReleaseArtifact,
) -> Result<(), Box<dyn Error>> {
    let expected_file = format!("quirl-v{version}-{}.tar", artifact.target);
    if artifact.logical_name != "quirl"
        || artifact.file != expected_file
        || artifact.provenance != format!("{expected_file}.provenance.json")
        || artifact.byte_size == 0
        || artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || artifact.url != release_url(version, &expected_file)
    {
        return Err(input_error(format!(
            "invalid Homebrew artifact {}",
            artifact.file
        )));
    }
    Ok(())
}

fn validate_release_version(version: &str) -> Result<(), Box<dyn Error>> {
    if version.is_empty() || version.len() > 64 || version.contains(['-', '+']) {
        return Err(input_error(
            "Homebrew release version must be stable SemVer",
        ));
    }
    let fields = version.split('.').collect::<Vec<_>>();
    if fields.len() != 3
        || fields.iter().any(|field| {
            field.is_empty()
                || (field.len() > 1 && field.starts_with('0'))
                || !field.bytes().all(|byte| byte.is_ascii_digit())
        })
    {
        return Err(input_error(
            "Homebrew release version must be stable SemVer",
        ));
    }
    Ok(())
}

fn offline_package_test(
    root: &Path,
    release_manifest: &Path,
    package_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let manifest: ReleaseManifest = read_json_bounded(&absolute(root, release_manifest))?;
    validate_manifest(root, &manifest)?;
    let target = host_release_target()?;
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.target == target)
        .ok_or_else(|| input_error(format!("release manifest has no package for host {target}")))?;
    let archive = read_bounded(
        &absolute(root, package_root).join(&artifact.file),
        PACKAGE_BYTES_MAX,
    )?;
    if u64::try_from(archive.len()).ok() != Some(artifact.byte_size)
        || sha256_hex(&archive) != artifact.sha256
    {
        return Err(input_error(
            "offline Homebrew package differs from the release manifest",
        ));
    }
    let package = extract_native_package(&archive)?;
    if [
        package.product_license.as_slice(),
        package.third_party_notices.as_slice(),
        package.third_party_licenses.as_slice(),
    ]
    .iter()
    .any(|document| document.is_empty())
    {
        return Err(input_error(
            "native package contains an empty redistribution document",
        ));
    }
    let temporary = install_temporary_binary(&package.binary)?;
    verify_binary_version(&temporary.path, &manifest.version)
}

fn host_release_target() -> Result<&'static str, Box<dyn Error>> {
    match (env::consts::ARCH, env::consts::OS) {
        ("aarch64", "macos") => Ok("aarch64-apple-darwin"),
        ("x86_64", "macos") => Ok("x86_64-apple-darwin"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        (architecture, operating_system) => Err(input_error(format!(
            "unsupported offline Homebrew test host {architecture}-{operating_system}"
        ))),
    }
}

struct NativePackage {
    binary: Vec<u8>,
    product_license: Vec<u8>,
    third_party_notices: Vec<u8>,
    third_party_licenses: Vec<u8>,
}

fn extract_native_package(archive: &[u8]) -> Result<NativePackage, Box<dyn Error>> {
    if archive.len() < 1_536 || !archive.len().is_multiple_of(512) {
        return Err(input_error(
            "native package is not a complete ustar archive",
        ));
    }
    let mut offset = 0usize;
    let mut names = BTreeSet::new();
    let mut binary = None;
    let mut product_license = None;
    let mut third_party_notices = None;
    let mut third_party_licenses = None;
    while archive.len().saturating_sub(offset) > 1_024 {
        let entry = parse_package_tar_entry(archive, offset)?;
        if !names.insert(entry.name) {
            return Err(input_error("native package contains a duplicate entry"));
        }
        match entry.name {
            b"bin/quirl" if entry.mode == 0o755 => binary = Some(entry.bytes.to_vec()),
            b"LICENSE" if entry.mode == 0o644 => {
                product_license = Some(entry.bytes.to_vec());
            }
            b"THIRD_PARTY_NOTICES.md" if entry.mode == 0o644 => {
                third_party_notices = Some(entry.bytes.to_vec());
            }
            b"THIRD_PARTY_LICENSES.txt" if entry.mode == 0o644 => {
                third_party_licenses = Some(entry.bytes.to_vec());
            }
            _ => return Err(input_error("native package contains an unexpected entry")),
        }
        offset = entry.next_offset;
    }
    if archive.len().checked_sub(offset) != Some(1_024)
        || !archive[offset..].iter().all(|byte| *byte == 0)
        || names
            != BTreeSet::from([
                b"bin/quirl".as_slice(),
                b"LICENSE".as_slice(),
                b"THIRD_PARTY_NOTICES.md".as_slice(),
                b"THIRD_PARTY_LICENSES.txt".as_slice(),
            ])
    {
        return Err(input_error(
            "native package does not contain the exact release file set",
        ));
    }
    Ok(NativePackage {
        binary: binary.ok_or_else(|| input_error("native package binary is missing"))?,
        product_license: product_license
            .ok_or_else(|| input_error("native package product license is missing"))?,
        third_party_notices: third_party_notices
            .ok_or_else(|| input_error("native package third-party notices are missing"))?,
        third_party_licenses: third_party_licenses
            .ok_or_else(|| input_error("native package third-party licenses are missing"))?,
    })
}

struct PackageTarEntry<'a> {
    name: &'a [u8],
    bytes: &'a [u8],
    mode: usize,
    next_offset: usize,
}

fn parse_package_tar_entry(
    archive: &[u8],
    offset: usize,
) -> Result<PackageTarEntry<'_>, Box<dyn Error>> {
    let header_end = offset
        .checked_add(512)
        .ok_or_else(|| input_error("native package header offset overflowed"))?;
    let header = archive
        .get(offset..header_end)
        .ok_or_else(|| input_error("native package entry header is truncated"))?;
    if &header[257..263] != b"ustar\0" || header[156] != b'0' {
        return Err(input_error(
            "native package entry is not a regular ustar file",
        ));
    }
    let name_end = header[..100]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(100);
    let size = parse_tar_octal(&header[124..136], "entry size")?;
    let mode = parse_tar_octal(&header[100..108], "entry mode")?;
    if size == 0 || size > PACKAGE_BYTES_MAX {
        return Err(input_error(
            "native package entry size is outside its limit",
        ));
    }
    let end = header_end
        .checked_add(size)
        .ok_or_else(|| input_error("native package entry size overflowed"))?;
    let next_offset = end
        .checked_add(511)
        .and_then(|value| value.checked_div(512))
        .and_then(|value| value.checked_mul(512))
        .ok_or_else(|| input_error("native package padding overflowed"))?;
    let bytes = archive
        .get(header_end..end)
        .ok_or_else(|| input_error("native package entry is truncated"))?;
    let padding = archive
        .get(end..next_offset)
        .ok_or_else(|| input_error("native package entry padding is truncated"))?;
    if !padding.iter().all(|byte| *byte == 0) {
        return Err(input_error("native package entry padding is not zeroed"));
    }
    Ok(PackageTarEntry {
        name: &header[..name_end],
        bytes,
        mode,
        next_offset,
    })
}

fn parse_tar_octal(field: &[u8], label: &str) -> Result<usize, Box<dyn Error>> {
    let value = std::str::from_utf8(field)?
        .trim_matches(char::from(0))
        .trim();
    usize::from_str_radix(value, 8)
        .map_err(|_| input_error(format!("native package has an invalid {label}")))
}

struct TemporaryBinary {
    path: PathBuf,
}

impl Drop for TemporaryBinary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn install_temporary_binary(bytes: &[u8]) -> Result<TemporaryBinary, Box<dyn Error>> {
    let path = env::temp_dir().join(format!(
        "quirl-homebrew-offline-test-{}",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let install = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    })();
    if let Err(error) = install {
        let _ = fs::remove_file(&path);
        return Err(error.into());
    }
    Ok(TemporaryBinary { path })
}

fn render_formula(manifest: &ReleaseManifest) -> Result<String, Box<dyn Error>> {
    let artifacts = manifest
        .artifacts
        .iter()
        .map(|artifact| (artifact.target.as_str(), artifact))
        .collect::<BTreeMap<_, _>>();
    let mac_arm = artifact(&artifacts, "aarch64-apple-darwin")?;
    let mac_intel = artifact(&artifacts, "x86_64-apple-darwin")?;
    let linux_arm = artifact(&artifacts, "aarch64-unknown-linux-gnu")?;
    let linux_intel = artifact(&artifacts, "x86_64-unknown-linux-gnu")?;
    let mut output = String::new();
    output.push_str("class Quirl < Formula\n");
    output.push_str("  desc \"Everything you need, mixed in\"\n");
    output.push_str("  homepage \"https://github.com/niklas-heer/quirl\"\n");
    output.push_str(&format!("  version \"{}\"\n", manifest.version));
    output.push_str("  license \"MIT\"\n\n");
    render_platform(&mut output, "macos", mac_arm, mac_intel);
    output.push('\n');
    render_platform(&mut output, "linux", linux_arm, linux_intel);
    output.push_str("\n  def install\n");
    output.push_str("    bin.install \"bin/quirl\"\n");
    output.push_str(
        "    (pkgshare/\"licenses\").install \"LICENSE\", \"THIRD_PARTY_NOTICES.md\", \"THIRD_PARTY_LICENSES.txt\"\n",
    );
    output.push_str("  end\n\n");
    output.push_str("  test do\n");
    output.push_str(&format!(
        "    assert_match \"quirl {}\", shell_output(\"#{{bin}}/quirl --version\", 0)\n",
        manifest.version
    ));
    output.push_str(
        "    %w[LICENSE THIRD_PARTY_NOTICES.md THIRD_PARTY_LICENSES.txt].each do |notice|\n",
    );
    output.push_str("      assert_path_exists pkgshare/\"licenses\"/notice\n");
    output.push_str("    end\n");
    output.push_str("  end\n");
    output.push_str("end\n");
    if output.len() > FORMULA_BYTES_MAX {
        return Err(input_error(
            "generated Homebrew formula exceeds its byte limit",
        ));
    }
    Ok(output)
}

fn render_platform(
    output: &mut String,
    platform: &str,
    arm: &ReleaseArtifact,
    intel: &ReleaseArtifact,
) {
    output.push_str(&format!("  on_{platform} do\n"));
    output.push_str("    if Hardware::CPU.arm?\n");
    output.push_str(&format!("      url \"{}\"\n", arm.url));
    output.push_str(&format!("      sha256 \"{}\"\n", arm.sha256));
    output.push_str("    else\n");
    output.push_str(&format!("      url \"{}\"\n", intel.url));
    output.push_str(&format!("      sha256 \"{}\"\n", intel.sha256));
    output.push_str("    end\n");
    output.push_str("  end\n");
}

fn artifact<'a>(
    artifacts: &'a BTreeMap<&str, &ReleaseArtifact>,
    target: &str,
) -> Result<&'a ReleaseArtifact, Box<dyn Error>> {
    artifacts
        .get(target)
        .copied()
        .ok_or_else(|| input_error(format!("release manifest is missing {target}")))
}

fn validate_formula_shape(formula: &str) -> Result<(), Box<dyn Error>> {
    let required = [
        "class Quirl < Formula",
        "  on_macos do",
        "  on_linux do",
        "bin.install \"bin/quirl\"",
        "(pkgshare/\"licenses\").install \"LICENSE\", \"THIRD_PARTY_NOTICES.md\", \"THIRD_PARTY_LICENSES.txt\"",
        "assert_path_exists pkgshare/\"licenses\"/notice",
        "shell_output(\"#{bin}/quirl --version\", 0)",
    ];
    if required.iter().any(|marker| !formula.contains(marker))
        || formula.matches("      url \"https://").count() != 4
        || formula.matches("      sha256 \"").count() != 4
        || formula.contains("asset")
        || formula.contains("curl")
    {
        return Err(input_error(
            "Formula/quirl.rb is not a bounded offline Quirl formula",
        ));
    }
    Ok(())
}

fn executable_on_path(name: &str) -> Result<bool, Box<dyn Error>> {
    let Some(path) = env::var_os("PATH") else {
        return Ok(false);
    };
    let directories = env::split_paths(&path).take(256);
    for directory in directories {
        let candidate = directory.join(name);
        if fs::metadata(candidate).is_ok_and(|metadata| metadata.is_file()) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn print_json(value: &impl Serialize) -> Result<(), Box<dyn Error>> {
    io::stdout().write_all(&json_bytes(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_artifact(target: &str) -> ReleaseArtifact {
        let file = format!("quirl-v0.1.0-{target}.tar");
        ReleaseArtifact {
            logical_name: "quirl".to_owned(),
            target: target.to_owned(),
            file: file.clone(),
            byte_size: 1,
            sha256: "a".repeat(64),
            url: format!("https://github.com/niklas-heer/quirl/releases/download/v0.1.0/{file}"),
            provenance: format!("{file}.provenance.json"),
        }
    }

    #[test]
    fn formula_selects_all_four_native_artifacts_and_never_downloads_assets() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            product: "quirl".to_owned(),
            version: "0.1.0".to_owned(),
            tag: "v0.1.0".to_owned(),
            candidate_commit: "a".repeat(40),
            source_date_epoch: 1,
            artifacts: [
                "aarch64-apple-darwin",
                "aarch64-unknown-linux-gnu",
                "x86_64-apple-darwin",
                "x86_64-unknown-linux-gnu",
            ]
            .into_iter()
            .map(release_artifact)
            .collect(),
            asset_manifest: Some("asset-manifest-v2.json".to_owned()),
            assets: Vec::new(),
        };
        let formula = render_formula(&manifest).unwrap();
        assert!(validate_formula_shape(&formula).is_ok());
        assert_eq!(formula.matches("      sha256 \"").count(), 4);
        assert_eq!(
            formula.matches("(pkgshare/\"licenses\").install").count(),
            1
        );
        for name in [
            "LICENSE",
            "THIRD_PARTY_NOTICES.md",
            "THIRD_PARTY_LICENSES.txt",
        ] {
            assert!(formula.contains(name));
        }
        assert!(!formula.contains("completion-database"));
        assert!(!formula.contains("command-model"));
    }

    #[test]
    fn formula_inputs_reject_ruby_injection_and_mismatched_urls() {
        assert!(validate_release_version("0.1.0\"\nsystem('owned')").is_err());
        let mut artifact = release_artifact("aarch64-apple-darwin");
        assert!(validate_artifact_for_manifest("0.1.0", &artifact).is_ok());
        artifact.url.push_str(".different");
        assert!(validate_artifact_for_manifest("0.1.0", &artifact).is_err());
    }

    #[test]
    fn offline_install_accepts_only_the_exact_release_file_set() {
        let archive = crate::release::render_tar_entries(
            &[
                ("bin/quirl", b"binary", 0o755),
                ("LICENSE", b"license", 0o644),
                ("THIRD_PARTY_NOTICES.md", b"notices", 0o644),
                ("THIRD_PARTY_LICENSES.txt", b"dependency licenses", 0o644),
            ],
            1,
        )
        .unwrap();
        let package = extract_native_package(&archive).unwrap();
        assert_eq!(package.binary, b"binary");
        assert_eq!(package.product_license, b"license");
        assert_eq!(package.third_party_notices, b"notices");
        assert_eq!(package.third_party_licenses, b"dependency licenses");
        let extra = crate::release::render_tar_entries(
            &[
                ("bin/quirl", b"binary", 0o755),
                ("LICENSE", b"license", 0o644),
                ("THIRD_PARTY_NOTICES.md", b"notices", 0o644),
                ("THIRD_PARTY_LICENSES.txt", b"dependency licenses", 0o644),
                ("unexpected", b"x", 0o644),
            ],
            1,
        )
        .unwrap();
        assert!(extract_native_package(&extra).is_err());
        let executable_license = crate::release::render_tar_entries(
            &[
                ("bin/quirl", b"binary", 0o755),
                ("LICENSE", b"license", 0o755),
                ("THIRD_PARTY_NOTICES.md", b"notices", 0o644),
                ("THIRD_PARTY_LICENSES.txt", b"dependency licenses", 0o644),
            ],
            1,
        )
        .unwrap();
        assert!(extract_native_package(&executable_license).is_err());
    }
}
