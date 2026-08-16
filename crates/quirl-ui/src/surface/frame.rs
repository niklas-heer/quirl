use super::{
    completion::CompletionState,
    editor::EditorState,
    highlight::{DiagnosticSeverity, SurfaceDiagnostic},
    overlay::PickerLayout,
    statusbar::StatusBarModel,
    theme::Theme,
};
use crate::SurfaceSymbols;
use quirl_core::{escape_terminal_controls, escape_terminal_line};
use quirl_syntax::{HighlightKind, HighlightSpan, Mode};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub struct FrameModel<'a> {
    pub context_left: &'a str,
    pub context_right: &'a str,
    pub editor: &'a EditorState,
    pub completion: &'a CompletionState,
    pub mode: Mode,
    pub diagnostic: Option<&'a SurfaceDiagnostic>,
    pub highlight_spans: &'a [HighlightSpan],
    pub theme: Theme,
    pub unicode: bool,
    pub symbols: SurfaceSymbols,
    pub semantic_hints: bool,
    pub hints: bool,
    pub timings: Option<&'a str>,
    /// Compact terminals keep diagnostics in the status row to preserve the editor.
    pub compact: bool,
    pub picker_query: Option<&'a str>,
    pub picker_layout: PickerLayout,
    pub picker_preview: bool,
    pub detail_scroll: u16,
}

