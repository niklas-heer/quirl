use super::{
    completion::CompletionState,
    editor::EditorState,
    environment::EnvironmentExplorer,
    highlight::{DiagnosticSeverity, SurfaceDiagnostic},
    overlay::PickerLayout,
    runtime::{PANEL_VISIBLE_ROWS_MAX, RuntimeSurfaceState},
    statusbar::StatusBarModel,
    transcript::Transcript,
};
use crate::SurfaceSymbols;
use crate::theme::Theme;
use quirl_core::{escape_terminal_controls, escape_terminal_line};
use quirl_syntax::{HighlightKind, HighlightSpan, Mode};
use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Wrap,
    },
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
    pub environment: Option<&'a EnvironmentExplorer>,
    pub runtime: &'a RuntimeSurfaceState,
    pub transcript: Option<&'a Transcript>,
    pub transcript_truncated: bool,
    pub output_focus: bool,
    pub output_notice: Option<&'a str>,
    /// Set while a foreground command still owns execution: the input row's
    /// indicator is replaced with this animated glyph instead of rendering
    /// identically to an idle, ready-for-input prompt.
    pub busy_glyph: Option<char>,
}

impl FrameModel<'_> {
    /// Return the transcript rectangle produced by the same partition used for drawing.
    pub(super) fn transcript_area(&self, area: Rect) -> Rect {
        if self.environment.is_some() {
            return Rect::default();
        }
        let input = self.input_render(usize::from(area.width));
        frame_layout(
            area,
            u16::try_from(input.lines.len()).unwrap_or(u16::MAX),
            self.diagnostic.is_some() && !self.compact,
            self.intent_activity_rows(area.width),
            self.transcript.map_or(0, Transcript::line_count),
            self.transcript.is_none_or(Transcript::follows_tail),
        )
        .transcript
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.height == 0 || area.width == 0 {
            return;
        }
        if let Some(environment) = self.environment {
            environment.render(frame, area, self.theme, self.mode, self.symbols);
            return;
        }
        let input = self.input_render(usize::from(area.width));
        let layout = frame_layout(
            area,
            u16::try_from(input.lines.len()).unwrap_or(u16::MAX),
            self.diagnostic.is_some() && !self.compact,
            self.intent_activity_rows(area.width),
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

        if let Some(activity_area) = layout.intent_activity {
            self.render_intent_activity(frame, activity_area);
        }

        if let Some((diagnostic, diagnostic_area)) = self
            .diagnostic
            .filter(|_| !self.compact)
            .zip(layout.diagnostic)
        {
            let glyph = diagnostic_glyph(diagnostic.severity, self.symbols);
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
        let information_area = information_area(
            &layout,
            information_requested,
            self.picker_layout == PickerLayout::Full,
        );
        if information_area != layout.information {
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
                    (!self.intent_activity_visible())
                        .then_some(self.output_notice)
                        .flatten()
                })
                .or_else(|| {
                    self.output_focus
                        .then_some("OUTPUT · drag/↑↓ select · y or Ctrl-C copy · Esc return")
                })
                .or_else(|| {
                    self.transcript
                        .is_some_and(|transcript| !transcript.follows_tail())
                        .then_some("SCROLL · PageUp/PageDown or wheel · Ctrl-End return")
                })
                .or_else(|| {
                    self.transcript_truncated
                        .then_some("older output evicted · PageUp/PageDown scroll")
                })
                .or_else(|| self.runtime.notice())
                .or_else(|| self.runtime.activity()),
            timings: self.timings,
            symbols: self.symbols,
            assistant_busy: self.mode == Mode::Natural && self.busy_glyph.is_some(),
            assistant_has_proposal: self
                .output_notice
                .is_some_and(|notice| notice.lines().any(|line| line.starts_with("COMMAND\t"))),
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
                .max(if area.width >= 100 { 12 } else { 3 });
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
        } else if area.height >= 3
            && let Some((id, panel)) = self.runtime.focused_panel()
        {
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

    fn intent_activity_visible(&self) -> bool {
        self.mode == Mode::Natural && self.output_notice.is_some()
    }

    fn intent_activity_rows(&self, area_width: u16) -> u16 {
        if !self.intent_activity_visible() {
            return 0;
        }
        let Some(notice) = self.output_notice else {
            return 0;
        };
        let parsed = intent_panel_parts(notice);
        let horizontal_inset = u16::from(area_width > 56).saturating_mul(2);
        let inner_width = usize::from(
            area_width
                .saturating_sub(horizontal_inset)
                .clamp(1, 96)
                .saturating_sub(2)
                .max(1),
        );
        let content_width = u16::try_from(
            inner_width
                .saturating_sub(usize::from(INTENT_ROLE_WIDTH))
                .max(1),
        )
        .unwrap_or(u16::MAX);
        let mut content_rows = parsed
            .lines
            .iter()
            .map(|(_, text)| intent_wrapped_rows(text, content_width))
            .fold(0_usize, usize::saturating_add);
        if let Some((phase, elapsed)) = parsed.busy.as_ref() {
            let text = format!("{}  {elapsed}", sentence_case(phase));
            content_rows = content_rows.saturating_add(intent_wrapped_rows(&text, content_width));
        }
        u16::try_from(content_rows.clamp(1, 8))
            .unwrap_or(8)
            .saturating_add(2)
    }

    fn render_intent_activity(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(notice) = self.output_notice else {
            return;
        };
        let parsed = intent_panel_parts(notice);
        if area.height < 3 {
            let summary = parsed
                .busy
                .as_ref()
                .map(|(phase, elapsed)| format!("CODEX  {phase}  {elapsed}"))
                .or_else(|| {
                    parsed
                        .lines
                        .last()
                        .map(|(_, text)| format!("CODEX  {text}"))
                })
                .unwrap_or_else(|| "CODEX".to_owned());
            let line = Line::from(Span::styled(summary, self.theme.accent(Mode::Natural)));
            frame.render_widget(Paragraph::new(line), area);
            return;
        }

        let horizontal_inset = u16::from(area.width > 56).saturating_mul(2);
        let card_width = area.width.saturating_sub(horizontal_inset).clamp(1, 96);
        let card_area = Rect::new(
            area.x.saturating_add(horizontal_inset),
            area.y,
            card_width,
            area.height,
        );
        let title = Line::from(vec![
            Span::styled(" CODEX ", self.theme.selected(Mode::Natural)),
            Span::styled(
                format!("  {} ", parsed.model),
                self.theme.context_secondary(),
            ),
            Span::styled("· ", self.theme.dim()),
            Span::styled(
                format!("{} ", parsed.effort.to_uppercase()),
                self.theme.context(),
            ),
        ]);
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(
                self.theme
                    .context_secondary()
                    .remove_modifier(Modifier::BOLD),
            )
            .title(title);
        if let Some((turn_total, session_total)) = parsed.token_usage {
            block = block.title(
                Line::from(Span::styled(
                    token_usage_title(turn_total, session_total),
                    self.theme.dim(),
                ))
                .right_aligned(),
            );
        }
        let inner = block.inner(card_area);
        frame.render_widget(block, card_area);
        let mut entries = parsed
            .lines
            .into_iter()
            .map(|(role, text)| IntentRenderEntry::Conversation { role, text })
            .collect::<Vec<_>>();
        if let Some((phase, elapsed)) = parsed.busy {
            entries.push(IntentRenderEntry::Busy {
                spinner: self.busy_glyph.unwrap_or('•'),
                phase,
                elapsed,
            });
        }
        self.render_intent_entries(frame, inner, entries);
    }

    fn render_intent_entries(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        entries: Vec<IntentRenderEntry>,
    ) {
        let role_width = INTENT_ROLE_WIDTH.min(area.width);
        let content_width = area.width.saturating_sub(role_width);
        if content_width == 0 || area.height == 0 {
            return;
        }
        let heights = entries
            .iter()
            .map(|entry| entry.rows(content_width))
            .collect::<Vec<_>>();
        let total_rows = heights.iter().copied().fold(0_usize, usize::saturating_add);
        let mut rows_to_skip = total_rows.saturating_sub(usize::from(area.height));
        let mut y = area.y;
        for (entry, entry_rows) in entries.into_iter().zip(heights) {
            if rows_to_skip >= entry_rows {
                rows_to_skip = rows_to_skip.saturating_sub(entry_rows);
                continue;
            }
            let scroll = rows_to_skip;
            rows_to_skip = 0;
            let visible_rows = entry_rows
                .saturating_sub(scroll)
                .min(usize::from(area.bottom().saturating_sub(y)));
            if visible_rows == 0 {
                break;
            }
            let height = u16::try_from(visible_rows).unwrap_or(u16::MAX);
            if scroll == 0 {
                frame.render_widget(
                    Paragraph::new(entry.role_line(self.theme)),
                    Rect::new(area.x, y, role_width, 1),
                );
            }
            let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
            let paragraph = Paragraph::new(entry.content_line(content_width, self.theme))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0));
            frame.render_widget(
                paragraph,
                Rect::new(area.x.saturating_add(role_width), y, content_width, height),
            );
            y = y.saturating_add(height);
            if y >= area.bottom() {
                break;
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
                if text.starts_with('│') && !transcript_line_is_selected(line_index, selection) {
                    return Some(table_transcript_line(text, self.theme));
                }
                let style = transcript_base_style(text, self.theme, self.mode);
                Some(transcript_line(
                    text,
                    line_index,
                    selection,
                    style,
                    self.theme.selected(self.mode),
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), area);
        if let Some(metrics) = scrollbar_metrics(transcript.line_count(), visible.clone()) {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some(if self.unicode { "│" } else { "|" }))
                .thumb_symbol(if self.unicode { "█" } else { "#" })
                .thumb_style(self.theme.accent(self.mode));
            let mut state = ScrollbarState::new(metrics.position_count)
                .position(metrics.position)
                .viewport_content_length(metrics.viewport_line_count);
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
                Span::styled(
                    left.get(..branch_start).unwrap_or_default().to_owned(),
                    self.theme.context(),
                ),
                Span::styled(
                    left.get(branch_start..).unwrap_or_default().to_owned(),
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

    #[allow(
        clippy::arithmetic_side_effects,
        reason = "line offsets accumulate disjoint substrings of one bounded editor buffer"
    )]
    fn input_render(&self, width: usize) -> InputRender {
        let mut lines = Vec::new();
        let mut offset = 0_usize;
        let cursor_line = self
            .editor
            .buffer()
            .get(..self.editor.cursor())
            .unwrap_or_default()
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        let mut cursor_column = 0_usize;
        let busy_indicator = self
            .busy_glyph
            .filter(|_| self.mode != Mode::Natural)
            .map(|glyph| format!("{glyph} "));
        for (line_index, part) in self.editor.buffer().split('\n').enumerate() {
            let indicator = if line_index == 0 {
                busy_indicator
                    .as_deref()
                    .unwrap_or_else(|| self.symbols.input_indicator(self.mode))
            } else {
                self.symbols.multiline_indicator()
            };
            let indicator_style = if busy_indicator.is_some() {
                self.theme.dim()
            } else {
                self.theme.accent(self.mode)
            };
            let mut spans = vec![Span::styled(indicator.to_owned(), indicator_style)];
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
                && let Some(suggestion) = self.editor.autosuggestion()
            {
                spans.push(Span::styled(
                    escape_terminal_line(suggestion),
                    self.theme.dim().add_modifier(Modifier::ITALIC),
                ));
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
                    Span::styled(
                        match self.symbols {
                            SurfaceSymbols::NerdFont => "\u{f002} ",
                            SurfaceSymbols::Unicode => "⌕ ",
                            SurfaceSymbols::Plain => "> ",
                        },
                        self.theme.dim(),
                    ),
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
                let glyph = item.kind.glyph(self.symbols);
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
                let mut docs = vec![
                    Line::styled(
                        escape_terminal_line(&item.display),
                        self.theme.accent(self.mode),
                    ),
                    Line::styled(
                        format!("source: {} · {}", item.source, item.trust),
                        self.theme.dim(),
                    ),
                    Line::raw(""),
                ];
                docs.extend(
                    item.detail
                        .lines()
                        .map(|line| Line::raw(escape_terminal_line(line))),
                );
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
struct ScrollbarMetrics {
    position_count: usize,
    position: usize,
    viewport_line_count: usize,
}

fn scrollbar_metrics(
    total_line_count: usize,
    visible: std::ops::Range<usize>,
) -> Option<ScrollbarMetrics> {
    let viewport_line_count = visible.end.saturating_sub(visible.start);
    if viewport_line_count == 0 || total_line_count <= viewport_line_count {
        return None;
    }
    // Ratatui adds the viewport length to `content_length - 1` when it computes
    // the thumb ratio. Supplying the number of possible viewport starts makes
    // that denominator equal the actual retained transcript length.
    Some(ScrollbarMetrics {
        position_count: total_line_count
            .saturating_sub(viewport_line_count)
            .saturating_add(1),
        position: visible.start,
        viewport_line_count,
    })
}

fn transcript_base_style(text: &str, theme: Theme, mode: Mode) -> Style {
    if text.starts_with('❯') {
        theme.accent(mode)
    } else if text.starts_with("── exit ") {
        theme.dim()
    } else if matches!(text.chars().next(), Some('╭' | '├' | '╰')) {
        theme.border()
    } else {
        Style::default()
    }
}

fn transcript_line_is_selected(
    line_index: usize,
    selection: Option<(
        super::transcript::TextPosition,
        super::transcript::TextPosition,
    )>,
) -> bool {
    selection.is_some_and(|(start, end)| {
        line_index >= start.line_index
            && line_index <= end.line_index
            && (start.line_index != end.line_index || start.byte_offset < end.byte_offset)
    })
}

fn table_transcript_line(text: &str, theme: Theme) -> Line<'static> {
    let content_style = if table_heading_line(text) {
        theme.accent(Mode::Command)
    } else {
        Style::default()
    };
    let mut spans = Vec::new();
    let mut content_start = 0_usize;
    for (index, character) in text.char_indices() {
        if character != '│' {
            continue;
        }
        if content_start < index {
            spans.push(Span::styled(
                escape_terminal_line(text.get(content_start..index).unwrap_or_default()),
                content_style,
            ));
        }
        spans.push(Span::styled("│", theme.border()));
        content_start = index.saturating_add(character.len_utf8());
    }
    if content_start < text.len() {
        spans.push(Span::styled(
            escape_terminal_line(text.get(content_start..).unwrap_or_default()),
            content_style,
        ));
    }
    Line::from(spans)
}

fn table_heading_line(text: &str) -> bool {
    text.strip_prefix('│')
        .and_then(|text| text.split_once('│'))
        .is_some_and(|(first_cell, _)| first_cell.trim() == "#")
}

fn transcript_line(
    text: &str,
    line_index: usize,
    selection: Option<(
        super::transcript::TextPosition,
        super::transcript::TextPosition,
    )>,
    base_style: Style,
    selected_style: Style,
) -> Line<'static> {
    let Some((start, end)) = selection
        .filter(|(start, end)| line_index >= start.line_index && line_index <= end.line_index)
    else {
        return Line::styled(escape_terminal_line(text), base_style);
    };
    let selected_start = if line_index == start.line_index {
        start.byte_offset
    } else {
        0
    };
    let selected_end = if line_index == end.line_index {
        end.byte_offset
    } else {
        text.len()
    };
    if selected_start >= selected_end {
        return Line::styled(escape_terminal_line(text), base_style);
    }
    Line::from(vec![
        Span::styled(
            escape_terminal_line(text.get(..selected_start).unwrap_or_default()),
            base_style,
        ),
        Span::styled(
            escape_terminal_line(text.get(selected_start..selected_end).unwrap_or_default()),
            selected_style,
        ),
        Span::styled(
            escape_terminal_line(text.get(selected_end..).unwrap_or_default()),
            base_style,
        ),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameLayout {
    transcript: Rect,
    context: Option<Rect>,
    input: Rect,
    intent_activity: Option<Rect>,
    diagnostic: Option<Rect>,
    information: Rect,
    status: Rect,
}

/// Prefer enough rows to read documentation, borrowing transcript rows without
/// moving the editor. Border/source rows alone are not a usable help panel.
fn information_area(layout: &FrameLayout, requested: bool, full: bool) -> Rect {
    // Borders, source metadata, and a wrapped usage example can consume eight
    // rows before the first explanatory paragraph becomes visible.
    const READABLE_ROWS_MIN: u16 = 12;
    if !requested || layout.information.height >= READABLE_ROWS_MIN {
        return layout.information;
    }
    let height = if full {
        layout.transcript.height
    } else {
        layout.transcript.height.min(READABLE_ROWS_MIN)
    };
    if height <= layout.information.height {
        return layout.information;
    }
    Rect::new(
        layout.transcript.x,
        layout.transcript.bottom().saturating_sub(height),
        layout.transcript.width,
        height,
    )
}

fn frame_layout(
    area: Rect,
    input_rows: u16,
    diagnostic_visible: bool,
    intent_activity_rows: u16,
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
            intent_activity: None,
            diagnostic: None,
            information: Rect::new(area.x, status_y, area.width, 0),
            status,
        };
    }

    let context_height = u16::from(rows_above_status >= 2);
    let available_after_context = rows_above_status.saturating_sub(context_height);
    // Ask only for the rows the content needs. Preserve one editor row on a
    // short terminal, then degrade to a one-line summary instead of reserving
    // a mostly empty fixed-height rectangle.
    let intent_activity_height =
        intent_activity_rows.min(available_after_context.saturating_sub(1));
    let available_after_activity = available_after_context.saturating_sub(intent_activity_height);
    let diagnostic_height = u16::from(diagnostic_visible && available_after_activity >= 2);
    let input_height = input_rows.min(available_after_activity.saturating_sub(diagnostic_height));
    let current_height = context_height
        .saturating_add(input_height)
        .saturating_add(intent_activity_height)
        .saturating_add(diagnostic_height);
    let transcript_height = u16::try_from(transcript_line_count)
        .unwrap_or(u16::MAX)
        .min(rows_above_status.saturating_sub(current_height));
    let transcript = Rect::new(area.x, area.y, area.width, transcript_height);
    let context_y = transcript.bottom();
    let context = (context_height == 1).then_some(Rect::new(area.x, context_y, area.width, 1));
    let input_y = context_y.saturating_add(context_height);
    let input = Rect::new(area.x, input_y, area.width, input_height);
    let intent_activity_y = input.bottom();
    let intent_activity = (intent_activity_height > 0).then_some(Rect::new(
        area.x,
        intent_activity_y,
        area.width,
        intent_activity_height,
    ));
    let diagnostic_y = intent_activity_y.saturating_add(intent_activity_height);
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
        intent_activity,
        diagnostic,
        information,
        status,
    }
}

struct IntentPanelParts {
    model: String,
    effort: String,
    token_usage: Option<(u64, u64)>,
    lines: Vec<(IntentPanelRole, String)>,
    busy: Option<(String, String)>,
}

#[derive(Clone, Copy)]
enum IntentPanelRole {
    User,
    Assistant,
    Command,
}

const INTENT_ROLE_WIDTH: u16 = 9;

enum IntentRenderEntry {
    Conversation {
        role: IntentPanelRole,
        text: String,
    },
    Busy {
        spinner: char,
        phase: String,
        elapsed: String,
    },
}

impl IntentRenderEntry {
    fn rows(&self, content_width: u16) -> usize {
        match self {
            Self::Conversation { text, .. } => intent_wrapped_rows(text, content_width),
            Self::Busy { phase, elapsed, .. } => intent_wrapped_rows(
                &format!("{}  {elapsed}", sentence_case(phase)),
                content_width,
            ),
        }
    }

    fn role_line(&self, theme: Theme) -> Line<'static> {
        match self {
            Self::Conversation { role, .. } => intent_role_line(*role, theme),
            Self::Busy { spinner, .. } => Line::from(Span::styled(
                format!("  {spinner}"),
                theme.context_secondary(),
            )),
        }
    }

    fn content_line(&self, content_width: u16, theme: Theme) -> Line<'static> {
        match self {
            Self::Conversation { role, text } => intent_content_line(*role, text, theme),
            Self::Busy { phase, elapsed, .. } => {
                intent_busy_line(phase, elapsed, usize::from(content_width), theme)
            }
        }
    }
}

fn intent_panel_parts(notice: &str) -> IntentPanelParts {
    let mut model = "Codex".to_owned();
    let mut effort = "model pending".to_owned();
    let mut token_usage = None;
    let mut lines = Vec::new();
    let mut busy = None;
    for line in notice.lines() {
        let mut fields = line.splitn(3, '\t');
        match fields.next().unwrap_or_default() {
            "MODEL" => {
                model = fields.next().unwrap_or("Codex").to_owned();
                effort = fields.next().unwrap_or("default").to_owned();
            }
            "TOKENS" => {
                let turn_total = fields.next().and_then(|value| value.parse::<u64>().ok());
                let session_total = fields.next().and_then(|value| value.parse::<u64>().ok());
                token_usage = turn_total
                    .zip(session_total)
                    .filter(|(turn_total, session_total)| session_total >= turn_total);
            }
            "USER" => lines.push((
                IntentPanelRole::User,
                fields.next().unwrap_or_default().to_owned(),
            )),
            "ASSISTANT" => lines.push((
                IntentPanelRole::Assistant,
                fields.next().unwrap_or_default().to_owned(),
            )),
            "COMMAND" => lines.push((
                IntentPanelRole::Command,
                fields.next().unwrap_or_default().to_owned(),
            )),
            "BUSY" => {
                busy = Some((
                    fields.next().unwrap_or("working").to_owned(),
                    fields.next().unwrap_or_default().to_owned(),
                ));
            }
            _ => {}
        }
    }
    IntentPanelParts {
        model,
        effort,
        token_usage,
        lines,
        busy,
    }
}

fn token_usage_title(turn_total: u64, session_total: u64) -> String {
    if turn_total == session_total {
        format!(" {} tokens ", compact_token_count(turn_total))
    } else {
        format!(
            " {} turn · {} session ",
            compact_token_count(turn_total),
            compact_token_count(session_total)
        )
    }
}

fn compact_token_count(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        return compact_scaled_count(tokens, 1_000, "k");
    }
    compact_scaled_count(tokens, 1_000_000, "m")
}

