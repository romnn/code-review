//! Flattens a step's units into single-line display rows for the virtualized list.
//!
//! Every decision about what a unit shows (collapse, moves, interdiffs, descriptions, notes) comes
//! from review-core; this module only lays those answers out as lines.

use std::collections::HashMap;
use std::ops::Range;

use review_core::analysis::UnitKind;
use review_core::{
    Collapse, DiffLineKind, LineEnding, NoteAttachment, RevisionView, SideDiff, UnitId, UnitState,
    Verdict, split_line_ending,
};

/// What a row shows, which decides its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// A unit's header: ID, state and location.
    Header,
    /// A removed line.
    Removed,
    /// An added line.
    Added,
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
    /// The unit the row belongs to.
    pub unit: UnitId,
    /// The index of the row's display hunk in [`Layout::hunks`].
    pub hunk: usize,
    /// What the row shows.
    pub kind: RowKind,
    /// The row's text, without a diff sign.
    pub text: String,
    /// Byte ranges of `text` that a token diff marks as really changed.
    pub highlights: Vec<Range<usize>>,
    /// For an added line, where it is in the head file.
    pub head: Option<HeadLine>,
    /// For a header, the review state badge, which is the bracketed part of `text`.
    pub badge: Option<Badge>,
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
    fn of(state: UnitState) -> Self {
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
}

/// Lays out `units` (in review order) of `view`.
#[must_use]
pub fn build(view: &RevisionView<'_>, units: &[UnitId]) -> Layout {
    let mut rows = Layout::default();
    let notes = view.notes();
    for hunk in view.display_hunks(units) {
        let hunk_index = rows.hunks.len();
        for &unit in &hunk {
            rows.unit_rows.insert(unit, rows.rows.len());
            rows.rows.push(Row {
                unit,
                hunk: hunk_index,
                kind: RowKind::Header,
                text: header(view, unit),
                highlights: Vec::new(),
                head: None,
                badge: view.state(unit).ok().map(Badge::of),
            });
            let mut push = |kind, text: String, highlights: Vec<Range<usize>>, head| {
                rows.rows.push(Row {
                    unit,
                    hunk: hunk_index,
                    kind,
                    text,
                    highlights,
                    head,
                    badge: None,
                });
            };
            unit_body(view, unit, &mut push);
            for note in &notes {
                if note.attached == (NoteAttachment::Unit { unit }) {
                    push(
                        RowKind::Note,
                        format!("note ({}): {}", note.written_on, note.text),
                        Vec::new(),
                        None,
                    );
                }
            }
        }
        rows.hunks.push(hunk);
    }
    rows
}

/// A unit's header line.
fn header(view: &RevisionView<'_>, unit: UnitId) -> String {
    let state = view
        .state(unit)
        .map_or_else(|_| "?".to_owned(), state_label);
    let location = view.location(unit).map_or_else(
        |_| String::new(),
        |location| format!("{}:{}-{}", location.path, location.line, location.end_line),
    );
    format!("{unit}  [{state}]  {location}")
}

/// Describes a unit's review state in a few words.
#[must_use]
pub fn state_label(state: UnitState) -> String {
    match state {
        UnitState::New => "new".to_owned(),
        UnitState::Unseen { .. } => "unseen".to_owned(),
        UnitState::Reviewed {
            verdict, inherited, ..
        } => {
            if inherited {
                format!("{} (carried)", verdict.name())
            } else {
                verdict.name().to_owned()
            }
        }
        UnitState::ChangedSinceReviewed { reviewed, .. } => format!("changed since {reviewed}"),
    }
}

type Push<'a> = dyn FnMut(RowKind, String, Vec<Range<usize>>, Option<HeadLine>) + 'a;

