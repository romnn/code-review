//! A read-only view of one revision: its units with their states, interdiffs, notes, moves and
//! progress counts.

use serde::Serialize;

use crate::analysis::{
    Analysis, Chunk, EntrySide, FileChange, FileUnit, LineRange, Move, MoveKind, NoiseCategory,
    Unit, UnitKind,
};
use crate::correspond::{LinkKind, same_entry};
use crate::error::Error;
use crate::git::{Blob, BlobReader, ChangeStatus, RawChange};
use crate::history::{History, RevisionData, RevisionRecord, UnitState, Verdict};
use crate::ids::{MoveId, RevisionNumber, UnitId, UnitIndex};
use crate::scope::{Scope, ScopeMap};
use crate::standing::{ReviewPolicy, Standing};
use crate::text::{diff_lines, split_lines};

/// A view of one revision of a review, deciding under its [`ReviewPolicy`] which units still
/// need the reviewer.
///
/// It is the revision's [`Changes`], which it dereferences to, with their review state.
#[derive(Clone, Copy)]
pub struct RevisionView<'h> {
    history: &'h History,
    changes: Changes<'h>,
    policy: ReviewPolicy,
}

/// The changes of one revision as its analysis found them, with no review state: what a front
/// end shows of a revision before it has read the review's history, which alone says where each
/// unit stands.
#[derive(Clone, Copy)]
pub struct Changes<'h> {
    analysis: &'h Analysis,
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
    /// Whether the file's base entry (path, mode or content) differs from the reviewed one's:
    /// the code around the unit changed in the base, even where both sides of the change did not.
    pub base_file_changed: bool,
}

/// What a chunk that changed since it was reviewed shows: only what changed on top of the
/// reviewed change, or, where that cannot show everything the change does now, the whole
/// current change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinceReviewed<'h> {
    /// The reviewed lines against the current head's; see [`Delta`].
    Delta(Delta<'h>),
    /// The whole current change, for this reason.
    Whole(Reopened),
}

/// What changed on top of a reviewed change whose base lines stayed the same, so that the
/// reviewed change and the [runs](Self::runs) together are the whole current change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delta<'h> {
    /// The unit whose change this is.
    pub unit: UnitId,
    /// The unit the reviewed text was approved on: the unit a whole mark was made on, or the
    /// one the last partial approval was.
    pub reviewed: UnitId,
    /// The reviewed text: the head lines a whole mark approved, or those partial approvals
    /// left, each with its terminator.
    pub before: &'h str,
    /// Where `before` comes from.
    pub baseline: ReviewedBaseline<'h>,
    /// The change now.
    pub after: &'h Chunk,
}

/// Where the reviewed text of a [`Delta`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewedBaseline<'h> {
    /// The head lines a whole mark approved, in the head file it was reviewed in, for their
    /// syntax.
    Head(ReviewedHead<'h>),
    /// The text partial approvals left, which is in no file.
    Partial,
}

/// Where a reviewed text is in a head file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewedHead<'h> {
    /// The head file.
    pub entry: &'h EntrySide,
    /// The zero-based line of the text's first line there.
    pub start: usize,
}

impl<'h> Delta<'h> {
    /// The runs of lines that differ, in order; the lines between them are the same in both.
    #[must_use]
    pub fn runs(&self) -> Vec<LineRun<'h>> {
        let (before, after): (&'h str, &'h str) = (self.before, &self.after.added);
        let old = split_lines(before);
        let new = split_lines(after);
        let (old_starts, new_starts) = (line_starts(&old), line_starts(&new));
        diff_lines(&old, &new)
            .into_iter()
            .map(|run| LineRun {
                unit: self.unit,
                old: LineRange {
                    start: run.before.start,
                    len: run.before.len(),
                },
                new: LineRange {
                    start: self.after.new.start + run.after.start,
                    len: run.after.len(),
                },
                removed: lines_of(before, &old_starts, &run.before),
                added: lines_of(after, &new_starts, &run.after),
            })
            .collect()
    }

    /// The reviewed text with `run`, one of [`Self::runs`], approved: its reviewed lines
    /// replaced by its current ones, byte for byte, so line terminators and a missing final
    /// newline stay as they are on each side.
    pub(crate) fn approving(&self, run: &LineRun<'_>) -> String {
        let lines = split_lines(self.before);
        let starts = line_starts(&lines);
        let kept = |range: std::ops::Range<usize>| lines_of(self.before, &starts, &range);
        [
            kept(0..run.old.start),
            run.added,
            kept(run.old.end()..lines.len()),
        ]
        .concat()
    }
}

