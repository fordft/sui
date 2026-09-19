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
        // one splice — per-char insert would memmove the tail for every
        // pasted character (O(paste × buffer) on a mid-buffer paste).
        // Strip C0 controls/ESC first: pasted text renders verbatim into
        // the terminal, so a hostile clipboard payload could inject
        // escape sequences (screen clear, OSC clipboard writes).
        let cleaned: Vec<char> = s
            .chars()
            .flat_map(|c| match c {
                '\n' | '\r' => vec!['\n'],
                '\t' => vec![' ', ' ', ' ', ' '],
                c if (c as u32) < 0x20 || c == '\u{7f}' => vec![],
                c => vec![c],
            })
            .collect();
        let n = cleaned.len();
        self.chars.splice(self.cursor..self.cursor, cleaned);
        self.cursor += n;
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
