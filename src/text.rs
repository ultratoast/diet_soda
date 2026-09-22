//! Terminal text-safety helpers.
//!
//! Untrusted model/tool output reaches the terminal, so we strip characters that
//! can move the cursor, erase prior output, or forge visual order rather than
//! display as text. Policy:
//! - Remove all Cc controls; they are terminal commands, not content.
//! - Remove Cf format chars that have no normal rendering: bidi overrides and
//!   isolates, zero-width space, BOM, soft hyphen, and tag characters. These
//!   can spoof ordering or hide content without appearing on screen.
//! - Remove U+2028/U+2029 line/paragraph separators; they break line counting.
//! - Preserve U+200C (ZWNJ) and U+200D (ZWJ): legitimate in scripts and emoji.
//! - Ordinary printable Unicode is content and is never rewritten.

use std::borrow::Cow;

/// Characters unsafe to emit to a terminal, either because they are control
/// codes or because they render invisibly and can misrepresent text.
pub fn is_unsafe_terminal_char(c: char) -> bool {
    // ZWNJ/ZWJ shape real text; keep them despite living in the Cf range.
    if c == '\u{200C}' || c == '\u{200D}' {
        return false;
    }
    // Cc: C0/C1 controls and DEL.
    if c.is_control() {
        return true;
    }
    matches!(
        c,
        // Soft hyphen, Arabic number/bidi marks, Syriac abbreviation, etc.
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            // Zero-width space and bidi marks (ZWNJ/ZWJ handled above).
            | '\u{200B}'
            | '\u{200E}'..='\u{200F}'
            // Line/paragraph separators.
            | '\u{2028}'..='\u{2029}'
            // Bidi embeddings/overrides and isolates.
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            // BOM, interlinear annotations, and lesser-used format controls.
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            // Tag characters (deprecated tags and language tag).
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// Remove unsafe characters from `input`, converting tab to a single space (a
/// printable gap, not terminal movement). When `preserve_newlines` is true,
/// `\n` is retained unchanged so multi-line content keeps its layout. Otherwise
/// `\n` and `\r\n` become a single space, so line breaks separate words instead
/// of concatenating them; a lone `\r` is still removed. Returns a borrowed `Cow`
/// when nothing changes, so the common safe case is allocation-free.
pub fn sanitize_terminal_text(input: &str, preserve_newlines: bool) -> Cow<'_, str> {
    let needs_edit = input.chars().any(|c| {
        if c == '\t' {
            return true;
        }
        if c == '\n' {
            return !preserve_newlines;
        }
        is_unsafe_terminal_char(c)
    });
    if !needs_edit {
        return Cow::Borrowed(input);
    }

    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\t' {
            out.push(' ');
        } else if c == '\n' {
            out.push(if preserve_newlines { '\n' } else { ' ' });
        } else if c == '\r' {
            // Collapse CRLF into one space; a lone `\r` stays removed.
            if !preserve_newlines && chars.peek() == Some(&'\n') {
                chars.next();
                out.push(' ');
            }
        } else if !is_unsafe_terminal_char(c) {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::{is_unsafe_terminal_char, sanitize_terminal_text};
    use std::borrow::Cow;

    #[test]
    fn identifies_c0_del_and_c1_controls_as_unsafe() {
        for code in 0x00..=0x1f {
            assert!(is_unsafe_terminal_char(char::from_u32(code).unwrap()));
        }
        assert!(is_unsafe_terminal_char('\u{7f}'));
        for code in 0x80..=0x9f {
            assert!(is_unsafe_terminal_char(char::from_u32(code).unwrap()));
        }
    }

    #[test]
    fn removes_esc_and_osc_control_framing() {
        let input = "before\u{1b}]0;window title\u{7}\u{1b}\\after";

        let output = sanitize_terminal_text(input, false);

        assert_eq!(output, "before]0;window title\\after");
    }

    #[test]
    fn removes_line_separators_bidi_controls_and_invisible_format_chars() {
        let input = concat!(
            "a\u{2028}b\u{2029}c",
            "\u{202a}override\u{202c}",
            "\u{2066}isolate\u{2069}",
            "x\u{200b}\u{feff}\u{00ad}",
            "tag\u{e0001}end",
        );

        let output = sanitize_terminal_text(input, true);

        assert_eq!(output, "abcoverrideisolatextagend");
    }

    #[test]
    fn preserves_zwnj_zwj_and_shaped_script_and_family_emoji() {
        let input = "می\u{200c}رود خانواده: 👨‍👩‍👧‍👦";

        let output = sanitize_terminal_text(input, false);

        assert_eq!(output, input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }

    #[test]
    fn converts_tabs_to_one_space() {
        let output = sanitize_terminal_text("left\t\tmiddle\tright", false);

        assert_eq!(output, "left  middle right");
        assert!(matches!(output, Cow::Owned(_)));
    }

    #[test]
    fn preserves_newlines_only_when_requested() {
        let input = "first\nsecond\r\nthird";

        let preserved = sanitize_terminal_text(input, true);
        let removed = sanitize_terminal_text(input, false);

        assert_eq!(preserved, "first\nsecond\nthird");
        assert_eq!(removed, "first second third");
        assert!(!removed.contains("firstsecond"));
        assert!(!removed.contains("secondthird"));
    }

    #[test]
    fn replaces_single_line_breaks_with_spaces_without_concatenating_words() {
        assert_eq!(sanitize_terminal_text("left\nright", false), "left right");
        assert_eq!(sanitize_terminal_text("left\r\nright", false), "left right");
    }

    #[test]
    fn returns_borrowed_cow_for_unchanged_ascii_and_unicode() {
        let ascii = "plain terminal text";
        let unicode = "Résumé — Ελληνικά — 日本語";

        assert!(
            matches!(sanitize_terminal_text(ascii, false), Cow::Borrowed(value) if value == ascii)
        );
        assert!(
            matches!(sanitize_terminal_text(unicode, false), Cow::Borrowed(value) if value == unicode)
        );
    }

    #[test]
    fn returns_owned_cow_when_text_changes() {
        let output = sanitize_terminal_text("safe\u{200b}text", false);

        assert!(matches!(output, Cow::Owned(value) if value == "safetext"));
    }
}
