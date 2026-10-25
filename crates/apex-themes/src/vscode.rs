//! Best-effort import of VS Code color themes.
//!
//! Only a curated set of workbench `colors` keys and TextMate `tokenColors`
//! scopes is mapped. Anything the VS Code theme does not specify is inherited
//! from `apex-vs-dark` or `apex-vs-light` (matching the theme's type), so the result is
//! always a complete [`Theme`].

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::builtin::{builtin_theme, default_theme};
use crate::color::Rgba;
use crate::error::ThemeError;
use crate::theme::{SyntaxStyle, Theme, ThemeKind, TokenKind};

/// Representative TextMate scopes (as emitted by the standard Python grammar)
/// for each token kind. A `tokenColors` selector applies to a kind when it is a
/// dot-segment prefix of one of these scopes.
const TOKEN_SCOPES: [(TokenKind, &[&str]); 12] = [
    (
        TokenKind::Keyword,
        &[
            "keyword.control.flow.python",
            "keyword.control.import.python",
            "storage.type.function.python",
            "storage.type.class.python",
        ],
    ),
    (
        TokenKind::String,
        &[
            "string.quoted.single.python",
            "string.quoted.docstring.multi.python",
        ],
    ),
    (TokenKind::Number, &["constant.numeric.dec.python"]),
    (TokenKind::Comment, &["comment.line.number-sign.python"]),
    (
        TokenKind::Function,
        &[
            "entity.name.function.python",
            "meta.function-call.generic.python",
        ],
    ),
    (TokenKind::Type, &["entity.name.type.class.python"]),
    (
        TokenKind::Variable,
        &[
            "variable.other.python",
            "variable.parameter.function.language.python",
        ],
    ),
    (
        TokenKind::Constant,
        &["constant.language.python", "variable.other.constant.python"],
    ),
    (
        TokenKind::Operator,
        &[
            "keyword.operator.arithmetic.python",
            "keyword.operator.comparison.python",
        ],
    ),
    (
        TokenKind::Punctuation,
        &[
            "punctuation.separator.element.python",
            "punctuation.parenthesis.begin.python",
        ],
    ),
    (
        TokenKind::Decorator,
        &[
            "entity.name.function.decorator.python",
            "meta.function.decorator.python",
        ],
    ),
    (
        TokenKind::Builtin,
        &["support.function.builtin.python", "support.type.python"],
    ),
];

/// Whether a parsed JSON value looks like a VS Code color theme rather than an
/// Apex theme: an object with `colors` or `tokenColors` and no Apex `ui` key.
pub fn looks_like_vscode_theme(value: &Value) -> bool {
    value.as_object().is_some_and(|o| {
        !o.contains_key("ui") && (o.contains_key("colors") || o.contains_key("tokenColors"))
    })
}

/// Converts a VS Code color theme (JSON, comments and trailing commas allowed)
/// into an Apex [`Theme`] with the given `id`.
///
/// The display name comes from the theme's `name` (falling back to `id`); the
/// kind from `type` (`dark`/`light`/`hc`/`hcLight`), or from the editor
/// background's luminance when `type` is missing. Unparseable color values are
/// ignored. `include` and string-valued `tokenColors` (external file references)
/// are not followed.
pub fn import_vscode_theme(json: &str, id: &str) -> Result<Theme, ThemeError> {
    let value: Value = serde_json::from_str(&strip_jsonc(json))?;
    let Some(obj) = value.as_object() else {
        return Err(invalid("the top-level value is not an object"));
    };
    if !looks_like_vscode_theme(&value) {
        return Err(invalid("expected a \"colors\" or \"tokenColors\" key"));
    }

    let colors = parse_colors(obj);
    let kind = match obj.get("type").and_then(Value::as_str) {
        Some("light" | "hcLight") => ThemeKind::Light,
        Some("dark" | "hc" | "hcDark") => ThemeKind::Dark,
        _ => match colors.get("editor.background") {
            Some(bg) if bg.over(Rgba::BLACK).relative_luminance() > 0.4 => ThemeKind::Light,
            _ => ThemeKind::Dark,
        },
    };
    let base_id = match kind {
        ThemeKind::Light => "apex-vs-light",
        ThemeKind::Dark => "apex-vs-dark",
    };
    let mut theme = builtin_theme(base_id).unwrap_or_else(default_theme).clone();
    theme.id = id.to_owned();
    theme.kind = kind;
    // Chrome colors belong to the base theme's look; the import derives its
    // chrome from its own panels instead.
    theme.ui.chrome = None;
    theme.name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or(id)
        .to_owned();

    let rules = parse_token_rules(obj);
    apply_ui_colors(&mut theme, &colors, rules.global_foreground);
    apply_token_rules(&mut theme, &rules.rules);
    theme.validate()?;
    Ok(theme)
}

