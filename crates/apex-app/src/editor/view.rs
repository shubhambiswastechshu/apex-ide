//! Pure view logic of the code editor: the per-line highlight cache, folding
//! (display rows versus buffer lines), scrolling math, and the shapes the
//! Slint view paints (selection spans, squiggles, the minimap). Nothing here
//! touches Slint objects, so it is unit tested headless.

// Line lists are passed as slices of ranges; a one-range slice is intended.
#![allow(clippy::single_range_in_vec_init)]

use std::collections::BTreeSet;
use std::ops::Range;

use apex_editor::cursor::{expand_tabs, visual_col, Selection};
use apex_editor::outline::{self, FoldRange};
use apex_editor::{Buffer, Highlighter, LineEdit, TokenKind};

/// One coloured piece of a line, ready to paint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunData {
    /// Text with tabs expanded.
    pub text: String,
    /// Visual column of its first char.
    pub col: usize,
    /// Token kind.
    pub kind: TokenKind,
}

/// Highlighted runs of the lines the view painted.
///
/// Only lines the view needs (rows on screen, the minimap) are highlighted,
/// with [`Highlighter::highlight_rope_range`]: the buffer's rope is parsed
/// in place and incrementally (the buffer's edit log is fed to the
/// highlighter), and queries run only over the requested lines, so typing
/// stays fast in 10k-line files and no copy of the text is kept.
///
/// An edit makes every line *stale* (an opened string can recolour the rest
/// of the file): stale runs are still good enough for the minimap, while
/// the rows on screen are re-highlighted on the next render.
#[derive(Debug, Default)]
pub struct LineCache {
    lines: Vec<Option<Vec<RunData>>>,
    fresh: Vec<bool>,
    /// Buffer revision the line structure matches.
    pub revision: u64,
    /// Buffer revision the highlighter's parse tree matches (`None` before
    /// the first parse).
    parsed: Option<u64>,
}

/// Splits highlighted runs of one line into paintable runs (tabs expanded,
/// whitespace-only runs dropped, positions in visual columns).
pub fn line_runs(runs: &[(String, TokenKind)]) -> Vec<RunData> {
    let mut out = Vec::with_capacity(runs.len());
    let mut line = String::new();
    for (text, kind) in runs {
        let col = visual_col(&line, usize::MAX);
        line.push_str(text);
        let text = text.trim_end_matches('\r');
        if text.chars().all(|c| c == ' ' || c == '\t') {
            continue;
        }
        // Expand tabs relative to the run's own start column.
        let expanded = if text.contains('\t') {
            let pad = " ".repeat(col % apex_editor::cursor::TAB_WIDTH);
            expand_tabs(&format!("{pad}{text}"))[pad.len()..].to_owned()
        } else {
            text.to_owned()
        };
        out.push(RunData {
            text: expanded,
            col,
            kind: *kind,
        });
    }
    out
}

/// Plain runs for a line that is not highlighted (yet).
pub fn plain_runs(line: &str) -> Vec<RunData> {
    line_runs(&[(line.to_owned(), TokenKind::Plain)])
}

impl LineCache {
    /// Runs of `line` if highlighted for the current revision.
    pub fn get(&self, line: usize) -> Option<&[RunData]> {
        if !self.fresh.get(line).copied().unwrap_or(false) {
            return None;
        }
        self.lines.get(line)?.as_deref()
    }

    /// Marks every line for re-highlighting (after a deferred full parse).
    pub fn invalidate(&mut self) {
        self.fresh.iter_mut().for_each(|f| *f = false);
    }

    /// Runs of `line`, possibly from an older revision.
    pub fn get_any(&self, line: usize) -> Option<&[RunData]> {
        self.lines.get(line)?.as_deref()
    }

