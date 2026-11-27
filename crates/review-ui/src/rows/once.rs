//! A chunk's lines as the unified layout shows them, from its edit script (ADR 6): where the
//! change is tokens inserted and a few short ones deleted, each new line shows once, the
//! tokens it inserted marked and the deleted ones struck out where they stood; everywhere else
//! the old lines show above the new ones, with the words that changed marked.
//!
//! Lines go together when kept code joins them, an old line with the new line its kept tokens
//! went to, and when one change or whitespace change touches them: each group is a stretch of
//! old lines and a stretch of new ones that no edit crosses. A line no kept code joins is a
//! removed or an added line of its own. In a chunk where a group shows both versions, unmarked
//! code reads as kept, so there such a line marks all its code.
//!
//! A group shows once only when the reviewer still sees everything that changed in it: it has
//! no whitespace change the proof could not show means nothing; every deletion in it is short,
//! on one old line that kept some of its code, with a place on one of the group's new lines,
//! and takes less of its line than the code the line kept; and every mark starts and ends
//! between whole characters, so a deleted accent never joins the letter before it. The marks
//! cover every byte a change replaced, whitespace inside a string too.

use std::ops::Range;

use review_core::analysis::{Chunk, EditKind};
use unicode_segmentation::UnicodeSegmentation;

use super::moves;

/// The longest deletion a line shows struck out, in characters.
const MAX_DELETION: usize = 32;

/// How one line of a chunk shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Shown {
    /// Removed line `line`, with the words it changed, in its content's bytes.
    Removed {
        line: usize,
        words: Vec<Range<usize>>,
    },
    /// Added line `line`, with the words it changed.
    Added {
        line: usize,
        words: Vec<Range<usize>>,
    },
    /// Added line `line` shown once: `text` is its content with the deleted words spliced in at
    /// `deleted`, and `inserted` the words it inserted, both in `text`'s bytes.
    Once {
        line: usize,
        text: String,
        inserted: Vec<Range<usize>>,
        deleted: Vec<Range<usize>>,
    },
}

/// The lines of `chunk` as they show, each new line once where it can when `once` allows it, and
/// otherwise its old lines above its new ones. In the unified layout, the lines between two
/// groups that show once show as one block, old above new, so a rewrite whose few kept words tie
/// its lines into small groups still reads as its old code and then its new code. Side by side
/// pairs the old and new lines of each group with each other, so there each group and the lines
/// before it show on their own.
pub(super) fn lines(chunk: &Chunk, once: bool) -> Vec<Shown> {
    let shape = Shape::of(chunk);
    let groups: Vec<(Group, Option<Vec<Shown>>)> =
        groups(chunk, &shape.old.lines, &shape.new.lines)
            .into_iter()
            .map(|group| {
                let lines = once.then(|| shape.once_lines(&group)).flatten();
                (group, lines)
            })
            .collect();
    // In a chunk where a group shows both versions, unmarked code reads as kept, so a line of
    // its own, changed whole, marks all its code.
    let alone = if groups.iter().any(|(_, lines)| lines.is_none()) {
        Mark::Whole
    } else {
        Mark::None
    };
    let mut shown = Vec::new();
    // The block's old lines and new lines so far.
    let mut block = (Vec::new(), Vec::new());
    let (mut old_at, mut new_at) = (0, 0);
    for (group, lines) in groups {
        block
            .0
            .extend((old_at..group.old.start).map(|line| shape.removed(line, alone)));
        block
            .1
            .extend((new_at..group.new.start).map(|line| shape.added(line, alone)));
        if lines.is_some() || !once {
            end_block(&mut shown, &mut block);
        }
        if let Some(lines) = lines {
            shown.extend(lines);
        } else {
            block.0.extend(
                group
                    .old
                    .clone()
                    .map(|line| shape.removed(line, Mark::Words)),
            );
            block
                .1
                .extend(group.new.clone().map(|line| shape.added(line, Mark::Words)));
            if !once {
                end_block(&mut shown, &mut block);
            }
        }
        (old_at, new_at) = (group.old.end, group.new.end);
    }
    block
        .0
        .extend((old_at..shape.old.lines.count()).map(|line| shape.removed(line, alone)));
    block
        .1
        .extend((new_at..shape.new.lines.count()).map(|line| shape.added(line, alone)));
    end_block(&mut shown, &mut block);
    shown
}

/// What a removed or an added line marks.
#[derive(Debug, Clone, Copy)]
enum Mark {
    /// Nothing.
    None,
    /// The words it changed, as a line of a group that shows both versions.
    Words,
    /// All its code, as a line of its own, changed whole, in a chunk with such a group.
    Whole,
}

/// Shows `block`, its old lines above its new ones, and starts a new one.
fn end_block(shown: &mut Vec<Shown>, block: &mut (Vec<Shown>, Vec<Shown>)) {
    shown.append(&mut block.0);
    shown.append(&mut block.1);
}

/// The lines of one side of a chunk.
struct Lines<'a> {
    text: &'a str,
    /// Where each line starts, then the text's end.
    starts: Vec<usize>,
}

