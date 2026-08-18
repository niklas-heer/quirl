//! Bounded local-shell completion process boundary.
//!
//! Callers admit an exact shell executable, completion roots, scripts, and
//! environment. The boundary starts no user startup files, contains the shell
//! process group, enforces resource limits, and decodes only length-framed
//! candidate records. See `LOCAL_COMPLETION_DESIGN.md` and
//! `THIRD_PARTY_NOTICES.md` in this crate for the failure model and provenance.

use quirl_core::{ErrorCode, ShellError};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const FRAME_MAGIC_BYTES: &[u8; 4] = b"QLB1";
const FRAME_MAGIC_SCALARS: &[u8; 4] = b"QLU1";
const FRAME_HEADER_BYTES: usize = 16;
const IO_CHUNK_BYTES: usize = 8 * 1024;
const IO_READS_PER_TURN_MAX: usize = 16;
const POLL_INTERVAL: Duration = Duration::from_millis(1);
const PROVIDER_UNAVAILABLE_STATUS: i32 = 78;

/// Maximum wall time accepted for one local completion request.
pub const LOCAL_COMPLETION_DEADLINE_MAX: Duration = Duration::from_secs(30);
/// Hard ceiling for total stdout and stderr bytes observed from one provider.
pub const LOCAL_COMPLETION_OUTPUT_BYTES_HARD_MAX: usize = 16 * 1024 * 1024;
/// Hard ceiling for framed records decoded from one provider response.
pub const LOCAL_COMPLETION_RECORDS_HARD_MAX: usize = 65_536;
/// Hard ceiling for one request or result field in bytes.
pub const LOCAL_COMPLETION_FIELD_BYTES_HARD_MAX: usize = 1024 * 1024;
/// Hard ceiling for retained completion candidates.
pub const LOCAL_COMPLETION_CANDIDATES_HARD_MAX: usize = 65_536;
/// Hard ceiling for nested command-path components.
pub const LOCAL_COMPLETION_PATH_DEPTH_HARD_MAX: usize = 64;
/// Hard ceiling for command arguments, including the current word.
pub const LOCAL_COMPLETION_ARGUMENTS_HARD_MAX: usize = 4_096;
/// Hard ceiling for explicitly admitted completion roots.
pub const LOCAL_COMPLETION_ROOTS_HARD_MAX: usize = 256;
/// Hard ceiling for explicitly admitted completion scripts.
pub const LOCAL_COMPLETION_SCRIPTS_HARD_MAX: usize = 256;
/// Hard ceiling for explicitly admitted environment variables.
pub const LOCAL_COMPLETION_ENVIRONMENT_VARIABLES_HARD_MAX: usize = 4_096;
/// Hard ceiling for retained environment key and value bytes.
pub const LOCAL_COMPLETION_ENVIRONMENT_BYTES_HARD_MAX: usize = 8 * 1024 * 1024;
/// Hard ceiling for aggregate request path and command bytes.
pub const LOCAL_COMPLETION_INPUT_BYTES_HARD_MAX: usize = 8 * 1024 * 1024;
/// Hard ceiling for concurrent provider processes owned by one boundary.
pub const LOCAL_COMPLETION_SLOTS_HARD_MAX: usize = 256;

/// Local shell provider used to resolve completion candidates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalCompletionProvider {
    /// Zsh completion through an interactive `zsh/zpty` and `compadd` capture.
    Zsh,
    /// Fish completion through `complete --do-complete`.
    Fish,
}

/// Caller-selected limits for one local completion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalCompletionLimits {
    /// Aggregate stdout and stderr bytes permitted from the provider.
    pub output_bytes_max: usize,
    /// Maximum framed records accepted before decoding stops.
    pub record_count_max: usize,
    /// Maximum UTF-8 bytes in one request or result field.
    pub field_bytes_max: usize,
    /// Maximum retained candidates returned to the caller.
    pub candidate_count_max: usize,
    /// Maximum nested components in [`LocalCompletionRequest::command_path`].
    pub path_depth_max: usize,
    /// Maximum trailing arguments, including the word being completed.
    pub argument_count_max: usize,
    /// Maximum explicitly admitted completion roots.
    pub completion_root_count_max: usize,
    /// Maximum explicitly admitted completion scripts.
    pub completion_script_count_max: usize,
    /// Maximum explicitly admitted environment variables.
    pub environment_variable_count_max: usize,
    /// Aggregate environment key and value bytes permitted.
    pub environment_bytes_max: usize,
    /// Aggregate command, path, root, and script bytes permitted.
    pub input_bytes_max: usize,
}

impl Default for LocalCompletionLimits {
    fn default() -> Self {
        Self {
            output_bytes_max: 1024 * 1024,
            record_count_max: 4_096,
            field_bytes_max: 64 * 1024,
            candidate_count_max: 4_096,
            path_depth_max: 16,
            argument_count_max: 256,
            completion_root_count_max: 64,
            completion_script_count_max: 64,
            environment_variable_count_max: 256,
            environment_bytes_max: 1024 * 1024,
            input_bytes_max: 1024 * 1024,
        }
    }
}

/// Fully explicit request for local shell completion.
#[derive(Clone)]
pub struct LocalCompletionRequest {
    /// Provider whose native completion registrations should be queried.
    pub provider: LocalCompletionProvider,
    /// Absolute path to the admitted shell executable.
    pub shell_path: PathBuf,
    /// Nested command path, beginning with the executable name.
    ///
    /// Every component becomes a distinct shell word. The last component may
    /// be a nested subcommand; it is not interpreted as shell source.
    pub command_path: Vec<String>,
    /// Arguments after the command path, including the current partial word.
    /// An empty final string represents completion after a space.
    pub arguments: Vec<String>,
    /// Search roots admitted for this request. The adapters replace their
    /// native completion/function paths with exactly these roots.
    pub completion_roots: Vec<PathBuf>,
    /// Completion scripts explicitly admitted for sourcing by this request.
    pub completion_scripts: Vec<PathBuf>,
    /// Exact child environment before fixed locale/protocol variables are set.
    /// The host environment is never inherited.
    pub environment: Vec<(String, String)>,
    /// Remaining wall-time budget, capped by [`LOCAL_COMPLETION_DEADLINE_MAX`].
    pub deadline: Duration,
    /// Shared cancellation flag observed before spawn and during polling.
    pub cancelled: Arc<AtomicBool>,
    /// Request-specific resource limits, each subject to a hard ceiling.
    pub limits: LocalCompletionLimits,
}

/// One completion candidate returned by a local provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalCompletionCandidate {
    /// Exact UTF-8 candidate text emitted by the provider adapter.
    pub value: String,
    /// Optional provider description; empty descriptions become `None`.
    pub description: Option<String>,
}

/// Successfully decoded local completion response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalCompletionResult {
    /// Provider that produced the candidates.
    pub provider: LocalCompletionProvider,
    /// Candidates in provider order.
    pub candidates: Vec<LocalCompletionCandidate>,
}

/// Stable reason why a local provider could not serve a valid request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalCompletionUnavailableReason {
    /// The requested shell executable does not exist or is not a regular file.
    MissingShell,
    /// The shell has no registered completion provider for the command.
    MissingProvider,
    /// The current platform cannot provide the required containment contract.
    UnsupportedPlatform,
}

/// Typed unavailable outcome that permits composition roots to try fallbacks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalCompletionUnavailable {
    /// Provider that was requested.
    pub provider: LocalCompletionProvider,
    /// Machine-readable unavailability reason.
    pub reason: LocalCompletionUnavailableReason,
}

/// Result of a bounded local completion attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalCompletionOutcome {
    /// The provider ran and returned a valid (possibly empty) candidate list.
    Completed(LocalCompletionResult),
    /// The request was valid but this provider is not available.
    Unavailable(LocalCompletionUnavailable),
}