    /// Moves the line structure to the buffer's revision: edited lines are
    /// dropped, all others become stale.
    fn follow(&mut self, buffer: &Buffer) {
        let n = buffer.len_lines();
        if self.revision == buffer.revision() && self.lines.len() == n {
            return;
        }
        match buffer.line_edits_since(self.revision) {
            Some(edits) => {
                for e in &edits {
                    splice(&mut self.lines, e);
                }
            }
            None => self.lines.clear(),
        }
        if self.lines.len() != n {
            self.lines = vec![None; n];
        }
        self.fresh = vec![false; n];
        self.revision = buffer.revision();
    }

    /// Makes sure every line in `ranges` is highlighted for the buffer's
    /// current revision.
    pub fn ensure(
        &mut self,
        buffer: &Buffer,
        highlighter: &mut Highlighter,
        ranges: &[std::ops::Range<usize>],
    ) {
        self.follow(buffer);
        let n = self.lines.len();
        let mut synced = false;
        for r in ranges {
            let mut line = r.start.min(n);
            let end = r.end.min(n);
            while line < end {
                if self.fresh[line] {
                    line += 1;
                    continue;
                }
                let start = line;
                while line < end && !self.fresh[line] {
                    line += 1;
                }
                if !synced {
                    // Feed the edits since the last parse to the tree.
                    match self.parsed.map(|rev| buffer.line_edits_since(rev)) {
                        Some(Some(edits)) => edits.iter().for_each(|e| highlighter.edit(e)),
                        _ => highlighter.reset(),
                    }
                    self.parsed = Some(buffer.revision());
                    synced = true;
                }
                let runs = highlighter.highlight_rope_range(buffer.rope(), start..line);
                for (i, r) in runs.iter().enumerate() {
                    self.lines[start + i] = Some(line_runs(r));
                    self.fresh[start + i] = true;
                }
            }
        }
    }
}

/// Applies one line edit to per-line data: the touched lines are replaced
/// by empty entries.
fn splice<T: Clone>(lines: &mut Vec<Option<T>>, e: &LineEdit) {
    let start = e.start_line.min(lines.len());
    let end = (e.start_line + e.removed_breaks + 1).min(lines.len());
    let fresh = std::iter::repeat_n(None, e.inserted_breaks + 1);
    lines.splice(start..end, fresh);
}

/// Merges sorted line numbers into contiguous ranges.
pub fn line_ranges(lines: impl IntoIterator<Item = usize>) -> Vec<std::ops::Range<usize>> {
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    for l in lines {
        match out.last_mut() {
            Some(r) if r.end == l => r.end = l + 1,
            _ => out.push(l..l + 1),
        }
    }
    out
}

/// Shifts line numbers (fold starts, ...) for one edit. Lines inside the
/// edited region are dropped.
pub fn shift_lines(lines: &BTreeSet<usize>, e: &LineEdit) -> BTreeSet<usize> {
    lines
        .iter()
        .filter_map(|&l| {
            if l < e.start_line {
                Some(l)
            } else if l == e.start_line {
                // Text inserted at the very start of the line pushes it
                // down; other edits of the line keep it (unless lines were
                // removed).
                if e.start_col == 0 && e.removed_breaks == 0 {
                    Some(l + e.inserted_breaks)
                } else {
                    (e.removed_breaks == 0).then_some(l)
                }
            } else if l <= e.start_line + e.removed_breaks {
                None
            } else {
                Some(l + e.inserted_breaks - e.removed_breaks)
            }
        })
        .collect()
}

/// Indentation-based structure of a buffer, cached per revision.
#[derive(Debug, Default)]
pub struct Outline {
    /// Revision this was computed for (`u64::MAX` = never).
    pub revision: Option<u64>,
    /// Fold ranges.
    pub folds: Vec<FoldRange>,
    /// Effective indentation per line (visual columns).
    pub indents: Vec<usize>,
    /// Classes and functions with nesting depth.
    pub defs: Vec<(outline::Scope, usize)>,
    /// The file indents with tabs.
    pub uses_tabs: bool,
    /// Widest line in visual columns.
    pub max_cols: usize,
}

