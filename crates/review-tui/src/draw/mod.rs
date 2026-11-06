//! Drawing one frame into ratatui's buffer from review-ui's [`paint`] pieces, which decide
//! what shows and in which colors; here is only where each piece goes on the cell grid: the
//! files, the diff or the file buffer, the bars, and the overlays over them.
//!
//! Rows are written straight into the buffer, only those on screen, with no ratatui widget in
//! between: every cell gets the opaque colors review-gui would show, composited as gpui
//! blends them.
//!
//! [`paint`]: review_ui::paint

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use review_ui::color::{Hsla, Rgba, rgb};
use review_ui::frame::List;
use review_ui::line_edit::LineEdit;
use review_ui::model::{Mode, Model, Overlay, SheetStep};
use review_ui::paint::Palette;

use crate::color::{Colors, over};
use crate::measure::Panes;

mod bars;
mod buffer;
mod diff;
mod files;
mod overview;
pub mod painter;
mod picker;
mod settings;
mod sheet;

pub use painter::{Brush, Painter};

/// `color` as the opaque color it shows over black, for the colors that are the bottom layer.
#[must_use]
pub fn opaque(color: Hsla) -> Rgba {
    over(rgb(0), color)
}

/// The first whole row `list` shows: rows are whole cells, so a list scrolled part of a row
/// shows from the row its top is in.
#[must_use]
pub fn first_row(model: &Model, list: List) -> usize {
    review_ui::scroll::whole_rows(
        model
            .list_scroll(list)
            .map_or(0.0, review_ui::frame::ListScroll::offset),
    )
}

/// `percent` of `cells`, rounded down.
#[must_use]
pub fn share(cells: u16, percent: u16) -> u16 {
    u16::try_from(u32::from(cells) * u32::from(percent) / 100).unwrap_or(cells)
}

/// Where the cheat sheet or the overview is scrolled, in rows, which the terminal keeps as
/// review-gui's window keeps its own scroll position of them; drawing them learns how far
/// they can scroll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SheetScroll {
    /// The rows scrolled past.
    pub offset: u16,
    /// The most rows it can scroll, as last drawn.
    pub max: u16,
    /// The rows that show, as last drawn.
    pub viewport: u16,
}

impl SheetScroll {
    /// Scrolls by `rows`, down when positive, within what it can.
    pub fn scroll_by(&mut self, rows: i32) {
        let offset = i32::from(self.offset).saturating_add(rows);
        self.offset = u16::try_from(offset.clamp(0, i32::from(self.max))).unwrap_or(0);
    }

    /// Scrolls as the model's sheet keys ask.
    pub fn step(&mut self, step: SheetStep) {
        let rows = match step {
            SheetStep::Lines(lines) => lines,
            SheetStep::HalfPages(pages) => {
                pages.saturating_mul(i32::from((self.viewport / 2).max(1)))
            }
        };
        self.scroll_by(rows);
    }
}

/// What a frame draws besides the model: the panes it is laid out in, the text field's editor
/// and where a sheet is scrolled.
pub struct Scene<'a> {
    /// The panes.
    pub panes: &'a Panes,
    /// The editor of the field taking typing.
    pub editor: &'a LineEdit,
    /// Where the cheat sheet or the overview is scrolled.
    pub sheet: &'a mut SheetScroll,
}

/// Draws `model` as `scene` lays it out into `buffer` at `now`, in `colors`.
pub fn frame(buffer: &mut Buffer, colors: Colors, model: &Model, scene: Scene<'_>, now: Instant) {
    let Scene {
        panes,
        editor,
        sheet,
    } = scene;
    let palette = Palette::of(model);
    let mut painter = Painter::new(buffer, colors);
    let background = opaque(palette.background);
    painter.fill(panes.area, background);
    if let Some(sidebar) = &panes.sidebar {
        files::draw(&mut painter, model, &palette, sidebar);
    }
    if let Some(divider) = panes.divider {
        divider_line(&mut painter, &palette, divider);
    }
    if model.mode == Mode::Buffer && model.buffer.is_some() {
        buffer::draw(&mut painter, model, &palette, panes);
    } else {
        diff::draw(&mut painter, model, &palette, panes, now);
    }
    if let Some(find) = panes.find {
        bars::find_bar(&mut painter, model, &palette, find, editor);
    }
    bars::status(&mut painter, model, &palette, panes.status, now);
    bars::hints(&mut painter, model, &palette, panes.hints);
    match model.overlay {
        Some(Overlay::Help) => sheet::draw(&mut painter, model, &palette, panes.area, sheet),
        Some(Overlay::Overview) => {
            overview::draw(&mut painter, model, &palette, panes.area, sheet);
        }
        Some(Overlay::Picker) => picker::draw(&mut painter, model, &palette, panes, editor),
        Some(Overlay::Settings) => settings::draw(&mut painter, model, &palette, panes, editor),
        None => {}
    }
    painter.finish();
}

/// The hairline between the files and the diff.
fn divider_line(painter: &mut Painter<'_>, palette: &Palette, area: Rect) {
    let surface = opaque(palette.surface);
    for y in area.top()..area.bottom() {
        painter.symbol(area.x, y, "│", Brush::new(palette.hairline, surface));
    }
}
