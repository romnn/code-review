//! Lays a step's units out as single-line rows for the virtualized list, in file sections.
//!
//! Every decision about what a unit shows (collapse, moves, what changed since a review,
//! descriptions, notes) comes from review-core; this module only lays those answers out as
//! lines, and applies the view's folds: collapsed files and hunks, expanded moves, and files
//! shown in full.
//!
//! [`build`] lays out the unified rows; [`present`](fn@present) then projects them for the
//! layout and density the reviewer chose, pairing lines side by side and adding air, without
//! deciding anything new about what shows; [`project`] lays those rows out on the screen rows
//! the list scrolls, wrapping long lines when asked.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use crate::config::{Approved, DiffLayout, DiffView};
use crate::paint::display::display_width;
use crate::syntax::FileSyntax;
use review_core::analysis::{Chunk, FileUnit, UnitKind};
use review_core::scope::breadcrumb;
use review_core::{
    Changes, Collapse, Delta, LineEnding, LineRun, MoveView, NoteAttachment, Reopened,
    ResolvedNote, ReviewedBaseline, RevisionView, ShownRun, Side, SideDiff, SinceReviewed,
    Standing, UnitId, UnitState, Verdict, split_line_ending,
};

mod moves;
mod once;
mod present;
mod wrap;

pub use present::{Presentation, present};
pub use wrap::{ScreenRow, WrapRoom, project};

/// What rows are laid out from: a revision of the review's history, or, before the review is
/// verified, the changes of its last recorded revision alone, which stand nowhere: nothing in
/// them shows reviewed, folds by review state, shows what changed since a review or carries a
/// note.
#[derive(Clone, Copy)]
pub enum Source<'h> {
    /// A revision of the review's history.
    Verified(RevisionView<'h>),
    /// The changes alone of a revision whose review state is still being checked.
    Pending(Changes<'h>),
}

impl<'h> std::ops::Deref for Source<'h> {
    type Target = Changes<'h>;

    fn deref(&self) -> &Changes<'h> {
        match self {
            Self::Verified(view) => view,
            Self::Pending(changes) => changes,
        }
    }
}

impl<'h> Source<'h> {
    /// The revision of the review's history, once it is verified.
    #[must_use]
    pub fn review(&self) -> Option<&RevisionView<'h>> {
        match self {
            Self::Verified(view) => Some(view),
            Self::Pending(_) => None,
        }
    }

    /// Where `unit` stands with the reviewer, once that is known.
    fn standing(&self, unit: UnitId) -> Option<Standing> {
        self.review()?.standing(unit).ok()
    }

    /// `unit`'s review state, once it is known.
    fn state(&self, unit: UnitId) -> Option<UnitState> {
        self.review()?.state(unit).ok()
    }

    /// What changed since `unit` was reviewed, once that is known.
    fn since_reviewed(&self, unit: UnitId) -> Option<SinceReviewed<'h>> {
        self.review()?.since_reviewed(unit).ok().flatten()
    }

    /// The notes on the revision, once they are read.
    fn notes(&self) -> Vec<ResolvedNote> {
        self.review().map(RevisionView::notes).unwrap_or_default()
    }
}

/// What a row shows, which decides its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// The start of a file section: path, stats and state, drawn from [`FileSection`].
    FileHeader,
    /// Space between file sections.
    Spacer,
    /// A unit's header: ID, state and lines.
    Header,
    /// A removed line.
    Removed,
    /// An added line.
    Added,
    /// An unchanged line of a file shown in full.
    Context,
    /// A line of the head that a hunk changed in place, shown once: the tokens it inserted
    /// marked, and the ones it deleted struck out where they stood.
    Edited,
    /// An old line of a move's inner diff.
    NestedOld,
    /// A new line of a move's inner diff.
    NestedNew,
    /// An unchanged line of a move's inner diff.
    NestedSame,
    /// A one-line summary: a file unit's description or a hint.
    Info,
    /// A move: where the code went or came from.
    Moved,
    /// A collapsed formatting-only or noise unit.
    Collapsed,
    /// A note.
    Note,
    /// Unchanged lines left out between hunks, which expand on request.
    Gap,
    /// A base line beside a head line, in the side-by-side layout; see [`Pair`].
    Pair,
}

/// What a row folds or unfolds when toggled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fold {
    /// A file section, by index into [`Layout::files`].
    File(usize),
    /// A collapsed move, formatting-only or noise unit, which expands to show its code.
    Expand(UnitId),
    /// Unchanged lines left out before the head line `at` of a file section.
    Gap {
        /// The file section, by index into [`Layout::files`].
        file: usize,
        /// The head line the gap ends at.
        at: usize,
    },
}

/// A line of the head file, where go-to-definition can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadLine {
    /// The repository-relative path in the head snapshot.
    pub path: String,
    /// The zero-based line.
    pub line: usize,
}

/// A line of code: one row of the unified layout, or one side of a [`Pair`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// What the line is: removed, added, unchanged, or a line of a nested diff.
    pub kind: RowKind,
    /// The line, without a diff sign, with a line-ending marker after the code.
    pub text: String,
    /// The length of the code in `text`, before any line-ending marker.
    pub content_len: usize,
    /// Byte ranges of `text` that a token diff marks as really changed; on an edited line, the
    /// tokens it inserted.
    pub highlights: Vec<Range<usize>>,
    /// On an edited line, the byte ranges of `text` that hold deleted text, which is no line's
    /// code: copying leaves it out, and the line's own bytes follow it.
    pub deleted: Vec<Range<usize>>,
    /// For a line of the head file, where it is.
    pub head: Option<HeadLine>,
    /// For a line of the base file, its zero-based line there: a removed line, or the base side
    /// of an unchanged line.
    pub base: Option<usize>,
    /// For a line of a formatting-only change, whether to draw it without emphasis.
    pub quiet: bool,
    /// Whether the line is moved code: a moved line at either end of a move, or a line a move
    /// with edits keeps, which draw in the move's color rather than as removed or added.
    pub moved: bool,
    /// For a line of a change as it was reviewed, where it is in the head reviewed then, for
    /// its syntax: it has no line of the current files.
    pub reviewed: Option<ReviewedLine>,
}

/// A line of a head file as it was reviewed at an earlier revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewedLine {
    /// The unit that was reviewed, on a recorded revision, so its ID never names another.
    pub unit: UnitId,
    /// That head file.
    pub side: crate::state::SideKey,
    /// The zero-based line there.
    pub line: usize,
}

impl Cell {
    /// The code without its line-ending marker, which copying takes.
    #[must_use]
    pub fn code(&self) -> &str {
        self.text.get(..self.content_len).unwrap_or(&self.text)
    }

    /// How many cells of the monospace grid the line takes.
    #[must_use]
    pub fn width(&self) -> usize {
        display_width(&self.text)
    }

    /// The line's own code, without the deleted text an edited line shows.
    #[must_use]
    pub fn own_code(&self) -> String {
        let code = self.code();
        let mut own = String::with_capacity(code.len());
        let mut at = 0;
        for range in &self.deleted {
            own.push_str(code.get(at..range.start).unwrap_or_default());
            at = range.end;
        }
        own.push_str(code.get(at..).unwrap_or_default());
        own
    }

    /// Where byte `at` of the line's own code lies in `text`, past the deleted text spliced in
    /// before it: at a splice, a `start` lies after the deleted text and an end before it.
    #[must_use]
    pub fn shown_at(&self, at: usize, start: bool) -> usize {
        let mut moved = 0;
        for range in &self.deleted {
            // Where the deleted text stands in the line's own code.
            let stands = range.start - moved;
            if stands < at || (start && stands == at) {
                moved += range.len();
            }
        }
        at + moved
    }
}

/// One of the tool's own lines: a header, a gap, a note, a move or a description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// What the line is.
    pub kind: RowKind,
    /// Its text.
    pub text: String,
    /// For a header, the review state badge of its first unit.
    pub badge: Option<Badge>,
    /// For a move, the unit at its other end.
    pub link: Option<UnitId>,
    /// What toggling this row folds.
    pub fold: Option<Fold>,
    /// For a unit header, the named scopes around the unit, such as `impl Ledger › fn apply`.
    pub scope: Option<String>,
    /// For a unit header, a short label of what kind of change it is, such as
    /// "reformatted only", "reformatted only · auto-approved", "whitespace changed · not
    /// proven equivalent" or "changed since revision 1".
    pub tag: Option<String>,
    /// For a hunk header, whether the hunk shows its header alone.
    pub folded: bool,
}

impl Note {
    /// The line `kind` showing `text`, and nothing more.
    fn plain(kind: RowKind, text: String) -> Self {
        Self {
            kind,
            text,
            badge: None,
            link: None,
            fold: None,
            scope: None,
            tag: None,
            folded: false,
        }
    }

    /// What Enter does on the row, shown after its text: following a move to its other end,
    /// or showing a collapsed unit's code.
    #[must_use]
    pub fn hint(&self) -> Option<&'static str> {
        match self.kind {
            RowKind::Moved if self.link.is_some() => Some("⏎ follow"),
            RowKind::Collapsed => Some("⏎ shows the code"),
            _ => None,
        }
    }

    /// The most cells the note's row draws in from where code starts: its text, then its tag,
    /// innermost scope and hint, each with at most three cells before it.
    ///
    /// Outer scopes count for nothing, since they are cut first when room runs out; whatever
    /// else a row says, the code pans far enough for its end to show.
    #[must_use]
    pub fn width(&self) -> usize {
        let extra = |text: Option<&str>| text.map_or(0, |text| display_width(text) + 3);
        let inner = self
            .scope
            .as_deref()
            .map(|scope| scope.rsplit(" › ").next().unwrap_or(scope));
        display_width(&self.text) + extra(self.tag.as_deref()) + extra(inner) + extra(self.hint())
    }
}

/// A base line beside a head line.
///
/// A side without a line is padding, never a blank line: a real empty line is a cell with empty
/// text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pair {
    /// The removed or unchanged line of the base.
    pub left: Option<Cell>,
    /// The added or unchanged line of the head.
    pub right: Option<Cell>,
}

/// What a row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A line of code across the whole width.
    Code(Cell),
    /// Two lines of code side by side.
    Pair(Pair),
    /// One of the tool's own lines, across the whole width.
    Note(Note),
}

/// One display row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The file section the row belongs to, by index into [`Layout::files`].
    pub file: usize,
    /// The unit the row belongs to; for a file header, the file's first unit.
    pub unit: UnitId,
    /// The display hunk the row belongs to, by index into [`Layout::hunks`]: set only for a
    /// unit's header and body, never for file headers, gaps, notes or unchanged context, so a
    /// hunk command can never reach a hunk through a row that is not part of it.
    pub hunk: Option<usize>,
    /// The first row of the unified layout this row shows, which stays the same whatever the
    /// presentation; see [`Layout::projected`].
    pub source: usize,
    /// What the row shows.
    pub body: Body,
}

impl Row {
    /// What the row shows, which decides its color: its cell's kind, [`RowKind::Pair`], or its
    /// note's kind.
    #[must_use]
    pub fn kind(&self) -> RowKind {
        match &self.body {
            Body::Code(cell) => cell.kind,
            Body::Pair(_) => RowKind::Pair,
            Body::Note(note) => note.kind,
        }
    }

    /// Whether the row shows code, which copying and selecting take.
    #[must_use]
    pub fn is_code(&self) -> bool {
        matches!(self.body, Body::Code(_) | Body::Pair(_))
    }

    /// The row's note, when it is one of the tool's own lines.
    #[must_use]
    pub fn note(&self) -> Option<&Note> {
        match &self.body {
            Body::Note(note) => Some(note),
            Body::Code(_) | Body::Pair(_) => None,
        }
    }

    /// The row's lines of code: one across the width, or each side of a pair that has one.
    pub fn cells(&self) -> impl Iterator<Item = &Cell> {
        let (first, second) = match &self.body {
            Body::Code(cell) => (Some(cell), None),
            Body::Pair(pair) => (pair.left.as_ref(), pair.right.as_ref()),
            Body::Note(_) => (None, None),
        };
        first.into_iter().chain(second)
    }

    /// The row's lines of code by side, as [`ScreenRow::shown`] names them: the line across
    /// the width or a pair's left line, then a pair's right line.
    #[must_use]
    pub fn sides(&self) -> [Option<&Cell>; 2] {
        match &self.body {
            Body::Code(cell) => [Some(cell), None],
            Body::Pair(pair) => [pair.left.as_ref(), pair.right.as_ref()],
            Body::Note(_) => [None, None],
        }
    }

    /// The row as a unified code row's or a note's text; a pair has none of its own.
    #[must_use]
    pub fn text(&self) -> &str {
        match &self.body {
            Body::Code(cell) => &cell.text,
            Body::Note(note) => &note.text,
            Body::Pair(_) => "",
        }
    }
}

