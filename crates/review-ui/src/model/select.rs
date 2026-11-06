//! Selecting and copying text in the diff and the buffer, and clicking hunks and gaps, once the
//! front end has found which text the pointer is over.

use super::{Effect, Mode, Model, plural};
use crate::frame::List;
use crate::rows::{Body, Cell, Row};
use crate::text::{self, Column, Pane, Selection, TextPos};

/// The scrolled list a pane's text is in.
#[must_use]
pub fn list_of(pane: Pane) -> List {
    match pane {
        Pane::Diff => List::Diff,
        Pane::Source => List::Buffer,
    }
}

/// The text of a diff row that copying takes in `column`: a line's code without its marker,
/// the side of a pair the column names, or a note's text; nothing for a side without a line.
fn row_text(row: &Row, column: Column) -> Option<&str> {
    match &row.body {
        Body::Code(cell) => Some(cell.code()),
        Body::Pair(pair) => match column {
            Column::Left => pair.left.as_ref().map(Cell::code),
            Column::Right => pair.right.as_ref().map(Cell::code),
            Column::Whole => None,
        },
        Body::Note(note) => Some(&note.text),
    }
}

/// A press of the main button on text of the diff or the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextPress {
    /// The pane pressed in.
    pub pane: Pane,
    /// The column of the pane the selection reads from.
    pub column: Column,
    /// Where in the pane's text the press is.
    pub pos: TextPos,
    /// Which click of a quick series it is: 2 selects a word, 3 or more a line.
    pub clicks: usize,
    /// Whether it extends the selection, as Shift does.
    pub extend: bool,
}

impl Model {
    /// The text of `row` in `pane` that copying takes: code without markers or gutters, from
    /// the side `column` names in a pair, or nothing for the padding of a side without a line.
    #[must_use]
    pub fn copyable(&self, pane: Pane, column: Column, row: usize) -> Option<&str> {
        match pane {
            Pane::Diff => row_text(self.rows.rows.get(row)?, column),
            Pane::Source => self
                .buffer
                .as_ref()
                .and_then(|buffer| buffer.lines.lines.get(row))
                .map(String::as_str),
        }
    }

    /// The selected text; a selection across rows of the diff leaves out our annotations, and
    /// one in a half of the side-by-side diff takes only that side's lines, skipping padding
    /// but keeping blank lines.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.filter(|selection| !selection.is_empty())?;
        let (start, end) = selection.ordered();
        let multi = start.row != end.row;
        Some(selection.text(|row| {
            if multi
                && selection.pane == Pane::Diff
                && self.rows.rows.get(row).is_some_and(|row| !row.is_code())
            {
                return None;
            }
            self.copyable(selection.pane, selection.column, row)
        }))
    }

    /// Copies the selected text.
    pub(super) fn copy(&mut self) {
        match self.selected_text() {
            Some(text) => {
                let lines = text.lines().count().max(1);
                self.effect(Effect::Copy(text));
                self.set_status(format!("Copied {}.", plural(lines, "line")));
            }
            None => self.set_status("Nothing is selected; drag over text to select it."),
        }
    }

    /// A press in the diff or the buffer: selects the clicked hunk or moves the cursor, expands
    /// a clicked gap, and starts a text selection; a double click selects a word, a triple
    /// click a line.
    pub fn press_text(&mut self, press: TextPress) {
        let TextPress {
            pane,
            column,
            pos,
            clicks,
            extend,
        } = press;
        match pane {
            Pane::Diff => {
                if self.mode != Mode::Diff {
                    self.enter_mode(Mode::Diff);
                }
                if self.is_gap(pos.row) {
                    self.expand_gap(pos.row);
                    return;
                }
                if let Some(hunk) = self.rows.rows.get(pos.row).and_then(|row| row.hunk) {
                    self.hunk = hunk;
                }
            }
            Pane::Source => {
                if let Some(buffer) = &mut self.buffer {
                    buffer.cursor = pos;
                }
            }
        }
        let line = self.copyable(pane, column, pos.row).unwrap_or_default();
        self.selection = Some(match clicks {
            2 => {
                let word = text::word_at(line, pos.col);
                Selection {
                    pane,
                    column,
                    anchor: TextPos {
                        row: pos.row,
                        col: word.start,
                    },
                    head: TextPos {
                        row: pos.row,
                        col: word.end,
                    },
                }
            }
            count if count >= 3 => Selection {
                pane,
                column,
                anchor: TextPos {
                    row: pos.row,
                    col: 0,
                },
                head: TextPos {
                    row: pos.row,
                    col: line.len(),
                },
            },
            _ => match self.selection {
                Some(selection) if extend && selection.pane == pane => Selection {
                    head: pos,
                    ..selection
                },
                _ => Selection::caret(pane, column, pos),
            },
        });
        self.dragging = Some(pane);
    }

    /// The pointer moved to `pos` of `pane` while a selection is dragged there.
    pub fn drag_text(&mut self, pane: Pane, pos: TextPos) {
        if let Some(selection) = &mut self.selection
            && selection.pane == pane
            && selection.head != pos
        {
            selection.head = pos;
        }
    }

    /// The pointer moved with the button up while a selection was dragged: the drag ends,
    /// copying nothing.
    pub fn drop_drag(&mut self) {
        self.dragging = None;
    }

    /// The button came up: a dragged selection ends, and its text goes to the primary
    /// selection.
    pub fn release_text(&mut self) {
        if self.dragging.take().is_some()
            && let Some(text) = self.selected_text()
        {
            // Linux's primary selection, pasted with the middle button.
            self.effect(Effect::CopyPrimary(text));
        }
    }

    /// A double click on the divider between the files and the diff: the files' width goes
    /// back to its default and is remembered.
    pub fn reset_sidebar_width(&mut self) {
        self.layout.sidebar = self.settings.front_end.default_sidebar();
        self.remember_layout();
    }
}

#[cfg(test)]
mod tests {
    use crate::rows::{Pair, RowKind};

    use super::*;

    fn cell(kind: RowKind, text: &str) -> Cell {
        Cell {
            kind,
            text: text.to_owned(),
            content_len: text.len(),
            highlights: Vec::new(),
            head: None,
            base: None,
            quiet: false,
        }
    }

    fn pair(left: Option<Cell>, right: Option<Cell>) -> Row {
        Row {
            file: 0,
            unit: "u1-0".parse().unwrap(),
            hunk: Some(0),
            source: 0,
            body: Body::Pair(Pair { left, right }),
        }
    }

    /// A selection down the left half of a replacement copies the base's lines: padding where
    /// the base has no line is skipped, and a real blank line stays a line.
    #[test]
    fn copying_a_half_skips_padding_and_keeps_blank_lines() {
        let rows = [
            pair(
                Some(cell(RowKind::Removed, "old")),
                Some(cell(RowKind::Added, "new")),
            ),
            pair(Some(cell(RowKind::Removed, "")), None),
            pair(None, Some(cell(RowKind::Added, "more"))),
            pair(
                Some(cell(RowKind::Context, "same")),
                Some(cell(RowKind::Context, "same")),
            ),
        ];
        let selection = Selection {
            pane: Pane::Diff,
            column: Column::Left,
            anchor: TextPos { row: 0, col: 0 },
            head: TextPos { row: 3, col: 4 },
        };
        let line = |index: usize| rows.get(index).and_then(|row| row_text(row, Column::Left));
        assert_eq!(selection.text(line), "old\n\nsame");
        let right = |index: usize| rows.get(index).and_then(|row| row_text(row, Column::Right));
        assert_eq!(selection.text(right), "new\nmore\nsame");
    }
}
