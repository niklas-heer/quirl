//! Versioned contracts for Quirl's Phase 3 plugin platform.
//!
//! This crate validates plugin identity, permissions, reproducible lock state,
//! and isolated runtime boundaries. Filesystem mutation and process execution
//! remain in `quirl-cli`; trusted Lua execution remains in `quirl-lua`.

use quirl_catalog::{
    ArgumentKind as CatalogArgumentKind, ArgumentSpec, Catalog, CommandSpec, Effect, IoContract,
    Provenance, ProvenanceInfo,
};
use quirl_contract::{ArgumentKind as PackageArgumentKind, PackageCommand, stable_hash};
use quirl_core::{ErrorCode, ShellError, ValueInputContract, ValueOutputContract};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use wasmparser::{Encoding, Parser, Payload, Validator};

/// Current version of the deny-unknown plugin manifest contract.
pub const PLUGIN_SCHEMA_VERSION: u32 = 2;
/// Current lockfile version. Versions 1 and 2 are authenticated, then fail
/// closed because they cannot prove the executable manifest-v2 contract.
pub const LOCK_SCHEMA_VERSION: u32 = 3;
/// Plugin host API version required by manifests and lock entries.
pub const PLUGIN_API_VERSION: &str = "0.1.0";
/// Canonical filename used when persisting the deterministic plugin lock.
pub const PLUGIN_LOCK_FILE: &str = "plugins.lock.json";
/// WIT world a WebAssembly component manifest must declare.
pub const WASM_WORLD: &str = "quirl:plugin/api@0.1.0";
/// Sole component import admitted at the WebAssembly security boundary.
pub const WASM_HOST_IMPORT: &str = "quirl:plugin/host@0.1.0";
/// Sole component export required at the WebAssembly security boundary.
pub const WASM_GUEST_EXPORT: &str = "quirl:plugin/guest@0.1.0";
/// Wire-protocol discriminator for isolated adapter initialization.
pub const ADAPTER_PROTOCOL: &str = "quirl.plugin.v1";
/// Version of the adapter initialization request and response schema.
pub const ADAPTER_SCHEMA_VERSION: u32 = 1;
/// Hard upper bound for any message accepted from an isolated adapter.
pub const MAX_ADAPTER_MESSAGE_BYTES: u64 = 4 * 1024 * 1024;
/// Hard upper bound, in milliseconds, for an isolated adapter callback.
pub const MAX_ADAPTER_CALLBACK_TIMEOUT_MS: u64 = 60_000;
/// Hard upper bound for arguments passed to an isolated adapter process.
pub const MAX_ADAPTER_ARGUMENTS: usize = 32;
/// Hard upper bound for the aggregate UTF-8 bytes in adapter arguments.
pub const MAX_ADAPTER_ARGUMENT_BYTES: usize = 64 * 1024;
/// Hard host ceiling for a future WebAssembly component memory policy.
pub const MAX_WASM_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
/// Hard host ceiling for a future WebAssembly component fuel policy.
pub const MAX_WASM_FUEL: u64 = 100_000_000;
/// Hard host ceiling, in milliseconds, for a WebAssembly callback policy.
pub const MAX_WASM_CALLBACK_TIMEOUT_MS: u64 = 60_000;
/// Checked-in WIT world. WIT cannot express recursive `Value` or the full
/// `ShellError` shape, so this is a narrower projection of
/// `quirl_core::COMMON_ABI_SCHEMA_DESCRIPTOR`: wasm `value` drops nested
/// `list<Value>` / `record<string,Value>` in favor of `string-list`, and
/// `shell-error` omits `labels`, `command`, and `exit_status`.
pub const WASM_WIT: &str = include_str!("../wit/quirl-plugin.wit");

/// Canonical structural description used to fingerprint [`PluginManifest`].
pub const PLUGIN_SCHEMA_DESCRIPTOR: &str = "PluginManifest{deny_unknown;schema_version:2;plugin:PluginMetadata{deny_unknown;name:string;version:string;entry:relative-path;quirl:version-range;api:string;runtime:trusted_lua|wasm_component|out_of_process;summary:string};capabilities:PluginCapabilities{deny_unknown;request:array<capability>};contributes:PluginContributions{deny_unknown;commands:array<string>;completions:array<string>;events:array<string>;panels:array<string>;indexers:array<string>};public_commands:array<PackageCommand@quirl.package@1+LuaExecutableIo{input:Nothing|Bool|Int|UInt|Decimal|String|List|Record|Path|Duration|Size|DateTime|Pattern;output:Nothing|Bool|Int|UInt|Decimal|String|List|Record|Path|Duration|Size|DateTime|Pattern|Values<same>;live_streams:rejected;limits{values:512;nodes:512;depth:6;fields:256;text_bytes:245760}}>;wasm:null|WasmComponentBoundary{deny_unknown;world:string;max_memory_bytes:u64;fuel:u64;callback_timeout_ms:u64};adapter:null|OutOfProcessBoundary{deny_unknown;protocol:string;executable:string;arguments:array<string>;callback_timeout_ms:u64;max_message_bytes:u64}}";
/// Historical manifest-v1 descriptor retained for fail-closed identity tests.
pub const PLUGIN_SCHEMA_V1_DESCRIPTOR: &str = "PluginManifest{deny_unknown;schema_version:u32;plugin:PluginMetadata{deny_unknown;name:string;version:string;entry:relative-path;quirl:version-range;api:string;runtime:trusted_lua|wasm_component|out_of_process;summary:string};capabilities:PluginCapabilities{deny_unknown;request:array<capability>};contributes:PluginContributions{deny_unknown;commands:array<string>;completions:array<string>;events:array<string>;panels:array<string>;indexers:array<string>};public_commands:array<PackageCommand@quirl.package@1>;wasm:null|WasmComponentBoundary{deny_unknown;world:string;max_memory_bytes:u64;fuel:u64;callback_timeout_ms:u64};adapter:null|OutOfProcessBoundary{deny_unknown;protocol:string;executable:string;arguments:array<string>;callback_timeout_ms:u64;max_message_bytes:u64}}";
/// Canonical structural description used to fingerprint current [`PluginLockfile`] data.
pub const LOCK_SCHEMA_DESCRIPTOR: &str = "PluginLockfile{deny_unknown;document_type:string;schema_version:3;schema_hash:string;resolved_api_version:string;plugins:array<LockedPlugin{deny_unknown;name:string;version:string;source:string;runtime:PluginRuntime;resolved_api_version:string;runtime_schema_hash:plugin-manifest-v2|wasm-world;manifest_checksum:string;entry_checksum:string;source_checksum:string;requested_capabilities:array<string>;granted_capabilities:array<string>;enabled:bool}>}";
/// Historical descriptor used exclusively to authenticate and reject lock schema v2.
pub const LOCK_SCHEMA_V2_DESCRIPTOR: &str = "PluginLockfile{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;resolved_api_version:string;plugins:array<LockedPlugin{deny_unknown;name:string;version:string;source:string;runtime:PluginRuntime;resolved_api_version:string;runtime_schema_hash:string;manifest_checksum:string;entry_checksum:string;source_checksum:string;requested_capabilities:array<string>;granted_capabilities:array<string>;enabled:bool}>}";
/// Historical descriptor used exclusively to authenticate and reject lock schema v1.
pub const LOCK_SCHEMA_V1_DESCRIPTOR: &str = "PluginLockfile{deny_unknown;document_type:string;schema_version:u32;schema_hash:string;resolved_api_version:string;plugins:array<LockedPlugin{deny_unknown;name:string;version:string;source:string;runtime:PluginRuntime;resolved_api_version:string;manifest_checksum:string;entry_checksum:string;source_checksum:string;requested_capabilities:array<string>;granted_capabilities:array<string>;enabled:bool}>}";

/// Computes the structural identity of the plugin manifest and embedded command schema.
pub fn plugin_manifest_schema_hash() -> String {
    stable_hash(
        format!(
            "{PLUGIN_SCHEMA_DESCRIPTOR};{}",
            quirl_contract::package_manifest_schema_hash()
        )
        .as_bytes(),
    )
}

#[cfg(test)]
fn plugin_manifest_schema_v1_hash() -> String {
    stable_hash(
        format!(
            "{PLUGIN_SCHEMA_V1_DESCRIPTOR};{}",
            quirl_contract::PACKAGE_SCHEMA_DESCRIPTOR
        )
        .as_bytes(),
    )
}

/// Computes the structural identity expected in current plugin lockfiles.
pub fn plugin_lock_schema_hash() -> String {
    stable_hash(LOCK_SCHEMA_DESCRIPTOR.as_bytes())
}

/// Computes the content identity of the exact checked-in WIT contract.
pub fn wasm_world_hash() -> String {
    stable_hash(WASM_WIT.as_bytes())
}

/// Execution isolation boundary selected by a plugin manifest.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum PluginRuntime {
    /// Lua executed in Quirl's policy-restricted trusted extension runtime.
    #[default]
    TrustedLua,
    /// Validated component-model binary constrained by an explicit WIT world and budgets.
    WasmComponent,
    /// Relative executable contacted only through the bounded adapter protocol.
    OutOfProcess,
}

/// Strict, deny-unknown declaration of plugin identity, authority, and runtime bounds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Manifest version required to equal [`PLUGIN_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Plugin identity, compatibility, entry point, and runtime selection.
    pub plugin: PluginMetadata,
    #[serde(default)]
    /// Explicit host authorities requested by the plugin.
    pub capabilities: PluginCapabilities,
    #[serde(default)]
    /// Runtime registrations requested by the plugin.
    pub contributes: PluginContributions,
    #[serde(default)]
    /// Complete machine contracts corresponding one-to-one with contributed commands.
    pub public_commands: Vec<PackageCommand>,
    #[serde(default)]
    /// Required bounds and WIT identity for a WebAssembly component runtime.
    pub wasm: Option<WasmComponentBoundary>,
    #[serde(default)]
    /// Required wire, process, and resource bounds for an out-of-process runtime.
    pub adapter: Option<OutOfProcessBoundary>,
}

