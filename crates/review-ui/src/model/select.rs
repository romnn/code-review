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

/// The deleted text an edited line of `row` shows in `column`, which copying leaves out.
fn deleted(row: &Row, column: Column) -> &[std::ops::Range<usize>] {
    let cell = match &row.body {
        Body::Code(cell) => Some(cell),
        Body::Pair(pair) => match column {
            Column::Left => pair.left.as_ref(),
            Column::Right => pair.right.as_ref(),
            Column::Whole => None,
        },
        Body::Note(_) => None,
    };
    cell.map_or(&[], |cell| &cell.deleted)
}

/// Where on a screen row of the diff the pointer is, for [`Model::diff_pos`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// At this byte of the text the row draws.
    Byte(usize),
    /// Above the list: at the start of what the row draws.
    Start,
    /// Below the list, or past the text the row draws: at its end.
    End,
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
    /// The screen row pressed, which is `pos.row` but in the diff, whose rows can take
    /// several screen rows each.
    pub screen: usize,
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

    /// The text of `column` that screen row `screen` of the diff draws: a line's piece, a
    /// note's text, or nothing on a side without a line or past where it ended.
    #[must_use]
    pub fn diff_shown(&self, screen: usize, column: Column) -> Option<&str> {
        let range = self.diff_shown_range(screen, column)?;
        let row = self.rows.row_on_screen(screen)?;
        row_text(row, column)?.get(range)
    }

    /// The bytes of its row's text in `column` that screen row `screen` of the diff draws.
    fn diff_shown_range(&self, screen: usize, column: Column) -> Option<std::ops::Range<usize>> {
        let piece = self.rows.screen.get(screen)?;
        match &self.rows.rows.get(piece.row)?.body {
            Body::Note(note) => Some(0..note.text.len()),
            Body::Code(_) | Body::Pair(_) => {
                let side = usize::from(column == Column::Right);
                piece.shown.get(side)?.clone()
            }
        }
    }

    /// Where `hit` on screen row `screen` of the diff is in its text in `column`: kept within
    /// what the row draws, so a press past a wrapped piece's end lands at its end rather than
    /// in the next one, and within the code copying takes.
    #[must_use]
    pub fn diff_pos(&self, screen: usize, column: Column, hit: Hit) -> Option<TextPos> {
        let row = self.rows.screen.get(screen)?.row;
        let len = self.copyable(Pane::Diff, column, row).map_or(0, str::len);
        // A side whose line ended on an earlier screen row draws nothing: past its line's end.
        let shown = self.diff_shown_range(screen, column).unwrap_or(len..len);
        let col = match hit {
            Hit::Byte(byte) => (shown.start + byte).min(shown.end),
            Hit::Start => shown.start,
            Hit::End => shown.end,
        };
        Some(TextPos {
            row,
            col: col.min(len),
        })
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
            let text = self.copyable(selection.pane, selection.column, row)?;
            let skipped = match selection.pane {
                Pane::Diff => self
                    .rows
                    .rows
                    .get(row)
                    .map_or(&[][..], |row| deleted(row, selection.column)),
                Pane::Source => &[],
            };
            Some((text, skipped))
        }))
    }

    /// Copies the selected text to the clipboard and the primary selection.
    pub(super) fn copy(&mut self) {
        match self.selected_text() {
            Some(text) => {
                let lines = text.lines().count().max(1);
                // Linux's primary selection too, pasted with the middle button.
                self.effect(Effect::CopyPrimary(text.clone()));
                self.effect(Effect::Copy(text));
                self.set_status(format!("Copied {}.", plural(lines, "line")));
            }
            None => self.set_status("Nothing is selected; drag over text to select it."),
        }
    }

    /// A press in the diff or the buffer: selects the clicked hunk or moves the cursor and
    /// expands a clicked gap, where the mouse may navigate, and starts a text selection, where
    /// it may select; a double click selects a word, a triple click a line.
    pub fn press_text(&mut self, press: TextPress) {
        let TextPress {
            pane,
            column,
            pos,
            screen,
            clicks,
            extend,
        } = press;
        if self.settings.mouse.navigate {
            match pane {
                Pane::Diff => {
                    // A press in a folder's diff opens the file pressed, at the line pressed;
                    // the rows it pressed are gone, so nothing else of the press applies.
                    if self.folder.is_some() {
                        if self.open_from_folder(Some(screen)) {
                            self.enter_mode(Mode::Diff);
                        }
                        return;
                    }
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
        }
        if !self.settings.mouse.select {
            return;
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

    /// The button came up: a dragged selection ends, and with copy-on-select its text is
    /// copied as an explicit copy does; without it, selecting copies nothing.
    pub fn release_text(&mut self) {
        if self.dragging.take().is_some() && self.settings.mouse.copy_on_select {
            self.copy();
        }
    }

    /// A double click on the divider between the files and the diff: the files fit their tree
    /// again, which is remembered.
    pub fn reset_sidebar_width(&mut self) {
        if !self.settings.mouse.navigate {
            return;
        }
        self.layout.sidebar = crate::panes::SidebarWidth::Fit;
        self.remember_layout();
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::rows::{Pair, RowKind};

    use super::*;

    fn cell(kind: RowKind, text: &str) -> Cell {
        Cell {
            kind,
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

    fn pair(left: Option<Cell>, right: Option<Cell>) -> Row {
        Row {
            file: 0,
            unit: "u1-0".parse().unwrap(),
            hunk: Some(0),
            source: 0,
            body: Body::Pair(Pair { left, right }),
        }
    }

    /// Copying an edited line takes its code, never the deleted text struck out in it.
    #[test]
    fn copying_an_edited_line_leaves_out_its_deleted_text() {
        let row = Row {
            file: 0,
            unit: "u1-0".parse().unwrap(),
            hunk: Some(0),
            source: 0,
            body: Body::Code(Cell {
                deleted: std::iter::once(4..6).collect(),
                ..cell(RowKind::Edited, "min(3220)")
            }),
        };
        let selection = Selection {
            pane: Pane::Diff,
            column: Column::Whole,
            anchor: TextPos { row: 0, col: 2 },
            head: TextPos { row: 0, col: 9 },
        };
        let line = |_| Some((row_text(&row, Column::Whole)?, deleted(&row, Column::Whole)));
        assert_eq!(selection.text(line), "n(20)");
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
        let line = |index: usize| {
            let row = rows.get(index)?;
            Some((row_text(row, Column::Left)?, deleted(row, Column::Left)))
        };
        assert_eq!(
            selection.text(line),
            indoc! {"
                old

                same"}
        );
        let right = |index: usize| {
            let row = rows.get(index)?;
            Some((row_text(row, Column::Right)?, deleted(row, Column::Right)))
        };
        assert_eq!(
            selection.text(right),
            indoc! {"
                new
                more
                same"}
        );
    }
}
