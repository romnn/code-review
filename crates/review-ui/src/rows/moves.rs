//! A move's edits as the diff shows them: the lines the move kept, and the lines it edited,
//! paired old with new and with the words that changed marked, as delta marks them.
//!
//! A move that only re-indented its code, by the same amount on every line, shows each line once,
//! in its new indentation with the indentation it gained marked, and its note says how far it
//! moved in: the reviewer sees the code where it now is and is told what changed on every line.
//! The engine makes such a move plain where a syntax tree proves that formatting is all it
//! changed; where it cannot, as in Python, the note still names the re-indent. A line whose
//! indentation changed in an uneven re-indent pairs with its old version, only the indentation
//! marked, since there the change differs line by line.

use std::ops::Range;

use review_core::{DiffLine, DiffLineKind, SideDiff};

/// One line of a move's edits as the diff shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditLine<'a> {
    /// A line the move kept as it was.
    Kept(&'a DiffLine),
    /// A line of a move that re-indented every line alike, in its new indentation.
    Shifted(&'a DiffLine),
    /// A line only the source has, or the old version of an edited line.
    Old(&'a DiffLine),
    /// A line only the target has, or the new version of an edited line.
    New(&'a DiffLine),
}

/// A move's edits, line by line, with the words each edited line changed.
#[derive(Debug, Default)]
pub(super) struct Edits<'a> {
    /// The lines in the order the diff shows them, each with the byte ranges of its words the
    /// edit changed, empty for a kept line.
    pub lines: Vec<(EditLine<'a>, Vec<Range<usize>>)>,
    /// Whether all the move changed is the indentation of its code, by the same amount on every
    /// line.
    pub reindented: bool,
    /// How many columns deeper the move indented its code, fewer when negative, a tab as four;
    /// zero unless it is [`Self::reindented`].
    pub shift: isize,
}

/// The edits of a move whose inner diff is `diff`.
#[must_use]
pub(super) fn edits(diff: &SideDiff) -> Edits<'_> {
    let mut lines = Vec::new();
    // How far each line that is not blank moved its indentation, the kept ones by none: the move
    // re-indented its code only when every one of them shifted alike.
    let mut shifts: Vec<isize> = Vec::new();
    // Whether any line changed beyond its indentation.
    let mut beyond = false;
    // The run of changed lines since the last kept one: its old lines and its new ones.
    let (mut olds, mut news): (Vec<&DiffLine>, Vec<&DiffLine>) = (Vec::new(), Vec::new());
    for line in &diff.lines {
        match line.kind {
            DiffLineKind::Same => {
                beyond |= pair_run(&olds, &news, &mut lines, &mut shifts);
                (olds, news) = (Vec::new(), Vec::new());
                if !line.text.trim().is_empty() {
                    shifts.push(0);
                }
                lines.push((EditLine::Kept(line), Vec::new()));
            }
            DiffLineKind::Old => olds.push(line),
            DiffLineKind::New => news.push(line),
        }
    }
    beyond |= pair_run(&olds, &news, &mut lines, &mut shifts);
    let common = shifts.first().copied();
    let uniform = shifts.iter().all(|&shift| Some(shift) == common);
    let reindented = !beyond && uniform && common.is_some_and(|shift| shift != 0);
    // A line that changed only its indentation shows once when every line shifted alike, and as
    // old and new otherwise.
    let mut shown = Vec::with_capacity(lines.len());
    let mut pending: Option<&DiffLine> = None;
    for (line, words) in lines {
        match (line, pending.take()) {
            // An indentation pair arrives as its old line, then its new one, both `Shifted`.
            (EditLine::Shifted(old), None) => pending = Some(old),
            (EditLine::Shifted(new), Some(old)) if reindented => {
                shown.push((EditLine::Shifted(new), gained(&old.text, &new.text)));
            }
            (EditLine::Shifted(new), Some(old)) => {
                shown.push((EditLine::Old(old), leading(&old.text)));
                shown.push((EditLine::New(new), leading(&new.text)));
            }
            (line, _) => shown.push((line, words)),
        }
    }
    Edits {
        lines: shown,
        reindented,
        shift: if reindented { common.unwrap_or(0) } else { 0 },
    }
}

/// Pairs a run of `olds` replaced by `news`: a line equal but for its leading whitespace pairs
/// with its old version, only the indentation marked, recording its shift in `shifts`; the
/// lines between such pairs are edits, each old line paired with a new one in order and marked
/// with the words it changed, delta's way: the old lines of a stretch, then its new ones.
/// Returns whether the run has such edits.
fn pair_run<'a>(
    olds: &[&'a DiffLine],
    news: &[&'a DiffLine],
    lines: &mut Vec<(EditLine<'a>, Vec<Range<usize>>)>,
    shifts: &mut Vec<isize>,
) -> bool {
    let trimmed = |line: &&DiffLine| line.text.trim_start().to_owned();
    // A run too long to align shows whole, its old lines then its new ones.
    let matched = lcs(
        &olds.iter().map(trimmed).collect::<Vec<_>>(),
        &news.iter().map(trimmed).collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    let mut beyond = false;
    let (mut old_at, mut new_at) = (0, 0);
    for (old_end, new_end) in matched
        .iter()
        .copied()
        .chain(std::iter::once((olds.len(), news.len())))
    {
        let gap_olds = olds.get(old_at..old_end).unwrap_or_default();
        let gap_news = news.get(new_at..new_end).unwrap_or_default();
        beyond |= !gap_olds.is_empty() || !gap_news.is_empty();
        let paired: Vec<(Words, Words)> = gap_olds
            .iter()
            .zip(gap_news)
            .map(|(old, new)| word_changes(&old.text, &new.text))
            .collect();
        for (index, old) in gap_olds.iter().enumerate() {
            let words = paired
                .get(index)
                .map(|(words, _)| words.clone())
                .unwrap_or_default();
            lines.push((EditLine::Old(old), words));
        }
        for (index, new) in gap_news.iter().enumerate() {
            let words = paired
                .get(index)
                .map(|(_, words)| words.clone())
                .unwrap_or_default();
            lines.push((EditLine::New(new), words));
        }
        if let (Some(old), Some(new)) = (olds.get(old_end), news.get(new_end)) {
            if !new.text.trim().is_empty() {
                shifts.push(width(indent(&new.text)) - width(indent(&old.text)));
            }
            // A line kept but for its ending shows both versions, their markers saying how
            // each ends, and is more than a re-indent.
            let same_ending = old.ending == new.ending;
            beyond |= !same_ending;
            if old.text == new.text && same_ending {
                lines.push((EditLine::Kept(new), Vec::new()));
            } else if old.text == new.text || !same_ending {
                lines.push((EditLine::Old(old), Vec::new()));
                lines.push((EditLine::New(new), Vec::new()));
            } else {
                // Only the indentation changed: `edits` shows it once or as old and new, once it
                // knows whether every line shifted alike.
                lines.push((EditLine::Shifted(old), Vec::new()));
                lines.push((EditLine::Shifted(new), Vec::new()));
            }
        }
        (old_at, new_at) = (old_end + 1, new_end + 1);
    }
    beyond
}

/// The byte ranges of words in a line.
type Words = Vec<Range<usize>>;

/// The indentation `new` gained over `old`, as the range a re-indented line marks: none when
/// it lost some.
fn gained(old: &str, new: &str) -> Words {
    let (old, new) = (indent(old), indent(new));
    if new.len() <= old.len() {
        return Vec::new();
    }
    let start = if new.starts_with(old) { old.len() } else { 0 };
    std::iter::once(start..new.len()).collect()
}

/// The leading whitespace of `text` as the one range a line marks when only it changed.
fn leading(text: &str) -> Words {
    std::iter::once(0..indent(text).len()).collect()
}

/// The leading whitespace of `text`.
fn indent(text: &str) -> &str {
    &text[..text.len() - text.trim_start().len()]
}

/// How wide leading whitespace is, a tab as one level of four.
fn width(indent: &str) -> isize {
    indent
        .chars()
        .map(|space| if space == '\t' { 4 } else { 1 })
        .sum()
}

/// The most cells an alignment's table may take, so a huge move or a huge line costs bounded
/// memory and time: past it, lines show unaligned and words unmarked.
const MAX_ALIGNMENT_CELLS: usize = 1 << 20;

/// The pairs of positions a longest common subsequence of `old` and `new` matches, in order, or
/// `None` when aligning them would take more than [`MAX_ALIGNMENT_CELLS`].
fn lcs<T: PartialEq>(old: &[T], new: &[T]) -> Option<Vec<(usize, usize)>> {
    let columns = new.len() + 1;
    if (old.len() + 1).saturating_mul(columns) > MAX_ALIGNMENT_CELLS {
        return None;
    }
    // The length of the longest common subsequence of `old[row..]` and `new[column..]`, at
    // `row * columns + column`.
    let mut table = vec![0_usize; (old.len() + 1) * columns];
    let at = |table: &[usize], row: usize, column: usize| {
        table.get(row * columns + column).copied().unwrap_or(0)
    };
    for (row, old_item) in old.iter().enumerate().rev() {
        for (column, new_item) in new.iter().enumerate().rev() {
            let length = if old_item == new_item {
                at(&table, row + 1, column + 1) + 1
            } else {
                at(&table, row + 1, column).max(at(&table, row, column + 1))
            };
            if let Some(cell) = table.get_mut(row * columns + column) {
                *cell = length;
            }
        }
    }
    let (mut row, mut column) = (0, 0);
    let mut matched = Vec::new();
    while let (Some(old_item), Some(new_item)) = (old.get(row), new.get(column)) {
        if old_item == new_item {
            matched.push((row, column));
            row += 1;
            column += 1;
        } else if at(&table, row + 1, column) >= at(&table, row, column + 1) {
            row += 1;
        } else {
            column += 1;
        }
    }
    Some(matched)
}

/// The byte ranges of the words `old` and `new` do not share, in each, words being runs of
/// letters, digits and `_`, single other characters, and runs of whitespace, which are marked
/// only on a line whose only change they are.
fn word_changes(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let (old_words, new_words) = (words(old), words(new));
    let text = |line: &str, words: &[Range<usize>]| -> Vec<String> {
        words
            .iter()
            .map(|word| line[word.clone()].to_owned())
            .collect()
    };
    // Lines too long to align mark no words; both still show whole.
    let Some(matched) = lcs(&text(old, &old_words), &text(new, &new_words)) else {
        return (Vec::new(), Vec::new());
    };
    let unmatched = |line: &str, words: &[Range<usize>], kept: Vec<usize>| {
        let changed: Vec<&Range<usize>> = words
            .iter()
            .enumerate()
            .filter(|(index, _)| !kept.contains(index))
            .map(|(_, word)| word)
            .collect();
        // Whitespace is marked only on a line whose only change it is, so that change shows.
        let only_space = changed
            .iter()
            .all(|word| line[(*word).clone()].trim().is_empty());
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for word in changed {
            if !only_space && line[word.clone()].trim().is_empty() {
                continue;
            }
            match ranges.last_mut() {
                // Words changed together, with only whitespace between them, mark as one.
                Some(last) if line[last.end..word.start].trim().is_empty() => last.end = word.end,
                _ => ranges.push(word.clone()),
            }
        }
        ranges
    };
    (
        unmatched(
            old,
            &old_words,
            matched.iter().map(|&(old, _)| old).collect(),
        ),
        unmatched(
            new,
            &new_words,
            matched.iter().map(|&(_, new)| new).collect(),
        ),
    )
}

/// The words of `line` as byte ranges: runs of letters, digits and `_`, runs of whitespace,
/// and single other characters.
fn words(line: &str) -> Vec<Range<usize>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |letter: char| {
        if letter.is_alphanumeric() || letter == '_' {
            Class::Word
        } else if letter.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };
    let mut words: Vec<Range<usize>> = Vec::new();
    let mut last = None;
    for (at, letter) in line.char_indices() {
        let this = class(letter);
        let end = at + letter.len_utf8();
        match (words.last_mut(), &last) {
            (Some(word), Some(previous)) if *previous == this && this != Class::Other => {
                word.end = end;
            }
            _ => words.push(at..end),
        }
        last = Some(this);
    }
    words
}

#[cfg(test)]
mod tests {
    use review_core::LineEnding;

    use super::*;

    /// A byte range, as a word's.
    fn span(start: usize, end: usize) -> Range<usize> {
        start..end
    }

    fn diff(lines: &[(DiffLineKind, &str)]) -> SideDiff {
        SideDiff {
            changed: true,
            lines: lines
                .iter()
                .map(|&(kind, text)| DiffLine {
                    kind,
                    text: text.to_owned(),
                    ending: LineEnding::Lf,
                })
                .collect(),
        }
    }

    /// The shapes of `edits`' lines: `=` kept, `-` old and `+` new, each with its text.
    fn shapes(edits: &Edits<'_>) -> Vec<String> {
        edits
            .lines
            .iter()
            .map(|(line, _)| match line {
                EditLine::Kept(kept) => format!("= {}", kept.text),
                EditLine::Shifted(shifted) => format!("~ {}", shifted.text),
                EditLine::Old(old) => format!("- {}", old.text),
                EditLine::New(new) => format!("+ {}", new.text),
            })
            .collect()
    }

    use DiffLineKind::{New, Old, Same};

    /// A block re-indented as a whole is named re-indented by how far, and each line shows once
    /// in its new indentation, the indentation it gained marked.
    #[test]
    fn a_block_shifted_as_one_is_reindented() {
        let diff = diff(&[
            (Old, "fn a() {"),
            (Old, "    b();"),
            (Old, "}"),
            (New, "    fn a() {"),
            (New, "        b();"),
            (New, "    }"),
        ]);
        let edits = edits(&diff);
        assert!(edits.reindented);
        assert_eq!(edits.shift, 4);
        assert_eq!(
            shapes(&edits),
            ["~     fn a() {", "~         b();", "~     }"]
        );
        assert_eq!(edits.lines[0].1, [span(0, 4)]);
        assert_eq!(edits.lines[1].1, [span(4, 8)]);
    }

    /// An uneven re-indent may change what the code means, so its lines are edits of their
    /// leading whitespace.
    #[test]
    fn an_uneven_reindent_is_an_edit() {
        let diff = diff(&[
            (Old, "if a:"),
            (Old, "    b()"),
            (Old, "c()"),
            (New, "if a:"),
            (New, "    b()"),
            (New, "    c()"),
        ]);
        let edits = edits(&diff);
        assert!(!edits.reindented);
        assert_eq!(
            shapes(&edits),
            ["= if a:", "=     b()", "- c()", "+     c()"]
        );
        assert_eq!(edits.lines[3].1, [span(0, 4)]);
    }

    /// A line kept but for its ending shows old and new, so the change of ending stays visible,
    /// and a move with one is more than re-indented.
    #[test]
    fn a_changed_ending_is_not_kept() {
        let mut diff = diff(&[(Old, "    let a = 1;"), (New, "let a = 1;")]);
        diff.lines.push(DiffLine {
            kind: Old,
            text: "b();".to_owned(),
            ending: LineEnding::Lf,
        });
        diff.lines.push(DiffLine {
            kind: New,
            text: "b();".to_owned(),
            ending: LineEnding::CrLf,
        });
        let edits = edits(&diff);
        assert!(!edits.reindented);
        assert_eq!(
            shapes(&edits),
            ["-     let a = 1;", "+ let a = 1;", "- b();", "+ b();"]
        );
    }

    /// A run too long to align shows whole, unpaired, rather than costing unbounded memory.
    #[test]
    fn a_huge_run_shows_unaligned() {
        let old: Vec<(DiffLineKind, String)> = (0..1100)
            .map(|line| (Old, format!("line {line}")))
            .collect();
        let new: Vec<(DiffLineKind, String)> = (0..1100)
            .map(|line| (New, format!("  line {line}")))
            .collect();
        let lines: Vec<(DiffLineKind, &str)> = old
            .iter()
            .chain(&new)
            .map(|(kind, text)| (*kind, text.as_str()))
            .collect();
        let huge = diff(&lines);
        let edits = edits(&huge);
        assert!(!edits.reindented);
        assert_eq!(edits.lines.len(), 2200);
        assert!(lcs(&[0; 2000], &[0; 2000]).is_none());
    }

    /// An edited line pairs with its old version, and only the words that changed are marked.
    #[test]
    fn an_edit_marks_the_words_it_changed() {
        let diff = diff(&[
            (Same, "let x = 1;"),
            (Old, "out.push_str(&format!(\"{a}  {b}\"));"),
            (New, "out.push_str(&format!(\"{a} | {b}\"));"),
        ]);
        let edits = edits(&diff);
        assert!(!edits.reindented);
        // Its old line changed only whitespace, which shows marked.
        assert_eq!(edits.lines[1].1, [span(26, 28)]);
        assert_eq!(edits.lines[2].1, [span(27, 28)]);
        let (old, new) = word_changes("let width = 3;", "let height = 4;");
        assert_eq!(old, [span(4, 9), span(12, 13)]);
        assert_eq!(new, [span(4, 10), span(13, 14)]);
    }
}
