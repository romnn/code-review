//! The files pane: its header, the review's progress under it, a rule, and the tree.

use review_ui::color::Rgba;
use review_ui::frame::List;
use review_ui::model::Model;
use review_ui::paint::files::{self, ProgressLine, TreeRow};
use review_ui::paint::{Bar, Palette, Room};

use super::painter::{Brush, Painter, ellipsize_end, width};
use super::{first_row, opaque};
use crate::color::over;
use crate::measure::Sidebar;

/// The cells the tree's state column takes: the glyph and a space.
const COLUMN: u16 = 2;

/// The files pane in `sidebar`.
pub fn draw(painter: &mut Painter<'_>, model: &Model, palette: &Palette, sidebar: &Sidebar) {
    let surface = opaque(palette.surface);
    painter.fill(sidebar.area, surface);
    header(painter, model, palette, sidebar, surface);
    progress(painter, model, palette, sidebar, surface);
    let rule = sidebar.rule;
    if rule.height > 0 {
        let line = "─".repeat(usize::from(rule.width));
        painter.text(
            rule.x,
            rule.y,
            rule.width,
            &line,
            Brush::new(palette.hairline, surface),
        );
    }
    tree(painter, model, palette, sidebar, surface);
}

/// What the review is of, such as `#128 feature/reconcile`, and at the right the filter with
/// how many files it leaves while one narrows the tree; the branch makes way first, then the
/// query, and the number always shows.
fn header(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    sidebar: &Sidebar,
    surface: Rgba,
) {
    let area = sidebar.header;
    if area.height == 0 {
        return;
    }
    let header = files::files_header(model, palette);
    let y = area.y;
    let right = area.right().saturating_sub(1);
    let mut at = area.x + 1;
    if let Some(number) = &header.number {
        at += painter.piece(at, y, right.saturating_sub(at), number, surface) + 1;
    }
    let mut name_end = right;
    if let Some(filter) = &header.filter {
        let count_width = width(&filter.count.text);
        let count_at = right.saturating_sub(count_width).max(at);
        painter.piece(
            count_at,
            y,
            right.saturating_sub(count_at),
            &filter.count,
            surface,
        );
        let query = ellipsize_end(&filter.query.text, count_at.saturating_sub(at + 1));
        let query_at = count_at.saturating_sub(width(&query) + 1).max(at);
        painter.text(
            query_at,
            y,
            count_at.saturating_sub(query_at),
            &query,
            Brush::piece(&filter.query, surface),
        );
        name_end = query_at.saturating_sub(2);
    }
    let name = ellipsize_end(&header.name.text, name_end.saturating_sub(at));
    painter.text(
        at,
        y,
        name_end.saturating_sub(at),
        &name,
        Brush::piece(&header.name, surface),
    );
}

/// The files and hunks progress lines, with the parts that fit.
fn progress(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    sidebar: &Sidebar,
    surface: Rgba,
) {
    let area = sidebar.progress;
    let budget = area.width.saturating_sub(2);
    let lines = files::progress_lines(
        model,
        palette,
        Room {
            budget: f32::from(budget),
            cell: 1.0,
            gap: 1.0,
        },
    );
    for (line, y) in lines.into_iter().zip(area.top()..area.bottom()) {
        progress_line(painter, &line, area.x + 1, y, budget, surface);
    }
}

/// One progress line from `x` on row `y`, `budget` cells wide: the label, the bar over the
/// room left, the counts and the percent; without the bar, the counts and the percent keep
/// to the right edge.
fn progress_line(
    painter: &mut Painter<'_>,
    line: &ProgressLine,
    x: u16,
    y: u16,
    budget: u16,
    surface: Rgba,
) {
    let ProgressLine {
        label,
        bar,
        counts,
        percent,
    } = line;
    let right = x + budget;
    let tail = width(&counts.text)
        + percent
            .as_ref()
            .map_or(0, |percent| 1 + width(&percent.text));
    let mut at = x;
    if let Some(label) = label {
        at += painter.piece(at, y, budget, label, surface) + 1;
    }
    let tail_start = right.saturating_sub(tail);
    if let Some(bar) = bar {
        let room = tail_start.saturating_sub(at + 1);
        meter_bar(painter, at, y, room, bar, surface);
    }
    let mut at = tail_start.max(x);
    at += painter.piece(at, y, right.saturating_sub(at), counts, surface);
    if let Some(percent) = percent {
        painter.piece(at + 1, y, right.saturating_sub(at + 1), percent, surface);
    }
}

