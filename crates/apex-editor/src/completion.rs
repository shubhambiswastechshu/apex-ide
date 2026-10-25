//! Completion items, fuzzy filtering/ranking, and a built-in Python
//! provider that works without a language server (keywords, builtins,
//! identifiers in the file, snippets).

use std::borrow::Cow;
use std::collections::HashSet;
use std::rc::Rc;

use crate::snippet::SnippetDef;

/// What a completion item is, for its icon and ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CompletionKind {
    /// A language keyword.
    Keyword,
    /// A snippet.
    Snippet,
    /// A function or method.
    Function,
    /// A class or type.
    Class,
    /// A module or package.
    Module,
    /// A variable, parameter or attribute.
    Variable,
    /// A constant or enum member.
    Constant,
    /// A property or field.
    Property,
    /// Plain text (e.g. a word from the file).
    Text,
}

impl CompletionKind {
    /// One-letter badge for the popup.
    pub fn badge(self) -> &'static str {
        match self {
            CompletionKind::Keyword => "k",
            CompletionKind::Snippet => "s",
            CompletionKind::Function => "f",
            CompletionKind::Class => "C",
            CompletionKind::Module => "m",
            CompletionKind::Variable => "v",
            CompletionKind::Constant => "c",
            CompletionKind::Property => "p",
            CompletionKind::Text => "t",
        }
    }
}

/// Where an item came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CompletionSource {
    /// The language server.
    Lsp,
    /// The built-in provider.
    Builtin,
}

/// One entry of the completion popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Shown in the list (and matched against when `filter_text` is empty).
    pub label: String,
    /// Kind, for the icon.
    pub kind: CompletionKind,
    /// Extra detail (signature, type, snippet description).
    pub detail: String,
    /// Longer documentation, plain text.
    pub documentation: String,
    /// What to insert in place of the typed word.
    pub insert: String,
    /// `insert` uses snippet syntax.
    pub is_snippet: bool,
    /// Text to match the typed word against.
    pub filter_text: String,
    /// Server ordering key (lower first) among equal scores.
    pub sort_text: String,
    /// Origin.
    pub source: CompletionSource,
}

impl CompletionItem {
    /// A plain item inserting its label.
    pub fn simple(label: &str, kind: CompletionKind, detail: &str) -> Self {
        Self {
            label: label.into(),
            kind,
            detail: detail.into(),
            documentation: String::new(),
            insert: label.into(),
            is_snippet: false,
            filter_text: label.into(),
            sort_text: String::new(),
            source: CompletionSource::Builtin,
        }
    }

    /// A snippet item.
    pub fn snippet(def: &SnippetDef) -> Self {
        Self {
            label: def.prefix.clone(),
            kind: CompletionKind::Snippet,
            detail: if def.description.is_empty() {
                def.name.clone()
            } else {
                def.description.clone()
            },
            documentation: def.preview(),
            insert: def.body.clone(),
            is_snippet: true,
            filter_text: def.prefix.clone(),
            sort_text: String::new(),
            source: CompletionSource::Builtin,
        }
    }
}

/// Scores how well `pattern` fuzzy-matches `candidate` (case-insensitive
/// subsequence). `None` if it does not match. Higher is better: prefix
/// matches, consecutive runs, word-boundary hits and exact case score more;
/// an empty pattern matches everything with score 0.
pub fn fuzzy_score(pattern: &str, candidate: &str) -> Option<i32> {
    if pattern.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = candidate.chars().collect();
    let mut score = 0i32;
    let mut ci = 0usize;
    let mut prev_match: Option<usize> = None;
    for (pi, pc) in pattern.chars().enumerate() {
        let pl = pc.to_lowercase().next().unwrap_or(pc);
        let mut found = None;
        while ci < cand.len() {
            let c = cand[ci];
            if c.to_lowercase().next().unwrap_or(c) == pl {
                found = Some(ci);
                break;
            }
            ci += 1;
        }
        let at = found?;
        let boundary = at == 0
            || matches!(cand[at - 1], '_' | '.' | '-' | ' ')
            || (cand[at].is_uppercase() && cand[at - 1].is_lowercase());
        score += 1;
        if cand[at] == pc {
            score += 1;
        }
        if at == pi && prev_match.is_none_or(|p| p + 1 == at) {
            score += 6; // still a prefix
        } else if prev_match.is_some_and(|p| p + 1 == at) {
            score += 4;
        } else if boundary {
            score += 3;
        } else {
            score -= 1;
        }
        prev_match = Some(at);
        ci = at + 1;
    }
    // Shorter candidates win ties; an exact match wins outright.
    score -= (cand.len() as i32 - pattern.chars().count() as i32).min(20) / 4;
    if candidate == pattern {
        score += 10;
    }
    Some(score)
}

