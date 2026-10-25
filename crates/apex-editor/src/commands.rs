//! Editing commands on a [`Buffer`] and a [`Selection`]: typing with
//! auto-closed brackets and quotes, Backspace/Delete, Enter with
//! indentation, Tab/Shift+Tab, comment toggling, clipboard helpers.
//!
//! Every command returns the new selection; the buffer records undo steps.

use std::ops::Range;

use crate::cursor::{char_class, word_left, word_right, CharClass, Selection};
use crate::{indent_for_newline_in, Buffer, EditorError, Language, Result, INDENT_UNIT};

/// Bracket and quote pairs that auto-close.
const PAIRS: [(char, char); 6] = [
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
];

fn char_at(buffer: &Buffer, i: usize) -> Option<char> {
    (i < buffer.len_chars()).then(|| buffer.rope().char(i))
}

fn closing_for(c: char) -> Option<char> {
    PAIRS.iter().find(|(o, _)| *o == c).map(|(_, cl)| *cl)
}

fn is_closer(c: char) -> bool {
    PAIRS.iter().any(|(_, cl)| *cl == c)
}

/// Replaces `range` with `text` as its own undo step.
fn replace_step(buffer: &mut Buffer, range: Range<usize>, text: &str) -> Result<()> {
    buffer.break_undo_group();
    buffer.replace_range(range, text)?;
    buffer.break_undo_group();
    Ok(())
}

fn clamp(buffer: &Buffer, sel: Selection) -> Selection {
    sel.clamped(buffer.len_chars())
}

/// Types `text` (one or more chars from the keyboard). With `auto_close`,
/// an opening bracket or quote also inserts its partner (wrapping a
/// non-empty selection), and typing a closer that is already next to the
/// caret steps over it.
pub fn type_text(
    buffer: &mut Buffer,
    sel: Selection,
    text: &str,
    auto_close: bool,
) -> Result<Selection> {
    let sel = clamp(buffer, sel);
    let range = sel.range();
    let mut chars = text.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    if let (true, Some(c)) = (auto_close, single) {
        let next = char_at(buffer, range.end);
        let prev = range.start.checked_sub(1).and_then(|p| char_at(buffer, p));
        // Step over an existing closer.
        if sel.is_empty() && is_closer(c) && next == Some(c) {
            let is_quote = matches!(c, '"' | '\'' | '`');
            if !is_quote || prev != Some('\\') {
                return Ok(Selection::caret(range.end + 1));
            }
        }
        if let Some(close) = closing_for(c) {
            let is_quote = close == c;
            if !sel.is_empty() {
                let inner = buffer.slice(range.clone())?;
                replace_step(buffer, range.clone(), &format!("{c}{inner}{close}"))?;
                let n = inner.chars().count();
                return Ok(Selection::new(range.start + 1, range.start + 1 + n));
            }
            let next_ok =
                next.is_none_or(|n| n.is_whitespace() || is_closer(n) || n == ',' || n == ':');
            let prev_ok = !is_quote
                || prev.is_none_or(|p| char_class(p) != CharClass::Word && p != c && p != '\\');
            if next_ok && prev_ok {
                buffer.replace_range(range.clone(), &format!("{c}{close}"))?;
                return Ok(Selection::caret(range.start + 1));
            }
        }
    }
    let inserted = replaced_len(buffer, range.clone(), text, false)?;
    Ok(Selection::caret(range.start + inserted))
}

/// Inserts `text` (for example from the clipboard) as one undo step.
pub fn insert_text(buffer: &mut Buffer, sel: Selection, text: &str) -> Result<Selection> {
    let range = clamp(buffer, sel).range();
    let inserted = replaced_len(buffer, range.clone(), text, true)?;
    Ok(Selection::caret(range.start + inserted))
}

/// Replaces `range` with `text` (as its own undo step with `step`) and
/// returns how many chars were inserted. That can be fewer than `text` has:
/// the buffer stores `\r\n` as `\n`, so pasted Windows text shrinks.
fn replaced_len(buffer: &mut Buffer, range: Range<usize>, text: &str, step: bool) -> Result<usize> {
    let before = buffer.len_chars();
    if step {
        replace_step(buffer, range.clone(), text)?;
    } else {
        buffer.replace_range(range.clone(), text)?;
    }
    Ok(buffer.len_chars() + range.len() - before)
}