impl Outline {
    /// Recomputes if `buffer` changed.
    pub fn update(&mut self, buffer: &Buffer) {
        if self.revision == Some(buffer.revision()) {
            return;
        }
        let text = buffer.text();
        let lines: Vec<&str> = text.split('\n').collect();
        self.folds = outline::fold_ranges(&lines);
        self.indents = outline::effective_indents(&lines);
        self.defs = outline::definitions(&lines);
        self.uses_tabs = lines.iter().any(|l| l.starts_with('\t'));
        self.max_cols = lines
            .iter()
            .map(|l| apex_editor::cursor::display_width(l))
            .max()
            .unwrap_or(0);
        self.revision = Some(buffer.revision());
    }

    /// The fold that starts at `line`.
    pub fn fold_at(&self, line: usize) -> Option<FoldRange> {
        self.folds
            .binary_search_by_key(&line, |f| f.start)
            .ok()
            .map(|i| self.folds[i])
    }

    /// Innermost enclosing class and function names at `line`.
    pub fn scope_names(&self, line: usize) -> (String, String) {
        let mut class = String::new();
        let mut func = String::new();
        let line_indent = self.indents.get(line).copied().unwrap_or(0);
        // Walk definitions above `line` whose block contains it.
        let mut limit = line_indent + 1;
        for (scope, depth) in self.defs.iter().rev().filter(|(s, _)| s.line <= line) {
            let indent = depth * apex_editor::cursor::TAB_WIDTH;
            let inside =
                scope.line == line || self.fold_at(scope.line).is_some_and(|f| f.end >= line);
            if indent < limit && inside {
                if scope.kind == "class" && class.is_empty() {
                    class = scope.name.clone();
                } else if scope.kind == "def" && func.is_empty() && class.is_empty() {
                    func = scope.name.clone();
                }
                limit = indent;
            }
        }
        (class, func)
    }
}

/// Which lines are hidden by folds, and the mapping between display rows
/// and buffer lines.
#[derive(Debug, Default, Clone)]
pub struct FoldMap {
    /// Buffer line of each display row; empty means "identity".
    row_lines: Vec<usize>,
    line_count: usize,
}

impl FoldMap {
    /// Builds the map for `line_count` lines with `folded` ranges collapsed.
    pub fn new(line_count: usize, folded: &[FoldRange]) -> Self {
        if folded.is_empty() {
            return Self {
                row_lines: Vec::new(),
                line_count,
            };
        }
        let mut hidden = vec![false; line_count];
        for f in folded {
            for h in hidden
                .iter_mut()
                .take((f.end + 1).min(line_count))
                .skip(f.start + 1)
            {
                *h = true;
            }
        }
        Self {
            row_lines: (0..line_count).filter(|l| !hidden[*l]).collect(),
            line_count,
        }
    }

    /// Number of display rows.
    pub fn rows(&self) -> usize {
        if self.row_lines.is_empty() {
            self.line_count
        } else {
            self.row_lines.len()
        }
    }

    /// Buffer line shown on `row` (clamped).
    pub fn line_of_row(&self, row: usize) -> usize {
        if self.row_lines.is_empty() {
            return row.min(self.line_count.saturating_sub(1));
        }
        self.row_lines[row.min(self.row_lines.len() - 1)]
    }

    /// Display row of `line`; a hidden line maps to the row of its fold
    /// header.
    pub fn row_of_line(&self, line: usize) -> usize {
        if self.row_lines.is_empty() {
            return line;
        }
        match self.row_lines.binary_search(&line) {
            Ok(r) => r,
            Err(r) => r.saturating_sub(1),
        }
    }

    /// Whether `line` is hidden inside a fold.
    pub fn is_hidden(&self, line: usize) -> bool {
        !self.row_lines.is_empty() && self.row_lines.binary_search(&line).is_err()
    }
}

