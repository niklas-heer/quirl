//! Plugin management and durable lockfile publication.
//!
//! # Persistence failure model and resource bounds
//!
//! Concurrent commands may update the same lock. Mutations hold the bounded
//! process/OS coordination lock across reading, validation and publication.
//! Lock serialization and reads stop at 4 MiB. Each mutation enforces a 1024-entry
//! state-directory limit and refuses to stage more data while a previous lock
//! transaction temporary or recovery name remains. Existing lock and backup files
//! are replaced with core's verified atomic transaction, so the active name
//! stays readable while its complete predecessor is saved as the backup.
//!
//! Temporary creation may collide with a file or symlink; 64 create-new attempts
//! never truncate an existing entry. Initial publication uses a synchronized
//! private temporary and a no-replace hard link. Returned failures preserve
//! transaction names with diagnostic context, because portable Rust cannot
//! condition cleanup on identity. Successful unlink assumes a cooperative
//! containing namespace, matching core's atomic replacement contract.

use crate::bounded_file::{ReadFileOptions, read_optional_regular_file, read_regular_file};
use crate::coordination::{self, CoordinationGuard, CoordinationKind, CoordinationWait};
use crate::lua_worker::LuaWorkerRuntime as LuaRuntime;
use clap::{Subcommand, ValueEnum};
use quirl_core::{
    AtomicReplaceOptions, ContributionKind, ErrorCode, ShellError, escape_json_terminal_controls,
    escape_terminal_controls, replace_file_atomically,
};
use quirl_lua::{CommandRegistration, LuaPolicy};
use quirl_plugin::{
    ADAPTER_PROTOCOL, AdapterInitializeRequest, AdapterInitializeResponse, DoctorReport,
    PLUGIN_LOCK_FILE, PermissionDiff, PluginLockfile, PluginManifest, PluginRuntime, doctor_plugin,
    parse_plugin_manifest, permission_diff, resolve_plugin, validate_plugin_manifest,
};
use quirl_process::{ChildProcessTree, sandboxed_process_host};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    env, fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};

