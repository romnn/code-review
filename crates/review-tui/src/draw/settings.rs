//! The settings list, as review-gui draws it: in the files pane's place, or over the left of
//! the diff when the files pane is hidden or narrow, with its title, its search, its sections
//! and entries, and a footer saying what the cursor's entry does.

use ratatui::layout::Rect;
use review_ui::color::Rgba;
use review_ui::frame::List;
use review_ui::line_edit::LineEdit;
use review_ui::model::{Field, Model};
use review_ui::paint::Palette;
use review_ui::paint::settings::{
    self, DETAILS_MIN_CELLS, LABEL_CELLS, SettingsEntry, SettingsLine, Swatch,
};

use super::bars::{FieldAt, field};
use super::painter::{Brush, Painter, ellipsize_end, width};
use super::{first_row, opaque};
use crate::color::over;
use crate::measure::{Panes, SettingsPanes};

/// The cells of the applied marker's column.
const MARKER: u16 = 2;

/// The settings panel.
pub fn draw(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    panes: &Panes,
    editor: &LineEdit,
) {
    let Some(at) = panes.settings else {
        return;
    };
    let surface = opaque(palette.surface);
    painter.fill(at.area, surface);
    if let Some(edge) = at.edge {
        painter.fill(edge, surface);
        for y in edge.top()..edge.bottom() {
            painter.symbol(edge.x, y, "│", Brush::new(palette.hairline, surface));
        }
    }
    let title = settings::title(palette);
    painter.piece(
        at.title.x + 1,
        at.title.y,
        at.title.width.saturating_sub(2),
        &title,
        surface,
    );
    search(painter, model, palette, &at, editor);
    rule(
        painter,
        at.footer_rule,
        Brush::new(palette.hairline, surface),
    );
    entries(painter, model, palette, at.list);
    footer(painter, model, palette, at.footer);
}

/// A rule across `area`.
fn rule(painter: &mut Painter<'_>, area: Rect, brush: Brush) {
    if area.height == 0 {
        return;
    }
    let line = "─".repeat(usize::from(area.width));
    painter.text(area.x, area.y, area.width, &line, brush);
}

/// The search row and the rule under it, accented while the field has the keys.
fn search(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    at: &SettingsPanes,
    editor: &LineEdit,
) {
    let surface = opaque(palette.surface);
    let Some(search) = settings::search(model, palette) else {
        return;
    };
    rule(painter, at.rule, Brush::new(search.rule, surface));
    let area = at.search;
    let right = area.right().saturating_sub(1);
    let count_width = width(&search.count.text);
    let count_at = right.saturating_sub(count_width);
    painter.piece(count_at, area.y, count_width, &search.count, surface);
    painter.piece(area.x + 1, area.y, 1, &search.slash, surface);
    let x = area.x + 3;
    field(
        painter,
        model,
        palette,
        FieldAt {
            field: Field::SettingsSearch,
            x,
            y: area.y,
            room: count_at.saturating_sub(x + 1),
            bg: surface,
        },
        editor,
    );
}

/// The list's lines that show in `area`: section titles, and entries with their marker,
/// label, swatch and, when the panel is wide enough, their detail.
fn entries(painter: &mut Painter<'_>, model: &Model, palette: &Palette, area: Rect) {
    let surface = opaque(palette.surface);
    let lines = settings::settings_lines(model, palette);
    if lines.is_empty() {
        let empty = settings::empty(palette);
        let room = area.width.saturating_sub(2);
        painter.text(
            area.x + 1,
            area.y,
            room,
            &ellipsize_end(&empty.text, room),
            Brush::piece(&empty, surface),
        );
        return;
    }
    let details = f32::from(area.width) >= DETAILS_MIN_CELLS;
    let first = first_row(model, List::Settings);
    for (line, y) in lines.iter().skip(first).zip(area.top()..area.bottom()) {
        match line {
            // A section's title, with a hairline after it for every section but the first, which
            // sets the groups apart.
            SettingsLine::Section { title, rule } => {
                let x = area.x + 1;
                let room = area.width.saturating_sub(2);
                let taken = painter.piece(x, y, room, title, surface);
                if let Some(color) = rule {
                    let at = x + taken + 1;
                    let rest = area.right().saturating_sub(at);
                    painter.text(
                        at,
                        y,
                        rest,
                        &"─".repeat(usize::from(rest)),
                        Brush::new(*color, surface),
                    );
                }
            }
            SettingsLine::Entry(entry) => entry_row(painter, entry, area, y, details, surface),
        }
    }
}

