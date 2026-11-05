//! Lays a step's units out as single-line rows for the virtualized list, in file sections.
//!
//! Every decision about what a unit shows (collapse, moves, interdiffs, descriptions, notes) comes
//! from review-core; this module only lays those answers out as lines, and applies the view's
//! folds: collapsed files and hunks, expanded moves, and files shown in full.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use crate::syntax::FileSyntax;
use review_core::analysis::{Chunk, FileUnit, UnitKind};
use review_core::scope::breadcrumb;
use review_core::{
    Collapse, DiffLineKind, LineEnding, MoveView, NoteAttachment, RevisionView, Side, SideDiff,
    UnitId, UnitState, Verdict, split_line_ending,
};

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
    /// An old line of an interdiff or of a move's inner diff.
    NestedOld,
    /// A new line of an interdiff or of a move's inner diff.
    NestedNew,
    /// An unchanged line of an interdiff or of a move's inner diff.
    NestedSame,
    /// A one-line summary: a file unit's description or a hint.
    Info,
    /// A move: where the code went or came from.
    Moved,
    /// A collapsed formatting-only or noise unit.
    Collapsed,
    /// A section title of a "changed since reviewed" interdiff.
    Interdiff,
    /// A note.
    Note,
    /// Unchanged lines left out between hunks, which expand on request.
    Gap,
}

impl RowKind {
    /// Whether the row is a line of code, which copying and selecting a unit take.
    #[must_use]
    pub fn is_code(self) -> bool {
        matches!(
            self,
            Self::Removed
                | Self::Added
                | Self::Context
                | Self::NestedOld
                | Self::NestedNew
                | Self::NestedSame
        )
    }
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
    /// What the row shows.
    pub kind: RowKind,
    /// The row's text, without a diff sign.
    pub text: String,
    /// The length of the code in `text`, before any line-ending marker.
    pub content_len: usize,
    /// Byte ranges of `text` that a token diff marks as really changed.
    pub highlights: Vec<Range<usize>>,
    /// For a line of the head file, where it is.
    pub head: Option<HeadLine>,
    /// For a removed line, its zero-based line in the base file.
    pub base: Option<usize>,
    /// For a header, the review state badge, which is the bracketed part of `text`.
    pub badge: Option<Badge>,
    /// For a move, the unit at its other end.
    pub link: Option<UnitId>,
    /// What toggling this row folds.
    pub fold: Option<Fold>,
    /// For a unit header, the named scopes around the unit, such as `impl Ledger › fn apply`.
    pub scope: Option<String>,
    /// For a unit header, a short label of what kind of change it is, such as
    /// "only reformatting", "whitespace changed · not proven equivalent" or "changed since
    /// revision 1".
    pub tag: Option<String>,
    /// For a code row of a formatting-only change, whether to draw it without emphasis.
    pub quiet: bool,
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
    /// Flagged, possibly carried.
    Flagged,
    /// Reviewed in an earlier revision, but changed since.
    Changed,
}

impl Badge {
    /// The badge of a unit in `state`.
    #[must_use]
    pub fn of(state: UnitState) -> Self {
        match state {
            UnitState::New | UnitState::Unseen { .. } => Self::Unreviewed,
            UnitState::Reviewed { verdict, .. } => match verdict {
                Verdict::Seen => Self::Seen,
                Verdict::Accepted => Self::Accepted,
                Verdict::Flagged => Self::Flagged,
            },
            UnitState::ChangedSinceReviewed { .. } => Self::Changed,
        }
    }

