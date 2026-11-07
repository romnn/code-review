//! Measuring the window for the model: the code font, the sizes everything takes from the code
//! size, and the lists' bounds as gpui laid them out, as the model's [`Geometry`].
//!
//! The lists' bounds come from the last frame gpui laid out.
//! The model works out the rows and cells from them in the [`frame::Sizes`] that [`scale`]
//! hands it, at any code size: the bars around the lists follow the code size, so a transition
//! to a new code size keeps the reviewer's place against the viewports the next frame will
//! have.

use gpui::{Font, UniformListScrollHandle};
use gpui_component::scroll::ScrollbarHandle as _;
use review_ui::frame::{self, Chrome, Geometry, Heights, Metrics, Scale};
use review_ui::look;
use review_ui::model::{PaneLayout, Settings};

use super::ReviewApp;
use super::render::{GRID, LINE, bar_height, strip_height};

/// How far a track click's page keeps the page before in view, in logical pixels.
const PAGE_CONTEXT: f32 = 40.0;

/// The room the side-by-side code pans past its widest half, in logical pixels.
const HALF_SLACK: f32 = GRID * 4.0;

/// The room after a buffer's line numbers, in code cells.
const NUMBER_ROOM: f32 = 1.25;

/// The settings panel's row height as a multiple of its text size.
pub const PANEL_LINE_HEIGHT: f32 = 1.75;

/// The accent bar along a row's left edge, in logical pixels.
pub const FRAME: f32 = 5.0;

/// A code row's line-number gutter, in cells: four digits and the room after them.
const GUTTER_CELLS: f32 = 5.0;

/// A code row's sign column, in cells.
const SIGN_CELLS: f32 = 1.5;

/// The most pixels the spacing of one of the tool's own rows adds past the text it shows and
/// the start of code: a note's accent and padding, and the gaps and chip padding between a hunk
/// header's parts, which [`review_ui::rows::Note::width`] counts in cells of text instead.
const ANNOTATION_SPACING: f32 = GRID * 10.0;

/// The line between the halves of the side-by-side diff.
pub const DIVIDER: f32 = 1.0;

/// A code row's line-number gutter, for code cells `cell` pixels wide.
#[must_use]
pub fn gutter(cell: f32) -> f32 {
    (cell * GUTTER_CELLS).round()
}

/// The change marker along the left edge of a buffer's line, in logical pixels.
pub const BUFFER_MARKER: f32 = 3.0;

/// The line numbers' column of a buffer `lines` lines long, for code cells `cell` pixels wide:
/// the last line's number, four digits at least, and a cell and a quarter of room.
#[must_use]
pub fn buffer_numbers(cell: f32, lines: usize) -> f32 {
    frame::number_column(cell, NUMBER_ROOM, lines)
}

/// A code row's sign column, for code cells `cell` pixels wide.
#[must_use]
pub fn sign(cell: f32) -> f32 {
    (cell * SIGN_CELLS).round()
}

/// The diff pane's width in a window `window` wide, beside the sidebar when it shows, with the
/// window's text in cells `ui_cell` pixels wide.
fn pane_width(layout: PaneLayout, sidebar_shown: bool, window: f32, ui_cell: f32) -> f32 {
    if sidebar_shown {
        window - review_ui::panes::sidebar_width(layout.sidebar, window, ui_cell) - (GRID + 1.0)
    } else {
        window
    }
}

/// The diff pane's width a window `window` wide opens with, for code in a font whose cells are
/// `advance` of its size wide.
pub fn opening_width(settings: &Settings, advance: f32, window: f32) -> f32 {
    let sizes = Sizes {
        code_size: settings.code_size,
        line_height: look::line_height(settings.appearance.shown().density),
        advance,
    };
    pane_width(
        settings.layout,
        settings.layout.sidebar_visible,
        window,
        sizes.ui_cell(),
    )
}

impl ReviewApp {
    /// The diff pane's width in a window `window` wide, beside the sidebar when it shows.
    pub(super) fn diff_width_in(&self, window: f32) -> f32 {
        pane_width(
            self.layout,
            self.sidebar_shown(),
            window,
            self.sizes().ui_cell(),
        )
    }
}

