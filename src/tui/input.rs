//! A UTF-8-safe input buffer. Cursor offsets are always character boundaries.
#[derive(Default)]
pub struct Input {
    pub text: String,
    pub cursor: usize,
}
impl Input {
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }
    pub fn left(&mut self) {
        self.cursor = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }
    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }
    /// Move the cursor up one logical line (`'\n'`-separated), keeping the
    /// column where possible. Returns `true` when the cursor moved.
    pub fn up(&mut self) -> bool {
        self.move_vertical(-1)
    }

    /// Move the cursor down one logical line, keeping the column where
    /// possible. Returns `true` when the cursor moved.
    pub fn down(&mut self) -> bool {
        self.move_vertical(1)
    }

    /// Shared vertical movement. Columns are counted in characters so a line
    /// with multibyte text clamps correctly. Never splits a character.
    fn move_vertical(&mut self, delta: isize) -> bool {
        let text = &self.text;
        let line_start = text[..self.cursor]
            .rfind('\n')
            .map(|index| index + 1)
            .unwrap_or(0);
        let column = text[line_start..self.cursor].chars().count();
        let offset = |start: usize, end: usize| -> usize {
            text[start..end]
                .chars()
                .take(column)
                .map(char::len_utf8)
                .sum()
        };
        if delta < 0 {
            if line_start == 0 {
                return false;
            }
            let previous_end = line_start - 1; // the '\n' terminating the line above
            let previous_start = text[..previous_end]
                .rfind('\n')
                .map(|index| index + 1)
                .unwrap_or(0);
            self.cursor = previous_start + offset(previous_start, previous_end);
            true
        } else {
            let Some(line_end) = text[self.cursor..].find('\n').map(|i| self.cursor + i) else {
                return false;
            };
            let next_start = line_end + 1;
            let next_end = text[next_start..]
                .find('\n')
                .map(|i| next_start + i)
                .unwrap_or(text.len());
            self.cursor = next_start + offset(next_start, next_end);
            true
        }
    }
    pub fn backspace(&mut self) {
        let old = self.cursor;
        self.left();
        self.text.drain(self.cursor..old);
    }
    pub fn delete_word_backward(&mut self) {
        let end = self.cursor;
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            self.left();
        }
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.left();
        }
        self.text.drain(self.cursor..end);
    }
    pub fn word_left(&mut self) {
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            self.left();
        }
        while self.cursor > 0
            && self.text[..self.cursor]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.left();
        }
    }
    pub fn word_right(&mut self) {
        while self.cursor < self.text.len()
            && self.text[self.cursor..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            self.right();
        }
        while self.cursor < self.text.len()
            && self.text[self.cursor..]
                .chars()
                .next()
                .is_some_and(|c| !c.is_whitespace())
        {
            self.right();
        }
    }
    pub(super) fn delete(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.text.drain(self.cursor..self.cursor + c.len_utf8());
        }
    }
    pub(super) fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }
    pub(super) fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_multibyte_text_without_splitting_characters() {
        let mut input = Input::default();
        input.insert("a🍞é");
        input.left();
        input.backspace();
        assert_eq!(input.text, "aé");
        input.insert("漢");
        assert_eq!(input.text, "a漢é");
        input.delete();
        assert_eq!(input.text, "a漢");
    }
    #[test]
    fn moves_and_deletes_by_word_without_splitting_utf8() {
        let mut input = Input::default();
        input.insert("one  two🍞 three");
        input.word_left();
        assert_eq!(&input.text[..input.cursor], "one  two🍞 ");
        input.delete_word_backward();
        assert_eq!(input.text, "one  three");
        input.word_right();
        assert_eq!(input.cursor, input.text.len());
    }
    #[test]
    fn up_and_down_move_between_logical_lines_keeping_the_column() {
        let mut input = Input::default();
        input.insert("abc\ndefgh\nij");
        // Line 1 ("defgh") is chars 4..9; cursor at column 4 (byte 8).
        input.cursor = "abc\ndefgh".len() - 1;
        assert!(input.up(), "up() from line 1 should move");
        assert_eq!(
            input.cursor, 3,
            "column 4 exceeds line 0's 3 chars, so it clamps to line 0's end"
        );
        assert!(!input.up(), "up() on line 0 should return false");
        assert_eq!(input.cursor, 3, "a failed up() leaves the cursor unchanged");
        // down() keeps the column when it fits.
        assert!(input.down(), "down() from line 0 should move");
        assert_eq!(input.cursor, 7, "column 3 is kept on line 1");
        // down() clamps on the shorter last line.
        assert!(input.down(), "down() from line 1 should move");
        assert_eq!(
            input.cursor,
            input.text.len(),
            "column 3 clamps to line 2's 2 chars ('ij')"
        );
        assert!(!input.down(), "down() on the last line should return false");
        assert_eq!(
            input.cursor,
            input.text.len(),
            "a failed down() leaves the cursor unchanged"
        );
        // up() keeps the column when it fits.
        assert!(input.up(), "up() from line 2 should move");
        assert_eq!(input.cursor, 6, "column 2 is kept on line 1");
        assert!(input.up(), "up() from line 1 should move");
        assert_eq!(input.cursor, 2, "column 2 is kept on line 0");
    }
    #[test]
    fn vertical_movement_never_splits_multibyte_characters() {
        let mut input = Input::default();
        input.insert("a🍞é\n漢x");
        input.cursor = input.text.len(); // end of line 1, column 2
        assert!(input.up(), "up() across multibyte lines should move");
        assert_eq!(
            input.cursor,
            5, // a (1 byte) + 🍞 (4 bytes): column 2 keeps the whole char
            "column 2 on line 0 lands after 🍞"
        );
        assert!(
            input.text.is_char_boundary(input.cursor),
            "cursor must land on a char boundary"
        );
        assert_eq!(
            input.text, "a🍞é\n漢x",
            "vertical movement must not edit the text"
        );

        // Clamping: line 1 ("漢xé", 3 chars) is longer than line 0 ("é", 1 char).
        let mut input = Input::default();
        input.insert("é\n漢xé");
        input.cursor = input.text.len(); // end of line 1, column 3
        assert!(input.up(), "up() should move");
        assert_eq!(
            input.cursor, 2,
            "column 3 clamps to line 0's single char é (2 bytes)"
        );
        assert!(
            input.text.is_char_boundary(input.cursor),
            "cursor must land on a char boundary"
        );
        assert_eq!(
            input.text, "é\n漢xé",
            "vertical movement must not edit the text"
        );
    }
}