fn invalid(reason: &str) -> ThemeError {
    ThemeError::InvalidVsCodeTheme {
        reason: reason.to_owned(),
    }
}

fn parse_colors(obj: &Map<String, Value>) -> HashMap<String, Rgba> {
    obj.get("colors")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), Rgba::from_hex(v.as_str()?).ok()?)))
                .collect()
        })
        .unwrap_or_default()
}

fn apply_ui_colors(theme: &mut Theme, colors: &HashMap<String, Rgba>, global_fg: Option<Rgba>) {
    let pick = |keys: &[&str]| keys.iter().find_map(|k| colors.get(*k).copied());
    let ui = &mut theme.ui;

    let editor_bg_given = pick(&["editor.background"]);
    if let Some(c) = editor_bg_given {
        ui.editor_background = c.over(ui.editor_background);
        ui.background = ui.editor_background;
    }
    let eb = ui.editor_background;
    // Panel backgrounds: use the theme's value, else follow a replaced editor
    // background so we never mix the base theme's chrome with foreign colors.
    let fallback_surface = editor_bg_given.map(|_| eb);
    if let Some(c) = pick(&["sideBar.background", "panel.background"]).or(fallback_surface) {
        ui.surface = c.over(eb);
    }
    let surface = ui.surface;
    let alt_keys = [
        "editorWidget.background",
        "input.background",
        "dropdown.background",
        "list.hoverBackground",
    ];
    if let Some(c) = pick(&alt_keys).or(editor_bg_given.map(|_| surface)) {
        ui.surface_alt = c.over(surface);
    }

    let set = |target: &mut Rgba, keys: &[&str]| {
        if let Some(c) = pick(keys) {
            *target = c;
        }
    };
    set(
        &mut ui.border,
        &[
            "panel.border",
            "sideBar.border",
            "editorGroup.border",
            "contrastBorder",
        ],
    );
    set(
        &mut ui.foreground,
        &["foreground", "sideBar.foreground", "editor.foreground"],
    );
    set(
        &mut ui.foreground_muted,
        &[
            "descriptionForeground",
            "disabledForeground",
            "tab.inactiveForeground",
        ],
    );
    set(
        &mut ui.accent,
        &[
            "button.background",
            "focusBorder",
            "activityBarBadge.background",
        ],
    );
    set(
        &mut ui.accent_foreground,
        &["button.foreground", "activityBarBadge.foreground"],
    );
    set(
        &mut ui.selection,
        &[
            "editor.selectionBackground",
            "list.activeSelectionBackground",
        ],
    );
    set(
        &mut ui.error,
        &["editorError.foreground", "errorForeground"],
    );
    set(
        &mut ui.warning,
        &["editorWarning.foreground", "list.warningForeground"],
    );
    set(
        &mut ui.success,
        &[
            "gitDecoration.addedResourceForeground",
            "terminal.ansiGreen",
        ],
    );
    set(&mut ui.gutter_foreground, &["editorLineNumber.foreground"]);
    set(&mut ui.line_highlight, &["editor.lineHighlightBackground"]);

    if let Some(c) = pick(&["editor.foreground", "foreground"]).or(global_fg) {
        ui.editor_foreground = c;
    }
    ui.accent = ui.accent.over(surface);
    theme.syntax.plain.color = theme.ui.editor_foreground;
}

/// One `tokenColors` entry, flattened.
struct TokenRule {
    selectors: Vec<String>,
    foreground: Option<Rgba>,
    /// `(bold, italic)` when `fontStyle` was present.
    font_style: Option<(bool, bool)>,
}

struct TokenRules {
    rules: Vec<TokenRule>,
    /// Foreground of a scope-less (global) rule, as tmTheme conversions emit.
    global_foreground: Option<Rgba>,
}

fn parse_token_rules(obj: &Map<String, Value>) -> TokenRules {
    let mut out = TokenRules {
        rules: Vec::new(),
        global_foreground: None,
    };
    let Some(entries) = obj.get("tokenColors").and_then(Value::as_array) else {
        return out;
    };
    for entry in entries {
        let Some(settings) = entry.get("settings").and_then(Value::as_object) else {
            continue;
        };
        let foreground = settings
            .get("foreground")
            .and_then(Value::as_str)
            .and_then(|s| Rgba::from_hex(s).ok());
        let font_style = settings.get("fontStyle").and_then(Value::as_str).map(|s| {
            let words: Vec<&str> = s.split_whitespace().collect();
            (words.contains(&"bold"), words.contains(&"italic"))
        });
        let selectors: Vec<String> = match entry.get("scope") {
            Some(Value::String(s)) => s.split(',').map(|p| p.trim().to_owned()).collect(),
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .flat_map(|s| s.split(','))
                .map(|p| p.trim().to_owned())
                .collect(),
            _ => {
                if out.global_foreground.is_none() {
                    out.global_foreground = foreground;
                }
                continue;
            }
        };
        out.rules.push(TokenRule {
            selectors,
            foreground,
            font_style,
        });
    }
    out
}