/// Filters `items` by `prefix` and returns indices, best first. Ties keep
/// language-server items (in server order) before built-in ones, then
/// shorter and alphabetical labels. Private names (`_x`) sink unless the
/// prefix starts with `_`.
pub fn rank(items: &[CompletionItem], prefix: &str) -> Vec<usize> {
    rank_among(items, 0..items.len(), prefix)
}

/// Like [`rank`], but only considers the items at `candidates`. Fuzzy
/// matching is a subsequence test, so whatever matches `"pri"` is among
/// what matched `"pr"`: filtering while the user types can rank the
/// previous matches instead of every item.
pub fn rank_among(
    items: &[CompletionItem],
    candidates: impl IntoIterator<Item = usize>,
    prefix: &str,
) -> Vec<usize> {
    let mut scored: Vec<(i32, usize)> = candidates
        .into_iter()
        .filter_map(|i| {
            let it = items.get(i)?;
            let key = if it.filter_text.is_empty() {
                &it.label
            } else {
                &it.filter_text
            };
            let mut s = fuzzy_score(prefix, key)?;
            if !prefix.starts_with('_') && key.starts_with('_') {
                s -= if key.starts_with("__") { 8 } else { 4 };
            }
            Some((s, i))
        })
        .collect();
    scored.sort_by(|(sa, ia), (sb, ib)| {
        let (a, b) = (&items[*ia], &items[*ib]);
        sb.cmp(sa)
            .then(a.source.cmp(&b.source))
            .then(a.sort_text.cmp(&b.sort_text))
            .then(a.label.len().cmp(&b.label.len()))
            .then(a.label.cmp(&b.label))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

/// Merges language-server items into built-in ones: a built-in item whose
/// label the server also returned is dropped (snippets are kept).
pub fn merge(lsp: Vec<CompletionItem>, builtin: Vec<CompletionItem>) -> Vec<CompletionItem> {
    let labels: HashSet<String> = lsp.iter().map(|i| i.label.clone()).collect();
    let mut out = lsp;
    out.extend(
        builtin
            .into_iter()
            .filter(|i| i.kind == CompletionKind::Snippet || !labels.contains(&i.label)),
    );
    out
}

/// Python keywords.
pub const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "match", "case", "nonlocal", "not", "or", "pass", "raise", "return",
    "try", "while", "with", "yield",
];

/// Python built-in functions, types and constants.
pub const PYTHON_BUILTINS: &[(&str, CompletionKind)] = &[
    ("abs", CompletionKind::Function),
    ("aiter", CompletionKind::Function),
    ("all", CompletionKind::Function),
    ("anext", CompletionKind::Function),
    ("any", CompletionKind::Function),
    ("ascii", CompletionKind::Function),
    ("bin", CompletionKind::Function),
    ("bool", CompletionKind::Class),
    ("breakpoint", CompletionKind::Function),
    ("bytearray", CompletionKind::Class),
    ("bytes", CompletionKind::Class),
    ("callable", CompletionKind::Function),
    ("chr", CompletionKind::Function),
    ("classmethod", CompletionKind::Class),
    ("compile", CompletionKind::Function),
    ("complex", CompletionKind::Class),
    ("delattr", CompletionKind::Function),
    ("dict", CompletionKind::Class),
    ("dir", CompletionKind::Function),
    ("divmod", CompletionKind::Function),
    ("enumerate", CompletionKind::Class),
    ("eval", CompletionKind::Function),
    ("exec", CompletionKind::Function),
    ("filter", CompletionKind::Class),
    ("float", CompletionKind::Class),
    ("format", CompletionKind::Function),
    ("frozenset", CompletionKind::Class),
    ("getattr", CompletionKind::Function),
    ("globals", CompletionKind::Function),
    ("hasattr", CompletionKind::Function),
    ("hash", CompletionKind::Function),
    ("help", CompletionKind::Function),
    ("hex", CompletionKind::Function),
    ("id", CompletionKind::Function),
    ("input", CompletionKind::Function),
    ("int", CompletionKind::Class),
    ("isinstance", CompletionKind::Function),
    ("issubclass", CompletionKind::Function),
    ("iter", CompletionKind::Function),
    ("len", CompletionKind::Function),
    ("list", CompletionKind::Class),
    ("locals", CompletionKind::Function),
    ("map", CompletionKind::Class),
    ("max", CompletionKind::Function),
    ("memoryview", CompletionKind::Class),
    ("min", CompletionKind::Function),
    ("next", CompletionKind::Function),
    ("object", CompletionKind::Class),
    ("oct", CompletionKind::Function),
    ("open", CompletionKind::Function),
    ("ord", CompletionKind::Function),
    ("pow", CompletionKind::Function),
    ("print", CompletionKind::Function),
    ("property", CompletionKind::Class),
    ("range", CompletionKind::Class),
    ("repr", CompletionKind::Function),
    ("reversed", CompletionKind::Class),
    ("round", CompletionKind::Function),
    ("set", CompletionKind::Class),
    ("setattr", CompletionKind::Function),
    ("slice", CompletionKind::Class),
    ("sorted", CompletionKind::Function),
    ("staticmethod", CompletionKind::Class),
    ("str", CompletionKind::Class),
    ("sum", CompletionKind::Function),
    ("super", CompletionKind::Class),
    ("tuple", CompletionKind::Class),
    ("type", CompletionKind::Class),
    ("vars", CompletionKind::Function),
    ("zip", CompletionKind::Class),
    ("__import__", CompletionKind::Function),
    ("__name__", CompletionKind::Variable),
    ("__file__", CompletionKind::Variable),
    ("self", CompletionKind::Variable),
    ("Exception", CompletionKind::Class),
    ("ValueError", CompletionKind::Class),
    ("TypeError", CompletionKind::Class),
    ("KeyError", CompletionKind::Class),
    ("IndexError", CompletionKind::Class),
    ("RuntimeError", CompletionKind::Class),
    ("AttributeError", CompletionKind::Class),
    ("ImportError", CompletionKind::Class),
    ("FileNotFoundError", CompletionKind::Class),
    ("OSError", CompletionKind::Class),
    ("NotImplementedError", CompletionKind::Class),
    ("StopIteration", CompletionKind::Class),
    ("ZeroDivisionError", CompletionKind::Class),
    ("NotImplemented", CompletionKind::Constant),
    ("Ellipsis", CompletionKind::Constant),
];

/// Identifiers in `text` (at least two chars, not starting with a digit),
/// with what they look like: a name after `def` is a function, after
/// `class` a class, after `import` a module. `skip` (the word being typed)
/// is left out. `after_dot` keeps only names that appear after a `.`.
pub fn identifiers_in(text: &str, skip: &str, after_dot: bool) -> Vec<(String, CompletionKind)> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut prev_word = "";
    let mut in_comment = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'#' {
            in_comment = true;
        } else if c == b'\n' {
            in_comment = false;
        }
        let is_start = c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
        if !is_start || in_comment {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] >= 0x80)
        {
            i += 1;
        }
        // Keep to char boundaries for non-ASCII identifiers.
        while !text.is_char_boundary(i) {
            i += 1;
        }
        let word = &text[start..i];
        let dotted = start > 0 && bytes[start - 1] == b'.';
        let kind = match prev_word {
            "def" => CompletionKind::Function,
            "class" => CompletionKind::Class,
            "import" | "from" => CompletionKind::Module,
            _ if word
                .chars()
                .all(|c| c.is_uppercase() || c == '_' || c.is_ascii_digit())
                && word.len() > 1 =>
            {
                CompletionKind::Constant
            }
            _ if dotted => CompletionKind::Property,
            _ => CompletionKind::Text,
        };
        prev_word = word;
        if word.chars().count() < 2 || word == skip || (after_dot && !dotted) {
            continue;
        }
        if PYTHON_KEYWORDS.contains(&word) {
            continue;
        }
        if seen.insert(word) {
            out.push((word.to_owned(), kind));
        }
    }
    out
}

