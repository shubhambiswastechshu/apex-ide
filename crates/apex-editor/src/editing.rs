//! Language-aware editing helpers that work on plain strings.

use crate::Language;

/// One indentation level for Python: four spaces (PEP 8).
pub const INDENT_UNIT: &str = "    ";

/// Python statements after which the next line is dedented one level.
const DEDENT_KEYWORDS: [&str; 5] = ["return", "pass", "break", "continue", "raise"];

/// Leading spaces/tabs of `line`.
fn leading_ws(line: &str) -> &str {
    let end = line
        .char_indices()
        .find(|&(_, c)| c != ' ' && c != '\t')
        .map_or(line.len(), |(i, _)| i);
    &line[..end]
}

/// `line` with any trailing `#` comment removed. String literals are
/// respected, so `"a#b"` is not treated as a comment.
fn strip_python_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '#' => return &line[..i],
                '\'' | '"' => quote = Some(c),
                _ => {}
            },
        }
    }
    line
}

/// The indentation unit implied by an existing indent: a tab if the indent
/// starts with one, otherwise [`INDENT_UNIT`].
fn unit_for(indent: &str) -> &'static str {
    if indent.starts_with('\t') {
        "\t"
    } else {
        INDENT_UNIT
    }
}

/// Removes one indentation level from the end of `indent`.
fn dedent(indent: &str) -> &str {
    if let Some(rest) = indent.strip_suffix('\t') {
        return rest;
    }
    let trailing_spaces = indent.len() - indent.trim_end_matches(' ').len();
    let remove = trailing_spaces.min(INDENT_UNIT.len());
    &indent[..indent.len() - remove]
}

/// Indentation to insert after pressing Enter at the end of `prev_line`,
/// using Python rules:
///
/// * the previous line's indent is kept;
/// * one level is added after a line ending in `:` (block opener) or an
///   opening bracket `(`, `[`, `{` — trailing comments are ignored;
/// * one level is removed after `return`, `pass`, `break`, `continue` or
///   `raise`.
///
/// A level is a tab if the existing indent starts with a tab, otherwise
/// four spaces.
pub fn indent_for_newline(prev_line: &str) -> String {
    let prev_line = prev_line.trim_end_matches(['\r', '\n']);
    let indent = leading_ws(prev_line);
    let code = strip_python_comment(prev_line).trim_end();

    if code.ends_with([':', '(', '[', '{']) {
        return format!("{indent}{}", unit_for(indent));
    }

    let body = code.trim_start();
    let first_word = body
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    if DEDENT_KEYWORDS.contains(&first_word) {
        return dedent(indent).to_string();
    }
    indent.to_string()
}

/// Like [`indent_for_newline`] but for any language: Python uses the Python
/// rules, every other language simply keeps the previous line's indent.
pub fn indent_for_newline_in(language: Language, prev_line: &str) -> String {
    match language {
        Language::Python => indent_for_newline(prev_line),
        _ => leading_ws(prev_line.trim_end_matches(['\r', '\n'])).to_string(),
    }
}

