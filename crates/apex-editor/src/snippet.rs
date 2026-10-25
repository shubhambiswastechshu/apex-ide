//! VS Code-style snippets.
//!
//! A snippet body is text with tab stops: `$1`, `${2:placeholder}` (which
//! may nest), `${3|one,two|}` (the first choice is used), `$0` for the final
//! caret, and variables such as `$TM_FILENAME` or `${CLIPBOARD:default}`.
//! `\$`, `\}` and `\\` are escapes.
//!
//! [`Snippet::parse`] turns a body into plain text plus tab stop ranges;
//! [`expand`] also applies the indentation of the line it is inserted on;
//! [`SnippetSession`] tracks the stops while the user types and Tab jumps.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::{EditorError, Result, INDENT_UNIT};

/// A parsed snippet: the text to insert and its tab stops.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snippet {
    /// Text with every placeholder replaced by its default.
    pub text: String,
    /// Tab stops in visiting order (`1, 2, ...` then `0`). Each entry holds
    /// every occurrence of that stop as char ranges into `text`.
    pub stops: Vec<Vec<Range<usize>>>,
}

/// Values for snippet variables; unknown variables use their default (or
/// become empty).
pub type Variables = BTreeMap<String, String>;

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    out: String,
    out_chars: usize,
    stops: BTreeMap<u32, Vec<Range<usize>>>,
    vars: &'a Variables,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn push(&mut self, c: char) {
        self.out.push(c);
        self.out_chars += 1;
    }

    fn push_str(&mut self, s: &str) {
        self.out.push_str(s);
        self.out_chars += s.chars().count();
    }

    /// Parses until end of input or an unmatched `}` when `nested`.
    fn parse_seq(&mut self, nested: bool) {
        while let Some(c) = self.peek() {
            match c {
                '\\' => {
                    let next = self.chars.get(self.pos + 1).copied();
                    if let Some(n @ ('$' | '}' | '\\' | ',' | '|')) = next {
                        self.push(n);
                        self.pos += 2;
                    } else {
                        self.push('\\');
                        self.pos += 1;
                    }
                }
                '}' if nested => return,
                '$' => self.parse_dollar(),
                _ => {
                    self.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    fn read_while(&mut self, f: impl Fn(char) -> bool) -> String {
        let start = self.pos;
        while self.peek().is_some_and(&f) {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn add_stop(&mut self, index: u32, range: Range<usize>) {
        self.stops.entry(index).or_default().push(range);
    }

    fn parse_dollar(&mut self) {
        let dollar = self.pos;
        self.pos += 1;
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                let n = self.read_while(|c| c.is_ascii_digit());
                let at = self.out_chars;
                self.add_stop(n.parse().unwrap_or(0), at..at);
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let name = self.read_while(|c| c.is_ascii_alphanumeric() || c == '_');
                let value = self.vars.get(&name).cloned().unwrap_or_default();
                self.push_str(&value);
            }
            Some('{') => {
                self.pos += 1;
                if self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    let n: u32 = self.read_while(|c| c.is_ascii_digit()).parse().unwrap_or(0);
                    let start = self.out_chars;
                    match self.peek() {
                        Some(':') => {
                            self.pos += 1;
                            self.parse_seq(true);
                        }
                        Some('|') => {
                            self.pos += 1;
                            let mut first = String::new();
                            let mut in_first = true;
                            while let Some(c) = self.peek() {
                                self.pos += 1;
                                match c {
                                    '\\' => {
                                        if let Some(n) = self.peek() {
                                            self.pos += 1;
                                            if in_first {
                                                first.push(n);
                                            }
                                        }
                                    }
                                    ',' => in_first = false,
                                    '|' => break,
                                    _ if in_first => first.push(c),
                                    _ => {}
                                }
                            }
                            self.push_str(&first);
                        }
                        _ => {}
                    }
                    if self.peek() == Some('}') {
                        self.pos += 1;
                    }
                    let end = self.out_chars;
                    self.add_stop(n, start..end);
                } else {
                    let name = self.read_while(|c| c.is_ascii_alphanumeric() || c == '_');
                    if name.is_empty() {
                        // Not a snippet construct: keep it literally.
                        self.pos = dollar + 1;
                        self.push('$');
                        return;
                    }
                    match (self.peek(), self.vars.get(&name).cloned()) {
                        (Some(':'), Some(value)) => {
                            // Skip the default.
                            let mut depth = 0;
                            while let Some(c) = self.peek() {
                                if c == '}' && depth == 0 {
                                    break;
                                }
                                if c == '{' {
                                    depth += 1;
                                }
                                if c == '}' {
                                    depth -= 1;
                                }
                                self.pos += 1;
                            }
                            self.push_str(&value);
                        }
                        (Some(':'), None) => {
                            self.pos += 1;
                            self.parse_seq(true);
                        }
                        (_, value) => self.push_str(&value.unwrap_or_default()),
                    }
                    // Ignore transforms such as ${VAR/regex/fmt/}.
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == '}' {
                            break;
                        }
                    }
                }
            }
            _ => self.push('$'),
        }
    }
}

impl Snippet {
    /// Parses a snippet body with no variables set.
    pub fn parse(body: &str) -> Snippet {
        Self::parse_with(body, &Variables::new())
    }

    /// Parses a snippet body, substituting `vars`.
    pub fn parse_with(body: &str, vars: &Variables) -> Snippet {
        let mut p = Parser {
            chars: body.chars().collect(),
            pos: 0,
            out: String::new(),
            out_chars: 0,
            stops: BTreeMap::new(),
            vars,
        };
        p.parse_seq(false);
        // Stray `}` at top level are literal.
        while p.pos < p.chars.len() {
            let c = p.chars[p.pos];
            p.push(c);
            p.pos += 1;
            p.parse_seq(false);
        }
        let mut stops: Vec<Vec<Range<usize>>> = Vec::new();
        let mut final_stop = None;
        for (index, ranges) in p.stops {
            if index == 0 {
                final_stop = Some(ranges);
            } else {
                stops.push(ranges);
            }
        }
        let end = p.out_chars;
        #[allow(clippy::single_range_in_vec_init)]
        stops.push(final_stop.unwrap_or_else(|| vec![end..end]));
        Snippet { text: p.out, stops }
    }
}

/// Re-indents a snippet body for insertion on a line that starts with
/// `indent`: every line after the first gets `indent`, and leading tabs
/// become [`INDENT_UNIT`]s.
pub fn indent_body(body: &str, indent: &str) -> String {
    let mut out = String::with_capacity(body.len() + 16);
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(indent);
            }
        }
        let tabs = line.chars().take_while(|c| *c == '\t').count();
        for _ in 0..tabs {
            out.push_str(INDENT_UNIT);
        }
        out.push_str(&line[tabs..]);
    }
    out
}

