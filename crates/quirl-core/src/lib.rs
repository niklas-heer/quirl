//! Shared execution and error contracts for every Quirl surface.

#![cfg_attr(
    test,
    allow(
        dead_code_pub_in_binary,
        reason = "the libtest harness is an executable, but these public items remain library API"
    )
)]

mod atomic_file;
mod error;
mod execution;
mod extension;
mod process;
mod protocol;

pub use atomic_file::{AtomicReplaceOptions, replace_file_atomically};
pub use error::{ErrorCode, ErrorLabel, ShellError};
pub use execution::{
    EXECUTION_ARGUMENT_BYTES_MAX, EXECUTION_ARGUMENTS_MAX, EXECUTION_BYTES_MAX,
    EXECUTION_CAPTURE_BYTES_MAX, EXECUTION_DEADLINE_MAX, EXECUTION_DIAGNOSTICS_MAX,
    EXECUTION_SOURCE_BYTES_MAX, EXECUTION_SOURCE_NAME_BYTES_MAX, EXECUTION_VALUE_DEPTH_MAX,
    EXECUTION_VALUE_NODES_MAX, EXECUTION_VALUE_TEXT_BYTES_MAX, ExecutionCancellation,
    ExecutionCleanupOwner, ExecutionCleanupState, ExecutionDeadline, ExecutionEffect,
    ExecutionEffects, ExecutionInput, ExecutionMode, ExecutionOutcome, ExecutionOutput,
    ExecutionOutputTarget, ExecutionPlan, ExecutionRequest, ExecutionSource, ExecutionSpan,
    ExecutionStatus, StructuredValue, StructuredValueKind, ValueInputContract, ValueOutputContract,
};
pub use extension::{
    ContributionKind, ContributionRegistration, EXTENSION_PROTOCOL_VERSION,
    EXTENSION_SCHEMA_DESCRIPTOR, EXTENSION_SCHEMA_V1_DESCRIPTOR, EventKind, EventSubscription,
    ExtensionAction, ExtensionCapability, ExtensionEvent, ExtensionEventData,
    JSON_TERMINAL_VALUE_DEPTH_MAX, JSON_TERMINAL_VALUE_NODES_MAX,
    JSON_TERMINAL_VALUE_TEXT_BYTES_MAX, MAX_EXTENSION_DEADLINE_MS, OutputStream,
    escape_json_terminal_controls, escape_terminal_controls, escape_terminal_line,
    extension_schema_hash, reject_json_terminal_controls, reject_terminal_controls,
    validate_contribution_set,
};
pub use process::{
    CommandOutcome, DirectoryOptions, DirectorySort, Entry, EntryKind, ProcessHost, ProcessRequest,
    directory_entries, directory_entries_with_options,
};
pub use protocol::{
    COMMON_ABI_SCHEMA_DESCRIPTOR, COMMON_ABI_SCHEMA_V1_DESCRIPTOR, COMMON_ABI_SCHEMA_VERSION,
    CompatibilityPolicy, PROTOCOL_FREEZE_VERSION, VersionPolicy, schema_fingerprint,
};
