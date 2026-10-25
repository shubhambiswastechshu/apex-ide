//! The [`Theme`] data model and its JSON representation.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::color::Rgba;
use crate::error::ThemeError;

/// Maximum length of a theme id.
pub const MAX_THEME_ID_LEN: usize = 64;

/// Whether a theme is light or dark. UIs use this to pick matching icons and
/// native window decorations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeKind {
    /// Light text on a dark background.
    Dark,
    /// Dark text on a light background.
    Light,
}

/// Colors for the application chrome and the editor surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiColors {
    /// Main window / frame background.
    pub background: Rgba,
    /// Tool windows, side panels and status bar.
    pub surface: Rgba,
    /// Raised elements: inputs, popups, hovered rows.
    pub surface_alt: Rgba,
    /// Separators and outlines.
    pub border: Rgba,
    /// Primary UI text.
    pub foreground: Rgba,
    /// Secondary UI text: hints, descriptions, inactive tabs.
    pub foreground_muted: Rgba,
    /// Brand / focus color: primary buttons, focus rings, active indicators.
    pub accent: Rgba,
    /// Text drawn on top of `accent`.
    pub accent_foreground: Rgba,
    /// Selection background (editor and lists). May be translucent.
    pub selection: Rgba,
    /// Errors and destructive actions.
    pub error: Rgba,
    /// Warnings.
    pub warning: Rgba,
    /// Success states.
    pub success: Rgba,
    /// Code editor background.
    pub editor_background: Rgba,
    /// Default code editor text.
    pub editor_foreground: Rgba,
    /// Line numbers in the editor gutter.
    pub gutter_foreground: Rgba,
    /// Background of the line holding the caret. May be translucent.
    pub line_highlight: Rgba,
    /// Optional window-chrome colors for themes whose frame differs from
    /// their panels (like Visual Studio's "Blue" theme). When absent the UI
    /// derives the chrome from the colors above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome: Option<ChromeColors>,
}

/// Window-chrome colors: title/menu bar, the dock frame around documents and
/// tool windows, document tabs and the status bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChromeColors {
    /// Title bar, menu bar and main toolbar.
    pub title_bar: Rgba,
    /// Text and icons on `title_bar`.
    pub title_bar_foreground: Rgba,
    /// The frame behind document tabs and tool window title bars.
    pub frame: Rgba,
    /// Text on `frame` (inactive tabs, tool window titles).
    pub frame_foreground: Rgba,
    /// Background of the active document tab.
    pub tab_active: Rgba,
    /// Text on the active document tab.
    pub tab_active_foreground: Rgba,
    /// Status bar background.
    pub status_bar: Rgba,
    /// Status bar text.
    pub status_bar_foreground: Rgba,
}

/// Visual style for one kind of syntax token.
///
/// In JSON this is either a bare color string (`"#cf8e6d"`) or an object
/// (`{ "color": "#7a7e85", "italic": true }`); `bold` and `italic` default to `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct SyntaxStyle {
    /// Foreground color of the token.
    pub color: Rgba,
    /// Render in bold.
    #[serde(skip_serializing_if = "is_false")]
    pub bold: bool,
    /// Render in italics.
    #[serde(skip_serializing_if = "is_false")]
    pub italic: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl SyntaxStyle {
    /// A plain (non-bold, non-italic) style.
    pub const fn new(color: Rgba) -> Self {
        Self {
            color,
            bold: false,
            italic: false,
        }
    }

    /// Returns a copy with `bold` set.
    pub const fn bold(mut self, bold: bool) -> Self {
        self.bold = bold;
        self
    }

    /// Returns a copy with `italic` set.
    pub const fn italic(mut self, italic: bool) -> Self {
        self.italic = italic;
        self
    }
}

impl<'de> Deserialize<'de> for SyntaxStyle {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Object {
            color: Rgba,
            #[serde(default)]
            bold: bool,
            #[serde(default)]
            italic: bool,
        }

        struct StyleVisitor;

        impl<'de> Visitor<'de> for StyleVisitor {
            type Value = SyntaxStyle;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a hex color string or an object { \"color\", \"bold\", \"italic\" }")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<SyntaxStyle, E> {
                Rgba::from_hex(v).map(SyntaxStyle::new).map_err(E::custom)
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<SyntaxStyle, A::Error> {
                let o = Object::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(SyntaxStyle {
                    color: o.color,
                    bold: o.bold,
                    italic: o.italic,
                })
            }
        }

