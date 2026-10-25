//! Themes that ship inside the binary.

use std::sync::OnceLock;

use crate::theme::Theme;

/// Id of the theme used when nothing else is selected or a lookup fails.
pub const DEFAULT_THEME_ID: &str = "apex-vs-blue";

/// Ids of the built-in themes, in display order. The first is [`DEFAULT_THEME_ID`].
pub const BUILTIN_THEME_IDS: [&str; 7] = [
    "apex-vs-blue",
    "apex-vs-dark",
    "apex-vs-light",
    "apex-dark",
    "apex-light",
    "apex-midnight",
    "apex-high-contrast",
];

/// `(file name, JSON source)` for every built-in theme, in [`BUILTIN_THEME_IDS`] order.
const SOURCES: [(&str, &str); 7] = [
    (
        "apex-vs-blue.json",
        include_str!("../themes/apex-vs-blue.json"),
    ),
    (
        "apex-vs-dark.json",
        include_str!("../themes/apex-vs-dark.json"),
    ),
    (
        "apex-vs-light.json",
        include_str!("../themes/apex-vs-light.json"),
    ),
    ("apex-dark.json", include_str!("../themes/apex-dark.json")),
    ("apex-light.json", include_str!("../themes/apex-light.json")),
    (
        "apex-midnight.json",
        include_str!("../themes/apex-midnight.json"),
    ),
    (
        "apex-high-contrast.json",
        include_str!("../themes/apex-high-contrast.json"),
    ),
];

/// All built-in themes, parsed once on first use, in [`BUILTIN_THEME_IDS`] order.
///
/// The JSON is embedded at compile time and validated by this crate's tests, so
/// parsing cannot fail in a released build; a malformed file is a build defect
/// and panics with the file name on first access.
pub fn builtin_themes() -> &'static [Theme] {
    static THEMES: OnceLock<Vec<Theme>> = OnceLock::new();
    THEMES.get_or_init(|| {
        SOURCES
            .iter()
            .map(|(file, src)| {
                Theme::from_json(src)
                    .unwrap_or_else(|e| panic!("built-in theme {file} is invalid: {e}"))
            })
            .collect()
    })
}

/// The fallback theme (`apex-vs-blue`).
pub fn default_theme() -> &'static Theme {
    // BUILTIN_THEME_IDS is non-empty and SOURCES has the same fixed length,
    // so the first element always exists.
    &builtin_themes()[0]
}

/// Looks up a built-in theme by id.
pub fn builtin_theme(id: &str) -> Option<&'static Theme> {
    builtin_themes().iter().find(|t| t.id == id)
}