/// Backspace: deletes the selection, an empty auto-closed pair around the
/// caret, one indent level inside leading whitespace, or one char.
pub fn backspace(buffer: &mut Buffer, sel: Selection) -> Result<Selection> {
    let sel = clamp(buffer, sel);
    if !sel.is_empty() {
        let r = sel.range();
        buffer.replace_range(r.clone(), "")?;
        return Ok(Selection::caret(r.start));
    }
    let at = sel.head;
    if at == 0 {
        return Ok(sel);
    }
    let prev = char_at(buffer, at - 1);
    let next = char_at(buffer, at);
    if let (Some(p), Some(n)) = (prev, next) {
        if closing_for(p) == Some(n) {
            buffer.replace_range(at - 1..at + 1, "")?;
            return Ok(Selection::caret(at - 1));
        }
    }
    let line = buffer.rope().char_to_line(at);
    let line_start = buffer.rope().line_to_char(line);
    let before = buffer.slice(line_start..at)?;
    if !before.is_empty() && before.chars().all(|c| c == ' ') {
        let n = before.chars().count();
        let unit = INDENT_UNIT.len();
        let cut = match n % unit {
            0 => unit,
            r => r,
        };
        buffer.replace_range(at - cut..at, "")?;
        return Ok(Selection::caret(at - cut));
    }
    // A verbatim `\r\n` (mixed line endings) goes as one break; leaving a
    // lone `\r` would still split the line for Python.
    let cut = if prev == Some('\n') && at >= 2 && char_at(buffer, at - 2) == Some('\r') {
        2
    } else {
        1
    };
    buffer.replace_range(at - cut..at, "")?;
    Ok(Selection::caret(at - cut))
}

/// Delete: deletes the selection or the char after the caret.
pub fn delete_forward(buffer: &mut Buffer, sel: Selection) -> Result<Selection> {
    let sel = clamp(buffer, sel);
    let r = if sel.is_empty() {
        let crlf =
            char_at(buffer, sel.head) == Some('\r') && char_at(buffer, sel.head + 1) == Some('\n');
        let n = if crlf { 2 } else { 1 };
        sel.head..(sel.head + n).min(buffer.len_chars())
    } else {
        sel.range()
    };
    buffer.replace_range(r.clone(), "")?;
    Ok(Selection::caret(r.start))
}

/// Ctrl+Backspace / Ctrl+Delete: deletes to the previous word start or the
/// next word end (or the selection).
pub fn delete_word(buffer: &mut Buffer, sel: Selection, forward: bool) -> Result<Selection> {
    let sel = clamp(buffer, sel);
    let r = if !sel.is_empty() {
        sel.range()
    } else if forward {
        sel.head..word_right(buffer, sel.head)
    } else {
        word_left(buffer, sel.head)..sel.head
    };
    replace_step(buffer, r.clone(), "")?;
    Ok(Selection::caret(r.start))
}

/// Enter: a line break plus the indentation the language calls for. Between
/// a bracket pair (`(|)`) the closer moves to its own line.
pub fn newline(buffer: &mut Buffer, sel: Selection) -> Result<Selection> {
    let range = clamp(buffer, sel).range();
    let line = buffer.rope().char_to_line(range.start);
    let line_start = buffer.rope().line_to_char(line);
    let before = buffer.slice(line_start..range.start)?;
    let indent = indent_for_newline_in(buffer.language(), &before);
    let prev = range.start.checked_sub(1).and_then(|p| char_at(buffer, p));
    let next = char_at(buffer, range.end);
    let between = matches!(
        (prev, next),
        (Some('('), Some(')')) | (Some('['), Some(']')) | (Some('{'), Some('}'))
    );
    let insert = if between {
        let outer: String = before
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let inner = if indent.len() > outer.len() {
            indent.clone()
        } else {
            format!("{outer}{INDENT_UNIT}")
        };
        let text = format!("\n{inner}\n{outer}");
        replace_step(buffer, range.clone(), &text)?;
        return Ok(Selection::caret(range.start + 1 + inner.chars().count()));
    } else {
        format!("\n{indent}")
    };
    replace_step(buffer, range.clone(), &insert)?;
    Ok(Selection::caret(range.start + insert.chars().count()))
}

/// The lines touched by a range; a range ending at a line start excludes
/// that line.
fn line_span(buffer: &Buffer, range: &Range<usize>) -> (usize, usize) {
    let rope = buffer.rope();
    let first = rope.char_to_line(range.start);
    let end_line = rope.char_to_line(range.end);
    let last = if end_line > first && rope.line_to_char(end_line) == range.end {
        end_line - 1
    } else {
        end_line
    };
    (first, last)
}

