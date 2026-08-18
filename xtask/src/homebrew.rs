//! Deterministic Homebrew formula generation and bounded offline validation.

use clap::Subcommand;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeMap,
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
        || manifest.asset_manifest.as_deref() != Some("asset-manifest-v1.json")
    {
        return Err(input_error(
            "release manifest is not a complete Quirl v1 release",
        ));
    }
    crate::assets::AssetManifest {
        schema_version: 1,
        release_version: manifest.version.clone(),
        candidate_commit: manifest.candidate_commit.clone(),
        source_date_epoch: manifest.source_date_epoch,
        assets: manifest.assets.clone(),
    }
    .validate_for_release(&manifest.version)?;
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
    let binary = extract_single_binary(&archive)?;
    let temporary = install_temporary_binary(&binary)?;
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

fn extract_single_binary(archive: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    if archive.len() < 1_536 || !archive.len().is_multiple_of(512) {
        return Err(input_error(
            "native package is not a complete ustar archive",
        ));
    }
    let header = &archive[..512];
    if &header[257..263] != b"ustar\0" {
        return Err(input_error("native package has no ustar header"));
    }
    let name_end = header[..100]
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(100);
    if &header[..name_end] != b"bin/quirl" {
        return Err(input_error(
            "native package does not install exactly bin/quirl",
        ));
    }
    let size_field = std::str::from_utf8(&header[124..136])?
        .trim_matches(char::from(0))
        .trim();
    let size = usize::from_str_radix(size_field, 8)
        .map_err(|_| input_error("native package has an invalid entry size"))?;
    if size == 0 || size > PACKAGE_BYTES_MAX {
        return Err(input_error(
            "native package binary size is outside its limit",
        ));
    }
    let end = 512usize
        .checked_add(size)
        .ok_or_else(|| input_error("native package entry size overflowed"))?;
    let padded_end = end
        .checked_add(511)
        .and_then(|value| value.checked_div(512))
        .and_then(|value| value.checked_mul(512))
        .ok_or_else(|| input_error("native package padding overflowed"))?;
    if padded_end.checked_add(1_024) != Some(archive.len())
        || !archive[end..].iter().all(|byte| *byte == 0)
    {
        return Err(input_error(
            "native package contains unexpected entries or trailing bytes",
        ));
    }
    Ok(archive[512..end].to_vec())
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
    output.push_str("  end\n\n");
    output.push_str("  test do\n");
    output.push_str(&format!(
        "    assert_match \"quirl {}\", shell_output(\"#{{bin}}/quirl --version\", 0)\n",
        manifest.version
    ));
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
            asset_manifest: Some("asset-manifest-v1.json".to_owned()),
            assets: Vec::new(),
        };
        let formula = render_formula(&manifest).unwrap();
        assert!(validate_formula_shape(&formula).is_ok());
        assert_eq!(formula.matches("      sha256 \"").count(), 4);
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
    fn offline_install_accepts_only_the_single_expected_binary_entry() {
        let archive =
            crate::release::render_tar_entries(&[("bin/quirl", b"binary", 0o755)], 1).unwrap();
        assert_eq!(extract_single_binary(&archive).unwrap(), b"binary");
        let extra = crate::release::render_tar_entries(
            &[("bin/quirl", b"binary", 0o755), ("unexpected", b"x", 0o644)],
            1,
        )
        .unwrap();
        assert!(extract_single_binary(&extra).is_err());
    }
}
