//! Cached, width-aware rendering. Completed messages are highlighted once;
//! redraws clone only visible lines, and streaming updates invalidate one entry.
use super::{
    app::{App, Entry},
    commands::HELP,
    kitty::{self, KittyVariant},
    picker::{Picker, PickerKind},
};
use crate::{config::Theme, tools};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, ThemeSet},
    parsing::SyntaxSet,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Default)]
pub(super) struct Renderer {
    cache: Vec<CachedEntry>,
    width: usize,
    theme: Option<Theme>,
    generation: u64,
    #[cfg(test)]
    rebuilds: usize,
    /// Per-session launch variant offset. Real TUI startup picks a fresh
    /// `Uuid::new_v4`-derived offset so different sessions land on different
    /// artwork, but `Default` keeps `0` for deterministic tests. The renderer
    /// stores the offset explicitly so `set_launch`, `current_variant`, and
    /// `variant_dirty` all agree on which variant is the starting one, and the
    /// first `variant_dirty` call after startup never reports a spurious flip.
    variant_offset: usize,
    /// Last variant index seen by `variant_dirty`. Drives the 40 ms tick dirty
    /// flag when the 900 s boundary flips it. Initialized to the launch
    /// offset so the first check at zero elapsed is a no-op. The default
    /// `0` matches the default offset `0`, which keeps the blob sequence
    /// deterministic for tests.
    last_variant_index: usize,
    /// Whether the last `advance` saw a busy run, so busy-end resets the frame.
    was_busy: bool,
    /// Animation frame index while processing. Reset to zero on busy-end.
    processing_frame: usize,
    /// Time the current processing frame was shown; the next frame appears
    /// once the current frame's per-variant delay has elapsed.
    processing_tick: Option<Instant>,
}
struct CachedEntry {
    revision: u64,
    lines: Vec<Line<'static>>,
}

impl Renderer {
    fn history(&mut self, app: &App, width: usize, height: usize) -> Vec<Line<'static>> {
        if self.width != width
            || self.theme.as_ref() != Some(&app.theme)
            || self.generation != app.history_generation
        {
            self.cache.clear();
            self.width = width;
            self.theme = Some(app.theme.clone());
            self.generation = app.history_generation;
        }
        self.cache.truncate(app.entries.len());
        for (index, entry) in app.entries.iter().enumerate() {
            if self
                .cache
                .get(index)
                .is_some_and(|c| c.revision == entry.revision)
            {
                continue;
            }
            let cached = CachedEntry {
                revision: entry.revision,
                lines: render_entry(entry, width, &app.theme),
            };
            if index == self.cache.len() {
                self.cache.push(cached);
            } else {
                self.cache[index] = cached;
            }
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        let total: usize = self.cache.iter().map(|c| c.lines.len()).sum();
        let max_scroll = total.saturating_sub(height);
        let mut skip = max_scroll.saturating_sub(app.scroll.min(max_scroll));
        let mut visible = Vec::with_capacity(height);
        for cached in &self.cache {
            if skip >= cached.lines.len() {
                skip -= cached.lines.len();
                continue;
            }
            let count = (height - visible.len()).min(cached.lines.len() - skip);
            visible.extend_from_slice(&cached.lines[skip..skip + count]);
            skip = 0;
            if visible.len() == height {
                break;
            }
        }
        visible
    }