/// The code font, which the whole window uses.
#[derive(Debug, Clone)]
pub struct Fonts {
    /// The code font, with ligatures off.
    pub code: Font,
    /// A cell's width as a share of the font size, measured from the code font.
    ///
    /// The window's text is in the same monospace font, so columns sized in cells hold their
    /// text at any size.
    pub advance: f32,
}

/// The sizes of the window at one code size and density, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sizes {
    /// The code size.
    pub code_size: f32,
    /// The line height as a multiple of the code size, which the density sets.
    pub line_height: f32,
    /// A cell's width as a share of the font size.
    pub advance: f32,
}

impl Sizes {
    /// The sizes at `metrics` in a font whose cells are `advance` of its size wide.
    pub fn new(metrics: &Metrics, advance: f32) -> Self {
        Self {
            code_size: metrics.code_size,
            line_height: look::line_height(metrics.density),
            advance,
        }
    }

    /// The size of the window's own text: labels, the tree and the bars.
    pub fn ui_size(&self) -> f32 {
        (self.code_size * 0.86).round()
    }

    /// The height of every row; the lists are uniform so they lay out only visible rows.
    pub fn row_height(&self) -> f32 {
        (self.code_size * self.line_height).round()
    }

    /// A cell's width in text `size` pixels tall.
    pub fn cell(&self, size: f32) -> f32 {
        size * self.advance
    }

    /// A cell of the window's own text.
    pub fn ui_cell(&self) -> f32 {
        self.cell(self.ui_size())
    }

    /// A cell of code.
    pub fn code_cell(&self) -> f32 {
        self.cell(self.code_size)
    }

    /// The height of a pane header, the find bar and the like, which hold one line of the
    /// window's text.
    pub fn bar(&self) -> f32 {
        bar_height(self.ui_size())
    }

    /// The height of each of the two bottom bars.
    pub fn strip(&self) -> f32 {
        strip_height(self.ui_size())
    }

    /// The height of the bars above and below the diff and the buffer that follow the code
    /// size: the pane header and the two bottom bars.
    fn list_chrome(&self) -> f32 {
        self.bar() + 2.0 * self.strip()
    }

    /// The same for the tree, whose pane has its two progress lines above it instead of a
    /// header, and the two bottom bars below.
    fn tree_chrome(&self) -> f32 {
        2.0 * self.strip() + 2.0 * self.ui_size() * LINE
    }

    /// The same for the picker's entries: its query line and its footer.
    fn picker_chrome(&self) -> f32 {
        self.bar() + GRID * 2.0 + self.strip()
    }

    /// The same for the settings list, which keeps its own text size but ends above the two
    /// bottom bars.
    fn settings_chrome(&self) -> f32 {
        2.0 * self.strip()
    }
}

/// The text size and row height of the settings panel, which keeps those it opened with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelSizes {
    /// The panel's text size in pixels.
    pub text_size: f32,
    /// The height of the panel's rows in pixels.
    pub row_height: f32,
}

impl PanelSizes {
    /// The panel's sizes when it takes its text size from code `code_size` pixels tall.
    pub fn new(code_size: f32) -> Self {
        let text_size = (code_size * 0.86).round();
        Self {
            text_size,
            row_height: (text_size * PANEL_LINE_HEIGHT).round(),
        }
    }
}

/// The gpui handles of the lists the model scrolls, one per [`review_ui::frame::List`].
#[derive(Clone)]
pub struct Handles {
    pub diff: UniformListScrollHandle,
    pub buffer: UniformListScrollHandle,
    pub tree: UniformListScrollHandle,
    pub picker: UniformListScrollHandle,
    pub settings: UniformListScrollHandle,
}

/// The height of a list's last layout in pixels, `None` before its first.
fn laid_out_height(list: &UniformListScrollHandle) -> Option<f32> {
    let height = f32::from(list.viewport_bounds().size.height);
    (height > 0.0).then_some(height)
}

