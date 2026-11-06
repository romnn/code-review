//! The pickers, as review-gui draws them: a box over a dimmed screen with the query line, the
//! entries, for the content search a preview of the lines around the selected hit, and a
//! footer saying what the keys do.

use ratatui::layout::Rect;
use review_ui::color::{Hsla, Rgba};
use review_ui::frame::List;
use review_ui::line_edit::LineEdit;
use review_ui::model::Field;
use review_ui::model::Model;
use review_ui::model::picker::Picker;
use review_ui::paint::Palette;
use review_ui::paint::picker::{self, Content, Preview};

use super::bars::{FieldAt, field};
use super::painter::{Brush, Painter, ellipsize_end, ellipsize_start, width};
use super::{first_row, opaque};
use crate::color::over;
use crate::measure::{Panes, PickerPanes};

/// The cells of an entry's change-kind column.
const LETTER: u16 = 2;

/// The cells of a file entry's progress glyph column, as wide as the tree's.
const GLYPH: u16 = 2;

/// The open picker over the screen.
pub fn draw(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    panes: &Panes,
    editor: &LineEdit,
) {
    let (Some(picker), Some(at)) = (model.picker.as_ref(), panes.picker) else {
        return;
    };
    painter.veil(panes.area, picker::backdrop(palette));
    let surface = opaque(palette.surface);
    painter.fill(at.area, surface);
    let border = picker::border(palette, picker.editing);
    frame(painter, &at, [border, palette.hairline], surface);
    query(painter, model, palette, picker, &at, editor);
    entries(painter, model, palette, picker, at.list);
    if let Some(area) = at.preview {
        preview(painter, palette, picker, area);
    }
    let footer = picker::footer(picker, palette);
    let shown = ellipsize_end(&footer.text, at.footer.width.saturating_sub(2));
    painter.text(
        at.footer.x + 1,
        at.footer.y,
        at.footer.width.saturating_sub(2),
        &shown,
        Brush::piece(&footer, surface),
    );
}

/// The box's border in the first of `colors`, and in the second, hairline, the rules under
/// the query and over the footer and the line between the entries and the preview.
fn frame(painter: &mut Painter<'_>, at: &PickerPanes, colors: [Hsla; 2], bg: Rgba) {
    let area = at.area;
    if area.width < 2 || area.height < 6 {
        return;
    }
    let [border, rule] = colors;
    let brush = Brush::new(border, bg);
    let (left, right) = (area.left(), area.right() - 1);
    let (top, bottom) = (area.top(), area.bottom() - 1);
    let rules = [top + 2, bottom - 2];
    for x in left + 1..right {
        for y in [top, bottom] {
            painter.symbol(x, y, "─", brush);
        }
    }
    for y in top + 1..bottom {
        let rule = rules.contains(&y);
        painter.symbol(left, y, if rule { "├" } else { "│" }, brush);
        painter.symbol(right, y, if rule { "┤" } else { "│" }, brush);
    }
    // The rules inside are hairlines, as the panes' own are.
    let hairline = Brush::new(rule, bg);
    for y in rules {
        for x in left + 1..right {
            painter.symbol(x, y, "─", hairline);
        }
    }
    painter.symbol(left, top, "┌", brush);
    painter.symbol(right, top, "┐", brush);
    painter.symbol(left, bottom, "└", brush);
    painter.symbol(right, bottom, "┘", brush);
    if let Some(preview) = at.preview {
        let x = preview.x.saturating_sub(1);
        for y in preview.top()..preview.bottom() {
            painter.symbol(x, y, "│", hairline);
        }
        painter.symbol(x, rules[0], "┬", hairline);
        painter.symbol(x, rules[1], "┴", hairline);
    }
}