    pub fn draw(&mut self, frame: &mut Frame, app: &App) {
        let area = frame.area();
        let theme = &app.theme;
        let base = Style::default()
            .bg(color(&theme.background))
            .fg(color(&theme.foreground));
        frame.render_widget(Block::default().style(base), area);
        // Portable terminals expose cells, not pixel-sized CSS padding.
        let area = area.inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let busy = app.busy.is_some();
        let now = Instant::now();
        self.advance(busy, now);
        let variant = self.current_variant(now);
        let kitty_rows = kitty_rows(variant, busy, self.processing_frame, theme);
        let kitty_width = kitty_rows.iter().map(Line::width).max().unwrap_or(0) as u16;
        let input_lines = wrap_lines(
            vec![Line::raw(app.input.text.clone())],
            area.width.saturating_sub(4) as usize,
        );
        // Keep three editable text rows visible before growing for wrapped input.
        // Terminal layout is cell-based; the border supplies the practical padding.
        let input_height = (input_lines.len() as u16 + 2)
            .max(5)
            .clamp(5, 12)
            .min(area.height.saturating_sub(4));
        let regions = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(2),
        ])
        .split(area);
        draw_header(frame, app, regions[0], kitty_width);
        let history = self.history(
            app,
            regions[1].width.saturating_sub(2) as usize,
            regions[1].height as usize,
        );
        frame.render_widget(
            Paragraph::new(history)
                .block(border_block(theme).borders(Borders::LEFT | Borders::RIGHT)),
            regions[1],
        );
        self.draw_kitty(frame, regions[1], &kitty_rows);
        draw_input(frame, app, regions[2]);
        let footer =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(regions[3]);
        frame.render_widget(
            Paragraph::new(app.status.clone()).style(Style::default().fg(color(&theme.muted))),
            footer[0],
        );
        let buttons = if app.busy.is_some() {
            vec![
                button(" Ctrl+C Cancel ", theme),
                Span::raw("  /tools and /mcp work during a run"),
            ]
        } else {
            vec![
                button(" Enter Send ", theme),
                Span::raw("  "),
                button(" /help Commands ", theme),
                Span::raw("  Tab mode | Alt+Enter newline"),
            ]
        };
        // The workspace path anchors the bottom-right corner. Long paths keep
        // their tail so the current directory stays visible on narrow terminals.
        let path_column =
            (UnicodeWidthStr::width(app.workspace.as_str()) as u16).min(regions[3].width / 2);
        let columns = Layout::horizontal([Constraint::Min(1), Constraint::Length(path_column)])
            .split(footer[1]);
        frame.render_widget(Paragraph::new(Line::from(buttons)), columns[0]);
        frame.render_widget(
            Paragraph::new(tail_text(&app.workspace, path_column as usize))
                .alignment(Alignment::Right)
                .style(Style::default().fg(color(&theme.muted))),
            columns[1],
        );
        if let Some(picker) = &app.picker {
            draw_picker(
                frame,
                picker,
                theme,
                area,
                app.approval.is_none() && !app.help,
            );
        }
        if app.workflow_complete {
            draw_overlay(
                frame,
                "Workflow complete",
                "n: start a new workflow\nr: repeat this workflow\nq: exit workflow mode",
                None,
                app,
                area,
            );
        }
        if app.help {
            draw_overlay(frame, "Help", HELP, None, app, area);
        }
        if let Some(approval) = &app.approval {
            let choices = if approval.workflow {
                " y Continue | r Retry | s Skip | q Abort "
            } else {
                " y Approve | n Reject | q Abort "
            };
            draw_overlay(
                frame,
                &approval.title,
                &approval.detail,
                Some(choices),
                app,
                area,
            );
        }
    }

    /// Called once by `mod.rs` at TUI startup with a fresh, UUID-derived
    /// offset, before `set_launch`. `set_variant_offset` latches the initial
    /// rotation index so the first `variant_dirty` call cannot fire a spurious
    /// redraw. `Default` keeps the offset at `0`, which keeps the blob
    /// sequence deterministic for tests.
    pub(super) fn set_variant_offset(&mut self, offset: usize) {
        self.variant_offset = offset % kitty::VARIANT_COUNT;
        self.last_variant_index = self.variant_offset;
    }

    /// Retained as a startup hook for the event loop; rotation is disabled,
    /// so no launch timestamp is needed.
    pub(super) fn set_launch(&mut self, _launch: Instant) {}

    fn current_variant(&self, _now: Instant) -> KittyVariant {
        // Keep the launch-selected kitty static; rotation is intentionally
        // disabled so the header and artwork remain stable for the session.
        kitty::variant_at(Duration::ZERO, self.variant_offset)
    }

    /// Kitty rotation is disabled. Retain this hook so the event loop can keep
    /// its existing dirty-check path without scheduling periodic redraws.
    pub(super) fn variant_dirty(&mut self, _now: Instant) -> bool {
        false
    }

    /// Advance the processing animation state machine. `now` is injected so
    /// tests can step time without sleeping. A new run starts at frame zero,
    /// the frame advances when the current frame's delay has elapsed, and the
    /// state resets when the run ends so the next run starts at the rest pose.
    fn advance(&mut self, busy: bool, now: Instant) {
        if busy {
            if !self.was_busy {
                self.processing_frame = 0;
                self.processing_tick = Some(now);
            } else if self.processing_tick.is_some_and(|tick| {
                now.saturating_duration_since(tick)
                    >= Duration::from_millis(kitty::frame_delay_ms(
                        self.current_variant(now),
                        self.processing_frame,
                    ))
            }) {
                self.processing_frame += 1;
                self.processing_tick = Some(now);
            }
        } else {
            self.processing_frame = 0;
            self.processing_tick = None;
        }
        self.was_busy = busy;
    }

    fn draw_kitty(&mut self, frame: &mut Frame, chat_area: Rect, rows: &[Line<'static>]) {
        let width = rows.iter().map(Line::width).max().unwrap_or(0) as u16;
        let first_content_row = rows.iter().position(|line| line.width() > 0).unwrap_or(0);
        let height = rows.len().saturating_sub(first_content_row) as u16;
        if width == 0 {
            return;
        }
        let visible_width = width.min(chat_area.width).min(frame.area().width);
        let visible_height = height.min(chat_area.height).min(frame.area().height);
        if visible_width == 0 || visible_height == 0 {
            return;
        }
        // Lift the kitty one row so its artwork overlaps the top-right
        // corner of the chat history instead of hanging below it. The shared
        // canvas keeps its leading padding row, which places the first
        // visible glyph at the history's top edge while the model metadata
        // remains right-aligned above it.
        let content = frame.area().inner(Margin {
            horizontal: 1,
            vertical: 1,
        });
        let kitty_area = Rect::new(
            content.right().saturating_sub(visible_width),
            content.y,
            visible_width,
            visible_height,
        );
        let buffer = frame.buffer_mut();
        for (row, line) in rows
            .iter()
            .skip(first_content_row)
            .take(kitty_area.height as usize)
            .enumerate()
        {
            let mut column = 0;
            for span in &line.spans {
                let style = line.style.patch(span.style);
                for character in span.content.chars() {
                    let cells = character.width().unwrap_or(0) as u16;
                    if !character.is_whitespace() && column < kitty_area.width {
                        let x = kitty_area.x + column;
                        let y = kitty_area.y + row as u16;
                        // Actual glyphs, not sampled pixels: the artwork reads
                        // as text and theme colors separate body, eyes, and Z's.
                        let mut glyph = [0u8; 4];
                        buffer[(x, y)]
                            .set_symbol(character.encode_utf8(&mut glyph))
                            .set_style(style);
                    }
                    column = column.saturating_add(cells);
                }
            }
        }
    }
}

