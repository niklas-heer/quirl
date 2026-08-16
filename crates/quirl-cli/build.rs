use std::{env, path::PathBuf, process::Command};

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

fn main() {
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
    println!(
        "cargo:rustc-env=QUIRL_BUILD_OPT_LEVEL={}",
        env::var("OPT_LEVEL").unwrap_or_else(|_| "unknown".to_owned())
    );
}