impl<'a> Lines<'a> {
    fn new(text: &'a str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        if starts.last() != Some(&text.len()) {
            starts.push(text.len());
        }
        Self { text, starts }
    }

    fn count(&self) -> usize {
        self.starts.len() - 1
    }

    /// The line holding byte `at`; the end of the text belongs to the last line.
    fn of(&self, at: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= at)
            .saturating_sub(1)
            .min(self.count().saturating_sub(1))
    }

    /// The lines `range` touches, none when it is empty.
    fn span(&self, range: &Range<usize>) -> Range<usize> {
        if range.is_empty() {
            return 0..0;
        }
        self.of(range.start)..self.of(range.end - 1) + 1
    }

    /// The bytes of line `line` without its ending.
    fn content(&self, line: usize) -> Range<usize> {
        let start = self.starts.get(line).copied().unwrap_or(self.text.len());
        let end = self
            .starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        let text = self.text.get(start..end).unwrap_or_default();
        // As `split_line_ending` splits it: a `\r` is part of the ending only before `\n`.
        let content = match text.strip_suffix('\n') {
            Some(rest) => rest.strip_suffix('\r').unwrap_or(rest),
            None => text,
        };
        start..start + content.len()
    }

    /// The parts of `range` on each line it touches, without line endings.
    fn parts(&self, range: &Range<usize>) -> Vec<Word> {
        self.span(range)
            .map(|line| {
                let content = self.content(line);
                range.start.max(content.start)..range.end.min(content.end)
            })
            .filter(|part| !part.is_empty())
            .collect()
    }
}

/// One side of a chunk, worked out once for all its groups.
struct Side<'a> {
    lines: Lines<'a>,
    /// The words marked on each line, in the line's own bytes.
    marks: Vec<Vec<Range<usize>>>,
    /// The characters of code each line keeps: all but its whitespace and its marked words.
    kept: Vec<usize>,
    /// Whether a whitespace change touches each line.
    spaced: Vec<bool>,
}

impl<'a> Side<'a> {
    /// `lines` with the `words` marked on them and the whitespace changes over `spaces`.
    fn new(lines: Lines<'a>, words: &[Word], spaces: impl Iterator<Item = Range<usize>>) -> Self {
        let mut marks = vec![Vec::new(); lines.count()];
        // Each word lies on one line.
        for word in words {
            let line = lines.of(word.start);
            let start = lines.content(line).start;
            if let Some(marked) = marks.get_mut(line) {
                marked.push(word.start - start..word.end - start);
            }
        }
        let code = |text: &str| {
            text.chars()
                .filter(|letter| !letter.is_whitespace())
                .count()
        };
        // A line's marks do not overlap.
        let kept = marks
            .iter()
            .enumerate()
            .map(|(line, marked): (usize, &Vec<Range<usize>>)| {
                let content = lines.text.get(lines.content(line)).unwrap_or_default();
                let marked: usize = marked
                    .iter()
                    .map(|word| code(content.get(word.clone()).unwrap_or_default()))
                    .sum();
                code(content).saturating_sub(marked)
            })
            .collect();
        let mut whitespace = vec![false; lines.count()];
        for range in spaces {
            for line in lines.span(&range) {
                if let Some(touched) = whitespace.get_mut(line) {
                    *touched = true;
                }
            }
        }
        Self {
            lines,
            marks,
            kept,
            spaced: whitespace,
        }
    }

    fn marks(&self, line: usize) -> &[Range<usize>] {
        self.marks.get(line).map(Vec::as_slice).unwrap_or_default()
    }

    /// What line `line` marks, as `mark` asks. Changed words with only whitespace between them
    /// mark as one run, which reads better than a box per word and hides nothing on a line that
    /// shows whole; all its code is the code without the whitespace around it, as a changed word
    /// starts at its code.
    fn marked(&self, line: usize, mark: Mark) -> Vec<Range<usize>> {
        let content = self
            .lines
            .text
            .get(self.lines.content(line))
            .unwrap_or_default();
        match mark {
            Mark::None => Vec::new(),
            Mark::Words => {
                let mut runs: Vec<Range<usize>> = Vec::new();
                for word in self.marks(line) {
                    match runs.last_mut() {
                        Some(last)
                            if content
                                .get(last.end..word.start)
                                .is_some_and(|gap| gap.trim().is_empty()) =>
                        {
                            last.end = word.end;
                        }
                        _ => runs.push(word.clone()),
                    }
                }
                runs
            }
            Mark::Whole => {
                let start = content.len() - content.trim_start().len();
                let end = content.trim_end().len();
                if start < end {
                    std::iter::once(start..end).collect()
                } else {
                    Vec::new()
                }
            }
        }
    }

    fn kept(&self, line: usize) -> usize {
        self.kept.get(line).copied().unwrap_or_default()
    }

    fn spaced(&self, lines: Range<usize>) -> bool {
        self.spaced.get(lines).unwrap_or_default().contains(&true)
    }
}