/// A unit's review state as its header shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Needs review: new, or explicitly unseen.
    Unreviewed,
    /// Seen, possibly carried from an earlier revision.
    Seen,
    /// Accepted, possibly carried.
    Accepted,
    /// Approved automatically as proven formatting-only, without a mark.
    Auto,
    /// Flagged, possibly carried.
    Flagged,
    /// Reviewed in an earlier revision, but changed since.
    Changed,
}

impl Badge {
    /// The badge of a unit that stands at `standing`.
    #[must_use]
    pub fn of(standing: Standing) -> Self {
        match standing {
            Standing::Open => Self::Unreviewed,
            Standing::AutoApproved => Self::Auto,
            Standing::Reviewed(verdict) => match verdict {
                Verdict::Seen => Self::Seen,
                Verdict::Accepted => Self::Accepted,
                Verdict::Flagged => Self::Flagged,
            },
            Standing::Changed => Self::Changed,
        }
    }

    /// Whether the badge counts as reviewed: marked with any verdict or approved
    /// automatically.
    #[must_use]
    pub fn is_reviewed(self) -> bool {
        matches!(
            self,
            Self::Seen | Self::Accepted | Self::Auto | Self::Flagged
        )
    }
}

/// A file section of the list: what its sticky header shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSection {
    /// The path at the head, or at the base for a deletion.
    pub path: String,
    /// The path at the base, when the file was renamed.
    pub old_path: Option<String>,
    /// Added lines over the section's units.
    pub added: usize,
    /// Removed lines over the section's units.
    pub removed: usize,
    /// Reviewed units of the section.
    pub reviewed: usize,
    /// Units of the section.
    pub total: usize,
    /// Flagged units of the section.
    pub flagged: usize,
    /// Units changed since they were reviewed.
    pub changed: usize,
    /// Whether every chunk of the file is proven formatting-only.
    pub formatting_only: bool,
    /// Whether code moved into or out of the file.
    pub moved: bool,
    /// The noise category, such as "generated" or "lockfile".
    pub noise: Option<&'static str>,
    /// The header's row.
    pub header_row: usize,
    /// Whether the section is folded to its header.
    pub collapsed: bool,
    /// Whether the file exists at the head, so it can be shown in full.
    pub has_head: bool,
    /// The head file's line count, once its text has loaded.
    pub lines: Option<usize>,
    /// The section's units, in review order.
    pub units: Vec<UnitId>,
    /// How the file changed, for its kind letter and color.
    pub change: Option<review_core::FileChangeSummary>,
}

/// A file's text, for showing it in full.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FullText {
    /// The head file's lines, without terminators.
    pub lines: Vec<String>,
    /// The index of the longest line, which sets the width a list of the lines scrolls to.
    pub longest: usize,
}

impl FullText {
    /// `text` split into lines.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let longest = lines
            .iter()
            .enumerate()
            .max_by_key(|(_, line)| display_width(line))
            .map_or(0, |(index, _)| index);
        Self { lines, longest }
    }
}

/// The cells a tab takes.
pub const TAB_CELLS: usize = 4;

/// The view's folds, which [`build`] applies.
#[derive(Debug, Clone)]
pub struct Folds {
    /// Unchanged lines shown around each hunk.
    pub context: usize,
    /// Head files' text, by path, for context lines.
    pub texts: HashMap<String, Arc<FullText>>,
    /// Extra unchanged lines revealed in a gap, by path and the head line the gap ends at.
    pub gaps: HashMap<(String, usize), usize>,
    /// Files folded or unfolded by the reviewer, by path; others fold when fully reviewed.
    pub files: HashMap<String, bool>,
    /// Units whose bodies the reviewer folded.
    pub units: HashSet<UnitId>,
    /// Approved units the reviewer unfolded; other approved units show only their header,
    /// unless `approved` shows them all.
    pub opened: HashSet<UnitId>,
    /// Whether approved units and fully reviewed files fold by default or show like any other,
    /// in the hunks view; the file view shows them all.
    pub approved: Approved,
    /// How much of a file its section shows: the whole file without hunk headers, or each hunk
    /// with a few lines around it.
    pub view: DiffView,
    /// The layout the rows are presented in: unified shows a changed line once where it can,
    /// side by side always both versions.
    pub layout: DiffLayout,
    /// Whether the layout is one file's diff, without file headers: an empty row stands in for
    /// its header.
    pub single_file: bool,
    /// Marks the reviewer made that are still saving, by unit: an approval folds its hunk the
    /// moment it is made, so the rows change only when the reviewer acts, never later when the
    /// save lands.
    pub pending: HashMap<UnitId, review_core::MarkVerdict>,
    /// Moves, formatting-only and noise units expanded to show their code.
    pub expanded: HashSet<UnitId>,
    /// Both versions' highlights and scopes, by display path, once they are computed.
    pub syntax: HashMap<String, Arc<FileSyntax>>,
    /// What the selected changed PDF's unit shows, once its pages are read.
    pub pdf: Option<PdfRows>,
}

/// What a changed PDF's opaque unit shows: its page navigator and the text diff of the page
/// pair it selects, or why it shows as a binary file.
#[derive(Debug, Clone, PartialEq)]
pub struct PdfRows {
    /// The PDF's display path.
    pub path: String,
    /// What its unit shows.
    pub shows: PdfShows,
}

/// What the rows of a PDF's page pair show.
#[derive(Debug, Clone, PartialEq)]
pub enum PageRows {
    /// The changes from the base page's text to the head page's, in order.
    Edits(Arc<Vec<Chunk>>),
    /// Passages that only moved, at their places in the head page.
    Moved(Arc<Vec<MovedPassage>>),
}

/// A passage of a PDF page that moved unchanged, as one of [`PageRows::Moved`].
#[derive(Debug, Clone, PartialEq)]
pub struct MovedPassage {
    /// The passage arriving at its place in the head page's rows.
    pub chunk: Chunk,
    /// Its first row in the base page's rows, zero-based.
    pub from: usize,
}

/// What a changed PDF's unit shows.
#[derive(Debug, Clone, PartialEq)]
pub enum PdfShows {
    /// The navigator's line and what the selected page pair's rows show, whose lines are in
    /// no file.
    Page {
        /// The navigator's line, such as `Base 2/8 → Head 4/9 · reordered`.
        navigator: String,
        /// What its rows show.
        rows: PageRows,
    },
    /// Why the PDF shows as a binary file, such as `head is encrypted`.
    Binary(String),
}

impl Folds {
    /// Whether `unit`'s body is folded: when the reviewer folded it, or, in the hunks view, when
    /// it is approved (seen, accepted or approved automatically, and unchanged since, or by a
    /// mark still saving),
    /// approved units fold, and the reviewer has not unfolded it. Flagged and unreviewed units
    /// always show.
    #[must_use]
    pub fn is_folded(&self, view: &Source<'_>, unit: UnitId) -> bool {
        if self.units.contains(&unit) {
            return true;
        }
        if self.view == DiffView::File || self.approved == Approved::Shown {
            return false;
        }
        let approved = match self.pending.get(&unit) {
            Some(verdict) => matches!(
                verdict,
                review_core::MarkVerdict::Accepted | review_core::MarkVerdict::Seen
            ),
            None => view.standing(unit).is_some_and(Standing::is_approved),
        };
        approved && !self.opened.contains(&unit)
    }
}

impl Default for Folds {
    fn default() -> Self {
        Self {
            context: 3,
            texts: HashMap::new(),
            gaps: HashMap::new(),
            files: HashMap::new(),
            units: HashSet::new(),
            opened: HashSet::new(),
            approved: Approved::Folded,
            view: DiffView::Hunks,
            layout: DiffLayout::Unified,
            single_file: false,
            pending: HashMap::new(),
            expanded: HashSet::new(),
            syntax: HashMap::new(),
            pdf: None,
        }
    }
}

/// A display hunk: what one stop of `j` and `k` selects and one press of Space marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// The unit it marks.
    pub unit: UnitId,
    /// For one change of a unit that shows what changed since it was reviewed, that change as
    /// the engine made it, which Space approves alone; the unit's other changes are hunks of
    /// their own.
    pub change: Option<ShownRun>,
}

impl Hunk {
    /// The head lines of its change, for one change of a unit; `None` for a whole unit. The
    /// range is empty for a change that only removes lines, at the line after them.
    #[must_use]
    pub fn lines(&self) -> Option<Range<usize>> {
        self.change.as_ref().map(|change| change.new.range())
    }

    /// The head line its change starts at, for one change of a unit; `None` for a whole unit.
    #[must_use]
    pub fn at(&self) -> Option<usize> {
        self.change.as_ref().map(|change| change.new.start)
    }
}

/// The rows of one or more steps.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// The rows in display order.
    pub rows: Vec<Row>,
    /// The display hunks, in row order.
    pub hunks: Vec<Hunk>,
    /// The header row of each unit: the first row of its first hunk.
    pub unit_rows: HashMap<UnitId, usize>,
    /// The file sections in display order.
    pub files: Vec<FileSection>,
    /// The full-width code row and the annotation row that take the most cells.
    ///
    /// Code and annotations render at different font sizes and indents, so the view compares
    /// the two in pixels and measures the wider one to set how far it scrolls sideways.
    pub widest_code: Option<(usize, usize)>,
    /// The annotation row that takes the most cells, as (row, cells) like
    /// [`Self::widest_code`].
    pub widest_annotation: Option<(usize, usize)>,
    /// The most cells a side of a pair takes, which sets how far the side-by-side halves scroll.
    pub widest_half: usize,
    /// How the rows are presented.
    pub presentation: Presentation,
    /// For each row of the unified layout, by [`Row::source`], the row that shows it now; a
    /// place kept by source row comes back to the same line whatever the presentation.
    pub projected: Vec<usize>,
    /// What each display hunk's header says, by index into [`Self::hunks`], whether or not
    /// the hunk has a header row: the file view shows the selected hunk's in the file header.
    pub hunk_notes: Vec<Note>,
    /// The rows on screen, which the list scrolls, as [`project`] lays them out: one for each
    /// row, or more for a line that wraps. Everything else here counts rows, not screen rows.
    pub screen: Vec<ScreenRow>,
    /// For each row, its first screen row, then the count of screen rows: row `r` shows on
    /// `starts[r]..starts[r + 1]`.
    pub starts: Vec<usize>,
    /// The room long lines wrapped at, `None` while they do not wrap.
    pub wrapped: Option<WrapRoom>,
}

impl Layout {
    /// The hunk that is exactly `hunk`, the same unit and the same change, or else where the
    /// reviewer was: [`Self::hunk_of`] its unit at its change's line.
    ///
    /// Only the first is the same review target; the second is a place to read on from.
    #[must_use]
    pub fn hunk_like(&self, hunk: &Hunk) -> Option<usize> {
        self.hunks
            .iter()
            .position(|own| own == hunk)
            .or_else(|| self.hunk_of(hunk.unit, hunk.at()))
    }

    /// The hunk of `unit`: its first, or with `at`, its change at that head line, else its
    /// first change after it, else its last hunk, as when that change was approved or folded.
    #[must_use]
    pub fn hunk_of(&self, unit: UnitId, at: Option<usize>) -> Option<usize> {
        let mut own = self
            .hunks
            .iter()
            .enumerate()
            .filter(|(_, hunk)| hunk.unit == unit);
        let Some(line) = at else {
            return own.next().map(|(index, _)| index);
        };
        let mut last = None;
        for (index, hunk) in own {
            if hunk.at().is_some_and(|start| start >= line) {
                return Some(index);
            }
            last = Some(index);
        }
        last
    }

    /// The row screen row `screen` shows a piece of.
    #[must_use]
    pub fn row_on_screen(&self, screen: usize) -> Option<&Row> {
        self.rows.get(self.screen.get(screen)?.row)
    }

    /// The screen rows row `row` shows on; none past the last row.
    #[must_use]
    pub fn screen_rows(&self, row: usize) -> Range<usize> {
        match (self.starts.get(row), self.starts.get(row + 1)) {
            (Some(&start), Some(&end)) => start..end,
            _ => self.screen.len()..self.screen.len(),
        }
    }

    /// The rows screen rows `screen` show pieces of.
    #[must_use]
    pub fn rows_on_screen(&self, screen: Range<usize>) -> Range<usize> {
        let row = |index: usize| self.screen.get(index).map(|piece| piece.row);
        match (row(screen.start), screen.end.checked_sub(1).and_then(row)) {
            (Some(start), Some(last)) => start..last + 1,
            _ => 0..0,
        }
    }

    /// The first screen row of row `row`, the end of the screen past the last row.
    #[must_use]
    pub fn first_on_screen(&self, row: usize) -> usize {
        self.screen_rows(row).start
    }

