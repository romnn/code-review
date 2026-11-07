//! Projects the unified rows for a presentation: removed and added lines paired side by side, a
//! blank row before each hunk and around each note for the normal and comfortable densities,
//! and the indices that point into the rows (unit headers, file headers, the widest rows) found
//! again afterwards.
//!
//! The projection only moves what [`super::build`] decided.
//! Every line of code survives exactly once, in order on its side; a missing side is explicit
//! padding; notes, gaps and headers keep their rows across the width, and every row keeps its
//! owners.
//! A nested diff (a move's edits, or what changed since a review) pairs the same way within its
//! own block: its old side on the left, its new side on the right.

use crate::config::{Density, DiffLayout};

use super::{Body, Cell, Layout, Note, Pair, Row, RowKind};

/// How the rows are laid out on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Presentation {
    /// The layout that shows, which a narrow window may have made unified.
    pub layout: DiffLayout,
    /// How much air the rows have.
    pub density: Density,
    /// Whether a blank row also comes between each gap of unchanged lines and the hunk after
    /// it: the comfortable density's air where rows cannot be taller.
    pub roomy: bool,
}

impl Default for Presentation {
    /// The rows as [`super::build`] makes them: unified, without blank rows.
    fn default() -> Self {
        Self {
            layout: DiffLayout::Unified,
            density: Density::Compact,
            roomy: false,
        }
    }
}

/// Projects the unified rows of `layout` for `presentation`, and finalizes the indices into them.
#[must_use]
pub fn present(mut layout: Layout, presentation: Presentation) -> Layout {
    for (index, row) in layout.rows.iter_mut().enumerate() {
        row.source = index;
    }
    let count = layout.rows.len();
    let mut projected: Vec<usize> = (0..count).collect();
    let rows = std::mem::take(&mut layout.rows);
    let rows = match presentation.layout {
        DiffLayout::Unified => rows,
        DiffLayout::SideBySide => pair(rows, &mut projected),
    };
    layout.rows = match presentation.density {
        Density::Compact => rows,
        Density::Normal | Density::Comfortable => space(rows, &mut projected, presentation.roomy),
    };
    let at = |row: usize| projected.get(row).copied().unwrap_or(row);
    for row in layout.unit_rows.values_mut() {
        *row = at(*row);
    }
    for section in &mut layout.files {
        section.header_row = at(section.header_row);
    }
    measure(&mut layout);
    layout.projected = projected;
    layout.presentation = presentation;
    layout
}

/// Which side of a pair a unified row's line goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Removed,
    Added,
    Context,
}

/// Where a line pairs: its side, and whether it is a line of a nested diff, whose lines pair
/// only with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lane {
    side: Side,
    nested: bool,
}

/// The lane a row pairs in, or `None` for a row that stays across the width: the tool's own
/// rows.
fn lane_of(row: &Row) -> Option<Lane> {
    let (side, nested) = match row.kind() {
        RowKind::Removed => (Side::Removed, false),
        RowKind::Added => (Side::Added, false),
        RowKind::Context => (Side::Context, false),
        RowKind::NestedOld => (Side::Removed, true),
        RowKind::NestedNew => (Side::Added, true),
        RowKind::NestedSame => (Side::Context, true),
        _ => return None,
    };
    Some(Lane { side, nested })
}

/// Takes the row's cell; only called on code rows.
fn into_cell(row: Row) -> Option<Cell> {
    match row.body {
        Body::Code(cell) => Some(cell),
        Body::Pair(_) | Body::Note(_) => None,
    }
}