    /// Whether the badge counts as reviewed.
    #[must_use]
    pub fn is_reviewed(self) -> bool {
        matches!(self, Self::Seen | Self::Accepted | Self::Flagged)
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

/// How many monospace cells `text` takes: wide characters count two, a tab four, and
/// combining marks none, so the widest line, not the longest in bytes, sets the scroll width.
#[must_use]
pub fn display_width(text: &str) -> usize {
    text.chars()
        .map(|character| match character {
            '\t' => 4,
            other => unicode_width::UnicodeWidthChar::width(other).unwrap_or(0),
        })
        .sum()
}

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
    /// Approved units the reviewer unfolded; other approved units show only their header.
    pub opened: HashSet<UnitId>,
    /// Whether the layout is one file's diff, without file headers.
    pub single_file: bool,
    /// Moves, formatting-only and noise units expanded to show their code.
    pub expanded: HashSet<UnitId>,
    /// Both versions' highlights and scopes, by display path, once they are computed.
    pub syntax: HashMap<String, Arc<FileSyntax>>,
}

impl Folds {
    /// Whether `unit`'s body is folded: when the reviewer folded it, or when it is approved
    /// (seen or accepted, and unchanged since) and the reviewer has not unfolded it. Flagged
    /// and unreviewed units always show.
    #[must_use]
    pub fn is_folded(&self, view: &RevisionView<'_>, unit: UnitId) -> bool {
        if self.units.contains(&unit) {
            return true;
        }
        let approved = matches!(
            view.state(unit),
            Ok(UnitState::Reviewed {
                verdict: Verdict::Seen | Verdict::Accepted,
                ..
            })
        );
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
            single_file: false,
            expanded: HashSet::new(),
            syntax: HashMap::new(),
        }
    }
}

/// The rows of one or more steps.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    /// The rows in display order.
    pub rows: Vec<Row>,
    /// The display hunks, each a list of units marked together.
    pub hunks: Vec<Vec<UnitId>>,
    /// The header row of each unit.
    pub unit_rows: HashMap<UnitId, usize>,
    /// The file sections in display order.
    pub files: Vec<FileSection>,
    /// The code row and the annotation row that take the most cells. Code and annotations
    /// render at different font sizes and indents, so the view compares the two in pixels and
    /// measures the wider one to set how far it scrolls sideways.
    pub widest_code: Option<(usize, usize)>,
    pub widest_annotation: Option<(usize, usize)>,
}

impl Row {
    /// The cells a row's text takes: a code row's code, or an annotation's text with the tag
    /// and scope that follow it on the same line.
    #[must_use]
    pub fn cells(&self) -> usize {
        let extra =
            |text: &Option<String>| text.as_deref().map_or(0, |text| display_width(text) + 3);
        if self.kind.is_code() {
            display_width(&self.text)
        } else {
            display_width(&self.text) + extra(&self.tag) + extra(&self.scope)
        }
    }
}

/// Lays out `units` (in review order) of `view`, applying `folds`.
#[must_use]
pub fn build(view: &RevisionView<'_>, units: &[UnitId], folds: &Folds) -> Layout {
    let mut layout = Layout::default();
    let notes = view.notes();
    for group in file_groups(view, units) {
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
            let text = folds.texts.get(&path).cloned();
            section_rows(
                &mut layout,
                view,
                file_index,
                &group,
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
        layout.rows.push(Row {
            fold: Some(Fold::File(file_index)),
            ..plain_row(
                file_index,
                first,
                None,
                RowKind::FileHeader,
                section.path.clone(),
            )
        });
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
            text.as_deref(),
            context,
            folds,
            &notes,
        );
    }
    let widest = |code: bool| {
        layout
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.kind.is_code() == code)
            .map(|(index, row)| (index, row.cells()))
            .max_by_key(|&(_, cells)| cells)
    };
    layout.widest_code = widest(true);
    layout.widest_annotation = widest(false);
    layout
}

