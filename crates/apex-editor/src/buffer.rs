//! The text buffer.

use std::borrow::Cow;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ropey::Rope;

use crate::history::{Edit, History};
use crate::line_ending::{count as count_line_endings, normalize, LineEnding};
use crate::search::{self, SearchOptions};
use crate::{language_for_path, toggle_line_comment, EditorError, Language, Result};

/// Default maximum number of undo steps kept per buffer.
pub const DEFAULT_UNDO_LIMIT: usize = 10_000;

const UTF8_BOM: &str = "\u{feff}";

/// A zero-based line/column position. `col` counts chars, not bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Position {
    /// Zero-based line index.
    pub line: usize,
    /// Zero-based column, in chars from the start of the line.
    pub col: usize,
}

impl Position {
    /// Creates a position.
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// Describes the single contiguous change made by
/// [`Buffer::apply_full_text`]: at char `start`, `removed_chars` chars were
/// replaced by `inserted_chars` chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextChange {
    /// Char index where the change begins.
    pub start: usize,
    /// Number of chars removed.
    pub removed_chars: usize,
    /// Number of chars inserted.
    pub inserted_chars: usize,
}

/// How one edit changed the buffer's lines, for views that cache per-line
/// data (see [`Buffer::line_edits_since`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LineEdit {
    /// First line touched (as numbered before the edit).
    pub start_line: usize,
    /// Line breaks removed by the edit.
    pub removed_breaks: usize,
    /// Line breaks inserted by the edit.
    pub inserted_breaks: usize,
    /// Char index where the edit starts.
    pub start: usize,
    /// Column (in chars) where the edit starts on `start_line`.
    pub start_col: usize,
    /// Chars removed.
    pub removed_chars: usize,
    /// Chars inserted.
    pub inserted_chars: usize,
    /// Byte offset where the edit starts.
    pub start_byte: usize,
    /// Byte offset where the removed text ended (before the edit).
    pub old_end_byte: usize,
    /// Byte offset where the inserted text ends (after the edit).
    pub new_end_byte: usize,
    /// `(line, byte column)` of the start.
    pub start_point: (usize, usize),
    /// `(line, byte column)` of the old end.
    pub old_end_point: (usize, usize),
    /// `(line, byte column)` of the new end.
    pub new_end_point: (usize, usize),
}

/// How many recent [`LineEdit`]s a buffer remembers.
const LINE_EDIT_LOG: usize = 256;

/// What the file looked like when the buffer last loaded or saved it, to
/// notice changes made by other programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiskStamp {
    modified: Option<SystemTime>,
    len: u64,
    hash: u64,
}

impl DiskStamp {
    fn new(path: &Path, bytes: &[u8]) -> Option<Self> {
        let meta = fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            hash: content_hash(bytes),
        })
    }
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// A text document: rope-backed contents plus file metadata and history.
///
/// `\r\n` line breaks are stored as `\n` and written back with the file's
/// [`LineEnding`]; a lone `\r` is ordinary content. A file that mixes LF and
/// CRLF lines is kept byte-for-byte (its `\r`s stay in the text, see
/// [`has_mixed_line_endings`](Self::has_mixed_line_endings)) until a line
/// ending is chosen with [`set_line_ending`](Self::set_line_ending).
/// All positions are char indices.
#[derive(Debug, Clone)]
pub struct Buffer {
    rope: Rope,
    path: Option<PathBuf>,
    line_ending: LineEnding,
    has_bom: bool,
    /// The text mixes LF and CRLF and is kept verbatim.
    mixed_line_endings: bool,
    history: History,
    /// History state id at the last load/save.
    saved_state: u64,
    /// Set when non-text metadata (line ending, BOM) changed since save.
    meta_dirty: bool,
    /// The file as last loaded or saved.
    disk: Option<DiskStamp>,
    /// Bumped by every change to the text.
    revision: u64,
    /// The most recent edits as `(revision after the edit, edit)`.
    line_edits: std::collections::VecDeque<(u64, LineEdit)>,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// An empty, untitled, clean buffer using LF line endings.
    pub fn new() -> Self {
        Self::from_text("")
    }

    /// An untitled, clean buffer holding `text`. The line ending is detected
    /// from `text` (LF if it has none).
    ///
    /// Text mixing LF and CRLF line breaks is kept verbatim (see
    /// [`has_mixed_line_endings`](Self::has_mixed_line_endings)).
    pub fn from_text(text: &str) -> Self {
        let (text, has_bom) = match text.strip_prefix(UTF8_BOM) {
            Some(rest) => (rest, true),
            None => (text, false),
        };
        let (lf, crlf) = count_line_endings(text);
        let mixed_line_endings = lf > 0 && crlf > 0;
        let (line_ending, stored) = if mixed_line_endings {
            (LineEnding::Lf, Cow::Borrowed(text))
        } else {
            (
                LineEnding::detect(text).unwrap_or_default(),
                normalize(text),
            )
        };
        let history = History::new(DEFAULT_UNDO_LIMIT);
        let saved_state = history.state_id();
        Self {
            rope: Rope::from_str(&stored),
            path: None,
            line_ending,
            has_bom,
            mixed_line_endings,
            history,
            saved_state,
            meta_dirty: false,
            disk: None,
            revision: 0,
            line_edits: std::collections::VecDeque::new(),
        }
    }

