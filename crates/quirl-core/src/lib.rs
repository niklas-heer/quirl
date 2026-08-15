//! Shared execution and error contracts for every Quirl surface.

mod error;
mod extension;
mod process;
mod protocol;

pub use error::{ErrorCode, ErrorLabel, ShellError};
pub use extension::{
    escape_json_terminal_controls, escape_terminal_controls, extension_schema_hash,
    reject_json_terminal_controls, reject_terminal_controls, validate_contribution_set,
    ContributionKind, ContributionRegistration, EventKind, EventSubscription, ExtensionAction,
    ExtensionCapability, ExtensionEvent, ExtensionEventData, OutputStream,
    EXTENSION_PROTOCOL_VERSION, EXTENSION_SCHEMA_DESCRIPTOR, MAX_EXTENSION_DEADLINE_MS,
};
pub use process::{
    directory_entries, CommandOutcome, CommandRunner, Entry, EntryKind, ProcessHost, ProcessRequest,
};
pub use protocol::{
    schema_fingerprint, CompatibilityPolicy, VersionPolicy, COMMON_ABI_SCHEMA_DESCRIPTOR,
    COMMON_ABI_SCHEMA_VERSION, PROTOCOL_FREEZE_VERSION,
};