const MANIFEST_FILE: &str = "plugin.toml";
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;
const MAX_LOCK_BYTES: usize = 4 * 1024 * 1024;
const LOCK_TEMPORARY_ATTEMPTS_MAX: usize = 64;
const LOCK_DIRECTORY_ENTRIES_MAX: usize = 1024;
static LOCK_TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Validate a legacy trusted Lua plugin registration file.
    Check {
        /// Lua plugin registration file to validate without installing it.
        file: PathBuf,
        /// Output representation for the validation result.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Add a local package after an explicit permission review.
    Add {
        /// Local plugin source directory or manifest location.
        source: String,
        /// Capability approved for this installation; repeat for each grant.
        #[arg(long = "allow")]
        allow: Vec<String>,
        /// Output representation for the installation result.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Show requested, granted, and currently missing permissions.
    Permissions {
        /// Installed plugin name.
        name: String,
        /// Output representation for the permission report.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Enable an installed plugin after checksum verification.
    Enable {
        /// Installed plugin name.
        name: String,
        /// Output representation for the updated lock state.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Disable an installed plugin without deleting its permission record.
    Disable {
        /// Installed plugin name.
        name: String,
        /// Output representation for the updated lock state.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Verify schema, source checksums, permissions, and runtime boundary.
    Doctor {
        /// Installed plugin name.
        name: String,
        /// Output representation for the integrity report.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Verify installed sources without changing the locked resolution.
    Update {
        /// Require every resolved version and permission grant to remain unchanged.
        #[arg(long)]
        locked: bool,
        /// Output representation for the verification result.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Remove an installed plugin record without deleting its source.
    Remove {
        /// Installed plugin name.
        name: String,
        /// Output representation for the updated lock state.
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PluginOutputFormat {
    Text,
    Json,
}

#[derive(Debug, Serialize)]
struct MutationOutput<'a> {
    document_type: &'static str,
    schema_version: u32,
    action: &'static str,
    plugin: &'a str,
    permission_diff: Option<&'a PermissionDiff>,
    lockfile: &'a PluginLockfile,
}

#[derive(Debug, Serialize)]
struct PermissionsOutput<'a> {
    document_type: &'static str,
    schema_version: u32,
    plugin: &'a str,
    requested: &'a [String],
    granted: &'a [String],
    diff: PermissionDiff,
}

pub fn wants_json(command: &PluginCommand) -> bool {
    match command {
        PluginCommand::Check { format, .. }
        | PluginCommand::Add { format, .. }
        | PluginCommand::Permissions { format, .. }
        | PluginCommand::Enable { format, .. }
        | PluginCommand::Disable { format, .. }
        | PluginCommand::Doctor { format, .. }
        | PluginCommand::Update { format, .. }
        | PluginCommand::Remove { format, .. } => matches!(format, PluginOutputFormat::Json),
    }
}

pub fn execute(command: PluginCommand) -> Result<i32, ShellError> {
    let root = plugin_root()?;
    let _coordination = match &command {
        PluginCommand::Add { .. }
        | PluginCommand::Enable { .. }
        | PluginCommand::Disable { .. }
        | PluginCommand::Update { .. }
        | PluginCommand::Remove { .. } => Some(acquire_plugin_lock(&root)?),
        PluginCommand::Check { .. }
        | PluginCommand::Permissions { .. }
        | PluginCommand::Doctor { .. } => None,
    };
    match command {
        PluginCommand::Check { file, format } => check_legacy(&file, format),
        PluginCommand::Add {
            source,
            allow,
            format,
        } => {
            let package = read_source_package(&source)?;
            let (resolved, diff) = resolve_plugin(
                &package.manifest,
                package.manifest_source.as_bytes(),
                &package.entry_bytes,
                &package.source_id,
                &allow,
                env!("CARGO_PKG_VERSION"),
            )?;
            validate_runtime(
                &package.manifest,
                &package.entry_path,
                &package.entry_bytes,
                &resolved.granted_capabilities,
                false,
            )?;
            let lock = load_lock(&root)?.install(resolved)?;
            save_lock(&root, &lock)?;
            print_mutation(
                "add",
                &package.manifest.plugin.name,
                Some(&diff),
                &lock,
                format,
            )?;
            Ok(0)
        }
        PluginCommand::Permissions { name, format } => {
            let lock = load_lock(&root)?;
            let plugin = lock.find(&name)?;
            let output = PermissionsOutput {
                document_type: "quirl.plugin.permissions",
                schema_version: 1,
                plugin: &plugin.name,
                requested: &plugin.requested_capabilities,
                granted: &plugin.granted_capabilities,
                diff: permission_diff(&plugin.granted_capabilities, &plugin.requested_capabilities),
            };
            print_permissions(&output, format)?;
            Ok(0)
        }
        PluginCommand::Enable { name, format } => {
            let lock = load_lock(&root)?;
            let plugin = lock.find(&name)?;
            let package = read_source_package(&plugin.source)?;
            let report = doctor_plugin(
                plugin,
                package.manifest_source.as_bytes(),
                &package.entry_bytes,
            );
            if !report.healthy {
                return Err(unhealthy_error(&report));
            }
            validate_runtime(
                &package.manifest,
                &package.entry_path,
                &package.entry_bytes,
                &plugin.granted_capabilities,
                true,
            )?;
            let candidate = lock.set_enabled(&name, true)?;
            save_lock(&root, &candidate)?;
            print_mutation("enable", &name, None, &candidate, format)?;
            Ok(0)
        }
        PluginCommand::Disable { name, format } => {
            let candidate = load_lock(&root)?.set_enabled(&name, false)?;
            save_lock(&root, &candidate)?;
            print_mutation("disable", &name, None, &candidate, format)?;
            Ok(0)
        }
        PluginCommand::Doctor { name, format } => {
            let lock = load_lock(&root)?;
            let plugin = lock.find(&name)?;
            let package = read_source_package(&plugin.source)?;
            let mut report = doctor_plugin(
                plugin,
                package.manifest_source.as_bytes(),
                &package.entry_bytes,
            );
            if let Err(error) = validate_runtime(
                &package.manifest,
                &package.entry_path,
                &package.entry_bytes,
                &plugin.granted_capabilities,
                false,
            ) {
                report.healthy = false;
                report.diagnostics.push(quirl_plugin::DoctorDiagnostic {
                    severity: quirl_plugin::DoctorSeverity::Error,
                    code: "plugin.runtime_boundary".to_owned(),
                    message: error.message,
                    help: error
                        .details
                        .help
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "Repair the plugin runtime boundary".to_owned()),
                });
            }
            print_doctor(&report, format)?;
            Ok(i32::from(!report.healthy))
        }
        PluginCommand::Update { locked, format } => {
            if !locked {
                return Err(ShellError::new(
                    ErrorCode::InvalidArgument,
                    "plugin update requires --locked in platform v0.1",
                )
                .with_help("Use --locked to verify sources without changing versions, checksums, or permissions"));
            }
            let lock = load_lock(&root)?;
            let mut candidate = lock.clone();
            for plugin in &lock.plugins {
                let package = read_source_package(&plugin.source)?;
                let (resolved, _) = resolve_plugin(
                    &package.manifest,
                    package.manifest_source.as_bytes(),
                    &package.entry_bytes,
                    &package.source_id,
                    &plugin.granted_capabilities,
                    env!("CARGO_PKG_VERSION"),
                )?;
                candidate = candidate.replace_locked(resolved)?;
            }
            save_lock(&root, &candidate)?;
            print_mutation("update_locked", "all", None, &candidate, format)?;
            Ok(0)
        }
        PluginCommand::Remove { name, format } => {
            let candidate = load_lock(&root)?.remove(&name)?;
            save_lock(&root, &candidate)?;
            print_mutation("remove", &name, None, &candidate, format)?;
            Ok(0)
        }
    }
}

#[derive(Debug)]
struct SourcePackage {
    source_id: String,
    manifest_source: String,
    manifest: PluginManifest,
    entry_path: PathBuf,
    entry_bytes: Vec<u8>,
}

fn read_source_package(source: &str) -> Result<SourcePackage, ShellError> {
    let source_path = source.strip_prefix("file:").unwrap_or(source);
    if source_path.contains(':') && !Path::new(source_path).exists() {
        return Err(ShellError::new(
            ErrorCode::InvalidArgument,
            format!("plugin source `{source}` requires an unavailable network resolver"),
        )
        .with_help("Platform v0.1 installs local directories or file: paths; fetch remote sources separately"));
    }
    let source_path = fs::canonicalize(source_path).map_err(|error| {
        io_error(
            format!("cannot resolve plugin source `{source}`"),
            error,
            "Pass a local plugin directory or its plugin.toml path",
        )
    })?;
    let source_is_directory = source_path.is_dir();
    let manifest_path = if source_is_directory {
        source_path.join(MANIFEST_FILE)
    } else {
        source_path.clone()
    };
    let manifest_path = fs::canonicalize(&manifest_path).map_err(|error| {
        io_error(
            format!("cannot resolve {}", manifest_path.display()),
            error,
            "Provide a readable plugin.toml inside the plugin package",
        )
    })?;
    if source_is_directory && !manifest_path.starts_with(&source_path) {
        return Err(package_escape_error("plugin manifest"));
    }
    let manifest_bytes = read_bounded(
        &manifest_path,
        MAX_MANIFEST_BYTES,
        "plugin manifest",
        "Keep plugin.toml below 256 KiB",
    )?;
    let manifest_source = String::from_utf8(manifest_bytes).map_err(|error| {
        ShellError::new(ErrorCode::Validation, "plugin.toml is not valid UTF-8")
            .with_context(error.to_string())
            .with_help("Encode the manifest as UTF-8")
    })?;
    let manifest = parse_plugin_manifest(&manifest_source, &manifest_path.display().to_string())?;
    let entry = safe_package_path(&manifest.plugin.entry)?;
    let directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let entry_path = fs::canonicalize(directory.join(entry)).map_err(|error| {
        io_error(
            format!(
                "cannot resolve plugin entry declared by {}",
                manifest_path.display()
            ),
            error,
            "Correct the manifest entry path",
        )
    })?;
    if !entry_path.starts_with(directory) {
        return Err(package_escape_error("plugin entry"));
    }
    let entry_bytes = read_bounded(
        &entry_path,
        MAX_ENTRY_BYTES,
        "plugin entry",
        "Keep the plugin entry below 4 MiB and move data into bounded runtime inputs",
    )?;
    let source_id = format!("file:{}", manifest_path.display());
    Ok(SourcePackage {
        source_id,
        manifest_source,
        manifest,
        entry_path,
        entry_bytes,
    })
}

/// Builds the restricted runtime for a trusted-Lua plugin. This is the single
/// place that maps `process.spawn` grants onto `LuaPolicy::allow_process`, so
/// activation and validation cannot drift apart.
pub(crate) fn trusted_lua_runtime(grants: &[String]) -> Result<LuaRuntime, ShellError> {
    trusted_lua_runtime_with_policy(grants, LuaPolicy::config())
}

fn trusted_lua_runtime_with_policy(
    grants: &[String],
    mut policy: LuaPolicy,
) -> Result<LuaRuntime, ShellError> {
    policy.allow_process = grants
        .iter()
        .any(|grant| grant == "process.spawn" || grant.starts_with("process.spawn:"));
    if policy.allow_process {
        LuaRuntime::new_with_capabilities_and_process_host(
            policy,
            grants,
            Some(sandboxed_process_host()),
        )
    } else {
        LuaRuntime::new_with_capabilities(policy, grants)
    }
}

fn validate_runtime(
    manifest: &PluginManifest,
    entry_path: &Path,
    entry_bytes: &[u8],
    grants: &[String],
    require_runnable: bool,
) -> Result<(), ShellError> {
    validate_runtime_with_lua_policy(
        manifest,
        entry_path,
        entry_bytes,
        grants,
        require_runnable,
        LuaPolicy::config(),
    )
}

// Functional fixtures can exercise real isolated validation without treating
// the production 100 ms wall budget as a scheduler guarantee. The caller's
// policy still crosses the worker's bounded policy admission unchanged; grants
// remain the sole authority for enabling process calls.
fn validate_runtime_with_lua_policy(
    manifest: &PluginManifest,
    entry_path: &Path,
    entry_bytes: &[u8],
    grants: &[String],
    require_runnable: bool,
    lua_policy: LuaPolicy,
) -> Result<(), ShellError> {
    validate_plugin_manifest(manifest, entry_bytes, env!("CARGO_PKG_VERSION"))?;
    match manifest.plugin.runtime {
        PluginRuntime::OutOfProcess => {
            if require_runnable {
                execute_out_of_process_adapter(manifest, entry_path, entry_bytes, grants, None)?;
            }
            return Ok(());
        }
        PluginRuntime::WasmComponent => {
            if require_runnable {
                return Err(ShellError::new(
                    ErrorCode::Validation,
                    format!(
                        "plugin `{}` has a validated but non-executing Wasm component boundary",
                        manifest.plugin.name
                    ),
                )
                .with_help("Keep it disabled until a component runtime is installed"));
            }
            return Ok(());
        }
        PluginRuntime::TrustedLua => {}
    }
    let entry_source = std::str::from_utf8(entry_bytes).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            "trusted Lua plugin entry is not valid UTF-8",
        )
        .with_context(error.to_string())
        .with_help("Encode the locked plugin entry as UTF-8")
    })?;
    let source_name = entry_path.display().to_string();
    LuaRuntime::check_source_with_policy(entry_source, &source_name, lua_policy)?;
    let runtime = trusted_lua_runtime_with_policy(grants, lua_policy)?;
    let registrations = runtime.load_plugin_source(entry_source, &source_name)?;
    let registered_commands = registrations
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect::<Vec<_>>();
    let declared_commands = manifest
        .contributes
        .commands
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let registered_completions = registrations
        .completion_providers
        .iter()
        .map(|item| item.command.as_str())
        .collect::<Vec<_>>();
    let declared_completions = manifest
        .contributes
        .completions
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let registered_events = registrations
        .events
        .iter()
        .map(|item| item.name.as_str())
        .collect::<Vec<_>>();
    let declared_events = manifest
        .contributes
        .events
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let registered_panels =
        contribution_names(&registrations.contributions, ContributionKind::Panel);
    let declared_panels = manifest
        .contributes
        .panels
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let registered_indexers =
        contribution_names(&registrations.contributions, ContributionKind::Catalog);
    let declared_indexers = manifest
        .contributes
        .indexers
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    if registered_commands != declared_commands
        || registered_completions != declared_completions
        || registered_events != declared_events
        || registered_panels != declared_panels
        || registered_indexers != declared_indexers
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "trusted Lua registrations differ from plugin.toml contributions",
        )
        .with_context(format!(
            "declared commands/completions/events/panels/indexers: {declared_commands:?} / {declared_completions:?} / {declared_events:?} / {declared_panels:?} / {declared_indexers:?}"
        ))
        .with_context(format!(
            "registered commands/completions/events/panels/indexers: {registered_commands:?} / {registered_completions:?} / {registered_events:?} / {registered_panels:?} / {registered_indexers:?}"
        ))
        .with_help(
            "Keep manifest contributions and typed Lua registrations identical and sorted",
        ));
    }
    validate_registered_command_metadata(manifest, &registrations.commands)?;
    Ok(())
}

