//! Shared execution and error contracts for every Quirl surface.

mod error;
mod process;

pub use error::{ErrorCode, ErrorLabel, ShellError};
pub use process::{directory_entries, CommandOutcome, CommandRunner, Entry, EntryKind};