/// Cloneable completion boundary with a shared concurrent-process slot limit.
pub struct LocalCompletionProcess {
    slots_max: usize,
    active_slots: Arc<AtomicUsize>,
}

impl Clone for LocalCompletionProcess {
    fn clone(&self) -> Self {
        Self {
            slots_max: self.slots_max,
            active_slots: Arc::clone(&self.active_slots),
        }
    }
}

impl LocalCompletionProcess {
    /// Construct a process boundary that permits at most `slots_max` active calls.
    ///
    /// Zero or values above [`LOCAL_COMPLETION_SLOTS_HARD_MAX`] return
    /// [`ErrorCode::InvalidArgument`] before any process is created.
    pub fn new(slots_max: usize) -> Result<Self, ShellError> {
        if !(1..=LOCAL_COMPLETION_SLOTS_HARD_MAX).contains(&slots_max) {
            return Err(invalid_limit_error(
                "concurrent completion slots",
                slots_max,
                LOCAL_COMPLETION_SLOTS_HARD_MAX,
            ));
        }
        Ok(Self {
            slots_max,
            active_slots: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Execute one request within its limits and this boundary's shared slots.
    ///
    /// All effectful failures are returned as [`ShellError`]. Missing providers
    /// and unsupported platforms are represented by [`LocalCompletionOutcome`]
    /// so callers can select another completion source without parsing errors.
    pub fn complete(
        &self,
        request: LocalCompletionRequest,
    ) -> Result<LocalCompletionOutcome, ShellError> {
        validate_request(&request)?;
        let _slot = ActiveSlot::acquire(&self.active_slots, self.slots_max)?;
        if request.cancelled.load(Ordering::Relaxed) {
            return Err(cancelled_error());
        }
        run_provider(&request)
    }
}

struct ActiveSlot {
    active_slots: Arc<AtomicUsize>,
}

impl ActiveSlot {
    fn acquire(active_slots: &Arc<AtomicUsize>, slots_max: usize) -> Result<Self, ShellError> {
        let mut observed = active_slots.load(Ordering::Relaxed);
        loop {
            if observed >= slots_max {
                return Err(ShellError::new(
                    ErrorCode::ResourceLimit,
                    "local completion process slots are exhausted",
                )
                .with_context(format!(
                    "limit {slots_max} active processes; observed {observed}"
                ))
                .with_help("Wait for an active completion request to finish before retrying"));
            }
            match active_slots.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Ok(Self {
                        active_slots: Arc::clone(active_slots),
                    });
                }
                Err(actual) => observed = actual,
            }
        }
    }
}

impl Drop for ActiveSlot {
    fn drop(&mut self) {
        let previous = self.active_slots.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "completion slot count underflowed");
    }
}

fn validate_request(request: &LocalCompletionRequest) -> Result<(), ShellError> {
    validate_limits(request.limits)?;
    if request.deadline.is_zero() || request.deadline > LOCAL_COMPLETION_DEADLINE_MAX {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "local completion deadline is outside its supported range",
        )
        .with_context(format!(
            "deadline {} ms; maximum {} ms",
            request.deadline.as_millis(),
            LOCAL_COMPLETION_DEADLINE_MAX.as_millis()
        ))
        .with_help("Use a nonzero local completion deadline no longer than 30 seconds"));
    }
    if !request.shell_path.is_absolute() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "local completion shell path must be absolute",
        )
        .with_context(request.shell_path.display().to_string())
        .with_help("Resolve and admit one exact Zsh or Fish executable path"));
    }
    validate_request_counts(request)?;
    validate_request_fields(request)
}

fn validate_request_counts(request: &LocalCompletionRequest) -> Result<(), ShellError> {
    let limits = request.limits;
    validate_count(
        "command path depth",
        request.command_path.len(),
        limits.path_depth_max,
        true,
    )?;
    validate_count(
        "completion argument count",
        request.arguments.len(),
        limits.argument_count_max,
        false,
    )?;
    validate_count(
        "completion root count",
        request.completion_roots.len(),
        limits.completion_root_count_max,
        false,
    )?;
    validate_count(
        "completion script count",
        request.completion_scripts.len(),
        limits.completion_script_count_max,
        false,
    )?;
    validate_count(
        "completion environment variable count",
        request.environment.len(),
        limits.environment_variable_count_max,
        false,
    )
}

fn validate_request_fields(request: &LocalCompletionRequest) -> Result<(), ShellError> {
    let mut input_bytes = path_bytes_len(&request.shell_path)?;
    for field in request.command_path.iter().chain(&request.arguments) {
        validate_text_field(
            "completion command field",
            field,
            request.limits.field_bytes_max,
        )?;
        input_bytes = input_bytes.saturating_add(field.len());
    }
    for root in &request.completion_roots {
        input_bytes = input_bytes.saturating_add(validate_path_field(
            "completion root",
            root,
            request.limits.field_bytes_max,
        )?);
    }
    for script in &request.completion_scripts {
        input_bytes = input_bytes.saturating_add(validate_path_field(
            "completion script",
            script,
            request.limits.field_bytes_max,
        )?);
    }
    validate_environment(request, &mut input_bytes)?;
    if input_bytes > request.limits.input_bytes_max {
        return Err(resource_limit_error(
            "local completion input exceeds its aggregate byte limit",
            request.limits.input_bytes_max,
            input_bytes,
            "Shorten the command, paths, scripts, or environment",
        ));
    }
    Ok(())
}

fn validate_environment(
    request: &LocalCompletionRequest,
    input_bytes: &mut usize,
) -> Result<(), ShellError> {
    let mut environment_bytes = 0_usize;
    for (name, value) in &request.environment {
        validate_environment_name(name)?;
        validate_text_field(
            "completion environment value",
            value,
            request.limits.field_bytes_max,
        )?;
        environment_bytes = environment_bytes
            .saturating_add(name.len())
            .saturating_add(value.len());
    }
    if environment_bytes > request.limits.environment_bytes_max {
        return Err(resource_limit_error(
            "local completion environment exceeds its byte limit",
            request.limits.environment_bytes_max,
            environment_bytes,
            "Remove or shorten admitted environment variables",
        ));
    }
    *input_bytes = input_bytes.saturating_add(environment_bytes);
    Ok(())
}

fn validate_environment_name(name: &str) -> Result<(), ShellError> {
    let mut characters = name.chars();
    let valid = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if !valid {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("invalid local completion environment name `{name}`"),
        )
        .with_help("Use ASCII letters, digits, and underscores in environment names"));
    }
    Ok(())
}

fn validate_text_field(context: &str, field: &str, bytes_max: usize) -> Result<(), ShellError> {
    if field.contains('\0') {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{context} contains an interior NUL byte"),
        )
        .with_help("Remove the NUL byte before requesting local completion"));
    }
    if field.len() > bytes_max {
        return Err(resource_limit_error(
            &format!("{context} exceeds its byte limit"),
            bytes_max,
            field.len(),
            "Shorten the completion request field",
        ));
    }
    Ok(())
}

fn validate_path_field(context: &str, path: &Path, bytes_max: usize) -> Result<usize, ShellError> {
    if !path.is_absolute() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{context} path must be absolute"),
        )
        .with_context(path.display().to_string())
        .with_help("Resolve and explicitly admit an absolute completion path"));
    }
    let bytes = path_bytes_len(path)?;
    if bytes > bytes_max {
        return Err(resource_limit_error(
            &format!("{context} path exceeds its byte limit"),
            bytes_max,
            bytes,
            "Use a shorter admitted completion path",
        ));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn path_bytes_len(path: &Path) -> Result<usize, ShellError> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    if bytes.contains(&0) {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            "local completion path contains an interior NUL byte",
        )
        .with_help("Use a path without NUL bytes"));
    }
    Ok(bytes.len())
}

#[cfg(not(unix))]
fn path_bytes_len(path: &Path) -> Result<usize, ShellError> {
    path.to_str().map(str::len).ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "local completion path is not valid Unicode on this platform",
        )
        .with_help("Use a Unicode absolute path for the unavailable-platform check")
    })
}

