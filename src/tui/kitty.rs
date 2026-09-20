//! Spinning cat artwork. Three variants (Blob, Cbear, Fly Girl) rotate every
//! 900 s from TUI launch. The launch variant is picked once at startup using a
//! UUID-derived offset so different sessions start on different artwork, but
//! every subsequent rotation is a fixed sequence. While a query is processing,
//! the variant's animated segment cycles through a small set of frames; idle
//! draws a single rest frame.
//!
//! Every variant shares one six-row canvas: row 0 is padding above the artwork
//! and each asset starts at row 1, so every variant has room to move its
//! animated segment up by one row without shifting the chat layout. The
//! renderer launch time is captured once by the TUI; the variant index and
//! processing frames come from pure helpers so tests can pin them without
//! sleeping.

use super::render;
use crate::config::Theme;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use std::time::Duration;
use uuid::Uuid;

/// One rotation step in seconds. At `n * ROTATION_SECONDS` (plus the launch
/// offset) the index advances, so elapsed 0/899 s lands on the offset variant,
/// elapsed 900/1799 s on the next, and 1800 s on the third.
pub const ROTATION_SECONDS: u64 = 900;

/// Number of kitty variants. Order is fixed: Blob, Cbear, Fly Girl.
pub const VARIANT_COUNT: usize = 3;

/// Shared canvas height so rotation and animation never shift the chat layout.
pub const CANVAS_HEIGHT: usize = 6;

/// Source assets are loaded at compile time so the binary carries them and
/// tests do not need filesystem access. The flygrl asset's filename includes
/// a space; `include_str!` accepts that unchanged.
const BLOB_SOURCE: &str = include_str!("../../assetts/blob_kitty.txt");
const CBEAR_SOURCE: &str = include_str!("../../assetts/cbear_kitty1.txt");
const FLYGRL_SOURCE: &str = include_str!("../../assetts/flygrl kitty.txt");

/// Fixed variant order. The launch offset is randomized, so the variant shown
/// at launch is not always Blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyVariant {
    Blob,
    Cbear,
    FlyGirl,
}

/// All variants in the fixed order used for rotation.
pub const VARIANTS: [KittyVariant; VARIANT_COUNT] = [
    KittyVariant::Blob,
    KittyVariant::Cbear,
    KittyVariant::FlyGirl,
];

