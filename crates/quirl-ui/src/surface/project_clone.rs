//! Bounded, explicit project-clone choices and deferred project navigation.
//!
//! The modal never executes commands or persists preferences. Enter and Escape
//! retain the submitted command unless navigation explicitly selected a choice.
//! A borrowed input lease restores cooked mode after every outcome, including
//! drawing/input failures. Rendering partitions history and controls rather than
//! painting controls over retained command output.

use super::{
    EVENT_POLL, InteractiveSignal, PROJECT_FIELD_BYTES_MAX, RichSurface, SurfaceTerminal,
    resize_fixed_terminal, terminal_error, transcript::Transcript, validate_rich_terminal_size,
};
use crate::{QuirlPrompt, theme::Theme};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use quirl_core::{ErrorCode, ExecutionCancellation, ShellError, escape_terminal_line};
use quirl_syntax::Mode;
use ratatui::{Frame, layout::Rect, text::Line, widgets::Paragraph};
use std::path::PathBuf;
use unicode_width::UnicodeWidthChar;

/// Explicit outcome of the interactive managed-clone suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectCloneChoice {
    /// Execute the submitted command unchanged; the initial Enter/Escape default.
    Original,
    /// Use the displayed managed location for this clone only.
    ManagedOnce,
    /// Use the displayed location and remember managed cloning for future suggestions.
    ManagedAlways,
    /// Execute unchanged and disable future interactive clone suggestions.
    NeverSuggest,
    /// Cancel the submitted clone without executing it or changing preferences.
    Cancel,
}

const CHOICES: [(ProjectCloneChoice, &str); 4] = [
    (
        ProjectCloneChoice::Original,
        "Clone here (original command)",
    ),
    (ProjectCloneChoice::ManagedOnce, "Use this location"),
    (ProjectCloneChoice::ManagedAlways, "Always organize clones"),
    (ProjectCloneChoice::NeverSuggest, "Don't suggest again"),
];

#[derive(Default)]
struct CloneDialog {
    selected: usize,
    path_scroll: usize,
}

impl RichSurface {
    /// Suggest a GHQ-style destination before starting an interactive clone.
    ///
    /// `destination` is the exact proposed path, limited to 4 KiB before terminal
    /// control escaping. The modal reserves its own viewport rows, processes one
    /// event per 16 ms turn, observes the shared cancellation handle each turn,
    /// and restores cooked execution mode on all exits.
    /// Enter initially retains the original command; arrows/Tab select an explicit
    /// alternative. Escape retains the original command, while Ctrl-C/Ctrl-D cancel.
    /// Terminals smaller than 40 columns by 9 rows permit only the original
    /// command or cancellation until resized; hidden alternatives cannot be accepted.
    /// This method requires the rich terminal retained after `read_line` succeeds.
    pub fn choose_clone_location(
        &mut self,
        destination: &str,
        prompt: &QuirlPrompt,
        cancellation: &ExecutionCancellation,
    ) -> Result<ProjectCloneChoice, ShellError> {
        validate_destination(destination)?;
        if cancellation.is_cancelled() {
            return Ok(ProjectCloneChoice::Cancel);
        }
        let destination = escape_terminal_line(destination);
        self.dismiss_picker();
        self.transcript.scroll_to_end();
        let mut lease = CloneInputLease::acquire(&mut self.terminal)?;
        let result = run_dialog(
            &mut lease,
            &self.transcript,
            &destination,
            self.theme,
            prompt.mode,
            cancellation,
        );
        let restored = lease.finish();
        result.and_then(|choice| restored.map(|()| choice))
    }