    /// Normalises text coming into the buffer, unless the buffer keeps mixed
    /// line endings verbatim.
    fn incoming<'a>(&self, text: &'a str) -> Cow<'a, str> {
        if self.mixed_line_endings {
            Cow::Borrowed(text)
        } else {
            normalize(text)
        }
    }

    /// Loads a UTF-8 file. A leading BOM is remembered and written back on
    /// save; the dominant line ending is detected and preserved.
    ///
    /// # Errors
    ///
    /// [`EditorError::Io`] if the file cannot be read and
    /// [`EditorError::InvalidUtf8`] if it is not valid UTF-8.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| EditorError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let disk = DiskStamp::new(path, &bytes);
        let text = String::from_utf8(bytes).map_err(|e| EditorError::InvalidUtf8 {
            path: path.to_path_buf(),
            valid_up_to: e.utf8_error().valid_up_to(),
        })?;
        let mut buffer = Self::from_text(&text);
        buffer.path = Some(path.to_path_buf());
        buffer.disk = disk;
        Ok(buffer)
    }

    /// Whether another program changed the file since this buffer loaded or
    /// last saved it. Only content changes count (a `touch` does not); a
    /// deleted file does not either, since saving just recreates it.
    pub fn is_changed_on_disk(&self) -> bool {
        let (Some(path), Some(stamp)) = (self.path.as_deref(), self.disk) else {
            return false;
        };
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        if meta.len() == stamp.len && meta.modified().ok() == stamp.modified {
            return false;
        }
        match fs::read(path) {
            Ok(bytes) => content_hash(&bytes) != stamp.hash,
            Err(_) => true,
        }
    }

    /// Writes the buffer to its path and marks it clean.
    ///
    /// # Errors
    ///
    /// [`EditorError::NoPath`] for an untitled buffer,
    /// [`EditorError::ChangedOnDisk`] if another program changed the file
    /// since it was loaded or saved (nothing is written; see
    /// [`reload`](Self::reload) and [`save_overwrite`](Self::save_overwrite)),
    /// [`EditorError::Io`] if writing fails.
    pub fn save(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or(EditorError::NoPath)?;
        if self.is_changed_on_disk() {
            return Err(EditorError::ChangedOnDisk { path });
        }
        self.write_to(&path)
    }

    /// Like [`save`](Self::save), but replaces the file even if another
    /// program changed it.
    ///
    /// # Errors
    ///
    /// [`EditorError::NoPath`] for an untitled buffer, [`EditorError::Io`]
    /// if writing fails.
    pub fn save_overwrite(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or(EditorError::NoPath)?;
        self.write_to(&path)
    }

    /// Replaces the contents with the file on disk, as one undo step, and
    /// marks the buffer clean. Line ending and BOM are taken from the file.
    ///
    /// # Errors
    ///
    /// [`EditorError::NoPath`] for an untitled buffer, [`EditorError::Io`] or
    /// [`EditorError::InvalidUtf8`] if the file cannot be read.
    pub fn reload(&mut self) -> Result<()> {
        let path = self.path.clone().ok_or(EditorError::NoPath)?;
        let fresh = Self::open(&path)?;
        // Adopt the file's mode first so its text is stored as-is.
        self.mixed_line_endings = fresh.mixed_line_endings;
        self.set_text(&fresh.rope.to_string());
        self.line_ending = fresh.line_ending;
        self.has_bom = fresh.has_bom;
        self.disk = fresh.disk;
        self.mark_clean();
        Ok(())
    }

    /// Writes the buffer to `path`, adopts it as the buffer's path, and
    /// marks the buffer clean.
    ///
    /// # Errors
    ///
    /// [`EditorError::Io`] if writing fails; the buffer's path is unchanged
    /// in that case.
    pub fn save_as(&mut self, path: impl Into<PathBuf>) -> Result<()> {
        let path = path.into();
        self.write_to(&path)?;
        self.path = Some(path);
        Ok(())
    }

    fn write_to(&mut self, path: &Path) -> Result<()> {
        let bytes = self.to_disk_string();
        write_file_safely(path, bytes.as_bytes()).map_err(|source| EditorError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        self.disk = DiskStamp::new(path, bytes.as_bytes());
        self.mark_clean();
        Ok(())
    }

    /// The exact contents [`save`](Self::save) would write: the text with
    /// the buffer's line ending applied and the BOM restored if there was one.
    pub fn to_disk_string(&self) -> String {
        let eol = self.line_ending.as_str();
        let mut out = String::with_capacity(self.rope.len_bytes() + self.rope.len_lines());
        if self.has_bom {
            out.push_str(UTF8_BOM);
        }
        for chunk in self.rope.chunks() {
            if eol == "\n" {
                out.push_str(chunk);
            } else {
                for (i, piece) in chunk.split('\n').enumerate() {
                    if i > 0 {
                        out.push_str(eol);
                    }
                    out.push_str(piece);
                }
            }
        }
        out
    }

    // ----------------------------------------------------------------------
    // Metadata
    // ----------------------------------------------------------------------

    /// The file this buffer is bound to, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Rebinds the buffer to another path without writing anything.
    pub fn set_path(&mut self, path: Option<PathBuf>) {
        if path != self.path {
            self.disk = None;
        }
        self.path = path;
    }

    /// The language implied by the buffer's path ([`Language::Plain`] when
    /// untitled).
    pub fn language(&self) -> Language {
        self.path
            .as_deref()
            .map_or(Language::Plain, language_for_path)
    }

    /// The line ending used when saving.
    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Changes the line ending used when saving. Marks the buffer dirty if
    /// it differs from the current one.
    ///
    /// On a buffer with mixed line endings this converts every line to the
    /// chosen ending (the `\r\n` to `\n` part is one undo step).
    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        if self.mixed_line_endings {
            self.mixed_line_endings = false;
            self.set_text(&self.rope.to_string());
            self.line_ending = line_ending;
            self.meta_dirty = true;
            return;
        }
        if line_ending != self.line_ending {
            self.line_ending = line_ending;
            self.meta_dirty = true;
        }
    }

    /// Whether the text mixes LF and CRLF line breaks. Such text is kept
    /// verbatim (CRLF lines keep their `\r`), so saving never rewrites lines
    /// that were not edited.
    pub fn has_mixed_line_endings(&self) -> bool {
        self.mixed_line_endings
    }

    /// Whether the file started with a UTF-8 byte-order mark.
    pub fn has_bom(&self) -> bool {
        self.has_bom
    }

    /// Whether the contents differ from the last load/save. Undoing back to
    /// the saved state makes the buffer clean again.
    pub fn is_dirty(&self) -> bool {
        self.meta_dirty || self.history.state_id() != self.saved_state
    }

    /// Declares the current contents to be the saved state.
    pub fn mark_clean(&mut self) {
        self.saved_state = self.history.state_id();
        self.meta_dirty = false;
    }

    /// Sets the maximum number of undo steps (at least 1). Older steps are
    /// discarded first.
    pub fn set_undo_limit(&mut self, limit: usize) {
        self.history.set_limit(limit);
    }

    // ----------------------------------------------------------------------
    // Reading
    // ----------------------------------------------------------------------

    /// The underlying rope, for efficient read-only access.
    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    /// A number that changes whenever the text changes (edits, undo, redo,
    /// reload). Views compare it to know when cached data is stale.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The line edits made after `revision`, oldest first, or `None` if
    /// they are no longer all remembered (the caller should then rebuild
    /// its per-line data from scratch).
    pub fn line_edits_since(&self, revision: u64) -> Option<Vec<LineEdit>> {
        if revision == self.revision {
            return Some(Vec::new());
        }
        if revision > self.revision {
            return None;
        }
        let first = self.line_edits.front()?.0;
        if revision + 1 < first {
            return None;
        }
        Some(
            self.line_edits
                .iter()
                .filter(|(r, _)| *r > revision)
                .map(|(_, e)| *e)
                .collect(),
        )
    }

    /// The whole text as a `String` (with `\n` line breaks).
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Number of chars.
    pub fn len_chars(&self) -> usize {
        self.rope.len_chars()
    }

    /// Number of UTF-8 bytes (with `\n` line breaks).
    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Number of lines. Always at least 1; a trailing `\n` starts a new,
    /// empty last line.
    pub fn len_lines(&self) -> usize {
        self.rope.len_lines()
    }

    /// Whether the buffer holds no text.
    pub fn is_empty(&self) -> bool {
        self.rope.len_chars() == 0
    }

    fn check_line(&self, line: usize) -> Result<()> {
        let len_lines = self.rope.len_lines();
        if line >= len_lines {
            return Err(EditorError::LineOutOfBounds { line, len_lines });
        }
        Ok(())
    }

    fn check_range(&self, range: &Range<usize>) -> Result<()> {
        let len = self.rope.len_chars();
        if range.start > range.end || range.end > len {
            return Err(EditorError::InvalidRange {
                start: range.start,
                end: range.end,
                len,
            });
        }
        Ok(())
    }

    fn check_char(&self, index: usize) -> Result<()> {
        let len = self.rope.len_chars();
        if index > len {
            return Err(EditorError::CharOutOfBounds { index, len });
        }
        Ok(())
    }

    /// Length of `line` in chars, excluding its line break (a `\r\n` kept
    /// verbatim in a file with mixed line endings counts as the break, so
    /// End never puts the caret between `\r` and `\n`).
    ///
    /// # Errors
    ///
    /// [`EditorError::LineOutOfBounds`].
    pub fn line_len(&self, line: usize) -> Result<usize> {
        self.check_line(line)?;
        let slice = self.rope.line(line);
        let len = slice.len_chars();
        Ok(if len > 0 && slice.char(len - 1) == '\n' {
            if len > 1 && slice.char(len - 2) == '\r' {
                len - 2
            } else {
                len - 1
            }
        } else {
            len
        })
    }

    /// Text of `line` without its line break.
    ///
    /// # Errors
    ///
    /// [`EditorError::LineOutOfBounds`].
    pub fn line(&self, line: usize) -> Result<String> {
        let len = self.line_len(line)?;
        Ok(self.rope.line(line).slice(..len).to_string())
    }

    /// Char index of the first char of `line`.
    ///
    /// # Errors
    ///
    /// [`EditorError::LineOutOfBounds`].
    pub fn line_to_char(&self, line: usize) -> Result<usize> {
        self.check_line(line)?;
        Ok(self.rope.line_to_char(line))
    }

    /// Text in a char range.
    ///
    /// # Errors
    ///
    /// [`EditorError::InvalidRange`].
    pub fn slice(&self, range: Range<usize>) -> Result<String> {
        self.check_range(&range)?;
        Ok(self.rope.slice(range).to_string())
    }

    /// Converts a char index (`0..=len_chars`) to a line/column position.
    ///
    /// # Errors
    ///
    /// [`EditorError::CharOutOfBounds`].
    pub fn char_to_position(&self, char_idx: usize) -> Result<Position> {
        self.check_char(char_idx)?;
        let line = self.rope.char_to_line(char_idx);
        let col = char_idx - self.rope.line_to_char(line);
        Ok(Position { line, col })
    }

    /// Converts a line/column position to a char index. A column past the
    /// end of the line is clamped to the end of that line (before its line
    /// break), matching how editors place a caret.
    ///
    /// # Errors
    ///
    /// [`EditorError::LineOutOfBounds`].
    pub fn position_to_char(&self, pos: Position) -> Result<usize> {
        let len = self.line_len(pos.line)?;
        Ok(self.rope.line_to_char(pos.line) + pos.col.min(len))
    }

    /// Converts a position whose `col` counts UTF-16 code units (the Language
    /// Server Protocol's default encoding) to a char index. A column past the
    /// end of the line, or one that falls inside a surrogate pair, is clamped
    /// like [`position_to_char`](Self::position_to_char).
    ///
    /// # Errors
    ///
    /// [`EditorError::LineOutOfBounds`].
    pub fn utf16_position_to_char(&self, pos: Position) -> Result<usize> {
        let len = self.line_len(pos.line)?;
        let start = self.rope.line_to_char(pos.line);
        let start_cu = self.rope.char_to_utf16_cu(start);
        let end_cu = self.rope.char_to_utf16_cu(start + len);
        let cu = start_cu.saturating_add(pos.col).min(end_cu);
        Ok(self.rope.utf16_cu_to_char(cu))
    }

    /// Converts a char index to a position whose `col` counts UTF-16 code
    /// units (for sending to a language server).
    ///
    /// # Errors
    ///
    /// [`EditorError::CharOutOfBounds`].
    pub fn char_to_utf16_position(&self, char_idx: usize) -> Result<Position> {
        self.check_char(char_idx)?;
        let line = self.rope.char_to_line(char_idx);
        let start = self.rope.line_to_char(line);
        let col = self.rope.char_to_utf16_cu(char_idx) - self.rope.char_to_utf16_cu(start);
        Ok(Position { line, col })
    }

    /// Converts a char index to a UTF-8 byte offset.
    ///
    /// # Errors
    ///
    /// [`EditorError::CharOutOfBounds`].
    pub fn char_to_byte(&self, char_idx: usize) -> Result<usize> {
        self.check_char(char_idx)?;
        Ok(self.rope.char_to_byte(char_idx))
    }

    /// Converts a UTF-8 byte offset (as used by Slint cursors) to a char
    /// index.
    ///
    /// # Errors
    ///
    /// [`EditorError::ByteOutOfBounds`] if the offset is past the end or
    /// falls inside a multi-byte char.
    pub fn byte_to_char(&self, byte_idx: usize) -> Result<usize> {
        let len = self.rope.len_bytes();
        let err = EditorError::ByteOutOfBounds {
            index: byte_idx,
            len,
        };
        if byte_idx > len {
            return Err(err);
        }
        let char_idx = self.rope.byte_to_char(byte_idx);
        if self.rope.char_to_byte(char_idx) != byte_idx {
            return Err(err);
        }
        Ok(char_idx)
    }

    // ----------------------------------------------------------------------
    // Editing
    // ----------------------------------------------------------------------

    /// Applies an edit to the rope without touching history. The range must
    /// already be validated and `text` normalised.
    fn apply_raw(&mut self, start: usize, remove_chars: usize, text: &str) {
        let start_line = self.rope.char_to_line(start);
        let removed_breaks = if remove_chars > 0 {
            let slice = self.rope.slice(start..start + remove_chars);
            slice.len_lines() - 1
        } else {
            0
        };
        let inserted_breaks = text.bytes().filter(|&b| b == b'\n').count();
        let rope = &self.rope;
        let start_byte = rope.char_to_byte(start);
        let old_end_byte = rope.char_to_byte(start + remove_chars);
        let start_point = (start_line, start_byte - rope.line_to_byte(start_line));
        let old_end_line = start_line + removed_breaks;
        let old_end_point = (old_end_line, old_end_byte - rope.line_to_byte(old_end_line));
        let new_end_point = match text.rfind('\n') {
            Some(i) => (start_line + inserted_breaks, text.len() - i - 1),
            None => (start_line, start_point.1 + text.len()),
        };
        self.revision += 1;
        if self.line_edits.len() == LINE_EDIT_LOG {
            self.line_edits.pop_front();
        }
        self.line_edits.push_back((
            self.revision,
            LineEdit {
                start_line,
                removed_breaks,
                inserted_breaks,
                start,
                start_col: start - self.rope.line_to_char(start_line),
                removed_chars: remove_chars,
                inserted_chars: text.chars().count(),
                start_byte,
                old_end_byte,
                new_end_byte: start_byte + text.len(),
                start_point,
                old_end_point,
                new_end_point,
            },
        ));
        if remove_chars > 0 {
            self.rope.remove(start..start + remove_chars);
        }
        if !text.is_empty() {
            self.rope.insert(start, text);
        }
    }

    /// Replaces `range` with `text`, recording one undo step. Consecutive
    /// single-char typing and deleting are coalesced into one step.
    /// Replacing an empty range with empty text is a no-op.
    ///
    /// # Errors
    ///
    /// [`EditorError::InvalidRange`].
    pub fn replace_range(&mut self, range: Range<usize>, text: &str) -> Result<()> {
        self.check_range(&range)?;
        let text = self.incoming(text);
        if range.is_empty() && text.is_empty() {
            return Ok(());
        }
        let removed = self.rope.slice(range.clone()).to_string();
        if removed == *text {
            return Ok(());
        }
        let edit = Edit::new(range.start, removed, text.into_owned());
        self.apply_raw(edit.start, edit.removed_chars, &edit.inserted);
        self.history.record(edit);
        Ok(())
    }

    /// Inserts `text` at `char_idx`.
    ///
    /// # Errors
    ///
    /// [`EditorError::CharOutOfBounds`].
    pub fn insert(&mut self, char_idx: usize, text: &str) -> Result<()> {
        self.check_char(char_idx)?;
        self.replace_range(char_idx..char_idx, text)
    }

    /// Deletes the chars in `range`.
    ///
    /// # Errors
    ///
    /// [`EditorError::InvalidRange`].
    pub fn delete(&mut self, range: Range<usize>) -> Result<()> {
        self.replace_range(range, "")
    }

    /// Replaces the whole text as a single undo step. Does nothing if the
    /// text is unchanged.
    pub fn set_text(&mut self, text: &str) {
        let text = self.incoming(text);
        let len = self.rope.len_chars();
        let removed = self.rope.to_string();
        if removed == *text {
            return;
        }
        let edit = Edit::new(0, removed, text.into_owned());
        self.apply_raw(0, len, &edit.inserted);
        self.history.record_group(vec![edit]);
    }

    /// Updates the buffer to `new_text` by computing the minimal single
    /// changed region (common prefix and suffix are kept) and applying it as
    /// a normal edit, so undo, typing coalescing and dirty tracking work as
    /// if the change had been made with [`replace_range`](Self::replace_range).
    ///
    /// Intended for UI widgets (such as Slint's `TextEdit`) that only report
    /// the full new text. Returns the change, or `None` if nothing changed.
    pub fn apply_full_text(&mut self, new_text: &str) -> Option<TextChange> {
        let new_text = self.incoming(new_text);
        let old_len = self.rope.len_chars();
        let new_len = new_text.chars().count();

        let prefix = self
            .rope
            .chars()
            .zip(new_text.chars())
            .take_while(|(a, b)| a == b)
            .count();
        if prefix == old_len && prefix == new_len {
            return None;
        }

        let max_suffix = old_len.min(new_len) - prefix;
        let mut old_rev = self.rope.chars_at(old_len);
        let mut new_rev = new_text.chars().rev();
        let mut suffix = 0;
        while suffix < max_suffix {
            match (old_rev.prev(), new_rev.next()) {
                (Some(a), Some(b)) if a == b => suffix += 1,
                _ => break,
            }
        }

        let removed_end = old_len - suffix;
        let inserted_end = new_len - suffix;
        let byte_at = |char_idx: usize| {
            new_text
                .char_indices()
                .nth(char_idx)
                .map_or(new_text.len(), |(i, _)| i)
        };
        let inserted = &new_text[byte_at(prefix)..byte_at(inserted_end)];

        let removed = self.rope.slice(prefix..removed_end).to_string();
        let edit = Edit::new(prefix, removed, inserted.to_string());
        let change = TextChange {
            start: prefix,
            removed_chars: edit.removed_chars,
            inserted_chars: edit.inserted_chars,
        };
        self.apply_raw(edit.start, edit.removed_chars, &edit.inserted);
        self.history.record(edit);
        Some(change)
    }

    /// Toggles a line comment on lines `first_line..=last_line` as one undo
    /// step (see [`toggle_line_comment`] for the rules). Returns whether
    /// anything changed. The line arguments may be given in either order.
    ///
    /// # Errors
    ///
    /// [`EditorError::LineOutOfBounds`].
    pub fn toggle_comment(
        &mut self,
        first_line: usize,
        last_line: usize,
        token: &str,
    ) -> Result<bool> {
        let (first, last) = (first_line.min(last_line), first_line.max(last_line));
        self.check_line(last)?;
        let start = self.rope.line_to_char(first);
        let end = self.rope.line_to_char(last) + self.line_len(last)?;
        let old = self.rope.slice(start..end).to_string();
        let new = toggle_line_comment(&old, token);
        if new == old {
            return Ok(false);
        }
        self.history.seal();
        self.replace_range(start..end, &new)?;
        self.history.seal();
        Ok(true)
    }

    // ----------------------------------------------------------------------
    // History
    // ----------------------------------------------------------------------

    /// Undoes the last step. Returns a suggested caret position (char index)
    /// or `None` if there was nothing to undo.
    pub fn undo(&mut self) -> Option<usize> {
        let edits = self.history.undo()?.edits.clone();
        for e in edits.iter().rev() {
            self.apply_raw(e.start, e.inserted_chars, &e.removed);
        }
        edits.first().map(|e| e.start + e.removed_chars)
    }

    /// Redoes the last undone step. Returns a suggested caret position
    /// (char index) or `None` if there was nothing to redo.
    pub fn redo(&mut self) -> Option<usize> {
        let edits = self.history.redo()?.edits.clone();
        for e in &edits {
            self.apply_raw(e.start, e.removed_chars, &e.inserted);
        }
        edits.last().map(|e| e.start + e.inserted_chars)
    }

    /// Whether [`undo`](Self::undo) would do anything.
    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    /// Whether [`redo`](Self::redo) would do anything.
    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Ends the current typing group so the next edit starts a new undo
    /// step. Call this when the caret is moved by the user, on focus loss,
    /// or after a pause in typing.
    pub fn break_undo_group(&mut self) {
        self.history.seal();
    }

    // ----------------------------------------------------------------------
    // Search
    // ----------------------------------------------------------------------

    /// All non-overlapping matches of `query` as char ranges, left to right.
    /// `\r\n` in the query is treated as `\n`. An empty query matches
    /// nothing.
    pub fn find_all(&self, query: &str, opts: SearchOptions) -> Vec<Range<usize>> {
        let needle: Vec<char> = self.incoming(query).chars().collect();
        if needle.is_empty() {
            return Vec::new();
        }
        let haystack: Vec<char> = self.rope.chars().collect();
        search::find_all(&haystack, &needle, opts)
    }

    /// The first match of `query` starting at or after char `from`,
    /// wrapping around to the start of the buffer if needed.
    pub fn find_next(&self, query: &str, from: usize, opts: SearchOptions) -> Option<Range<usize>> {
        let all = self.find_all(query, opts);
        all.iter()
            .find(|r| r.start >= from)
            .or_else(|| all.first())
            .cloned()
    }

    /// Replaces every match of `query` with `replacement` as a single undo
    /// step. Returns the number of replacements.
    pub fn replace_all(&mut self, query: &str, replacement: &str, opts: SearchOptions) -> usize {
        let matches = self.find_all(query, opts);
        if matches.is_empty() {
            return 0;
        }
        let replacement = self.incoming(replacement).into_owned();
        let mut edits = Vec::with_capacity(matches.len());
        // Back to front so earlier indices stay valid while applying.
        for range in matches.iter().rev() {
            let removed = self.rope.slice(range.clone()).to_string();
            let edit = Edit::new(range.start, removed, replacement.clone());
            self.apply_raw(edit.start, edit.removed_chars, &edit.inserted);
            edits.push(edit);
        }
        self.history.record_group(edits);
        matches.len()
    }
}

