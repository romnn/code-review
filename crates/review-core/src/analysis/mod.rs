//! The analysis of one revision: its review units, their evidence and classifications, and moves.
//!
//! An analysis is a pure function of its `(base, head)` snapshot pair.
//! It is content-addressed by those IDs, cached on disk, and knows nothing about revision numbers.

mod build;
mod edits;
mod moves;
mod noise;

use std::ops::Range;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub use build::analyze;
pub use noise::NoiseCategory;

use crate::git::{ChangeStatus, FileMode};
use crate::ids::{ObjectId, UnitIndex};
use crate::syntax::Language;

/// The version of the analysis format and algorithm; cached analyses of another version are
/// recomputed.
///
/// Version 13 keeps each chunk's edit script in place of its highlighted ranges.
pub const ANALYSIS_FORMAT: u32 = 13;

/// Units within this many lines of each other are shown together as one hunk.
pub const HUNK_MERGE_DISTANCE: usize = 3;

/// The analysis of one `(base, head)` snapshot pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Analysis {
    /// The [`ANALYSIS_FORMAT`] this analysis was computed with.
    pub format: u32,
    /// The base snapshot: the merge base of the target and the head.
    pub base: ObjectId,
    /// The head snapshot.
    pub head: ObjectId,
    /// Every changed tree entry, sorted by path.
    pub files: Vec<FileChange>,
    /// Every review unit, grouped by file in file order and by position within a file.
    pub units: Vec<Unit>,
    /// Detected moves, as a presentation over the units.
    pub moves: Vec<Move>,
}

/// One side of a changed tree entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntrySide {
    /// The repository-relative path.
    pub path: String,
    /// The tree entry mode.
    pub mode: FileMode,
    /// The blob, symlink or submodule commit ID.
    pub oid: ObjectId,
    /// The target of a symlink, escaped losslessly: `\\`, `"`, control bytes and bytes that are not
    /// UTF-8 appear as `\\`, `\"` and `\xNN`.
    /// The object ID is the evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

/// One changed tree entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    /// The entry at the base, absent for an addition.
    pub old: Option<EntrySide>,
    /// The entry at the head, absent for a deletion.
    pub new: Option<EntrySide>,
    /// Git's classification of the change.
    pub status: ChangeStatus,
    /// The language, when it is supported for the formatting proof.
    pub language: Option<Language>,
    /// The noise category, when the file is generated, vendored or a lockfile.
    pub noise: Option<NoiseCategory>,
    /// Whether every chunk of this file is proven formatting-only.
    pub formatting_only: bool,
    /// This file's units, in position order.
    pub units: Vec<UnitIndex>,
}

impl FileChange {
    /// Returns the path to show for this file: the new path, or the old one for a deletion.
    #[must_use]
    pub fn display_path(&self) -> &str {
        self.new
            .as_ref()
            .or(self.old.as_ref())
            .map_or("", |side| side.path.as_str())
    }
}

/// A half-open range of zero-based line numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineRange {
    /// The first line, or the insertion point for an empty range.
    pub start: usize,
    /// The number of lines.
    pub len: usize,
}

impl LineRange {
    /// Returns the range as `start..start + len`.
    #[must_use]
    pub fn range(self) -> Range<usize> {
        self.start..self.start + self.len
    }

    /// Returns the end, one past the last line.
    #[must_use]
    pub fn end(self) -> usize {
        self.start + self.len
    }
}

/// A review unit: the atomic unit of review state within one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    /// The index of the file this unit belongs to in [`Analysis::files`].
    pub file: usize,
    /// What changed.
    pub kind: UnitKind,
    /// Whether the change is proven formatting-only, by its file's whole-file proof or, for a
    /// chunk, by its own.
    pub formatting_only: bool,
    /// Whether one plain move covers every changed line of this unit, so acknowledging that move
    /// marks it.
    pub moved: bool,
}

impl Unit {
    /// Returns the chunk, if this unit is one.
    #[must_use]
    pub fn chunk(&self) -> Option<&Chunk> {
        match &self.kind {
            UnitKind::Chunk(chunk) => Some(chunk),
            UnitKind::File(_) => None,
        }
    }
}

/// The two kinds of review unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum UnitKind {
    /// A maximal run of changed lines with zero context.
    Chunk(Chunk),
    /// A change that is not a line change.
    File(FileUnit),
}

/// A maximal run of changed lines in one file, with zero context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    /// The removed lines in the base file, or the insertion point when none were removed.
    pub old: LineRange,
    /// The added lines in the head file, or the deletion point when none were added.
    pub new: LineRange,
    /// The removed lines, each with its line terminator.
    pub removed: String,
    /// The added lines, each with its line terminator.
    pub added: String,
    /// The edit script from `removed` to `added`: a token diff that ignores whitespace, with
    /// each changed gap between tokens judged by the language's formatting proof.
    pub edits: Vec<Edit>,
    /// Whether both sides have equal tokens, so only whitespace differs.
    /// This is a display hint, never a reason to hide or skip the chunk.
    pub whitespace_only: bool,
    /// A digest of the context lines around the chunk on both sides, used to tell apart
    /// identical changes in one file.
    pub context: String,
}