    /// Whether screen row `screen` shows the first piece of its row, where the line number and
    /// sign show.
    #[must_use]
    pub fn starts_row(&self, screen: usize) -> bool {
        self.screen
            .get(screen)
            .is_some_and(|piece| self.first_on_screen(piece.row) == screen)
    }

    /// The screen row of row `row` that shows byte `byte` of its line on `side` (0 across the
    /// width or on the left, 1 on the right): the last piece starting at or before it.
    #[must_use]
    pub fn screen_row_of(&self, row: usize, side: usize, byte: usize) -> usize {
        let rows = self.screen_rows(row);
        let start = rows.start;
        rows.rev()
            .find(|&screen| {
                self.screen
                    .get(screen)
                    .and_then(|piece| piece.shown.get(side)?.as_ref())
                    .is_some_and(|shown| shown.start <= byte)
            })
            .unwrap_or(start)
    }
}

/// Lays out `units` (in review order) of `view` as unified rows, applying `folds`.
///
/// The rows' sources, indices and widths are final once [`present`](fn@present) has projected
/// them for a presentation, which the unified, compact one leaves as they are.
#[must_use]
#[tracing::instrument(skip_all, fields(units = units.len()))]
pub fn build(view: &Source<'_>, units: &[UnitId], folds: &Folds) -> Layout {
    let mut layout = Layout::default();
    let notes = view.notes();
    let groups = file_groups(view, units);
    let file_units = file_units(view, &groups);
    for group in groups {
        let file_index = layout.files.len();
        let Some(&first) = group.first() else {
            continue;
        };
        let mut section = file_section(view, &group, folds);
        if folds.single_file {
            section.header_row = layout.rows.len();
            section.collapsed = false;
            let path = section.path.clone();
            layout.files.push(section);
            // An empty row where a folder's diff names the file, so the file's code shows on
            // the same rows alone as first in its folder's diff.
            push_plain(
                &mut layout,
                file_index,
                first,
                RowKind::Spacer,
                String::new(),
            );
            let text = folds.texts.get(&path).cloned();
            section_rows(
                &mut layout,
                view,
                file_index,
                &group,
                file_units.get(&path).map_or(&[][..], Vec::as_slice),
                text.as_deref(),
                folds.context,
                folds,
                &notes,
            );
            continue;
        }
        if file_index > 0 {
            push_plain(
                &mut layout,
                file_index,
                first,
                RowKind::Spacer,
                String::new(),
            );
        }
        section.header_row = layout.rows.len();
        layout.rows.push(note_row(
            file_index,
            first,
            None,
            Note {
                fold: Some(Fold::File(file_index)),
                ..Note::plain(RowKind::FileHeader, section.path.clone())
            },
        ));
        let collapsed = section.collapsed;
        let path = section.path.clone();
        layout.files.push(section);
        if collapsed {
            // Units still need a header row for navigation and marking.
            for &unit in &group {
                layout
                    .unit_rows
                    .entry(unit)
                    .or_insert(layout.rows.len() - 1);
            }
            continue;
        }
        let text = folds.texts.get(&path).cloned();
        let context = folds.context;
        section_rows(
            &mut layout,
            view,
            file_index,
            &group,
            file_units.get(&path).map_or(&[][..], Vec::as_slice),
            text.as_deref(),
            context,
            folds,
            &notes,
        );
    }
    layout
}

/// Every unit of the files `groups` lay out, by display path, from one pass over the
/// revision's files: a section's context leaves out the file's changes in other groups, and a
/// folder's diff has many sections.
fn file_units(view: &Source<'_>, groups: &[Vec<UnitId>]) -> HashMap<String, Vec<UnitId>> {
    let paths: HashSet<&str> = groups
        .iter()
        .filter_map(|group| view.file(*group.first()?).ok())
        .map(review_core::analysis::FileChange::display_path)
        .collect();
    let mut units: HashMap<String, Vec<UnitId>> = HashMap::new();
    for file in &view.analysis().files {
        let path = file.display_path();
        if paths.contains(path) {
            units
                .entry(path.to_owned())
                .or_default()
                .extend(file.units.iter().map(|&index| view.id(index)));
        }
    }
    units
}

/// Splits `units` into runs of consecutive units of one file.
fn file_groups(view: &Source<'_>, units: &[UnitId]) -> Vec<Vec<UnitId>> {
    let mut groups: Vec<Vec<UnitId>> = Vec::new();
    let mut current = None;
    for &unit in units {
        let file = view.unit(unit).ok().map(|unit| unit.file);
        match groups.last_mut() {
            Some(group) if file == current => group.push(unit),
            _ => {
                groups.push(vec![unit]);
                current = file;
            }
        }
    }
    groups
}

/// Summarizes the file of `group` for its header.
fn file_section(view: &Source<'_>, group: &[UnitId], folds: &Folds) -> FileSection {
    let file = group.first().and_then(|&unit| view.file(unit).ok());
    let path = file.map_or_else(String::new, |file| file.display_path().to_owned());
    let old_path = file.and_then(|file| match (&file.old, &file.new) {
        (Some(old), Some(new)) if old.path != new.path => Some(old.path.clone()),
        _ => None,
    });
    let (mut added, mut removed, mut reviewed, mut flagged, mut changed) = (0, 0, 0, 0, 0);
    let mut moved = false;
    for &unit in group {
        if let Ok(record) = view.unit(unit) {
            moved |= record.moved;
            if let Some(chunk) = record.chunk() {
                added += chunk.added.lines().count();
                removed += chunk.removed.lines().count();
            }
        }
        match view.standing(unit).map(Badge::of) {
            Some(Badge::Flagged) => {
                reviewed += 1;
                flagged += 1;
            }
            Some(badge) if badge.is_reviewed() => reviewed += 1,
            Some(Badge::Changed) => changed += 1,
            _ => {}
        }
    }
    let total = group.len();
    // A file whose units all fold folds with them, unless the reviewer chose: a flagged or
    // changed unit never folds, so neither does its file.
    let collapsed = folds.files.get(&path).copied().unwrap_or(
        folds.view == DiffView::Hunks
            && total > 0
            && group.iter().all(|&unit| folds.is_folded(view, unit)),
    );
    FileSection {
        has_head: file.is_some_and(|file| file.new.is_some()),
        lines: folds.texts.get(&path).map(|text| text.lines.len()),
        formatting_only: file.is_some_and(|file| file.formatting_only),
        noise: file
            .and_then(|file| file.noise)
            .map(review_core::analysis::NoiseCategory::name),
        path,
        old_path,
        added,
        removed,
        reviewed,
        total,
        flagged,
        changed,
        moved,
        header_row: 0,
        collapsed,
        units: group.to_vec(),
        change: group.first().and_then(|&unit| view.file_change(unit).ok()),
    }
}

/// A row showing `note`; its source is set when the rows are presented.
fn note_row(file: usize, unit: UnitId, hunk: Option<usize>, note: Note) -> Row {
    Row {
        file,
        unit,
        hunk,
        source: 0,
        body: Body::Note(note),
    }
}

/// A row showing `cell` across the width.
fn code_row(file: usize, unit: UnitId, hunk: Option<usize>, cell: Cell) -> Row {
    Row {
        file,
        unit,
        hunk,
        source: 0,
        body: Body::Code(cell),
    }
}

/// A line of code `kind` with no position, highlights or line-ending marker yet.
fn plain_cell(kind: RowKind, text: String) -> Cell {
    Cell {
        kind,
        content_len: text.len(),
        text,
        highlights: Vec::new(),
        deleted: Vec::new(),
        head: None,
        base: None,
        quiet: false,
        moved: false,
        reviewed: None,
    }
}

fn push_plain(layout: &mut Layout, file: usize, unit: UnitId, kind: RowKind, text: String) {
    layout
        .rows
        .push(note_row(file, unit, None, Note::plain(kind, text)));
}

/// Pushes one row of a unit body.
struct Pusher<'a> {
    layout: &'a mut Layout,
    file: usize,
    unit: UnitId,
    hunk: usize,
    /// Whether the unit's code draws without emphasis, as a formatting-only change does.
    quiet: bool,
    /// Whether the lines pushed now are moved code.
    moved: bool,
    /// Whether a changed line may show once, as the unified layout shows it.
    once: bool,
}

impl Pusher<'_> {
    /// A line of the current unit's code, to adjust before adding it.
    fn cell(&self, kind: RowKind, text: String) -> Cell {
        Cell {
            quiet: self.quiet,
            moved: self.moved,
            ..plain_cell(kind, text)
        }
    }

    /// Adds a line of code of the current unit's hunk.
    fn add_cell(&mut self, cell: Cell) {
        self.layout
            .rows
            .push(code_row(self.file, self.unit, Some(self.hunk), cell));
    }

    /// Adds one of the tool's own lines to the current unit's hunk.
    fn add_note(&mut self, note: Note) {
        self.layout
            .rows
            .push(note_row(self.file, self.unit, Some(self.hunk), note));
    }

    fn push(&mut self, kind: RowKind, text: String) {
        self.add_note(Note::plain(kind, text));
    }

    /// Pushes the lines of one chunk side, with their highlights and line-ending markers;
    /// added lines carry their head-file position, counting from `head`. Lines inside
    /// `moved` show as one linked move row instead, unless the move is expanded.
    fn side(
        &mut self,
        text: &str,
        highlights: &[Range<usize>],
        kind: RowKind,
        head: Option<&HeadLine>,
        base: Option<usize>,
        moved: Option<&MovedSpan<'_>>,
    ) {
        // Blank lines right beside a moved block fold into it rather than standing alone.
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let blank = |index: usize| lines.get(index).is_some_and(|line| line.trim().is_empty());
        let moved_lines = moved.map(|span| {
            let (mut start, mut end) = (span.lines.start, span.lines.end);
            while start > 0 && blank(start - 1) {
                start -= 1;
            }
            while end < lines.len() && blank(end) {
                end += 1;
            }
            (
                start..end,
                (span.lines.start - start) + (end - span.lines.end),
            )
        });
        let mut offset = 0;
        for (index, line) in lines.iter().copied().enumerate() {
            if let (Some(span), Some((range, blanks))) = (moved, &moved_lines) {
                if index == range.start {
                    let unit = self.unit;
                    let mut description = move_text(span.found, span.source);
                    if *blanks > 0 {
                        description = format!("{description} · +{}", plural_lines(*blanks));
                    }
                    self.add_note(Note {
                        link: Some(other_end(span.found, unit)),
                        fold: Some(Fold::Expand(unit)),
                        ..Note::plain(RowKind::Moved, description)
                    });
                    if span.expanded
                        && !span.source
                        && let Some(inner) = &span.found.inner
                    {
                        // The move's edits say more than the moved lines themselves; their new
                        // side is this side's moved lines, and their old side the source's.
                        let start = head.map(|head| HeadLine {
                            path: head.path.clone(),
                            line: head.line + span.lines.start,
                        });
                        let source = span.found.source.location.line.saturating_sub(1);
                        self.move_edits(inner, start.as_ref(), Some(source));
                    }
                }
                let shows_inner = !span.source && span.found.inner.is_some();
                if range.contains(&index) && (!span.expanded || shows_inner) {
                    offset += line.len();
                    continue;
                }
            }
            let split = split_line_ending(line);
            let line_highlights = highlights
                .iter()
                .filter(|range| range.end > offset && range.start < offset + split.content.len())
                .map(|range| {
                    range.start.saturating_sub(offset)
                        ..(range.end - offset).min(split.content.len())
                })
                .collect();
            let position = head.map(|head| HeadLine {
                path: head.path.clone(),
                line: head.line + index,
            });
            let moved = moved_lines
                .as_ref()
                .is_some_and(|(range, _)| range.contains(&index));
            self.add_cell(Cell {
                content_len: split.content.len(),
                highlights: line_highlights,
                head: position,
                base: base.map(|start| start + index),
                moved,
                ..self.cell(kind, with_ending(split.content, split.ending))
            });
            offset += line.len();
        }
    }

    /// Pushes the lines of `chunk` as its edit script shows them: each new line once where the
    /// layout allows it and nothing that changed would hide, and otherwise its old lines above
    /// its new ones, the words that changed marked. `old` says where its old lines are.
    fn edited_chunk(&mut self, chunk: &Chunk, head_path: Option<&str>, old: &OldLines) {
        let old_lines: Vec<&str> = chunk.removed.split_inclusive('\n').collect();
        let new: Vec<&str> = chunk.added.split_inclusive('\n').collect();
        let head = |line: usize| {
            head_path.map(|path| HeadLine {
                path: path.to_owned(),
                line: chunk.new.start + line,
            })
        };
        for shown in once::lines(chunk, self.once) {
            let cell = match shown {
                once::Shown::Removed { line, words } => {
                    let split = split_line_ending(old_lines.get(line).copied().unwrap_or_default());
                    let at = chunk.old.start + line;
                    Cell {
                        content_len: split.content.len(),
                        highlights: words,
                        base: matches!(old, OldLines::Base).then_some(at),
                        reviewed: match old {
                            OldLines::Reviewed(first) => Some(ReviewedLine {
                                line: first.line + at,
                                ..first.clone()
                            }),
                            OldLines::Base | OldLines::Nowhere => None,
                        },
                        ..self.cell(RowKind::Removed, with_ending(split.content, split.ending))
                    }
                }
                once::Shown::Added { line, words } => {
                    let split = split_line_ending(new.get(line).copied().unwrap_or_default());
                    Cell {
                        content_len: split.content.len(),
                        highlights: words,
                        head: head(line),
                        ..self.cell(RowKind::Added, with_ending(split.content, split.ending))
                    }
                }
                once::Shown::Once {
                    line,
                    text,
                    inserted,
                    deleted,
                } => {
                    let split = split_line_ending(new.get(line).copied().unwrap_or_default());
                    Cell {
                        content_len: text.len(),
                        highlights: inserted,
                        deleted,
                        head: head(line),
                        ..self.cell(RowKind::Edited, with_ending(&text, split.ending))
                    }
                }
            };
            self.add_cell(cell);
        }
    }

    /// Pushes a move's edits, `diff`, as moved code: the lines it kept, re-indented with their
    /// block or not, in the move's color, and its edited lines old and new, marked with the
    /// words they changed; numbered as [`Self::diff`] numbers.
    fn move_edits(&mut self, diff: &SideDiff, head: Option<&HeadLine>, old: Option<usize>) {
        let mut new_line = 0;
        let mut old_line = 0;
        for (line, changed) in moves::edits(diff).lines {
            // A kept line shows its new version, numbered on both sides.
            let (kind, shown) = match line {
                moves::EditLine::Kept(line) | moves::EditLine::Shifted(line) => {
                    (RowKind::NestedSame, line)
                }
                moves::EditLine::Old(old) => (RowKind::NestedOld, old),
                moves::EditLine::New(new) => (RowKind::NestedNew, new),
            };
            let mut cell = Cell {
                content_len: shown.text.len(),
                highlights: changed,
                moved: true,
                ..self.cell(kind, with_ending(&shown.text, shown.ending))
            };
            if kind != RowKind::NestedOld {
                cell.head = head.map(|head| HeadLine {
                    path: head.path.clone(),
                    line: head.line + new_line,
                });
                new_line += 1;
            }
            if kind != RowKind::NestedNew {
                cell.base = old.map(|start| start + old_line);
                old_line += 1;
            }
            self.add_cell(cell);
        }
    }

    fn chunk(&mut self, chunk: &Chunk, head_path: Option<&str>) {
        self.chunk_with_moves(chunk, head_path, None, None);
    }

    fn chunk_with_moves(
        &mut self,
        chunk: &Chunk,
        head_path: Option<&str>,
        removed: Option<&MovedSpan<'_>>,
        added: Option<&MovedSpan<'_>>,
    ) {
        // Moved and formatting-only code shows its own way, line for line.
        if removed.is_none() && added.is_none() && !self.moved && !self.quiet {
            self.edited_chunk(chunk, head_path, &OldLines::Base);
            return;
        }
        self.side(
            &chunk.removed,
            &chunk.removed_highlights(),
            RowKind::Removed,
            None,
            Some(chunk.old.start),
            removed,
        );
        let head = head_path.map(|path| HeadLine {
            path: path.to_owned(),
            line: chunk.new.start,
        });
        self.side(
            &chunk.added,
            &chunk.added_highlights(),
            RowKind::Added,
            head.as_ref(),
            None,
            added,
        );
    }
}

