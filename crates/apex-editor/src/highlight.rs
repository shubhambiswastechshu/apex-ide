//! Syntax highlighting.
//!
//! Python is highlighted with tree-sitter and the grammar's bundled
//! `highlights.scm` (plus a few extra patterns for punctuation, class names
//! and `self`/`cls`). Other languages currently render as plain text.
//!
//! # How overlapping captures are resolved
//!
//! Tree-sitter queries produce overlapping captures (for example a node can
//! be captured as both `@variable` and `@function`). Captures are painted
//! from the largest node to the smallest, so inner nodes win (an f-string
//! interpolation inside a string). For captures of the exact same node the
//! more specific kind wins (see [`TokenKind::priority`]).

use std::fmt;

use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::{Language, Result};

/// Longest a re-parse may take while typing before the previous tree is
/// used instead (see [`Highlighter::is_stale`]).
const PARSE_BUDGET: std::time::Duration = std::time::Duration::from_micros(2500);

/// Extra Python patterns on top of the grammar's highlights query.
const PYTHON_EXTRA_QUERY: &str = r#"
["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ":" ";"] @punctuation.delimiter
(class_definition name: (identifier) @type)
((identifier) @variable.builtin
 (#match? @variable.builtin "^(self|cls)$"))
"#;

/// The category of a highlighted token. A theme maps each kind to a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TokenKind {
    /// Language keywords (`def`, `if`, `import`, ...).
    Keyword,
    /// String literals, including escapes.
    String,
    /// Numeric literals.
    Number,
    /// Comments.
    Comment,
    /// Function and method names (definitions and calls).
    Function,
    /// Types and class names.
    Type,
    /// Variables, parameters and attributes.
    Variable,
    /// Constants (`None`, `True`, `ALL_CAPS` names).
    Constant,
    /// Operators (`+`, `==`, `and`, `not in`, ...).
    Operator,
    /// Brackets and delimiters.
    Punctuation,
    /// Decorators (`@property`, `@app.route`).
    Decorator,
    /// Built-in functions and names (`print`, `len`, `self`).
    Builtin,
    /// Anything without a specific kind.
    Plain,
}

impl TokenKind {
    /// Every variant, in declaration order.
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

    /// Stable lower-case name, suitable as a theme key (e.g. `"keyword"`).
    pub fn name(self) -> &'static str {
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

    /// Maps a tree-sitter capture name (such as `function.builtin` or
    /// `punctuation.special`) to a kind. Returns `None` for unknown names.
    pub fn from_capture_name(name: &str) -> Option<TokenKind> {
        let kind = match name {
            "function.builtin" | "variable.builtin" | "type.builtin" => TokenKind::Builtin,
            "constant.builtin" | "boolean" => TokenKind::Constant,
            "constructor" => TokenKind::Type,
            "property" | "parameter" => TokenKind::Variable,
            "escape" => TokenKind::String,
            "embedded" => TokenKind::Plain,
            "attribute" | "decorator" => TokenKind::Decorator,
            "float" => TokenKind::Number,
            _ => match name.split('.').next().unwrap_or(name) {
                "keyword" | "include" | "conditional" | "repeat" | "exception" => {
                    TokenKind::Keyword
                }
                "string" | "character" => TokenKind::String,
                "number" => TokenKind::Number,
                "comment" => TokenKind::Comment,
                "function" | "method" => TokenKind::Function,
                "type" => TokenKind::Type,
                "variable" => TokenKind::Variable,
                "constant" => TokenKind::Constant,
                "operator" => TokenKind::Operator,
                "punctuation" => TokenKind::Punctuation,
                _ => return None,
            },
        };
        Some(kind)
    }

    /// Tie-break priority when several captures cover exactly the same
    /// node; higher wins. Generic kinds (variable) lose to specific ones
    /// (function), which lose to the most specific (builtin, constant).
    pub fn priority(self) -> u8 {
        match self {
            TokenKind::Plain => 0,
            TokenKind::Variable => 1,
            TokenKind::Punctuation | TokenKind::Operator => 2,
            TokenKind::Function => 3,
            TokenKind::Type => 4,
            TokenKind::Decorator => 5,
            TokenKind::Constant => 6,
            TokenKind::Builtin => 7,
            TokenKind::Number | TokenKind::String | TokenKind::Comment => 8,
            TokenKind::Keyword => 9,
        }
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A highlighted region within a single line.
///
/// Spans never cross a line break, never overlap, are sorted by position,
/// and never have kind [`TokenKind::Plain`] (unhighlighted text produces no
/// span).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HighlightSpan {
    /// UTF-8 byte offset of the start, in the highlighted text.
    pub start_byte: usize,
    /// UTF-8 byte offset of the end (exclusive).
    pub end_byte: usize,
    /// Zero-based line index.
    pub line: usize,
    /// Zero-based start column in chars.
    pub col_start: usize,
    /// Zero-based end column in chars (exclusive).
    pub col_end: usize,
    /// The token kind.
    pub kind: TokenKind,
}

/// Runs of one line: consecutive `(text, kind)` pieces whose texts
/// concatenate to exactly the line's text (without its line break).
pub type LineRuns = Vec<(String, TokenKind)>;

struct PythonEngine {
    parser: Parser,
    query: Query,
    /// Kind for each capture index of `query`.
    capture_kinds: Vec<Option<TokenKind>>,
    cursor: QueryCursor,
    /// Text and tree of the last incremental parse.
    tree: Option<tree_sitter::Tree>,
    /// `tree` was edited but the re-parse ran out of time: it is still
    /// usable (positions are right) but may colour the edit wrongly.
    stale: bool,
    /// No edit since `tree` was parsed.
    unchanged: bool,
}

/// Produces syntax highlighting for one [`Language`].
///
/// Construct once per editor tab (or per language) and reuse it: creating
/// it compiles the highlight query. Each call reparses the full text.
pub struct Highlighter {
    language: Language,
    python: Option<PythonEngine>,
}

impl fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Highlighter")
            .field("language", &self.language)
            .field("syntax_aware", &self.is_syntax_aware())
            .finish()
    }
}

impl Highlighter {
    /// Creates a highlighter. Languages without a grammar yet are accepted
    /// and produce plain output.
    ///
    /// # Errors
    ///
    /// [`EditorError::Grammar`](crate::EditorError::Grammar) or
    /// [`EditorError::Query`](crate::EditorError::Query) if the bundled
    /// grammar or query is broken (a build problem, not a runtime one).
    pub fn new(language: Language) -> Result<Self> {
        let python = match language {
            Language::Python => Some(PythonEngine::new()?),
            _ => None,
        };
        Ok(Self { language, python })
    }

    /// Shorthand for `Highlighter::new(Language::Python)`.
    ///
    /// # Errors
    ///
    /// See [`Highlighter::new`].
    pub fn python() -> Result<Self> {
        Self::new(Language::Python)
    }

    /// The language this highlighter was created for.
    pub fn language(&self) -> Language {
        self.language
    }

    /// Whether output contains real syntax kinds (false means everything is
    /// [`TokenKind::Plain`]).
    pub fn is_syntax_aware(&self) -> bool {
        self.python.is_some()
    }

    /// Highlights `text` and returns sorted, non-overlapping, single-line
    /// spans. `\r\n` line breaks are accepted; the `\r` is never part of a
    /// span.
    pub fn highlight(&mut self, text: &str) -> Vec<HighlightSpan> {
        let kinds = self.byte_kinds(text);
        let mut spans = Vec::new();
        walk_runs(
            text,
            &kinds,
            |line, col_start, col_end, start, end, kind| {
                if kind != TokenKind::Plain {
                    spans.push(HighlightSpan {
                        start_byte: start,
                        end_byte: end,
                        line,
                        col_start,
                        col_end,
                        kind,
                    });
                }
            },
        );
        spans
    }

    /// Highlights `text` and returns, for every line, runs that cover the
    /// whole line text (adjacent runs of the same kind are merged). The
    /// result has exactly one entry per line, counting a final empty line
    /// after a trailing `\n` (matching [`Buffer::len_lines`](crate::Buffer::len_lines)).
    /// Empty lines have no runs.
    pub fn highlight_line_runs(&mut self, text: &str) -> Vec<LineRuns> {
        let kinds = self.byte_kinds(text);
        let line_count = text.bytes().filter(|&b| b == b'\n').count() + 1;
        let mut lines: Vec<LineRuns> = vec![Vec::new(); line_count];
        walk_runs(text, &kinds, |line, _, _, start, end, kind| {
            lines[line].push((text[start..end].to_string(), kind));
        });
        lines
    }

    /// Tells the highlighter about an edit of the text it last parsed, so
    /// the next [`highlight_rope_range`](Self::highlight_rope_range)
    /// re-parses incrementally. Edits must be passed in order.
    pub fn edit(&mut self, e: &crate::LineEdit) {
        if let Some(p) = self.python.as_mut() {
            p.unchanged = false;
        }
        if let Some(tree) = self.python.as_mut().and_then(|p| p.tree.as_mut()) {
            let pt = |(row, column): (usize, usize)| tree_sitter::Point { row, column };
            tree.edit(&tree_sitter::InputEdit {
                start_byte: e.start_byte,
                old_end_byte: e.old_end_byte,
                new_end_byte: e.new_end_byte,
                start_position: pt(e.start_point),
                old_end_position: pt(e.old_end_point),
                new_end_position: pt(e.new_end_point),
            });
        }
    }

    /// Forgets the parse tree (the next call parses from scratch).
    pub fn reset(&mut self) {
        if let Some(p) = self.python.as_mut() {
            p.tree = None;
            p.stale = false;
            p.unchanged = false;
        }
    }

    /// Whether the last range highlight used an out-of-date parse (the
    /// re-parse exceeded its time budget); call
    /// [`finish_parse`](Self::finish_parse) when the user pauses.
    pub fn is_stale(&self) -> bool {
        self.python.as_ref().is_some_and(|p| p.stale)
    }

    /// Completes a parse of `rope` without a time limit.
    pub fn finish_parse(&mut self, rope: &ropey::Rope) {
        if let Some(p) = self.python.as_mut() {
            p.parse_rope(rope, None);
        }
    }

    /// Highlights only `lines` (0-based, end exclusive, clamped) of `rope`
    /// and returns one entry per line in that range, like
    /// [`highlight_line_runs`](Self::highlight_line_runs).
    ///
    /// Meant for an editor that paints only what is on screen: the rope is
    /// parsed in place (no copy of the text), incrementally after
    /// [`edit`](Self::edit), and highlight queries run only over the range.
    pub fn highlight_rope_range(
        &mut self,
        rope: &ropey::Rope,
        lines: std::ops::Range<usize>,
    ) -> Vec<LineRuns> {
        let n = rope.len_lines();
        let first = lines.start.min(n);
        let last = lines.end.min(n);
        if first >= last {
            return Vec::new();
        }
        let start_byte = rope.line_to_byte(first);
        let end_byte = rope.line_to_byte(last);
        let slice = rope.byte_slice(start_byte..end_byte).to_string();
        let kinds = match &mut self.python {
            Some(engine) => engine.rope_range_kinds(rope, start_byte..end_byte),
            None => vec![TokenKind::Plain; slice.len()],
        };
        let mut out: Vec<LineRuns> = vec![Vec::new(); last - first];
        walk_runs(&slice, &kinds, |line, _, _, s, e, kind| {
            if let Some(l) = out.get_mut(line) {
                l.push((slice[s..e].to_string(), kind));
            }
        });
        out
    }

    /// The kind of every byte of `text`.
    fn byte_kinds(&mut self, text: &str) -> Vec<TokenKind> {
        match &mut self.python {
            Some(engine) => engine.byte_kinds(text),
            None => vec![TokenKind::Plain; text.len()],
        }
    }
}

impl PythonEngine {
    fn new() -> Result<Self> {
        let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language)?;
        let source = format!(
            "{}\n{}",
            tree_sitter_python::HIGHLIGHTS_QUERY,
            PYTHON_EXTRA_QUERY
        );
        let query = Query::new(&language, &source)?;
        let capture_kinds = query
            .capture_names()
            .iter()
            .map(|name| TokenKind::from_capture_name(name))
            .collect();
        Ok(Self {
            parser,
            query,
            capture_kinds,
            cursor: QueryCursor::new(),
            tree: None,
            stale: false,
            unchanged: false,
        })
    }

    /// Parses `rope` in place, reusing the (edited) previous tree.
    /// Parses `rope` in place, reusing the (edited) previous tree. With a
    /// `budget`, gives up after that long (error recovery after some edits
    /// can take 100 ms in a large file) and keeps the edited old tree,
    /// marked stale, until a later unbudgeted parse.
    fn parse_rope(
        &mut self,
        rope: &ropey::Rope,
        budget: Option<std::time::Duration>,
    ) -> Option<tree_sitter::Tree> {
        // Nothing changed since the last parse (or since a cancelled one:
        // retrying on every render would cost the budget each time).
        if self.unchanged && self.tree.is_some() && budget.is_some() {
            return self.tree.clone();
        }
        // The first parse of a file always completes.
        let budget = budget.filter(|_| self.tree.is_some());
        let len = rope.len_bytes();
        let started = std::time::Instant::now();
        let mut cancel =
            |_: &tree_sitter::ParseState| budget.is_some_and(|b| started.elapsed() > b);
        let options = tree_sitter::ParseOptions::new().progress_callback(&mut cancel);
        let parsed = self.parser.parse_with_options(
            &mut |byte: usize, _: tree_sitter::Point| -> &[u8] {
                if byte >= len {
                    return &[];
                }
                let (chunk, chunk_start, _, _) = rope.chunk_at_byte(byte);
                &chunk.as_bytes()[byte - chunk_start..]
            },
            self.tree.as_ref(),
            Some(options),
        );
        match parsed {
            Some(tree) => {
                self.tree = Some(tree.clone());
                self.stale = false;
                self.unchanged = true;
                Some(tree)
            }
            None => {
                // Cancelled: do not resume later with different text.
                self.parser.reset();
                self.stale = self.tree.is_some();
                self.unchanged = true;
                self.tree.clone()
            }
        }
    }

    /// Kinds of the bytes in `range` of `rope` (indices relative to the
    /// range start).
    fn rope_range_kinds(
        &mut self,
        rope: &ropey::Rope,
        range: std::ops::Range<usize>,
    ) -> Vec<TokenKind> {
        let mut kinds = vec![TokenKind::Plain; range.len()];
        let Some(tree) = self.parse_rope(rope, Some(PARSE_BUDGET)) else {
            return kinds;
        };
        let len = rope.len_bytes();
        self.cursor.set_byte_range(range.clone());
        let mut captures: Vec<(usize, usize, TokenKind)> = Vec::new();
        let provider = |node: Node<'_>| {
            let r = node.start_byte().min(len)..node.end_byte().min(len);
            rope.byte_slice(r).chunks().map(str::as_bytes)
        };
        let mut matches = self.cursor.matches(&self.query, tree.root_node(), provider);
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let Some(Some(kind)) = self.capture_kinds.get(cap.index as usize).copied() else {
                    continue;
                };
                let node = cap.node;
                let kind = if in_decorator_name(node) {
                    TokenKind::Decorator
                } else {
                    kind
                };
                let (start, end) = (node.start_byte(), node.end_byte().min(len));
                if start < end {
                    captures.push((start, end, kind));
                }
            }
        }
        self.cursor.set_byte_range(0..usize::MAX);
        captures.sort_by(|a, b| {
            (b.1 - b.0)
                .cmp(&(a.1 - a.0))
                .then(a.2.priority().cmp(&b.2.priority()))
        });
        for (start, end, kind) in captures {
            let s = start.max(range.start);
            let e = end.min(range.end);
            if s < e {
                kinds[s - range.start..e - range.start].fill(kind);
            }
        }
        kinds
    }

    fn byte_kinds(&mut self, text: &str) -> Vec<TokenKind> {
        let mut kinds = vec![TokenKind::Plain; text.len()];
        // `parse` only fails when cancelled or without a language; neither
        // can happen here, but degrade to plain text rather than panic.
        let Some(tree) = self.parser.parse(text, None) else {
            return kinds;
        };

        // (start, end, kind) for every capture we understand.
        let mut captures: Vec<(usize, usize, TokenKind)> = Vec::new();
        let mut matches = self
            .cursor
            .matches(&self.query, tree.root_node(), text.as_bytes());
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let Some(Some(kind)) = self.capture_kinds.get(cap.index as usize).copied() else {
                    continue;
                };
                let node = cap.node;
                let kind = if in_decorator_name(node) {
                    TokenKind::Decorator
                } else {
                    kind
                };
                let (start, end) = (node.start_byte(), node.end_byte().min(text.len()));
                if start < end {
                    captures.push((start, end, kind));
                }
            }
        }

        // Paint large nodes first so nested nodes override them; for equal
        // ranges, paint low priority first so the most specific kind wins.
        captures.sort_by(|a, b| {
            (b.1 - b.0)
                .cmp(&(a.1 - a.0))
                .then(a.2.priority().cmp(&b.2.priority()))
        });
        for (start, end, kind) in captures {
            kinds[start..end].fill(kind);
        }
        kinds
    }
}