/// Splits `units` into runs of consecutive units of one file.
fn file_groups(view: &RevisionView<'_>, units: &[UnitId]) -> Vec<Vec<UnitId>> {
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
fn file_section(view: &RevisionView<'_>, group: &[UnitId], folds: &Folds) -> FileSection {
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
        match view.state(unit).map(Badge::of) {
            Ok(Badge::Flagged) => {
                reviewed += 1;
                flagged += 1;
            }
            Ok(badge) if badge.is_reviewed() => reviewed += 1,
            Ok(Badge::Changed) => changed += 1,
            _ => {}
        }
    }
    let total = group.len();
    let collapsed = folds
        .files
        .get(&path)
        .copied()
        .unwrap_or(total > 0 && reviewed == total);
    FileSection {
        has_head: file.is_some_and(|file| file.new.is_some()),
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

fn plain_row(file: usize, unit: UnitId, hunk: Option<usize>, kind: RowKind, text: String) -> Row {
    Row {
        file,
        unit,
        hunk,
        kind,
        content_len: text.len(),
        text,
        highlights: Vec::new(),
        head: None,
        base: None,
        badge: None,
        link: None,
        fold: None,
        scope: None,
        tag: None,
        quiet: false,
    }
}

fn push_plain(layout: &mut Layout, file: usize, unit: UnitId, kind: RowKind, text: String) {
    layout.rows.push(plain_row(file, unit, None, kind, text));
}

/// Pushes one row of a unit body.
struct Pusher<'a> {
    layout: &'a mut Layout,
    file: usize,
    unit: UnitId,
    hunk: usize,
    /// Whether the unit's code draws without emphasis, as a formatting-only change does.
    quiet: bool,
}

impl Pusher<'_> {
    /// A row of the current unit, to adjust before adding it.
    fn row(&self, kind: RowKind, text: String) -> Row {
        let mut row = plain_row(self.file, self.unit, Some(self.hunk), kind, text);
        row.quiet = self.quiet && kind.is_code();
        row
    }

    fn add(&mut self, row: Row) {
        self.layout.rows.push(row);
    }

    fn push(&mut self, kind: RowKind, text: String) {
        let row = self.row(kind, text);
        self.add(row);
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
                    let mut description = move_text(span.found, unit);
                    if *blanks > 0 {
                        description = format!("{description} · +{}", plural_lines(*blanks));
                    }
                    let mut row = self.row(RowKind::Moved, description);
                    row.link = Some(other_end(span.found, unit));
                    row.fold = Some(Fold::Expand(unit));
                    self.add(row);
                    if span.expanded
                        && span.found.target.unit == self.unit
                        && let Some(inner) = &span.found.inner
                    {
                        // The move's edits say more than the moved lines themselves; their new
                        // side is this side's moved lines.
                        let start = head.map(|head| HeadLine {
                            path: head.path.clone(),
                            line: head.line + span.lines.start,
                        });
                        self.diff(inner, start.as_ref());
                    }
                }
                let shows_inner = span.found.target.unit == self.unit && span.found.inner.is_some();
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
            let mut row = self.row(kind, with_ending(split.content, split.ending));
            row.content_len = split.content.len();
            row.highlights = line_highlights;
            row.head = position;
            row.base = base.map(|start| start + index);
            self.add(row);
            offset += line.len();
        }
    }

    /// Pushes a nested diff. With `head`, the head line its new side starts at, its new and
    /// unchanged lines know their head lines, for line numbers and syntax.
    fn diff(&mut self, diff: &SideDiff, head: Option<&HeadLine>) {
        let mut new_line = 0;
        for line in &diff.lines {
            let kind = match line.kind {
                DiffLineKind::Same => RowKind::NestedSame,
                DiffLineKind::Old => RowKind::NestedOld,
                DiffLineKind::New => RowKind::NestedNew,
            };
            let mut row = self.row(kind, with_ending(&line.text, line.ending));
            row.content_len = line.text.len();
            if line.kind != DiffLineKind::Old {
                row.head = head.map(|head| HeadLine {
                    path: head.path.clone(),
                    line: head.line + new_line,
                });
                new_line += 1;
            }
            self.add(row);
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
        self.side(
            &chunk.removed,
            &chunk.removed_highlights,
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
            &chunk.added_highlights,
            RowKind::Added,
            head.as_ref(),
            None,
            added,
        );
    }
}

/// Pushes the one header row of display hunk `hunk`, made of `units`: where it is, what kind of
/// change it is, and the scopes around it. Every unit's header row is this row.
fn hunk_header(
    layout: &mut Layout,
    view: &RevisionView<'_>,
    file: usize,
    hunk: usize,
    units: &[UnitId],
    folds: &Folds,
) {
    let Some(&first) = units.first() else {
        return;
    };
    for &unit in units {
        layout.unit_rows.insert(unit, layout.rows.len());
    }
    let text = units
        .iter()
        .map(|&unit| header(view, unit))
        .reduce(|first, last| join_lines(&first, &last))
        .unwrap_or_default();
    let mut row = plain_row(file, first, Some(hunk), RowKind::Header, text);
    row.badge = view.state(first).ok().map(Badge::of);
    row.tag = hunk_tag(units.iter().map(|&unit| header_tag(view, unit)).collect());
    let path = layout.files.get(file).map(|section| section.path.clone());
    row.scope = view.scope_anchor(first).ok().flatten().and_then(|anchor| {
        let syntax = folds.syntax.get(path.as_deref()?)?;
        let side = match anchor.side {
            Side::Head => syntax.head.as_ref(),
            Side::Base => syntax.base.as_ref(),
        }?;
        let enclosing = side.scopes.enclosing(anchor.line);
        (!enclosing.is_empty()).then(|| breadcrumb(&enclosing))
    });
    layout.rows.push(row);
}