/// Where each of `lines` starts in their text, and where the text ends.
fn line_starts(lines: &[&str]) -> Vec<usize> {
    std::iter::once(0)
        .chain(lines.iter().scan(0, |end, line| {
            *end += line.len();
            Some(*end)
        }))
        .collect()
}

/// The lines `range` of `text`, whose lines start at `starts`, as one slice of it.
fn lines_of<'t>(text: &'t str, starts: &[usize], range: &std::ops::Range<usize>) -> &'t str {
    let at = |line: usize| starts.get(line).copied().unwrap_or_default();
    text.get(at(range.start)..at(range.end)).unwrap_or_default()
}

/// A run of lines that differ between a reviewed change and the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRun<'h> {
    /// The unit whose change it is, which only its delta knows.
    unit: UnitId,
    /// The lines of the reviewed text, counted from its first line.
    pub old: LineRange,
    /// The lines in the current head.
    pub new: LineRange,
    /// The reviewed lines, each with its terminator.
    pub removed: &'h str,
    /// The current lines, each with its terminator.
    pub added: &'h str,
}

/// A change since review of one unit as a view showed it, for
/// [`Review::approve_run`](crate::Review::approve_run) to approve once the log still has it.
///
/// Two are equal when they are the same change: the same unit, current lines at the same
/// place, and reviewed lines replaced, which is what the engine compares a run with.
/// Where the reviewed lines are in the reviewed text is no part of it: approving another
/// change of the unit before them moves them without changing what the change does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownRun {
    /// The unit.
    pub unit: UnitId,
    /// Its lines in the current head.
    pub new: LineRange,
    /// The reviewed lines, each with its terminator.
    pub removed: String,
    /// The current lines, each with its terminator.
    pub added: String,
}

impl LineRun<'_> {
    /// The run as it is shown, to approve later.
    #[must_use]
    pub fn shown(&self) -> ShownRun {
        ShownRun {
            unit: self.unit,
            new: self.new,
            removed: self.removed.to_owned(),
            added: self.added.to_owned(),
        }
    }

    /// Whether the run is the change `shown` is, as [`ShownRun`]'s equality has it.
    pub(crate) fn is(&self, shown: &ShownRun) -> bool {
        self.shown() == *shown
    }

    /// The run as a chunk with its edit script, to show it as a change; its old lines count
    /// from the reviewed text's first line, as [`Self::old`] does.
    #[must_use]
    pub fn to_chunk(&self) -> Chunk {
        Chunk::of_lines(
            self.old,
            self.new,
            self.removed.to_owned(),
            self.added.to_owned(),
        )
    }
}

/// Why a chunk that changed since it was reviewed shows its whole current change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reopened {
    /// The file's base entry changed: the target branch moved.
    TargetMoved,
    /// The change now affects other base lines than the reviewed one did.
    OtherBaseLines,
    /// The chain back to the reviewed unit is not one-to-one with trusted links, or the head
    /// file is not the same file, so the reviewed lines may be another change's.
    Uncertain,
    /// The change is the same as the reviewed one, but it was reopened in between.
    SameCode,
    /// The reviewer flagged the change as it was, so all of it still needs them.
    Flagged,
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
    /// One plain move covers all its lines; shown as one line naming the move.
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
    /// Never reviewed and not approved automatically, or explicitly marked unseen.
    pub new: usize,
    /// Reviewed before, but changed since.
    pub changed: usize,
    /// Reviewed as seen.
    pub reviewed: usize,
    /// Reviewed and accepted.
    pub accepted: usize,
    /// Reviewed and flagged.
    pub flagged: usize,
    /// Approved automatically as proven formatting-only, without a mark; see [`Standing`].
    pub auto_approved: usize,
    /// In generated, lockfile or snapshot files.
    pub noise: usize,
    /// Proven formatting-only.
    pub formatting_only: usize,
    /// Covered completely by plain moves.
    pub moved: usize,
}