/// Identifiers found in one text: what the popup offers besides keywords,
/// builtins and snippets. Plain data, so a worker thread can build it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentScan {
    /// Every identifier, with the kind of its first use.
    pub all: Vec<(String, CompletionKind)>,
    /// Identifiers used after a `.`.
    pub dotted: Vec<(String, CompletionKind)>,
}

impl IdentScan {
    /// Scans `text` (see [`identifiers_in`]).
    pub fn of(text: &str) -> Self {
        Self {
            all: identifiers_in(text, "", false),
            dotted: identifiers_in(text, "", true),
        }
    }
}

/// The identifier scan of one revision of one file, with the candidate
/// lists built from it.
#[derive(Debug, Clone)]
struct FileIdents {
    file: u64,
    revision: u64,
    scan: IdentScan,
    names: HashSet<String>,
    /// Ready candidate lists: `[normal, after a dot]`.
    lists: [Option<Rc<Vec<CompletionItem>>>; 2],
}

/// The built-in Python completion provider, built for speed: keywords,
/// builtins and snippets are made into items once; the identifiers of the
/// open file are cached per revision (the app refreshes them off the UI
/// thread) and the candidate lists are shared, not copied, between popups.
///
/// Between refreshes, names typed near the caret since the last scan are
/// picked up from a small window of text around it.
#[derive(Debug, Clone, Default)]
pub struct PythonIndex {
    fixed: Vec<CompletionItem>,
    builtin_names: HashSet<&'static str>,
    file: Option<FileIdents>,
}

