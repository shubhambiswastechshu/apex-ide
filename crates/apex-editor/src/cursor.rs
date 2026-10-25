//! Caret and selection movement.
//!
//! Pure functions over a [`Buffer`]: a UI keeps a [`Cursor`] per editor and
//! asks [`move_cursor`] where it goes for each key. Positions are char
//! indices; columns shown on screen are *visual* columns, where a tab
//! advances to the next multiple of [`TAB_WIDTH`].

use std::ops::Range;

use crate::Buffer;

/// Width of a tab stop in visual columns.
pub const TAB_WIDTH: usize = 4;

/// A selection: `anchor` stays put, `head` is where the caret is. Both are
/// char indices; `anchor == head` means just a caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Selection {
    /// The end that stays put while extending.
    pub anchor: usize,
    /// The caret.
    pub head: usize,
}

impl Selection {
    /// A collapsed selection at `at`.
    pub fn caret(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    /// A selection from `anchor` to `head`.
    pub fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    /// Whether nothing is selected.
    pub fn is_empty(self) -> bool {
        self.anchor == self.head
    }

    /// The covered range, start first.
    pub fn range(self) -> Range<usize> {
        self.anchor.min(self.head)..self.anchor.max(self.head)
    }

    /// Clamps both ends into `0..=len`.
    pub fn clamped(self, len: usize) -> Self {
        Self {
            anchor: self.anchor.min(len),
            head: self.head.min(len),
        }
    }
}

/// A selection plus the visual column vertical moves try to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// The selection.
    pub sel: Selection,
    /// Column remembered across Up/Down moves through shorter lines.
    pub preferred_col: Option<usize>,
}

impl Cursor {
    /// A cursor with a collapsed selection at `at`.
    pub fn at(at: usize) -> Self {
        Self {
            sel: Selection::caret(at),
            preferred_col: None,
        }
    }

    /// A cursor with `sel`.
    pub fn with(sel: Selection) -> Self {
        Self {
            sel,
            preferred_col: None,
        }
    }
}

/// Where a key moves the caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    /// One char left (or to the selection start).
    Left,
    /// One char right (or to the selection end).
    Right,
    /// One line up.
    Up,
    /// One line down.
    Down,
    /// To the previous word start.
    WordLeft,
    /// To the next word end.
    WordRight,
    /// Smart Home: first non-blank char, then column 0.
    LineStart,
    /// End of the line.
    LineEnd,
    /// Start of the document.
    DocStart,
    /// End of the document.
    DocEnd,
    /// Up by this many lines.
    PageUp(usize),
    /// Down by this many lines.
    PageDown(usize),
}

/// The visual column of char column `char_col` in `line` (tabs expanded).
pub fn visual_col(line: &str, char_col: usize) -> usize {
    let mut v = 0;
    for c in line.chars().take(char_col) {
        v = advance(v, c);
    }
    v
}

fn advance(v: usize, c: char) -> usize {
    if c == '\t' {
        (v / TAB_WIDTH + 1) * TAB_WIDTH
    } else {
        v + 1
    }
}

/// The char column nearest to visual column `vcol` in `line` (clamped to
/// the line end). A click in the right half of a tab lands after it.
pub fn char_col_for_visual(line: &str, vcol: usize) -> usize {
    let mut v = 0;
    for (i, c) in line.chars().enumerate() {
        let next = advance(v, c);
        if vcol < next {
            return if vcol - v > (next - v - 1) / 2 && next - v > 1 {
                i + 1
            } else {
                i
            };
        }
        v = next;
    }
    line.chars().count()
}

/// Width of `line` in visual columns.
pub fn display_width(line: &str) -> usize {
    line.chars().fold(0, advance)
}

/// `line` with tabs expanded to spaces (for painting).
pub fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_owned();
    }
    let mut out = String::with_capacity(line.len() + 8);
    let mut v = 0;
    for c in line.chars() {
        let next = advance(v, c);
        if c == '\t' {
            out.extend(std::iter::repeat_n(' ', next - v));
        } else {
            out.push(c);
        }
        v = next;
    }
    out
}

/// Character classes used for word movement and double-click selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    /// Letters, digits and `_`.
    Word,
    /// Spaces and tabs.
    Space,
    /// A line break.
    Newline,
    /// Anything else.
    Punct,
}

/// The class of `c`.
pub fn char_class(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c == '\n' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Punct
    }
}

fn char_at(buffer: &Buffer, i: usize) -> Option<char> {
    (i < buffer.len_chars()).then(|| buffer.rope().char(i))
}

/// Line and char column of `idx` (clamped into the buffer).
pub fn line_col(buffer: &Buffer, idx: usize) -> (usize, usize) {
    let idx = idx.min(buffer.len_chars());
    let rope = buffer.rope();
    let line = rope.char_to_line(idx);
    (line, idx - rope.line_to_char(line))
}