impl Counts {
    fn add(&mut self, standing: Standing, unit: &Unit, file: Option<&FileChange>) {
        self.units += 1;
        *self.slot(standing) += 1;
        self.noise += usize::from(file.is_some_and(|file| file.noise.is_some()));
        self.formatting_only += usize::from(unit.formatting_only);
        self.moved += usize::from(unit.moved);
    }

    /// Adds `other`'s counts to these.
    pub fn merge(&mut self, other: &Self) {
        self.units += other.units;
        self.new += other.new;
        self.changed += other.changed;
        self.reviewed += other.reviewed;
        self.accepted += other.accepted;
        self.flagged += other.flagged;
        self.auto_approved += other.auto_approved;
        self.noise += other.noise;
        self.formatting_only += other.formatting_only;
        self.moved += other.moved;
    }

    /// Returns the number of units that still need review.
    #[must_use]
    pub fn unreviewed(&self) -> usize {
        self.new + self.changed
    }

    /// Returns the number of units that count as reviewed: marked with any verdict, flagged
    /// included, or approved automatically.
    #[must_use]
    pub fn done(&self) -> usize {
        self.reviewed + self.accepted + self.flagged + self.auto_approved
    }

    /// Returns the number of units that count as approved: seen, accepted or approved
    /// automatically.
    #[must_use]
    pub fn approved(&self) -> usize {
        self.reviewed + self.accepted + self.auto_approved
    }

    /// Returns the count a unit that stands at `standing` falls under.
    pub fn slot(&mut self, standing: Standing) -> &mut usize {
        match standing {
            Standing::Open => &mut self.new,
            Standing::AutoApproved => &mut self.auto_approved,
            Standing::Reviewed(Verdict::Seen) => &mut self.reviewed,
            Standing::Reviewed(Verdict::Accepted) => &mut self.accepted,
            Standing::Reviewed(Verdict::Flagged) => &mut self.flagged,
            Standing::Changed => &mut self.changed,
        }
    }
}

/// Counts for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStatus {
    /// The display path.
    pub path: String,
    /// The file's counts.
    pub counts: Counts,
    /// For a renamed file, its path at the base.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    /// How the file changed.
    pub change: FileChangeSummary,
}

impl FileStatus {
    /// The file git found `change`d before its changes are analyzed: its path and how its entry
    /// changed, with nothing counted.
    pub(crate) fn unanalyzed(change: RawChange) -> Option<Self> {
        let kind = ChangeKind::of(change.status);
        let path = change.new.or(change.old)?.path;
        Some(Self {
            path,
            counts: Counts::default(),
            renamed_from: None,
            change: FileChangeSummary {
                kind,
                ..FileChangeSummary::modified()
            },
        })
    }
}

/// Which version of a file a line number refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The base, before the change.
    Base,
    /// The head, after the change.
    Head,
}

/// Where a unit's scope breadcrumb is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ScopeAnchor {
    /// The version of the file.
    pub side: Side,
    /// The zero-based line in that version.
    pub line: usize,
}

/// How a file changed as a whole, as git classifies the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// The file is new.
    Added,
    /// The file was removed.
    Deleted,
    /// The file changed in place.
    Modified,
    /// The file moved to a new path, possibly with edits.
    Renamed,
    /// The entry changed type, for example from a file to a symlink.
    TypeChanged,
}

impl ChangeKind {
    /// How git classified the entry.
    fn of(status: ChangeStatus) -> Self {
        match status {
            ChangeStatus::Added => Self::Added,
            ChangeStatus::Deleted => Self::Deleted,
            ChangeStatus::Modified => Self::Modified,
            ChangeStatus::TypeChanged => Self::TypeChanged,
            ChangeStatus::Renamed { .. } => Self::Renamed,
        }
    }

    /// The one-letter code `git status` uses: `A`, `D`, `M`, `R` or `T`.
    #[must_use]
    pub fn letter(self) -> char {
        match self {
            Self::Added => 'A',
            Self::Deleted => 'D',
            Self::Modified => 'M',
            Self::Renamed => 'R',
            Self::TypeChanged => 'T',
        }
    }
}

