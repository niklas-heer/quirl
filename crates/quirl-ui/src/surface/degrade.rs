use std::{env, io::IsTerminal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Rich,
    Simple,
}

pub fn select_surface(requested: &str) -> SurfaceKind {
    let stderr_is_terminal = std::io::stderr().is_terminal();
    let term_is_dumb = env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"));
    let terminal_height = crossterm::terminal::size().ok().map(|(_, height)| height);
    select_surface_for_capabilities(requested, stderr_is_terminal, term_is_dumb, terminal_height)
}

fn select_surface_for_capabilities(
    requested: &str,
    stderr_is_terminal: bool,
    term_is_dumb: bool,
    terminal_height: Option<u16>,
) -> SurfaceKind {
    let terminal_is_capable =
        stderr_is_terminal && !term_is_dumb && terminal_height.is_some_and(|height| height >= 5);
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
            select_surface_for_capabilities("simple", true, false, Some(24)),
            SurfaceKind::Simple
        );
    }

    #[test]
    fn capable_terminal_selects_the_rich_surface() {
        assert_eq!(
            select_surface_for_capabilities("auto", true, false, Some(24)),
            SurfaceKind::Rich
        );
        assert_eq!(
            select_surface_for_capabilities("rich", true, false, Some(5)),
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
        for (stderr_is_terminal, term_is_dumb, terminal_height) in [
            (false, false, Some(24)),
            (true, true, Some(24)),
            (true, false, Some(4)),
        ] {
            assert_eq!(
                select_surface_for_capabilities(
                    "rich",
                    stderr_is_terminal,
                    term_is_dumb,
                    terminal_height,
                ),
                SurfaceKind::Simple
            );
        }
    }
}
