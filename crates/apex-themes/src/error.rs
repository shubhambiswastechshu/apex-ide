//! Error types for theme parsing, validation and loading.

use std::fmt;
use std::path::PathBuf;

/// Everything that can go wrong while parsing, validating or registering a theme.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    /// The JSON was malformed or did not match the theme schema.
    ///
    /// The inner error carries line/column information and names the
    /// offending field (including invalid colors).
    #[error("invalid theme JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// A theme file could not be read.
    #[error("could not read {}: {source}", path.display())]
    Io {
        /// The file or directory that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The theme id is empty or uses characters outside `a-z 0-9 - _ .`.
    #[error(
        "invalid theme id {id:?}: ids must be 1-64 characters of lowercase a-z, 0-9, '-', '_' or '.'"
    )]
    InvalidId {
        /// The rejected id.
        id: String,
    },

    /// The theme has an empty display name.
    #[error("theme {id:?} has an empty name")]
    EmptyName {
        /// Id of the offending theme.
        id: String,
    },

    /// A non-built-in theme tried to use the id of a built-in theme.
    #[error("theme id {id:?} is reserved by a built-in theme")]
    BuiltinConflict {
        /// The reserved id.
        id: String,
    },

    /// Two files loaded from the same directory declared the same theme id.
    #[error("theme id {id:?} is already defined by another file in this directory")]
    DuplicateId {
        /// The duplicated id.
        id: String,
    },

    /// The input is valid JSON but not a recognisable VS Code color theme.
    #[error("not a VS Code color theme: {reason}")]
    InvalidVsCodeTheme {
        /// Human-readable reason.
        reason: String,
    },
}

/// A theme file that was skipped while loading a directory, and why.
#[derive(Debug)]
pub struct ThemeLoadError {
    /// The file that failed to load.
    pub path: PathBuf,
    /// What went wrong.
    pub error: ThemeError,
}

impl fmt::Display for ThemeLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}

impl std::error::Error for ThemeLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}