fn validate_registered_command_metadata(
    manifest: &PluginManifest,
    registrations: &[CommandRegistration],
) -> Result<(), ShellError> {
    for command in &manifest.public_commands {
        let registration = registrations
            .iter()
            .find(|registration| registration.name == command.path)
            .ok_or_else(|| {
                ShellError::new(
                    ErrorCode::Validation,
                    format!("plugin command `{}` registration disappeared", command.path),
                )
                .with_help("Keep Lua registrations identical to plugin.toml")
            })?;
        let registered_effects = registration
            .effects
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let declared_effects = command
            .effects
            .iter()
            .map(|effect| match effect {
                quirl_catalog::Effect::ReadFilesystem => "read_filesystem",
                quirl_catalog::Effect::WriteFilesystem => "write_filesystem",
                quirl_catalog::Effect::SpawnProcess => "spawn_process",
                quirl_catalog::Effect::ChangeDirectory => "change_directory",
            })
            .collect::<BTreeSet<_>>();
        let matches = registration.signature == command.signature
            && registration.summary == command.summary
            && registration.details == command.details
            && registration.input_type == command.input_type
            && registration.output_type == command.output_type
            && registration.examples == command.examples
            && registration.error_codes == command.error_codes
            && registered_effects == declared_effects;
        if !matches {
            return Err(ShellError::new(
                ErrorCode::Validation,
                format!(
                    "plugin command `{}` registration differs from plugin.toml",
                    command.path
                ),
            )
            .with_context(format!(
                "declared I/O: {} -> {}; registered I/O: {} -> {}",
                command.input_type,
                command.output_type,
                registration.input_type,
                registration.output_type
            ))
            .with_help(
                "Keep signature, documentation, I/O, examples, effects, and statuses byte-for-byte aligned",
            ));
        }
    }
    Ok(())
}

/// An owner-only executable copy of the exact adapter bytes that passed lock
/// verification. The random owner-only directory and non-writable file remain
/// alive through adapter exit because shebang interpreters may reopen the
/// script path after `Command::spawn` returns.
#[cfg(unix)]
struct VerifiedExecutableSnapshot {
    directory: Option<PathBuf>,
    path: PathBuf,
}

#[cfg(unix)]
impl VerifiedExecutableSnapshot {
    fn stage(entry_path: &Path, entry_bytes: &[u8]) -> Result<Self, ShellError> {
        const CREATE_ATTEMPTS: usize = 16;
        let temporary_root = env::temp_dir();
        let mut directory = None;
        for _ in 0..CREATE_ATTEMPTS {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|error| {
                ShellError::new(
                    ErrorCode::Io,
                    "could not generate a private adapter snapshot name",
                )
                .with_context(error.to_string())
                .with_help("Retry adapter activation; report repeated random-source failures")
            })?;
            let suffix = random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let candidate = temporary_root.join(format!("quirl-adapter-{suffix}"));
            match fs::DirBuilder::new().mode(0o700).create(&candidate) {
                Ok(()) => {
                    directory = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io_error(
                        "could not create a private adapter snapshot directory",
                        error,
                        "Check temporary-directory permissions and retry activation",
                    ));
                }
            }
        }
        let directory = directory.ok_or_else(|| {
            ShellError::new(
                ErrorCode::Io,
                "could not allocate a unique adapter snapshot directory",
            )
            .with_help("Clear stale temporary files and retry adapter activation")
        })?;
        let name = entry_path
            .file_name()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| std::ffi::OsStr::new("adapter"));
        let path = directory.join(name);
        let staged = (|| {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o700)
                .open(&path)
                .map_err(|error| {
                    io_error(
                        "could not create the verified adapter snapshot",
                        error,
                        "Check temporary-directory permissions and retry activation",
                    )
                })?;
            file.write_all(entry_bytes).map_err(|error| {
                io_error(
                    "could not write the verified adapter snapshot",
                    error,
                    "Check temporary storage capacity and retry activation",
                )
            })?;
            file.sync_all().map_err(|error| {
                io_error(
                    "could not sync the verified adapter snapshot",
                    error,
                    "Check temporary storage health and retry activation",
                )
            })?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).map_err(|error| {
                io_error(
                    "could not make the verified adapter snapshot non-writable",
                    error,
                    "Check temporary filesystem permission support and retry activation",
                )
            })?;
            Ok(Self {
                directory: Some(directory.clone()),
                path: path.clone(),
            })
        })();
        if staged.is_err() {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir(&directory);
        }
        staged
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
impl Drop for VerifiedExecutableSnapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        if let Some(directory) = self.directory.take() {
            let _ = fs::remove_dir(directory);
        }
    }
}

/// Execute the narrow v1 initialization handshake for a locked process
/// adapter. This lives in the CLI composition root because it owns process
/// creation; `quirl-plugin` owns the typed protocol and boundary validation.
pub(crate) fn execute_out_of_process_adapter(
    manifest: &PluginManifest,
    entry_path: &Path,
    entry_bytes: &[u8],
    grants: &[String],
    cancellation: Option<&AtomicBool>,
) -> Result<(), ShellError> {
    validate_plugin_manifest(manifest, entry_bytes, env!("CARGO_PKG_VERSION"))?;
    let adapter = manifest.adapter.as_ref().ok_or_else(|| {
        ShellError::new(
            ErrorCode::Validation,
            "out-of-process adapter boundary is absent",
        )
        .with_help("Restore a plugin.toml with a valid [adapter] section")
    })?;
    let required_grant = format!("process.spawn:{}", adapter.executable);
    if grants != [required_grant.clone()] {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "out-of-process adapter does not have its exact locked launch grant",
        )
        .with_context(format!("required grant: {required_grant}"))
        .with_context(format!("locked grants: {grants:?}"))
        .with_help("Re-add the plugin and approve only its scoped process.spawn capability"));
    }
    if manifest.plugin.entry != adapter.executable || adapter.protocol != ADAPTER_PROTOCOL {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "out-of-process adapter boundary no longer matches its executable contract",
        )
        .with_help("Restore the locked plugin manifest and run plugin doctor"));
    }
    let request = AdapterInitializeRequest::new(
        manifest.plugin.name.clone(),
        manifest.plugin.version.clone(),
    );
    let request = serde_json::to_vec(&request).map_err(|error| {
        ShellError::new(
            ErrorCode::Io,
            "cannot serialize adapter initialization request",
        )
        .with_context(error.to_string())
        .with_help("Report this as an adapter protocol defect")
    })?;
    let message_bytes_max = usize::try_from(adapter.max_message_bytes).map_err(|_| {
        adapter_limit_error(
            "adapter message limit cannot be represented on this host",
            adapter.max_message_bytes,
        )
    })?;
    if request.len() > message_bytes_max {
        return Err(adapter_limit_error(
            "initialization request exceeds the adapter message limit",
            adapter.max_message_bytes,
        ));
    }

    let package_root = entry_path.parent().unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    let executable_snapshot = VerifiedExecutableSnapshot::stage(entry_path, entry_bytes)?;
    #[cfg(unix)]
    let executable_path = executable_snapshot.path();
    #[cfg(not(unix))]
    let executable_path = {
        let _ = entry_bytes;
        entry_path
    };
    let mut command = Command::new(executable_path);
    command
        .args(&adapter.arguments)
        .current_dir(package_root)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let containment = ChildProcessTree::new()?;
    containment.configure(&mut command);
    let mut child = command.spawn().map_err(|error| {
        ShellError::new(
            ErrorCode::ProcessSpawn,
            format!(
                "could not start isolated adapter `{}`",
                manifest.plugin.name
            ),
        )
        .with_context(error.to_string())
        .with_help(
            "Ensure temporary storage permits execution and the locked adapter is a self-contained relocatable executable, then run plugin doctor",
        )
    })?;
    containment.assign(&mut child).map_err(|error| {
        let _ = child.kill();
        let _ = child.wait();
        error.with_help("Retry after restoring the adapter; process containment is required")
    })?;
    let output_budget = Arc::new(AdapterOutputBudget::new(message_bytes_max));
    let stdout = child
        .stdout
        .take()
        .map(|stream| spawn_adapter_reader(stream, output_budget.clone()));
    let stderr = child
        .stderr
        .take()
        .map(|stream| spawn_adapter_reader(stream, output_budget));
    let write_result = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(&request)
            .and_then(|()| stdin.write_all(b"\n")),
        _ => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "adapter stdin pipe is unavailable",
        )),
    };
    if let Err(error) = write_result {
        let termination = terminate_adapter(&mut child, &containment);
        let _ = join_adapter_reader(stdout, "adapter stdout");
        let _ = join_adapter_reader(stderr, "adapter stderr");
        termination?;
        return Err(ShellError::new(
            ErrorCode::Io,
            "could not send initialization request to isolated adapter",
        )
        .with_context(error.to_string())
        .with_help("Ensure the adapter accepts one JSON request on standard input"));
    }

    let deadline = Instant::now()
        .checked_add(Duration::from_millis(adapter.callback_timeout_ms))
        .ok_or_else(|| {
            ShellError::new(
                ErrorCode::ResourceLimit,
                "isolated adapter deadline exceeds the platform clock range",
            )
            .with_context(format!("timeout: {} ms", adapter.callback_timeout_ms))
            .with_help("Configure a smaller adapter callback timeout")
        })?;
    let status = loop {
        if cancellation.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            let termination = terminate_adapter(&mut child, &containment);
            let _ = join_adapter_reader(stdout, "adapter stdout");
            let _ = join_adapter_reader(stderr, "adapter stderr");
            termination?;
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "isolated adapter initialization was cancelled",
            )
            .with_help("Retry when cancellation is no longer requested"));
        }
        if Instant::now() >= deadline {
            let termination = terminate_adapter(&mut child, &containment);
            let _ = join_adapter_reader(stdout, "adapter stdout");
            let _ = join_adapter_reader(stderr, "adapter stderr");
            termination?;
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "isolated adapter initialization exceeded its callback deadline",
            )
            .with_context(format!("deadline: {} ms", adapter.callback_timeout_ms))
            .with_help("Reduce initialization work or raise callback_timeout_ms after review"));
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => thread::sleep(Duration::from_millis(1)),
            Err(error) => {
                let termination = terminate_adapter(&mut child, &containment);
                let _ = join_adapter_reader(stdout, "adapter stdout");
                let _ = join_adapter_reader(stderr, "adapter stderr");
                termination?;
                return Err(ShellError::new(
                    ErrorCode::Io,
                    "could not observe isolated adapter status",
                )
                .with_context(error.to_string())
                .with_help("Retry the adapter; report repeated process observation failures"));
            }
        }
    };
    let stdout = join_adapter_reader(stdout, "adapter stdout")?;
    let stderr = join_adapter_reader(stderr, "adapter stderr")?;
    let discarded_bytes = stdout
        .discarded_bytes
        .saturating_add(stderr.discarded_bytes);
    if discarded_bytes > 0 {
        return Err(adapter_limit_error(
            "isolated adapter exceeded its output message limit",
            adapter.max_message_bytes,
        )
        .with_context(format!(
            "retained {} bytes; discarded {discarded_bytes} bytes",
            stdout.bytes.len().saturating_add(stderr.bytes.len())
        )));
    }
    if !status.success() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "isolated adapter initialization failed",
        )
        .with_context(format!("exit status: {}", status.code().unwrap_or(1)))
        .with_context(truncate_adapter_diagnostic(&stderr.bytes))
        .with_help("Make the adapter return a ready response and exit successfully")
        .with_help(
            "Adapters run from a private relocated snapshot; resolve sidecars from the package working directory rather than from the executable path",
        ));
    }
    if !stderr.bytes.is_empty() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "isolated adapter wrote unexpected diagnostic output during initialization",
        )
        .with_context(truncate_adapter_diagnostic(&stderr.bytes))
        .with_help(
            "Return protocol data only on stdout and reserve a clean initialization for activation",
        ));
    }
    let response_bytes = newline_delimited_adapter_response(&stdout.bytes)?;
    let response =
        serde_json::from_slice::<AdapterInitializeResponse>(response_bytes).map_err(|error| {
            ShellError::new(
                ErrorCode::Validation,
                "isolated adapter returned an invalid initialization response",
            )
            .with_context(error.to_string())
            .with_help("Return one deny-unknown JSON AdapterInitializeResponse object on stdout")
        })?;
    response.validate_for(&AdapterInitializeRequest::new(
        manifest.plugin.name.clone(),
        manifest.plugin.version.clone(),
    ))
}

