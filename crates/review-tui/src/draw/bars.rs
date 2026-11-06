//! The bars along the bottom: the `/` bar while it takes typing, the status line, and what the
//! keys act on with the current mode's keys.

use std::time::Instant;

use ratatui::layout::Rect;
use review_ui::color::Rgba;
use review_ui::line_edit::LineEdit;
use review_ui::model::{Field, Model};
use review_ui::paint::Palette;
use review_ui::paint::bars::{self, BarHint, HintRoom};
use review_ui::paint::display::DisplayLine;

use super::opaque;
use super::painter::{Brush, Painter, ellipsize_end, width};
use crate::color::over;

/// The cells the target label keeps before hints make way, as review-gui keeps 28 cells.
const TARGET_CELLS: u16 = 28;

/// The cells between two hints.
const HINT_GAP: u16 = 2;

/// The status line: the last thing that happened, in the error color and on a fading tint
/// after a failure, and a newer revision at the right on the accent.
pub fn status(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    area: Rect,
    now: Instant,
) {
    if area.height == 0 {
        return;
    }
    let status = bars::status(model, palette, now);
    let surface = opaque(palette.surface);
    let bg = over(surface, status.background);
    painter.fill(area, bg);
    let mut right = area.right().saturating_sub(1);
    if let Some(newer) = &status.newer {
        let text = format!(" {} ", newer.text.text);
        let cells = width(&text).min(area.width.saturating_sub(2));
        right = right.saturating_sub(cells);
        let notice_bg = over(bg, newer.background);
        painter.text(
            right,
            area.y,
            cells,
            &text,
            Brush::piece(&newer.text, notice_bg),
        );
        right = right.saturating_sub(HINT_GAP);
    }
    let room = right.saturating_sub(area.x + 1);
    let text = ellipsize_end(&status.text.text, room);
    painter.text(
        area.x + 1,
        area.y,
        room,
        &text,
        Brush::piece(&status.text, bg),
    );
}

/// What the keys act on at the left, then a divider and the hints that fit at the right, in
/// rank order with `? keys` last at the edge.
pub fn hints(painter: &mut Painter<'_>, model: &Model, palette: &Palette, area: Rect) {
    if area.height == 0 {
        return;
    }
    let surface = opaque(palette.surface);
    painter.fill(area, surface);
    let budget = area.width.saturating_sub(2 + TARGET_CELLS + 3);
    let hints = bars::bar_hints(
        model,
        palette,
        HintRoom {
            budget: f32::from(budget),
            inner_gap: 1.0,
            gap: f32::from(HINT_GAP),
        },
        |text| f32::from(width(text)),
    );
    let hint_width = |hint: &BarHint| width(&hint.keys.text) + 1 + width(&hint.label.text);
    let count = u16::try_from(hints.len()).unwrap_or(u16::MAX);
    let total = hints.iter().map(hint_width).sum::<u16>()
        + HINT_GAP.saturating_mul(count.saturating_sub(1));
    let right = area.right().saturating_sub(1);
    let mut at = right.saturating_sub(total);
    for hint in &hints {
        at += painter.piece(at, area.y, right.saturating_sub(at), &hint.keys, surface) + 1;
        at += painter.piece(at, area.y, right.saturating_sub(at), &hint.label, surface) + HINT_GAP;
    }
    let divider = right.saturating_sub(total + 2);
    let label_end = if hints.is_empty() {
        right
    } else {
        painter.symbol(divider, area.y, "│", Brush::new(palette.hairline, surface));
        divider.saturating_sub(1)
    };
    let room = label_end.saturating_sub(area.x + 1);
    let label = ellipsize_end(&bars::target_label(model), room);
    painter.text(
        area.x + 1,
        area.y,
        room,
        &label,
        Brush::new(palette.text, surface),
    );
}

/// The `/` bar: the slash, the field's text with its cursor or its placeholder, and how many
/// files or matches there are.
pub fn find_bar(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    area: Rect,
    editor: &LineEdit,
) {
    let Some(bar) = bars::find_bar(model, palette) else {
        return;
    };
    let surface = opaque(palette.surface);
    painter.fill(area, surface);
    let right = area.right().saturating_sub(1);
    let count_width = width(&bar.count.text);
    let count_at = right.saturating_sub(count_width);
    painter.piece(count_at, area.y, count_width, &bar.count, surface);
    let x = area.x + 1;
    painter.piece(x, area.y, 1, &bar.slash, surface);
    let field_at = x + 2;
    let room = count_at.saturating_sub(field_at + 2);
    field(
        painter,
        model,
        palette,
        FieldAt {
            field: bar.field,
            x: field_at,
            y: area.y,
            room,
            bg: surface,
        },
        editor,
    );
}

/// Where a text field is drawn.
#[derive(Debug, Clone, Copy)]
pub struct FieldAt {
    /// The field.
    pub field: Field,
    /// Its first cell.
    pub x: u16,
    /// Its row.
    pub y: u16,
    /// The cells it has.
    pub room: u16,
    /// The opaque background it is on.
    pub bg: Rgba,
}

/// The text field `at.field`: its text, or its placeholder while empty, and while it takes
/// typing, the editor's cursor as a block, the text keeping the cursor in its room as a text
/// input scrolls.
pub fn field(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    at: FieldAt,
    editor: &LineEdit,
) {
    let FieldAt {
        field,
        x,
        y,
        room,
        bg,
    } = at;
    let Some(state) = model
        .fields()
        .into_iter()
        .find(|state| state.field == field)
    else {
        return;
    };
    let (text, cursor) = if state.typing {
        (editor.text(), Some(editor.cursor()))
    } else {
        (state.text.as_str(), None)
    };
    if room == 0 {
        return;
    }
    if text.is_empty() {
        painter.text(x, y, room, state.placeholder, Brush::new(palette.faint, bg));
    }
    let line = DisplayLine::new(text);
    // The cursor: a block over the cells of what it is on, or one cell past the end.
    let cursor_cells = cursor.map(|cursor| {
        let cells = line.cells(cursor..cursor + 1);
        if cells.is_empty() {
            cells.start..cells.start + 1
        } else {
            cells
        }
    });
    // The cursor stays in view: the text moves left once it would pass the field's end.
    let skip = cursor_cells
        .as_ref()
        .map_or(0, |cells| cells.end.saturating_sub(usize::from(room)));
    let text_brush = Brush::new(palette.text, bg);
    let block = Brush::new(palette.background, opaque(palette.text));
    let on_cursor = |cell: usize| {
        cursor_cells
            .as_ref()
            .is_some_and(|cells| cells.contains(&cell))
    };
    if !text.is_empty() {
        painter.line(x, y, room, skip, &line, |cell| {
            if on_cursor(cell) { block } else { text_brush }
        });
    }
    // Past the end, the cursor is a block of its own.
    if let Some(cells) = cursor_cells
        && cells.start >= line.width()
        && let Some(column) = cells
            .start
            .checked_sub(skip)
            .and_then(|column| u16::try_from(column).ok())
            .filter(|&column| column < room)
    {
        painter.text(x + column, y, 1, " ", block);
    }
}