/// Specificity (number of dot segments) with which `selector` matches `scope`,
/// using TextMate prefix semantics; `None` if it does not match.
fn match_specificity(selector: &str, scope: &str) -> Option<usize> {
    // Descendant selectors ("source.python keyword") are approximated by their
    // last segment; exclusions ("a - b") are not supported and never match.
    if selector.starts_with('-') || selector.contains(" -") {
        return None;
    }
    let target = selector.split_whitespace().last()?;
    let matches = scope == target
        || (scope.starts_with(target) && scope.as_bytes().get(target.len()) == Some(&b'.'));
    matches.then(|| target.split('.').count())
}

fn apply_token_rules(theme: &mut Theme, rules: &[TokenRule]) {
    let plain = theme.syntax.plain.color;
    for (kind, scopes) in TOKEN_SCOPES {
        let mut best_fg: Option<(usize, Rgba)> = None;
        let mut best_style: Option<(usize, (bool, bool))> = None;
        for rule in rules {
            let Some(spec) = rule
                .selectors
                .iter()
                .flat_map(|sel| {
                    scopes
                        .iter()
                        .filter_map(move |sc| match_specificity(sel, sc))
                })
                .max()
            else {
                continue;
            };
            // Later rules win ties, as in VS Code.
            if let Some(fg) = rule.foreground {
                if !matches!(best_fg, Some((s, _)) if s > spec) {
                    best_fg = Some((spec, fg));
                }
            }
            if let Some(fs) = rule.font_style {
                if !matches!(best_style, Some((s, _)) if s > spec) {
                    best_style = Some((spec, fs));
                }
            }
        }

        let style = theme.syntax.get_mut(kind);
        match best_fg {
            Some((_, fg)) => *style = SyntaxStyle::new(fg),
            // Neutral kinds follow the imported text color rather than the base
            // theme's palette; semantic kinds keep the base color for contrast.
            None if matches!(
                kind,
                TokenKind::Variable | TokenKind::Operator | TokenKind::Punctuation
            ) =>
            {
                *style = SyntaxStyle::new(plain);
            }
            None => {}
        }
        if let Some((_, (bold, italic))) = best_style {
            style.bold = bold;
            style.italic = italic;
        }
    }
}