    /// Offer explicit navigation after a successful clone without opening a modal.
    ///
    /// The next editor shows an Alt-Q u action when no saved typing is pending.
    /// Navigation returns `InteractiveSignal::OpenProject` with the current
    /// buffer intact. The composition root must revalidate the directory and Git
    /// marker before changing directory; this presentation API performs no I/O.
    /// Paths must be absolute and fit the 4 KiB project-path ceiling.
    pub fn offer_project_open(&mut self, path: PathBuf) -> Result<(), ShellError> {
        let observed_bytes = path.as_os_str().as_encoded_bytes().len();
        if observed_bytes > PROJECT_FIELD_BYTES_MAX {
            return Err(ShellError::new(
                ErrorCode::ResourceLimit,
                "cloned project path exceeds the project byte limit",
            )
            .with_context(format!(
                "limit {PROJECT_FIELD_BYTES_MAX} bytes; observed {observed_bytes} bytes"
            ))
            .with_help("Use a shorter project root before offering navigation"));
        }
        if !path.is_absolute() {
            return Err(ShellError::new(
                ErrorCode::Validation,
                "cloned project path is not absolute",
            )
            .with_help("Resolve the cloned repository directory before offering navigation"));
        }
        if self.pending_input.is_some() || self.pending_prefill.is_some() {
            return Ok(());
        }
        self.output_notice = Some(format!(
            "Alt-Q u open project · {} · Alt-Q g all projects",
            super::safe_project_text(&path.to_string_lossy(), PROJECT_FIELD_BYTES_MAX)
        ));
        self.pending_project_open = Some(path);
        Ok(())
    }

    pub(super) fn take_project_open_signal(
        &mut self,
        buffer: String,
        cursor: usize,
    ) -> Option<InteractiveSignal> {
        let path = self.pending_project_open.take()?;
        self.leader_active = false;
        Some(InteractiveSignal::OpenProject {
            path,
            buffer,
            cursor,
        })
    }
}

fn validate_destination(destination: &str) -> Result<(), ShellError> {
    if destination.len() > PROJECT_FIELD_BYTES_MAX {
        return Err(ShellError::new(
            ErrorCode::ResourceLimit,
            "clone destination exceeds the display limit",
        )
        .with_context(format!(
            "limit {PROJECT_FIELD_BYTES_MAX} bytes; observed {} bytes",
            destination.len()
        ))
        .with_help("Use a shorter project root or clone with an explicit destination"));
    }
    if destination.is_empty() {
        return Err(
            ShellError::new(ErrorCode::Validation, "clone destination is empty")
                .with_help("Choose a project root before suggesting a managed clone"),
        );
    }
    Ok(())
}

struct CloneInputLease<'a> {
    terminal: &'a mut SurfaceTerminal,
    active: bool,
}

impl<'a> CloneInputLease<'a> {
    fn acquire(terminal: &'a mut SurfaceTerminal) -> Result<Self, ShellError> {
        if !terminal.active {
            return Err(ShellError::new(
                ErrorCode::Io,
                "clone suggestion requires an active rich terminal",
            )
            .with_help("Run git clone from Quirl's interactive Normal mode"));
        }
        terminal.resume_input()?;
        Ok(Self {
            terminal,
            active: true,
        })
    }

