//! Quirl's persistent, in-process Steel host.

use quirl_core::{CommandRunner, ErrorCode, ShellError};
use std::{fs, path::Path};
use steel::{
    rvals::SteelVal,
    steel_vm::{engine::Engine, register_fn::RegisterFn},
};

/// A long-lived Steel VM. Interactive data mode reuses this instance.
pub struct SteelRuntime {
    engine: Engine,
}

impl Default for SteelRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl SteelRuntime {
    pub fn new() -> Self {
        let mut engine = Engine::new();
        engine.register_fn("quirl-version", quirl_version);
        engine.register_fn("quirl-cwd", quirl_cwd);
        engine.register_fn("quirl-command", quirl_command);
        Self { engine }
    }

    pub fn eval(&mut self, source: &str) -> Result<Vec<String>, ShellError> {
        self.engine
            .run(source.to_owned())
            .map(|values| {
                values
                    .into_iter()
                    .filter(|value| !matches!(value, SteelVal::Void))
                    .map(|value| value.to_string())
                    .collect()
            })
            .map_err(|error| steel_error(error.to_string(), None, source.len()))
    }

    pub fn run_file(&mut self, path: &Path) -> Result<Vec<String>, ShellError> {
        let source = fs::read_to_string(path).map_err(|error| {
            ShellError::new(
                ErrorCode::ScriptRead,
                format!("cannot read script {}", path.display()),
            )
            .with_context(error.to_string())
        })?;
        let source = strip_shebang(&source);
        self.engine
            .compile_and_run_raw_program_with_path(source.to_owned(), path.to_path_buf())
            .map(|values| {
                values
                    .into_iter()
                    .filter(|value| !matches!(value, SteelVal::Void))
                    .map(|value| value.to_string())
                    .collect()
            })
            .map_err(|error| {
                steel_error(
                    error.to_string(),
                    Some(path.display().to_string()),
                    source.len(),
                )
            })
    }

    pub fn check_file(path: &Path) -> Result<(), ShellError> {
        let source = fs::read_to_string(path).map_err(|error| {
            ShellError::new(
                ErrorCode::ScriptRead,
                format!("cannot read script {}", path.display()),
            )
            .with_context(error.to_string())
        })?;
        let source = strip_shebang(&source);
        Engine::emit_ast(source).map(|_| ()).map_err(|error| {
            steel_error(
                error.to_string(),
                Some(path.display().to_string()),
                source.len(),
            )
        })
    }
}

fn strip_shebang(source: &str) -> &str {
    if source.starts_with("#!") {
        source.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        source
    }
}

fn quirl_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

fn quirl_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn quirl_command(input: String) -> Result<String, String> {
    let outcome = CommandRunner::default()
        .execute_capture(&input)
        .map_err(|error| error.to_string())?;
    if outcome.status == 0 {
        Ok(outcome.stdout.unwrap_or_default())
    } else {
        Err(outcome
            .stderr
            .unwrap_or_else(|| format!("command exited with status {}", outcome.status)))
    }
}

fn steel_error(message: String, source: Option<String>, source_len: usize) -> ShellError {
    ShellError::new(ErrorCode::Steel, "Steel could not evaluate the program")
        .with_context(message)
        .with_help("Run `quirl check <file> --format json` for a machine-readable diagnostic")
        .with_label(source, 0, source_len, "invalid Steel program")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_values_in_a_persistent_vm() {
        let mut runtime = SteelRuntime::new();
        assert_eq!(
            runtime.eval("(define answer 40)").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(runtime.eval("(+ answer 2)").unwrap(), vec!["42"]);
    }

    #[test]
    fn exposes_a_real_rust_host_function() {
        let mut runtime = SteelRuntime::new();
        assert_eq!(
            runtime.eval("(quirl-command \"printf quirl\")").unwrap(),
            vec!["\"quirl\""]
        );
    }
}