/// Char index of `line` / visual column `vcol`, both clamped.
pub fn index_at_visual(buffer: &Buffer, line: usize, vcol: usize) -> usize {
    let line = line.min(buffer.len_lines().saturating_sub(1));
    let text = buffer.line(line).unwrap_or_default();
    let start = buffer.rope().line_to_char(line);
    start + char_col_for_visual(&text, vcol)
}

/// Visual column of char index `idx`.
pub fn visual_col_of(buffer: &Buffer, idx: usize) -> usize {
    let (line, col) = line_col(buffer, idx);
    visual_col(&buffer.line(line).unwrap_or_default(), col)
}

/// Start of the previous word from `idx` (Ctrl+Left).
pub fn word_left(buffer: &Buffer, idx: usize) -> usize {
    let mut i = idx.min(buffer.len_chars());
    if i == 0 {
        return 0;
    }
    if char_at(buffer, i - 1) == Some('\n') {
        return i - 1;
    }
    while i > 0 && char_at(buffer, i - 1).map(char_class) == Some(CharClass::Space) {
        i -= 1;
    }
    let Some(class) = i
        .checked_sub(1)
        .and_then(|p| char_at(buffer, p))
        .map(char_class)
    else {
        return i;
    };
    if class == CharClass::Newline {
        return i;
    }
    while i > 0 && char_at(buffer, i - 1).map(char_class) == Some(class) {
        i -= 1;
    }
    i
}

/// End of the next word from `idx` (Ctrl+Right).
pub fn word_right(buffer: &Buffer, idx: usize) -> usize {
    let len = buffer.len_chars();
    let mut i = idx.min(len);
    if i == len {
        return len;
    }
    if char_at(buffer, i) == Some('\n') {
        return i + 1;
    }
    while i < len && char_at(buffer, i).map(char_class) == Some(CharClass::Space) {
        i += 1;
    }
    let Some(class) = char_at(buffer, i).map(char_class) else {
        return i;
    };
    if class == CharClass::Newline {
        return i;
    }
    while i < len && char_at(buffer, i).map(char_class) == Some(class) {
        i += 1;
    }
    i
}

/// The word (or run of same-class chars) under `idx`, for double-click.
pub fn word_range_at(buffer: &Buffer, idx: usize) -> Range<usize> {
    let len = buffer.len_chars();
    let idx = idx.min(len);
    let class_here = char_at(buffer, idx).map(char_class);
    let class_before = idx
        .checked_sub(1)
        .and_then(|p| char_at(buffer, p))
        .map(char_class);
    let class = match (class_here, class_before) {
        (Some(CharClass::Word), _) => CharClass::Word,
        (_, Some(CharClass::Word)) => CharClass::Word,
        (Some(c), _) if c != CharClass::Newline => c,
        _ => return idx..idx,
    };
    let mut start = idx;
    while start > 0 && char_at(buffer, start - 1).map(char_class) == Some(class) {
        start -= 1;
    }
    let mut end = idx;
    while end < len && char_at(buffer, end).map(char_class) == Some(class) {
        end += 1;
    }
    start..end
}

/// The identifier ending at `idx` as `(start, text)`; empty when the char
/// before `idx` is not part of an identifier.
pub fn word_before(buffer: &Buffer, idx: usize) -> (usize, String) {
    let idx = idx.min(buffer.len_chars());
    let mut start = idx;
    while start > 0 && char_at(buffer, start - 1).map(char_class) == Some(CharClass::Word) {
        start -= 1;
    }
    (start, buffer.slice(start..idx).unwrap_or_default())
}

/// Moves `idx` off the middle of a `\r\n` pair (kept verbatim in files with
/// mixed line endings): to the `\r` when moving `back`, else past the `\n`.
/// Typing between the two would leave a lone `\r`, which splits the line.
fn off_crlf(buffer: &Buffer, idx: usize, back: bool) -> usize {
    let mid =
        idx > 0 && char_at(buffer, idx - 1) == Some('\r') && char_at(buffer, idx) == Some('\n');
    match (mid, back) {
        (false, _) => idx,
        (true, true) => idx - 1,
        (true, false) => idx + 1,
    }
}

