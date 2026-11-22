//! Move detection across all files of a revision.
//!
//! A move is code that left its place and arrived elsewhere, so it needs evidence of both:
//! - Code that may have stayed is never part of a move. Within one region of a file (see
//!   [`detect`]), a line of code whose trimmed text is both removed and added may be the same
//!   line, re-indented or rewrapped where it stands, as fields are when a call is wrapped around
//!   them. Such lines are taken before any match, so no move starts on one, runs through one or
//!   merges across one, and no copy elsewhere can claim it.
//! - The other removed and added line runs are matched by their trimmed text, longest first,
//!   one-to-one and without overlap. Blank lines and lone brackets are never taken in advance,
//!   so they travel inside a run.
//!
//! A match is a plain move when every line is byte-for-byte equal, or when the caller proves
//! that the two blocks differ only in formatting, as a re-indented block or one whose lines a
//! formatter wrapped anew does; anything else is at most a move with edits, whose differences
//! stay visible.
//! Nearby matches between the same two chunks merge into one move with edits when at least
//! [`MIN_EQUAL_RATIO`] of the lines are equal and no line between them is taken.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use super::{Move, MoveKind, MoveSide, Unit, UnitKind};
use crate::ids::UnitIndex;
use crate::text::{generic_tokens, split_lines};

/// The minimum number of lines in a move.
const MIN_LINES: usize = 3;
/// The minimum number of tokens in a move.
const MIN_TOKENS: usize = 20;
/// The minimum share of equal lines in a move with edits.
const MIN_EQUAL_RATIO: f64 = 0.8;
/// The largest gap, in lines on either side, bridged when merging matches.
const MAX_GAP: usize = 3;
/// Lines occurring more often than this among added lines are not used as match anchors, which
/// bounds the work on repetitive code.
const MAX_ANCHOR_OCCURRENCES: usize = 64;

/// The removed and added lines of one chunk unit.
struct ChunkLines<'a> {
    unit: UnitIndex,
    /// The unit's file and the chunk's region in it.
    file: usize,
    region: usize,
    /// Whether the chunk is proven to change only formatting, so all of its code stayed.
    formatting_only: bool,
    removed: Vec<&'a str>,
    added: Vec<&'a str>,
    /// The removed lines no further match may take: code that may have stayed, then the lines of
    /// assigned matches.
    removed_taken: Vec<bool>,
    /// The added lines no further match may take.
    added_taken: Vec<bool>,
}

/// A line position: a chunk (index into the chunk list) and a line offset within one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Position {
    chunk: usize,
    line: usize,
}

/// A run of equal (after trimming) lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Match {
    source: Position,
    target: Position,
    len: usize,
}

/// A candidate move: one or more merged matches.
struct Block {
    source: Position,
    target: Position,
    source_len: usize,
    target_len: usize,
    equal_lines: usize,
    merged: bool,
}

/// Detects moves among the chunk units and sets [`Unit::moved`] on units that one plain move
/// covers completely.
///
/// `regions` holds each chunk unit's region in its file: chunks of one file share a region when
/// no unchanged line of code that identifies its place separates them, so code going from one to
/// the other went nowhere. `formatting_only` says whether a move with edits differs only in
/// formatting, which makes it a plain move.
pub(super) fn detect(
    units: &mut [Unit],
    regions: &HashMap<UnitIndex, usize>,
    mut formatting_only: impl FnMut(&Move, &[Unit]) -> bool,
) -> Vec<Move> {
    let mut chunks: Vec<ChunkLines<'_>> = Vec::new();
    for (index, unit) in units.iter().enumerate() {
        let UnitKind::Chunk(chunk) = &unit.kind else {
            continue;
        };
        let unit_index = UnitIndex::new(index);
        let (removed, added) = (split_lines(&chunk.removed), split_lines(&chunk.added));
        chunks.push(ChunkLines {
            unit: unit_index,
            file: unit.file,
            region: regions.get(&unit_index).copied().unwrap_or_default(),
            formatting_only: unit.formatting_only,
            removed_taken: vec![false; removed.len()],
            added_taken: vec![false; added.len()],
            removed,
            added,
        });
    }
    take_what_may_have_stayed(&mut chunks);
    let candidates = candidates(&chunks);
    let matches = assign(&mut chunks, candidates);
    let mut moves: Vec<Move> = merge(&chunks, matches)
        .into_iter()
        .filter_map(|block| finish(&chunks, &block))
        .collect();
    for found in &mut moves {
        if found.kind == MoveKind::MovedWithEdits && formatting_only(found, units) {
            found.kind = MoveKind::Moved;
        }
    }
    // A move can cover only the units at its two ends.
    for found in moves.iter().filter(|found| found.kind == MoveKind::Moved) {
        for index in [found.source.unit, found.target.unit] {
            if let Some(unit) = units.get_mut(index.get())
                && unit.chunk().is_some_and(|chunk| found.covers(index, chunk))
            {
                unit.moved = true;
            }
        }
    }
    moves
}