    fn finish(&mut self) -> Result<(), ShellError> {
        self.terminal.pause_for_execution()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for CloneInputLease<'_> {
    fn drop(&mut self) {
        if self.active {
            if self.terminal.active {
                let _ = self.terminal.pause_for_execution();
            } else {
                // A failed pause already released ownership, but its terminal
                // restoration may itself have failed. Retry cleanup without
                // asserting that the alternate screen remains owned.
                self.terminal.reset_best_effort();
            }
        }
    }
}

fn run_dialog(
    lease: &mut CloneInputLease<'_>,
    transcript: &Transcript,
    destination: &str,
    theme: Theme,
    mode: Mode,
    cancellation: &ExecutionCancellation,
) -> Result<ProjectCloneChoice, ShellError> {
    let mut dialog = CloneDialog::default();
    let mut dirty = true;
    let mut choices_visible = false;
    loop {
        if cancellation.is_cancelled() {
            return Ok(ProjectCloneChoice::Cancel);
        }
        if dirty {
            choices_visible = draw_dialog(
                lease.terminal,
                &mut dialog,
                transcript,
                destination,
                theme,
                mode,
            )?;
            dirty = false;
        }
        if !event::poll(EVENT_POLL).map_err(terminal_error("poll clone choices"))? {
            continue;
        }
        match event::read().map_err(terminal_error("read clone choice"))? {
            Event::Key(key) => {
                if let Some(choice) = dialog.handle_visible_key(key, choices_visible) {
                    return Ok(choice);
                }
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
}

impl CloneDialog {
    fn handle_visible_key(
        &mut self,
        key: KeyEvent,
        choices_visible: bool,
    ) -> Option<ProjectCloneChoice> {
        if !choices_visible {
            self.selected = 0;
            if !matches!(
                key.code,
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('c' | 'd')
            ) {
                return None;
            }
        }
        self.handle_key(key)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<ProjectCloneChoice> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('c' | 'd') => Some(ProjectCloneChoice::Cancel),
                _ => None,
            };
        }
        match key.code {
            KeyCode::Esc => Some(ProjectCloneChoice::Original),
            KeyCode::Enter => CHOICES.get(self.selected).map(|(choice, _)| *choice),
            KeyCode::Down | KeyCode::Tab => {
                self.selected = if self.selected.saturating_add(1) >= CHOICES.len() {
                    0
                } else {
                    self.selected.saturating_add(1)
                };
                None
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = if self.selected == 0 {
                    CHOICES.len().saturating_sub(1)
                } else {
                    self.selected.saturating_sub(1)
                };
                None
            }
            KeyCode::PageDown => {
                self.path_scroll = self
                    .path_scroll
                    .saturating_add(3)
                    .min(PROJECT_FIELD_BYTES_MAX.saturating_mul(6));
                None
            }
            KeyCode::PageUp => {
                self.path_scroll = self.path_scroll.saturating_sub(3);
                None
            }
            _ => None,
        }
    }
}

fn draw_dialog(
    owner: &mut SurfaceTerminal,
    dialog: &mut CloneDialog,
    transcript: &Transcript,
    destination: &str,
    theme: Theme,
    mode: Mode,
) -> Result<bool, ShellError> {
    let size = crate::terminal_size().map_err(terminal_error("measure clone choices"))?;
    validate_rich_terminal_size(size)?;
    // Clamp the owned scroll state after every key/resize, not only its drawing
    // offset, so scrolling back from the end never requires undoing hidden steps.
    let path_rows = usize::from(size.1.min(12).saturating_sub(8)).max(1);
    let path_lines = wrap_path(destination, usize::from(size.0)).len();
    dialog.path_scroll = dialog.path_scroll.min(path_lines.saturating_sub(path_rows));
    let terminal = owner.terminal.as_mut().ok_or_else(|| {
        ShellError::new(ErrorCode::Io, "clone choice viewport is unavailable")
            .with_help("Restart Quirl with the simple surface")
    })?;
    let area = Rect::new(0, 0, size.0, size.1);
    if resize_fixed_terminal(terminal, owner.last_size, area)
        .map_err(terminal_error("resize clone choices"))?
    {
        owner.last_size = Some(size);
    }
    terminal
        .draw(|frame| render_dialog(frame, dialog, transcript, destination, theme, mode))
        .map_err(terminal_error("draw clone choices"))?;
    Ok(size.0 >= 40 && size.1 >= 9)
}

fn render_dialog(
    frame: &mut Frame<'_>,
    dialog: &CloneDialog,
    transcript: &Transcript,
    destination: &str,
    theme: Theme,
    mode: Mode,
) {
    let area = frame.area();
    if area.width < 40 || area.height < 9 {
        frame.render_widget(
            Paragraph::new(
                "Resize to 40 columns / 9 rows for clone choices.
Enter/Esc: original command; Ctrl-C: cancel",
            ),
            area,
        );
        return;
    }
    let dialog_height = area.height.min(12);
    let history_height = area.height.saturating_sub(dialog_height);
    let history = transcript.visible_range(usize::from(history_height));
    let lines: Vec<_> = history
        .filter_map(|index| transcript.line(index).map(Line::raw))
        .collect();
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y, area.width, history_height),
    );
    let top = area.y.saturating_add(history_height);
    let lines = dialog_lines(dialog, destination, area.width, dialog_height, theme, mode);
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, top, area.width, dialog_height),
    );
}

fn dialog_lines(
    dialog: &CloneDialog,
    destination: &str,
    width: u16,
    height: u16,
    theme: Theme,
    mode: Mode,
) -> Vec<Line<'static>> {
    let path_rows = usize::from(height.saturating_sub(8));
    let path_lines = wrap_path(destination, usize::from(width));
    let scroll = dialog
        .path_scroll
        .min(path_lines.len().saturating_sub(path_rows.max(1)));
    let mut lines = vec![
        Line::styled("Keep your Git projects organized?", theme.accent(mode)),
        Line::styled(
            "Separate repositories under root/host/owner/project",
            theme.dim(),
        ),
    ];
    lines.extend(
        path_lines
            .into_iter()
            .skip(scroll)
            .take(path_rows)
            .map(Line::raw),
    );
    while lines.len() < path_rows.saturating_add(2) {
        lines.push(Line::raw(""));
    }
    lines.push(Line::raw(""));
    lines.extend(CHOICES.iter().enumerate().map(|(index, (_, label))| {
        Line::styled(
            format!(
                "{} {label}",
                if index == dialog.selected { ">" } else { " " }
            ),
            if index == dialog.selected {
                theme.selected(mode)
            } else {
                theme.dim()
            },
        )
    }));
    lines.push(Line::styled("Up/Down Tab Enter Esc PgUp/PgDn", theme.dim()));
    lines
}

