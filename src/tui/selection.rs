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
    for row_index in start_row..=end_row {
        let row = &rows[row_index];
        let start_col = if row_index == start_row { start.col } else { 0 };
        let end_col = if row_index == end_row { end.col } else { usize::MAX };
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

    #[test]
    fn ordered_forward() {
        let s = sel((0, 0), (0, 5));
        assert_eq!(
            s.ordered(),
            (
                TextPos { row: 0, col: 0 },
                TextPos { row: 0, col: 5 }
            )
        );
    }

    #[test]
    fn ordered_backward() {
        let s = sel((2, 3), (0, 1));
        assert_eq!(
            s.ordered(),
            (
                TextPos { row: 0, col: 1 },
                TextPos { row: 2, col: 3 }
            )
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
        let expected_ascii = format!("\x1b]52;c;{}\x07", base64(&"a".repeat(100_000).into_bytes()));
        assert_eq!(osc52(&ascii), expected_ascii);
    }
}