#[derive(Debug)]
struct AdapterOutput {
    bytes: Vec<u8>,
    discarded_bytes: u64,
}

#[derive(Debug)]
struct AdapterOutputBudget {
    limit: usize,
    consumed: AtomicUsize,
}

impl AdapterOutputBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            consumed: AtomicUsize::new(0),
        }
    }

    fn claim(&self, requested: usize) -> usize {
        let mut consumed = self.consumed.load(Ordering::Relaxed);
        loop {
            let claimed = requested.min(self.limit.saturating_sub(consumed));
            match self.consumed.compare_exchange_weak(
                consumed,
                consumed.saturating_add(claimed),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return claimed,
                Err(observed) => consumed = observed,
            }
        }
    }
}

fn spawn_adapter_reader(
    mut stream: impl Read + Send + 'static,
    budget: Arc<AdapterOutputBudget>,
) -> thread::JoinHandle<io::Result<AdapterOutput>> {
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(budget.limit.min(8 * 1024));
        let mut discarded_bytes = 0_u64;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let retained = budget.claim(read);
            bytes.extend_from_slice(buffer.get(..retained).unwrap_or_default());
            discarded_bytes = discarded_bytes
                .saturating_add(u64::try_from(read.saturating_sub(retained)).unwrap_or(u64::MAX));
        }
        Ok(AdapterOutput {
            bytes,
            discarded_bytes,
        })
    })
}

fn newline_delimited_adapter_response(bytes: &[u8]) -> Result<&[u8], ShellError> {
    let Some(response) = bytes.strip_suffix(b"\n") else {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "isolated adapter response is not newline-delimited",
        )
        .with_help("Write exactly one JSON response followed by a single newline"));
    };
    if response.is_empty() || response.contains(&b'\n') || response.contains(&b'\r') {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "isolated adapter emitted multiple or malformed initialization responses",
        )
        .with_help("Write exactly one compact JSON response followed by a single newline"));
    }
    Ok(response)
}

fn join_adapter_reader(
    reader: Option<thread::JoinHandle<io::Result<AdapterOutput>>>,
    description: &str,
) -> Result<AdapterOutput, ShellError> {
    let Some(reader) = reader else {
        return Ok(AdapterOutput {
            bytes: Vec::new(),
            discarded_bytes: 0,
        });
    };
    match reader.join() {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(ShellError::new(
            ErrorCode::Io,
            format!("could not read {description}"),
        )
        .with_context(error.to_string())
        .with_help("Retry the adapter; report repeated output capture failures")),
        Err(_) => Err(
            ShellError::new(ErrorCode::Io, format!("{description} reader task failed"))
                .with_help("Retry the adapter; report repeated output capture failures"),
        ),
    }
}

fn terminate_adapter(
    child: &mut std::process::Child,
    containment: &ChildProcessTree,
) -> Result<(), ShellError> {
    let result = containment.terminate(child);
    if result.is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
    result
}

fn adapter_limit_error(message: &str, limit: u64) -> ShellError {
    ShellError::new(ErrorCode::ResourceLimit, message)
        .with_context(format!("message limit: {limit} bytes"))
        .with_help("Reduce adapter input/output or raise max_message_bytes after review")
}

fn truncate_adapter_diagnostic(bytes: &[u8]) -> String {
    const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;
    String::from_utf8_lossy(
        bytes
            .get(..bytes.len().min(MAX_DIAGNOSTIC_BYTES))
            .unwrap_or_default(),
    )
    .into_owned()
}

fn contribution_names(
    registrations: &[quirl_core::ContributionRegistration],
    kind: ContributionKind,
) -> Vec<&str> {
    registrations
        .iter()
        .filter(|item| item.kind == kind)
        .map(|item| item.name.as_str())
        .collect()
}

fn check_legacy(path: &Path, format: PluginOutputFormat) -> Result<i32, ShellError> {
    let lua = LuaRuntime::new(LuaPolicy::config())?;
    let registrations = lua.load_plugin_file(path)?;
    match format {
        PluginOutputFormat::Json => print_json(&registrations)?,
        PluginOutputFormat::Text => println!(
            "✓ {} registered {} commands, {} completions, {} events, and {} prompt segments",
            escape_terminal_controls(&path.display().to_string()),
            registrations.commands.len(),
            registrations.completion_providers.len(),
            registrations.events.len(),
            registrations.prompt_segments.len()
        ),
    }
    Ok(0)
}

fn load_lock(root: &Path) -> Result<PluginLockfile, ShellError> {
    let path = root.join(PLUGIN_LOCK_FILE);
    let options = plugin_read_options(
        &path,
        MAX_LOCK_BYTES,
        "plugin lockfile",
        "Repair the lockfile as a readable regular file at or below 4 MiB, or restore its backup",
    );
    let Some(bytes) = read_optional_regular_file(options)? else {
        return Ok(PluginLockfile::empty());
    };
    PluginLockfile::from_json(&bytes)
        .map_err(|error| error.with_context(format!("lockfile: {}", path.display())))
}

