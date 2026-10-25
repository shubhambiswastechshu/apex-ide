//! Maps `apex-themes` themes onto the Slint `AppTheme` global and turns
//! highlighted source lines into colored runs.

use std::path::Path;

use apex_core::Settings;
use apex_editor::LineRuns;
use apex_themes::{Rgba, SyntaxStyle, Theme, ThemeRegistry, TokenKind as ThemeToken};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::{AppTheme, AppWindow, HlLine, HlRun};

/// Converts a theme color to a Slint color (alpha preserved).
pub fn to_color(c: Rgba) -> slint::Color {
    slint::Color::from_argb_u8(c.a, c.r, c.g, c.b)
}

/// The theme style for an editor token kind. Both crates use the same
/// lower-case names; an unknown name falls back to plain text.
pub fn syntax_style(theme: &Theme, kind: apex_editor::TokenKind) -> SyntaxStyle {
    let token = kind
        .name()
        .parse::<ThemeToken>()
        .unwrap_or(ThemeToken::Plain);
    theme.style(token)
}

/// Loads built-in themes plus the user's theme folder. Returns the registry
/// and one message per theme file that failed to load.
pub fn load_registry(user_dir: Option<&Path>) -> (ThemeRegistry, Vec<String>) {
    match user_dir {
        Some(dir) => {
            let (registry, report) = ThemeRegistry::with_user_dir(dir);
            let errors = report.errors.iter().map(ToString::to_string).collect();
            (registry, errors)
        }
        None => (ThemeRegistry::new(), Vec::new()),
    }
}

/// Pushes every UI color of `theme`, and the editor font from `settings`,
/// into the `AppTheme` global.
pub fn apply(ui: &AppWindow, theme: &Theme, settings: &Settings) {
    let g = ui.global::<AppTheme>();
    let c = &theme.ui;
    g.set_dark(theme.is_dark());
    g.set_background(to_color(c.background));
    g.set_surface(to_color(c.surface));
    g.set_surface_alt(to_color(c.surface_alt));
    g.set_border(to_color(c.border));
    g.set_foreground(to_color(c.foreground));
    g.set_foreground_muted(to_color(c.foreground_muted));
    g.set_accent(to_color(c.accent));
    g.set_accent_foreground(to_color(c.accent_foreground));
    g.set_selection(to_color(c.selection));
    g.set_error(to_color(c.error));
    g.set_warning(to_color(c.warning));
    g.set_success(to_color(c.success));
    g.set_editor_background(to_color(c.editor_background));
    g.set_editor_foreground(to_color(c.editor_foreground));
    g.set_gutter_foreground(to_color(c.gutter_foreground));
    g.set_line_highlight(to_color(c.line_highlight));
    g.set_editor_font(SharedString::from(settings.font_family.as_str()));
    // Used as logical pixels, like VS Code's `editor.fontSize`: 14 points
    // (18.7 px) reads far larger than the same number in other editors.
    g.set_editor_font_size(settings.font_size);
    g.set_code_font(SharedString::from(settings.font_family.as_str()));
    g.set_code_font_size(settings.font_size);
    g.set_ui_font(SharedString::from(ui_font_family()));

    // Window chrome: explicit in framed themes (VS Blue), else derived.
    match &c.chrome {
        Some(ch) => {
            g.set_framed(true);
            g.set_title_bar(to_color(ch.title_bar));
            g.set_title_bar_foreground(to_color(ch.title_bar_foreground));
            g.set_frame(to_color(ch.frame));
            g.set_frame_foreground(to_color(ch.frame_foreground));
            g.set_tab_active(to_color(ch.tab_active));
            g.set_tab_active_foreground(to_color(ch.tab_active_foreground));
            g.set_status_bar(to_color(ch.status_bar));
            g.set_status_bar_foreground(to_color(ch.status_bar_foreground));
        }
        None => {
            g.set_framed(false);
            g.set_title_bar(to_color(c.background));
            g.set_title_bar_foreground(to_color(c.foreground));
            g.set_frame(to_color(c.background));
            g.set_frame_foreground(to_color(c.foreground));
            g.set_tab_active(to_color(c.editor_background));
            g.set_tab_active_foreground(to_color(c.foreground));
            g.set_status_bar(to_color(c.accent));
            g.set_status_bar_foreground(to_color(c.accent_foreground));
        }
    }

    // Popups sit one step above the panels they open from.
    g.set_popup_background(to_color(c.surface));
    g.set_popup_border(to_color(c.border));
    g.set_popup_selection(to_color(c.selection.over(c.surface)));
    g.set_popup_shadow(if theme.is_dark() {
        slint::Color::from_argb_u8(0x90, 0, 0, 0)
    } else {
        slint::Color::from_argb_u8(0x38, 0, 0, 0)
    });

    let s = |k: ThemeToken| to_color(theme.style(k).color);
    g.set_syntax_keyword(s(ThemeToken::Keyword));
    g.set_syntax_string(s(ThemeToken::String));
    g.set_syntax_number(s(ThemeToken::Number));
    g.set_syntax_comment(s(ThemeToken::Comment));
    g.set_syntax_function(s(ThemeToken::Function));
    g.set_syntax_type(s(ThemeToken::Type));
    g.set_syntax_variable(s(ThemeToken::Variable));
    g.set_syntax_constant(s(ThemeToken::Constant));
    g.set_syntax_operator(s(ThemeToken::Operator));
    g.set_syntax_punctuation(s(ThemeToken::Punctuation));
    g.set_syntax_decorator(s(ThemeToken::Decorator));
    g.set_syntax_builtin(s(ThemeToken::Builtin));
    g.set_syntax_plain(s(ThemeToken::Plain));
}