impl Chunk {
    /// The change from lines `removed` at `old` of one text to lines `added` at `new` of
    /// another, each line with its terminator, with an edit script over plain words and
    /// punctuation: no language proves a whitespace change means nothing, so every one shows.
    #[must_use]
    pub(crate) fn of_lines(old: LineRange, new: LineRange, removed: String, added: String) -> Self {
        let removed_tokens = crate::text::generic_tokens(&removed);
        let added_tokens = crate::text::generic_tokens(&added);
        let whitespace_only = !removed.is_empty()
            && !added.is_empty()
            && removed_tokens
                .iter()
                .map(|range| removed.get(range.clone()))
                .eq(added_tokens.iter().map(|range| added.get(range.clone())));
        let edits = edits::edit_script(
            &edits::ScriptSide {
                text: &removed,
                tokens: &removed_tokens,
                keys: &vec![None; removed_tokens.len()],
            },
            &edits::ScriptSide {
                text: &added,
                tokens: &added_tokens,
                keys: &vec![None; added_tokens.len()],
            },
        );
        Self {
            old,
            new,
            removed,
            added,
            edits,
            whitespace_only,
            context: String::new(),
        }
    }

    /// The stretches of `removed` that changed, each within one line and without the whitespace
    /// around it: the words a removed line marks. None when nothing was added in their place.
    #[must_use]
    pub fn removed_highlights(&self) -> Vec<Range<usize>> {
        if self.added.is_empty() {
            return Vec::new();
        }
        changed_words(
            &self.removed,
            self.edits.iter().map(|edit| (edit.kind, &edit.removed)),
        )
    }

    /// The stretches of `added` that changed, each within one line and without the whitespace
    /// around it: the words an added line marks. None when nothing was removed in their place.
    #[must_use]
    pub fn added_highlights(&self) -> Vec<Range<usize>> {
        if self.removed.is_empty() {
            return Vec::new();
        }
        changed_words(
            &self.added,
            self.edits.iter().map(|edit| (edit.kind, &edit.added)),
        )
    }
}

/// The parts of `text` that `stretches` changed, split at line breaks and trimmed of whitespace.
fn changed_words<'a>(
    text: &str,
    stretches: impl Iterator<Item = (EditKind, &'a Range<usize>)>,
) -> Vec<Range<usize>> {
    let mut words = Vec::new();
    for (_, stretch) in stretches.filter(|(kind, _)| *kind == EditKind::Changed) {
        let Some(changed) = text.get(stretch.clone()) else {
            continue;
        };
        let mut start = stretch.start;
        for line in changed.split_inclusive('\n') {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let at = start + (line.len() - line.trim_start().len());
                words.push(at..at + trimmed.len());
            }
            start += line.len();
        }
    }
    words
}

/// One step of a chunk's edit script: a stretch of its removed text and the stretch of its added
/// text that stands in its place. The steps cover both texts in order, byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    /// How the two stretches differ.
    pub kind: EditKind,
    /// The stretch of [`Chunk::removed`].
    pub removed: Range<usize>,
    /// The stretch of [`Chunk::added`].
    pub added: Range<usize>,
}

/// How the two stretches of an [`Edit`] differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditKind {
    /// They are the same text.
    Same,
    /// They are whitespace that the language's formatting proof shows equivalent, such as a line
    /// broken anew inside brackets, so changing one into the other changes nothing.
    Layout,
    /// Tokens that changed, with the whitespace between them; either side may be empty, for an
    /// insertion or a deletion.
    Changed,
    /// Whitespace that changed where the proof cannot show it means nothing: indentation that
    /// Python reads, a line broken or joined where the language minds, a changed line ending, or
    /// any whitespace in a language the proof does not support.
    Whitespace,
}

/// A change that is not a line change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileUnit {
    /// The path changed.
    Rename,
    /// The executable bit changed.
    Mode,
    /// The entry type changed, for example from a file to a symlink.
    Type,
    /// A submodule was added, removed or moved to another commit.
    Submodule,
    /// A symlink target was added, removed or changed.
    Symlink,
    /// Binary content was added, removed or changed.
    Binary,
    /// Content too large to diff was added, removed or changed.
    Oversized,
    /// Content that is not UTF-8 text was added, removed or changed.
    NotUtf8,
    /// An empty file was added.
    EmptyAdded,
    /// An empty file was deleted.
    EmptyDeleted,
    /// The entry changed in a way no other unit describes.
    Other,
}

