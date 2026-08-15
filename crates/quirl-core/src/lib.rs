//! Shared execution and error contracts for every Quirl surface.

mod error;
mod extension;
mod process;

pub use error::{ErrorCode, ErrorLabel, ShellError};
pub use extension::{
    reject_json_terminal_controls, reject_terminal_controls, validate_contribution_set,
    ContributionKind, ContributionRegistration, EventKind, EventSubscription, ExtensionAction,
    ExtensionCapability, ExtensionEvent, ExtensionEventData, OutputStream,
    EXTENSION_PROTOCOL_VERSION, MAX_EXTENSION_DEADLINE_MS,
};
pub use process::{directory_entries, CommandOutcome, CommandRunner, Entry, EntryKind};