/// Validated plugin identity, compatibility range, and selected runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginMetadata {
    /// Lowercase registry-style name and mandatory command namespace.
    pub name: String,
    /// Three-component semantic plugin version.
    pub version: String,
    /// Relative, package-contained runtime entry path.
    pub entry: String,
    /// Version requirement that the installed Quirl release must satisfy.
    pub quirl: String,
    #[serde(default = "default_api_version")]
    /// Exact plugin host API version required by this plugin.
    pub api: String,
    #[serde(default)]
    /// Isolation boundary under which the entry is interpreted.
    pub runtime: PluginRuntime,
    /// Non-empty public description used in discovery and review.
    pub summary: String,
}

/// Explicit capabilities requested at the host security boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginCapabilities {
    #[serde(default)]
    /// Sorted, unique capability names or validated path-scoped authorities.
    pub request: Vec<String>,
}

/// Named extension points a plugin asks the host to register.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginContributions {
    #[serde(default)]
    /// Sorted, unique command paths, each requiring a public command contract.
    pub commands: Vec<String>,
    #[serde(default)]
    /// Sorted, unique completion providers requiring `completion.register`.
    pub completions: Vec<String>,
    #[serde(default)]
    /// Sorted, unique event observers requiring `events.observe`.
    pub events: Vec<String>,
    #[serde(default)]
    /// Sorted, unique UI panels requiring both UI and extension authority.
    pub panels: Vec<String>,
    #[serde(default)]
    /// Sorted, unique catalog indexers requiring both catalog and extension authority.
    pub indexers: Vec<String>,
}

/// Resource and interface bounds for an untrusted WebAssembly component.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WasmComponentBoundary {
    /// WIT world identity; validation requires exact equality with [`WASM_WORLD`].
    pub world: String,
    /// Runtime memory ceiling in bytes, bounded by [`MAX_WASM_MEMORY_BYTES`].
    pub max_memory_bytes: u64,
    /// Instruction/fuel budget bounded by [`MAX_WASM_FUEL`].
    pub fuel: u64,
    /// Callback deadline bounded by [`MAX_WASM_CALLBACK_TIMEOUT_MS`].
    pub callback_timeout_ms: u64,
}

/// Process, protocol, and I/O bounds for an isolated executable adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OutOfProcessBoundary {
    /// Wire protocol identity; validation requires [`ADAPTER_PROTOCOL`].
    pub protocol: String,
    /// Relative executable path, required to equal the plugin entry point.
    pub executable: String,
    #[serde(default)]
    /// Fixed arguments bounded by [`MAX_ADAPTER_ARGUMENTS`] and
    /// [`MAX_ADAPTER_ARGUMENT_BYTES`].
    pub arguments: Vec<String>,
    /// Callback deadline bounded by [`MAX_ADAPTER_CALLBACK_TIMEOUT_MS`].
    pub callback_timeout_ms: u64,
    /// Message-size ceiling bounded by [`MAX_ADAPTER_MESSAGE_BYTES`].
    pub max_message_bytes: u64,
}

/// The only request currently admitted to an isolated process adapter. The
/// narrow handshake deliberately exposes no host callbacks or ambient shell
/// state: it proves executable isolation before command/event delegation is
/// added in a future protocol version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdapterInitializeRequest {
    /// Wire protocol discriminator required to equal [`ADAPTER_PROTOCOL`].
    pub protocol: String,
    /// Request schema version required to equal [`ADAPTER_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Host API version the adapter must implement exactly.
    pub api_version: String,
    /// Requested operation; protocol v1 admits only `initialize`.
    pub operation: String,
    /// Locked plugin identity the child process is being asked to initialize.
    pub plugin: AdapterPluginIdentity,
    /// Host authorities delegated through the protocol; always empty in version 1.
    pub granted_capabilities: Vec<String>,
}

impl AdapterInitializeRequest {
    /// Constructs the sole protocol-v1 request with no delegated host capabilities.
    ///
    /// The caller-provided identity is informational and must come from a validated
    /// lock entry. Launch authority is consumed by the host and is not passed onward.
    pub fn new(name: String, version: String) -> Self {
        Self {
            protocol: ADAPTER_PROTOCOL.to_owned(),
            schema_version: ADAPTER_SCHEMA_VERSION,
            api_version: PLUGIN_API_VERSION.to_owned(),
            operation: "initialize".to_owned(),
            plugin: AdapterPluginIdentity { name, version },
            // v1 does not expose host authority to child processes. The
            // process.spawn grant authorizes this one host-side launch only.
            granted_capabilities: Vec::new(),
        }
    }
}

/// Minimal locked identity sent across the adapter process boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdapterPluginIdentity {
    /// Validated plugin name from the lock entry.
    pub name: String,
    /// Validated plugin version from the lock entry.
    pub version: String,
}

/// A successful adapter response is intentionally assertion-only. Returning
/// arbitrary registrations here would create an unvalidated authority channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdapterInitializeResponse {
    /// Echoed wire protocol identity.
    pub protocol: String,
    /// Response schema version matching the request contract.
    pub schema_version: u32,
    /// Exact plugin host API version implemented by the adapter.
    pub api_version: String,
    /// Echoed initialization operation.
    pub operation: String,
    /// Assertion-only readiness status; protocol v1 requires `ready`.
    pub status: String,
}

impl AdapterInitializeResponse {
    /// Validates the complete assertion-only response against a request.
    ///
    /// Any mismatched discriminator, version, operation, or status returns
    /// [`ErrorCode::Validation`]. No response field can grant authority or register
    /// plugin contributions.
    pub fn validate_for(&self, request: &AdapterInitializeRequest) -> Result<(), ShellError> {
        if self.protocol != ADAPTER_PROTOCOL
            || self.schema_version != ADAPTER_SCHEMA_VERSION
            || self.api_version != PLUGIN_API_VERSION
            || self.operation != request.operation
            || self.status != "ready"
        {
            return Err(validation_error(
                "out-of-process adapter returned an incompatible initialization response",
                "Return exactly protocol `quirl.plugin.v1`, schema_version = 1, api_version = `0.1.0`, operation = `initialize`, and status = `ready`",
            ));
        }
        Ok(())
    }
}

/// Reproducible, deny-unknown record of resolved plugin content and granted authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginLockfile {
    /// Discriminator; current lockfiles use `quirl.plugin.lock`.
    pub document_type: String,
    /// Lock contract version required to equal [`LOCK_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Structural identity computed by [`plugin_lock_schema_hash`].
    pub schema_hash: String,
    /// Plugin API version against which all entries were resolved.
    pub resolved_api_version: String,
    /// Entries sorted uniquely by plugin name for deterministic serialization.
    pub plugins: Vec<LockedPlugin>,
}

/// Content-addressed plugin resolution and its reviewed capability grant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LockedPlugin {
    /// Validated plugin name and unique key within the lockfile.
    pub name: String,
    /// Exact resolved plugin version.
    pub version: String,
    /// Non-empty canonical source identity used during resolution.
    pub source: String,
    /// Isolation boundary under which this entry may run.
    pub runtime: PluginRuntime,
    /// Exact plugin API version accepted during resolution.
    pub resolved_api_version: String,
    /// Manifest schema hash or WIT content hash governing the runtime boundary.
    pub runtime_schema_hash: String,
    /// SHA-256 identity of the exact manifest bytes reviewed for installation.
    pub manifest_checksum: String,
    /// SHA-256 identity of the exact entry bytes reviewed for installation.
    pub entry_checksum: String,
    /// SHA-256 identity binding manifest, entry, and source identities together.
    pub source_checksum: String,
    /// Sorted capabilities declared by the reviewed manifest.
    pub requested_capabilities: Vec<String>,
    /// Sorted subset of requested authority explicitly granted by the user.
    pub granted_capabilities: Vec<String>,
    /// Whether the host may activate this entry after all integrity checks pass.
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LegacyPluginLockfileV1 {
    document_type: String,
    schema_version: u32,
    schema_hash: String,
    resolved_api_version: String,
    plugins: Vec<LegacyLockedPluginV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct LegacyLockedPluginV1 {
    name: String,
    version: String,
    source: String,
    runtime: PluginRuntime,
    resolved_api_version: String,
    manifest_checksum: String,
    entry_checksum: String,
    source_checksum: String,
    requested_capabilities: Vec<String>,
    granted_capabilities: Vec<String>,
    enabled: bool,
}

/// Deterministic change set used to require review of newly requested authority.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PermissionDiff {
    /// Capabilities present only in the new request and requiring approval.
    pub added: Vec<String>,
    /// Previously requested capabilities no longer present.
    pub removed: Vec<String>,
    /// Capabilities present in both sets.
    pub unchanged: Vec<String>,
}

/// Importance of a plugin health finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    /// Integrity or compatibility failure that makes activation unsafe.
    Error,
    /// Non-fatal condition that should be reviewed.
    Warning,
}

/// Machine-readable integrity or compatibility finding from plugin diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DoctorDiagnostic {
    /// Whether the finding makes the plugin unhealthy.
    pub severity: DoctorSeverity,
    /// Stable identifier for automation and remediation routing.
    pub code: String,
    /// Human-readable description of the observed mismatch.
    pub message: String,
    /// Actionable guidance for restoring a trusted state.
    pub help: String,
}

/// Complete deterministic health report for one locked plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DoctorReport {
    /// Discriminator; reports use `quirl.plugin.doctor`.
    pub document_type: String,
    /// Plugin contract version used for the checks.
    pub schema_version: u32,
    /// Name of the locked plugin that was checked.
    pub plugin: String,
    /// `true` exactly when no integrity or runtime-schema errors were found.
    pub healthy: bool,
    /// All findings in deterministic check order.
    pub diagnostics: Vec<DoctorDiagnostic>,
}