fn lines_range(buffer: &Buffer, first: usize, last: usize) -> Result<Range<usize>> {
    Ok(buffer.line_to_char(first)?..buffer.line_to_char(last)? + buffer.line_len(last)?)
}

/// Tab: inserts spaces to the next indent stop, or indents every line of a
/// multi-line selection.
pub fn indent(buffer: &mut Buffer, sel: Selection) -> Result<Selection> {
    let sel = clamp(buffer, sel);
    let range = sel.range();
    let rope = buffer.rope();
    if rope.char_to_line(range.start) == rope.char_to_line(range.end) {
        let line_start = rope.line_to_char(rope.char_to_line(range.start));
        let col = crate::cursor::visual_col(&buffer.slice(line_start..range.start)?, usize::MAX);
        let n = INDENT_UNIT.len() - col % INDENT_UNIT.len();
        let spaces = " ".repeat(n);
        replace_step(buffer, range.clone(), &spaces)?;
        return Ok(Selection::caret(range.start + n));
    }
    let (first, last) = line_span(buffer, &range);
    let block = lines_range(buffer, first, last)?;
    let old = buffer.slice(block.clone())?;
    let new: Vec<String> = old
        .split('\n')
        .map(|l| {
            if l.trim().is_empty() {
                l.to_owned()
            } else {
                format!("{INDENT_UNIT}{l}")
            }
        })
        .collect();
    let new = new.join("\n");
    replace_step(buffer, block.clone(), &new)?;
    Ok(Selection::new(
        block.start,
        block.start + new.chars().count(),
    ))
}

/// Shift+Tab: removes one indent level from every touched line.
pub fn dedent(buffer: &mut Buffer, sel: Selection) -> Result<Selection> {
    let sel = clamp(buffer, sel);
    let range = sel.range();
    let (first, last) = line_span(buffer, &range);
    let block = lines_range(buffer, first, last)?;
    let old = buffer.slice(block.clone())?;
    let mut removed_first = 0;
    let new: Vec<&str> = old
        .split('\n')
        .enumerate()
        .map(|(i, l)| {
            let cut = if l.starts_with('\t') {
                1
            } else {
                l.chars()
                    .take(INDENT_UNIT.len())
                    .take_while(|c| *c == ' ')
                    .count()
            };
            if i == 0 {
                removed_first = cut;
            }
            &l[cut..]
        })
        .collect();
    let new = new.join("\n");
    if new == old {
        return Ok(sel);
    }
    replace_step(buffer, block.clone(), &new)?;
    if first == last {
        let caret = sel.head.saturating_sub(removed_first).max(block.start);
        let anchor = sel.anchor.saturating_sub(removed_first).max(block.start);
        return Ok(Selection::new(anchor, caret));
    }
    Ok(Selection::new(
        block.start,
        block.start + new.chars().count(),
    ))
}

/// Ctrl+/: toggles the language's line comment on the touched lines.
/// Returns `None` if the language has no line comment.
pub fn toggle_comment(buffer: &mut Buffer, sel: Selection) -> Result<Option<Selection>> {
    let Some(token) = buffer.language().line_comment_token() else {
        return Ok(None);
    };
    let sel = clamp(buffer, sel);
    let range = sel.range();
    let (first, last) = line_span(buffer, &range);
    let before = lines_range(buffer, first, last)?;
    buffer.toggle_comment(first, last, token)?;
    let after = lines_range(buffer, first, last)?;
    if sel.is_empty() {
        let caret = (range.start + after.end).saturating_sub(before.end);
        return Ok(Some(Selection::caret(caret.clamp(after.start, after.end))));
    }
    Ok(Some(Selection::new(after.start, after.end)))
}

/// Text to copy: the selection, or the whole caret line (with its line
/// break) when nothing is selected. The flag tells whether it is a line.
pub fn copy_text(buffer: &Buffer, sel: Selection) -> (String, bool) {
    let sel = sel.clamped(buffer.len_chars());
    if !sel.is_empty() {
        return (buffer.slice(sel.range()).unwrap_or_default(), false);
    }
    let line = buffer.rope().char_to_line(sel.head);
    (buffer.rope().line(line).to_string(), true)
}

/// Cut: removes what [`copy_text`] returns.
pub fn cut(buffer: &mut Buffer, sel: Selection) -> Result<(String, Selection)> {
    let sel = clamp(buffer, sel);
    let (text, whole_line) = copy_text(buffer, sel);
    let range = if whole_line {
        let line = buffer.rope().char_to_line(sel.head);
        let start = buffer.rope().line_to_char(line);
        start..start + text.chars().count()
    } else {
        sel.range()
    };
    replace_step(buffer, range.clone(), "")?;
    Ok((text, Selection::caret(range.start.min(buffer.len_chars()))))
}