/// Pushes the one header row of display hunk `hunk`: where it is, what kind of change it is,
/// and the scopes around it. It is the header row of its unit when the unit has none yet.
fn hunk_header(layout: &mut Layout, file: usize, hunk: usize) {
    let (Some(unit), Some(note)) = (
        layout.hunks.get(hunk).map(|hunk| hunk.unit),
        layout.hunk_notes.get(hunk).cloned(),
    ) else {
        return;
    };
    layout.unit_rows.entry(unit).or_insert(layout.rows.len());
    layout.rows.push(note_row(file, unit, Some(hunk), note));
}

/// What the header of display hunk `hunk` says: where it is, what kind of change it is, and
/// the scopes around it.
fn hunk_note(layout: &Layout, view: &Source<'_>, file: usize, hunk: &Hunk, folds: &Folds) -> Note {
    let unit = hunk.unit;
    let scope = layout.files.get(file).and_then(|section| match hunk.at() {
        Some(line) => head_scope(&section.path, line, folds),
        None => unit_scope(view, &section.path, unit, folds),
    });
    let mut text = header(view, unit, hunk.lines().as_ref());
    let mut tag = header_tag(view, unit);
    match pdf_of(view, unit, folds) {
        Some(PdfShows::Page { .. }) => "PDF".clone_into(&mut text),
        Some(PdfShows::Binary(why)) => {
            tag = Some(tag.map_or_else(|| why.clone(), |tag| format!("{why} · {tag}")));
        }
        None => {}
    }
    Note {
        badge: view.standing(unit).map(Badge::of),
        tag,
        scope,
        folded: folds.is_folded(view, unit),
        ..Note::plain(RowKind::Header, text)
    }
}

/// What `unit` shows as a changed PDF's opaque unit, when it is one whose pages were read.
fn pdf_of<'f>(view: &Source<'_>, unit: UnitId, folds: &'f Folds) -> Option<&'f PdfShows> {
    let pdf = folds.pdf.as_ref()?;
    let opaque = matches!(
        view.unit(unit).map(|record| &record.kind),
        Ok(UnitKind::File(
            FileUnit::Binary | FileUnit::NotUtf8 | FileUnit::Oversized
        ))
    );
    let path = view.file(unit).ok()?.display_path();
    (opaque && path == pdf.path).then_some(&pdf.shows)
}

/// The scopes around head line `line` of the file at `path`, once its syntax is known.
fn head_scope(path: &str, line: usize, folds: &Folds) -> Option<String> {
    let side = folds.syntax.get(path)?.head.as_ref()?;
    let enclosing = side.scopes.enclosing(line);
    (!enclosing.is_empty()).then(|| breadcrumb(&enclosing))
}

/// The scopes around `unit` of the file at `path`, such as `impl Ledger › fn apply`, once its
/// syntax is known.
fn unit_scope(view: &Source<'_>, path: &str, unit: UnitId, folds: &Folds) -> Option<String> {
    let anchor = view.scope_anchor(unit).ok().flatten()?;
    let syntax = folds.syntax.get(path)?;
    let side = match anchor.side {
        Side::Head => syntax.head.as_ref(),
        Side::Base => syntax.base.as_ref(),
    }?;
    let enclosing = side.scopes.enclosing(anchor.line);
    (!enclosing.is_empty()).then(|| breadcrumb(&enclosing))
}

/// A note's lines, one per row; an empty note still takes one.
fn note_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() { vec![""] } else { lines }
}

/// The label of a proven formatting-only unit.
const REFORMATTED: &str = "reformatted only";

/// The label of a proven formatting-only unit that no mark decides, approved automatically.
const AUTO_APPROVED: &str = "reformatted only · auto-approved";

/// Pushes the rows of display hunk `index`, `hunk`, unless its unit is folded: the unit's
/// body, or the one change of it the hunk is, and after the unit's last rows its notes.
fn hunk_rows(
    layout: &mut Layout,
    view: &Source<'_>,
    file: usize,
    index: usize,
    hunk: &Hunk,
    folds: &Folds,
    extras: &Extras<'_, '_>,
) {
    let unit = hunk.unit;
    if folds.is_folded(view, unit) {
        return;
    }
    let quiet = matches!(view.collapse(unit), Ok(Some(Collapse::FormattingOnly)));
    let mut pusher = Pusher {
        layout,
        file,
        unit,
        hunk: index,
        quiet,
        moved: false,
        once: folds.layout == DiffLayout::Unified,
    };
    let shown = hunk
        .at()
        .and_then(|at| Some((at, extras.deltas.get(&unit)?)));
    if let Some((at, shown)) = shown {
        // What changed since the review is no reformatting, even where the whole change is.
        pusher.quiet = false;
        shown.push_run(&mut pusher, view, at);
        if shown.runs.last().is_some_and(|last| last.new.start != at) {
            return;
        }
        move_notes(&mut pusher, &view.moves_touching(unit), &[]);
    } else {
        unit_body(&mut pusher, view, unit, folds);
    }
    for note in extras.notes {
        if note.attached == (NoteAttachment::Unit { unit }) {
            // Rows have one height, so each line of a note is a row of its own; the first says
            // where the note comes from.
            for (index, line) in note_lines(&note.text).into_iter().enumerate() {
                let tag =
                    (index == 0).then(|| format!("note · revision {}", note.written_on.revision()));
                pusher.layout.rows.push(note_row(
                    file,
                    unit,
                    None,
                    Note {
                        tag,
                        ..Note::plain(RowKind::Note, line.to_owned())
                    },
                ));
            }
        }
    }
}

/// The header's label for a change that is not an ordinary edit.
fn header_tag(view: &Source<'_>, unit: UnitId) -> Option<String> {
    if let Some(UnitState::ChangedSinceReviewed { reviewed, .. }) = view.state(unit) {
        let since = reviewed.revision();
        return Some(match view.since_reviewed(unit) {
            Some(SinceReviewed::Delta(Delta {
                baseline: ReviewedBaseline::Partial,
                ..
            })) => format!("partly approved in revision {since}"),
            Some(SinceReviewed::Whole(Reopened::TargetMoved)) => {
                format!("target branch moved since revision {since} · shown whole")
            }
            Some(SinceReviewed::Whole(Reopened::OtherBaseLines)) => {
                format!("changes other lines since revision {since} · shown whole")
            }
            Some(SinceReviewed::Whole(Reopened::Uncertain)) => {
                format!("changed since revision {since} · shown whole")
            }
            Some(SinceReviewed::Whole(Reopened::Flagged)) => {
                format!("changed since flagged in revision {since} · shown whole")
            }
            Some(SinceReviewed::Whole(Reopened::SameCode)) => {
                format!("same as revision {since}, reopened in between")
            }
            _ => format!("changed since revision {since}"),
        });
    }
    match view.collapse(unit) {
        Ok(Some(Collapse::FormattingOnly)) => {
            let auto = matches!(view.standing(unit), Some(Standing::AutoApproved));
            return Some(if auto { AUTO_APPROVED } else { REFORMATTED }.to_owned());
        }
        Ok(Some(Collapse::Noise(category))) => return Some(category.name().to_owned()),
        _ => {}
    }
    let record = view.unit(unit).ok()?;
    let chunk = record.chunk()?;
    (chunk.whitespace_only && !record.formatting_only)
        .then(|| "whitespace changed · not proven equivalent".to_owned())
}

/// A unit's header line: where it is, such as "L27–29", or what a file unit changes; for one
/// change of a unit that shows what changed since it was reviewed, where that change is.
fn header(view: &Source<'_>, unit: UnitId, run: Option<&Range<usize>>) -> String {
    if let Some(lines) = run {
        return match lines.len() {
            0 => format!("removed at L{}", lines.start + 1),
            1 => format!("L{}", lines.start + 1),
            _ => format!("L{}–{}", lines.start + 1, lines.end),
        };
    }
    match view.unit(unit).ok().and_then(|record| record.chunk()) {
        Some(chunk) if chunk.added.is_empty() => format!("removed at L{}", chunk.new.start + 1),
        Some(chunk) if chunk.new.end() > chunk.new.start + 1 => {
            format!("L{}–{}", chunk.new.start + 1, chunk.new.end())
        }
        Some(chunk) => format!("L{}", chunk.new.start + 1),
        None => {
            let described = view
                .describe(unit)
                .ok()
                .flatten()
                .unwrap_or_else(|| "file".to_owned());
            // Object IDs say nothing at a glance; the file header has the entry details.
            let terse = matches!(
                view.unit(unit).map(|record| &record.kind),
                Ok(UnitKind::File(
                    FileUnit::Binary
                        | FileUnit::Oversized
                        | FileUnit::NotUtf8
                        | FileUnit::Type
                        | FileUnit::Other
                ))
            );
            match described.split_once(": ") {
                Some((label, _)) if terse => label.to_owned(),
                _ => described,
            }
        }
    }
}

/// The lines of one chunk side that a move takes away or brings in.
struct MovedSpan<'a> {
    found: &'a MoveView,
    /// Whether these are the lines the move takes away, rather than the ones it brings in; a
    /// move within one changed stretch has both ends in one unit.
    source: bool,
    /// Line indices within the side's text.
    lines: Range<usize>,
    expanded: bool,
}

