//! The edit script of a chunk: how its removed text becomes its added text, token by token.
//!
//! Tokens are aligned by a diff that ignores whitespace. Whitespace is judged where it stands:
//! - A gap both sides have, before a token both keep or before a run of replaced tokens, is the
//!   same text, or layout when the language's formatting proof gives the two gaps the same key
//!   and no line ending changes, or else a whitespace change of its own.
//! - Where one side inserts or deletes tokens, it has one gap more than the other. A run of
//!   whole lines is one change from its first line's indentation through its last line's break,
//!   when the line break before it and the indentation after it make up the other side's gap.
//!   Otherwise the other side's gap pairs with the one before the run when they are the same
//!   text, and else with the one after it.
//! - A gap only one side has, beside or inside a run of changed tokens, belongs to the change
//!   unless it breaks a line. A line break there is layout where the proof shows that it means
//!   nothing, and a whitespace change otherwise, as between Python statements. Only the new
//!   side's gap shows: one only the old side had shows as nothing, the tokens around it
//!   touching, so it is layout only where they may touch, as next to a bracket.

use std::ops::Range;

use super::{Edit, EditKind};
use crate::syntax::{breaks_free, may_vanish};
use crate::text::diff_tokens;

/// One side of a chunk: its text, its tokens, and for each token the formatting proof's key of
/// the gap before it, where the proof has one.
pub(super) struct ScriptSide<'a> {
    pub text: &'a str,
    pub tokens: &'a [Range<usize>],
    pub keys: &'a [Option<&'a [u8]>],
}

impl ScriptSide<'_> {
    fn token(&self, index: usize) -> &str {
        self.tokens
            .get(index)
            .and_then(|token| self.text.get(token.clone()))
            .unwrap_or_default()
    }

    /// Where token `index` starts, or the end of the text past the last token.
    fn start(&self, index: usize) -> usize {
        self.tokens
            .get(index)
            .map_or(self.text.len(), |token| token.start)
    }

    /// Where token `index` ends.
    fn end(&self, index: usize) -> usize {
        self.tokens.get(index).map_or(0, |token| token.end)
    }

    fn key(&self, index: usize) -> Option<&[u8]> {
        self.keys.get(index).copied().flatten()
    }

    fn get(&self, range: &Range<usize>) -> &str {
        self.text.get(range.clone()).unwrap_or_default()
    }

    /// The tokens of `run` in pieces split where a line breaks between two of them: each
    /// piece's bytes and its first token.
    fn pieces(&self, run: &Range<usize>) -> Vec<(Range<usize>, usize)> {
        let mut pieces: Vec<(Range<usize>, usize)> = Vec::new();
        for index in run.clone() {
            match pieces.last_mut() {
                Some((piece, _)) if !self.get(&(piece.end..self.start(index))).contains('\n') => {
                    piece.end = self.end(index);
                }
                _ => pieces.push((self.start(index)..self.end(index), index)),
            }
        }
        pieces
    }
}

/// A side of the chunk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Old,
    New,
}

/// The edit script from `old` to `new`.
pub(super) fn edit_script(old: &ScriptSide<'_>, new: &ScriptSide<'_>) -> Vec<Edit> {
    // Lines only added or only removed are one change, whitespace and all.
    if old.text.is_empty() || new.text.is_empty() {
        return vec![Edit {
            kind: EditKind::Changed,
            removed: 0..old.text.len(),
            added: 0..new.text.len(),
        }];
    }
    let before: Vec<&str> = (0..old.tokens.len())
        .map(|index| old.token(index))
        .collect();
    let after: Vec<&str> = (0..new.tokens.len())
        .map(|index| new.token(index))
        .collect();
    let mut script = Script {
        old,
        new,
        edits: Vec::new(),
        old_at: 0,
        new_at: 0,
    };
    let (mut i, mut j) = (0, 0);
    for hunk in diff_tokens(&before, &after) {
        while i < hunk.before.start && j < hunk.after.start {
            script.keep(i, j);
            (i, j) = (i + 1, j + 1);
        }
        script.run(&hunk.before, &hunk.after);
        (i, j) = (hunk.before.end, hunk.after.end);
    }
    while i < before.len() && j < after.len() {
        script.keep(i, j);
        (i, j) = (i + 1, j + 1);
    }
    // What follows the last token, a line ending above all, is never layout: a changed ending
    // or a missing final newline shows.
    let rest = (script.old_at..old.text.len(), script.new_at..new.text.len());
    let kind = if old.get(&rest.0) == new.get(&rest.1) {
        EditKind::Same
    } else {
        EditKind::Whitespace
    };
    script.push(kind, rest.0, rest.1);
    script.edits
}