impl PluginLockfile {
    /// Constructs a valid current lockfile with no installed plugins.
    pub fn empty() -> Self {
        Self {
            document_type: "quirl.plugin.lock".to_owned(),
            schema_version: LOCK_SCHEMA_VERSION,
            schema_hash: plugin_lock_schema_hash(),
            resolved_api_version: PLUGIN_API_VERSION.to_owned(),
            plugins: Vec::new(),
        }
    }

    /// Validates lock identity, ordering, runtime compatibility, authority, and checksums.
    ///
    /// The method does not read plugin files or recompute content checksums; use
    /// [`doctor_plugin`] with trusted bytes for that integrity check. All malformed or
    /// incompatible state is reported as [`ErrorCode::Validation`].
    pub fn validate(&self) -> Result<(), ShellError> {
        if self.document_type != "quirl.plugin.lock"
            || self.schema_version != LOCK_SCHEMA_VERSION
            || self.schema_hash != plugin_lock_schema_hash()
            || self.resolved_api_version != PLUGIN_API_VERSION
        {
            return Err(validation_error(
                "plugin lockfile schema or resolved API version is incompatible",
                "Regenerate the lockfile with the installed Quirl version",
            ));
        }
        let mut names = self
            .plugins
            .iter()
            .map(|plugin| plugin.name.as_str())
            .collect::<Vec<_>>();
        let original = names.clone();
        names.sort_unstable();
        names.dedup();
        if names != original {
            return Err(validation_error(
                "plugin lock entries must be sorted and unique",
                "Run `quirl plugin doctor` and rebuild the lockfile",
            ));
        }
        for plugin in &self.plugins {
            if plugin.enabled && plugin.runtime == PluginRuntime::WasmComponent {
                return Err(validation_error(
                    "non-executing Wasm component boundaries cannot be marked enabled",
                    "Keep Wasm entries disabled until a component runtime is installed",
                ));
            }
            if plugin.resolved_api_version != PLUGIN_API_VERSION || plugin.source.trim().is_empty()
            {
                return Err(validation_error(
                    "locked plugin has an incompatible API or empty source identity",
                    "Re-add the plugin with the installed Quirl version",
                ));
            }
            let expected_runtime_hash = match plugin.runtime {
                PluginRuntime::WasmComponent => wasm_world_hash(),
                PluginRuntime::TrustedLua | PluginRuntime::OutOfProcess => {
                    plugin_manifest_schema_hash()
                }
            };
            if plugin.runtime_schema_hash != expected_runtime_hash {
                return Err(validation_error(
                    "locked plugin runtime schema hash is incompatible",
                    "Re-add the plugin against the installed host schemas",
                ));
            }
            validate_sorted_unique(
                &plugin.requested_capabilities,
                "locked requested capabilities",
            )?;
            validate_sorted_unique(&plugin.granted_capabilities, "locked granted capabilities")?;
            validate_capabilities(&plugin.requested_capabilities)?;
            validate_capabilities(&plugin.granted_capabilities)?;
            if !plugin
                .granted_capabilities
                .iter()
                .all(|grant| plugin.requested_capabilities.contains(grant))
            {
                return Err(validation_error(
                    "plugin lock grants authority absent from the manifest request",
                    "Remove the unexpected grant and re-add the plugin",
                ));
            }
            validate_sha256(&plugin.manifest_checksum)?;
            validate_sha256(&plugin.entry_checksum)?;
            validate_sha256(&plugin.source_checksum)?;
        }
        Ok(())
    }

    /// Parses and validates a deny-unknown JSON lockfile.
    ///
    /// Current version-3 data is validated directly. Historical version-1 and
    /// version-2 data is decoded with its exact deny-unknown shape and identity,
    /// then rejected with migration guidance: a lock alone cannot prove that its
    /// reviewed manifest satisfies the executable manifest-v2 I/O contract.
    /// Unsupported versions and malformed JSON return [`ErrorCode::Validation`].
    pub fn from_json(bytes: &[u8]) -> Result<Self, ShellError> {
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|error| {
            validation_error(
                format!("plugin lockfile is not valid JSON: {error}"),
                "Restore the lockfile backup or re-add plugins after review",
            )
        })?;
        let version = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                validation_error(
                    "plugin lockfile has no integer schema_version",
                    "Restore a versioned lockfile or re-add plugins after review",
                )
            })?;
        match version {
            3 => {
                let lock: Self = serde_json::from_value(value).map_err(|error| {
                    validation_error(
                        format!("plugin lockfile schema is invalid: {error}"),
                        "Restore the lockfile backup or re-add plugins after review",
                    )
                })?;
                lock.validate()?;
                Ok(lock)
            }
            2 => {
                let legacy: Self = serde_json::from_value(value).map_err(|error| {
                    validation_error(
                        format!("legacy plugin lockfile v2 schema is invalid: {error}"),
                        "Restore the original v2 lockfile before migration",
                    )
                })?;
                if legacy.document_type != "quirl.plugin.lock"
                    || legacy.schema_hash != stable_hash(LOCK_SCHEMA_V2_DESCRIPTOR.as_bytes())
                    || legacy.resolved_api_version != PLUGIN_API_VERSION
                {
                    return Err(validation_error(
                        "legacy plugin lockfile v2 identity is invalid",
                        "Restore the original v2 lockfile before migration",
                    ));
                }
                Err(legacy_lock_contract_error(2))
            }
            1 => {
                let legacy: LegacyPluginLockfileV1 =
                    serde_json::from_value(value).map_err(|error| {
                        validation_error(
                            format!("legacy plugin lockfile schema is invalid: {error}"),
                            "Use a Quirl release that can read lock schema v1",
                        )
                    })?;
                if legacy.document_type != "quirl.plugin.lock"
                    || legacy.schema_hash != stable_hash(LOCK_SCHEMA_V1_DESCRIPTOR.as_bytes())
                    || legacy.resolved_api_version != PLUGIN_API_VERSION
                {
                    return Err(validation_error(
                        "legacy plugin lockfile identity is invalid",
                        "Restore the original v1 lockfile before migration",
                    ));
                }
                Err(legacy_lock_contract_error(1))
            }
            _ => Err(validation_error(
                format!("plugin lockfile schema version {version} is unsupported"),
                "Upgrade Quirl for newer locks or migrate older locks with an intermediate release",
            )),
        }
    }

    /// Returns a validated lockfile with one new, uniquely named entry installed.
    ///
    /// Both the existing lock and resulting candidate are validated. The receiver is
    /// unchanged on failure, and the returned entries are sorted by name.
    pub fn install(&self, plugin: LockedPlugin) -> Result<Self, ShellError> {
        self.validate()?;
        if self.plugins.iter().any(|item| item.name == plugin.name) {
            return Err(validation_error(
                format!("plugin `{}` is already installed", plugin.name),
                "Use `quirl plugin update --locked` or remove the installed plugin first",
            ));
        }
        let mut candidate = self.clone();
        candidate.plugins.push(plugin);
        candidate
            .plugins
            .sort_by(|left, right| left.name.cmp(&right.name));
        candidate.validate()?;
        Ok(candidate)
    }

    /// Updates only the source identity of an existing locked resolution.
    ///
    /// Version, reviewed bytes, runtime schema, requested authority, and grants must
    /// remain unchanged. The supplied source checksum must bind the new source to the
    /// locked manifest and entry checksums or validation fails.
    pub fn replace_locked(&self, plugin: LockedPlugin) -> Result<Self, ShellError> {
        self.validate()?;
        let Some(existing) = self.plugins.iter().find(|item| item.name == plugin.name) else {
            return Err(validation_error(
                format!("plugin `{}` is not installed", plugin.name),
                "Install it with `quirl plugin add <source>`",
            ));
        };
        if existing.version != plugin.version
            || existing.manifest_checksum != plugin.manifest_checksum
            || existing.entry_checksum != plugin.entry_checksum
            || existing.runtime_schema_hash != plugin.runtime_schema_hash
            || existing.requested_capabilities != plugin.requested_capabilities
            || existing.granted_capabilities != plugin.granted_capabilities
        {
            return Err(validation_error(
                format!("locked plugin `{}` changed", plugin.name),
                "Review the version, checksum, and permission diff; locked updates never rewrite them",
            ));
        }
        let expected_source_checksum = derived_source_checksum(
            &plugin.manifest_checksum,
            &plugin.entry_checksum,
            &plugin.source,
        );
        if plugin.source_checksum != expected_source_checksum {
            return Err(validation_error(
                format!(
                    "locked plugin `{}` has an inconsistent source checksum",
                    plugin.name
                ),
                "Re-resolve the plugin so the source identity checksum matches its source",
            ));
        }
        let mut candidate = self.clone();
        let item = candidate
            .plugins
            .iter_mut()
            .find(|item| item.name == plugin.name)
            .ok_or_else(|| validation_error("installed plugin disappeared", "Retry the update"))?;
        item.source = plugin.source;
        item.source_checksum = plugin.source_checksum;
        candidate.validate()?;
        Ok(candidate)
    }

    /// Returns a validated copy with the named plugin enabled or disabled.
    ///
    /// Enabling a WebAssembly component is rejected while its runtime remains a
    /// non-executing validation boundary. Unknown names return an invalid-command error.
    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<Self, ShellError> {
        self.validate()?;
        let mut candidate = self.clone();
        let plugin = candidate
            .plugins
            .iter_mut()
            .find(|plugin| plugin.name == name)
            .ok_or_else(|| unknown_plugin(name))?;
        plugin.enabled = enabled;
        candidate.validate()?;
        Ok(candidate)
    }

    /// Returns a copy without the named plugin after validating the source lock.
    ///
    /// An unknown name returns an invalid-command error and leaves the receiver intact.
    pub fn remove(&self, name: &str) -> Result<Self, ShellError> {
        self.validate()?;
        let mut candidate = self.clone();
        let before = candidate.plugins.len();
        candidate.plugins.retain(|plugin| plugin.name != name);
        if candidate.plugins.len() == before {
            return Err(unknown_plugin(name));
        }
        Ok(candidate)
    }

    /// Looks up an installed entry by its unique plugin name.
    ///
    /// This is a lookup only; callers requiring a trust decision should first call
    /// [`Self::validate`] and, when bytes are available, [`doctor_plugin`].
    pub fn find(&self, name: &str) -> Result<&LockedPlugin, ShellError> {
        self.plugins
            .iter()
            .find(|plugin| plugin.name == name)
            .ok_or_else(|| unknown_plugin(name))
    }
}