/// The lines of `chunk` that `found` covers at its `source` end or its target, as indices within
/// that side.
fn moved_lines(found: &MoveView, source: bool, chunk: &Chunk) -> Option<Range<usize>> {
    let (location, range, text) = if source {
        (&found.source.location, chunk.old, &chunk.removed)
    } else {
        (&found.target.location, chunk.new, &chunk.added)
    };
    let count = text.lines().count();
    let start = location
        .line
        .saturating_sub(1)
        .saturating_sub(range.start)
        .min(count);
    let end = location.end_line.saturating_sub(range.start).min(count);
    (start < end).then_some(start..end)
}

/// "1 blank line" or "N blank lines".
fn plural_lines(count: usize) -> String {
    if count == 1 {
        "1 blank line".to_owned()
    } else {
        format!("{count} blank lines")
    }
}

/// The arrowed one-line description of a move from its `source` end or its target, such as "↗ moved to
/// src/hash.rs:1–9", or for a move within one file "↗ moved to lines 40–48 of this file"; the
/// move's ID stays out of the reviewer's way.
fn move_text(found: &MoveView, source: bool) -> String {
    let (arrow, direction, other) = if source {
        ("↗", "to", &found.target.location)
    } else {
        ("↙", "from", &found.source.location)
    };
    // A move that only re-indented its code says how far, after where it went.
    let (edits, shifted) = match (found.kind, &found.inner) {
        (review_core::analysis::MoveKind::Moved, _) => ("", String::new()),
        (review_core::analysis::MoveKind::MovedWithEdits, Some(inner)) => {
            let edits = moves::edits(inner);
            if edits.reindented {
                let way = if edits.shift > 0 {
                    "deeper"
                } else {
                    "shallower"
                };
                (
                    "",
                    format!(", re-indented {} {way}", edits.shift.unsigned_abs()),
                )
            } else {
                (" with edits", String::new())
            }
        }
        (review_core::analysis::MoveKind::MovedWithEdits, None) => (" with edits", String::new()),
    };
    if found.source.location.path == found.target.location.path {
        let lines = if other.line == other.end_line {
            format!("line {}", other.line)
        } else {
            format!("lines {}–{}", other.line, other.end_line)
        };
        return format!("{arrow} moved{edits} {direction} {lines} of this file{shifted}");
    }
    format!(
        "{arrow} moved{edits} {direction} {}:{}–{}{shifted}",
        other.path, other.line, other.end_line
    )
}

/// The unit at the other end of `found` from `unit`.
fn other_end(found: &MoveView, unit: UnitId) -> UnitId {
    if found.target.unit == unit {
        found.source.unit
    } else {
        found.target.unit
    }
}

fn unit_body(pusher: &mut Pusher<'_>, view: &Source<'_>, unit: UnitId, folds: &Folds) {
    let (Ok(record), Ok(file)) = (view.unit(unit), view.file(unit)) else {
        return;
    };
    let head_path = file.new.as_ref().map(|side| side.path.as_str());
    let expanded = folds.expanded.contains(&unit);
    // A chunk that changed since it was reviewed shows here when it shows its whole change,
    // never folded into a move or shown quietly; its changes since the review show as hunks of
    // their own (`Shown::push_run`). Noise stays folded.
    let collapse = view.collapse(unit);
    let since = match collapse {
        Ok(Some(Collapse::Noise(_))) => None,
        _ => view.since_reviewed(unit),
    };
    if let (Some(_), Some(chunk)) = (since, record.chunk()) {
        // What changed since the review is no reformatting, even where the whole change is.
        pusher.quiet = false;
        pusher.chunk(chunk, head_path);
        move_notes(pusher, &view.moves_touching(unit), &[]);
        return;
    }
    match collapse {
        // Reformatting shows its code, without emphasis; the header says what it is.
        Ok(Some(Collapse::FormattingOnly)) => {
            if let Some(chunk) = record.chunk() {
                pusher.chunk(chunk, head_path);
            }
            return;
        }
        Ok(Some(Collapse::Noise(category))) => {
            pusher.add_note(Note {
                fold: Some(Fold::Expand(unit)),
                ..Note::plain(RowKind::Collapsed, format!("{} file", category.name()))
            });
            if expanded && let Some(chunk) = record.chunk() {
                pusher.chunk(chunk, head_path);
            }
            return;
        }
        Ok(Some(Collapse::Moved(found))) => {
            pusher.add_note(Note {
                link: Some(other_end(&found, unit)),
                fold: Some(Fold::Expand(unit)),
                ..Note::plain(RowKind::Moved, move_text(&found, found.target.unit != unit))
            });
            // The file view shows every line, a moved one too, under its note, unless the
            // reviewer folds it; the hunks view folds it into the note unless they open it.
            let open = expanded != (folds.view == DiffView::File);
            if open && let Some(chunk) = record.chunk() {
                pusher.moved = true;
                pusher.chunk(chunk, head_path);
                pusher.moved = false;
            }
            return;
        }
        Ok(None) | Err(_) => {}
    }
    let moves = view.moves_touching(unit);
    let mut described = Vec::new();
    match &record.kind {
        UnitKind::Chunk(chunk) => {
            // Moved lines fold into one linked row on each side, in place.
            let span = |source: bool| {
                moves.iter().find_map(|found| {
                    let ours = if source {
                        found.source.unit
                    } else {
                        found.target.unit
                    };
                    (ours == unit).then_some(())?;
                    // A move with edits shows its edits where the code arrives unless the
                    // reviewer folds them: they are changes to review like any other. The file
                    // view shows every line, so there the moved lines show too, under their
                    // note.
                    let edits = !source && found.inner.is_some();
                    let open = edits || folds.view == DiffView::File;
                    Some(MovedSpan {
                        found,
                        source,
                        lines: moved_lines(found, source, chunk)?,
                        expanded: expanded != open,
                    })
                })
            };
            let (removed, added) = (span(true), span(false));
            for shown in [&removed, &added].into_iter().flatten() {
                described.push(shown.found.id);
            }
            pusher.chunk_with_moves(chunk, head_path, removed.as_ref(), added.as_ref());
        }
        // A change without lines shows its evidence in full: the entries before and after,
        // with every value, such as a symlink's target.
        UnitKind::File(_) => {
            if let Ok(Some((old, new))) = view.entries(unit) {
                pusher.push(RowKind::Info, format!("before  {old}"));
                pusher.push(RowKind::Info, format!("after   {new}"));
            }
            // A PDF's page text is in no file, so its lines have no numbers.
            if let Some(PdfShows::Page { navigator, rows }) = pdf_of(view, unit, folds) {
                pdf_page(pusher, navigator, rows);
            }
        }
    }
    move_notes(pusher, &moves, &described);
}

/// Pushes a PDF page pair's navigator line and its rows, whose lines are in no file: its changes
/// in order, or each passage that moved where it is now, under a note of where it was.
fn pdf_page(pusher: &mut Pusher<'_>, navigator: &str, rows: &PageRows) {
    pusher.push(RowKind::Info, navigator.to_owned());
    match rows {
        PageRows::Edits(chunks) => {
            for chunk in chunks.iter() {
                pusher.edited_chunk(chunk, None, &OldLines::Nowhere);
            }
        }
        PageRows::Moved(passages) => {
            for passage in passages.iter() {
                pusher.add_note(Note::plain(
                    RowKind::Moved,
                    format!("↙ moved from row {} of the base page", passage.from + 1),
                ));
                pusher.moved = true;
                pusher.edited_chunk(&passage.chunk, None, &OldLines::Nowhere);
                pusher.moved = false;
            }
        }
    }
}

/// Pushes a linked note for each of `moves` not among `described`.
fn move_notes(pusher: &mut Pusher<'_>, moves: &[MoveView], described: &[review_core::MoveId]) {
    let unit = pusher.unit;
    for found in moves.iter().filter(|found| !described.contains(&found.id)) {
        pusher.add_note(Note {
            link: Some(other_end(found, unit)),
            ..Note::plain(RowKind::Moved, move_text(found, found.target.unit != unit))
        });
    }
}

/// Lays out a file section: file units first, then each chunk with `context` unchanged lines
/// around it from `text`, and gap rows for the unchanged lines left out. Without `text`, chunks
/// show without context. With `context` at `usize::MAX`, the whole file shows.
///
/// A chunk that shows what changed since it was reviewed is a hunk for each of those changes,
/// and its lines the same as reviewed around and between them are context like any unchanged
/// line.
///
/// `all_units` are every unit of the file, those of other groups too, whose lines never show
/// as unchanged context.
///
/// A folded hunk is its header alone, and unchanged lines between folded hunks or between one
/// and the file's edge show nothing at all, so consecutive folded hunks read as a list of
/// headers.
#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one layout pass over a file's parts, whose context and gaps depend on each other"
)]
fn section_rows(
    layout: &mut Layout,
    view: &Source<'_>,
    file: usize,
    group: &[UnitId],
    all_units: &[UnitId],
    text: Option<&FullText>,
    context: usize,
    folds: &Folds,
    notes: &[review_core::ResolvedNote],
) {
    let path = layout
        .files
        .get(file)
        .map(|section| section.path.clone())
        .unwrap_or_default();
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut file_units = Vec::new();
    // What changed since review, aligned once for each unit that shows it, and the lines such
    // units have the same as reviewed.
    let mut deltas = Deltas::new();
    let mut reviewed = Vec::new();
    for &unit in group {
        if view
            .unit(unit)
            .ok()
            .and_then(|record| record.chunk())
            .is_none()
        {
            file_units.push(unit);
            continue;
        }
        // A unit the reviewer folded is one hunk, its header alone.
        let shown = shown_delta(view, unit)
            .filter(|_| !folds.is_folded(view, unit))
            .map(|delta| Shown {
                runs: delta.runs(),
                delta,
            })
            .filter(|shown| !shown.runs.is_empty());
        let Some(shown) = shown else {
            hunks.push(Hunk { unit, change: None });
            continue;
        };
        hunks.extend(shown.runs.iter().map(|run| Hunk {
            unit,
            change: Some(run.shown()),
        }));
        reviewed.extend(shown.reviewed_spans());
        deltas.insert(unit, shown);
    }
    let extras = Extras {
        notes,
        deltas: &deltas,
    };
    // Changes without lines, such as a rename and a mode change, are a hunk each, as every
    // change is.
    for unit in file_units {
        let hunk = Hunk { unit, change: None };
        let index = push_hunk(layout, view, file, hunk.clone(), folds);
        hunk_header(layout, file, index);
        hunk_rows(layout, view, file, index, &hunk, folds, &extras);
    }
    let Some(text) = text else {
        for hunk in hunks {
            let index = push_hunk(layout, view, file, hunk.clone(), folds);
            hunk_header(layout, file, index);
            hunk_rows(layout, view, file, index, &hunk, folds, &extras);
        }
        return;
    };
    hunks.sort_by_key(|hunk| head_span(view, hunk).start);
    reviewed.sort_by_key(|span| span.head.start);
    // Changes of this file in other steps are never shown as unchanged context.
    let in_group: HashSet<UnitId> = group.iter().copied().collect();
    let others: Vec<(usize, usize, UnitId)> = all_units
        .iter()
        .copied()
        .filter(|unit| !in_group.contains(unit))
        .filter_map(|unit| {
            let chunk = view.unit(unit).ok()?.chunk()?;
            Some((
                chunk.new.start,
                chunk.new.end().max(chunk.new.start + 1),
                unit,
            ))
        })
        .collect();
    let lines = text.lines.len();
    let mut emitter = Context {
        file,
        path: path.clone(),
        text,
        others: &others,
        bases: BaseLines::of(view, all_units),
        reviewed,
    };
    if folds.view == DiffView::File {
        whole_file_rows(layout, view, &mut emitter, &hunks, folds, &extras);
        return;
    }
    let mut line = 0;
    // Whether the unchanged lines from `line` on follow the file's start or a folded hunk.
    // With a folded hunk or the file's end after them as well, they are context without a
    // change, and show nothing: no lines, no gap.
    let mut bare = true;
    for (position, hunk) in hunks.iter().enumerate() {
        let unit = hunk.unit;
        let Range { start, end } = head_span(view, hunk);
        // Context after the hunk stops where the next hunk's changes start.
        let next_start = hunks
            .get(position + 1)
            .map_or(lines, |next| head_span(view, next).start);
        let revealed = folds.gaps.get(&(path.clone(), start)).copied().unwrap_or(0);
        // A folded hunk is its header alone; its lines count toward the gaps around it.
        let folded = folds.is_folded(view, unit);
        let hidden = folded && bare;
        bare = folded;
        let context_before = if folded {
            revealed
        } else {
            context.saturating_add(revealed)
        };
        let from = if hidden {
            start
        } else {
            start.saturating_sub(context_before)
        }
        .max(line);
        // A gap that would hide only a few lines shows them instead.
        let from = if !hidden && from - line < SHOWN_UNCHANGED {
            line
        } else {
            from
        };
        if from > line && !hidden {
            // The gap already breaks the code's flow, so it names the scopes of the hunk after
            // it, where a header row would break it once more.
            let scope = match hunk.at() {
                Some(at) => head_scope(&path, at, folds),
                None => unit_scope(view, &path, unit, folds),
            };
            emitter.gap(layout, unit, line..from, start, scope);
        }
        // A header only where the hunk's code cannot say it: what kind of change it is, or the
        // hunk itself while it is folded; else its code reads on from the code around it, as
        // in the file view, and the file's header names the selected hunk's scopes. One
        // change since review reads on from the lines the same as reviewed around it, as the
        // unit's other changes do, so its bar runs along that change alone.
        let headed = folded || (hunk.change.is_none() && header_tag(view, unit).is_some());
        let index = push_hunk(layout, view, file, hunk.clone(), folds);
        if headed {
            hunk_header(layout, file, index);
        }
        emitter.lines(layout, unit, from..start);
        if folded {
            line = end.max(start);
            continue;
        }
        layout.unit_rows.entry(unit).or_insert(layout.rows.len());
        hunk_rows(layout, view, file, index, hunk, folds, &extras);
        let until = end
            .saturating_add(context)
            .min(lines)
            .min(next_start.max(end));
        emitter.lines(layout, unit, end..until.max(end));
        line = until.max(end).max(start);
    }
    // After a folded last hunk, the rest of the file is context without a change too.
    let after_folded = bare && !hunks.is_empty();
    if line < lines && !after_folded {
        let owner = hunks
            .last()
            .map_or_else(|| group.first().copied(), |hunk| Some(hunk.unit));
        if let Some(owner) = owner {
            let revealed = folds.gaps.get(&(path.clone(), lines)).copied().unwrap_or(0);
            let until = line.saturating_add(revealed).min(lines);
            // A gap that would hide only a few lines shows them instead.
            let until = if lines - until < SHOWN_UNCHANGED {
                lines
            } else {
                until
            };
            emitter.lines(layout, owner, line..until);
            if until < lines {
                emitter.gap(layout, owner, until..lines, lines, None);
            }
        }
    }
}

