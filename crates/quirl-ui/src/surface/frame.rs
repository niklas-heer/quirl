use super::{
    completion::CompletionState,
    editor::EditorState,
    highlight::{DiagnosticSeverity, SurfaceDiagnostic},
    overlay::PickerLayout,
    runtime::{RuntimeSurfaceState, PANEL_VISIBLE_ROWS_MAX},
    statusbar::StatusBarModel,
    transcript::Transcript,
};
use crate::theme::Theme;
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
    pub runtime: &'a RuntimeSurfaceState,
    pub transcript: Option<&'a Transcript>,
    pub transcript_truncated: bool,
    pub output_focus: bool,
    pub output_notice: Option<&'a str>,
}

impl FrameModel<'_> {
    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.height == 0 || area.width == 0 {
            return;
        }
        let input = self.input_render(usize::from(area.width));
        let layout = frame_layout(
            area,
            u16::try_from(input.lines.len()).unwrap_or(u16::MAX),
            self.diagnostic.is_some() && !self.compact,
            self.transcript.map_or(0, Transcript::line_count),
            self.transcript.is_none_or(Transcript::follows_tail),
        );
        if let Some(transcript) = self.transcript {
            self.render_transcript(frame, layout.transcript, transcript);
        }
        if let Some(context_area) = layout.context {
            self.render_context(frame, context_area);
        }
        let input_line_start = visible_input_start(
            input.lines.len(),
            input.cursor_line,
            usize::from(layout.input.height),
        );
        let visible_input = input
            .lines
            .into_iter()
            .skip(input_line_start)
            .take(usize::from(layout.input.height))
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible_input), layout.input);

        if let Some((diagnostic, diagnostic_area)) = self
            .diagnostic
            .filter(|_| !self.compact)
            .zip(layout.diagnostic)
        {
            let glyph = diagnostic_glyph(diagnostic.severity, self.unicode);
            let style = self.theme.diagnostic(diagnostic.severity);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(glyph, style),
                    Span::styled(escape_terminal_line(&diagnostic.message), style),
                ])),
                diagnostic_area,
            );
        }
        let information_requested = self.completion.open || self.runtime.focused_panel().is_some();
        let information_area = if layout.information.height >= 3 || !information_requested {
            layout.information
        } else {
            // Once a session reaches the bottom row there is no space below the
            // live prompt. Reuse a bounded tail slice of scrollback as an
            // overlay so opening completion never moves the prompt.
            let height = if self.picker_layout == PickerLayout::Full {
                layout.transcript.height
            } else {
                layout.transcript.height.min(12)
            };
            Rect::new(
                layout.transcript.x,
                layout.transcript.bottom().saturating_sub(height),
                layout.transcript.width,
                height,
            )
        };
        if information_requested && layout.information.height < 3 {
            frame.render_widget(Clear, information_area);
        }
        self.render_information(frame, information_area);

        let status = StatusBarModel {
            editor: self.editor,
            completion: self.completion,
            mode: self.mode,
            width: area.width,
            hints: self.hints,
            notice: self
                .diagnostic
                .filter(|_| self.compact)
                .map(|diagnostic| diagnostic.message.as_str())
                .or_else(|| {
                    self.transcript
                        .is_some_and(|transcript| !transcript.follows_tail())
                        .then_some("SCROLL · PageUp/PageDown or wheel · Ctrl-End return")
                })
                .or(self.output_notice)
                .or_else(|| {
                    self.output_focus
                        .then_some("OUTPUT · ↑↓ select · y copy · Esc return")
                })
                .or_else(|| {
                    self.transcript_truncated
                        .then_some("older output evicted · PageUp/PageDown scroll")
                })
                .or_else(|| self.runtime.notice())
                .or_else(|| self.runtime.activity()),
            timings: self.timings,
            unicode: self.unicode,
        };
        frame.render_widget(Paragraph::new(status.line(self.theme)), layout.status);

        if layout.input.height == 0 {
            return;
        }
        let x = area
            .x
            .saturating_add(u16::try_from(input.cursor_column).unwrap_or(u16::MAX))
            .min(area.right().saturating_sub(1));
        let cursor_line = input.cursor_line.saturating_sub(input_line_start);
        let y = layout
            .input
            .y
            .saturating_add(u16::try_from(cursor_line).unwrap_or(u16::MAX))
            .min(layout.input.bottom().saturating_sub(1));
        frame.set_cursor_position(Position::new(x, y));
    }

    fn render_information(&self, frame: &mut Frame<'_>, area: Rect) {
        if self.completion.open && area.height >= 2 {
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
            let popup_height = area.height.min(desired_popup_height);
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
                area.y,
                area.width.saturating_sub(offset),
                popup_height,
            );
            let docs_allowed = self.picker_query.map_or(area.width >= 72, |_| {
                self.picker_preview
                    && (area.width >= 100
                        || (self.picker_layout == PickerLayout::Full && area.width >= 72))
            });
            self.render_completion(frame, popup_area, docs_allowed);
        } else if area.height >= 3 {
            if let Some((id, panel)) = self.runtime.focused_panel() {
                let desired = u16::try_from(panel.rows.len().min(PANEL_VISIBLE_ROWS_MAX))
                    .unwrap_or(u16::MAX)
                    .saturating_add(3);
                self.render_panel(
                    frame,
                    Rect::new(area.x, area.y, area.width, area.height.min(desired)),
                    id,
                    panel,
                );
            }
        }
    }

    fn render_transcript(&self, frame: &mut Frame<'_>, area: Rect, transcript: &Transcript) {
        if area.height == 0 || transcript.line_count() == 0 {
            return;
        }
        let visible_count = usize::from(area.height);
        let visible = transcript.visible_range(visible_count);
        let selection = transcript.selection_range();
        let lines = visible
            .clone()
            .filter_map(|line_index| {
                let text = transcript.line(line_index)?;
                let selected = selection.is_some_and(|(start, end)| {
                    line_index >= start.line_index && line_index <= end.line_index
                });
                let style = if selected {
                    self.theme.selected(self.mode)
                } else if text.starts_with('❯') {
                    self.theme.accent(self.mode)
                } else if text.starts_with("── exit ") {
                    self.theme.dim()
                } else {
                    Default::default()
                };
                Some(Line::styled(escape_terminal_line(text), style))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
        if transcript.line_count() > visible_count {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some(if self.unicode { "│" } else { "|" }))
                .thumb_symbol(if self.unicode { "█" } else { "#" })
                .thumb_style(self.theme.accent(self.mode));
            let mut state = ScrollbarState::new(transcript.line_count())
                .position(visible.start)
                .viewport_content_length(visible_count);
            frame.render_stateful_widget(scrollbar, area, &mut state);
        }
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

    fn render_panel(&self, frame: &mut Frame<'_>, area: Rect, id: &str, panel: &crate::PanelModel) {
        let title = format!(
            " {} · {} · {}/{} · F6 next ",
            escape_terminal_line(id),
            escape_terminal_line(&panel.title),
            self.runtime.panel_focus_position(),
            self.runtime.panel_count()
        );
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(self.theme.border());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        let mut lines = Vec::new();
        lines.push(Line::styled(
            panel
                .columns
                .iter()
                .map(|value| escape_terminal_line(value))
                .collect::<Vec<_>>()
                .join(" │ "),
            self.theme.context_secondary(),
        ));
        lines.extend(
            panel
                .rows
                .iter()
                .take(usize::from(inner.height.saturating_sub(1)))
                .map(|row| {
                    Line::raw(
                        row.iter()
                            .map(|value| escape_terminal_line(value))
                            .collect::<Vec<_>>()
                            .join(" │ "),
                    )
                }),
        );
        frame.render_widget(Paragraph::new(lines), inner);
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
            for span in self
                .highlight_spans
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameLayout {
    transcript: Rect,
    context: Option<Rect>,
    input: Rect,
    diagnostic: Option<Rect>,
    information: Rect,
    status: Rect,
}

fn frame_layout(
    area: Rect,
    input_rows: u16,
    diagnostic_visible: bool,
    transcript_line_count: usize,
    follows_tail: bool,
) -> FrameLayout {
    // Completion and panel state deliberately do not participate in this partition. Both reuse
    // the remaining bounded rectangle, so asynchronous completion transitions cannot move input.
    let status_y = area.bottom().saturating_sub(1);
    let status = Rect::new(area.x, status_y, area.width, 1);
    let rows_above_status = status_y.saturating_sub(area.y);
    if transcript_line_count > 0 && !follows_tail {
        let transcript = Rect::new(area.x, area.y, area.width, rows_above_status);
        return FrameLayout {
            transcript,
            context: None,
            input: Rect::new(area.x, status_y, area.width, 0),
            diagnostic: None,
            information: Rect::new(area.x, status_y, area.width, 0),
            status,
        };
    }

    let context_height = u16::from(rows_above_status >= 2);
    let available_after_context = rows_above_status.saturating_sub(context_height);
    let diagnostic_height = u16::from(diagnostic_visible && available_after_context >= 2);
    let input_height = input_rows.min(available_after_context.saturating_sub(diagnostic_height));
    let current_height = context_height
        .saturating_add(input_height)
        .saturating_add(diagnostic_height);
    let transcript_height = u16::try_from(transcript_line_count)
        .unwrap_or(u16::MAX)
        .min(rows_above_status.saturating_sub(current_height));
    let transcript = Rect::new(area.x, area.y, area.width, transcript_height);
    let context_y = transcript.bottom();
    let context = (context_height == 1).then_some(Rect::new(area.x, context_y, area.width, 1));
    let input_y = context_y.saturating_add(context_height);
    let input = Rect::new(area.x, input_y, area.width, input_height);
    let diagnostic_y = input.bottom();
    let diagnostic = (diagnostic_height == 1).then_some(Rect::new(
        area.x,
        diagnostic_y,
        area.width,
        diagnostic_height,
    ));
    let information_y = diagnostic_y.saturating_add(diagnostic_height);
    let information = Rect::new(
        area.x,
        information_y,
        area.width,
        status_y.saturating_sub(information_y),
    );
    FrameLayout {
        transcript,
        context,
        input,
        diagnostic,
        information,
        status,
    }
}

fn visible_input_start(line_count: usize, cursor_line: usize, visible_rows: usize) -> usize {
    if visible_rows == 0 {
        return 0;
    }
    cursor_line
        .saturating_add(1)
        .saturating_sub(visible_rows)
        .min(line_count.saturating_sub(visible_rows))
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
        let runtime = RuntimeSurfaceState::new();
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
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
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

    fn draw_runtime_model(
        terminal: &mut Terminal<TestBackend>,
        editor: &EditorState,
        completion: &CompletionState,
        runtime: &RuntimeSurfaceState,
    ) {
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~/project",
                    context_right: "",
                    editor,
                    completion,
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
                    picker_query: None,
                    picker_layout: PickerLayout::Adaptive,
                    picker_preview: true,
                    detail_scroll: 0,
                    runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                }
                .render(frame);
            })
            .unwrap();
    }

    #[test]
    fn rest_frame_has_context_input_and_persistent_status_rows() {
        let terminal = rendered_model(78, 3, "git status", None, |_| {});
        assert!(row(&terminal, 0).contains("~/P/q  on main"));
        assert!(row(&terminal, 1).contains("❯ git status"));
        assert!(row(&terminal, 2).contains("NORMAL"));
        assert_eq!(
            terminal.backend().buffer().cell((2, 1)).unwrap().fg,
            Color::Rgb(158, 206, 106)
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
        let runtime = RuntimeSurfaceState::new();
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
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
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
            Color::Rgb(158, 206, 106)
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
        let runtime = RuntimeSurfaceState::new();
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
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                }
                .render(frame);
            })
            .unwrap();
        assert!(row(&terminal, 1).contains("> git"));
        assert!(!row(&terminal, 1).contains("status"));
        assert_eq!(
            terminal.backend().buffer().cell((2, 1)).unwrap().fg,
            Color::Rgb(158, 206, 106)
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
        assert!(row(&terminal, 1).contains("▦ files"));
        assert!(row(&terminal, 2).contains("DATA"));
        assert!(!row(&terminal, 2).contains("NORMAL"));
    }

    #[test]
    fn cached_panel_region_renders_without_displacing_the_status_row() {
        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let mut runtime = RuntimeSurfaceState::new();
        assert!(
            runtime.install_panel_batch(crate::surface::runtime::InteractivePanelBatch {
                generation: 1,
                panels: vec![crate::surface::runtime::InteractivePanelSnapshot {
                    id: "demo".to_owned(),
                    model: crate::PanelModel::new(
                        "status",
                        vec!["name".to_owned(), "state".to_owned()],
                        vec![vec!["worker".to_owned(), "ready".to_owned()]],
                        "no workers",
                    )
                    .unwrap(),
                }],
            })
        );
        let mut terminal = Terminal::new(TestBackend::new(78, 7)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
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
                    picker_query: None,
                    picker_layout: PickerLayout::Adaptive,
                    picker_preview: true,
                    detail_scroll: 0,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                }
                .render(frame);
            })
            .unwrap();
        let rendered = (0..7).map(|y| row(&terminal, y)).collect::<String>();
        assert!(rendered.contains("demo · status"));
        assert!(rendered.contains("worker │ ready"));
        assert!(row(&terminal, 6).contains("NORMAL"));
    }

    #[test]
    fn completion_transitions_keep_editor_cursor_and_panel_region_stable() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("git st");
        let mut completion = CompletionState::new(Catalog::builtin(), None);
        let mut runtime = RuntimeSurfaceState::new();
        assert!(
            runtime.install_panel_batch(crate::surface::runtime::InteractivePanelBatch {
                generation: 1,
                panels: vec![crate::surface::runtime::InteractivePanelSnapshot {
                    id: "demo".to_owned(),
                    model: crate::PanelModel::new(
                        "status",
                        vec!["state".to_owned()],
                        vec![vec!["ready".to_owned()]],
                        "empty",
                    )
                    .unwrap(),
                }],
            })
        );
        let mut terminal = Terminal::new(TestBackend::new(78, 12)).unwrap();

        draw_runtime_model(&mut terminal, &editor, &completion, &runtime);
        let cursor_at_rest = terminal.get_cursor_position().unwrap();
        assert!(row(&terminal, 1).contains("❯ git st"));
        assert!(row(&terminal, 2).contains("demo · status"));

        completion.open_manual(
            vec![CompletionItem {
                value: "git status".to_owned(),
                display: "status".to_owned(),
                summary: "working tree".to_owned(),
                detail: "git status".to_owned(),
                replace_start: 4,
                replace_end: 6,
                match_indices: vec![0, 1],
                kind: CompletionKind::Command,
                source: "catalog",
                trust: "builtin",
            }],
            "catalog",
        );
        draw_runtime_model(&mut terminal, &editor, &completion, &runtime);
        assert_eq!(terminal.get_cursor_position().unwrap(), cursor_at_rest);
        assert!(row(&terminal, 1).contains("❯ git st"));
        assert!(row(&terminal, 2).contains("completions"));

        completion.dismiss();
        draw_runtime_model(&mut terminal, &editor, &completion, &runtime);
        assert_eq!(terminal.get_cursor_position().unwrap(), cursor_at_rest);
        assert!(row(&terminal, 1).contains("❯ git st"));
        assert!(row(&terminal, 2).contains("demo · status"));
        assert!(row(&terminal, 11).contains("NORMAL"));
    }

    #[test]
    fn tiny_terminal_suppresses_the_optional_panel_region() {
        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let mut runtime = RuntimeSurfaceState::new();
        assert!(
            runtime.install_panel_batch(crate::surface::runtime::InteractivePanelBatch {
                generation: 1,
                panels: vec![crate::surface::runtime::InteractivePanelSnapshot {
                    id: "demo".to_owned(),
                    model: crate::PanelModel::new(
                        "status",
                        vec!["value".to_owned()],
                        vec![vec!["ready".to_owned()]],
                        "empty",
                    )
                    .unwrap(),
                }],
            })
        );
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
            compact: true,
            picker_query: None,
            picker_layout: PickerLayout::Adaptive,
            picker_preview: true,
            detail_scroll: 0,
            runtime: &runtime,
            transcript: None,
            transcript_truncated: false,
            output_focus: false,
            output_notice: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(78, 4)).unwrap();
        terminal.draw(|frame| model.render(frame)).unwrap();
        let rendered = (0..4).map(|y| row(&terminal, y)).collect::<String>();
        assert!(!rendered.contains("demo"));
        assert!(row(&terminal, 3).contains("NORMAL"));
    }

    #[test]
    fn tiny_layout_prioritizes_status_then_editor_without_out_of_bounds_regions() {
        let one_row = frame_layout(Rect::new(0, 0, 1, 1), 1, true, 0, true);
        assert_eq!(one_row.status, Rect::new(0, 0, 1, 1));
        assert_eq!(one_row.input.height, 0);
        assert_eq!(one_row.information.height, 0);
        assert!(one_row.context.is_none());
        assert!(one_row.diagnostic.is_none());

        let two_rows = frame_layout(Rect::new(0, 0, 1, 2), u16::MAX, true, 0, true);
        assert_eq!(two_rows.input, Rect::new(0, 0, 1, 1));
        assert_eq!(two_rows.status, Rect::new(0, 1, 1, 1));
        assert!(two_rows.context.is_none());
        assert!(two_rows.diagnostic.is_none());

        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut terminal = Terminal::new(TestBackend::new(1, 2)).unwrap();
        draw_runtime_model(&mut terminal, &editor, &completion, &runtime);
        assert_eq!(terminal.get_cursor_position().unwrap(), Position::new(0, 0));
    }

    #[test]
    fn resized_multiline_editor_keeps_the_cursor_on_a_visible_input_row() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("one\ntwo\nthree\nfour");
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).unwrap();

        draw_runtime_model(&mut terminal, &editor, &completion, &runtime);
        assert!(row(&terminal, 2).contains("four"));
        assert_eq!(terminal.get_cursor_position().unwrap().y, 2);
        assert!(row(&terminal, 3).contains("NORMAL"));

        terminal.backend_mut().resize(12, 3);
        draw_runtime_model(&mut terminal, &editor, &completion, &runtime);
        assert!(row(&terminal, 1).contains("four"));
        assert_eq!(terminal.get_cursor_position().unwrap().y, 1);
        assert!(row(&terminal, 2).contains("NORMAL"));
    }

    #[test]
    fn fullscreen_status_stays_on_the_actual_bottom_row_after_resize() {
        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
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
            picker_query: None,
            picker_layout: PickerLayout::Adaptive,
            picker_preview: true,
            detail_scroll: 0,
            runtime: &runtime,
            transcript: None,
            transcript_truncated: false,
            output_focus: false,
            output_notice: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(78, 12)).unwrap();
        terminal.draw(|frame| model.render(frame)).unwrap();
        assert!(row(&terminal, 0).contains("~/project"));
        assert!(row(&terminal, 1).contains('❯'));
        assert!(row(&terminal, 11).contains("NORMAL"));
        assert!(row(&terminal, 3).trim().is_empty());
        assert!(row(&terminal, 10).trim().is_empty());

        terminal.backend_mut().resize(52, 6);
        terminal.draw(|frame| model.render(frame)).unwrap();
        assert!(row(&terminal, 0).contains("~/project"));
        assert!(row(&terminal, 1).contains('❯'));
        assert!(row(&terminal, 5).contains("NORMAL"));
        assert!(row(&terminal, 4).trim().is_empty());
    }

    #[test]
    fn transcript_pushes_the_live_prompt_down_then_scrolls_under_the_fixed_status() {
        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut transcript = Transcript::new(crate::surface::transcript::TranscriptLimits {
            line_count_max: 64,
            retained_bytes_max: 4_096,
        });
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();

        for line in ["❯ pwd", "/workspace"] {
            transcript.append_line(line);
        }
        let draw = |terminal: &mut Terminal<TestBackend>, transcript: &Transcript| {
            terminal
                .draw(|frame| {
                    FrameModel {
                        context_left: "~/workspace",
                        context_right: "2ms",
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
                        picker_query: None,
                        picker_layout: PickerLayout::Adaptive,
                        picker_preview: true,
                        detail_scroll: 0,
                        runtime: &runtime,
                        transcript: Some(transcript),
                        transcript_truncated: false,
                        output_focus: false,
                        output_notice: None,
                    }
                    .render(frame);
                })
                .unwrap();
        };

        draw(&mut terminal, &transcript);
        assert!(row(&terminal, 0).contains("❯ pwd"));
        assert!(row(&terminal, 2).contains("~/workspace"));
        assert!(row(&terminal, 3).contains('❯'));
        assert!(row(&terminal, 7).contains("NORMAL"));

        for index in 0..10 {
            transcript.append_line(&format!("output-{index}"));
        }
        draw(&mut terminal, &transcript);
        assert!(row(&terminal, 4).contains("output-9"));
        assert!(row(&terminal, 5).contains("~/workspace"));
        assert!(row(&terminal, 6).contains('❯'));
        assert!(row(&terminal, 7).contains("NORMAL"));

        assert!(transcript.page_up(7));
        draw(&mut terminal, &transcript);
        assert!(!row(&terminal, 5).contains("~/workspace"));
        assert!(!row(&terminal, 6).contains('❯'));
        assert!(row(&terminal, 7).contains("SCROLL"));
    }

    #[test]
    fn full_transcript_completion_overlays_output_without_moving_the_prompt() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("git st");
        let mut completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut transcript = Transcript::new(crate::surface::transcript::TranscriptLimits {
            line_count_max: 64,
            retained_bytes_max: 4_096,
        });
        for index in 0..20 {
            transcript.append_line(&format!("output-{index}"));
        }
        let mut terminal = Terminal::new(TestBackend::new(78, 12)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>, completion: &CompletionState| {
            terminal
                .draw(|frame| {
                    FrameModel {
                        context_left: "~/workspace",
                        context_right: "",
                        editor: &editor,
                        completion,
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
                        picker_query: None,
                        picker_layout: PickerLayout::Adaptive,
                        picker_preview: true,
                        detail_scroll: 0,
                        runtime: &runtime,
                        transcript: Some(&transcript),
                        transcript_truncated: false,
                        output_focus: false,
                        output_notice: None,
                    }
                    .render(frame);
                })
                .unwrap();
        };

        draw(&mut terminal, &completion);
        let cursor_at_rest = terminal.get_cursor_position().unwrap();
        assert_eq!(cursor_at_rest.y, 10);
        completion.open_manual(
            vec![CompletionItem {
                value: "git status".to_owned(),
                display: "status".to_owned(),
                summary: "Show working tree status".to_owned(),
                detail: "git status [--short]".to_owned(),
                replace_start: 4,
                replace_end: 6,
                match_indices: vec![0, 1],
                kind: CompletionKind::Command,
                source: "catalog",
                trust: "builtin",
            }],
            "catalog",
        );
        draw(&mut terminal, &completion);
        assert_eq!(terminal.get_cursor_position().unwrap(), cursor_at_rest);
        let rendered = (0..12).map(|y| row(&terminal, y)).collect::<String>();
        assert!(rendered.contains("completions"));
        assert!(row(&terminal, 10).contains("❯ git st"));
        assert!(row(&terminal, 11).contains("NORMAL"));
    }

    #[test]
    fn diagnostics_render_as_a_separate_advisory_row() {
        let terminal = rendered_model(78, 4, "gti status", Some("unknown command `gti`"), |_| {});
        assert!(row(&terminal, 2).contains("unknown command `gti`"));
        assert!(row(&terminal, 3).contains("NORMAL"));
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
        let runtime = RuntimeSurfaceState::new();
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
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                };
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
    fn adaptive_command_palette_uses_the_stable_top_information_region() {
        let editor = EditorState::new("emacs", Vec::new());
        let mut completion = CompletionState::new(Catalog::builtin(), None);
        completion.show_picker_results(
            vec![CompletionItem {
                value: "git status".to_owned(),
                display: "git status".to_owned(),
                summary: "working tree".to_owned(),
                detail: "preview detail".to_owned(),
                replace_start: 0,
                replace_end: 0,
                match_indices: Vec::new(),
                kind: CompletionKind::Command,
                source: "catalog",
                trust: "validated",
            }],
            "picker",
        );
        let runtime = RuntimeSurfaceState::new();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
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
                    picker_query: Some(""),
                    picker_layout: PickerLayout::Adaptive,
                    picker_preview: true,
                    detail_scroll: 0,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                };
                model.render(frame);
            })
            .unwrap();

        assert!(row(&terminal, 0).contains("~/project"));
        assert!(row(&terminal, 1).contains('❯'));
        assert!(row(&terminal, 2).contains("picker"));
        assert!(row(&terminal, 19).trim().is_empty());
        assert!(row(&terminal, 29).contains("NORMAL"));
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
        let runtime = RuntimeSurfaceState::new();
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
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                }
                .render(frame);
            })
            .unwrap();
        let flag_cell = terminal
            .backend()
            .buffer()
            .cell((u16::try_from(range.start + 2).unwrap(), 1))
            .unwrap();
        assert_eq!(flag_cell.fg, Color::Rgb(224, 175, 104));
        assert!(flag_cell.modifier.contains(Modifier::UNDERLINED));
        assert!(row(&terminal, 2).contains("▲ unknown flag"));
    }
}
