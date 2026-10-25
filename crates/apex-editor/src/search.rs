//! Plain-text search over char sequences.

use std::ops::Range;

/// Options for [`Buffer::find_all`](crate::Buffer::find_all) and
/// [`Buffer::replace_all`](crate::Buffer::replace_all).
///
/// The default is case-insensitive, substring matching (like PyCharm's find
/// bar with "Match Case" and "Words" off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SearchOptions {
    /// Match letter case exactly.
    pub case_sensitive: bool,
    /// Only match where the needle is not adjacent to identifier characters
    /// (letters, digits, `_`).
    pub whole_word: bool,
}

impl SearchOptions {
    /// Case-sensitive substring matching.
    pub fn exact() -> Self {
        Self {
            case_sensitive: true,
            whole_word: false,
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn chars_eq(a: char, b: char, case_sensitive: bool) -> bool {
    if a == b {
        return true;
    }
    if case_sensitive {
        return false;
    }
    if a.is_ascii() && b.is_ascii() {
        return a.eq_ignore_ascii_case(&b);
    }
    a.to_lowercase().eq(b.to_lowercase())
}

/// Returns the non-overlapping char ranges in `haystack` matching `needle`,
/// scanning left to right. An empty needle matches nothing.
pub(crate) fn find_all(
    haystack: &[char],
    needle: &[char],
    opts: SearchOptions,
) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let m = needle.len();
    if m == 0 || m > haystack.len() {
        return out;
    }
    let mut i = 0;
    while i + m <= haystack.len() {
        let hit = haystack[i..i + m]
            .iter()
            .zip(needle)
            .all(|(&a, &b)| chars_eq(a, b, opts.case_sensitive));
        let boundary_ok = !opts.whole_word
            || ((i == 0 || !is_word_char(haystack[i - 1]))
                && !matches!(haystack.get(i + m), Some(&c) if is_word_char(c)));
        if hit && boundary_ok {
            out.push(i..i + m);
            i += m;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(h: &str, n: &str, opts: SearchOptions) -> Vec<Range<usize>> {
        let h: Vec<char> = h.chars().collect();
        let n: Vec<char> = n.chars().collect();
        find_all(&h, &n, opts)
    }

    #[test]
    fn basic_and_non_overlapping() {
        assert_eq!(run("aaaa", "aa", SearchOptions::exact()), vec![0..2, 2..4]);
        assert!(run("abc", "", SearchOptions::exact()).is_empty());
        assert!(run("ab", "abc", SearchOptions::exact()).is_empty());
    }

    #[test]
    fn case_insensitive_including_unicode() {
        let o = SearchOptions::default();
        assert_eq!(run("Foo foo FOO", "foo", o), vec![0..3, 4..7, 8..11]);
        assert_eq!(run("ÄBC äbc", "äbc", o), vec![0..3, 4..7]);
        assert_eq!(run("Foo foo", "foo", SearchOptions::exact()), vec![4..7]);
    }

    #[test]
    fn whole_word() {
        let o = SearchOptions {
            case_sensitive: true,
            whole_word: true,
        };
        assert_eq!(run("x x_1 ax x(x)", "x", o), vec![0..1, 9..10, 11..12]);
    }
}