fn validate_limits(limits: LocalCompletionLimits) -> Result<(), ShellError> {
    let checks = [
        (
            "provider output bytes",
            limits.output_bytes_max,
            LOCAL_COMPLETION_OUTPUT_BYTES_HARD_MAX,
        ),
        (
            "provider record count",
            limits.record_count_max,
            LOCAL_COMPLETION_RECORDS_HARD_MAX,
        ),
        (
            "completion field bytes",
            limits.field_bytes_max,
            LOCAL_COMPLETION_FIELD_BYTES_HARD_MAX,
        ),
        (
            "completion candidates",
            limits.candidate_count_max,
            LOCAL_COMPLETION_CANDIDATES_HARD_MAX,
        ),
        (
            "command path depth",
            limits.path_depth_max,
            LOCAL_COMPLETION_PATH_DEPTH_HARD_MAX,
        ),
        (
            "completion arguments",
            limits.argument_count_max,
            LOCAL_COMPLETION_ARGUMENTS_HARD_MAX,
        ),
        (
            "completion roots",
            limits.completion_root_count_max,
            LOCAL_COMPLETION_ROOTS_HARD_MAX,
        ),
        (
            "completion scripts",
            limits.completion_script_count_max,
            LOCAL_COMPLETION_SCRIPTS_HARD_MAX,
        ),
        (
            "environment variables",
            limits.environment_variable_count_max,
            LOCAL_COMPLETION_ENVIRONMENT_VARIABLES_HARD_MAX,
        ),
        (
            "environment bytes",
            limits.environment_bytes_max,
            LOCAL_COMPLETION_ENVIRONMENT_BYTES_HARD_MAX,
        ),
        (
            "request input bytes",
            limits.input_bytes_max,
            LOCAL_COMPLETION_INPUT_BYTES_HARD_MAX,
        ),
    ];
    for (name, value, hard_max) in checks {
        if value == 0 || value > hard_max {
            return Err(invalid_limit_error(name, value, hard_max));
        }
    }
    Ok(())
}

fn validate_count(
    name: &str,
    observed: usize,
    limit: usize,
    require_nonempty: bool,
) -> Result<(), ShellError> {
    if require_nonempty && observed == 0 {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("{name} must not be empty"),
        )
        .with_help("Provide the command name before requesting completion"));
    }
    if observed > limit {
        return Err(resource_limit_error(
            &format!("{name} exceeds its limit"),
            limit,
            observed,
            "Reduce the local completion request size",
        ));
    }
    Ok(())
}

fn invalid_limit_error(name: &str, observed: usize, hard_max: usize) -> ShellError {
    ShellError::new(ErrorCode::InvalidArgument, format!("invalid {name} limit"))
        .with_context(format!(
            "requested {observed}; supported range 1..={hard_max}"
        ))
        .with_help("Choose a nonzero limit within the documented hard ceiling")
}

fn resource_limit_error(message: &str, limit: usize, observed: usize, help: &str) -> ShellError {
    ShellError::new(ErrorCode::ResourceLimit, message)
        .with_context(format!("limit {limit}; observed {observed}"))
        .with_help(help)
}

fn cancelled_error() -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "local completion request was cancelled",
    )
    .with_help("Retry completion if the command line is still current")
}

fn deadline_error(deadline: Duration) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "local completion request exceeded its deadline",
    )
    .with_context(format!("deadline {} ms", deadline.as_millis()))
    .with_help("Use a faster completion provider or increase the bounded deadline")
}