impl FrameModel<'_> {
    pub fn height(&self, terminal_height: u16) -> u16 {
        let input_rows =
            u16::try_from(self.editor.buffer().lines().count().max(1)).unwrap_or(u16::MAX);
        let diagnostics = u16::from(self.diagnostic.is_some() && !self.compact);
        let picker_rows = if self.picker_layout == PickerLayout::Full && self.picker_query.is_some()
        {
            usize::from(terminal_height)
        } else {
            10
        };
        let popup = if self.completion.open {
            u16::try_from(self.completion.items.len().min(picker_rows))
                .unwrap_or(u16::MAX)
                .saturating_add(2)
                .saturating_add(u16::from(self.picker_query.is_some()))
                .max(7)
        } else {
            0
        };
        let height = 2_u16
            .saturating_add(input_rows)
            .saturating_add(diagnostics)
            .saturating_add(popup)
            .min(terminal_height.max(1));
        if self.completion.open
            && self.picker_query.is_some()
            && self.picker_layout == PickerLayout::Full
        {
            terminal_height.max(1)
        } else {
            height
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.height == 0 || area.width == 0 {
            return;
        }
        self.render_context(frame, Rect::new(area.x, area.y, area.width, 1));
        let input = self.input_render(usize::from(area.width));
        let input_height = u16::try_from(input.lines.len()).unwrap_or(u16::MAX);
        let input_area = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            input_height.min(area.height.saturating_sub(1)),
        );
        frame.render_widget(Paragraph::new(input.lines), input_area);

        let mut next_y = input_area.y.saturating_add(input_area.height);
        if let Some(diagnostic) = self.diagnostic.filter(|_| !self.compact) {
            let glyph = diagnostic_glyph(diagnostic.severity, self.unicode);
            let style = self.theme.diagnostic(diagnostic.severity);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(glyph, style),
                    Span::styled(escape_terminal_line(&diagnostic.message), style),
                ])),
                Rect::new(area.x, next_y, area.width, 1),
            );
            next_y = next_y.saturating_add(1);
        }
        if self.completion.open && next_y < area.bottom().saturating_sub(1) {
            let row_limit =
                if self.picker_query.is_some() && self.picker_layout == PickerLayout::Full {
                    usize::from(area.height)
                } else {
                    10
                };
            let desired_popup_height = u16::try_from(self.completion.items.len().min(row_limit))
                .unwrap_or(u16::MAX)
                .saturating_add(2)
                .saturating_add(u16::from(self.picker_query.is_some()))
                .max(if area.width >= 100 { 7 } else { 3 });
            let popup_height = area
                .bottom()
                .saturating_sub(next_y)
                .saturating_sub(1)
                .min(desired_popup_height);
            let replace_start = self
                .completion
                .selected_item()
                .map_or(self.editor.cursor(), |item| item.replace_start);
            let (_, token_column) = cursor_visual_position(self.editor.buffer(), replace_start);
            let requested_offset =
                2_u16.saturating_add(u16::try_from(token_column).unwrap_or(u16::MAX));
            let minimum_width = if area.width >= 100 { 80 } else { 32 }.min(area.width);
            let offset = if self.picker_query.is_some() {
                0
            } else {
                requested_offset.min(area.width.saturating_sub(minimum_width))
            };
            let popup_area = Rect::new(
                area.x.saturating_add(offset),
                next_y,
                area.width.saturating_sub(offset),
                popup_height,
            );
            let docs_allowed = self.picker_query.map_or(area.width >= 100, |_| {
                self.picker_preview
                    && (area.width >= 100
                        || (self.picker_layout == PickerLayout::Full && area.width >= 72))
            });
            self.render_completion(frame, popup_area, docs_allowed);
        }
        let status_area = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        let status = StatusBarModel {
            editor: self.editor,
            completion: self.completion,
            mode: self.mode,
            width: area.width,
            hints: self.hints,
            notice: self
                .diagnostic
                .filter(|_| self.compact)
                .map(|diagnostic| diagnostic.message.as_str()),
            timings: self.timings,
            unicode: self.unicode,
        };
        frame.render_widget(Paragraph::new(status.line(self.theme)), status_area);

        let x = area
            .x
            .saturating_add(u16::try_from(input.cursor_column).unwrap_or(u16::MAX))
            .min(area.right().saturating_sub(1));
        let y = input_area
            .y
            .saturating_add(u16::try_from(input.cursor_line).unwrap_or(u16::MAX))
            .min(input_area.bottom().saturating_sub(1));
        frame.set_cursor_position(Position::new(x, y));
    }

    fn render_context(&self, frame: &mut Frame<'_>, area: Rect) {
        let left = escape_terminal_line(self.context_left);
        let right = escape_terminal_line(self.context_right);
        let left_width = UnicodeWidthStr::width(left.as_str());
        let right_width = UnicodeWidthStr::width(right.as_str());
        let gap = usize::from(area.width).saturating_sub(left_width.saturating_add(right_width));
        let right = if gap == 0 && left_width.saturating_add(right_width) > usize::from(area.width)
        {
            String::new()
        } else {
            right
        };
        let left_spans = if let Some(branch_start) = left.find("on ") {
            vec![
                Span::styled(left[..branch_start].to_owned(), self.theme.context()),
                Span::styled(
                    left[branch_start..].to_owned(),
                    self.theme.context_secondary(),
                ),
            ]
        } else {
            vec![Span::styled(left, self.theme.context())]
        };
        let mut spans = left_spans;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(right, self.theme.context_secondary()));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn input_render(&self, width: usize) -> InputRender {
        let mut lines = Vec::new();
        let mut offset = 0_usize;
        let cursor_line = self.editor.buffer()[..self.editor.cursor()]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let mut cursor_column = 0_usize;
        for (line_index, part) in self.editor.buffer().split('\n').enumerate() {
            let indicator = if line_index == 0 {
                self.symbols.input_indicator(self.mode)
            } else {
                self.symbols.multiline_indicator()
            };
            let mut spans = vec![Span::styled(indicator, self.theme.accent(self.mode))];
            let line_end = offset.saturating_add(part.len());
            let local_cursor = (line_index == cursor_line)
                .then(|| self.editor.cursor().saturating_sub(offset).min(part.len()));
            let indicator_width = UnicodeWidthStr::width(indicator);
            let viewport =
                horizontal_viewport(part, local_cursor, width.saturating_sub(indicator_width));
            if let Some(local_cursor) = local_cursor {
                cursor_column = indicator_width.saturating_add(escaped_width(
                    part.get(viewport.start..local_cursor).unwrap_or_default(),
                ));
            }
            let visible_start = offset.saturating_add(viewport.start);
            let visible_end = offset.saturating_add(viewport.end);
            let mut rendered_until = visible_start;
            let highlight_spans = if self.semantic_hints {
                self.highlight_spans
            } else {
                &[]
            };
            for span in highlight_spans
                .iter()
                .filter(|span| span.range.start < visible_end && span.range.end > visible_start)
            {
                let start = span.range.start.max(visible_start);
                let end = span.range.end.min(visible_end);
                if rendered_until < start {
                    let local = rendered_until.saturating_sub(offset)..start.saturating_sub(offset);
                    spans.push(Span::styled(
                        escape_terminal_controls(part.get(local).unwrap_or_default()),
                        self.theme.highlight(HighlightKind::Argument),
                    ));
                }
                let local = start.saturating_sub(offset)..end.saturating_sub(offset);
                let text = escape_terminal_controls(part.get(local).unwrap_or_default());
                spans.push(Span::styled(text, self.input_style(span.kind, start..end)));
                rendered_until = end;
            }
            if rendered_until < visible_end {
                let local = rendered_until.saturating_sub(offset)..viewport.end;
                spans.push(Span::styled(
                    escape_terminal_controls(part.get(local).unwrap_or_default()),
                    self.theme.highlight(HighlightKind::Argument),
                ));
            }
            if self.semantic_hints
                && viewport.end == part.len()
                && line_index + 1 == self.editor.buffer().lines().count().max(1)
            {
                if let Some(suggestion) = self.editor.autosuggestion() {
                    spans.push(Span::styled(
                        escape_terminal_line(suggestion),
                        self.theme.dim().add_modifier(Modifier::ITALIC),
                    ));
                }
            }
            lines.push(Line::from(spans));
            offset = line_end.saturating_add(1);
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                self.symbols.input_indicator(self.mode),
                self.theme.accent(self.mode),
            )));
        }
        InputRender {
            lines,
            cursor_line,
            cursor_column,
        }
    }

    fn input_style(&self, kind: HighlightKind, range: std::ops::Range<usize>) -> Style {
        let base = self.theme.highlight(kind);
        let Some(diagnostic) = self.diagnostic else {
            return base;
        };
        let Some(diagnostic_range) = &diagnostic.range else {
            return base;
        };
        if diagnostic_range.start >= range.end || diagnostic_range.end <= range.start {
            return base;
        }
        base.patch(
            self.theme
                .diagnostic(diagnostic.severity)
                .add_modifier(Modifier::UNDERLINED),
        )
    }

    fn render_completion(&self, frame: &mut Frame<'_>, area: Rect, docs_allowed: bool) {
        if area.height < 2 {
            return;
        }
        frame.render_widget(Clear, area);
        let docs = docs_allowed;
        let list_width = if docs { area.width / 2 } else { area.width };
        let list_area = Rect::new(area.x, area.y, list_width, area.height);
        let block = Block::default()
            .title(if self.picker_query.is_some() {
                " picker "
            } else {
                " completions "
            })
            .borders(Borders::ALL)
            .border_style(self.theme.border());
        let inner = block.inner(list_area);
        frame.render_widget(block, list_area);
        let results_area = if let Some(query) = self.picker_query {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(if self.unicode { "⌕ " } else { "> " }, self.theme.dim()),
                    Span::styled(escape_terminal_line(query), self.theme.accent(self.mode)),
                ])),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            Rect::new(
                inner.x,
                inner.y.saturating_add(1),
                inner.width,
                inner.height.saturating_sub(1),
            )
        } else {
            inner
        };
        let visible_rows = usize::from(results_area.height);
        let start = self
            .completion
            .selected
            .saturating_sub(visible_rows.saturating_sub(1))
            .min(self.completion.items.len().saturating_sub(visible_rows));
        let overflow = self.completion.items.len() > visible_rows;
        let content_area = Rect::new(
            results_area.x,
            results_area.y,
            results_area.width.saturating_sub(u16::from(overflow)),
            results_area.height,
        );
        let rows = self
            .completion
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(visible_rows);
        let lines = rows
            .map(|(index, item)| {
                let glyph = item.kind.glyph(self.unicode);
                let selected = index == self.completion.selected;
                let row_style = if selected {
                    self.theme.selected(self.mode)
                } else {
                    Default::default()
                };
                let mut spans = vec![Span::styled(format!("{glyph} "), row_style)];
                for (character_index, character) in item.display.chars().enumerate() {
                    let text = escape_terminal_controls(&character.to_string());
                    let style = if item.match_indices.contains(&character_index) {
                        self.theme.accent(self.mode)
                    } else {
                        row_style
                    };
                    spans.push(Span::styled(text, style));
                }
                let display = escape_terminal_line(&item.display);
                let padding = 20_usize.saturating_sub(UnicodeWidthStr::width(display.as_str()));
                spans.push(Span::styled(
                    " ".repeat(padding.saturating_add(1)),
                    row_style,
                ));
                spans.push(Span::styled(escape_terminal_line(&item.summary), row_style));
                Line::from(spans).style(row_style)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), content_area);
        if overflow {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some(if self.unicode { "│" } else { "|" }))
                .thumb_symbol(if self.unicode { "█" } else { "#" })
                .thumb_style(self.theme.accent(self.mode));
            let mut state = ScrollbarState::new(self.completion.items.len())
                .position(start)
                .viewport_content_length(visible_rows);
            frame.render_stateful_widget(scrollbar, results_area, &mut state);
        }
        if docs {
            let docs_area = Rect::new(
                area.x.saturating_add(list_width),
                area.y,
                area.width.saturating_sub(list_width),
                area.height,
            );
            let block = Block::default()
                .title(" documentation ")
                .borders(Borders::ALL)
                .border_style(self.theme.border());
            let inner = block.inner(docs_area);
            frame.render_widget(block, docs_area);
            if let Some(item) = self.completion.selected_item() {
                let docs = vec![
                    Line::styled(
                        escape_terminal_line(&item.display),
                        self.theme.accent(self.mode),
                    ),
                    Line::raw(""),
                    Line::raw(escape_terminal_line(&item.detail)),
                    Line::raw(""),
                    Line::styled(
                        format!("source: {} · {}", item.source, item.trust),
                        self.theme.dim(),
                    ),
                ];
                frame.render_widget(
                    Paragraph::new(docs)
                        .wrap(ratatui::widgets::Wrap { trim: true })
                        .scroll((self.detail_scroll, 0)),
                    inner,
                );
            }
        }
    }
}