/// Pairs removed lines with the added lines right after them, and unchanged lines with
/// themselves; a nested diff's old lines likewise with its new lines.
///
/// A run of removed lines and the run of added lines that follows it pair only within one unit
/// and hunk, and only with lines of their own kind (a nested diff's with a nested diff's),
/// line by line, and the shorter side is padded; anything between them, such as a move's row,
/// ends the run.
fn pair(rows: Vec<Row>, projected: &mut [usize]) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    let mut rows = rows.into_iter().peekable();
    while let Some(row) = rows.next() {
        let Some(Lane { side, nested }) = lane_of(&row) else {
            set(projected, row.source, out.len());
            out.push(row);
            continue;
        };
        match side {
            Side::Context => {
                set(projected, row.source, out.len());
                let Row {
                    file,
                    unit,
                    hunk,
                    source,
                    body,
                } = row;
                let Body::Code(cell) = body else {
                    continue;
                };
                // Each side shows the line where it is in its own file. A nested line keeps
                // its head line on the left too, which colors it as the head colors it; the
                // left half numbers it by its old side all the same.
                let left = if nested {
                    cell.clone()
                } else {
                    Cell {
                        head: None,
                        ..cell.clone()
                    }
                };
                let right = Cell { base: None, ..cell };
                out.push(Row {
                    file,
                    unit,
                    hunk,
                    source,
                    body: Body::Pair(Pair {
                        left: Some(left),
                        right: Some(right),
                    }),
                });
            }
            Side::Removed | Side::Added => {
                let (file, unit, hunk) = (row.file, row.unit, row.hunk);
                let run = |next: &Row, side: Side| {
                    lane_of(next) == Some(Lane { side, nested })
                        && next.unit == unit
                        && next.hunk == hunk
                };
                let mut removed: Vec<Row> = Vec::new();
                let mut added: Vec<Row> = Vec::new();
                if side == Side::Removed {
                    removed.push(row);
                    while let Some(next) = rows.next_if(|next| run(next, Side::Removed)) {
                        removed.push(next);
                    }
                } else {
                    added.push(row);
                }
                // The added lines right after the removed ones, of the same unit and hunk.
                while let Some(next) = rows.next_if(|next| run(next, Side::Added)) {
                    added.push(next);
                }
                let lines = removed.len().max(added.len());
                let mut removed = removed.into_iter();
                let mut added = added.into_iter();
                for _ in 0..lines {
                    let left = removed.next();
                    let right = added.next();
                    let index = out.len();
                    let mut source = usize::MAX;
                    for row in [&left, &right].into_iter().flatten() {
                        set(projected, row.source, index);
                        source = source.min(row.source);
                    }
                    out.push(Row {
                        file,
                        unit,
                        hunk,
                        source,
                        body: Body::Pair(Pair {
                            left: left.and_then(into_cell),
                            right: right.and_then(into_cell),
                        }),
                    });
                }
            }
        }
    }
    out
}

/// Records that the unified row `source` now shows at the row `at`, through whatever earlier
/// passes moved it.
fn set(projected: &mut [usize], source: usize, at: usize) {
    if let Some(slot) = projected.get_mut(source) {
        *slot = at;
    }
}