/// An entry on row `y` of `area`: the cursor's bar and background, the applied marker, the
/// label, the swatch at the edge or after the label column, and the detail.
fn entry_row(
    painter: &mut Painter<'_>,
    entry: &SettingsEntry,
    area: Rect,
    y: u16,
    details: bool,
    surface: Rgba,
) {
    let bg = entry
        .background
        .map_or(surface, |background| over(surface, background));
    painter.fill(Rect::new(area.x, y, area.width, 1), bg);
    if let Some(bar) = entry.bar {
        painter.symbol(area.x, y, "▎", Brush::new(bar, bg));
    }
    let right = area.right().saturating_sub(1);
    let marker_at = area.x + 1;
    painter.piece(marker_at, y, 1, &entry.marker, bg);
    let label_at = marker_at + MARKER;
    let swatch_width = entry.swatch.map_or(0, |_| SWATCH);
    // With details, labels share one column so swatches and details line up down the list;
    // without, the label takes the room and the swatch sits at the edge.
    let (label_room, swatch_at) = if details {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a column of a few cells"
        )]
        let column = LABEL_CELLS as u16;
        (column, label_at + column + 1)
    } else {
        let swatch_at = right.saturating_sub(swatch_width);
        (swatch_at.saturating_sub(label_at + 1), swatch_at)
    };
    let label = ellipsize_end(&entry.label.text, label_room);
    painter.text(
        label_at,
        y,
        label_room,
        &label,
        Brush::piece(&entry.label, bg),
    );
    if let Some(swatch) = entry.swatch {
        draw_swatch(painter, &swatch, swatch_at, y, bg);
    }
    if details {
        let detail_at = swatch_at + swatch_width + u16::from(entry.swatch.is_some());
        let room = right.saturating_sub(detail_at);
        let shown = ellipsize_end(&entry.detail.text, room);
        painter.text(detail_at, y, room, &shown, Brush::piece(&entry.detail, bg));
    }
}

/// The cells of a theme's swatch: four squares.
const SWATCH: u16 = 4;

/// A theme's swatch from `x` on row `y`: its four colors as squares, which stand apart from
/// the rows above and below as review-gui's do.
fn draw_swatch(painter: &mut Painter<'_>, swatch: &Swatch, x: u16, y: u16, bg: Rgba) {
    let Swatch { squares, .. } = swatch;
    for (square, at) in squares.iter().zip(x..) {
        painter.symbol(at, y, "■", Brush::new(*square, bg));
    }
}

/// The footer: `preview` while the diff previews the cursor's entry, then what the entry
/// does.
fn footer(painter: &mut Painter<'_>, model: &Model, palette: &Palette, area: Rect) {
    if area.height == 0 {
        return;
    }
    let surface = opaque(palette.surface);
    let footer = settings::footer(model, palette);
    let right = area.right().saturating_sub(1);
    let mut x = area.x + 1;
    if let Some(preview) = &footer.preview {
        x += painter.piece(x, area.y, right.saturating_sub(x), preview, surface) + 2;
    }
    if let Some(detail) = &footer.detail {
        let room = right.saturating_sub(x);
        let shown = ellipsize_end(&detail.text, room);
        painter.text(x, area.y, room, &shown, Brush::piece(detail, surface));
    }
}