fn unit_body(view: &RevisionView<'_>, unit: UnitId, push: &mut Push<'_>) {
    match view.collapse(unit) {
        Ok(Some(Collapse::FormattingOnly)) => {
            push(
                RowKind::Collapsed,
                "formatting-only change (collapsed)".to_owned(),
                Vec::new(),
                None,
            );
            return;
        }
        Ok(Some(Collapse::Noise(category))) => {
            push(
                RowKind::Collapsed,
                format!("{} file (collapsed)", category.name()),
                Vec::new(),
                None,
            );
            return;
        }
        Ok(Some(Collapse::Moved(found))) => {
            let text = found
                .describe_for(unit)
                .unwrap_or_else(|| "moved".to_owned());
            push(RowKind::Moved, text, Vec::new(), None);
            return;
        }
        Ok(None) | Err(_) => {}
    }
    let (Ok(record), Ok(file)) = (view.unit(unit), view.file(unit)) else {
        return;
    };
    match &record.kind {
        UnitKind::Chunk(chunk) => {
            side_rows(
                &chunk.removed,
                &chunk.removed_highlights,
                RowKind::Removed,
                None,
                push,
            );
            let head = file.new.as_ref().map(|side| HeadLine {
                path: side.path.clone(),
                line: chunk.new.start,
            });
            side_rows(
                &chunk.added,
                &chunk.added_highlights,
                RowKind::Added,
                head.as_ref(),
                push,
            );
            if chunk.whitespace_only && !record.formatting_only {
                push(
                    RowKind::Info,
                    "whitespace only; not proven equivalent".to_owned(),
                    Vec::new(),
                    None,
                );
            }
        }
        UnitKind::File(_) => {
            let description = view.describe(unit).ok().flatten().unwrap_or_default();
            push(RowKind::Info, description, Vec::new(), None);
        }
    }
    for found in view.moves_touching(unit) {
        if let Some(text) = found.describe_for(unit) {
            push(RowKind::Moved, text, Vec::new(), None);
        }
        if found.target.unit == unit
            && let Some(inner) = &found.inner
        {
            diff_rows(inner, push);
        }
    }
    interdiff_rows(view, unit, push);
}

/// Pushes the interdiff of a unit that changed since it was reviewed.
fn interdiff_rows(view: &RevisionView<'_>, unit: UnitId, push: &mut Push<'_>) {
    if let Ok(Some(interdiff)) = view.interdiff(unit) {
        push(
            RowKind::Interdiff,
            format!("interdiff against {}", interdiff.reviewed),
            Vec::new(),
            None,
        );
        if !interdiff.base.changed && !interdiff.head.changed {
            push(
                RowKind::Interdiff,
                "identical to the reviewed version, but it changed in between".to_owned(),
                Vec::new(),
                None,
            );
        }
        for (title, side) in [
            (
                "removed side changed (the target branch moved)",
                &interdiff.base,
            ),
            (
                "added side changed (the change itself changed)",
                &interdiff.head,
            ),
        ] {
            if side.changed {
                push(RowKind::Interdiff, title.to_owned(), Vec::new(), None);
                diff_rows(side, push);
            }
        }
    }
}

/// Pushes the lines of one chunk side, with their highlights and line-ending markers; added
/// lines carry their head-file position, counting from `head`.
fn side_rows(
    text: &str,
    highlights: &[Range<usize>],
    kind: RowKind,
    head: Option<&HeadLine>,
    push: &mut Push<'_>,
) {
    let mut offset = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let split = split_line_ending(line);
        let line_highlights = highlights
            .iter()
            .filter(|range| range.end > offset && range.start < offset + split.content.len())
            .map(|range| {
                range.start.saturating_sub(offset)..(range.end - offset).min(split.content.len())
            })
            .collect();
        let position = head.map(|head| HeadLine {
            path: head.path.clone(),
            line: head.line + index,
        });
        push(
            kind,
            with_ending(split.content, split.ending),
            line_highlights,
            position,
        );
        offset += line.len();
    }
}

fn diff_rows(diff: &SideDiff, push: &mut Push<'_>) {
    for line in &diff.lines {
        let kind = match line.kind {
            DiffLineKind::Same => RowKind::NestedSame,
            DiffLineKind::Old => RowKind::NestedOld,
            DiffLineKind::New => RowKind::NestedNew,
        };
        push(kind, with_ending(&line.text, line.ending), Vec::new(), None);
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
}
