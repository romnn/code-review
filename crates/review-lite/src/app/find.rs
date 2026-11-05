//! Find within the current step: case-insensitive matches over row texts.

use std::ops::Range;

/// One match: a row and a byte range of its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The row.
    pub row: usize,
    /// The matched bytes.
    pub range: Range<usize>,
}

/// Finds every match of `query` in `rows`, ignoring ASCII case.
pub fn matches<'a>(rows: impl IntoIterator<Item = &'a str>, query: &str) -> Vec<Match> {
    let needle = query.as_bytes();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for (row, text) in rows.into_iter().enumerate() {
        let haystack = text.as_bytes();
        let mut start = 0;
        while start + needle.len() <= haystack.len() {
            let window = haystack
                .get(start..start + needle.len())
                .unwrap_or_default();
            if window.eq_ignore_ascii_case(needle) && text.is_char_boundary(start) {
                found.push(Match {
                    row,
                    range: start..start + needle.len(),
                });
                start += needle.len();
            } else {
                start += 1;
            }
        }
    }
    found
}

/// The index of the first match at or after `from` (or before it, backwards), wrapping.
#[must_use]
pub fn next_from(found: &[Match], from: (usize, usize), forward: bool) -> Option<usize> {
    if found.is_empty() {
        return None;
    }
    let key = |found: &Match| (found.row, found.range.start);
    if forward {
        found
            .iter()
            .position(|candidate| key(candidate) >= from)
            .or(Some(0))
    } else {
        found
            .iter()
            .rposition(|candidate| key(candidate) < from)
            .or(Some(found.len() - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_ignore_case_and_do_not_overlap() {
        let found = matches(["Format format", "none", "FORMATformat"], "format");
        let places: Vec<(usize, usize)> = found.iter().map(|m| (m.row, m.range.start)).collect();
        assert_eq!(places, [(0, 0), (0, 7), (2, 0), (2, 6)]);
        assert_eq!(matches(["abc"], ""), Vec::new());
        assert_eq!(matches(["aaaa"], "aa").len(), 2);
    }

    #[test]
    fn next_and_previous_wrap_around() {
        let found = matches(["x", "ab x", "x"], "x");
        assert_eq!(next_from(&found, (1, 0), true), Some(1));
        assert_eq!(next_from(&found, (2, 1), true), Some(0));
        assert_eq!(next_from(&found, (1, 3), false), Some(0));
        assert_eq!(next_from(&found, (0, 0), false), Some(2));
        assert_eq!(next_from(&[], (0, 0), true), None);
    }
}
