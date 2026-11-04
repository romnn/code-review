//! A read-only view of one revision: its units with their states, interdiffs, notes, moves and
//! progress counts.

use serde::Serialize;

use crate::analysis::{
    Analysis, EntrySide, FileChange, Move, MoveKind, NoiseCategory, Unit, UnitKind,
};
use crate::correspond::LinkKind;
use crate::error::Error;
use crate::history::{History, RevisionData, RevisionRecord, UnitState, Verdict};
use crate::ids::{MoveId, RevisionNumber, UnitId, UnitIndex};
use crate::text::{diff_lines, split_lines};

/// A view of one revision of a review.
pub struct RevisionView<'h> {
    history: &'h History,
    number: RevisionNumber,
}

/// Where a unit is, for display: the path and one-based line numbers in the head file, or in the
/// base file under its old path for a chunk that only removes lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Location {
    /// The repository-relative path.
    pub path: String,
    /// The first line, one-based.
    pub line: usize,
    /// The last line, one-based and inclusive; equal to `line` for an empty range.
    pub end_line: usize,
}

/// One line of an interdiff side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffLine {
    /// Whether the line is shared, only in the reviewed version, or only in the current one.
    pub kind: DiffLineKind,
    /// The line, without its terminator.
    pub text: String,
    /// The line's terminator, so line-ending changes stay visible.
    pub ending: LineEnding,
}

/// How a line ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    /// `\n`.
    Lf,
    /// `\r\n`.
    CrLf,
    /// No terminator: the last line of a file without a final newline.
    Missing,
}

/// A line split into its content and its terminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitLine<'a> {
    /// The line without its terminator; a lone `\r` that does not precede `\n` stays here.
    pub content: &'a str,
    /// The terminator.
    pub ending: LineEnding,
}

/// Splits one line (as produced by splitting after each `\n`) into content and terminator.
#[must_use]
pub fn split_line_ending(line: &str) -> SplitLine<'_> {
    match line.strip_suffix('\n') {
        Some(content) => match content.strip_suffix('\r') {
            Some(content) => SplitLine {
                content,
                ending: LineEnding::CrLf,
            },
            None => SplitLine {
                content,
                ending: LineEnding::Lf,
            },
        },
        None => SplitLine {
            content: line,
            ending: LineEnding::Missing,
        },
    }
}

/// The origin of an interdiff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    /// Present in both versions.
    Same,
    /// Only in the older version: the reviewed one in an interdiff, the source in a move.
    Old,
    /// Only in the newer version: the current one in an interdiff, the target in a move.
    New,
}

/// A line diff between two versions of some lines: one side of an interdiff, or the inner diff
/// of a move with edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SideDiff {
    /// Whether the two versions differ.
    pub changed: bool,
    /// The lines, old and new interleaved.
    pub lines: Vec<DiffLine>,
}

/// What changed in a unit since it was last reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Interdiff {
    /// The unit that was reviewed.
    pub reviewed: UnitId,
    /// The removed side: differences here mean the target branch moved.
    pub base: SideDiff,
    /// The added side: differences here mean the change itself changed.
    pub head: SideDiff,
}

/// Where a note is attached in the viewed revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum NoteAttachment {
    /// The note followed its unit's chain to this unit.
    Unit {
        /// The unit.
        unit: UnitId,
    },
    /// The chain ended or split, so the note falls back to the file and is outdated.
    File {
        /// The last path the note's unit had.
        path: String,
    },
}

/// A note resolved for the viewed revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedNote {
    /// The note text.
    pub text: String,
    /// The unit the note was written on, as provenance.
    pub written_on: UnitId,
    /// Where it is attached now.
    pub attached: NoteAttachment,
    /// Seconds since the Unix epoch when it was written.
    pub at: u64,
}

impl ResolvedNote {
    /// Returns whether the note lost its unit and fell back to the file.
    #[must_use]
    pub fn is_outdated(&self) -> bool {
        matches!(self.attached, NoteAttachment::File { .. })
    }
}

/// One end of a move, for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveEnd {
    /// The chunk unit holding the lines.
    pub unit: UnitId,
    /// The lines in the base (source) or head (target) file.
    pub location: Location,
}

/// A chunk's head-side lines within its file.
#[derive(Debug, Clone, Copy)]
struct LineSpan {
    file: usize,
    start: usize,
    end: usize,
}