impl PythonIndex {
    /// Indexes the keywords, builtins and `snippets`.
    pub fn new(snippets: &[SnippetDef]) -> Self {
        let mut fixed =
            Vec::with_capacity(PYTHON_KEYWORDS.len() + PYTHON_BUILTINS.len() + snippets.len());
        fixed.extend(
            PYTHON_KEYWORDS
                .iter()
                .map(|k| CompletionItem::simple(k, CompletionKind::Keyword, "keyword")),
        );
        fixed.extend(
            PYTHON_BUILTINS
                .iter()
                .map(|(n, k)| CompletionItem::simple(n, *k, "builtin")),
        );
        fixed.extend(snippets.iter().map(CompletionItem::snippet));
        Self {
            fixed,
            builtin_names: PYTHON_BUILTINS.iter().map(|(n, _)| *n).collect(),
            file: None,
        }
    }

    /// The keyword, builtin and snippet items.
    pub fn fixed(&self) -> &[CompletionItem] {
        &self.fixed
    }

    /// Whether the identifiers of `file` (any stable id) are cached for
    /// exactly `revision`.
    pub fn is_fresh(&self, file: u64, revision: u64) -> bool {
        self.file
            .as_ref()
            .is_some_and(|f| f.file == file && f.revision == revision)
    }

    /// Stores a scan of revision `revision` of `file`. A scan older than the
    /// cached one of the same file is ignored (scans can finish out of
    /// order).
    pub fn set_identifiers(&mut self, file: u64, revision: u64, scan: IdentScan) {
        if self
            .file
            .as_ref()
            .is_some_and(|f| f.file == file && f.revision > revision)
        {
            return;
        }
        let names = scan.all.iter().map(|(n, _)| n.clone()).collect();
        self.file = Some(FileIdents {
            file,
            revision,
            scan,
            names,
            lists: [None, None],
        });
    }

    /// The candidates for a popup in `file`: keywords, builtins and
    /// snippets (unless `after_dot`), the file's cached identifiers, and
    /// any new names in `nearby` (the text around the caret). When nothing
    /// of `file` is cached yet, `full_text` is scanned now (once per file).
    ///
    /// The returned list is shared with later calls while it stays valid,
    /// so opening a popup does not copy thousands of items.
    pub fn complete<'a>(
        &mut self,
        file: u64,
        revision: u64,
        full_text: impl FnOnce() -> Cow<'a, str>,
        nearby: &str,
        after_dot: bool,
    ) -> Rc<Vec<CompletionItem>> {
        if self.file.as_ref().is_none_or(|f| f.file != file) {
            self.set_identifiers(file, revision, IdentScan::of(&full_text()));
        }
        let Some(cache) = self.file.as_mut() else {
            return Rc::new(Vec::new());
        };
        let slot = usize::from(after_dot);
        let base = match &cache.lists[slot] {
            Some(list) => list.clone(),
            None => {
                let idents = if after_dot {
                    &cache.scan.dotted
                } else {
                    &cache.scan.all
                };
                let mut items = Vec::with_capacity(self.fixed.len() + idents.len());
                if !after_dot {
                    items.extend(self.fixed.iter().cloned());
                }
                items.extend(
                    idents
                        .iter()
                        .filter(|(n, _)| after_dot || !self.builtin_names.contains(n.as_str()))
                        .map(|(n, k)| file_item(n, *k)),
                );
                let list = Rc::new(items);
                cache.lists[slot] = Some(list.clone());
                list
            }
        };
        if cache.revision == revision || nearby.is_empty() {
            return base;
        }
        let extra: Vec<CompletionItem> = identifiers_in(nearby, "", after_dot)
            .into_iter()
            .filter(|(n, _)| {
                !cache.names.contains(n.as_str())
                    && (after_dot || !self.builtin_names.contains(n.as_str()))
            })
            .map(|(n, k)| file_item(&n, k))
            .collect();
        if extra.is_empty() {
            return base;
        }
        let mut items = Vec::with_capacity(base.len() + extra.len());
        items.extend(base.iter().cloned());
        items.extend(extra);
        Rc::new(items)
    }
}