/// A chunk's two sides and the deletions its changes made.
struct Shape<'a> {
    chunk: &'a Chunk,
    old: Side<'a>,
    new: Side<'a>,
    /// The deletions, in the order of their old bytes.
    deletions: Vec<Deletion>,
}

impl<'a> Shape<'a> {
    fn of(chunk: &'a Chunk) -> Self {
        let (old, new) = (Lines::new(&chunk.removed), Lines::new(&chunk.added));
        let marks = Marks::of(chunk, &old, &new);
        let spaces = chunk
            .edits
            .iter()
            .filter(|edit| edit.kind == EditKind::Whitespace);
        Self {
            chunk,
            old: Side::new(
                old,
                &marks.old,
                spaces.clone().map(|edit| edit.removed.clone()),
            ),
            new: Side::new(new, &marks.new, spaces.map(|edit| edit.added.clone())),
            deletions: marks.deletions,
        }
    }

    /// Removed line `line`, marked as `mark` asks.
    fn removed(&self, line: usize, mark: Mark) -> Shown {
        Shown::Removed {
            line,
            words: self.old.marked(line, mark),
        }
    }

    /// Added line `line`, marked as `mark` asks.
    fn added(&self, line: usize, mark: Mark) -> Shown {
        Shown::Added {
            line,
            words: self.new.marked(line, mark),
        }
    }

    /// The new lines of `group` shown once, or `None` when the group must show both versions.
    fn once_lines(&self, group: &Group) -> Option<Vec<Shown>> {
        let (old, new) = (&self.old, &self.new);
        if old.spaced(group.old.clone()) || new.spaced(group.new.clone()) {
            return None;
        }
        let first = self
            .deletions
            .partition_point(|deletion| old.lines.of(deletion.word.start) < group.old.start);
        let mut deletions: Vec<(usize, &Deletion)> = Vec::new();
        let mut deleted_on = vec![0_usize; group.new.len()];
        for deletion in self
            .deletions
            .get(first..)
            .unwrap_or_default()
            .iter()
            .take_while(|deletion| old.lines.of(deletion.word.start) < group.old.end)
        {
            let text = self
                .chunk
                .removed
                .get(deletion.word.clone())
                .unwrap_or_default();
            let new_line = new.lines.of(deletion.at);
            let chars = text.chars().count();
            let placeable = !deletion.across
                && old.kept(old.lines.of(deletion.word.start)) > 0
                && group.new.contains(&new_line)
                && chars <= MAX_DELETION;
            if !placeable {
                return None;
            }
            if let Some(total) = deleted_on.get_mut(new_line - group.new.start) {
                *total += chars;
            }
            deletions.push((new_line, deletion));
        }
        if group
            .new
            .clone()
            .zip(&deleted_on)
            .any(|(line, &deleted)| deleted > new.kept(line))
        {
            return None;
        }
        let mut shown = Vec::with_capacity(group.new.len());
        for line in group.new.clone() {
            let ghosts: Vec<(usize, &str)> = deletions
                .iter()
                .filter(|(new_line, _)| *new_line == line)
                .map(|(_, deletion)| {
                    (
                        deletion.at - new.lines.content(line).start,
                        self.chunk
                            .removed
                            .get(deletion.word.clone())
                            .unwrap_or_default(),
                    )
                })
                .collect();
            // A line that keeps no code and deletes nothing is a new line, as an added line
            // shows it.
            if new.kept(line) == 0 && ghosts.is_empty() {
                shown.push(self.added(line, Mark::None));
                continue;
            }
            let content = self
                .chunk
                .added
                .get(new.lines.content(line))
                .unwrap_or_default();
            let once = splice(line, content, new.marks(line), &ghosts);
            if !whole_characters(&once) {
                return None;
            }
            shown.push(once);
        }
        Some(shown)
    }
}

/// A stretch of old lines and the stretch of new lines that go together.
#[derive(Debug, Clone)]
struct Group {
    old: Range<usize>,
    new: Range<usize>,
}

/// The old lines and the new lines a set of lines spans, where it has any.
type Spans = (Option<Range<usize>>, Option<Range<usize>>);

