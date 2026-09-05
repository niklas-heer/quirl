//! Quirl-owned simple editor resource admission.
//!
//! Failure model: repeated individually legal pastes, completion replacements and
//! history recall can otherwise retain unlimited source and undo snapshots. Growth
//! is rejected before copying into the owning buffer. The editor rolls back the
//! failed command or callback, latches its error, and the event engine fails before a later Enter
//! can submit partially edited source. No input is truncated or executed on failure.
//! Vi numeric prefixes are admitted before expansion. The owning engine bounds
//! raw collection to 1024 events / 256 KiB text / 20 ms per batch, then admits
//! at most 1024 parsed actions before effects. A continuous terminal
//! writer cannot keep the collection loop alive indefinitely.

use std::{fmt, io};

pub(crate) const INPUT_BYTES_MAX: usize = 64 * 1024;
pub(crate) const UNDO_STATES_MAX: usize = 128;
pub(crate) const INPUT_ACTIONS_MAX: usize = 1024;
pub(crate) const INPUT_BATCH_TEXT_BYTES_MAX: usize = 256 * 1024;
const _: () = assert!(INPUT_BYTES_MAX * UNDO_STATES_MAX == 8 * 1024 * 1024);

#[derive(Debug)]
struct InputLimitError {
    resource: &'static str,
    limit: u64,
    observed: u64,
}

impl fmt::Display for InputLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "simple editor {} exceeds limit {} (attempted {})",
            self.resource, self.limit, self.observed
        )
    }
}
impl std::error::Error for InputLimitError {}

pub(crate) fn input_limit_error(observed_bytes: usize) -> io::Error {
    resource_limit_error("input bytes", INPUT_BYTES_MAX, observed_bytes)
}

pub(crate) fn resource_limit_error(
    resource: &'static str,
    limit: usize,
    observed: usize,
) -> io::Error {
    io::Error::other(InputLimitError {
        resource,
        limit: u64::try_from(limit).unwrap_or(u64::MAX),
        observed: u64::try_from(observed).unwrap_or(u64::MAX),
    })
}

/// Whether an I/O error reports a simple editor source or Vi parsing resource limit.
///
/// Rejected edits preserve the preceding text, cursor and undo history. Vi counts
/// and expanded actions are limited to 1,024, and sequence caching to 64 characters.
/// Each input batch also admits at most 1,024 actions and 256 KiB raw text.
/// `read_line` returns this error after restoring its terminal modes; the caller
/// must report the resource limit and must not execute the rejected input.
pub fn is_input_limit_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|inner| inner.is::<InputLimitError>())
}

/// Admission state for one batch; failed events never consume its allowance.
#[derive(Default)]
pub(crate) struct InputActionBudget {
    admitted: usize,
}

impl InputActionBudget {
    pub(crate) fn admit(&mut self, event: &crate::ReedlineEvent) -> io::Result<()> {
        let observed = self
            .admitted
            .saturating_add(event_weight(std::slice::from_ref(event))?);
        if observed > INPUT_ACTIONS_MAX {
            return Err(resource_limit_error(
                "input batch actions",
                INPUT_ACTIONS_MAX,
                observed,
            ));
        }
        self.admitted = observed;
        Ok(())
    }
}

// Count before batch fusion or repeat cloning. An explicit stack also bounds
// malformed custom mode trees: at most 1024 references and 4096 visited nodes.
// The extra node allowance includes fixed wrapper events around 1024 actions.
pub(crate) fn event_weight(events: &[crate::ReedlineEvent]) -> io::Result<usize> {
    use crate::ReedlineEvent;
    let mut pending = Vec::new();
    if events.len() > INPUT_ACTIONS_MAX {
        return Err(resource_limit_error(
            "expanded actions",
            INPUT_ACTIONS_MAX,
            events.len(),
        ));
    }
    pending.extend(events);
    let mut weight = 0usize;
    let mut visited = 0usize;
    while let Some(event) = pending.pop() {
        visited = visited.saturating_add(1);
        if visited > 4096 {
            return Err(resource_limit_error("event tree nodes", 4096, visited));
        }
        match event {
            ReedlineEvent::None => {}
            ReedlineEvent::Multiple(children) | ReedlineEvent::UntilFound(children)
                if !children.is_empty() =>
            {
                let references = pending.len().saturating_add(children.len());
                if references > INPUT_ACTIONS_MAX {
                    return Err(resource_limit_error(
                        "event tree references",
                        INPUT_ACTIONS_MAX,
                        references,
                    ));
                }
                pending.extend(children);
            }
            ReedlineEvent::Edit(commands) => weight = weight.saturating_add(commands.len().max(1)),
            _ => weight = weight.saturating_add(1),
        }
        if weight > INPUT_ACTIONS_MAX {
            return Err(resource_limit_error(
                "expanded actions",
                INPUT_ACTIONS_MAX,
                weight,
            ));
        }
    }
    Ok(weight)
}