#[cfg(not(unix))]
fn run_provider(request: &LocalCompletionRequest) -> Result<LocalCompletionOutcome, ShellError> {
    Ok(LocalCompletionOutcome::Unavailable(
        LocalCompletionUnavailable {
            provider: request.provider,
            reason: LocalCompletionUnavailableReason::UnsupportedPlatform,
        },
    ))
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::ContainedChild;
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::{
        fs::{self, File, OpenOptions},
        io::{ErrorKind, Read, Write},
        os::{fd::AsFd, unix::fs::OpenOptionsExt},
        process::{ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
        sync::atomic::AtomicU64,
        time::Instant,
    };

    static ADAPTER_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    const ZSH_OUTER_ADAPTER: &str = r#"
emulate -L zsh
setopt no_rcs no_global_rcs
zmodload zsh/zpty || exit 70
exec 3>&1
exec 1>/dev/null
local admitted_shell=$1
local inner_adapter=$2
shift 2
local root_count=$1
local script_count=$2
local token_count=$3
zpty quirl_completion "$admitted_shell" -dfi || exit 70
local -a quoted_arguments
quoted_arguments=( "${(@q)@}" )
local initialization="source ${(q)inner_adapter} ${(j: :)quoted_arguments}"
zpty -w quirl_completion "$initialization" || exit 70
local line
local ready=0
repeat 256; do
    zpty -r quirl_completion line || break
    [[ $line == *QUIRL_PROVIDER_UNAVAILABLE* ]] && exit 78
    if [[ $line == *QUIRL_COMPLETION_READY* ]]; then
        ready=1
        break
    fi
done
(( ready )) || exit 70
shift 3
shift $(( root_count + script_count ))
local -a completion_tokens quoted_tokens
completion_tokens=( "${@[1,token_count]}" )
quoted_tokens=( "${(@q)completion_tokens}" )
local completion_line="${(j: :)quoted_tokens}"
zpty -w quirl_completion "$completion_line"$'\t' || exit 70
while zpty -r quirl_completion line; do :; done
exit 0
"#;

    const ZSH_INNER_ADAPTER: &str = r#"
emulate -L zsh
setopt no_rcs no_global_rcs
PROMPT=
RPROMPT=
local root_count=$1
local script_count=$2
local token_count=$3
shift 3
local -a admitted_roots admitted_scripts completion_tokens
admitted_roots=( "${@[1,root_count]}" )
shift root_count
admitted_scripts=( "${@[1,script_count]}" )
shift script_count
completion_tokens=( "${@[1,token_count]}" )
fpath=( "${admitted_roots[@]}" )
autoload -Uz compinit || exit 70
compinit -D || exit 70
local admitted_script
for admitted_script in "${admitted_scripts[@]}"; do
    source "$admitted_script" || exit 70
done
if (( ! ${+_comps[${completion_tokens[1]}]} )); then
    print -r -- QUIRL_PROVIDER_UNAVAILABLE
    exit 78
fi
zmodload zsh/zutil || exit 70
compadd () {
    if [[ ${@[1,(i)(-|--)]} == *-(O|A|D)\ * ]]; then
        builtin compadd "$@"
        return $?
    fi
    typeset -a quirl_hits quirl_descriptions
    local quirl_description_reference
    if (( $@[(I)-d] )); then
        quirl_description_reference=${@[$(( ${@[(i)-d]} + 1 ))]}
        if [[ $quirl_description_reference != \(* ]]; then
            quirl_descriptions=( "${(@P)quirl_description_reference}" )
        fi
    fi
    builtin compadd -A quirl_hits -D quirl_descriptions "$@"
    setopt localoptions norcexpandparam extendedglob
    typeset -A quirl_apre quirl_hpre quirl_hsuf quirl_asuf
    zparseopts -E P:=quirl_apre p:=quirl_hpre S:=quirl_asuf s:=quirl_hsuf
    local quirl_candidate quirl_description quirl_hit
    integer quirl_index
    for quirl_index in {1..$#quirl_hits}; do
        quirl_hit=$quirl_hits[$quirl_index]
        quirl_candidate=$IPREFIX$quirl_apre$quirl_hpre$quirl_hit$quirl_hsuf$quirl_asuf
        quirl_description=${quirl_descriptions[$quirl_index]-}
        quirl_description=${${quirl_description}##$quirl_hit #}
        printf '%08x%08x' ${#quirl_candidate} ${#quirl_description} >&3
        print -rn -u 3 -- "$quirl_candidate$quirl_description"
    done
}
comppostfuncs=( exit )
bindkey '^M' undefined
bindkey '^J' undefined
bindkey '^I' complete-word
zstyle ':completion:*' list-grouped false
zstyle ':completion:*' insert-tab false
zstyle ':completion:*' list-separator ''
print -rn -u 3 -- QLB1
print -r -- QUIRL_COMPLETION_READY
"#;

    const FISH_ADAPTER: &str = r#"
set -l root_count $argv[1]
set -l script_count $argv[2]
set -l token_count $argv[3]
set -l cursor 4
set -l admitted_roots
set -l remaining $root_count
while test $remaining -gt 0
    set -a admitted_roots $argv[$cursor]
    set cursor (math $cursor + 1)
    set remaining (math $remaining - 1)
end
set -l admitted_scripts
set remaining $script_count
while test $remaining -gt 0
    set -a admitted_scripts $argv[$cursor]
    set cursor (math $cursor + 1)
    set remaining (math $remaining - 1)
end
set -l completion_tokens
set remaining $token_count
while test $remaining -gt 0
    set -a completion_tokens $argv[$cursor]
    set cursor (math $cursor + 1)
    set remaining (math $remaining - 1)
end
set -g fish_complete_path $admitted_roots
set -g fish_function_path $admitted_roots
for admitted_script in $admitted_scripts
    source $admitted_script; or exit 70
end
set -l quoted_tokens
for token in $completion_tokens
    set -a quoted_tokens (string escape -- $token)
end
set -l completion_line (string join ' ' -- $quoted_tokens)
printf QLU1
complete --do-complete --escape "$completion_line" | while read -l record
    set -l fields (string split -m 1 \t -- $record)
    set -l candidate $fields[1]
    set -l description
    if test (count $fields) -gt 1
        set description $fields[2]
    end
    printf '%08x%08x' (string length -- $candidate) (string length -- $description)
    printf '%s%s' $candidate $description
end
set -l provider_definition
complete -c $completion_tokens[1] | read -l provider_definition
if test -z "$provider_definition"
    exit 78
end
"#;

    pub(super) fn run_provider(
        request: &LocalCompletionRequest,
    ) -> Result<LocalCompletionOutcome, ShellError> {
        if !shell_is_available(&request.shell_path)? {
            return Ok(unavailable(
                request.provider,
                LocalCompletionUnavailableReason::MissingShell,
            ));
        }
        validate_admitted_filesystem(request)?;
        let adapter = match request.provider {
            LocalCompletionProvider::Zsh => Some(TemporaryAdapter::create(ZSH_INNER_ADAPTER)?),
            LocalCompletionProvider::Fish => None,
        };
        let result = run_admitted_provider(request, adapter.as_ref().map(TemporaryAdapter::path));
        finish_adapter_cleanup(adapter, result)
    }

    fn run_admitted_provider(
        request: &LocalCompletionRequest,
        zsh_adapter_path: Option<&Path>,
    ) -> Result<LocalCompletionOutcome, ShellError> {
        let deadline = Instant::now()
            .checked_add(request.deadline)
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    "local completion deadline is outside the platform range",
                )
                .with_help("Use a shorter local completion deadline")
            })?;
        let mut command = provider_command(request, zsh_adapter_path)?;
        let mut child = ContainedChild::spawn(&mut command)?;
        let mut stdout = take_stdout(&mut child)?;
        let mut stderr = take_stderr(&mut child)?;
        set_nonblocking(&stdout, "stdout")?;
        set_nonblocking(&stderr, "stderr")?;
        let execution = poll_provider(request, deadline, &mut child, &mut stdout, &mut stderr);
        execution.and_then(|(status, output)| finish_provider(request, status, output))
    }

    fn shell_is_available(path: &Path) -> Result<bool, ShellError> {
        match fs::metadata(path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(ShellError::new(
                ErrorCode::Io,
                "could not inspect local completion shell",
            )
            .with_context(format!("{}: {error}", path.display()))
            .with_help("Check access to the admitted shell executable")),
        }
    }

    fn validate_admitted_filesystem(request: &LocalCompletionRequest) -> Result<(), ShellError> {
        for root in &request.completion_roots {
            let metadata =
                fs::metadata(root).map_err(|error| admitted_path_error("root", root, error))?;
            if !metadata.is_dir() {
                return Err(admitted_path_kind_error("root", root, "directory"));
            }
        }
        for script in &request.completion_scripts {
            let metadata = fs::metadata(script)
                .map_err(|error| admitted_path_error("script", script, error))?;
            if !metadata.is_file() {
                return Err(admitted_path_kind_error("script", script, "regular file"));
            }
        }
        Ok(())
    }

    fn admitted_path_error(kind: &str, path: &Path, error: std::io::Error) -> ShellError {
        ShellError::new(
            ErrorCode::Io,
            format!("could not inspect admitted completion {kind}"),
        )
        .with_context(format!("{}: {error}", path.display()))
        .with_help("Update or remove the unavailable admitted completion path")
    }

    fn admitted_path_kind_error(kind: &str, path: &Path, expected: &str) -> ShellError {
        ShellError::new(
            ErrorCode::InvalidArgument,
            format!("admitted completion {kind} is not a {expected}"),
        )
        .with_context(path.display().to_string())
        .with_help("Admit only completion directories and regular script files")
    }

    pub(super) struct TemporaryAdapter {
        path: PathBuf,
        _file: File,
        removed: bool,
    }

    impl TemporaryAdapter {
        pub(super) fn create(source: &str) -> Result<Self, ShellError> {
            const ATTEMPTS_MAX: usize = 32;
            const ADAPTER_PATH_BYTES_MAX: usize = 4096;
            let directory = std::env::temp_dir();
            let directory_bytes = path_bytes_len(&directory)?;
            if directory_bytes > ADAPTER_PATH_BYTES_MAX {
                return Err(resource_limit_error(
                    "local completion temporary path exceeds its byte limit",
                    ADAPTER_PATH_BYTES_MAX,
                    directory_bytes,
                    "Configure a shorter host temporary-directory path",
                ));
            }
            for _ in 0..ATTEMPTS_MAX {
                let sequence = ADAPTER_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = directory.join(format!(
                    "quirl-completion-adapter-{}-{sequence}.zsh",
                    std::process::id()
                ));
                match create_adapter_file(&path, source) {
                    Ok(file) => {
                        return Ok(Self {
                            path,
                            _file: file,
                            removed: false,
                        });
                    }
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(adapter_file_error(&path, error)),
                }
            }
            Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "local completion adapter file attempts are exhausted",
            )
            .with_context(format!("limit {ATTEMPTS_MAX} create attempts"))
            .with_help("Remove stale Quirl adapter files from the temporary directory"))
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        fn remove(mut self) -> Result<(), ShellError> {
            match fs::remove_file(&self.path) {
                Ok(()) => {
                    self.removed = true;
                    Ok(())
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    self.removed = true;
                    Ok(())
                }
                Err(error) => Err(adapter_removal_error(&self.path, error)),
            }
        }
    }

    impl Drop for TemporaryAdapter {
        fn drop(&mut self) {
            if !self.removed {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn finish_adapter_cleanup(
        adapter: Option<TemporaryAdapter>,
        result: Result<LocalCompletionOutcome, ShellError>,
    ) -> Result<LocalCompletionOutcome, ShellError> {
        let cleanup = adapter.map_or(Ok(()), TemporaryAdapter::remove);
        match (result, cleanup) {
            (result, Ok(())) => result,
            (Ok(_), Err(cleanup)) => Err(cleanup),
            (Err(original), Err(cleanup)) => Err(cleanup
                .with_context(format!("original failure: {}", original.message))
                .with_help("Restore temporary-directory cleanup and retry completion")),
        }
    }

    fn create_adapter_file(path: &Path, source: &str) -> std::io::Result<File> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        if let Err(error) = file.write_all(source.as_bytes()) {
            let _ = fs::remove_file(path);
            return Err(error);
        }
        Ok(file)
    }

    fn adapter_file_error(path: &Path, error: std::io::Error) -> ShellError {
        ShellError::new(
            ErrorCode::Io,
            "could not create the local completion adapter file",
        )
        .with_context(format!("{}: {error}", path.display()))
        .with_help("Check temporary-directory access and available filesystem space")
    }

    fn adapter_removal_error(path: &Path, error: std::io::Error) -> ShellError {
        ShellError::new(
            ErrorCode::Io,
            "could not remove the local completion adapter file",
        )
        .with_context(format!("{}: {error}", path.display()))
        .with_help("Remove the mode-0600 adapter file and check temporary-directory access")
    }

    fn provider_command(
        request: &LocalCompletionRequest,
        zsh_adapter_path: Option<&Path>,
    ) -> Result<Command, ShellError> {
        let mut command = Command::new(&request.shell_path);
        command.env_clear();
        for (name, value) in &request.environment {
            command.env(name, value);
        }
        command
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let tokens = request
            .command_path
            .iter()
            .chain(&request.arguments)
            .map(String::as_str)
            .collect::<Vec<_>>();
        append_provider_arguments(&mut command, request, &tokens, zsh_adapter_path)?;
        Ok(command)
    }

    fn append_provider_arguments(
        command: &mut Command,
        request: &LocalCompletionRequest,
        tokens: &[&str],
        zsh_adapter_path: Option<&Path>,
    ) -> Result<(), ShellError> {
        let roots = decimal_argument("completion root count", request.completion_roots.len())?;
        let scripts =
            decimal_argument("completion script count", request.completion_scripts.len())?;
        let token_count = decimal_argument("completion token count", tokens.len())?;
        match request.provider {
            LocalCompletionProvider::Zsh => {
                let adapter_path = zsh_adapter_path.ok_or_else(|| {
                    ShellError::new(
                        ErrorCode::Io,
                        "Zsh local completion adapter path is unavailable",
                    )
                    .with_help("Retry; report repeated adapter initialization failures")
                })?;
                command.args(["-d", "-f", "-c", ZSH_OUTER_ADAPTER, "--"]);
                command.arg(&request.shell_path).arg(adapter_path);
            }
            LocalCompletionProvider::Fish => {
                command.args(["--no-config", "--command", FISH_ADAPTER, "--"]);
            }
        }
        command.args([roots, scripts, token_count]);
        command.args(&request.completion_roots);
        command.args(&request.completion_scripts);
        command.args(tokens);
        Ok(())
    }

    fn decimal_argument(context: &str, value: usize) -> Result<String, ShellError> {
        u32::try_from(value)
            .map(|value| value.to_string())
            .map_err(|error| {
                ShellError::new(
                    ErrorCode::ResourceLimit,
                    format!("{context} is outside the provider protocol range"),
                )
                .with_context(error.to_string())
                .with_help("Reduce the bounded local completion request")
            })
    }

    fn take_stdout(child: &mut ContainedChild) -> Result<ChildStdout, ShellError> {
        child.child_mut().stdout.take().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "local completion stdout pipe is unavailable")
                .with_help("Retry; report repeated completion pipe failures")
        })
    }

    fn take_stderr(child: &mut ContainedChild) -> Result<ChildStderr, ShellError> {
        child.child_mut().stderr.take().ok_or_else(|| {
            ShellError::new(ErrorCode::Io, "local completion stderr pipe is unavailable")
                .with_help("Retry; report repeated completion pipe failures")
        })
    }

    fn set_nonblocking(descriptor: &impl AsFd, stream: &str) -> Result<(), ShellError> {
        let flags = fcntl(descriptor, FcntlArg::F_GETFL)
            .map_err(|error| pipe_mode_error(stream, "read", error))?;
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        fcntl(descriptor, FcntlArg::F_SETFL(flags))
            .map(|_| ())
            .map_err(|error| pipe_mode_error(stream, "set", error))
    }

    fn pipe_mode_error(stream: &str, action: &str, error: nix::errno::Errno) -> ShellError {
        ShellError::new(
            ErrorCode::Io,
            format!("could not {action} local completion {stream} pipe mode"),
        )
        .with_context(error.to_string())
        .with_help("Retry; report repeated nonblocking pipe failures")
    }

    struct ProviderOutput {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        observed_bytes: usize,
        bytes_max: usize,
    }

    impl ProviderOutput {
        fn new(bytes_max: usize) -> Self {
            Self {
                stdout: Vec::new(),
                stderr: Vec::new(),
                observed_bytes: 0,
                bytes_max,
            }
        }

        fn retain(&mut self, stream: OutputStream, bytes: &[u8]) {
            let available = self.bytes_max.saturating_sub(self.retained_bytes());
            let retained = bytes.len().min(available);
            match stream {
                OutputStream::Stdout => self.stdout.extend_from_slice(&bytes[..retained]),
                OutputStream::Stderr => self.stderr.extend_from_slice(&bytes[..retained]),
            }
            self.observed_bytes = self.observed_bytes.saturating_add(bytes.len());
        }

        fn retained_bytes(&self) -> usize {
            self.stdout.len().saturating_add(self.stderr.len())
        }

        fn ensure_within_limit(&self) -> Result<(), ShellError> {
            if self.observed_bytes <= self.bytes_max {
                return Ok(());
            }
            Err(resource_limit_error(
                "local completion provider output exceeds its byte limit",
                self.bytes_max,
                self.observed_bytes,
                "Reduce provider output or use a tighter completion scope",
            ))
        }
    }

    #[derive(Clone, Copy)]
    enum OutputStream {
        Stdout,
        Stderr,
    }

    fn poll_provider(
        request: &LocalCompletionRequest,
        deadline: Instant,
        child: &mut ContainedChild,
        stdout: &mut ChildStdout,
        stderr: &mut ChildStderr,
    ) -> Result<(ExitStatus, ProviderOutput), ShellError> {
        let mut output = ProviderOutput::new(request.limits.output_bytes_max);
        loop {
            let stdout_progress = drain_stream_turn(stdout, OutputStream::Stdout, &mut output)?;
            let stderr_progress = drain_stream_turn(stderr, OutputStream::Stderr, &mut output)?;
            if let Err(error) = output.ensure_within_limit() {
                return terminate_after_error(child, error);
            }
            if request.cancelled.load(Ordering::Relaxed) {
                let error = runtime_error_with_stderr(
                    cancelled_error(),
                    &output.stderr,
                    request.limits.field_bytes_max,
                );
                return terminate_after_error(child, error);
            }
            if Instant::now() >= deadline {
                let error = runtime_error_with_stderr(
                    deadline_error(request.deadline),
                    &output.stderr,
                    request.limits.field_bytes_max,
                );
                return terminate_after_error(child, error);
            }
            if let Some(status) = child.try_wait()? {
                child.terminate_and_reap()?;
                drain_after_termination(stdout, stderr, &mut output)?;
                output.ensure_within_limit()?;
                return Ok((status, output));
            }
            if !stdout_progress && !stderr_progress {
                std::thread::sleep(POLL_INTERVAL);
            }
        }
    }

    fn drain_after_termination(
        stdout: &mut ChildStdout,
        stderr: &mut ChildStderr,
        output: &mut ProviderOutput,
    ) -> Result<(), ShellError> {
        let bytes_per_turn = IO_CHUNK_BYTES.saturating_mul(IO_READS_PER_TURN_MAX);
        let turns_max = output
            .bytes_max
            .saturating_div(bytes_per_turn)
            .saturating_add(2);
        for _ in 0..turns_max {
            let stdout_progress = drain_stream_turn(stdout, OutputStream::Stdout, output)?;
            let stderr_progress = drain_stream_turn(stderr, OutputStream::Stderr, output)?;
            output.ensure_within_limit()?;
            if !stdout_progress && !stderr_progress {
                return Ok(());
            }
        }
        Err(resource_limit_error(
            "local completion output drain exceeds its turn limit",
            turns_max,
            turns_max.saturating_add(1),
            "Reduce provider output or report a descriptor that remained writable",
        ))
    }

    fn runtime_error_with_stderr(error: ShellError, stderr: &[u8], bytes_max: usize) -> ShellError {
        if stderr.is_empty() {
            return error;
        }
        error.with_context(format!(
            "provider stderr: {}",
            terminal_safe_excerpt(stderr, bytes_max)
        ))
    }

    fn terminate_after_error<T>(
        child: &mut ContainedChild,
        error: ShellError,
    ) -> Result<T, ShellError> {
        match child.terminate_and_reap() {
            Ok(_) => Err(error),
            Err(cleanup) => Err(ShellError::new(cleanup.code, cleanup.message)
                .with_context(format!("original failure: {}", error.message))
                .with_help("Report the completion process cleanup failure")),
        }
    }

    fn drain_stream_turn(
        reader: &mut impl Read,
        stream: OutputStream,
        output: &mut ProviderOutput,
    ) -> Result<bool, ShellError> {
        let mut progress = false;
        let mut buffer = [0_u8; IO_CHUNK_BYTES];
        for _ in 0..IO_READS_PER_TURN_MAX {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(progress),
                Ok(bytes) => {
                    output.retain(stream, &buffer[..bytes]);
                    progress = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(progress),
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(ShellError::new(
                        ErrorCode::Io,
                        "could not read local completion provider output",
                    )
                    .with_context(error.to_string())
                    .with_help("Retry; report repeated completion pipe failures"));
                }
            }
        }
        Ok(progress)
    }

    fn finish_provider(
        request: &LocalCompletionRequest,
        status: ExitStatus,
        output: ProviderOutput,
    ) -> Result<LocalCompletionOutcome, ShellError> {
        let status_code = exit_status_code(status);
        if status_code == PROVIDER_UNAVAILABLE_STATUS {
            return Ok(unavailable(
                request.provider,
                LocalCompletionUnavailableReason::MissingProvider,
            ));
        }
        if !status.success() {
            let context = terminal_safe_excerpt(&output.stderr, request.limits.field_bytes_max);
            return Err(ShellError::new(
                ErrorCode::ProcessSpawn,
                "local completion provider exited unsuccessfully",
            )
            .with_context(format!("status {status_code}; stderr: {context}"))
            .with_help("Check the admitted completion roots and scripts"));
        }
        let candidates = decode_frames(&output.stdout, request.limits).map_err(|error| {
            error.with_context(format!(
                "provider output prefix: {}",
                hexadecimal_prefix(&output.stdout)
            ))
        })?;
        Ok(LocalCompletionOutcome::Completed(LocalCompletionResult {
            provider: request.provider,
            candidates,
        }))
    }

    fn exit_status_code(status: ExitStatus) -> i32 {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .unwrap_or(1)
    }

    fn terminal_safe_excerpt(bytes: &[u8], bytes_max: usize) -> String {
        let retained = bytes.len().min(bytes_max);
        let text = String::from_utf8_lossy(&bytes[..retained]);
        quirl_core::escape_terminal_line(&text)
    }

    fn hexadecimal_prefix(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut prefix = String::new();
        for byte in bytes.iter().take(128) {
            let _ = write!(prefix, "{byte:02x}");
        }
        prefix
    }
}

