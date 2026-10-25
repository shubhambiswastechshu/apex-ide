//! Text editing core for Apex IDE.
//!
//! This crate is UI-agnostic. It provides:
//!
//! * [`Buffer`] — a rope-backed document with a file path, dirty tracking,
//!   line-ending preservation, undo/redo, search and replace, and safe
//!   load/save.
//! * [`Highlighter`] — tree-sitter based syntax highlighting that produces
//!   sorted, non-overlapping [`HighlightSpan`]s or per-line runs a UI can
//!   paint directly.
//! * [`Language`] and [`language_for_path`] — file-type detection.
//! * [`cursor`] (caret/selection movement), [`commands`] (typing, Enter,
//!   Tab, clipboard, ...), [`completion`] (fuzzy ranking and a built-in
//!   Python provider), [`snippet`] (VS Code-style snippets) and
//!   [`outline`] (folding, indent guides, scopes) for a code editor view.
//! * Small editing helpers: [`indent_for_newline`] and
//!   [`toggle_line_comment`].
//!
//! # Indices
//!
//! Unless a name says otherwise (`*_byte`), positions are **char indices**
//! (Unicode scalar values), and lines and columns are zero-based. Use
//! [`Buffer::char_to_byte`] / [`Buffer::byte_to_char`] to convert to the
//! UTF-8 byte offsets that UI toolkits such as Slint use for cursors.
//!
//! # Line endings
//!
//! A buffer always stores text with `\n` line breaks. The line ending found
//! on load (see [`LineEnding`]) is re-applied when saving, so a CRLF file
//! stays CRLF on disk.

#![warn(missing_docs)]

mod buffer;
pub mod commands;
pub mod completion;
pub mod cursor;
mod editing;
mod error;
mod highlight;
mod history;
mod language;
mod line_ending;
pub mod outline;
mod search;
pub mod snippet;

pub use buffer::{Buffer, LineEdit, Position, TextChange, DEFAULT_UNDO_LIMIT};
pub use cursor::{Cursor, Motion, Selection};
pub use editing::{indent_for_newline, indent_for_newline_in, toggle_line_comment, INDENT_UNIT};
pub use error::{EditorError, Result};
pub use highlight::{HighlightSpan, Highlighter, LineRuns, TokenKind};
pub use language::{language_for_path, Language};
pub use line_ending::LineEnding;
pub use search::SearchOptions;
