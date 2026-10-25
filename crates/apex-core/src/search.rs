//! Project search: a fuzzy matcher, a file-name index for "Go to File", and
//! a cancellable "Find in Files" content search. Everything here is plain
//! data and blocking I/O, meant to run on worker threads.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::file_tree::{FileNode, FileTree, FileTreeOptions};
use crate::Result;

/// Files larger than this are skipped by [`find_in_files`].
pub const MAX_SEARCH_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// A fuzzy match: a score (higher is better) and the matched character
/// positions (char indices into the candidate), for highlighting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    /// Ranking score; higher is better.
    pub score: i64,
    /// Char indices of the matched characters, ascending.
    pub positions: Vec<usize>,
}

fn is_word_start(chars: &[char], i: usize) -> bool {
    i == 0
        || matches!(chars[i - 1], ' ' | '.' | '_' | '-' | '/' | '\\' | ':')
        || (chars[i].is_uppercase() && chars[i - 1].is_lowercase())
}

/// Case-insensitive subsequence match of `query` (whitespace ignored) in
/// `text`. Prefers consecutive runs, word starts and early matches; uses a
/// greedy pass that re-anchors on word starts, which is fast and good
/// enough for file names and command titles.
pub fn fuzzy_match(query: &str, text: &str) -> Option<FuzzyMatch> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if query.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();

    // Try every start position of the first query char and keep the best
    // greedy match from there (n is small: names and titles).
    let mut best: Option<FuzzyMatch> = None;
    for start in (0..lower.len()).filter(|&i| lower[i] == query[0]) {
        let mut positions = Vec::with_capacity(query.len());
        positions.push(start);
        let mut qi = 1;
        let mut i = start + 1;
        while qi < query.len() && i < lower.len() {
            if lower[i] == query[qi] {
                // Prefer a later word-start occurrence over a mid-word one
                // when the previous match was not adjacent.
                let adjacent = positions.last() == Some(&(i - 1));
                if !adjacent && !is_word_start(&chars, i) {
                    if let Some(ws) = (i + 1..lower.len())
                        .find(|&j| lower[j] == query[qi] && is_word_start(&chars, j))
                    {
                        let remaining = query.len() - qi;
                        if lower.len() - ws >= remaining {
                            i = ws;
                        }
                    }
                }
                positions.push(i);
                qi += 1;
            }
            i += 1;
        }
        if qi < query.len() {
            continue;
        }
        let mut score = 0i64;
        for (k, &p) in positions.iter().enumerate() {
            score += 10;
            if k > 0 && positions[k - 1] + 1 == p {
                score += 20;
            }
            if is_word_start(&chars, p) {
                score += 15;
            }
        }
        let len = i64::try_from(chars.len()).unwrap_or(i64::MAX / 4);
        score -= i64::try_from(start).unwrap_or(0) + len / 4;
        if best.as_ref().is_none_or(|b| score > b.score) {
            best = Some(FuzzyMatch { score, positions });
        }
    }
    best
}

/// One file in a [`FileIndex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    /// Path relative to the project root, with `/` separators.
    pub rel: String,
    /// Byte offset of the file name inside `rel`.
    pub name_start: usize,
}

impl IndexEntry {
    /// The file name part of `rel`.
    pub fn name(&self) -> &str {
        &self.rel[self.name_start..]
    }
}

/// All files of a project (respecting the same ignore rules as the tree),
/// for fast fuzzy "Go to File".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileIndex {
    /// Project root.
    pub root: PathBuf,
    /// Files, sorted by relative path.
    pub entries: Vec<IndexEntry>,
}

/// A "Go to File" result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHit {
    /// Index into [`FileIndex::entries`].
    pub entry: usize,
    /// Ranking score.
    pub score: i64,
    /// Matched char positions inside the file *name*.
    pub name_positions: Vec<usize>,
}

impl FileIndex {
    /// Walks `root` (blocking) and indexes every file.
    pub fn build(root: &Path) -> Result<Self> {
        let tree = FileTree::build_with(root, &FileTreeOptions::default())?;
        Ok(Self::from_tree(&tree))
    }

    /// Indexes the files of an already built (fully loaded) tree.
    pub fn from_tree(tree: &FileTree) -> Self {
        let root = tree.root.path.clone();
        let mut entries = Vec::new();
        collect(&tree.root, &root, &mut entries);
        entries.sort_by(|a, b| a.rel.cmp(&b.rel));
        Self { root, entries }
    }

    /// Number of indexed files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Absolute path of entry `i`.
    pub fn path(&self, i: usize) -> Option<PathBuf> {
        self.entries.get(i).map(|e| self.root.join(&e.rel))
    }