/// The head lines a chunk's display hunk stands at: its change's, or its unit's.
fn head_span(view: &Source<'_>, hunk: &Hunk) -> Range<usize> {
    if let Some(lines) = hunk.lines() {
        return lines;
    }
    view.unit(hunk.unit)
        .ok()
        .and_then(|record| record.chunk())
        .map_or(0..0, |chunk| {
            chunk.new.start..chunk.new.end().max(chunk.new.start)
        })
}

/// Head lines the same as reviewed, and where the first is in the head it was reviewed in,
/// unless partial approvals left them, in no file.
struct ReviewedSpan {
    head: Range<usize>,
    first: Option<ReviewedLine>,
}

/// What a unit that shows what changed since it was reviewed shows.
struct Shown<'h> {
    delta: Delta<'h>,
    /// The delta's runs, aligned once.
    runs: Vec<LineRun<'h>>,
}

impl Shown<'_> {
    /// Where a line of the reviewed text is, counting from its first line; `None` when that
    /// text is in no file.
    fn reviewed_line(&self, line: usize) -> Option<ReviewedLine> {
        let ReviewedBaseline::Head(head) = self.delta.baseline else {
            return None;
        };
        Some(ReviewedLine {
            unit: self.delta.reviewed,
            side: crate::state::SideKey {
                oid: head.entry.oid.clone(),
                path: head.entry.path.clone(),
            },
            line: head.start + line,
        })
    }

    /// The unit's head lines the same as reviewed: those before, between and after its runs.
    fn reviewed_spans(&self) -> Vec<ReviewedSpan> {
        let current = self.delta.after.new;
        let mut spans = Vec::new();
        let (mut head, mut old) = (current.start, 0);
        for run in &self.runs {
            spans.push(ReviewedSpan {
                head: head..run.new.start,
                first: self.reviewed_line(old),
            });
            (head, old) = (run.new.end(), run.old.end());
        }
        spans.push(ReviewedSpan {
            head: head..current.end(),
            first: self.reviewed_line(old),
        });
        spans.retain(|span| !span.head.is_empty());
        spans
    }

    /// Pushes the run at head line `at` as a chunk's lines show, its old lines the reviewed
    /// ones.
    fn push_run(&self, pusher: &mut Pusher<'_>, view: &Source<'_>, at: usize) {
        let Some(run) = self.runs.iter().find(|run| run.new.start == at) else {
            return;
        };
        let head_path = view
            .file(pusher.unit)
            .ok()
            .and_then(|file| file.new.as_ref())
            .map(|side| side.path.as_str());
        let old = self
            .reviewed_line(0)
            .map_or(OldLines::Nowhere, OldLines::Reviewed);
        pusher.edited_chunk(&run.to_chunk(), head_path, &old);
    }
}

/// Where the old lines of a chunk are, for their numbers and syntax.
enum OldLines {
    /// In the base file, at the chunk's old lines.
    Base,
    /// In the head a change was reviewed in, its lines counted from this one.
    Reviewed(ReviewedLine),
    /// In no file: the text partial approvals left.
    Nowhere,
}

/// What changed since review, by unit, of the units of a file section that show only that.
type Deltas<'h> = HashMap<UnitId, Shown<'h>>;

/// What the rows of a file section's units show besides their code.
struct Extras<'a, 'h> {
    /// Every note, which shows under the unit it is attached to.
    notes: &'a [review_core::ResolvedNote],
    /// What changed since review; see [`Deltas`].
    deltas: &'a Deltas<'h>,
}

/// Registers display hunk `hunk`, with what its header says, and returns its index.
fn push_hunk(
    layout: &mut Layout,
    view: &Source<'_>,
    file: usize,
    hunk: Hunk,
    folds: &Folds,
) -> usize {
    let note = hunk_note(layout, view, file, &hunk, folds);
    layout.hunk_notes.push(note);
    layout.hunks.push(hunk);
    layout.hunks.len() - 1
}

/// Whether `unit` shows its changes since it was reviewed as hunks of their own, as it does
/// unless the reviewer folds it.
#[must_use]
pub fn shows_changes_apart(view: &RevisionView<'_>, unit: UnitId) -> bool {
    shown_delta(&Source::Verified(*view), unit).is_some_and(|delta| !delta.runs().is_empty())
}

/// What changed since `unit` was reviewed, when it shows only that: a noise unit stays folded
/// as noise, so it shows whole.
fn shown_delta<'h>(view: &Source<'h>, unit: UnitId) -> Option<Delta<'h>> {
    if let Ok(Some(Collapse::Noise(_))) = view.collapse(unit) {
        return None;
    }
    match view.since_reviewed(unit) {
        Some(SinceReviewed::Delta(delta)) => Some(delta),
        _ => None,
    }
}

/// The fewest unchanged lines the hunks view folds into a gap row, between two hunks' context or
/// between one and the file's start or end; fewer show whole, since a gap row would hide hardly
/// more than it takes.
const SHOWN_UNCHANGED: usize = 10;

/// Lays out the chunks of a file section in the file view: every head line of the file in
/// order, each display hunk's changes in place, without a header row unless nothing of the hunk
/// shows (the reviewer folded it), and without gaps.
///
/// Each unit's row in [`Layout::unit_rows`] is the first row of its code, or the hunk's header
/// when only that shows.
fn whole_file_rows(
    layout: &mut Layout,
    view: &Source<'_>,
    emitter: &mut Context<'_>,
    hunks: &[Hunk],
    folds: &Folds,
    extras: &Extras<'_, '_>,
) {
    let file = emitter.file;
    let lines = emitter.text.lines.len();
    let mut line = 0;
    for hunk in hunks {
        let unit = hunk.unit;
        let Range { start, end } = head_span(view, hunk);
        emitter.lines(layout, unit, line..start.max(line));
        let index = push_hunk(layout, view, file, hunk.clone(), folds);
        let first = layout.rows.len();
        layout.unit_rows.entry(unit).or_insert(first);
        hunk_rows(layout, view, file, index, hunk, folds, extras);
        line = end.max(start).max(line);
        // A hunk that shows nothing of its own, folded by the reviewer, keeps its header so it
        // can still be selected, marked and unfolded.
        let shows = layout
            .rows
            .get(first..)
            .is_some_and(|rows| rows.iter().any(|row| row.hunk == Some(index)));
        if !shows {
            let note = hunk_note(layout, view, file, hunk, folds);
            layout
                .rows
                .insert(first, note_row(file, unit, Some(index), note));
            layout.unit_rows.insert(unit, first);
        }
    }
    let owner = hunks.last().map(|hunk| hunk.unit);
    if let Some(owner) = owner.or_else(|| layout.files.get(file)?.units.first().copied()) {
        emitter.lines(layout, owner, line..lines);
    }
}

/// Where unchanged head lines are in the base file, from every chunk of the file: those of
/// other steps and folded hunks count too, so the numbers never depend on what shows.
struct BaseLines {
    /// Each chunk's end in the head and in the base, in head order.
    ends: Vec<ChunkEnd>,
}

/// Where a chunk ends on each side, one past its last line.
struct ChunkEnd {
    head: usize,
    base: usize,
}

impl BaseLines {
    /// The chunks of a file in `view`, whose units are `units`.
    fn of(view: &Source<'_>, units: &[UnitId]) -> Self {
        let mut ends: Vec<ChunkEnd> = units
            .iter()
            .copied()
            .filter_map(|unit| {
                let chunk = view.unit(unit).ok()?.chunk()?;
                Some(ChunkEnd {
                    head: chunk.new.end(),
                    base: chunk.old.end(),
                })
            })
            .collect();
        ends.sort_by_key(|end| end.head);
        Self { ends }
    }

    /// The base line of head line `head`, which no chunk changes: as far past the last chunk
    /// before it in the base as it is in the head.
    fn base_of(&self, head: usize) -> usize {
        let before = self.ends.partition_point(|end| end.head <= head);
        match before.checked_sub(1).and_then(|last| self.ends.get(last)) {
            Some(end) => end.base + (head - end.head),
            None => head,
        }
    }
}

/// Emits unchanged head lines of one file section.
struct Context<'a> {
    file: usize,
    path: String,
    text: &'a FullText,
    others: &'a [(usize, usize, UnitId)],
    bases: BaseLines,
    /// Head lines a unit changed that are the same as when it was reviewed, which have no base
    /// line but may have one in the head they were reviewed in; disjoint, in order.
    reviewed: Vec<ReviewedSpan>,
}

impl Context<'_> {
    /// Pushes head lines `range` as context rows owned by `owner`, replacing lines that another
    /// step changes with one note each.
    fn lines(&mut self, layout: &mut Layout, owner: UnitId, range: Range<usize>) {
        let hunk = None;
        let mut index = range.start;
        while index < range.end.min(self.text.lines.len()) {
            if let Some(&(_, end, unit)) = self
                .others
                .iter()
                .find(|&&(start, end, _)| start <= index && index < end)
            {
                layout.rows.push(note_row(
                    self.file,
                    owner,
                    hunk,
                    Note::plain(
                        RowKind::Info,
                        format!("⋯ lines changed by {unit}, in another step"),
                    ),
                ));
                index = end;
                continue;
            }
            let content = self.text.lines.get(index).cloned().unwrap_or_default();
            let span = self
                .reviewed
                .get(self.reviewed.partition_point(|span| span.head.end <= index))
                .filter(|span| span.head.contains(&index));
            let reviewed = span.and_then(|span| {
                let first = span.first.as_ref()?;
                Some(ReviewedLine {
                    line: first.line + (index - span.head.start),
                    ..first.clone()
                })
            });
            let cell = Cell {
                head: Some(HeadLine {
                    path: self.path.clone(),
                    line: index,
                }),
                base: span.is_none().then(|| self.bases.base_of(index)),
                reviewed,
                ..plain_cell(RowKind::Context, content)
            };
            layout.rows.push(code_row(self.file, owner, hunk, cell));
            index += 1;
        }
    }

    /// Pushes a gap row for the unchanged lines `range`, which ends at head line `at`, naming
    /// `scope`, the scopes of the hunk after it.
    fn gap(
        &mut self,
        layout: &mut Layout,
        owner: UnitId,
        range: Range<usize>,
        at: usize,
        scope: Option<String>,
    ) {
        let count = range.len();
        let text = format!(
            "{count} unchanged line{}",
            if count == 1 { "" } else { "s" }
        );
        let note = Note {
            fold: Some(Fold::Gap {
                file: self.file,
                at,
            }),
            scope,
            ..Note::plain(RowKind::Gap, text)
        };
        layout.rows.push(note_row(self.file, owner, None, note));
    }
}