/// The character of a file's change, for coloring file lists, ordered from least to most
/// significant so a directory can show its most significant file's tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTone {
    /// A generated file, lockfile or snapshot.
    Generated,
    /// Every chunk is proven formatting-only.
    FormattingOnly,
    /// Plain moves cover every changed line.
    Moved,
    /// Edited in place.
    Modified,
    /// The entry changed type.
    TypeChanged,
    /// Renamed, possibly with edits.
    Renamed,
    /// New.
    Added,
    /// Removed.
    Deleted,
}

/// What kind of change a file has, beyond its review counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChangeSummary {
    /// How the entry changed.
    pub kind: ChangeKind,
    /// The noise category of a generated file, lockfile or snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise: Option<NoiseCategory>,
    /// Whether every chunk is proven formatting-only.
    pub formatting_only: bool,
    /// Whether some of the file's lines are part of a detected move.
    pub moves: bool,
    /// Whether every chunk is covered by one plain move.
    pub moved_only: bool,
    /// The changes that are not line changes, other than the rename itself: binary content,
    /// mode, type, symlink and submodule changes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub entry_changes: Vec<FileUnit>,
}

impl FileChangeSummary {
    /// A plain modification with nothing special about it.
    #[must_use]
    pub fn modified() -> Self {
        Self {
            kind: ChangeKind::Modified,
            noise: None,
            formatting_only: false,
            moves: false,
            moved_only: false,
            entry_changes: Vec::new(),
        }
    }

    /// The tone to color the file with. Deletions, additions and renames show as such even
    /// when generated, except that a new file holding only moved code reads as a move;
    /// otherwise noise and formatting-only changes stay quiet.
    #[must_use]
    pub fn tone(&self) -> FileTone {
        match self.kind {
            ChangeKind::Deleted => FileTone::Deleted,
            ChangeKind::Added if self.moved_only => FileTone::Moved,
            ChangeKind::Added => FileTone::Added,
            ChangeKind::Renamed => FileTone::Renamed,
            ChangeKind::TypeChanged => FileTone::TypeChanged,
            ChangeKind::Modified if self.noise.is_some() => FileTone::Generated,
            ChangeKind::Modified if self.formatting_only => FileTone::FormattingOnly,
            ChangeKind::Modified if self.moved_only => FileTone::Moved,
            ChangeKind::Modified => FileTone::Modified,
        }
    }

    fn of(analysis: &Analysis, index: usize, file: &FileChange) -> Self {
        let kind = ChangeKind::of(file.status);
        let units: Vec<&Unit> = file
            .units
            .iter()
            .filter_map(|unit| analysis.units.get(unit.get()))
            .collect();
        let moves = analysis.moves.iter().any(|moved| {
            [moved.source.unit, moved.target.unit].iter().any(|unit| {
                analysis
                    .units
                    .get(unit.get())
                    .is_some_and(|unit| unit.file == index)
            })
        });
        let chunks: Vec<&Unit> = units
            .iter()
            .copied()
            .filter(|unit| unit.chunk().is_some())
            .collect();
        let mut entry_changes: Vec<FileUnit> = units
            .iter()
            .filter_map(|unit| match unit.kind {
                UnitKind::File(FileUnit::Rename) | UnitKind::Chunk(_) => None,
                UnitKind::File(change) => Some(change),
            })
            .collect();
        entry_changes.dedup();
        Self {
            kind,
            noise: file.noise,
            formatting_only: file.formatting_only,
            moves,
            moved_only: !chunks.is_empty() && chunks.iter().all(|unit| unit.moved),
            entry_changes,
        }
    }
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
    pub(crate) fn new(history: &'h History, changes: Changes<'h>, policy: ReviewPolicy) -> Self {
        Self {
            history,
            changes,
            policy,
        }
    }

    /// Returns the policy this view decides under: the one given to [`History::view_under`],
    /// or [`ReviewPolicy::MARKS_ONLY`] for [`History::view`].
    #[must_use]
    pub fn policy(&self) -> ReviewPolicy {
        self.policy
    }

