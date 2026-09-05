use std::{env, io::IsTerminal};

use super::{RICH_TERMINAL_COLUMNS_MAX, RICH_TERMINAL_ROWS_MAX};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Interactive rendering implementation selected for current terminal capabilities.
pub enum SurfaceKind {
    /// Full-screen raw-mode interface with completion, highlighting, and overlays.
    Rich,
    /// Conservative line editor that avoids alternate-screen terminal ownership.
    Simple,
}

/// Select the safe interactive surface from configuration and host capabilities.
///
/// `"simple"` always selects [`SurfaceKind::Simple`]. Any other request may use
/// [`SurfaceKind::Rich`] only when stderr is a terminal, `TERM` is not `dumb`,
/// terminal dimensions are available, height is at least five rows, and the
/// dimensions fit the bounded rich frame. Missing
/// or inadequate capability information fails closed to the simple surface,
/// including when the caller explicitly requested `"rich"`.
pub fn select_surface(requested: &str) -> SurfaceKind {
    let stderr_is_terminal = std::io::stderr().is_terminal();
    let term_is_dumb = env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
    let terminal_size = crate::terminal_size().ok();
    select_surface_for_capabilities(requested, stderr_is_terminal, term_is_dumb, terminal_size)
}

fn select_surface_for_capabilities(
    requested: &str,
    stderr_is_terminal: bool,
    term_is_dumb: bool,
    terminal_size: Option<(u16, u16)>,
) -> SurfaceKind {
    let terminal_is_capable = stderr_is_terminal
        && !term_is_dumb
        && terminal_size.is_some_and(|(columns, rows)| {
            columns > 0
                && rows >= 5
                && columns <= RICH_TERMINAL_COLUMNS_MAX
                && rows <= RICH_TERMINAL_ROWS_MAX
        });
    if requested == "simple" || !terminal_is_capable {
        SurfaceKind::Simple
    } else {
        SurfaceKind::Rich
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_simple_surface_always_degrades() {
        assert_eq!(
            select_surface_for_capabilities("simple", true, false, Some((80, 24))),
            SurfaceKind::Simple
        );
    }

    #[test]
    fn capable_terminal_selects_the_rich_surface() {
        assert_eq!(
            select_surface_for_capabilities("auto", true, false, Some((80, 24))),
            SurfaceKind::Rich
        );
        assert_eq!(
            select_surface_for_capabilities("rich", true, false, Some((80, 5))),
            SurfaceKind::Rich
        );
    }

    #[test]
    fn unavailable_terminal_size_fails_closed_to_the_simple_surface() {
        assert_eq!(
            select_surface_for_capabilities("auto", true, false, None),
            SurfaceKind::Simple
        );
        assert_eq!(
            select_surface_for_capabilities("rich", true, false, None),
            SurfaceKind::Simple
        );
    }

    #[test]
    fn hard_terminal_limits_override_a_rich_request() {
        for (stderr_is_terminal, term_is_dumb, terminal_size) in [
            (false, false, Some((80, 24))),
            (true, true, Some((80, 24))),
            (true, false, Some((80, 4))),
            (true, false, Some((0, 24))),
            (true, false, Some((RICH_TERMINAL_COLUMNS_MAX + 1, 24))),
            (true, false, Some((80, RICH_TERMINAL_ROWS_MAX + 1))),
        ] {
            assert_eq!(
                select_surface_for_capabilities(
                    "rich",
                    stderr_is_terminal,
                    term_is_dumb,
                    terminal_size,
                ),
                SurfaceKind::Simple
            );
        }
    }
}