fn create_plugin_state_directory(root: &Path) -> Result<(), ShellError> {
    const DEPTH_MAX: usize = 64;
    let depth = root.components().take(DEPTH_MAX.saturating_add(1)).count();
    if depth > DEPTH_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "plugin state path exceeds its depth limit",
        )
        .with_context(format!("limit: {DEPTH_MAX}; observed: at least {depth}"))
        .with_help("Choose a shorter QUIRL_PLUGIN_HOME path"));
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(root).map_err(|error| {
        io_error(
            "cannot create plugin state directory",
            error,
            "Choose a private writable QUIRL_PLUGIN_HOME",
        )
    })?;
    let metadata = fs::symlink_metadata(root).map_err(|error| {
        io_error(
            "cannot inspect plugin state directory",
            error,
            "Choose a private writable QUIRL_PLUGIN_HOME",
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "plugin state must be a real directory",
        )
        .with_help("Replace the plugin state symlink or special file with a private directory"));
    }
    Ok(())
}

fn acquire_plugin_lock(root: &Path) -> Result<CoordinationGuard, ShellError> {
    create_plugin_state_directory(root)?;
    coordination::acquire(
        &root.join(PLUGIN_LOCK_FILE),
        CoordinationKind::Plugin,
        CoordinationWait::Explicit,
    )?
    .ok_or_else(|| {
        ShellError::new(
            ErrorCode::ResourceLimit,
            "plugin lockfile coordination remained busy",
        )
        .with_help("Wait for the other plugin command to finish and retry")
    })
}

// Mutation callers hold acquire_plugin_lock across their read-modify-write.
fn save_lock(root: &Path, lock: &PluginLockfile) -> Result<(), ShellError> {
    save_lock_with_hook(root, lock, || Ok(()))
}

fn save_lock_with_hook(
    root: &Path,
    lock: &PluginLockfile,
    after_backup: impl FnOnce() -> io::Result<()>,
) -> Result<(), ShellError> {
    lock.validate()?;
    let mut writer = LockBytesWriter {
        bytes: Vec::new(),
        exceeded: false,
    };
    serde_json::to_writer_pretty(&mut writer, lock).map_err(|error| {
        if writer.exceeded {
            return lock_bytes_limit_error(MAX_LOCK_BYTES.saturating_add(1));
        }
        io_error(
            "cannot serialize plugin lockfile",
            io::Error::other(error),
            "Report this as a plugin platform schema defect",
        )
    })?;
    create_plugin_state_directory(root)?;
    ensure_lock_transactions_clear(root)?;
    let path = root.join(PLUGIN_LOCK_FILE);
    let backup = root.join(format!("{PLUGIN_LOCK_FILE}.bak"));
    let expected = read_optional_regular_file(plugin_read_options(
        &path,
        MAX_LOCK_BYTES,
        "plugin lockfile",
        "Restore a valid regular plugin lockfile before retrying",
    ))?;
    if let Some(previous) = expected {
        PluginLockfile::from_json(&previous)?;
        publish_lock_bytes(&backup, &previous)?;
        after_backup().map_err(|error| {
            io_error(
                "plugin lock update stopped after saving backup",
                error,
                "The previous lock remains active; retry the update",
            )
        })?;
        replace_file_atomically(
            &path,
            &previous,
            &writer.bytes,
            AtomicReplaceOptions {
                bytes_max: MAX_LOCK_BYTES,
            },
        )
    } else {
        publish_lock_bytes(&path, &writer.bytes)
    }
}

fn ensure_lock_transactions_clear(root: &Path) -> Result<(), ShellError> {
    let entries = fs::read_dir(root).map_err(|error| {
        io_error(
            "cannot inspect plugin lock transactions",
            error,
            "Repair the plugin state directory",
        )
    })?;
    let prefixes = [
        format!(".{PLUGIN_LOCK_FILE}.tmp-"),
        format!(".{PLUGIN_LOCK_FILE}.bak.tmp-"),
        format!(".{PLUGIN_LOCK_FILE}.quirl-format-"),
        format!(".{PLUGIN_LOCK_FILE}.bak.quirl-format-"),
    ];
    for (index, entry) in entries.enumerate() {
        if index >= LOCK_DIRECTORY_ENTRIES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "plugin state directory exceeds its entry limit",
            )
            .with_context(format!(
                "limit: {LOCK_DIRECTORY_ENTRIES_MAX}; observed: at least {}",
                index.saturating_add(1)
            ))
            .with_help("Move unrelated files out of the plugin state directory and retry"));
        }
        let entry = entry.map_err(|error| {
            io_error(
                "cannot inspect plugin lock transaction entry",
                error,
                "Repair the plugin state directory",
            )
        })?;
        let name = entry.file_name();
        if prefixes
            .iter()
            .any(|prefix| name.to_string_lossy().starts_with(prefix))
        {
            return Err(ShellError::new(ErrorCode::ResourceLimit, "plugin lock has an unfinished transaction")
                .with_context(format!("pending transaction limit: 0; observed: at least 1; preserved: {}", entry.path().display()))
                .with_help("Review and move preserved lock transaction files aside before retrying; keep the active lock and backup"));
        }
    }
    Ok(())
}

struct LockBytesWriter {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for LockBytesWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > MAX_LOCK_BYTES {
            self.exceeded = true;
            return Err(io::Error::other("plugin lockfile byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn lock_bytes_limit_error(observed: usize) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        "plugin lockfile exceeds its byte limit",
    )
    .with_context(format!("limit: {MAX_LOCK_BYTES}; observed: {observed}"))
    .with_help("Remove unused plugin records before retrying")
}

fn publish_lock_bytes(path: &Path, bytes: &[u8]) -> Result<(), ShellError> {
    publish_lock_bytes_with_hook(path, bytes, || Ok(()))
}

fn publish_lock_bytes_with_hook(
    path: &Path,
    bytes: &[u8],
    before_publish: impl FnOnce() -> io::Result<()>,
) -> Result<(), ShellError> {
    let previous = read_optional_regular_file(plugin_read_options(
        path,
        MAX_LOCK_BYTES,
        "plugin lockfile document",
        "Use a regular plugin lock or backup without links",
    ))?;
    if let Some(previous) = previous {
        return replace_file_atomically(
            path,
            &previous,
            bytes,
            AtomicReplaceOptions {
                bytes_max: MAX_LOCK_BYTES,
            },
        );
    }
    let (temporary, mut file) = create_lock_temporary(path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        validate_lock_temporary(&temporary, &file, 1)?;
        before_publish()?;
        // The final name is never overwritten when another entry appears.
        fs::hard_link(&temporary, path)?;
        validate_lock_temporary(path, &file, 2)?;
        validate_lock_temporary(&temporary, &file, 2)?;
        fs::remove_file(&temporary)?;
        Ok::<(), io::Error>(())
    })();
    result.map_err(|error| {
        io_error(
            format!("cannot publish plugin lock document {}", path.display()),
            error,
            "Review preserved transaction files before retrying",
        )
        .with_context(format!(
            "preserved transaction temporary: {}",
            temporary.display()
        ))
    })?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

fn validate_lock_temporary(path: &Path, file: &fs::File, links_expected: u64) -> io::Result<()> {
    let named = fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    if !named.file_type().is_file() || !opened.file_type().is_file() {
        return Err(io::Error::other(
            "plugin lock temporary is not a regular file",
        ));
    }
    #[cfg(unix)]
    if named.dev() != opened.dev()
        || named.ino() != opened.ino()
        || opened.nlink() != links_expected
    {
        return Err(io::Error::other(
            "plugin lock temporary changed during publication",
        ));
    }
    #[cfg(not(unix))]
    let _ = links_expected;
    Ok(())
}

fn create_lock_temporary(path: &Path) -> Result<(PathBuf, fs::File), ShellError> {
    let name = path.file_name().ok_or_else(|| {
        ShellError::new(
            ErrorCode::InvalidArgument,
            "plugin lock path has no file name",
        )
        .with_help("Use a nested plugin state directory")
    })?;
    for _ in 0..LOCK_TEMPORARY_ATTEMPTS_MAX {
        let sequence = LOCK_TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = std::ffi::OsString::from(".");
        temporary_name.push(name);
        temporary_name.push(format!(".tmp-{}-{sequence}", std::process::id()));
        let temporary = path.with_file_name(temporary_name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temporary) {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(io_error(
                    "cannot create private plugin lock temporary",
                    error,
                    "Check available disk space and directory permissions",
                ));
            }
        }
    }
    Err(ShellError::new(
        ErrorCode::ResourceLimit,
        "plugin lock temporary creation exhausted its attempt limit",
    )
    .with_context(format!("limit: {LOCK_TEMPORARY_ATTEMPTS_MAX}"))
    .with_help("Remove stale plugin lock temporary entries before retrying"))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ShellError> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            io_error(
                format!("cannot sync plugin state directory {}", path.display()),
                error,
                "Use a local filesystem that supports durable directory updates",
            )
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), ShellError> {
    // Rust's portable File API cannot open directories on Windows. The file
    // contents are still flushed before the replace and rollback is preserved.
    Ok(())
}

fn plugin_root() -> Result<PathBuf, ShellError> {
    crate::extensions::resolve_plugin_state_directory(
        env::var_os("QUIRL_PLUGIN_HOME"),
        env::var_os("QUIRL_CONFIG_DIR"),
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("HOME"),
    )
    .ok_or_else(|| {
        ShellError::new(ErrorCode::Io, "cannot determine plugin state directory")
            .with_help("Set QUIRL_PLUGIN_HOME or QUIRL_CONFIG_DIR to a writable directory")
    })
}

fn safe_package_path(path: &str) -> Result<PathBuf, ShellError> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || !path
            .components()
            .all(|item| matches!(item, Component::Normal(_) | Component::CurDir))
    {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "plugin entry escapes its package directory",
        )
        .with_help("Use a relative entry path without parent components"));
    }
    Ok(path.to_owned())
}

