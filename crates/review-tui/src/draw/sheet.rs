//! The `?` cheat sheet, as review-gui draws it: a box over a dimmed screen with the title,
//! how to rebind and close, and every bound action in sections, in as many columns as fit.
//!
//! A sheet taller than the screen scrolls; see [`SheetScroll`].

use ratatui::layout::Rect;
use review_ui::model::Model;
use review_ui::paint::bars::{self, HELP_COLUMN_CELLS, HELP_KEYS_CELLS, HelpSection};
use review_ui::paint::{Palette, Piece};

use super::diff::border;
use super::painter::{Brush, Painter, ellipsize_end, width, wrap};
use super::{SheetScroll, opaque};

/// The widest sheet, in cells.
const SHEET_CELLS: u16 = 160;

/// The cells between the sheet's border and its text, left and right.
const PAD: u16 = 3;

/// The cells between two columns.
const GAP: u16 = 4;

/// The cells between an entry's keys and what they do.
const KEYS_GAP: u16 = 1;

/// A row of a column of the sheet.
enum Line<'a> {
    /// A section's title, with a hairline after it.
    Title(&'a Piece),
    /// An entry's keys, on its first row only, and a row of what they do.
    Entry {
        keys: Option<&'a Piece>,
        help: &'a Piece,
        text: String,
    },
    /// The air between sections.
    Blank,
}

/// The cheat sheet over `screen`, scrolled as `scroll` says, which learns how far it can go.
pub fn draw(
    painter: &mut Painter<'_>,
    model: &Model,
    palette: &Palette,
    screen: Rect,
    scroll: &mut SheetScroll,
) {
    let width = SHEET_CELLS.min(super::share(screen.width, 94));
    let inner = width.saturating_sub(2 + 2 * PAD);
    let count = bars::help_column_count(f32::from(inner), HELP_COLUMN_CELLS, f32::from(GAP));
    let help = bars::help(model, palette, count);
    painter.veil(screen, help.backdrop);
    let count = u16::try_from(help.columns.len().max(1)).unwrap_or(1);
    let column_width = inner.saturating_sub(GAP * (count - 1)) / count;
    let columns: Vec<Vec<Line<'_>>> = help
        .columns
        .iter()
        .map(|sections| column(sections, column_width))
        .collect();
    // The note goes beside the title, or under it when they do not fit together.
    let together = width_of(&help.title) + 2 + width_of(&help.note) <= inner;
    let title_rows: u16 = if together { 1 } else { 2 };
    let tallest = columns.iter().map(Vec::len).max().unwrap_or(0);
    let content = title_rows + 1 + u16::try_from(tallest).unwrap_or(u16::MAX);
    let height = (content + 4).min(screen.height);
    let viewport = height.saturating_sub(4);
    scroll.max = content.saturating_sub(viewport);
    scroll.viewport = viewport;
    scroll.offset = scroll.offset.min(scroll.max);
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(width) / 2,
        y: screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let surface = opaque(palette.surface);
    painter.fill(area, surface);
    border(painter, palette.hairline, area, surface);
    let left = area.x + 1 + PAD;
    let top = area.y + 2;
    for (y, row) in (top..top + viewport).zip(scroll.offset..) {
        if row < title_rows {
            let piece = if row == 0 { &help.title } else { &help.note };
            painter.piece(left, y, inner, piece, surface);
            if row == 0 && together {
                let note_width = width_of(&help.note);
                let at = left + inner - note_width;
                painter.piece(at, y, note_width, &help.note, surface);
            }
            continue;
        }
        let Some(index) = usize::from(row).checked_sub(usize::from(title_rows) + 1) else {
            continue;
        };
        for (lines, x) in columns
            .iter()
            .zip((0..).map(|column: u16| left + column * (column_width + GAP)))
        {
            if let Some(line) = lines.get(index) {
                draw_line(painter, palette, line, x, y, column_width, surface);
            }
        }
    }
}

/// The cells `piece` takes.
fn width_of(piece: &Piece) -> u16 {
    width(&piece.text)
}

/// The rows of a column of `sections`, `cells` wide: each section's title, then its entries
/// with what they do wrapped beside the keys, and a blank row between sections.
fn column(sections: &[HelpSection], cells: u16) -> Vec<Line<'_>> {
    let keys = keys_cells();
    let help_cells = cells.saturating_sub(keys + KEYS_GAP).max(1);
    let mut lines = Vec::new();
    for section in sections {
        if !lines.is_empty() {
            lines.push(Line::Blank);
        }
        lines.push(Line::Title(&section.title));
        for entry in &section.entries {
            let help = &entry.help;
            for (index, range) in wrap(&help.text, help_cells).into_iter().enumerate() {
                lines.push(Line::Entry {
                    keys: (index == 0).then_some(&entry.keys),
                    help,
                    text: help.text.get(range).unwrap_or_default().to_owned(),
                });
            }
        }
    }
    lines
}

/// The cells of the keys' column.
fn keys_cells() -> u16 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a column of a few cells"
    )]
    let cells = HELP_KEYS_CELLS as u16;
    cells
}

/// One row of a column at `x` on row `y`, `cells` wide.
fn draw_line(
    painter: &mut Painter<'_>,
    palette: &Palette,
    line: &Line<'_>,
    x: u16,
    y: u16,
    cells: u16,
    bg: review_ui::color::Rgba,
) {
    match line {
        Line::Title(title) => {
            let taken = painter.piece(x, y, cells, title, bg);
            let at = x + taken + 1;
            let rest = (x + cells).saturating_sub(at);
            painter.text(
                at,
                y,
                rest,
                &"─".repeat(usize::from(rest)),
                Brush::new(palette.hairline, bg),
            );
        }
        Line::Entry { keys, help, text } => {
            let keys_width = keys_cells();
            if let Some(keys) = keys {
                let shown = ellipsize_end(&keys.text, keys_width);
                painter.text(x, y, keys_width, &shown, Brush::piece(keys, bg));
            }
            let at = x + keys_width + KEYS_GAP;
            painter.text(
                at,
                y,
                (x + cells).saturating_sub(at),
                text,
                Brush::piece(help, bg),
            );
        }
        Line::Blank => {}
    }
}
