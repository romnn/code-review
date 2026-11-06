//! The files pane: its header, the review's progress under it, and the tree's rows.

use review_core::ChangeKind;
use review_core::tree::NodeKind;

use super::{Bar, Palette, Piece, Room, Weight};
use crate::color::Hsla;
use crate::model::Model;
use crate::scroll::float;
use crate::state::Progress;

/// The files pane's header: how many files there are and, while the tree is filtered, how
/// many show and the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesHeader {
    /// `Files 12`, or `Files 3/12` while filtered; bright while the tree has the keys.
    pub title: Piece,
    /// The filter, such as `/engine`, in the accent.
    pub filter: Option<Piece>,
}

/// The files pane's header.
#[must_use]
pub fn files_header(model: &Model, palette: &Palette) -> FilesHeader {
    let files = model
        .tree
        .iter()
        .filter(|node| node.kind == NodeKind::File)
        .count();
    let filter = model.filter_query().map(|query| {
        let shown = model
            .visible_tree()
            .iter()
            .filter(|&&index| {
                model
                    .tree
                    .get(index)
                    .is_some_and(|node| node.kind == NodeKind::File)
            })
            .count();
        (format!("/{query}"), shown)
    });
    let title = match &filter {
        Some((_, shown)) => format!("Files {shown}/{files}"),
        None => format!("Files {files}"),
    };
    FilesHeader {
        title: Piece::new(
            title,
            if tree_focused(model) {
                palette.text
            } else {
                palette.dim
            },
        )
        .weighted(Weight::Semibold),
        filter: filter.map(|(query, _)| Piece::new(query, palette.accent)),
    }
}

/// Whether the tree has the keys.
fn tree_focused(model: &Model) -> bool {
    model.mode == crate::model::Mode::Tree && model.overlay.is_none()
}

/// A row of the file tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// The node, by index into the model's tree, for a press.
    pub index: usize,
    /// How deep the node is: its indent, in columns as wide as the chevron's.
    pub depth: usize,
    /// A folder's `▸` when folded and `▾` when open; empty for a file.
    pub chevron: Piece,
    /// The change kind's letter, colored by kind, or a blank in the same column.
    pub kind: Option<Piece>,
    /// Whether the file was deleted, which strikes its name through.
    pub deleted: bool,
    /// The name, colored by review state; a folder's ends with `/` and is a little quieter.
    pub name: Piece,
    /// The review state's glyph, only for state that needs action or confirms done.
    pub glyph: Piece,
    /// The row's background: the cursor's while the tree has the keys, a fainter one on the
    /// file whose diff shows.
    pub background: Option<Hsla>,
}

/// The row at `position` among the visible tree rows.
#[must_use]
pub fn tree_row(model: &Model, palette: &Palette, position: usize) -> Option<TreeRow> {
    let visible = model.visible_tree();
    let index = *visible.get(position)?;
    let node = model.tree.get(index)?;
    let directory = node.kind == NodeKind::Directory;
    let progress = Progress::of(&node.counts);
    let collapsed = model.tree_collapsed.contains(&node.path);
    let shown = !directory && model.selected.as_deref() == Some(node.path.as_str());
    let cursor = model.tree_cursor.min(visible.len().saturating_sub(1));
    let on_cursor = position == cursor && tree_focused(model);
    // Only what still needs the reviewer carries ink; done files recede, so the eye lands on
    // open, flagged and changed ones.
    let done = progress == Progress::Done;
    let kind = node.change.as_ref().map(|change| {
        let color = if done {
            palette.faint
        } else {
            palette.tone(change.tone())
        };
        Piece::new(change.kind.letter().to_string(), color)
    });
    let (name_color, glyph_color) = if done {
        (palette.dim, palette.faint)
    } else {
        (palette.progress(progress), palette.progress(progress))
    };
    let background = if on_cursor {
        Some(palette.raised)
    } else if shown {
        Some(palette.raised.alpha(0.5))
    } else {
        None
    };
    Some(TreeRow {
        index,
        depth: node.depth,
        chevron: Piece::new(
            match (directory, collapsed) {
                (true, true) => "▸",
                (true, false) => "▾",
                (false, _) => "",
            },
            palette.faint,
        ),
        kind,
        deleted: node
            .change
            .as_ref()
            .is_some_and(|change| change.kind == ChangeKind::Deleted),
        name: if directory {
            Piece::new(format!("{}/", node.name), name_color.alpha(0.8))
        } else {
            Piece::new(node.name.clone(), name_color)
        },
        glyph: Piece::new(progress_glyph(progress), glyph_color),
        background,
    })
}

/// The glyph of a review progress: one per file or folder.
#[must_use]
pub fn progress_glyph(progress: Progress) -> &'static str {
    match progress {
        // Only state that needs action or confirms done carries ink.
        Progress::Open => "",
        Progress::Partial => "◐",
        Progress::Done => "✓",
        Progress::Flagged => "!",
        Progress::Changed => "↻",
    }
}

/// The cells of the files pane's progress labels, `files` and `hunks`.
const PROGRESS_LABEL_CELLS: usize = 5;

/// The cells of a progress line's percent, up to `100%`.
const PERCENT_CELLS: usize = 4;

/// The fewest cells a progress line's bar takes before it makes way.
const PROGRESS_BAR_CELLS: usize = 3;