fn wrap_path(path: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut columns: usize = 0;
    for character in path.chars() {
        let character_width = character.width().unwrap_or(0);
        if columns.saturating_add(character_width) > width.max(1) && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            columns = 0;
        }
        line.push(character);
        columns = columns.saturating_add(character_width);
    }
    lines.push(line);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::transcript::TranscriptLimits;
    use quirl_catalog::Catalog;
    use quirl_lua::QuirlConfig;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn surface() -> RichSurface {
        RichSurface::new(
            Arc::new(Catalog::builtin()),
            None,
            Arc::new(crate::StablePickerRanker),
            &QuirlConfig::default(),
            PathBuf::new(),
        )
        .unwrap()
    }

    #[test]
    fn cancelled_clone_proposal_never_acquires_the_terminal() {
        let mut surface = surface();
        let cancellation = ExecutionCancellation::default();
        cancellation.cancel();
        let prompt = QuirlPrompt::with_config(Mode::Command, &QuirlConfig::default());
        assert_eq!(
            surface
                .choose_clone_location("/Projects/host/team/repo", &prompt, &cancellation)
                .unwrap(),
            ProjectCloneChoice::Cancel
        );
        assert!(!surface.terminal.active);
    }

    #[test]
    fn clone_choices_preserve_the_original_command_until_explicit_navigation() {
        let mut dialog = CloneDialog::default();
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            Some(ProjectCloneChoice::Original)
        );
        assert_eq!(dialog.handle_key(key(KeyCode::Down)), None);
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            Some(ProjectCloneChoice::ManagedOnce)
        );
        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            Some(ProjectCloneChoice::ManagedAlways)
        );
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            Some(ProjectCloneChoice::NeverSuggest)
        );
        assert_eq!(
            dialog.handle_key(key(KeyCode::Esc)),
            Some(ProjectCloneChoice::Original)
        );
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.selected, 0);
        dialog.handle_key(key(KeyCode::BackTab));
        assert_eq!(dialog.selected, 3);
    }

    #[test]
    fn shrinking_the_terminal_never_accepts_an_invisible_alternative() {
        let mut dialog = CloneDialog::default();
        dialog.handle_visible_key(key(KeyCode::Down), true);
        assert_eq!(dialog.selected, 1);
        assert_eq!(
            dialog.handle_visible_key(key(KeyCode::Enter), false),
            Some(ProjectCloneChoice::Original)
        );
        dialog.handle_visible_key(key(KeyCode::Down), false);
        assert_eq!(dialog.selected, 0);
        dialog.handle_visible_key(key(KeyCode::Down), true);
        assert_eq!(
            dialog.handle_visible_key(key(KeyCode::Enter), true),
            Some(ProjectCloneChoice::ManagedOnce)
        );
    }

    #[test]
    fn cancellation_and_key_release_cannot_accept_managed_cloning() {
        let mut dialog = CloneDialog::default();
        let mut released = key(KeyCode::Down);
        released.kind = KeyEventKind::Release;
        assert_eq!(dialog.handle_key(released), None);
        assert_eq!(dialog.selected, 0);
        for code in [KeyCode::Char('c'), KeyCode::Char('d')] {
            assert_eq!(
                dialog.handle_key(KeyEvent::new(code, KeyModifiers::CONTROL)),
                Some(ProjectCloneChoice::Cancel)
            );
        }
        dialog.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
        assert_eq!(dialog.selected, 0);
    }

    #[test]
    fn destination_limits_and_terminal_controls_are_validated_before_terminal_ownership() {
        assert!(validate_destination(&"x".repeat(PROJECT_FIELD_BYTES_MAX)).is_ok());
        assert_eq!(
            validate_destination(&"x".repeat(PROJECT_FIELD_BYTES_MAX.saturating_add(1)))
                .unwrap_err()
                .code,
            ErrorCode::ResourceLimit
        );
        assert_eq!(
            validate_destination("").unwrap_err().code,
            ErrorCode::Validation
        );
        let path = "/projects/evil\u{1b}[2J\nrepo";
        let safe = escape_terminal_line(path);
        assert!(!safe.contains('\u{1b}'));
        assert!(!safe.contains('\n'));
        assert!(safe.contains("\\u{1b}"));
        assert_eq!(wrap_path(&safe, 12).concat(), safe);
    }

    #[test]
    fn clone_dialog_reserves_history_rows_and_keeps_all_choices_visible() {
        let mut transcript = Transcript::new(TranscriptLimits {
            line_count_max: 100,
            retained_bytes_max: 4096,
        });
        for index in 0..40 {
            transcript.append_line(&format!("history-{index}"));
        }
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|frame| {
                render_dialog(
                    frame,
                    &CloneDialog::default(),
                    &transcript,
                    "/Projects/github.com/team/repository",
                    Theme::new(false),
                    Mode::Command,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = buffer
            .content
            .chunks(90)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        assert!(rows[0].contains("history-28"));
        assert!(rows[11].contains("history-39"));
        assert!(rows[12].contains("Keep your Git projects organized?"));
        assert!(rows[14].contains("/Projects/github.com/team/repository"));
        assert!(rows[19].contains("> Clone here"));
        assert!(rows[20].contains("Use this location"));
        assert!(rows[21].contains("Always organize clones"));
        assert!(rows[22].contains("Don't suggest again"));
        assert_eq!(transcript.line_count(), 40);
    }

    #[test]
    fn long_unicode_destinations_wrap_without_losing_bytes_and_scroll_independently() {
        let path = "/Projects/".to_owned() + &"日本語/repo/".repeat(20);
        let wrapped = wrap_path(&path, 18);
        assert_eq!(wrapped.concat(), path);
        assert!(
            wrapped
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 18)
        );
        let mut dialog = CloneDialog::default();
        dialog.handle_key(key(KeyCode::PageDown));
        let lines = dialog_lines(&dialog, &path, 18, 12, Theme::new(false), Mode::Command);
        assert_eq!(lines[2].to_string(), wrapped[3]);
        assert_eq!(dialog.selected, 0);
        dialog.handle_key(key(KeyCode::PageUp));
        assert_eq!(dialog.path_scroll, 0);
    }

    #[test]
    fn project_open_preserves_exact_paths_and_current_editor_text() {
        let mut surface = surface();
        let path = std::env::temp_dir().join("cloned project");
        surface.offer_project_open(path.clone()).unwrap();
        assert!(
            surface
                .output_notice
                .as_deref()
                .unwrap()
                .starts_with("Alt-Q u open project")
        );
        surface.open_leader(12);
        assert!(
            surface
                .completion
                .items
                .iter()
                .any(|item| item.value == "u")
        );
        let signal = surface
            .take_project_open_signal("git status".to_owned(), 3)
            .unwrap();
        assert!(!surface.leader_active);
        assert_eq!(
            signal,
            InteractiveSignal::OpenProject {
                path,
                buffer: "git status".to_owned(),
                cursor: 3
            }
        );
        assert!(surface.take_project_open_signal(String::new(), 0).is_none());
    }

    #[test]
    fn project_open_offer_does_not_replace_saved_or_recovered_typing() {
        for restored in [false, true] {
            let mut surface = surface();
            if restored {
                surface.restore_input("git diff".to_owned(), 3).unwrap();
            } else {
                surface.prefill_command("git diff").unwrap();
            }
            surface
                .offer_project_open(std::env::temp_dir().join("cloned"))
                .unwrap();
            assert!(surface.pending_project_open.is_none());
            assert!(surface.output_notice.is_none());
            if restored {
                assert_eq!(surface.pending_input, Some(("git diff".to_owned(), 3)));
            } else {
                assert_eq!(surface.pending_prefill.as_deref(), Some("git diff"));
            }
        }
    }

    #[test]
    fn project_open_rejects_relative_paths_without_mutating_an_existing_offer() {
        let mut surface = surface();
        let path = std::env::temp_dir().join("first");
        surface.offer_project_open(path.clone()).unwrap();
        assert_eq!(
            surface
                .offer_project_open(PathBuf::from("relative"))
                .unwrap_err()
                .code,
            ErrorCode::Validation
        );
        assert_eq!(surface.pending_project_open, Some(path));
    }
}
