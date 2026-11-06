//! Changing how the diff is presented while keeping the reviewer's place.
//!
//! The layout (unified or side by side), the density, the font size, the narrow-window
//! fallback and whether approved hunks fold all change which rows show or how tall they are.
//! Each change, and each preview of a look and its rollback, goes through
//! `Model::transition`, which:
//!
//! - notes where every list is by row identity before anything changes: the diff by the unified
//!   row at its top, the tree and the buffer by their top row;
//! - stops scroll animations and track-click pages, which aim at offsets in the old rows;
//! - drops a drag, and a diff selection whose rows were projected anew;
//! - projects the rows again when the presentation changed, or lays them out again when
//!   approved hunks fold or show now, then puts every list back at its row, clamped to the
//!   extent the next frame lays out, which the model works out for the new code size from the
//!   last frame's geometry.
//!   Rows laid out with other folds than the place was taken on are other rows, so the diff
//!   finds its place again by what its top row showed: the same line, or else the header of
//!   the same unit.
//!
//! Places kept for files not shown are row identities too, so returning to a file after a
//! density change lands on the same line.

use super::{Model, ZOOM_RANGE};
use crate::config::{Approved, DiffLayout};
use crate::frame::{self, Geometry, Grid, Metrics, Scale, Viewports};
use crate::look;
use crate::rows::{Layout, Presentation, Row, RowKind};
use crate::scroll;
use crate::text::Pane;

/// The fewest cells of code each half must show for the side-by-side layout; narrower, the
/// diff shows unified until the window is wide enough again.
pub const MIN_HALF_CELLS: f32 = 40.0;

/// Where a list is, by row: the row at its top, and how far into that row.
///
/// The row and fraction are those of the offset narrowed to `f32` (see [`scroll::narrow`]), so a
/// place kept across a change of row height lands where it always did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Anchor {
    /// For the diff, the unified row ([`crate::rows::Row::source`]), which no presentation
    /// changes; for the tree and the buffer, the row itself.
    pub(crate) row: usize,
    /// How far into the row the top is, from 0 to 1.
    pub(crate) fraction: f32,
}

impl Anchor {
    /// The anchor of a list scrolled `offset` rows down.
    #[must_use]
    pub(crate) fn at(offset: f64) -> Self {
        let rows = scroll::narrow(offset).max(0.0);
        let row = scroll::whole(rows);
        Self {
            row,
            fraction: rows - scroll::float(row),
        }
    }

    /// The offset in rows that puts the anchor's row, `row` rows down, back at the top.
    #[must_use]
    pub(crate) fn offset(self, row: usize) -> f64 {
        f64::from(scroll::float(row) + self.fraction)
    }
}

/// A diff row by what it shows rather than where it is, to find it again in rows laid out
/// with other folds: its unit, and the head or base line of its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowKey {
    unit: review_core::UnitId,
    head: Option<usize>,
    base: Option<usize>,
    /// Whether the line belongs to a nested diff, whose lines may be another file's.
    nested: bool,
}

impl RowKey {
    /// The key of `row`.
    #[must_use]
    pub(crate) fn of(row: &Row) -> Self {
        Self {
            unit: row.unit,
            head: row
                .cells()
                .find_map(|cell| cell.head.as_ref().map(|head| head.line)),
            base: row.cells().find_map(|cell| cell.base),
            nested: row.cells().any(|cell| {
                matches!(
                    cell.kind,
                    RowKind::NestedOld | RowKind::NestedNew | RowKind::NestedSame
                )
            }),
        }
    }

    /// Whether `row` shows the same line of code.
    fn same_line(self, row: &Row) -> bool {
        let other = Self::of(row);
        other.nested == self.nested
            && ((self.head.is_some() && other.head == self.head)
                || (self.base.is_some() && other.base == self.base))
    }