/// Ctrl+D: duplicates the selection, or the caret line.
pub fn duplicate(buffer: &mut Buffer, sel: Selection) -> Result<Selection> {
    let sel = clamp(buffer, sel);
    if !sel.is_empty() {
        let r = sel.range();
        let text = buffer.slice(r.clone())?;
        replace_step(buffer, r.end..r.end, &text)?;
        let n = text.chars().count();
        return Ok(Selection::new(r.end, r.end + n));
    }
    let line = buffer.rope().char_to_line(sel.head);
    let start = buffer.line_to_char(line)?;
    let end = start + buffer.line_len(line)?;
    let text = buffer.slice(start..end)?;
    replace_step(buffer, end..end, &format!("\n{text}"))?;
    Ok(Selection::caret(sel.head + text.chars().count() + 1))
}

/// Undo; the new caret, or `None` if there was nothing to undo.
pub fn undo(buffer: &mut Buffer) -> Option<Selection> {
    buffer
        .undo()
        .map(|c| Selection::caret(c.min(buffer.len_chars())))
}

/// Redo; the new caret, or `None` if there was nothing to redo.
pub fn redo(buffer: &mut Buffer) -> Option<Selection> {
    buffer
        .redo()
        .map(|c| Selection::caret(c.min(buffer.len_chars())))
}

/// Validates that `range` lies in the buffer.
pub fn check_range(buffer: &Buffer, range: &Range<usize>) -> Result<()> {
    if range.start > range.end || range.end > buffer.len_chars() {
        return Err(EditorError::InvalidRange {
            start: range.start,
            end: range.end,
            len: buffer.len_chars(),
        });
    }
    Ok(())
}