struct InputRender {
    lines: Vec<Line<'static>>,
    cursor_line: usize,
    cursor_column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HorizontalViewport {
    start: usize,
    end: usize,
}

fn horizontal_viewport(
    value: &str,
    cursor: Option<usize>,
    available_width: usize,
) -> HorizontalViewport {
    if available_width == 0 || value.is_empty() {
        let cursor = cursor.unwrap_or(0).min(value.len());
        return HorizontalViewport {
            start: cursor,
            end: cursor,
        };
    }
    if escaped_width(value) <= available_width {
        return HorizontalViewport {
            start: 0,
            end: value.len(),
        };
    }

    let cursor = cursor.unwrap_or(0).min(value.len());
    let desired_left_width = available_width.saturating_mul(2) / 3;
    let mut start = cursor;
    let mut used_left = 0_usize;
    for (index, grapheme) in value[..cursor].grapheme_indices(true).rev() {
        let grapheme_width = escaped_width(grapheme);
        if used_left.saturating_add(grapheme_width) > desired_left_width {
            break;
        }
        used_left = used_left.saturating_add(grapheme_width);
        start = index;
    }

    let mut end = start;
    let mut used = 0_usize;
    for (offset, grapheme) in value[start..].grapheme_indices(true) {
        let grapheme_width = escaped_width(grapheme);
        if used.saturating_add(grapheme_width) > available_width {
            break;
        }
        used = used.saturating_add(grapheme_width);
        end = start.saturating_add(offset).saturating_add(grapheme.len());
    }
    HorizontalViewport { start, end }
}

fn escaped_width(value: &str) -> usize {
    let escaped = escape_terminal_controls(value);
    UnicodeWidthStr::width(escaped.as_str())
}

fn diagnostic_glyph(severity: DiagnosticSeverity, unicode: bool) -> &'static str {
    match (severity, unicode) {
        (DiagnosticSeverity::Error, true) => "  ✘ ",
        (DiagnosticSeverity::Warning, true) => "  ▲ ",
        (DiagnosticSeverity::Hint, true) => "  ℹ ",
        (DiagnosticSeverity::Error, false) => "  E ",
        (DiagnosticSeverity::Warning, false) => "  W ",
        (DiagnosticSeverity::Hint, false) => "  H ",
    }
}

