//! Line and token diffs over text, and line maps between two versions of a file.

use std::hash::Hash;
use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};

/// Splits `text` into lines, each keeping its terminator, so that a missing final newline is a
/// visible difference.
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// A changed region between two sequences, as half-open index ranges into each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    pub before: Range<usize>,
    pub after: Range<usize>,
}

/// A borrowed token sequence that imara-diff can intern.
struct Tokens<'a, T>(&'a [T]);

impl<'a, T: Copy + Hash + Eq> TokenSource for Tokens<'a, T> {
    type Token = T;
    type Tokenizer = std::iter::Copied<std::slice::Iter<'a, T>>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.iter().copied()
    }

    fn estimate_tokens(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
    }
}

/// Diffs two line sequences with the histogram algorithm and git's indentation heuristic.
pub(crate) fn diff_lines(before: &[&str], after: &[&str]) -> Vec<Hunk> {
    let input = InternedInput::new(Tokens(before), Tokens(after));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);
    collect_hunks(&diff)
}

/// Diffs two token sequences with the histogram algorithm.
pub(crate) fn diff_tokens(before: &[&str], after: &[&str]) -> Vec<Hunk> {
    let input = InternedInput::new(Tokens(before), Tokens(after));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_no_heuristic(&input);
    collect_hunks(&diff)
}

fn collect_hunks(diff: &Diff) -> Vec<Hunk> {
    diff.hunks()
        .map(|hunk| Hunk {
            before: hunk.before.start as usize..hunk.before.end as usize,
            after: hunk.after.start as usize..hunk.after.end as usize,
        })
        .collect()
}

/// Maps the lines of one version of a file to the lines of another along a line diff.
///
/// Only lines outside every changed region map; a line the diff removed or replaced has no
/// image.
#[derive(Debug, Clone)]
pub(crate) struct LineMap {
    forward: Vec<Option<usize>>,
    after_len: usize,
}

impl LineMap {
    /// Builds the map from the diff of `before` against `after`.
    pub(crate) fn new(before: &[&str], after: &[&str]) -> Self {
        let hunks = diff_lines(before, after);
        let mut forward = vec![None; before.len()];
        let mut before_line = 0;
        let mut after_line = 0;
        let mut unchanged_until = |end: usize, before_line: &mut usize, after_line: &mut usize| {
            while *before_line < end {
                if let Some(slot) = forward.get_mut(*before_line) {
                    *slot = Some(*after_line);
                }
                *before_line += 1;
                *after_line += 1;
            }
        };
        for hunk in &hunks {
            unchanged_until(hunk.before.start, &mut before_line, &mut after_line);
            before_line = hunk.before.end;
            after_line = hunk.after.end;
        }
        unchanged_until(before.len(), &mut before_line, &mut after_line);
        Self {
            forward,
            after_len: after.len(),
        }
    }

    /// Builds the identity map for two equal versions with `len` lines.
    pub(crate) fn identity(len: usize) -> Self {
        Self {
            forward: (0..len).map(Some).collect(),
            after_len: len,
        }
    }

    /// Returns the image of line `line`, if it is unchanged.
    pub(crate) fn line(&self, line: usize) -> Option<usize> {
        self.forward.get(line).copied().flatten()
    }

    /// Maps the gap before line `gap` (an insertion point) leniently: through whichever
    /// neighboring line maps.
    pub(crate) fn gap_lenient(&self, gap: usize) -> Option<usize> {
        let below = self.line(gap);
        let above = gap
            .checked_sub(1)
            .and_then(|line| self.line(line))
            .map(|line| line + 1);
        below
            .or(above)
            .or_else(|| self.forward.is_empty().then_some(0))
    }

    /// Maps the gap before line `gap` exactly: every existing neighbor must map, and both must
    /// agree that nothing was inserted between them.
    pub(crate) fn gap_exact(&self, gap: usize) -> Option<usize> {
        let below = (gap < self.forward.len()).then(|| self.line(gap));
        let above = gap
            .checked_sub(1)
            .map(|line| self.line(line).map(|line| line + 1));
        match (above, below) {
            (Some(Some(above)), Some(Some(below))) => (above == below).then_some(above),
            (Some(Some(above)), None) => (above == self.after_len).then_some(above),
            (None, Some(Some(below))) => (below == 0).then_some(0),
            (None, None) => (self.after_len == 0).then_some(0),
            _ => None,
        }
    }

