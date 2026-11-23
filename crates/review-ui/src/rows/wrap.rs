//! Projects the rows onto the screen: one screen row for each row, or, while long lines wrap,
//! as many as each line of code needs at the room the diff has.
//!
//! The rows keep everything that names text, so a selection, a find match or a copy means the
//! same text however wide the diff is; only the screen rows, which scrolling counts, follow the
//! width. Each screen row shows a piece of one row, and a row's pieces are consecutive.

use std::ops::Range;

use super::{Body, Cell, Layout};
use crate::paint::display::DisplayLine;

/// The code cells a line has room for, where long lines wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapRoom {
    /// A line across the diff's width.
    pub whole: usize,
    /// A side of a pair.
    pub half: usize,
}

/// One screen row: a piece of one of [`Layout::rows`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenRow {
    /// The row it shows a piece of, by index into [`Layout::rows`].
    pub row: usize,
    /// The bytes of the text of the row's cell across the width, or of a pair's left cell, that
    /// it shows, then of a pair's right cell; `None` for a side without a line, and for one
    /// whose line ended on an earlier screen row, which shows its background alone.
    pub shown: [Option<Range<usize>>; 2],
}

impl ScreenRow {
    /// The side and byte of its row's line where it starts: on the first side it shows text
    /// of, `None` for a note.
    #[must_use]
    pub fn start(&self) -> Option<(usize, usize)> {
        self.shown
            .iter()
            .enumerate()
            .find_map(|(side, shown)| shown.as_ref().map(|shown| (side, shown.start)))
    }
}

/// Lays `layout`'s rows out on screen rows, wrapping lines of code wider than `room` when
/// there is one, and notes the room in [`Layout::wrapped`].
pub fn project(layout: &mut Layout, room: Option<WrapRoom>) {
    let mut screen = Vec::with_capacity(layout.rows.len());
    let mut starts = Vec::with_capacity(layout.rows.len() + 1);
    for (index, row) in layout.rows.iter().enumerate() {
        starts.push(screen.len());
        let width = room.map(|room| match row.body {
            Body::Pair(_) => room.half,
            Body::Code(_) | Body::Note(_) => room.whole,
        });
        let sides = row.sides().map(|cell| cell.map(|cell| pieces(cell, width)));
        let count = sides.iter().flatten().map(Vec::len).max().unwrap_or(1);
        for piece in 0..count {
            screen.push(ScreenRow {
                row: index,
                shown: sides
                    .each_ref()
                    .map(|side| side.as_ref().and_then(|pieces| pieces.get(piece).cloned())),
            });
        }
    }
    starts.push(screen.len());
    layout.screen = screen;
    layout.starts = starts;
    layout.wrapped = room;
}

/// The pieces `cell` shows on, wrapped at `room` cells when there is a room.
fn pieces(cell: &Cell, room: Option<usize>) -> Vec<Range<usize>> {
    match room {
        Some(room) if cell.width() > room => DisplayLine::new(&cell.text).wrap(room),
        _ => std::iter::once(0..cell.text.len()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use review_core::UnitId;

    use super::super::{Note, Pair, Row, RowKind};
    use super::*;

    fn cell(text: &str) -> Cell {
        Cell {
            kind: RowKind::Context,
            text: text.to_owned(),
            content_len: text.len(),
            highlights: Vec::new(),
            deleted: Vec::new(),
            head: None,
            base: None,
            quiet: false,
            moved: false,
        }
    }

    fn row(body: Body) -> Row {
        Row {
            file: 0,
            unit: "u1-0".parse::<UnitId>().unwrap(),
            hunk: None,
            source: 0,
            body,
        }
    }

    /// The shown text of each screen row, a side at a time.
    fn shown(layout: &Layout) -> Vec<[Option<&str>; 2]> {
        layout
            .screen
            .iter()
            .map(|screen| {
                let cells = layout.rows[screen.row].sides();
                [0, 1].map(|side| {
                    let range = screen.shown[side].clone()?;
                    cells[side].map(|cell| &cell.text[range])
                })
            })
            .collect()
    }

    /// A long line takes as many screen rows as it needs and a note one; the rows each start
    /// at their first screen row, and without a room every row takes one.
    #[test]
    fn long_lines_take_the_screen_rows_they_need() {
        let mut layout = Layout {
            rows: vec![
                row(Body::Code(cell("one two three"))),
                row(Body::Note(Note::plain(RowKind::Info, "a note".to_owned()))),
                row(Body::Code(cell(""))),
            ],
            ..Layout::default()
        };
        project(&mut layout, Some(WrapRoom { whole: 8, half: 4 }));
        assert_eq!(
            shown(&layout),
            [
                [Some("one two "), None],
                [Some("three"), None],
                [None, None],
                [Some(""), None],
            ]
        );
        assert_eq!(layout.starts, [0, 2, 3, 4]);
        project(&mut layout, None);
        assert_eq!(layout.starts, [0, 1, 2, 3]);
        assert_eq!(layout.wrapped, None);
    }

    /// A pair takes the screen rows its longer side needs: the shorter side's line shows once,
    /// then its background alone, and a side without a line has none on any of them.
    #[test]
    fn a_pair_wraps_each_side_apart() {
        let mut layout = Layout {
            rows: vec![
                row(Body::Pair(Pair {
                    left: Some(cell("aaaa bbbb cccc")),
                    right: Some(cell("")),
                })),
                row(Body::Pair(Pair {
                    left: None,
                    right: Some(cell("dddd eeee")),
                })),
            ],
            ..Layout::default()
        };
        project(&mut layout, Some(WrapRoom { whole: 20, half: 5 }));
        assert_eq!(
            shown(&layout),
            [
                [Some("aaaa "), Some("")],
                [Some("bbbb "), None],
                [Some("cccc"), None],
                [None, Some("dddd ")],
                [None, Some("eeee")],
            ]
        );
    }
}