/// An identifier of the open file as a popup item (sorted after others).
fn file_item(name: &str, kind: CompletionKind) -> CompletionItem {
    let mut it = CompletionItem::simple(name, kind, IN_THIS_FILE);
    it.sort_text = "~".into();
    it
}

/// Detail text of identifiers found in the open file.
pub const IN_THIS_FILE: &str = "in this file";

/// The built-in Python provider: after a `.` only attribute-like names from
/// the file; otherwise keywords, builtins, file identifiers and snippets.
pub fn python_fallback(
    text: &str,
    typed: &str,
    after_dot: bool,
    snippets: &[SnippetDef],
) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    if !after_dot {
        items.extend(
            PYTHON_KEYWORDS
                .iter()
                .map(|k| CompletionItem::simple(k, CompletionKind::Keyword, "keyword")),
        );
        items.extend(
            PYTHON_BUILTINS
                .iter()
                .map(|(n, k)| CompletionItem::simple(n, *k, "builtin")),
        );
        items.extend(snippets.iter().map(CompletionItem::snippet));
    }
    let builtin_names: HashSet<&str> = PYTHON_BUILTINS.iter().map(|(n, _)| *n).collect();
    items.extend(
        identifiers_in(text, typed, after_dot)
            .into_iter()
            .filter(|(n, _)| after_dot || !builtin_names.contains(n.as_str()))
            .map(|(n, k)| file_item(&n, k)),
    );
    items
}