        deserializer.deserialize_any(StyleVisitor)
    }
}

/// The token categories a theme assigns styles to.
///
/// This is deliberately coarse; the editor's highlighter maps its finer-grained
/// captures onto these kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    /// `def`, `class`, `if`, `import`, ...
    Keyword,
    /// String and bytes literals.
    String,
    /// Numeric literals.
    Number,
    /// Comments.
    Comment,
    /// Function and method names.
    Function,
    /// Class and type names.
    Type,
    /// Variables, parameters, attributes.
    Variable,
    /// Constants: `True`, `None`, `ALL_CAPS` names.
    Constant,
    /// Operators: `+`, `==`, `and`, ...
    Operator,
    /// Brackets, commas, colons.
    Punctuation,
    /// Decorators such as `@dataclass`.
    Decorator,
    /// Built-in functions and types: `print`, `len`, `int`.
    Builtin,
    /// Anything else.
    Plain,
}

impl TokenKind {
    /// Every token kind, in declaration order.
    pub const ALL: [TokenKind; 13] = [
        TokenKind::Keyword,
        TokenKind::String,
        TokenKind::Number,
        TokenKind::Comment,
        TokenKind::Function,
        TokenKind::Type,
        TokenKind::Variable,
        TokenKind::Constant,
        TokenKind::Operator,
        TokenKind::Punctuation,
        TokenKind::Decorator,
        TokenKind::Builtin,
        TokenKind::Plain,
    ];

    /// The lowercase name used in theme JSON (e.g. `"keyword"`).
    pub const fn as_str(self) -> &'static str {
        match self {
            TokenKind::Keyword => "keyword",
            TokenKind::String => "string",
            TokenKind::Number => "number",
            TokenKind::Comment => "comment",
            TokenKind::Function => "function",
            TokenKind::Type => "type",
            TokenKind::Variable => "variable",
            TokenKind::Constant => "constant",
            TokenKind::Operator => "operator",
            TokenKind::Punctuation => "punctuation",
            TokenKind::Decorator => "decorator",
            TokenKind::Builtin => "builtin",
            TokenKind::Plain => "plain",
        }
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown [`TokenKind`] name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown token kind {0:?}")]
pub struct UnknownTokenKind(pub String);

impl FromStr for TokenKind {
    type Err = UnknownTokenKind;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TokenKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| UnknownTokenKind(s.to_owned()))
    }
}

/// Styles for every [`TokenKind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyntaxColors {
    /// See [`TokenKind::Keyword`].
    pub keyword: SyntaxStyle,
    /// See [`TokenKind::String`].
    pub string: SyntaxStyle,
    /// See [`TokenKind::Number`].
    pub number: SyntaxStyle,
    /// See [`TokenKind::Comment`].
    pub comment: SyntaxStyle,
    /// See [`TokenKind::Function`].
    pub function: SyntaxStyle,
    /// See [`TokenKind::Type`].
    #[serde(rename = "type")]
    pub type_: SyntaxStyle,
    /// See [`TokenKind::Variable`].
    pub variable: SyntaxStyle,
    /// See [`TokenKind::Constant`].
    pub constant: SyntaxStyle,
    /// See [`TokenKind::Operator`].
    pub operator: SyntaxStyle,
    /// See [`TokenKind::Punctuation`].
    pub punctuation: SyntaxStyle,
    /// See [`TokenKind::Decorator`].
    pub decorator: SyntaxStyle,
    /// See [`TokenKind::Builtin`].
    pub builtin: SyntaxStyle,
    /// See [`TokenKind::Plain`].
    pub plain: SyntaxStyle,
}

impl SyntaxColors {
    /// The style for `kind`.
    pub fn get(&self, kind: TokenKind) -> &SyntaxStyle {
        match kind {
            TokenKind::Keyword => &self.keyword,
            TokenKind::String => &self.string,
            TokenKind::Number => &self.number,
            TokenKind::Comment => &self.comment,
            TokenKind::Function => &self.function,
            TokenKind::Type => &self.type_,
            TokenKind::Variable => &self.variable,
            TokenKind::Constant => &self.constant,
            TokenKind::Operator => &self.operator,
            TokenKind::Punctuation => &self.punctuation,
            TokenKind::Decorator => &self.decorator,
            TokenKind::Builtin => &self.builtin,
            TokenKind::Plain => &self.plain,
        }
    }