/// Moves `cursor` by `motion`. With `extend` the anchor stays (Shift);
/// otherwise the selection collapses.
pub fn move_cursor(buffer: &Buffer, cursor: Cursor, motion: Motion, extend: bool) -> Cursor {
    let len = buffer.len_chars();
    let sel = cursor.sel.clamped(len);
    let head = sel.head;
    let vertical = |lines_up: isize| -> (usize, Option<usize>) {
        let (line, _) = line_col(buffer, head);
        let want = cursor
            .preferred_col
            .unwrap_or_else(|| visual_col_of(buffer, head));
        let last = buffer.len_lines().saturating_sub(1) as isize;
        let target = line as isize + lines_up;
        if target < 0 {
            return (0, Some(want));
        }
        if target > last {
            return (len, Some(want));
        }
        (index_at_visual(buffer, target as usize, want), Some(want))
    };
    let (new_head, preferred) = match motion {
        Motion::Left if !extend && !sel.is_empty() => (sel.range().start, None),
        Motion::Right if !extend && !sel.is_empty() => (sel.range().end, None),
        Motion::Left => (head.saturating_sub(1), None),
        Motion::Right => ((head + 1).min(len), None),
        Motion::Up => vertical(-1),
        Motion::Down => vertical(1),
        Motion::PageUp(n) => vertical(-(n.max(1) as isize)),
        Motion::PageDown(n) => vertical(n.max(1) as isize),
        Motion::WordLeft => (word_left(buffer, head), None),
        Motion::WordRight => (word_right(buffer, head), None),
        Motion::LineStart => {
            let (line, col) = line_col(buffer, head);
            let text = buffer.line(line).unwrap_or_default();
            let first = text.chars().take_while(|c| *c == ' ' || *c == '\t').count();
            let start = buffer.rope().line_to_char(line);
            let col = if col == first { 0 } else { first };
            (start + col, None)
        }
        Motion::LineEnd => {
            let (line, _) = line_col(buffer, head);
            let start = buffer.rope().line_to_char(line);
            (start + buffer.line_len(line).unwrap_or(0), None)
        }
        Motion::DocStart => (0, None),
        Motion::DocEnd => (len, None),
    };
    let new_head = off_crlf(buffer, new_head, new_head < head);
    let anchor = if extend { sel.anchor } else { new_head };
    Cursor {
        sel: Selection::new(anchor, new_head),
        preferred_col: preferred,
    }
}

