//! Line-ending detection and normalisation.

use std::borrow::Cow;
use std::fmt;

/// The line terminator used when a buffer is written to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LineEnding {
    /// Unix style, `\n`.
    #[default]
    Lf,
    /// Windows style, `\r\n`.
    Crlf,
}

impl LineEnding {
    /// The terminator as a string.
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }

    /// The conventional line ending of the current OS (CRLF on Windows,
    /// LF elsewhere). Useful as a default for brand-new files.
    pub fn native() -> Self {
        if cfg!(windows) {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        }
    }

    /// Detects the dominant line ending in `text`.
    ///
    /// Returns `None` when `text` has no line breaks. When both styles occur
    /// the more frequent one wins; ties go to [`LineEnding::Lf`].
    pub fn detect(text: &str) -> Option<Self> {
        match count(text) {
            (0, 0) => None,
            (lf, crlf) if crlf > lf => Some(LineEnding::Crlf),
            _ => Some(LineEnding::Lf),
        }
    }

    /// Short label for a status bar: `"LF"` or `"CRLF"`.
    pub fn label(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }
}

impl fmt::Display for LineEnding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Counts `(LF-only, CRLF)` line breaks in `text`.
pub(crate) fn count(text: &str) -> (usize, usize) {
    let bytes = text.as_bytes();
    let (mut lf, mut crlf) = (0usize, 0usize);
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf += 1;
            }
        }
    }
    (lf, crlf)
}

/// Converts every `\r\n` to `\n`. A lone `\r` is content (for example
/// inside a string literal) and is kept. Borrows when nothing changes.
pub(crate) fn normalize(text: &str) -> Cow<'_, str> {
    if !text.contains("\r\n") {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace("\r\n", "\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_styles() {
        assert_eq!(LineEnding::detect("no breaks"), None);
        assert_eq!(LineEnding::detect("a\nb\n"), Some(LineEnding::Lf));
        assert_eq!(LineEnding::detect("a\r\nb\r\n"), Some(LineEnding::Crlf));
        assert_eq!(LineEnding::detect("a\r\nb\r\nc\n"), Some(LineEnding::Crlf));
        assert_eq!(LineEnding::detect("a\r\nb\n"), Some(LineEnding::Lf));
    }

    #[test]
    fn normalize_all_forms() {
        assert!(matches!(normalize("a\nb"), Cow::Borrowed(_)));
        assert!(matches!(normalize("a\rb"), Cow::Borrowed(_)));
        assert_eq!(normalize("a\r\nb\rc\n"), "a\nb\rc\n");
        assert_eq!(normalize("\r\r\n"), "\r\n");
        assert_eq!(count("a\nb\r\nc\rd\n"), (2, 1));
    }

    #[test]
    fn strings_and_labels() {
        assert_eq!(LineEnding::Crlf.as_str(), "\r\n");
        assert_eq!(LineEnding::Lf.to_string(), "LF");
        assert_eq!(
            LineEnding::native(),
            if cfg!(windows) {
                LineEnding::Crlf
            } else {
                LineEnding::Lf
            }
        );
    }
}