    /// Mutable access to the style for `kind`.
    pub fn get_mut(&mut self, kind: TokenKind) -> &mut SyntaxStyle {
        match kind {
            TokenKind::Keyword => &mut self.keyword,
            TokenKind::String => &mut self.string,
            TokenKind::Number => &mut self.number,
            TokenKind::Comment => &mut self.comment,
            TokenKind::Function => &mut self.function,
            TokenKind::Type => &mut self.type_,
            TokenKind::Variable => &mut self.variable,
            TokenKind::Constant => &mut self.constant,
            TokenKind::Operator => &mut self.operator,
            TokenKind::Punctuation => &mut self.punctuation,
            TokenKind::Decorator => &mut self.decorator,
            TokenKind::Builtin => &mut self.builtin,
            TokenKind::Plain => &mut self.plain,
        }
    }

    /// Iterates over `(kind, style)` pairs in [`TokenKind::ALL`] order.
    pub fn iter(&self) -> impl Iterator<Item = (TokenKind, &SyntaxStyle)> + '_ {
        TokenKind::ALL.into_iter().map(move |k| (k, self.get(k)))
    }
}

/// A complete color theme.
///
/// The JSON form is:
///
/// ```json
/// {
///   "id": "my-theme",
///   "name": "My Theme",
///   "kind": "dark",
///   "ui": { "background": "#1e1f22", "...": "every UiColors field is required" },
///   "syntax": { "keyword": "#cf8e6d", "comment": { "color": "#7a7e85", "italic": true }, "...": "" }
/// }
/// ```
///
/// Unknown keys inside `ui` and `syntax` are rejected so typos surface as errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    /// Stable identifier, e.g. `"apex-dark"`. See [`is_valid_theme_id`].
    pub id: String,
    /// Display name, e.g. `"Apex Dark"`.
    pub name: String,
    /// Dark or light.
    pub kind: ThemeKind,
    /// Application and editor surface colors.
    pub ui: UiColors,
    /// Syntax highlighting styles.
    pub syntax: SyntaxColors,
}

impl Theme {
    /// Parses a theme from strict JSON and [validates](Self::validate) it.
    pub fn from_json(json: &str) -> Result<Self, ThemeError> {
        let theme: Theme = serde_json::from_str(json)?;
        theme.validate()?;
        Ok(theme)
    }

    /// Serializes the theme as pretty-printed JSON that [`Theme::from_json`] accepts.
    pub fn to_json_pretty(&self) -> Result<String, ThemeError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Checks the invariants serde cannot express: a valid id and a non-empty name.
    pub fn validate(&self) -> Result<(), ThemeError> {
        if !is_valid_theme_id(&self.id) {
            return Err(ThemeError::InvalidId {
                id: self.id.clone(),
            });
        }
        if self.name.trim().is_empty() {
            return Err(ThemeError::EmptyName {
                id: self.id.clone(),
            });
        }
        Ok(())
    }

    /// The style for a token kind.
    pub fn style(&self, kind: TokenKind) -> SyntaxStyle {
        *self.syntax.get(kind)
    }

    /// `true` for [`ThemeKind::Dark`].
    pub fn is_dark(&self) -> bool {
        self.kind == ThemeKind::Dark
    }
}

/// Whether `id` is a valid theme id: 1 to [`MAX_THEME_ID_LEN`] characters of
/// lowercase ASCII letters, digits, `-`, `_` or `.`.
pub fn is_valid_theme_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_THEME_ID_LEN
        && id.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// Turns an arbitrary name (e.g. a file stem) into a valid theme id, or `None`