    fn data(&self) -> Option<&'h RevisionData> {
        self.history.revision(self.number)
    }

    /// Returns the history this view belongs to.
    #[must_use]
    pub fn history(&self) -> &'h History {
        self.history
    }

    /// Returns whether this revision is provisional: the working tree as captured for a live
    /// review, shown but not recorded; see [`History::is_provisional`].
    #[must_use]
    pub fn is_provisional(&self) -> bool {
        self.history.is_provisional(self.number)
    }

    /// Returns the revision's record.
    #[must_use]
    pub fn record(&self) -> Option<&'h RevisionRecord> {
        self.data().map(|data| &data.record)
    }
}

impl<'h> std::ops::Deref for RevisionView<'h> {
    type Target = Changes<'h>;

    fn deref(&self) -> &Changes<'h> {
        &self.changes
    }
}

impl<'h> Changes<'h> {
    /// The changes `analysis` found, as revision `number` has them.
    #[must_use]
    pub fn of(analysis: &'h Analysis, number: RevisionNumber) -> Self {
        Self { analysis, number }
    }

    /// Returns the revision's number.
    #[must_use]
    pub fn number(&self) -> RevisionNumber {
        self.number
    }

    /// Returns the revision's analysis.
    #[must_use]
    pub fn analysis(&self) -> &'h Analysis {
        self.analysis
    }

    /// Returns each changed file, with nothing counted.
    #[must_use]
    pub fn files(&self) -> Vec<FileStatus> {
        self.analysis
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| FileStatus {
                path: file.display_path().to_owned(),
                counts: Counts::default(),
                renamed_from: match (&file.old, &file.new) {
                    (Some(old), Some(new)) if old.path != new.path => Some(old.path.clone()),
                    _ => None,
                },
                change: FileChangeSummary::of(self.analysis, index, file),
            })
            .collect()
    }

    /// Returns the IDs of every unit, in file and position order.
    #[must_use]
    pub fn unit_ids(&self) -> Vec<UnitId> {
        (0..self.analysis.units.len())
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
        self.analysis.unit(index).ok_or(Error::UnknownUnit(id))
    }

    /// Returns the file of unit `id`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn file(&self, id: UnitId) -> Result<&'h FileChange, Error> {
        let unit = self.unit(id)?;
        self.analysis.file_of(unit).ok_or(Error::UnknownUnit(id))
    }

    /// Returns the digest of unit `id`'s full evidence.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn evidence_digest(&self, id: UnitId) -> Result<String, Error> {
        let index = self.index(id)?;
        self.analysis
            .evidence_digest(index)
            .ok_or(Error::UnknownUnit(id))
    }
}

impl RevisionView<'_> {
    /// Returns the review state of unit `id`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Changes::unit`].
    pub fn state(&self, id: UnitId) -> Result<UnitState, Error> {
        let index = self.index(id)?;
        self.data()
            .and_then(|data| data.states.get(index.get()).copied())
            .ok_or(Error::UnknownUnit(id))
    }

    /// Returns where unit `id` stands with the reviewer under this view's policy: its
    /// [state](Self::state), with a proven formatting-only unit that no mark decides approved
    /// automatically when the policy says so.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Changes::unit`].
    pub fn standing(&self, id: UnitId) -> Result<Standing, Error> {
        let state = self.state(id)?;
        let unit = self.unit(id)?;
        Ok(Standing::of(state, unit.formatting_only, self.policy))
    }
}