/// Whether `node` is (part of) a decorator's name, e.g. `property` in
/// `@property` or `app`/`route` in `@app.route("/")` — but not the
/// decorator call's arguments.
fn in_decorator_name(node: Node<'_>) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        match n.kind() {
            "decorator" => return true,
            "argument_list" | "decorated_definition" | "block" | "module" => return false,
            _ => current = n.parent(),
        }
    }
    false
}

/// Walks `text`, calling `emit(line, col_start, col_end, start_byte,
/// end_byte, kind)` for each maximal same-kind run within a line. Line
/// breaks (`\n`, and a `\r` directly before it) are never part of a run.
fn walk_runs(
    text: &str,
    kinds: &[TokenKind],
    mut emit: impl FnMut(usize, usize, usize, usize, usize, TokenKind),
) {
    let bytes = text.as_bytes();
    let mut line = 0;
    let mut col = 0;
    // (start_byte, start_col, kind) of the open run.
    let mut run: Option<(usize, usize, TokenKind)> = None;

    for (i, c) in text.char_indices() {
        let is_break = c == '\n' || (c == '\r' && bytes.get(i + 1) == Some(&b'\n'));
        if is_break {
            if let Some((start, start_col, kind)) = run.take() {
                emit(line, start_col, col, start, i, kind);
            }
            if c == '\n' {
                line += 1;
                col = 0;
            }
            continue;
        }
        let kind = kinds.get(i).copied().unwrap_or(TokenKind::Plain);
        match run {
            Some((_, _, k)) if k == kind => {}
            Some((start, start_col, k)) => {
                emit(line, start_col, col, start, i, k);
                run = Some((i, col, kind));
            }
            None => run = Some((i, col, kind)),
        }
        col += 1;
    }
    if let Some((start, start_col, kind)) = run {
        emit(line, start_col, col, start, text.len(), kind);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn py() -> Highlighter {
        Highlighter::python().expect("bundled python grammar and query compile")
    }

    /// Kind of the first span whose text equals `token` on `line`.
    fn kind_of(text: &str, line: usize, token: &str) -> Option<TokenKind> {
        let runs = py().highlight_line_runs(text);
        runs.get(line)?
            .iter()
            .find(|(t, _)| t.trim() == token)
            .map(|&(_, k)| k)
    }

    #[test]
    fn query_compiles_and_all_captures_are_known_or_ignored() {
        let h = py();
        let engine = h.python.as_ref().unwrap();
        // Every capture of the shipped query maps to a kind.
        for (name, kind) in engine
            .query
            .capture_names()
            .iter()
            .zip(&engine.capture_kinds)
        {
            assert!(kind.is_some(), "unmapped capture @{name}");
        }
    }

    #[test]
    fn capture_name_mapping() {
        assert_eq!(
            TokenKind::from_capture_name("keyword"),
            Some(TokenKind::Keyword)
        );
        assert_eq!(
            TokenKind::from_capture_name("keyword.function"),
            Some(TokenKind::Keyword)
        );
        assert_eq!(
            TokenKind::from_capture_name("function.method"),
            Some(TokenKind::Function)
        );
        assert_eq!(
            TokenKind::from_capture_name("function.builtin"),
            Some(TokenKind::Builtin)
        );
        assert_eq!(
            TokenKind::from_capture_name("constant.builtin"),
            Some(TokenKind::Constant)
        );
        assert_eq!(
            TokenKind::from_capture_name("constructor"),
            Some(TokenKind::Type)
        );
        assert_eq!(
            TokenKind::from_capture_name("property"),
            Some(TokenKind::Variable)
        );
        assert_eq!(
            TokenKind::from_capture_name("punctuation.special"),
            Some(TokenKind::Punctuation)
        );
        assert_eq!(
            TokenKind::from_capture_name("escape"),
            Some(TokenKind::String)
        );
        assert_eq!(
            TokenKind::from_capture_name("embedded"),
            Some(TokenKind::Plain)
        );
        assert_eq!(TokenKind::from_capture_name("nonsense"), None);
        assert_eq!(TokenKind::Keyword.to_string(), "keyword");
        assert_eq!(TokenKind::ALL.len(), 13);
    }

    #[test]
    fn python_token_kinds() {
        let src = "import os\n\
                   @property\n\
                   def area(self, r: int) -> float:\n    \
                   \"\"\"Doc.\"\"\"\n    \
                   return 3.14 * r ** 2  # pi r^2\n\
                   class Circle(Base):\n    \
                   MAX = None\n\
                   print(len([1, 2]))\n\
                   obj.method(x)\n";
        assert_eq!(kind_of(src, 0, "import"), Some(TokenKind::Keyword));
        assert_eq!(kind_of(src, 1, "@property"), Some(TokenKind::Decorator));
        assert_eq!(kind_of(src, 2, "def"), Some(TokenKind::Keyword));
        assert_eq!(kind_of(src, 2, "area"), Some(TokenKind::Function));
        assert_eq!(kind_of(src, 2, "self"), Some(TokenKind::Builtin));
        assert_eq!(kind_of(src, 2, "int"), Some(TokenKind::Type));
        assert_eq!(kind_of(src, 2, "->"), Some(TokenKind::Operator));
        assert_eq!(kind_of(src, 3, "\"\"\"Doc.\"\"\""), Some(TokenKind::String));
        assert_eq!(kind_of(src, 4, "3.14"), Some(TokenKind::Number));
        assert_eq!(kind_of(src, 4, "**"), Some(TokenKind::Operator));
        assert_eq!(kind_of(src, 4, "# pi r^2"), Some(TokenKind::Comment));
        assert_eq!(kind_of(src, 5, "Circle"), Some(TokenKind::Type));
        assert_eq!(kind_of(src, 6, "MAX"), Some(TokenKind::Constant));
        assert_eq!(kind_of(src, 6, "None"), Some(TokenKind::Constant));
        assert_eq!(kind_of(src, 7, "print"), Some(TokenKind::Builtin));
        assert_eq!(kind_of(src, 7, "len"), Some(TokenKind::Builtin));
        assert_eq!(kind_of(src, 7, "1"), Some(TokenKind::Number));
        assert_eq!(kind_of(src, 8, "obj"), Some(TokenKind::Variable));
        assert_eq!(kind_of(src, 8, "method"), Some(TokenKind::Function));
        assert_eq!(kind_of(src, 8, "("), Some(TokenKind::Punctuation));
    }

    #[test]
    fn decorator_call_arguments_keep_their_kinds() {
        let src = "@app.route(\"/home\")\ndef home(): pass\n";
        let runs = py().highlight_line_runs(src);
        let line0 = &runs[0];
        assert!(line0
            .iter()
            .any(|(t, k)| t.contains("app") && *k == TokenKind::Decorator));
        assert!(line0
            .iter()
            .any(|(t, k)| t == "\"/home\"" && *k == TokenKind::String));
    }

    #[test]
    fn fstring_interpolation_is_not_string_coloured() {
        let src = "s = f\"hi {name}!\"\n";
        assert_eq!(kind_of(src, 0, "name"), Some(TokenKind::Variable));
        assert_eq!(kind_of(src, 0, "{"), Some(TokenKind::Punctuation));
    }

    fn assert_spans_well_formed(text: &str, spans: &[HighlightSpan]) {
        let mut prev_end = 0;
        for s in spans {
            assert!(s.start_byte >= prev_end, "overlap/unsorted: {s:?}");
            assert!(s.start_byte < s.end_byte, "empty span: {s:?}");
            assert_ne!(s.kind, TokenKind::Plain);
            let piece = &text[s.start_byte..s.end_byte];
            assert!(
                !piece.contains('\n') && !piece.ends_with('\r'),
                "crosses line: {s:?}"
            );
            assert_eq!(piece.chars().count(), s.col_end - s.col_start);
            // line/col agree with byte offsets
            let line_start = text[..s.start_byte].rfind('\n').map_or(0, |i| i + 1);
            assert_eq!(text[..s.start_byte].matches('\n').count(), s.line);
            assert_eq!(text[line_start..s.start_byte].chars().count(), s.col_start);
            prev_end = s.end_byte;
        }
    }

    #[test]
    fn spans_are_sorted_non_overlapping_and_single_line() {
        let src = "def f(x):\r\n    \"\"\"multi\r\n    line é😀\"\"\"\r\n    return {'k': [x, 1.5]}  # ok\r\n";
        let spans = py().highlight(src);
        assert!(!spans.is_empty());
        assert_spans_well_formed(src, &spans);
        // The docstring is split into one span per line.
        let strings: Vec<_> = spans
            .iter()
            .filter(|s| s.kind == TokenKind::String)
            .collect();
        assert!(strings.iter().any(|s| s.line == 1) && strings.iter().any(|s| s.line == 2));
    }

    #[test]
    fn line_runs_cover_every_line_exactly() {
        let src = "x = 'é'\n\n  if x:\n\tpass\r\ny";
        let runs = py().highlight_line_runs(src);
        let expected: Vec<&str> = src
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l))
            .collect();
        assert_eq!(runs.len(), expected.len());
        for (line_runs, want) in runs.iter().zip(&expected) {
            let joined: String = line_runs.iter().map(|(t, _)| t.as_str()).collect();
            assert_eq!(&joined, want);
            // adjacent runs differ in kind
            for pair in line_runs.windows(2) {
                assert_ne!(pair[0].1, pair[1].1);
            }
        }
        assert!(runs[1].is_empty());
    }

    #[test]
    fn trailing_newline_gives_empty_last_line() {
        let runs = py().highlight_line_runs("x\n");
        assert_eq!(runs.len(), 2);
        assert!(runs[1].is_empty());
        assert_eq!(py().highlight_line_runs("").len(), 1);
    }

    #[test]
    fn invalid_python_still_highlights() {
        let src = "def (:\n  'unterminated\nimport\n";
        let mut h = py();
        let spans = h.highlight(src);
        assert_spans_well_formed(src, &spans);
        assert!(spans.iter().any(|s| s.kind == TokenKind::Keyword));
    }

    #[test]
    fn highlighter_is_reusable() {
        let mut h = py();
        let a = h.highlight("x = 1\n");
        let b = h.highlight("x = 1\n");
        assert_eq!(a, b);
        assert_ne!(h.highlight("def f(): pass"), a);
    }

    #[test]
    fn plain_languages_produce_plain_runs() {
        let mut h = Highlighter::new(Language::Toml).unwrap();
        assert!(!h.is_syntax_aware());
        assert_eq!(h.language(), Language::Toml);
        assert!(h.highlight("[tool]\nx = 1\n").is_empty());
        assert_eq!(
            h.highlight_line_runs("[tool]\nx = 1"),
            vec![
                vec![("[tool]".to_string(), TokenKind::Plain)],
                vec![("x = 1".to_string(), TokenKind::Plain)],
            ]
        );
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;
    use crate::Buffer;

    #[test]
    fn rope_ranges_match_full_highlighting_across_edits() {
        let mut b = Buffer::from_text(
            "x = 1\ndef f(a):\n    \"\"\"doc\n    more\"\"\"\n    return a  # c\n",
        );
        let mut ranged = Highlighter::python().unwrap();
        let mut rev = b.revision();
        for step in 0..5 {
            for e in b.line_edits_since(rev).unwrap() {
                ranged.edit(&e);
            }
            rev = b.revision();
            let full = Highlighter::python()
                .unwrap()
                .highlight_line_runs(&b.text());
            let n = full.len();
            for start in 0..n {
                let end = (start + 2).min(n);
                let part = ranged.highlight_rope_range(b.rope(), start..end);
                assert_eq!(part, full[start..end].to_vec(), "step {step} start {start}");
            }
            match step {
                0 => b.insert(0, "import os\n").unwrap(),
                1 => {
                    let at = b.text().find("doc").unwrap();
                    let c = b.byte_to_char(at).unwrap();
                    b.replace_range(c..c + 3, "dóc\n\"\"\"\nx = '").unwrap();
                }
                2 => {
                    let end = b.len_chars();
                    b.insert(end, "print('é')\n").unwrap();
                }
                3 => {
                    b.delete(0..12).unwrap();
                }
                _ => {}
            }
        }
        assert!(ranged.highlight_rope_range(b.rope(), 99..120).is_empty());
    }
}