/// The UI (chrome) font family. Windows ships Segoe UI Variable / Segoe UI;
/// elsewhere the bundled Inter (registered in `ui/fonts`) is used.
pub fn ui_font_family() -> &'static str {
    if cfg!(windows) {
        "Segoe UI"
    } else {
        "Inter"
    }
}

/// Builds the highlighted view model: one entry per line, each a list of
/// colored runs. Tabs are expanded to four spaces so columns line up.
pub fn highlight_lines(theme: &Theme, lines: &[LineRuns]) -> Vec<HlLine> {
    lines
        .iter()
        .enumerate()
        .map(|(i, runs)| {
            let runs: Vec<HlRun> = runs
                .iter()
                .map(|(text, kind)| {
                    let style = syntax_style(theme, *kind);
                    HlRun {
                        text: SharedString::from(text.replace('\t', "    ")),
                        color: to_color(style.color),
                        bold: style.bold,
                        italic: style.italic,
                    }
                })
                .collect();
            HlLine {
                number: i32::try_from(i + 1).unwrap_or(i32::MAX),
                runs: ModelRc::new(VecModel::from(runs)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_keep_alpha_and_channels() {
        let c = to_color(Rgba::new(1, 2, 3, 4));
        assert_eq!((c.red(), c.green(), c.blue(), c.alpha()), (1, 2, 3, 4));
    }

    #[test]
    fn every_editor_token_kind_has_a_theme_style() {
        let theme = apex_themes::default_theme();
        for kind in apex_editor::TokenKind::ALL {
            let parsed = kind.name().parse::<ThemeToken>();
            assert!(parsed.is_ok(), "{} has no theme token", kind.name());
            assert_eq!(syntax_style(theme, kind), theme.style(parsed.unwrap()));
        }
    }

    #[test]
    fn highlight_lines_expand_tabs_and_number_lines() {
        let theme = apex_themes::default_theme();
        let mut h = apex_editor::Highlighter::python().unwrap();
        let runs = h.highlight_line_runs("def f():\n\treturn 1\n");
        let lines = highlight_lines(theme, &runs);
        assert_eq!(lines.len(), runs.len());
        assert_eq!(lines[1].number, 2);
        use slint::Model;
        let first = lines[1].runs.row_data(0).unwrap();
        assert!(first.text.starts_with("    "));
        let kw = lines[0].runs.row_data(0).unwrap();
        assert_eq!(kw.text, "def");
        assert_eq!(kw.color, to_color(theme.style(ThemeToken::Keyword).color));
    }

    #[test]
    fn user_theme_folder_errors_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("broken.json"), "{ not json").unwrap();
        let (registry, errors) = load_registry(Some(dir.path()));
        assert_eq!(errors.len(), 1);
        assert!(registry.contains(apex_themes::DEFAULT_THEME_ID));
        let (registry, errors) = load_registry(None);
        assert!(errors.is_empty());
        assert!(registry.len() >= apex_themes::BUILTIN_THEME_IDS.len());
    }
}
