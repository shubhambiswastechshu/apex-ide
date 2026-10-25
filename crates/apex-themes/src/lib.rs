//! Color themes for Apex IDE.
//!
//! - [`Rgba`]: hex-parsed colors with WCAG contrast helpers.
//! - [`Theme`]: UI colors plus per-[`TokenKind`] syntax styles, stored as JSON.
//! - Seven built-in themes (`apex-vs-blue` (default), `apex-vs-dark`, `apex-vs-light`, `apex-dark`, `apex-light`, `apex-midnight`,
//!   `apex-high-contrast`), embedded at compile time and checked for WCAG contrast.
//! - [`ThemeRegistry`]: built-ins + user theme directory + plugin themes, with a
//!   guaranteed fallback to `apex-vs-blue`.
//! - [`import_vscode_theme`]: best-effort conversion of VS Code color themes.
//!
//! ```
//! use apex_themes::{ThemeRegistry, TokenKind};
//!
//! let registry = ThemeRegistry::new();
//! let theme = registry.get_or_default("apex-light");
//! assert_eq!(theme.id, "apex-light");
//! let keyword = theme.style(TokenKind::Keyword);
//! assert!(keyword.color.contrast_ratio(theme.ui.editor_background) >= 4.5);
//! ```

#![warn(missing_docs)]

mod builtin;
mod color;
mod error;
mod registry;
mod theme;
mod vscode;

pub use builtin::{
    builtin_theme, builtin_themes, default_theme, is_builtin, BUILTIN_THEME_IDS, DEFAULT_THEME_ID,
};
pub use color::{ColorParseError, Rgba};
pub use error::{ThemeError, ThemeLoadError};
pub use registry::{LoadReport, ThemeRegistry, ThemeSource};
pub use theme::{
    is_valid_theme_id, slugify_theme_id, ChromeColors, SyntaxColors, SyntaxStyle, Theme, ThemeKind,
    TokenKind, UiColors, UnknownTokenKind, MAX_THEME_ID_LEN,
};
pub use vscode::{import_vscode_theme, looks_like_vscode_theme, strip_jsonc};