/// Rows for the current kitty state: the rest pose while idle, the variant's
/// processing cycle while a run is active. Pure so tests can pin every input;
/// animation is impossible while idle no matter what the frame index says.
fn kitty_rows(
    variant: KittyVariant,
    busy: bool,
    processing_frame: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if busy {
        kitty::render_processing(variant, processing_frame, theme)
    } else {
        kitty::render_idle(variant, theme)
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect, kitty_width: u16) {
    let theme = &app.theme;
    let rows = Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).split(area);
    // Reserve the currently displayed kitty's width on the right so header
    // metadata ends exactly where the artwork begins, regardless of variant.
    let columns = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(kitty_width.min(rows[0].width)),
    ])
    .split(rows[0]);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                " diet_",
                Style::default()
                    .fg(color(&theme.border))
                    .add_modifier(Modifier::ITALIC | Modifier::BOLD),
            ),
            Line::styled(
                " soda",
                Style::default()
                    .fg(Color::Rgb(255, 79, 163))
                    .add_modifier(Modifier::ITALIC | Modifier::BOLD),
            ),
        ]),
        columns[0],
    );
    let model_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(columns[1]);
    frame.render_widget(
        Paragraph::new(app.model_label.clone())
            .alignment(Alignment::Right)
            .style(
                Style::default()
                    .fg(color(&theme.accent))
                    .add_modifier(Modifier::BOLD),
            ),
        model_rows[0],
    );
    let spend_color = if app.spend.unpriced_requests == 0 {
        &theme.success
    } else {
        &theme.warning
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{} | context {}/{}",
            app.spend.display(),
            app.context_tokens,
            app.context_limit
        ))
        .alignment(Alignment::Right)
        .style(Style::default().fg(color(spend_color))),
        model_rows[1],
    );
    let agent = app.selection.agent.as_deref().unwrap_or("default");
    let detail = format!(" agent: {agent} | effort: {}", app.effort_label);
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(color(&theme.muted)))
            .block(border_block(theme).borders(Borders::BOTTOM)),
        rows[1],
    );
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let block = border_block(theme)
        .title(" Input ")
        .border_style(Style::default().fg(color(&theme.accent)));
    let inner = block.inner(area);
    let mut lines = wrap_lines(
        vec![Line::raw(app.input.text.clone())],
        inner.width as usize,
    );
    let prefix = wrap_lines(
        vec![Line::raw(app.input.text[..app.input.cursor].to_owned())],
        inner.width as usize,
    );
    let mut row = prefix.len().saturating_sub(1);
    let mut column = prefix.last().map(Line::width).unwrap_or(0);
    if column >= inner.width as usize && inner.width > 0 {
        row += 1;
        column = 0;
        lines.push(Line::raw(""));
    }
    let offset = row.saturating_sub(inner.height.saturating_sub(1) as usize);
    if app.input.text.starts_with('/') || app.input.text == ":q" {
        for line in &mut lines {
            line.style = Style::default().fg(color(&theme.accent));
        }
    }
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(offset).collect::<Vec<_>>()),
        inner,
    );
    if app.approval.is_none()
        && !app.help
        && app.picker.is_none()
        && inner.width > 0
        && inner.height > 0
    {
        frame.set_cursor_position((
            inner.x + (column as u16).min(inner.width - 1),
            inner.y + ((row - offset) as u16).min(inner.height - 1),
        ));
    }
}

fn draw_picker(frame: &mut Frame, picker: &Picker, theme: &Theme, area: Rect, focused: bool) {
    let width = area.width.min(100);
    let height = area.height.min(24);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let block = border_block(theme)
        .title(format!(
            " {} ({}/{}) ",
            picker.title(),
            picker.matches.len(),
            picker.choices.len()
        ))
        .style(
            Style::default()
                .bg(color(&theme.background))
                .fg(color(&theme.foreground)),
        )
        .border_style(Style::default().fg(color(&theme.accent)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    let search = border_block(theme).title(" Search ");
    let search_area = search.inner(rows[0]);
    frame.render_widget(search, rows[0]);
    if search_area.width > 0 && search_area.height > 0 {
        let prefix = &picker.query.text[..picker.query.cursor];
        let cursor_column = UnicodeWidthStr::width(prefix);
        let desired_skip = cursor_column.saturating_sub(search_area.width as usize - 1);
        let mut skipped = 0;
        let visible: String = picker
            .query
            .text
            .chars()
            .skip_while(|c| {
                if skipped < desired_skip {
                    skipped += c.width().unwrap_or(0);
                    true
                } else {
                    false
                }
            })
            .collect();
        frame.render_widget(Paragraph::new(visible), search_area);
        if focused {
            frame.set_cursor_position((
                search_area.x + cursor_column.saturating_sub(skipped) as u16,
                search_area.y,
            ));
        }
    }
    if picker.matches.is_empty() {
        frame.render_widget(
            Paragraph::new("No matching entries").style(Style::default().fg(color(&theme.muted))),
            rows[1],
        );
    } else {
        // Center selection in the viewport; only render visible labels.
        let height = rows[1].height as usize;
        let start = picker
            .selected
            .saturating_sub(height / 2)
            .min(picker.matches.len().saturating_sub(height));
        let lines = picker
            .matches
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(position, &index)| {
                let choice = &picker.choices[index];
                let mut spans = vec![];
                if let Some(enabled) = choice.enabled {
                    spans.push(Span::styled(
                        if enabled { "[on ] " } else { "[off] " },
                        Style::default().fg(color(if enabled {
                            &theme.success
                        } else {
                            &theme.muted
                        })),
                    ));
                }
                spans.push(if position == picker.selected {
                    button(&choice.label, theme)
                } else {
                    Span::raw(choice.label.as_str())
                });
                Line::from(spans)
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(lines), rows[1]);
    }
    frame.render_widget(
        Paragraph::new(picker.hint()).style(Style::default().fg(color(&theme.muted))),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            button(
                if matches!(picker.kind, PickerKind::Mcps) {
                    " Enter Toggle "
                } else {
                    " Enter Select "
                },
                theme,
            ),
            Span::raw(" Up/Down browse | PgUp/PgDn | Esc close"),
        ])),
        rows[3],
    );
}