/// The groups of `chunk`'s lines that show together, in order, none crossing another.
fn groups(chunk: &Chunk, old: &Lines<'_>, new: &Lines<'_>) -> Vec<Group> {
    // One node per old line, then one per new line.
    let mut sets = Sets::new(old.count() + new.count());
    let offset = old.count();
    for edit in &chunk.edits {
        match edit.kind {
            // A kept line's code joins its old line and its new line; the same text breaks
            // lines in the same places on both sides.
            EditKind::Same => {
                let text = chunk.removed.get(edit.removed.clone()).unwrap_or_default();
                let mut at = 0;
                for piece in text.split_inclusive('\n') {
                    if !piece.trim().is_empty() {
                        sets.union(
                            old.of(edit.removed.start + at),
                            offset + new.of(edit.added.start + at),
                        );
                    }
                    at += piece.len();
                }
            }
            EditKind::Changed | EditKind::Whitespace => {
                let lines: Vec<usize> = old
                    .span(&edit.removed)
                    .chain(new.span(&edit.added).map(|line| offset + line))
                    .collect();
                for pair in lines.windows(2) {
                    if let &[first, second] = pair {
                        sets.union(first, second);
                    }
                }
            }
            EditKind::Layout => {}
        }
    }
    // The old and the new lines each set spans, by its root.
    let mut spans: Vec<Spans> = vec![(None, None); old.count() + new.count()];
    for node in 0..old.count() + new.count() {
        let root = sets.find(node);
        let Some((old_span, new_span)) = spans.get_mut(root) else {
            continue;
        };
        let (side, line) = if node < offset {
            (old_span, node)
        } else {
            (new_span, node - offset)
        };
        *side = Some(side.take().map_or(line..line + 1, |span| {
            span.start.min(line)..span.end.max(line + 1)
        }));
    }
    let mut groups: Vec<Group> = spans
        .into_iter()
        .filter_map(|(old, new)| {
            Some(Group {
                old: old?,
                new: new?,
            })
        })
        .collect();
    groups.sort_by_key(|group| (group.old.start, group.new.start));
    // Groups that overlap or cross on either side become one, so no edit or line falls between
    // two of them.
    let mut merged: Vec<Group> = Vec::new();
    for group in groups {
        match merged.last_mut() {
            Some(last) if group.old.start < last.old.end || group.new.start < last.new.end => {
                last.old = last.old.start.min(group.old.start)..last.old.end.max(group.old.end);
                last.new = last.new.start.min(group.new.start)..last.new.end.max(group.new.end);
            }
            _ => merged.push(group),
        }
    }
    merged
}

/// Disjoint sets of lines, joined as kept code and edits tie them.
struct Sets {
    parents: Vec<usize>,
}

impl Sets {
    fn new(count: usize) -> Self {
        Self {
            parents: (0..count).collect(),
        }
    }

    fn find(&mut self, node: usize) -> usize {
        let mut root = node;
        while let Some(&parent) = self.parents.get(root)
            && parent != root
        {
            root = parent;
        }
        let mut at = node;
        while let Some(parent) = self.parents.get_mut(at)
            && *parent != root
        {
            at = std::mem::replace(parent, root);
        }
        root
    }

    fn union(&mut self, first: usize, second: usize) {
        let (first, second) = (self.find(first), self.find(second));
        if let Some(parent) = self.parents.get_mut(first.max(second)) {
            *parent = first.min(second);
        }
    }
}

/// A word to mark on one side: its bytes in that side's text.
type Word = Range<usize>;

/// A deletion shown struck out: its bytes in the old text, where it stands in the new text,
/// and whether its edit deleted code on more than one line, which no line can show.
#[derive(Debug, Clone)]
struct Deletion {
    word: Word,
    at: usize,
    across: bool,
}

/// The words the edit script changed on each side, and where each deleted word stands in the
/// new text.
#[derive(Default)]
struct Marks {
    old: Vec<Word>,
    new: Vec<Word>,
    deletions: Vec<Deletion>,
}

impl Marks {
    fn of(chunk: &Chunk, old: &Lines<'_>, new: &Lines<'_>) -> Self {
        let mut marks = Self::default();
        for edit in chunk
            .edits
            .iter()
            .filter(|edit| edit.kind == EditKind::Changed)
        {
            let old_words = old.parts(&edit.removed);
            let new_words = new.parts(&edit.added);
            // One line's code replaced by another's marks only what differs, of a word what it
            // gained or lost: `ThreadItem` to `ThreadItemVariant` adds `Variant`, and a comment
            // that gained a word marks that word.
            if let ([old_word], [new_word]) = (old_words.as_slice(), new_words.as_slice()) {
                let PairMarks { deleted, inserted } = pair_marks(
                    chunk.removed.get(old_word.clone()).unwrap_or_default(),
                    chunk.added.get(new_word.clone()).unwrap_or_default(),
                );
                for (part, at) in deleted {
                    let part = old_word.start + part.start..old_word.start + part.end;
                    marks.deletions.push(Deletion {
                        word: part.clone(),
                        at: new_word.start + at,
                        across: false,
                    });
                    marks.old.push(part);
                }
                marks.new.extend(
                    inserted
                        .into_iter()
                        .map(|part| new_word.start + part.start..new_word.start + part.end),
                );
                continue;
            }
            let at = new_words
                .first()
                .map_or(edit.added.start, |word| word.start);
            let across = old_words.len() > 1;
            for word in old_words {
                marks.deletions.push(Deletion {
                    word: word.clone(),
                    at,
                    across,
                });
                marks.old.push(word);
            }
            marks.new.extend(new_words);
        }
        marks
    }
}

/// What a line's code replaced by another's marks, in the bytes of each.
#[derive(Debug, Default)]
struct PairMarks {
    /// The parts of the old code it deleted, each with where it stands in the new code.
    deleted: Vec<(Range<usize>, usize)>,
    /// The parts of the new code it inserted.
    inserted: Vec<Range<usize>>,
}

