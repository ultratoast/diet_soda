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
    pub fn backspace(&mut self) {
        let old = self.cursor;
        self.left();
        self.text.drain(self.cursor..old);
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
}
