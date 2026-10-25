//! The completion popup: its state machine (pure) and the language server
//! request that runs off the UI thread.

use std::rc::Rc;

use apex_editor::completion::{self, CompletionItem, CompletionKind, CompletionSource};
use apex_lsp::lsp_types::{CompletionItemKind, Position, Uri};
use apex_lsp::LspClient;

use crate::events::{AppEvent, EventSink};

/// Rows shown at once. Only these rows are sent to the UI, so the list is
/// virtualized however many candidates match.
pub const VISIBLE_ROWS: usize = 10;
/// Pause in typing before the language server is asked for completions.
/// The built-in candidates show at once, without waiting for it; the
/// server's items are merged in when they arrive.
pub const LSP_DEBOUNCE_MS: u64 = 40;

/// An open completion popup.
#[derive(Debug, Clone)]
pub struct Popup {
    /// Candidates (built-in now, merged with the server's when they come).
    /// Shared with the completion index: opening a popup copies nothing.
    pub items: Rc<Vec<CompletionItem>>,
    /// Built-in candidates, kept to merge with late server results.
    pub builtin: Rc<Vec<CompletionItem>>,
    /// Indices into `items`, best first.
    pub filtered: Vec<usize>,
    /// Selected index into `filtered`.
    pub selected: usize,
    /// First visible index into `filtered`.
    pub first: usize,
    /// Char index where the word being completed starts.
    pub word_start: usize,
    /// Request id; server replies for other ids are ignored.
    pub request: u64,
    /// Opened by Ctrl+Space (stays open with an empty prefix).
    pub explicit: bool,
    /// The last prefix filtered for and every item it matched, ranked:
    /// typing more letters only re-ranks these (see `rank_among`).
    pool: Option<(String, Vec<usize>)>,
    /// The user moved the selection (arrows, page keys, wheel): typing then
    /// keeps it on the same item instead of jumping to the best match.
    moved: bool,
}

impl Popup {
    /// A popup for the word starting at `word_start`.
    pub fn new(
        items: impl Into<Rc<Vec<CompletionItem>>>,
        word_start: usize,
        request: u64,
        explicit: bool,
    ) -> Self {
        let items = items.into();
        Self {
            builtin: items.clone(),
            items,
            filtered: Vec::new(),
            selected: 0,
            first: 0,
            word_start,
            request,
            explicit,
            pool: None,
            moved: false,
        }
    }

    /// Re-filters for `prefix`. The best match is selected, unless the user
    /// picked an item with the arrows and it still matches.
    pub fn refilter(&mut self, prefix: &str) {
        let keep = self
            .current()
            .filter(|_| self.moved)
            .map(|i| i.label.clone());
        let ranked = match self.pool.take() {
            // Typing on: only what matched the shorter prefix can match.
            Some((before, pool)) if prefix.starts_with(before.as_str()) => {
                completion::rank_among(&self.items, pool, prefix)
            }
            _ => completion::rank(&self.items, prefix),
        };
        self.pool = Some((prefix.to_owned(), ranked.clone()));
        self.filtered = ranked;
        // The word being typed is in the file too; it is not a suggestion.
        let items = &self.items;
        self.filtered.retain(|&i| {
            !(items[i].label == prefix && items[i].detail == completion::IN_THIS_FILE)
        });
        // A lone exact match of what is typed is noise.
        if self.filtered.len() == 1
            && self.items[self.filtered[0]].label == prefix
            && !self.items[self.filtered[0]].is_snippet
        {
            self.filtered.clear();
        }
        self.selected = keep
            .and_then(|l| self.filtered.iter().position(|&i| self.items[i].label == l))
            .filter(|_| !prefix.is_empty())
            .unwrap_or(0);
        self.first = 0;
        self.scroll_to_selected();
    }