/// An edit script being built: the steps so far, merged where they are of one kind, and the
/// next byte of each side.
struct Script<'a> {
    old: &'a ScriptSide<'a>,
    new: &'a ScriptSide<'a>,
    edits: Vec<Edit>,
    old_at: usize,
    new_at: usize,
}

impl<'a> Script<'a> {
    fn side(&self, side: Side) -> &'a ScriptSide<'a> {
        match side {
            Side::Old => self.old,
            Side::New => self.new,
        }
    }

    fn at(&self, side: Side) -> usize {
        match side {
            Side::Old => self.old_at,
            Side::New => self.new_at,
        }
    }

    fn push(&mut self, kind: EditKind, removed: Range<usize>, added: Range<usize>) {
        if removed.is_empty() && added.is_empty() {
            return;
        }
        (self.old_at, self.new_at) = (removed.end, added.end);
        match self.edits.last_mut() {
            Some(last) if last.kind == kind => {
                last.removed.end = removed.end;
                last.added.end = added.end;
            }
            _ => self.edits.push(Edit {
                kind,
                removed,
                added,
            }),
        }
    }

    /// A step of `kind` over the bytes of `side` up to `end`, and nothing of the other side.
    fn push_one(&mut self, side: Side, kind: EditKind, end: usize) {
        let range = self.at(side)..end;
        match side {
            Side::Old => self.push(kind, range, self.new_at..self.new_at),
            Side::New => self.push(kind, self.old_at..self.old_at, range),
        }
    }

    /// Token `i` of the old side and token `j` of the new one, the same text, with the gap
    /// before each.
    fn keep(&mut self, i: usize, j: usize) {
        let (old, new) = (self.old, self.new);
        self.both_gap(old.start(i), new.start(j), (old.key(i), new.key(j)));
        self.push(
            EditKind::Same,
            old.start(i)..old.end(i),
            new.start(j)..new.end(j),
        );
    }

    /// The gap both sides have, up to `old_end` and `new_end`: the same text, layout when the
    /// proof's `keys` of the two are equal and no line ending changes, or a whitespace change.
    fn both_gap(&mut self, old_end: usize, new_end: usize, keys: (Option<&[u8]>, Option<&[u8]>)) {
        let (removed, added) = (self.old_at..old_end, self.new_at..new_end);
        let (old_gap, new_gap) = (self.old.get(&removed), self.new.get(&added));
        let kind = if old_gap == new_gap {
            EditKind::Same
        } else if !old_gap.trim().is_empty() || !new_gap.trim().is_empty() {
            // Bytes no token holds are never whitespace to judge.
            EditKind::Changed
        } else if matches!(keys, (Some(old_key), Some(new_key)) if old_key == new_key)
            && same_endings(old_gap, new_gap)
        {
            EditKind::Layout
        } else {
            EditKind::Whitespace
        };
        self.push(kind, removed, added);
    }

    /// The gap only `side` has, up to `end`, before the token whose gap key is `key`: part of
    /// the change beside it unless it breaks a line, which is layout only where the code means
    /// the same as it shows: the new side's gap as it is, the old side's as nothing.
    fn one_gap(&mut self, side: Side, end: usize, key: Option<&[u8]>) {
        let gap = self.side(side).get(&(self.at(side)..end));
        let free = match side {
            Side::Old => may_vanish,
            Side::New => breaks_free,
        };
        let kind = if !gap.contains('\n') {
            EditKind::Changed
        } else if key.is_some_and(free) {
            EditKind::Layout
        } else {
            EditKind::Whitespace
        };
        self.push_one(side, kind, end);
    }

    /// A step of `kind` over the bytes of `side` up to `end` and of the other side up to
    /// `other_end`.
    fn push_both(&mut self, kind: EditKind, side: Side, end: usize, other_end: usize) {
        let (old_end, new_end) = match side {
            Side::Old => (end, other_end),
            Side::New => (other_end, end),
        };
        self.push(kind, self.old_at..old_end, self.new_at..new_end);
    }

