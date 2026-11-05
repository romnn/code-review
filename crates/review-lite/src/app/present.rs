//! Changing how the diff is presented while keeping the reviewer's place.
//!
//! The layout (unified or side by side), the density, the font size and the narrow-window
//! fallback all change which rows show or how tall they are.
//! Each change, and each preview of a look and its rollback, goes through
//! [`ReviewApp::transition`], which:
//!
//! - notes where every list is by row identity before anything changes: the diff by the unified
//!   row at its top, the tree and the buffer by their top row;
//! - stops scroll animations and track-click pages, which aim at pixels of the old rows;
//! - drops a drag, and a diff selection whose rows were projected anew;
//! - projects the rows again when the presentation changed, then puts every list back at its
//!   row, clamped to the new extent.
//!
//! Places kept for files not shown are row identities too, so returning to a file after a
//! density change lands on the same line.

use gpui::{Context, Window};
use gpui_component::scroll::ScrollbarHandle as _;

use super::render::GRID;
use super::text::Pane;
use super::{ReviewApp, panes, scroll};
use crate::config::DiffLayout;
use crate::look;
use crate::rows::{Layout, Presentation};

/// The accent bar along a row's left edge, in logical pixels.
pub const FRAME: f32 = 3.0;

/// A code row's line-number gutter.
pub const GUTTER: f32 = GRID * 13.0;

/// A code row's sign column.
pub const SIGN: f32 = GRID * 4.0;

/// The line between the halves of the side-by-side diff.
pub const DIVIDER: f32 = 1.0;

/// The fewest cells of code each half must show for the side-by-side layout; narrower, the
/// diff shows unified until the window is wide enough again.
pub const MIN_HALF_CELLS: f32 = 40.0;

/// A monospace cell's width as a share of the font size, for deciding what fits.
const CELL: f32 = 0.6;

/// Where a list is, by row: the row at its top, and how far into that row.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Anchor {
    /// For the diff, the unified row ([`crate::rows::Row::source`]), which no presentation
    /// changes; for the tree and the buffer, the row itself.
    pub row: usize,
    /// How far into the row the top is, from 0 to 1.
    pub fraction: f32,
}

impl Anchor {
    /// The anchor of a list scrolled `offset` pixels down rows `height` pixels tall.
    #[must_use]
    pub fn at(offset: f32, height: f32) -> Self {
        let rows = (offset / height.max(1.0)).max(0.0);
        let row = scroll::whole(rows);
        Self {
            row,
            fraction: rows - scroll::float(row),
        }
    }

    /// The offset that puts the anchor's row, `row` rows down, back at the top.
    #[must_use]
    pub fn offset(self, row: usize, height: f32) -> f32 {
        (scroll::float(row) + self.fraction) * height
    }
}

/// Where every list is, by row: the diff by its unified row, the tree and the buffer by their
/// own rows.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Places {
    /// The diff, by its unified row.
    pub diff: Anchor,
    /// The file tree.
    pub tree: Anchor,
    /// The file buffer.
    pub buffer: Anchor,
}

/// The layout that shows for `requested` in a diff `width` pixels wide with code `code_size`
/// pixels tall: side by side only while each half fits [`MIN_HALF_CELLS`] after its gutter.
#[must_use]
pub fn effective_layout(requested: DiffLayout, width: f32, code_size: f32) -> DiffLayout {
    match requested {
        DiffLayout::Unified => DiffLayout::Unified,
        DiffLayout::SideBySide => {
            let half = (width - FRAME - DIVIDER) / 2.0 - GUTTER - SIGN;
            if half / (code_size * CELL) >= MIN_HALF_CELLS {
                DiffLayout::SideBySide
            } else {
                DiffLayout::Unified
            }
        }
    }
}

/// The presentation and the diff's width a window `window` wide opens with, setting the line
/// height `fonts` take from the look's density.
pub fn opening(
    settings: &super::Settings,
    fonts: &mut super::Fonts,
    window: f32,
) -> (Presentation, f32) {
    let look = settings.appearance.shown();
    fonts.line_height = look::line_height(look.density);
    let diff_width = if settings.layout.sidebar_visible {
        window - panes::sidebar_width(settings.layout.sidebar, window) - (GRID + 1.0)
    } else {
        window
    };
    let presentation = Presentation {
        layout: effective_layout(look.layout, diff_width, fonts.code_size),
        density: look.density,
    };
    (presentation, diff_width)
}

/// The diff's top as a unified row of `layout`, from an offset into rows `height` tall.
#[must_use]
pub fn diff_anchor(layout: &Layout, offset: f32, height: f32) -> Anchor {
    let top = Anchor::at(offset, height);
    match layout.rows.get(top.row) {
        Some(row) => Anchor {
            row: row.source,
            ..top
        },
        None => Anchor::default(),
    }
}

/// The offset that shows `anchor`, a unified row, at the top of `layout` with rows `height`
/// tall.
#[must_use]
pub fn diff_offset(layout: &Layout, anchor: Anchor, height: f32) -> f32 {
    let row = layout.projected.get(anchor.row).copied().unwrap_or(0);
    anchor.offset(row, height)
}