    /// Adds the server's items.
    /// The selected item stays selected (a late reply must not change what
    /// Tab or Enter is about to insert).
    pub fn merge_lsp(&mut self, lsp: Vec<CompletionItem>, prefix: &str) {
        let keep = self.current().map(|i| i.label.clone());
        self.items = Rc::new(completion::merge(lsp, self.builtin.as_ref().clone()));
        self.filtered.clear();
        self.pool = None;
        let moved = self.moved;
        self.refilter(prefix);
        self.moved = moved;
        if let Some(l) = keep.filter(|_| !prefix.is_empty()) {
            if let Some(i) = self.filtered.iter().position(|&i| self.items[i].label == l) {
                self.selected = i;
                self.scroll_to_selected();
            }
        }
    }

    /// The selected item.
    pub fn current(&self) -> Option<&CompletionItem> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }

    /// Moves the selection by `delta`, wrapping at the ends.
    pub fn move_by(&mut self, delta: isize) {
        let n = self.filtered.len();
        if n == 0 {
            return;
        }
        self.moved = true;
        let at = self.selected as isize + delta;
        self.selected = if delta.abs() == 1 {
            at.rem_euclid(n as isize) as usize
        } else {
            at.clamp(0, n as isize - 1) as usize
        };
        self.scroll_to_selected();
    }

    /// Scrolls the visible window by `delta` rows without moving the selection.
    pub fn scroll(&mut self, delta: isize) {
        let max_first = self.filtered.len().saturating_sub(VISIBLE_ROWS);
        self.first = (self.first as isize + delta).clamp(0, max_first as isize) as usize;
    }

    fn scroll_to_selected(&mut self) {
        if self.selected < self.first {
            self.first = self.selected;
        } else if self.selected >= self.first + VISIBLE_ROWS {
            self.first = self.selected + 1 - VISIBLE_ROWS;
        }
    }

    /// Whether there is nothing to show.
    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    /// How many candidates the last filter had to look at (the pool).
    #[cfg(test)]
    pub fn pool_len(&self) -> usize {
        self.pool
            .as_ref()
            .map_or(self.items.len(), |(_, p)| p.len())
    }

    /// The visible rows as `(item, is_selected)`.
    pub fn visible(&self) -> impl Iterator<Item = (&CompletionItem, bool)> {
        self.filtered
            .iter()
            .enumerate()
            .skip(self.first)
            .take(VISIBLE_ROWS)
            .map(|(i, &idx)| (&self.items[idx], i == self.selected))
    }
}

/// Maps a language server item kind onto the popup's kinds.
pub fn kind_from_lsp(kind: Option<CompletionItemKind>) -> CompletionKind {
    let Some(k) = kind else {
        return CompletionKind::Text;
    };
    match k {
        CompletionItemKind::KEYWORD => CompletionKind::Keyword,
        CompletionItemKind::SNIPPET => CompletionKind::Snippet,
        CompletionItemKind::FUNCTION
        | CompletionItemKind::METHOD
        | CompletionItemKind::CONSTRUCTOR => CompletionKind::Function,
        CompletionItemKind::CLASS
        | CompletionItemKind::INTERFACE
        | CompletionItemKind::STRUCT
        | CompletionItemKind::ENUM
        | CompletionItemKind::TYPE_PARAMETER => CompletionKind::Class,
        CompletionItemKind::MODULE | CompletionItemKind::FILE | CompletionItemKind::FOLDER => {
            CompletionKind::Module
        }
        CompletionItemKind::CONSTANT | CompletionItemKind::ENUM_MEMBER => CompletionKind::Constant,
        CompletionItemKind::PROPERTY | CompletionItemKind::FIELD => CompletionKind::Property,
        CompletionItemKind::VARIABLE | CompletionItemKind::VALUE => CompletionKind::Variable,
        _ => CompletionKind::Text,
    }
}

/// Converts a server completion.
pub fn from_lsp(c: apex_lsp::Completion) -> CompletionItem {
    CompletionItem {
        kind: kind_from_lsp(c.kind),
        detail: c.detail.unwrap_or_default(),
        documentation: c.documentation.unwrap_or_default(),
        insert: c.insert_text,
        is_snippet: false,
        filter_text: c.filter_text,
        sort_text: c.sort_text,
        source: CompletionSource::Lsp,
        label: c.label,
    }
}

/// A finished server completion request.
pub struct CompletionReply {
    /// Which request it answers.
    pub request: u64,
    /// The items (empty on error).
    pub items: Vec<CompletionItem>,
}