    /// The unified row of `layout` that shows the same line, of the same unit when it can;
    /// or else the header of the same unit.
    #[must_use]
    pub(crate) fn find(self, layout: &Layout) -> Option<usize> {
        let rows = &layout.rows;
        let row = rows
            .iter()
            .find(|row| row.unit == self.unit && self.same_line(row))
            .or_else(|| rows.iter().find(|row| self.same_line(row)))
            .or_else(|| rows.get(*layout.unit_rows.get(&self.unit)?))?;
        Some(row.source)
    }
}

/// Where every list is, by row: the diff by its unified row, the tree and the buffer by their
/// own rows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Places {
    /// The diff, by its unified row.
    pub(crate) diff: Anchor,
    /// What the diff's top row shows, for rows laid out with other folds.
    pub(crate) diff_key: Option<RowKey>,
    /// Whether approved hunks folded in the rows the diff's place was taken on.
    pub(crate) approved: Approved,
    /// The file tree.
    pub(crate) tree: Anchor,
    /// The file buffer.
    pub(crate) buffer: Anchor,
}

/// The layout that shows for `requested` when each half of a side-by-side diff has room for
/// `half_cells` cells of code: side by side only while that is [`MIN_HALF_CELLS`] or more.
#[must_use]
pub(crate) fn effective_layout(requested: DiffLayout, half_cells: f32) -> DiffLayout {
    match requested {
        DiffLayout::Unified => DiffLayout::Unified,
        DiffLayout::SideBySide => {
            if half_cells >= MIN_HALF_CELLS {
                DiffLayout::SideBySide
            } else {
                DiffLayout::Unified
            }
        }
    }
}

/// The presentation the window opens with in `geometry`, drawn in the sizes `scale` gives.
#[must_use]
pub(crate) fn opening(
    settings: &super::Settings,
    scale: &Scale,
    geometry: &Geometry,
) -> Presentation {
    let look = settings.appearance.shown();
    let grid = geometry.grid(
        scale,
        &Metrics {
            code_size: settings.code_size,
            density: look.density,
            panel: None,
        },
    );
    Presentation {
        layout: effective_layout(look.layout, grid.half_cells),
        density: look.density,
    }
}

/// The diff's top as a unified row of `layout`, from an offset in rows.
#[must_use]
pub(crate) fn diff_anchor(layout: &Layout, offset: f64) -> Anchor {
    let top = Anchor::at(offset);
    match layout.rows.get(top.row) {
        Some(row) => Anchor {
            row: row.source,
            ..top
        },
        None => Anchor::default(),
    }
}

/// The offset in rows that shows `anchor`, a unified row, at the top of `layout`.
#[must_use]
pub(crate) fn diff_offset(layout: &Layout, anchor: Anchor) -> f64 {
    let row = layout.projected.get(anchor.row).copied().unwrap_or(0);
    anchor.offset(row)
}

impl Model {
    /// The presentation the diff should have in `grid`: the shown look's density, and its
    /// layout unless the diff is too narrow for side by side.
    #[must_use]
    pub(crate) fn presentation_in(&self, grid: &Grid) -> Presentation {
        let look = self.look();
        Presentation {
            layout: effective_layout(look.layout, grid.half_cells),
            density: look.density,
        }
    }

    /// Where every list is now, by row, or where a scroll animation takes it.
    pub(super) fn places(&self) -> Places {
        let offset = self.diff_scroll.target();
        Places {
            diff: diff_anchor(&self.rows, offset),
            diff_key: self.rows.rows.get(Anchor::at(offset).row).map(RowKey::of),
            approved: self.folds.approved,
            tree: Anchor::at(self.tree_scroll.offset()),
            buffer: Anchor::at(self.buffer_scroll.target()),
        }
    }

    /// Changes the presentation with `change` and keeps every list at its row; see the module
    /// documentation.
    ///
    /// Changes of colors alone pass through without touching the rows.
    pub(super) fn transition(&mut self, change: impl FnOnce(&mut Self)) {
        // A file previewed by its header alone is laid out first, so its place is a row of its
        // own diff.
        self.finish_preview();
        let places = self.places();
        self.transition_from(places, change);
    }

