//! A changed image's comparison above the diff's rows: both versions' labels, what shows in
//! place of a version without an image, and the line under them; the images themselves go in
//! over the finished frame, since their cells' colors name them.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget as _;
use ratatui_image::Image;
use review_ui::color::Rgba;
use review_ui::paint::Palette;
use review_ui::paint::images::{ComparisonPaint, VersionPaint};

use super::opaque;
use super::painter::{Painter, width};
use crate::images::{ComparisonPanes, Drawn, placement};

/// The cells between a version's name and its detail.
const GAP: u16 = 2;

/// The comparison `shown` in `panes`, but for its images.
pub fn draw(
    painter: &mut Painter<'_>,
    palette: &Palette,
    shown: &ComparisonPaint,
    panes: &ComparisonPanes,
) {
    let surface = opaque(palette.surface);
    painter.fill(panes.area, surface);
    let background = opaque(palette.background);
    for (version, room) in [(&shown.base, panes.base), (&shown.head, panes.head)] {
        // The label, over the version's room.
        let y = panes.labels.y;
        let name = painter.piece(room.x, y, room.width, &version.name, surface);
        let rest = room.width.saturating_sub(name + GAP);
        painter.piece(room.x + name + GAP, y, rest, &version.detail, surface);
        painter.fill(room, background);
        note(painter, version, room, background);
    }
    let mut x = panes.status.x;
    for piece in &shown.status {
        let room = panes.status.right().saturating_sub(x);
        x += painter.piece(x, panes.status.y, room, piece, surface);
    }
}

/// What shows in the middle of `room` in place of `version`'s image, when it has none.
fn note(painter: &mut Painter<'_>, version: &VersionPaint, room: Rect, background: Rgba) {
    let (None, Some(note)) = (&version.raster, &version.note) else {
        return;
    };
    let cells = width(&note.text).min(room.width);
    let x = room.x + (room.width - cells) / 2;
    let y = room.y + room.height / 2;
    painter.piece(x, y, cells, note, background);
}

/// The prepared images `drawn` of `shown`, each in the middle of its room in `panes`.
pub fn images(
    buffer: &mut Buffer,
    shown: &ComparisonPaint,
    panes: &ComparisonPanes,
    drawn: &Drawn<'_>,
) {
    let versions = [
        (&shown.base, panes.base, drawn.base),
        (&shown.head, panes.head, drawn.head),
    ];
    for (version, room, protocol) in versions {
        if let (Some(raster), Some(protocol)) = (&version.raster, protocol) {
            Image::new(protocol).render(placement(raster, room, drawn.cell), buffer);
        }
    }
}
