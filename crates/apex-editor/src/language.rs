//! File-type detection.

use std::fmt;
use std::path::Path;

/// Languages the editor knows about. Only [`Language::Python`] is
/// syntax-highlighted today; the others are recognised so the UI can show
/// the right label and comment token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Language {
    /// Python source (`.py`, `.pyi`, `.pyw`).
    Python,
    /// TOML (`.toml`, `Pipfile`, `uv.lock`, `poetry.lock`, `Cargo.lock`).
    Toml,
    /// JSON (`.json`, `.jsonc`, `.ipynb`).
    Json,
    /// Markdown (`.md`, `.markdown`).
    Markdown,
    /// SQL (`.sql`).
    Sql,
    /// HTML and Django/Jinja templates (`.html`, `.htm`, `.jinja`, ...).
    Html,
    /// Anything else.
    #[default]
    Plain,
}

impl Language {
    /// Every variant, in declaration order.
    pub const ALL: [Language; 7] = [
        Language::Python,
        Language::Toml,
        Language::Json,
        Language::Markdown,
        Language::Sql,
        Language::Html,
        Language::Plain,
    ];

    /// Human-readable name, e.g. for a status bar.
    pub fn name(self) -> &'static str {
        match self {
            Language::Python => "Python",
            Language::Toml => "TOML",
            Language::Json => "JSON",
            Language::Markdown => "Markdown",
            Language::Sql => "SQL",
            Language::Html => "HTML",
            Language::Plain => "Plain Text",
        }
    }

    /// The single-line comment token, if the language has one.
    pub fn line_comment_token(self) -> Option<&'static str> {
        match self {
            Language::Python | Language::Toml => Some("#"),
            Language::Sql => Some("--"),
            Language::Json | Language::Markdown | Language::Html | Language::Plain => None,
        }
    }

    /// Whether [`Highlighter`](crate::Highlighter) produces real syntax
    /// colours for this language (otherwise everything is plain).
    pub fn is_highlighted(self) -> bool {
        matches!(self, Language::Python)
    }

    /// Maps a file extension (without the dot, any case) to a language.
    pub fn from_extension(ext: &str) -> Language {
        match ext.to_ascii_lowercase().as_str() {
            "py" | "pyi" | "pyw" => Language::Python,
            "toml" => Language::Toml,
            "json" | "jsonc" | "ipynb" => Language::Json,
            "md" | "markdown" => Language::Markdown,
            "sql" => Language::Sql,
            "html" | "htm" | "xhtml" | "jinja" | "jinja2" | "j2" => Language::Html,
            _ => Language::Plain,
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Picks a [`Language`] from a path's file name or extension.
///
/// Matching is case-insensitive and works with both `/` and `\` separated
/// paths on the current platform.
pub fn language_for_path(path: &Path) -> Language {
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        match name.to_ascii_lowercase().as_str() {
            "pipfile" | "uv.lock" | "poetry.lock" | "cargo.lock" => return Language::Toml,
            _ => {}
        }
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map_or(Language::Plain, Language::from_extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extensions() {
        let cases = [
            ("main.py", Language::Python),
            ("stubs.PYI", Language::Python),
            ("pyproject.toml", Language::Toml),
            ("uv.lock", Language::Toml),
            ("Pipfile", Language::Toml),
            ("data.json", Language::Json),
            ("nb.ipynb", Language::Json),
            ("README.md", Language::Markdown),
            ("schema.sql", Language::Sql),
            ("base.html", Language::Html),
            ("Makefile", Language::Plain),
            ("archive.tar.gz", Language::Plain),
            (".gitignore", Language::Plain),
        ];
        for (name, want) in cases {
            assert_eq!(language_for_path(Path::new(name)), want, "{name}");
        }
    }

    #[test]
    fn nested_paths() {
        let p: PathBuf = ["project", "app", "views.py"].iter().collect();
        assert_eq!(language_for_path(&p), Language::Python);
    }

    #[test]
    fn metadata() {
        assert_eq!(Language::Python.line_comment_token(), Some("#"));
        assert_eq!(Language::Sql.line_comment_token(), Some("--"));
        assert_eq!(Language::Json.line_comment_token(), None);
        assert!(Language::Python.is_highlighted());
        assert!(!Language::Toml.is_highlighted());
        assert_eq!(Language::Plain.to_string(), "Plain Text");
        assert_eq!(Language::ALL.len(), 7);
    }
}