    /// Changes the presentation with `change` and puts every list at its row in `places`,
    /// rather than where it is now: the settings list's previews all start from where the
    /// lists were when it opened, so a preview clamped at the end of a list never moves the
    /// place for good.
    pub(super) fn transition_from(&mut self, places: Places, change: impl FnOnce(&mut Self)) {
        let Places {
            diff,
            diff_key,
            approved,
            tree,
            buffer,
        } = places;
        let approved_before = self.folds.approved;

        // Motion aimed at offsets in the old rows stops.
        self.diff_scroll.stop();
        self.buffer_scroll.stop();
        self.dragging = None;

        change(self);
        // The bars around the lists follow the code size, so the next frame lays them out in
        // other viewports than the last; the grid works those out, for the new code size and
        // density.
        let grid = self.grid();
        let presentation = self.presentation_in(&grid);
        if presentation != self.presented || self.folds.approved != approved_before {
            self.presented = presentation;
            // The diff's rows are new, so row and column of a selection in it mean other text.
            self.selection = self
                .selection
                .filter(|selection| selection.pane != Pane::Diff);
            self.rebuild();
        }

        // Every list back at its row, within the new extent.
        let diff = match diff_key {
            Some(key) if self.folds.approved != approved => key
                .find(&self.rows)
                .map_or(diff, |row| Anchor { row, ..diff }),
            _ => diff,
        };
        let Viewports {
            diff: diff_viewport,
            buffer: buffer_viewport,
            tree: tree_viewport,
            ..
        } = grid.viewports;
        let rows = self.rows.rows.len();
        self.diff_scroll.jump(frame::clamp(
            diff_offset(&self.rows, diff),
            rows,
            diff_viewport,
        ));
        let rows = self.visible_tree().len();
        self.tree_scroll
            .jump(frame::clamp(tree.offset(tree.row), rows, tree_viewport));
        let rows = self.list_rows(frame::List::Buffer);
        self.buffer_scroll.jump(frame::clamp(
            buffer.offset(buffer.row),
            rows,
            buffer_viewport,
        ));
        self.text.clamp_to_extent();
        self.unified_text.clamp_to_extent();
    }

