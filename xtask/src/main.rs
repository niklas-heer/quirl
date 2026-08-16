//! Reproducible development, documentation, test, and release tasks for Quirl.

use clap::{Parser, Subcommand};
use std::{
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};

const DEFAULT_TEST_CASES: usize = 128;
const DEFAULT_TEST_SEED: u64 = 7_640_891_576_956_012_809;

#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "Cargo-native Quirl development tasks")]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Format every crate in the workspace.
    Fmt,
    /// Run Clippy for every workspace target with warnings denied.
    Lint,
    /// Run deterministic Rust, differential, real-PTY, and guest Lua tests.
    Test {
        /// Reproducible seed for generated differential shell cases.
        #[arg(long, default_value_t = DEFAULT_TEST_SEED)]
        seed: u64,
        /// Number of generated differential cases per reference shell.
        #[arg(long, default_value_t = DEFAULT_TEST_CASES, value_parser = parse_test_cases)]
        cases: usize,
    },
    /// Run the complete pre-commit quality gate without modifying sources.
    Check {
        /// Reproducible seed for generated differential shell cases.
        #[arg(long, default_value_t = DEFAULT_TEST_SEED)]
        seed: u64,
        /// Number of generated differential cases per reference shell.
        #[arg(long, default_value_t = DEFAULT_TEST_CASES, value_parser = parse_test_cases)]
        cases: usize,
    },
    /// Build all public Rust API documentation with warnings denied.
    Docs,
    /// Regenerate the checked-in LuaLS SDK atomically.
    Sdk,
    /// Start Quirl, forwarding remaining arguments after `--`.
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<OsString>,
    },
    /// Build release Quirl and run the isolated text product tour.
    Demo,
    /// Record the prebuilt measured artifact after verifying its digest.
    DemoRecord {
        /// Independently measured SHA-256 of target/release/quirl.
        expected_sha256: String,
    },
    /// Build release artifacts and run non-enforcing measurements.
    ReleasePreview,
    /// Enforce release budgets against the already-built artifacts.
    ReleaseGate {
        /// Independently measured SHA-256 of target/release/quirl.
        expected_sha256: String,
    },
}

fn main() {
    if let Err(error) = execute(Cli::parse()) {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}

fn execute(cli: Cli) -> Result<(), Box<dyn Error>> {
    let root = workspace_root()?;
    match cli.task {
        Task::Fmt => task_fmt(&root),
        Task::Lint => task_lint(&root),
        Task::Test { seed, cases } => task_test(&root, seed, cases),
        Task::Check { seed, cases } => task_check(&root, seed, cases),
        Task::Docs => task_docs(&root),
        Task::Sdk => task_sdk(&root),
        Task::Run { arguments } => task_run(&root, &arguments),
        Task::Demo => task_demo(&root),
        Task::DemoRecord { expected_sha256 } => task_demo_record(&root, &expected_sha256),
        Task::ReleasePreview => task_release_preview(&root),
        Task::ReleaseGate { expected_sha256 } => task_release_gate(&root, &expected_sha256),
    }
}

fn workspace_root() -> Result<PathBuf, Box<dyn Error>> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other("xtask manifest has no workspace parent").into())
}

fn task_fmt(root: &Path) -> Result<(), Box<dyn Error>> {
    run(root, "cargo", ["fmt", "--all"], &[])
}

