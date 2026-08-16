//! Shared execution and error contracts for every Quirl surface.

mod atomic_file;
mod error;
mod execution;
mod extension;
mod process;
mod protocol;

pub use atomic_file::{replace_file_atomically, AtomicReplaceOptions};
pub use error::{ErrorCode, ErrorLabel, ShellError};
pub use execution::{
    ExecutionCancellation, ExecutionCleanupOwner, ExecutionCleanupState, ExecutionDeadline,
    ExecutionEffect, ExecutionEffects, ExecutionInput, ExecutionMode, ExecutionOutcome,
    ExecutionOutput, ExecutionOutputTarget, ExecutionPlan, ExecutionRequest, ExecutionSource,
    ExecutionSpan, ExecutionStatus, StructuredValue, StructuredValueKind, ValueInputContract,
    ValueOutputContract, EXECUTION_ARGUMENTS_MAX, EXECUTION_ARGUMENT_BYTES_MAX,
    EXECUTION_BYTES_MAX, EXECUTION_CAPTURE_BYTES_MAX, EXECUTION_DEADLINE_MAX,
    EXECUTION_DIAGNOSTICS_MAX, EXECUTION_SOURCE_BYTES_MAX, EXECUTION_SOURCE_NAME_BYTES_MAX,
    EXECUTION_VALUE_DEPTH_MAX, EXECUTION_VALUE_NODES_MAX, EXECUTION_VALUE_TEXT_BYTES_MAX,
};
pub use extension::{
    escape_json_terminal_controls, escape_terminal_controls, escape_terminal_line,
    extension_schema_hash, reject_json_terminal_controls, reject_terminal_controls,
    validate_contribution_set, ContributionKind, ContributionRegistration, EventKind,
    EventSubscription, ExtensionAction, ExtensionCapability, ExtensionEvent, ExtensionEventData,
    OutputStream, EXTENSION_PROTOCOL_VERSION, EXTENSION_SCHEMA_DESCRIPTOR,
    MAX_EXTENSION_DEADLINE_MS,
};
pub use process::{
    directory_entries, directory_entries_with_options, CommandOutcome, DirectoryOptions,
    DirectorySort, Entry, EntryKind, ProcessHost, ProcessRequest,
};
pub use protocol::{
    schema_fingerprint, CompatibilityPolicy, VersionPolicy, COMMON_ABI_SCHEMA_DESCRIPTOR,
    COMMON_ABI_SCHEMA_VERSION, PROTOCOL_FREEZE_VERSION,
};