/// Toggles a line comment on every line of `text` (lines separated by `\n`).
///
/// Follows PyCharm's behaviour:
///
/// * if every non-blank line already starts (after indentation) with
///   `token`, the token and one following space are removed from each;
/// * otherwise `token` plus a space is inserted into every non-blank line at
///   the smallest indentation column among them, so the block stays aligned.
///
/// Blank lines are never changed. An empty `token` returns `text` unchanged.
pub fn toggle_line_comment(text: &str, token: &str) -> String {
    if token.is_empty() {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let is_blank = |l: &str| l.trim().is_empty();
    let non_blank: Vec<&str> = lines.iter().copied().filter(|l| !is_blank(l)).collect();
    if non_blank.is_empty() {
        return text.to_string();
    }

    let all_commented = non_blank
        .iter()
        .all(|l| l.trim_start_matches([' ', '\t']).starts_with(token));

    let out: Vec<String> = if all_commented {
        lines
            .iter()
            .map(|&l| {
                if is_blank(l) {
                    return l.to_string();
                }
                let indent = leading_ws(l);
                let rest = &l[indent.len() + token.len()..];
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                format!("{indent}{rest}")
            })
            .collect()
    } else {
        // Indent chars are single-byte (space/tab), so byte length == column.
        let col = non_blank
            .iter()
            .map(|l| leading_ws(l).len())
            .min()
            .unwrap_or(0);
        lines
            .iter()
            .map(|&l| {
                if is_blank(l) {
                    l.to_string()
                } else {
                    format!("{}{token} {}", &l[..col], &l[col..])
                }
            })
            .collect()
    };
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_indent() {
        assert_eq!(indent_for_newline("    x = 1"), "    ");
        assert_eq!(indent_for_newline("x = 1"), "");
        assert_eq!(indent_for_newline(""), "");
        assert_eq!(indent_for_newline("        "), "        ");
    }

    #[test]
    fn indents_after_colon_and_brackets() {
        assert_eq!(indent_for_newline("def f():"), "    ");
        assert_eq!(indent_for_newline("    if x:"), "        ");
        assert_eq!(indent_for_newline("    if x:   # comment"), "        ");
        assert_eq!(indent_for_newline("class A:\r\n"), "    ");
        assert_eq!(indent_for_newline("items = ["), "    ");
        assert_eq!(indent_for_newline("\tfor i in x:"), "\t\t");
    }

    #[test]
    fn colon_inside_string_or_comment_does_not_indent() {
        assert_eq!(indent_for_newline("x = 1  # note:"), "");
        assert_eq!(indent_for_newline("s = 'a:'"), "");
        assert_eq!(indent_for_newline("s = \"#\"  # done:"), "");
        assert_eq!(indent_for_newline("s = \"a\\\"#:\""), "");
    }

    #[test]
    fn dedents_after_flow_keywords() {
        assert_eq!(indent_for_newline("        return x"), "    ");
        assert_eq!(indent_for_newline("    pass"), "");
        assert_eq!(indent_for_newline("  raise ValueError()"), "");
        assert_eq!(indent_for_newline("\t\tbreak"), "\t");
        assert_eq!(indent_for_newline("return"), "");
        // Identifiers that merely start with a keyword are not dedented.
        assert_eq!(indent_for_newline("    return_value = 1"), "    ");
        assert_eq!(indent_for_newline("    passed = True"), "    ");
    }

    #[test]
    fn other_languages_keep_indent() {
        assert_eq!(indent_for_newline_in(Language::Toml, "  [x]:"), "  ");
        assert_eq!(indent_for_newline_in(Language::Python, "if x:"), "    ");
    }

    #[test]
    fn comment_block_at_min_indent() {
        let src = "def f():\n    x = 1\n\n    return x";
        let want = "# def f():\n#     x = 1\n\n#     return x";
        assert_eq!(toggle_line_comment(src, "#"), want);
        assert_eq!(toggle_line_comment(want, "#"), src);

        let indented = "    a\n        b";
        assert_eq!(toggle_line_comment(indented, "#"), "    # a\n    #     b");
    }

    #[test]
    fn mixed_block_gets_commented() {
        assert_eq!(toggle_line_comment("# a\nb", "#"), "# # a\n# b");
    }

    #[test]
    fn uncomment_without_space_and_sql_token() {
        assert_eq!(toggle_line_comment("#a\n  # b", "#"), "a\n  b");
        assert_eq!(toggle_line_comment("select 1", "--"), "-- select 1");
        assert_eq!(toggle_line_comment("-- select 1", "--"), "select 1");
    }

    #[test]
    fn blank_and_empty_inputs_unchanged() {
        assert_eq!(toggle_line_comment("", "#"), "");
        assert_eq!(toggle_line_comment("  \n", "#"), "  \n");
        assert_eq!(toggle_line_comment("x", ""), "x");
    }
}