/// What a line's code `old` replaced by `new` marks. A word that is part of the other marks
/// only what the longer adds; other text marks the words that differ; and text too long to
/// align marks whole.
fn pair_marks(old: &str, new: &str) -> PairMarks {
    const SHORTEST: usize = 3;
    let around = |longer: &str, at: usize, len: usize| -> Vec<Range<usize>> {
        [0..at, at + len..longer.len()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect()
    };
    if old.len() >= SHORTEST
        && let Some(at) = new.find(old)
    {
        return PairMarks {
            deleted: Vec::new(),
            inserted: around(new, at, old.len()),
        };
    }
    if new.len() >= SHORTEST
        && let Some(at) = old.find(new)
    {
        // A part before the kept text stands before it, one after it after it.
        let deleted = around(old, at, new.len())
            .into_iter()
            .map(|part| {
                let stands = if part.start == 0 { 0 } else { new.len() };
                (part, stands)
            })
            .collect();
        return PairMarks {
            deleted,
            inserted: Vec::new(),
        };
    }
    word_marks(old, new).unwrap_or_else(|| PairMarks {
        deleted: std::iter::once((0..old.len(), 0)).collect(),
        inserted: std::iter::once(0..new.len()).collect(),
    })
}

/// The words `old` and `new` do not share, whitespace too, aligned word by word: each run of
/// deleted words of `old` with where it stands in `new`, before what took its place, and each
/// run of inserted words of `new`; `None` when they share no word of letters or digits, whose
/// marks would split code into characters, or are too long to align.
fn word_marks(old: &str, new: &str) -> Option<PairMarks> {
    let (old_words, new_words) = (moves::words(old), moves::words(new));
    let old_texts: Vec<&str> = old_words
        .iter()
        .map(|word| old.get(word.clone()).unwrap_or_default())
        .collect();
    let new_texts: Vec<&str> = new_words
        .iter()
        .map(|word| new.get(word.clone()).unwrap_or_default())
        .collect();
    let matched = moves::lcs(&old_texts, &new_texts)?;
    if !matched.iter().any(|&(at, _)| {
        old_texts
            .get(at)
            .is_some_and(|word| word.chars().any(char::is_alphanumeric))
    }) {
        return None;
    }
    let mut marks = PairMarks::default();
    let (mut i, mut j) = (0, 0);
    for (old_end, new_end) in matched
        .into_iter()
        .chain(std::iter::once((old_words.len(), new_words.len())))
    {
        let stands = new_words.get(j).map_or(new.len(), |word| word.start);
        for word in old_words.get(i..old_end).unwrap_or_default() {
            match marks.deleted.last_mut() {
                // Words deleted together show as one.
                Some((last, _)) if last.end == word.start => last.end = word.end,
                _ => marks.deleted.push((word.clone(), stands)),
            }
        }
        for word in new_words.get(j..new_end).unwrap_or_default() {
            match marks.inserted.last_mut() {
                Some(last) if last.end == word.start => last.end = word.end,
                _ => marks.inserted.push(word.clone()),
            }
        }
        (i, j) = (old_end + 1, new_end + 1);
    }
    Some(marks)
}

/// New line `line`, `content`, with the `ghosts` (where each stands, and its text) spliced in
/// before whatever stands there, and its `inserted` words moved past them. No ghost stands
/// inside an inserted word: a deletion stands where the words that took its place start.
fn splice(
    line: usize,
    content: &str,
    inserted: &[Range<usize>],
    ghosts: &[(usize, &str)],
) -> Shown {
    let mut text = String::with_capacity(content.len());
    let mut deleted = Vec::new();
    let mut at = 0;
    for (ghost_at, ghost) in ghosts {
        text.push_str(content.get(at..*ghost_at).unwrap_or_default());
        deleted.push(text.len()..text.len() + ghost.len());
        text.push_str(ghost);
        at = *ghost_at;
    }
    text.push_str(content.get(at..).unwrap_or_default());
    let inserted = inserted
        .iter()
        .map(|word| {
            let shift: usize = ghosts
                .iter()
                .filter(|(ghost_at, _)| *ghost_at <= word.start)
                .map(|(_, ghost)| ghost.len())
                .sum();
            word.start + shift..word.end + shift
        })
        .collect();
    Shown::Once {
        line,
        text,
        inserted,
        deleted,
    }
}

/// Whether every mark of a line shown once starts and ends between whole characters as a
/// screen draws them, so no mark splits a letter from the accent or joiner that completes it.
fn whole_characters(shown: &Shown) -> bool {
    let Shown::Once {
        text,
        inserted,
        deleted,
        ..
    } = shown
    else {
        return true;
    };
    let mut boundaries: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
    boundaries.push(text.len());
    inserted.iter().chain(deleted).all(|mark| {
        [mark.start, mark.end]
            .iter()
            .all(|at| boundaries.binary_search(at).is_ok())
    })
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use review_core::analysis::{Edit, LineRange};

    use super::*;

    /// A chunk whose edit script is `steps`, each a kind with its old and its new text.
    fn chunk(steps: &[(EditKind, &str, &str)]) -> Chunk {
        let (mut removed, mut added) = (String::new(), String::new());
        let mut edits = Vec::new();
        for &(kind, old, new) in steps {
            edits.push(Edit {
                kind,
                removed: removed.len()..removed.len() + old.len(),
                added: added.len()..added.len() + new.len(),
            });
            removed.push_str(old);
            added.push_str(new);
        }
        Chunk {
            old: LineRange {
                start: 0,
                len: removed.lines().count(),
            },
            new: LineRange {
                start: 0,
                len: added.lines().count(),
            },
            removed,
            added,
            edits,
            whitespace_only: false,
            context: String::new(),
        }
    }

    use EditKind::{Changed, Layout, Same, Whitespace};

    /// Each line as it shows, with its marks spelled out: `~` once, its inserted words in `[]`
    /// and its deleted ones in `{}`; `-` removed and `+` added, their words in `[]`.
    fn shapes(chunk: &Chunk) -> Vec<String> {
        let line = |text: &str, line: usize| text.lines().nth(line).unwrap_or_default().to_owned();
        let marked = |text: &str, marks: &[(Range<usize>, char, char)]| {
            let mut out = String::new();
            for (at, letter) in text.char_indices() {
                for (range, open, _) in marks {
                    if range.start == at {
                        out.push(*open);
                    }
                }
                out.push(letter);
                for (range, _, close) in marks {
                    if range.end == at + letter.len_utf8() {
                        out.push(*close);
                    }
                }
            }
            out
        };
        lines(chunk, true)
            .into_iter()
            .map(|shown| match shown {
                Shown::Removed { line: at, words } => {
                    let marks: Vec<_> = words.into_iter().map(|word| (word, '[', ']')).collect();
                    format!("- {}", marked(&line(&chunk.removed, at), &marks))
                }
                Shown::Added { line: at, words } => {
                    let marks: Vec<_> = words.into_iter().map(|word| (word, '[', ']')).collect();
                    format!("+ {}", marked(&line(&chunk.added, at), &marks))
                }
                Shown::Once {
                    text,
                    inserted,
                    deleted,
                    ..
                } => {
                    let marks: Vec<_> = inserted
                        .into_iter()
                        .map(|word| (word, '[', ']'))
                        .chain(deleted.into_iter().map(|word| (word, '{', '}')))
                        .collect();
                    format!("~ {}", marked(&text, &marks))
                }
            })
            .collect()
    }

    /// Checks that the rows of `chunk` read back as its old version and its new one, each
    /// without the layout the proof showed means nothing: a line shown once gives the old
    /// version all but what it inserted, and the new version all but what it deleted. Returns how
    /// many lines shown once inserted and deleted something.
    fn read_back(chunk: &Chunk) -> (usize, usize) {
        let layout = |side: fn(&Edit) -> &Range<usize>| -> Vec<Range<usize>> {
            chunk
                .edits
                .iter()
                .filter(|edit| edit.kind == Layout)
                .map(|edit| side(edit).clone())
                .collect()
        };
        let (old_layout, new_layout) = (layout(|edit| &edit.removed), layout(|edit| &edit.added));
        let in_any =
            |ranges: &[Range<usize>], at: usize| ranges.iter().any(|range| range.contains(&at));
        // The bytes of `text` in `bytes`, but its layout.
        let read = |out: &mut String, text: &str, bytes: Range<usize>, layout: &[Range<usize>]| {
            for (at, letter) in text.get(bytes.clone()).unwrap_or_default().char_indices() {
                if !in_any(layout, bytes.start + at) {
                    out.push(letter);
                }
            }
        };
        let (old_side, new_side) = (Lines::new(&chunk.removed), Lines::new(&chunk.added));
        let line_bytes =
            |lines: &Lines<'_>, line: usize| lines.starts[line]..lines.starts[line + 1];
        let (mut old, mut new) = (String::new(), String::new());
        let (mut inserting, mut deleting) = (0, 0);
        for shown in lines(chunk, true) {
            match shown {
                Shown::Removed { line, .. } => {
                    read(
                        &mut old,
                        &chunk.removed,
                        line_bytes(&old_side, line),
                        &old_layout,
                    );
                }
                Shown::Added { line, .. } => {
                    read(
                        &mut new,
                        &chunk.added,
                        line_bytes(&new_side, line),
                        &new_layout,
                    );
                }
                Shown::Once {
                    line,
                    text,
                    inserted,
                    deleted,
                } => {
                    inserting += usize::from(!inserted.is_empty());
                    deleting += usize::from(!deleted.is_empty());
                    // Each byte of `text` but deleted text is the next byte of the line.
                    let mut at = new_side.content(line).start;
                    for (shown_at, letter) in text.char_indices() {
                        if in_any(&deleted, shown_at) {
                            old.push(letter);
                            continue;
                        }
                        if !in_any(&new_layout, at) {
                            new.push(letter);
                            if !in_any(&inserted, shown_at) {
                                old.push(letter);
                            }
                        }
                        at += letter.len_utf8();
                    }
                    // The line's ending, the same on both sides unless it is layout.
                    let ending = new_side.content(line).end..new_side.starts[line + 1];
                    read(&mut new, &chunk.added, ending.clone(), &new_layout);
                    read(&mut old, &chunk.added, ending, &new_layout);
                }
            }
        }
        let mut expected = (String::new(), String::new());
        read(
            &mut expected.0,
            &chunk.removed,
            0..chunk.removed.len(),
            &old_layout,
        );
        read(
            &mut expected.1,
            &chunk.added,
            0..chunk.added.len(),
            &new_layout,
        );
        assert_eq!((old, new), expected, "{chunk:?}");
        (inserting, deleting)
    }

    /// Every chunk of every revision of the demo shows all it changed, byte for byte, as
    /// [`read_back`] reads its rows, and a whitespace change always shows both versions.
    #[test]
    fn every_change_of_the_demo_shows() {
        // How many lines shown once inserted and deleted something, so the check checks.
        let (mut inserting, mut deleting) = (0, 0);
        for revision in 1..=3 {
            let (_dir, review) = super::super::tests::demo_review(revision);
            let history = review.history().unwrap();
            let view = history.current_view().unwrap();
            for chunk in view.analysis().units.iter().filter_map(|unit| unit.chunk()) {
                let (inserted, deleted) = read_back(chunk);
                inserting += inserted;
                deleting += deleted;
                let old_side = Lines::new(&chunk.removed);
                let removed: Vec<usize> = lines(chunk, true)
                    .into_iter()
                    .filter_map(|shown| match shown {
                        Shown::Removed { line, .. } => Some(line),
                        _ => None,
                    })
                    .collect();
                for edit in chunk.edits.iter().filter(|edit| edit.kind == Whitespace) {
                    assert!(
                        old_side
                            .span(&edit.removed)
                            .all(|line| removed.contains(&line)),
                        "{chunk:?}"
                    );
                }
            }
        }
        assert!(inserting > 0 && deleting > 0, "{inserting} {deleting}");
    }

    /// Code wrapped in a call and rewrapped shows once, only what the wrapping inserted marked,
    /// and of a renamed word only what it gained.
    #[test]
    fn a_wrapped_call_shows_once() {
        let chunk = chunk(&[
            (Same, "    ", "    "),
            (Changed, "ThreadItem", "item(ThreadItemVariant"),
            (Same, "::Reasoning(Box::new(", "::Reasoning(Box::new("),
            (Layout, "", "\n        "),
            (Same, "ReasoningThreadItem {", "ReasoningThreadItem {"),
            (Layout, "\n        ", "\n            "),
            (Same, "id: id,", "id: id,"),
            (Layout, "\n    ", "\n        "),
            (Same, "}", "}"),
            (Changed, "", ","),
            (Layout, "", "\n    "),
            (Changed, "", ")"),
            (Same, "))\n", "))\n"),
        ]);
        assert_eq!(
            shapes(&chunk),
            [
                "~     [item(]ThreadItem[Variant]::Reasoning(Box::new(",
                "~         ReasoningThreadItem {",
                "~             id: id,",
                "~         }[,]",
                "~     [)]))",
            ]
        );
    }

    /// A short deletion shows struck out where it stood, before what took its place.
    #[test]
    fn a_short_deletion_shows_in_place() {
        let chunk = chunk(&[
            (
                Same,
                "let shift = self.attempt.min(",
                "let shift = self.attempt.min(",
            ),
            (Changed, "32", "20"),
            (Same, ");\n", ");\n"),
        ]);
        assert_eq!(
            shapes(&chunk),
            ["~ let shift = self.attempt.min({32}[20]);"]
        );
    }

    /// A comment, one token, marks only the words that changed: one it gained, and, where the
    /// two share no word, all that changed.
    #[test]
    fn a_reworded_comment_marks_its_words() {
        let gained = chunk(&[
            (
                Changed,
                "/// The queue was full.",
                "/// The ready queue was full.",
            ),
            (Same, "\n", "\n"),
        ]);
        assert_eq!(shapes(&gained), ["~ /// The [ready ]queue was full."]);
        let reworded = chunk(&[
            (
                Same,
                "/* Drives its C interface",
                "/* Drives its C interface",
            ),
            (Changed, ", as a", ": the FFI"),
            (Same, " smoke test. */\n", " smoke test. */\n"),
        ]);
        assert_eq!(
            shapes(&reworded),
            ["~ /* Drives its C interface{, as a}[: the FFI] smoke test. */"]
        );
    }

    /// A string whose only change is its spacing marks that spacing, and an accent added to or
    /// deleted from a letter, which no mark can cover apart from the letter, shows both versions.
    #[test]
    fn every_changed_byte_shows() {
        let spaced = chunk(&[
            (Same, "say(\"", "say(\""),
            (Changed, "hello world", "hello  world"),
            (Same, "\");\n", "\");\n"),
        ]);
        assert_eq!(shapes(&spaced), ["~ say(\"hello{ }[  ]world\");"]);
        for (old, new) in [("Cafe\u{301}", "Cafe"), ("Cafe", "Cafe\u{301}")] {
            let accent = chunk(&[
                (Same, "say(\"", "say(\""),
                (Changed, old, new),
                (Same, "\");\n", "\");\n"),
            ]);
            let shown = shapes(&accent);
            assert!(
                shown.len() == 2 && shown[0].starts_with('-') && shown[1].starts_with('+'),
                "{shown:?}"
            );
        }
    }

    /// Groups that show both versions, and the lines between them, show as one block, old above
    /// new, as a rewrite reads best; a line deleted whole among them marks all its code, so none
    /// of it reads as kept.
    #[test]
    fn a_rewrite_shows_its_old_code_above_its_new_code() {
        let rewrite = chunk(&[
            (Same, "x ", "x "),
            (Changed, "verylongdeletedtext", "short"),
            (Same, "\n", "\n"),
            (Changed, "    removed line\n", ""),
            (Same, "y ", "y "),
            (Changed, "anotherlongdeletion", "tiny"),
            (Same, "\n", "\n"),
        ]);
        assert_eq!(
            shapes(&rewrite),
            [
                "- x [verylongdeletedtext]",
                "-     [removed line]",
                "- y [anotherlongdeletion]",
                "+ x [short]",
                "+ y [tiny]"
            ]
        );
    }

    /// Side by side keeps each group's lines together, after the lines before it, so a new
    /// comment above a changed line faces padding and the two versions of the line face each
    /// other.
    #[test]
    fn side_by_side_pairs_each_group_on_its_own() {
        let commented = chunk(&[
            (Changed, "", "    // Nothing to spread.\n"),
            (Same, "    if ceiling ", "    if ceiling "),
            (Changed, "== 0", "<= 1"),
            (Same, " {\n", " {\n"),
        ]);
        let kinds: Vec<(&str, usize)> = lines(&commented, false)
            .iter()
            .map(|shown| match shown {
                Shown::Removed { line, .. } => ("-", *line),
                Shown::Added { line, .. } => ("+", *line),
                Shown::Once { line, .. } => ("~", *line),
            })
            .collect();
        assert_eq!(kinds, [("+", 0), ("-", 0), ("+", 1)]);
    }

    /// A line that would delete more than it keeps shows its old version above its new one.
    #[test]
    fn a_heavy_replacement_shows_both_versions() {
        let chunk = chunk(&[
            (Same, "let ", "let "),
            (Changed, "width", "height"),
            (Same, " = ", " = "),
            (Changed, "3", "4"),
            (Same, ";\n", ";\n"),
        ]);
        assert_eq!(
            shapes(&chunk),
            ["- let [width] = [3];", "+ let [height] = [4];"]
        );
    }

    /// On a line that shows whole, changed words with only spaces between them mark as one run.
    #[test]
    fn changed_words_mark_as_one_run() {
        let reworded = chunk(&[
            (
                Changed,
                "/// Writes the ledger to the legacy file.",
                "/// Returns the accounts that differ.",
            ),
            (Same, "\n", "\n"),
        ]);
        assert_eq!(
            shapes(&reworded),
            [
                "- /// [Writes] the [ledger to the legacy file].",
                "+ /// [Returns] the [accounts that differ]."
            ]
        );
    }

    /// A whitespace change the proof cannot show means nothing, such as a dedent in Python,
    /// shows both versions, even beside a rename.
    #[test]
    fn a_whitespace_change_shows_both_versions() {
        let chunk = chunk(&[
            (Whitespace, "    ", ""),
            (Changed, "send", "deliver"),
            (Same, "(payload)\n", "(payload)\n"),
        ]);
        assert_eq!(
            shapes(&chunk),
            ["-     [send](payload)", "+ [deliver](payload)"]
        );
    }

    /// A deletion across lines shows both versions; a line deleted whole is a removed line of
    /// its own, beside lines that show once.
    #[test]
    fn deleted_lines_show_removed() {
        let second = indoc! {"
            ,
                 second"};
        let across = chunk(&[
            (Same, "call(first", "call(first"),
            (Changed, second, ""),
            (Same, ");\n", ");\n"),
        ]);
        assert_eq!(
            shapes(&across),
            ["- call(first[,]", "- [     second]);", "+ call(first);"]
        );
        let whole = chunk(&[
            (Same, "a(", "a("),
            (Changed, "", "b"),
            (Same, ");\n", ");\n"),
            (Changed, "gone();", ""),
            (Layout, "\n", ""),
            (Same, "c();\n", "c();\n"),
        ]);
        assert_eq!(shapes(&whole), ["~ a([b]);", "- gone();", "~ c();"]);
    }
}