/// The bracket matching the one at or just before `idx`, as
/// `(bracket, partner)` char indices. Scans at most `limit` chars and
/// ignores brackets inside strings only approximately (it does not parse).
pub fn matching_bracket(buffer: &Buffer, idx: usize, limit: usize) -> Option<(usize, usize)> {
    let pairs = [('(', ')'), ('[', ']'), ('{', '}')];
    let candidates = [Some(idx), idx.checked_sub(1)];
    for pos in candidates.into_iter().flatten() {
        let Some(c) = char_at(buffer, pos) else {
            continue;
        };
        for (open, close) in pairs {
            if c == open {
                let mut depth = 0usize;
                let end = (pos + limit).min(buffer.len_chars());
                for (i, ch) in buffer.rope().chars_at(pos).enumerate().take(end - pos) {
                    if ch == open {
                        depth += 1;
                    } else if ch == close {
                        depth -= 1;
                        if depth == 0 {
                            return Some((pos, pos + i));
                        }
                    }
                }
                return None;
            }
            if c == close {
                let mut depth = 0usize;
                let mut it = buffer.rope().chars_at(pos + 1);
                let mut i = pos + 1;
                while i > 0 && pos + 1 - i < limit {
                    let Some(ch) = it.prev() else { break };
                    i -= 1;
                    if ch == close {
                        depth += 1;
                    } else if ch == open {
                        depth -= 1;
                        if depth == 0 {
                            return Some((pos, i));
                        }
                    }
                }
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(text: &str, at: usize, m: Motion, extend: bool) -> Selection {
        let b = Buffer::from_text(text);
        move_cursor(&b, Cursor::at(at), m, extend).sel
    }

    #[test]
    fn caret_never_stops_inside_a_verbatim_crlf() {
        // Mixed line endings keep `\r\n` in the text: "ab\r\ncd\nef".
        let text = "ab\r\ncd\nef";
        assert!(Buffer::from_text(text).has_mixed_line_endings());
        assert_eq!(mv(text, 0, Motion::LineEnd, false), Selection::caret(2));
        assert_eq!(mv(text, 2, Motion::Right, false), Selection::caret(4));
        assert_eq!(mv(text, 4, Motion::Left, false), Selection::caret(2));
        assert_eq!(mv(text, 4, Motion::WordLeft, false), Selection::caret(2));
        assert_eq!(mv(text, 5, Motion::Up, false), Selection::caret(1));
    }

    #[test]
    fn visual_columns_expand_tabs() {
        assert_eq!(visual_col("\tab", 1), 4);
        assert_eq!(visual_col("a\tb", 2), 4);
        assert_eq!(display_width("a\tbc"), 6);
        assert_eq!(char_col_for_visual("\tab", 4), 1);
        assert_eq!(char_col_for_visual("\tab", 1), 0);
        assert_eq!(char_col_for_visual("\tab", 3), 1);
        assert_eq!(char_col_for_visual("ab", 99), 2);
        assert_eq!(expand_tabs("a\tb"), "a   b");
    }

    #[test]
    fn left_right_collapse_selection() {
        let b = Buffer::from_text("hello");
        let c = Cursor::with(Selection::new(1, 4));
        assert_eq!(
            move_cursor(&b, c, Motion::Left, false).sel,
            Selection::caret(1)
        );
        assert_eq!(
            move_cursor(&b, c, Motion::Right, false).sel,
            Selection::caret(4)
        );
        assert_eq!(
            move_cursor(&b, c, Motion::Right, true).sel,
            Selection::new(1, 5)
        );
        assert_eq!(mv("ab", 0, Motion::Left, false), Selection::caret(0));
        assert_eq!(mv("ab", 2, Motion::Right, false), Selection::caret(2));
    }

    #[test]
    fn vertical_moves_keep_preferred_column() {
        let b = Buffer::from_text("abcdef\nab\nabcdef");
        let c = move_cursor(&b, Cursor::at(5), Motion::Down, false);
        assert_eq!(c.sel.head, 9); // end of "ab"
        let c = move_cursor(&b, c, Motion::Down, false);
        assert_eq!(c.sel.head, 10 + 5);
        let c = move_cursor(&b, c, Motion::Down, false);
        assert_eq!(c.sel.head, b.len_chars());
        let c = move_cursor(&b, Cursor::at(3), Motion::Up, true);
        assert_eq!(c.sel, Selection::new(3, 0));
        let c = move_cursor(&b, Cursor::at(2), Motion::PageDown(10), false);
        assert_eq!(c.sel.head, b.len_chars());
    }

    #[test]
    fn vertical_moves_respect_tabs() {
        let b = Buffer::from_text("\tx\n    y");
        // After the tab (visual col 4) down to the line with 4 spaces.
        let c = move_cursor(&b, Cursor::at(1), Motion::Down, false);
        assert_eq!(c.sel.head, 3 + 4);
    }

    #[test]
    fn word_moves() {
        let t = "foo.bar  baz\nqux";
        let b = Buffer::from_text(t);
        assert_eq!(word_right(&b, 0), 3);
        assert_eq!(word_right(&b, 3), 4);
        assert_eq!(word_right(&b, 4), 7);
        assert_eq!(word_right(&b, 7), 12);
        assert_eq!(word_right(&b, 12), 13);
        assert_eq!(word_left(&b, 13), 12);
        assert_eq!(word_left(&b, 12), 9);
        assert_eq!(word_left(&b, 9), 4);
        assert_eq!(word_left(&b, 4), 3);
        assert_eq!(word_left(&b, 3), 0);
        assert_eq!(mv(t, 0, Motion::WordRight, true), Selection::new(0, 3));
    }

    #[test]
    fn smart_home_and_end() {
        let t = "    pass\nx";
        assert_eq!(mv(t, 6, Motion::LineStart, false).head, 4);
        assert_eq!(mv(t, 4, Motion::LineStart, false).head, 0);
        assert_eq!(mv(t, 0, Motion::LineEnd, false).head, 8);
        assert_eq!(mv(t, 3, Motion::DocEnd, true), Selection::new(3, 10));
        assert_eq!(mv(t, 3, Motion::DocStart, false), Selection::caret(0));
    }

    #[test]
    fn double_click_word_and_word_before() {
        let b = Buffer::from_text("x = foo_bar(1)");
        assert_eq!(word_range_at(&b, 6), 4..11);
        assert_eq!(word_range_at(&b, 11), 4..11);
        assert_eq!(word_before(&b, 8), (4, "foo_".to_owned()));
        assert_eq!(word_before(&b, 3).1, "");
    }

    #[test]
    fn brackets_match_both_ways() {
        let b = Buffer::from_text("f(a[1], (b))");
        assert_eq!(matching_bracket(&b, 1, 1000), Some((1, 11)));
        assert_eq!(matching_bracket(&b, 12, 1000), Some((11, 1)));
        assert_eq!(matching_bracket(&b, 3, 1000), Some((3, 5)));
        assert_eq!(matching_bracket(&b, 0, 1000), None);
    }

    #[test]
    fn line_edits_are_logged() {
        let mut b = Buffer::from_text("a\nb\nc");
        let r = b.revision();
        b.insert(1, "\nx").unwrap();
        b.delete(0..2).unwrap();
        let edits = b.line_edits_since(r).unwrap();
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].start_line, 0);
        assert_eq!(edits[0].inserted_breaks, 1);
        assert_eq!(edits[1].removed_breaks, 1);
        assert_eq!(b.line_edits_since(b.revision()).unwrap().len(), 0);
        assert!(b.line_edits_since(r + 99).is_none());
    }
}