/// A progress bar `cells` wide from `x` on row `y`: its filled share in the fill color, the
/// part of it approved automatically at its end in the quieter color, and the rest of its
/// track, as heavy rules.
pub fn meter_bar(painter: &mut Painter<'_>, x: u16, y: u16, cells: u16, bar: &Bar, bg: Rgba) {
    let filled = filled_cells(bar.fraction, cells);
    let marked = filled_cells(bar.fraction - bar.auto, cells).min(filled);
    for offset in 0..cells {
        let color = if offset < marked {
            bar.fill
        } else if offset < filled {
            bar.auto_fill
        } else {
            bar.track
        };
        painter.symbol(x + offset, y, "━", Brush::new(color, bg));
    }
}

/// The cells of a bar `cells` wide that a share `fraction` fills, rounded down so only a
/// complete share fills it all.
fn filled_cells(fraction: f32, cells: u16) -> u16 {
    let filled = (fraction.clamp(0.0, 1.0) * f32::from(cells)).floor();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a share of the bar's cells, between 0 and the cells"
    )]
    let filled = filled as u16;
    filled.min(cells)
}

/// The visible tree rows.
fn tree(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    sidebar: &Sidebar,
    surface: Rgba,
) {
    let area = sidebar.tree;
    let first = first_row(model, List::Tree);
    for (offset, y) in (area.top()..area.bottom()).enumerate() {
        let Some(row) = files::tree_row(model, palette, first + offset) else {
            break;
        };
        tree_row(painter, &row, area.x, y, area.width, surface);
    }
}

/// A tree row as lazygit draws one: its indent, its marker (a folder's arrow or a file's change
/// kind), a space and the name, with the state glyph at the right, on the cursor's or the shown
/// file's background.
fn tree_row(painter: &mut Painter<'_>, row: &TreeRow, x: u16, y: u16, cells: u16, surface: Rgba) {
    let bg = row
        .background
        .map_or(surface, |background| over(surface, background));
    painter.fill(ratatui::layout::Rect::new(x, y, cells, 1), bg);
    let right = x + cells;
    let indent = row.depth.saturating_mul(files::TREE_INDENT_CELLS);
    let mut at = x
        .saturating_add(1)
        .saturating_add(u16::try_from(indent).unwrap_or(u16::MAX));
    // The glyph's column at the right, with a cell of room after it.
    let glyph_at = right.saturating_sub(1 + COLUMN);
    let marker_cells = u16::try_from(files::TREE_MARKER_CELLS).unwrap_or(u16::MAX);
    if at + marker_cells + 1 >= glyph_at {
        return;
    }
    painter.piece(at, y, 1, &row.marker, bg);
    at += marker_cells;
    let room = glyph_at.saturating_sub(at + 1);
    let name = ellipsize_end(&row.name.text, room);
    let mut brush = Brush::piece(&row.name, bg);
    brush.crossed = row.deleted;
    painter.text(at, y, room, &name, brush);
    painter.piece(glyph_at, y, COLUMN, &row.glyph, bg);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bar fills its cells by the share done, rounded down, and fills them all only when
    /// all is done.
    #[test]
    fn bars_fill_by_share() {
        assert_eq!(filled_cells(0.0, 10), 0);
        assert_eq!(filled_cells(0.25, 10), 2);
        assert_eq!(filled_cells(0.99, 10), 9);
        assert_eq!(filled_cells(1.0, 10), 10);
        assert_eq!(filled_cells(1.5, 10), 10);
    }
}