/// Parses `body` after re-indenting it for a line starting with `indent`.
pub fn expand(body: &str, indent: &str, vars: &Variables) -> Snippet {
    Snippet::parse_with(&indent_body(&body.replace("\r\n", "\n"), indent), vars)
}

/// Active tab stops of an inserted snippet, in absolute buffer positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetSession {
    stops: Vec<Vec<Range<usize>>>,
    current: usize,
}

impl SnippetSession {
    /// Starts a session for `snippet` inserted at char `at`. Returns the
    /// session (if there is more than the final stop) and the first range
    /// to select.
    pub fn start(snippet: &Snippet, at: usize) -> (Option<SnippetSession>, Range<usize>) {
        let stops: Vec<Vec<Range<usize>>> = snippet
            .stops
            .iter()
            .map(|rs| rs.iter().map(|r| r.start + at..r.end + at).collect())
            .collect();
        let first = stops
            .first()
            .and_then(|r| r.first())
            .cloned()
            .unwrap_or(at..at);
        let session = (stops.len() > 1).then_some(SnippetSession { stops, current: 0 });
        (session, first)
    }

    /// The range of the current stop.
    pub fn current(&self) -> Option<Range<usize>> {
        self.stops.get(self.current)?.first().cloned()
    }

    /// Whether the current stop is the final one.
    pub fn at_last(&self) -> bool {
        self.current + 1 >= self.stops.len()
    }

    /// Moves to the next stop and returns its range; `None` (and the
    /// session should end) once past the final stop.
    pub fn next_stop(&mut self) -> Option<Range<usize>> {
        if self.at_last() {
            return None;
        }
        self.current += 1;
        self.current()
    }

    /// Moves to the previous stop and returns its range.
    pub fn prev_stop(&mut self) -> Option<Range<usize>> {
        self.current = self.current.saturating_sub(1);
        self.current()
    }

    /// Updates the stops for an edit that replaced `removed` chars at
    /// `start` with `inserted` chars. Returns `false` if the edit broke the
    /// snippet (it overlapped a stop boundary); the session should end.
    pub fn on_edit(&mut self, start: usize, removed: usize, inserted: usize) -> bool {
        let end = start + removed;
        let current = self.current;
        for (si, ranges) in self.stops.iter_mut().enumerate() {
            for r in ranges.iter_mut() {
                let inside = start >= r.start && end <= r.end;
                let strictly = start > r.start && end < r.end;
                if inside && (si == current || strictly) {
                    r.end = r.end - removed + inserted;
                } else if r.end <= start {
                } else if r.start >= end {
                    r.start = r.start - removed + inserted;
                    r.end = r.end - removed + inserted;
                } else {
                    return false;
                }
            }
        }
        true
    }