#[cfg(unix)]
use unix::run_provider;

fn unavailable(
    provider: LocalCompletionProvider,
    reason: LocalCompletionUnavailableReason,
) -> LocalCompletionOutcome {
    LocalCompletionOutcome::Unavailable(LocalCompletionUnavailable { provider, reason })
}

#[derive(Clone, Copy)]
enum FrameLengthUnit {
    Bytes,
    UnicodeScalars,
}

fn decode_frames(
    bytes: &[u8],
    limits: LocalCompletionLimits,
) -> Result<Vec<LocalCompletionCandidate>, ShellError> {
    let (unit, mut cursor) = decode_magic(bytes)?;
    let mut candidates = Vec::new();
    let mut records = 0_usize;
    while cursor < bytes.len() {
        records = records.saturating_add(1);
        if records > limits.record_count_max {
            return Err(resource_limit_error(
                "local completion frame count exceeds its limit",
                limits.record_count_max,
                records,
                "Reduce provider output or increase the bounded record limit",
            ));
        }
        let (candidate_length, description_length) = decode_header(bytes, &mut cursor)?;
        let candidate = decode_field(bytes, &mut cursor, candidate_length, unit, limits)?;
        let description = decode_field(bytes, &mut cursor, description_length, unit, limits)?;
        if candidates.len() >= limits.candidate_count_max {
            return Err(resource_limit_error(
                "local completion candidate count exceeds its limit",
                limits.candidate_count_max,
                candidates.len().saturating_add(1),
                "Use a narrower completion context or raise the bounded candidate limit",
            ));
        }
        candidates.push(LocalCompletionCandidate {
            value: candidate,
            description: (!description.is_empty()).then_some(description),
        });
    }
    Ok(candidates)
}