/// Generic provider for non-Python files: words from the file.
pub fn word_fallback(text: &str, typed: &str) -> Vec<CompletionItem> {
    identifiers_in(text, typed, false)
        .into_iter()
        .map(|(n, _)| CompletionItem::simple(&n, CompletionKind::Text, ""))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snippet::builtin_python_snippets;

    #[test]
    fn fuzzy_matches_subsequences() {
        assert!(fuzzy_score("prt", "print").is_some());
        assert!(fuzzy_score("xyz", "print").is_none());
        assert_eq!(fuzzy_score("", "x"), Some(0));
        // Prefix beats scattered; case-insensitive.
        let prefix = fuzzy_score("pri", "print").unwrap();
        let scattered = fuzzy_score("pri", "superior_index").unwrap();
        assert!(prefix > scattered);
        assert!(fuzzy_score("RT", "read_text").is_some());
        // Word boundary beats mid-word.
        let boundary = fuzzy_score("rc", "read_csv").unwrap();
        let mid = fuzzy_score("rc", "pyrocket").unwrap();
        assert!(boundary > mid);
        assert!(fuzzy_score("len", "len").unwrap() > fuzzy_score("len", "length").unwrap());
    }

    #[test]
    fn ranking_orders_and_sinks_private_names() {
        let items: Vec<CompletionItem> = ["__init__", "print", "_private", "pprint", "isinstance"]
            .iter()
            .map(|l| CompletionItem::simple(l, CompletionKind::Function, ""))
            .collect();
        let order: Vec<&str> = rank(&items, "pr")
            .into_iter()
            .map(|i| items[i].label.as_str())
            .collect();
        assert_eq!(order[0], "print");
        assert!(order.contains(&"pprint"));
        assert!(!order.contains(&"isinstance"));
        let all = rank(&items, "");
        assert_eq!(items[*all.last().unwrap()].label, "__init__");
        let under = rank(&items, "_p");
        assert_eq!(items[under[0]].label, "_private");
    }

    #[test]
    fn lsp_items_win_ties_and_merge_dedups() {
        let mut lsp = CompletionItem::simple("print", CompletionKind::Function, "lsp");
        lsp.source = CompletionSource::Lsp;
        let merged = merge(
            vec![lsp],
            python_fallback("", "", false, &builtin_python_snippets()),
        );
        assert_eq!(merged.iter().filter(|i| i.label == "print").count(), 1);
        assert_eq!(merged[0].detail, "lsp");
        assert!(merged.iter().any(|i| i.is_snippet && i.label == "def"));
    }

    #[test]
    fn identifiers_are_collected_with_kinds() {
        let text = "import numpy\nclass Car:\n    def drive(self):\n        self.speed = MAX_SPEED  # not_me\n";
        let ids = identifiers_in(text, "drive", false);
        let get = |n: &str| ids.iter().find(|(x, _)| x == n).map(|(_, k)| *k);
        assert_eq!(get("numpy"), Some(CompletionKind::Module));
        assert_eq!(get("Car"), Some(CompletionKind::Class));
        assert_eq!(get("drive"), None, "skipped word");
        assert_eq!(get("MAX_SPEED"), Some(CompletionKind::Constant));
        assert_eq!(get("not_me"), None, "comments are ignored");
        assert_eq!(get("def"), None, "keywords are not identifiers");
        let dotted = identifiers_in(text, "", true);
        assert_eq!(dotted, vec![("speed".to_owned(), CompletionKind::Property)]);
    }

    #[test]
    fn index_matches_the_plain_fallback() {
        let text = "import numpy
class Car:
    def drive(self):
        self.speed = 1
total = 1
";
        let snippets = builtin_python_snippets();
        let mut index = PythonIndex::new(&snippets);
        let labels = |v: &[CompletionItem]| {
            let mut l: Vec<String> = v.iter().map(|i| i.label.clone()).collect();
            l.sort();
            l
        };
        let first = index.complete(7, 1, || text.into(), "", false);
        assert_eq!(
            labels(&first),
            labels(&python_fallback(text, "", false, &snippets))
        );
        let dotted = index.complete(7, 1, || unreachable!("cached"), "", true);
        assert_eq!(
            labels(&dotted),
            labels(&python_fallback(text, "", true, &snippets))
        );
        // Same file: the list is shared, the text not rescanned.
        let again = index.complete(7, 1, || unreachable!("cached"), "", false);
        assert!(Rc::ptr_eq(&first, &again));
        // A newer revision without a rescan: names near the caret are added.
        let newer = index.complete(7, 2, || unreachable!("cached"), "fresh_name = 3", false);
        assert!(newer.iter().any(|i| i.label == "fresh_name"));
        assert!(newer.iter().any(|i| i.label == "total"));
        assert!(!index.is_fresh(7, 2));
        // A background scan of revision 2 lands; a late revision-1 scan
        // does not overwrite it.
        index.set_identifiers(7, 2, IdentScan::of("renamed = 2"));
        index.set_identifiers(7, 1, IdentScan::of(text));
        assert!(index.is_fresh(7, 2));
        let fresh = index.complete(7, 2, || unreachable!("cached"), "", false);
        assert!(fresh.iter().any(|i| i.label == "renamed"));
        assert!(!fresh.iter().any(|i| i.label == "total"));
        // Another file is scanned on first use.
        let other = index.complete(8, 1, || "other_file_name = 1".into(), "", false);
        assert!(other.iter().any(|i| i.label == "other_file_name"));
    }

    #[test]
    fn ranking_a_subset_equals_ranking_all() {
        let items = python_fallback("print_all = 1\npriority = 2", "", false, &[]);
        let all = rank(&items, "pr");
        let narrowed = rank_among(&items, all.iter().copied(), "pri");
        assert_eq!(narrowed, rank(&items, "pri"));
    }

    #[test]
    fn fallback_always_has_keywords_builtins_and_snippets() {
        let items = python_fallback("total = 1", "", false, &builtin_python_snippets());
        for want in ["def", "print", "total", "main"] {
            assert!(items.iter().any(|i| i.label == want), "missing {want}");
        }
        let after_dot = python_fallback("x.append(1)", "", true, &[]);
        assert!(after_dot.iter().all(|i| i.label != "print"));
        assert!(after_dot.iter().any(|i| i.label == "append"));
        assert_eq!(word_fallback("hello world", "").len(), 2);
    }
}
