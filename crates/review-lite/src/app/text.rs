//! Text selection over the rows of a pane, and the word rules that clicks and tokens use.

use std::ops::Range;

/// A place in a pane's text: a row and a byte offset within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPos {
    /// The row.
    pub row: usize,
    /// The byte offset within the row's text.
    pub col: usize,
}

/// Which pane a selection belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pane {
    /// The hunk list.
    Diff,
    /// The read-only source view.
    Source,
}

/// A selection: fixed at `anchor`, extended to `head` by dragging or shift-clicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// The pane the selection is in.
    pub pane: Pane,
    /// Where the selection started.
    pub anchor: TextPos,
    /// Where it ends now.
    pub head: TextPos,
}

impl Selection {
    /// An empty selection at `pos`.
    #[must_use]
    pub fn caret(pane: Pane, pos: TextPos) -> Self {
        Self {
            pane,
            anchor: pos,
            head: pos,
        }
    }

    /// The start and end in text order.
    #[must_use]
    pub fn ordered(&self) -> (TextPos, TextPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether nothing is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The selected byte range of `row`, whose text is `len` bytes long; rows inside a
    /// multi-line selection are selected whole.
    #[must_use]
    pub fn range_in_row(&self, row: usize, len: usize) -> Option<Range<usize>> {
        let (start, end) = self.ordered();
        if self.is_empty() || row < start.row || row > end.row {
            return None;
        }
        let from = if row == start.row {
            start.col.min(len)
        } else {
            0
        };
        let to = if row == end.row {
            end.col.min(len)
        } else {
            len
        };
        (from < to || (row != end.row && from == to)).then_some(from..to)
    }

    /// The selected text, one line per row, where `line` gives a row's copyable text.
    pub fn text<'a>(&self, line: impl Fn(usize) -> Option<&'a str>) -> String {
        let (start, end) = self.ordered();
        let mut lines = Vec::new();
        for row in start.row..=end.row {
            let Some(text) = line(row) else {
                continue;
            };
            let from = if row == start.row {
                start.col.min(text.len())
            } else {
                0
            };
            let to = if row == end.row {
                end.col.min(text.len())
            } else {
                text.len()
            };
            lines.push(text.get(from..to.max(from)).unwrap_or_default());
        }
        lines.join("\n")
    }
}

/// Whether `ch` belongs to a word.
fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// The word (or run of other non-space characters) around byte `col` of `text`, for
/// double-click selection.
#[must_use]
pub fn word_at(text: &str, col: usize) -> Range<usize> {
    let col = floor_char_boundary(text, col.min(text.len()));
    let class = |ch: char| {
        if is_word(ch) {
            0
        } else if ch.is_whitespace() {
            1
        } else {
            2
        }
    };
    let Some(here) = text[col..]
        .chars()
        .next()
        .or_else(|| text[..col].chars().next_back())
    else {
        return col..col;
    };
    let kind = class(here);
    let start = text[..col]
        .char_indices()
        .rev()
        .take_while(|&(_, ch)| class(ch) == kind)
        .last()
        .map_or(col, |(index, _)| index);
    let end = text[col..]
        .char_indices()
        .find(|&(_, ch)| class(ch) != kind)
        .map_or(text.len(), |(index, _)| col + index);
    start..end
}

/// Byte ranges of the identifiers in `text`, skipping numbers.
#[must_use]
pub fn identifiers(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        match (is_word(ch), start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                found.push(begin..index);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        found.push(begin..text.len());
    }
    found.retain(|range| {
        !text
            .get(range.clone())
            .is_some_and(|word| word.starts_with(|ch: char| ch.is_ascii_digit()))
    });
    found
}

/// The largest char boundary at or below `index`.
#[must_use]
pub fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(row: usize, col: usize) -> TextPos {
        TextPos { row, col }
    }

    /// A backwards drag over three rows selects the tail of the first row, the whole middle
    /// row and the head of the last one, and copies them as lines.
    #[test]
    fn multi_line_selection_covers_whole_middle_rows() {
        let rows = ["alpha beta", "gamma", "delta epsilon"];
        let selection = Selection {
            pane: Pane::Diff,
            anchor: pos(2, 5),
            head: pos(0, 6),
        };
        assert_eq!(selection.range_in_row(0, 10), Some(6..10));
        assert_eq!(selection.range_in_row(1, 5), Some(0..5));
        assert_eq!(selection.range_in_row(2, 13), Some(0..5));
        assert_eq!(selection.range_in_row(3, 4), None);
        assert_eq!(
            selection.text(|row| rows.get(row).copied()),
            "beta\ngamma\ndelta"
        );
        assert_eq!(
            Selection::caret(Pane::Diff, pos(1, 2)).text(|row| rows.get(row).copied()),
            ""
        );
    }

    #[test]
    fn words_group_identifiers_spaces_and_punctuation() {
        let text = "let total = sum(a_b);";
        assert_eq!(&text[word_at(text, 5)], "total");
        assert_eq!(&text[word_at(text, 9)], " ");
        assert_eq!(&text[word_at(text, 15)], "(");
        assert_eq!(&text[word_at(text, text.len())], ");");
        assert_eq!(word_at("", 3), 0..0);
        assert_eq!(&"aé b"[word_at("aé b", 2)], "aé");
    }

    #[test]
    fn identifiers_skip_numbers_and_punctuation() {
        let text = "let total = sum(a_b, 42) + café;";
        let found: Vec<&str> = identifiers(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["let", "total", "sum", "a_b", "café"]);
    }
}