/// Variant metadata. `base_lines` is the rest pose on the shared canvas;
/// processing frames only reposition the variant's animated segment.
#[derive(Debug, Clone, Copy)]
pub struct VariantSpec {
    pub variant: KittyVariant,
    /// Number of frames the variant cycles through while processing.
    pub frame_count: usize,
    /// Per-frame delay in milliseconds, indexed by `frame % frame_count`.
    pub delays_ms: &'static [u64],
    /// Rest pose. Lines carry no trailing whitespace; row 0 stays empty so
    /// upward movement always has somewhere to go.
    pub base_lines: [&'static str; CANVAS_HEIGHT],
}

/// Split a compile-time asset into `CANVAS_HEIGHT` rows on the shared canvas.
/// Only trailing whitespace is trimmed so the visible width matches the
/// painted glyphs; the artwork starts at row 1 and row 0 stays empty padding.
/// The returned slices borrow the `include_str!` source, so nothing leaks.
fn load_lines(source: &'static str) -> [&'static str; CANVAS_HEIGHT] {
    let mut rows: [&'static str; CANVAS_HEIGHT] = [""; CANVAS_HEIGHT];
    for (index, line) in source.lines().enumerate() {
        let row = index + 1;
        if row >= CANVAS_HEIGHT {
            break;
        }
        rows[row] = line.trim_end();
    }
    rows
}

fn variant_spec(variant: KittyVariant) -> VariantSpec {
    match variant {
        KittyVariant::Blob => VariantSpec {
            variant: KittyVariant::Blob,
            frame_count: 4,
            delays_ms: &[700, 500, 500, 700],
            base_lines: load_lines(BLOB_SOURCE),
        },
        KittyVariant::Cbear => VariantSpec {
            variant: KittyVariant::Cbear,
            frame_count: 3,
            delays_ms: &[800, 600, 600],
            base_lines: load_lines(CBEAR_SOURCE),
        },
        KittyVariant::FlyGirl => VariantSpec {
            variant: KittyVariant::FlyGirl,
            frame_count: 3,
            delays_ms: &[600, 500, 600],
            base_lines: load_lines(FLYGRL_SOURCE),
        },
    }
}

/// Pure helper used by the renderer and the tests: given how long the TUI has
/// been running and the per-session launch offset, return the variant index.
/// Elapsed is a `Duration`, never a wall-clock read, so rotation is
/// deterministic under test. The offset is captured once at launch so every
/// session starts on a different variant; the rotation sequence after launch
/// is fixed.
pub fn variant_index(elapsed: Duration, offset: usize) -> usize {
    let step = (elapsed.as_secs() / ROTATION_SECONDS) as usize;
    (step + offset % VARIANT_COUNT) % VARIANT_COUNT
}

pub fn variant_at(elapsed: Duration, offset: usize) -> KittyVariant {
    VARIANTS[variant_index(elapsed, offset)]
}

/// Pick a launch variant offset from a UUID v4. Random per call, so two
/// sessions launched in quick succession land on different artwork without
/// touching a new dependency.
pub fn random_offset() -> usize {
    let uuid = Uuid::new_v4();
    let bytes = uuid.as_bytes();
    (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize) % VARIANT_COUNT
}

/// Render the idle (non-processing) rest pose for the variant.
pub fn render_idle(variant: KittyVariant, theme: &Theme) -> Vec<Line<'static>> {
    let spec = variant_spec(variant);
    paint_lines(&spec.base_lines.map(str::to_owned), theme)
}

/// Render a processing frame for the variant. `frame` wraps modulo the
/// variant's own `frame_count`, so callers do not need to know the count.
pub fn render_processing(variant: KittyVariant, frame: usize, theme: &Theme) -> Vec<Line<'static>> {
    let spec = variant_spec(variant);
    paint_lines(&apply_animation(&spec, frame), theme)
}

/// Per-variant delay for the supplied processing frame, in milliseconds.
pub fn frame_delay_ms(variant: KittyVariant, frame: usize) -> u64 {
    let spec = variant_spec(variant);
    spec.delays_ms[frame % spec.frame_count]
}

/// Dispatch to the per-variant frame builder. Frames never alter the asset's
/// body; they only reposition the animated segment by one cell vertically.
fn apply_animation(spec: &VariantSpec, frame: usize) -> [String; CANVAS_HEIGHT] {
    match spec.variant {
        KittyVariant::Blob => blob_animation(frame, spec),
        KittyVariant::Cbear => cbear_animation(frame, spec),
        KittyVariant::FlyGirl => flygrl_animation(frame, spec),
    }
}

// --- Blob -------------------------------------------------------------------
//
// Source asset on canvas rows 1-4:
//   row 1: "      /█   /█"
//   row 2: "    ▄████████"
//   row 3: "██▀▀███▄▄██▄▄█"
//   row 4: "      ▀▀▀▀▀▀▀"
//
// The lower `▀▀▀▀▀▀▀` segment on source line 4 (canvas row 4) is the only
// moving part. While processing it rises one row onto canvas row 3, where it
// overlays the lower body cells, and row 4 becomes empty. Row 5 (padding)
// stays empty in every frame. The body and face never move.
fn blob_animation(frame: usize, spec: &VariantSpec) -> [String; CANVAS_HEIGHT] {
    let phase = frame % spec.frame_count;
    let tail_up = phase == 1;
    let mut rows = spec.base_lines.map(str::to_owned);
    if tail_up {
        // Overlay the lower body cells (col 6 onward) with the tail segment;
        // row 4 empties to make the upward movement visible.
        rows[3] = overwrite(spec.base_lines[3], 6, "▀▀▀▀▀▀▀");
        rows[4] = String::new();
    }
    rows
}