/// How the window's sizes follow the model's metrics, in a code font whose cells are `advance`
/// of its size wide.
pub fn scale(advance: f32) -> Scale {
    Scale::new(move |metrics| {
        let sizes = Sizes::new(metrics, advance);
        let code_cell = sizes.code_cell();
        frame::Sizes {
            row_height: sizes.row_height(),
            code_cell,
            text_cell: sizes.ui_cell(),
            frame: FRAME,
            divider: DIVIDER,
            gutter: gutter(code_cell),
            sign: sign(code_cell),
            marker: BUFFER_MARKER,
            number_room: NUMBER_ROOM,
            half_slack: HALF_SLACK,
            note_spacing: ANNOTATION_SPACING,
            page_context: PAGE_CONTEXT,
            chrome: Chrome {
                code: sizes.list_chrome(),
                tree: sizes.tree_chrome(),
                picker: sizes.picker_chrome(),
                settings: sizes.settings_chrome(),
            },
            panel_row_height: PanelSizes::new(metrics.panel.unwrap_or(metrics.code_size))
                .row_height,
        }
    })
}

/// The lists `handles` as gpui last laid them out, at code `laid_out` pixels tall, with the
/// diff pane `diff_width` pixels wide.
pub fn measure(handles: &Handles, laid_out: f32, diff_width: f32) -> Geometry {
    let diff_list = f32::from(handles.diff.viewport_bounds().size.width);
    Geometry {
        laid_out,
        diff_width,
        diff_list,
        diff_half: (diff_list - FRAME - DIVIDER) / 2.0,
        buffer_list: f32::from(handles.buffer.viewport_bounds().size.width),
        heights: Heights {
            diff: laid_out_height(&handles.diff),
            buffer: laid_out_height(&handles.buffer),
            tree: laid_out_height(&handles.tree),
            picker: laid_out_height(&handles.picker),
            settings: laid_out_height(&handles.settings),
        },
    }
}

#[cfg(test)]
mod tests {
    use review_ui::config::Density;

    use super::*;

    /// The metrics of code `code_size` pixels tall at the normal density, rows 1.6 times as
    /// tall, without the settings list.
    fn at(code_size: f32) -> Metrics {
        Metrics {
            code_size,
            density: Density::Normal,
            panel: None,
        }
    }

    /// Side by side needs room for both halves after their gutters; a larger font needs more.
    #[test]
    fn side_by_side_needs_room_for_both_halves() {
        // A 13 pt font whose advance is 0.6 of its size.
        let scale = scale(0.6);
        let size = review_ui::config::points_to_pixels(13.0);
        let fits = |width: f32, code_size: f32| {
            let geometry = Geometry {
                laid_out: code_size,
                diff_width: width,
                ..Geometry::default()
            };
            geometry.grid(&scale, &at(code_size)).half_cells
                >= review_ui::model::present::MIN_HALF_CELLS
        };
        assert!(fits(1300.0, size));
        assert!(!fits(700.0, size));
        assert!(
            !fits(1300.0, size * 2.0),
            "a font twice as large no longer fits"
        );
    }

    /// Laid out at one code size and worked out for a larger one, a viewport shrinks by as
    /// much as the window's bars around it grow, in rows of the new height; at the same size it
    /// stays as laid out, and a list not laid out yet has no viewport at either.
    #[test]
    fn a_new_code_size_predicts_the_viewports() {
        let scale = scale(0.6);
        let (before, after) = (
            Sizes::new(&at(17.333_334), 0.6),
            Sizes::new(&at(26.666_666), 0.6),
        );
        let geometry = Geometry {
            laid_out: before.code_size,
            heights: Heights {
                diff: Some(876.0),
                ..Heights::default()
            },
            ..Geometry::default()
        };
        let grown = before.list_chrome() - after.list_chrome();
        assert!(grown < 0.0, "larger text, taller bars");
        let predicted = geometry.grid(&scale, &at(after.code_size)).viewports;
        assert_eq!(
            predicted.diff,
            Some(f64::from(876.0 + grown) / f64::from(after.row_height()))
        );
        assert_eq!(predicted.buffer, None);
        let same = geometry.grid(&scale, &at(before.code_size)).viewports;
        assert_eq!(same.diff, Some(876.0 / 28.0));
    }

    /// The settings panel's sizes follow the code size it opened with, as its text does.
    #[test]
    fn the_panel_keeps_its_own_sizes() {
        let sizes = PanelSizes::new(17.333_334);
        assert_eq!(sizes.text_size, 15.0);
        assert_eq!(sizes.row_height, 26.0);
    }
}