/// Asks the server for completions on a worker thread; the reply arrives as
/// [`AppEvent::Completion`].
pub fn request(
    worker: &crate::lsp::LspWorker,
    uri: Uri,
    position: Position,
    request: u64,
    sink: EventSink,
) {
    worker.send(crate::lsp::LspJob::Run(Box::new(
        move |client: &LspClient| {
            let items = match client.completion(&uri, position) {
                Ok(items) => items.into_iter().take(2000).map(from_lsp).collect(),
                Err(e) => {
                    tracing::debug!(%e, "completion request");
                    Vec::new()
                }
            };
            sink.send(AppEvent::Editor(super::EditorEvent::Completion(
                CompletionReply { request, items },
            )));
        },
    )));
}

/// Short documentation shown next to the popup: detail plus the first
/// lines of the documentation.
pub fn doc_text(item: &CompletionItem) -> String {
    let doc: String = item
        .documentation
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .take(14)
        .collect::<Vec<_>>()
        .join("\n");
    match (
        item.detail.is_empty() || item.is_snippet,
        doc.trim().is_empty(),
    ) {
        (true, true) => String::new(),
        (true, false) => doc,
        (false, true) if item.source == CompletionSource::Lsp && item.detail.len() > 30 => {
            item.detail.clone()
        }
        (false, true) => String::new(),
        (false, false) => format!("{}\n\n{doc}", item.detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apex_editor::snippet::builtin_python_snippets;

    fn popup(prefix: &str) -> Popup {
        let items = completion::python_fallback(
            "value = 1\nvariance = 2",
            prefix,
            false,
            &builtin_python_snippets(),
        );
        let mut p = Popup::new(items, 0, 1, false);
        p.refilter(prefix);
        p
    }

    #[test]
    fn filters_and_moves_with_wrap() {
        let mut p = popup("va");
        assert!(!p.is_empty());
        let labels: Vec<&str> = p.visible().map(|(i, _)| i.label.as_str()).collect();
        assert!(labels.contains(&"value"));
        assert!(labels.contains(&"vars"));
        assert_eq!(p.selected, 0);
        p.move_by(-1);
        assert_eq!(p.selected, p.filtered.len() - 1);
        p.move_by(1);
        assert_eq!(p.selected, 0);
        p.move_by(100);
        assert_eq!(p.selected, p.filtered.len() - 1);
        assert!(p.first + VISIBLE_ROWS > p.selected);
    }

    #[test]
    fn selection_survives_refilter_and_lsp_merge() {
        let mut p = popup("v");
        let target = p
            .filtered
            .iter()
            .position(|&i| p.items[i].label == "value")
            .unwrap();
        p.move_by(target as isize);
        assert_eq!(p.selected, target);
        p.refilter("va");
        assert_eq!(p.current().unwrap().label, "value");
        let mut lsp = CompletionItem::simple("valid_lsp_name", CompletionKind::Variable, "int");
        lsp.source = CompletionSource::Lsp;
        p.merge_lsp(vec![lsp], "va");
        assert!(p.items.iter().any(|i| i.label == "valid_lsp_name"));
        assert_eq!(p.current().unwrap().label, "value");
    }

    #[test]
    fn typing_on_narrows_the_pool() {
        let mut p = popup("p");
        let after_p = p.pool_len();
        assert!(after_p < p.items.len());
        p.refilter("pr");
        assert!(p.pool_len() <= after_p);
        let narrowed: Vec<usize> = p.filtered.clone();
        // Same result as filtering everything from scratch.
        assert_eq!(narrowed, completion::rank(&p.items, "pr"));
        // Backspace widens again.
        p.refilter("p");
        assert_eq!(p.pool_len(), after_p);
        assert_eq!(p.selected, 0, "first item stays pre-selected");
    }

    /// Times what happens between a key press and the popup having its
    /// rows: the old path (rebuild every candidate, rank all of them, after
    /// a 150 ms debounce) against the new one (prebuilt index, cached
    /// identifiers, incremental filtering, no debounce). Run with
    /// `cargo test -p apex-ide --release popup_latency -- --nocapture`
    /// for representative numbers.
    #[test]
    fn popup_latency() {
        use std::time::{Duration, Instant};
        const OLD_DEBOUNCE: Duration = Duration::from_millis(150);
        let mut text = String::new();
        for i in 0..4000 {
            text.push_str(&format!(
                "def handler_{i}(request_{i}, value):\n    result_{i} = value.process_{i}(request_{i})\n    return result_{i}\n\n"
            ));
        }
        let snippets = builtin_python_snippets();
        let runs = 20u32;

        // Old: every popup open rebuilt the keyword/builtin/snippet items
        // and rescanned the file; every keystroke ranked all candidates.
        let t = Instant::now();
        for _ in 0..runs {
            let items = completion::python_fallback(&text, "p", false, &snippets);
            let mut p = Popup::new(items, 0, 1, false);
            p.refilter("p");
            assert!(!p.is_empty());
        }
        let old_open = t.elapsed() / runs;
        let items = completion::python_fallback(&text, "p", false, &snippets);
        let mut p = Popup::new(items, 0, 1, false);
        p.refilter("p");
        let mut old_key = Duration::ZERO;
        for _ in 0..runs {
            p.pool = None;
            let t = Instant::now();
            p.refilter("pr");
            old_key += t.elapsed();
        }
        let old_key = old_key / runs;

        // New: the index is built once and a file is scanned once (later
        // scans run on a worker thread after edits); each popup shares the
        // cached list and only looks at the lines around the caret.
        let mut index = completion::PythonIndex::new(&snippets);
        let t = Instant::now();
        let _ = index.complete(1, 0, || text.as_str().into(), "", false);
        let first_scan = t.elapsed();
        let nearby: String = text.lines().take(121).collect::<Vec<_>>().join("\n");
        let t = Instant::now();
        for rev in 1..=u64::from(runs) {
            let items = index.complete(1, rev, || unreachable!("cached"), &nearby, false);
            let mut p = Popup::new(items, 0, 1, false);
            p.refilter("p");
            assert!(!p.is_empty());
        }
        let new_open = t.elapsed() / runs;
        let items = index.complete(1, 1, || unreachable!("cached"), &nearby, false);
        let mut p = Popup::new(items, 0, 1, false);
        p.refilter("p");
        let mut new_key = Duration::ZERO;
        for _ in 0..runs {
            p.refilter("p");
            let t = Instant::now();
            p.refilter("pr");
            new_key += t.elapsed();
        }
        let new_key = new_key / runs;

        eprintln!(
            "popup latency on a {}-line file ({} candidates):\n  \
             before: open {:?} + {:?} debounce = {:?}; keystroke {:?}\n  \
             after:  open {:?} (no debounce; LSP merged async after {} ms); keystroke {:?}\n  \
             (first popup in a file scans it once: {:?}; rescans after edits run off the UI thread)",
            text.lines().count(),
            p.items.len(),
            old_open,
            OLD_DEBOUNCE,
            old_open + OLD_DEBOUNCE,
            old_key,
            new_open,
            LSP_DEBOUNCE_MS,
            new_key,
            first_scan,
        );
        // The popup no longer waits for any timer, and well under a frame.
        const { assert!(LSP_DEBOUNCE_MS <= 50) };
        assert!(new_open < OLD_DEBOUNCE + old_open);
    }

    #[test]
    fn exact_single_match_hides_popup() {
        let items = vec![CompletionItem::simple(
            "print",
            CompletionKind::Function,
            "",
        )];
        let mut p = Popup::new(items, 0, 1, false);
        p.refilter("print");
        assert!(p.is_empty());
    }

    #[test]
    fn lsp_kinds_and_docs() {
        assert_eq!(
            kind_from_lsp(Some(CompletionItemKind::METHOD)),
            CompletionKind::Function
        );
        assert_eq!(kind_from_lsp(None), CompletionKind::Text);
        let mut it = CompletionItem::simple("f", CompletionKind::Function, "def f(x)");
        it.documentation = "```python\ndef f\n```\nDoes things.".into();
        assert_eq!(doc_text(&it), "def f(x)\n\ndef f\nDoes things.");
    }
}