/// The scroll position and size of an editor's code area, in logical px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Horizontal scroll.
    pub sx: f32,
    /// Vertical scroll.
    pub sy: f32,
    /// Code area width.
    pub width: f32,
    /// Code area height.
    pub height: f32,
    /// Width of one char.
    pub char_width: f32,
    /// Height of one row.
    pub line_height: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            sx: 0.0,
            sy: 0.0,
            width: 800.0,
            height: 600.0,
            char_width: 8.0,
            line_height: 19.0,
        }
    }
}

/// Left padding of the code area (matches `pad` in editor.slint).
pub const PAD: f32 = 4.0;

impl Viewport {
    /// Rows that fit on screen.
    pub fn page_rows(&self) -> usize {
        ((self.height / self.line_height.max(1.0)) as usize).max(1)
    }

    /// First row (partly) visible.
    pub fn first_row(&self) -> usize {
        (self.sy / self.line_height.max(1.0)).floor().max(0.0) as usize
    }

    /// The scroll position that makes `row`/`col` visible with a margin, or
    /// `None` if it already is.
    pub fn reveal(&self, row: usize, col: usize) -> Option<(f32, f32)> {
        let lh = self.line_height.max(1.0);
        let y = row as f32 * lh;
        let mut sy = self.sy;
        if y < sy {
            sy = y;
        } else if y + lh > sy + self.height {
            sy = (y + lh - self.height).max(0.0);
        }
        let x = PAD + col as f32 * self.char_width;
        let mut sx = self.sx;
        let margin = 4.0 * self.char_width;
        if x - margin < sx {
            sx = (x - margin * 4.0).max(0.0);
        } else if x + margin > sx + self.width {
            sx = x + margin * 4.0 - self.width;
        }
        ((sx - self.sx).abs() > 0.5 || (sy - self.sy).abs() > 0.5).then_some((sx, sy))
    }

    /// The scroll position that puts `row` near the vertical centre.
    pub fn center_on(&self, row: usize) -> (f32, f32) {
        let y = row as f32 * self.line_height - self.height / 3.0;
        (0.0, y.max(0.0))
    }
}

/// A painted rectangle on one row, in visual columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Display row.
    pub row: usize,
    /// First visual column.
    pub col: usize,
    /// Width in columns.
    pub cols: usize,
    /// See `EdSpan.kind` in editor.slint.
    pub kind: i32,
}

/// Selection rectangles for the rows in `rows` (display rows). A selection
/// that continues past a line end shows one extra column.
pub fn selection_spans(
    buffer: &Buffer,
    map: &FoldMap,
    sel: Selection,
    rows: Range<usize>,
) -> Vec<Span> {
    let mut out = Vec::new();
    if sel.is_empty() {
        return out;
    }
    let range = sel.range();
    let rope = buffer.rope();
    let first_line = rope.char_to_line(range.start.min(rope.len_chars()));
    let last_line = rope.char_to_line(range.end.min(rope.len_chars()));
    let first_row = map.row_of_line(first_line).max(rows.start);
    let last_row = map.row_of_line(last_line).min(rows.end.saturating_sub(1));
    for row in first_row..=last_row {
        if row < rows.start || row >= rows.end {
            continue;
        }
        let line = map.line_of_row(row);
        let start = rope.line_to_char(line);
        let text = buffer.line(line).unwrap_or_default();
        let len = text.chars().count();
        let a = range.start.saturating_sub(start).min(len);
        let b = if range.end > start + len {
            usize::MAX
        } else {
            range.end.saturating_sub(start)
        };
        if range.end <= start && line != first_line {
            continue;
        }
        let ca = visual_col(&text, a);
        let cb = if b == usize::MAX {
            visual_col(&text, len) + 1
        } else {
            visual_col(&text, b)
        };
        if cb > ca {
            out.push(Span {
                row,
                col: ca,
                cols: cb - ca,
                kind: 0,
            });
        }
    }
    out
}