/// Why a unit is collapsed by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum Collapse {
    /// Its file is generated, a lockfile or a snapshot.
    Noise(NoiseCategory),
    /// Its change is proven formatting-only.
    FormattingOnly,
    /// Plain moves cover all its lines; shown as one line naming the move.
    Moved(Box<MoveView>),
}

/// A move, for display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MoveView {
    /// The move's ID.
    pub id: MoveId,
    /// Whether it is exact.
    pub kind: MoveKind,
    /// The removed lines.
    pub source: MoveEnd,
    /// The added lines.
    pub target: MoveEnd,
    /// For a move with edits, the diff from the source lines to the target lines, which shows
    /// the edits; `None` for a plain move, whose sides are equal.
    pub inner: Option<SideDiff>,
}

impl MoveView {
    /// Describes the move from the side of `unit`: "moved from `a.rs:3-9` (m1-0000)" at the
    /// target, "moved to …" at the source, with "with edits" for a move with edits. `None` when
    /// the move does not touch `unit`.
    #[must_use]
    pub fn describe_for(&self, unit: UnitId) -> Option<String> {
        let edits = match self.kind {
            MoveKind::Moved => "",
            MoveKind::MovedWithEdits => " with edits",
        };
        let (direction, other) = if self.target.unit == unit {
            ("from", &self.source.location)
        } else if self.source.unit == unit {
            ("to", &self.target.location)
        } else {
            return None;
        };
        Some(format!(
            "moved{edits} {direction} {}:{}-{} ({})",
            other.path, other.line, other.end_line, self.id
        ))
    }
}

/// Progress counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    /// All units.
    pub units: usize,
    /// Never reviewed, or explicitly marked unseen.
    pub new: usize,
    /// Reviewed before, but changed since.
    pub changed: usize,
    /// Reviewed as seen.
    pub reviewed: usize,
    /// Reviewed and accepted.
    pub accepted: usize,
    /// Reviewed and flagged.
    pub flagged: usize,
    /// In generated, lockfile or snapshot files.
    pub noise: usize,
    /// Proven formatting-only.
    pub formatting_only: usize,
    /// Covered completely by plain moves.
    pub moved: usize,
}

impl Counts {
    fn add(&mut self, state: UnitState, unit: &Unit, file: Option<&FileChange>) {
        self.units += 1;
        match state {
            UnitState::New | UnitState::Unseen { .. } => self.new += 1,
            UnitState::ChangedSinceReviewed { .. } => self.changed += 1,
            UnitState::Reviewed { verdict, .. } => match verdict {
                Verdict::Seen => self.reviewed += 1,
                Verdict::Accepted => self.accepted += 1,
                Verdict::Flagged => self.flagged += 1,
            },
        }
        self.noise += usize::from(file.is_some_and(|file| file.noise.is_some()));
        self.formatting_only += usize::from(unit.formatting_only);
        self.moved += usize::from(unit.moved);
    }

    /// Returns the number of units that still need review.
    #[must_use]
    pub fn unreviewed(&self) -> usize {
        self.new + self.changed
    }
}

/// Counts for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStatus {
    /// The display path.
    pub path: String,
    /// The file's counts.
    pub counts: Counts,
}

/// Progress of a revision, per file and in total.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The revision.
    pub revision: RevisionNumber,
    /// Totals.
    pub total: Counts,
    /// Per file, in file order.
    pub files: Vec<FileStatus>,
}

impl<'h> RevisionView<'h> {
    pub(crate) fn new(history: &'h History, number: RevisionNumber) -> Self {
        Self { history, number }
    }