/// Whether a move is exact or carries edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveKind {
    /// Both sides are byte-for-byte equal; shown as one collapsed line.
    Moved,
    /// Most lines are equal; shown expanded with the inner diff, never as noise.
    MovedWithEdits,
}

/// One side of a move: a run of lines within one chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveSide {
    /// The chunk unit holding the lines.
    pub unit: UnitIndex,
    /// The first line, as an offset into the chunk's removed or added lines.
    pub offset: usize,
    /// The number of lines.
    pub len: usize,
}

/// A run of removed lines matched to a run of added lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Move {
    /// Whether the move is exact.
    pub kind: MoveKind,
    /// The removed lines.
    pub source: MoveSide,
    /// The added lines.
    pub target: MoveSide,
}

impl Move {
    /// Whether this move covers every changed line of `chunk`, unit `unit`, on both sides: the
    /// one rule for collapsing a unit as moved and for marking it when the move is acknowledged.
    pub(crate) fn covers(&self, unit: UnitIndex, chunk: &Chunk) -> bool {
        let side_covered = |lines: usize, side: MoveSide| {
            lines == 0 || (side.unit == unit && side.offset == 0 && side.len == lines)
        };
        side_covered(chunk.old.len, self.source) && side_covered(chunk.new.len, self.target)
    }
}

/// The full evidence of a unit, as recorded with a mark: both sides of the change, paths,
/// modes and object IDs, and nothing derived such as highlights.
#[derive(Debug, Serialize)]
struct Evidence<'a> {
    old: Option<&'a EntrySide>,
    new: Option<&'a EntrySide>,
    change: EvidenceChange<'a>,
}

/// The change part of a unit's evidence.
#[derive(Debug, Serialize)]
enum EvidenceChange<'a> {
    Chunk {
        old: LineRange,
        new: LineRange,
        removed: &'a str,
        added: &'a str,
    },
    File(FileUnit),
}

impl Analysis {
    /// Returns the unit at `index`, if it exists.
    #[must_use]
    pub fn unit(&self, index: UnitIndex) -> Option<&Unit> {
        self.units.get(index.get())
    }

    /// Returns the file a unit belongs to.
    #[must_use]
    pub fn file_of(&self, unit: &Unit) -> Option<&FileChange> {
        self.files.get(unit.file)
    }

    /// Returns a digest of the unit's full evidence.
    ///
    /// Marks and notes store it, so a recomputed analysis that numbers units differently can
    /// never attach a mark to the wrong code.
    #[must_use]
    pub fn evidence_digest(&self, index: UnitIndex) -> Option<String> {
        let unit = self.unit(index)?;
        let file = self.file_of(unit)?;
        let change = match &unit.kind {
            UnitKind::Chunk(chunk) => EvidenceChange::Chunk {
                old: chunk.old,
                new: chunk.new,
                removed: &chunk.removed,
                added: &chunk.added,
            },
            UnitKind::File(kind) => EvidenceChange::File(*kind),
        };
        let evidence = Evidence {
            old: file.old.as_ref(),
            new: file.new.as_ref(),
            change,
        };
        let bytes = serde_json::to_vec(&evidence).ok()?;
        Some(hex_digest(&bytes))
    }

    /// Finds the unit whose evidence has `digest`, if exactly one has.
    #[must_use]
    pub fn find_by_digest(&self, digest: &str) -> Option<UnitIndex> {
        let mut found = (0..self.units.len())
            .map(UnitIndex::new)
            .filter(|&index| self.evidence_digest(index).as_deref() == Some(digest));
        let first = found.next()?;
        found.next().is_none().then_some(first)
    }

    /// Groups the units of each file into display hunks: units within
    /// [`HUNK_MERGE_DISTANCE`] lines of each other share a hunk.
    #[must_use]
    pub fn hunks(&self) -> Vec<Vec<UnitIndex>> {
        let mut hunks = Vec::new();
        for file in &self.files {
            let mut current: Vec<UnitIndex> = Vec::new();
            let mut current_end: Option<usize> = None;
            for &index in &file.units {
                let Some(unit) = self.unit(index) else {
                    continue;
                };
                let Some(chunk) = unit.chunk() else {
                    hunks.push(vec![index]);
                    continue;
                };
                let joins =
                    current_end.is_some_and(|end| chunk.new.start <= end + HUNK_MERGE_DISTANCE);
                if !joins && !current.is_empty() {
                    hunks.push(std::mem::take(&mut current));
                }
                current.push(index);
                current_end = Some(chunk.new.end());
            }
            if !current.is_empty() {
                hunks.push(current);
            }
        }
        hunks
    }
}

/// Returns the lowercase hex SHA-256 of `bytes`.
pub(crate) fn hex_digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        // Writing to a `String` cannot fail.
        write!(hex, "{byte:02x}").ok();
    }
    hex
}