/// Whether `id` belongs to a built-in theme.
pub fn is_builtin(id: &str) -> bool {
    BUILTIN_THEME_IDS.contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Rgba;
    use crate::theme::{ThemeKind, TokenKind};

    #[test]
    fn all_builtins_parse_in_declared_order() {
        let ids: Vec<&str> = builtin_themes().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, BUILTIN_THEME_IDS);
        assert_eq!(default_theme().id, DEFAULT_THEME_ID);
        assert!(is_builtin("apex-light"));
        assert!(!is_builtin("dracula"));
        assert_eq!(
            builtin_theme("apex-midnight").map(|t| t.kind),
            Some(ThemeKind::Dark)
        );
        assert_eq!(
            builtin_theme("apex-light").map(|t| t.kind),
            Some(ThemeKind::Light)
        );
    }

    #[test]
    fn builtin_files_are_canonical() {
        // Each embedded file must survive a serialize/parse cycle unchanged, which
        // also guarantees every color string in it is valid.
        for theme in builtin_themes() {
            let again = Theme::from_json(&theme.to_json_pretty().unwrap()).unwrap();
            assert_eq!(&again, theme);
        }
    }

    /// Core accessibility gate: WCAG AA (4.5:1) for body text, AAA (7:1) for the
    /// high-contrast theme.
    #[test]
    fn builtin_themes_meet_wcag_contrast() {
        let mut failures = Vec::new();
        let mut check = |theme: &str, what: String, fg: Rgba, bg: Rgba, min: f64| {
            let ratio = fg.contrast_ratio(bg);
            if ratio < min {
                failures.push(format!(
                    "{theme}: {what} {fg} on {bg} = {ratio:.2}:1 (< {min}:1)"
                ));
            }
        };

        for t in builtin_themes() {
            let hc = t.id == "apex-high-contrast";
            let text = if hc { 7.0 } else { 4.5 };
            let secondary = if hc { 7.0 } else { 3.0 };
            let ui = &t.ui;
            let editor_bg = ui.editor_background;
            let caret_line = ui.line_highlight.over(editor_bg);
            let selected = ui.selection.over(editor_bg);

            // Editor text.
            check(
                &t.id,
                "editor_foreground".into(),
                ui.editor_foreground,
                editor_bg,
                text,
            );
            check(
                &t.id,
                "editor_foreground on line_highlight".into(),
                ui.editor_foreground,
                caret_line,
                text,
            );
            check(
                &t.id,
                "editor_foreground on selection".into(),
                ui.editor_foreground,
                selected,
                text,
            );
            // UI text on every surface.
            for (name, bg) in [
                ("background", ui.background),
                ("surface", ui.surface),
                ("surface_alt", ui.surface_alt),
            ] {
                check(
                    &t.id,
                    format!("foreground on {name}"),
                    ui.foreground,
                    bg,
                    text,
                );
                check(
                    &t.id,
                    format!("foreground_muted on {name}"),
                    ui.foreground_muted,
                    bg,
                    secondary,
                );
            }
            check(
                &t.id,
                "accent_foreground on accent".into(),
                ui.accent_foreground,
                ui.accent,
                text,
            );
            check(
                &t.id,
                "gutter_foreground".into(),
                ui.gutter_foreground,
                editor_bg,
                secondary,
            );
            if let Some(ch) = &ui.chrome {
                for (name, fg, bg) in [
                    ("title_bar", ch.title_bar_foreground, ch.title_bar),
                    ("frame", ch.frame_foreground, ch.frame),
                    ("tab_active", ch.tab_active_foreground, ch.tab_active),
                    ("status_bar", ch.status_bar_foreground, ch.status_bar),
                ] {
                    check(&t.id, format!("chrome {name}"), fg, bg, text);
                }
            }
            for (name, c) in [
                ("error", ui.error),
                ("warning", ui.warning),
                ("success", ui.success),
            ] {
                check(&t.id, name.into(), c, editor_bg, if hc { 7.0 } else { 4.5 });
            }
            // Every syntax color; comments may be quieter but must stay legible.
            for (kind, style) in t.syntax.iter() {
                let min = match kind {
                    TokenKind::Comment if !hc => 4.0,
                    _ => text,
                };
                check(&t.id, format!("syntax.{kind}"), style.color, editor_bg, min);
                check(
                    &t.id,
                    format!("syntax.{kind} on line_highlight"),
                    style.color,
                    caret_line,
                    min.min(4.0),
                );
            }
        }
        assert!(
            failures.is_empty(),
            "contrast failures:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn builtin_kinds_match_their_backgrounds() {
        for t in builtin_themes() {
            let dark_bg = t.ui.editor_background.relative_luminance() < 0.2;
            assert_eq!(
                t.is_dark(),
                dark_bg,
                "{} kind does not match its background",
                t.id
            );
        }
    }

    #[test]
    fn syntax_colors_are_distinguishable() {
        // The main semantic categories should not collapse onto the same color.
        let distinct = [
            TokenKind::Keyword,
            TokenKind::String,
            TokenKind::Number,
            TokenKind::Comment,
            TokenKind::Function,
            TokenKind::Type,
            TokenKind::Decorator,
            TokenKind::Builtin,
        ];
        for t in builtin_themes() {
            for (i, a) in distinct.iter().enumerate() {
                for b in &distinct[i + 1..] {
                    assert_ne!(
                        t.style(*a).color,
                        t.style(*b).color,
                        "{}: {a} and {b} share a color",
                        t.id
                    );
                }
            }
        }
    }
}