    /// The best `limit` matches for `query`, best first. File-name matches
    /// rank above matches that need the folder part. An empty query lists
    /// the first `limit` files.
    pub fn search(&self, query: &str, limit: usize) -> Vec<FileHit> {
        let q = query.trim();
        if q.is_empty() {
            return (0..self.entries.len().min(limit))
                .map(|entry| FileHit {
                    entry,
                    score: 0,
                    name_positions: Vec::new(),
                })
                .collect();
        }
        // A query with a separator matches against the whole path.
        let path_query = q.contains('/') || q.contains('\\');
        let q_norm = q.replace('\\', "/");
        let mut hits: Vec<FileHit> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(entry, e)| {
                if !path_query {
                    if let Some(m) = fuzzy_match(&q_norm, e.name()) {
                        return Some(FileHit {
                            entry,
                            score: m.score + 1000,
                            name_positions: m.positions,
                        });
                    }
                }
                let m = fuzzy_match(&q_norm, &e.rel)?;
                let name_chars = e.rel[..e.name_start].chars().count();
                Some(FileHit {
                    entry,
                    score: m.score,
                    name_positions: m
                        .positions
                        .iter()
                        .filter(|&&p| p >= name_chars)
                        .map(|p| p - name_chars)
                        .collect(),
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| {
                    self.entries[a.entry]
                        .rel
                        .len()
                        .cmp(&self.entries[b.entry].rel.len())
                })
                .then_with(|| a.entry.cmp(&b.entry))
        });
        hits.truncate(limit);
        hits
    }
}

fn collect(node: &FileNode, root: &Path, out: &mut Vec<IndexEntry>) {
    for c in &node.children {
        if c.is_dir {
            collect(c, root, out);
        } else if let Ok(rel) = c.path.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            let name_start = rel.rfind('/').map_or(0, |i| i + 1);
            out.push(IndexEntry { rel, name_start });
        }
    }
}

/// Options for [`find_in_files`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindOptions {
    /// Match case exactly.
    pub case_sensitive: bool,
    /// Stop after this many matching lines in total.
    pub max_results: usize,
}

impl Default for FindOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            max_results: 2000,
        }
    }
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    /// Zero-based line number.
    pub line: usize,
    /// Zero-based char column of the match.
    pub col: usize,
    /// The line, trimmed and shortened for display.
    pub preview: String,
}

/// The matches in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    /// Path relative to the project root.
    pub rel: String,
    /// Matching lines, in order.
    pub hits: Vec<LineHit>,
}

/// Result of [`find_in_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindResult {
    /// Files with at least one match, in index order.
    pub files: Vec<FileMatches>,
    /// Total matching lines.
    pub total: usize,
    /// Whether the search stopped at `max_results`.
    pub truncated: bool,
    /// Whether it was cancelled.
    pub cancelled: bool,
}

const PREVIEW_CHARS: usize = 160;

fn preview(line: &str, col: usize) -> String {
    let trimmed_start = line.len() - line.trim_start().len();
    let text = line.trim();
    let col = col.saturating_sub(line[..trimmed_start].chars().count());
    let count = text.chars().count();
    if count <= PREVIEW_CHARS {
        return text.to_owned();
    }
    // Keep the match visible in long lines.
    let start = col.saturating_sub(PREVIEW_CHARS / 3);
    let s: String = text.chars().skip(start).take(PREVIEW_CHARS).collect();
    format!("{}{s}…", if start > 0 { "…" } else { "" })
}