    /// Maps a non-empty line range exactly: every line must map, consecutively.
    pub(crate) fn range_exact(&self, range: &Range<usize>) -> Option<Range<usize>> {
        let start = self.line(range.start)?;
        for (offset, line) in range.clone().enumerate() {
            if self.line(line)? != start + offset {
                return None;
            }
        }
        Some(start..start + range.len())
    }
}

/// Splits text into coarse tokens for highlighting and size checks: identifier and number runs,
/// and single punctuation characters; whitespace separates tokens and is never one.
pub(crate) fn generic_tokens(text: &str) -> Vec<Range<usize>> {
    let mut tokens = Vec::new();
    let mut word_start: Option<usize> = None;
    for (offset, character) in text.char_indices() {
        let is_word = character.is_alphanumeric() || character == '_';
        if is_word {
            word_start.get_or_insert(offset);
            continue;
        }
        if let Some(start) = word_start.take() {
            tokens.push(start..offset);
        }
        if !character.is_whitespace() {
            tokens.push(offset..offset + character.len_utf8());
        }
    }
    if let Some(start) = word_start {
        tokens.push(start..text.len());
    }
    tokens
}

/// Returns the ranges of `before_tokens` and `after_tokens` that a token diff marks as changed,
/// merging touching ranges.
pub(crate) fn changed_token_ranges(
    before_text: &str,
    before_tokens: &[Range<usize>],
    after_text: &str,
    after_tokens: &[Range<usize>],
) -> ChangedRanges {
    let before: Vec<&str> = before_tokens
        .iter()
        .map(|range| before_text.get(range.clone()).unwrap_or(""))
        .collect();
    let after: Vec<&str> = after_tokens
        .iter()
        .map(|range| after_text.get(range.clone()).unwrap_or(""))
        .collect();
    let mut changed = ChangedRanges::default();
    for hunk in diff_tokens(&before, &after) {
        push_span(&mut changed.before, before_tokens, &hunk.before);
        push_span(&mut changed.after, after_tokens, &hunk.after);
    }
    changed
}

/// Changed byte ranges on both sides of a token diff.
#[derive(Debug, Default)]
pub(crate) struct ChangedRanges {
    pub before: Vec<Range<usize>>,
    pub after: Vec<Range<usize>>,
}

fn push_span(spans: &mut Vec<Range<usize>>, tokens: &[Range<usize>], hunk: &Range<usize>) {
    let (Some(first), Some(last)) = (
        tokens.get(hunk.start),
        hunk.end.checked_sub(1).and_then(|end| tokens.get(end)),
    ) else {
        return;
    };
    if hunk.is_empty() {
        return;
    }
    let span = first.start..last.end;
    match spans.last_mut() {
        Some(previous) if previous.end >= span.start => previous.end = span.end.max(previous.end),
        _ => spans.push(span),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_map_maps_only_unchanged_lines() {
        let before = split_lines("a\nb\nc\nd\n");
        let after = split_lines("x\na\nb\nd\n");
        let map = LineMap::new(&before, &after);
        assert_eq!(map.line(0), Some(1));
        assert_eq!(map.line(1), Some(2));
        assert_eq!(map.line(2), None);
        assert_eq!(map.line(3), Some(3));
        assert_eq!(map.range_exact(&(0..2)), Some(1..3));
        assert_eq!(map.gap_exact(3), None);
        assert_eq!(map.gap_exact(4), Some(4));
        assert_eq!(map.gap_lenient(3), Some(3));
    }

    #[test]
    fn generic_tokens_split_words_and_punctuation() {
        let text = "foo(bar_1, 2)";
        let tokens: Vec<&str> = generic_tokens(text)
            .iter()
            .map(|r| &text[r.clone()])
            .collect();
        assert_eq!(tokens, ["foo", "(", "bar_1", ",", "2", ")"]);
    }
}