/// A note's lines, one per row; an empty note still takes one.
fn note_lines(text: &str) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() { vec![""] } else { lines }
}

/// The label of a hunk from its units' labels. A label the units share speaks for the hunk; when
/// they differ, each says only that the hunk includes it, so one proven reflow never labels a
/// real edit beside it "only reformatting".
fn hunk_tag(tags: Vec<Option<String>>) -> Option<String> {
    let first = tags.first()?.clone();
    if tags.iter().all(|tag| *tag == first) {
        return first;
    }
    let mut parts: Vec<String> = Vec::new();
    for tag in tags.into_iter().flatten() {
        let part = match tag.as_str() {
            "only reformatting" => "includes reformatting".to_owned(),
            "whitespace changed · not proven equivalent" => {
                "includes unproven whitespace changes".to_owned()
            }
            other if other.starts_with("changed since") => format!("partly {other}"),
            other => format!("includes {other}"),
        };
        if !parts.contains(&part) {
            parts.push(part);
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Joins two units' line descriptions into the span of a display hunk: "lines 3–9".
fn join_lines(first: &str, last: &str) -> String {
    let start = first
        .rsplit(' ')
        .next()
        .and_then(|range| range.split('–').next());
    let end = last
        .rsplit(' ')
        .next()
        .and_then(|range| range.rsplit('–').next());
    match (start, end) {
        (Some(start), Some(end))
            if start.parse::<usize>().is_ok() && end.parse::<usize>().is_ok() =>
        {
            format!("lines {start}–{end}")
        }
        _ => format!("{first} · {last}"),
    }
}

/// Pushes a unit's body and notes, unless the unit is folded.
fn unit_rows(
    layout: &mut Layout,
    view: &RevisionView<'_>,
    file: usize,
    hunk: usize,
    unit: UnitId,
    folds: &Folds,
    notes: &[review_core::ResolvedNote],
) {
    if folds.is_folded(view, unit) {
        return;
    }
    let quiet = matches!(view.collapse(unit), Ok(Some(Collapse::FormattingOnly)));
    let mut pusher = Pusher {
        layout,
        file,
        unit,
        hunk,
        quiet,
    };
    unit_body(&mut pusher, view, unit, folds);
    for note in notes {
        if note.attached == (NoteAttachment::Unit { unit }) {
            // Rows have one height, so each line of a note is a row of its own; the first says
            // where the note comes from.
            for (index, line) in note_lines(&note.text).into_iter().enumerate() {
                let mut row = pusher.row(RowKind::Note, line.to_owned());
                row.hunk = None;
                if index == 0 {
                    row.tag = Some(format!("note · revision {}", note.written_on.revision()));
                }
                pusher.add(row);
            }
        }
    }
}

/// The header's label for a change that is not an ordinary edit.
fn header_tag(view: &RevisionView<'_>, unit: UnitId) -> Option<String> {
    if let Ok(UnitState::ChangedSinceReviewed { reviewed, .. }) = view.state(unit) {
        return Some(format!("changed since revision {}", reviewed.revision()));
    }
    match view.collapse(unit) {
        Ok(Some(Collapse::FormattingOnly)) => return Some("only reformatting".to_owned()),
        Ok(Some(Collapse::Noise(category))) => return Some(category.name().to_owned()),
        _ => {}
    }
    let record = view.unit(unit).ok()?;
    let chunk = record.chunk()?;
    (chunk.whitespace_only && !record.formatting_only)
        .then(|| "whitespace changed · not proven equivalent".to_owned())
}

/// A unit's header line: where it is, such as "lines 27–29", or what a file unit changes.
fn header(view: &RevisionView<'_>, unit: UnitId) -> String {
    match view.unit(unit).ok().and_then(|record| record.chunk()) {
        Some(chunk) if chunk.added.is_empty() => format!("removed at line {}", chunk.new.start + 1),
        Some(chunk) if chunk.new.end() > chunk.new.start + 1 => {
            format!("lines {}–{}", chunk.new.start + 1, chunk.new.end())
        }
        Some(chunk) => format!("line {}", chunk.new.start + 1),
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
    /// Line indices within the side's text.
    lines: Range<usize>,
    expanded: bool,
}

/// The lines of `chunk` that `found` covers on `unit`'s side, as indices within that side.
fn moved_lines(found: &MoveView, unit: UnitId, chunk: &Chunk) -> Option<Range<usize>> {
    let (location, range, text) = if found.target.unit == unit {
        (&found.target.location, chunk.new, &chunk.added)
    } else if found.source.unit == unit {
        (&found.source.location, chunk.old, &chunk.removed)
    } else {
        return None;
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

/// The arrowed one-line description of a move from `unit`'s side, such as "↗ moved to
/// src/hash.rs:1–9"; the move's ID stays out of the reviewer's way.
fn move_text(found: &MoveView, unit: UnitId) -> String {
    let (arrow, direction, other) = if found.target.unit == unit {
        ("↙", "from", &found.source.location)
    } else {
        ("↗", "to", &found.target.location)
    };
    let edits = match found.kind {
        review_core::analysis::MoveKind::Moved => "",
        review_core::analysis::MoveKind::MovedWithEdits => " with edits",
    };
    format!(
        "{arrow} moved{edits} {direction} {}:{}–{}",
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

fn unit_body(pusher: &mut Pusher<'_>, view: &RevisionView<'_>, unit: UnitId, folds: &Folds) {
    let (Ok(record), Ok(file)) = (view.unit(unit), view.file(unit)) else {
        return;
    };
    let head_path = file.new.as_ref().map(|side| side.path.as_str());
    let expanded = folds.expanded.contains(&unit);
    match view.collapse(unit) {
        // Reformatting shows its code, without emphasis; the header says what it is.
        Ok(Some(Collapse::FormattingOnly)) => {
            if let Some(chunk) = record.chunk() {
                pusher.chunk(chunk, head_path);
            }
            return;
        }
        Ok(Some(Collapse::Noise(category))) => {
            let mut row = pusher.row(RowKind::Collapsed, format!("{} file", category.name()));
            row.fold = Some(Fold::Expand(unit));
            pusher.add(row);
            if expanded && let Some(chunk) = record.chunk() {
                pusher.chunk(chunk, head_path);
            }
            return;
        }
        Ok(Some(Collapse::Moved(found))) => {
            let mut row = pusher.row(RowKind::Moved, move_text(&found, unit));
            row.link = Some(other_end(&found, unit));
            row.fold = Some(Fold::Expand(unit));
            pusher.add(row);
            if expanded && let Some(chunk) = record.chunk() {
                pusher.chunk(chunk, head_path);
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
                    // reviewer folds them: they are changes to review like any other.
                    let edits = !source && found.inner.is_some();
                    Some(MovedSpan {
                        found,
                        lines: moved_lines(found, unit, chunk)?,
                        expanded: expanded != edits,
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
        }
    }
    for found in moves.iter().filter(|found| !described.contains(&found.id)) {
        let mut row = pusher.row(RowKind::Moved, move_text(found, unit));
        row.link = Some(other_end(found, unit));
        pusher.add(row);
    }
    interdiff_rows(pusher, view, unit);
}

/// Pushes the interdiff of a unit that changed since it was reviewed.
fn interdiff_rows(pusher: &mut Pusher<'_>, view: &RevisionView<'_>, unit: UnitId) {
    let Ok(Some(interdiff)) = view.interdiff(unit) else {
        return;
    };
    let since = interdiff.reviewed.revision();
    if !interdiff.base.changed && !interdiff.head.changed {
        pusher.push(
            RowKind::Interdiff,
            format!("same as reviewed in revision {since}, but it changed in between"),
        );
    }
    for (title, side) in [
        ("the target branch moved", &interdiff.base),
        ("the change itself changed", &interdiff.head),
    ] {
        if side.changed {
            pusher.push(
                RowKind::Interdiff,
                format!("since revision {since}: {title}"),
            );
            pusher.diff(side, None);
        }
    }
}

/// Lays out a file section: file units first, then each chunk with `context` unchanged lines
/// around it from `text`, and gap rows for the unchanged lines left out. Without `text`, chunks
/// show without context. With `context` at `usize::MAX`, the whole file shows.
#[expect(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one layout pass over a file's parts, whose context and gaps depend on each other"
)]
fn section_rows(
    layout: &mut Layout,
    view: &RevisionView<'_>,
    file: usize,
    group: &[UnitId],
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
    let mut chunks: Vec<(UnitId, usize, usize)> = Vec::new();
    let mut file_units = Vec::new();
    for &unit in group {
        match view.unit(unit).ok().and_then(|record| record.chunk()) {
            Some(chunk) => {
                chunks.push((unit, chunk.new.start, chunk.new.end().max(chunk.new.start)));
            }
            None => file_units.push(unit),
        }
    }
    if !file_units.is_empty() {
        let hunk = layout.hunks.len();
        layout.hunks.push(file_units.clone());
        hunk_header(layout, view, file, hunk, &file_units, folds);
        for unit in file_units {
            unit_rows(layout, view, file, hunk, unit, folds, notes);
        }
    }
    let Some(text) = text else {
        let ids: Vec<UnitId> = chunks.iter().map(|&(unit, ..)| unit).collect();
        for hunk in view.display_hunks(&ids) {
            let hunk_index = layout.hunks.len();
            hunk_header(layout, view, file, hunk_index, &hunk, folds);
            for &unit in &hunk {
                unit_rows(layout, view, file, hunk_index, unit, folds, notes);
            }
            layout.hunks.push(hunk);
        }
        return;
    };
    chunks.sort_by_key(|&(_, start, _)| start);
    // Changes of this file in other steps are never shown as unchanged context.
    let shown: HashSet<UnitId> = group.iter().copied().collect();
    let others: Vec<(usize, usize, UnitId)> = view
        .units_of_file(&path)
        .into_iter()
        .filter(|unit| !shown.contains(unit))
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
    };
    // Display hunks come from the engine's grouping of the units alone, never from whether the
    // text has arrived, so a hunk approves the same units before and after.
    let sorted: Vec<UnitId> = chunks.iter().map(|&(unit, ..)| unit).collect();
    let sizes: Vec<usize> = view.display_hunks(&sorted).iter().map(Vec::len).collect();
    let mut line = 0;
    let mut index = 0;
    for size in sizes {
        let Some(&(first, first_start, _)) = chunks.get(index) else {
            break;
        };
        let last = index + size.max(1) - 1;
        // Context after the hunk stops where the next hunk's changes start.
        let next_start = chunks.get(last + 1).map_or(lines, |&(_, start, _)| start);
        let revealed = folds
            .gaps
            .get(&(path.clone(), first_start))
            .copied()
            .unwrap_or(0);
        let hunk_units: Vec<UnitId> = chunks
            .get(index..=last)
            .unwrap_or_default()
            .iter()
            .map(|&(unit, ..)| unit)
            .collect();
        // A folded hunk is its header alone; its lines count toward the gaps around it.
        let folded = hunk_units.iter().all(|&unit| folds.is_folded(view, unit));
        let context_before = if folded {
            revealed
        } else {
            context.saturating_add(revealed)
        };
        let from = first_start.saturating_sub(context_before).max(line);
        if from > line {
            emitter.gap(layout, first, line..from, first_start);
        }
        let hunk_index = layout.hunks.len();
        hunk_header(layout, view, file, hunk_index, &hunk_units, folds);
        layout.hunks.push(hunk_units);
        emitter.lines(layout, first, from..first_start);
        if folded {
            let end = chunks.get(last).map_or(line, |&(_, _, end)| end);
            line = end.max(first_start);
            index = last + 1;
            continue;
        }
        for position in index..=last {
            let Some(&(unit, start, end)) = chunks.get(position) else {
                continue;
            };
            unit_rows(layout, view, file, hunk_index, unit, folds, notes);
            let until = match chunks.get(position + 1).filter(|_| position < last) {
                Some(&(_, next, _)) => next,
                None => end
                    .saturating_add(context)
                    .min(lines)
                    .min(next_start.max(end)),
            };
            emitter.lines(layout, unit, end..until.max(end));
            line = until.max(end).max(start);
        }
        index = last + 1;
    }
    if line < lines {
        let owner = chunks
            .last()
            .map_or_else(|| group.first().copied(), |&(unit, ..)| Some(unit));
        if let Some(owner) = owner {
            let revealed = folds.gaps.get(&(path.clone(), lines)).copied().unwrap_or(0);
            let until = line.saturating_add(revealed).min(lines);
            emitter.lines(layout, owner, line..until);
            if until < lines {
                emitter.gap(layout, owner, until..lines, lines);
            }
        }
    }
}

/// Emits unchanged head lines of one file section.
struct Context<'a> {
    file: usize,
    path: String,
    text: &'a FullText,
    others: &'a [(usize, usize, UnitId)],
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
                layout.rows.push(plain_row(
                    self.file,
                    owner,
                    hunk,
                    RowKind::Info,
                    format!("⋯ lines changed by {unit}, in another step"),
                ));
                index = end;
                continue;
            }
            let content = self.text.lines.get(index).cloned().unwrap_or_default();
            let mut row = plain_row(self.file, owner, hunk, RowKind::Context, content);
            row.head = Some(HeadLine {
                path: self.path.clone(),
                line: index,
            });
            layout.rows.push(row);
            index += 1;
        }
    }

    /// Pushes a gap row for the unchanged lines `range`, which ends at head line `at`.
    fn gap(&mut self, layout: &mut Layout, owner: UnitId, range: Range<usize>, at: usize) {
        let count = range.len();
        let mut row = plain_row(
            self.file,
            owner,
            None,
            RowKind::Gap,
            format!(
                "{count} unchanged line{}",
                if count == 1 { "" } else { "s" }
            ),
        );
        row.fold = Some(Fold::Gap {
            file: self.file,
            at,
        });
        layout.rows.push(row);
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
    use super::*;

    #[test]
    fn line_endings_are_marked() {
        assert_eq!(with_ending("a", LineEnding::Lf), "a");
        assert_eq!(with_ending("a", LineEnding::CrLf), "a␍");
        assert!(with_ending("a", LineEnding::Missing).contains("no newline"));
    }

    /// Builds the demo's layout with `folds`.
    fn demo_layout(folds: &Folds) -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().unwrap();
        let review = review_core::demo::prepare(
            &review_core::demo::DemoPaths::under(dir.path()),
            &review_core::demo::fixture_dir(),
            review_core::demo::DemoRequest::Fresh,
        )
        .unwrap();
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let order = review_core::walkthrough::partition(&view).order();
        let layout = build(&view, &order, folds);
        (dir, layout)
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
                    .join("trees/feature-1")
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
        assert_eq!(layout.rows[first.header_row].kind, RowKind::FileHeader);
        for section in layout.files.iter().skip(1) {
            assert_eq!(layout.rows[section.header_row - 1].kind, RowKind::Spacer);
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
    /// annotation's tag and scope.
    #[test]
    fn code_and_annotations_have_their_own_widest_row() {
        let (_dir, layout) = demo_layout(&Folds::default());
        let (code, code_cells) = layout.widest_code.unwrap();
        let (note, note_cells) = layout.widest_annotation.unwrap();
        assert!(layout.rows[code].kind.is_code());
        assert!(!layout.rows[note].kind.is_code());
        for row in &layout.rows {
            let limit = if row.kind.is_code() {
                code_cells
            } else {
                note_cells
            };
            assert!(row.cells() <= limit);
        }
    }

    /// The widest line sets the scroll width, not the longest in bytes.
    #[test]
    fn the_widest_line_wins_over_the_longest_in_bytes() {
        let accents = "é".repeat(200);
        let ascii = "x".repeat(300);
        let text = FullText::new(&format!("{accents}\n{ascii}\n"));
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
            .filter(|row| row.kind == RowKind::Info)
            .map(|row| row.text.as_str())
            .collect();
        assert!(
            info.contains(&"before  docs/current symlink (120000) to \"v1.md\""),
            "{info:?}"
        );
        assert!(info.contains(&"after   docs/current symlink (120000) to \"v2.md\""));
    }

    /// A hunk is "only reformatting" only when every unit is; otherwise it says what it
    /// includes.
    #[test]
    fn hunk_labels_need_every_unit() {
        let reflow = Some("only reformatting".to_owned());
        assert_eq!(
            hunk_tag(vec![reflow.clone(), reflow.clone()]),
            reflow,
            "shared by every unit"
        );
        assert_eq!(
            hunk_tag(vec![reflow.clone(), None]).as_deref(),
            Some("includes reformatting")
        );
        assert_eq!(
            hunk_tag(vec![
                None,
                Some("changed since revision 1".to_owned()),
                reflow
            ])
            .as_deref(),
            Some("partly changed since revision 1 · includes reformatting")
        );
        assert_eq!(hunk_tag(vec![None, None]), None);
    }

    /// A move with edits shows one linked row at each end, and its edits where the code
    /// arrives, which the reviewer can fold away.
    #[test]
    fn moves_show_as_linked_rows_with_their_edits() {
        let (_dir, layout) = demo_layout(&Folds::default());
        let moved: Vec<&Row> = layout
            .rows
            .iter()
            .filter(|row| row.kind == RowKind::Moved && row.text.contains("with edits"))
            .collect();
        assert_eq!(moved.len(), 2, "both ends of the move with edits");
        let target = moved.iter().find(|row| row.text.starts_with('↙')).unwrap();
        let source = moved.iter().find(|row| row.text.starts_with('↗')).unwrap();
        assert_eq!(target.link, Some(source.unit));
        assert_eq!(source.link, Some(target.unit));
        assert!(
            !target.text.contains("m1-"),
            "the move's ID stays out of the text: {}",
            target.text
        );
        let edits = |layout: &Layout| {
            layout
                .rows
                .iter()
                .any(|row| row.unit == target.unit && row.kind == RowKind::NestedNew)
        };
        assert!(edits(&layout), "the edits show by default");
        let added = layout
            .rows
            .iter()
            .filter(|row| row.unit == target.unit && row.kind == RowKind::Added)
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
        assert_eq!(next.kind, RowKind::Spacer);
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
            review_core::demo::fixture_dir().join("trees/feature-1/src/engine.rs"),
        )
        .unwrap();
        let folds = Folds {
            texts: HashMap::from([("src/engine.rs".to_owned(), Arc::new(FullText::new(&engine)))]),
            ..Folds::default()
        };
        let (_dir, layout) = demo_layout(&folds);
        for row in &layout.rows {
            let owned = matches!(
                row.kind,
                RowKind::FileHeader
                    | RowKind::Spacer
                    | RowKind::Gap
                    | RowKind::Context
                    | RowKind::Note
            );
            assert_eq!(row.hunk.is_none(), owned, "{:?} {:?}", row.kind, row.text);
            if let Some(hunk) = row.hunk {
                // The row's unit is one of its hunk's units.
                assert!(layout.hunks[hunk].contains(&row.unit), "{row:?}");
            }
        }
        // Each hunk's rows are contiguous and start at its header.
        for (hunk, units) in layout.hunks.iter().enumerate() {
            let rows: Vec<usize> = (0..layout.rows.len())
                .filter(|&index| layout.rows[index].hunk == Some(hunk))
                .collect();
            assert_eq!(layout.rows[rows[0]].kind, RowKind::Header);
            assert_eq!(layout.unit_rows[&units[0]], rows[0]);
        }
    }

    /// Hunks show three unchanged lines around them, the rest of the file folds into gap rows,
    /// and expanding a gap reveals more lines above the next hunk.
    #[test]
    fn hunks_show_context_and_gaps() {
        let engine = std::fs::read_to_string(
            review_core::demo::fixture_dir().join("trees/feature-1/src/engine.rs"),
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
            .filter(|row| row.kind == RowKind::Context)
            .filter_map(|row| row.head.as_ref().map(|head| head.line))
            .collect();
        // The unreviewed hunk at line 27 (index 26) shows the three lines above it as context;
        // line 0 is far from every hunk.
        assert!(context.contains(&23) && context.contains(&25) && !context.contains(&0));
        let gaps: Vec<&&Row> = rows.iter().filter(|row| row.kind == RowKind::Gap).collect();
        assert_ne!(
            gaps.len(),
            0,
            "the file has unchanged lines between its hunks"
        );
        let Some(Fold::Gap { at, .. }) = gaps[0].fold else {
            panic!("a gap row folds a gap");
        };

        let mut wider = folds.clone();
        wider.gaps.insert(("src/engine.rs".to_owned(), at), 20);
        let (_dir, expanded) = demo_layout(&wider);
        let shown = expanded
            .rows
            .iter()
            .filter(|row| row.file == index && row.kind == RowKind::Context)
            .count();
        assert!(shown > context.len(), "{shown} > {}", context.len());
    }
}