/// Searches the contents of every indexed file for `needle` (blocking).
/// Binary files (containing NUL) and files over [`MAX_SEARCH_FILE_BYTES`]
/// are skipped. Checks `cancel` between files.
pub fn find_in_files(
    index: &FileIndex,
    needle: &str,
    options: &FindOptions,
    cancel: &AtomicBool,
) -> FindResult {
    let mut result = FindResult::default();
    if needle.is_empty() {
        return result;
    }
    let needle_cmp = if options.case_sensitive {
        needle.to_owned()
    } else {
        needle.to_lowercase()
    };
    let mut buf = Vec::new();
    for entry in &index.entries {
        if cancel.load(Ordering::Relaxed) {
            result.cancelled = true;
            break;
        }
        let path = index.root.join(&entry.rel);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if meta.len() > MAX_SEARCH_FILE_BYTES {
            continue;
        }
        buf.clear();
        if std::fs::File::open(&path)
            .and_then(|mut f| f.read_to_end(&mut buf))
            .is_err()
        {
            continue;
        }
        if buf.contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let mut hits = Vec::new();
        for (line_no, line) in text.lines().enumerate() {
            let hay = if options.case_sensitive {
                std::borrow::Cow::Borrowed(line)
            } else {
                std::borrow::Cow::Owned(line.to_lowercase())
            };
            if let Some(byte) = hay.find(&needle_cmp) {
                // Lower-casing can change byte lengths; map via chars.
                let col = hay[..byte].chars().count();
                hits.push(LineHit {
                    line: line_no,
                    col,
                    preview: preview(line, col),
                });
                result.total += 1;
                if result.total >= options.max_results {
                    result.truncated = true;
                    break;
                }
            }
        }
        if !hits.is_empty() {
            result.files.push(FileMatches {
                rel: entry.rel.clone(),
                hits,
            });
        }
        if result.truncated {
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for (rel, text) in [
            ("main.py", "import os\nprint('Hello World')\n"),
            ("app/models.py", "class User:\n    name = 'hello'\n"),
            ("app/views.py", "def index():\n    return 'HELLO'\n"),
            ("README.md", "# Demo\n"),
            ("__pycache__/x.pyc", "hello"),
        ] {
            let p = r.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        fs::write(r.join("blob.bin"), b"hello\0world").unwrap();
        dir
    }

    #[test]
    fn fuzzy_match_reports_positions_and_ranks() {
        let m = fuzzy_match("mp", "main.py").unwrap();
        assert_eq!(m.positions, [0, 5]);
        assert!(fuzzy_match("zz", "main.py").is_none());
        assert_eq!(fuzzy_match("", "x").unwrap().positions, Vec::<usize>::new());
        // Word starts win over earlier mid-word letters.
        let m = fuzzy_match("us", "app/user_service.py").unwrap();
        assert_eq!(m.positions, [4, 5]);
        let m = fuzzy_match("ufs", "user_file_store").unwrap();
        assert_eq!(m.positions, [0, 5, 10]);
        let exact = fuzzy_match("views", "views.py").unwrap().score;
        let scattered = fuzzy_match("views", "a_v_i_e_w_s.py").unwrap().score;
        assert!(exact > scattered);
    }

    #[test]
    fn index_skips_ignored_folders_and_searches_names_first() {
        let dir = project();
        let index = FileIndex::build(dir.path()).unwrap();
        let rels: Vec<&str> = index.entries.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(
            rels,
            [
                "README.md",
                "app/models.py",
                "app/views.py",
                "blob.bin",
                "main.py"
            ]
        );
        assert_eq!(index.entries[1].name(), "models.py");

        let hits = index.search("mod", 10);
        assert_eq!(index.entries[hits[0].entry].rel, "app/models.py");
        assert_eq!(hits[0].name_positions, [0, 1, 2]);
        // Folder-qualified query matches the path.
        let hits = index.search("app/vi", 10);
        assert_eq!(index.entries[hits[0].entry].rel, "app/views.py");
        assert_eq!(hits[0].name_positions, [0, 1]);
        assert_eq!(index.search("", 2).len(), 2);
        assert!(index.search("qqq", 10).is_empty());
        assert!(index.path(0).unwrap().ends_with("README.md"));
    }

    #[test]
    fn find_in_files_matches_lines_and_skips_binaries() {
        let dir = project();
        let index = FileIndex::build(dir.path()).unwrap();
        let cancel = AtomicBool::new(false);
        let r = find_in_files(&index, "hello", &FindOptions::default(), &cancel);
        let files: Vec<&str> = r.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(files, ["app/models.py", "app/views.py", "main.py"]);
        assert_eq!(r.total, 3);
        let main = &r.files[2].hits[0];
        assert_eq!((main.line, main.col), (1, 7));
        assert_eq!(main.preview, "print('Hello World')");

        let cs = FindOptions {
            case_sensitive: true,
            ..FindOptions::default()
        };
        let r = find_in_files(&index, "HELLO", &cs, &cancel);
        assert_eq!(r.total, 1);

        let capped = FindOptions {
            max_results: 2,
            ..FindOptions::default()
        };
        let r = find_in_files(&index, "hello", &capped, &cancel);
        assert!(r.truncated);
        assert_eq!(r.total, 2);

        cancel.store(true, Ordering::Relaxed);
        let r = find_in_files(&index, "hello", &FindOptions::default(), &cancel);
        assert!(r.cancelled && r.files.is_empty());
        assert!(
            find_in_files(&index, "", &FindOptions::default(), &AtomicBool::new(false))
                .files
                .is_empty()
        );
    }

    #[test]
    fn previews_keep_long_line_matches_visible() {
        let line = format!("{}needle{}", "a".repeat(400), "b".repeat(400));
        let p = preview(&line, 400);
        assert!(p.contains("needle"));
        assert!(p.starts_with('…') && p.ends_with('…'));
        assert_eq!(preview("    x = 1", 4), "x = 1");
    }
}
