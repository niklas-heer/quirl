use clap::{Subcommand, ValueEnum};
use quirl_core::{
    escape_json_terminal_controls, escape_terminal_controls, ContributionKind, ErrorCode,
    ShellError,
};
use quirl_lua::{LuaPolicy, LuaRuntime};
use quirl_plugin::{
    doctor_plugin, parse_plugin_manifest, permission_diff, resolve_plugin,
    validate_plugin_manifest, AdapterInitializeRequest, AdapterInitializeResponse, DoctorReport,
    PermissionDiff, PluginLockfile, PluginManifest, PluginRuntime, ADAPTER_PROTOCOL,
    PLUGIN_LOCK_FILE,
};
use quirl_process::{sandboxed_process_host, ChildProcessTree};
use serde::Serialize;
use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

const MANIFEST_FILE: &str = "plugin.toml";
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_ENTRY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum PluginCommand {
    /// Validate a legacy trusted Lua plugin registration file.
    Check {
        file: PathBuf,
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Add a local package after an explicit permission review.
    Add {
        source: String,
        #[arg(long = "allow")]
        allow: Vec<String>,
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Show requested, granted, and currently missing permissions.
    Permissions {
        name: String,
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Enable an installed plugin after checksum verification.
    Enable {
        name: String,
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Disable an installed plugin without deleting its permission record.
    Disable {
        name: String,
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Verify schema, source checksums, permissions, and runtime boundary.
    Doctor {
        name: String,
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Verify installed sources without changing the locked resolution.
    Update {
        #[arg(long)]
        locked: bool,
        #[arg(long, value_enum, default_value_t = PluginOutputFormat::Text)]
        format: PluginOutputFormat,
    },
    /// Remove an installed plugin record without deleting its source.
    Remove {
        name: String,
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
    let mut policy = LuaPolicy::config();
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
    grants: &[String],
    require_runnable: bool,
) -> Result<(), ShellError> {
    let entry_bytes = read_bounded(
        entry_path,
        MAX_ENTRY_BYTES,
        "plugin entry",
        "Restore a locked entry smaller than 4 MiB before activation",
    )?;
    validate_plugin_manifest(manifest, &entry_bytes, env!("CARGO_PKG_VERSION"))?;
    match manifest.plugin.runtime {
        PluginRuntime::OutOfProcess => {
            if require_runnable {
                execute_out_of_process_adapter(manifest, entry_path, grants, None)?;
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
    LuaRuntime::check_file(entry_path)?;
    let runtime = trusted_lua_runtime(grants)?;
    let registrations = runtime.load_plugin_file(entry_path)?;
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
    Ok(())
}

/// Execute the narrow v1 initialization handshake for a locked process
/// adapter. This lives in the CLI composition root because it owns process
/// creation; `quirl-plugin` owns the typed protocol and boundary validation.
pub(crate) fn execute_out_of_process_adapter(
    manifest: &PluginManifest,
    entry_path: &Path,
    grants: &[String],
    cancellation: Option<&AtomicBool>,
) -> Result<(), ShellError> {
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
    if request.len() > adapter.max_message_bytes as usize {
        return Err(adapter_limit_error(
            "initialization request exceeds the adapter message limit",
            adapter.max_message_bytes,
        ));
    }

    let package_root = entry_path.parent().unwrap_or_else(|| Path::new("."));
    let mut command = Command::new(entry_path);
    command
        .args(&adapter.arguments)
        .current_dir(package_root)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let containment = ChildProcessTree::new()?;
    let mut child = command.spawn().map_err(|error| {
        ShellError::new(
            ErrorCode::ProcessSpawn,
            format!(
                "could not start isolated adapter `{}`",
                manifest.plugin.name
            ),
        )
        .with_context(error.to_string())
        .with_help("Restore the executable bit and locked adapter entry, then run plugin doctor")
    })?;
    containment.assign(&mut child).map_err(|error| {
        let _ = child.kill();
        let _ = child.wait();
        error.with_help("Retry after restoring the adapter; process containment is required")
    })?;
    let output_budget = Arc::new(AdapterOutputBudget::new(adapter.max_message_bytes as usize));
    let stdout = child
        .stdout
        .take()
        .map(|stream| spawn_adapter_reader(stream, output_budget.clone()));
    let stderr = child
        .stderr
        .take()
        .map(|stream| spawn_adapter_reader(stream, output_budget));
    let write_result = if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(&request)
            .and_then(|()| stdin.write_all(b"\n"))
    } else {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "adapter stdin pipe is unavailable",
        ))
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

    let deadline = Instant::now() + Duration::from_millis(adapter.callback_timeout_ms);
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
    if stdout.exceeded || stderr.exceeded {
        return Err(adapter_limit_error(
            "isolated adapter exceeded its output message limit",
            adapter.max_message_bytes,
        ));
    }
    if !status.success() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            "isolated adapter initialization failed",
        )
        .with_context(format!("exit status: {}", status.code().unwrap_or(1)))
        .with_context(truncate_adapter_diagnostic(&stderr.bytes))
        .with_help("Make the adapter return a ready response and exit successfully"));
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
    exceeded: bool,
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
}

fn spawn_adapter_reader(
    mut stream: impl Read + Send + 'static,
    budget: Arc<AdapterOutputBudget>,
) -> thread::JoinHandle<io::Result<AdapterOutput>> {
    thread::spawn(move || {
        let mut bytes = Vec::with_capacity(budget.limit.min(8 * 1024));
        let mut exceeded = false;
        let mut buffer = [0_u8; 8 * 1024];
        loop {
            let read = stream.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let previously_consumed = budget.consumed.fetch_add(read, Ordering::Relaxed);
            let available = budget.limit.saturating_sub(previously_consumed);
            let retained = available.min(read);
            bytes.extend_from_slice(&buffer[..retained]);
            exceeded |= retained != read;
        }
        Ok(AdapterOutput { bytes, exceeded })
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
            exceeded: false,
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
    #[cfg(unix)]
    if let Ok(group) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
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
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_DIAGNOSTIC_BYTES)]).into_owned()
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
    if !path.exists() {
        return Ok(PluginLockfile::empty());
    }
    let bytes = fs::read(&path).map_err(|error| {
        io_error(
            format!("cannot read plugin lockfile {}", path.display()),
            error,
            "Repair permissions or restore the lockfile backup",
        )
    })?;
    PluginLockfile::from_json(&bytes)
        .map_err(|error| error.with_context(format!("lockfile: {}", path.display())))
}

fn save_lock(root: &Path, lock: &PluginLockfile) -> Result<(), ShellError> {
    lock.validate()?;
    fs::create_dir_all(root).map_err(|error| {
        io_error(
            format!("cannot create plugin state directory {}", root.display()),
            error,
            "Choose a writable QUIRL_PLUGIN_HOME",
        )
    })?;
    let path = root.join(PLUGIN_LOCK_FILE);
    let temporary = root.join(format!(".{PLUGIN_LOCK_FILE}.tmp-{}", std::process::id()));
    let backup = root.join(format!("{PLUGIN_LOCK_FILE}.bak"));
    let bytes = serde_json::to_vec_pretty(lock).map_err(|error| {
        ShellError::new(ErrorCode::Io, "cannot serialize plugin lockfile")
            .with_context(error.to_string())
            .with_help("Report this as a plugin platform schema defect")
    })?;
    let mut temporary_file = fs::File::create(&temporary).map_err(|error| {
        io_error(
            format!("cannot create temporary lockfile {}", temporary.display()),
            error,
            "Check available space and directory permissions",
        )
    })?;
    temporary_file.write_all(&bytes).map_err(|error| {
        io_error(
            format!("cannot write temporary lockfile {}", temporary.display()),
            error,
            "Check available space and directory permissions",
        )
    })?;
    temporary_file.sync_all().map_err(|error| {
        io_error(
            format!("cannot sync temporary lockfile {}", temporary.display()),
            error,
            "Check the plugin state filesystem",
        )
    })?;
    drop(temporary_file);
    let replaced = path.exists();
    if replaced {
        match fs::remove_file(&backup) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io_error(
                    format!("cannot replace stale lockfile backup {}", backup.display()),
                    error,
                    "Repair the plugin state directory before retrying",
                ));
            }
        }
        fs::rename(&path, &backup).map_err(|error| {
            io_error(
                format!("cannot preserve lockfile backup {}", backup.display()),
                error,
                "The current lock remains active; repair the state directory",
            )
        })?;
        if let Err(error) = sync_directory(root) {
            let rollback = fs::rename(&backup, &path)
                .map(|()| "previous lock restored".to_owned())
                .unwrap_or_else(|rollback_error| {
                    format!(
                        "rollback failed: {rollback_error}; backup remains at {}",
                        backup.display()
                    )
                });
            return Err(error.with_context(rollback));
        }
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let rollback = if replaced {
            fs::rename(&backup, &path)
                .map(|()| "previous lock restored".to_owned())
                .unwrap_or_else(|rollback_error| {
                    format!(
                        "rollback failed: {rollback_error}; backup remains at {}",
                        backup.display()
                    )
                })
        } else {
            "no previous lock existed".to_owned()
        };
        return Err(io_error(
            format!("cannot atomically install lockfile {}", path.display()),
            error,
            "Retry after repairing the state directory",
        )
        .with_context(rollback));
    }
    sync_directory(root)
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
    let file = fs::File::open(path).map_err(|error| {
        io_error(
            format!("cannot read {context} {}", path.display()),
            error,
            help,
        )
    })?;
    let size = file
        .metadata()
        .map_err(|error| {
            io_error(
                format!("cannot inspect {context} {}", path.display()),
                error,
                help,
            )
        })?
        .len();
    if size > limit as u64 {
        return Err(resource_limit_error(context, size, limit, help));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            io_error(
                format!("cannot read {context} {}", path.display()),
                error,
                help,
            )
        })?;
    if bytes.len() > limit {
        return Err(resource_limit_error(
            context,
            bytes.len() as u64,
            limit,
            help,
        ));
    }
    Ok(bytes)
}

fn resource_limit_error(context: &str, size: u64, limit: usize, help: &str) -> ShellError {
    ShellError::new(
        ErrorCode::ResourceLimit,
        format!("{context} exceeds its read limit"),
    )
    .with_context(format!("bytes: {size}; limit: {limit}"))
    .with_help(help)
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
        format!(
            r#"schema_version = 1
[plugin]
name = "bounded"
version = "0.1.0"
entry = "{entry}"
quirl = ">=0.1, <0.2"
api = "0.1.0"
runtime = "trusted_lua"
summary = "Bounded plugin"
"#
        )
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
        assert!(!directory
            .join(format!(".{PLUGIN_LOCK_FILE}.tmp-{}", std::process::id()))
            .exists());
        fs::remove_dir_all(directory).unwrap();
    }

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
        let source = format!(
            r#"schema_version = 1
[plugin]
name = "adapter"
version = "0.1.0"
entry = "adapter"
quirl = ">=0.1, <0.2"
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
            1_000,
            65_536,
        );
        let entry = directory.join("adapter");
        assert!(validate_runtime(&manifest, &entry, &[], false).is_ok());
        let result = validate_runtime(
            &manifest,
            &entry,
            &["process.spawn:adapter".to_owned()],
            true,
        );
        assert!(result.is_ok(), "{result:?}");
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
            1_000,
            65_536,
        );
        let entry = directory.join("adapter");
        let grant_error = validate_runtime(&manifest, &entry, &[], true).unwrap_err();
        assert_eq!(grant_error.code, ErrorCode::Validation);
        assert!(grant_error.message.contains("exact locked launch grant"));
        let response_error = validate_runtime(
            &manifest,
            &entry,
            &["process.spawn:adapter".to_owned()],
            true,
        )
        .unwrap_err();
        assert_eq!(
            response_error.code,
            ErrorCode::Validation,
            "{response_error:?}"
        );
        assert!(response_error
            .message
            .contains("invalid initialization response"));
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
        let deadline = validate_runtime(
            &timeout,
            &entry,
            &["process.spawn:adapter".to_owned()],
            true,
        )
        .unwrap_err();
        assert_eq!(deadline.code, ErrorCode::ResourceLimit, "{deadline:?}");
        assert!(deadline.message.contains("deadline"), "{deadline:?}");

        let output = write_isolated_adapter(
            &directory,
            "#!/bin/sh\nread request\nprintf '%s' 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'\nprintf '%s' 'yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy' >&2\n",
            1_000,
            512,
        );
        let output_error =
            validate_runtime(&output, &entry, &["process.spawn:adapter".to_owned()], true)
                .unwrap_err();
        assert_eq!(output_error.code, ErrorCode::ResourceLimit);
        assert!(output_error.message.contains("output message limit"));

        let cancelled = AtomicBool::new(true);
        let cancellation_error = execute_out_of_process_adapter(
            &timeout,
            &entry,
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
    fn granted_trusted_plugin_process_runs_through_the_bounded_host() {
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
        let manifest = parse_plugin_manifest(
            r#"schema_version = 1
[plugin]
name = "trusted"
version = "0.1.0"
entry = "plugin.lua"
quirl = ">=0.1, <0.2"
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
"#,
            "plugin.toml",
        )
        .unwrap();
        let result = validate_runtime(
            &manifest,
            &entry,
            &[
                "commands.register".to_owned(),
                "process.spawn:printf".to_owned(),
            ],
            true,
        );
        assert!(result.is_ok(), "{result:?}");
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