fn read_bounded(
    path: &Path,
    limit: usize,
    context: &str,
    help: &str,
) -> Result<Vec<u8>, ShellError> {
    read_regular_file(plugin_read_options(path, limit, context, help))
}

fn plugin_read_options<'a>(
    path: &'a Path,
    limit: usize,
    context: &'a str,
    help: &'a str,
) -> ReadFileOptions<'a> {
    ReadFileOptions {
        path,
        bytes_max: limit,
        context,
        help,
        io_error_code: ErrorCode::Io,
    }
}

fn package_escape_error(context: &str) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("{context} resolves outside its package directory"),
    )
    .with_help(
        "Keep the manifest and entry inside the package; external symlink targets are rejected",
    )
}

fn print_mutation(
    action: &'static str,
    plugin: &str,
    diff: Option<&PermissionDiff>,
    lock: &PluginLockfile,
    format: PluginOutputFormat,
) -> Result<(), ShellError> {
    let output = MutationOutput {
        document_type: "quirl.plugin.mutation",
        schema_version: 1,
        action,
        plugin,
        permission_diff: diff,
        lockfile: lock,
    };
    match format {
        PluginOutputFormat::Json => print_json(&output),
        PluginOutputFormat::Text => {
            println!("✓ {action} plugin {}", escape_terminal_controls(plugin));
            if let Some(diff) = diff {
                println!("  permissions added: {}", list_or_none(&diff.added));
                println!("  permissions removed: {}", list_or_none(&diff.removed));
            }
            Ok(())
        }
    }
}

fn print_permissions(
    output: &PermissionsOutput<'_>,
    format: PluginOutputFormat,
) -> Result<(), ShellError> {
    match format {
        PluginOutputFormat::Json => print_json(output),
        PluginOutputFormat::Text => {
            println!(
                "Permissions for {}",
                escape_terminal_controls(output.plugin)
            );
            println!("  requested: {}", list_or_none(output.requested));
            println!("  granted: {}", list_or_none(output.granted));
            println!(
                "  ungranted additions: {}",
                list_or_none(&output.diff.added)
            );
            Ok(())
        }
    }
}

fn print_doctor(report: &DoctorReport, format: PluginOutputFormat) -> Result<(), ShellError> {
    match format {
        PluginOutputFormat::Json => print_json(report),
        PluginOutputFormat::Text => {
            println!(
                "{} plugin {}",
                if report.healthy {
                    "✓ healthy"
                } else {
                    "✗ unhealthy"
                },
                escape_terminal_controls(&report.plugin)
            );
            for diagnostic in &report.diagnostics {
                println!(
                    "  {}: {}\n    help: {}",
                    escape_terminal_controls(&diagnostic.code),
                    escape_terminal_controls(&diagnostic.message),
                    escape_terminal_controls(&diagnostic.help)
                );
            }
            Ok(())
        }
    }
}

fn print_json(value: &impl Serialize) -> Result<(), ShellError> {
    let json = serde_json::to_string_pretty(value).map_err(|error| {
        ShellError::new(ErrorCode::Io, "cannot serialize plugin platform output")
            .with_context(error.to_string())
            .with_help("Report this as a plugin platform schema defect")
    })?;
    println!("{}", escape_json_terminal_controls(&json));
    Ok(())
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        escape_terminal_controls(&values.join(", "))
    }
}

fn unhealthy_error(report: &DoctorReport) -> ShellError {
    ShellError::new(
        ErrorCode::Validation,
        format!("plugin `{}` failed doctor checks", report.plugin),
    )
    .with_context(
        report
            .diagnostics
            .iter()
            .map(|item| format!("{}: {}", item.code, item.message))
            .collect::<Vec<_>>()
            .join("; "),
    )
    .with_help("Restore the locked source or explicitly remove and re-add it after review")
}