/// Parses a strict TOML plugin manifest and rejects unknown fields.
///
/// `origin` is included in the returned [`ShellError`] so the CLI can identify the
/// untrusted source. Semantic, capability, and runtime-boundary checks are performed
/// separately by [`validate_plugin_manifest`].
pub fn parse_plugin_manifest(source: &str, origin: &str) -> Result<PluginManifest, ShellError> {
    toml::from_str::<PluginManifest>(source).map_err(|error| {
        ShellError::new(
            ErrorCode::Validation,
            format!("invalid plugin manifest {origin}"),
        )
        .with_context(error.to_string())
        .with_help("Use schema_version = 2 and remove unknown plugin manifest fields")
    })
}

/// Validates manifest identity, compatibility, authority, contributions, and runtime bounds.
///
/// For WebAssembly, `entry_bytes` must be the exact candidate component and are
/// structurally validated against the sole admitted import/export contract. Trusted
/// Lua is not executed here. Every expected rejection is returned as
/// [`ErrorCode::Validation`].
pub fn validate_plugin_manifest(
    manifest: &PluginManifest,
    entry_bytes: &[u8],
    quirl_version: &str,
) -> Result<(), ShellError> {
    if manifest.schema_version != PLUGIN_SCHEMA_VERSION {
        return Err(validation_error(
            format!(
                "unsupported plugin schema version {}",
                manifest.schema_version
            ),
            format!("Set schema_version = {PLUGIN_SCHEMA_VERSION}"),
        ));
    }
    if !valid_name(&manifest.plugin.name)
        || !valid_version(&manifest.plugin.version)
        || manifest.plugin.summary.trim().is_empty()
        || manifest.plugin.api != PLUGIN_API_VERSION
        || !supports_version(&manifest.plugin.quirl, quirl_version)
    {
        return Err(validation_error(
            "plugin identity, API, or Quirl version is invalid",
            format!(
                "Use a lowercase name, semantic version, summary, api = `{PLUGIN_API_VERSION}`, and a compatible Quirl range"
            ),
        ));
    }
    validate_relative_path(&manifest.plugin.entry)?;
    validate_sorted_unique(&manifest.capabilities.request, "requested capabilities")?;
    validate_capabilities(&manifest.capabilities.request)?;
    validate_contributions(manifest)?;
    if manifest.plugin.runtime != PluginRuntime::TrustedLua
        && !manifest.contributes.commands.is_empty()
    {
        return Err(validation_error(
            "only trusted-Lua plugins can contribute executable commands in ABI v1",
            "Remove command contributions or select runtime = `trusted_lua`; Wasm and out-of-process command execution are not installed",
        ));
    }
    match manifest.plugin.runtime {
        PluginRuntime::TrustedLua => {
            if !manifest.plugin.entry.ends_with(".lua")
                || manifest.wasm.is_some()
                || manifest.adapter.is_some()
            {
                return Err(validation_error(
                    "trusted Lua plugins require a .lua entry and no isolated adapter block",
                    "Set runtime = `trusted_lua` and remove wasm/adapter configuration",
                ));
            }
        }
        PluginRuntime::WasmComponent => validate_wasm_component(manifest, entry_bytes)?,
        PluginRuntime::OutOfProcess => validate_out_of_process(manifest)?,
    }
    Ok(())
}

/// Resolves reviewed manifest and entry bytes into a disabled, content-addressed lock entry.
///
/// Every newly requested capability must appear in `approved_capabilities`; approval
/// never grants authority absent from the manifest. On success, SHA-256 checksums bind
/// the exact source bytes and source identity, and the plugin remains disabled until a
/// separate enable transition.
pub fn resolve_plugin(
    manifest: &PluginManifest,
    manifest_bytes: &[u8],
    entry_bytes: &[u8],
    source: &str,
    approved_capabilities: &[String],
    quirl_version: &str,
) -> Result<(LockedPlugin, PermissionDiff), ShellError> {
    validate_plugin_manifest(manifest, entry_bytes, quirl_version)?;
    let diff = permission_diff(&[], &manifest.capabilities.request);
    let approved = approved_capabilities.iter().collect::<BTreeSet<_>>();
    let missing = diff
        .added
        .iter()
        .filter(|permission| !approved.contains(permission))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ShellError::new(
            ErrorCode::Validation,
            format!(
                "plugin requests unapproved permissions: {}",
                missing.join(", ")
            ),
        )
        .with_context(format!("permission diff: +{}", diff.added.join(", +")))
        .with_help(
            "Review the source, then repeat --allow <capability> for every approved addition",
        ));
    }
    let manifest_checksum = sha256(manifest_bytes);
    let entry_checksum = sha256(entry_bytes);
    let source_checksum = derived_source_checksum(&manifest_checksum, &entry_checksum, source);
    Ok((
        LockedPlugin {
            name: manifest.plugin.name.clone(),
            version: manifest.plugin.version.clone(),
            source: source.to_owned(),
            runtime: manifest.plugin.runtime,
            resolved_api_version: PLUGIN_API_VERSION.to_owned(),
            runtime_schema_hash: match manifest.plugin.runtime {
                PluginRuntime::WasmComponent => wasm_world_hash(),
                PluginRuntime::TrustedLua | PluginRuntime::OutOfProcess => {
                    plugin_manifest_schema_hash()
                }
            },
            manifest_checksum,
            entry_checksum,
            source_checksum,
            requested_capabilities: manifest.capabilities.request.clone(),
            granted_capabilities: manifest.capabilities.request.clone(),
            enabled: false,
        },
        diff,
    ))
}

/// Computes sorted capability additions, removals, and intersections as mathematical sets.
pub fn permission_diff(previous: &[String], requested: &[String]) -> PermissionDiff {
    let previous = previous.iter().cloned().collect::<BTreeSet<_>>();
    let requested = requested.iter().cloned().collect::<BTreeSet<_>>();
    PermissionDiff {
        added: requested.difference(&previous).cloned().collect(),
        removed: previous.difference(&requested).cloned().collect(),
        unchanged: previous.intersection(&requested).cloned().collect(),
    }
}