/// Converts JSON-with-comments into strict JSON.
///
/// `//` and `/* */` comments and trailing commas before `}` / `]` are replaced
/// by spaces (newlines are kept), so byte offsets and line numbers in parser
/// errors still refer to the original text. String contents are never touched.
pub fn strip_jsonc(input: &str) -> String {
    let mut out = input.as_bytes().to_vec();
    let n = out.len();

    // Pass 1: comments.
    let mut i = 0;
    let mut in_string = false;
    while i < n {
        let b = out[i];
        if in_string {
            match b {
                b'\\' => i += 2,
                b'"' => {
                    in_string = false;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match (b, out.get(i + 1).copied()) {
            (b'"', _) => {
                in_string = true;
                i += 1;
            }
            (b'/', Some(b'/')) => {
                while i < n && out[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
            }
            (b'/', Some(b'*')) => {
                out[i] = b' ';
                out[i + 1] = b' ';
                i += 2;
                while i < n {
                    if out[i] == b'*' && out.get(i + 1) == Some(&b'/') {
                        out[i] = b' ';
                        out[i + 1] = b' ';
                        i += 2;
                        break;
                    }
                    if out[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }

    // Pass 2: trailing commas.
    let mut i = 0;
    let mut in_string = false;
    let mut pending_comma: Option<usize> = None;
    while i < n {
        let b = out[i];
        if in_string {
            match b {
                b'\\' => i += 2,
                b'"' => {
                    in_string = false;
                    i += 1;
                }
                _ => i += 1,
            }
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                pending_comma = None;
            }
            b',' => pending_comma = Some(i),
            b'}' | b']' => {
                if let Some(c) = pending_comma.take() {
                    out[c] = b' ';
                }
            }
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => pending_comma = None,
        }
        i += 1;
    }

    // Only ASCII bytes were replaced by ASCII spaces, and every byte of a
    // multi-byte character inside a comment was replaced, so this is valid UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"{
        // Dracula-ish sample
        "name": "Sample Night",
        "type": "dark",
        "colors": {
            "editor.background": "#282a36",
            "editor.foreground": "#f8f8f2",
            "editorLineNumber.foreground": "#6272a4",
            "editor.selectionBackground": "#44475a",
            "editor.lineHighlightBackground": "#44475a75",
            "sideBar.background": "#21222c",
            "button.background": "#bd93f9",
            "button.foreground": "#282a36",
            "errorForeground": "#ff5555",
            "not.a.color": "transparent", /* ignored */
        },
        "tokenColors": [
            { "settings": { "foreground": "#f8f8f2", "background": "#282a36" } },
            { "scope": ["comment", "punctuation.definition.comment"],
              "settings": { "foreground": "#6272a4", "fontStyle": "italic" } },
            { "scope": "string, string.quoted", "settings": { "foreground": "#f1fa8c" } },
            { "scope": "keyword, storage.type", "settings": { "foreground": "#ff79c6" } },
            { "scope": "constant.numeric", "settings": { "foreground": "#bd93f9" } },
            { "scope": "entity.name.function", "settings": { "foreground": "#50fa7b" } },
            { "scope": "entity.name.function.decorator", "settings": { "foreground": "#ffb86c", "fontStyle": "bold italic" } },
            { "scope": "entity.name.type", "settings": { "foreground": "#8be9fd", "fontStyle": "italic" } },
            { "scope": "support.function", "settings": { "foreground": "#8be9fd" } },
            { "scope": "source.python keyword.operator", "settings": { "foreground": "#ff79c6" } },
            { "scope": "constant - constant.numeric", "settings": { "foreground": "#000000" } },
        ],
    }"##;

    fn hex(s: &str) -> Rgba {
        Rgba::from_hex(s).unwrap()
    }

    #[test]
    fn imports_ui_colors() {
        let t = import_vscode_theme(SAMPLE, "sample-night").unwrap();
        assert_eq!(t.id, "sample-night");
        assert_eq!(t.name, "Sample Night");
        assert_eq!(t.kind, ThemeKind::Dark);
        assert_eq!(t.ui.editor_background, hex("#282a36"));
        assert_eq!(t.ui.background, hex("#282a36"));
        assert_eq!(t.ui.editor_foreground, hex("#f8f8f2"));
        assert_eq!(t.ui.gutter_foreground, hex("#6272a4"));
        assert_eq!(t.ui.line_highlight, hex("#44475a75"));
        assert_eq!(t.ui.surface, hex("#21222c"));
        // No widget color given: follows the imported surface, not apex-vs-dark.
        assert_eq!(t.ui.surface_alt, hex("#21222c"));
        assert_eq!(t.ui.accent, hex("#bd93f9"));
        assert_eq!(t.ui.accent_foreground, hex("#282a36"));
        assert_eq!(t.ui.error, hex("#ff5555"));
        // Unmapped keys inherit from apex-vs-dark.
        assert_eq!(
            t.ui.warning,
            builtin_theme("apex-vs-dark").unwrap().ui.warning
        );
    }

    #[test]
    fn imports_token_colors_by_specificity() {
        let t = import_vscode_theme(SAMPLE, "sample-night").unwrap();
        let s = &t.syntax;
        assert_eq!(s.comment, SyntaxStyle::new(hex("#6272a4")).italic(true));
        assert_eq!(s.string.color, hex("#f1fa8c"));
        assert_eq!(s.keyword.color, hex("#ff79c6"));
        assert_eq!(s.number.color, hex("#bd93f9"));
        assert_eq!(s.function.color, hex("#50fa7b"));
        // The more specific decorator rule beats entity.name.function.
        assert_eq!(
            s.decorator,
            SyntaxStyle::new(hex("#ffb86c")).bold(true).italic(true)
        );
        assert_eq!(s.type_, SyntaxStyle::new(hex("#8be9fd")).italic(true));
        assert_eq!(s.builtin.color, hex("#8be9fd"));
        // Descendant selector matched by its last segment.
        assert_eq!(s.operator.color, hex("#ff79c6"));
        // Exclusion selectors are ignored, so constant keeps the base color.
        assert_eq!(
            s.constant.color,
            builtin_theme("apex-vs-dark").unwrap().syntax.constant.color
        );
        // Neutral kinds follow the editor text.
        assert_eq!(s.plain.color, hex("#f8f8f2"));
        assert_eq!(s.variable.color, hex("#f8f8f2"));
        assert_eq!(s.punctuation.color, hex("#f8f8f2"));
    }

    #[test]
    fn later_rules_win_ties() {
        let json = r##"{ "tokenColors": [
            { "scope": "string", "settings": { "foreground": "#111111" } },
            { "scope": "string", "settings": { "foreground": "#222222" } },
            { "scope": "string.quoted.single", "settings": { "foreground": "#333333" } },
            { "scope": "string", "settings": { "foreground": "#444444" } }
        ] }"##;
        let t = import_vscode_theme(json, "ties").unwrap();
        assert_eq!(t.syntax.string.color, hex("#333333"));
    }

    #[test]
    fn infers_kind_and_uses_light_base() {
        let json = r##"{ "colors": { "editor.background": "#fdf6e3" } }"##;
        let t = import_vscode_theme(json, "solar").unwrap();
        assert_eq!(t.kind, ThemeKind::Light);
        assert_eq!(t.name, "solar");
        let light = builtin_theme("apex-vs-light").unwrap();
        assert_eq!(t.syntax.keyword, light.syntax.keyword);
        assert_eq!(t.ui.editor_foreground, light.ui.editor_foreground);

        let hc_light = r##"{ "type": "hcLight", "tokenColors": "./external.tmTheme" }"##;
        assert_eq!(
            import_vscode_theme(hc_light, "x").unwrap().kind,
            ThemeKind::Light
        );
    }

    #[test]
    fn global_rule_sets_text_when_editor_foreground_missing() {
        let json = r##"{ "tokenColors": [ { "settings": { "foreground": "#abcdef" } } ] }"##;
        let t = import_vscode_theme(json, "g").unwrap();
        assert_eq!(t.ui.editor_foreground, hex("#abcdef"));
        assert_eq!(t.syntax.plain.color, hex("#abcdef"));
    }

    #[test]
    fn rejects_non_themes() {
        assert!(matches!(
            import_vscode_theme("[1, 2]", "x"),
            Err(ThemeError::InvalidVsCodeTheme { .. })
        ));
        assert!(matches!(
            import_vscode_theme(r#"{ "name": "x" }"#, "x"),
            Err(ThemeError::InvalidVsCodeTheme { .. })
        ));
        assert!(matches!(
            import_vscode_theme("{ nope", "x"),
            Err(ThemeError::Json(_))
        ));
        assert!(matches!(
            import_vscode_theme(r#"{ "colors": {} }"#, "Bad Id"),
            Err(ThemeError::InvalidId { .. })
        ));
    }

    #[test]
    fn detection() {
        let v: Value = serde_json::from_str(r#"{ "colors": {} }"#).unwrap();
        assert!(looks_like_vscode_theme(&v));
        let v: Value = serde_json::from_str(r#"{ "ui": {}, "colors": {} }"#).unwrap();
        assert!(!looks_like_vscode_theme(&v));
        assert!(!looks_like_vscode_theme(&Value::Null));
    }

    #[test]
    fn specificity_matching() {
        assert_eq!(
            match_specificity("keyword", "keyword.control.flow.python"),
            Some(1)
        );
        assert_eq!(
            match_specificity("keyword.control", "keyword.control.flow.python"),
            Some(2)
        );
        assert_eq!(
            match_specificity("keyword.con", "keyword.control.flow.python"),
            None
        );
        assert_eq!(
            match_specificity(
                "keyword.control.flow.python.x",
                "keyword.control.flow.python"
            ),
            None
        );
        assert_eq!(
            match_specificity("source.python string", "string.quoted"),
            Some(1)
        );
        assert_eq!(match_specificity("-string", "string.quoted"), None);
        assert_eq!(match_specificity("", "string.quoted"), None);
    }

    #[test]
    fn strip_jsonc_keeps_strings_and_positions() {
        let input = "{\n  // c1\n  \"a\": \"http://x/*y*/\", /* c2\n */ \"b\": [1, 2,],\n  \"c\": \"q\\\"//\",\n}";
        let out = strip_jsonc(input);
        assert_eq!(out.len(), input.len());
        assert_eq!(out.lines().count(), input.lines().count());
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["a"], "http://x/*y*/");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
        assert_eq!(v["c"], "q\"//");
        // Non-ASCII inside comments and strings survives; unterminated comments end the text.
        let out = strip_jsonc("{\"k\": \"\u{e9}\" /* \u{e9}\u{4e2d} */} /* open");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["k"], "\u{e9}");
        // A comma followed by a value is not trailing.
        assert_eq!(strip_jsonc("[1,2]"), "[1,2]");
    }
}
