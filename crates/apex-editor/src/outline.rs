//! Indentation-based structure: fold ranges, indent guides and the
//! class/function scope at a line (for a navigation bar).
//!
//! Works on any language that uses indentation for blocks (Python first);
//! bracketed continuation lines fold too because they are indented.

use crate::cursor::{display_width, TAB_WIDTH};

/// Leading indentation of `line` in visual columns, or `None` for a blank
/// line.
pub fn indent_width(line: &str) -> Option<usize> {
    if line.trim().is_empty() {
        return None;
    }
    let lead: String = line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    Some(display_width(&lead))
}

/// Effective indentation of every line: blank lines take the smaller
/// indentation of the nearest non-blank lines around them, so guides and
/// folds run through blank lines inside a block.
pub fn effective_indents(lines: &[&str]) -> Vec<usize> {
    let raw: Vec<Option<usize>> = lines.iter().map(|l| indent_width(l)).collect();
    let mut next = vec![0usize; raw.len()];
    let mut after = 0;
    for i in (0..raw.len()).rev() {
        next[i] = after;
        if let Some(w) = raw[i] {
            after = w;
        }
    }
    let mut before = 0;
    raw.iter()
        .enumerate()
        .map(|(i, w)| match w {
            Some(w) => {
                before = *w;
                *w
            }
            None => before.min(next[i]),
        })
        .collect()
}

/// A foldable region: `start` is the header line, `end` the last line
/// hidden when folded (inclusive). Trailing blank lines are not included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FoldRange {
    /// Header line (stays visible).
    pub start: usize,
    /// Last folded line.
    pub end: usize,
}

/// Fold ranges by indentation: a non-blank line followed by more-indented
/// lines starts a region that ends before the next line indented no deeper
/// than it.
pub fn fold_ranges(lines: &[&str]) -> Vec<FoldRange> {
    let raw: Vec<Option<usize>> = lines.iter().map(|l| indent_width(l)).collect();
    let mut out = Vec::new();
    // Stack of (indent, start line) of open regions.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut last_nonblank = 0usize;
    for (i, w) in raw.iter().enumerate() {
        let Some(w) = *w else { continue };
        while let Some(&(sw, start)) = stack.last() {
            if w <= sw {
                stack.pop();
                if last_nonblank > start {
                    out.push(FoldRange {
                        start,
                        end: last_nonblank,
                    });
                }
            } else {
                break;
            }
        }
        stack.push((w, i));
        last_nonblank = i;
    }
    while let Some((_, start)) = stack.pop() {
        if last_nonblank > start {
            out.push(FoldRange {
                start,
                end: last_nonblank,
            });
        }
    }
    out.sort_by_key(|r| r.start);
    out
}

/// Visual columns where indent guides are drawn on a line with effective
/// indentation `indent` (one per level, starting at the first level).
pub fn guide_columns(indent: usize) -> impl Iterator<Item = usize> {
    (1..=indent / TAB_WIDTH).map(|l| (l - 1) * TAB_WIDTH)
}

/// A class or function that encloses a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// `"class"` or `"def"`.
    pub kind: &'static str,
    /// Its name.
    pub name: String,
    /// Line of the definition.
    pub line: usize,
}

fn definition(line: &str) -> Option<(&'static str, String)> {
    let t = line.trim_start();
    let t = t.strip_prefix("async ").unwrap_or(t);
    let (kind, rest) = if let Some(r) = t.strip_prefix("def ") {
        ("def", r)
    } else if let Some(r) = t.strip_prefix("class ") {
        ("class", r)
    } else {
        return None;
    };
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some((kind, name))
}

/// The classes/functions enclosing `line`, outermost first (a definition
/// line counts as inside itself).
pub fn scopes_at(lines: &[&str], line: usize) -> Vec<Scope> {
    if lines.is_empty() {
        return Vec::new();
    }
    let line = line.min(lines.len() - 1);
    let mut limit = match indent_width(lines[line]) {
        Some(w) => w + 1,
        None => {
            // Use the next non-blank line's indentation.
            lines[line..]
                .iter()
                .find_map(|l| indent_width(l))
                .unwrap_or(0)
                + 1
        }
    };
    let mut out = Vec::new();
    for i in (0..=line).rev() {
        let Some(w) = indent_width(lines[i]) else {
            continue;
        };
        if w < limit {
            if let Some((kind, name)) = definition(lines[i]) {
                out.push(Scope {
                    kind,
                    name,
                    line: i,
                });
            }
            limit = w;
            if w == 0 {
                break;
            }
        }
    }
    out.reverse();
    out
}

/// Every class and function in the file, in order, with nesting depth.
pub fn definitions(lines: &[&str]) -> Vec<(Scope, usize)> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let (kind, name) = definition(l)?;
            let depth = indent_width(l).unwrap_or(0) / TAB_WIDTH;
            Some((
                Scope {
                    kind,
                    name,
                    line: i,
                },
                depth,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "import os\n\nclass A:\n    def f(self):\n        x = 1\n\n        return x\n\n    def g(self):\n        pass\n\ny = A()\n";

    fn lines() -> Vec<&'static str> {
        SRC.split('\n').collect()
    }

    #[test]
    fn folds_follow_indentation() {
        let f = fold_ranges(&lines());
        assert_eq!(
            f,
            vec![
                FoldRange { start: 2, end: 9 },
                FoldRange { start: 3, end: 6 },
                FoldRange { start: 8, end: 9 },
            ]
        );
    }

    #[test]
    fn blank_lines_inherit_indent_for_guides() {
        let e = effective_indents(&lines());
        assert_eq!(e[5], 8, "blank line inside f");
        assert_eq!(e[7], 4, "blank line between methods");
        assert_eq!(e[10], 0);
        assert_eq!(guide_columns(8).collect::<Vec<_>>(), vec![0, 4]);
        assert_eq!(guide_columns(0).count(), 0);
    }

    #[test]
    fn scopes_and_definitions() {
        let l = lines();
        let s = scopes_at(&l, 4);
        let names: Vec<&str> = s.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["A", "f"]);
        assert_eq!(scopes_at(&l, 9).last().unwrap().name, "g");
        assert!(scopes_at(&l, 11).is_empty());
        assert_eq!(scopes_at(&l, 3).last().unwrap().name, "f");
        let d = definitions(&l);
        assert_eq!(d.len(), 3);
        assert_eq!(d[1].1, 1);
        assert_eq!(definition("async def run():"), Some(("def", "run".into())));
    }
}
