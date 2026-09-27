//! Unicode-safe multi-line input buffer: edits on char boundaries, never
//! mid-codepoint. InputView owns wrapping and cursor display coordinates.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub struct InputView {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

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
        self.normalize_cursor();
    }
    pub fn insert_str(&mut self, s: &str) {
        // one splice — per-char insert would memmove the tail for every
        // pasted character (O(paste × buffer) on a mid-buffer paste).
        // Strip C0 controls/ESC first: pasted text renders verbatim into
        // the terminal, so a hostile clipboard payload could inject
        // escape sequences (screen clear, OSC clipboard writes).
        let normalized = s.replace("\r\n", "\n");
        let cleaned: Vec<char> = normalized
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
        self.normalize_cursor();
    }
    /// Edits may join neighboring graphemes (ZWJ, marks, flag pairs).
    /// Keep the insertion point after the newly formed cluster.
    fn normalize_cursor(&mut self) {
        self.cursor = self
            .boundaries()
            .into_iter()
            .find(|&boundary| boundary >= self.cursor)
            .unwrap_or(self.chars.len());
    }
    fn boundaries(&self) -> Vec<usize> {
        let mut count = 0;
        let mut out = vec![0];
        for g in self.text().graphemes(true) {
            count += g.chars().count();
            out.push(count);
        }
        out
    }
    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.chars.drain(self.cursor..end);
        self.normalize_cursor();
    }
    pub fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        self.chars.drain(start..self.cursor);
        self.cursor = start;
        self.normalize_cursor();
    }
    pub fn left(&mut self) {
        self.cursor = self
            .boundaries()
            .into_iter()
            .rev()
            .find(|&i| i < self.cursor)
            .unwrap_or(0);
    }
    pub fn right(&mut self) {
        self.cursor = self
            .boundaries()
            .into_iter()
            .find(|&i| i > self.cursor)
            .unwrap_or(self.chars.len());
    }

    /// Wrap once for both the renderer and caret; cursor uses character offsets,
    /// display coordinates use grapheme widths. Reserve a row after an exact wrap.
    pub fn view(&self, width: usize) -> InputView {
        let width = width.max(1);
        let mut lines = vec![String::new()];
        let (mut offset, mut col) = (0, 0);
        let (mut cursor_row, mut cursor_col) = (0, 0);
        for g in self.text().graphemes(true) {
            let n = g.chars().count();
            let gw = UnicodeWidthStr::width(g);
            if g != "\n" && col + gw > width && col > 0 {
                lines.push(String::new());
                col = 0;
            }
            if (offset..offset + n).contains(&self.cursor) {
                cursor_row = lines.len() - 1 + usize::from(col >= width);
                cursor_col = if col >= width { 0 } else { col };
            }
            if g == "\n" {
                lines.push(String::new());
                col = 0;
            } else {
                lines
                    .last_mut()
                    .unwrap()
                    .push_str(if gw > width { "�" } else { g });
                col += gw.min(width);
            }
            offset += n;
        }
        if col >= width {
            lines.push(String::new());
            col = 0;
        }
        if self.cursor >= offset {
            cursor_row = lines.len() - 1;
            cursor_col = col;
        }
        InputView {
            lines,
            cursor_row,
            cursor_col,
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

/// Fit a single display line without splitting a grapheme. Ellipsis explicitly
/// marks omitted text; callers keep the original value for details and copying.
pub fn ellipsize(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if used + cells > width - 1 {
            break;
        }
        out.push_str(grapheme);
        used += cells;
    }
    out.push('…');
    out
}
