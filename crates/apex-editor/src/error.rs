//! Error type for the editor crate.

use std::io;
use std::path::PathBuf;

/// Everything that can go wrong in `apex-editor`.
#[derive(Debug, thiserror::Error)]
pub enum EditorError {
    /// Reading or writing a file failed.
    #[error("I/O error on {}: {source}", path.display())]
    Io {
        /// The file being read or written.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// A file could not be opened because it is not valid UTF-8.
    #[error("{} is not valid UTF-8 (first invalid byte at offset {valid_up_to})", path.display())]
    InvalidUtf8 {
        /// The file that was read.
        path: PathBuf,
        /// Number of leading bytes that were valid UTF-8.
        valid_up_to: usize,
    },

    /// [`Buffer::save`](crate::Buffer::save) refused to write because the
    /// file was changed by another program since it was loaded or saved.
    /// Use [`Buffer::reload`](crate::Buffer::reload) or
    /// [`Buffer::save_overwrite`](crate::Buffer::save_overwrite).
    #[error("{} was changed on disk by another program", path.display())]
    ChangedOnDisk {
        /// The file.
        path: PathBuf,
    },

    /// [`Buffer::save`](crate::Buffer::save) was called on a buffer that has
    /// never been given a path.
    #[error("buffer has no file path; use save_as")]
    NoPath,

    /// A char index was past the end of the buffer.
    #[error("char index {index} is out of bounds (buffer has {len} chars)")]
    CharOutOfBounds {
        /// The offending index.
        index: usize,
        /// Number of chars in the buffer.
        len: usize,
    },

    /// A byte index was past the end of the buffer or not on a char boundary.
    #[error(
        "byte index {index} is out of bounds or not on a char boundary (buffer has {len} bytes)"
    )]
    ByteOutOfBounds {
        /// The offending index.
        index: usize,
        /// Number of bytes in the buffer.
        len: usize,
    },

    /// A snippet file (VS Code JSON format) could not be read.
    #[error("invalid snippet file: {0}")]
    Snippets(String),

    /// A range had `start > end` or ended past the end of the buffer.
    #[error("invalid char range {start}..{end} (buffer has {len} chars)")]
    InvalidRange {
        /// Range start.
        start: usize,
        /// Range end (exclusive).
        end: usize,
        /// Number of chars in the buffer.
        len: usize,
    },

    /// A line index was past the last line.
    #[error("line {line} is out of bounds (buffer has {len_lines} lines)")]
    LineOutOfBounds {
        /// The offending line index.
        line: usize,
        /// Number of lines in the buffer.
        len_lines: usize,
    },

    /// The tree-sitter grammar is incompatible with the linked tree-sitter.
    #[error("incompatible tree-sitter grammar: {0}")]
    Grammar(#[from] tree_sitter::LanguageError),

    /// A highlight query failed to compile.
    #[error("invalid highlight query: {0}")]
    Query(#[from] tree_sitter::QueryError),
}

/// Result alias using [`EditorError`].
pub type Result<T> = std::result::Result<T, EditorError>;
