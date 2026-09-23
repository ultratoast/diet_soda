//! Cached, width-aware rendering. Completed messages are highlighted once;
//! redraws clone only visible lines, and streaming updates invalidate one entry.
use super::{
    app::{ActivitySummary, App, Entry, LayoutSnapshot, TimelineItem},
    commands::HELP,
    kitty::{self, KittyVariant},
    picker::{Picker, PickerKind},
};
use crate::{
    config::Theme,
    model::{ActivityKind, ActivityStatus},
    text::{is_unsafe_terminal_char, sanitize_terminal_text},
    tools,
};
use ratatui::{
    layout::{Alignment, Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};
use std::{
    borrow::Cow,
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
    /// Hit-ready mapping aligned one-to-one with the chat viewport rows.
    /// `Some(ActivitySummary { id })` marks a one-line activity summary
    /// row (the only toggle target); every entry/body row is `None`.
    /// Keyed by viewport *line* index, so live mouse dispatch can resolve
    /// a click's terminal row against the same lines the renderer drew
    /// without re-walking the timeline.
    hit_map: Vec<Option<ActivitySummary>>,
    /// Last chat-history `Rect` the renderer drew into. Storing it on the
    /// renderer (rather than `App`) keeps geometry out of the model so
    /// `/clear`, `/reload`, and resume do not have to reset it on every
    /// entry mutation. `draw` overwrites the field every frame, and mouse
    /// dispatch resolves clicks against it (`activity_at`).
    last_history_rect: Option<ratatui::layout::Rect>,
    /// The kitty's reserved column (x/width) for the last frame, taken from the
    /// header's own horizontal Layout split. This is the single source of truth
    /// for where the artwork lives, so the session divider can stop exactly at
    /// its left edge. `None` when a narrow terminal suppresses the kitty;
    /// `draw` assigns it every frame, so a resize that suppresses the kitty
    /// clears it.
    last_kitty_reservation: Option<Rect>,
    /// Terminal rows the kitty actually painted, as `(y, x_start, x_end)`
    /// spans with an exclusive `x_end`. This is the visible artwork, not
    /// the transparent full canvas: rows and columns with no glyph are
    /// omitted so mouse dispatch can exclude painted cells from activity
    /// hit tests. `draw_kitty` overwrites the vector every frame.
    last_kitty_paint: Vec<(u16, u16, u16)>,
}
struct CachedEntry {
    revision: u64,
    lines: Vec<Line<'static>>,
}

impl Renderer {
    /// Two-pass windowed history assembly. Pass A walks the timeline once
    /// to total the visible logical line count without formatting a single
    /// summary or cloning a cached line. The scroll window is then derived.
    /// Pass B walks the timeline again and materializes only the rows that
    /// fall inside the viewport: one summary `Line` per visible activity
    /// row, and only the overlapping `Line`s cloned from each cached entry.
    /// It stops as soon as the window's last line has been emitted, so
    /// off-screen entries are never formatted or cloned. The one
    /// `LayoutSnapshot` built here is shared by both passes.
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
        // Rebuild only the entry whose revision changed. Activity Start/End,
        // expansion toggles, and status updates never bump an entry's
        // revision, so the cache stays valid on those events.
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
                lines: if entry.streaming {
                    render_streaming_entry(entry, width, &app.theme)
                } else {
                    render_entry(entry, width, &app.theme)
                },
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
        // One snapshot for both passes. Visibility rules are identical in
        // Pass A and Pass B; only Pass B formats or clones anything.
        let snapshot = app.layout_snapshot();
        // Pass A: total visible logical lines, cheapest possible walk.
        let mut total = 0usize;
        for item in &app.timeline {
            match item {
                TimelineItem::Activity(id) => {
                    if app.ancestor_visible_for(&snapshot, id) {
                        total += 1;
                    }
                }
                TimelineItem::Entry(index) => {
                    if entry_visible(app, &snapshot, *index) {
                        if let Some(cached) = self.cache.get(*index) {
                            total += cached.lines.len();
                        }
                    }
                }
            }
        }
        let max_scroll = total.saturating_sub(height);
        let scroll = app.scroll.min(max_scroll);
        let start = max_scroll.saturating_sub(scroll);
        let end = (start + height).min(total);
        let window = end.saturating_sub(start);
        let mut visible = Vec::with_capacity(window);
        let mut hit_map = Vec::with_capacity(window);
        // Pass B: materialize the window only. `cursor` is the first
        // logical line of the current source item. Stop once the window's
        // last line has been emitted so trailing items are never touched.
        let mut cursor = 0usize;
        for item in &app.timeline {
            if cursor >= end {
                break;
            }
            match item {
                TimelineItem::Activity(id) => {
                    if !app.ancestor_visible_for(&snapshot, id) {
                        continue;
                    }
                    let row = cursor;
                    cursor += 1;
                    if row >= start && row < end {
                        visible.push(activity_summary_line_snapshot(
                            app, &snapshot, id, width, &app.theme,
                        ));
                        hit_map.push(Some(ActivitySummary { id: id.clone() }));
                    }
                }
                TimelineItem::Entry(index) => {
                    if !entry_visible(app, &snapshot, *index) {
                        continue;
                    }
                    let Some(cached) = self.cache.get(*index) else {
                        continue;
                    };
                    let count = cached.lines.len();
                    let item_start = cursor;
                    let item_end = cursor + count;
                    cursor = item_end;
                    if count > 0 && item_end > start && item_start < end {
                        let from = start.max(item_start) - item_start;
                        let to = end.min(item_end) - item_start;
                        for line in cached.lines[from..to].iter().cloned() {
                            visible.push(line);
                            hit_map.push(None);
                        }
                    }
                }
            }
        }
        // Save the viewport hit map so the live mouse dispatch can resolve a
        // click's row against the same rows the renderer drew. Every
        // entry/body row is `None`; only activity summaries are `Some`, so
        // the map never clones entry content.
        self.hit_map = hit_map;
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
            area.width.saturating_sub(2) as usize,
        );
        // Keep three editable text rows visible before growing for wrapped input.
        // Terminal layout is cell-based; the border supplies the practical padding.
        // Cap the input band at the height left after the fixed rows so it never
        // grows into the header, the divider, the history content row, or the
        // footer. Below roughly ten terminal rows the input band degrades to
        // border-only or fully invisible; the divider row survives down to
        // content height five, and one history content row is guaranteed from
        // content height six upward.
        let input_height = (input_lines.len() as u16 + 2)
            .max(5)
            .clamp(5, 12)
            .min(area.height.saturating_sub(INPUT_RESERVED_ROWS));
        let regions = Layout::vertical([
            Constraint::Length(HEADER_HEIGHT),
            Constraint::Min(1),
            Constraint::Length(input_height),
            Constraint::Length(2),
        ])
        .split(area);
        // One horizontal Layout owns the header geometry: logo, metadata,
        // separator, and the kitty's reserved right column. The reserved rect is
        // passed to both the header (which must not draw into it) and the
        // artwork (which anchors to its right edge), so the two can never
        // disagree. On a narrow terminal the reservation collapses to zero and
        // the kitty is suppressed rather than overpainting metadata.
        let show_kitty = area.width
            >= HEADER_LOGO_WIDTH + HEADER_SEPARATOR_WIDTH + kitty_width + HEADER_MIN_METADATA_WIDTH;
        let reserved = if show_kitty { kitty_width } else { 0 };
        let header_columns = Layout::horizontal([
            Constraint::Length(HEADER_LOGO_WIDTH),
            Constraint::Min(0),
            Constraint::Length(HEADER_SEPARATOR_WIDTH),
            Constraint::Length(reserved),
        ])
        .split(regions[0]);
        draw_header(frame, app, header_columns[0], header_columns[1]);
        // The header's own Layout split is the single source of truth for the
        // kitty's reserved column. Assigned every frame so a resize that
        // suppresses the kitty clears the reservation; the divider below reads
        // it to stop exactly at the artwork's left edge.
        self.last_kitty_reservation = if show_kitty {
            Some(header_columns[3])
        } else {
            None
        };
        // The history band's first row (`regions[1].y`) carries the session
        // divider; content sits one row below. This is the true asymmetric
        // inner rect for a LEFT|RIGHT|TOP border set: one column per side and
        // exactly one top row. A zero-height band skips divider and paragraph
        // entirely so no u16 arithmetic can underflow.
        let history_inner = Rect::new(
            regions[1].x + 1,
            regions[1].y + 1,
            regions[1].width.saturating_sub(2),
            regions[1].height.saturating_sub(1),
        );
        if regions[1].height == 0 {
            self.last_history_rect = None;
        } else {
            draw_history_divider(frame, theme, regions[1], self.last_kitty_reservation);
            let history = self.history(
                app,
                history_inner.width as usize,
                history_inner.height as usize,
            );
            // Record the content-only inner rect so the live mouse dispatch can
            // resolve clicks against the same coordinates the renderer just
            // drew. Geometry stays out of `App`; only the renderer touches it.
            self.last_history_rect = Some(history_inner);
            frame.render_widget(
                Paragraph::new(history)
                    .block(border_block(theme).borders(Borders::LEFT | Borders::RIGHT)),
                Rect::new(
                    regions[1].x,
                    regions[1].y + 1,
                    regions[1].width,
                    regions[1].height.saturating_sub(1),
                ),
            );
        }
        // The kitty canvas' top is terminal row 0 (the outer margin row), and
        // its height is the distance from there down to the input region, so
        // lower canvas rows survive short terminals. x/width come from the
        // reservation the header Layout produced. Suppression passes width 0 so
        // `draw_kitty` still clears `last_kitty_paint` before its early return.
        self.draw_kitty(
            frame,
            Rect::new(
                self.last_kitty_reservation.map_or(0, |r| r.x),
                frame.area().y,
                self.last_kitty_reservation.map_or(0, |r| r.width),
                regions[2].y.saturating_sub(frame.area().y),
            ),
            &kitty_rows,
        );
        draw_input(frame, app, regions[2]);
        let footer =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(regions[3]);
        frame.render_widget(
            Paragraph::new(sanitize_terminal_text(&app.status, false).into_owned())
                .style(Style::default().fg(color(&theme.muted))),
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
                Span::raw("  Tab agent | Alt+Enter newline | F6 activity"),
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
            let mut body = String::from(
                "n: start a new workflow\nr: repeat this workflow\nq: exit workflow mode",
            );
            if let Some(blocked) = &app.queue_blocked {
                body.push_str(&format!(
                    "\n\nQueue blocked ({} message(s) pending):\n{}\n\nPress Enter to retry the front message.",
                    blocked.pending, blocked.error
                ));
            }
            draw_overlay(frame, "Workflow complete", &body, None, app, area);
        }
        if app.help {
            draw_overlay(frame, "Help", HELP, None, app, area);
        }
        if let Some(approval) = &app.approval {
            let choices = if approval.workflow {
                " y Continue | r Retry | s Skip | a Abort "
            } else if approval.persist_allowed {
                " y Yes | p Yes-persist | n No | a Abort "
            } else {
                " y Yes | n No | a Abort "
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

    /// Resolve a terminal cell to the activity-summary id drawn there, if
    /// any. Returns `None` outside the last chat-history rect, on a cell the
    /// kitty actually painted, and for every non-summary row (entry bodies and
    /// blank padding included). Screen row maps to the viewport-relative
    /// index used to build `hit_map`; stale or empty metadata and a rect that
    /// no longer matches the last paint are all handled without panicking. The
    /// session divider row and the left/right border columns are intentionally
    /// not activity-selectable: they lie outside `last_history_rect`, which is
    /// the content-only inner rect.
    pub fn activity_at(&self, column: u16, row: u16) -> Option<String> {
        let rect = self.last_history_rect?;
        if row < rect.y || row >= rect.y.saturating_add(rect.height) {
            return None;
        }
        if column < rect.x || column >= rect.x.saturating_add(rect.width) {
            return None;
        }
        if self
            .last_kitty_paint
            .iter()
            .any(|&(y, start, end)| y == row && column >= start && column < end)
        {
            return None;
        }
        let index = (row - rect.y) as usize;
        match self.hit_map.get(index) {
            Some(Some(summary)) => Some(summary.id.clone()),
            _ => None,
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

    /// Test-only accessor for the cached chat-entry lines. Used by the
    /// release-mode perf harnesses to assert that the warm draw
    /// populated one cache row per detail entry and that subsequent
    /// draws hit the cache without invalidating it.
    #[cfg(test)]
    pub(super) fn cache_len(&self) -> usize {
        self.cache.len()
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

    /// Paint the kitty canvas into the column `area` reserves for it.
    ///
    /// `area.x`/`area.width` come from the header's own horizontal Layout split,
    /// so the artwork and the reserved metadata column cannot disagree. `area.y`
    /// is the canvas's top row (terminal row 0, the outer margin row) and
    /// `area.height` is the vertical clamp (rows available above the input
    /// region), so lower canvas rows survive short terminals. The artwork
    /// occupies only the reserved column: its rows may vertically coincide with
    /// the header metadata rows, the divider, and history rows without ever
    /// overlapping non-reserved columns. A zero `area.width` is the
    /// narrow-terminal suppression: metadata wins and nothing is painted.
    /// Painted cells are recorded in `last_kitty_paint` so activity hit tests
    /// can exclude them, including spans that overlap header rows.
    fn draw_kitty(&mut self, frame: &mut Frame, area: Rect, rows: &[Line<'static>]) {
        // Clear before any early return so a resize that collapses the
        // visible area cannot leave stale occlusion spans behind.
        self.last_kitty_paint.clear();
        let width = rows.iter().map(Line::width).max().unwrap_or(0) as u16;
        let height = rows.len() as u16;
        if width == 0 || area.width == 0 {
            return;
        }
        let visible_width = width.min(area.width).min(frame.area().width);
        // Clamp by the rows available above the input region, not the chat band
        // height, so a short terminal keeps Fly Girl's lower body rows.
        let visible_height = height
            .min(area.height)
            .min(frame.area().height.saturating_sub(area.y));
        if visible_width == 0 || visible_height == 0 {
            return;
        }
        // Canvas row 0 is empty padding in the idle rest pose, anchored to
        // terminal row 0 (`area.y`, the outer margin row). Animated processing
        // frames may move a glyph into row 0 — Fly Girl's rising Z in
        // `kitty.rs` — which then paints on the top margin row inside the
        // reserved column. The right edge aligns to that area's right edge (the
        // reserved column's `right()`). The header's reservation and the artwork
        // read the same `area` rect, so placement cannot drift. Body and face
        // rows never shift because we iterate the source vertically exactly as
        // the canvas lays them out — no first-content-row re-anchoring.
        let kitty_area = Rect::new(
            area.right().saturating_sub(visible_width),
            area.y,
            visible_width,
            visible_height,
        );
        // Record the cells the artwork actually paints, one `(y, x_start,
        // x_end)` span per terminal row with at least one glyph. This
        // excludes the transparent padding cells of the full canvas so the
        // live mouse dispatch can hit-test the visible artwork. Geometry
        // stays on the renderer; App never sees it.
        let mut paint: Vec<(u16, u16, u16)> = Vec::new();
        let buffer = frame.buffer_mut();
        for (row, line) in rows.iter().take(kitty_area.height as usize).enumerate() {
            let mut column = 0u16;
            let y = kitty_area.y + row as u16;
            let mut painted_start: Option<u16> = None;
            let mut painted_end = 0u16;
            for span in &line.spans {
                let style = line.style.patch(span.style);
                for character in span.content.chars() {
                    let cells = character.width().unwrap_or(0) as u16;
                    if !character.is_whitespace() && column < kitty_area.width {
                        let x = kitty_area.x + column;
                        // Actual glyphs, not sampled pixels: the artwork reads
                        // as text and theme colors separate body, eyes, and Z's.
                        let mut glyph = [0u8; 4];
                        buffer[(x, y)]
                            .set_symbol(character.encode_utf8(&mut glyph))
                            .set_style(style);
                        painted_start = Some(painted_start.map_or(x, |s| s.min(x)));
                        // `cells.max(1)` keeps a zero-width combining glyph
                        // inside the painted span instead of collapsing it.
                        painted_end = painted_end.max(x.saturating_add(cells.max(1)));
                    }
                    column = column.saturating_add(cells);
                }
            }
            if let Some(start) = painted_start {
                paint.push((y, start, painted_end));
            }
        }
        self.last_kitty_paint = paint;
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

/// Header band height: two shared rows. The logo column occupies both rows;
/// the metadata column uses row 0 for the composed model/agent/effort line and
/// row 1 for the spend/context line.
const HEADER_HEIGHT: u16 = 2;

/// Rows preserved outside the input band: the two-row header, the one-row
/// session divider, one history content row, and the two-row footer.
/// `input_height` is capped at `content.height - INPUT_RESERVED_ROWS` so the
/// input band cannot grow into them. Below roughly ten terminal rows the input
/// band degrades to border-only or fully invisible; the divider row survives
/// down to content height five, and one history content row is guaranteed from
/// content height six upward. This deterministic degradation replaces the
/// previously unsatisfiable constraint set.
const INPUT_RESERVED_ROWS: u16 = 6;

/// Exact cell width of the widest logo text line (`" diet_"`), so the logo
/// column is sized with `Length` and never grows. The metadata column is the
/// only `Min` constraint and therefore receives all remaining slack.
const HEADER_LOGO_WIDTH: u16 = 6;

/// Blank column between the metadata column and the kitty's reserved column.
const HEADER_SEPARATOR_WIDTH: u16 = 1;

/// Floor below which the kitty is suppressed so the metadata line stays
/// right-aligned and readable rather than clipped. This is not a width at
/// which the whole line survives: `tail_text` keeps the tail and drops the
/// head, and the `" | agent X | effort Y"` suffix alone meets or exceeds 27
/// cells, so at exactly this floor the model label may be dropped entirely
/// and the agent name may be truncated mid-word; only the effort suffix is
/// reliably visible. Because the logo column is fixed at `HEADER_LOGO_WIDTH`
/// and only the metadata column grows, metadata receives
/// `content_width - HEADER_LOGO_WIDTH - HEADER_SEPARATOR_WIDTH - kitty_width`,
/// and the kitty is suppressed whenever that remainder would fall below this
/// minimum.
const HEADER_MIN_METADATA_WIDTH: u16 = 24;

/// Draw the two-row header. The caller owns the horizontal geometry (logo rect,
/// metadata rect, and the kitty's reserved right column) so the header and the
/// artwork share one Layout. `logo_area` spans both header rows; the metadata
/// column carries the composed model/agent/effort line on top and the
/// spend/context line below.
fn draw_header(frame: &mut Frame, app: &App, logo_area: Rect, metadata_area: Rect) {
    let theme = &app.theme;
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
        logo_area,
    );
    let metadata_rows =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(metadata_area);
    // Top metadata line: the effective model, the agent resolved exactly like
    // `Engine::scope` (explicit selection, else the configured default agent,
    // else the `default` sentinel) and computed in `App::refresh_model`, and
    // the effective effort. Legacy agent modes are intentionally never shown.
    // When the composed string is wider than the metadata column, `tail_text`
    // truncates from the left so the agent and effort suffixes survive instead
    // of being dropped by Ratatui's head-keeping truncation.
    let composed = format!(
        "{} | agent {} | effort {}",
        app.model_label, app.effective_agent_label, app.effort_label
    );
    let composed = sanitize_terminal_text(&composed, false);
    let metadata_width = metadata_rows[0].width as usize;
    let metadata = if UnicodeWidthStr::width(composed.as_ref()) > metadata_width {
        tail_text(composed.as_ref(), metadata_width)
    } else {
        composed.into_owned()
    };
    frame.render_widget(
        Paragraph::new(metadata).alignment(Alignment::Right).style(
            Style::default()
                .fg(color(&theme.accent))
                .add_modifier(Modifier::BOLD),
        ),
        metadata_rows[0],
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
        metadata_rows[1],
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

/// Visibility rule for a chat entry during history assembly. Unowned
/// entries (main-context prose) always show; an owned entry shows only
/// when its owner is expanded and every ancestor of the owner is
/// expanded. Kept as a free function so both history passes share one
/// implementation. Reads only indexes, so it clones nothing.
fn entry_visible(app: &App, snapshot: &LayoutSnapshot, index: usize) -> bool {
    let Some(owner) = app
        .entries
        .get(index)
        .and_then(|e| e.activity_id.as_deref())
    else {
        return true;
    };
    app.activity(owner).is_some_and(|node| node.expanded)
        && app.ancestor_visible_for(snapshot, owner)
}

/// Render the one-line summary for an activity from the per-frame
/// `LayoutSnapshot`. Pure: the function reads the node and theme but
/// mutates nothing. Depth, ancestor visibility, and the `( +N )`
/// descendant badge all come from the snapshot's precomputed vectors,
/// so the per-row work is a sequence of `O(1)` lookups instead of a
/// parent-chain climb. Cheap on purpose — toggling expansion or
/// refreshing the status re-emits the line each frame without
/// rebuilding the entry cache, which is the property the Wave 2 tests
/// assert.
fn activity_summary_line_snapshot(
    app: &App,
    snapshot: &LayoutSnapshot,
    id: &str,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let node = match app.activity(id) {
        Some(node) => node,
        // Defensive: an unknown id should never reach the renderer
        // because `ancestor_visible_for` filters them out, but a
        // missing row is safer than a panic if a future refactor
        // changes that contract. Sanitize the id so a malformed one
        // from a producer cannot inject control characters or
        // newlines into the rendered summary.
        None => {
            return Line::raw(format!("[?] {}", App::sanitize_activity_id(id)));
        }
    };
    let depth = app.depth_for(snapshot, id) as usize;
    let indent = summary_indent(depth, theme.ascii);
    let marker = if node.expanded { "[-]" } else { "[+]" };
    let kind_color = kind_color(node.start.kind, theme);
    let status_text = status_label(node.status);
    let status_color = status_color(node.status, theme);
    let title = sanitize_title(&node.start.title);
    let count = app.descendant_count_for(snapshot, id);
    let count_text = if count > 0 {
        format!(" (+{count})")
    } else {
        String::new()
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !indent.is_empty() {
        spans.push(Span::styled(
            indent,
            Style::default().fg(color(&theme.border)),
        ));
    }
    spans.push(Span::styled(
        format!("{marker} "),
        Style::default().fg(color(&theme.muted)),
    ));
    spans.push(Span::styled(
        title,
        Style::default()
            .fg(color(kind_color))
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!(" [{status_text}]"),
        Style::default().fg(color(status_color)),
    ));
    if !count_text.is_empty() {
        spans.push(Span::styled(
            count_text,
            Style::default().fg(color(&theme.muted)),
        ));
    }
    let mut line = truncate_line(spans, width);
    // Keyboard focus on the activity spine is shown with the same CTA
    // selection colors the pickers use. Only the background is applied so
    // the kind/status foregrounds stay readable; the row remains a single
    // line and entry-cache/hit-map behavior is untouched.
    if app.activity_focused() && app.focused_activity_id() == Some(id) {
        line.style = Style::default().bg(color(&theme.cta_background));
    }
    line
}

/// Per-depth branch indent. Unicode uses a 2-cell `│ ` pair per level; the
/// ASCII fallback mirrors the border style with `|  `. Both pad with spaces
/// for every additional depth so the marker column stays fixed.
fn summary_indent(depth: usize, ascii: bool) -> String {
    if depth == 0 {
        return String::new();
    }
    let unit = if ascii { "|  " } else { "│  " };
    let mut out = String::with_capacity(unit.len() * depth);
    for _ in 0..depth {
        out.push_str(unit);
    }
    out
}

fn kind_color(kind: ActivityKind, theme: &Theme) -> &str {
    match kind {
        ActivityKind::Tool => &theme.tool,
        ActivityKind::Subagent => &theme.assistant,
        ActivityKind::WorkflowStep => &theme.accent,
    }
}

fn status_label(status: Option<ActivityStatus>) -> &'static str {
    match status {
        None => "running",
        Some(ActivityStatus::Success) => "ok",
        Some(ActivityStatus::Error) => "error",
        Some(ActivityStatus::Cancelled) => "cancelled",
        Some(ActivityStatus::Denied) => "denied",
    }
}

fn status_color(status: Option<ActivityStatus>, theme: &Theme) -> &str {
    match status {
        None => &theme.muted,
        Some(ActivityStatus::Success) => &theme.success,
        Some(ActivityStatus::Error) => &theme.error,
        Some(ActivityStatus::Cancelled) => &theme.warning,
        Some(ActivityStatus::Denied) => &theme.error,
    }
}

/// Replace control characters and trim to a single-line so a hostile title
/// never escapes the summary row. Tabs become spaces; embedded newlines are
/// folded to single spaces; trailing whitespace is dropped.
fn sanitize_title(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut pending_space = false;
    for character in title.chars() {
        if character == '\n' || character == '\r' {
            pending_space = true;
            continue;
        }
        if is_unsafe_terminal_char(character) && character != '\t' {
            continue;
        }
        let character = if character == '\t' { ' ' } else { character };
        if pending_space && !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        pending_space = false;
        out.push(character);
    }
    out
}

/// Clamp the summary to exactly one row at the requested cell width. When
/// the spans fit they pass through unchanged; otherwise the text span is
/// trimmed at a character boundary and a leading ellipsis is appended so the
/// row still terminates cleanly. Spans are coalesced so the result holds a
/// minimal span count.
fn truncate_line(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let width = width.max(1);
    let used: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if used <= width {
        return Line::from(spans);
    }
    let mut budget = width.saturating_sub(1); // ellipsis cell
    let mut out: Vec<Span<'static>> = Vec::new();
    for span in spans {
        let cell_width = UnicodeWidthStr::width(span.content.as_ref());
        if cell_width <= budget {
            budget -= cell_width;
            out.push(span);
            continue;
        }
        let mut text = String::new();
        let mut used_in_span = 0usize;
        for character in span.content.chars() {
            let cells = UnicodeWidthChar::width(character).unwrap_or(0);
            if used_in_span + cells > budget {
                break;
            }
            text.push(character);
            used_in_span += cells;
        }
        text.push('…');
        out.push(Span::styled(text, span.style));
        break;
    }
    Line::from(out)
}

/// Text the transcript should actually show for an entry. Tool results are
/// expanded to their readable payload first (parsed/pretty-printed JSON,
/// stdout/stderr, and error calls), so the completed-entry size and line caps
/// apply to what is displayed rather than the compact wire form. Every other
/// role renders `entry.text` unchanged. Expansion keeps the rich path's
/// existing 100 KiB guard, so a pathological raw payload is never parsed.
fn entry_display_text(entry: &Entry) -> Cow<'_, str> {
    if entry.role == "tool" && entry.text.len() <= 100_000 {
        Cow::Owned(tool_result_text(&entry.text))
    } else {
        Cow::Borrowed(entry.text.as_str())
    }
}

fn render_entry(entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    // Caps are evaluated against the expanded display text, not the raw wire
    // JSON: a small compact tool payload can pretty-print into a much larger
    // document. `entry.text` itself is never modified.
    let display = entry_display_text(entry);
    let total_lines = logical_line_count(display.as_ref());
    if display.len() > COMPLETED_RENDER_BYTES || total_lines > COMPLETED_RENDER_LINES {
        return render_bounded_entry(entry, display.as_ref(), width, theme, total_lines);
    }
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
    let content_lines = markdown(display.as_ref(), theme, color(role_color));
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

/// Byte cap for the lightweight streaming renderer. Only the trailing slice of
/// a still-streaming entry is formatted, so the per-delta render cost is
/// bounded by this constant regardless of how much text has arrived.
const STREAMING_RENDER_BYTES: usize = 8 * 1024;

/// Byte and logical-line ceilings for a completed entry that still uses the
/// full Markdown/syntect pipeline. Above either limit the renderer switches to
/// the bounded plain-text path: highlighting and wrapping a multi-megabyte
/// message that cannot fit on screen is not worth the cost. Counting is a
/// single O(n) pass and only runs when the entry's revision changed, because
/// `history` rebuilds a cached entry only on a revision bump.
const COMPLETED_RENDER_BYTES: usize = 128 * 1024;
const COMPLETED_RENDER_LINES: usize = 4_000;

/// Logical-line windows kept for a line-oversized completed entry. The head
/// and tail windows render whole logical lines with the single truncation
/// marker between them, so a message with millions of short lines still draws
/// in bounded space. When the windows themselves exceed
/// `COMPLETED_RENDER_BYTES`, `excerpt_parts` applies balanced UTF-8-safe byte
/// cuts instead.
const COMPLETED_HEAD_LINES: usize = 64;
const COMPLETED_TAIL_LINES: usize = 64;

/// Logical line count for a completed entry: embedded newlines plus one for a
/// non-empty tail. Empty text is zero lines, matching how an empty entry
/// renders.
fn logical_line_count(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.as_bytes()
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + 1
    }
}

/// Bounded plain-text renderer for an oversized completed entry. It keeps the
/// ordinary role/context header, gutter, and borders. A line-oversized entry
/// (`> COMPLETED_RENDER_LINES`) shows the first `COMPLETED_HEAD_LINES` and last
/// `COMPLETED_TAIL_LINES` logical lines with the single muted marker between
/// them; a byte-only oversize keeps the balanced UTF-8-safe head/tail excerpt
/// with the marker above it. Either way the excerpt totals at most
/// `COMPLETED_RENDER_BYTES` and one muted marker names the true size. The
/// Markdown/syntect pipeline is skipped entirely, so nothing that cannot be
/// displayed is parsed or highlighted. `text` is the display slice chosen by
/// `entry_display_text` (the expanded tool payload, or `entry.text` itself for
/// every other role); `entry.text`, the session log, and exports are never
/// modified.
fn render_bounded_entry(
    entry: &Entry,
    text: &str,
    width: usize,
    theme: &Theme,
    total_lines: usize,
) -> Vec<Line<'static>> {
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
    let prefix = Span::styled(
        format!("{vertical} "),
        Style::default().fg(color(&theme.border)),
    );
    let content_style = Style::default().fg(color(role_color));
    let total_kib = text.len().div_ceil(1024);
    let marker = format!(
        "[display truncated: {total_lines} lines, {total_kib} KiB total; use /export for full output]"
    );
    let push_marker = |lines: &mut Vec<Line<'static>>| {
        lines.push(truncate_line(
            vec![
                prefix.clone(),
                Span::styled(marker.clone(), Style::default().fg(color(&theme.muted))),
            ],
            width,
        ));
    };
    if total_lines > COMPLETED_RENDER_LINES {
        // Logical-line oversize: show whole lines from the head and tail with
        // the single marker between them. Only the window slices are
        // collected, never the full line vector.
        let head = text
            .split('\n')
            .take(COMPLETED_HEAD_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let mut tail_lines = text
            .split('\n')
            .rev()
            .take(COMPLETED_TAIL_LINES)
            .collect::<Vec<_>>();
        tail_lines.reverse();
        let tail = tail_lines.join("\n");
        // Large windows still respect the byte budget via balanced,
        // UTF-8-safe cuts.
        let (head, tail) = if head.len() + tail.len() + 1 > COMPLETED_RENDER_BYTES {
            excerpt_parts(&format!("{head}\n{tail}"), COMPLETED_RENDER_BYTES)
        } else {
            (head, tail)
        };
        push_wrapped_content(&mut lines, &prefix, &head, content_style, width);
        push_marker(&mut lines);
        push_wrapped_content(&mut lines, &prefix, &tail, content_style, width);
    } else {
        // Byte-only oversize: keep the balanced UTF-8-safe head/tail excerpt
        // with the marker above it.
        push_marker(&mut lines);
        let excerpt = bounded_excerpt(text, COMPLETED_RENDER_BYTES);
        push_wrapped_content(&mut lines, &prefix, &excerpt, content_style, width);
    }
    lines.push(Line::styled(
        format!("{bottom_left} {bottom_right}"),
        Style::default().fg(color(&theme.border)),
    ));
    lines
}

/// Head and tail excerpt joined by a single newline, UTF-8-safe at both cuts
/// and never longer than `cap` bytes. The halves are balanced: the head takes
/// up to half the budget and the tail takes what remains after its boundary
/// snap, so a multibyte cut can only make the excerpt shorter.
fn bounded_excerpt(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let (head, tail) = excerpt_parts(text, cap);
    let mut out = String::with_capacity(head.len() + 1 + tail.len());
    out.push_str(&head);
    out.push('\n');
    out.push_str(&tail);
    out
}

/// Balanced, UTF-8-safe head and tail parts of `text` whose combined byte
/// length plus the one-byte separator is at most `cap`. The head takes up to
/// half the budget and the tail takes what remains after its boundary snap, so
/// a multibyte cut can only make the parts shorter. Callers that need the
/// truncation marker between the halves use the parts directly;
/// `bounded_excerpt` joins them with the separator newline.
fn excerpt_parts(text: &str, cap: usize) -> (String, String) {
    // Reserve one byte for the newline that separates the two halves.
    let budget = cap.saturating_sub(1);
    let half = budget / 2;
    let head_end = floor_char_boundary(text, half);
    let head = &text[..head_end];
    let tail_budget = budget - head.len();
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_budget));
    (head.to_owned(), text[tail_start..].to_owned())
}

/// Append the wrapped transcript rows for `text`, one `Line` per logical line.
/// Splitting first keeps every embedded newline a row boundary instead of
/// passing one multiline string through a single `Line::styled`. `width` is the
/// full entry width, so the two-cell gutter is subtracted here exactly as the
/// rich path does.
fn push_wrapped_content(
    lines: &mut Vec<Line<'static>>,
    prefix: &Span<'static>,
    text: &str,
    style: Style,
    width: usize,
) {
    for logical in text.split('\n') {
        let content = Line::styled(logical.to_owned(), style);
        for wrapped in wrap_lines_at_words(vec![content], width.saturating_sub(2)) {
            let mut spans = vec![prefix.clone()];
            spans.extend(wrapped.spans);
            lines.push(Line::from(spans));
        }
    }
}

/// Largest char boundary at or below `index`.
fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Smallest char boundary at or above `index`.
fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Lightweight renderer for an entry that is still receiving provider deltas.
/// It skips the Markdown/syntect pipeline entirely: syntax highlighting a
/// partial document on every delta is the expensive path this avoids. The
/// role/context header, gutter, and wrapping match `render_entry` so the
/// transcript does not visually jump when the final rich render replaces it.
/// Only the trailing `STREAMING_RENDER_BYTES` of `entry.text` are formatted,
/// starting on a UTF-8 boundary; when anything earlier is hidden, one muted
/// single-line marker reports the total size and the truncation. `entry.text`
/// itself is never modified.
fn render_streaming_entry(entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
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
    let total = entry.text.len();
    let (truncated, visible) = if total > STREAMING_RENDER_BYTES {
        let mut start = total - STREAMING_RENDER_BYTES;
        // Never split a multi-byte codepoint: snap forward to the next
        // boundary so `visible` is always valid UTF-8.
        while start < total && !entry.text.is_char_boundary(start) {
            start += 1;
        }
        (true, &entry.text[start..])
    } else {
        (false, entry.text.as_str())
    };
    let prefix = Span::styled(
        format!("{vertical} "),
        Style::default().fg(color(&theme.border)),
    );
    if truncated {
        let total_kib = total.div_ceil(1024);
        let marker = format!(
            "[streaming output truncated: {total_kib} KiB total, showing latest {} KiB]",
            STREAMING_RENDER_BYTES / 1024
        );
        lines.push(truncate_line(
            vec![
                prefix.clone(),
                Span::styled(marker, Style::default().fg(color(&theme.muted))),
            ],
            width,
        ));
    }
    push_wrapped_content(
        &mut lines,
        &prefix,
        visible,
        Style::default().fg(color(role_color)),
        width,
    );
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
    let title = sanitize_terminal_text(title, false);
    let block = border_block(theme)
        .title(title.into_owned())
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
/// Border glyph set selected by `theme.ascii`, shared by `border_block` and the
/// session divider so both use identical corners and rules.
fn border_symbols(theme: &Theme) -> border::Set {
    if theme.ascii {
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
    }
}
fn border_block(theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_set(border_symbols(theme))
        .border_style(Style::default().fg(color(&theme.border)))
}

/// Draw the session divider as a custom row of cells in `band`'s first row.
///
/// The divider stops at the kitty reservation's left edge (`reservation.x`);
/// no glyph is ever written at or beyond it, including cells where the kitty
/// artwork is transparent. When the kitty is suppressed the rule runs to the
/// band's right edge and closes with the top-right corner glyph so it joins the
/// right side border. `band.height == 0` is the caller's guard, but this also
/// tolerates a zero-height or zero-width band without underflow.
fn draw_history_divider(frame: &mut Frame, theme: &Theme, band: Rect, reservation: Option<Rect>) {
    if band.height == 0 || band.width == 0 {
        return;
    }
    let symbols = border_symbols(theme);
    let style = Style::default().fg(color(&theme.border));
    let y = band.y;
    let cutoff = reservation.map_or(band.right(), |r| r.x).min(band.right());
    let buffer = frame.buffer_mut();
    if band.x < cutoff {
        buffer[(band.x, y)]
            .set_symbol(symbols.top_left)
            .set_style(style);
    }
    let mut x = band.x.saturating_add(1);
    while x < cutoff {
        buffer[(x, y)]
            .set_symbol(symbols.horizontal_top)
            .set_style(style);
        x = x.saturating_add(1);
    }
    // No kitty: the rule reaches the right edge, so close it with the top-right
    // corner to join the right side border.
    if cutoff >= band.right() && band.width >= 2 {
        buffer[(band.right() - 1, y)]
            .set_symbol(symbols.top_right)
            .set_style(style);
    }
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
                if is_unsafe_terminal_char(character) && character != '\t' {
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

/// Fast path for [`wrap_lines_at_words`]: a single-span line whose content
/// is non-empty printable ASCII (`0x21..=0x7E`) has no whitespace, no
/// newline, no control byte, and no wide glyph, so wrapping is a pure
/// `width`-sized byte split. Returns `None` for every other input, which
/// must keep flowing through the general wrapper to preserve its
/// whitespace collapsing, trailing trim, wide-glyph, style-run, and
/// newline behaviour. The source span style is patched against the line
/// style exactly as the general path does, and an exactly-fitting or
/// over-wide line still yields one row per `width` bytes (no trailing
/// empty row).
fn wrap_ascii_run(line: &Line<'static>, width: usize) -> Option<Vec<Line<'static>>> {
    if line.spans.len() != 1 {
        return None;
    }
    let span = &line.spans[0];
    let content = span.content.as_ref();
    if content.is_empty() || !content.bytes().all(|byte| byte.is_ascii_graphic()) {
        return None;
    }
    let style = line.style.patch(span.style);
    let mut rows = Vec::with_capacity(content.len().div_ceil(width));
    for chunk in content.as_bytes().chunks(width) {
        // Printable ASCII was verified above, so each byte slice is
        // already valid UTF-8; `from_utf8` cannot fail here.
        let text = std::str::from_utf8(chunk).expect("ascii run is valid utf-8");
        rows.push(Line::from(Span::styled(text.to_string(), style)));
    }
    Some(rows)
}

/// Wrap chat entries at whitespace where possible, keeping long unbroken values intact until
/// they exceed the terminal width.
fn wrap_lines_at_words(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut output = vec![];
    for line in lines {
        // Fast path for the overwhelmingly common chat/streaming shape:
        // a single span of printable ASCII with no whitespace. Every
        // character is exactly one cell wide and there is no word or
        // whitespace to break on, so the bytes can be sliced directly
        // into `width`-sized chunks. This skips building the
        // `Vec<(char, Style)>` below, which dominates the per-line setup
        // cost for long unbroken tokens. Anything else (Unicode, tabs,
        // spaces, newlines, control bytes, or multiple spans) falls
        // through to the general wrapper so its semantics are untouched.
        if let Some(rows) = wrap_ascii_run(&line, width) {
            output.extend(rows);
            continue;
        }
        // Pre-size the per-line character buffer so the unbroken-line
        // hot path pays zero Vec growth events. Byte length is an upper
        // bound on codepoint count for ASCII text but a strict *over*
        // estimate for multibyte text — every non-ASCII codepoint takes
        // at least 2 bytes in UTF-8, so a multi-codepoint string can be
        // many times longer in bytes than in chars. The Vec only rounds
        // the request up; the slight over-allocation on multibyte input
        // is cheaper than recomputing the codepoint count here.
        let capacity: usize = line.spans.iter().map(|span| span.content.len()).sum();
        let mut characters = Vec::with_capacity(capacity);
        for span in line.spans {
            let style = line.style.patch(span.style);
            for character in span.content.chars() {
                if character == '\n' {
                    push_wrapped_line(&mut output, std::mem::take(&mut characters), width);
                } else if !is_unsafe_terminal_char(character) || character == '\t' {
                    characters.push((if character == '\t' { ' ' } else { character }, style));
                }
            }
        }
        // Push the trailing slice even when it is empty: a fully-blank
        // input line still maps to one output row (matches the original
        // "1:1 empty-line preserved" behaviour callers depend on, e.g.
        // markdown paragraphs separated by a blank line). The empty case
        // is handled inside `push_wrapped_line` so the caller does not
        // need to know about it.
        push_wrapped_line(&mut output, std::mem::take(&mut characters), width);
    }
    output
}

fn push_wrapped_line(
    output: &mut Vec<Line<'static>>,
    characters: Vec<(char, Style)>,
    width: usize,
) {
    if characters.is_empty() {
        output.push(Line::default());
        return;
    }
    // Walk the input forward with a single index. Each row scans from
    // `start` to `end` once, so the entire wrap is O(n) instead of the
    // O(n*width) front-drain behaviour the previous implementation had.
    // `start` skips over leading whitespace at every wrap boundary, and
    // `trimmed_end` mirrors the trailing-whitespace trim of the previous
    // implementation. Visual semantics, Unicode widths, styles, empty
    // lines, ASCII themes, and over-wide single glyphs are preserved.
    let mut start = 0usize;
    while start < characters.len() {
        // Skip leading whitespace at a wrap boundary so the next row does
        // not start with the spaces that broke the previous row. We only
        // skip after at least one row has been emitted; the very first
        // row preserves leading whitespace just like the old loop did.
        if start > 0 {
            while start < characters.len() && characters[start].0.is_whitespace() {
                start += 1;
            }
        }
        if start >= characters.len() {
            break;
        }
        let mut used = 0usize;
        let mut end = start;
        let mut last_break: Option<usize> = None;
        // Fold the "all chars in this row share one style" probe into
        // the inner scan so the fast-path decision below is O(1).
        let first_style = characters[start].1;
        let mut all_same_style = true;
        while end < characters.len() {
            let cells = characters[end].0.width().unwrap_or(0);
            // Mirrors the previous `used + cells > width && fit > 0` guard:
            // the very first character of a row is always accepted even
            // when it is wider than the column, so over-wide single glyphs
            // still produce a non-empty row.
            if used + cells > width && end > start {
                break;
            }
            if all_same_style && characters[end].1 != first_style {
                all_same_style = false;
            }
            used += cells;
            end += 1;
            if characters[end - 1].0.is_whitespace() {
                last_break = Some(end);
            }
        }
        let original_len = characters.len();
        let break_at = if end == original_len || characters[end].0.is_whitespace() {
            end
        } else {
            last_break
                .filter(|index| *index > start)
                .unwrap_or_else(|| end.max(start + 1))
        };
        // Trim trailing whitespace from the produced row, matching the
        // previous `line.pop()` loop. `break_at` may itself point at a
        // whitespace char when we kept it as the next row's lead; the trim
        // happens against the copy we are about to emit.
        let mut trimmed_end = break_at;
        while trimmed_end > start && characters[trimmed_end - 1].0.is_whitespace() {
            trimmed_end -= 1;
        }
        // Fast path for the common case where every char in this row
        // carries the same style: a single `Span::styled` is enough and
        // we skip the per-row `Vec<Span>` and span coalescing that the
        // slice helper does.
        if all_same_style {
            let text: String = characters[start..trimmed_end]
                .iter()
                .map(|&(c, _)| c)
                .collect();
            output.push(Line::from(Span::styled(text, first_style)));
        } else {
            output.push(characters_to_line_slice(&characters[start..trimmed_end]));
        }
        start = break_at;
    }
}

/// Build a `Line` from a slice of `(char, Style)` pairs. The linear wrap
/// path slices the shared character buffer per row and calls this helper,
/// so each output row reuses the source memory instead of allocating an
/// intermediate `Vec`. Consecutive same-style runs are coalesced into a
/// single `Span`, and an empty input produces an empty `Line`.
fn characters_to_line_slice(characters: &[(char, Style)]) -> Line<'static> {
    let mut spans = vec![];
    let mut text = String::new();
    let mut style: Option<Style> = None;
    for &(character, character_style) in characters {
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
        model::{ActivityEvent, ActivityKind, ActivityPhase, Message, UiEvent},
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
    fn test_backend_cells_contain_no_terminal_controls_or_bidi_marks() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.status = "status\u{1b}]0;title\u{7}\u{202e}\u{200b}\tline\nnext".into();
        app.input
            .set("input\u{1b}[31m\u{2066}\u{200d}\nwrapped".into());
        let mut renderer = Renderer::default();

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| renderer.draw(frame, &app)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(!text.chars().any(crate::text::is_unsafe_terminal_char));
        assert!(!text.contains('\t'));
        assert!(text.contains("status]0;title"));
        assert!(text.contains("input[31m"));
    }

    #[test]
    fn input_height_uses_inner_width_at_wrap_boundary() {
        let width = 30;
        let exact = "a".repeat((width - 4) as usize * 3);
        let overflow = "a".repeat((width - 4) as usize * 3 + 1);

        let mut exact_app = App::new(&Config::default(), Selection::default());
        exact_app.input.set(exact);
        let exact_screen = screen(&mut Renderer::default(), &exact_app, width, 24);
        let exact_row = exact_screen
            .lines()
            .position(|line| line.contains(" Input "))
            .unwrap();

        let mut overflow_app = App::new(&Config::default(), Selection::default());
        overflow_app.input.set(overflow);
        let overflow_screen = screen(&mut Renderer::default(), &overflow_app, width, 24);
        let overflow_row = overflow_screen
            .lines()
            .position(|line| line.contains(" Input "))
            .unwrap();

        assert_eq!(exact_row, overflow_row + 1);
    }

    #[test]
    fn chat_wraps_between_words() {
        let lines = wrap_lines_at_words(vec![Line::raw("one two three")], 7);
        let text = lines.iter().map(Line::to_string).collect::<Vec<_>>();
        assert_eq!(text, ["one two", "three"]);
    }

    // ---------------------------------------------------------------------
    // Structural-equivalence coverage for the linear wrap path. These
    // tests pin the exact output of `wrap_lines_at_words` across the
    // tricky shapes the old quadratic implementation had to handle:
    // whitespace boundaries (including tabs), wide/Unicode display
    // widths, styled spans, very long unbroken lines, and rows that fit
    // exactly at the column boundary. Each test asserts both the textual
    // content and the per-row display width so the Wave 1 refactor cannot
    // silently regress any of them.
    // ---------------------------------------------------------------------
    fn wrap_text(lines: &[Line<'static>], width: usize) -> Vec<String> {
        wrap_lines_at_words(lines.to_vec(), width)
            .into_iter()
            .map(|line| line.to_string())
            .collect()
    }

    #[test]
    fn wrap_collapses_runs_of_spaces_and_drops_leading_whitespace_per_row() {
        // Many spaces between words: each row should trim its trailing
        // whitespace and the wrap should drop the leading whitespace of
        // the next row, exactly like the old front-drain implementation.
        // The break point is the last whitespace *before* `width` cells,
        // so 4 spaces in the middle of a width-7 line break the row at
        // position 7 and emit just "aaa"; the long stretch of 5 spaces
        // after "bbbb" then breaks again to give a single "bbbb".
        let lines = wrap_text(&[Line::raw("aaa    bbbb     cc d")], 7);
        assert_eq!(lines, ["aaa", "bbbb", "cc d"]);
        assert!(lines.iter().all(|row| row.width() <= 7));
    }

    #[test]
    fn wrap_substitutes_tabs_with_single_spaces_before_breaking() {
        // Tabs become single spaces (see `wrap_lines_at_words`), so they
        // never show up in the output and never act as wide characters.
        // The wrap must still trim trailing whitespace from each row.
        let lines = wrap_text(&[Line::raw("foo\tbar\tbaz qux")], 7);
        assert_eq!(lines, ["foo bar", "baz qux"]);
        assert!(lines.iter().all(|row| row.width() <= 7));
        // No literal tab should ever appear in the output.
        assert!(lines.iter().all(|row| !row.contains('\t')));
    }

    #[test]
    fn wrap_breaks_at_spaces_inside_long_word_free_text() {
        // The whole input is one long word; we expect a hard break every
        // `width` cells with no whitespace to align against. This is the
        // case Wave 1 made linear.
        let text = "a".repeat(25);
        let lines = wrap_text(&[Line::raw(text)], 7);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].chars().count(), 7);
        assert_eq!(lines[1].chars().count(), 7);
        assert_eq!(lines[2].chars().count(), 7);
        assert_eq!(lines[3].chars().count(), 4);
        assert!(lines.iter().all(|row| row.width() <= 7));
        // Total chars preserved.
        assert_eq!(
            lines.iter().map(|row| row.chars().count()).sum::<usize>(),
            25
        );
    }

    #[test]
    fn wrap_respects_unicode_display_width_for_wide_glyphs() {
        // CJK characters occupy two cells each. Eight CJK characters are
        // 16 cells total. width=6 fits three CJK characters per row
        // (6 cells), leaving two wide glyphs (4 cells) for the trailing
        // row. The wrap must use cell widths, not char counts.
        let lines = wrap_text(&[Line::raw("漢字漢字漢字漢字")], 6);
        assert_eq!(lines.len(), 3);
        // Rows 0 and 1: three wide chars each, 6 cells exactly.
        assert_eq!(lines[0].chars().count(), 3);
        assert_eq!(lines[0].width(), 6);
        assert_eq!(lines[1].chars().count(), 3);
        assert_eq!(lines[1].width(), 6);
        // Row 2: the trailing pair, 4 cells.
        assert_eq!(lines[2].chars().count(), 2);
        assert_eq!(lines[2].width(), 4);
        assert!(lines.iter().all(|row| row.width() <= 6));
        // Total chars preserved across rows.
        assert_eq!(
            lines.iter().map(|row| row.chars().count()).sum::<usize>(),
            8
        );
    }

    #[test]
    fn wrap_keeps_wide_glyphs_intact_when_one_already_overflows_the_row() {
        // A single wide glyph (2 cells) at width=1 must still produce a
        // non-empty row, and the wrap must advance past it cleanly. The
        // display width of the wide row is 2 cells — that is exactly the
        // behaviour the old quadratic implementation had: it never
        // refused to put an over-wide glyph in a row, so the row's cell
        // width can exceed the column count. ASCII rows on either side
        // stay at 1 cell.
        let lines = wrap_text(&[Line::raw("漢a漢b漢")], 1);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "漢");
        assert_eq!(lines[1], "a");
        assert_eq!(lines[2], "漢");
        assert_eq!(lines[3], "b");
        assert_eq!(lines[4], "漢");
        // ASCII rows must be at most 1 cell; wide rows are exactly 2
        // cells (the documented over-wide single-glyph behaviour).
        assert_eq!(lines[1].width(), 1);
        assert_eq!(lines[3].width(), 1);
        assert_eq!(lines[0].width(), 2);
        assert_eq!(lines[2].width(), 2);
        assert_eq!(lines[4].width(), 2);
    }

    #[test]
    fn wrap_preserves_styles_within_a_row_and_across_rows() {
        // Three different foreground colors across the line; the wrap
        // must keep the red run on the first row, keep each row's color
        // order intact, and never drop a span. We pick a width that
        // forces a wrap inside the green span so we can verify both the
        // "row opens with green" and "row closes with green" semantics.
        let red = Style::default().fg(Color::Red);
        let green = Style::default().fg(Color::Green);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("red one two ", red),
            Span::styled("green three ", green),
            Span::styled("blue four", blue),
        ]);
        let wrapped = wrap_lines_at_words(vec![line], 10);
        // Per the trace: the wrap breaks on the last whitespace before
        // `width=10`. Row 0 = "red one" (red), row 1 = "two green"
        // (red + green, opens with red and ends with green because the
        // break landed on the green span's trailing space),
        // row 2 = "three blue" (green + blue), row 3 = "four" (blue).
        assert_eq!(wrapped.len(), 4);
        assert_eq!(wrapped[0].to_string(), "red one");
        assert_eq!(wrapped[1].to_string(), "two green");
        assert_eq!(wrapped[2].to_string(), "three blue");
        assert_eq!(wrapped[3].to_string(), "four");
        assert!(wrapped.iter().all(|row| row.width() <= 10));
        // Color order on each row is the order in which spans appear.
        let row0_fg: Vec<_> = wrapped[0].spans.iter().filter_map(|s| s.style.fg).collect();
        assert_eq!(row0_fg, vec![Color::Red]);
        // Row 1 must open with red and contain green somewhere after it.
        assert_eq!(wrapped[1].spans[0].style.fg, Some(Color::Red));
        assert!(wrapped[1]
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::Green)));
        // Row 2 must open with green and contain blue somewhere after it.
        assert_eq!(wrapped[2].spans[0].style.fg, Some(Color::Green));
        assert!(wrapped[2]
            .spans
            .iter()
            .any(|s| s.style.fg == Some(Color::Blue)));
        // Row 3 is the trailing blue fragment.
        assert_eq!(wrapped[3].spans[0].style.fg, Some(Color::Blue));
    }

    #[test]
    fn wrap_handles_width_boundaries_with_no_partial_words() {
        // width=5, three short words of length 2 separated by spaces:
        // "ab cd ef". The fit loop fills "ab cd" exactly (5 cells, last
        // char a space), then "cd" is treated as the wrap anchor and
        // "ef" starts the next row. The next iteration must not strand a
        // leading space in front of "ef".
        let lines = wrap_text(&[Line::raw("ab cd ef")], 5);
        assert_eq!(lines, ["ab cd", "ef"]);
        assert!(lines.iter().all(|row| row.width() <= 5));
    }

    #[test]
    fn wrap_handles_a_line_that_exactly_fits_the_width() {
        // The input is exactly `width` cells of non-whitespace, so the
        // whole input fits in a single row without a wrap.
        let lines = wrap_text(&[Line::raw("abcdefghij")], 10);
        assert_eq!(lines, ["abcdefghij"]);
        // One row, exactly at the boundary, no clipping.
        let wrapped = wrap_lines_at_words(vec![Line::raw("abcdefghij")], 10);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].width(), 10);
    }

    #[test]
    fn ascii_fast_path_handles_exact_fit_width_plus_one_and_width_one() {
        let exact = wrap_ascii_run(&Line::raw("abcdefghij"), 10).unwrap();
        let over = wrap_ascii_run(&Line::raw("abcdefghijk"), 10).unwrap();
        let narrow = wrap_ascii_run(&Line::raw("abcd"), 1).unwrap();

        assert_eq!(
            exact.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["abcdefghij"]
        );
        assert_eq!(
            over.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["abcdefghij", "k"]
        );
        assert_eq!(
            narrow.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn ascii_fast_path_preserves_line_and_span_styles_after_patching() {
        let line_style = Style::default()
            .bg(Color::Blue)
            .add_modifier(Modifier::BOLD);
        let span_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::ITALIC);
        let line = Line::from(Span::styled("abcdef", span_style)).style(line_style);

        let wrapped = wrap_ascii_run(&line, 3).unwrap();

        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped[0].to_string(), "abc");
        assert_eq!(wrapped[1].to_string(), "def");
        for row in wrapped {
            assert_eq!(row.style, Style::default());
            assert_eq!(row.spans.len(), 1);
            let patched = row.spans[0].style;
            assert_eq!(patched, line_style.patch(span_style));
            assert_eq!(patched.bg, Some(Color::Blue));
            assert_eq!(patched.fg, Some(Color::Yellow));
            assert!(patched.add_modifier.contains(Modifier::BOLD));
            assert!(patched.add_modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn ascii_fast_path_matches_general_wrapper_for_printable_runs() {
        for (text, width) in [
            ("A1-=[]{}", 3),
            ("0123456789", 4),
            ("xyz", 20),
            ("QWERTY", 1),
        ] {
            let line = Line::raw(text);
            let fast = wrap_lines_at_words(vec![line.clone()], width);
            let general = wrap_lines(vec![line], width);

            assert_eq!(fast, general, "text={text:?}, width={width}");
        }
    }

    #[test]
    fn non_printable_ascii_and_non_single_span_lines_use_general_semantics() {
        let whitespace = Line::raw("ab cd");
        let tab = Line::raw("ab\tcd");
        let control = Line::raw("ab\u{0007}cd");
        let newline = Line::raw("ab\ncd");
        let unicode = Line::raw("漢字漢字");
        let multi_span = Line::from(vec![Span::raw("ab"), Span::raw("cd")]);
        let empty = Line::raw("");

        assert!(wrap_ascii_run(&whitespace, 3).is_none());
        assert!(wrap_ascii_run(&tab, 3).is_none());
        assert!(wrap_ascii_run(&control, 3).is_none());
        assert!(wrap_ascii_run(&newline, 3).is_none());
        assert!(wrap_ascii_run(&unicode, 3).is_none());
        assert!(wrap_ascii_run(&multi_span, 3).is_none());
        assert!(wrap_ascii_run(&empty, 3).is_none());

        assert_eq!(wrap_text(&[whitespace], 3), ["ab", "cd"]);
        assert_eq!(wrap_text(&[tab], 3), ["ab", "cd"]);
        assert_eq!(wrap_text(&[control], 3), ["abc", "d"]);
        assert_eq!(wrap_text(&[newline], 3), ["abc", "d"]);
        assert_eq!(wrap_text(&[unicode], 3), ["漢", "字", "漢", "字"]);
        assert_eq!(wrap_text(&[multi_span], 3), ["abc", "d"]);
        assert_eq!(wrap_text(&[empty], 3), [""]);
    }

    #[test]
    fn ascii_fast_path_wraps_a_large_input_without_loss() {
        let text = "p".repeat(1 << 20);
        let wrapped = wrap_ascii_run(&Line::raw(text), 100).unwrap();

        assert_eq!(wrapped.len(), (1usize << 20).div_ceil(100));
        assert!(wrapped.iter().all(|line| line.width() <= 100));
        assert_eq!(
            wrapped
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.len())
                .sum::<usize>(),
            1 << 20
        );
    }

    #[test]
    fn wrap_drops_trailing_whitespace_even_when_followed_by_more_text() {
        // Trailing whitespace at the wrap point must be trimmed from
        // the emitted row, even when the next character is not itself
        // whitespace. The next row picks up at the next non-space char.
        let lines = wrap_text(&[Line::raw("hello world   foo")], 7);
        assert_eq!(lines, ["hello", "world", "foo"]);
        assert!(lines.iter().all(|row| row.width() <= 7));
    }

    #[test]
    fn wrap_keeps_empty_input_as_an_empty_line() {
        // Empty input still produces one row, matching the original
        // behaviour where an empty `characters` Vec pushed a default
        // `Line`. Callers that rely on a 1:1 mapping between input
        // and output lines (one per newline-separated block) keep it.
        let lines = wrap_text(&[Line::raw("")], 10);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "");
    }

    #[test]
    fn wrap_keeps_one_mib_unbroken_output_exactly() {
        // 1 MiB of 'a' characters, no whitespace, no newlines. The wrap
        // must cover the entire input exactly once across every row,
        // and every row must fit within the column width. This is the
        // structural assertion paired with the perf harness: same shape,
        // no semantic drift after the Wave 1 refactor.
        let text = "a".repeat(1 << 20);
        let wrapped = wrap_lines_at_words(vec![Line::raw(text)], 100);
        assert!(wrapped.len() >= (1 << 20) / 100);
        assert!(wrapped.iter().all(|line| line.width() <= 100));
        let total_chars: usize = wrapped
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>()
            })
            .sum();
        assert_eq!(total_chars, 1 << 20);
    }

    #[test]
    fn wrapped_chat_lines_keep_the_message_gutter() {
        let entry = Entry {
            role: "assistant".into(),
            context: "main".into(),
            text: "one two three".into(),
            revision: 0,
            streaming: false,
            activity_id: None,
        };
        let lines = render_entry(&entry, 10, &Theme::default());
        assert!(lines[1].to_string().starts_with("│ "));
        assert!(lines[2].to_string().starts_with("│ "));
    }

    #[test]
    fn active_stream_render_is_plain_bounded_and_utf8_safe() {
        let prefix = "old-prefix-that-must-not-be-rendered ";
        let mut text = prefix.to_owned();
        text.push_str(&"x漢".repeat((8 * 1024) / 4 + 100));
        text.push_str("\n```rust\nfn final_answer() {}\n```\nstream-tail");
        let entry = Entry {
            role: "assistant".into(),
            context: "main".into(),
            text: text.clone(),
            revision: 7,
            streaming: true,
            activity_id: None,
        };

        let lines = render_streaming_entry(&entry, 80, &Theme::default());
        let rendered = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let marker = "[streaming output truncated:";

        assert_eq!(entry.text, text);
        assert_eq!(rendered.matches(marker).count(), 1);
        assert!(!rendered.contains(prefix));
        assert!(rendered.contains("stream-tail"));
        assert!(rendered.contains("```rust"));
        assert!(
            lines.len() < 140,
            "streaming line count was {}",
            lines.len()
        );
        let content_bytes: usize = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.len())
            .sum();
        assert!(
            content_bytes <= 8 * 1024 + marker.len() + 600,
            "streaming output was {content_bytes} bytes"
        );

        let role_color = color(&Theme::default().assistant);
        let mut body_spans = lines
            .iter()
            .skip(1)
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains("```") || span.content.contains("fn final"));
        assert!(body_spans.clone().count() > 0);
        assert!(body_spans.all(|span| span.style.fg == Some(role_color)));
    }

    #[test]
    fn active_stream_render_keeps_multiline_text_on_separate_rows() {
        let entry = Entry {
            role: "assistant".into(),
            context: "main".into(),
            text: "first logical line\nsecond logical line\nthird logical line".into(),
            revision: 1,
            streaming: true,
            activity_id: None,
        };

        let lines = render_streaming_entry(&entry, 80, &Theme::default());
        let body = lines[1..lines.len() - 1]
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        assert_eq!(
            body,
            [
                "│ first logical line",
                "│ second logical line",
                "│ third logical line",
            ]
        );
    }

    fn rendered_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn completed_entry(text: String) -> Entry {
        Entry {
            role: "assistant".into(),
            context: "main".into(),
            text,
            revision: 1,
            streaming: false,
            activity_id: None,
        }
    }

    #[test]
    fn completed_entry_at_byte_limit_remains_rich() {
        let mut text = "```rust\nfn main() { let value = 42; }\n".to_owned();
        let padding = COMPLETED_RENDER_BYTES - text.len() - 4;
        text.push_str(&"plain ".repeat(padding / 6));
        text.push_str(&"x".repeat(padding % 6));
        text.truncate(COMPLETED_RENDER_BYTES - 4);
        text.push_str("```\n");
        assert_eq!(text.len(), COMPLETED_RENDER_BYTES);

        let lines = render_entry(&completed_entry(text), 100, &Theme::default());
        let output = rendered_text(&lines);

        assert!(!output.contains("[display truncated:"));
        assert!(output.contains("```rust"));
        assert!(output.contains("fn main"));
    }

    #[test]
    fn completed_entry_at_line_limit_remains_rich() {
        let mut text = String::from("# rich boundary\n");
        text.push_str(&"ordinary line\n".repeat(COMPLETED_RENDER_LINES - 2));
        text.push_str("last line");
        assert_eq!(logical_line_count(&text), COMPLETED_RENDER_LINES);
        assert!(text.len() < COMPLETED_RENDER_BYTES);

        let lines = render_entry(&completed_entry(text), 100, &Theme::default());
        let output = rendered_text(&lines);

        assert!(!output.contains("[display truncated:"));
        assert!(lines.iter().any(|line| {
            line.spans.iter().any(|span| {
                span.content.contains("rich boundary")
                    && span.style.add_modifier.contains(Modifier::BOLD)
            })
        }));
    }

    #[test]
    fn oversized_completed_entry_by_bytes_is_utf8_safe_plain_and_bounded() {
        let head = "HEAD漢".repeat(14_000);
        let text = format!("{head}BYTE_MIDDLE_SENTINEL{}TAILΩ", "x".repeat(70_000));
        assert!(text.len() > COMPLETED_RENDER_BYTES);
        let entry = completed_entry(text);
        let original = entry.text.clone();

        let lines = render_entry(&entry, 120, &Theme::default());
        let output = rendered_text(&lines);
        let marker = "[display truncated:";

        assert_eq!(
            lines
                .iter()
                .filter(|line| line.to_string().contains(marker))
                .count(),
            1
        );
        assert!(output.contains("HEAD漢"));
        assert!(output.contains("TAILΩ"));
        assert!(!output.contains("BYTE_MIDDLE_SENTINEL"));
        assert!(output.is_char_boundary(output.len()));
        assert!(
            lines.len() < 1_200,
            "bounded output had {} lines",
            lines.len()
        );
        let body_bytes: usize = lines
            .iter()
            .skip(2)
            .take(lines.len().saturating_sub(3))
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.as_ref() != "│ ")
            .map(|span| span.content.len())
            .sum();
        assert!(body_bytes <= COMPLETED_RENDER_BYTES);
        let role_color = color(&Theme::default().assistant);
        assert!(lines[2..lines.len() - 1]
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.as_ref() != "│ ")
            .all(|span| span.style == Style::default().fg(role_color)));
        assert_eq!(entry.text, original);
    }

    #[test]
    fn oversized_completed_entry_by_lines_omits_middle_and_stays_bounded() {
        let sentinel = "LINE_MIDDLE_SENTINEL";
        let mut logical_lines = Vec::new();
        logical_lines
            .extend((0..COMPLETED_HEAD_LINES).map(|index| format!("HEAD_PADDING_{index:02}_漢")));
        logical_lines.extend(std::iter::repeat_n(
            sentinel.to_owned(),
            COMPLETED_RENDER_LINES + 100,
        ));
        logical_lines
            .extend((0..COMPLETED_TAIL_LINES).map(|index| format!("TAIL_PADDING_{index:02}_Ω")));
        let text = logical_lines.join("\n");
        assert!(logical_line_count(&text) > COMPLETED_RENDER_LINES);
        assert!(text.len() < COMPLETED_RENDER_BYTES);
        let entry = completed_entry(text);
        let original = entry.text.clone();

        let lines = render_entry(&entry, 120, &Theme::default());
        let output = rendered_text(&lines);

        assert_eq!(output.matches("[display truncated:").count(), 1);
        assert!(output.contains("HEAD_PADDING_00_漢"));
        assert!(output.contains("HEAD_PADDING_63_漢"));
        assert!(output.contains("TAIL_PADDING_00_Ω"));
        assert!(output.contains("TAIL_PADDING_63_Ω"));
        assert!(!output.contains(sentinel));
        assert!(
            lines.len() < 1_200,
            "bounded output had {} lines",
            lines.len()
        );
        assert!(output.is_char_boundary(output.len()));
        assert_eq!(entry.text, original);
    }

    #[test]
    fn oversized_expanded_tool_result_is_bounded_without_mutating_raw_json() {
        let mut payload = vec![serde_json::json!({"a": "TOOL_HEAD_SENTINEL"})];
        payload.extend((0..9_000).map(|_| serde_json::json!({"a": "x"})));
        payload.insert(
            payload.len() / 2,
            serde_json::json!({"a": "TOOL_MIDDLE_SENTINEL"}),
        );
        payload.push(serde_json::json!({"a": "TOOL_TAIL_SENTINEL"}));
        let raw = serde_json::json!({"content": payload}).to_string();
        assert!(raw.len() < 100_000);

        let expanded = tool_result_text(&raw);
        assert!(expanded.len() > COMPLETED_RENDER_BYTES);
        assert!(logical_line_count(&expanded) > COMPLETED_RENDER_LINES);

        let entry = Entry {
            role: "tool".into(),
            context: "main".into(),
            text: raw.clone(),
            revision: 1,
            streaming: false,
            activity_id: None,
        };
        let lines = render_entry(&entry, 120, &Theme::default());
        let output = rendered_text(&lines);

        assert_eq!(output.matches("[display truncated:").count(), 1);
        assert!(output.contains("TOOL_HEAD_SENTINEL"));
        assert!(output.contains("TOOL_TAIL_SENTINEL"));
        assert!(!output.contains("TOOL_MIDDLE_SENTINEL"));
        assert_eq!(entry.text, raw);
    }

    #[test]
    fn small_tool_result_keeps_rich_pretty_rendering() {
        let entry = Entry {
            role: "tool".into(),
            context: "main".into(),
            text: r#"{"content":{"language":"rust","code":"fn main() {}"}}"#.into(),
            revision: 1,
            streaming: false,
            activity_id: None,
        };

        let output = rendered_text(&render_entry(&entry, 120, &Theme::default()));

        assert!(output.contains("```json"));
        assert!(output.contains("\"language\": \"rust\""));
        assert!(!output.contains("[display truncated:"));
    }

    #[test]
    #[ignore = "release-only render performance harness"]
    fn oversized_completed_entry_release_render_harness() {
        let text = "render-harness漢".repeat((2 * 1024 * 1024) / "render-harness漢".len() + 1);
        let entry = completed_entry(text);
        let started = std::time::Instant::now();
        let lines = render_entry(&entry, 120, &Theme::default());
        eprintln!(
            "oversized completed-entry render: {:?}, {} lines",
            started.elapsed(),
            lines.len()
        );
    }

    #[test]
    fn final_message_switches_active_entry_to_rich_render_once() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.event(UiEvent::Delta {
            context: "main".into(),
            text: "```rust\nfn main() { let n = 42; }\n```".into(),
        });
        let mut renderer = Renderer::default();
        screen(&mut renderer, &app, 100, 24);
        let streaming_rebuilds = renderer.rebuilds;
        assert!(app.entries[0].streaming);
        let streaming_colors: std::collections::HashSet<_> = renderer.cache[0]
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter_map(|span| span.style.fg)
            .collect();
        assert!(streaming_colors.len() <= 2);

        app.event(UiEvent::Message {
            context: "main".into(),
            message: Message::new("assistant", "```rust\nfn main() { let n = 42; }\n```"),
        });
        assert!(!app.entries[0].streaming);
        screen(&mut renderer, &app, 100, 24);
        assert_eq!(renderer.rebuilds, streaming_rebuilds + 1);
        let rich_colors: std::collections::HashSet<_> = renderer.cache[0]
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter_map(|span| span.style.fg)
            .collect();
        assert!(rich_colors.len() >= 3);
        let rich_rebuilds = renderer.rebuilds;
        screen(&mut renderer, &app, 100, 24);
        assert_eq!(renderer.rebuilds, rich_rebuilds);
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

    /// Idle rows for the launch-time variant must equal the canonical
    /// rest pose. The renderer's `draw_kitty` paints rows in canvas
    /// order starting at terminal row 0 (`area.y`), so the
    /// body rows sit at the same offsets every variant gets. The helper
    /// that builds the rows must not be painting or hiding body cells
    /// based on busy state — busy only animates the moving segment.
    #[test]
    fn kitty_idle_body_rows_match_the_unmodified_asset_for_every_variant() {
        let theme = Theme::default();
        for variant in kitty::VARIANTS {
            let idle = kitty_rows(variant, false, 0, &theme);
            let canonical = kitty::render_idle(variant, &theme);
            for (index, (painted, expected)) in idle.iter().zip(canonical.iter()).enumerate() {
                assert_eq!(
                    painted.to_string(),
                    expected.to_string(),
                    "variant {variant:?} idle row {index} must equal the asset; got {painted:?}, expected {expected:?}",
                );
            }
        }
    }

    /// Processing rows must keep the body cells that idle shows. Per
    /// variant, the *face* is the part of the canvas that never moves;
    /// the *animated segment* is the only part that differs between
    /// idle and processing. The face is on the left side of each row
    /// (the variant's eyes / nose / mouth / belly), so we pin the
    /// leftmost characters of selected rows that the animation never
    /// touches. The full-row invariants are covered separately by the
    /// kitty unit tests inside `src/tui/kitty.rs`.
    #[test]
    fn kitty_processing_keeps_every_variant_face_identical_to_idle() {
        struct FacePiece {
            label: &'static str,
            variant: KittyVariant,
            frame_count: usize,
            rows: &'static [(usize, usize)],
        }
        // (label, variant, frame_count, [row, face_cols]) where
        // `face_cols` is the column where the animation starts (i.e.
        // the prefix length that must stay pixel-equal between idle
        // and processing).
        let face_pieces: &[FacePiece] = &[
            FacePiece {
                // Blob's head sits on row 1 (cols 0-13); the belly
                // sits on row 4 (cols 0-12). The animation only
                // overwrites the leftmost two columns of rows 2 and 3,
                // never rows 1 or 4.
                label: "Blob",
                variant: KittyVariant::Blob,
                frame_count: 4,
                rows: &[(1, 14), (4, 13)],
            },
            FacePiece {
                // Cbear's eyes / ears are on row 1 cols 0-6
                // (`∩ ∩   `); the body frame extends across rows 2-3
                // cols 0-4 (and row 4 cols 0-4 for the legs). Frame 1
                // overwrites row 1 col 7+, row 2 cols 6+, row 3
                // cols 5+, row 4 cols 5+, never the leftmost columns
                // we pin.
                label: "Cbear",
                variant: KittyVariant::Cbear,
                frame_count: 3,
                rows: &[(1, 7), (2, 6), (3, 5), (4, 5)],
            },
            FacePiece {
                // Fly Girl's body, ears, and base are on rows 2-5;
                // the free-floating Z on row 1 is the only animated
                // cell and moves to row 0 in frame 1, so rows 2-5 are
                // pixel-equal in every frame.
                label: "FlyGirl",
                variant: KittyVariant::FlyGirl,
                frame_count: 3,
                rows: &[(2, 8), (3, 8), (4, 8), (5, 7)],
            },
        ];
        // Pad a `Line`'s rendered text with trailing spaces so the
        // comparison can compare fixed-width prefixes. `Line::to_string`
        // trims trailing whitespace, which makes idle/processing
        // strings unequal-length even when their face cells are
        // identical. Pad to at least `width` characters.
        fn padded(line: &Line<'_>, width: usize) -> String {
            let mut rendered: String = line.to_string();
            while rendered.chars().count() < width {
                rendered.push(' ');
            }
            rendered
        }
        for piece in face_pieces {
            let idle = kitty_rows(piece.variant, false, 0, &Theme::default());
            for frame in 0..piece.frame_count {
                let processing = kitty_rows(piece.variant, true, frame, &Theme::default());
                for (row, face_cols) in piece.rows {
                    let prefix = *face_cols;
                    let idle_line = padded(&idle[*row], prefix);
                    let processing_line = padded(&processing[*row], prefix);
                    assert_eq!(
                        processing_line.chars().take(prefix).collect::<String>(),
                        idle_line.chars().take(prefix).collect::<String>(),
                        "{} processing frame {frame} shifted the face cells on row {row} (cols 0..={prefix})",
                        piece.label,
                    );
                }
            }
        }
    }

    /// `draw_kitty` anchors canvas row 0 to terminal row 0, not to the chat
    /// band, so the artwork's padded first glyph row lands on terminal row 1 and
    /// the body extends down over the chat. The canvas is clamped to the rows
    /// above the input region. Pixel-pin this contract: the previous
    /// implementation anchored to `chat_area.y`, which is what this test's
    /// assertions originally guarded against; they now pin the terminal-top
    /// anchor instead.
    #[test]
    fn kitty_anchor_is_exact_for_every_variant_and_terminal_size() {
        // Drive the screen through the regular `screen` helper so the
        // header + kitty + chat layout are all exercised end-to-end.
        let app = App::new(&Config::default(), Selection::default());
        for (offset, variant) in kitty::VARIANTS.into_iter().enumerate() {
            for (width, height) in [(80, 24), (100, 30), (60, 20)] {
                let mut renderer = Renderer::default();
                renderer.set_variant_offset(offset);
                let _ = screen(&mut renderer, &app, width, height);
                let painted = &renderer.last_kitty_paint;
                assert!(!painted.is_empty());
                assert_eq!(painted.iter().map(|&(y, _, _)| y).min(), Some(1));
                assert_eq!(
                    painted.iter().map(|&(_, _, end)| end - 1).max(),
                    Some(width - 2)
                );
                let kitty_width = kitty_rows(variant, false, 0, &Theme::default())
                    .iter()
                    .map(Line::width)
                    .max()
                    .unwrap() as u16;
                assert!(painted.iter().all(|&(y, start, end)| {
                    y >= 1 && start >= width - 1 - kitty_width && end < width
                }));
            }
        }
    }

    #[test]
    fn kitty_processing_frames_keep_the_idle_anchor() {
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
        let mut renderer = Renderer::default();
        screen(&mut renderer, &app, 80, 24);
        assert_eq!(
            renderer.last_kitty_paint.iter().map(|&(y, _, _)| y).min(),
            Some(1)
        );
        assert_eq!(
            renderer
                .last_kitty_paint
                .iter()
                .map(|&(_, _, end)| end - 1)
                .max(),
            Some(78)
        );

        // Fly Girl's processing frame 1 moves the free-floating Z into canvas
        // row 0. Keep the animation on terminal row 0 and inside the
        // reserved kitty column rather than allowing it into the header text.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let task = runtime
            .handle()
            .spawn(async { std::future::pending::<Result<String, anyhow::Error>>().await });
        drop(runtime);
        let mut fly_app = App::new(&Config::default(), Selection::default());
        fly_app.busy = Some(Busy {
            task,
            cancel: tokio_util::sync::CancellationToken::new(),
        });
        let mut idle = Renderer::default();
        idle.set_variant_offset(
            kitty::VARIANTS
                .iter()
                .position(|variant| *variant == KittyVariant::FlyGirl)
                .unwrap(),
        );
        let idle_app = App::new(&Config::default(), Selection::default());
        screen(&mut idle, &idle_app, 80, 24);
        let idle_paint = idle.last_kitty_paint.clone();

        let mut animated = Renderer::default();
        animated.set_variant_offset(
            kitty::VARIANTS
                .iter()
                .position(|variant| *variant == KittyVariant::FlyGirl)
                .unwrap(),
        );
        // `advance` treats a fresh renderer becoming busy as a new run and
        // resets the frame to zero. Seed the state as an already-running
        // animation so draw preserves the frame under test.
        animated.was_busy = true;
        animated.processing_frame = 1;
        animated.processing_tick = Some(Instant::now());
        screen(&mut animated, &fly_app, 80, 24);
        assert_eq!(
            animated.processing_frame, 1,
            "draw must preserve the seeded animated frame"
        );
        assert_ne!(
            animated.last_kitty_paint, idle_paint,
            "animated frame paint spans must differ from the Fly Girl rest pose"
        );

        let kitty_width = kitty_rows(KittyVariant::FlyGirl, true, 1, &Theme::default())
            .iter()
            .map(Line::width)
            .max()
            .unwrap() as u16;
        let content_right = 79;
        let kitty_left = content_right - kitty_width;
        let top_row = animated
            .last_kitty_paint
            .iter()
            .find(|&&(y, _, _)| y == 0)
            .copied()
            .expect("Fly Girl's rising Z must paint on terminal row 0");
        assert!(top_row.1 >= kitty_left);
        assert!(top_row.2 <= content_right);
        assert!(top_row.1 < top_row.2);
    }

    #[test]
    fn narrow_terminal_suppresses_kitty_but_keeps_composed_metadata() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.model_label = "provider:very-long-model".into();
        app.effective_agent_label = "writer".into();
        app.effort_label = "high".into();
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 40, 20);
        assert!(renderer.last_kitty_paint.is_empty());
        assert!(output.contains("effort high"), "{output:?}");
    }

    #[test]
    fn composed_metadata_tail_truncates_left_and_preserves_suffix() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.model_label = "openrouter:anthropic/claude-sonnet-4".into();
        app.effective_agent_label = "writer".into();
        app.effort_label = "default".into();

        let mut wide_renderer = Renderer::default();
        let wide_output = screen(&mut wide_renderer, &app, 100, 24);
        let wide_line = wide_output.lines().nth(1).unwrap();
        let wide_end = wide_renderer
            .last_kitty_reservation
            .map_or(wide_line.len(), |reservation| reservation.x as usize);
        let wide_line = wide_line.chars().take(wide_end).collect::<String>();
        let wide_line = wide_line.trim_end();
        assert!(!wide_line.contains('…'));
        assert!(wide_line.contains("openrouter:anthropic/claude-sonnet-4"));
        assert!(wide_line.ends_with("agent writer | effort default"));

        let mut narrow_renderer = Renderer::default();
        let narrow_output = screen(&mut narrow_renderer, &app, 80, 24);
        let narrow_line = narrow_output.lines().nth(1).unwrap();
        let narrow_end = narrow_renderer
            .last_kitty_reservation
            .map_or(narrow_line.len(), |reservation| reservation.x as usize);
        let narrow_line = narrow_line.chars().take(narrow_end).collect::<String>();
        let narrow_line = narrow_line.trim_end();
        assert!(narrow_line.contains('…'));
        assert!(narrow_line.contains("claude-sonnet-4"));
        assert!(narrow_line.ends_with("effort default"));
    }

    #[test]
    fn kitty_is_cleared_when_the_same_renderer_is_resized_narrow() {
        let app = App::new(&Config::default(), Selection::default());
        let mut renderer = Renderer::default();
        screen(&mut renderer, &app, 100, 24);
        assert!(!renderer.last_kitty_paint.is_empty());
        assert!(renderer.last_kitty_reservation.is_some());
        screen(&mut renderer, &app, 40, 18);
        assert!(renderer.last_kitty_paint.is_empty());
        assert!(renderer.last_kitty_reservation.is_none());
        assert_eq!(renderer.activity_at(78, 3), None);
    }

    #[test]
    fn history_divider_stops_before_unicode_kitty_reservation() {
        let app = App::new(&Config::default(), Selection::default());
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 80, 24);
        let history = renderer.last_history_rect.unwrap();
        let reservation = renderer.last_kitty_reservation.unwrap();
        let row = output.lines().nth((history.y - 1) as usize).unwrap();
        assert_eq!(row.chars().nth((history.x - 1) as usize), Some('╭'));
        for column in history.x..reservation.x {
            assert_eq!(row.chars().nth(column as usize), Some('─'));
        }
        assert!(!row
            .chars()
            .skip(reservation.x as usize)
            .any(|c| "╭─╮".contains(c)));
    }

    #[test]
    fn history_divider_uses_ascii_symbols() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.theme.ascii = true;
        for (offset, _) in kitty::VARIANTS.into_iter().enumerate() {
            let mut renderer = Renderer::default();
            renderer.set_variant_offset(offset);
            let output = screen(&mut renderer, &app, 80, 24);
            let history = renderer.last_history_rect.unwrap();
            let reservation = renderer.last_kitty_reservation.unwrap();
            let row = output.lines().nth((history.y - 1) as usize).unwrap();
            assert_eq!(row.chars().nth((history.x - 1) as usize), Some('+'));
            for column in history.x..reservation.x {
                assert_eq!(row.chars().nth(column as usize), Some('-'));
            }
        }

        // Cbear's artwork contains '-', so checking for ASCII divider symbols
        // after the reservation is meaningful only for the default Blob.
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 80, 24);
        let history = renderer.last_history_rect.unwrap();
        let reservation = renderer.last_kitty_reservation.unwrap();
        let row = output.lines().nth((history.y - 1) as usize).unwrap();
        assert!(!row
            .chars()
            .skip(reservation.x as usize)
            .any(|c| "+-".contains(c)));
    }

    #[test]
    fn suppressed_kitty_divider_closes_at_the_history_band_right_edge() {
        let app = App::new(&Config::default(), Selection::default());
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 40, 24);
        let history = renderer.last_history_rect.unwrap();
        let row = output.lines().nth((history.y - 1) as usize).unwrap();
        assert!(renderer.last_kitty_reservation.is_none());
        assert_eq!(row.chars().nth((history.x - 1) as usize), Some('╭'));
        assert_eq!(row.chars().nth(history.right() as usize), Some('╮'));
    }

    #[test]
    fn divider_never_paints_inside_any_kitty_reservation() {
        let app = App::new(&Config::default(), Selection::default());
        for (offset, _) in kitty::VARIANTS.into_iter().enumerate() {
            let mut renderer = Renderer::default();
            renderer.set_variant_offset(offset);
            let output = screen(&mut renderer, &app, 80, 24);
            let history = renderer.last_history_rect.unwrap();
            let reservation = renderer.last_kitty_reservation.unwrap();
            let row = output.lines().nth((history.y - 1) as usize).unwrap();
            assert!(!row
                .chars()
                .skip(reservation.x as usize)
                .any(|c| "╭─╮".contains(c)));
        }
    }

    #[test]
    fn divider_and_borders_are_not_activity_targets() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.event(UiEvent::Activity(activity_start("activity")));
        let mut renderer = Renderer::default();
        screen(&mut renderer, &app, 80, 24);
        let history = renderer.last_history_rect.unwrap();
        assert_eq!(renderer.activity_at(history.x - 1, history.y - 1), None);
        assert_eq!(renderer.activity_at(history.x - 1, history.y), None);
        assert_eq!(renderer.activity_at(history.right(), history.y), None);
        assert_eq!(
            renderer.activity_at(history.x, history.y),
            Some("activity".into())
        );
    }

    #[test]
    fn short_terminals_skip_empty_history_without_overwriting_input() {
        let app = App::new(&Config::default(), Selection::default());
        for height in 1..=8 {
            let mut renderer = Renderer::default();
            let _ = screen(&mut renderer, &app, 40, height);
            let area = Rect::new(0, 0, 40, height).inner(Margin {
                horizontal: 1,
                vertical: 1,
            });
            let input_height = 5.min(area.height.saturating_sub(INPUT_RESERVED_ROWS));
            let regions = Layout::vertical([
                Constraint::Length(HEADER_HEIGHT),
                Constraint::Min(1),
                Constraint::Length(input_height),
                Constraint::Length(2),
            ])
            .split(area);
            if height <= 2 {
                assert_eq!(renderer.last_history_rect, None);
                assert_eq!(regions[1].height, 0);
                let output = screen(&mut Renderer::default(), &app, 40, height);
                assert!(!output.contains('─'));
            } else {
                let history = renderer
                    .last_history_rect
                    .expect("non-empty history band should record its inner rect");
                assert_eq!(history.height, regions[1].height - 1);
                assert!(history.y - 1 < regions[2].y);

                if regions[2].height > 0 {
                    let output = screen(&mut Renderer::default(), &app, 40, height);
                    let input_top = output.lines().nth(regions[2].y as usize).unwrap();
                    assert_eq!(input_top.chars().nth(regions[2].x as usize), Some('╭'));
                }
            }
        }
    }

    #[test]
    fn idle_kitty_canvas_row_zero_is_blank_and_reservation_tracks_visibility() {
        let app = App::new(&Config::default(), Selection::default());
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 80, 24);
        let reservation = renderer.last_kitty_reservation.unwrap();
        assert!(!renderer
            .last_kitty_paint
            .iter()
            .any(|&(row, _, _)| row == 0));
        assert!(output
            .lines()
            .next()
            .unwrap()
            .chars()
            .skip(reservation.x as usize)
            .all(char::is_whitespace));
        screen(&mut renderer, &app, 40, 24);
        assert!(renderer.last_kitty_reservation.is_none());
    }

    #[test]
    fn old_header_format_is_not_rendered_and_new_header_uses_two_rows() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.model_label = "openai:gpt-5".into();
        app.effective_agent_label = "writer".into();
        app.effort_label = "high".into();
        app.spend.unpriced_requests = 1;
        app.context_tokens = 12;
        app.context_limit = 100;
        let mut renderer = Renderer::default();
        let output = screen(&mut renderer, &app, 120, 24);
        let rows: Vec<&str> = output.lines().collect();
        assert!(rows[1].contains("openai:gpt-5 | agent writer | effort high"));
        assert!(rows[2].contains("context 12/100"));
        assert!(!output.contains("agent: writer | model:"));
        // The inner width is 118. At 120 columns the 14-cell kitty reserves
        // 118 - 6 - 1 - 14 = 97 metadata cells; including the one-cell outer
        // margin and six-cell logo, the rendered line ends at column 104.
        let content_width = 120 - 2;
        let kitty_width = kitty_rows(KittyVariant::Blob, false, 0, &Theme::default())
            .iter()
            .map(Line::width)
            .max()
            .unwrap() as u16;
        let metadata_width =
            content_width - HEADER_LOGO_WIDTH - HEADER_SEPARATOR_WIDTH - kitty_width;
        let crop_end = renderer
            .last_kitty_reservation
            .map_or(rows[1].len(), |r| r.x as usize);
        let expected_trimmed_width = (1 + HEADER_LOGO_WIDTH + metadata_width) as usize;
        assert_eq!(rows[1][..crop_end].trim_end().len(), expected_trimmed_width);
        assert!(rows[2].contains("$"));
    }

    #[test]
    fn kitty_anchor_test_replaced_old_layout_contract() {
        let app = App::new(&Config::default(), Selection::default());
        let output = screen(&mut Renderer::default(), &app, 80, 24);
        let first_visible = output.lines().position(|line| line.contains('█')).unwrap();
        assert_eq!(first_visible, 1);
    }

    #[test]
    fn renderer_metadata_tracks_history_viewport_and_real_kitty_spans() {
        let mut app = App::new(&Config::default(), Selection::default());
        for index in 0..5 {
            app.message(
                "main".into(),
                Message::new("assistant", format!("history entry {index}")),
            );
        }
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| renderer.draw(frame, &app)).unwrap();

        let history = renderer.last_history_rect.expect("history rect recorded");
        assert_eq!(history, Rect::new(2, 4, 76, 12));
        assert_eq!(renderer.hit_map.len(), history.height as usize);
        assert!(renderer.hit_map.iter().all(Option::is_none));

        let mut hit_app = App::new(&Config::default(), Selection::default());
        hit_app.event(UiEvent::Activity(activity_start("activity")));
        terminal
            .draw(|frame| renderer.draw(frame, &hit_app))
            .unwrap();
        assert!(renderer.hit_map.iter().any(Option::is_some));

        assert!(!renderer.last_kitty_paint.is_empty());
        for &(_y, start, end) in &renderer.last_kitty_paint {
            assert!(start < end, "kitty span must contain a painted cell");
        }
        let overlap = renderer
            .last_kitty_paint
            .iter()
            .find(|&&(y, start, end)| {
                y >= history.y
                    && y < history.bottom()
                    && renderer
                        .hit_map
                        .get((y - history.y) as usize)
                        .is_some_and(Option::is_some)
                    && start < history.right()
                    && end > history.x
            })
            .copied()
            .expect("kitty must overlap history");
        assert_eq!(
            renderer.activity_at(overlap.1.max(history.x), overlap.0),
            None
        );
        let adjacent = (history.x..history.right())
            .find(|&column| {
                !renderer
                    .last_kitty_paint
                    .iter()
                    .any(|&(y, start, end)| y == overlap.0 && column >= start && column < end)
            })
            .expect("overlapped summary row needs a non-kitty cell");
        assert_eq!(
            renderer.activity_at(adjacent, overlap.0),
            Some("activity".into())
        );
    }

    fn activity_start(id: &str) -> ActivityEvent {
        ActivityEvent {
            id: id.into(),
            parent_id: None,
            context: "main".into(),
            kind: ActivityKind::Tool,
            phase: ActivityPhase::Start,
            title: format!("{id} title"),
            external_id: None,
            status: None,
        }
    }

    #[test]
    fn activity_at_resolves_summary_rows_but_not_entries_or_outside_history() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.event(UiEvent::Activity(activity_start("activity")));
        app.message("main".into(), Message::new("assistant", "entry body"));
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| renderer.draw(frame, &app)).unwrap();

        let history = renderer.last_history_rect.unwrap();
        let summary_row = (0..history.height)
            .find(|offset| renderer.hit_map[*offset as usize].is_some())
            .unwrap();
        let summary_column = history.x + 1;
        assert_eq!(
            renderer.activity_at(summary_column, history.y + summary_row),
            Some("activity".into())
        );

        let entry_row = (0..history.height)
            .find(|offset| renderer.hit_map[*offset as usize].is_none())
            .unwrap();
        assert_eq!(
            renderer.activity_at(summary_column, history.y + entry_row),
            None
        );
        assert_eq!(
            renderer.activity_at(history.x, history.y.saturating_sub(1)),
            None
        );
        assert_eq!(renderer.activity_at(history.right(), history.y), None);
    }

    #[test]
    fn activity_at_rejects_stale_metadata_and_painted_kitty_cells() {
        assert_eq!(Renderer::default().activity_at(0, 0), None);
        let mut app = App::new(&Config::default(), Selection::default());
        app.event(UiEvent::Activity(activity_start("activity")));
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| renderer.draw(frame, &app)).unwrap();
        let history = renderer.last_history_rect.unwrap();
        let summary_row = history.y;

        let painted = renderer
            .last_kitty_paint
            .iter()
            .find(|&&(row, _, _)| row == summary_row)
            .copied()
            .or_else(|| renderer.last_kitty_paint.first().copied())
            .unwrap();
        assert_eq!(renderer.activity_at(painted.1, painted.0), None);

        renderer.hit_map.clear();
        assert_eq!(renderer.activity_at(history.x + 1, summary_row), None);
        renderer.last_history_rect = None;
        assert_eq!(renderer.activity_at(history.x + 1, summary_row), None);
    }

    #[test]
    fn transparent_kitty_canvas_cell_resolves_underlying_summary() {
        let mut app = App::new(&Config::default(), Selection::default());
        app.event(UiEvent::Activity(activity_start("activity")));
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| renderer.draw(frame, &app)).unwrap();
        let history = renderer.last_history_rect.unwrap();
        let canvas_start = renderer.last_kitty_reservation.unwrap().x;
        let transparent_column = canvas_start;
        let transparent_row = history.y;
        assert!(transparent_column >= history.x && transparent_column < history.right());
        assert!(!renderer.last_kitty_paint.iter().any(|&(row, start, end)| {
            row == transparent_row && transparent_column >= start && transparent_column < end
        }));
        assert_eq!(
            renderer.activity_at(transparent_column, transparent_row),
            Some("activity".into())
        );
    }

    #[test]
    fn kitty_paint_metadata_excludes_canvas_padding_and_gutters() {
        let rect = Rect::new(20, 7, 30, kitty::CANVAS_HEIGHT as u16);
        let theme = Theme::default();
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| {
                renderer.draw_kitty(
                    frame,
                    rect,
                    &kitty_rows(KittyVariant::FlyGirl, false, 0, &theme),
                )
            })
            .unwrap();

        assert!(!renderer
            .last_kitty_paint
            .iter()
            .any(|&(y, _, _)| y == rect.y));
        assert!(!renderer
            .last_kitty_paint
            .iter()
            .any(|&(_, start, _)| start == rect.x));
        assert!(renderer
            .last_kitty_paint
            .iter()
            .all(|&(y, start, end)| y >= rect.y
                && y < rect.bottom()
                && start >= rect.x
                && end <= rect.right()
                && start < end));

        let kitty_width = kitty_rows(KittyVariant::FlyGirl, false, 0, &theme)
            .iter()
            .map(Line::width)
            .max()
            .unwrap() as u16;
        let real_glyph = (rect.right() - kitty_width + 2, rect.y + 2);
        let transparent_cell = (rect.x, rect.y);
        assert!(renderer.last_kitty_paint.iter().any(|&(y, start, end)| {
            y == real_glyph.1 && real_glyph.0 >= start && real_glyph.0 < end
        }));
        assert!(!renderer.last_kitty_paint.iter().any(|&(y, start, end)| {
            y == transparent_cell.1 && transparent_cell.0 >= start && transparent_cell.0 < end
        }));
    }

    #[test]
    fn zero_size_kitty_draw_clears_previous_paint_metadata() {
        let theme = Theme::default();
        let rows = kitty_rows(KittyVariant::Blob, false, 0, &theme);
        let mut renderer = Renderer::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| renderer.draw_kitty(frame, Rect::new(20, 7, 30, 6), &rows))
            .unwrap();
        assert!(!renderer.last_kitty_paint.is_empty());

        terminal
            .draw(|frame| renderer.draw_kitty(frame, Rect::new(20, 7, 0, 6), &rows))
            .unwrap();
        assert!(renderer.last_kitty_paint.is_empty());
    }

    #[test]
    fn kitty_variants_keep_static_coordinates_while_animation_moves_only_its_segment() {
        fn glyphs(rows: &[Line<'static>]) -> std::collections::BTreeSet<(usize, usize)> {
            let mut painted = std::collections::BTreeSet::new();
            for (row, line) in rows.iter().enumerate() {
                let mut column = 0;
                for span in &line.spans {
                    for character in span.content.chars() {
                        let cells = UnicodeWidthChar::width(character).unwrap_or(0);
                        if !character.is_whitespace() {
                            painted.insert((row, column));
                        }
                        column += cells;
                    }
                }
            }
            painted
        }

        let theme = Theme::default();
        let cases = [
            (KittyVariant::Blob, 2..4, 1..2),
            (KittyVariant::Cbear, 1..5, 0..1),
            (KittyVariant::FlyGirl, 0..2, 2..6),
        ];
        for (variant, animated_rows, stable_rows) in cases {
            let idle = glyphs(&kitty_rows(variant, false, 0, &theme));
            for frame in 0..3 {
                let processing = glyphs(&kitty_rows(variant, true, frame, &theme));
                for &(row, column) in &idle {
                    if stable_rows.contains(&row) || !animated_rows.contains(&row) {
                        assert!(processing.contains(&(row, column)));
                    }
                }
                for &(row, column) in &processing {
                    if stable_rows.contains(&row) || !animated_rows.contains(&row) {
                        assert!(idle.contains(&(row, column)));
                    }
                }
            }
        }
        let fly_idle = glyphs(&kitty_rows(KittyVariant::FlyGirl, false, 0, &theme));
        let fly_raised = glyphs(&kitty_rows(KittyVariant::FlyGirl, true, 1, &theme));
        assert!(fly_raised.contains(&(0, 8)));
        assert!(fly_idle
            .iter()
            .filter(|(row, _)| *row >= 2)
            .all(|cell| fly_raised.contains(cell)));
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

    // ---------------------------------------------------------------------
    // Wave 0 and Wave 1 performance harnesses (non-gating). These exercise
    // the real wrapping function and the real App/Renderer path on extreme
    // inputs. Each is #[ignore] so ordinary `cargo test` stays fast; run
    // them in release mode with, e.g.:
    //
    //   cargo test --release --lib -- --ignored \
    //       perf_harness:: --nocapture
    //
    // They print timing to stdout (use --nocapture to see it). They are
    // measurement tools, not pass/fail assertions, so they always pass
    // after recording the sample. Hardware timing is informational; the
    // goal is reproducible comparison across optimization passes. Wave 1
    // replaced the quadratic front-drain implementation in
    // `push_wrapped_line` with a single forward-index pass; the 1 MiB
    // unbroken-line harness is the primary regression target for that
    // change.
    // ---------------------------------------------------------------------
    mod perf_harness {
        use super::*;
        use std::time::Instant;

        fn fmt_ms(secs: f64) -> String {
            format!("{:.3} ms", secs * 1000.0)
        }

        /// 1 MiB unbroken line through the real chat wrapping function.
        /// Exercises `wrap_lines_at_words`, which is what `render_entry`
        /// applies to every chat entry. The output size (~width rows for
        /// the entire 1 MiB) is intentionally large so this also doubles
        /// as an allocation/iteration bound on the wrap path.
        #[test]
        #[ignore = "performance harness; release-only, run with --ignored --nocapture"]
        fn perf_wrap_one_mib_unbroken_line() {
            let width: usize = 100;
            // 1 MiB of ASCII text, no whitespace to break at, no newlines.
            let mut text = String::with_capacity(1 << 20);
            for _ in 0..(1 << 20) {
                text.push('a');
            }
            assert_eq!(text.len(), 1 << 20);
            let start = Instant::now();
            let wrapped = wrap_lines_at_words(vec![Line::raw(text)], width);
            let elapsed = start.elapsed();
            // Sanity: every row holds at most `width` characters (cells) and
            // rows cover the entire 1 MiB string with no loss. The final
            // row may be shorter than `width` because the input has no
            // trailing whitespace and the wrap ends mid-line.
            assert!(wrapped.len() >= (1 << 20) / width);
            assert!(wrapped.iter().all(|line| line.width() <= width));
            let total_chars: usize = wrapped
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|s| s.content.chars().count())
                        .sum::<usize>()
                })
                .sum();
            assert_eq!(total_chars, 1 << 20);
            eprintln!(
                "[perf_wrap_one_mib_unbroken_line] width={width} rows={} elapsed={}",
                wrapped.len(),
                fmt_ms(elapsed.as_secs_f64()),
            );
        }

        /// Render a ~50 KiB mixed Markdown / fenced-code response through
        /// the real App + Renderer path while it streams in. The delta
        /// events grow one entry; the renderer rebuilds the affected
        /// cache entry each frame and redraws the full TUI each time.
        /// This is the worst case the live TUI sees on long answers.
        #[test]
        #[ignore = "performance harness; release-only, run with --ignored --nocapture"]
        fn perf_render_50kib_streaming_mixed_markdown_response() {
            let config = Config::default();
            let mut app = App::new(&config, Selection::default());
            let payload = build_growing_payload(50 * 1024);
            // Drain the stream in ~50 chunks so the harness spends time in
            // the streaming path, not in setup. Each chunk is a UiEvent
            // that revs the entry revision, forcing a cache rebuild.
            let chunks = 50usize;
            let mut loop_and_draw_secs = 0.0f64;
            let mut renderer = Renderer::default();
            // Warm the renderer once so syntax grammars and themes are
            // initialized (OnceLock); their cost is one-time and would
            // otherwise dominate the first delta.
            screen(&mut renderer, &app, 120, 40);
            let loop_start = Instant::now();
            // Slice the payload into `chunks` roughly equal byte runs.
            // Each chunk's start is derived from the previous chunk's
            // adjusted end so that bumping `end` to the next char
            // boundary never invalidates subsequent slices, even when
            // a multi-byte codepoint straddles the boundary.
            let step = payload.len().div_ceil(chunks).max(1);
            let mut next_start = 0usize;
            for _ in 0..chunks {
                let start = next_start;
                if start >= payload.len() {
                    break;
                }
                let mut end = (start + step).min(payload.len());
                // Round `end` up to the next char boundary so the
                // slice stays valid UTF-8 even when we cut mid-codepoint.
                while end < payload.len() && !payload.is_char_boundary(end) {
                    end += 1;
                }
                next_start = end;
                let chunk = &payload[start..end];
                app.event(UiEvent::Delta {
                    context: "main".into(),
                    text: chunk.to_owned(),
                });
                let frame_start = Instant::now();
                screen(&mut renderer, &app, 120, 40);
                loop_and_draw_secs += frame_start.elapsed().as_secs_f64();
            }
            // One final draw at the full size.
            let final_start = Instant::now();
            screen(&mut renderer, &app, 120, 40);
            loop_and_draw_secs += final_start.elapsed().as_secs_f64();
            // Capture rebuild count after the loop, once every delta has
            // had a chance to invalidate the cache.
            let total_rebuilds = renderer.rebuilds;
            let total_loop = loop_start.elapsed();
            // The single entry must hold the full payload.
            assert_eq!(app.entries.len(), 1);
            assert_eq!(app.entries[0].text.len(), payload.len());
            assert!(app.entries[0].streaming);
            // Every delta should have invalidated the cache at least once.
            assert!(total_rebuilds >= 1);
            eprintln!(
                "[perf_render_50kib_streaming_mixed_markdown_response] bytes={} chunks={} loop_and_draw_total={} rebuilds={total_rebuilds} loop_total={}",
                payload.len(),
                chunks,
                fmt_ms(loop_and_draw_secs),
                fmt_ms(total_loop.as_secs_f64()),
            );
        }

        /// Render one active 2 MiB entry through the real App + Renderer
        /// path. This complements the growing 50 KiB harness above and
        /// reports the bounded live-render cost at a much larger answer size.
        #[test]
        #[ignore = "performance harness; release-only, run with --ignored --nocapture"]
        fn perf_render_two_mib_active_stream() {
            let config = Config::default();
            let mut app = App::new(&config, Selection::default());
            let payload = build_growing_payload(2 * 1024 * 1024);
            let chunks = 64usize;
            let mut renderer = Renderer::default();
            screen(&mut renderer, &app, 120, 40);
            let start = Instant::now();
            let step = payload.len().div_ceil(chunks).max(1);
            let mut next_start = 0usize;
            let mut draws = 0usize;
            while next_start < payload.len() {
                let chunk_start = next_start;
                let mut end = (chunk_start + step).min(payload.len());
                while end < payload.len() && !payload.is_char_boundary(end) {
                    end += 1;
                }
                next_start = end;
                app.event(UiEvent::Delta {
                    context: "main".into(),
                    text: payload[chunk_start..end].to_owned(),
                });
                screen(&mut renderer, &app, 120, 40);
                draws += 1;
            }
            let elapsed = start.elapsed();

            assert_eq!(app.entries.len(), 1);
            assert_eq!(app.entries[0].text.len(), payload.len());
            assert!(app.entries[0].streaming);
            eprintln!(
                "[perf_render_two_mib_active_stream] bytes={} chunks={draws} elapsed={} rebuilds={}",
                payload.len(),
                fmt_ms(elapsed.as_secs_f64()),
                renderer.rebuilds,
            );
        }

        /// Steady rendering of ~10,000 transcript entries. After the
        /// cache is warm the renderer is supposed to skip rebuilding any
        /// entry whose revision has not changed, so the steady-state cost
        /// is dominated by `history()` walking the cache and the final
        /// `screen` driving ratatui over a 120x40 backend.
        #[test]
        #[ignore = "performance harness; release-only, run with --ignored --nocapture"]
        fn perf_render_ten_k_steady_transcript() {
            let count = 10_000usize;
            let config = Config::default();
            let mut app = App::new(&config, Selection::default());
            for index in 0..count {
                // Short mixed Markdown lines keep total bytes small while
                // exercising header/body/footer path for every message.
                let role = if index % 2 == 0 { "user" } else { "assistant" };
                let body = format!(
                    "{}\n\nMessage #{index} with `inline code` and a fenced block:\n```rust\nfn f() {{ {index} }}\n```",
                    short_paragraph(),
                );
                app.message("main".into(), Message::new(role, body));
            }
            let mut renderer = Renderer::default();
            // Warm the renderer to populate the cache for every entry.
            screen(&mut renderer, &app, 120, 40);
            let warm_rebuilds = renderer.rebuilds;
            // Steady-state: no events fire between draws; rebuilds must
            // not advance.
            let redraws = 50usize;
            let start = Instant::now();
            for _ in 0..redraws {
                screen(&mut renderer, &app, 120, 40);
            }
            let elapsed = start.elapsed();
            assert_eq!(renderer.rebuilds, warm_rebuilds);
            eprintln!(
                "[perf_render_ten_k_steady_transcript] entries={count} warm_rebuilds={} redraws={redraws} total={} avg/frame={}",
                warm_rebuilds,
                fmt_ms(elapsed.as_secs_f64()),
                fmt_ms(elapsed.as_secs_f64() / redraws as f64),
            );
        }

        /// Render a transcript with ~2,000 collapsed Tool activities,
        /// each owning one detail entry. The harness is the regression
        /// target for the snapshot-backed assembly path: the cached
        /// entries keep the per-chat-entry work amortized, and the
        /// timeline assembly must read every depth / visibility /
        /// descendant count from a single `LayoutSnapshot`. Cost per
        /// draw is `O(timeline + rendered lines)`, NOT
        /// `O(visible × timeline × depth)`. All activities stay
        /// collapsed so the renderer still emits every summary each
        /// frame (the worst case for the snapshot fallback path).
        /// No timing assertion is made; the harness is measurement
        /// only and prints average frame time.
        #[test]
        #[ignore = "performance harness; release-only, run with --ignored --nocapture"]
        fn perf_render_two_k_collapsed_tools_with_entries() {
            let activity_count = 2_000usize;
            let redraws = 50usize;
            let config = Config::default();
            let mut app = App::new(&config, Selection::default());

            // Build the workload: 2,000 Tool activities, each owned by
            // a detail entry (a tool-result message). The Tool stays
            // collapsed for the lifetime of the harness so its detail
            // entry never reaches the visible-rows vector, but it
            // still occupies the cache and an `Entry` timeline row.
            // Wiring Tool Activity → Tool-result message → End is
            // enough to make the entry owned by the Tool via the
            // `external_id` registry.
            for index in 0..activity_count {
                let tool_id = format!("tool-{index}");
                let call_id = format!("call-{index}");
                app.event(UiEvent::Activity(crate::model::ActivityEvent {
                    id: tool_id.clone(),
                    parent_id: None,
                    context: "main".into(),
                    kind: crate::model::ActivityKind::Tool,
                    phase: crate::model::ActivityPhase::Start,
                    title: format!("Tool #{index}"),
                    external_id: Some(call_id.clone()),
                    status: None,
                }));
                app.message("main".into(), Message::tool(&call_id, "ok"));
                app.event(UiEvent::Activity(crate::model::ActivityEvent {
                    id: tool_id,
                    parent_id: None,
                    context: "main".into(),
                    kind: crate::model::ActivityKind::Tool,
                    phase: crate::model::ActivityPhase::End,
                    title: String::new(),
                    external_id: None,
                    status: Some(crate::model::ActivityStatus::Success),
                }));
            }
            // Build size guarantees: 2,000 Activity timelines rows
            // for the Tool Start (no End row), 2,000 Entry rows
            // (owned but hidden because the Tools are collapsed).
            assert_eq!(app.activities.len(), activity_count);
            assert!(app.timeline.len() >= activity_count);
            assert_eq!(app.entries.len(), activity_count);

            let mut renderer = Renderer::default();
            // Warm the renderer once so syntax grammars and themes
            // are initialized; their one-time cost otherwise dominates
            // the first draw and would bias the average.
            screen(&mut renderer, &app, 120, 40);
            let warm_rebuilds = renderer.rebuilds;
            // After the warm draw, the per-entry cache holds one row
            // per detail entry. Cache size is `Renderer::cache.len()`,
            // exposed for the diagnostic via a public accessor during
            // tests; the `perf_*` harnesses rely on the assertion that
            // nothing in `App` mutates between draws, so the rebuild
            // count must stay at zero across the loop.
            let warm_cache_len = renderer.cache_len();

            let start = Instant::now();
            for _ in 0..redraws {
                screen(&mut renderer, &app, 120, 40);
            }
            let elapsed = start.elapsed();
            // Rebuild count stays flat in steady state; nothing on
            // the App mutates between draws.
            assert_eq!(renderer.rebuilds, warm_rebuilds);
            eprintln!(
                "[perf_render_two_k_collapsed_tools_with_entries] tools={} timeline={} cache_entries={warm_cache_len} warm_rebuilds={} redraws={redraws} total={} avg/frame={}",
                activity_count,
                app.timeline.len(),
                warm_rebuilds,
                fmt_ms(elapsed.as_secs_f64()),
                fmt_ms(elapsed.as_secs_f64() / redraws as f64),
            );
        }

        /// A short, varied paragraph used by the steady-state harness so
        /// each entry has real Markdown to wrap and highlight.
        fn short_paragraph() -> &'static str {
            "# Heading\nSome prose with `inline code` and a fenced snippet:\n\
             ```python\ndef hello(name):\n    return f\"hi {name}\"\n```\n\
             Plain tail."
        }

        /// Build a deterministic growing mixed Markdown payload of
        /// approximately `target` bytes, alternating prose, inline code,
        /// and fenced blocks so the renderer exercises the highlighter
        /// and the chat wrap path on every chunk.
        fn build_growing_payload(target: usize) -> String {
            let seeds: &[&str] = &[
                "# Heading\n",
                "Prose with `inline code` and prose. ",
                "```rust\nfn alpha() { let n = 1; }\n```\n",
                "```python\ndef beta(name):\n    return f\"hi {name}\"\n```\n",
                "More **bold** and _italic_ prose.\n",
                "```json\n{ \"k\": [1,2,3] }\n```\n",
                "Final line of the chunk. ",
            ];
            let mut out =
                String::with_capacity(target + seeds.iter().map(|s| s.len()).sum::<usize>());
            while out.len() < target {
                for seed in seeds {
                    out.push_str(seed);
                    if out.len() >= target {
                        break;
                    }
                }
            }
            out.truncate(target);
            out
        }
    }

    // ---------------------------------------------------------------------
    // Wave 2 activity summary tests. Cover the rendering contract:
    // collapsed root, expanded details, nested child, error collapsed,
    // timeline ordering, ASCII/Unicode markers, one-line truncation,
    // bottom anchoring, cache no-rebuild toggle, agent header visibility,
    // and kitty no-header-overlap.
    // ---------------------------------------------------------------------
    mod wave2_activity {
        use super::*;
        use crate::model::{
            ActivityEvent, ActivityKind, ActivityPhase, ActivityStatus, Message, UiEvent,
        };
        use crate::{engine::Engine, session::Session};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        fn start(
            id: &str,
            parent: Option<&str>,
            context: &str,
            kind: ActivityKind,
            external: Option<&str>,
            title: &str,
        ) -> ActivityEvent {
            ActivityEvent {
                id: id.into(),
                parent_id: parent.map(str::to_owned),
                context: context.into(),
                kind,
                phase: ActivityPhase::Start,
                title: title.into(),
                external_id: external.map(str::to_owned),
                status: None,
            }
        }

        fn end(id: &str, context: &str, status: Option<ActivityStatus>) -> ActivityEvent {
            ActivityEvent {
                id: id.into(),
                parent_id: None,
                context: context.into(),
                kind: ActivityKind::Tool,
                phase: ActivityPhase::End,
                title: format!("{id} end"),
                external_id: None,
                status,
            }
        }

        /// Test the summary line directly so we can pin the exact spans
        /// without depending on the full screen render. Avoids ratatui
        /// terminal allocation while still going through the same code
        /// path the renderer uses.
        fn summary(app: &App, id: &str, width: usize) -> Line<'static> {
            let snapshot = app.layout_snapshot();
            activity_summary_line_snapshot(app, &snapshot, id, width, &app.theme)
        }

        async fn enter_activity_focus(app: &mut App) -> (tempfile::TempDir, Engine) {
            let directory = tempfile::tempdir().unwrap();
            let session = Session::open(directory.path(), Some("render-focus")).unwrap();
            let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let engine = Engine::new(Config::default(), session, events);
            app.handle_key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE), &engine)
                .await
                .unwrap();
            (directory, engine)
        }

        #[tokio::test]
        async fn focused_summary_only_gets_cta_background_and_keeps_hit_target() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.theme.cta_background = "#123456".into();
            app.event(UiEvent::Activity(start(
                "first",
                None,
                "main",
                ActivityKind::Tool,
                None,
                "first summary",
            )));
            app.event(UiEvent::Activity(start(
                "second",
                None,
                "main",
                ActivityKind::Tool,
                None,
                "second summary",
            )));
            app.event(UiEvent::Activity(end(
                "second",
                "main",
                Some(ActivityStatus::Success),
            )));

            let first_before = summary(&app, "first", 80);
            let second_before = summary(&app, "second", 80);
            assert_eq!(first_before.style.bg, None);
            assert_eq!(second_before.style.bg, None);

            let (_directory, _engine) = enter_activity_focus(&mut app).await;
            assert_eq!(app.focused_activity_id(), Some("second"));

            let first = summary(&app, "first", 80);
            let second = summary(&app, "second", 80);
            assert_eq!(
                first.style.bg, None,
                "other summaries keep normal background"
            );
            assert_eq!(
                second.style.bg,
                Some(color(&app.theme.cta_background)),
                "the focused matching summary uses the CTA background"
            );
            assert_eq!(first.to_string(), first_before.to_string());
            assert_eq!(second.to_string(), second_before.to_string());
            assert_eq!(second.to_string().lines().count(), 1);
            assert!(second.to_string().len() <= 80);
            assert!(second.to_string().contains("[ok]"));

            let mut renderer = Renderer::default();
            let lines = renderer.history(&app, 80, 4);
            assert_eq!(lines.len(), renderer.hit_map.len());
            assert_eq!(
                renderer.hit_map,
                vec![
                    Some(ActivitySummary { id: "first".into() }),
                    Some(ActivitySummary {
                        id: "second".into()
                    }),
                ]
            );
            assert_eq!(lines[0].to_string(), first.to_string());
            assert_eq!(lines[1].to_string(), second.to_string());
        }

        #[tokio::test]
        async fn focused_ascii_summary_uses_cta_background_without_changing_output() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.theme.ascii = true;
            app.theme.cta_background = "#654321".into();
            app.event(UiEvent::Activity(start(
                "root",
                None,
                "main",
                ActivityKind::Subagent,
                None,
                "planner",
            )));
            app.event(UiEvent::Activity(start(
                "child",
                Some("root"),
                "main",
                ActivityKind::Tool,
                None,
                "read files",
            )));
            app.set_activity_expanded("root", Some(true));

            let root_before = summary(&app, "root", 80);
            let before = summary(&app, "child", 80);
            let (_directory, engine) = enter_activity_focus(&mut app).await;
            app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &engine)
                .await
                .unwrap();
            let focused = summary(&app, "root", 80);
            assert_eq!(app.focused_activity_id(), Some("root"));
            assert_eq!(focused.style.bg, Some(color(&app.theme.cta_background)));
            assert_eq!(focused.to_string(), root_before.to_string());
            assert_eq!(before.to_string(), "|  [+] read files [running]");
            assert_eq!(before.to_string().lines().count(), 1);
        }

        #[test]
        fn collapsed_root_renders_one_summary_line() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.event(UiEvent::Activity(start(
                "tool-1",
                None,
                "main",
                ActivityKind::Tool,
                Some("req-1"),
                "shell ls",
            )));
            let line = summary(&app, "tool-1", 80);
            let text = line.to_string();
            assert!(
                text.starts_with("[+] "),
                "collapsed root must show [+], got {text:?}"
            );
            assert!(text.contains("[+] shell ls [running]"));
            assert!(text.contains("[running]"));
        }

        #[test]
        fn expanded_root_shows_marker_and_dropped_count() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.event(UiEvent::Activity(start(
                "tool-1",
                None,
                "main",
                ActivityKind::Tool,
                Some("req-1"),
                "shell ls",
            )));
            app.set_activity_expanded("tool-1", Some(true));
            let line = summary(&app, "tool-1", 80);
            let text = line.to_string();
            assert!(text.starts_with("[-] "));
            assert!(!text.contains("(+"));
        }

        #[test]
        fn error_status_is_visible_when_collapsed() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.event(UiEvent::Activity(start(
                "tool-err",
                None,
                "main",
                ActivityKind::Tool,
                None,
                "shell bogus",
            )));
            app.event(UiEvent::Activity(end(
                "tool-err",
                "main",
                Some(ActivityStatus::Error),
            )));
            // Collapsed by default per the contract.
            assert!(!app.activity("tool-err").unwrap().expanded);
            let line = summary(&app, "tool-err", 80);
            let text = line.to_string();
            assert!(text.starts_with("[+] "));
            assert!(text.contains("[error]"));
        }

        #[test]
        fn nested_child_only_renders_when_parent_is_expanded() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.event(UiEvent::Activity(start(
                "sub",
                None,
                "main",
                ActivityKind::Subagent,
                None,
                "planner",
            )));
            app.event(UiEvent::Activity(start(
                "tool-a",
                Some("sub"),
                "main",
                ActivityKind::Tool,
                None,
                "read",
            )));
            // Child hidden while parent is collapsed.
            assert!(!app.visible_activity_ids().contains(&"tool-a".to_string()));
            // Expand the parent; child becomes visible.
            app.set_activity_expanded("sub", Some(true));
            assert!(app.visible_activity_ids().contains(&"tool-a".to_string()));
            let line = summary(&app, "tool-a", 80);
            let text = line.to_string();
            // depth=1, so an indent prefix is rendered.
            assert!(
                text.starts_with("│  ") || text.starts_with("|  "),
                "nested child must be indented, got {text:?}"
            );
            assert!(text.contains("[+] "));
        }

        #[test]
        fn summary_text_truncates_to_one_line() {
            let mut app = App::new(&Config::default(), Selection::default());
            let long = "a".repeat(200);
            app.event(UiEvent::Activity(start(
                "tool-1",
                None,
                "main",
                ActivityKind::Tool,
                None,
                &long,
            )));
            let line = summary(&app, "tool-1", 40);
            let text = line.to_string();
            assert!(
                text.chars().count() <= 40,
                "summary must not wrap, got {:?}",
                text
            );
            assert!(
                text.contains('…'),
                "truncated summary must end with ellipsis"
            );
        }

        #[test]
        fn ascii_mode_uses_ascii_branch_indent() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.theme.ascii = true;
            app.event(UiEvent::Activity(start(
                "sub",
                None,
                "main",
                ActivityKind::Subagent,
                None,
                "planner",
            )));
            app.event(UiEvent::Activity(start(
                "tool-a",
                Some("sub"),
                "main",
                ActivityKind::Tool,
                None,
                "read",
            )));
            app.set_activity_expanded("sub", Some(true));
            let line = summary(&app, "tool-a", 80);
            let text = line.to_string();
            assert!(
                text.starts_with("|  "),
                "ascii indent must use pipes, got {text:?}"
            );
            assert!(!text.contains('│'), "ascii mode must avoid Unicode branch");
        }

        #[test]
        fn unicode_mode_uses_branch_indent() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.theme.ascii = false;
            app.event(UiEvent::Activity(start(
                "sub",
                None,
                "main",
                ActivityKind::Subagent,
                None,
                "planner",
            )));
            app.event(UiEvent::Activity(start(
                "tool-a",
                Some("sub"),
                "main",
                ActivityKind::Tool,
                None,
                "read",
            )));
            app.set_activity_expanded("sub", Some(true));
            let line = summary(&app, "tool-a", 80);
            let text = line.to_string();
            assert!(
                text.starts_with("│  "),
                "unicode indent must use box-drawing, got {text:?}"
            );
        }

        #[test]
        fn timeline_ordering_is_preserved_through_summary_insertions() {
            let mut app = App::new(&Config::default(), Selection::default());
            // user -> assistant -> sub (root) -> tool-a (root) -> tool-b
            // (nested under sub) -> assistant follow-up -> tool-c (root).
            app.message("main".into(), Message::new("user", "hi"));
            app.message("main".into(), Message::new("assistant", "hello"));
            app.event(UiEvent::Activity(start(
                "sub",
                None,
                "main",
                ActivityKind::Subagent,
                None,
                "planner",
            )));
            app.event(UiEvent::Activity(start(
                "tool-a",
                None,
                "main",
                ActivityKind::Tool,
                None,
                "shell a",
            )));
            app.event(UiEvent::Activity(start(
                "tool-b",
                Some("sub"),
                "main",
                ActivityKind::Tool,
                None,
                "shell b",
            )));
            app.message("main".into(), Message::new("assistant", "follow-up"));
            app.event(UiEvent::Activity(start(
                "tool-c",
                None,
                "main",
                ActivityKind::Tool,
                None,
                "shell c",
            )));
            app.set_activity_expanded("sub", Some(true));
            let output = screen(&mut Renderer::default(), &app, 120, 30);
            // Pin the timeline-arrival order of the visible summary lines
            // and the chat entries so a refactor of the timeline walk
            // cannot silently reorder them.
            let sub = output.find("planner").expect("planner present");
            let tool_a = output.find("shell a").expect("shell a present");
            let tool_b = output.find("shell b").expect("shell b present");
            let follow_up = output.find("follow-up").expect("follow-up present");
            let tool_c = output.find("shell c").expect("shell c present");
            assert!(sub < tool_a, "sub summary precedes the later tool-a root");
            assert!(tool_a < tool_b, "tool-a precedes the sub-nested tool-b");
            assert!(tool_b < follow_up, "tool-b precedes later chat");
            assert!(follow_up < tool_c, "later chat precedes tool-c");
        }

        #[test]
        fn cache_no_rebuild_when_only_expansion_changes() {
            // Create an activity that owns a short entry body so the summary
            // and the body both fit in the chat window. Toggling must update
            // the summary (marker flips) without invalidating the cached body.
            let mut app = App::new(&Config::default(), Selection::default());
            app.event(UiEvent::Activity(start(
                "tool-1",
                None,
                "main",
                ActivityKind::Tool,
                Some("tool-1"),
                "shell long",
            )));
            app.message("main".into(), Message::tool("tool-1", "short body"));
            // Warm the renderer so the owned entry body is in the cache.
            let mut renderer = Renderer::default();
            screen(&mut renderer, &app, 100, 24);
            let rebuilds = renderer.rebuilds;
            // Toggle the activity several times. The summary line should
            // update (verified by the marker), but no entry body should
            // rebuild because the cached body is still valid.
            app.toggle_activity_expanded("tool-1");
            let output = screen(&mut renderer, &app, 100, 24);
            assert!(
                output.contains("[-]"),
                "expanded marker must appear after toggle"
            );
            assert_eq!(renderer.rebuilds, rebuilds);
            app.toggle_activity_expanded("tool-1");
            screen(&mut renderer, &app, 100, 24);
            assert_eq!(renderer.rebuilds, rebuilds);
        }

        #[test]
        fn history_viewport_windows_preserve_order_and_hit_targets() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.message("main".into(), Message::new("assistant", "before"));

            app.event(UiEvent::Activity(start(
                "collapsed",
                None,
                "main",
                ActivityKind::Tool,
                Some("collapsed-call"),
                "collapsed",
            )));
            app.message(
                "main".into(),
                Message::tool("collapsed-call", "hidden-collapsed"),
            );
            app.event(UiEvent::Activity(start(
                "collapsed-child",
                Some("collapsed"),
                "main",
                ActivityKind::Tool,
                Some("collapsed-child-call"),
                "collapsed child",
            )));
            app.message(
                "main".into(),
                Message::tool("collapsed-child-call", "hidden-descendant"),
            );

            app.event(UiEvent::Activity(start(
                "expanded",
                None,
                "main",
                ActivityKind::Tool,
                Some("expanded-call"),
                "expanded",
            )));
            app.message(
                "main".into(),
                Message::tool("expanded-call", "detail-one\ndetail-two"),
            );
            app.event(UiEvent::Activity(start(
                "nested",
                Some("expanded"),
                "main",
                ActivityKind::Tool,
                Some("nested-call"),
                "nested",
            )));
            app.message("main".into(), Message::new("assistant", "after"));

            // The expanded activity owns the multi-line detail; its child is
            // visible as a summary, while the collapsed activity hides both
            // its detail and its descendant.
            app.set_activity_expanded("expanded", Some(true));
            let expected = vec![
                "┌ assistant ┐",
                "│ before",
                "└ ┘",
                "[+] collapsed [ok] (+2)",
                "[-] expanded [ok] (+2)",
                "┌ tool ┐",
                "│ detail-one",
                "│ detail-two",
                "└ ┘",
                "│  [+] nested [running]",
                "┌ assistant ┐",
                "│ after",
                "└ ┘",
            ];

            macro_rules! assert_semantic_hit_targets {
                ($lines:expr, $hit_map:expr) => {
                    for (line, hit) in $lines.iter().zip(&$hit_map) {
                        let text = line.to_string();
                        if let Some(summary) = hit {
                            let marker_and_title = match summary.id.as_str() {
                                "collapsed" => "[+] collapsed",
                                "expanded" => "[-] expanded",
                                "nested" => "[+] nested",
                                id => panic!("unexpected activity summary id: {id}"),
                            };
                            assert!(
                                text.contains(marker_and_title),
                                "summary row for {} must contain {marker_and_title:?}: {text:?}",
                                summary.id
                            );
                        }
                        for body in [
                            "hidden-collapsed",
                            "hidden-descendant",
                            "detail-one",
                            "detail-two",
                        ] {
                            if text.contains(body) {
                                assert!(
                                    hit.is_none(),
                                    "entry-body row {body:?} must not have a hit target: {text:?}"
                                );
                            }
                        }
                    }
                };
            }

            let mut renderer = Renderer::default();
            let rebuilds = {
                app.scroll = usize::MAX;
                let lines = renderer.history(&app, 80, 4);
                assert_eq!(
                    lines.iter().map(Line::to_string).collect::<Vec<_>>(),
                    expected[..4]
                );
                assert_eq!(renderer.hit_map.len(), lines.len());
                assert_eq!(
                    renderer.hit_map,
                    vec![
                        None,
                        None,
                        None,
                        Some(ActivitySummary {
                            id: "collapsed".into()
                        })
                    ]
                );
                assert_semantic_hit_targets!(lines, renderer.hit_map);

                app.scroll = 3;
                let lines = renderer.history(&app, 80, 4);
                assert_eq!(
                    lines.iter().map(Line::to_string).collect::<Vec<_>>(),
                    expected[6..10]
                );
                assert_eq!(renderer.hit_map.len(), lines.len());
                assert_eq!(
                    renderer.hit_map,
                    vec![
                        None,
                        None,
                        None,
                        Some(ActivitySummary {
                            id: "nested".into()
                        })
                    ]
                );
                assert_semantic_hit_targets!(lines, renderer.hit_map);

                app.scroll = 0;
                let lines = renderer.history(&app, 80, 4);
                assert_eq!(
                    lines.iter().map(Line::to_string).collect::<Vec<_>>(),
                    expected[9..13]
                );
                assert_eq!(renderer.hit_map.len(), lines.len());
                assert_eq!(
                    renderer.hit_map,
                    vec![
                        Some(ActivitySummary {
                            id: "nested".into()
                        }),
                        None,
                        None,
                        None
                    ]
                );
                assert_semantic_hit_targets!(lines, renderer.hit_map);
                renderer.rebuilds
            };

            // Expansion changes only summary visibility and the layout window;
            // cached entry bodies remain valid.
            app.set_activity_expanded("expanded", Some(false));
            app.scroll = 0;
            let lines = renderer.history(&app, 80, 4);
            assert_eq!(renderer.rebuilds, rebuilds);
            assert_eq!(renderer.hit_map.len(), lines.len());
            assert!(
                lines
                    .iter()
                    .all(|line| !line.to_string().contains("detail-")),
                "collapsed details must not appear"
            );
            assert!(
                renderer.hit_map.iter().all(|hit| match hit {
                    Some(summary) => summary.id == "expanded",
                    None => true,
                }),
                "only visible activity summaries may have hit targets"
            );
            assert!(!lines.iter().any(|line| line.to_string().contains("nested")));
        }

        #[test]
        fn owned_entry_detail_is_hidden_when_owner_is_collapsed() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.event(UiEvent::Activity(start(
                "tool-1",
                None,
                "main",
                ActivityKind::Tool,
                Some("tool-1"),
                "shell long",
            )));
            app.message("main".into(), Message::tool("tool-1", "hello"));
            assert!(!app.entry_is_visible(0));
            app.set_activity_expanded("tool-1", Some(true));
            assert!(app.entry_is_visible(0));
        }

        #[test]
        fn bottom_anchoring_keeps_latest_lines_visible() {
            // Streaming accumulation that ends below the viewport must
            // anchor at the bottom so the new lines are visible. This
            // pins the existing scroll semantics across the timeline walk.
            let mut app = App::new(&Config::default(), Selection::default());
            for index in 0..30 {
                let body = format!("message number {index}");
                app.event(UiEvent::Delta {
                    context: "main".into(),
                    text: format!("{body}\n"),
                });
                app.event(UiEvent::Message {
                    context: "main".into(),
                    message: Message::new("assistant", body),
                });
            }
            let mut renderer = Renderer::default();
            let output = screen(&mut renderer, &app, 80, 24);
            // The very last line of text must be visible — bottom anchoring.
            assert!(
                output.contains("message number 29"),
                "latest line must remain visible at the bottom"
            );
        }

        #[test]
        fn header_composes_model_agent_effort_on_the_top_row() {
            let mut app = App::new(&Config::default(), Selection::default());
            app.selection.agent = Some("writer".into());
            app.effective_agent_label = "writer".into();
            app.model_label = "openai:gpt-5".into();
            app.effort_label = "high".into();
            let mut renderer = Renderer::default();
            let output = screen(&mut renderer, &app, 120, 24);
            assert!(
                output.contains("openai:gpt-5 | agent writer | effort high"),
                "{output:?}"
            );
            assert!(!output.contains("agent: writer | model:"));
            let content_width = 120 - 2;
            let kitty_width = kitty_rows(KittyVariant::Blob, false, 0, &Theme::default())
                .iter()
                .map(Line::width)
                .max()
                .unwrap() as u16;
            let metadata_width =
                content_width - HEADER_LOGO_WIDTH - HEADER_SEPARATOR_WIDTH - kitty_width;
            let crop_end = renderer
                .last_kitty_reservation
                .map_or(output.lines().nth(1).unwrap().len(), |r| r.x as usize);
            let expected_trimmed_width = (1 + HEADER_LOGO_WIDTH + metadata_width) as usize;
            assert_eq!(
                output.lines().nth(1).unwrap()[..crop_end].trim_end().len(),
                expected_trimmed_width
            );
        }

        #[test]
        fn kitty_first_visible_row_is_anchored_to_terminal_top() {
            let app = App::new(&Config::default(), Selection::default());
            let output = screen(&mut Renderer::default(), &app, 80, 24);
            let kitty_row = output
                .lines()
                .position(|line| line.contains('█'))
                .expect("kitty glyph visible");
            assert_eq!(kitty_row, 1);
        }

        #[test]
        fn hidden_descendants_are_not_cloned_into_visible_lines() {
            // Three nested activities; only the root is expanded. The screen
            // must contain the root summary plus the (collapsed) child
            // summary, but the owned entry detail behind the collapsed
            // child must not render at all — its body must not be cloned
            // into the visible lines.
            let mut app = App::new(&Config::default(), Selection::default());
            app.message("main".into(), Message::new("user", "u1"));
            app.message("main".into(), Message::new("assistant", "a1"));
            app.event(UiEvent::Activity(start(
                "sub",
                None,
                "main",
                ActivityKind::Subagent,
                None,
                "planner",
            )));
            app.event(UiEvent::Activity(start(
                "tool-a",
                Some("sub"),
                "main",
                ActivityKind::Tool,
                Some("tool-a"),
                "inner-tool",
            )));
            app.message("main".into(), Message::tool("tool-a", "payload"));
            app.message("main".into(), Message::new("user", "u2"));
            // Expand only the root. Nested tool-a stays collapsed so the
            // owned entry detail must remain hidden, even though its
            // parent (sub) is expanded.
            app.set_activity_expanded("sub", Some(true));
            let output = screen(&mut Renderer::default(), &app, 120, 30);
            assert!(output.contains("planner"), "root summary visible");
            assert!(
                output.contains("inner-tool"),
                "collapsed child summary visible (sub is expanded), got:\n{output}"
            );
            assert!(
                !output.contains("payload"),
                "owned entry detail behind collapsed child must not render, got:\n{output}"
            );
        }
    }
}
