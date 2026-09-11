//! Unicode-safe multi-line input buffer: edits on char boundaries, never
//! mid-codepoint. Width is the renderer's job.

#[derive(Default, Clone)]
pub struct Buf {
    chars: Vec<char>,
    pub cursor: usize,
}

impl Buf {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn from(s: &str) -> Self {
        Self {
            chars: s.chars().collect(),
            cursor: s.chars().count(),
        }
    }
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
    pub fn set(&mut self, s: &str) {
        *self = Buf::from(s);
    }
    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(c);
        }
    }
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }
    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }
    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }
    pub fn home(&mut self) {
        self.cursor = 0;
    }
    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }
}
