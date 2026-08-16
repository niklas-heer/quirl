//! Reproducible development, documentation, test, and release tasks for Quirl.

use clap::{Parser, Subcommand};
mod simulation;

use std::{
    env,
    error::Error,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
};
use xshell::{cmd, Shell};

const DEFAULT_TEST_CASES: usize = 128;
const DEFAULT_TEST_SEED: u64 = 7_640_891_576_956_012_809;
const DEFAULT_SIMULATION_SESSIONS: usize = 256;
const DEFAULT_SIMULATION_STEPS: usize = 8;

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
    /// Explore replayable shell sessions against clean Bash and Zsh references.
    Simulate {
        /// Reproducible seed for the generated session swarm.
        #[arg(long, default_value_t = DEFAULT_TEST_SEED)]
        seed: u64,
        /// Number of generated sessions.
        #[arg(long, default_value_t = DEFAULT_SIMULATION_SESSIONS, value_parser = parse_test_cases)]
        sessions: usize,
        /// Maximum stateful steps in one session.
        #[arg(long, default_value_t = DEFAULT_SIMULATION_STEPS, value_parser = parse_simulation_steps)]
        steps: usize,
        /// Evaluate only this zero-based session after advancing the generator to it.
        #[arg(long, requires = "sessions")]
        session: Option<usize>,
        /// Parent directory for the seed-specific report and failure artifacts.
        #[arg(long, default_value = "target/simulations")]
        output: PathBuf,
    },
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
        Task::Simulate {
            seed,
            sessions,
            steps,
            session,
            output,
        } => task_simulate(&root, seed, sessions, steps, session, &output),
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
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo fmt --all").run()?;
    Ok(())
}

fn task_lint(root: &Path) -> Result<(), Box<dyn Error>> {
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo clippy --workspace --all-targets -- -D warnings").run()?;
    Ok(())
}

fn task_test(root: &Path, seed: u64, cases: usize) -> Result<(), Box<dyn Error>> {
    let sh = workspace_shell(root)?;
    let seed = seed.to_string();
    let cases = cases.to_string();
    cmd!(sh, "cargo test --workspace")
        .env("QUIRL_TEST_SEED", &seed)
        .env("QUIRL_TEST_CASES", &cases)
        .run()?;
    #[cfg(unix)]
    {
        cmd!(sh, "cargo build -p quirl-cli")
            .env("QUIRL_TEST_SEED", &seed)
            .env("QUIRL_TEST_CASES", &cases)
            .run()?;
        cmd!(sh, "python3 scripts/check-rich-pty.py target/debug/quirl")
            .env("QUIRL_TEST_SEED", &seed)
            .env("QUIRL_TEST_CASES", &cases)
            .run()?;
    }
    cmd!(sh, "cargo run -p quirl-cli -- test examples/lua_tests.lua")
        .env("QUIRL_TEST_SEED", &seed)
        .env("QUIRL_TEST_CASES", &cases)
        .run()?;
    Ok(())
}

fn task_check(root: &Path, seed: u64, cases: usize) -> Result<(), Box<dyn Error>> {
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo fmt --all -- --check").run()?;
    cmd!(sh, "cargo run --quiet -p quirl-cli -- fmt examples --check").run()?;
    task_lint(root)?;
    task_docs(root)?;
    task_test(root, seed, cases)
}

fn task_docs(root: &Path) -> Result<(), Box<dyn Error>> {
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo doc --workspace --no-deps")
        .env("RUSTDOCFLAGS", "-D warnings")
        .run()?;
    Ok(())
}

fn task_simulate(
    root: &Path,
    seed: u64,
    sessions: usize,
    steps: usize,
    session: Option<usize>,
    output: &Path,
) -> Result<(), Box<dyn Error>> {
    if session.is_some_and(|index| index >= sessions) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session index must be less than the session count ({sessions})"),
        )
        .into());
    }
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo build -p quirl-cli").run()?;
    let quirl = debug_quirl_binary(root);
    let summary = simulation::run(simulation::SimulationOptions {
        workspace_root: root.to_path_buf(),
        quirl,
        seed,
        session_count: sessions,
        steps_max: steps,
        only_session: session,
        output_root: if output.is_absolute() {
            output.to_path_buf()
        } else {
            root.join(output)
        },
    })?;
    println!(
        "simulation: seed={} evaluated={} mismatches={} artifacts={}",
        summary.seed,
        summary.sessions_evaluated,
        summary.mismatch_count,
        summary.run_directory.display()
    );
    if summary.mismatch_count > 0 {
        return Err(io::Error::other(format!(
            "{} compatibility mismatch(es); inspect {}",
            summary.mismatch_count,
            summary.run_directory.display()
        ))
        .into());
    }
    Ok(())
}

fn debug_quirl_binary(root: &Path) -> PathBuf {
    let target = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        })
        .unwrap_or_else(|| root.join("target"));
    target
        .join("debug")
        .join(format!("quirl{}", env::consts::EXE_SUFFIX))
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
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo run -p quirl-cli -- {arguments...}").run()?;
    Ok(())
}

fn task_demo(root: &Path) -> Result<(), Box<dyn Error>> {
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo build --quiet --release -p quirl-cli").run()?;
    cmd!(sh, "scripts/demo.sh target/release/quirl").run()?;
    Ok(())
}

fn task_demo_record(root: &Path, expected_sha256: &str) -> Result<(), Box<dyn Error>> {
    validate_sha256(expected_sha256)?;
    let sh = workspace_shell(root)?;
    cmd!(
        sh,
        "scripts/record-demo.sh target/release/quirl {expected_sha256}"
    )
    .run()?;
    Ok(())
}

fn task_release_preview(root: &Path) -> Result<(), Box<dyn Error>> {
    let sh = workspace_shell(root)?;
    cmd!(sh, "cargo build --release -p quirl-cli -p quirl-bench").run()?;
    cmd!(
        sh,
        "target/release/quirl-bench preview --quirl target/release/quirl"
    )
    .run()?;
    Ok(())
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
    let sh = workspace_shell(root)?;
    cmd!(
        sh,
        "target/release/quirl-bench release --quirl target/release/quirl --expected-sha256 {expected_sha256}"
    )
    .run()?;
    Ok(())
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

fn parse_simulation_steps(value: &str) -> Result<usize, String> {
    let steps = value
        .parse::<usize>()
        .map_err(|_| "simulation steps must be an integer".to_owned())?;
    if (3..=32).contains(&steps) {
        Ok(steps)
    } else {
        Err("simulation steps must be between 3 and 32".to_owned())
    }
}

fn workspace_shell(root: &Path) -> Result<Shell, Box<dyn Error>> {
    let sh = Shell::new()?;
    sh.change_dir(root);
    Ok(sh)
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

    #[test]
    fn simulation_steps_are_bounded() {
        assert!(parse_simulation_steps("3").is_ok());
        assert!(parse_simulation_steps("32").is_ok());
        assert!(parse_simulation_steps("2").is_err());
        assert!(parse_simulation_steps("33").is_err());
    }

    #[test]
    fn simulation_session_must_be_inside_the_generated_swarm() {
        let cli = Cli::try_parse_from([
            "cargo xtask",
            "simulate",
            "--sessions",
            "8",
            "--session",
            "7",
        ]);
        assert!(cli.is_ok());
    }
}