fn cursor_visual_position(value: &str, cursor: usize) -> (usize, usize) {
    let prefix = &value[..cursor.min(value.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let column_text = prefix.rsplit_once('\n').map_or(prefix, |(_, tail)| tail);
    let escaped_column = escape_terminal_controls(column_text);
    let column = escaped_column
        .chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum();
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::{
        completion::{CompletionItem, CompletionKind},
        editor::EditAction,
        CompletionState,
    };
    use quirl_catalog::Catalog;
    use ratatui::{backend::TestBackend, style::Color, Terminal};

    fn rendered_model_in_mode(
        width: u16,
        height: u16,
        buffer: &str,
        diagnostic: Option<&str>,
        mode: Mode,
        compact: bool,
        configure: impl FnOnce(&mut CompletionState),
    ) -> Terminal<TestBackend> {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste(buffer);
        if !buffer.is_empty() {
            editor.apply(crate::surface::editor::EditAction::MoveLeft);
            editor.apply(crate::surface::editor::EditAction::MoveRight);
        }
        let diagnostic = diagnostic.map(SurfaceDiagnostic::error);
        let highlight_spans = quirl_syntax::highlight(buffer, mode);
        let catalog = Catalog::builtin();
        let mut completion = CompletionState::new(catalog, None);
        configure(&mut completion);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~/P/q  on main",
                    context_right: "12ms",
                    editor: &editor,
                    completion: &completion,
                    mode,
                    diagnostic: diagnostic.as_ref(),
                    highlight_spans: &highlight_spans,
                    theme: Theme::new(true),
                    unicode: true,
                    symbols: SurfaceSymbols::Unicode,
                    semantic_hints: true,
                    hints: true,
                    timings: None,
                    compact,
                    picker_query: None,
                    picker_layout: PickerLayout::Adaptive,
                    picker_preview: true,
                    detail_scroll: 0,
                }
                .render(frame);
            })
            .unwrap();
        terminal
    }

    fn rendered_model(
        width: u16,
        height: u16,
        buffer: &str,
        diagnostic: Option<&str>,
        configure: impl FnOnce(&mut CompletionState),
    ) -> Terminal<TestBackend> {
        rendered_model_in_mode(
            width,
            height,
            buffer,
            diagnostic,
            Mode::Command,
            false,
            configure,
        )
    }

    fn row(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)))
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn rest_frame_has_context_input_and_persistent_status_rows() {
        let terminal = rendered_model(78, 3, "git status", None, |_| {});
        assert!(row(&terminal, 0).contains("~/P/q  on main"));
        assert!(row(&terminal, 1).contains("❯ git status"));
        assert!(row(&terminal, 2).contains("command"));
        assert_eq!(
            terminal.backend().buffer().cell((2, 1)).unwrap().fg,
            Color::Green
        );
    }

    #[test]
    fn long_input_viewport_keeps_grapheme_cursor_and_highlight_visible() {
        let input = format!("start-👨‍👩‍👧‍👦-{}END", "x".repeat(80));
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste(&input);
        let catalog = Catalog::builtin();
        let completion = CompletionState::new(catalog, None);
        let end_start = input.len().saturating_sub(3);
        let spans = vec![HighlightSpan {
            range: end_start..input.len(),
            kind: HighlightKind::StringDouble,
        }];
        let mut terminal = Terminal::new(TestBackend::new(24, 3)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~/project",
                    context_right: "",
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Command,
                    diagnostic: None,
                    highlight_spans: &spans,
                    theme: Theme::new(true),
                    unicode: true,
                    symbols: SurfaceSymbols::Unicode,
                    semantic_hints: true,
                    hints: true,
                    timings: None,
                    compact: false,
                    picker_query: None,
                    picker_layout: PickerLayout::Adaptive,
                    picker_preview: true,
                    detail_scroll: 0,
                }
                .render(frame);
            })
            .unwrap();
        let input_row = row(&terminal, 1);
        assert!(input_row.contains("END"));
        assert!(!input_row.contains("start-"));
        let end_x = input_row.find('E').unwrap();
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((u16::try_from(end_x).unwrap(), 1))
                .unwrap()
                .fg,
            Color::Yellow
        );
        let cursor = terminal.get_cursor_position().unwrap();
        assert!(cursor.x < 24);
        assert_eq!(cursor.y, 1);
    }

    #[test]
    fn rich_input_honors_plain_symbols_and_disabled_semantic_hints() {
        let mut editor = EditorState::new("emacs", vec!["git status".to_owned()]);
        editor.insert_paste("git");
        let catalog = Catalog::builtin();
        let completion = CompletionState::new(catalog, None);
        let spans = vec![HighlightSpan {
            range: 0..3,
            kind: HighlightKind::Command,
        }];
        let mut terminal = Terminal::new(TestBackend::new(40, 3)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "project",
                    context_right: "",
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Command,
                    diagnostic: None,
                    highlight_spans: &spans,
                    theme: Theme::new(true),
                    unicode: false,
                    symbols: SurfaceSymbols::Plain,
                    semantic_hints: false,
                    hints: false,
                    timings: None,
                    compact: false,
                    picker_query: None,
                    picker_layout: PickerLayout::Adaptive,
                    picker_preview: true,
                    detail_scroll: 0,
                }
                .render(frame);
            })
            .unwrap();
        assert!(row(&terminal, 1).contains("> git"));
        assert!(!row(&terminal, 1).contains("status"));
        assert_eq!(
            terminal.backend().buffer().cell((2, 1)).unwrap().fg,
            Color::Reset
        );
    }

    #[test]
    fn nerd_font_profile_uses_only_explicit_private_use_chrome() {
        assert_eq!(
            SurfaceSymbols::NerdFont.input_indicator(Mode::Command),
            "\u{f105} "
        );
        assert!(SurfaceSymbols::NerdFont.uses_unicode());
    }

    #[test]
    fn data_mode_repaints_indicator_and_status_without_relying_on_color() {
        let terminal = rendered_model_in_mode(78, 3, "files", None, Mode::Data, false, |_| {});
        assert!(row(&terminal, 1).contains("◆ files"));
        assert!(row(&terminal, 2).contains("data"));
        assert!(!row(&terminal, 2).contains("command"));
    }

    #[test]
    fn diagnostics_render_as_a_separate_advisory_row() {
        let terminal = rendered_model(78, 4, "gti status", Some("unknown command `gti`"), |_| {});
        assert!(row(&terminal, 2).contains("unknown command `gti`"));
        assert!(row(&terminal, 3).contains("command"));
    }

    #[test]
    fn completion_popup_keeps_documentation_and_source_metadata() {
        let terminal = rendered_model(110, 12, "git st", None, |completion| {
            completion.open_manual(
                vec![CompletionItem {
                    value: "git status".to_owned(),
                    display: "status".to_owned(),
                    summary: "Show working tree status".to_owned(),
                    detail: "git status [--short]".to_owned(),
                    replace_start: 0,
                    replace_end: 6,
                    match_indices: vec![0, 1],
                    kind: CompletionKind::Command,
                    source: "catalog",
                    trust: "builtin",
                }],
                "catalog",
            );
        });
        let rendered = (0..12).map(|y| row(&terminal, y)).collect::<String>();
        assert!(rendered.contains("completions"));
        assert!(rendered.contains("documentation"));
        assert!(rendered.contains("source: catalog"));
        assert!(rendered.contains("· builtin"));
    }

    #[test]
    fn picker_query_and_preview_configuration_render_honestly_inline() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.apply(EditAction::Insert('g'));
        let catalog = Catalog::builtin();
        let mut completion = CompletionState::new(catalog, None);
        completion.show_picker_results(
            vec![CompletionItem {
                value: "git status".to_owned(),
                display: "git status".to_owned(),
                summary: "working tree".to_owned(),
                detail: "preview detail".to_owned(),
                replace_start: 0,
                replace_end: 1,
                match_indices: vec![0, 4],
                kind: CompletionKind::Command,
                source: "catalog",
                trust: "builtin",
            }],
            "history",
        );
        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();
        terminal
            .draw(|frame| {
                let model = FrameModel {
                    context_left: "~/project",
                    context_right: "",
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Command,
                    diagnostic: None,
                    highlight_spans: &[],
                    theme: Theme::new(true),
                    unicode: true,
                    symbols: SurfaceSymbols::Unicode,
                    semantic_hints: true,
                    hints: true,
                    timings: None,
                    compact: false,
                    picker_query: Some("sta"),
                    picker_layout: PickerLayout::Full,
                    picker_preview: false,
                    detail_scroll: 0,
                };
                assert_eq!(model.height(12), 12);
                model.render(frame);
            })
            .unwrap();
        let rendered = (0..12).map(|y| row(&terminal, y)).collect::<String>();
        assert!(rendered.contains("picker"));
        assert!(rendered.contains("⌕ sta"));
        assert!(!rendered.contains("documentation"));
        assert!(!rendered.contains("preview detail"));
    }

    #[test]
    fn hostile_editor_and_completion_text_cannot_emit_terminal_controls() {
        let terminal = rendered_model(110, 12, "echo \u{1b}]0;owned\u{7}", None, |completion| {
            completion.open_manual(
                vec![CompletionItem {
                    value: "safe".to_owned(),
                    display: "bad\u{1b}[2J\u{009b}2J".to_owned(),
                    summary: "line\rrewritten".to_owned(),
                    detail: "detail\nnext".to_owned(),
                    replace_start: 0,
                    replace_end: 0,
                    match_indices: Vec::new(),
                    kind: CompletionKind::Value,
                    source: "plugin",
                    trust: "trusted",
                }],
                "plugin",
            );
        });
        let rendered = (0..12).map(|y| row(&terminal, y)).collect::<String>();
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(!rendered.contains('\r'));
        assert!(rendered.contains("\\u{1b}"));
    }

    #[test]
    fn compact_height_moves_diagnostics_into_the_status_row() {
        let terminal = rendered_model_in_mode(
            78,
            6,
            "gti status",
            Some("unknown command `gti`"),
            Mode::Command,
            true,
            |_| {},
        );
        assert!(!row(&terminal, 2).contains("unknown command"));
        assert!(row(&terminal, 5).contains("unknown command"));
    }

    #[test]
    fn completion_popup_is_token_anchored_and_keeps_selection_visible() {
        let terminal = rendered_model(78, 12, "echo item", None, |completion| {
            let items = (0..20)
                .map(|index| CompletionItem {
                    value: format!("item-{index}"),
                    display: format!("item-{index}"),
                    summary: "choice".to_owned(),
                    detail: "detail".to_owned(),
                    replace_start: 5,
                    replace_end: 9,
                    match_indices: vec![0, 1],
                    kind: CompletionKind::Value,
                    source: "catalog",
                    trust: "builtin",
                })
                .collect();
            completion.open_manual(items, "catalog");
            completion.selected = 15;
        });
        let popup_top = row(&terminal, 2);
        assert_eq!(popup_top.find('┌'), Some(7));
        let rendered = (0..12).map(|y| row(&terminal, y)).collect::<String>();
        assert!(rendered.contains("item-15"));
        assert!(!rendered.contains("item-0 "));
        assert!(rendered.contains('█'));
    }

    #[test]
    fn warning_diagnostic_underlines_the_flag_without_using_error_color() {
        let input = "quirl describe --unknown";
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste(input);
        let catalog = Catalog::builtin();
        let completion = CompletionState::new(catalog, None);
        let range = input.find("--unknown").unwrap()..input.len();
        let diagnostic = SurfaceDiagnostic {
            message: "unknown flag `--unknown`".to_owned(),
            severity: DiagnosticSeverity::Warning,
            range: Some(range.clone()),
        };
        let spans = quirl_syntax::highlight(input, Mode::Command);
        let mut terminal = Terminal::new(TestBackend::new(78, 4)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~/P/q",
                    context_right: "",
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Command,
                    diagnostic: Some(&diagnostic),
                    highlight_spans: &spans,
                    theme: Theme::new(true),
                    unicode: true,
                    symbols: SurfaceSymbols::Unicode,
                    semantic_hints: true,
                    hints: true,
                    timings: None,
                    compact: false,
                    picker_query: None,
                    picker_layout: PickerLayout::Adaptive,
                    picker_preview: true,
                    detail_scroll: 0,
                }
                .render(frame);
            })
            .unwrap();
        let flag_cell = terminal
            .backend()
            .buffer()
            .cell((u16::try_from(range.start + 2).unwrap(), 1))
            .unwrap();
        assert_eq!(flag_cell.fg, Color::Yellow);
        assert!(flag_cell.modifier.contains(Modifier::UNDERLINED));
        assert!(row(&terminal, 2).contains("▲ unknown flag"));
    }
}