fn io_error(
    message: impl Into<String>,
    error: std::io::Error,
    help: impl Into<String>,
) -> ShellError {
    ShellError::new(ErrorCode::Io, message)
        .with_context(error.to_string())
        .with_help(help)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_package_directory(name: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "quirl-plugin-package-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ))
    }

    fn trusted_manifest(entry: &str) -> String {
        let host_version = env!("CARGO_PKG_VERSION");
        format!(
            r#"schema_version = 2
[plugin]
name = "bounded"
version = "0.1.0"
entry = "{entry}"
quirl = "={host_version}"
api = "0.1.0"
runtime = "trusted_lua"
summary = "Bounded plugin"
"#
        )
    }

    fn test_locked_plugin(name: &str) -> quirl_plugin::LockedPlugin {
        let source = trusted_manifest("plugin.lua")
            .replace("name = \"bounded\"", &format!("name = \"{name}\""));
        let manifest = parse_plugin_manifest(&source, "test.toml").unwrap();
        resolve_plugin(
            &manifest,
            source.as_bytes(),
            b"return {}",
            "file:test.toml",
            &[],
            env!("CARGO_PKG_VERSION"),
        )
        .unwrap()
        .0
    }

    #[cfg(unix)]
    #[test]
    fn predictable_lock_temporary_symlink_never_truncates_external_file() {
        use std::os::unix::fs::symlink;
        let directory = test_package_directory("temporary-link");
        fs::create_dir_all(&directory).unwrap();
        let external = directory.join("external-data");
        fs::write(&external, b"preserve user data").unwrap();
        let legacy_temporary =
            directory.join(format!(".{PLUGIN_LOCK_FILE}.tmp-{}", std::process::id()));
        let lock = PluginLockfile::empty();
        save_lock(&directory, &lock).unwrap();
        symlink(&external, &legacy_temporary).unwrap();
        for _ in 0..3 {
            let error = save_lock(&directory, &lock).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 3);
        }
        assert_eq!(fs::read(&external).unwrap(), b"preserve user data");
        assert!(
            legacy_temporary
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(load_lock(&directory).unwrap(), lock);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_first_publication_preserves_foreign_entry_and_blocks_further_staging() {
        let directory = test_package_directory("failed-first-save");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(PLUGIN_LOCK_FILE);
        let lock = PluginLockfile::empty();
        let bytes = serde_json::to_vec(&lock).unwrap();
        let error =
            publish_lock_bytes_with_hook(&path, &bytes, || fs::write(&path, b"concurrent entry"))
                .unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert_eq!(fs::read(&path).unwrap(), b"concurrent entry");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        for _ in 0..3 {
            let error = save_lock(&directory, &lock).unwrap_err();
            assert_eq!(error.code, ErrorCode::ResourceLimit);
            assert_eq!(fs::read_dir(&directory).unwrap().count(), 2);
        }
        assert_eq!(fs::read(&path).unwrap(), b"concurrent entry");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transaction_admission_accepts_entry_limit_and_rejects_one_more() {
        let directory = test_package_directory("transaction-entry-limit");
        fs::create_dir_all(&directory).unwrap();
        for index in 0..LOCK_DIRECTORY_ENTRIES_MAX {
            fs::write(directory.join(format!("unrelated-{index}")), b"").unwrap();
        }
        ensure_lock_transactions_clear(&directory).unwrap();
        fs::write(directory.join("one-more"), b"").unwrap();
        let error = ensure_lock_transactions_clear(&directory).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn interrupted_save_keeps_previous_lock_readable_and_preserves_backup() {
        let directory = test_package_directory("interrupted-save");
        let previous = PluginLockfile::empty();
        save_lock(&directory, &previous).unwrap();
        let candidate = previous.install(test_locked_plugin("new-plugin")).unwrap();
        let error = save_lock_with_hook(&directory, &candidate, || {
            assert_eq!(load_lock(&directory).unwrap(), previous);
            Err(io::Error::other("injected failure after backup"))
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Io);
        assert_eq!(load_lock(&directory).unwrap(), previous);
        let backup = read_bounded(
            &directory.join(format!("{PLUGIN_LOCK_FILE}.bak")),
            MAX_LOCK_BYTES,
            "backup",
            "test",
        )
        .unwrap();
        assert_eq!(PluginLockfile::from_json(&backup).unwrap(), previous);
        save_lock(&directory, &candidate).unwrap();
        assert_eq!(load_lock(&directory).unwrap(), candidate);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn redirected_backup_is_rejected_without_changing_active_lock_or_target() {
        use std::os::unix::fs::symlink;
        let directory = test_package_directory("backup-link");
        let previous = PluginLockfile::empty();
        save_lock(&directory, &previous).unwrap();
        let external = directory.join("external-data");
        fs::write(&external, b"preserved backup target").unwrap();
        symlink(&external, directory.join(format!("{PLUGIN_LOCK_FILE}.bak"))).unwrap();
        let candidate = previous.install(test_locked_plugin("new-plugin")).unwrap();
        assert!(save_lock(&directory, &candidate).is_err());
        assert_eq!(fs::read(&external).unwrap(), b"preserved backup target");
        assert_eq!(load_lock(&directory).unwrap(), previous);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn oversized_serialization_preserves_previous_lock_and_creates_no_backup() {
        let directory = test_package_directory("oversized-lock");
        let previous = PluginLockfile::empty();
        save_lock(&directory, &previous).unwrap();
        let mut plugin = test_locked_plugin("large-plugin");
        plugin.source = "x".repeat(MAX_LOCK_BYTES + 1);
        let candidate = previous.install(plugin).unwrap();
        let error = save_lock(&directory, &candidate).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert_eq!(load_lock(&directory).unwrap(), previous);
        assert!(!directory.join(format!("{PLUGIN_LOCK_FILE}.bak")).exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_plugin_mutations_preserve_both_installed_entries() {
        let directory = test_package_directory("concurrent-save");
        fs::create_dir_all(&directory).unwrap();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let threads = ["first-plugin", "second-plugin"].map(|name| {
            let directory = directory.clone();
            let ready = Arc::clone(&ready);
            thread::spawn(move || {
                ready.wait();
                let _guard = acquire_plugin_lock(&directory).unwrap();
                let candidate = load_lock(&directory)
                    .unwrap()
                    .install(test_locked_plugin(name))
                    .unwrap();
                save_lock(&directory, &candidate).unwrap();
            })
        });
        for thread in threads {
            thread.join().unwrap();
        }
        let lock = load_lock(&directory).unwrap();
        assert_eq!(lock.plugins.len(), 2);
        assert_eq!(lock.plugins[0].name, "first-plugin");
        assert_eq!(lock.plugins[1].name, "second-plugin");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_candidate_never_replaces_last_known_good_lock() {
        let directory = env::temp_dir().join(format!(
            "quirl-plugin-state-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir_all(&directory).unwrap();
        let good = PluginLockfile::empty();
        save_lock(&directory, &good).unwrap();
        let mut invalid = good.clone();
        invalid.schema_hash = "tampered".to_owned();
        assert!(save_lock(&directory, &invalid).is_err());
        assert_eq!(load_lock(&directory).unwrap(), good);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn durable_replace_preserves_a_valid_previous_lock_backup() {
        let directory = env::temp_dir().join(format!(
            "quirl-plugin-backup-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir_all(&directory).unwrap();
        let lock = PluginLockfile::empty();
        save_lock(&directory, &lock).unwrap();
        save_lock(&directory, &lock).unwrap();
        let backup = fs::read(directory.join(format!("{PLUGIN_LOCK_FILE}.bak"))).unwrap();
        let decoded: PluginLockfile = serde_json::from_slice(&backup).unwrap();
        assert_eq!(decoded, lock);
        assert!(
            !directory
                .join(format!(".{PLUGIN_LOCK_FILE}.tmp-{}", std::process::id()))
                .exists()
        );
        fs::remove_dir_all(directory).unwrap();
    }

    // These fixtures assert protocol, relocation, permission, and output-limit
    // behavior. Their real child processes need scheduling headroom on loaded
    // hosts; the dedicated 5 ms deadline fixture below keeps its own budget.
    #[cfg(unix)]
    const ADAPTER_FUNCTIONAL_TIMEOUT_MS: u64 = 5_000;
    #[cfg(unix)]
    const _: () =
        assert!(ADAPTER_FUNCTIONAL_TIMEOUT_MS <= quirl_plugin::MAX_ADAPTER_CALLBACK_TIMEOUT_MS);

    #[cfg(unix)]
    fn write_isolated_adapter(
        directory: &Path,
        body: &str,
        timeout_ms: u64,
        max_bytes: u64,
    ) -> PluginManifest {
        use std::os::unix::fs::PermissionsExt;

        let entry = directory.join("adapter");
        fs::write(&entry, body).unwrap();
        let mut permissions = fs::metadata(&entry).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&entry, permissions).unwrap();
        let host_version = env!("CARGO_PKG_VERSION");
        let source = format!(
            r#"schema_version = 2
[plugin]
name = "adapter"
version = "0.1.0"
entry = "adapter"
quirl = "={host_version}"
api = "0.1.0"
runtime = "out_of_process"
summary = "Isolation adapter contract"
[capabilities]
request = ["process.spawn:adapter"]
[adapter]
protocol = "quirl.plugin.v1"
executable = "adapter"
arguments = []
callback_timeout_ms = {timeout_ms}
max_message_bytes = {max_bytes}
"#
        );
        parse_plugin_manifest(&source, "plugin.toml").unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn isolated_adapter_executes_a_bounded_versioned_handshake() {
        let directory = env::temp_dir().join(format!(
            "quirl-plugin-adapter-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir_all(&directory).unwrap();
        let manifest = write_isolated_adapter(
            &directory,
            "#!/bin/sh\nread request\nprintf '%s\\n' '{\"protocol\":\"quirl.plugin.v1\",\"schema_version\":1,\"api_version\":\"0.1.0\",\"operation\":\"initialize\",\"status\":\"ready\"}'\n",
            ADAPTER_FUNCTIONAL_TIMEOUT_MS,
            65_536,
        );
        let entry = directory.join("adapter");
        let entry_bytes = fs::read(&entry).unwrap();
        assert!(validate_runtime(&manifest, &entry, &entry_bytes, &[], false).is_ok());
        let result = validate_runtime(
            &manifest,
            &entry,
            &entry_bytes,
            &["process.spawn:adapter".to_owned()],
            true,
        );
        assert!(result.is_ok(), "{result:?}");

        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn isolated_adapter_executes_the_verified_snapshot_after_package_replacement() {
        let directory = test_package_directory("adapter-snapshot");
        fs::create_dir_all(&directory).unwrap();
        let manifest = write_isolated_adapter(
            &directory,
            "#!/bin/sh\nread request\nprintf '%s\\n' '{\"protocol\":\"quirl.plugin.v1\",\"schema_version\":1,\"api_version\":\"0.1.0\",\"operation\":\"initialize\",\"status\":\"ready\"}'\n",
            ADAPTER_FUNCTIONAL_TIMEOUT_MS,
            65_536,
        );
        let entry = directory.join("adapter");
        let verified_bytes = fs::read(&entry).unwrap();
        fs::write(&entry, "#!/bin/sh\nexit 97\n").unwrap();

        let result = validate_runtime(
            &manifest,
            &entry,
            &verified_bytes,
            &["process.spawn:adapter".to_owned()],
            true,
        );
        assert!(result.is_ok(), "{result:?}");
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn verified_adapter_snapshot_is_private_non_writable_and_removed_on_drop() {
        let entry = Path::new("adapter");
        let snapshot = VerifiedExecutableSnapshot::stage(entry, b"#!/bin/sh\nexit 0\n").unwrap();
        let path = snapshot.path().to_owned();
        let directory = snapshot.directory.as_ref().unwrap().clone();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o500
        );
        drop(snapshot);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[cfg(unix)]
    #[test]
    fn isolated_adapter_reports_the_self_relative_sidecar_constraint() {
        let directory = test_package_directory("adapter-sidecar");
        fs::create_dir_all(&directory).unwrap();
        let manifest = write_isolated_adapter(
            &directory,
            "#!/bin/sh\nread request || exit 43\ntest -f \"${0%/*}/sidecar\" || exit 42\n",
            ADAPTER_FUNCTIONAL_TIMEOUT_MS,
            65_536,
        );
        fs::write(
            directory.join("sidecar"),
            "present only beside the package entry",
        )
        .unwrap();
        let entry = directory.join("adapter");
        let entry_bytes = fs::read(&entry).unwrap();

        let error = validate_runtime(
            &manifest,
            &entry,
            &entry_bytes,
            &["process.spawn:adapter".to_owned()],
            true,
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation, "{error:?}");
        assert!(
            error
                .details
                .context
                .iter()
                .any(|context| context == "exit status: 42"),
            "{error:?}"
        );
        assert!(
            error
                .details
                .help
                .iter()
                .any(|help| help.contains("private relocated snapshot"))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn isolated_adapter_refuses_unlocked_capability_and_unknown_response_fields() {
        let directory = test_package_directory("adapter-refusal");
        fs::create_dir_all(&directory).unwrap();
        let manifest = write_isolated_adapter(
            &directory,
            "#!/bin/sh\nread request\nprintf '%s\\n' '{\"protocol\":\"quirl.plugin.v1\",\"schema_version\":1,\"api_version\":\"0.1.0\",\"operation\":\"initialize\",\"status\":\"ready\",\"forged\":true}'\n",
            ADAPTER_FUNCTIONAL_TIMEOUT_MS,
            65_536,
        );
        let entry = directory.join("adapter");
        let entry_bytes = fs::read(&entry).unwrap();
        let grant_error = validate_runtime(&manifest, &entry, &entry_bytes, &[], true).unwrap_err();
        assert_eq!(grant_error.code, ErrorCode::Validation);
        assert!(grant_error.message.contains("exact locked launch grant"));
        let mut expanded_policy = manifest.clone();
        expanded_policy
            .adapter
            .as_mut()
            .unwrap()
            .callback_timeout_ms = quirl_plugin::MAX_ADAPTER_CALLBACK_TIMEOUT_MS + 1;
        let policy_error = execute_out_of_process_adapter(
            &expanded_policy,
            &entry,
            &entry_bytes,
            &["process.spawn:adapter".to_owned()],
            None,
        )
        .unwrap_err();
        assert_eq!(policy_error.code, ErrorCode::Validation);
        assert!(policy_error.message.contains("unbounded"));
        let response_error = validate_runtime(
            &manifest,
            &entry,
            &entry_bytes,
            &["process.spawn:adapter".to_owned()],
            true,
        )
        .unwrap_err();
        assert_eq!(
            response_error.code,
            ErrorCode::Validation,
            "{response_error:?}"
        );
        assert!(
            response_error
                .message
                .contains("invalid initialization response")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn isolated_adapter_deadline_and_output_limits_terminate_the_process() {
        let directory = test_package_directory("adapter-limits");
        fs::create_dir_all(&directory).unwrap();
        let timeout =
            write_isolated_adapter(&directory, "#!/bin/sh\nwhile :; do :; done\n", 5, 512);
        let entry = directory.join("adapter");
        let timeout_bytes = fs::read(&entry).unwrap();
        let deadline = validate_runtime(
            &timeout,
            &entry,
            &timeout_bytes,
            &["process.spawn:adapter".to_owned()],
            true,
        )
        .unwrap_err();
        assert_eq!(deadline.code, ErrorCode::ResourceLimit, "{deadline:?}");
        assert!(deadline.message.contains("deadline"), "{deadline:?}");

        let output = write_isolated_adapter(
            &directory,
            "#!/bin/sh\nread request\nprintf '%s' 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'\nprintf '%s' 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy' >&2\n",
            ADAPTER_FUNCTIONAL_TIMEOUT_MS,
            512,
        );
        let output_bytes = fs::read(&entry).unwrap();
        let output_error = validate_runtime(
            &output,
            &entry,
            &output_bytes,
            &["process.spawn:adapter".to_owned()],
            true,
        )
        .unwrap_err();
        assert_eq!(output_error.code, ErrorCode::ResourceLimit);
        assert!(output_error.message.contains("output message limit"));
        assert!(
            output_error
                .details
                .context
                .iter()
                .any(|context| context.contains("discarded") && context.contains("retained"))
        );

        let cancelled = AtomicBool::new(true);
        let cancellation_error = execute_out_of_process_adapter(
            &timeout,
            &entry,
            &timeout_bytes,
            &["process.spawn:adapter".to_owned()],
            Some(&cancelled),
        )
        .unwrap_err();
        assert_eq!(cancellation_error.code, ErrorCode::ResourceLimit);
        assert!(cancellation_error.message.contains("cancelled"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn trusted_plugin_requires_full_registration_parity_and_bounded_process_grants() {
        // This tests registration parity and process authority, not latency.
        // Keep production memory/instruction limits, but allow bounded scheduling
        // time for isolated Lua and its real sandboxed process round trip.
        let fixture_policy = LuaPolicy {
            wall_time: Duration::from_secs(5),
            ..LuaPolicy::config()
        };
        let directory = test_package_directory("trusted-process");
        fs::create_dir_all(&directory).unwrap();
        let entry = directory.join("plugin.lua");
        fs::write(
            &entry,
            r#"local result = quirl.process.run("printf bounded")
quirl.plugin.command {
  name = "trusted run", signature = "trusted run", summary = "Run trusted process",
  details = "Runs a bounded process through the host.", input_type = "Nothing",
  output_type = "String", examples = { "trusted run" }, effects = { "spawn_process" },
  error_codes = { resource_limit = "bounded" }, run = function(_) return result.value end,
}
"#,
        )
        .unwrap();
        let manifest_source = r#"schema_version = 2
[plugin]
name = "trusted"
version = "0.1.0"
entry = "plugin.lua"
quirl = "=@QUIRL_VERSION@"
api = "0.1.0"
runtime = "trusted_lua"
summary = "Trusted bounded process plugin"
[capabilities]
request = ["commands.register", "process.spawn:printf"]
[contributes]
commands = ["trusted run"]
[[public_commands]]
path = "trusted run"
signature = "trusted run"
summary = "Run trusted process"
details = "Runs a bounded process through the host."
input_type = "Nothing"
output_type = "String"
examples = ["trusted run"]
effects = ["spawn_process"]
error_codes = { "resource_limit" = "bounded" }
"#
        .replace("@QUIRL_VERSION@", env!("CARGO_PKG_VERSION"));
        let manifest = parse_plugin_manifest(&manifest_source, "plugin.toml").unwrap();
        let result = validate_runtime_with_lua_policy(
            &manifest,
            &entry,
            &fs::read(&entry).unwrap(),
            &[
                "commands.register".to_owned(),
                "process.spawn:printf".to_owned(),
            ],
            true,
            fixture_policy,
        );
        assert!(result.is_ok(), "{result:?}");
        let mismatched_entry = fs::read_to_string(&entry)
            .unwrap()
            .replace("input_type = \"Nothing\"", "input_type = \"String\"");
        let mismatch = validate_runtime_with_lua_policy(
            &manifest,
            &entry,
            mismatched_entry.as_bytes(),
            &[
                "commands.register".to_owned(),
                "process.spawn:printf".to_owned(),
            ],
            true,
            fixture_policy,
        )
        .unwrap_err();
        assert_eq!(mismatch.code, ErrorCode::Validation);
        assert!(mismatch.message.contains("registration differs"));
        assert!(mismatch.details.context[0].contains("Nothing -> String"));
        let denied = validate_runtime_with_lua_policy(
            &manifest,
            &entry,
            &fs::read(&entry).unwrap(),
            &[
                "commands.register".to_owned(),
                "process.spawn:other".to_owned(),
            ],
            true,
            fixture_policy,
        )
        .unwrap_err();
        assert_eq!(denied.code, ErrorCode::Lua);
        assert!(
            denied
                .details
                .context
                .iter()
                .any(|context| context.contains("capability denied"))
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn plugin_entry_symlink_cannot_escape_package_directory() {
        use std::os::unix::fs::symlink;

        let root = test_package_directory("symlink-escape");
        let package = root.join("package");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join(MANIFEST_FILE), trusted_manifest("entry.lua")).unwrap();
        let outside = root.join("outside.lua");
        fs::write(&outside, "return {}\n").unwrap();
        symlink(&outside, package.join("entry.lua")).unwrap();

        let error = read_source_package(package.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, ErrorCode::Validation);
        assert!(error.message.contains("outside"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_plugin_manifest_is_rejected_before_allocation() {
        let directory = test_package_directory("oversized-manifest");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join(MANIFEST_FILE),
            vec![b'x'; MAX_MANIFEST_BYTES + 1],
        )
        .unwrap();

        let error = read_source_package(directory.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("manifest"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn oversized_plugin_entry_is_rejected_before_runtime_loading() {
        let directory = test_package_directory("oversized-entry");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(MANIFEST_FILE), trusted_manifest("entry.lua")).unwrap();
        fs::write(directory.join("entry.lua"), vec![b'x'; MAX_ENTRY_BYTES + 1]).unwrap();

        let error = read_source_package(directory.to_str().unwrap()).unwrap_err();
        assert_eq!(error.code, ErrorCode::ResourceLimit);
        assert!(error.message.contains("entry"));
        fs::remove_dir_all(directory).unwrap();
    }
}