/// Adds a blank row before each hunk, before its header or before the gap of unchanged lines
/// that leads to it, and around each note; when `roomy`, also between such a gap and the
/// hunk's header.
///
/// Never at the top, never twice in a row, and never between two folded hunks' headers, so a
/// run of them reads as one list.
fn space(rows: Vec<Row>, projected: &mut [usize], roomy: bool) -> Vec<Row> {
    let folded = |row: &Row| row.note().is_some_and(|note| note.folded);
    let mut out: Vec<Row> = Vec::with_capacity(rows.len() + rows.len() / 8);
    // Where each row moved, so the earlier pass's targets can follow.
    let mut moved: Vec<usize> = Vec::with_capacity(rows.len());
    let mut rows = rows.into_iter().peekable();
    while let Some(row) = rows.next() {
        let kind = row.kind();
        let starts_hunk = match kind {
            RowKind::Header => out
                .last()
                .is_none_or(|last| last.kind() != RowKind::Gap && !(folded(last) && folded(&row))),
            RowKind::Gap => rows
                .peek()
                .is_some_and(|next| next.kind() == RowKind::Header),
            _ => false,
        };
        let after_air = out
            .last()
            .is_none_or(|last| matches!(last.kind(), RowKind::Spacer | RowKind::FileHeader));
        // A note stands apart from the code around it, a blank row above and below.
        let after_note = out.last().is_some_and(|last| last.kind() == RowKind::Note);
        let around_note = match kind {
            RowKind::Note => !after_note,
            RowKind::Spacer | RowKind::FileHeader => false,
            _ => after_note,
        };
        let after_gap = roomy
            && kind == RowKind::Header
            && out.last().is_some_and(|last| last.kind() == RowKind::Gap);
        if (starts_hunk || around_note || after_gap) && !after_air {
            out.push(Row {
                file: row.file,
                unit: row.unit,
                hunk: None,
                source: row.source,
                body: Body::Note(Note::plain(RowKind::Spacer, String::new())),
            });
        }
        moved.push(out.len());
        out.push(row);
    }
    for slot in projected.iter_mut() {
        if let Some(&at) = moved.get(*slot) {
            *slot = at;
        }
    }
    out
}