fn task_lint(root: &Path) -> Result<(), Box<dyn Error>> {
    run(
        root,
        "cargo",
        [
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        &[],
    )
}

fn task_test(root: &Path, seed: u64, cases: usize) -> Result<(), Box<dyn Error>> {
    let seed = seed.to_string();
    let cases = cases.to_string();
    let environment = [
        ("QUIRL_TEST_SEED", seed.as_str()),
        ("QUIRL_TEST_CASES", cases.as_str()),
    ];
    run(root, "cargo", ["test", "--workspace"], &environment)?;
    #[cfg(unix)]
    {
        run(root, "cargo", ["build", "-p", "quirl-cli"], &environment)?;
        run(
            root,
            "python3",
            ["scripts/check-rich-pty.py", "target/debug/quirl"],
            &environment,
        )?;
    }
    run(
        root,
        "cargo",
        [
            "run",
            "-p",
            "quirl-cli",
            "--",
            "test",
            "examples/lua_tests.lua",
        ],
        &environment,
    )
}

fn task_check(root: &Path, seed: u64, cases: usize) -> Result<(), Box<dyn Error>> {
    run(root, "cargo", ["fmt", "--all", "--", "--check"], &[])?;
    run(
        root,
        "cargo",
        [
            "run",
            "--quiet",
            "-p",
            "quirl-cli",
            "--",
            "fmt",
            "examples",
            "--check",
        ],
        &[],
    )?;
    task_lint(root)?;
    task_docs(root)?;
    task_test(root, seed, cases)
}

fn task_docs(root: &Path) -> Result<(), Box<dyn Error>> {
    run(
        root,
        "cargo",
        ["doc", "--workspace", "--no-deps"],
        &[("RUSTDOCFLAGS", "-D warnings")],
    )
}

fn task_sdk(root: &Path) -> Result<(), Box<dyn Error>> {
    let output = Command::new("cargo")
        .current_dir(root)
        .args([
            "run",
            "--quiet",
            "-p",
            "quirl-cli",
            "--",
            "sdk",
            "--format",
            "text",
        ])
        .stderr(Stdio::inherit())
        .output()?;
    ensure_success("cargo run -p quirl-cli -- sdk", output.status)?;

    let target = root.join("docs/quirl.lua");
    let temporary = root.join(format!("docs/.quirl.lua.xtask-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let install = (|| -> io::Result<()> {
        file.write_all(&output.stdout)?;
        file.sync_all()?;
        fs::rename(&temporary, &target)
    })();
    if install.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    install?;
    Ok(())
}

fn task_run(root: &Path, arguments: &[OsString]) -> Result<(), Box<dyn Error>> {
    let mut command = Command::new("cargo");
    command
        .current_dir(root)
        .args(["run", "-p", "quirl-cli", "--"]);
    command.args(arguments);
    run_command("cargo run -p quirl-cli", &mut command)
}

fn task_demo(root: &Path) -> Result<(), Box<dyn Error>> {
    run(
        root,
        "cargo",
        ["build", "--quiet", "--release", "-p", "quirl-cli"],
        &[],
    )?;
    run(
        root,
        root.join("scripts/demo.sh"),
        ["target/release/quirl"],
        &[],
    )
}

fn task_demo_record(root: &Path, expected_sha256: &str) -> Result<(), Box<dyn Error>> {
    validate_sha256(expected_sha256)?;
    run(
        root,
        root.join("scripts/record-demo.sh"),
        ["target/release/quirl", expected_sha256],
        &[],
    )
}

fn task_release_preview(root: &Path) -> Result<(), Box<dyn Error>> {
    run(
        root,
        "cargo",
        ["build", "--release", "-p", "quirl-cli", "-p", "quirl-bench"],
        &[],
    )?;
    run(
        root,
        root.join("target/release/quirl-bench"),
        ["preview", "--quirl", "target/release/quirl"],
        &[],
    )
}

fn task_release_gate(root: &Path, expected_sha256: &str) -> Result<(), Box<dyn Error>> {
    validate_sha256(expected_sha256)?;
    for artifact in ["target/release/quirl", "target/release/quirl-bench"] {
        if !root.join(artifact).is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "release artifact {artifact} is missing; run `cargo xtask release-preview`"
                ),
            )
            .into());
        }
    }
    run(
        root,
        root.join("target/release/quirl-bench"),
        [
            "release",
            "--quirl",
            "target/release/quirl",
            "--expected-sha256",
            expected_sha256,
        ],
        &[],
    )
}

fn validate_sha256(value: &str) -> Result<(), Box<dyn Error>> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "expected_sha256 must contain exactly 64 hexadecimal digits",
    )
    .into())
}

fn parse_test_cases(value: &str) -> Result<usize, String> {
    let cases = value
        .parse::<usize>()
        .map_err(|_| "test cases must be an integer".to_owned())?;
    if (1..=10_000).contains(&cases) {
        Ok(cases)
    } else {
        Err("test cases must be between 1 and 10000".to_owned())
    }
}

fn run<I, S, P>(
    root: &Path,
    program: P,
    arguments: I,
    environment: &[(&str, &str)],
) -> Result<(), Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    P: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command.current_dir(root).args(arguments);
    for (key, value) in environment {
        command.env(key, value);
    }
    let label = format!("{command:?}");
    run_command(&label, &mut command)
}

fn run_command(label: &str, command: &mut Command) -> Result<(), Box<dyn Error>> {
    let status = command.status()?;
    ensure_success(label, status)
}

fn ensure_success(label: &str, status: ExitStatus) -> Result<(), Box<dyn Error>> {
    if status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!("{label} failed with {status}")).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_digest_requires_exact_hex_shape() {
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256(&"A".repeat(64)).is_ok());
        assert!(validate_sha256(&"g".repeat(64)).is_err());
        assert!(validate_sha256(&"a".repeat(63)).is_err());
        assert!(validate_sha256(&"a".repeat(65)).is_err());
    }

    #[test]
    fn workspace_root_contains_the_workspace_manifest() {
        assert!(workspace_root().unwrap().join("Cargo.toml").is_file());
    }
}