/// Normalize manifest-declared plugin commands into the same semantic catalog
/// contract used by builtins. Namespacing is mandatory in platform v0.1;
/// shadowing a builtin is therefore impossible without a future explicit
/// approval field and lockfile transition.
pub fn normalize_plugin_commands(
    manifest: &PluginManifest,
    source: &str,
    fingerprint: &str,
) -> Result<Vec<CommandSpec>, ShellError> {
    validate_contributions(manifest)?;
    if Catalog::builtin().commands.iter().any(|command| {
        command.path.split_whitespace().next() == Some(manifest.plugin.name.as_str())
    }) {
        return Err(validation_error(
            format!(
                "plugin namespace `{}` collides with an installed command namespace",
                manifest.plugin.name
            ),
            "Rename the plugin; command shadow approval is not available in platform v0.1",
        ));
    }
    let namespace = format!("{} ", manifest.plugin.name);
    let provenance = ProvenanceInfo {
        source: Provenance::Plugin,
        confidence: quirl_catalog::Confidence::Exact,
        trust: quirl_catalog::Trust::Trusted,
        origin: Some(source.to_owned()),
        fingerprint: Some(fingerprint.to_owned()),
        generated_at: None,
    };
    let mut commands = Vec::new();
    for command in &manifest.public_commands {
        if command.path != manifest.plugin.name && !command.path.starts_with(&namespace) {
            return Err(validation_error(
                format!(
                    "plugin command `{}` is outside namespace `{}`",
                    command.path, manifest.plugin.name
                ),
                "Prefix contributed commands with the plugin name; shadowing is not available in platform v0.1",
            ));
        }
        let arguments = command
            .arguments
            .iter()
            .map(|argument| ArgumentSpec {
                names: argument.names.clone(),
                kind: match argument.kind {
                    PackageArgumentKind::Positional => CatalogArgumentKind::Positional,
                    PackageArgumentKind::Option => CatalogArgumentKind::Option,
                    PackageArgumentKind::Flag => CatalogArgumentKind::Flag,
                },
                value_type: argument.value_type.clone(),
                required: argument.required,
                repeatable: argument.repeatable,
                values: None,
                conflicts: Vec::new(),
                documentation: argument.documentation.clone(),
                examples: command.examples.clone(),
                provenance: provenance.clone(),
            })
            .collect();
        let exit_codes = command
            .error_codes
            .iter()
            .map(|(code, summary)| {
                code.parse::<i32>()
                    .map(|code| (code, summary.clone()))
                    .map_err(|_| {
                        validation_error(
                            format!(
                                "plugin command `{}` has non-numeric exit code `{code}`",
                                command.path
                            ),
                            "Use signed integer exit-code keys",
                        )
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        commands.push(CommandSpec {
            id: format!(
                "plugin:{}/{}",
                manifest.plugin.name,
                command
                    .path
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("/")
            ),
            version: Some(manifest.plugin.version.clone()),
            path: command.path.clone(),
            aliases: Vec::new(),
            parent: command.path.rsplit_once(' ').map(|(parent, _)| {
                format!(
                    "plugin:{}/{}",
                    manifest.plugin.name,
                    parent.split_whitespace().collect::<Vec<_>>().join("/")
                )
            }),
            signature: command.signature.clone(),
            summary: command.summary.clone(),
            details: command.details.clone(),
            options: arguments,
            examples: command.examples.clone(),
            io: IoContract {
                input: command.input_type.clone(),
                output: command.output_type.clone(),
                // `Values<T>` is a bounded materialized batch. Catalog streaming
                // means a live incremental source, which Lua ABI v1 cannot expose.
                streaming: false,
            },
            effects: command.effects.clone(),
            exit_codes,
            provenance: provenance.clone(),
        });
    }
    commands.sort_by(|left, right| left.path.cmp(&right.path));
    let ids = commands
        .iter()
        .map(|command| command.id.clone())
        .collect::<BTreeSet<_>>();
    for command in &mut commands {
        if command
            .parent
            .as_ref()
            .is_some_and(|parent| !ids.contains(parent))
        {
            command.parent = None;
        }
    }
    Ok(commands)
}

/// Checks locked manifest, entry, source, and runtime-schema identities without execution.
///
/// The caller supplies bytes read through its own containment-safe filesystem boundary.
/// Any mismatch makes `healthy` false and produces a stable error diagnostic; this
/// function performs no Lua, WebAssembly, process, or network operation.
pub fn doctor_plugin(
    locked: &LockedPlugin,
    manifest_bytes: &[u8],
    entry_bytes: &[u8],
) -> DoctorReport {
    let mut diagnostics = Vec::new();
    let manifest_checksum = sha256(manifest_bytes);
    let entry_checksum = sha256(entry_bytes);
    if manifest_checksum != locked.manifest_checksum {
        diagnostics.push(doctor_error(
            "plugin.manifest_tampered",
            "manifest checksum differs from the permission lock",
        ));
    }
    if entry_checksum != locked.entry_checksum {
        diagnostics.push(doctor_error(
            "plugin.entry_tampered",
            "entry checksum differs from the permission lock",
        ));
    }
    let source_checksum =
        derived_source_checksum(&manifest_checksum, &entry_checksum, &locked.source);
    if source_checksum != locked.source_checksum {
        diagnostics.push(doctor_error(
            "plugin.source_lock_tampered",
            "source identity checksum differs from the permission lock",
        ));
    }
    let expected_runtime_hash = match locked.runtime {
        PluginRuntime::WasmComponent => wasm_world_hash(),
        PluginRuntime::TrustedLua | PluginRuntime::OutOfProcess => plugin_manifest_schema_hash(),
    };
    if locked.runtime_schema_hash != expected_runtime_hash {
        diagnostics.push(doctor_error(
            "plugin.runtime_schema_mismatch",
            "runtime schema or WIT world hash differs from the installed host",
        ));
    }
    DoctorReport {
        document_type: "quirl.plugin.doctor".to_owned(),
        schema_version: PLUGIN_SCHEMA_VERSION,
        plugin: locked.name.clone(),
        healthy: diagnostics.is_empty(),
        diagnostics,
    }
}

fn validate_contributions(manifest: &PluginManifest) -> Result<(), ShellError> {
    for (label, values) in [
        ("commands", &manifest.contributes.commands),
        ("completions", &manifest.contributes.completions),
        ("events", &manifest.contributes.events),
        ("panels", &manifest.contributes.panels),
        ("indexers", &manifest.contributes.indexers),
    ] {
        validate_sorted_unique(values, label)?;
    }
    require_capability(
        !manifest.contributes.commands.is_empty(),
        "commands.register",
        &manifest.capabilities.request,
    )?;
    require_capability(
        !manifest.contributes.completions.is_empty(),
        "completion.register",
        &manifest.capabilities.request,
    )?;
    require_capability(
        !manifest.contributes.events.is_empty(),
        "events.observe",
        &manifest.capabilities.request,
    )?;
    require_capability(
        !manifest.contributes.panels.is_empty(),
        "ui.panel",
        &manifest.capabilities.request,
    )?;
    require_capability(
        !manifest.contributes.indexers.is_empty(),
        "catalog.register",
        &manifest.capabilities.request,
    )?;
    require_capability(
        !manifest.contributes.panels.is_empty() || !manifest.contributes.indexers.is_empty(),
        "extension.contribute",
        &manifest.capabilities.request,
    )?;
    let declared = manifest
        .contributes
        .commands
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let documented = manifest
        .public_commands
        .iter()
        .map(|command| command.path.as_str())
        .collect::<BTreeSet<_>>();
    if declared != documented {
        return Err(validation_error(
            "every contributed command needs one exact public command contract",
            "Keep contributes.commands and public_commands paths identical",
        ));
    }
    for command in &manifest.public_commands {
        validate_command(command)?;
    }
    Ok(())
}

fn validate_command(command: &PackageCommand) -> Result<(), ShellError> {
    if command.path.trim().is_empty()
        || command.signature.trim().is_empty()
        || command.summary.trim().is_empty()
        || command.details.trim().is_empty()
        || command.input_type.trim().is_empty()
        || command.output_type.trim().is_empty()
        || command.examples.is_empty()
        || command.effects.is_empty()
        || command.error_codes.is_empty()
    {
        return Err(validation_error(
            format!("public command `{}` has incomplete metadata", command.path),
            "Document its signature, types, examples, effects, and error codes",
        ));
    }
    if ValueInputContract::parse_exact(&command.input_type).is_none() {
        return Err(executable_type_error(
            &command.path,
            "input_type",
            &command.input_type,
            "Use `Nothing` or one exact value kind: Bool, Int, UInt, Decimal, String, List, Record, Path, Duration, Size, DateTime, or Pattern",
        ));
    }
    if ValueOutputContract::parse_exact(&command.output_type).is_none() {
        return Err(executable_type_error(
            &command.path,
            "output_type",
            &command.output_type,
            "Use one exact value kind, or `Values<T>` for a bounded finite batch; live `Stream<T>` output is unsupported",
        ));
    }
    let _effects = command.effects.iter().map(effect_name).collect::<Vec<_>>();
    Ok(())
}

fn executable_type_error(command: &str, field: &str, declaration: &str, help: &str) -> ShellError {
    validation_error(
        format!("plugin command `{command}` has unsupported executable {field} `{declaration}`"),
        help,
    )
}

fn validate_wasm_component(
    manifest: &PluginManifest,
    entry_bytes: &[u8],
) -> Result<(), ShellError> {
    let boundary = manifest.wasm.as_ref().ok_or_else(|| {
        validation_error(
            "Wasm component plugins require a [wasm] boundary",
            "Declare the imported world and non-zero memory, fuel, and callback budgets",
        )
    })?;
    if manifest.adapter.is_some()
        || !manifest.plugin.entry.ends_with(".wasm")
        || boundary.world != WASM_WORLD
        || boundary.max_memory_bytes == 0
        || boundary.max_memory_bytes > MAX_WASM_MEMORY_BYTES
        || boundary.fuel == 0
        || boundary.fuel > MAX_WASM_FUEL
        || boundary.callback_timeout_ms == 0
        || boundary.callback_timeout_ms > MAX_WASM_CALLBACK_TIMEOUT_MS
    {
        return Err(validation_error(
            "invalid or unbounded WebAssembly component boundary",
            format!(
                "Use world `{WASM_WORLD}` with memory <= {MAX_WASM_MEMORY_BYTES} bytes, fuel <= {MAX_WASM_FUEL}, and callback_timeout_ms <= {MAX_WASM_CALLBACK_TIMEOUT_MS}"
            ),
        ));
    }
    validate_component_contract(entry_bytes)?;
    Ok(())
}

fn validate_component_contract(bytes: &[u8]) -> Result<(), ShellError> {
    Validator::new().validate_all(bytes).map_err(|error| {
        validation_error(
            format!("invalid WebAssembly component: {error}"),
            "Build a validated component for the checked-in quirl-plugin.wit world",
        )
    })?;
    let mut component = false;
    let mut saw_encoding = false;
    let mut component_types = 0usize;
    let mut imports = BTreeSet::new();
    let mut exports = BTreeSet::new();
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| {
            validation_error(
                format!("cannot inspect WebAssembly component metadata: {error}"),
                "Rebuild the component from the checked-in WIT world",
            )
        })? {
            Payload::Version { encoding, .. } => {
                if !saw_encoding {
                    component = encoding == Encoding::Component;
                    saw_encoding = true;
                }
            }
            Payload::ComponentTypeSection(reader) => {
                for ty in reader {
                    ty.map_err(|error| {
                        validation_error(
                            format!("invalid component type metadata: {error}"),
                            "Rebuild the component from the checked-in WIT world",
                        )
                    })?;
                    component_types = component_types.saturating_add(1);
                }
            }
            Payload::ComponentImportSection(reader) => {
                for import in reader {
                    imports.insert(
                        import
                            .map_err(|error| {
                                validation_error(
                                    format!("invalid component import metadata: {error}"),
                                    "Rebuild the component from the checked-in WIT world",
                                )
                            })?
                            .name
                            .name
                            .to_owned(),
                    );
                }
            }
            Payload::ComponentExportSection(reader) => {
                for export in reader {
                    exports.insert(
                        export
                            .map_err(|error| {
                                validation_error(
                                    format!("invalid component export metadata: {error}"),
                                    "Rebuild the component from the checked-in WIT world",
                                )
                            })?
                            .name
                            .name
                            .to_owned(),
                    );
                }
            }
            _ => {}
        }
    }
    if !component
        || component_types == 0
        || imports != BTreeSet::from([WASM_HOST_IMPORT.to_owned()])
        || exports != BTreeSet::from([WASM_GUEST_EXPORT.to_owned()])
    {
        return Err(validation_error(
            "WebAssembly component imports or exports do not match the Quirl WIT world",
            format!("Import exactly `{WASM_HOST_IMPORT}` and export exactly `{WASM_GUEST_EXPORT}`"),
        ));
    }
    Ok(())
}

fn validate_out_of_process(manifest: &PluginManifest) -> Result<(), ShellError> {
    let adapter = manifest.adapter.as_ref().ok_or_else(|| {
        validation_error(
            "out-of-process plugins require an [adapter] boundary",
            "Declare protocol, relative executable, message limit, and callback timeout",
        )
    })?;
    validate_relative_path(&adapter.executable)?;
    let argument_bytes = adapter.arguments.iter().try_fold(0_usize, |total, argument| {
        total.checked_add(argument.len()).ok_or_else(|| {
            validation_error(
                "out-of-process adapter argument bytes overflow the host range",
                format!("Keep aggregate argument text at or below {MAX_ADAPTER_ARGUMENT_BYTES} bytes"),
            )
        })
    })?;
    if manifest.wasm.is_some()
        || manifest.plugin.entry != adapter.executable
        || adapter.protocol != ADAPTER_PROTOCOL
        || adapter.arguments.len() > MAX_ADAPTER_ARGUMENTS
        || argument_bytes > MAX_ADAPTER_ARGUMENT_BYTES
        || adapter.callback_timeout_ms == 0
        || adapter.callback_timeout_ms > MAX_ADAPTER_CALLBACK_TIMEOUT_MS
        || adapter.max_message_bytes == 0
        || adapter.max_message_bytes > MAX_ADAPTER_MESSAGE_BYTES
    {
        return Err(validation_error(
            "invalid or unbounded out-of-process adapter boundary",
            format!(
                "Use the entry itself as the relative executable, protocol `quirl.plugin.v1`, at most {MAX_ADAPTER_ARGUMENTS} arguments and {MAX_ADAPTER_ARGUMENT_BYTES} argument bytes, a callback deadline at most {MAX_ADAPTER_CALLBACK_TIMEOUT_MS} ms, and a message limit at most {MAX_ADAPTER_MESSAGE_BYTES} bytes"
            ),
        ));
    }
    let launch_grant = format!("process.spawn:{}", adapter.executable);
    if manifest.capabilities.request != [launch_grant.clone()] {
        return Err(validation_error(
            "out-of-process adapters must request only their scoped launch capability",
            format!(
                "Set capabilities.request = [\"{launch_grant}\"]; protocol v1 exposes no other host capabilities"
            ),
        ));
    }
    Ok(())
}

fn require_capability(active: bool, needed: &str, requested: &[String]) -> Result<(), ShellError> {
    if active && !requested.iter().any(|item| item == needed) {
        return Err(validation_error(
            format!("contribution requires capability `{needed}`"),
            format!("Add `{needed}` to capabilities.request and approve the permission diff"),
        ));
    }
    Ok(())
}

fn validate_sorted_unique(values: &[String], label: &str) -> Result<(), ShellError> {
    let mut normalized = values.to_vec();
    normalized.sort();
    normalized.dedup();
    if normalized != values || values.iter().any(|item| item.trim().is_empty()) {
        return Err(validation_error(
            format!("{label} must be non-empty, sorted, and unique"),
            "Sort values lexicographically and remove duplicates",
        ));
    }
    Ok(())
}

fn validate_capabilities(capabilities: &[String]) -> Result<(), ShellError> {
    const EXACT: &[&str] = &[
        "analysis.register",
        "catalog.register",
        "commands.register",
        "completion.register",
        "environment.mutate",
        "events.observe",
        "execution.block",
        "extension.contribute",
        "filesystem.read",
        "filesystem.write",
        "knowledge.register",
        "output.read",
        "plan.rewrite",
        "process.spawn",
        "prompt.register",
        "ui.panel",
        "view.register",
    ];
    for capability in capabilities {
        if EXACT.contains(&capability.as_str()) {
            continue;
        }
        let Some((name, scope)) = capability.split_once(':') else {
            return Err(unsupported_capability(capability));
        };
        let valid = match name {
            "process.spawn" => valid_executable_scope(scope),
            "filesystem.read" | "filesystem.write" => valid_filesystem_scope(scope),
            _ => false,
        };
        if !valid {
            return Err(unsupported_capability(capability));
        }
    }
    Ok(())
}

fn valid_executable_scope(scope: &str) -> bool {
    let path = std::path::Path::new(scope);
    !scope.is_empty()
        && !scope.starts_with('-')
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
        && scope.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b'+')
        })
}

