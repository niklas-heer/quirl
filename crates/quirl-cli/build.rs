//! Embeds reproducible build metadata in the Quirl command-line executable.

use std::{
    env, fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const WORKSPACE_CRATES_MAX: usize = 128;

fn git_output(arguments: &[&str]) -> Option<String> {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?);
    let output = Command::new("git")
        .arg("-C")
        .arg(manifest)
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn git_path(arguments: &[&str]) -> Option<PathBuf> {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR")?);
    let path = PathBuf::from(git_output(arguments)?);
    Some(if path.is_absolute() {
        path
    } else {
        manifest.join(path)
    })
}

fn watch_git_path(path: Option<PathBuf>) {
    if let Some(path) = path {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn watch_workspace_sources() {
    let Some(manifest) = env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from) else {
        return;
    };
    let Some(workspace) = manifest.parent().and_then(|path| path.parent()) else {
        return;
    };
    for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        println!("cargo:rerun-if-changed={}", workspace.join(name).display());
    }
    let Ok(entries) = fs::read_dir(workspace.join("crates")) else {
        return;
    };
    for entry in entries.flatten().take(WORKSPACE_CRATES_MAX) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        println!(
            "cargo:rerun-if-changed={}",
            path.join("Cargo.toml").display()
        );
        println!("cargo:rerun-if-changed={}", path.join("src").display());
    }
}

fn is_official_release(dirty: &str) -> bool {
    if env::var("QUIRL_OFFICIAL_RELEASE").is_ok_and(|value| value == "1") {
        return true;
    }
    if dirty != "false" {
        return false;
    }
    let Ok(version) = env::var("CARGO_PKG_VERSION") else {
        return false;
    };
    let expected_tag = format!("v{version}");
    git_output(&[
        "describe",
        "--tags",
        "--exact-match",
        "--match",
        &expected_tag,
    ])
    .is_some_and(|tag| tag == expected_tag)
}

pub(crate) fn main() {
    println!("cargo:rerun-if-env-changed=QUIRL_OFFICIAL_RELEASE");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    watch_workspace_sources();
    // Ask Git for every path instead of assuming a repository-local `.git`
    // directory. That covers linked worktrees, a `.git` gitdir file, and
    // separate common object directories.
    for name in ["HEAD", "index", "packed-refs", "shallow"] {
        watch_git_path(git_path(&["rev-parse", "--git-path", name]));
    }
    if let Some(reference) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(git_path(&["rev-parse", "--git-path", &reference]));
    }
    if let Some(common_dir) = git_path(&["rev-parse", "--git-common-dir"]) {
        println!(
            "cargo:rerun-if-changed={}",
            common_dir.join("refs").display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            common_dir.join("packed-refs").display()
        );
    }
    let commit = git_output(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let dirty = git_output(&["status", "--porcelain", "--untracked-files=normal"])
        .map(|status| if status.is_empty() { "false" } else { "true" })
        .unwrap_or("unknown");
    println!("cargo:rustc-env=QUIRL_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=QUIRL_BUILD_DIRTY={dirty}");
    let timestamp = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_secs())
        });
    let official = is_official_release(dirty);
    println!("cargo:rustc-env=QUIRL_BUILD_TIMESTAMP={timestamp}");
    println!("cargo:rustc-env=QUIRL_OFFICIAL_RELEASE={official}");
    println!(
        "cargo:rustc-env=QUIRL_BUILD_OPT_LEVEL={}",
        env::var("OPT_LEVEL").unwrap_or_else(|_| "unknown".to_owned())
    );
}