    /// `Ctrl+=`, `Ctrl+-` and `Ctrl+0`: the code size, kept to a sane range.
    pub(super) fn set_code_size(&mut self, size: f32) {
        self.transition(|this| {
            this.code_size = size.clamp(ZOOM_RANGE.start, ZOOM_RANGE.end);
        });
        self.set_status(format!(
            "Code size {:.0} pt.",
            look::points_of(self.code_size)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Side by side shows only while each half has room for its fewest cells; unified always
    /// shows as asked.
    #[test]
    fn side_by_side_falls_back_when_narrow() {
        assert_eq!(
            effective_layout(DiffLayout::SideBySide, MIN_HALF_CELLS),
            DiffLayout::SideBySide
        );
        assert_eq!(
            effective_layout(DiffLayout::SideBySide, MIN_HALF_CELLS - 0.01),
            DiffLayout::Unified
        );
        assert_eq!(
            effective_layout(DiffLayout::Unified, 200.0),
            DiffLayout::Unified
        );
    }

    /// A place found again by what its row shows, in rows laid out with approved hunks shown
    /// and then folded: a header stays its header, and a line that folds away gives way to
    /// its unit's header.
    #[test]
    fn row_keys_find_their_line_in_rows_laid_out_anew() {
        use crate::rows::{Folds, FullText, build, present};
        use std::collections::HashMap;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let review = review_core::demo::prepare(
            &review_core::demo::DemoPaths::under(dir.path()),
            &review_core::demo::fixture_dir(),
            review_core::demo::DemoRequest::Fresh,
        )
        .unwrap();
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let units = view.units_of_file("src/engine.rs");
        review
            .mark(&view, &units, review_core::MarkVerdict::Accepted)
            .unwrap();
        let history = review.history().unwrap();
        let view = history.current_view().unwrap();
        let engine = std::fs::read_to_string(
            review_core::demo::fixture_dir().join("trees/feature-1/src/engine.rs"),
        )
        .unwrap();
        let lay_out = |approved| {
            let folds = Folds {
                texts: HashMap::from([(
                    "src/engine.rs".to_owned(),
                    Arc::new(FullText::new(&engine)),
                )]),
                single_file: true,
                approved,
                ..Folds::default()
            };
            present(build(&view, &units, &folds), Presentation::default())
        };
        let shown = lay_out(Approved::Shown);
        let folded = lay_out(Approved::Folded);

        let unit = folded.hunks[1][0];
        let header = RowKey::of(&folded.rows[folded.unit_rows[&unit]]);
        assert_eq!(header.find(&shown), Some(shown.unit_rows[&unit]));

        let added = shown
            .rows
            .iter()
            .find(|row| row.unit == unit && row.kind() == RowKind::Added)
            .unwrap();
        assert_eq!(
            RowKey::of(added).find(&shown),
            Some(added.source),
            "a line finds itself"
        );
        assert_eq!(
            RowKey::of(added).find(&folded),
            Some(folded.unit_rows[&unit]),
            "a folded line gives way to its unit's header"
        );
    }

    /// An anchor keeps the row and how far into it, and puts the same row back at the top.
    #[test]
    fn anchors_keep_the_row_and_the_fraction() {
        let anchor = Anchor::at(500.5);
        assert_eq!(anchor.row, 500);
        assert!((anchor.fraction - 0.5).abs() < 1e-4);
        assert!((anchor.offset(anchor.row) - 500.5).abs() < 1e-4);
        assert_eq!(Anchor::at(-3.0), Anchor::default(), "never above the top");
    }

    /// A place kept through a change of code size comes back at the same row and fraction,
    /// within the extent worked out for the new size from the last frame: a larger font keeps
    /// it, and a smaller one, which fits more of the list, clamps it to the list's new end.
    #[test]
    fn places_survive_a_code_size_change() {
        use crate::config::Density;
        use crate::frame::{Chrome, Heights, Sizes};

        // A window whose rows are 1.6 times the code size, in whole pixels, and whose diff has
        // bars three rows tall around it.
        let scale = Scale::new(|metrics: &Metrics| {
            let row_height = (metrics.code_size * 1.6).round();
            Sizes {
                row_height,
                code_cell: metrics.code_size * 0.6,
                text_cell: metrics.code_size * 0.6,
                frame: 0.0,
                divider: 0.0,
                gutter: 0.0,
                sign: 0.0,
                marker: 0.0,
                number_room: 0.0,
                half_slack: 0.0,
                note_spacing: 0.0,
                page_context: 0.0,
                chrome: Chrome {
                    code: 3.0 * row_height,
                    ..Chrome::default()
                },
                panel_row_height: row_height,
            }
        });
        // Laid out at 17.333 pixels, rows 28 pixels tall: an 840-pixel window less 84 of bars.
        let geometry = Geometry {
            laid_out: 17.333_334,
            heights: Heights {
                diff: Some(756.0),
                ..Heights::default()
            },
            ..Geometry::default()
        };
        let viewport = |code_size| {
            let metrics = Metrics {
                code_size,
                density: Density::Normal,
                panel: None,
            };
            geometry.grid(&scale, &metrics).viewports.diff
        };
        let place = Anchor::at(60.5);
        let restore = |code_size| frame::clamp(place.offset(place.row), 100, viewport(code_size));
        assert_eq!(restore(26.666_666), 60.5, "a larger font keeps the place");
        // Rows 16 pixels tall: 792 pixels hold 49.5 of them, so the list ends at 50.5.
        assert_eq!(restore(10.0), 50.5, "a smaller font ends the list sooner");
    }

    /// An anchor taken from pixels rounds as the pixel quotient always did, so a place kept
    /// through a change of row height lands on the same pixel: 468 pixels down rows 28 tall,
    /// back in rows 28 tall, is 467.99997 pixels, not 468.
    #[test]
    fn anchors_round_as_pixel_quotients() {
        let anchor = Anchor::at(f64::from(468.0_f32) / 28.0);
        let pixels = anchor.offset(anchor.row) * 28.0;
        assert_eq!(scroll::narrow(pixels), 467.999_97);
    }
}