/// if nothing usable remains. `"My Theme (v2)"` becomes `"my-theme-v2"`.
pub fn slugify_theme_id(name: &str) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed: String = out
        .trim_matches(|c| c == '-' || c == '.')
        .chars()
        .take(MAX_THEME_ID_LEN)
        .collect();
    let trimmed = trimmed.trim_end_matches(['-', '.']).to_owned();
    is_valid_theme_id(&trimmed).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin::default_theme;

    #[test]
    fn json_round_trip_preserves_theme() {
        let theme = default_theme().clone();
        let json = theme.to_json_pretty().unwrap();
        let back = Theme::from_json(&json).unwrap();
        assert_eq!(back, theme);
    }

    #[test]
    fn syntax_style_accepts_string_or_object() {
        let s: SyntaxStyle = serde_json::from_str("\"#112233\"").unwrap();
        assert_eq!(s, SyntaxStyle::new(Rgba::rgb(0x11, 0x22, 0x33)));
        let s: SyntaxStyle =
            serde_json::from_str(r##"{ "color": "#112233", "italic": true }"##).unwrap();
        assert!(s.italic && !s.bold);
        let err = serde_json::from_str::<SyntaxStyle>(r##"{ "colour": "#112233" }"##).unwrap_err();
        assert!(err.to_string().contains("colour"), "{err}");
        let err = serde_json::from_str::<SyntaxStyle>("\"red\"").unwrap_err();
        assert!(err.to_string().contains("must start with '#'"), "{err}");
    }

    #[test]
    fn syntax_style_serializes_compactly() {
        let json = serde_json::to_string(&SyntaxStyle::new(Rgba::WHITE)).unwrap();
        assert_eq!(json, r##"{"color":"#ffffff"}"##);
        let json = serde_json::to_string(&SyntaxStyle::new(Rgba::WHITE).bold(true)).unwrap();
        assert_eq!(json, r##"{"color":"#ffffff","bold":true}"##);
    }

    #[test]
    fn errors_name_the_bad_field() {
        let mut v: serde_json::Value =
            serde_json::from_str(&default_theme().to_json_pretty().unwrap()).unwrap();
        v["ui"]["accent"] = "#12345".into();
        let err = Theme::from_json(&v.to_string()).unwrap_err().to_string();
        assert!(err.contains("5 hex digit"), "{err}");

        let mut v: serde_json::Value =
            serde_json::from_str(&default_theme().to_json_pretty().unwrap()).unwrap();
        v["ui"].as_object_mut().unwrap().remove("border");
        let err = Theme::from_json(&v.to_string()).unwrap_err().to_string();
        assert!(err.contains("border"), "{err}");

        let mut v: serde_json::Value =
            serde_json::from_str(&default_theme().to_json_pretty().unwrap()).unwrap();
        v["syntax"]["keywrd"] = "#ffffff".into();
        let err = Theme::from_json(&v.to_string()).unwrap_err().to_string();
        assert!(err.contains("keywrd"), "{err}");
    }

    #[test]
    fn validation_rejects_bad_ids_and_names() {
        let mut t = default_theme().clone();
        t.id = "Has Space".into();
        assert!(matches!(t.validate(), Err(ThemeError::InvalidId { .. })));
        t.id = "ok-id".into();
        t.name = "  ".into();
        assert!(matches!(t.validate(), Err(ThemeError::EmptyName { .. })));
    }

    #[test]
    fn theme_ids() {
        assert!(is_valid_theme_id("apex-dark"));
        assert!(is_valid_theme_id("a.b_c-1"));
        assert!(!is_valid_theme_id(""));
        assert!(!is_valid_theme_id("Apex"));
        assert!(!is_valid_theme_id(&"a".repeat(MAX_THEME_ID_LEN + 1)));
        assert_eq!(
            slugify_theme_id("My Theme (v2)").as_deref(),
            Some("my-theme-v2")
        );
        assert_eq!(slugify_theme_id("--Dracula--").as_deref(), Some("dracula"));
        assert_eq!(slugify_theme_id("!!!"), None);
        assert_eq!(
            slugify_theme_id(&"x".repeat(200)).map(|s| s.len()),
            Some(MAX_THEME_ID_LEN)
        );
    }

    #[test]
    fn token_kinds_round_trip_names() {
        for k in TokenKind::ALL {
            assert_eq!(k.as_str().parse::<TokenKind>().unwrap(), k);
            let json = serde_json::to_string(&k).unwrap();
            assert_eq!(json, format!("\"{}\"", k.as_str()));
        }
        assert!("nope".parse::<TokenKind>().is_err());
        // get/get_mut/iter agree.
        let mut syntax = default_theme().syntax.clone();
        syntax.get_mut(TokenKind::Type).bold = true;
        assert!(syntax.type_.bold);
        assert_eq!(syntax.iter().count(), TokenKind::ALL.len());
    }
}