/// Whether `line` is code rather than a blank line or a lone bracket.
pub(super) fn anchorable(line: &str) -> bool {
    line.chars().any(char::is_alphanumeric)
}

/// Takes every line of code that may have stayed: in one region of one file, a line whose trimmed
/// text is both removed and added there, and every line of a chunk proven to change only
/// formatting.
fn take_what_may_have_stayed(chunks: &mut [ChunkLines<'_>]) {
    // Chunks come in file order, so each region's chunks are consecutive.
    for region in
        chunks.chunk_by_mut(|left, right| (left.file, left.region) == (right.file, right.region))
    {
        // Only lines of code go into the sets, so a chunk of one repeated blank or bracket costs
        // nothing, and a region that only adds or only removes builds none.
        let two_sided = region.iter().any(|lines| !lines.removed.is_empty())
            && region.iter().any(|lines| !lines.added.is_empty());
        let mut removed: HashSet<&str> = HashSet::new();
        let mut added: HashSet<&str> = HashSet::new();
        if two_sided {
            for lines in region.iter() {
                removed.extend(
                    lines
                        .removed
                        .iter()
                        .map(|line| line.trim())
                        .filter(|text| anchorable(text)),
                );
                added.extend(
                    lines
                        .added
                        .iter()
                        .map(|line| line.trim())
                        .filter(|text| anchorable(text)),
                );
            }
        }
        for lines in region.iter_mut() {
            let proven = lines.formatting_only;
            let stays = |line: &&str| {
                let text = line.trim();
                anchorable(text) && (proven || (removed.contains(text) && added.contains(text)))
            };
            lines.removed_taken = lines.removed.iter().map(stays).collect();
            lines.added_taken = lines.added.iter().map(stays).collect();
        }
    }
}

/// Finds every maximal run of trimmed-equal lines between a removed and an added side, among the
/// lines not taken.
fn candidates(chunks: &[ChunkLines<'_>]) -> Vec<Match> {
    // Every added line of code counts toward an anchor's occurrences, taken or not.
    let mut index: HashMap<&str, Vec<Position>> = HashMap::new();
    for (chunk, lines) in chunks.iter().enumerate() {
        for (line, text) in lines.added.iter().enumerate() {
            let key = text.trim();
            if anchorable(key) {
                index.entry(key).or_default().push(Position { chunk, line });
            }
        }
    }
    let added = |position: Position| -> Option<&str> {
        let lines = chunks.get(position.chunk)?;
        if *lines.added_taken.get(position.line)? {
            return None;
        }
        Some(lines.added.get(position.line)?.trim())
    };
    let mut matches = Vec::new();
    let mut seen: HashSet<Match> = HashSet::new();
    for (chunk, lines) in chunks.iter().enumerate() {
        let removed = |line: usize| -> Option<&str> {
            if *lines.removed_taken.get(line)? {
                return None;
            }
            Some(lines.removed.get(line)?.trim())
        };
        for line in 0..lines.removed.len() {
            let Some(targets) = removed(line).and_then(|text| index.get(text)) else {
                continue;
            };
            if targets.len() > MAX_ANCHOR_OCCURRENCES {
                continue;
            }
            for &target in targets {
                if added(target).is_none() {
                    continue;
                }
                // Walk back to the first line of the maximal run, which may itself not be an
                // anchor (a lone `}` or blank line), and count each run once.
                let mut back = 0;
                while line > back
                    && target.line > back
                    && removed(line - back - 1).is_some_and(|source_line| {
                        added(Position {
                            chunk: target.chunk,
                            line: target.line - back - 1,
                        }) == Some(source_line)
                    })
                {
                    back += 1;
                }
                let start = Match {
                    source: Position {
                        chunk,
                        line: line - back,
                    },
                    target: Position {
                        chunk: target.chunk,
                        line: target.line - back,
                    },
                    len: 0,
                };
                if !seen.insert(start) {
                    continue;
                }
                let mut len = 0;
                while let Some(source_line) = removed(start.source.line + len)
                    && added(Position {
                        chunk: target.chunk,
                        line: start.target.line + len,
                    }) == Some(source_line)
                {
                    len += 1;
                }
                if len >= MIN_LINES {
                    matches.push(Match { len, ..start });
                }
            }
        }
    }
    matches
}

/// Assigns matches one-to-one and without overlap, longest first, taking the lines of each one
/// it accepts.
fn assign(chunks: &mut [ChunkLines<'_>], mut matches: Vec<Match>) -> Vec<Match> {
    matches.sort_by(|left, right| {
        right
            .len
            .cmp(&left.len)
            .then(left.source.cmp(&right.source))
            .then(left.target.cmp(&right.target))
    });
    let mut accepted = Vec::new();
    for candidate in matches {
        let source_lines = candidate.source.line..candidate.source.line + candidate.len;
        let target_lines = candidate.target.line..candidate.target.line + candidate.len;
        let (Some(source), Some(target)) = (
            chunks.get(candidate.source.chunk),
            chunks.get(candidate.target.chunk),
        ) else {
            continue;
        };
        if !free(&source.removed_taken, source_lines.clone())
            || !free(&target.added_taken, target_lines.clone())
        {
            continue;
        }
        if let Some(taken) = chunks
            .get_mut(candidate.source.chunk)
            .and_then(|chunk| chunk.removed_taken.get_mut(source_lines))
        {
            taken.fill(true);
        }
        if let Some(taken) = chunks
            .get_mut(candidate.target.chunk)
            .and_then(|chunk| chunk.added_taken.get_mut(target_lines))
        {
            taken.fill(true);
        }
        accepted.push(candidate);
    }
    accepted
}

/// Whether `taken` has all of `lines` and none of them is taken.
fn free(taken: &[bool], lines: Range<usize>) -> bool {
    taken
        .get(lines)
        .is_some_and(|lines| lines.iter().all(|taken| !taken))
}

/// Merges nearby matches between the same two chunks into blocks, never across a taken line:
/// code that may have stayed, or another match's.
fn merge(chunks: &[ChunkLines<'_>], mut matches: Vec<Match>) -> Vec<Block> {
    matches.sort_by(|left, right| {
        left.source
            .cmp(&right.source)
            .then(left.target.cmp(&right.target))
    });
    let mut blocks: Vec<Block> = Vec::new();
    for found in matches {
        if let Some(block) = blocks.last_mut() {
            let source_end = block.source.line + block.source_len;
            let target_end = block.target.line + block.target_len;
            let same_chunks = block.source.chunk == found.source.chunk
                && block.target.chunk == found.target.chunk;
            let close = found.source.line >= source_end
                && found.target.line >= target_end
                && found.source.line - source_end <= MAX_GAP
                && found.target.line - target_end <= MAX_GAP;
            let open_gaps = same_chunks
                && close
                && chunks
                    .get(found.source.chunk)
                    .is_some_and(|chunk| free(&chunk.removed_taken, source_end..found.source.line))
                && chunks
                    .get(found.target.chunk)
                    .is_some_and(|chunk| free(&chunk.added_taken, target_end..found.target.line));
            if open_gaps {
                let source_len = found.source.line + found.len - block.source.line;
                let target_len = found.target.line + found.len - block.target.line;
                let equal_lines = block.equal_lines + found.len;
                if ratio(equal_lines, source_len.max(target_len)) >= MIN_EQUAL_RATIO {
                    block.source_len = source_len;
                    block.target_len = target_len;
                    block.equal_lines = equal_lines;
                    block.merged = true;
                    continue;
                }
            }
        }
        blocks.push(Block {
            source: found.source,
            target: found.target,
            source_len: found.len,
            target_len: found.len,
            equal_lines: found.len,
            merged: false,
        });
    }
    blocks
}

fn ratio(equal: usize, total: usize) -> f64 {
    let equal = u32::try_from(equal).unwrap_or(u32::MAX);
    let total = u32::try_from(total.max(1)).unwrap_or(u32::MAX);
    f64::from(equal) / f64::from(total)
}

/// Turns a block into a move, or drops it when it is too small.
fn finish(chunks: &[ChunkLines<'_>], block: &Block) -> Option<Move> {
    let source_chunk = chunks.get(block.source.chunk)?;
    let target_chunk = chunks.get(block.target.chunk)?;
    let source_lines = source_chunk
        .removed
        .get(block.source.line..block.source.line + block.source_len)?;
    let target_lines = target_chunk
        .added
        .get(block.target.line..block.target.line + block.target_len)?;
    let source_text: String = source_lines.concat();
    if source_lines.len() < MIN_LINES || generic_tokens(&source_text).len() < MIN_TOKENS {
        return None;
    }
    let exact = !block.merged && source_lines == target_lines;
    Some(Move {
        kind: if exact {
            MoveKind::Moved
        } else {
            MoveKind::MovedWithEdits
        },
        source: MoveSide {
            unit: source_chunk.unit,
            offset: block.source.line,
            len: block.source_len,
        },
        target: MoveSide {
            unit: target_chunk.unit,
            offset: block.target.line,
            len: block.target_len,
        },
    })
}