    fn data(&self) -> Option<&'h RevisionData> {
        self.history.revision(self.number)
    }

    /// Returns the history this view belongs to.
    #[must_use]
    pub fn history(&self) -> &'h History {
        self.history
    }

    /// Returns the revision's number.
    #[must_use]
    pub fn number(&self) -> RevisionNumber {
        self.number
    }

    /// Returns the revision's record.
    #[must_use]
    pub fn record(&self) -> Option<&'h RevisionRecord> {
        self.data().map(|data| &data.record)
    }

    /// Returns the revision's analysis.
    #[must_use]
    pub fn analysis(&self) -> Option<&'h Analysis> {
        self.data().map(|data| data.analysis.as_ref())
    }

    /// Returns the IDs of every unit, in file and position order.
    #[must_use]
    pub fn unit_ids(&self) -> Vec<UnitId> {
        let count = self.analysis().map_or(0, |analysis| analysis.units.len());
        (0..count)
            .map(|index| UnitId::new(self.number, UnitIndex::new(index)))
            .collect()
    }

    /// Returns the ID of the unit at `index` in this revision.
    #[must_use]
    pub fn id(&self, index: UnitIndex) -> UnitId {
        UnitId::new(self.number, index)
    }

    fn index(&self, id: UnitId) -> Result<UnitIndex, Error> {
        if id.revision() != self.number {
            return Err(Error::WrongRevision {
                unit: id,
                expected: self.number,
            });
        }
        Ok(id.index())
    }

    /// Returns the unit `id`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::WrongRevision`] for a unit of another revision and
    /// [`Error::UnknownUnit`] for an index out of range.
    pub fn unit(&self, id: UnitId) -> Result<&'h Unit, Error> {
        let index = self.index(id)?;
        self.analysis()
            .and_then(|analysis| analysis.unit(index))
            .ok_or(Error::UnknownUnit(id))
    }

    /// Returns the file of unit `id`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn file(&self, id: UnitId) -> Result<&'h FileChange, Error> {
        let unit = self.unit(id)?;
        self.analysis()
            .and_then(|analysis| analysis.file_of(unit))
            .ok_or(Error::UnknownUnit(id))
    }

    /// Returns the digest of unit `id`'s full evidence.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn evidence_digest(&self, id: UnitId) -> Result<String, Error> {
        let index = self.index(id)?;
        self.analysis()
            .and_then(|analysis| analysis.evidence_digest(index))
            .ok_or(Error::UnknownUnit(id))
    }

    /// Returns the review state of unit `id`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn state(&self, id: UnitId) -> Result<UnitState, Error> {
        let index = self.index(id)?;
        self.data()
            .and_then(|data| data.states.get(index.get()).copied())
            .ok_or(Error::UnknownUnit(id))
    }

    /// Returns whether unit `id` belongs to a noise category, a formatting-only change or a
    /// plain move: the units collapsed by default.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn is_collapsed(&self, id: UnitId) -> Result<bool, Error> {
        let unit = self.unit(id)?;
        Ok(unit.formatting_only || unit.moved || self.noise(id)?.is_some())
    }

    /// Returns why unit `id` is collapsed by default, or `None` when it is shown in full.
    ///
    /// Noise wins over formatting-only, which wins over a plain move; a plain move names the move
    /// so the unit can be shown as one "moved from" or "moved to" line.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn collapse(&self, id: UnitId) -> Result<Option<Collapse>, Error> {
        let unit = self.unit(id)?;
        if let Some(category) = self.noise(id)? {
            return Ok(Some(Collapse::Noise(category)));
        }
        if unit.formatting_only {
            return Ok(Some(Collapse::FormattingOnly));
        }
        if unit.moved {
            let plain = self
                .moves_touching(id)
                .into_iter()
                .find(|found| found.kind == MoveKind::Moved);
            if let Some(found) = plain {
                return Ok(Some(Collapse::Moved(Box::new(found))));
            }
        }
        Ok(None)
    }

    /// Returns the moves whose source or target is unit `id`.
    #[must_use]
    pub fn moves_touching(&self, id: UnitId) -> Vec<MoveView> {
        self.moves()
            .into_iter()
            .filter(|found| found.source.unit == id || found.target.unit == id)
            .collect()
    }

    /// Groups `units` into display hunks: consecutive units of one file whose head-side lines are
    /// within [`crate::analysis::HUNK_MERGE_DISTANCE`] of each other share a hunk.
    #[must_use]
    pub fn display_hunks(&self, units: &[UnitId]) -> Vec<Vec<UnitId>> {
        let mut hunks: Vec<Vec<UnitId>> = Vec::new();
        // The file and head-side end line of the previous chunk, if it was a chunk.
        let mut previous: Option<LineSpan> = None;
        for &id in units {
            let Ok(unit) = self.unit(id) else {
                continue;
            };
            let span = unit.chunk().map(|chunk| LineSpan {
                file: unit.file,
                start: chunk.new.start,
                end: chunk.new.end(),
            });
            let joins = previous.zip(span).is_some_and(|(before, after)| {
                before.file == after.file
                    && after.start <= before.end + crate::analysis::HUNK_MERGE_DISTANCE
            });
            match hunks.last_mut() {
                Some(hunk) if joins => hunk.push(id),
                _ => hunks.push(vec![id]),
            }
            previous = span;
        }
        hunks
    }

    /// Returns unit `id`'s noise category.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn noise(&self, id: UnitId) -> Result<Option<NoiseCategory>, Error> {
        Ok(self.file(id)?.noise)
    }

    /// Returns where unit `id` is.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn location(&self, id: UnitId) -> Result<Location, Error> {
        let unit = self.unit(id)?;
        let file = self.file(id)?;
        let path = file.display_path().to_owned();
        let range = match unit.chunk() {
            Some(chunk) if chunk.new.len > 0 => chunk.new,
            // A deletion is located by the removed lines, in the base file under its old path.
            Some(chunk) if chunk.old.len > 0 => {
                let old_path = file.old.as_ref().map_or(path, |side| side.path.clone());
                let line = chunk.old.start + 1;
                return Ok(Location {
                    path: old_path,
                    line,
                    end_line: chunk.old.end().max(line),
                });
            }
            Some(chunk) => chunk.new,
            None => {
                return Ok(Location {
                    path,
                    line: 1,
                    end_line: 1,
                });
            }
        };
        let line = range.start + 1;
        Ok(Location {
            path,
            line,
            end_line: (range.end()).max(line),
        })
    }

    /// Returns a one-line excerpt of unit `id`, at most `max_chars` characters long.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn excerpt(&self, id: UnitId, max_chars: usize) -> Result<String, Error> {
        let unit = self.unit(id)?;
        let file = self.file(id)?;
        let text = match &unit.kind {
            UnitKind::Chunk(chunk) => {
                let first = |text: &str, sign: char| {
                    text.lines()
                        .find(|line| !line.trim().is_empty())
                        .map(|line| format!("{sign}{}", line.trim()))
                };
                first(&chunk.added, '+')
                    .or_else(|| first(&chunk.removed, '-'))
                    .unwrap_or_else(|| "(blank lines)".to_owned())
            }
            UnitKind::File(kind) => describe_file_unit(*kind, file),
        };
        Ok(truncate(&text, max_chars))
    }

    /// Returns the full description of file unit `id`, with every value that changed (modes,
    /// symlink targets, submodule commits), or `None` for a chunk, whose evidence is its lines.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn describe(&self, id: UnitId) -> Result<Option<String>, Error> {
        let unit = self.unit(id)?;
        let file = self.file(id)?;
        Ok(match &unit.kind {
            UnitKind::Chunk(_) => None,
            UnitKind::File(kind) => Some(describe_file_unit(*kind, file)),
        })
    }

    /// Returns the interdiff of unit `id` against its last reviewed predecessor, if it is
    /// changed since reviewed.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn interdiff(&self, id: UnitId) -> Result<Option<Interdiff>, Error> {
        let UnitState::ChangedSinceReviewed { reviewed, .. } = self.state(id)? else {
            return Ok(None);
        };
        let current_unit = self.unit(id)?;
        let current_file = self.file(id)?;
        let reviewed_view = self.history.view(reviewed.revision())?;
        let reviewed_unit = reviewed_view.unit(reviewed)?;
        let reviewed_file = reviewed_view.file(reviewed)?;
        let (base, head) = match (&reviewed_unit.kind, &current_unit.kind) {
            (UnitKind::Chunk(before), UnitKind::Chunk(after)) => (
                side_diff(&before.removed, &after.removed),
                side_diff(&before.added, &after.added),
            ),
            _ => (
                side_diff(
                    &describe_side(reviewed_file.old.as_ref()),
                    &describe_side(current_file.old.as_ref()),
                ),
                side_diff(
                    &describe_side(reviewed_file.new.as_ref()),
                    &describe_side(current_file.new.as_ref()),
                ),
            ),
        };
        Ok(Some(Interdiff {
            reviewed,
            base,
            head,
        }))
    }

    /// Returns every note written on this revision or earlier, resolved for this revision.
    #[must_use]
    pub fn notes(&self) -> Vec<ResolvedNote> {
        self.history
            .notes()
            .iter()
            .filter(|note| note.revision <= self.number)
            .map(|note| {
                let written_on = UnitId::new(note.revision, note.unit);
                ResolvedNote {
                    text: note.text.clone(),
                    written_on,
                    attached: self.follow_note(written_on),
                    at: note.at,
                }
            })
            .collect()
    }

    /// Follows a note's unit forward along one-to-one, unambiguous links.
    fn follow_note(&self, origin: UnitId) -> NoteAttachment {
        let mut current = origin;
        while current.revision() < self.number {
            let successors = self.history.successors(current);
            let next = match successors {
                [link] if link.kind != LinkKind::Ambiguous => current
                    .revision()
                    .next()
                    .map(|revision| UnitId::new(revision, link.newer))
                    .filter(|next| self.history.predecessors(*next).len() == 1),
                _ => None,
            };
            let Some(next) = next else {
                let path = self
                    .history
                    .view(current.revision())
                    .and_then(|view| {
                        view.file(current)
                            .map(|file| file.display_path().to_owned())
                    })
                    .unwrap_or_default();
                return NoteAttachment::File { path };
            };
            current = next;
        }
        NoteAttachment::Unit { unit: current }
    }

    /// Returns every move of this revision.
    #[must_use]
    pub fn moves(&self) -> Vec<MoveView> {
        let Some(analysis) = self.analysis() else {
            return Vec::new();
        };
        analysis
            .moves
            .iter()
            .enumerate()
            .filter_map(|(index, found)| self.move_view(MoveId::new(self.number, index), found))
            .collect()
    }

    fn move_view(&self, id: MoveId, found: &Move) -> Option<MoveView> {
        let end = |side: crate::analysis::MoveSide, removed: bool| -> Option<MoveEnd> {
            let unit = self.id(side.unit);
            let chunk = self.unit(unit).ok()?.chunk()?;
            let file = self.file(unit).ok()?;
            // Removed lines are numbered in the base file, added lines in the head file, so each
            // end names the path of its own side, which differs after a rename.
            let (start, entry) = if removed {
                (chunk.old.start, file.old.as_ref())
            } else {
                (chunk.new.start, file.new.as_ref())
            };
            let start = start + side.offset;
            Some(MoveEnd {
                unit,
                location: Location {
                    path: entry
                        .map_or(file.display_path(), |entry| entry.path.as_str())
                        .to_owned(),
                    line: start + 1,
                    end_line: start + side.len.max(1),
                },
            })
        };
        let lines = |side: crate::analysis::MoveSide, removed: bool| -> Option<String> {
            let chunk = self.unit(self.id(side.unit)).ok()?.chunk()?;
            let text = if removed {
                &chunk.removed
            } else {
                &chunk.added
            };
            let lines = split_lines(text);
            Some(lines.get(side.offset..side.offset + side.len)?.concat())
        };
        let inner = match found.kind {
            MoveKind::Moved => None,
            MoveKind::MovedWithEdits => Some(side_diff(
                &lines(found.source, true)?,
                &lines(found.target, false)?,
            )),
        };
        Some(MoveView {
            id,
            kind: found.kind,
            source: end(found.source, true)?,
            target: end(found.target, false)?,
            inner,
        })
    }

    /// Returns the units a plain move covers completely, on both sides: acknowledging the move
    /// marks exactly these.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownMove`] for a move of another revision or out of range, and
    /// [`Error::MoveHasEdits`] for a move with edits.
    pub fn units_covered_by_move(&self, id: MoveId) -> Result<Vec<UnitId>, Error> {
        let found = (id.revision() == self.number)
            .then(|| {
                self.analysis()
                    .and_then(|analysis| analysis.moves.get(id.index()))
            })
            .flatten()
            .ok_or(Error::UnknownMove(id))?;
        if found.kind != MoveKind::Moved {
            return Err(Error::MoveHasEdits(id));
        }
        let mut covered = Vec::new();
        let mut candidates = vec![found.source.unit, found.target.unit];
        candidates.dedup();
        for index in candidates {
            let unit = self.id(index);
            let Some(chunk) = self.unit(unit)?.chunk() else {
                continue;
            };
            let side_covered = |lines: usize, side: crate::analysis::MoveSide| {
                lines == 0 || (side.unit == index && side.offset == 0 && side.len == lines)
            };
            if side_covered(chunk.old.len, found.source)
                && side_covered(chunk.new.len, found.target)
            {
                covered.push(unit);
            }
        }
        Ok(covered)
    }

    /// Returns progress counts per file and in total.
    #[must_use]
    pub fn status(&self) -> Status {
        let mut status = Status {
            revision: self.number,
            total: Counts::default(),
            files: Vec::new(),
        };
        let Some(analysis) = self.analysis() else {
            return status;
        };
        for file in &analysis.files {
            let mut counts = Counts::default();
            for &index in &file.units {
                let id = self.id(index);
                let (Ok(unit), Ok(state)) = (self.unit(id), self.state(id)) else {
                    continue;
                };
                counts.add(state, unit, Some(file));
                status.total.add(state, unit, Some(file));
            }
            status.files.push(FileStatus {
                path: file.display_path().to_owned(),
                counts,
            });
        }
        status
    }
}