fn decode_magic(bytes: &[u8]) -> Result<(FrameLengthUnit, usize), ShellError> {
    if bytes.starts_with(FRAME_MAGIC_BYTES) {
        return Ok((FrameLengthUnit::Bytes, FRAME_MAGIC_BYTES.len()));
    }
    if bytes.starts_with(FRAME_MAGIC_SCALARS) {
        return Ok((FrameLengthUnit::UnicodeScalars, FRAME_MAGIC_SCALARS.len()));
    }
    Err(protocol_error(
        "local completion output has a missing or invalid frame magic",
        "Check that the admitted shell matches the selected provider",
    ))
}

fn decode_header(bytes: &[u8], cursor: &mut usize) -> Result<(usize, usize), ShellError> {
    let end = cursor.saturating_add(FRAME_HEADER_BYTES);
    let header = bytes.get(*cursor..end).ok_or_else(|| {
        protocol_error(
            "local completion output ends inside a frame header",
            "Fix or remove the malformed completion provider",
        )
    })?;
    let candidate = decode_hex_length(&header[..8])?;
    let description = decode_hex_length(&header[8..])?;
    *cursor = end;
    Ok((candidate, description))
}

fn decode_hex_length(bytes: &[u8]) -> Result<usize, ShellError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        protocol_error(
            "local completion frame length is not ASCII hexadecimal",
            "Fix or remove the malformed completion provider",
        )
    })?;
    u32::from_str_radix(text, 16)
        .map(|value| value as usize)
        .map_err(|error| {
            protocol_error(
                "local completion frame length is malformed",
                "Fix or remove the malformed completion provider",
            )
            .with_context(format!(
                "header `{}`: {error}",
                quirl_core::escape_terminal_line(text)
            ))
        })
}

fn decode_field(
    bytes: &[u8],
    cursor: &mut usize,
    length: usize,
    unit: FrameLengthUnit,
    limits: LocalCompletionLimits,
) -> Result<String, ShellError> {
    let end = match unit {
        FrameLengthUnit::Bytes => cursor.checked_add(length).ok_or_else(|| {
            protocol_error(
                "local completion frame length overflows the platform range",
                "Fix or remove the malformed completion provider",
            )
        })?,
        FrameLengthUnit::UnicodeScalars => scalar_field_end(bytes, *cursor, length)?,
    };
    let field = bytes.get(*cursor..end).ok_or_else(|| {
        protocol_error(
            "local completion output ends inside a framed field",
            "Fix or remove the truncated completion provider output",
        )
    })?;
    if field.len() > limits.field_bytes_max {
        return Err(resource_limit_error(
            "local completion field exceeds its byte limit",
            limits.field_bytes_max,
            field.len(),
            "Use shorter completion candidates or descriptions",
        ));
    }
    let decoded = std::str::from_utf8(field).map_err(|_| {
        protocol_error(
            "local completion field is not valid UTF-8",
            "Configure the provider to emit UTF-8 completion text",
        )
    })?;
    *cursor = end;
    Ok(decoded.to_owned())
}