/// The query line: what the picker finds, the query, and at the right the count and the scope
/// on its own background.
fn query(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    picker: &Picker,
    at: &PickerPanes,
    editor: &LineEdit,
) {
    let area = at.query;
    let surface = opaque(palette.surface);
    let shown = picker::query(picker, palette);
    let y = area.y;
    let right = area.right().saturating_sub(1);
    let scope_text = format!(" {} ", shown.scope.text);
    let scope_width = width(&scope_text);
    let scope_at = right.saturating_sub(scope_width);
    let scope_bg = over(surface, shown.scope_background);
    painter.text(
        scope_at,
        y,
        scope_width,
        &scope_text,
        Brush::piece(&shown.scope, scope_bg),
    );
    let count_width = width(&shown.count.text);
    let count_at = scope_at.saturating_sub(count_width + 1);
    painter.piece(count_at, y, count_width, &shown.count, surface);
    let mut x = area.x + 1;
    x += painter.piece(x, y, width(&shown.title.text), &shown.title, surface) + 2;
    field(
        painter,
        model,
        palette,
        FieldAt {
            field: Field::PickerQuery,
            x,
            y,
            room: count_at.saturating_sub(x + 2),
            bg: surface,
        },
        editor,
    );
}

/// The entries that show in `area`: the selected one on the raised background.
fn entries(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    picker: &Picker,
    area: Rect,
) {
    let surface = opaque(palette.surface);
    let first = first_row(model, List::Picker);
    for (offset, y) in (area.top()..area.bottom()).enumerate() {
        let Some(row) = picker::row(picker, palette, first + offset) else {
            break;
        };
        let bg = row
            .background
            .map_or(surface, |background| over(surface, background));
        painter.fill(Rect::new(area.x, y, area.width, 1), bg);
        let right = area.right().saturating_sub(1);
        let x = area.x + 1;
        painter.piece(x, y, 1, &row.letter, bg);
        let at = x + LETTER;
        match &row.content {
            Content::File {
                path,
                color,
                progress,
            } => {
                let mut end = right;
                if let Some([counts, glyph]) = progress {
                    let glyph_at = right.saturating_sub(GLYPH);
                    painter.piece(glyph_at + 1, y, 1, glyph, bg);
                    let counts_width = width(&counts.text);
                    let counts_at = glyph_at.saturating_sub(counts_width + 1);
                    painter.piece(counts_at, y, counts_width, counts, bg);
                    end = counts_at.saturating_sub(2);
                }
                painter.styled(at, y, end.saturating_sub(at), path, *color, bg);
            }
            Content::Hit {
                location,
                text,
                color,
            } => {
                // The location takes at most 45% of the row, keeping its end.
                let room = right.saturating_sub(at);
                let most = u16::try_from(u32::from(room) * 45 / 100).unwrap_or(room);
                let shown = ellipsize_start(&location.text, most);
                let taken = painter.text(at, y, most, &shown, Brush::piece(location, bg));
                let text_at = at + taken + 1;
                painter.styled(text_at, y, right.saturating_sub(text_at), text, *color, bg);
            }
        }
    }
}

/// The preview of the selected content hit in `area`: the file's path over the lines around
/// the hit, or why there are none.
fn preview(painter: &mut Painter<'_>, palette: &Palette, picker: &Picker, area: Rect) {
    let background = opaque(palette.background);
    painter.fill(area, background);
    let Some(shown) = picker::preview(picker, palette) else {
        return;
    };
    let right = area.right();
    let (path, lines) = match shown {
        Preview::Note(note) => {
            let room = area.width.saturating_sub(2);
            painter.text(
                area.x + 1,
                area.y,
                room,
                &ellipsize_end(&note.text, room),
                Brush::piece(&note, background),
            );
            return;
        }
        Preview::Lines { path, lines } => (path, lines),
    };
    let room = area.width.saturating_sub(2);
    painter.text(
        area.x + 1,
        area.y,
        room,
        &ellipsize_start(&path.text, room),
        Brush::piece(&path, background),
    );
    for (line, y) in lines.iter().zip(area.y + 1..area.bottom()) {
        let bg = line
            .background
            .map_or(background, |color| over(background, color));
        painter.fill(Rect::new(area.x, y, area.width, 1), bg);
        let mut x = area.x + 1;
        x += painter.piece(x, y, right.saturating_sub(x), &line.number, bg) + 2;
        painter.styled(x, y, right.saturating_sub(x), &line.text, line.color, bg);
    }
}