impl ReviewApp {
    /// The presentation the diff should have now: the shown look's density, and its layout
    /// unless the diff is too narrow for side by side.
    pub(super) fn presentation(&self) -> Presentation {
        let look = self.look();
        Presentation {
            layout: effective_layout(look.layout, self.diff_width, self.fonts.code_size),
            density: look.density,
        }
    }

    /// The diff pane's width in a window `window` wide, beside the sidebar when it shows.
    pub(super) fn diff_width_in(&self, window: f32) -> f32 {
        if self.sidebar_shown() {
            window - panes::sidebar_width(self.layout.sidebar, window) - (GRID + 1.0)
        } else {
            window
        }
    }

    /// Follows the window's width each frame: a resize, the sidebar or the screen mode can
    /// make the diff too narrow for side by side, or wide enough again.
    pub(super) fn follow_width(&mut self, window: &Window) {
        let width = self.diff_width_in(f32::from(window.viewport_size().width));
        if (width - self.diff_width).abs() < 0.5 {
            return;
        }
        self.diff_width = width;
        if self.presentation() != self.presented {
            self.transition(|_| {});
        }
    }

    /// Where every list is now, by row, or where a scroll animation takes it.
    pub(super) fn places(&self) -> Places {
        let height = self.fonts.row_height();
        Places {
            diff: diff_anchor(&self.rows, self.list_target(), height),
            tree: Anchor::at(scroll::offset_of(&self.tree_list), height),
            buffer: Anchor::at(self.buffer_target(), height),
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
        let Places { diff, tree, buffer } = places;

        // Motion aimed at the old rows' pixels stops.
        self.list_animation = None;
        self.buffer_animation = None;
        self.list_bar.cancel_page_request();
        self.buffer_bar.cancel_page_request();
        self.dragging = None;

        change(self);
        self.fonts.line_height = look::line_height(self.look().density);
        let presentation = self.presentation();
        if presentation != self.presented {
            self.presented = presentation;
            // The diff's rows are new, so row and column of a selection in it mean other text.
            self.selection = self
                .selection
                .filter(|selection| selection.pane != Pane::Diff);
            self.rebuild();
        }

        // Every list back at its row, within the new extent.
        let height = self.fonts.row_height();
        let clamp = |offset: f32, rows: usize, viewport: f32| {
            offset.clamp(0.0, (scroll::float(rows) * height - viewport).max(0.0))
        };
        let viewport = f32::from(self.list.viewport_bounds().size.height);
        let offset = diff_offset(&self.rows, diff, height);
        scroll::set_offset_of(&self.list, clamp(offset, self.rows.rows.len(), viewport));
        let viewport = f32::from(self.tree_list.viewport_bounds().size.height);
        let offset = tree.offset(tree.row, height);
        scroll::set_offset_of(
            &self.tree_list,
            clamp(offset, self.visible_tree().len(), viewport),
        );
        let lines = self
            .buffer
            .as_ref()
            .map_or(0, |buffer| buffer.lines.lines.len());
        let viewport = f32::from(self.buffer_list.viewport_bounds().size.height);
        let offset = buffer.offset(buffer.row, height);
        scroll::set_offset_of(&self.buffer_list, clamp(offset, lines, viewport));
        self.text_scroll.clamp_to_extent();
    }

    /// Where the buffer is, or is heading.
    fn buffer_target(&self) -> f32 {
        self.buffer_animation.map_or_else(
            || scroll::offset_of(&self.buffer_list),
            |animation| animation.target(),
        )
    }

    /// `Ctrl+=`, `Ctrl+-` and `Ctrl+0`: the code size, kept to a sane range.
    pub(super) fn set_code_size(&mut self, size: f32, cx: &mut Context<Self>) {
        self.transition(|this| {
            this.fonts.code_size = size.clamp(super::ZOOM_RANGE.start, super::ZOOM_RANGE.end);
        });
        self.set_status(format!(
            "Code size {:.0} pt.",
            look::points_of(self.fonts.code_size)
        ));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Side by side needs room for both halves after their gutters; a larger font needs more.
    #[test]
    fn side_by_side_falls_back_when_narrow() {
        let size = crate::config::points_to_pixels(13.0);
        assert_eq!(
            effective_layout(DiffLayout::SideBySide, 1300.0, size),
            DiffLayout::SideBySide
        );
        assert_eq!(
            effective_layout(DiffLayout::SideBySide, 700.0, size),
            DiffLayout::Unified
        );
        assert_eq!(
            effective_layout(DiffLayout::SideBySide, 1300.0, size * 2.0),
            DiffLayout::Unified,
            "a font twice as large no longer fits"
        );
        assert_eq!(
            effective_layout(DiffLayout::Unified, 3000.0, size),
            DiffLayout::Unified
        );
    }

    /// An anchor keeps the row and how far into it, whatever the row height.
    #[test]
    fn anchors_survive_a_row_height_change() {
        let anchor = Anchor::at(13_000.0 + 13.0, 26.0);
        assert_eq!(anchor.row, 500);
        assert!((anchor.fraction - 0.5).abs() < 1e-4);
        assert!((anchor.offset(anchor.row, 34.0) - 500.5 * 34.0).abs() < 0.01);
    }
}