fn render_entry(entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let role_color = match entry.role.as_str() {
        "user" => &theme.user,
        "assistant" => &theme.assistant,
        "tool" => &theme.tool,
        "error" => &theme.error,
        _ => &theme.muted,
    };
    let label = if entry.context == "main" {
        entry.role.clone()
    } else {
        format!("{} | {}", entry.role, entry.context)
    };
    let (top_left, top_right, vertical, bottom_left, bottom_right) = if theme.ascii {
        ('+', '+', '|', '+', '+')
    } else {
        ('┌', '┐', '│', '└', '┘')
    };
    let header_style = Style::default()
        .fg(color(role_color))
        .add_modifier(Modifier::BOLD);
    let mut lines = vec![Line::styled(
        format!("{top_left} {label} {top_right}"),
        header_style,
    )];
    let content = if entry.role == "tool" && entry.text.len() <= 100_000 {
        Some(tool_result_text(&entry.text))
    } else {
        None
    };
    let content_lines = markdown(
        content.as_deref().unwrap_or(&entry.text),
        theme,
        color(role_color),
    );
    let prefix = Span::styled(
        format!("{vertical} "),
        Style::default().fg(color(&theme.border)),
    );
    for line in content_lines {
        for wrapped in wrap_lines_at_words(vec![line], width.saturating_sub(2)) {
            let mut spans = vec![prefix.clone()];
            spans.extend(wrapped.spans);
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::styled(
        format!("{bottom_left} {bottom_right}"),
        Style::default().fg(color(&theme.border)),
    ));
    lines
}

/// Tool results are shown as their payload when one exists: file/web content as
/// text and process output as stdout/stderr. Error results keep the original
/// call, and any string that itself contains JSON is parsed and pretty-printed
/// so escaped JSON never reaches the transcript.
fn tool_result_text(text: &str) -> String {
    let Ok(raw) = serde_json::from_str::<serde_json::Value>(text) else {
        return readable_text(text);
    };
    let value = tools::normalize_json(raw);
    if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
        let mut output = format!("[error] {}", readable_text(error));
        if let Some(call) = value.get("call").and_then(|v| v.as_str()) {
            output.push('\n');
            output.push_str(call);
        }
        return output;
    }
    if let Some(content) = value.get("content") {
        return match content {
            serde_json::Value::String(content) => readable_text(content),
            content => fenced_json(content),
        };
    }
    let stdout_value = value.get("stdout");
    let stderr_value = value.get("stderr");
    let stderr = stderr_value.and_then(|v| v.as_str()).unwrap_or("");
    if value.get("exit_code").is_some() || stdout_value.is_some() || stderr_value.is_some() {
        let mut output = match stdout_value {
            Some(serde_json::Value::String(stdout)) => readable_text(stdout.trim_end()),
            Some(value) => fenced_json(value),
            None => String::new(),
        };
        if !stderr.trim().is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("[stderr]\n");
            output.push_str(stderr.trim_end());
        }
        if let Some(code) = value.get("exit_code").and_then(|v| v.as_i64()) {
            if code != 0 {
                output.push_str(&format!("\n[exit {code}]"));
            }
        }
        return output;
    }
    match value {
        serde_json::Value::String(inner) => readable_text(&inner),
        value => fenced_json(&value),
    }
}

fn fenced_json(value: &serde_json::Value) -> String {
    format!(
        "```json\n{}\n```",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )
}

/// A string holding a JSON object or array is rendered as formatted JSON;
/// everything else is returned unchanged.
fn readable_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return fenced_json(&tools::normalize_json(value));
        }
    }
    text.to_owned()
}

/// Keep the tail of a long path so the bottom-right corner always shows the
/// deepest directory; the leading ellipsis marks the truncation.
fn tail_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    let mut characters = vec![];
    let mut used = 0;
    for character in text.chars().rev() {
        let cells = character.width().unwrap_or(0);
        if used + cells > width.saturating_sub(1) {
            break;
        }
        used += cells;
        characters.push(character);
    }
    characters.reverse();
    format!("…{}", characters.into_iter().collect::<String>())
}

