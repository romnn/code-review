//! The file buffer's pane, as review-gui draws it: a header naming the file, the scopes
//! around the cursor and where the text comes from, then the whole file with each line's
//! change marker and number, the cursor's line, find matches and the selection.
//!
//! The code pans by the buffer's own offset, which follows the cursor, while markers and
//! numbers stay put; a line running past the right edge ends in the clip marker.

use ratatui::layout::Rect;
use review_ui::frame::List;
use review_ui::model::Model;
use review_ui::model::buffer::Marker;
use review_ui::paint::code::BufferLines;
use review_ui::paint::display::DisplayLine;
use review_ui::paint::header;
use review_ui::paint::{Ink, Palette};

use super::diff::clip_mark;
use super::painter::{Brush, Painter, ellipsize_end, width};
use super::{first_row, opaque};
use crate::color::over;
use crate::measure::{MARKER, Panes, buffer_numbers};

/// The cells between the parts of the header.
const HEADER_GAP: u16 = 2;

/// The buffer's pane: its header over its lines.
pub fn draw(painter: &mut Painter<'_>, model: &Model, palette: &Palette, panes: &Panes) {
    header_row(painter, model, palette, panes.header);
    let list = panes.list;
    let background = opaque(palette.background);
    painter.fill(list, background);
    let Some(lines) = BufferLines::of(model, *palette) else {
        return;
    };
    let text = lines.text();
    let numbers = buffer_numbers(text.lines.len());
    let code_at = list.x + MARKER + numbers;
    let room = list.right().saturating_sub(code_at);
    let first = first_row(model, List::Buffer);
    let skip = review_ui::scroll::whole_rows(model.buffer_text.offset());
    for (offset, y) in (list.top()..list.bottom()).enumerate() {
        let index = first + offset;
        let (Some(content), Some(line)) = (text.lines.get(index), lines.line(index)) else {
            break;
        };
        let bg = line
            .background
            .map_or(background, |color| over(background, color));
        painter.fill(Rect::new(list.x, y, list.width, 1), bg);
        // An added line's bar, or a rule over the line where lines were removed.
        match line.marker {
            Some(Marker::Added(_)) => {
                painter.symbol(list.x, y, "▎", Brush::new(palette.added_sign, bg));
            }
            Some(Marker::Removed(_)) => {
                painter.symbol(list.x, y, "▔", Brush::new(palette.removed_sign, bg));
            }
            None => {}
        }
        // The number ends a cell before the code.
        let digits = width(&line.number.text);
        let number_end = code_at.saturating_sub(1);
        painter.piece(
            number_end.saturating_sub(digits),
            y,
            digits,
            &line.number,
            bg,
        );
        let ink = Ink {
            foreground: palette.text,
            background: bg,
        };
        painter.layered(code_at, y, room, skip, content, &line.layers, ink);
        let cells = DisplayLine::new(content).width();
        clip_mark(painter, palette, cells, skip, code_at, room, y, bg);
    }
}

/// The header: the file and the scopes around the cursor, where the text comes from, and the
/// cursor's line and column at the right edge; the scopes and the origin make way first.
fn header_row(painter: &mut Painter<'_>, model: &Model, palette: &Palette, area: Rect) {
    if area.height == 0 {
        return;
    }
    let surface = opaque(palette.surface);
    painter.fill(area, surface);
    let Some(header) = header::buffer_header(model, palette) else {
        return;
    };
    let y = area.y;
    let right = area.right().saturating_sub(1);
    let cursor_width = width(&header.cursor.text);
    let cursor_at = right.saturating_sub(cursor_width);
    painter.piece(cursor_at, y, cursor_width, &header.cursor, surface);
    let end = cursor_at.saturating_sub(HEADER_GAP);
    let mut x = area.x + 1;
    let pieces = [
        (Some(&header.title), 0),
        (header.scope.as_ref(), 0),
        (Some(&header.origin), HEADER_GAP),
    ];
    for (piece, gap) in pieces {
        let Some(piece) = piece else {
            continue;
        };
        x += gap;
        let room = end.saturating_sub(x);
        if room == 0 {
            return;
        }
        let shown = ellipsize_end(&piece.text, room);
        x += painter.text(x, y, room, &shown, Brush::piece(piece, surface));
    }
}