fn compact_scaled_count(value: u64, scale: u64, suffix: &str) -> String {
    let tenths = value
        .saturating_mul(10)
        .checked_div(scale)
        .unwrap_or_default();
    let whole = tenths.checked_div(10).unwrap_or_default();
    if tenths.is_multiple_of(10) {
        format!("{whole}{suffix}")
    } else {
        let decimal = tenths.checked_rem(10).unwrap_or_default();
        format!("{whole}.{decimal:01}{suffix}")
    }
}

fn intent_role_line(role: IntentPanelRole, theme: Theme) -> Line<'static> {
    match role {
        IntentPanelRole::User => Line::from(Span::styled("  you", theme.context_secondary())),
        IntentPanelRole::Assistant => {
            Line::from(Span::styled("  codex", theme.accent(Mode::Natural)))
        }
        IntentPanelRole::Command => Line::from(Span::styled("  ›", theme.context_secondary())),
    }
}

fn intent_content_line(role: IntentPanelRole, text: &str, theme: Theme) -> Line<'static> {
    if !matches!(role, IntentPanelRole::Command) {
        return Line::from(text.to_owned());
    }
    let spans = quirl_syntax::highlight(text, Mode::Command)
        .into_iter()
        .map(|span| {
            let text = text.get(span.range).unwrap_or_default();
            Span::styled(escape_terminal_controls(text), theme.highlight(span.kind))
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

fn intent_busy_line(
    phase: &str,
    elapsed: &str,
    content_width: usize,
    theme: Theme,
) -> Line<'static> {
    let phase = sentence_case(phase);
    let occupied =
        UnicodeWidthStr::width(phase.as_str()).saturating_add(UnicodeWidthStr::width(elapsed));
    let gap = content_width.saturating_sub(occupied).max(2);
    Line::from(vec![
        Span::styled(phase, theme.accent(Mode::Natural)),
        Span::raw(" ".repeat(gap)),
        Span::styled(elapsed.to_owned(), theme.dim()),
    ])
}

fn intent_wrapped_rows(text: &str, content_width: u16) -> usize {
    let content_width = usize::from(content_width);
    if content_width == 0 {
        return 0;
    }
    let mut rows = 0_usize;
    let mut line_width = 0_usize;
    let mut word_width = 0_usize;
    let mut whitespace_width = 0_usize;
    let mut whitespace = std::collections::VecDeque::new();
    let mut previous_was_non_whitespace = false;
    for grapheme in text.graphemes(true) {
        let is_whitespace = grapheme.chars().all(char::is_whitespace);
        let symbol_width = UnicodeWidthStr::width(grapheme);
        if symbol_width > content_width {
            continue;
        }
        let word_found = previous_was_non_whitespace && is_whitespace;
        let untrimmed_overflow = line_width == 0
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > content_width;
        if word_found || untrimmed_overflow {
            line_width = line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width);
            whitespace.clear();
            whitespace_width = 0;
            word_width = 0;
        }

        let line_full = line_width >= content_width;
        let pending_word_overflow = symbol_width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= content_width;
        if line_full || pending_word_overflow {
            rows = rows.saturating_add(1);
            let mut remaining_width = content_width.saturating_sub(line_width);
            line_width = 0;
            while whitespace
                .front()
                .is_some_and(|width| *width <= remaining_width)
            {
                let Some(width) = whitespace.pop_front() else {
                    break;
                };
                whitespace_width = whitespace_width.saturating_sub(width);
                remaining_width = remaining_width.saturating_sub(width);
            }
            if is_whitespace && whitespace.is_empty() {
                previous_was_non_whitespace = false;
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            whitespace.push_back(symbol_width);
        } else {
            word_width = word_width.saturating_add(symbol_width);
        }
        previous_was_non_whitespace = !is_whitespace;
    }
    if line_width > 0 || whitespace_width > 0 || word_width > 0 || rows == 0 {
        rows = rows.saturating_add(1);
    }
    rows
}

fn sentence_case(value: &str) -> String {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    let mut rendered = String::with_capacity(value.len());
    rendered.extend(first.to_uppercase());
    rendered.extend(characters);
    rendered
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
    for (index, grapheme) in value
        .get(..cursor)
        .unwrap_or_default()
        .grapheme_indices(true)
        .rev()
    {
        let grapheme_width = escaped_width(grapheme);
        if used_left.saturating_add(grapheme_width) > desired_left_width {
            break;
        }
        used_left = used_left.saturating_add(grapheme_width);
        start = index;
    }

    let mut end = start;
    let mut used = 0_usize;
    for (offset, grapheme) in value
        .get(start..)
        .unwrap_or_default()
        .grapheme_indices(true)
    {
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

fn diagnostic_glyph(severity: DiagnosticSeverity, symbols: SurfaceSymbols) -> &'static str {
    match (severity, symbols) {
        (DiagnosticSeverity::Error, SurfaceSymbols::NerdFont) => "  \u{f057} ",
        (DiagnosticSeverity::Warning, SurfaceSymbols::NerdFont) => "  \u{f071} ",
        (DiagnosticSeverity::Hint, SurfaceSymbols::NerdFont) => "  \u{f05a} ",
        (DiagnosticSeverity::Error, SurfaceSymbols::Unicode) => "  ✘ ",
        (DiagnosticSeverity::Warning, SurfaceSymbols::Unicode) => "  ▲ ",
        (DiagnosticSeverity::Hint, SurfaceSymbols::Unicode) => "  ℹ ",
        (DiagnosticSeverity::Error, SurfaceSymbols::Plain) => "  E ",
        (DiagnosticSeverity::Warning, SurfaceSymbols::Plain) => "  W ",
        (DiagnosticSeverity::Hint, SurfaceSymbols::Plain) => "  H ",
    }
}

fn cursor_visual_position(value: &str, cursor: usize) -> (usize, usize) {
    let prefix = value.get(..cursor.min(value.len())).unwrap_or_default();
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
        CompletionState,
        completion::{CompletionItem, CompletionKind},
        editor::EditAction,
    };
    use quirl_catalog::Catalog;
    use ratatui::{Terminal, backend::TestBackend, style::Color};

    #[test]
    fn transcript_tables_use_theme_roles_without_embedded_ansi() {
        let theme = Theme::new(true);

        assert_eq!(
            transcript_base_style("╭───┬──────╮", theme, Mode::Command),
            theme.border()
        );
        assert_eq!(
            transcript_base_style("│ # │ name │", theme, Mode::Command),
            Style::default()
        );
        assert_eq!(
            transcript_base_style("│ 0 │ file │", theme, Mode::Command),
            Style::default()
        );

        let heading = table_transcript_line("│ # │ name │", theme);
        assert_eq!(heading.spans[0].content, "│");
        assert_eq!(heading.spans[0].style, theme.border());
        assert_eq!(heading.spans[1].content, " # ");
        assert_eq!(heading.spans[1].style, theme.accent(Mode::Command));
        assert_eq!(heading.spans[2].content, "│");
        assert_eq!(heading.spans[2].style, theme.border());

        let row = table_transcript_line("│ 0 │ file │", theme);
        assert_eq!(row.spans[0].style, theme.border());
        assert_eq!(row.spans[1].style, Style::default());
        assert_eq!(row.spans[2].style, theme.border());
    }

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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
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

    fn cell_sequence_column(
        terminal: &Terminal<TestBackend>,
        y: u16,
        expected: &str,
    ) -> Option<u16> {
        let buffer = terminal.backend().buffer();
        let expected = expected
            .chars()
            .map(|value| value.to_string())
            .collect::<Vec<_>>();
        (0..buffer.area.width).find(|start| {
            expected.iter().enumerate().all(|(offset, symbol)| {
                let offset = u16::try_from(offset).unwrap_or(u16::MAX);
                buffer
                    .cell((start.saturating_add(offset), y))
                    .is_some_and(|cell| cell.symbol() == symbol)
            })
        })
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
                    environment: None,
                    runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
                }
                .render(frame);
            })
            .unwrap();
    }

    #[test]
    fn rest_frame_has_context_input_and_persistent_status_rows() {
        let terminal = rendered_model(78, 3, "git status", None, |_| {});
        assert!(row(&terminal, 0).contains("~/P/q  on main"));
        assert!(row(&terminal, 1).contains("git status"));
        assert!(row(&terminal, 2).contains("NORMAL"));
        assert_eq!(
            terminal.backend().buffer().cell((2, 1)).unwrap().fg,
            Color::Rgb(158, 206, 106)
        );
    }

    #[test]
    fn codex_activity_renders_as_an_accented_card_below_the_intent() {
        let mut editor = EditorState::new("emacs", Vec::new());
        editor.insert_paste("list all files");
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut terminal = Terminal::new(TestBackend::new(96, 7)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~/project",
                    context_right: "",
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Natural,
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: Some(
                        "MODEL\tGPT-5.6-Luna\thigh\nUSER\tlist all files\nBUSY\tchecking the command semantics\t2.4s",
                    ),
                    busy_glyph: Some('🕑'),
                }
                .render(frame);
            })
            .unwrap();

        assert!(row(&terminal, 1).contains("list all files"));
        assert!(row(&terminal, 2).contains("CODEX"));
        assert!(row(&terminal, 2).contains("GPT-5.6-Luna"));
        let busy_row = row(&terminal, 4);
        assert!(busy_row.contains("🕑"));
        assert!(busy_row.contains("Checking the command semantics"));
        assert!(busy_row.contains("2.4s"));
        let user_column = cell_sequence_column(&terminal, 3, "list all files");
        let busy_column = cell_sequence_column(&terminal, 4, "Checking the command semantics");
        assert_eq!(user_column, busy_column);
        assert!(busy_row.find("2.4s").is_some_and(|index| {
            UnicodeWidthStr::width(busy_row.get(..index).unwrap_or_default()) >= 88
        }));
        assert!(
            terminal
                .backend()
                .buffer()
                .cell((5, 4))
                .unwrap()
                .modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(
            terminal.backend().buffer().cell((2, 4)).unwrap().fg,
            Color::Rgb(187, 154, 247)
        );
        assert!(!row(&terminal, 6).contains("choosing the best command"));
    }

    #[test]
    fn codex_wrapped_messages_keep_every_continuation_in_the_content_column() {
        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
        let message = "A".repeat(60);
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~/project",
                    context_right: "",
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Natural,
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: Some(&format!(
                        "MODEL\tGPT-5.6-Luna\thigh\nASSISTANT\t{message}"
                    )),
                    busy_glyph: None,
                }
                .render(frame);
            })
            .unwrap();

        let content_columns = (3..=5)
            .filter_map(|y| cell_sequence_column(&terminal, y, "A"))
            .collect::<Vec<_>>();
        assert_eq!(content_columns, vec![10, 10, 10]);
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
                }
                .render(frame);
            })
            .unwrap();
        assert!(row(&terminal, 1).starts_with("> git"));
        assert!(!row(&terminal, 1).contains("status"));
        assert_eq!(
            terminal.backend().buffer().cell((0, 1)).unwrap().fg,
            Color::Rgb(158, 206, 106)
        );
    }

    #[test]
    fn codex_reply_renders_conversation_and_a_normal_command_proposal() {
        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~/project",
                    context_right: "",
                    editor: &editor,
                    completion: &completion,
                    mode: Mode::Natural,
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: Some(
                        "MODEL\tGPT-5.6-Luna\thigh\nTOKENS\t1842\t6724\nUSER\tshow hidden files\nASSISTANT\tThis includes dotfiles.\nCOMMAND\tls -a",
                    ),
                    busy_glyph: None,
                }
                .render(frame);
            })
            .unwrap();

        let rendered = (0..10).map(|y| row(&terminal, y)).collect::<String>();
        assert!(rendered.contains("GPT-5.6-Luna"));
        assert!(rendered.contains("HIGH"));
        assert!(rendered.contains("1.8k turn · 6.7k session"));
        assert!(rendered.contains("you"));
        assert!(rendered.contains("This includes dotfiles."));
        assert!(rendered.contains("›      ls -a"));
        assert!(row(&terminal, 9).contains("Enter/Tab use"));
    }

    #[test]
    fn nerd_font_profile_applies_icons_across_rich_surface_chrome() {
        assert_eq!(
            SurfaceSymbols::NerdFont.input_indicator(Mode::Command),
            "❯ "
        );
        assert_eq!(SurfaceSymbols::NerdFont.multiline_indicator(), "∙ ");
        assert_eq!(
            SurfaceSymbols::NerdFont.status_mode_icon(Mode::Data),
            "\u{f1c0}"
        );
        assert_eq!(
            CompletionKind::Command.glyph(SurfaceSymbols::NerdFont),
            "\u{f120}"
        );
        assert_eq!(
            diagnostic_glyph(DiagnosticSeverity::Warning, SurfaceSymbols::NerdFont),
            "  \u{f071} "
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
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
        assert!(row(&terminal, 1).contains("git st"));
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
        assert!(row(&terminal, 1).contains("git st"));
        assert!(row(&terminal, 2).contains("completions"));

        completion.dismiss();
        draw_runtime_model(&mut terminal, &editor, &completion, &runtime);
        assert_eq!(terminal.get_cursor_position().unwrap(), cursor_at_rest);
        assert!(row(&terminal, 1).contains("git st"));
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
            environment: None,
            runtime: &runtime,
            transcript: None,
            transcript_truncated: false,
            output_focus: false,
            output_notice: None,
            busy_glyph: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(78, 4)).unwrap();
        terminal.draw(|frame| model.render(frame)).unwrap();
        let rendered = (0..4).map(|y| row(&terminal, y)).collect::<String>();
        assert!(!rendered.contains("demo"));
        assert!(row(&terminal, 3).contains("NORMAL"));
    }

    #[test]
    fn tiny_layout_prioritizes_status_then_editor_without_out_of_bounds_regions() {
        let one_row = frame_layout(Rect::new(0, 0, 1, 1), 1, true, 0, 0, true);
        assert_eq!(one_row.status, Rect::new(0, 0, 1, 1));
        assert_eq!(one_row.input.height, 0);
        assert_eq!(one_row.information.height, 0);
        assert!(one_row.context.is_none());
        assert!(one_row.diagnostic.is_none());

        let two_rows = frame_layout(Rect::new(0, 0, 1, 2), u16::MAX, true, 0, 0, true);
        assert_eq!(two_rows.input, Rect::new(0, 0, 1, 1));
        assert_eq!(two_rows.status, Rect::new(0, 1, 1, 1));
        assert!(two_rows.context.is_none());
        assert!(two_rows.diagnostic.is_none());

        let compact_activity = frame_layout(Rect::new(0, 0, 80, 4), 1, false, 7, 0, true);
        assert_eq!(compact_activity.input.height, 1);
        assert_eq!(compact_activity.intent_activity.unwrap().height, 1);
        let full_activity = frame_layout(Rect::new(0, 0, 80, 7), 1, false, 7, 0, true);
        assert_eq!(full_activity.input.height, 1);
        assert_eq!(full_activity.intent_activity.unwrap().height, 4);

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
            environment: None,
            runtime: &runtime,
            transcript: None,
            transcript_truncated: false,
            output_focus: false,
            output_notice: None,
            busy_glyph: None,
        };
        let mut terminal = Terminal::new(TestBackend::new(78, 12)).unwrap();
        terminal.draw(|frame| model.render(frame)).unwrap();
        assert!(row(&terminal, 0).contains("~/project"));
        assert_eq!(row(&terminal, 1).trim(), "❯");
        assert!(row(&terminal, 11).contains("NORMAL"));
        assert!(row(&terminal, 3).trim().is_empty());
        assert!(row(&terminal, 10).trim().is_empty());

        terminal.backend_mut().resize(52, 6);
        terminal.draw(|frame| model.render(frame)).unwrap();
        assert!(row(&terminal, 0).contains("~/project"));
        assert_eq!(row(&terminal, 1).trim(), "❯");
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
                        environment: None,
                        runtime: &runtime,
                        transcript: Some(transcript),
                        transcript_truncated: false,
                        output_focus: false,
                        output_notice: None,
                        busy_glyph: None,
                    }
                    .render(frame);
                })
                .unwrap();
        };

        draw(&mut terminal, &transcript);
        assert!(row(&terminal, 0).contains("❯ pwd"));
        assert!(row(&terminal, 2).contains("~/workspace"));
        assert_eq!(row(&terminal, 3).trim(), "❯");
        assert!(row(&terminal, 7).contains("NORMAL"));

        for index in 0..10 {
            transcript.append_line(&format!("output-{index}"));
        }
        draw(&mut terminal, &transcript);
        assert!(row(&terminal, 4).contains("output-9"));
        assert!(row(&terminal, 5).contains("~/workspace"));
        assert_eq!(row(&terminal, 6).trim(), "❯");
        assert!(row(&terminal, 7).contains("NORMAL"));

        assert!(transcript.page_up(7));
        draw(&mut terminal, &transcript);
        assert!(!row(&terminal, 5).contains("~/workspace"));
        assert!(!row(&terminal, 6).contains('❯'));
        assert!(row(&terminal, 7).contains("SCROLL"));
    }

    #[test]
    fn transcript_scrollbar_uses_exact_viewport_geometry() {
        assert_eq!(
            scrollbar_metrics(100, 40..60),
            Some(ScrollbarMetrics {
                position_count: 81,
                position: 40,
                viewport_line_count: 20,
            })
        );
        assert_eq!(
            scrollbar_metrics(100, 80..100),
            Some(ScrollbarMetrics {
                position_count: 81,
                position: 80,
                viewport_line_count: 20,
            })
        );
        assert_eq!(scrollbar_metrics(20, 0..20), None);
    }

    #[test]
    fn transcript_selection_highlights_only_the_selected_byte_range() {
        let mut transcript = Transcript::new(crate::surface::transcript::TranscriptLimits {
            line_count_max: 8,
            retained_bytes_max: 1_024,
        });
        transcript.append_line("zero selected tail");
        transcript.begin_selection(crate::surface::transcript::TextPosition {
            line_index: 0,
            byte_offset: 5,
        });
        transcript.update_selection(crate::surface::transcript::TextPosition {
            line_index: 0,
            byte_offset: 13,
        });
        let editor = EditorState::new("emacs", Vec::new());
        let completion = CompletionState::new(Catalog::builtin(), None);
        let runtime = RuntimeSurfaceState::new();
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        terminal
            .draw(|frame| {
                FrameModel {
                    context_left: "~",
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
                    environment: None,
                    runtime: &runtime,
                    transcript: Some(&transcript),
                    transcript_truncated: false,
                    output_focus: true,
                    output_notice: None,
                    busy_glyph: None,
                }
                .render(frame);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            !buffer
                .cell((4, 0))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            buffer
                .cell((5, 0))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            buffer
                .cell((12, 0))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buffer
                .cell((13, 0))
                .unwrap()
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn help_uses_readable_transcript_space_when_only_header_rows_remain() {
        let layout = frame_layout(Rect::new(0, 0, 120, 40), 1, false, 0, 33, true);
        assert_eq!(layout.information.height, 4);
        let area = information_area(&layout, true, false);
        assert_eq!(area.height, 12);
        assert!(area.bottom() <= layout.input.y);
        assert_eq!(information_area(&layout, false, false), layout.information);

        let short = frame_layout(Rect::new(0, 0, 60, 8), 1, false, 0, 2, true);
        assert_eq!(information_area(&short, true, false), short.information);
        let roomy = frame_layout(Rect::new(0, 0, 120, 40), 1, false, 0, 0, true);
        assert_eq!(information_area(&roomy, true, false), roomy.information);
    }

    #[test]
    fn help_keeps_explanatory_text_visible_at_intermediate_pane_heights() {
        for rows in 4_u16..12 {
            let layout = frame_layout(
                Rect::new(0, 0, 120, 40),
                1,
                false,
                0,
                usize::from(37 - rows),
                true,
            );
            assert_eq!(layout.information.height, rows);
            let area = information_area(&layout, true, false);
            assert_eq!(area.height, 12);
            assert!(area.bottom() <= layout.input.y);
        }
        let layout = frame_layout(Rect::new(0, 0, 120, 40), 1, false, 0, 25, true);
        assert_eq!(layout.information.height, 12);
        assert_eq!(information_area(&layout, true, false), layout.information);
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
                        environment: None,
                        runtime: &runtime,
                        transcript: Some(&transcript),
                        transcript_truncated: false,
                        output_focus: false,
                        output_notice: None,
                        busy_glyph: None,
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
        assert!(row(&terminal, 10).contains("git st"));
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
                };
                model.render(frame);
            })
            .unwrap();

        assert!(row(&terminal, 0).contains("~/project"));
        assert_eq!(row(&terminal, 1).trim(), "❯");
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
                    environment: None,
                    runtime: &runtime,
                    transcript: None,
                    transcript_truncated: false,
                    output_focus: false,
                    output_notice: None,
                    busy_glyph: None,
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
