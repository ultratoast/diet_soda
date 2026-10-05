#![allow(dead_code)]
//! Pure text-selection helpers for the TUI: no terminal I/O.
//!
//! A [`Selection`] names a region and an anchor/head [`TextPos`]. Rows are
//! described by [`RowInfo`], where `col` is a display-cell offset into the
//! row's text (gutter excluded) and a wide character is never split: a char is
//! included only when its first cell falls inside the requested range.

use unicode_width::UnicodeWidthChar;

/// Which pane a selection lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    History,
    Input,
    Popup,
}

/// A position within a region: `row` indexes the region's row list, `col` is a
/// display-cell column inside the row text (gutter excluded).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub row: usize,
    pub col: usize,
}

/// An ordered or reversed selection over a region. `anchor` is where the drag
/// started and `head` is where it currently ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub region: Region,
    pub anchor: TextPos,
    pub head: TextPos,
}

impl Selection {
    /// Returns `(start, end)` with `start <= end` regardless of drag direction.
    pub fn ordered(&self) -> (TextPos, TextPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// A single displayed row. `text` excludes the gutter; `continues_previous` is
/// true when this row soft-wraps a continuation of the previous row's logical
/// line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowInfo {
    pub text: String,
    pub continues_previous: bool,
}

/// Extracts the selected text from `rows`.
///
/// Rows outside `rows` are ignored (indices are clamped). Selected rows are
/// joined with `"\n"`, except when a row has `continues_previous == true`, in
/// which case it is appended with no separator. Trailing whitespace is kept.
pub fn extract(rows: &[RowInfo], sel: &Selection) -> String {
    let (start, end) = sel.ordered();
    if rows.is_empty() || start.row >= rows.len() {
        return String::new();
    }
    let last = rows.len() - 1;
    let start_row = start.row.min(last);
    let end_row = end.row.min(last);
    let mut out = String::new();
    for (row_index, row) in rows
        .iter()
        .enumerate()
        .skip(start_row)
        .take(end_row - start_row + 1)
    {
        let start_col = if row_index == start_row { start.col } else { 0 };
        let end_col = if row_index == end_row {
            end.col
        } else {
            usize::MAX
        };
        if row_index != start_row && !row.continues_previous {
            out.push('\n');
        }
        out.push_str(&slice_row(&row.text, start_col, end_col));
    }
    out
}

/// Slices `text` to the chars whose first display cell is in
/// `[start_col, end_col)`. A wide char is included whole or not at all.
fn slice_row(text: &str, start_col: usize, end_col: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for ch in text.chars() {
        let first = col;
        col += UnicodeWidthChar::width(ch).unwrap_or(0);
        if first >= start_col && first < end_col {
            out.push(ch);
        }
    }
    out
}

/// Clamps `(x, y)` into `rect = (x, y, width, height)`. A zero-sized rect
/// returns its own origin.
pub fn clamp_point(x: u16, y: u16, rect: (u16, u16, u16, u16)) -> (u16, u16) {
    let (rx, ry, rw, rh) = rect;
    if rw == 0 || rh == 0 {
        return (rx, ry);
    }
    let max_x = rx.saturating_add(rw - 1);
    let max_y = ry.saturating_add(rh - 1);
    (x.clamp(rx, max_x), y.clamp(ry, max_y))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard-alphabet base64 with `=` padding.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64[((n >> 18) & 63) as usize] as char);
        out.push(B64[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(B64[((n >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(B64[(n & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Builds an OSC 52 clipboard escape carrying `text`, truncated to at most
/// 100_000 bytes on a char boundary.
pub fn osc52(text: &str) -> String {
    let mut end = text.len().min(100_000);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::from("\x1b]52;c;");
    out.push_str(&base64(&text.as_bytes()[..end]));
    out.push('\x07');
    out
}

/// A selectable on-screen text region captured from the last drawn frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelRegion {
    pub region: Region,
    /// Screen rect of the selectable area: (x, y, width, height). Row i of `rows` is drawn at screen y = rect.1 + i.
    pub rect: (u16, u16, u16, u16),
    pub rows: Vec<RowInfo>,
    /// Screen x where each row's selectable text begins (after any gutter). Same length as `rows`.
    pub x0: Vec<u16>,
    /// Absolute index of rows[0]. Selection TextPos.row values are absolute, so they survive scrolling and new output.
    pub row_offset: usize,
}

/// True when (x, y) lies inside `rect` = (x, y, width, height); right/bottom edges are exclusive.
pub fn contains(rect: (u16, u16, u16, u16), x: u16, y: u16) -> bool {
    let (rx, ry, w, h) = rect;
    w > 0
        && h > 0
        && x >= rx
        && y >= ry
        && (x as u32) < rx as u32 + w as u32
        && (y as u32) < ry as u32 + h as u32
}

/// Index of the region a mouse-down at (x, y) belongs to.
/// If any Popup region exists, only the LAST popup (topmost) is a candidate:
/// return its index when the point is inside it, otherwise None.
/// With no popup, return the first region whose rect contains the point.
pub fn region_at(regions: &[SelRegion], x: u16, y: u16) -> Option<usize> {
    if let Some(top) = regions.iter().rposition(|r| r.region == Region::Popup) {
        return contains(regions[top].rect, x, y).then_some(top);
    }
    regions.iter().position(|r| contains(r.rect, x, y))
}

fn row_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// Map a screen point to a text position inside `region`, clamping to the region.
/// Above the first row -> (0,0). Below the last row -> end of the last row.
/// Left of the text start -> col 0. Right of the line end -> end of that line.
pub fn pos_in(region: &SelRegion, x: u16, y: u16) -> TextPos {
    let count = region.rows.len();
    if count == 0 {
        return TextPos {
            row: region.row_offset,
            col: 0,
        };
    }
    let top = region.rect.1;
    if y < top {
        return TextPos {
            row: region.row_offset,
            col: 0,
        };
    }
    let row = (y - top) as usize;
    if row >= count {
        let last = count - 1;
        return TextPos {
            row: region.row_offset + last,
            col: row_width(&region.rows[last].text),
        };
    }
    let start = region.x0.get(row).copied().unwrap_or(region.rect.0);
    let col = (x.saturating_sub(start) as usize).min(row_width(&region.rows[row].text));
    TextPos {
        row: region.row_offset + row,
        col,
    }
}

/// Convert an absolute selection to region-local row indices, clipping the parts outside the visible rows.
/// Returns None when the selection lies entirely outside the visible rows.
fn localize(region: &SelRegion, sel: &Selection) -> Option<Selection> {
    let len = region.rows.len();
    if len == 0 {
        return None;
    }
    let (start, end) = sel.ordered();
    let offset = region.row_offset;
    if end.row < offset || start.row >= offset + len {
        return None;
    }
    let local_start = if start.row < offset {
        TextPos { row: 0, col: 0 }
    } else {
        TextPos {
            row: start.row - offset,
            col: start.col,
        }
    };
    let local_end = if end.row >= offset + len {
        TextPos {
            row: len - 1,
            col: usize::MAX,
        }
    } else {
        TextPos {
            row: end.row - offset,
            col: end.col,
        }
    };
    Some(Selection {
        region: sel.region,
        anchor: local_start,
        head: local_end,
    })
}

/// Text covered by `sel` inside `region` (newline between logical lines, none across soft wraps).
pub fn selected_text(region: &SelRegion, sel: &Selection) -> String {
    match localize(region, sel) {
        Some(local) => extract(&region.rows, &local),
        None => String::new(),
    }
}

/// Screen cells (x, y) to highlight for `sel` inside `region`.
/// First row starts at start.col, last row ends at end.col (exclusive), middle rows are full width.
/// Every row before the last in range also gets one extra cell just past its text end (visualizes the newline)
/// when that cell is still inside the rect width. A zero-length selection yields no cells.
pub fn selected_cells(region: &SelRegion, sel: &Selection) -> Vec<(u16, u16)> {
    let Some(sel) = localize(region, sel) else {
        return Vec::new();
    };
    let sel = &sel;
    let (start, end) = sel.ordered();
    let mut cells = Vec::new();
    if start == end || region.rows.is_empty() {
        return cells;
    }
    let last_row = end.row.min(region.rows.len() - 1);
    let (rx, ry, rw, rh) = region.rect;
    for row in start.row..=last_row {
        if row >= region.rows.len() {
            break;
        }
        let y = ry as u32 + row as u32;
        if y >= ry as u32 + rh as u32 {
            break;
        }
        let width = row_width(&region.rows[row].text);
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { usize::MAX };
        let base = region.x0.get(row).copied().unwrap_or(rx) as u32;
        let right_edge = rx as u32 + rw as u32;
        let mut col = 0usize;
        for ch in region.rows[row].text.chars() {
            let first = col;
            col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if first < from || first >= to {
                continue;
            }
            for cell in first..col {
                let x = base + cell as u32;
                if x < right_edge {
                    cells.push((x as u16, y as u16));
                }
            }
        }
        if row < end.row {
            let x = base + width.max(from) as u32;
            if x < right_edge {
                cells.push((x as u16, y as u16));
            }
        }
    }
    cells
}

/// Selection mouse state machine; `None` means the event was consumed.
/// Left-down in a region starts a selection; left-drag moves the head (clamped to the starting region);
/// left-up without movement clears it and returns a synthetic left-down so click behavior still runs;
/// left-up after movement keeps the highlight and stores the text in `app.pending_copy`.
/// Everything else passes through.
pub fn handle_mouse(
    app: &mut super::app::App,
    regions: &[SelRegion],
    mouse: crossterm::event::MouseEvent,
) -> Option<crossterm::event::MouseEvent> {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    if !app.mouse_enabled {
        return Some(mouse);
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.text_selection = None;
            app.selection_dragging = false;
            match region_at(regions, mouse.column, mouse.row) {
                Some(index) => {
                    let point = pos_in(&regions[index], mouse.column, mouse.row);
                    app.text_selection = Some(Selection {
                        region: regions[index].region,
                        anchor: point,
                        head: point,
                    });
                    app.selection_dragging = true;
                    None
                }
                None => Some(mouse),
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if app.selection_dragging => {
            let kind = app.text_selection.as_ref().map(|s| s.region);
            let index = kind.and_then(|k| regions.iter().rposition(|r| r.region == k));
            match (index, app.text_selection.as_mut()) {
                (Some(index), Some(selection)) => {
                    selection.head = pos_in(&regions[index], mouse.column, mouse.row);
                }
                _ => {
                    app.text_selection = None;
                    app.selection_dragging = false;
                }
            }
            None
        }
        MouseEventKind::Up(MouseButton::Left) if app.selection_dragging => {
            app.selection_dragging = false;
            let selection = app.text_selection.clone()?;
            if selection.anchor == selection.head {
                app.text_selection = None;
                return Some(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    ..mouse
                });
            }
            if let Some(index) = regions.iter().rposition(|r| r.region == selection.region) {
                let text = selected_text(&regions[index], &selection);
                if !text.is_empty() {
                    app.pending_copy = Some(text);
                }
            }
            None
        }
        _ => Some(mouse),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str, continues_previous: bool) -> RowInfo {
        RowInfo {
            text: text.to_string(),
            continues_previous,
        }
    }

    fn sel(anchor: (usize, usize), head: (usize, usize)) -> Selection {
        Selection {
            region: Region::History,
            anchor: TextPos {
                row: anchor.0,
                col: anchor.1,
            },
            head: TextPos {
                row: head.0,
                col: head.1,
            },
        }
    }

    fn three_rows() -> SelRegion {
        SelRegion {
            region: Region::History,
            rect: (5, 2, 20, 3),
            rows: vec![
                RowInfo {
                    text: "hello world".into(),
                    continues_previous: false,
                },
                RowInfo {
                    text: "second".into(),
                    continues_previous: false,
                },
                RowInfo {
                    text: "third".into(),
                    continues_previous: false,
                },
            ],
            x0: vec![7, 7, 7],
            row_offset: 0,
        }
    }

    #[test]
    fn ordered_forward() {
        let s = sel((0, 0), (0, 5));
        assert_eq!(
            s.ordered(),
            (TextPos { row: 0, col: 0 }, TextPos { row: 0, col: 5 })
        );
    }

    #[test]
    fn ordered_backward() {
        let s = sel((2, 3), (0, 1));
        assert_eq!(
            s.ordered(),
            (TextPos { row: 0, col: 1 }, TextPos { row: 2, col: 3 })
        );
    }

    #[test]
    fn extract_single_row_partial() {
        let rows = [row("hello", false)];
        assert_eq!(extract(&rows, &sel((0, 1), (0, 4))), "ell");
    }

    #[test]
    fn extract_multi_row_inserts_newline() {
        let rows = [row("abc", false), row("def", false)];
        assert_eq!(extract(&rows, &sel((0, 0), (1, 3))), "abc\ndef");
    }

    #[test]
    fn extract_omits_newline_across_continuation() {
        let rows = [row("abc", false), row("def", true)];
        assert_eq!(extract(&rows, &sel((0, 0), (1, 3))), "abcdef");
    }

    #[test]
    fn extract_preserves_trailing_whitespace() {
        let rows = [row("hi   ", false)];
        assert_eq!(extract(&rows, &sel((0, 0), (0, 5))), "hi   ");
    }

    #[test]
    fn extract_wide_char_not_split() {
        let rows = [row("a漢b", false)];
        // Whole row.
        assert_eq!(extract(&rows, &sel((0, 0), (0, 4))), "a漢b");
        // Range covering exactly the wide char.
        assert_eq!(extract(&rows, &sel((0, 1), (0, 3))), "漢");
        // Starting in the middle of the wide char excludes it entirely.
        assert_eq!(extract(&rows, &sel((0, 2), (0, 3))), "");
    }

    #[test]
    fn extract_out_of_range_rows_ignored() {
        let rows = [row("only", false)];
        // End row clamps to the last available row.
        assert_eq!(extract(&rows, &sel((0, 0), (5, 5))), "only");
        // Start row beyond the available rows yields nothing.
        assert_eq!(extract(&rows, &sel((5, 0), (6, 0))), "");
    }

    #[test]
    fn clamp_point_inside_and_edges() {
        let rect = (10, 20, 5, 4);
        assert_eq!(clamp_point(12, 22, rect), (12, 22)); // inside
        assert_eq!(clamp_point(12, 0, rect), (12, 20)); // above
        assert_eq!(clamp_point(12, 100, rect), (12, 23)); // below
        assert_eq!(clamp_point(0, 22, rect), (10, 22)); // left
        assert_eq!(clamp_point(100, 22, rect), (14, 22)); // right
    }

    #[test]
    fn clamp_point_zero_sized_rect() {
        assert_eq!(clamp_point(99, 99, (1, 2, 0, 3)), (1, 2));
        assert_eq!(clamp_point(99, 99, (1, 2, 3, 0)), (1, 2));
    }

    #[test]
    fn base64_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_basic() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn osc52_truncates_on_char_boundary() {
        // Three bytes per char: 33333 chars == 99999 bytes, the largest
        // char-boundary prefix within the 100_000-byte cap.
        let text = "漢".repeat(33333);
        let expected = format!("\x1b]52;c;{}\x07", base64("漢".repeat(33333).as_bytes()));
        assert_eq!(osc52(&text), expected);
        assert_eq!(base64("漢".repeat(33333).as_bytes()).len(), 33333 * 4);

        // A large ASCII string is truncated to exactly 100_000 bytes.
        let ascii = "a".repeat(100_001);
        let expected_ascii = format!(
            "\x1b]52;c;{}\x07",
            base64(&"a".repeat(100_000).into_bytes())
        );
        assert_eq!(osc52(&ascii), expected_ascii);
    }

    fn region(kind: Region, rect: (u16, u16, u16, u16)) -> SelRegion {
        SelRegion {
            region: kind,
            rect,
            rows: Vec::new(),
            x0: Vec::new(),
            row_offset: 0,
        }
    }

    #[test]
    fn contains_inside_and_edges() {
        let rect = (0, 0, 80, 20);
        assert!(contains(rect, 0, 0)); // inside (top-left corner)
        assert!(contains(rect, 40, 10)); // inside
        assert!(!contains(rect, 80, 10)); // right edge exclusive
        assert!(!contains(rect, 10, 20)); // bottom edge exclusive
    }

    #[test]
    fn contains_zero_size_is_false() {
        assert!(!contains((0, 0, 0, 5), 0, 0));
        assert!(!contains((0, 0, 5, 0), 0, 0));
        assert!(!contains((0, 0, 0, 0), 0, 0));
    }

    #[test]
    fn region_at_popup_takes_priority() {
        let regions = [
            region(Region::History, (0, 0, 80, 20)),
            region(Region::Popup, (10, 5, 40, 10)),
        ];
        assert_eq!(region_at(&regions, 20, 8), Some(1)); // inside popup
        assert_eq!(region_at(&regions, 2, 2), None); // inside history but popup open
        assert_eq!(region_at(&regions, 60, 8), None); // inside neither
    }

    #[test]
    fn region_at_first_match_without_popup() {
        let regions = [
            region(Region::History, (0, 0, 80, 15)),
            region(Region::Input, (0, 15, 80, 5)),
        ];
        assert_eq!(region_at(&regions, 3, 16), Some(1));
        assert_eq!(region_at(&regions, 3, 3), Some(0));
        assert_eq!(region_at(&regions, 3, 30), None);
    }

    #[test]
    fn region_at_empty_slice() {
        assert_eq!(region_at(&[], 0, 0), None);
    }

    #[test]
    fn region_at_only_topmost_popup_counts() {
        let regions = [
            region(Region::Popup, (0, 0, 10, 10)),
            region(Region::Popup, (2, 2, 4, 4)),
        ];
        assert_eq!(region_at(&regions, 3, 3), Some(1)); // both contain it; topmost wins
        assert_eq!(region_at(&regions, 8, 8), None); // only lower popup contains it
    }

    #[test]
    fn pos_in_inside_region() {
        let r = three_rows();
        assert_eq!(pos_in(&r, 10, 3), TextPos { row: 1, col: 3 });
    }

    #[test]
    fn pos_in_above_region_clamps_to_origin() {
        let r = three_rows();
        assert_eq!(pos_in(&r, 10, 0), TextPos { row: 0, col: 0 });
    }

    #[test]
    fn pos_in_below_region_clamps_to_last_row_end() {
        let r = three_rows();
        assert_eq!(pos_in(&r, 10, 9), TextPos { row: 2, col: 5 });
    }

    #[test]
    fn pos_in_right_of_line_end_clamps_to_line_end() {
        let r = three_rows();
        assert_eq!(pos_in(&r, 19, 3), TextPos { row: 1, col: 6 });
    }

    #[test]
    fn pos_in_left_of_text_start_clamps_to_col_zero() {
        let r = three_rows();
        assert_eq!(pos_in(&r, 5, 3), TextPos { row: 1, col: 0 });
    }

    #[test]
    fn pos_in_empty_rows_returns_origin() {
        let r = region(Region::History, (5, 2, 20, 3));
        assert_eq!(pos_in(&r, 10, 3), TextPos { row: 0, col: 0 });
    }

    #[test]
    fn pos_in_returns_absolute_rows_with_offset() {
        let mut r = three_rows();
        r.row_offset = 10;
        assert_eq!(pos_in(&r, 10, 3), TextPos { row: 11, col: 3 });
        assert_eq!(pos_in(&r, 10, 0), TextPos { row: 10, col: 0 });
        assert_eq!(pos_in(&r, 10, 9), TextPos { row: 12, col: 5 });
    }

    #[test]
    fn selected_text_spans_logical_lines() {
        let r = three_rows();
        let s = Selection {
            region: Region::History,
            anchor: TextPos { row: 0, col: 6 },
            head: TextPos { row: 1, col: 3 },
        };
        assert_eq!(selected_text(&r, &s), "world\nsec");
    }

    #[test]
    fn selected_text_clips_selection_that_scrolled_partly_out_of_view() {
        let mut r = three_rows();
        r.row_offset = 10;
        let partly_above = Selection {
            region: Region::History,
            anchor: TextPos { row: 5, col: 2 },
            head: TextPos { row: 11, col: 3 },
        };
        assert_eq!(selected_text(&r, &partly_above), "hello world\nsec");
        let partly_below = Selection {
            region: Region::History,
            anchor: TextPos { row: 11, col: 3 },
            head: TextPos { row: 40, col: 0 },
        };
        assert_eq!(selected_text(&r, &partly_below), "ond\nthird");
    }

    #[test]
    fn selection_entirely_outside_visible_rows_is_empty() {
        let mut r = three_rows();
        r.row_offset = 10;
        let above = Selection {
            region: Region::History,
            anchor: TextPos { row: 1, col: 0 },
            head: TextPos { row: 4, col: 2 },
        };
        assert_eq!(selected_text(&r, &above), "");
        assert!(selected_cells(&r, &above).is_empty());
        let below = Selection {
            region: Region::History,
            anchor: TextPos { row: 20, col: 0 },
            head: TextPos { row: 30, col: 2 },
        };
        assert_eq!(selected_text(&r, &below), "");
        assert!(selected_cells(&r, &below).is_empty());
    }

    #[test]
    fn selected_cells_spans_rows_with_newline_cell() {
        let r = three_rows();
        let s = sel((0, 6), (1, 3));
        assert_eq!(
            selected_cells(&r, &s),
            vec![
                (13, 2),
                (14, 2),
                (15, 2),
                (16, 2),
                (17, 2),
                (18, 2),
                (7, 3),
                (8, 3),
                (9, 3),
            ]
        );
    }

    #[test]
    fn selected_cells_zero_length_is_empty() {
        let r = three_rows();
        let s = sel((1, 3), (1, 3));
        assert!(selected_cells(&r, &s).is_empty());
    }

    #[test]
    fn selected_cells_backward_matches_forward() {
        let r = three_rows();
        let forward = sel((0, 6), (1, 3));
        let backward = sel((1, 3), (0, 6));
        assert_eq!(selected_cells(&r, &forward), selected_cells(&r, &backward));
    }

    #[test]
    fn selected_cells_single_row_no_newline_cell() {
        let r = three_rows();
        let s = sel((1, 1), (1, 4));
        assert_eq!(selected_cells(&r, &s), vec![(8, 3), (9, 3), (10, 3)]);
    }

    #[test]
    fn selected_cells_clipped_by_rect_width() {
        let mut r = three_rows();
        r.rect = (5, 2, 10, 3);
        let s = sel((0, 6), (1, 3));
        let cells = selected_cells(&r, &s);
        assert!(cells.iter().all(|&(x, _)| x < 15), "cells: {cells:?}");
        assert_eq!(cells, vec![(13, 2), (14, 2), (7, 3), (8, 3), (9, 3)]);
    }

    fn test_app() -> crate::tui::app::App {
        crate::tui::app::App::new(
            &crate::config::Config::default(),
            crate::engine::Selection::default(),
        )
    }

    fn ev(
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
    ) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn popup_region() -> SelRegion {
        SelRegion {
            region: Region::Popup,
            rect: (30, 2, 20, 2),
            rows: vec![
                RowInfo { text: "popup one".into(), continues_previous: false },
                RowInfo { text: "popup two".into(), continues_previous: false },
            ],
            x0: vec![31, 31],
            row_offset: 0,
        }
    }

    #[test]
    fn drag_selects_and_stores_copy_text() {
        use crossterm::event::{MouseButton::Left, MouseEventKind as K};
        let mut app = test_app();
        let regions = vec![three_rows()];
        assert!(handle_mouse(&mut app, &regions, ev(K::Down(Left), 13, 2)).is_none());
        assert!(handle_mouse(&mut app, &regions, ev(K::Drag(Left), 10, 3)).is_none());
        assert!(handle_mouse(&mut app, &regions, ev(K::Up(Left), 10, 3)).is_none());
        assert_eq!(app.pending_copy, Some("world\nsec".to_string()));
        assert!(app.text_selection.is_some());
        assert!(!app.selection_dragging);
    }

    #[test]
    fn click_without_movement_returns_synthetic_down() {
        use crossterm::event::{MouseButton::Left, MouseEventKind as K};
        let mut app = test_app();
        let regions = vec![three_rows()];
        assert!(handle_mouse(&mut app, &regions, ev(K::Down(Left), 13, 2)).is_none());
        let up = handle_mouse(&mut app, &regions, ev(K::Up(Left), 13, 2)).expect("click forwarded");
        assert_eq!(up.kind, K::Down(Left));
        assert_eq!((up.column, up.row), (13, 2));
        assert!(app.text_selection.is_none());
        assert!(app.pending_copy.is_none());
    }

    #[test]
    fn down_outside_all_regions_passes_through() {
        use crossterm::event::{MouseButton::Left, MouseEventKind as K};
        let mut app = test_app();
        let regions = vec![three_rows()];
        assert!(handle_mouse(&mut app, &regions, ev(K::Down(Left), 0, 0)).is_some());
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn popup_drag_is_bounded_to_popup() {
        use crossterm::event::{MouseButton::Left, MouseEventKind as K};
        let regions = vec![three_rows(), popup_region()];
        let mut app = test_app();
        assert!(handle_mouse(&mut app, &regions, ev(K::Down(Left), 33, 2)).is_none());
        assert!(handle_mouse(&mut app, &regions, ev(K::Drag(Left), 6, 3)).is_none());
        assert!(handle_mouse(&mut app, &regions, ev(K::Up(Left), 6, 3)).is_none());
        let copied = app.pending_copy.clone().expect("copied");
        assert_eq!(copied, "pup one\n");
        assert!(!copied.contains("hello") && !copied.contains("second"));
        let mut fresh = test_app();
        assert!(handle_mouse(&mut fresh, &regions, ev(K::Down(Left), 6, 3)).is_some());
        assert!(fresh.text_selection.is_none());
    }

    #[test]
    fn wheel_and_disabled_mouse_pass_through() {
        use crossterm::event::{MouseButton::Left, MouseEventKind as K};
        let regions = vec![three_rows()];
        let mut app = test_app();
        assert!(handle_mouse(&mut app, &regions, ev(K::ScrollDown, 13, 2)).is_some());
        app.mouse_enabled = false;
        assert!(handle_mouse(&mut app, &regions, ev(K::Down(Left), 13, 2)).is_some());
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn selected_cells_highlights_both_cells_of_wide_glyphs() {
        let region = SelRegion {
            region: Region::History,
            rect: (5, 2, 20, 3),
            rows: vec![row("a漢b", false)],
            x0: vec![7],
            row_offset: 0,
        };
        let partial = Selection {
            region: Region::History,
            anchor: TextPos { row: 0, col: 0 },
            head: TextPos { row: 0, col: 3 },
        };
        assert_eq!(selected_cells(&region, &partial), vec![(7, 2), (8, 2), (9, 2)]);
        let full = Selection {
            region: Region::History,
            anchor: TextPos { row: 0, col: 0 },
            head: TextPos { row: 0, col: 99 },
        };
        assert_eq!(
            selected_cells(&region, &full),
            vec![(7, 2), (8, 2), (9, 2), (10, 2)]
        );
    }
}