// --- Cbear ------------------------------------------------------------------
//
// Source asset on canvas rows 1-4:
//   row 1: " ∩ ∩   Ω"
//   row 2: "│¬ ¬│ //"
//   row 3: "│ - │//"
//   row 4: "v───v"
//
// The right Ω/slash tail (Ω at row 1 col 7, `//` at row 2 cols 6-7, `//` at
// row 3 cols 5-6) drops one row as a unit and keeps its columns, so the
// diagonal shape survives. Face and body cells never move.
fn cbear_animation(frame: usize, spec: &VariantSpec) -> [String; CANVAS_HEIGHT] {
    let down = frame % spec.frame_count == 1;
    let mut rows = spec.base_lines.map(str::to_owned);
    if down {
        rows[1] = overwrite(spec.base_lines[1], 7, " ");
        rows[2] = overwrite(overwrite(spec.base_lines[2], 6, "  ").as_str(), 7, "Ω");
        rows[3] = overwrite(spec.base_lines[3], 5, " //");
        rows[4] = overwrite(spec.base_lines[4], 5, "//");
    }
    rows
}

// --- Fly Girl ---------------------------------------------------------------
//
// Source asset on canvas rows 1-5:
//   row 1: "        Z"
//   row 2: "  /\_/\Z"
//   row 3: "()(_ _)"
//   row 4: "()( ° )"
//   row 5: "(_____)"
//
// Only the free-floating Z on row 1 rises one row into the padding row 0; the
// Z attached to the cat's back on row 2 and the body never move.
fn flygrl_animation(frame: usize, spec: &VariantSpec) -> [String; CANVAS_HEIGHT] {
    let up = frame % spec.frame_count == 1;
    let mut rows = spec.base_lines.map(str::to_owned);
    if up {
        rows[0] = spec.base_lines[1].to_owned();
        rows[1] = String::new();
    }
    rows
}

// --- Painting ---------------------------------------------------------------
//
// `paint_lines` turns canvas rows into styled `Line`s of actual glyphs. The
// body uses the theme border color; eyes and the floating Z use the pink
// shades, matching the earlier pixel-art paint.

fn paint_lines(rows: &[String; CANVAS_HEIGHT], theme: &Theme) -> Vec<Line<'static>> {
    let body = body_color(theme);
    let pink = Color::Rgb(255, 79, 163);
    let light_pink = Color::Rgb(255, 183, 216);
    rows.iter()
        .enumerate()
        .map(|(row, line)| {
            let mut spans = Vec::new();
            let mut text = String::new();
            let mut style = body;
            for character in line.chars() {
                let next = classify(row, character, body, pink, light_pink);
                if next != style && !text.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut text),
                        Style::default().fg(style),
                    ));
                }
                style = next;
                text.push(character);
            }
            if !text.is_empty() {
                spans.push(Span::styled(text, Style::default().fg(style)));
            }
            // Bold keeps the cat visible on monochrome themes such as mama_j.
            Line::from(spans).style(Style::default().add_modifier(Modifier::BOLD))
        })
        .collect()
}

/// The free-floating Z lives on canvas rows 0-1 (light pink); the attached Z
/// and everything else follow the body or eye colors.
fn classify(row: usize, character: char, body: Color, pink: Color, light_pink: Color) -> Color {
    if character.is_whitespace() {
        return body;
    }
    match character {
        'Z' if row <= 1 => light_pink,
        'Z' => pink,
        '°' => light_pink,
        _ => body,
    }
}

/// Theme colors are hex strings; convert through the shared renderer helper so
/// the cat follows the active theme exactly like every other surface.
fn body_color(theme: &Theme) -> Color {
    render::color(&theme.border)
}

// --- String helpers ---------------------------------------------------------

