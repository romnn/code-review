//! The question whether to trust the checkout's code, as a box over a dimmed screen: the
//! question, why it is asked, and a row for each answer.

use ratatui::layout::Rect;
use review_ui::model::Model;
use review_ui::paint::trust::trust_prompt;
use review_ui::paint::{Palette, Piece};

use super::diff::border;
use super::opaque;
use super::painter::{Painter, width, wrap};

/// The widest box, in cells.
const BOX_CELLS: u16 = 72;

/// The cells between the box's border and its text, left and right.
const PAD: u16 = 3;

/// The cells an answer's key takes before what it does.
const KEY_CELLS: u16 = 5;

/// The trust question over `screen`, while it shows.
pub fn draw(painter: &mut Painter<'_>, model: &Model, palette: &Palette, screen: Rect) {
    let Some(prompt) = trust_prompt(model, palette) else {
        return;
    };
    let cells = BOX_CELLS.min(screen.width.saturating_sub(4));
    let inner = cells.saturating_sub(2 + 2 * PAD).max(1);
    let mut rows: Vec<(u16, Piece)> = vec![(0, prompt.title.clone()), (0, blank(palette))];
    for paragraph in &prompt.body {
        for range in wrap(&paragraph.text, inner) {
            rows.push((
                0,
                Piece {
                    text: paragraph.text.get(range).unwrap_or_default().to_owned(),
                    ..paragraph.clone()
                },
            ));
        }
        rows.push((0, blank(palette)));
    }
    let label_cells = inner.saturating_sub(KEY_CELLS).max(1);
    let mut answers: Vec<(Piece, Piece)> = Vec::new();
    for choice in &prompt.choices {
        for (index, range) in wrap(&choice.label.text, label_cells)
            .into_iter()
            .enumerate()
        {
            let key = if index == 0 {
                choice.key.clone()
            } else {
                blank(palette)
            };
            let label = Piece {
                text: choice.label.text.get(range).unwrap_or_default().to_owned(),
                ..choice.label.clone()
            };
            answers.push((key, label));
        }
    }
    let content = u16::try_from(rows.len() + answers.len()).unwrap_or(u16::MAX);
    let height = (content + 4).min(screen.height);
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(cells) / 2,
        y: screen.y + screen.height.saturating_sub(height) / 2,
        width: cells,
        height,
    };
    painter.veil(screen, palette.background.alpha(0.6));
    let surface = opaque(palette.surface);
    painter.fill(area, surface);
    border(painter, palette.hairline, area, surface);
    let left = area.x + 1 + PAD;
    let bottom = area.bottom().saturating_sub(2);
    let mut y = area.y + 2;
    for (_, piece) in &rows {
        if y > bottom {
            return;
        }
        painter.piece(left, y, inner, piece, surface);
        y += 1;
    }
    for (key, label) in &answers {
        if y > bottom {
            return;
        }
        painter.piece(left, y, KEY_CELLS.min(width(&key.text) + 1), key, surface);
        painter.piece(left + KEY_CELLS, y, label_cells, label, surface);
        y += 1;
    }
}

/// An empty row.
fn blank(palette: &Palette) -> Piece {
    Piece::new("", palette.text)
}