fn valid_filesystem_scope(scope: &str) -> bool {
    let path = std::path::Path::new(scope);
    !scope.is_empty()
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
        && scope.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b'+')
        })
}

fn derived_source_checksum(manifest_checksum: &str, entry_checksum: &str, source: &str) -> String {
    sha256(
        [
            manifest_checksum.as_bytes(),
            entry_checksum.as_bytes(),
            source.as_bytes(),
        ]
        .concat(),
    )
}

fn unsupported_capability(capability: &str) -> ShellError {
    validation_error(
        format!("unsupported or malformed plugin capability `{capability}`"),
        "Use a documented capability; only process.spawn:<executable> and filesystem.read/write:<relative-path> accept scopes",
    )
}

fn validate_relative_path(path: &str) -> Result<(), ShellError> {
    let path = std::path::Path::new(path);
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || !path.components().all(|component| {
            matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        return Err(validation_error(
            "plugin paths must stay relative to their package",
            "Remove absolute and parent-directory components",
        ));
    }
    Ok(())
}

fn sha256(bytes: impl AsRef<[u8]>) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes.as_ref()))
}

fn validate_sha256(checksum: &str) -> Result<(), ShellError> {
    let value = checksum.strip_prefix("sha256:").unwrap_or_default();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(validation_error(
            "plugin lockfile contains an invalid SHA-256 checksum",
            "Re-add the plugin from a trusted source",
        ));
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn valid_version(version: &str) -> bool {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let parts = core.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn supports_version(requirement: &str, installed: &str) -> bool {
    let Some(installed) = parse_version(installed) else {
        return false;
    };
    requirement.split(',').all(|constraint| {
        let constraint = constraint.trim();
        let (operator, version) = [">=", "<=", ">", "<", "="]
            .into_iter()
            .find_map(|operator| {
                constraint
                    .strip_prefix(operator)
                    .map(|version| (operator, version.trim()))
            })
            .unwrap_or(("=", constraint));
        let Some(required) = parse_version(version) else {
            return false;
        };
        match operator {
            ">=" => installed >= required,
            "<=" => installed <= required,
            ">" => installed > required,
            "<" => installed < required,
            _ => installed == required,
        }
    })
}

fn parse_version(version: &str) -> Option<[u64; 3]> {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut parsed = [0_u64; 3];
    let parts = core.split('.').collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for (index, part) in parts.into_iter().enumerate() {
        parsed[index] = part.parse().ok()?;
    }
    Some(parsed)
}

fn default_api_version() -> String {
    PLUGIN_API_VERSION.to_owned()
}

fn doctor_error(code: &str, message: &str) -> DoctorDiagnostic {
    DoctorDiagnostic {
        severity: DoctorSeverity::Error,
        code: code.to_owned(),
        message: message.to_owned(),
        help: "Restore the locked source or explicitly review and re-add the plugin".to_owned(),
    }
}

fn unknown_plugin(name: &str) -> ShellError {
    validation_error(
        format!("plugin `{name}` is not installed"),
        "List the lockfile or add the plugin before changing its state",
    )
}

fn legacy_lock_contract_error(version: u64) -> ShellError {
    validation_error(
        format!("plugin lockfile schema v{version} predates executable command I/O contracts"),
        format!(
            "Move plugins.lock.json intact to plugins.lock.json.legacy-v{version}, then re-add each plugin after reviewing a schema_version = 2 manifest; Quirl cannot infer the new runtime contract from an old lock"
        ),
    )
}

fn validation_error(message: impl Into<String>, help: impl Into<String>) -> ShellError {
    ShellError::new(ErrorCode::Validation, message).with_help(help)
}

fn effect_name(effect: &Effect) -> &'static str {
    match effect {
        Effect::ReadFilesystem => "read_filesystem",
        Effect::WriteFilesystem => "write_filesystem",
        Effect::SpawnProcess => "spawn_process",
        Effect::ChangeDirectory => "change_directory",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK_V1_FIXTURE: &str = r#"{"document_type":"quirl.plugin.lock","schema_version":1,"schema_hash":"fnv1a64:afa6bbde57eacd2a","resolved_api_version":"0.1.0","plugins":[{"name":"demo","version":"0.1.0","source":"file:/fixture","runtime":"trusted_lua","resolved_api_version":"0.1.0","manifest_checksum":"sha256:manifest","entry_checksum":"sha256:entry","source_checksum":"sha256:source","requested_capabilities":["commands.register"],"granted_capabilities":["commands.register"],"enabled":false}]}"#;
    const LOCK_V2_FIXTURE: &str = r#"{"document_type":"quirl.plugin.lock","schema_version":2,"schema_hash":"fnv1a64:97ae63dc47a0d36f","resolved_api_version":"0.1.0","plugins":[{"name":"demo","version":"0.1.0","source":"file:/fixture","runtime":"trusted_lua","resolved_api_version":"0.1.0","runtime_schema_hash":"fnv1a64:9d9e259ca934cb9c","manifest_checksum":"sha256:manifest","entry_checksum":"sha256:entry","source_checksum":"sha256:source","requested_capabilities":["commands.register"],"granted_capabilities":["commands.register"],"enabled":false}]}"#;

    const LUA_MANIFEST: &str = r#"
schema_version = 2

[plugin]
name = "demo"
version = "0.1.0"
entry = "plugin.lua"
quirl = ">=0.1, <0.2"
api = "0.1.0"
runtime = "trusted_lua"
summary = "Demonstrate typed plugin state"

[capabilities]
request = ["commands.register"]

[contributes]
commands = ["demo run"]
completions = []
events = []
panels = []
indexers = []

[[public_commands]]
path = "demo run"
signature = "demo run"
summary = "Run the demo"
details = "Returns one deterministic demo record."
input_type = "Nothing"
output_type = "Record"
examples = ["demo run"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }
"#;

    fn resolved(approved: &[String]) -> Result<LockedPlugin, ShellError> {
        let manifest = parse_plugin_manifest(LUA_MANIFEST, "plugin.toml")?;
        resolve_plugin(
            &manifest,
            LUA_MANIFEST.as_bytes(),
            b"quirl.plugin.command {}",
            "file:/demo",
            approved,
            "0.1.0",
        )
        .map(|value| value.0)
    }

    #[test]
    fn manifest_denies_unknown_fields_and_permission_escalation() {
        let unknown = format!("{LUA_MANIFEST}\nunknown = true\n");
        assert!(parse_plugin_manifest(&unknown, "plugin.toml").is_err());
        let error = resolved(&[]).unwrap_err();
        assert!(error.message.contains("unapproved permissions"));
    }

    #[test]
    fn plugin_manifest_hash_transitively_covers_package_command_contract() {
        let actual = plugin_manifest_schema_hash();
        let shallow = stable_hash(PLUGIN_SCHEMA_DESCRIPTOR.as_bytes());
        assert_ne!(actual, shallow);
        assert_eq!(actual, plugin_manifest_schema_hash());
        assert_eq!(plugin_manifest_schema_v1_hash(), "fnv1a64:9d9e259ca934cb9c");
    }

    #[test]
    fn manifests_and_locks_reject_unknown_or_malformed_capabilities() {
        for capability in [
            "network.everything",
            "process.spawn:printf\nsecond",
            "process.spawn:../escape",
            "filesystem.read:../../secret",
            "filesystem.read:evil\nname",
            "filesystem.write:dir/\u{1b}[31m",
            "commands.register:shadow",
        ] {
            let source = LUA_MANIFEST.replace(
                "request = [\"commands.register\"]",
                &format!("request = [\"{capability}\"]"),
            );
            if let Ok(manifest) = parse_plugin_manifest(&source, "plugin.toml") {
                assert!(
                    validate_plugin_manifest(&manifest, b"return true", "0.1.0").is_err(),
                    "accepted {capability}"
                );
            }
        }

        let mut locked = resolved(&["commands.register".to_owned()]).unwrap();
        locked
            .requested_capabilities
            .push("network.everything".to_owned());
        locked.requested_capabilities.sort();
        let lock = PluginLockfile {
            plugins: vec![locked],
            ..PluginLockfile::empty()
        };
        assert!(lock.validate().is_err());
    }

    #[test]
    fn lock_transitions_are_copy_on_validate_and_sorted() {
        let plugin = resolved(&["commands.register".to_owned()]).unwrap();
        let empty = PluginLockfile::empty();
        let installed = empty.install(plugin).unwrap();
        assert!(empty.plugins.is_empty());
        assert!(!installed.plugins[0].enabled);
        let enabled = installed.set_enabled("demo", true).unwrap();
        assert!(!installed.plugins[0].enabled);
        assert!(enabled.plugins[0].enabled);
        assert!(enabled.remove("demo").unwrap().plugins.is_empty());
    }

    #[test]
    fn historical_locks_authenticate_then_fail_closed_without_inventing_contracts() {
        assert_eq!(
            stable_hash(LOCK_SCHEMA_V1_DESCRIPTOR.as_bytes()),
            "fnv1a64:afa6bbde57eacd2a"
        );
        assert_eq!(
            stable_hash(LOCK_SCHEMA_V2_DESCRIPTOR.as_bytes()),
            "fnv1a64:97ae63dc47a0d36f"
        );
        let v1_error = PluginLockfile::from_json(LOCK_V1_FIXTURE.as_bytes()).unwrap_err();
        assert!(v1_error.message.contains("schema v1 predates executable"));
        assert!(v1_error.details.help[0].contains("plugins.lock.json.legacy-v1"));

        let v2_error = PluginLockfile::from_json(LOCK_V2_FIXTURE.as_bytes()).unwrap_err();
        assert!(v2_error.message.contains("schema v2 predates executable"));
        assert!(v2_error.details.help[0].contains("plugins.lock.json.legacy-v2"));

        let unknown = LOCK_V2_FIXTURE.replace(
            "\"schema_version\":2",
            "\"schema_version\":2,\"unexpected\":true",
        );
        assert!(PluginLockfile::from_json(unknown.as_bytes()).is_err());
        assert!(PluginLockfile::from_json(b"{").is_err());

        let current = PluginLockfile::empty();
        assert_eq!(
            PluginLockfile::from_json(&serde_json::to_vec(&current).unwrap()).unwrap(),
            current
        );
        let mut future = serde_json::to_value(&current).unwrap();
        future["schema_version"] = serde_json::json!(4);
        assert!(PluginLockfile::from_json(&serde_json::to_vec(&future).unwrap()).is_err());
    }

    #[test]
    fn doctor_detects_manifest_and_entry_tampering() {
        let plugin = resolved(&["commands.register".to_owned()]).unwrap();
        let report = doctor_plugin(&plugin, b"tampered", b"tampered");
        assert!(!report.healthy);
        assert_eq!(report.diagnostics.len(), 3);
    }

    #[test]
    fn locked_update_rejects_checksum_or_permission_changes() {
        let plugin = resolved(&["commands.register".to_owned()]).unwrap();
        let lock = PluginLockfile::empty().install(plugin.clone()).unwrap();
        let mut escalated = plugin;
        escalated
            .requested_capabilities
            .push("process.spawn:demo".to_owned());
        escalated.requested_capabilities.sort();
        assert!(lock.replace_locked(escalated).is_err());
        assert_eq!(lock.plugins.len(), 1);
    }

    #[test]
    fn locked_update_recomputes_source_checksum_from_the_new_source() {
        let plugin = resolved(&["commands.register".to_owned()]).unwrap();
        let lock = PluginLockfile::empty().install(plugin.clone()).unwrap();
        let mut relocated = plugin.clone();
        relocated.source = "file:/relocated/plugin.toml".to_owned();
        relocated.source_checksum = derived_source_checksum(
            &relocated.manifest_checksum,
            &relocated.entry_checksum,
            &relocated.source,
        );
        let updated = lock.replace_locked(relocated.clone()).unwrap();
        assert_eq!(updated.plugins[0].source, relocated.source);
        assert_eq!(
            updated.plugins[0].source_checksum,
            relocated.source_checksum
        );

        let mut stale = plugin;
        stale.source = "file:/stale/plugin.toml".to_owned();
        assert!(lock.replace_locked(stale).is_err());
    }

    #[test]
    fn wasm_boundary_accepts_components_and_rejects_core_modules() {
        let mut wit = wit_parser::Resolve::default();
        wit.push_str("quirl-plugin.wit", WASM_WIT).unwrap();
        let source = LUA_MANIFEST
            .replace("entry = \"plugin.lua\"", "entry = \"plugin.wasm\"")
            .replace("runtime = \"trusted_lua\"", "runtime = \"wasm_component\"")
            .replace("request = [\"commands.register\"]", "request = []")
            .replace("commands = [\"demo run\"]", "commands = []")
            .replace(
                r#"[[public_commands]]
path = "demo run"
signature = "demo run"
summary = "Run the demo"
details = "Returns one deterministic demo record."
input_type = "Nothing"
output_type = "Record"
examples = ["demo run"]
effects = ["read_filesystem"]
error_codes = { "0" = "success" }
"#,
                "",
            )
            + r#"

[wasm]
world = "quirl:plugin/api@0.1.0"
max_memory_bytes = 16777216
fuel = 1000000
callback_timeout_ms = 25
"#;
        let manifest = parse_plugin_manifest(&source, "plugin.toml").unwrap();
        let component = wat::parse_str(format!(
            r#"(component
                (type (instance))
                (import "{WASM_HOST_IMPORT}" (instance (type 0)))
                (export "{WASM_GUEST_EXPORT}" (instance 0))
            )"#
        ))
        .unwrap();
        assert!(validate_plugin_manifest(&manifest, &component, "0.1.0").is_ok());
        let mut exact_limits = manifest.clone();
        let exact_boundary = exact_limits.wasm.as_mut().unwrap();
        exact_boundary.max_memory_bytes = MAX_WASM_MEMORY_BYTES;
        exact_boundary.fuel = MAX_WASM_FUEL;
        exact_boundary.callback_timeout_ms = MAX_WASM_CALLBACK_TIMEOUT_MS;
        assert!(validate_plugin_manifest(&exact_limits, &component, "0.1.0").is_ok());

        for (memory_bytes, fuel, callback_timeout_ms) in [
            (0, MAX_WASM_FUEL, MAX_WASM_CALLBACK_TIMEOUT_MS),
            (
                MAX_WASM_MEMORY_BYTES + 1,
                MAX_WASM_FUEL,
                MAX_WASM_CALLBACK_TIMEOUT_MS,
            ),
            (MAX_WASM_MEMORY_BYTES, 0, MAX_WASM_CALLBACK_TIMEOUT_MS),
            (
                MAX_WASM_MEMORY_BYTES,
                MAX_WASM_FUEL + 1,
                MAX_WASM_CALLBACK_TIMEOUT_MS,
            ),
            (MAX_WASM_MEMORY_BYTES, MAX_WASM_FUEL, 0),
            (
                MAX_WASM_MEMORY_BYTES,
                MAX_WASM_FUEL,
                MAX_WASM_CALLBACK_TIMEOUT_MS + 1,
            ),
        ] {
            let mut invalid = manifest.clone();
            let boundary = invalid.wasm.as_mut().unwrap();
            boundary.max_memory_bytes = memory_bytes;
            boundary.fuel = fuel;
            boundary.callback_timeout_ms = callback_timeout_ms;
            assert!(validate_plugin_manifest(&invalid, &component, "0.1.0").is_err());
        }
        let (locked, _) = resolve_plugin(
            &manifest,
            source.as_bytes(),
            &component,
            "file:/demo/plugin.toml",
            &["commands.register".to_owned()],
            "0.1.0",
        )
        .unwrap();
        let lock = PluginLockfile::empty().install(locked).unwrap();
        assert!(lock.set_enabled("demo", true).is_err());
        assert!(validate_plugin_manifest(&manifest, b"\0asm\x01\0\0\0", "0.1.0").is_err());
        let nested_core = wat::parse_str(format!(
            r#"(component
                (core module $nested
                  (func (export "f") (result i32)
                    i32.const 1
                  )
                )
                (type (instance))
                (import "{WASM_HOST_IMPORT}" (instance (type 0)))
                (export "{WASM_GUEST_EXPORT}" (instance 0))
            )"#
        ))
        .unwrap();
        assert!(validate_plugin_manifest(&manifest, &nested_core, "0.1.0").is_ok());
        let wrong_world = wat::parse_str(
            r#"(component
                (type (instance))
                (import "ambient:host/api" (instance (type 0)))
                (export "quirl:plugin/guest@0.1.0" (instance 0))
            )"#,
        )
        .unwrap();
        assert!(validate_plugin_manifest(&manifest, &wrong_world, "0.1.0").is_err());
        assert!(wasm_world_hash().starts_with("fnv1a64:"));
    }

    #[test]
    fn process_adapter_requires_an_exact_scoped_launch_grant_and_bounded_contract() {
        let manifest = r#"schema_version = 2
[plugin]
name = "adapter"
version = "0.1.0"
entry = "adapter"
quirl = ">=0.1, <0.2"
api = "0.1.0"
runtime = "out_of_process"
summary = "Bounded process adapter"
[capabilities]
request = ["process.spawn:adapter"]
[adapter]
protocol = "quirl.plugin.v1"
executable = "adapter"
callback_timeout_ms = 25
max_message_bytes = 1024
"#;
        let manifest = parse_plugin_manifest(manifest, "plugin.toml").unwrap();
        assert!(validate_plugin_manifest(&manifest, b"adapter", "0.1.0").is_ok());

        let mut exact_limits = manifest.clone();
        let adapter = exact_limits.adapter.as_mut().unwrap();
        adapter.arguments = vec![
            "x".repeat(MAX_ADAPTER_ARGUMENT_BYTES / MAX_ADAPTER_ARGUMENTS);
            MAX_ADAPTER_ARGUMENTS
        ];
        adapter.callback_timeout_ms = MAX_ADAPTER_CALLBACK_TIMEOUT_MS;
        adapter.max_message_bytes = MAX_ADAPTER_MESSAGE_BYTES;
        assert!(validate_plugin_manifest(&exact_limits, b"adapter", "0.1.0").is_ok());

        let mut too_many_arguments = manifest.clone();
        too_many_arguments.adapter.as_mut().unwrap().arguments =
            vec![String::new(); MAX_ADAPTER_ARGUMENTS + 1];
        assert!(validate_plugin_manifest(&too_many_arguments, b"adapter", "0.1.0").is_err());
        let mut oversized_arguments = manifest.clone();
        oversized_arguments.adapter.as_mut().unwrap().arguments =
            vec!["x".repeat(MAX_ADAPTER_ARGUMENT_BYTES + 1)];
        assert!(validate_plugin_manifest(&oversized_arguments, b"adapter", "0.1.0").is_err());
        for (callback_timeout_ms, max_message_bytes) in [
            (0, MAX_ADAPTER_MESSAGE_BYTES),
            (
                MAX_ADAPTER_CALLBACK_TIMEOUT_MS + 1,
                MAX_ADAPTER_MESSAGE_BYTES,
            ),
            (MAX_ADAPTER_CALLBACK_TIMEOUT_MS, 0),
            (
                MAX_ADAPTER_CALLBACK_TIMEOUT_MS,
                MAX_ADAPTER_MESSAGE_BYTES + 1,
            ),
        ] {
            let mut invalid = manifest.clone();
            let adapter = invalid.adapter.as_mut().unwrap();
            adapter.callback_timeout_ms = callback_timeout_ms;
            adapter.max_message_bytes = max_message_bytes;
            assert!(validate_plugin_manifest(&invalid, b"adapter", "0.1.0").is_err());
        }

        let extra_capability = r#"schema_version = 2
[plugin]
name = "adapter"
version = "0.1.0"
entry = "adapter"
quirl = ">=0.1, <0.2"
api = "0.1.0"
runtime = "out_of_process"
summary = "Bounded process adapter"
[capabilities]
request = ["process.spawn:adapter", "output.read"]
[adapter]
protocol = "quirl.plugin.v1"
executable = "adapter"
callback_timeout_ms = 25
max_message_bytes = 1024"#;
        let extra_capability = parse_plugin_manifest(extra_capability, "plugin.toml").unwrap();
        assert!(validate_plugin_manifest(&extra_capability, b"adapter", "0.1.0").is_err());

        let request = AdapterInitializeRequest::new("adapter".to_owned(), "0.1.0".to_owned());
        let forged = serde_json::json!({
            "protocol": ADAPTER_PROTOCOL,
            "schema_version": ADAPTER_SCHEMA_VERSION,
            "api_version": PLUGIN_API_VERSION,
            "operation": "initialize",
            "status": "ready",
            "forged": true,
        });
        assert!(serde_json::from_value::<AdapterInitializeResponse>(forged).is_err());
        assert!(
            AdapterInitializeResponse {
                protocol: ADAPTER_PROTOCOL.to_owned(),
                schema_version: ADAPTER_SCHEMA_VERSION,
                api_version: PLUGIN_API_VERSION.to_owned(),
                operation: "initialize".to_owned(),
                status: "ready".to_owned(),
            }
            .validate_for(&request)
            .is_ok()
        );
    }

    #[test]
    fn plugin_commands_normalize_into_exact_namespaced_catalog_facts() {
        let manifest = parse_plugin_manifest(LUA_MANIFEST, "plugin.toml").unwrap();
        let commands = normalize_plugin_commands(&manifest, "file:/demo", "sha256:demo").unwrap();
        let command = &commands[0];
        assert_eq!(command.id, "plugin:demo/demo/run");
        assert_eq!(command.version.as_deref(), Some("0.1.0"));
        assert_eq!(command.path, "demo run");
        assert_eq!(command.io.input, "Nothing");
        assert_eq!(command.io.output, "Record");
        assert_eq!(
            command.exit_codes.get(&0).map(String::as_str),
            Some("success")
        );
        assert_eq!(command.provenance.source, Provenance::Plugin);
        assert_eq!(
            command.provenance.confidence,
            quirl_catalog::Confidence::Exact
        );
        assert_eq!(command.provenance.trust, quirl_catalog::Trust::Trusted);
        assert!(!command.io.streaming);
        assert_eq!(
            command.provenance.fingerprint.as_deref(),
            Some("sha256:demo")
        );
    }

    #[test]
    fn executable_command_contracts_reject_unknown_ambiguous_and_live_types() {
        for (field, declaration) in [
            ("input_type", "Unknown"),
            ("input_type", "String | Path"),
            ("input_type", "Stream<String>"),
            ("output_type", "Unknown"),
            ("output_type", "String | Path"),
            ("output_type", "Stream<String>"),
        ] {
            let original = if field == "input_type" {
                "input_type = \"Nothing\""
            } else {
                "output_type = \"Record\""
            };
            let source = LUA_MANIFEST.replace(original, &format!("{field} = \"{declaration}\""));
            let manifest = parse_plugin_manifest(&source, "plugin.toml").unwrap();
            let error = validate_plugin_manifest(&manifest, b"return true", "0.1.0").unwrap_err();
            assert_eq!(error.code, ErrorCode::Validation);
            assert!(error.message.contains("unsupported executable"));
            assert!(!error.details.help.is_empty());
        }

        let finite = LUA_MANIFEST.replace(
            "output_type = \"Record\"",
            "output_type = \"Values<String>\"",
        );
        let manifest = parse_plugin_manifest(&finite, "plugin.toml").unwrap();
        validate_plugin_manifest(&manifest, b"return true", "0.1.0").unwrap();
        let command =
            &normalize_plugin_commands(&manifest, "file:/demo", "sha256:demo").unwrap()[0];
        assert_eq!(command.io.output, "Values<String>");
        assert!(!command.io.streaming);

        let legacy = LUA_MANIFEST.replace("schema_version = 2", "schema_version = 1");
        let manifest = parse_plugin_manifest(&legacy, "plugin.toml").unwrap();
        let error = validate_plugin_manifest(&manifest, b"return true", "0.1.0").unwrap_err();
        assert!(
            error
                .message
                .contains("unsupported plugin schema version 1")
        );
        assert!(error.details.help[0].contains("schema_version = 2"));
    }

    #[test]
    fn plugin_commands_cannot_shadow_outside_their_namespace() {
        let source = LUA_MANIFEST
            .replace("commands = [\"demo run\"]", "commands = [\"git commit\"]")
            .replace("path = \"demo run\"", "path = \"git commit\"")
            .replace("signature = \"demo run\"", "signature = \"git commit\"");
        let manifest = parse_plugin_manifest(&source, "plugin.toml").unwrap();
        let error = normalize_plugin_commands(&manifest, "file:/demo", "sha256:demo").unwrap_err();
        assert!(error.message.contains("outside namespace"));

        let reserved = LUA_MANIFEST
            .replace("name = \"demo\"", "name = \"git\"")
            .replace("commands = [\"demo run\"]", "commands = [\"git extra\"]")
            .replace("path = \"demo run\"", "path = \"git extra\"")
            .replace("signature = \"demo run\"", "signature = \"git extra\"");
        let manifest = parse_plugin_manifest(&reserved, "plugin.toml").unwrap();
        let error = normalize_plugin_commands(&manifest, "file:/git", "sha256:git").unwrap_err();
        assert!(error.message.contains("collides"));
    }
}
