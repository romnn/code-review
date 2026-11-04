//! Export of accepted, flagged and annotated units as a numbered list for an agent.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::history::{UnitState, Verdict};
use crate::ids::{RevisionNumber, UnitId};
use crate::view::{Location, NoteAttachment, RevisionView};

/// Excerpts in exports are cut to this many characters.
const EXCERPT_CHARS: usize = 120;

/// One exported unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportItem {
    /// The item's one-based number.
    pub number: usize,
    /// The unit.
    pub unit: UnitId,
    /// Where it is.
    pub location: Location,
    /// The verdict of a direct or inherited mark that still holds.
    pub verdict: Option<Verdict>,
    /// The notes attached to the unit.
    pub notes: Vec<String>,
    /// A one-line excerpt of the change.
    pub excerpt: String,
}

/// A note that lost its unit and fell back to its file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutdatedNote {
    /// The file the note fell back to.
    pub path: String,
    /// The unit the note was written on.
    pub written_on: UnitId,
    /// The note text.
    pub text: String,
}

/// The export of one revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Export {
    /// The review key.
    pub review: String,
    /// The revision exported.
    pub revision: RevisionNumber,
    /// Flagged, accepted and annotated units, flagged first.
    pub items: Vec<ExportItem>,
    /// Notes whose units are gone.
    pub outdated_notes: Vec<OutdatedNote>,
}

/// Collects the flagged, accepted and annotated units of `view`.
#[must_use]
pub fn export(view: &RevisionView<'_>, review: &str) -> Export {
    let mut notes: BTreeMap<UnitId, Vec<String>> = BTreeMap::new();
    let mut outdated_notes = Vec::new();
    for note in view.notes() {
        match note.attached {
            NoteAttachment::Unit { unit } => notes.entry(unit).or_default().push(note.text),
            NoteAttachment::File { path } => outdated_notes.push(OutdatedNote {
                path,
                written_on: note.written_on,
                text: note.text,
            }),
        }
    }
    let mut items: Vec<ExportItem> = view
        .unit_ids()
        .into_iter()
        .filter_map(|id| {
            let verdict = match view.state(id).ok()? {
                UnitState::Reviewed { verdict, .. } => Some(verdict),
                _ => None,
            };
            let unit_notes = notes.remove(&id).unwrap_or_default();
            let wanted = matches!(verdict, Some(Verdict::Accepted | Verdict::Flagged))
                || !unit_notes.is_empty();
            wanted.then(|| -> Option<ExportItem> {
                Some(ExportItem {
                    number: 0,
                    unit: id,
                    location: view.location(id).ok()?,
                    verdict,
                    notes: unit_notes,
                    excerpt: view.excerpt(id, EXCERPT_CHARS).ok()?,
                })
            })?
        })
        .collect();
    items.sort_by_key(|item| (item.verdict != Some(Verdict::Flagged), item.unit));
    for (index, item) in items.iter_mut().enumerate() {
        item.number = index + 1;
    }
    Export {
        review: review.to_owned(),
        revision: view.number(),
        items,
        outdated_notes,
    }
}

impl Export {
    /// Renders the export as a numbered Markdown list.
    #[must_use]
    pub fn to_markdown(&self) -> String {
        use std::fmt::Write as _;

        let mut markdown = String::new();
        // Writing to a `String` cannot fail.
        writeln!(
            markdown,
            "# Review of {} (revision {})\n",
            self.review, self.revision
        )
        .ok();
        if self.items.is_empty() && self.outdated_notes.is_empty() {
            writeln!(markdown, "Nothing flagged, accepted or annotated.").ok();
        }
        for item in &self.items {
            let verdict = item.verdict.map_or("note", Verdict::name);
            let Location {
                path,
                line,
                end_line,
            } = &item.location;
            writeln!(
                markdown,
                "{}. **{verdict}** `{path}:{line}-{end_line}` ({})",
                item.number, item.unit
            )
            .ok();
            writeln!(markdown, "   `{}`", item.excerpt.replace('`', "'")).ok();
            for note in &item.notes {
                writeln!(markdown, "   - {note}").ok();
            }
        }
        if !self.outdated_notes.is_empty() {
            writeln!(markdown, "\n## Outdated notes\n").ok();
            for note in &self.outdated_notes {
                writeln!(
                    markdown,
                    "- `{}` (written on {}): {}",
                    note.path, note.written_on, note.text
                )
                .ok();
            }
        }
        markdown
    }
}