impl Changes<'_> {
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
        if unit.moved
            && let Some(chunk) = unit.chunk()
        {
            let covering = self.analysis.moves.iter().enumerate().find(|(_, found)| {
                found.kind == MoveKind::Moved && found.covers(id.index(), chunk)
            });
            if let Some(found) = covering
                .and_then(|(index, found)| self.move_view(MoveId::new(self.number, index), found))
            {
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

    /// Returns the complete old and new tree entries of file unit `id`, each described with
    /// every value (path, type, mode, blob, symlink target or submodule commit) and `nothing`
    /// for an absent side, or `None` for a chunk. Front ends show these as the evidence of a
    /// change that has no lines.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::unit`].
    pub fn entries(&self, id: UnitId) -> Result<Option<(String, String)>, Error> {
        let unit = self.unit(id)?;
        let file = self.file(id)?;
        let entry = |side: Option<&EntrySide>| {
            side.map_or_else(
                || "nothing".to_owned(),
                |side| format!("{} {}", side.path, describe_entry(side)),
            )
        };
        Ok(match &unit.kind {
            UnitKind::Chunk(_) => None,
            UnitKind::File(_) => Some((entry(file.old.as_ref()), entry(file.new.as_ref()))),
        })
    }
}

impl<'h> RevisionView<'h> {
    /// Returns the interdiff of unit `id` against its last reviewed predecessor, if it is
    /// changed since reviewed; the added side compares with the text partial approvals left,
    /// when they were the last review.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Changes::unit`].
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
                side_diff(
                    self.history
                        .approved_text(reviewed)
                        .unwrap_or(&before.added),
                    &after.added,
                ),
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
            base_file_changed: !same_entry(reviewed_file.old.as_ref(), current_file.old.as_ref()),
        }))
    }

    /// Returns what chunk unit `id` shows when it changed since it was reviewed: only what
    /// changed on top of the reviewed change when its base lines and file stayed the same and
    /// every link back to the reviewed unit is one-to-one and trusted, and else the whole
    /// current change; `None` for any other unit.
    ///
    /// The reviewed change is the text partial approvals left when one was the last review,
    /// and else the reviewed unit's head lines.
    /// A current change equal to it after a changed link shows whole as
    /// [`Reopened::SameCode`]: only a mark approves it.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Changes::unit`].
    pub fn since_reviewed(&self, id: UnitId) -> Result<Option<SinceReviewed<'h>>, Error> {
        let UnitState::ChangedSinceReviewed { reviewed, verdict } = self.state(id)? else {
            return Ok(None);
        };
        let current_file = self.file(id)?;
        let reviewed_view = self.history.view(reviewed.revision())?;
        let reviewed_file = reviewed_view.file(reviewed)?;
        let (UnitKind::Chunk(before), UnitKind::Chunk(after)) =
            (&reviewed_view.unit(reviewed)?.kind, &self.unit(id)?.kind)
        else {
            return Ok(None);
        };
        let approved = self.history.approved_text(reviewed);
        let reviewed_text = approved.unwrap_or(&before.added);
        let reviewed_head = match (&reviewed_file.new, &current_file.new) {
            (Some(older), Some(newer)) if older.path == newer.path && older.mode == newer.mode => {
                Some(older)
            }
            _ => None,
        };
        let reopened = if verdict == Verdict::Flagged {
            Some(Reopened::Flagged)
        } else if !same_entry(reviewed_file.old.as_ref(), current_file.old.as_ref()) {
            Some(Reopened::TargetMoved)
        } else if before.old != after.old {
            Some(Reopened::OtherBaseLines)
        } else if reviewed_head.is_none() || !self.trusted_chain(id, reviewed) {
            Some(Reopened::Uncertain)
        } else if reviewed_text == after.added {
            Some(Reopened::SameCode)
        } else {
            None
        };
        Ok(Some(match (reopened, reviewed_head) {
            (None, Some(entry)) => SinceReviewed::Delta(Delta {
                unit: id,
                reviewed,
                before: reviewed_text,
                baseline: match approved {
                    Some(_) => ReviewedBaseline::Partial,
                    None => ReviewedBaseline::Head(ReviewedHead {
                        entry,
                        start: before.new.start,
                    }),
                },
                after,
            }),
            (reopened, _) => SinceReviewed::Whole(reopened.unwrap_or(Reopened::Uncertain)),
        }))
    }

    /// Whether every link from unit `id` back to unit `reviewed` is the only link of both its
    /// ends and says the change is unchanged or changed, never untrusted or ambiguous.
    fn trusted_chain(&self, id: UnitId, reviewed: UnitId) -> bool {
        let mut current = id;
        while current != reviewed {
            let [link] = self.history.predecessors(current) else {
                return false;
            };
            let Some(revision) = current.revision().previous() else {
                return false;
            };
            let older = UnitId::new(revision, link.older);
            if !matches!(link.kind, LinkKind::Unchanged | LinkKind::Changed)
                || self.history.successors(older).len() != 1
                || revision < reviewed.revision()
            {
                return false;
            }
            current = older;
        }
        true
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
}