/// SVG path of a wavy underline `cols` columns wide, in a viewbox of
/// `cols * 4` by 3 units.
pub fn wave_path(cols: usize) -> String {
    let mut s = String::from("M 0 2");
    let steps = cols.max(1) * 2;
    for i in 1..=steps {
        let y = if i % 2 == 1 { 0 } else { 2 };
        s.push_str(&format!(" L {} {}", i * 2, y));
    }
    s
}

/// Pixel width of the minimap image.
pub const MINIMAP_WIDTH: usize = 88;

/// Paints rows `first..first + count` of the file into an RGBA image: each
/// row is 2 px tall, each char 1 px wide, coloured by `color` (which maps a
/// token kind to RGBA).
pub fn paint_minimap(
    lines: &[Vec<RunData>],
    color: impl Fn(TokenKind) -> [u8; 4],
) -> (usize, usize, Vec<u8>) {
    let w = MINIMAP_WIDTH;
    let h = (lines.len() * 2).max(1);
    let mut px = vec![0u8; w * h * 4];
    for (r, runs) in lines.iter().enumerate() {
        let y = r * 2;
        for run in runs {
            let c = color(run.kind);
            for (i, ch) in run.text.chars().enumerate() {
                let x = run.col + i;
                if x >= w {
                    break;
                }
                if ch == ' ' {
                    continue;
                }
                let o = (y * w + x) * 4;
                px[o..o + 4].copy_from_slice(&c);
            }
        }
    }
    (w, h, px)
}