    /// The pieces of a run of changed tokens on `side`, each after the gap before it.
    fn pieces(&mut self, side: Side, pieces: &[(Range<usize>, usize)]) {
        let tokens = self.side(side);
        for (piece, first) in pieces {
            self.one_gap(side, piece.start, tokens.key(*first));
            self.push_one(side, EditKind::Changed, piece.end);
        }
    }

    /// The old tokens `old_run` replaced by the new tokens `new_run`, one of them possibly empty.
    fn run(&mut self, old_run: &Range<usize>, new_run: &Range<usize>) {
        let (old, new) = (self.old, self.new);
        let (old_pieces, new_pieces) = (old.pieces(old_run), new.pieces(new_run));
        let (side, pieces, other_next) = match (old_pieces.split_first(), new_pieces.split_first())
        {
            (Some(((old_first, _), old_rest)), Some(((new_first, _), new_rest))) => {
                let keys = (old.key(old_run.start), new.key(new_run.start));
                self.both_gap(old_first.start, new_first.start, keys);
                self.push(EditKind::Changed, old_first.clone(), new_first.clone());
                self.pieces(Side::Old, old_rest);
                self.pieces(Side::New, new_rest);
                return;
            }
            (None, Some(_)) => (Side::New, new_pieces, old_run.start),
            (Some(_), None) => (Side::Old, old_pieces, new_run.start),
            (None, None) => return,
        };
        // `side` has a gap before the run and one after it where the other side has one: the
        // other's pairs with the gap before when they are the same text, and else with the gap
        // after, which the next kept token or the chunk's end then judges.
        let other = match side {
            Side::Old => Side::New,
            Side::New => Side::Old,
        };
        let other_end = self.side(other).start(other_next);
        let other_gap = self.side(other).get(&(self.at(other)..other_end));
        let (Some((first, _)), Some((last, _))) = (pieces.first(), pieces.last()) else {
            return;
        };
        let tokens = self.side(side);
        let next = match side {
            Side::Old => old_run.end,
            Side::New => new_run.end,
        };
        let lead = tokens.get(&(self.at(side)..first.start));
        let trail = tokens.get(&(last.end..tokens.start(next)));
        // A run of whole lines: it starts a line, after a line break or at the chunk's start, and
        // a line break ends it. The line break before it and the indentation after it are the
        // other side's gap, and the lines between are the change.
        let head = lead.rfind('\n').map_or(0, |at| at + 1);
        if let Some(line_end) = trail.find('\n')
            && (head > 0 || self.at(side) == 0)
            && let rest = trail.get(line_end + 1..).unwrap_or_default()
            && other_gap.len() == head + rest.len()
            && other_gap.starts_with(lead.get(..head).unwrap_or_default())
            && other_gap.ends_with(rest)
        {
            let other_at = self.at(other);
            self.push_both(EditKind::Same, side, self.at(side) + head, other_at + head);
            self.push_one(side, EditKind::Changed, last.end + line_end + 1);
            self.push_both(EditKind::Same, side, tokens.start(next), other_end);
            return;
        }
        if other_gap == lead {
            self.push_both(EditKind::Same, side, first.start, other_end);
            self.push_one(side, EditKind::Changed, first.end);
            self.pieces(side, pieces.get(1..).unwrap_or_default());
            // The gap after the run is this side's alone, unless the chunk ends there.
            if next < tokens.tokens.len() {
                self.one_gap(side, tokens.start(next), tokens.key(next));
            }
        } else {
            self.pieces(side, &pieces);
        }
    }
}

/// Whether no line ending changes between two gaps: one may break a line where the other does
/// not, but a line ending both have stays the same kind.
fn same_endings(old: &str, new: &str) -> bool {
    let endings = |gap: &str| {
        let crlf = gap.matches("\r\n").count();
        (crlf, gap.matches('\n').count() - crlf)
    };
    let ((old_crlf, old_lf), (new_crlf, new_lf)) = (endings(old), endings(new));
    !(old_crlf > 0 && new_lf > 0 || old_lf > 0 && new_crlf > 0)
}
