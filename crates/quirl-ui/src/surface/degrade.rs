use std::{env, io::IsTerminal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Rich,
    Simple,
}

pub fn select_surface(requested: &str) -> SurfaceKind {
    if requested == "simple"
        || !std::io::stderr().is_terminal()
        || env::var("TERM").is_ok_and(|term| term.eq_ignore_ascii_case("dumb"))
        || crossterm::terminal::size().is_ok_and(|(_, height)| height < 5)
    {
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
        assert_eq!(select_surface("simple"), SurfaceKind::Simple);
    }
}