/// First row shown in a minimap of `map_rows` rows for a file of `rows`
/// rows scrolled to `first_visible` with `page` rows on screen (the
/// minimap scrolls proportionally, as in VS Code).
pub fn minimap_first(rows: usize, map_rows: usize, first_visible: usize, page: usize) -> usize {
    if rows <= map_rows {
        return 0;
    }
    let scrollable = rows.saturating_sub(page).max(1);
    let frac = (first_visible as f64 / scrollable as f64).min(1.0);
    (frac * (rows - map_rows) as f64).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_expand_tabs_and_skip_blank_pieces() {
        let runs = vec![
            ("\t".to_owned(), TokenKind::Plain),
            ("return".to_owned(), TokenKind::Keyword),
            (" ".to_owned(), TokenKind::Plain),
            ("'a\tb'".to_owned(), TokenKind::String),
        ];
        let data = line_runs(&runs);
        assert_eq!(data.len(), 2);
        assert_eq!(data[0].col, 4);
        assert_eq!(data[1].col, 11);
        assert_eq!(data[1].text, "'a   b'");
    }

    #[test]
    fn cache_highlights_requested_lines_per_revision() {
        let mut b = Buffer::from_text("a = 1\nb = 2\nc = 3");
        b.set_path(Some("x.py".into()));
        let mut h = Highlighter::python().unwrap();
        let mut cache = LineCache::default();
        cache.ensure(&b, &mut h, &[1..2]);
        assert!(cache.get(0).is_none());
        assert_eq!(cache.get(1).unwrap()[0].text, "b");
        b.insert(6, "x\ny\n").unwrap();
        cache.ensure(&b, &mut h, &line_ranges([0, 1, 4]));
        assert!(cache.get(1).is_some());
        assert!(cache.get(2).is_none());
        assert!(cache.get_any(2).is_none(), "edited line has no stale runs");
        assert_eq!(cache.get(4).unwrap()[0].text, "c");
        assert_eq!(line_ranges([1, 2, 3, 7, 8]), vec![1..4, 7..9]);
    }

    #[test]
    fn fold_map_hides_and_maps_rows() {
        let map = FoldMap::new(10, &[FoldRange { start: 2, end: 5 }]);
        assert_eq!(map.rows(), 7);
        assert_eq!(map.line_of_row(3), 6);
        assert_eq!(map.row_of_line(6), 3);
        assert_eq!(map.row_of_line(4), 2, "hidden line maps to its header");
        assert!(map.is_hidden(4));
        assert!(!map.is_hidden(2));
        let id = FoldMap::new(4, &[]);
        assert_eq!((id.rows(), id.line_of_row(9), id.row_of_line(2)), (4, 3, 2));
    }

    #[test]
    fn fold_starts_shift_with_edits() {
        let set: BTreeSet<usize> = [2, 5, 9].into_iter().collect();
        let e = LineEdit {
            start_line: 3,
            removed_breaks: 0,
            inserted_breaks: 2,
            start: 0,
            start_col: 1,
            removed_chars: 0,
            inserted_chars: 2,
            ..LineEdit::default()
        };
        let shifted: Vec<usize> = shift_lines(&set, &e).into_iter().collect();
        assert_eq!(shifted, vec![2, 7, 11]);
        let e = LineEdit {
            start_line: 4,
            removed_breaks: 2,
            inserted_breaks: 0,
            start: 0,
            start_col: 1,
            removed_chars: 2,
            inserted_chars: 0,
            ..LineEdit::default()
        };
        let shifted: Vec<usize> = shift_lines(&set, &e).into_iter().collect();
        assert_eq!(shifted, vec![2, 7]);
    }

    #[test]
    fn reveal_scrolls_only_when_needed() {
        let v = Viewport {
            sx: 0.0,
            sy: 0.0,
            width: 400.0,
            height: 100.0,
            char_width: 10.0,
            line_height: 20.0,
        };
        assert_eq!(v.page_rows(), 5);
        assert!(v.reveal(2, 3).is_none());
        assert_eq!(v.reveal(10, 0), Some((0.0, 120.0)));
        let (sx, _) = v.reveal(0, 60).unwrap();
        assert!(sx > 200.0);
        let below = Viewport { sy: 200.0, ..v };
        assert_eq!(below.reveal(1, 0), Some((0.0, 20.0)));
        assert_eq!(below.first_row(), 10);
    }

    #[test]
    fn selection_spans_cover_rows() {
        let b = Buffer::from_text("ab\n\tcd\nef");
        let map = FoldMap::new(3, &[]);
        let spans = selection_spans(&b, &map, Selection::new(1, 5), 0..3);
        assert_eq!(
            spans,
            vec![
                Span {
                    row: 0,
                    col: 1,
                    cols: 2,
                    kind: 0
                },
                Span {
                    row: 1,
                    col: 0,
                    cols: 5,
                    kind: 0
                },
            ]
        );
        assert!(selection_spans(&b, &map, Selection::caret(1), 0..3).is_empty());
        // Only rows in view are produced.
        assert_eq!(
            selection_spans(&b, &map, Selection::new(0, 9), 2..3).len(),
            1
        );
    }

    #[test]
    fn minimap_paints_and_scrolls() {
        let lines = vec![plain_runs("ab c"), Vec::new()];
        let (w, h, px) = paint_minimap(&lines, |_| [1, 2, 3, 255]);
        assert_eq!((w, h), (MINIMAP_WIDTH, 4));
        assert_eq!(&px[0..4], &[1, 2, 3, 255]);
        assert_eq!(px[2 * 4 + 3], 0, "space stays transparent");
        assert_eq!(minimap_first(100, 200, 50, 20), 0);
        assert_eq!(minimap_first(1000, 200, 0, 50), 0);
        assert_eq!(minimap_first(1000, 200, 950, 50), 800);
        assert!(wave_path(2).starts_with("M 0 2 L 2 0"));
    }

    #[test]
    fn outline_scope_names() {
        let mut b =
            Buffer::from_text("class A:\n    def f(self):\n        x = 1\n\ndef g():\n    pass\n");
        b.set_path(Some("x.py".into()));
        let mut o = Outline::default();
        o.update(&b);
        assert_eq!(o.scope_names(2), ("A".into(), "f".into()));
        assert_eq!(o.scope_names(5), (String::new(), "g".into()));
        assert!(o.fold_at(0).is_some());
        assert!(!o.uses_tabs);
    }
}