fn side_diff(reviewed: &str, current: &str) -> SideDiff {
    let before = split_lines(reviewed);
    let after = split_lines(current);
    let line = |kind, text: &str| {
        let SplitLine { content, ending } = split_line_ending(text);
        DiffLine {
            kind,
            text: content.to_owned(),
            ending,
        }
    };
    let mut lines = Vec::new();
    let mut before_index = 0;
    for hunk in diff_lines(&before, &after) {
        for text in before
            .get(before_index..hunk.before.start)
            .unwrap_or_default()
        {
            lines.push(line(DiffLineKind::Same, text));
        }
        for text in before.get(hunk.before.clone()).unwrap_or_default() {
            lines.push(line(DiffLineKind::Old, text));
        }
        for text in after.get(hunk.after.clone()).unwrap_or_default() {
            lines.push(line(DiffLineKind::New, text));
        }
        before_index = hunk.before.end;
    }
    for text in before.get(before_index..).unwrap_or_default() {
        lines.push(line(DiffLineKind::Same, text));
    }
    SideDiff {
        changed: reviewed != current,
        lines,
    }
}

fn describe_side(side: Option<&EntrySide>) -> String {
    side.map_or_else(
        || "(absent)\n".to_owned(),
        |side| format!("{} {}\n", side.path, describe_entry(side)),
    )
}