fn draw_overlay(
    frame: &mut Frame,
    title: &str,
    text: &str,
    choices: Option<&str>,
    app: &App,
    area: Rect,
) {
    let theme = &app.theme;
    let rect = Rect {
        x: area.x + area.width / 12,
        y: area.y + area.height / 12,
        width: area.width * 5 / 6,
        height: area.height * 5 / 6,
    };
    frame.render_widget(Clear, rect);
    let block = border_block(theme)
        .title(title)
        .title_bottom(" PgUp/PgDn scroll ")
        .style(Style::default().bg(color(&theme.background)))
        .border_style(Style::default().fg(color(if choices.is_some() {
            &theme.warning
        } else {
            &theme.accent
        })));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let regions = Layout::vertical([
        Constraint::Length(u16::from(choices.is_some())),
        Constraint::Min(0),
    ])
    .split(inner);
    if let Some(choices) = choices {
        frame.render_widget(
            Paragraph::new(Line::from(button(choices, theme))),
            regions[0],
        );
    }
    let lines = wrap_lines(
        markdown(text, theme, color(&theme.foreground)),
        regions[1].width as usize,
    );
    let offset = app
        .overlay_scroll
        .min(lines.len().saturating_sub(regions[1].height as usize));
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(offset).collect::<Vec<_>>()),
        regions[1],
    );
}

fn button<'a>(label: &'a str, theme: &Theme) -> Span<'a> {
    Span::styled(
        label,
        Style::default()
            .fg(color(&theme.cta_foreground))
            .bg(color(&theme.cta_background))
            .add_modifier(Modifier::BOLD),
    )
}
fn border_block(theme: &Theme) -> Block<'static> {
    let symbols = if theme.ascii {
        border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        }
    } else {
        border::ROUNDED
    };
    Block::default()
        .borders(Borders::ALL)
        .border_set(symbols)
        .border_style(Style::default().fg(color(&theme.border)))
}
/// Hex color strings to ratatui RGB. Shared with the kitty module so theme
/// changes recolor the artwork identically to every other surface.
pub(super) fn color(hex: &str) -> Color {
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0xffffff);
    Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// Fenced code uses bundled grammars. Unknown languages and pathological long
/// lines fall back to plain text; no network, font, or grammar install is needed.
fn markdown(text: &str, theme: &Theme, foreground: Color) -> Vec<Line<'static>> {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    let mut highlighter = None;
    let mut in_code = false;
    let mut lines = vec![];
    for line in text.lines() {
        if let Some(language) = line.trim_start().strip_prefix("```") {
            in_code = !in_code;
            highlighter = None;
            if in_code && theme.syntax_highlighting {
                let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
                let themes = THEMES.get_or_init(crate::config::themes::syntax_themes);
                let syntax = syntaxes
                    .find_syntax_by_token(language.trim())
                    .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
                if let Some(palette) = themes.themes.get(&theme.syntax_theme) {
                    highlighter = Some(HighlightLines::new(syntax, palette));
                }
            }
            lines.push(Line::styled(
                line.to_owned(),
                Style::default().fg(color(&theme.muted)),
            ));
            continue;
        }
        if let Some(highlighter) = highlighter.as_mut().filter(|_| line.len() <= 4096) {
            if let Ok(tokens) =
                highlighter.highlight_line(&format!("{line}\n"), SYNTAXES.get().unwrap())
            {
                let spans = tokens
                    .into_iter()
                    .map(|(style, text)| {
                        let mut rendered = Style::default().fg(Color::Rgb(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        ));
                        if style.font_style.contains(FontStyle::BOLD) {
                            rendered = rendered.add_modifier(Modifier::BOLD);
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            rendered = rendered.add_modifier(Modifier::ITALIC);
                        }
                        Span::styled(text.trim_end_matches('\n').to_owned(), rendered)
                    })
                    .collect::<Vec<_>>();
                lines.push(Line::from(spans));
                continue;
            }
        }
        let style = Style::default().fg(foreground);
        if !in_code && line.starts_with('#') {
            lines.push(Line::styled(
                line.to_owned(),
                style.fg(color(&theme.accent)).add_modifier(Modifier::BOLD),
            ));
        } else if !in_code && line.contains('`') {
            let spans = line
                .split_inclusive('`')
                .enumerate()
                .map(|(i, part)| {
                    Span::styled(
                        part.to_owned(),
                        if i % 2 == 1 {
                            style.fg(color(&theme.accent))
                        } else {
                            style
                        },
                    )
                })
                .collect::<Vec<_>>();
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::styled(line.to_owned(), style));
        }
    }
    lines
}

/// Wrap by terminal cell width, retaining each token's style and UTF-8 boundary.
fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut output = vec![];
    for line in lines {
        let mut spans = vec![];
        let mut used = 0;
        for span in line.spans {
            let mut text = String::new();
            let style = line.style.patch(span.style);
            for character in span.content.chars() {
                if character == '\n' {
                    spans.push(Span::styled(std::mem::take(&mut text), style));
                    output.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                    continue;
                }
                if character.is_control() && character != '\t' {
                    continue;
                }
                let character = if character == '\t' { ' ' } else { character };
                let cells = character.width().unwrap_or(0);
                if used + cells > width && used > 0 {
                    spans.push(Span::styled(std::mem::take(&mut text), style));
                    output.push(Line::from(std::mem::take(&mut spans)));
                    used = 0;
                }
                text.push(character);
                used += cells;
            }
            if !text.is_empty() {
                spans.push(Span::styled(text, style));
            }
        }
        output.push(Line::from(spans));
    }
    output
}

/// Wrap chat entries at whitespace where possible, keeping long unbroken values intact until
/// they exceed the terminal width.
fn wrap_lines_at_words(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut output = vec![];
    for line in lines {
        let mut characters = vec![];
        for span in line.spans {
            let style = line.style.patch(span.style);
            for character in span.content.chars() {
                if character == '\n' {
                    push_wrapped_line(&mut output, &mut characters, width);
                } else if !character.is_control() || character == '\t' {
                    characters.push((if character == '\t' { ' ' } else { character }, style));
                }
            }
        }
        push_wrapped_line(&mut output, &mut characters, width);
    }
    output
}