/// Overwrite `replacement` starting at char index `start`, padding with spaces
/// when the row is too short. Trailing whitespace is trimmed so mutated rows
/// keep the same invariant as the trimmed source assets.
fn overwrite(line: &str, start: usize, replacement: &str) -> String {
    let mut chars: Vec<char> = line.chars().collect();
    let end = start + replacement.chars().count();
    while chars.len() < end {
        chars.push(' ');
    }
    for (offset, character) in replacement.chars().enumerate() {
        chars[start + offset] = character;
    }
    while chars.last().is_some_and(|c| c.is_whitespace()) {
        chars.pop();
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOB_TAIL: &str = "      ▀▀▀▀▀▀▀";
    const BLOB_BODY: &str = "██▀▀███▄▄██▄▄█";

    /// Exact rest-pose content. Pinned so the upward-only tail animation does
    /// not silently leak into the rest frame or onto the padding row.
    fn assert_idle_blob(text: &[String]) {
        assert_eq!(text[0], "");
        assert_eq!(text[1], "      /█   /█");
        assert_eq!(text[2], "    ▄████████");
        assert_eq!(text[3], BLOB_BODY);
        assert_eq!(text[4], BLOB_TAIL);
        assert_eq!(text[5], "");
        assert_eq!(text.len(), CANVAS_HEIGHT);
    }

    #[test]
    fn variant_index_with_offset_zero_is_documented_sequence() {
        let s = ROTATION_SECONDS;
        assert_eq!(variant_index(Duration::ZERO, 0), 0);
        assert_eq!(variant_index(Duration::from_secs(s - 1), 0), 0);
        assert_eq!(variant_index(Duration::from_secs(s), 0), 1);
        assert_eq!(variant_index(Duration::from_secs(2 * s - 1), 0), 1);
        assert_eq!(variant_index(Duration::from_secs(2 * s), 0), 2);
        assert_eq!(variant_index(Duration::from_secs(3 * s), 0), 0);
    }

    #[test]
    fn variant_index_offset_one_advances_through_the_remaining_variants() {
        let s = ROTATION_SECONDS;
        assert_eq!(variant_index(Duration::ZERO, 1), 1);
        assert_eq!(variant_index(Duration::from_secs(s - 1), 1), 1);
        assert_eq!(variant_index(Duration::from_secs(s), 1), 2);
        assert_eq!(variant_index(Duration::from_secs(2 * s - 1), 1), 2);
        assert_eq!(variant_index(Duration::from_secs(2 * s), 1), 0);
        assert_eq!(variant_index(Duration::from_secs(3 * s), 1), 1);
        assert_eq!(variant_at(Duration::ZERO, 1), KittyVariant::Cbear);
    }

    #[test]
    fn variant_index_offset_two_starts_on_fly_girl_and_wraps() {
        let s = ROTATION_SECONDS;
        assert_eq!(variant_index(Duration::ZERO, 2), 2);
        assert_eq!(variant_index(Duration::from_secs(s), 2), 0);
        assert_eq!(variant_index(Duration::from_secs(2 * s), 2), 1);
        assert_eq!(variant_index(Duration::from_secs(3 * s), 2), 2);
        assert_eq!(variant_at(Duration::ZERO, 2), KittyVariant::FlyGirl);
    }

    #[test]
    fn variant_index_wraps_around_every_three_steps() {
        for offset in 0..VARIANT_COUNT {
            // Three 900 s steps land back on the same variant regardless of
            // the offset, so rotation can never drift off the catalog.
            assert_eq!(
                variant_index(Duration::from_secs(3 * ROTATION_SECONDS), offset),
                offset,
            );
            // Six steps land on the same variant again.
            assert_eq!(
                variant_index(Duration::from_secs(6 * ROTATION_SECONDS), offset),
                offset,
            );
        }
    }

    #[test]
    fn variant_index_offset_larger_than_variant_count_is_modular() {
        let s = ROTATION_SECONDS;
        // 4 % 3 == 1, 5 % 3 == 2, 6 % 3 == 0.
        assert_eq!(variant_index(Duration::ZERO, 4), 1);
        assert_eq!(variant_index(Duration::ZERO, 5), 2);
        assert_eq!(variant_index(Duration::ZERO, 6), 0);
        assert_eq!(variant_index(Duration::from_secs(s), 5), 0);
    }

    #[test]
    fn rotation_boundaries_pin_the_documented_seconds() {
        assert_eq!(variant_index(Duration::from_secs(899), 0), 0);
        assert_eq!(variant_index(Duration::from_secs(900), 0), 1);
        assert_eq!(variant_index(Duration::from_secs(1799), 0), 1);
        assert_eq!(variant_index(Duration::from_secs(1800), 0), 2);
        assert_eq!(
            variant_at(Duration::from_secs(1800), 0),
            KittyVariant::FlyGirl
        );
    }

    #[test]
    fn startup_variant_with_offset_zero_is_blob() {
        // Without an offset the sequence is still deterministic and starts on
        // Blob; the offset only reorders the start.
        assert_eq!(variant_at(Duration::ZERO, 0), KittyVariant::Blob);
        assert_eq!(variant_index(Duration::ZERO, 0), 0);
    }

    #[test]
    fn random_offset_returns_values_in_the_variant_range() {
        for _ in 0..64 {
            let offset = random_offset();
            assert!(offset < VARIANT_COUNT);
        }
    }

    #[test]
    fn blob_idle_frame_uses_the_unmodified_asset() {
        let idle = render_idle(KittyVariant::Blob, &Theme::default());
        let text: Vec<String> = idle.iter().map(Line::to_string).collect();
        assert_idle_blob(&text);
    }

    #[test]
    fn cbear_idle_frame_uses_the_unmodified_asset() {
        let idle = render_idle(KittyVariant::Cbear, &Theme::default());
        let text: Vec<String> = idle.iter().map(Line::to_string).collect();
        assert_eq!(text[0], "");
        assert_eq!(text[1], " ∩ ∩   Ω");
        assert_eq!(text[2], "│¬ ¬│ //");
        assert_eq!(text[3], "│ - │//");
        assert_eq!(text[4], "v───v");
        assert_eq!(text.len(), CANVAS_HEIGHT);
    }

    #[test]
    fn flygrl_idle_frame_uses_the_unmodified_asset() {
        let idle = render_idle(KittyVariant::FlyGirl, &Theme::default());
        let text: Vec<String> = idle.iter().map(Line::to_string).collect();
        assert_eq!(text[0], "");
        assert_eq!(text[1], "        Z");
        assert_eq!(text[2], "  /\\_/\\Z");
        assert_eq!(text[3], "()(_ _)");
        assert_eq!(text[4], "()( ° )");
        assert_eq!(text[5], "(_____)");
        assert_eq!(text.len(), CANVAS_HEIGHT);
    }

    #[test]
    fn blob_rest_frames_match_the_unmodified_asset() {
        // Every frame that is not the tail-up frame must be the rest pose.
        for frame in [0, 2, 3] {
            let pose = render_processing(KittyVariant::Blob, frame, &Theme::default());
            let text: Vec<String> = pose.iter().map(Line::to_string).collect();
            assert_idle_blob(&text);
        }
    }

    #[test]
    fn blob_processing_frame_moves_only_the_tail_segment_up() {
        // Frame 1 is the only animated frame; the tail overlays the lower
        // body cells on row 3 and row 4 empties. Row 5 (padding) stays empty
        // and the head never moves.
        let frame = render_processing(KittyVariant::Blob, 1, &Theme::default());
        let text: Vec<String> = frame.iter().map(Line::to_string).collect();
        // The face and the upper body lines never change.
        assert_eq!(text[0], "");
        assert_eq!(text[1], "      /█   /█");
        assert_eq!(text[2], "    ▄████████");
        // The tail overlaid the lower body cells starting at column 6; the
        // original head (`██`) at columns 0-1 stays put.
        assert_eq!(text[3], "██▀▀██▀▀▀▀▀▀▀█");
        // Row 4 emptied because the tail rose one row.
        assert_eq!(text[4], "");
        // Row 5 (the padding row) must stay empty in every frame.
        assert_eq!(text[5], "");
    }

    #[test]
    fn blob_processing_never_moves_the_head() {
        // The head never moves up or down in any frame; the `██` on row 3
        // columns 0-1 stays put, and row 2 columns 0-1 stay blank.
        for frame in 0..variant_spec(KittyVariant::Blob).frame_count {
            let pose = render_processing(KittyVariant::Blob, frame, &Theme::default());
            let text: Vec<String> = pose.iter().map(Line::to_string).collect();
            // Row 2 columns 0-1 stay blank (no upward head movement).
            assert!(
                text[2].chars().take(2).all(|c| c == ' '),
                "frame {frame} moved the head into row 2: {:?}",
                text[2]
            );
            // Row 3 columns 0-1 stay `██` (no removed head cells).
            let head: String = text[3].chars().take(2).collect();
            assert_eq!(
                head, "██",
                "frame {frame} removed the head from row 3: {:?}",
                text[3]
            );
        }
    }

    #[test]
    fn blob_processing_keeps_padding_row_empty() {
        for frame in 0..variant_spec(KittyVariant::Blob).frame_count {
            let pose = render_processing(KittyVariant::Blob, frame, &Theme::default());
            let text: Vec<String> = pose.iter().map(Line::to_string).collect();
            assert_eq!(
                text[5], "",
                "frame {frame} left content in padding row 5: {:?}",
                text[5]
            );
        }
    }

    #[test]
    fn cbear_processing_moves_only_the_tail_structure() {
        let frame = render_processing(KittyVariant::Cbear, 1, &Theme::default());
        let text: Vec<String> = frame.iter().map(Line::to_string).collect();
        // Face and body cells stay; the Ω dropped one row.
        assert_eq!(text[1], " ∩ ∩");
        assert_eq!(text[2], "│¬ ¬│  Ω");
        assert_eq!(text[3], "│ - │ //");
        assert_eq!(text[4], "v───v//");
    }

    #[test]
    fn cbear_rest_and_last_frame_match_the_asset() {
        for frame in [0, 2] {
            let pose = render_processing(KittyVariant::Cbear, frame, &Theme::default());
            let idle = render_idle(KittyVariant::Cbear, &Theme::default());
            assert_eq!(pose, idle);
        }
    }

    #[test]
    fn flygrl_processing_moves_only_the_floating_z_up() {
        let frame = render_processing(KittyVariant::FlyGirl, 1, &Theme::default());
        let text: Vec<String> = frame.iter().map(Line::to_string).collect();
        // The free Z rose into the padding row 0; row 1 is empty.
        assert_eq!(text[0], "        Z");
        assert_eq!(text[1], "");
        // The attached Z on row 2 and the body are unchanged.
        assert_eq!(text[2], "  /\\_/\\Z");
        assert_eq!(text[3], "()(_ _)");
        assert_eq!(text[4], "()( ° )");
        assert_eq!(text[5], "(_____)");
    }

    #[test]
    fn every_variant_shares_the_same_canvas_and_alignment() {
        for variant in VARIANTS {
            let idle = render_idle(variant, &Theme::default());
            assert_eq!(idle.len(), CANVAS_HEIGHT);
            // Row 0 is empty padding for every resting variant, so the
            // artwork always starts on the same canvas row and rotation
            // cannot jump.
            assert_eq!(idle[0].to_string(), "");
            for frame in 0..variant_spec(variant).frame_count {
                let lines = render_processing(variant, frame, &Theme::default());
                // The canvas height never changes, whatever a frame animates.
                assert_eq!(lines.len(), CANVAS_HEIGHT);
                // Only Fly Girl's upward Z ever enters the padding row 0.
                if variant != KittyVariant::FlyGirl || frame % 3 != 1 {
                    assert_eq!(lines[0].to_string(), "");
                }
            }
        }
    }

    #[test]
    fn trailing_whitespace_is_trimmed_for_visible_width() {
        let blob = render_idle(KittyVariant::Blob, &Theme::default());
        // The asset ends with trailing spaces on source line 3; the trimmed
        // in-memory copy omits them so the painted width is exact.
        assert_eq!(blob[4].to_string(), "      ▀▀▀▀▀▀▀");
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(blob[4].to_string().as_str()),
            13
        );
        let flygrl = render_idle(KittyVariant::FlyGirl, &Theme::default());
        assert_eq!(flygrl[2].to_string(), "  /\\_/\\Z");
    }

    #[test]
    fn frame_delays_match_the_variant() {
        assert_eq!(frame_delay_ms(KittyVariant::Blob, 0), 700);
        assert_eq!(frame_delay_ms(KittyVariant::Blob, 1), 500);
        assert_eq!(frame_delay_ms(KittyVariant::Cbear, 1), 600);
        assert_eq!(frame_delay_ms(KittyVariant::FlyGirl, 2), 600);
    }
}