/// Describes what a tree entry holds: its type, mode, and the symlink target or submodule commit.
fn describe_entry(side: &EntrySide) -> String {
    use crate::git::EntryType;

    match side.mode.entry_type() {
        EntryType::Symlink => format!(
            "symlink ({}) to \"{}\"",
            side.mode,
            side.link_target.as_deref().unwrap_or("")
        ),
        EntryType::Submodule => format!("submodule at commit {}", side.oid),
        EntryType::File | EntryType::Other => format!("file ({}, blob {})", side.mode, side.oid),
    }
}

/// Describes a file unit in one line, with every value that changed.
pub(crate) fn describe_file_unit(kind: crate::analysis::FileUnit, file: &FileChange) -> String {
    use crate::analysis::FileUnit;

    let old_path = file.old.as_ref().map_or("", |side| side.path.as_str());
    let new_path = file.new.as_ref().map_or("", |side| side.path.as_str());
    let modes = || {
        format!(
            "{} -> {}",
            file.old
                .as_ref()
                .map_or_else(|| "none".to_owned(), |side| side.mode.to_string()),
            file.new
                .as_ref()
                .map_or_else(|| "none".to_owned(), |side| side.mode.to_string()),
        )
    };
    let entries = || {
        let entry =
            |side: Option<&EntrySide>| side.map_or_else(|| "nothing".to_owned(), describe_entry);
        format!(
            "{} -> {}",
            entry(file.old.as_ref()),
            entry(file.new.as_ref())
        )
    };
    let targets = || {
        let target = |side: Option<&EntrySide>| {
            side.and_then(|side| side.link_target.as_deref())
                .map_or_else(|| "(none)".to_owned(), |target| format!("\"{target}\""))
        };
        format!(
            "{} -> {}",
            target(file.old.as_ref()),
            target(file.new.as_ref())
        )
    };
    let commits = || {
        let commit = |side: Option<&EntrySide>| {
            side.map_or_else(|| "(none)".to_owned(), |side| side.oid.to_string())
        };
        format!(
            "{} -> {}",
            commit(file.old.as_ref()),
            commit(file.new.as_ref())
        )
    };
    match kind {
        FileUnit::Rename => format!("renamed from {old_path} to {new_path}"),
        FileUnit::Mode => format!("mode changed {}", modes()),
        FileUnit::Type => format!("type changed: {}", entries()),
        FileUnit::Submodule => format!("submodule commit changed: {}", commits()),
        FileUnit::Symlink => format!("symlink target changed: {}", targets()),
        FileUnit::Binary => format!("binary content changed: {}", entries()),
        FileUnit::Oversized => format!("content too large to diff changed: {}", entries()),
        FileUnit::NotUtf8 => format!("non-UTF-8 content changed: {}", entries()),
        FileUnit::EmptyAdded => "empty file added".to_owned(),
        FileUnit::EmptyDeleted => "empty file deleted".to_owned(),
        FileUnit::Other => format!("entry changed: {}", entries()),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}
