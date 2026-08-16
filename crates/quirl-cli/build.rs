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

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    if let Some(head) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        println!("cargo:rerun-if-changed=../../.git/{head}");
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