/// Finds the rows that set how far the list scrolls sideways: the widest full-width code row,
/// the widest note, and the widest side of a pair.
fn measure(layout: &mut Layout) {
    let mut code: Option<(usize, usize)> = None;
    let mut annotation: Option<(usize, usize)> = None;
    let mut half = 0;
    let wider = |best: &mut Option<(usize, usize)>, index: usize, cells: usize| {
        if best.is_none_or(|(_, widest)| cells > widest) {
            *best = Some((index, cells));
        }
    };
    for (index, row) in layout.rows.iter().enumerate() {
        match &row.body {
            Body::Code(cell) => wider(&mut code, index, cell.width()),
            Body::Note(note) => wider(&mut annotation, index, note.width()),
            Body::Pair(pair) => {
                for cell in [&pair.left, &pair.right].into_iter().flatten() {
                    half = half.max(cell.width());
                }
            }
        }
    }
    layout.widest_code = code;
    layout.widest_annotation = annotation;
    layout.widest_half = half;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use review_core::UnitId;

    use super::super::tests::demo_review;
    use super::super::{Folds, FullText, build};
    use super::*;

    fn unit_id(id: &str) -> UnitId {
        id.parse().unwrap()
    }

    /// A row of `unit` in `hunk` with one line of code.
    fn code(source_kind: RowKind, text: &str, unit: UnitId, hunk: Option<usize>) -> Row {
        Row {
            file: 0,
            unit,
            hunk,
            source: 0,
            body: Body::Code(Cell {
                kind: source_kind,
                text: text.to_owned(),
                content_len: text.len(),
                highlights: Vec::new(),
                head: None,
                base: None,
                quiet: false,
            }),
        }
    }

    fn note(kind: RowKind, text: &str, unit: UnitId, hunk: Option<usize>) -> Row {
        Row {
            file: 0,
            unit,
            hunk,
            source: 0,
            body: Body::Note(Note::plain(kind, text.to_owned())),
        }
    }

    fn layout(rows: Vec<Row>) -> Layout {
        Layout {
            rows,
            ..Layout::default()
        }
    }

    const SIDE_BY_SIDE: Presentation = Presentation {
        layout: DiffLayout::SideBySide,
        density: Density::Compact,
        roomy: false,
    };

    /// The lines of each side, in order, with `None` for padding.
    fn sides(layout: &Layout) -> (Vec<Option<String>>, Vec<Option<String>>) {
        layout
            .rows
            .iter()
            .filter_map(|row| match &row.body {
                Body::Pair(pair) => Some((
                    pair.left.as_ref().map(|cell| cell.text.clone()),
                    pair.right.as_ref().map(|cell| cell.text.clone()),
                )),
                Body::Code(_) | Body::Note(_) => None,
            })
            .unzip()
    }

    /// An unbalanced replacement pairs line by line and pads the shorter side; a real blank
    /// line stays a line, never padding, and every line survives once, in order.
    #[test]
    fn unbalanced_replacements_pad_without_losing_blank_lines() {
        let unit = unit_id("u1-0");
        let rows = vec![
            note(RowKind::Header, "lines 1–3", unit, Some(0)),
            code(RowKind::Removed, "old one", unit, Some(0)),
            code(RowKind::Removed, "", unit, Some(0)),
            code(RowKind::Removed, "old three", unit, Some(0)),
            code(RowKind::Added, "new one", unit, Some(0)),
            code(RowKind::Context, "same", unit, None),
            code(RowKind::Added, "", unit, Some(0)),
        ];
        let shown = present(layout(rows), SIDE_BY_SIDE);
        let (left, right) = sides(&shown);
        assert_eq!(
            left,
            [
                Some("old one".to_owned()),
                Some(String::new()),
                Some("old three".to_owned()),
                Some("same".to_owned()),
                None,
            ]
        );
        assert_eq!(
            right,
            [
                Some("new one".to_owned()),
                None,
                None,
                Some("same".to_owned()),
                Some(String::new()),
            ],
            "the blank added line after the context is its own line, not padding"
        );
        // The header stays across the width and keeps its hunk; context keeps no hunk.
        assert_eq!(shown.rows[0].kind(), RowKind::Header);
        assert_eq!(shown.rows[0].hunk, Some(0));
        assert_eq!(shown.rows[4].hunk, None);
        // Every unified row is found again: the second added line is in the last pair.
        assert_eq!(shown.projected, [0, 1, 2, 3, 1, 4, 5]);
    }

    /// A line-ending marker stays on its line through pairing, after the code that copying
    /// takes.
    #[test]
    fn line_ending_markers_survive_pairing() {
        let unit = unit_id("u1-0");
        let mut removed = code(RowKind::Removed, "", unit, Some(0));
        if let Body::Code(cell) = &mut removed.body {
            cell.text = super::super::with_ending("end", review_core::LineEnding::Missing);
            cell.content_len = 3;
        }
        let rows = vec![removed, code(RowKind::Added, "end", unit, Some(0))];
        let shown = present(layout(rows), SIDE_BY_SIDE);
        let Body::Pair(Pair {
            left: Some(left),
            right: Some(right),
        }) = &shown.rows[0].body
        else {
            panic!("one pair: {:?}", shown.rows);
        };
        assert!(left.text.contains("no newline at end of file"));
        assert_eq!(left.code(), "end");
        assert_eq!(right.code(), "end");
    }

    /// Pairs never cross a unit, a hunk or a row between the runs, and a nested diff's lines
    /// pair only with each other, never with the chunk's own.
    #[test]
    fn pairs_stay_within_their_unit_and_hunk() {
        let first = unit_id("u1-0");
        let second = unit_id("u1-1");
        let rows = vec![
            code(RowKind::Removed, "a", first, Some(0)),
            code(RowKind::Added, "b", second, Some(0)),
            code(RowKind::Removed, "c", second, Some(1)),
            note(RowKind::Moved, "↗ moved to x.rs:1–2", second, Some(1)),
            code(RowKind::Removed, "d", second, Some(1)),
            code(RowKind::NestedOld, "e", second, Some(1)),
            code(RowKind::NestedNew, "f", second, Some(1)),
            code(RowKind::NestedSame, "g", second, Some(1)),
            code(RowKind::Added, "h", second, Some(1)),
        ];
        let shown = present(layout(rows), SIDE_BY_SIDE);
        let kinds: Vec<RowKind> = shown.rows.iter().map(Row::kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::Pair,
                RowKind::Pair,
                RowKind::Pair,
                RowKind::Moved,
                RowKind::Pair,
                RowKind::Pair,
                RowKind::Pair,
                RowKind::Pair,
            ]
        );
        let (left, right) = sides(&shown);
        let some = |text: &str| Some(text.to_owned());
        assert_eq!(
            left,
            [
                some("a"),
                None,
                some("c"),
                some("d"),
                some("e"),
                some("g"),
                None
            ]
        );
        assert_eq!(
            right,
            [None, some("b"), None, None, some("f"), some("g"), some("h")]
        );
    }

    /// The normal and comfortable densities put a blank row before each hunk, or before the
    /// gap that leads to it, never at the top; unit and file header indices follow the rows.
    #[test]
    fn roomier_densities_add_air_before_hunks() {
        for density in [Density::Normal, Density::Comfortable] {
            air_before_hunks(density);
        }
    }

    fn air_before_hunks(density: Density) {
        let (unit, other) = (unit_id("u1-0"), unit_id("u1-1"));
        let rows = vec![
            note(RowKind::Gap, "3 unchanged lines", unit, None),
            note(RowKind::Header, "line 4", unit, Some(0)),
            code(RowKind::Added, "x", unit, Some(0)),
            note(RowKind::Gap, "9 unchanged lines", other, None),
            note(RowKind::Header, "line 14", other, Some(1)),
            code(RowKind::Removed, "y", other, Some(1)),
            note(RowKind::Header, "line 20", other, Some(2)),
        ];
        let mut unified = layout(rows);
        unified.unit_rows = HashMap::from([(unit, 1), (other, 4)]);
        let shown = present(
            unified,
            Presentation {
                layout: DiffLayout::Unified,
                density,
                roomy: false,
            },
        );
        let kinds: Vec<RowKind> = shown.rows.iter().map(Row::kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::Gap,
                RowKind::Header,
                RowKind::Added,
                RowKind::Spacer,
                RowKind::Gap,
                RowKind::Header,
                RowKind::Removed,
                RowKind::Spacer,
                RowKind::Header,
            ]
        );
        assert_eq!(shown.unit_rows[&unit], 1);
        assert_eq!(shown.unit_rows[&other], 5);
        assert_eq!(shown.projected, [0, 1, 2, 4, 5, 6, 8]);
        assert!(
            shown
                .rows
                .iter()
                .all(|row| row.kind() != RowKind::Spacer || row.hunk.is_none())
        );
    }

    /// The kinds of `rows` presented unified at `density`, roomy or not.
    fn spaced(rows: Vec<Row>, density: Density, roomy: bool) -> Vec<RowKind> {
        let presentation = Presentation {
            layout: DiffLayout::Unified,
            density,
            roomy,
        };
        present(layout(rows), presentation)
            .rows
            .iter()
            .map(Row::kind)
            .collect()
    }

    /// A note, however many lines long, stands apart with a blank row above and below in the
    /// normal and comfortable densities, and sits right under its code in the compact one.
    #[test]
    fn notes_have_air_unless_compact() {
        let unit = unit_id("u1-0");
        let rows = || {
            vec![
                note(RowKind::Header, "line 4", unit, Some(0)),
                code(RowKind::Added, "x", unit, Some(0)),
                note(RowKind::Note, "first", unit, None),
                note(RowKind::Note, "second", unit, None),
                code(RowKind::Context, "y", unit, None),
            ]
        };
        let lines = [RowKind::Note, RowKind::Note];
        for density in [Density::Normal, Density::Comfortable] {
            assert_eq!(
                spaced(rows(), density, false),
                [
                    &[RowKind::Header, RowKind::Added, RowKind::Spacer][..],
                    &lines,
                    &[RowKind::Spacer, RowKind::Context],
                ]
                .concat(),
                "{density:?}"
            );
        }
        assert_eq!(
            spaced(rows(), Density::Compact, false),
            [
                &[RowKind::Header, RowKind::Added][..],
                &lines,
                &[RowKind::Context],
            ]
            .concat()
        );
    }

    /// Where rows cannot be taller, the comfortable density also sets a gap of unchanged lines
    /// apart from the hunk after it.
    #[test]
    fn roomy_rows_set_gaps_apart_from_their_hunks() {
        let (unit, other) = (unit_id("u1-0"), unit_id("u1-1"));
        let rows = vec![
            note(RowKind::Header, "line 4", unit, Some(0)),
            code(RowKind::Added, "x", unit, Some(0)),
            note(RowKind::Gap, "9 unchanged lines", other, None),
            note(RowKind::Header, "line 14", other, Some(1)),
        ];
        assert_eq!(
            spaced(rows, Density::Comfortable, true),
            [
                RowKind::Header,
                RowKind::Added,
                RowKind::Spacer,
                RowKind::Gap,
                RowKind::Spacer,
                RowKind::Header,
            ]
        );
    }

    /// Folded hunks' headers in a row read as one list, without air between them; an unfolded
    /// hunk after them still gets its blank row.
    #[test]
    fn folded_headers_read_as_a_list() {
        let (first, second, third) = (unit_id("u1-0"), unit_id("u1-1"), unit_id("u1-2"));
        let folded = |unit, hunk| {
            let mut row = note(RowKind::Header, "line 1", unit, Some(hunk));
            if let Body::Note(note) = &mut row.body {
                note.folded = true;
            }
            row
        };
        let rows = vec![
            folded(first, 0),
            folded(second, 1),
            note(RowKind::Header, "line 9", third, Some(2)),
            code(RowKind::Added, "x", third, Some(2)),
            folded(third, 3),
        ];
        let shown = present(
            layout(rows),
            Presentation {
                layout: DiffLayout::Unified,
                density: Density::Normal,
                roomy: false,
            },
        );
        let kinds: Vec<RowKind> = shown.rows.iter().map(Row::kind).collect();
        assert_eq!(
            kinds,
            [
                RowKind::Header,
                RowKind::Header,
                RowKind::Spacer,
                RowKind::Header,
                RowKind::Added,
                RowKind::Spacer,
                RowKind::Header,
            ]
        );
    }

    /// Folds with the head text of every file of `layout`, read from the demo tree `tree`.
    fn with_texts(layout: &Layout, tree: &str) -> Folds {
        let texts: HashMap<String, Arc<FullText>> = layout
            .files
            .iter()
            .filter_map(|section| {
                let path = review_core::demo::fixture_dir()
                    .join("trees")
                    .join(tree)
                    .join(&section.path);
                let text = std::fs::read_to_string(path).ok()?;
                Some((section.path.clone(), Arc::new(FullText::new(&text))))
            })
            .collect();
        Folds {
            texts,
            ..Folds::default()
        }
    }

    /// On the demo, side by side: every line of code shows exactly once on its side, in order;
    /// moves, interdiffs and line-ending markers survive; context shows each side's own line
    /// numbers; and hunks keep their rows contiguous from their header.
    #[test]
    fn demo_pairs_keep_every_line_and_its_evidence() {
        let (_dir, review) = demo_review(2);
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let order = review_core::walkthrough::partition(&view).order();
        let folds = with_texts(&build(&view, &order, &Folds::default()), "feature-2");
        let unified = build(&view, &order, &folds);
        // An unchanged line shows on both halves; its left copy counts.
        let lines = |kinds: &[RowKind], rows: &[Row]| -> Vec<String> {
            rows.iter()
                .flat_map(|row| -> Vec<&Cell> {
                    match &row.body {
                        Body::Code(cell) => vec![cell],
                        Body::Pair(pair) => pair
                            .left
                            .iter()
                            .chain(pair.right.iter().filter(|cell| {
                                !matches!(cell.kind, RowKind::Context | RowKind::NestedSame)
                            }))
                            .collect(),
                        Body::Note(_) => Vec::new(),
                    }
                })
                .filter(|cell| kinds.contains(&cell.kind))
                .map(|cell| cell.text.clone())
                .collect()
        };
        let shown = present(unified.clone(), SIDE_BY_SIDE);
        for kinds in [
            &[RowKind::Removed][..],
            &[RowKind::Added][..],
            &[RowKind::NestedOld][..],
            &[RowKind::NestedNew][..],
            &[RowKind::NestedSame][..],
        ] {
            assert_eq!(
                lines(kinds, &shown.rows),
                lines(kinds, &unified.rows),
                "{kinds:?} lines survive once, in order"
            );
        }
        let notes = |rows: &[Row]| -> Vec<String> {
            rows.iter()
                .filter_map(Row::note)
                .map(|note| note.text.clone())
                .collect()
        };
        assert_eq!(notes(&shown.rows), notes(&unified.rows));
        assert!(
            shown
                .rows
                .iter()
                .any(|row| row.kind() == RowKind::Moved && row.text().contains("with edits")),
            "moves survive"
        );
        assert!(
            shown
                .rows
                .iter()
                .any(|row| row.kind() == RowKind::Interdiff),
            "interdiffs survive"
        );
        // An unchanged line shows its base line on the left and its head line on the right.
        let shifted = shown.rows.iter().find_map(|row| match &row.body {
            Body::Pair(Pair {
                left: Some(left),
                right: Some(right),
            }) if left.kind == RowKind::Context => {
                let head_line = right.head.as_ref()?.line;
                let base_line = left.base?;
                (head_line != base_line).then_some((head_line, base_line))
            }
            _ => None,
        });
        assert!(
            shifted.is_some(),
            "some context moved between base and head"
        );
        // Each hunk's rows are contiguous from its header, and a row's unit is its hunk's.
        for (hunk, units) in shown.hunks.iter().enumerate() {
            let rows: Vec<usize> = (0..shown.rows.len())
                .filter(|&index| shown.rows[index].hunk == Some(hunk))
                .collect();
            assert_eq!(shown.rows[rows[0]].kind(), RowKind::Header);
            assert_eq!(shown.unit_rows[&units[0]], rows[0]);
            for &index in &rows {
                assert!(units.contains(&shown.rows[index].unit));
            }
        }
        // Back to unified, the projection is the identity.
        let again = present(unified.clone(), Presentation::default());
        assert_eq!(again.rows.len(), unified.rows.len());
        assert!(
            again
                .projected
                .iter()
                .enumerate()
                .all(|(row, &at)| row == at)
        );
    }

    /// The rows of the nested block that follows the note at `note` in `rows`, with their
    /// pairs.
    fn nested_block(rows: &[Row], note: usize) -> Vec<(&Row, &Pair)> {
        let nested = |cell: &Cell| {
            matches!(
                cell.kind,
                RowKind::NestedOld | RowKind::NestedNew | RowKind::NestedSame
            )
        };
        rows.iter()
            .skip(note + 1)
            .map_while(|row| match &row.body {
                Body::Pair(pair) if pair.left.iter().chain(&pair.right).all(nested) => {
                    Some((row, pair))
                }
                Body::Code(_) | Body::Pair(_) | Body::Note(_) => None,
            })
            .collect()
    }

    /// The lines of a demo tree's file.
    fn tree_lines(tree: &str, path: &str) -> Vec<String> {
        let path = review_core::demo::fixture_dir()
            .join("trees")
            .join(tree)
            .join(path);
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Side by side, a move's edits pair like any diff: the left half is the source's lines
    /// numbered as they are in the source file (src/report.rs 9–19 in the demo's base), the
    /// right half the moved lines numbered as they are in src/format.rs; old lines pair only
    /// with new ones of the same block, and no row changes owners.
    #[test]
    fn a_move_with_edits_pairs_its_source_and_its_target() {
        let (_dir, review) = demo_review(1);
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let order = review_core::walkthrough::partition(&view).order();
        let shown = present(build(&view, &order, &Folds::default()), SIDE_BY_SIDE);
        let note = shown
            .rows
            .iter()
            .position(|row| {
                row.kind() == RowKind::Moved
                    && row.text().starts_with('↙')
                    && row.text().contains("from src/report.rs:9–19")
            })
            .unwrap();
        let moved = &shown.rows[note];
        let block = nested_block(&shown.rows, note);
        assert!(block.len() > 5, "the edits show paired: {}", block.len());
        let source = tree_lines("main-1", "src/report.rs");
        let target = tree_lines("feature-1", "src/format.rs");
        let mut lefts = Vec::new();
        let mut rights = Vec::new();
        for (row, pair) in &block {
            assert_eq!(
                (row.unit, row.hunk),
                (moved.unit, moved.hunk),
                "owners stay"
            );
            if let (Some(left), Some(right)) = (&pair.left, &pair.right) {
                let same = left.kind == RowKind::NestedSame;
                assert_eq!(
                    same,
                    right.kind == RowKind::NestedSame,
                    "unchanged lines pair with themselves, old lines with new ones"
                );
            }
            if let Some(left) = &pair.left {
                let line = left.base.expect("the source side is numbered");
                assert_eq!(left.code(), source[line], "left line {}", line + 1);
                lefts.push(line);
            }
            if let Some(right) = &pair.right {
                let head = right.head.as_ref().expect("the target side is numbered");
                assert_eq!(head.path, "src/format.rs");
                assert_eq!(
                    right.code(),
                    target[head.line],
                    "right line {}",
                    head.line + 1
                );
                rights.push(head.line);
            }
        }
        let expected: Vec<usize> = (8..8 + lefts.len()).collect();
        assert_eq!(lefts, expected, "the left numbers run from report.rs:9");
        assert_eq!(lefts.last().map(|line| line + 1), Some(19));
        assert!(rights.windows(2).all(|pair| pair[1] == pair[0] + 1));
    }

    /// Side by side, what changed since a review pairs too: the reviewed version's lines on
    /// the left, the current ones on the right, each block under its own title.
    #[test]
    fn an_interdiff_pairs_the_reviewed_and_the_current_version() {
        let (_dir, review) = demo_review(2);
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let order = review_core::walkthrough::partition(&view).order();
        let unified = build(&view, &order, &Folds::default());
        let shown = present(unified.clone(), SIDE_BY_SIDE);
        let titles: Vec<usize> = (0..shown.rows.len())
            .filter(|&index| shown.rows[index].kind() == RowKind::Interdiff)
            .collect();
        assert!(
            !titles.is_empty(),
            "the demo's second revision has interdiffs"
        );
        let mut changed = 0;
        for title in titles {
            let block = nested_block(&shown.rows, title);
            for (_, pair) in block {
                assert!(pair.left.iter().all(|cell| cell.kind != RowKind::NestedNew));
                assert!(
                    pair.right
                        .iter()
                        .all(|cell| cell.kind != RowKind::NestedOld)
                );
                if let (Some(left), Some(right)) = (&pair.left, &pair.right)
                    && left.kind == RowKind::NestedOld
                {
                    assert_eq!(right.kind, RowKind::NestedNew);
                    changed += 1;
                }
            }
        }
        assert!(changed > 0, "a changed line shows beside what was reviewed");
        let old = |rows: &[Row]| -> Vec<String> {
            rows.iter()
                .flat_map(|row| match &row.body {
                    Body::Code(cell) => vec![cell],
                    Body::Pair(pair) => pair.left.iter().collect(),
                    Body::Note(_) => Vec::new(),
                })
                .filter(|cell| cell.kind == RowKind::NestedOld)
                .map(|cell| cell.text.clone())
                .collect()
        };
        assert_eq!(old(&shown.rows), old(&unified.rows));
    }
}