fn push_wrapped_line(
    output: &mut Vec<Line<'static>>,
    characters: &mut Vec<(char, Style)>,
    width: usize,
) {
    if characters.is_empty() {
        output.push(Line::default());
        return;
    }
    let mut remaining = std::mem::take(characters);
    while !remaining.is_empty() {
        let mut used = 0;
        let mut fit = 0;
        let mut last_break = None;
        for (index, (character, _)) in remaining.iter().enumerate() {
            let cells = character.width().unwrap_or(0);
            if used + cells > width && fit > 0 {
                break;
            }
            used += cells;
            fit = index + 1;
            if character.is_whitespace() {
                last_break = Some(fit);
            }
        }
        let original_len = remaining.len();
        let break_at = if fit == original_len || remaining[fit].0.is_whitespace() {
            fit
        } else {
            last_break.filter(|index| *index > 0).unwrap_or(fit.max(1))
        };
        let mut line = remaining.drain(..break_at).collect::<Vec<_>>();
        while line
            .last()
            .is_some_and(|(character, _)| character.is_whitespace())
        {
            line.pop();
        }
        if break_at < original_len {
            while remaining
                .first()
                .is_some_and(|(character, _)| character.is_whitespace())
            {
                remaining.remove(0);
            }
        }
        output.push(characters_to_line(line));
    }
}