#[cfg(test)]
mod perf_probe {
    #[test]
    #[ignore]
    fn ten_thousand_lines() {
        let unit =
            "class A:\n    def f(self, x):\n        return [i * 2 for i in range(x)]  # note\n\n";
        let text = unit.repeat(2500);
        let mut h = super::Highlighter::python().unwrap();
        let t = std::time::Instant::now();
        let runs = h.highlight_line_runs(&text);
        eprintln!("lines={} highlight={:?}", runs.len(), t.elapsed());
        let mut p = tree_sitter::Parser::new();
        p.set_language(&tree_sitter_python::LANGUAGE.into())
            .unwrap();
        let t = std::time::Instant::now();
        let tree = p.parse(&text, None).unwrap();
        eprintln!("parse={:?}", t.elapsed());
        let t = std::time::Instant::now();
        let _ = p.parse(&text, Some(&tree)).unwrap();
        eprintln!("reparse={:?}", t.elapsed());
        let mut b = crate::Buffer::from_text(&text);
        let mut r = super::Highlighter::python().unwrap();
        let _ = r.highlight_rope_range(b.rope(), 5000..5060);
        let rev = b.revision();
        b.insert(b.line_to_char(5001).unwrap(), "x = 1 + ").unwrap();
        let t = std::time::Instant::now();
        for e in b.line_edits_since(rev).unwrap() {
            r.edit(&e);
        }
        let _ = r.highlight_rope_range(b.rope(), 5000..5060);
        eprintln!("range after edit={:?}", t.elapsed());
        let t = std::time::Instant::now();
        let _ = r.highlight_rope_range(b.rope(), 5000..5060);
        eprintln!("range again={:?}", t.elapsed());
        let rev = b.revision();
        b.insert(b.line_to_char(5002).unwrap() + 8, "z").unwrap();
        let t = std::time::Instant::now();
        for e in b.line_edits_since(rev).unwrap() {
            r.edit(&e);
        }
        let _ = r.highlight_rope_range(b.rope(), 5000..5060);
        eprintln!("range after typing z={:?}", t.elapsed());
    }
}
