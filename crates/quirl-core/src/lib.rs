//! Shared execution and error contracts for every Quirl surface.

mod error;
mod process;

pub use error::{ErrorCode, ErrorLabel, ShellError};
pub use process::{CommandOutcome, CommandRunner, Entry, EntryKind};