    /// Whether `pos` lies within the area spanned by the stops.
    pub fn contains(&self, pos: usize) -> bool {
        let all = self.stops.iter().flatten();
        let lo = all.clone().map(|r| r.start).min().unwrap_or(0);
        let hi = all.map(|r| r.end).max().unwrap_or(0);
        (lo..=hi).contains(&pos)
    }
}

/// A named snippet: typing `prefix` offers it in completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetDef {
    /// Trigger word.
    pub prefix: String,
    /// Display name.
    pub name: String,
    /// Body in VS Code snippet syntax (lines joined with `\n`).
    pub body: String,
    /// Short description.
    pub description: String,
}

impl SnippetDef {
    fn new(prefix: &str, name: &str, body: &str, description: &str) -> Self {
        Self {
            prefix: prefix.into(),
            name: name.into(),
            body: body.into(),
            description: description.into(),
        }
    }

    /// The body with placeholders filled in, for previews.
    pub fn preview(&self) -> String {
        Snippet::parse(&self.body).text
    }
}

/// Reads a VS Code snippet file (`{"name": {"prefix", "body",
/// "description"}}`; `prefix` and `body` may be strings or arrays).
/// Comments (`//`) on their own lines are allowed, as VS Code does.
///
/// # Errors
///
/// [`EditorError::Snippets`] if the JSON is invalid.
pub fn parse_vscode_snippets(json: &str) -> Result<Vec<SnippetDef>> {
    let cleaned: String = json
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let value: serde_json::Value =
        serde_json::from_str(&cleaned).map_err(|e| EditorError::Snippets(e.to_string()))?;
    let obj = value
        .as_object()
        .ok_or_else(|| EditorError::Snippets("top level must be an object".into()))?;
    let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
        match v {
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut out = Vec::new();
    for (name, def) in obj {
        let body = strings(def.get("body")).join("\n");
        let description = def
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or_default();
        for prefix in strings(def.get("prefix")) {
            if !prefix.is_empty() {
                out.push(SnippetDef::new(&prefix, name, &body, description));
            }
        }
    }
    Ok(out)
}

/// Built-in Python snippets, including Django and data-science ones.
pub fn builtin_python_snippets() -> Vec<SnippetDef> {
    [
        ("def", "Function", "def ${1:name}(${2}):\n\t${0:pass}", "Function definition"),
        ("adef", "Async function", "async def ${1:name}(${2}):\n\t${0:pass}", "Async function definition"),
        ("class", "Class", "class ${1:Name}:\n\tdef __init__(self${2}):\n\t\t${0:pass}", "Class definition"),
        ("if", "if", "if ${1:condition}:\n\t${0:pass}", "if statement"),
        ("ife", "if/else", "if ${1:condition}:\n\t${2:pass}\nelse:\n\t${0:pass}", "if/else statement"),
        ("for", "for", "for ${1:item} in ${2:items}:\n\t${0:pass}", "for loop"),
        ("fore", "for enumerate", "for ${1:i}, ${2:item} in enumerate(${3:items}):\n\t${0:pass}", "for loop with index"),
        ("while", "while", "while ${1:condition}:\n\t${0:pass}", "while loop"),
        ("try", "try/except", "try:\n\t${1:pass}\nexcept ${2:Exception} as ${3:e}:\n\t${0:raise}", "try/except block"),
        ("tryf", "try/finally", "try:\n\t${1:pass}\nfinally:\n\t${0:pass}", "try/finally block"),
        ("with", "with", "with ${1:open(${2:path})} as ${3:f}:\n\t${0:pass}", "with statement"),
        ("main", "main guard", "if __name__ == \"__main__\":\n\t${0:main()}", "if __name__ == \"__main__\""),
        ("lc", "list comprehension", "[${1:x} for ${2:x} in ${3:items}]$0", "List comprehension"),
        ("dc", "dict comprehension", "{${1:k}: ${2:v} for ${3:k}, ${4:v} in ${5:items}.items()}$0", "Dict comprehension"),
        ("lambda", "lambda", "lambda ${1:x}: ${0:x}", "Lambda expression"),
        ("dataclass", "dataclass", "from dataclasses import dataclass\n\n\n@dataclass\nclass ${1:Name}:\n\t${2:field}: ${3:int}$0", "Dataclass"),
        ("prop", "property", "@property\ndef ${1:name}(self):\n\treturn self._${1:name}$0", "Property"),
        ("djmodel", "Django model", "class ${1:Name}(models.Model):\n\t${2:name} = models.CharField(max_length=${3:100})\n\n\tdef __str__(self):\n\t\treturn self.${2:name}$0", "Django model class"),
        ("djview", "Django view", "def ${1:index}(request):\n\t${2:context = {}}\n\treturn render(request, \"${3:template.html}\", ${4:context})$0", "Django function view"),
        ("djcbv", "Django class view", "class ${1:Name}View(${2:TemplateView}):\n\ttemplate_name = \"${3:template.html}\"$0", "Django class-based view"),
        ("djurl", "Django urlpattern", "path(\"${1:route}/\", ${2:views.index}, name=\"${3:name}\"),$0", "Django URL pattern"),
        ("pdcsv", "pandas read_csv", "import pandas as pd\n\n${1:df} = pd.read_csv(\"${2:data.csv}\")\n${1:df}.head()$0", "Read a CSV file with pandas"),
        ("pltplot", "matplotlib plot", "import matplotlib.pyplot as plt\n\nfig, ax = plt.subplots()\nax.plot(${1:x}, ${2:y})\nax.set_title(\"${3:Title}\")\nplt.show()$0", "Line plot with matplotlib"),
    ]
    .into_iter()
    .map(|(p, n, b, d)| SnippetDef::new(p, n, b, d))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stops_placeholders_and_final() {
        let s = Snippet::parse("for ${1:x} in ${2:xs}:\n\t$0");
        assert_eq!(s.text, "for x in xs:\n\t");
        assert_eq!(s.stops, vec![vec![4..5], vec![9..11], vec![14..14]]);
    }

    #[test]
    fn nested_choice_escape_and_variables() {
        let s = Snippet::parse("${1:a${2:b}c} ${3|one,two|} \\$x \\} $");
        assert_eq!(s.text, "abc one $x } $");
        assert_eq!(s.stops[0], vec![0..3]);
        assert_eq!(s.stops[1], vec![1..2]);
        assert_eq!(s.stops[2], vec![4..7]);
        assert_eq!(s.stops[3], vec![14..14], "implicit final stop at the end");

        let mut vars = Variables::new();
        vars.insert("TM_FILENAME".into(), "main.py".into());
        let s = Snippet::parse_with("# $TM_FILENAME ${UNKNOWN:dflt} ${TM_FILENAME:x}", &vars);
        assert_eq!(s.text, "# main.py dflt main.py");
    }

    #[test]
    fn mirrors_are_grouped() {
        let s = Snippet::parse("$1 = $1");
        assert_eq!(s.stops[0], vec![0..0, 3..3]);
    }

    #[test]
    fn expansion_indents_following_lines() {
        let s = expand("if ${1:c}:\n\t${0:pass}", "    ", &Variables::new());
        assert_eq!(s.text, "if c:\n        pass");
        assert_eq!(s.stops[1], vec![14..18]);
    }

    #[test]
    fn session_tracks_typing_and_tab_jumps() {
        let s = Snippet::parse("for ${1:x} in ${2:xs}:$0");
        let (session, first) = SnippetSession::start(&s, 10);
        let mut session = session.unwrap();
        assert_eq!(first, 14..15);
        // Replace "x" with "item": delete 1, insert 4.
        assert!(session.on_edit(14, 1, 4));
        assert_eq!(session.current(), Some(14..18));
        assert_eq!(session.next_stop(), Some(22..24));
        assert_eq!(session.next_stop(), Some(25..25));
        assert!(session.at_last());
        assert_eq!(session.next_stop(), None);
        assert_eq!(session.prev_stop(), Some(22..24));
        // An edit across a stop boundary ends the session.
        assert!(!session.on_edit(23, 3, 0));
    }

    #[test]
    fn session_without_stops_is_none() {
        let (session, first) = SnippetSession::start(&Snippet::parse("pass"), 3);
        assert!(session.is_none());
        assert_eq!(first, 7..7);
    }

    #[test]
    fn vscode_json_is_read() {
        let json = r#"{
            // comment line
            "Print": { "prefix": ["pr", "print"], "body": ["print($1)", "$0"], "description": "Print" },
            "Bad": { "body": "x" }
        }"#;
        let defs = parse_vscode_snippets(json).unwrap();
        assert_eq!(defs.len(), 2);
        assert_eq!(defs[0].prefix, "pr");
        assert_eq!(defs[1].body, "print($1)\n$0");
        assert!(parse_vscode_snippets("[1]").is_err());
        assert!(parse_vscode_snippets("{").is_err());
    }

    #[test]
    fn builtins_parse_cleanly() {
        let defs = builtin_python_snippets();
        for want in [
            "def",
            "class",
            "if",
            "for",
            "while",
            "try",
            "with",
            "main",
            "lc",
            "adef",
            "dataclass",
            "djmodel",
            "djview",
            "djurl",
            "pdcsv",
            "pltplot",
        ] {
            assert!(defs.iter().any(|d| d.prefix == want), "missing {want}");
        }
        for d in &defs {
            let s = Snippet::parse(&d.body);
            assert!(
                !s.text.contains("${"),
                "{} left syntax: {}",
                d.prefix,
                s.text
            );
            assert!(!s.stops.is_empty());
        }
    }
}