impl Changes<'_> {
    /// Returns every move of this revision.
    #[must_use]
    pub fn moves(&self) -> Vec<MoveView> {
        self.analysis
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
            .then(|| self.analysis.moves.get(id.index()))
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
            if self
                .unit(unit)?
                .chunk()
                .is_some_and(|chunk| found.covers(index, chunk))
            {
                covered.push(unit);
            }
        }
        Ok(covered)
    }

    /// Returns the units of exactly the file shown at `path` (its head path, or its base path
    /// when it was deleted), never of files below a directory of that name. A selected file
    /// resolves through this; folder operations use [`Self::units_under`].
    #[must_use]
    pub fn units_of_file(&self, path: &str) -> Vec<UnitId> {
        self.analysis
            .files
            .iter()
            .filter(|file| file.display_path() == path)
            .flat_map(|file| file.units.iter().map(|&index| self.id(index)))
            .collect()
    }

    /// Returns every unit of the files at `paths`, or below them when a path is a directory,
    /// matching both the head and the base path of renamed files.
    #[must_use]
    pub fn units_under(&self, paths: &[String]) -> Vec<UnitId> {
        let matches = |path: &str| paths.iter().any(|node| crate::tree::is_under(path, node));
        self.analysis
            .files
            .iter()
            .filter(|file| {
                [&file.old, &file.new]
                    .into_iter()
                    .flatten()
                    .any(|side| matches(&side.path))
            })
            .flat_map(|file| file.units.iter().map(|&index| self.id(index)))
            .collect()
    }

    /// Where a chunk unit's scope breadcrumb is taken: its first added line at the head, or, for
    /// a deletion-only chunk, its first removed line at the base. `None` for file units.
    ///
    /// # Errors
    ///
    /// Returns an error when the unit does not exist in this revision.
    pub fn scope_anchor(&self, unit: UnitId) -> Result<Option<ScopeAnchor>, Error> {
        let record = self.unit(unit)?;
        Ok(record.chunk().map(|chunk| {
            if chunk.new.len > 0 {
                ScopeAnchor {
                    side: Side::Head,
                    line: chunk.new.start,
                }
            } else {
                ScopeAnchor {
                    side: Side::Base,
                    line: chunk.old.start,
                }
            }
        }))
    }

    /// The named scopes around `unit`, outermost first, such as `impl Ledger` and
    /// `fn apply_batch`, read from the file at the side [`Self::scope_anchor`] picks. Empty for
    /// file units, unsupported languages and unreadable blobs.
    ///
    /// # Errors
    ///
    /// Returns an error when the unit does not exist in this revision or the blob cannot be read.
    pub fn scope(&self, unit: UnitId, reader: &mut BlobReader) -> Result<Vec<Scope>, Error> {
        let Some(anchor) = self.scope_anchor(unit)? else {
            return Ok(Vec::new());
        };
        let file = self.file(unit)?;
        let side = match anchor.side {
            Side::Head => file.new.as_ref(),
            Side::Base => file.old.as_ref(),
        };
        let Some(side) = side else {
            return Ok(Vec::new());
        };
        let Blob::Content(bytes) = reader.read(&side.oid)? else {
            return Ok(Vec::new());
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return Ok(Vec::new());
        };
        let map = ScopeMap::for_path(&side.path, text);
        Ok(map.enclosing(anchor.line).into_iter().cloned().collect())
    }

    /// Returns how the file of `unit` changed.
    ///
    /// # Errors
    ///
    /// Returns an error when the unit does not exist in this revision.
    pub fn file_change(&self, unit: UnitId) -> Result<FileChangeSummary, Error> {
        let index = self.unit(unit)?.file;
        let file = self
            .analysis
            .files
            .get(index)
            .ok_or(Error::UnknownUnit(unit))?;
        Ok(FileChangeSummary::of(self.analysis, index, file))
    }
}

impl RevisionView<'_> {
    /// Returns progress counts per file and in total, under this view's policy.
    #[must_use]
    pub fn status(&self) -> Status {
        let mut total = Counts::default();
        let mut files = self.files();
        for (status, file) in files.iter_mut().zip(&self.analysis.files) {
            for &index in &file.units {
                let id = self.id(index);
                let (Ok(unit), Ok(standing)) = (self.unit(id), self.standing(id)) else {
                    continue;
                };
                status.counts.add(standing, unit, Some(file));
                total.add(standing, unit, Some(file));
            }
        }
        Status {
            revision: self.number,
            total,
            files,
        }
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