/// Appends a visible marker for CRLF and for a missing final newline.
fn with_ending(content: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => content.to_owned(),
        LineEnding::CrLf => format!("{content}␍"),
        LineEnding::Missing => format!("{content}  ⊘ no newline at end of file"),
    }
}

#[cfg(test)]
mod tests {
    use indoc::formatdoc;

    use super::*;

    /// A move to another file names the file; a move within one file names only its lines.
    #[test]
    fn a_move_within_a_file_names_only_its_lines() {
        let end = |unit: &str, path: &str, line, end_line| review_core::MoveEnd {
            unit: unit.parse().unwrap(),
            location: review_core::Location {
                path: path.to_owned(),
                line,
                end_line,
            },
        };
        let found = |target: &str| MoveView {
            id: "m1-0001".parse().unwrap(),
            kind: review_core::analysis::MoveKind::MovedWithEdits,
            source: end("u1-0001", "a.rs", 10, 12),
            target: end("u1-0002", target, 40, 48),
            inner: None,
        };
        assert_eq!(
            move_text(&found("a.rs"), true),
            "↗ moved with edits to lines 40–48 of this file"
        );
        assert_eq!(
            move_text(&found("a.rs"), false),
            "↙ moved with edits from lines 10–12 of this file"
        );
        assert_eq!(
            move_text(&found("b.rs"), true),
            "↗ moved with edits to b.rs:40–48"
        );
    }

    #[test]
    fn line_endings_are_marked() {
        assert_eq!(with_ending("a", LineEnding::Lf), "a");
        assert_eq!(with_ending("a", LineEnding::CrLf), "a␍");
        assert!(with_ending("a", LineEnding::Missing).contains("no newline"));
    }

    /// The demo review up to `revision`, in a temporary directory that lives as long as the
    /// directory handle.
    pub(super) fn demo_review(revision: u32) -> (tempfile::TempDir, review_core::Review) {
        let dir = tempfile::tempdir().unwrap();
        let request = match revision {
            1 => review_core::demo::DemoRequest::Fresh,
            later => review_core::demo::DemoRequest::UpTo(
                review_core::RevisionNumber::new(later).unwrap(),
            ),
        };
        let review = review_core::demo::prepare(
            &review_core::demo::DemoPaths::under(dir.path()),
            &review_core::demo::fixture_dir(),
            request,
        )
        .unwrap();
        (dir, review)
    }

    /// Builds the demo's layout with `folds`, with formatting-only hunks needing review like
    /// any other, so these tests see every hunk the demo's marks leave open.
    fn demo_layout(folds: &Folds) -> (tempfile::TempDir, Layout) {
        demo_layout_under(folds, review_core::ReviewPolicy::MARKS_ONLY)
    }

    /// The apps' default policy: proven formatting-only units are approved automatically.
    const AUTO_APPROVE: review_core::ReviewPolicy = review_core::ReviewPolicy {
        auto_approve_formatting: true,
    };

    /// Builds the demo's layout with `folds`, deciding under `policy` which units need review.
    fn demo_layout_under(
        folds: &Folds,
        policy: review_core::ReviewPolicy,
    ) -> (tempfile::TempDir, Layout) {
        let (dir, review) = demo_review(1);
        let history = review.history().unwrap();
        let view = history.current_view_under(policy).unwrap();
        let order = review_core::walkthrough::partition(&view).order();
        let layout = present(
            build(&crate::rows::Source::Verified(view), &order, folds),
            Presentation::default(),
        );
        (dir, layout)
    }

    /// Under the apps' default policy, the engine's proven brace reflow is approved automatically: it
    /// folds to its header like an approved hunk, says so in its label, and shows its code
    /// again when approved hunks show; nothing else in the file changes.
    #[test]
    fn formatting_only_hunks_fold_as_approved_automatically() {
        let hunk_of = |layout: &Layout| {
            let file = layout
                .files
                .iter()
                .position(|section| section.path == "src/engine.rs")
                .unwrap();
            layout
                .rows
                .iter()
                .filter(|row| row.file == file && row.kind() == RowKind::Header)
                .filter_map(|row| row.note())
                .find(|note| note.text == "L27–29")
                .cloned()
                .unwrap()
        };
        let folds = Folds::default();
        let (_dir, automatic) = demo_layout_under(&folds, AUTO_APPROVE);
        let header = hunk_of(&automatic);
        assert!(header.folded, "approved automatically, it folds");
        assert_eq!(header.badge, Some(Badge::Auto));
        assert_eq!(header.tag.as_deref(), Some(AUTO_APPROVED));

        let (_dir, manual) = demo_layout(&folds);
        let header = hunk_of(&manual);
        assert!(!header.folded, "needing review, it shows");
        assert_eq!(header.badge, Some(Badge::Unreviewed));
        assert_eq!(header.tag.as_deref(), Some(REFORMATTED));

        let shown = Folds {
            approved: Approved::Shown,
            ..Folds::default()
        };
        let (_dir, unfolded) = demo_layout_under(&shown, AUTO_APPROVE);
        assert!(
            !hunk_of(&unfolded).folded,
            "approved hunks shown show it too"
        );
    }

    /// Which units a hunk holds never depends on whether the file's text has arrived, nor on
    /// how much context shows, so a key acts on the same units before and after.
    #[test]
    fn hunks_do_not_depend_on_cached_text() {
        let (_dir, bare) = demo_layout(&Folds::default());
        let texts: HashMap<String, Arc<FullText>> = bare
            .files
            .iter()
            .filter_map(|section| {
                let path = review_core::demo::fixture_dir()
                    .join("trees/head-1")
                    .join(&section.path);
                let text = std::fs::read_to_string(path).ok()?;
                Some((section.path.clone(), Arc::new(FullText::new(&text))))
            })
            .collect();
        assert!(texts.contains_key("src/engine.rs"));
        for context in [0, 3, 10, 50] {
            let folds = Folds {
                texts: texts.clone(),
                context,
                ..Folds::default()
            };
            let (_dir, with_text) = demo_layout(&folds);
            assert_eq!(with_text.hunks, bare.hunks, "context {context}");
        }
    }

    /// Files start with a header, are separated by a spacer, and every unit has a header row.
    #[test]
    fn rows_form_file_sections() {
        let (_dir, layout) = demo_layout(&Folds::default());
        let first = layout.files.first().unwrap();
        assert_eq!(layout.rows[first.header_row].kind(), RowKind::FileHeader);
        for section in layout.files.iter().skip(1) {
            assert_eq!(layout.rows[section.header_row - 1].kind(), RowKind::Spacer);
        }
        assert!(
            layout
                .files
                .iter()
                .any(|section| section.old_path.is_some())
        );
        for section in &layout.files {
            for unit in &section.units {
                assert!(layout.unit_rows.contains_key(unit), "{unit} has no row");
            }
        }
    }

    /// Code and annotation rows compete separately for the widest row, by cells including an
    /// annotation's tag, innermost scope and hint.
    #[test]
    fn code_and_annotations_have_their_own_widest_row() {
        let (_dir, layout) = demo_layout(&Folds::default());
        let (code, code_cells) = layout.widest_code.unwrap();
        let (note, note_cells) = layout.widest_annotation.unwrap();
        assert!(layout.rows[code].is_code());
        assert!(!layout.rows[note].is_code());
        for row in &layout.rows {
            let cells = match &row.body {
                Body::Code(cell) => cell.width(),
                Body::Note(note) => note.width(),
                Body::Pair(_) => panic!("the unified layout has no pairs"),
            };
            let limit = if row.is_code() {
                code_cells
            } else {
                note_cells
            };
            assert!(cells <= limit);
        }
    }