/// Which parts of the files pane's progress lines show; the counts always do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressParts {
    /// The label, `files` or `hunks`.
    pub label: bool,
    /// The bar.
    pub bar: bool,
    /// The share done, such as `40%`.
    pub percent: bool,
}

/// One labeled progress line of the files pane: a bar across the pane, the done and total
/// counts, and the share done in whole percent, rounded down so 100% means all of it.
///
/// Its text is padded to the widest counts of the lines, so in a monospace font the bars of
/// lines with equally long labels line up.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressLine {
    /// `files` or `hunks`, when it shows.
    pub label: Option<Piece>,
    /// The bar, when it shows.
    pub bar: Option<Bar>,
    /// The counts, `done/total`, padded on the left.
    pub counts: Piece,
    /// The share done, such as ` 40%`, when it shows.
    pub percent: Option<Piece>,
}

/// The files pane's two progress lines, files then hunks, with the parts that fit `room`.
#[must_use]
pub fn progress_lines(model: &Model, palette: &Palette, room: Room) -> [ProgressLine; 2] {
    let (reviewed, total) = model.totals();
    let (files_done, files) = model.file_totals();
    let counts = count_width(&[(files_done, files), (reviewed, total)]);
    let parts = progress_parts(room, PROGRESS_LABEL_CELLS, counts);
    [
        progress_line("files", files_done, files, counts, parts, palette),
        progress_line("hunks", reviewed, total, counts, parts, palette),
    ]
}

/// The parts of a progress line that fit `room` with a label of `label` cells and counts of
/// `counts` cells: the bar makes way first, then the percent, then the label.
#[must_use]
pub fn progress_parts(room: Room, label: usize, counts: usize) -> ProgressParts {
    let candidates = [
        ProgressParts {
            label: true,
            bar: true,
            percent: true,
        },
        ProgressParts {
            label: true,
            bar: false,
            percent: true,
        },
        ProgressParts {
            label: true,
            bar: false,
            percent: false,
        },
    ];
    let width = |parts: &ProgressParts| {
        let cells = [
            Some(counts),
            parts.label.then_some(label),
            parts.bar.then_some(PROGRESS_BAR_CELLS),
            parts.percent.then_some(PERCENT_CELLS),
        ];
        let shown: Vec<usize> = cells.into_iter().flatten().collect();
        float(shown.iter().sum::<usize>()) * room.cell
            + room.gap * float(shown.len().saturating_sub(1))
    };
    candidates
        .into_iter()
        .find(|parts| width(parts) <= room.budget)
        .unwrap_or(ProgressParts {
            label: false,
            bar: false,
            percent: false,
        })
}

/// One progress line with only `parts` showing, so a narrow pane drops whole parts instead
/// of clipping them.
fn progress_line(
    label: &str,
    done: usize,
    total: usize,
    count_width: usize,
    parts: ProgressParts,
    palette: &Palette,
) -> ProgressLine {
    let percent = (done * 100).checked_div(total).unwrap_or(0);
    let complete = total > 0 && done >= total;
    ProgressLine {
        label: parts.label.then(|| Piece::new(label, palette.dim)),
        bar: parts.bar.then_some(Bar {
            fraction: float(percent) / 100.0,
            fill: if complete {
                palette.accepted
            } else {
                palette.accent
            },
            track: palette.hairline,
        }),
        counts: Piece::new(
            format!("{:>count_width$}", format!("{done}/{total}")),
            palette.dim,
        ),
        percent: parts.percent.then(|| {
            Piece::new(
                format!("{percent:>3}%"),
                if complete {
                    palette.accepted
                } else {
                    palette.text
                },
            )
        }),
    }
}

/// The characters the longest of `counts` takes as `done/total`.
fn count_width(counts: &[(usize, usize)]) -> usize {
    counts
        .iter()
        .map(|(done, total)| format!("{done}/{total}").len())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A narrowing files pane drops the bar first, then the percent, then the label; the
    /// counts always show.
    #[test]
    fn progress_lines_drop_parts_by_priority() {
        let parts = |label, bar, percent| ProgressParts {
            label,
            bar,
            percent,
        };
        // Cells 10 px wide and 8 px gaps: `files`, `12/34` and `100%` take 156 px, and the
        // bar's 30 px and its gap make 194.
        let fits = |budget| {
            progress_parts(
                Room {
                    budget,
                    cell: 10.0,
                    gap: 8.0,
                },
                PROGRESS_LABEL_CELLS,
                5,
            )
        };
        assert_eq!(fits(400.0), parts(true, true, true));
        assert_eq!(fits(194.0), parts(true, true, true));
        assert_eq!(fits(193.0), parts(true, false, true));
        assert_eq!(fits(156.0), parts(true, false, true));
        assert_eq!(fits(155.0), parts(true, false, false));
        assert_eq!(fits(108.0), parts(true, false, false));
        assert_eq!(fits(107.0), parts(false, false, false));
    }

    /// In a terminal's cells with one-cell gaps, the same parts make way in the same order.
    #[test]
    fn progress_lines_fit_cells_too() {
        let fits = |budget| {
            progress_parts(
                Room {
                    budget,
                    cell: 1.0,
                    gap: 1.0,
                },
                PROGRESS_LABEL_CELLS,
                5,
            )
        };
        // `files 12/34 100%` takes 16 cells and the bar 4 more.
        assert!(fits(20.0).bar);
        assert!(!fits(19.0).bar && fits(19.0).percent);
        assert!(!fits(15.0).percent && fits(15.0).label);
    }
}