/// Whether `language` wants auto-closed brackets and quotes.
pub fn auto_closes(language: Language) -> bool {
    !matches!(language, Language::Plain | Language::Markdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn py(text: &str) -> Buffer {
        let mut b = Buffer::from_text(text);
        b.set_path(Some(PathBuf::from("x.py")));
        b
    }

    #[test]
    fn brackets_auto_close_and_step_over() {
        let mut b = py("f");
        let s = type_text(&mut b, Selection::caret(1), "(", true).unwrap();
        assert_eq!((b.text().as_str(), s), ("f()", Selection::caret(2)));
        let s = type_text(&mut b, s, "x", true).unwrap();
        let s = type_text(&mut b, s, ")", true).unwrap();
        assert_eq!((b.text().as_str(), s), ("f(x)", Selection::caret(4)));
    }

    #[test]
    fn pasting_windows_text_puts_the_caret_after_it() {
        // `\r\n` is stored as `\n`: the caret must not overshoot (at the end
        // of the buffer it used to land past the last char).
        let mut b = py("x");
        let s = insert_text(&mut b, Selection::caret(1), "a\r\nb\r\n").unwrap();
        assert_eq!(b.text(), "xa\nb\n");
        assert_eq!(s, Selection::caret(b.len_chars()));
        let s = type_text(&mut b, s, "c\r\nd", false).unwrap();
        assert_eq!(s, Selection::caret(b.len_chars()));
        // Replacing a selection with identical text still moves past it.
        let mut b = py("abc");
        let s = insert_text(&mut b, Selection::new(0, 2), "ab").unwrap();
        assert_eq!(s, Selection::caret(2));
    }

    #[test]
    fn deleting_a_verbatim_crlf_removes_both_chars() {
        // A file with mixed endings keeps `\r\n`; joining lines must not
        // leave a lone `\r` (Python reads it as a line break).
        let mut b = py("a\r\nb\nc");
        assert!(b.has_mixed_line_endings());
        let s = backspace(&mut b, Selection::caret(3)).unwrap();
        assert_eq!((b.text().as_str(), s), ("ab\nc", Selection::caret(1)));
        let mut b = py("a\r\nb\nc");
        let s = delete_forward(&mut b, Selection::caret(1)).unwrap();
        assert_eq!((b.text().as_str(), s), ("ab\nc", Selection::caret(1)));
        // Typing at the end of such a line goes before the `\r`.
        let mut b = py("a\r\nb\nc");
        assert_eq!(b.line_len(0).unwrap(), 1);
        assert_eq!(b.line(0).unwrap(), "a");
        type_text(&mut b, Selection::caret(1), "x", false).unwrap();
        assert_eq!(b.to_disk_string(), "ax\r\nb\nc");
    }

    #[test]
    fn quotes_close_only_in_sensible_places() {
        let mut b = py("x = ");
        let s = type_text(&mut b, Selection::caret(4), "'", true).unwrap();
        assert_eq!(b.text(), "x = ''");
        let s = type_text(&mut b, s, "'", true).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("x = ''", 6));
        let mut b = py("don");
        type_text(&mut b, Selection::caret(3), "'", true).unwrap();
        assert_eq!(b.text(), "don'");
        let mut b = py("ab");
        type_text(&mut b, Selection::caret(0), "(", true).unwrap();
        assert_eq!(b.text(), "(ab", "no closer before a word");
    }

    #[test]
    fn selection_is_wrapped() {
        let mut b = py("abc");
        let s = type_text(&mut b, Selection::new(0, 3), "[", true).unwrap();
        assert_eq!((b.text().as_str(), s), ("[abc]", Selection::new(1, 4)));
    }

    #[test]
    fn backspace_pair_indent_and_char() {
        let mut b = py("()");
        let s = backspace(&mut b, Selection::caret(1)).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("", 0));
        let mut b = py("        x");
        let s = backspace(&mut b, Selection::caret(8)).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("    x", 4));
        let mut b = py("      x");
        backspace(&mut b, Selection::caret(6)).unwrap();
        assert_eq!(b.text(), "    x");
        let mut b = py("ab\ncd");
        let s = backspace(&mut b, Selection::caret(3)).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("abcd", 2));
        let s = delete_forward(&mut b, Selection::caret(0)).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("bcd", 0));
    }

    #[test]
    fn enter_indents_and_splits_brackets() {
        let mut b = py("def f():");
        let s = newline(&mut b, Selection::caret(8)).unwrap();
        assert_eq!(b.text(), "def f():\n    ");
        assert_eq!(s.head, b.len_chars());
        let mut b = py("x = [");
        let s = type_text(&mut b, Selection::caret(5), "", true).unwrap();
        let mut b2 = py("x = []");
        let s2 = newline(&mut b2, Selection::caret(5)).unwrap();
        assert_eq!(b2.text(), "x = [\n    \n]");
        assert_eq!(s2.head, 10);
        let _ = (s, &mut b);
    }

    #[test]
    fn tab_and_shift_tab() {
        let mut b = py("ab");
        let s = indent(&mut b, Selection::caret(1)).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("a   b", 4));
        let mut b = py("a\nb");
        let s = indent(&mut b, Selection::new(0, 3)).unwrap();
        assert_eq!(b.text(), "    a\n    b");
        let s = dedent(&mut b, s).unwrap();
        assert_eq!(b.text(), "a\nb");
        assert_eq!(s, Selection::new(0, 3));
    }

    #[test]
    fn comments_toggle() {
        let mut b = py("x = 1");
        let s = toggle_comment(&mut b, Selection::caret(1))
            .unwrap()
            .unwrap();
        assert_eq!((b.text().as_str(), s.head), ("# x = 1", 3));
        let mut plain = Buffer::from_text("x");
        assert!(toggle_comment(&mut plain, Selection::caret(0))
            .unwrap()
            .is_none());
    }

    #[test]
    fn clipboard_helpers() {
        let mut b = py("one\ntwo\n");
        assert_eq!(copy_text(&b, Selection::caret(5)), ("two\n".into(), true));
        assert_eq!(copy_text(&b, Selection::new(0, 2)), ("on".into(), false));
        let (text, s) = cut(&mut b, Selection::caret(1)).unwrap();
        assert_eq!(
            (text.as_str(), b.text().as_str(), s.head),
            ("one\n", "two\n", 0)
        );
        let s = insert_text(&mut b, Selection::caret(0), "a\nb").unwrap();
        assert_eq!((b.text().as_str(), s.head), ("a\nbtwo\n", 3));
        let s = duplicate(&mut b, Selection::caret(0)).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("a\na\nbtwo\n", 2));
        assert!(undo(&mut b).is_some());
        assert_eq!(b.text(), "a\nbtwo\n");
        assert!(redo(&mut b).is_some());
    }

    #[test]
    fn delete_words() {
        let mut b = py("foo bar");
        let s = delete_word(&mut b, Selection::caret(7), false).unwrap();
        assert_eq!((b.text().as_str(), s.head), ("foo ", 4));
        delete_word(&mut b, Selection::caret(0), true).unwrap();
        assert_eq!(b.text(), " ");
    }
}
