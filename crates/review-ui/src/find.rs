//! Find within the current step: case-insensitive matches over row texts, each half of a
//! side-by-side row apart.

use std::ops::Range;

use crate::text::Column;

/// One match: a row, the column its text is in, and a byte range of that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// The row.
    pub row: usize,
    /// The column the text is in: the whole width, or a half of a side-by-side row.
    pub column: Column,
    /// The matched bytes.
    pub range: Range<usize>,
}

impl Match {
    /// Where the match starts.
    #[must_use]
    pub fn start(&self) -> Spot {
        Spot {
            row: self.row,
            column: self.column,
            byte: self.range.start,
        }
    }
}

/// A place in the searched text: a row, a column of it, and a byte offset in that column's
/// text. Places order as the text reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Spot {
    /// The row.
    pub row: usize,
    /// The column; [`Column::Whole`] comes before both halves of a row.
    pub column: Column,
    /// The byte offset in the column's text.
    pub byte: usize,
}

impl Spot {
    /// The place `byte` bytes into row `row` across the whole width.
    #[must_use]
    pub fn whole(row: usize, byte: usize) -> Self {
        Self {
            row,
            column: Column::Whole,
            byte,
        }
    }
}

/// The byte ranges of `needle` in `text`, ignoring ASCII case, without overlaps.
#[must_use]
pub fn ranges(text: &str, needle: &str) -> Vec<Range<usize>> {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return Vec::new();
    }
    let haystack = text.as_bytes();
    let mut found = Vec::new();
    let mut start = 0;
    while start + needle.len() <= haystack.len() {
        let window = haystack
            .get(start..start + needle.len())
            .unwrap_or_default();
        if window.eq_ignore_ascii_case(needle) && text.is_char_boundary(start) {
            found.push(start..start + needle.len());
            start += needle.len();
        } else {
            start += 1;
        }
    }
    found
}

/// Finds every match of `query` in `texts`, each a row's text in one of its columns, given in
/// reading order.
pub fn matches<'a>(
    texts: impl IntoIterator<Item = (usize, Column, &'a str)>,
    query: &str,
) -> Vec<Match> {
    texts
        .into_iter()
        .flat_map(|(row, column, text)| {
            ranges(text, query)
                .into_iter()
                .map(move |range| Match { row, column, range })
        })
        .collect()
}

/// The index of the first match at or after `from` (or before it, backwards), wrapping.
#[must_use]
pub fn next_from(found: &[Match], from: Spot, forward: bool) -> Option<usize> {
    if found.is_empty() {
        return None;
    }
    if forward {
        found
            .iter()
            .position(|candidate| candidate.start() >= from)
            .or(Some(0))
    } else {
        found
            .iter()
            .rposition(|candidate| candidate.start() < from)
            .or(Some(found.len() - 1))
    }
}

/// The hunk a diff row belongs with: the one whose rows hold it, else the nearest by rows,
/// the one above on a tie.
#[must_use]
pub fn nearest_hunk(hunk_rows: &[Range<usize>], row: usize) -> Option<usize> {
    hunk_rows
        .iter()
        .enumerate()
        .filter(|(_, rows)| !rows.is_empty())
        .min_by_key(|(index, rows)| {
            let distance = if row < rows.start {
                rows.start - row
            } else {
                row.saturating_sub(rows.end - 1)
            };
            // Below the row ranks after above it at the same distance.
            (distance, usize::from(row < rows.start), *index)
        })
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows of whole-width text, as the buffer and the unified diff search them.
    fn lines<'a>(texts: &[&'a str]) -> Vec<(usize, Column, &'a str)> {
        texts
            .iter()
            .enumerate()
            .map(|(row, text)| (row, Column::Whole, *text))
            .collect()
    }

    #[test]
    fn matches_ignore_case_and_do_not_overlap() {
        let found = matches(lines(&["Format format", "none", "FORMATformat"]), "format");
        let places: Vec<(usize, usize)> = found.iter().map(|m| (m.row, m.range.start)).collect();
        assert_eq!(places, [(0, 0), (0, 7), (2, 0), (2, 6)]);
        assert_eq!(matches(lines(&["abc"]), ""), Vec::new());
        assert_eq!(matches(lines(&["aaaa"]), "aa").len(), 2);
        assert_eq!(ranges("aXbx", "x"), [1..2, 3..4]);
    }

    /// Both halves of a side-by-side row are searched apart, the base's first, and stepping
    /// goes through them as the row reads, from a place at the row's start.
    #[test]
    fn halves_of_a_row_match_apart_in_reading_order() {
        let found = matches(
            [
                (0, Column::Left, "let total = 1;"),
                (0, Column::Right, "let total = 2;"),
                (1, Column::Right, "total"),
            ],
            "total",
        );
        let places: Vec<(usize, Column, usize)> = found
            .iter()
            .map(|m| (m.row, m.column, m.range.start))
            .collect();
        assert_eq!(
            places,
            [
                (0, Column::Left, 4),
                (0, Column::Right, 4),
                (1, Column::Right, 0)
            ]
        );
        assert_eq!(next_from(&found, Spot::whole(0, 0), true), Some(0));
        let after_left = Spot {
            byte: 5,
            ..found.first().map(Match::start).unwrap_or(Spot::whole(0, 0))
        };
        assert_eq!(next_from(&found, after_left, true), Some(1));
        assert_eq!(next_from(&found, Spot::whole(1, 0), false), Some(1));
    }

    /// A match in a hunk selects it; one in unchanged code selects the nearest hunk.
    #[test]
    fn matches_select_their_nearest_hunk() {
        let hunks = [2..5, 10..12, 20..21];
        assert_eq!(nearest_hunk(&hunks, 3), Some(0));
        assert_eq!(
            nearest_hunk(&hunks, 6),
            Some(0),
            "one row below the first hunk"
        );
        assert_eq!(
            nearest_hunk(&hunks, 8),
            Some(1),
            "two rows above the second"
        );
        assert_eq!(
            nearest_hunk(&hunks, 7),
            Some(0),
            "a tie goes to the hunk above"
        );
        assert_eq!(nearest_hunk(&hunks, 0), Some(0));
        assert_eq!(nearest_hunk(&hunks, 99), Some(2));
        assert_eq!(nearest_hunk(&[], 3), None);
    }

    #[test]
    fn next_and_previous_wrap_around() {
        let found = matches(lines(&["x", "ab x", "x"]), "x");
        assert_eq!(next_from(&found, Spot::whole(1, 0), true), Some(1));
        assert_eq!(next_from(&found, Spot::whole(2, 1), true), Some(0));
        assert_eq!(next_from(&found, Spot::whole(1, 3), false), Some(0));
        assert_eq!(next_from(&found, Spot::whole(0, 0), false), Some(2));
        assert_eq!(next_from(&[], Spot::whole(0, 0), true), None);
    }
}