    /// The widest line sets the scroll width, not the longest in bytes.
    #[test]
    fn the_widest_line_wins_over_the_longest_in_bytes() {
        let accents = "é".repeat(200);
        let ascii = "x".repeat(300);
        let text = FullText::new(&formatdoc!(
            "
                {accents}
                {ascii}
            "
        ));
        assert!(accents.len() > ascii.len());
        assert_eq!(text.longest, 1);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("\tx"), 5);
    }

    /// A note of several lines takes one row per line, so fixed-height rows never clip it.
    #[test]
    fn multi_line_notes_take_a_row_per_line() {
        assert_eq!(note_lines(""), [""]);
    }

    /// A change without lines shows the full entries before and after, such as a symlink's
    /// old and new targets.
    #[test]
    fn file_units_show_their_entries_in_full() {
        let (_dir, layout) = demo_layout(&Folds::default());
        let info: Vec<&str> = layout
            .rows
            .iter()
            .filter(|row| row.kind() == RowKind::Info)
            .map(Row::text)
            .collect();
        assert!(
            info.contains(&"before  docs/current symlink (120000) to \"v1.md\""),
            "{info:?}"
        );
        assert!(info.contains(&"after   docs/current symlink (120000) to \"v2.md\""));
    }

    /// A move with edits shows one linked row at each end, and its edits where the code
    /// arrives, which the reviewer can fold away.
    #[test]
    fn moves_show_as_linked_rows_with_their_edits() {
        let (_dir, layout) = demo_layout(&Folds::default());
        let moved: Vec<&Row> = layout
            .rows
            .iter()
            .filter(|row| row.kind() == RowKind::Moved && row.text().contains("with edits"))
            .collect();
        assert_eq!(moved.len(), 2, "both ends of the move with edits");
        let target = moved
            .iter()
            .find(|row| row.text().starts_with('↙'))
            .unwrap();
        let source = moved
            .iter()
            .find(|row| row.text().starts_with('↗'))
            .unwrap();
        assert_eq!(target.note().unwrap().link, Some(source.unit));
        assert_eq!(source.note().unwrap().link, Some(target.unit));
        assert!(
            !target.text().contains("m1-"),
            "the move's ID stays out of the text: {}",
            target.text()
        );
        let edits = |layout: &Layout| {
            layout
                .rows
                .iter()
                .any(|row| row.unit == target.unit && row.kind() == RowKind::NestedNew)
        };
        assert!(edits(&layout), "the edits show by default");
        let added = layout
            .rows
            .iter()
            .filter(|row| row.unit == target.unit && row.kind() == RowKind::Added)
            .count();
        assert!(
            added <= 2,
            "only the lines outside the move are plain additions"
        );

        let folds = Folds {
            expanded: HashSet::from([target.unit]),
            ..Folds::default()
        };
        let (_dir, folded) = demo_layout(&folds);
        assert!(!edits(&folded), "folding hides the edits");
    }

    /// A folded file keeps only its header, and its units point at it.
    #[test]
    fn folded_files_keep_their_header() {
        let (_dir, layout) = demo_layout(&Folds::default());
        let path = layout.files[0].path.clone();
        let folds = Folds {
            files: HashMap::from([(path, true)]),
            ..Folds::default()
        };
        let (_dir, folded) = demo_layout(&folds);
        let section = &folded.files[0];
        assert!(section.collapsed);
        let next = folded.rows.get(section.header_row + 1).unwrap();
        assert_eq!(next.kind(), RowKind::Spacer);
        for unit in &section.units {
            assert_eq!(folded.unit_rows[unit], section.header_row);
        }
    }

    /// Only a hunk's own header and body name it: file headers, gaps, unchanged context and
    /// notes belong to no hunk, so a hunk command can never reach another file's hunk through
    /// them, folded files included.
    #[test]
    fn rows_own_hunks_explicitly() {
        let engine = std::fs::read_to_string(
            review_core::demo::fixture_dir().join("trees/head-1/src/engine.rs"),
        )
        .unwrap();
        let folds = Folds {
            texts: HashMap::from([("src/engine.rs".to_owned(), Arc::new(FullText::new(&engine)))]),
            ..Folds::default()
        };
        let (_dir, layout) = demo_layout(&folds);
        for row in &layout.rows {
            let owned = matches!(
                row.kind(),
                RowKind::FileHeader
                    | RowKind::Spacer
                    | RowKind::Gap
                    | RowKind::Context
                    | RowKind::Note
            );
            assert_eq!(
                row.hunk.is_none(),
                owned,
                "{:?} {:?}",
                row.kind(),
                row.text()
            );
            if let Some(hunk) = row.hunk {
                // The row's unit is its hunk's unit.
                assert_eq!(layout.hunks[hunk].unit, row.unit, "{row:?}");
            }
        }
        // Each hunk's rows start where its unit's place is: its header, when it has one, or
        // else its first changed line.
        for (hunk, Hunk { unit, .. }) in layout.hunks.iter().enumerate() {
            let rows: Vec<usize> = (0..layout.rows.len())
                .filter(|&index| layout.rows[index].hunk == Some(hunk))
                .collect();
            assert_eq!(layout.unit_rows[unit], rows[0]);
        }
    }

    /// Hunks show three unchanged lines around them, the rest of the file folds into gap rows,
    /// and expanding a gap reveals more lines above the next hunk.
    #[test]
    fn hunks_show_context_and_gaps() {
        let engine = std::fs::read_to_string(
            review_core::demo::fixture_dir().join("trees/head-1/src/engine.rs"),
        )
        .unwrap();
        let text = Arc::new(FullText::new(&engine));
        let folds = Folds {
            texts: HashMap::from([("src/engine.rs".to_owned(), Arc::clone(&text))]),
            files: HashMap::from([("src/engine.rs".to_owned(), false)]),
            ..Folds::default()
        };
        let (_dir, layout) = demo_layout(&folds);
        let index = layout
            .files
            .iter()
            .position(|section| section.path == "src/engine.rs")
            .unwrap();
        let rows: Vec<&Row> = layout.rows.iter().filter(|row| row.file == index).collect();
        let context: Vec<usize> = rows
            .iter()
            .filter(|row| row.kind() == RowKind::Context)
            .flat_map(|row| row.cells())
            .filter_map(|cell| cell.head.as_ref().map(|head| head.line))
            .collect();
        // The unreviewed hunk at line 27 (index 26) shows the three lines above it as context;
        // line 0 is far from every hunk.
        assert!(context.contains(&23) && context.contains(&25) && !context.contains(&0));
        let gaps: Vec<&&Row> = rows
            .iter()
            .filter(|row| row.kind() == RowKind::Gap)
            .collect();
        assert_ne!(
            gaps.len(),
            0,
            "the file has unchanged lines between its hunks"
        );
        let Some(Fold::Gap { at, .. }) = gaps[0].note().and_then(|note| note.fold) else {
            panic!("a gap row folds a gap");
        };

        let mut wider = folds.clone();
        wider.gaps.insert(("src/engine.rs".to_owned(), at), 20);
        let (_dir, expanded) = demo_layout(&wider);
        let shown = expanded
            .rows
            .iter()
            .filter(|row| row.file == index && row.kind() == RowKind::Context)
            .count();
        assert!(shown > context.len(), "{shown} > {}", context.len());
    }

    /// The demo's `src/engine.rs` in the file view: every head line shows once, in order, with
    /// no gaps and no hunk headers, each hunk's rows are contiguous and its units point at its
    /// first row, and every hunk has what its header would say.
    #[test]
    fn the_file_view_shows_every_line_without_headers() {
        let engine = std::fs::read_to_string(
            review_core::demo::fixture_dir().join("trees/head-1/src/engine.rs"),
        )
        .unwrap();
        let text = Arc::new(FullText::new(&engine));
        let folds = Folds {
            texts: HashMap::from([("src/engine.rs".to_owned(), Arc::clone(&text))]),
            view: DiffView::File,
            ..Folds::default()
        };
        let (_dir, layout) = demo_layout(&folds);
        let index = layout
            .files
            .iter()
            .position(|section| section.path == "src/engine.rs")
            .unwrap();
        let rows: Vec<&Row> = layout.rows.iter().filter(|row| row.file == index).collect();
        assert!(
            rows.iter()
                .all(|row| !matches!(row.kind(), RowKind::Gap | RowKind::Header)),
            "no gaps and no hunk headers"
        );
        // Unchanged, edited and added lines are the head file, each line once and in order.
        let head: Vec<usize> = rows
            .iter()
            .flat_map(|row| row.cells())
            .filter(|cell| {
                matches!(
                    cell.kind,
                    RowKind::Context | RowKind::Edited | RowKind::Added
                )
            })
            .filter_map(|cell| cell.head.as_ref().map(|head| head.line))
            .collect();
        assert_eq!(head, (0..text.lines.len()).collect::<Vec<_>>());
        for (hunk, Hunk { unit, .. }) in layout.hunks.iter().enumerate() {
            if !rows.iter().any(|row| row.unit == *unit) {
                continue;
            }
            let owned: Vec<usize> = (0..layout.rows.len())
                .filter(|&row| layout.rows[row].hunk == Some(hunk))
                .collect();
            let first = owned[0];
            assert_eq!(layout.unit_rows[unit], first);
            assert!(layout.rows[first].is_code() || layout.rows[first].note().is_some());
            assert_eq!(owned.len(), owned.last().unwrap() - first + 1, "contiguous");
            assert_eq!(layout.hunk_notes[hunk].kind, RowKind::Header);
        }
        assert_eq!(layout.hunk_notes.len(), layout.hunks.len());
    }

    /// Each change is a hunk of its own in either view, as git's hunks are, never joined with
    /// changes a few lines apart, as the demo's `scripts/install.sh` has.
    #[test]
    fn each_change_is_a_hunk() {
        // Reviewed files open, so both views lay out every unit.
        let (_dir, hunks) = demo_layout(&Folds {
            approved: Approved::Shown,
            ..Folds::default()
        });
        let (_dir, file) = demo_layout(&Folds {
            view: DiffView::File,
            ..Folds::default()
        });
        let units = |layout: &Layout| {
            layout
                .hunks
                .iter()
                .map(|hunk| hunk.unit)
                .collect::<Vec<_>>()
        };
        assert_eq!(units(&file), units(&hunks), "the same hunks in either view");
        let distinct: HashSet<UnitId> = units(&hunks).into_iter().collect();
        assert_eq!(distinct.len(), hunks.hunks.len(), "a hunk for each unit");
    }

    /// The file view shows approved hunks' code, and a hunk the reviewer folded keeps its
    /// header alone, so it can still be selected and unfolded.
    #[test]
    fn the_file_view_shows_approved_code_and_heads_folded_hunks() {
        let view_file = |folds: Folds| {
            approved_engine(Folds {
                view: DiffView::File,
                ..folds
            })
        };
        let (_dir, layout) = view_file(Folds::default());
        assert!(
            layout
                .rows
                .iter()
                .any(|row| matches!(row.kind(), RowKind::Added | RowKind::Removed)),
            "approved code shows"
        );
        assert!(!layout.files[0].collapsed, "a reviewed file stays open");
        assert!(layout.rows.iter().all(|row| row.kind() != RowKind::Header));

        let folded = layout.hunks[0].unit;
        let (_dir, layout) = view_file(Folds {
            units: HashSet::from([folded]),
            ..Folds::default()
        });
        let row = layout.unit_rows[&folded];
        assert_eq!(layout.rows[row].kind(), RowKind::Header);
        assert_eq!(layout.rows[row].hunk, Some(0));
        assert!(layout.rows[row].note().unwrap().folded);
    }

    /// The demo's `src/engine.rs` with its text, every hunk approved, laid out with `folds`
    /// in the normal density.
    fn approved_engine(folds: Folds) -> (tempfile::TempDir, Layout) {
        let (dir, review) = demo_review(1);
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let units = view.units_of_file("src/engine.rs");
        review
            .mark(&view, &units, review_core::MarkVerdict::Accepted)
            .unwrap();
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let engine = std::fs::read_to_string(
            review_core::demo::fixture_dir().join("trees/head-1/src/engine.rs"),
        )
        .unwrap();
        let folds = Folds {
            texts: HashMap::from([("src/engine.rs".to_owned(), Arc::new(FullText::new(&engine)))]),
            ..folds
        };
        let presentation = Presentation {
            density: crate::config::Density::Normal,
            ..Presentation::default()
        };
        let layout = present(
            build(&crate::rows::Source::Verified(view), &units, &folds),
            presentation,
        );
        (dir, layout)
    }

    fn kinds(layout: &Layout) -> Vec<RowKind> {
        let mut kinds: Vec<RowKind> = layout.rows.iter().map(Row::kind).collect();
        // The empty row a single file's diff starts with, where a folder's diff names it.
        if kinds.first() == Some(&RowKind::Spacer) {
            kinds.remove(0);
        }
        kinds
    }

    /// A single file's diff starts with an empty row where a folder's diff names the file, so
    /// its code shows on the same rows either way.
    #[test]
    fn a_single_files_diff_starts_with_an_empty_row() {
        let (_dir, layout) = approved_engine(Folds {
            single_file: true,
            ..Folds::default()
        });
        assert_eq!(layout.rows.first().map(Row::kind), Some(RowKind::Spacer));
        assert_eq!(
            layout.rows.get(1).map(Row::kind),
            Some(RowKind::Header),
            "its first hunk follows"
        );
    }

    /// Approved hunks fold to their headers, and with every hunk folded no unchanged line
    /// shows: no context, no gap and no spacer, only a list of headers.
    #[test]
    fn folded_hunks_show_no_context_without_a_change() {
        let (_dir, layout) = approved_engine(Folds {
            single_file: true,
            ..Folds::default()
        });
        assert_eq!(layout.hunks.len(), 3);
        assert_eq!(kinds(&layout), [RowKind::Header; 3]);
        // After the empty row the diff starts with, only the folded headers.
        assert!(
            layout
                .rows
                .iter()
                .skip(1)
                .all(|row| row.note().is_some_and(|note| note.folded))
        );
        for (hunk, Hunk { unit, .. }) in layout.hunks.iter().enumerate() {
            assert_eq!(layout.unit_rows[unit], hunk + 1);
            assert_eq!(layout.rows[hunk + 1].hunk, Some(hunk));
        }
    }

    /// An unfolded hunk between folded ones keeps the gaps that lead to and from it, with air
    /// before them, while the file's leading and trailing lines beside folded hunks stay out.
    #[test]
    fn context_beside_an_unfolded_hunk_stays() {
        let (_dir, folded) = approved_engine(Folds {
            single_file: true,
            ..Folds::default()
        });
        let middle = [folded.hunks[1].unit];
        let (_dir, layout) = approved_engine(Folds {
            single_file: true,
            opened: middle.iter().copied().collect(),
            ..Folds::default()
        });
        let kinds = kinds(&layout);
        assert_eq!(kinds.first(), Some(&RowKind::Header), "{kinds:?}");
        assert_eq!(kinds.last(), Some(&RowKind::Header), "{kinds:?}");
        assert_eq!(
            kinds.iter().filter(|&&kind| kind == RowKind::Gap).count(),
            2,
            "{kinds:?}"
        );
        assert_eq!(kinds[1..3], [RowKind::Spacer, RowKind::Gap]);
        let header = layout.unit_rows[&middle[0]];
        assert!(!layout.rows[header].note().unwrap().folded);
        assert_eq!(layout.rows[header + 1].kind(), RowKind::Context);
        let last = layout.rows.last().unwrap();
        assert!(last.note().unwrap().folded);
        assert_eq!(layout.rows[layout.rows.len() - 2].kind(), RowKind::Gap);
        // Lines keep their base numbers whatever folds around them.
        let unfolded = layout
            .rows
            .iter()
            .filter(|row| row.kind() == RowKind::Context)
            .filter_map(|row| row.cells().next())
            .all(|cell| cell.base.is_some() && cell.head.is_some());
        assert!(unfolded);
    }

    /// Showing approved hunks lays them out like any other, context and gaps included, and a
    /// fully reviewed file no longer folds to its header; the reviewer's own folds still win.
    #[test]
    fn approved_hunks_show_on_request() {
        let (_dir, shown) = approved_engine(Folds {
            single_file: true,
            approved: Approved::Shown,
            ..Folds::default()
        });
        let kinds = kinds(&shown);
        // The engine's first change is within its first ten lines and its last within its
        // last ten, so its start and end show whole; longer stretches between hunks fold, and
        // an open hunk without a tag has no header.
        assert_eq!(kinds.first(), Some(&RowKind::Context), "{kinds:?}");
        assert_eq!(kinds.last(), Some(&RowKind::Context), "{kinds:?}");
        assert!(kinds.contains(&RowKind::Gap), "{kinds:?}");
        assert!(kinds.contains(&RowKind::Added) || kinds.contains(&RowKind::Removed));
        assert!(
            shown
                .rows
                .iter()
                .filter_map(Row::note)
                .all(|note| !note.folded)
        );

        let (_dir, sections) = approved_engine(Folds::default());
        assert!(sections.files[0].collapsed, "a fully reviewed file folds");
        let (_dir, sections) = approved_engine(Folds {
            approved: Approved::Shown,
            ..Folds::default()
        });
        assert!(!sections.files[0].collapsed);

        let first = [shown.hunks[0].unit];
        let (_dir, refolded) = approved_engine(Folds {
            single_file: true,
            approved: Approved::Shown,
            units: first.iter().copied().collect(),
            ..Folds::default()
        });
        let header = refolded.unit_rows[&first[0]];
        assert_eq!(
            header, 1,
            "no context leads to a folded first hunk, after the empty row"
        );
        assert!(refolded.rows[header].note().unwrap().folded);
    }
}