/// Writes `bytes` to `path` via a temporary sibling file and a rename, so a
/// crash mid-write never leaves a truncated file. Symlinks are followed (the
/// link target is replaced, not the link). Falls back to a direct write if
/// the rename is refused (e.g. the file is held open on Windows).
fn write_file_safely(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path)?,
        _ => path.to_path_buf(),
    };
    let file_name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".apex-save.tmp");
    let tmp = dir.join(tmp_name);

    let write_tmp = || -> io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        Ok(())
    };

    if let Err(e) = write_tmp() {
        let _ = fs::remove_file(&tmp);
        // The directory may not allow extra files; try a direct write.
        return fs::write(&target, bytes).map_err(|_| e);
    }
    if fs::rename(&tmp, &target).is_err() {
        let _ = fs::remove_file(&tmp);
        fs::write(&target, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(text: &str) -> Buffer {
        Buffer::from_text(text)
    }

    // --- construction & metadata --------------------------------------

    #[test]
    fn new_buffer_is_clean_and_empty() {
        let b = Buffer::new();
        assert!(b.is_empty());
        assert!(!b.is_dirty());
        assert_eq!(b.len_lines(), 1);
        assert_eq!(b.line_ending(), LineEnding::Lf);
        assert_eq!(b.language(), Language::Plain);
        assert!(!b.can_undo());
    }

    #[test]
    fn crlf_is_normalised_and_detected() {
        let b = buf("a\r\nb\r\n");
        assert_eq!(b.text(), "a\nb\n");
        assert_eq!(b.line_ending(), LineEnding::Crlf);
        assert_eq!(b.to_disk_string(), "a\r\nb\r\n");
    }

    #[test]
    fn only_lf_counts_as_line_break() {
        // Form feed, NEL and U+2028 must not start new lines (Python and
        // LSP servers agree on this).
        let b = buf("a\x0cb\u{85}c\u{2028}d\ne");
        assert_eq!(b.len_lines(), 2);
        assert_eq!(b.line(0).unwrap(), "a\x0cb\u{85}c\u{2028}d");
    }

    // --- line/col ------------------------------------------------------

    #[test]
    fn line_queries() {
        let b = buf("ab\ncdé\n");
        assert_eq!(b.len_lines(), 3);
        assert_eq!(b.line(1).unwrap(), "cdé");
        assert_eq!(b.line_len(1).unwrap(), 3);
        assert_eq!(b.line(2).unwrap(), "");
        assert!(matches!(
            b.line(3),
            Err(EditorError::LineOutOfBounds {
                line: 3,
                len_lines: 3
            })
        ));
        assert_eq!(b.line_to_char(1).unwrap(), 3);
    }

    #[test]
    fn position_round_trip() {
        let b = buf("héllo\nwörld\n");
        for idx in 0..=b.len_chars() {
            let pos = b.char_to_position(idx).unwrap();
            assert_eq!(b.position_to_char(pos).unwrap(), idx, "{pos:?}");
        }
        assert_eq!(b.char_to_position(7).unwrap(), Position::new(1, 1));
        assert!(b.char_to_position(b.len_chars() + 1).is_err());
    }

    #[test]
    fn utf16_positions_count_surrogate_pairs() {
        // "😀" is one char but two UTF-16 code units.
        let b = buf("s = \"😀\"; undefined_name\nnext");
        let target = b
            .text()
            .find("undefined")
            .map(|byte| b.byte_to_char(byte).unwrap());
        let target = target.unwrap();
        assert_eq!(target, 9);
        // A language server reports the column as 10 (UTF-16 units).
        assert_eq!(
            b.utf16_position_to_char(Position::new(0, 10)).unwrap(),
            target
        );
        assert_eq!(
            b.char_to_utf16_position(target).unwrap(),
            Position::new(0, 10)
        );
        // Columns past the end clamp to the end of the line; line 1 works too.
        assert_eq!(
            b.utf16_position_to_char(Position::new(0, 999)).unwrap(),
            b.line_to_char(1).unwrap() - 1
        );
        assert_eq!(
            b.utf16_position_to_char(Position::new(1, 2)).unwrap(),
            b.line_to_char(1).unwrap() + 2
        );
        assert!(b.utf16_position_to_char(Position::new(5, 0)).is_err());
        for idx in 0..=b.len_chars() {
            let pos = b.char_to_utf16_position(idx).unwrap();
            assert_eq!(b.utf16_position_to_char(pos).unwrap(), idx);
        }
    }

    #[test]
    fn position_col_is_clamped() {
        let b = buf("ab\ncd");
        assert_eq!(b.position_to_char(Position::new(0, 99)).unwrap(), 2);
        assert_eq!(b.position_to_char(Position::new(1, 99)).unwrap(), 5);
        assert!(b.position_to_char(Position::new(2, 0)).is_err());
    }

    #[test]
    fn byte_char_conversion() {
        let b = buf("aé😀b");
        assert_eq!(b.char_to_byte(2).unwrap(), 3);
        assert_eq!(b.byte_to_char(3).unwrap(), 2);
        assert_eq!(b.byte_to_char(7).unwrap(), 3);
        assert!(matches!(
            b.byte_to_char(2),
            Err(EditorError::ByteOutOfBounds { .. })
        ));
        assert!(b.byte_to_char(100).is_err());
        assert!(b.char_to_byte(5).is_err());
    }

    // --- editing --------------------------------------------------------

    #[test]
    fn insert_delete_replace() {
        let mut b = buf("hello world");
        b.insert(5, ",").unwrap();
        assert_eq!(b.text(), "hello, world");
        b.delete(0..1).unwrap();
        assert_eq!(b.text(), "ello, world");
        b.replace_range(0..4, "HELLO").unwrap();
        assert_eq!(b.text(), "HELLO, world");
        assert!(b.is_dirty());
    }

    #[test]
    fn edits_validate_bounds() {
        let mut b = buf("abc");
        assert!(matches!(
            b.insert(4, "x"),
            Err(EditorError::CharOutOfBounds { index: 4, len: 3 })
        ));
        assert!(matches!(
            b.delete(2..5),
            Err(EditorError::InvalidRange { .. })
        ));
        #[allow(clippy::reversed_empty_ranges)]
        let backwards = 2..1;
        assert!(b.delete(backwards).is_err());
        assert_eq!(b.text(), "abc");
        assert!(!b.can_undo());
    }

    #[test]
    fn inserted_crlf_is_normalised() {
        let mut b = buf("");
        b.insert(0, "a\r\nb\rc").unwrap();
        assert_eq!(b.text(), "a\nb\rc");
        assert_eq!(b.len_lines(), 2);
    }

    #[test]
    fn lone_cr_is_content_and_survives_a_save() {
        let mut b = buf("s = \"a\rb\"\nx = 1\n");
        assert_eq!(b.len_lines(), 3);
        assert_eq!(b.line(0).unwrap(), "s = \"a\rb\"");
        b.replace_range(
            b.line_to_char(1).unwrap()..b.line_to_char(1).unwrap() + 1,
            "y",
        )
        .unwrap();
        assert_eq!(b.to_disk_string(), "s = \"a\rb\"\ny = 1\n");
    }

    #[test]
    fn mixed_line_endings_are_kept_verbatim() {
        let original = "a\nb\r\nc\nd\n";
        let mut b = buf(original);
        assert!(b.has_mixed_line_endings());
        assert_eq!(b.len_lines(), 5);
        assert_eq!(b.to_disk_string(), original);
        // Editing one line leaves every other line's ending alone, including
        // through the full-text path the UI uses.
        b.apply_full_text("A\nb\r\nc\nd\n").unwrap();
        assert_eq!(b.to_disk_string(), "A\nb\r\nc\nd\n");
        // Choosing a line ending converts every line.
        b.set_line_ending(LineEnding::Crlf);
        assert!(!b.has_mixed_line_endings());
        assert_eq!(b.text(), "A\nb\nc\nd\n");
        assert_eq!(b.to_disk_string(), "A\r\nb\r\nc\r\nd\r\n");
        assert!(b.is_dirty());
    }

    #[test]
    fn noop_edits_are_not_recorded() {
        let mut b = buf("abc");
        b.insert(1, "").unwrap();
        b.delete(1..1).unwrap();
        b.replace_range(0..1, "a").unwrap();
        b.set_text("abc");
        assert!(!b.can_undo());
        assert!(!b.is_dirty());
    }

    // --- undo/redo --------------------------------------------------------

    #[test]
    fn typing_is_one_undo_step() {
        let mut b = buf("");
        for (i, c) in "hello".chars().enumerate() {
            b.insert(i, &c.to_string()).unwrap();
        }
        assert_eq!(b.undo(), Some(0));
        assert_eq!(b.text(), "");
        assert!(!b.can_undo());
        assert_eq!(b.redo(), Some(5));
        assert_eq!(b.text(), "hello");
    }

    #[test]
    fn newline_and_group_break_split_steps() {
        let mut b = buf("");
        b.insert(0, "a").unwrap();
        b.insert(1, "b").unwrap();
        b.insert(2, "\n").unwrap();
        b.insert(3, "c").unwrap();
        b.break_undo_group();
        b.insert(4, "d").unwrap();
        b.undo();
        assert_eq!(b.text(), "ab\nc");
        b.undo();
        assert_eq!(b.text(), "ab\n");
        b.undo();
        assert_eq!(b.text(), "ab");
        b.undo();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn backspace_run_is_one_step() {
        let mut b = buf("abc");
        b.delete(2..3).unwrap();
        b.delete(1..2).unwrap();
        assert_eq!(b.text(), "a");
        assert_eq!(b.undo(), Some(3));
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn undo_redo_of_multi_char_edits() {
        let mut b = buf("one two");
        b.replace_range(4..7, "three").unwrap();
        b.delete(0..4).unwrap();
        assert_eq!(b.text(), "three");
        b.undo();
        assert_eq!(b.text(), "one three");
        b.undo();
        assert_eq!(b.text(), "one two");
        assert!(b.undo().is_none());
        b.redo();
        b.redo();
        assert_eq!(b.text(), "three");
        assert!(b.redo().is_none());
    }

    #[test]
    fn new_edit_after_undo_discards_redo() {
        let mut b = buf("x");
        b.insert(1, "y").unwrap();
        b.undo();
        b.insert(0, "z").unwrap();
        assert!(!b.can_redo());
        assert_eq!(b.text(), "zx");
    }

    #[test]
    fn dirty_tracks_saved_state_through_undo() {
        let mut b = buf("abc");
        b.insert(3, "d").unwrap();
        assert!(b.is_dirty());
        b.undo();
        assert!(!b.is_dirty());
        b.redo();
        assert!(b.is_dirty());
        b.mark_clean();
        assert!(!b.is_dirty());
        b.undo();
        assert!(b.is_dirty());
    }

    #[test]
    fn typing_after_save_is_dirty_even_when_merged() {
        let mut b = buf("");
        b.insert(0, "a").unwrap();
        b.mark_clean();
        b.insert(1, "b").unwrap();
        assert!(b.is_dirty());
        // Undo the (merged) typing group: back to "", which is not the
        // saved state "a".
        b.undo();
        assert_eq!(b.text(), "");
        assert!(b.is_dirty());
    }

    #[test]
    fn undo_limit_is_enforced() {
        let mut b = buf("");
        b.set_undo_limit(2);
        for i in 0..5 {
            b.insert(b.len_chars(), &format!("{i}\n")).unwrap();
        }
        assert!(b.undo().is_some());
        assert!(b.undo().is_some());
        assert!(b.undo().is_none());
        assert_eq!(b.text(), "0\n1\n2\n");
        assert!(b.is_dirty());
    }

    #[test]
    fn set_text_is_undoable() {
        let mut b = buf("old");
        b.set_text("new\r\ntext");
        assert_eq!(b.text(), "new\ntext");
        b.undo();
        assert_eq!(b.text(), "old");
        assert!(!b.is_dirty());
    }

    // --- apply_full_text ----------------------------------------------

    #[test]
    fn full_text_diff_finds_minimal_region() {
        let mut b = buf("hello world");
        let ch = b.apply_full_text("hello brave world").unwrap();
        assert_eq!(
            ch,
            TextChange {
                start: 6,
                removed_chars: 0,
                inserted_chars: 6
            }
        );
        let ch = b.apply_full_text("hello world").unwrap();
        assert_eq!((ch.start, ch.removed_chars, ch.inserted_chars), (6, 6, 0));
        let ch = b.apply_full_text("jello world").unwrap();
        assert_eq!((ch.start, ch.removed_chars, ch.inserted_chars), (0, 1, 1));
        assert!(b.apply_full_text("jello world").is_none());
    }

    #[test]
    fn full_text_handles_repeated_chars_and_unicode() {
        let mut b = buf("aa");
        let ch = b.apply_full_text("aaa").unwrap();
        assert_eq!(ch.removed_chars, 0);
        assert_eq!(ch.inserted_chars, 1);
        assert_eq!(b.text(), "aaa");

        let mut b = buf("é😀é");
        b.apply_full_text("é😀xé").unwrap();
        assert_eq!(b.text(), "é😀xé");
        b.apply_full_text("").unwrap();
        assert_eq!(b.text(), "");
        b.apply_full_text("😀").unwrap();
        assert_eq!(b.text(), "😀");
    }

    #[test]
    fn full_text_typing_coalesces_into_one_undo() {
        let mut b = buf("x = ");
        let mut text = b.text();
        for c in "42".chars() {
            text.push(c);
            b.apply_full_text(&text);
        }
        assert_eq!(b.text(), "x = 42");
        b.undo();
        assert_eq!(b.text(), "x = ");
        assert!(!b.is_dirty());
    }

    #[test]
    fn full_text_normalises_crlf() {
        let mut b = buf("a\nb");
        assert!(b.apply_full_text("a\r\nb").is_none());
        b.apply_full_text("a\r\nb\r\nc");
        assert_eq!(b.text(), "a\nb\nc");
    }

    #[test]
    fn full_text_matches_text_for_many_edits() {
        // Deterministic pseudo-random edits; the buffer must always equal
        // the target and undoing everything must restore the original.
        let original = "def f(x):\n    return x * 2\n";
        let mut b = buf(original);
        let mut state = 12345u64;
        let mut target: Vec<char> = original.chars().collect();
        for _ in 0..200 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = (state >> 33) as usize;
            let pos = r % (target.len() + 1);
            if r.is_multiple_of(3) && pos < target.len() {
                target.remove(pos);
            } else {
                target.insert(pos, ['a', '\n', 'é', ' ', 'x'][r % 5]);
            }
            let s: String = target.iter().collect();
            b.apply_full_text(&s);
            assert_eq!(b.text(), s);
        }
        while b.undo().is_some() {}
        assert_eq!(b.text(), original);
    }

    // --- search ---------------------------------------------------------

    #[test]
    fn find_and_replace_all() {
        let mut b = buf("foo Foo foo_bar\nfoo");
        assert_eq!(b.find_all("foo", SearchOptions::default()).len(), 4);
        let exact_words = SearchOptions {
            case_sensitive: true,
            whole_word: true,
        };
        assert_eq!(b.find_all("foo", exact_words), vec![0..3, 16..19]);
        assert_eq!(b.find_next("foo", 1, exact_words), Some(16..19));
        assert_eq!(b.find_next("foo", 17, exact_words), Some(0..3));
        assert_eq!(b.find_next("zzz", 0, exact_words), None);

        let n = b.replace_all("foo", "bazz", exact_words);
        assert_eq!(n, 2);
        assert_eq!(b.text(), "bazz Foo foo_bar\nbazz");
        b.undo();
        assert_eq!(b.text(), "foo Foo foo_bar\nfoo");
        assert_eq!(b.replace_all("nope", "x", exact_words), 0);
        assert!(b.find_all("", exact_words).is_empty());
    }

    #[test]
    fn find_multiline_query() {
        let b = buf("a\r\nb");
        assert_eq!(b.find_all("a\r\nb", SearchOptions::exact()), vec![0..3]);
    }

    // --- comments -------------------------------------------------------

    #[test]
    fn toggle_comment_on_lines() {
        let mut b = buf("a = 1\n    b = 2\nc = 3\n");
        assert!(b.toggle_comment(1, 0, "#").unwrap());
        assert_eq!(b.text(), "# a = 1\n#     b = 2\nc = 3\n");
        assert!(b.toggle_comment(0, 1, "#").unwrap());
        assert_eq!(b.text(), "a = 1\n    b = 2\nc = 3\n");
        assert!(!b.toggle_comment(3, 3, "#").unwrap()); // empty last line
        assert!(b.toggle_comment(0, 9, "#").is_err());
        b.undo();
        assert_eq!(b.text(), "# a = 1\n#     b = 2\nc = 3\n");
    }

    // --- disk I/O -------------------------------------------------------

    #[test]
    fn open_save_round_trip_preserves_crlf_and_bom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("win.py");
        fs::write(&path, "\u{feff}x = 1\r\ny = 2\r\n").unwrap();

        let mut b = Buffer::open(&path).unwrap();
        assert_eq!(b.text(), "x = 1\ny = 2\n");
        assert_eq!(b.line_ending(), LineEnding::Crlf);
        assert!(b.has_bom());
        assert_eq!(b.language(), Language::Python);
        assert!(!b.is_dirty());

        b.insert(b.len_chars(), "z = 3\n").unwrap();
        assert!(b.is_dirty());
        b.save().unwrap();
        assert!(!b.is_dirty());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "\u{feff}x = 1\r\ny = 2\r\nz = 3\r\n"
        );
        // No temporary file is left behind.
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn save_lf_file_and_switch_line_ending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unix.txt");
        fs::write(&path, "a\nb\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a\nb\n");

        b.set_line_ending(LineEnding::Crlf);
        assert!(b.is_dirty());
        b.save().unwrap();
        assert!(!b.is_dirty());
        assert_eq!(fs::read(&path).unwrap(), b"a\r\nb\r\n");
    }

    #[test]
    fn save_as_and_no_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = buf("print('hi')\n");
        assert!(matches!(b.save(), Err(EditorError::NoPath)));
        let path = dir.path().join("new.py");
        b.save_as(&path).unwrap();
        assert_eq!(b.path(), Some(path.as_path()));
        assert_eq!(fs::read_to_string(&path).unwrap(), "print('hi')\n");

        let bad = dir.path().join("missing-dir").join("x.py");
        assert!(matches!(b.save_as(&bad), Err(EditorError::Io { .. })));
        assert_eq!(b.path(), Some(path.as_path()));
    }

    #[test]
    fn open_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.py");
        assert!(matches!(
            Buffer::open(&missing),
            Err(EditorError::Io { .. })
        ));

        let bin = dir.path().join("bin.dat");
        fs::write(&bin, [b'o', b'k', 0xff, 0xfe]).unwrap();
        match Buffer::open(&bin) {
            Err(EditorError::InvalidUtf8 { valid_up_to, .. }) => assert_eq!(valid_up_to, 2),
            other => panic!("expected InvalidUtf8, got {other:?}"),
        }
    }

    #[test]
    fn save_refuses_to_clobber_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.py");
        fs::write(&path, "x = 1\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        assert!(!b.is_changed_on_disk());
        b.insert(0, "# mine\n").unwrap();

        // Another program (git pull, a formatter) rewrites the file.
        fs::write(&path, "x = 2  # theirs, a longer line\n").unwrap();
        assert!(b.is_changed_on_disk());
        assert!(matches!(b.save(), Err(EditorError::ChangedOnDisk { .. })));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "x = 2  # theirs, a longer line\n",
            "nothing was written"
        );
        assert!(b.is_dirty());

        // Reload takes their version (undoably) and is clean.
        b.reload().unwrap();
        assert_eq!(b.text(), "x = 2  # theirs, a longer line\n");
        assert!(!b.is_dirty());
        assert!(!b.is_changed_on_disk());
        b.undo();
        assert_eq!(b.text(), "# mine\nx = 1\n");

        // Overwrite wins explicitly; afterwards saves work normally again.
        fs::write(&path, "external again\n").unwrap();
        b.save_overwrite().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# mine\nx = 1\n");
        b.insert(0, "#").unwrap();
        b.save().unwrap();
    }

    #[test]
    fn rewriting_identical_content_is_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.py");
        fs::write(&path, "x = 1\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        fs::write(&path, "x = 1\n").unwrap();
        assert!(!b.is_changed_on_disk());
        b.insert(0, "y = 0\n").unwrap();
        b.save().unwrap();
        // A deleted file is simply recreated.
        fs::remove_file(&path).unwrap();
        assert!(!b.is_changed_on_disk());
        b.insert(0, "#").unwrap();
        b.save().unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn overwrite_existing_file_replaces_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.py");
        fs::write(&path, "a much longer original line\n").unwrap();
        let mut b = Buffer::open(&path).unwrap();
        b.set_text("short\n");
        b.save().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "short\n");
    }
}
