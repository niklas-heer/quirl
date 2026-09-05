use crate::{
    enums::{EventStatus, ReedlineEvent, ReedlineRawEvent},
    PromptEditMode,
};

/// Define the style of parsing for the edit events
/// Available default options:
/// - Emacs
/// - Vi
pub trait EditMode: Send {
    /// Translate the given user input event into what the `LineEditor` understands
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent;

    /// Take a pending input admission failure before applying parsed events.
    ///
    /// The engine calls this immediately after parsing each raw event. Wrappers
    /// must forward it to their inner mode, so rejected input cannot become a
    /// partial edit or submission. Modes without resource failures return None.
    fn take_input_error(&mut self) -> Option<std::io::Error> {
        None
    }

    /// What to display in the prompt indicator
    fn edit_mode(&self) -> PromptEditMode;

    /// Handles events that apply only to specific edit modes (e.g changing vi mode)
    fn handle_mode_specific_event(&mut self, _event: ReedlineEvent) -> EventStatus {
        EventStatus::Inapplicable
    }
}