fn characters_to_line(characters: Vec<(char, Style)>) -> Line<'static> {
    let mut spans = vec![];
    let mut text = String::new();
    let mut style = None;
    for (character, character_style) in characters {
        if style != Some(character_style) && !text.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut text), style.unwrap()));
        }
        style = Some(character_style);
        text.push(character);
    }
    if let Some(style) = style {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        engine::Selection,
        model::{Message, UiEvent},
        tui::app::Busy,
    };
    use ratatui::{backend::TestBackend, Terminal};

    fn screen(renderer: &mut Renderer, app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| renderer.draw(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn highlights_code_and_reuses_unchanged_history() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.message(
            "main".into(),
            Message::new(
                "assistant",
                "```rust\nfn main() { let text = \"hello\"; }\n```",
            ),
        );
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 90, 24);
        assert!(output.contains("fn main()"));
        assert!(output.contains("Enter Send"));
        assert!(output.contains("diet_"));
        assert!(output.contains("soda"));
        assert!(output.contains("┌ assistant ┐"));
        assert!(output.contains("│ "));
        let colors: std::collections::HashSet<_> = renderer.cache[0]
            .lines
            .iter()
            .flat_map(|l| &l.spans)
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(colors.len() >= 3);
        let rebuilds = renderer.rebuilds;
        screen(&mut renderer, &app, 90, 24);
        assert_eq!(renderer.rebuilds, rebuilds);
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "next".into(),
        });
        screen(&mut renderer, &app, 90, 24);
        assert_eq!(renderer.rebuilds, rebuilds + 1);
    }

    #[test]
    fn chat_wraps_between_words() {
        let lines = wrap_lines_at_words(vec![Line::raw("one two three")], 7);
        let text = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        assert_eq!(text, ["one two", "three"]);
    }

    #[test]
    fn wrapped_chat_lines_keep_the_message_gutter() {
        let entry = Entry {
            role: "assistant".into(),
            context: "main".into(),
            text: "one two three".into(),
            revision: 0,
        };
        let lines = render_entry(&entry, 10, &Theme::default());
        assert!(lines[1].to_string().starts_with("│ "));
        assert!(lines[2].to_string().starts_with("│ "));
    }

    #[test]
    fn header_shows_current_context_and_workspace_anchors_bottom_right() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.context_tokens = 1234;
        app.context_limit = 8192;
        app.workspace = "/Users/example/project".into();
        let output = screen(&mut Renderer::default(), &app, 80, 24);
        assert!(output.contains("context 1234/8192"));
        let line = output
            .lines()
            .find(|line| line.contains("/Users/example/project"))
            .unwrap();
        assert!(line.trim_end().ends_with("/Users/example/project"));
    }

    #[test]
    fn long_workspace_paths_keep_their_tail() {
        assert_eq!(tail_text("short", 10), "short");
        assert_eq!(tail_text("/very/long/path/to/project", 9), "…/project");
        assert_eq!(tail_text("/x", 0), "");
    }

    #[test]
    fn tool_results_show_payloads_instead_of_json_blobs() {
        assert_eq!(
            tool_result_text(r#"{"content":"hello","truncated":false}"#),
            "hello"
        );
        let process = tool_result_text(r#"{"stdout":"out\n","stderr":"err\n","exit_code":2}"#);
        assert!(process.contains("out"));
        assert!(process.contains("[stderr]"));
        assert!(process.contains("[exit 2]"));
        assert!(tool_result_text(r#"{"custom":1}"#).contains("```json"));
    }

    #[test]
    fn tool_errors_keep_the_call_and_escaped_json_is_parsed() {
        let error = tool_result_text(
            r#"{"error":"No such file or directory","tool":"shell","call":"Run `nope`"}"#,
        );
        assert!(error.starts_with("[error] No such file or directory"));
        assert!(error.contains("Run `nope`"));
        // Double-encoded payloads are parsed instead of printed with escapes.
        assert!(tool_result_text(r#""{\"a\":1}""#).contains("```json"));
        assert!(tool_result_text(r#"{"content":"{\"a\":1}"}"#).contains("```json"));
        assert!(
            tool_result_text(r#"{"stdout":"{\"a\":1}","stderr":"","exit_code":0}"#)
                .contains("```json")
        );
    }

    #[test]
    fn idle_kitty_shows_blob_glyphs_at_the_top_right() {
        let app = App::new(&Config::default(), Selection::default());
        let output = screen(&mut Renderer::default(), &app, 80, 24);
        // Actual asset glyphs render, anchored at the right edge of the chat.
        assert!(output.contains("▄████████"));
        assert!(output.contains("▀▀▀▀▀▀▀"));
        let art_rows: Vec<&str> = output.lines().filter(|l| l.contains('█')).collect();
        assert!(!art_rows.is_empty());
        assert!(art_rows
            .iter()
            .all(|line| line.trim_end().rfind('█').unwrap() >= 60));
    }

    #[test]
    fn busy_runs_show_the_cancel_button_and_keep_the_kitty_visible() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let task = runtime
            .handle()
            .spawn(async { std::future::pending::<Result<String, anyhow::Error>>().await });
        drop(runtime);
        let mut app = App::new(&Config::default(), Selection::default());
        app.busy = Some(Busy {
            task,
            cancel: tokio_util::sync::CancellationToken::new(),
        });
        let output = screen(&mut Renderer::default(), &app, 80, 24);
        assert!(output.contains("Ctrl+C Cancel"));
        assert!(output.contains("▄████████"));
    }

    #[test]
    fn kitty_animation_runs_only_while_busy() {
        let theme = Theme::default();
        // Idle ignores the frame index entirely and always shows the rest pose.
        assert_eq!(
            kitty_rows(KittyVariant::Blob, false, 3, &theme),
            kitty_rows(KittyVariant::Blob, false, 0, &theme)
        );
        // Processing frame 1 moves the tail segment up one row, overlaying
        // the lower body cells. The tail's original row empties.
        let moved = kitty_rows(KittyVariant::Blob, true, 1, &theme);
        assert_ne!(kitty_rows(KittyVariant::Blob, true, 0, &theme), moved);
        let text: Vec<String> = moved.iter().map(Line::to_string).collect();
        assert_eq!(text[2], "██  ▄████████");
        assert_eq!(text[3], "  ▀▀███▄▄██▄▄█");
        assert_eq!(text[4], "      ▀▀▀▀▀▀▀");
        assert_eq!(text[5], "");
    }

    #[test]
    fn processing_frames_advance_on_variant_delays_and_reset_after_the_run() {
        let mut renderer = Renderer::default();
        let now = Instant::now();
        // Idle: no frame state at all.
        renderer.advance(false, now);
        assert_eq!(renderer.processing_frame, 0);
        assert!(renderer.processing_tick.is_none());
        // A run starts at frame zero; blob frame 0 shows for 700 ms.
        renderer.advance(true, now);
        assert_eq!(renderer.processing_frame, 0);
        renderer.advance(true, now + Duration::from_millis(699));
        assert_eq!(renderer.processing_frame, 0);
        renderer.advance(true, now + Duration::from_millis(700));
        assert_eq!(renderer.processing_frame, 1);
        // Blob frame 1 shows for 500 ms.
        renderer.advance(true, now + Duration::from_millis(1199));
        assert_eq!(renderer.processing_frame, 1);
        renderer.advance(true, now + Duration::from_millis(1200));
        assert_eq!(renderer.processing_frame, 2);
        // The run ends: state resets so the next run starts from the rest pose.
        renderer.advance(false, now + Duration::from_millis(1201));
        assert_eq!(renderer.processing_frame, 0);
        assert!(renderer.processing_tick.is_none());
    }

    #[test]
    fn idle_rotation_flags_a_redraw_only_at_900s_boundaries() {
        // Offset 0: the renderer stays on Blob, Cbear, Fly Girl, Blob.
        let mut renderer = Renderer::default();
        let launch = Instant::now();
        renderer.set_launch(launch);
        assert!(!renderer.variant_dirty(launch + Duration::from_secs(899)));
        assert!(!renderer.variant_dirty(launch + Duration::from_secs(900)));
        assert!(!renderer.variant_dirty(launch + Duration::from_secs(1799)));
        assert!(!renderer.variant_dirty(launch + Duration::from_secs(1800)));
        // Exactly one dirty transition per boundary across three cycles.
        let launch2 = Instant::now() + Duration::from_secs(60);
        let mut renderer = Renderer::default();
        renderer.set_launch(launch2);
        let mut flips = 0;
        for second in 0..(6 * kitty::ROTATION_SECONDS) {
            if renderer.variant_dirty(launch2 + Duration::from_secs(second)) {
                flips += 1;
            }
        }
        assert_eq!(flips, 0);
    }

    #[test]
    fn idle_rotation_uses_the_launch_offset_and_latches_no_spurious_event() {
        // A fresh renderer's default offset is zero, and the initial index
        // is already latched, so the very first variant_dirty call cannot
        // fire a spurious redraw.
        let mut unset = Renderer::default();
        assert!(!unset.variant_dirty(Instant::now()));
        assert!(!unset.variant_dirty(Instant::now()));
        // Setting a non-zero offset before launch shifts the initial variant
        // to the offset (Cbear here) without ever reporting a dirty event at
        // zero elapsed.
        let mut offset_one = Renderer::default();
        offset_one.set_variant_offset(1);
        assert!(!offset_one.variant_dirty(Instant::now()));
        let launch = Instant::now();
        offset_one.set_launch(launch);
        // Exactly one dirty transition per 900 s boundary, starting from
        // the offset variant.
        assert!(!offset_one.variant_dirty(launch + Duration::from_secs(899)));
        assert!(!offset_one.variant_dirty(launch + Duration::from_secs(900)));
        assert!(!offset_one.variant_dirty(launch + Duration::from_secs(1799)));
        assert!(!offset_one.variant_dirty(launch + Duration::from_secs(1800)));
        assert!(!offset_one.variant_dirty(launch + Duration::from_secs(2699)));
        assert!(!offset_one.variant_dirty(launch + Duration::from_secs(2700)));
        // Offset 2 starts on Fly Girl; the sequence still rotates by one
        // every 900 s.
        let mut offset_two = Renderer::default();
        offset_two.set_variant_offset(2);
        let launch = Instant::now();
        offset_two.set_launch(launch);
        assert!(!offset_two.variant_dirty(launch + Duration::from_secs(899)));
        assert!(!offset_two.variant_dirty(launch + Duration::from_secs(900)));
        assert!(!offset_two.variant_dirty(launch + Duration::from_secs(1799)));
        assert!(!offset_two.variant_dirty(launch + Duration::from_secs(1800)));
        assert!(!offset_two.variant_dirty(launch + Duration::from_secs(2699)));
        assert!(!offset_two.variant_dirty(launch + Duration::from_secs(2700)));
    }

    #[test]
    fn streaming_output_snaps_only_when_the_stream_starts() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.scroll = 8;
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "first".into(),
        });
        assert_eq!(app.scroll, 0);

        app.scroll = 8;
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: " second".into(),
        });
        assert_eq!(app.scroll, 8);
    }
    #[test]
    fn small_terminals_and_plain_fonts_work() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.theme.ascii = true;
        app.input.insert("a漢é\nsecond line");
        for (width, height) in [(1, 1), (8, 3), (30, 8)] {
            screen(&mut Renderer::default(), &app, width, height);
        }
        assert!(screen(&mut Renderer::default(), &app, 90, 24).contains("+"));
    }

    #[test]
    fn model_picker_scrolls_selection_and_keeps_search_cursor_inside_small_viewports() {
        let mut config = Config::default();
        for i in 0..40 {
            let mut model = config.model.clone();
            model.model = format!("model-{i:02}");
            config.models.insert(format!("alias-{i:02}"), model);
        }
        let mut app = App::new(&config, Selection::default());
        app.picker = Some(Picker::models(&config, &config.model, None));
        app.picker
            .as_mut()
            .unwrap()
            .key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::End,
                crossterm::event::KeyModifiers::CONTROL,
            ));
        let selected = app
            .picker
            .as_ref()
            .unwrap()
            .current()
            .unwrap()
            .label
            .clone();
        let output = screen(&mut Renderer::default(), &app, 90, 24);
        assert!(output.contains("Search"));
        assert!(output.contains(&selected));
        assert!(output.lines().next().unwrap().trim().is_empty());
        assert!(output.lines().last().unwrap().trim().is_empty());
        assert!(output
            .lines()
            .all(|line| line.starts_with(' ') && line.ends_with(' ')));
        app.paste(&"漢é".repeat(40));
        for (width, height) in [(1, 1), (8, 3), (30, 12), (90, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| Renderer::default().draw(frame, &app))
                .unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert!(cursor.x < width && cursor.y < height);
        }
    }

    #[test]
    fn settings_pickers_render_states_and_theme_changes_invalidate_cached_syntax() {
        let config = Config { mcp_servers: serde_json::from_value(serde_json::json!({
            "browser":{"uuid":"browser-id","transport":"stdio","command":"never-started","enabled":false},
            "search":{"uuid":"search-id","transport":"stdio","command":"never-started","enabled":true}
        })).unwrap(), ..Config::default() };
        let mut app = App::new(&config, Selection::default());
        app.picker = Some(Picker::mcps(&config, &crate::tools::Switches::default()));
        let output = screen(&mut Renderer::default(), &app, 100, 24);
        assert!(output.contains("[off] browser"));
        assert!(output.contains("[on ] search"));
        app.picker = Some(Picker::themes(&app.theme, &config.theme));
        for (width, height) in [(1, 1), (8, 3), (30, 8), (100, 24)] {
            screen(&mut Renderer::default(), &app, width, height);
        }
        app.message(
            "main".into(),
            Message::new(
                "assistant",
                "```rust\nfn main() { let n = 42; println!(\"hello\"); }\n```",
            ),
        );
        let mut renderer = Renderer::default();
        screen(&mut renderer, &app, 100, 24);
        let before = renderer.rebuilds;
        for (name, _) in crate::config::themes::PRESETS {
            app.theme = crate::config::themes::preset(name).unwrap();
            screen(&mut renderer, &app, 100, 24);
            assert_eq!(renderer.theme.as_ref(), Some(&app.theme));
            if *name == "mama_j" {
                for rgb in renderer.cache[0]
                    .lines
                    .iter()
                    .flat_map(|l| &l.spans)
                    .filter_map(|s| s.style.fg)
                {
                    if let Color::Rgb(r, g, b) = rgb {
                        assert_eq!((r, g), (g, b));
                    }
                }
            }
        }
        assert_eq!(renderer.rebuilds, before + 6);
        let stable = renderer.rebuilds;
        screen(&mut renderer, &app, 100, 24);
        assert_eq!(renderer.rebuilds, stable);
    }
}