fn scalar_field_end(bytes: &[u8], start: usize, scalars: usize) -> Result<usize, ShellError> {
    let remaining = bytes.get(start..).ok_or_else(|| {
        protocol_error(
            "local completion scalar field starts outside the output",
            "Fix or remove the malformed completion provider",
        )
    })?;
    let text = std::str::from_utf8(remaining).map_err(|_| {
        protocol_error(
            "local completion scalar-framed output is not valid UTF-8",
            "Configure the Fish provider to emit UTF-8 completion text",
        )
    })?;
    if scalars == 0 {
        return Ok(start);
    }
    text.char_indices().nth(scalars).map_or_else(
        || {
            if text.chars().count() == scalars {
                Ok(bytes.len())
            } else {
                Err(protocol_error(
                    "local completion output ends inside a scalar-framed field",
                    "Fix or remove the truncated completion provider output",
                ))
            }
        },
        |(offset, _)| Ok(start + offset),
    )
}

fn protocol_error(message: &str, help: &str) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help(help)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    #[cfg(unix)]
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        process::Command,
        sync::atomic::AtomicUsize,
        thread,
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    static TEMP_DIRECTORY_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn limits() -> LocalCompletionLimits {
        LocalCompletionLimits {
            output_bytes_max: 4096,
            record_count_max: 8,
            field_bytes_max: 64,
            candidate_count_max: 8,
            ..LocalCompletionLimits::default()
        }
    }

    fn byte_frame(candidate: &[u8], description: &[u8]) -> Vec<u8> {
        let mut frame = Vec::from(FRAME_MAGIC_BYTES.as_slice());
        frame.extend_from_slice(
            format!("{:08x}{:08x}", candidate.len(), description.len()).as_bytes(),
        );
        frame.extend_from_slice(candidate);
        frame.extend_from_slice(description);
        frame
    }

    #[test]
    fn framed_decoder_preserves_delimiters_inside_fields() {
        let decoded = decode_frames(&byte_frame(b"a\t -- b", b"d\n--\tvalue"), limits()).unwrap();
        assert_eq!(
            decoded,
            vec![LocalCompletionCandidate {
                value: "a\t -- b".to_owned(),
                description: Some("d\n--\tvalue".to_owned()),
            }]
        );
    }

    #[test]
    fn scalar_framed_decoder_preserves_multibyte_fields() {
        let mut frame = Vec::from(FRAME_MAGIC_SCALARS.as_slice());
        frame.extend_from_slice(b"0000000200000001");
        frame.extend_from_slice("é界猫".as_bytes());
        let decoded = decode_frames(&frame, limits()).unwrap();
        assert_eq!(decoded[0].value, "é界");
        assert_eq!(decoded[0].description.as_deref(), Some("猫"));
    }

    #[test]
    fn framed_decoder_rejects_malformed_and_truncated_records() {
        let malformed = decode_frames(b"QLB1zzzzzzzz00000000", limits()).unwrap_err();
        assert_eq!(malformed.code, ErrorCode::Validation);
        let truncated = decode_frames(b"QLB10000000300000000ab", limits()).unwrap_err();
        assert_eq!(truncated.code, ErrorCode::Validation);
        assert!(truncated.message.contains("inside"));
    }

    #[test]
    fn framed_decoder_enforces_record_field_and_candidate_limits() {
        let mut two = byte_frame(b"a", b"");
        two.extend_from_slice(b"0000000100000000b");
        let mut record_limit = limits();
        record_limit.record_count_max = 1;
        assert_eq!(
            decode_frames(&two, record_limit).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
        let mut field_limit = limits();
        field_limit.field_bytes_max = 1;
        assert_eq!(
            decode_frames(&byte_frame(b"ab", b""), field_limit)
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        let mut candidate_limit = limits();
        candidate_limit.candidate_count_max = 1;
        assert_eq!(
            decode_frames(&two, candidate_limit).unwrap_err().code,
            ErrorCode::ResourceLimit
        );
    }

    #[cfg(unix)]
    #[test]
    fn request_validation_rejects_excessive_nested_path_depth() {
        let mut request = request(LocalCompletionProvider::Fish, PathBuf::from("/bin/sh"));
        request.command_path = vec!["qtool".to_owned(), "nested".to_owned()];
        request.limits.path_depth_max = 1;
        let error = LocalCompletionProcess::new(1)
            .unwrap()
            .complete(request)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("path depth"));
    }

    #[cfg(unix)]
    struct TestDirectory {
        path: PathBuf,
    }

    #[cfg(unix)]
    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEMP_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "quirl-local-completion-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn script(&self, name: &str, source: &str) -> PathBuf {
            let path = self.path.join(name);
            fs::write(&path, source).unwrap();
            path
        }

        fn executable(&self, name: &str, source: &str) -> PathBuf {
            let path = self.script(name, source);
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(&path, permissions).unwrap();
            path
        }
    }

    #[cfg(unix)]
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    fn shell_path(candidates: &[&str]) -> Option<PathBuf> {
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    }

    #[cfg(unix)]
    fn request(provider: LocalCompletionProvider, shell_path: PathBuf) -> LocalCompletionRequest {
        LocalCompletionRequest {
            provider,
            shell_path,
            command_path: vec!["qtool".to_owned()],
            arguments: vec![String::new()],
            completion_roots: Vec::new(),
            completion_scripts: Vec::new(),
            environment: Vec::new(),
            deadline: Duration::from_secs(2),
            cancelled: Arc::new(AtomicBool::new(false)),
            limits: LocalCompletionLimits {
                output_bytes_max: 64 * 1024,
                record_count_max: 256,
                field_bytes_max: 4096,
                candidate_count_max: 256,
                ..LocalCompletionLimits::default()
            },
        }
    }

    #[cfg(unix)]
    fn completed(outcome: LocalCompletionOutcome) -> LocalCompletionResult {
        match outcome {
            LocalCompletionOutcome::Completed(result) => result,
            LocalCompletionOutcome::Unavailable(unavailable) => {
                panic!("provider unexpectedly unavailable: {unavailable:?}")
            }
        }
    }

    #[cfg(unix)]
    fn zsh_function_roots(shell: &Path) -> Vec<PathBuf> {
        let output = Command::new(shell)
            .args(["-d", "-f", "-c", "print -rl -- $fpath"])
            .env_clear()
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn zsh_pty_capture_completes_nested_command_paths() {
        let Some(shell) = shell_path(&["/bin/zsh", "/usr/bin/zsh"]) else {
            return;
        };
        let directory = TestDirectory::new();
        let startup_marker = directory.path.join("unexpected-zshrc");
        directory.script(".zshrc", "print sourced > \"$STARTUP_MARKER\"\n");
        let script = directory.script(
            "qtool.zsh",
            r#"
_qtool() {
    if [[ $words[2] == remote ]]; then
        local -a descriptions
        descriptions=( 'add nested target' 'admin nested target' )
        compadd -d descriptions -- add admin
    fi
}
compdef _qtool qtool
"#,
        );
        let mut request = request(LocalCompletionProvider::Zsh, shell.clone());
        request.command_path = vec!["qtool".to_owned(), "remote".to_owned()];
        request.arguments = vec!["a".to_owned()];
        request.completion_roots = zsh_function_roots(&shell);
        request.completion_scripts = vec![script];
        request.environment = vec![
            (
                "HOME".to_owned(),
                directory.path.to_string_lossy().into_owned(),
            ),
            (
                "STARTUP_MARKER".to_owned(),
                startup_marker.to_string_lossy().into_owned(),
            ),
        ];
        let result = completed(
            LocalCompletionProcess::new(1)
                .unwrap()
                .complete(request)
                .unwrap(),
        );
        let values = result
            .candidates
            .iter()
            .map(|candidate| candidate.value.as_str())
            .collect::<Vec<_>>();
        assert!(values.contains(&"add"), "candidates: {values:?}");
        assert!(values.contains(&"admin"), "candidates: {values:?}");
        assert!(!startup_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn fish_do_complete_returns_framed_candidates() {
        let Some(shell) = shell_path(&[
            "/opt/homebrew/bin/fish",
            "/usr/local/bin/fish",
            "/usr/bin/fish",
            "/bin/fish",
        ]) else {
            return;
        };
        let directory = TestDirectory::new();
        let config_home = directory.path.join("fish-config");
        fs::create_dir_all(config_home.join("fish")).unwrap();
        let startup_marker = directory.path.join("unexpected-fish-config");
        fs::write(
            config_home.join("fish/config.fish"),
            "echo sourced > \"$STARTUP_MARKER\"\n",
        )
        .unwrap();
        let script = directory.script(
            "qtool.fish",
            "complete -c qtool -n 'contains -- remote (commandline -opc)' -a 'add admin' -d 'nested target'\n",
        );
        let mut request = request(LocalCompletionProvider::Fish, shell);
        request.command_path = vec!["qtool".to_owned(), "remote".to_owned()];
        request.arguments = vec!["a".to_owned()];
        request.completion_scripts = vec![script];
        request.environment = vec![
            (
                "XDG_CONFIG_HOME".to_owned(),
                config_home.to_string_lossy().into_owned(),
            ),
            (
                "STARTUP_MARKER".to_owned(),
                startup_marker.to_string_lossy().into_owned(),
            ),
        ];
        let result = completed(
            LocalCompletionProcess::new(1)
                .unwrap()
                .complete(request)
                .unwrap(),
        );
        assert_eq!(
            result.candidates,
            vec![
                LocalCompletionCandidate {
                    value: "add".to_owned(),
                    description: Some("nested target".to_owned()),
                },
                LocalCompletionCandidate {
                    value: "admin".to_owned(),
                    description: Some("nested target".to_owned()),
                },
            ]
        );
        assert!(!startup_marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn temporary_zsh_adapter_is_removed_on_drop() {
        let adapter = unix::TemporaryAdapter::create("print ready\n").unwrap();
        let path = adapter.path().to_owned();
        assert!(path.is_file());
        drop(adapter);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn missing_shell_and_provider_are_typed_unavailable_outcomes() {
        let missing = request(
            LocalCompletionProvider::Fish,
            PathBuf::from("/quirl/missing/fish"),
        );
        assert_eq!(
            LocalCompletionProcess::new(1)
                .unwrap()
                .complete(missing)
                .unwrap(),
            unavailable(
                LocalCompletionProvider::Fish,
                LocalCompletionUnavailableReason::MissingShell
            )
        );

        let directory = TestDirectory::new();
        let shell = directory.executable("unavailable", "#!/bin/sh\nexit 78\n");
        let provider = request(LocalCompletionProvider::Fish, shell);
        assert_eq!(
            LocalCompletionProcess::new(1)
                .unwrap()
                .complete(provider)
                .unwrap(),
            unavailable(
                LocalCompletionProvider::Fish,
                LocalCompletionUnavailableReason::MissingProvider
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn malformed_provider_frames_fail_at_the_process_boundary() {
        let directory = TestDirectory::new();
        let shell =
            directory.executable("malformed", "#!/bin/sh\nprintf 'QLB10000000300000000ab'\n");
        let error = LocalCompletionProcess::new(1)
            .unwrap()
            .complete(request(LocalCompletionProvider::Fish, shell))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("inside"));
    }

    #[cfg(unix)]
    #[test]
    fn hanging_provider_is_killed_at_the_deadline() {
        let directory = TestDirectory::new();
        let shell = directory.executable("hang", "#!/bin/sh\nexec /bin/sleep 30\n");
        let mut request = request(LocalCompletionProvider::Fish, shell);
        request.deadline = Duration::from_millis(30);
        let started = Instant::now();
        let error = LocalCompletionProcess::new(1)
            .unwrap()
            .complete(request)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_stops_an_active_provider() {
        let directory = TestDirectory::new();
        let shell = directory.executable("hang", "#!/bin/sh\nexec /bin/sleep 30\n");
        let mut request = request(LocalCompletionProvider::Fish, shell);
        request.deadline = Duration::from_secs(2);
        let cancelled = Arc::clone(&request.cancelled);
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            cancelled.store(true, Ordering::Relaxed);
        });
        let error = LocalCompletionProcess::new(1)
            .unwrap()
            .complete(request)
            .unwrap_err();
        canceller.join().unwrap();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("cancelled"));
    }

    #[cfg(unix)]
    #[test]
    fn excessive_provider_output_trips_the_byte_limit() {
        let directory = TestDirectory::new();
        let shell = directory.executable(
            "flood",
            "#!/bin/sh\nwhile :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done\n",
        );
        let mut request = request(LocalCompletionProvider::Fish, shell);
        request.limits.output_bytes_max = 128;
        let error = LocalCompletionProcess::new(1)
            .unwrap()
            .complete(request)
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("output"));
        assert!(error.details.context[0].contains("limit 128"));
    }

    #[cfg(unix)]
    #[test]
    fn process_slots_reject_overlapping_provider_work() {
        let directory = TestDirectory::new();
        let ready = directory.path.join("ready");
        let shell = directory.executable(
            "hang",
            "#!/bin/sh\nprintf ready > \"$READY_FILE\"\nexec /bin/sleep 30\n",
        );
        let process = LocalCompletionProcess::new(1).unwrap();
        let mut first = request(LocalCompletionProvider::Fish, shell.clone());
        first.environment = vec![(
            "READY_FILE".to_owned(),
            ready.to_string_lossy().into_owned(),
        )];
        let cancelled = Arc::clone(&first.cancelled);
        let worker_process = process.clone();
        let worker = thread::spawn(move || worker_process.complete(first));
        let wait_started = Instant::now();
        while !ready.exists() && wait_started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(ready.exists());
        let second = request(LocalCompletionProvider::Fish, shell);
        let error = process.complete(second).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("slots"));
        cancelled.store(true, Ordering::Relaxed);
        assert!(
            worker
                .join()
                .unwrap()
                .unwrap_err()
                .message
                .contains("cancelled")
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_kills_and_reaps_provider_descendants() {
        let directory = TestDirectory::new();
        let pid_file = directory.path.join("descendant.pid");
        let shell = directory.executable(
            "descendant",
            "#!/bin/sh\n/bin/sh -c 'printf %s \"$$\" > \"$PID_FILE\"; exec /bin/sleep 30' &\nwait\n",
        );
        let mut request = request(LocalCompletionProvider::Fish, shell);
        request.deadline = Duration::from_secs(2);
        request.environment = vec![(
            "PID_FILE".to_owned(),
            pid_file.to_string_lossy().into_owned(),
        )];
        let cancelled = Arc::clone(&request.cancelled);
        let process = LocalCompletionProcess::new(1).unwrap();
        let worker = thread::spawn(move || process.complete(request));
        let startup = Instant::now();
        while !pid_file.exists() && startup.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(pid_file.exists(), "descendant did not publish its pid");
        cancelled.store(true, Ordering::Relaxed);
        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        let pid = fs::read_to_string(&pid_file)
            .unwrap()
            .parse::<i32>()
            .unwrap();
        let started = Instant::now();
        loop {
            match kill(Pid::from_raw(pid), None) {
                Err(Errno::ESRCH) => break,
                _ if started.elapsed() < Duration::from_secs(5) => {
                    thread::sleep(Duration::from_millis(1));
                }
                result => panic!("descendant {pid} remained after cleanup: {result:?}"),
            }
        }
    }
}
