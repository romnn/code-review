//! Move detection across all files of a revision.
//!
//! Removed and added line runs are matched by their trimmed text, longest first, one-to-one and
//! without overlap.
//! A match is a plain move only when every line is byte-for-byte equal; anything else, including
//! re-indentation, is at most a move with edits, whose differences stay visible.
//! Nearby matches between the same two chunks merge into one move with edits when at least
//! [`MIN_EQUAL_RATIO`] of the lines are equal.

use std::collections::{HashMap, HashSet};

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
    removed: Vec<&'a str>,
    added: Vec<&'a str>,
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

/// Detects moves among the chunk units and sets [`Unit::moved`] on units that plain moves cover
/// completely.
pub(super) fn detect(units: &mut [Unit]) -> Vec<Move> {
    let chunks: Vec<ChunkLines<'_>> = units
        .iter()
        .enumerate()
        .filter_map(|(index, unit)| {
            let UnitKind::Chunk(chunk) = &unit.kind else {
                return None;
            };
            Some(ChunkLines {
                unit: UnitIndex::new(index),
                removed: split_lines(&chunk.removed),
                added: split_lines(&chunk.added),
            })
        })
        .collect();
    let matches = assign(&chunks, candidates(&chunks));
    let moves: Vec<Move> = merge(matches)
        .into_iter()
        .filter_map(|block| finish(&chunks, &block))
        .collect();
    for coverage in plain_coverage(&chunks, &moves) {
        if let Some(unit) = units.get_mut(coverage.unit.get()) {
            unit.moved = coverage.covered;
        }
    }
    moves
}

fn anchorable(line: &str) -> bool {
    line.chars().any(char::is_alphanumeric)
}

/// Finds every maximal run of trimmed-equal lines between a removed and an added side.
fn candidates(chunks: &[ChunkLines<'_>]) -> Vec<Match> {
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
        Some(chunks.get(position.chunk)?.added.get(position.line)?.trim())
    };
    let mut matches = Vec::new();
    let mut seen: HashSet<Match> = HashSet::new();
    for (chunk, lines) in chunks.iter().enumerate() {
        let removed = |line: usize| lines.removed.get(line).map(|text| text.trim());
        for (line, text) in lines.removed.iter().enumerate() {
            let Some(targets) = index.get(text.trim()) else {
                continue;
            };
            if targets.len() > MAX_ANCHOR_OCCURRENCES {
                continue;
            }
            for &target in targets {
                // Walk back to the first line of the maximal run, which may itself not be an
                // anchor (a lone `}` or blank line), and count each run once.
                let mut back = 0;
                while line > back
                    && target.line > back
                    && removed(line - back - 1)
                        == added(Position {
                            chunk: target.chunk,
                            line: target.line - back - 1,
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

/// Assigns matches one-to-one and without overlap, longest first.
fn assign(chunks: &[ChunkLines<'_>], mut matches: Vec<Match>) -> Vec<Match> {
    matches.sort_by(|left, right| {
        right
            .len
            .cmp(&left.len)
            .then(left.source.cmp(&right.source))
            .then(left.target.cmp(&right.target))
    });
    let mut removed_claimed: Vec<Vec<bool>> = chunks
        .iter()
        .map(|lines| vec![false; lines.removed.len()])
        .collect();
    let mut added_claimed: Vec<Vec<bool>> = chunks
        .iter()
        .map(|lines| vec![false; lines.added.len()])
        .collect();
    let mut accepted = Vec::new();
    for candidate in matches {
        let free = |claimed: &Vec<Vec<bool>>, position: Position| {
            claimed.get(position.chunk).is_some_and(|lines| {
                lines
                    .get(position.line..position.line + candidate.len)
                    .is_some_and(|range| range.iter().all(|claimed| !claimed))
            })
        };
        if !free(&removed_claimed, candidate.source) || !free(&added_claimed, candidate.target) {
            continue;
        }
        for (claimed, position) in [
            (&mut removed_claimed, candidate.source),
            (&mut added_claimed, candidate.target),
        ] {
            if let Some(lines) = claimed
                .get_mut(position.chunk)
                .and_then(|lines| lines.get_mut(position.line..position.line + candidate.len))
            {
                lines.fill(true);
            }
        }
        accepted.push(candidate);
    }
    accepted
}

/// Merges nearby matches between the same two chunks into blocks.
fn merge(mut matches: Vec<Match>) -> Vec<Block> {
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
            if same_chunks && close {
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

/// Whether plain moves cover every changed line of a chunk unit.
struct Coverage {
    unit: UnitIndex,
    covered: bool,
}

/// Returns, for every chunk unit, whether plain moves cover all of its changed lines.
fn plain_coverage(chunks: &[ChunkLines<'_>], moves: &[Move]) -> Vec<Coverage> {
    let position: HashMap<UnitIndex, usize> = chunks
        .iter()
        .enumerate()
        .map(|(position, lines)| (lines.unit, position))
        .collect();
    let mut removed: Vec<Vec<bool>> = chunks
        .iter()
        .map(|lines| vec![false; lines.removed.len()])
        .collect();
    let mut added: Vec<Vec<bool>> = chunks
        .iter()
        .map(|lines| vec![false; lines.added.len()])
        .collect();
    for found in moves.iter().filter(|found| found.kind == MoveKind::Moved) {
        for (covered, side) in [(&mut removed, found.source), (&mut added, found.target)] {
            if let Some(lines) = position
                .get(&side.unit)
                .and_then(|&chunk| covered.get_mut(chunk))
                .and_then(|lines| lines.get_mut(side.offset..side.offset + side.len))
            {
                lines.fill(true);
            }
        }
    }
    chunks
        .iter()
        .zip(removed.iter().zip(&added))
        .map(|(lines, (removed, added))| {
            let changed = !removed.is_empty() || !added.is_empty();
            let covered = removed.iter().chain(added).all(|&covered| covered);
            Coverage {
                unit: lines.unit,
                covered: changed && covered,
            }
        })
        .collect()
}
