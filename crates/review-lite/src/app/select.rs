//! The mouse: selecting and copying text in the diff and the buffer, clicking hunks and gaps,
//! and dragging the divider between the files and the diff.

use std::time::Instant;

use gpui::{
    ClipboardItem, Context, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Window,
    point, px,
};
use gpui_component::scroll::ScrollbarHandle as _;

use super::text::{self, Column, Pane, Selection, TextPos};
use super::{Copy, Mode, ReviewApp, panes, plural, scroll};
use crate::rows::{Body, Cell, Row};

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

impl ReviewApp {
    /// The text of `row` in `pane` that copying takes: code without markers or gutters, from
    /// the side `column` names in a pair, or nothing for the padding of a side without a line.
    pub(super) fn copyable(&self, pane: Pane, column: Column, row: usize) -> Option<&str> {
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
    pub(super) fn selected_text(&self) -> Option<String> {
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

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        match self.selected_text() {
            Some(text) => {
                let lines = text.lines().count().max(1);
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                self.set_status(format!("Copied {}.", plural(lines, "line")));
            }
            None => self.set_status("Nothing is selected; drag over text to select it."),
        }
        cx.notify();
    }

    /// The column of the diff at window position `x`: a half of the side-by-side diff, or the
    /// whole width.
    pub(super) fn column_at(&self, pane: Pane, x: Pixels) -> Column {
        if pane != Pane::Diff || !self.side_by_side() {
            return Column::Whole;
        }
        let viewport = self.list.viewport_bounds();
        let middle = viewport.left()
            + px(super::present::FRAME)
            + (viewport.size.width - px(super::present::FRAME + super::present::DIVIDER)) / 2.0;
        if x < middle {
            Column::Left
        } else {
            Column::Right
        }
    }

    /// Maps a window position over `pane` to a place in its text, reading a pair's line from
    /// `column`.
    pub(super) fn text_pos(
        &self,
        pane: Pane,
        column: Column,
        position: Point<Pixels>,
    ) -> Option<TextPos> {
        let (list, count) = match pane {
            Pane::Diff => (&self.list, self.rows.rows.len()),
            Pane::Source => (&self.buffer_list, self.buffer.as_ref()?.lines.lines.len()),
        };
        let last_row = count.checked_sub(1)?;
        let viewport = list.viewport_bounds();
        let y = f32::from(position.y - viewport.top()) + scroll::offset_of(list);
        let row = scroll::whole(y / self.fonts.row_height()).min(last_row);
        let len = self.copyable(pane, column, row).map_or(0, str::len);
        // A pair's line is painted in its half; any other row across the whole width.
        let painted_in = match self.rows.rows.get(row).map(|row| &row.body) {
            Some(Body::Pair(_)) if pane == Pane::Diff => column,
            _ => Column::Whole,
        };
        let col = match self.painted.borrow().get(&(pane, painted_in, row)) {
            Some(layout) => {
                let bounds = layout.bounds();
                let inside = point(position.x, bounds.center().y);
                match layout.index_for_position(inside) {
                    Ok(index) | Err(index) => index,
                }
            }
            None if position.y < viewport.top() => 0,
            None => len,
        };
        Some(TextPos {
            row,
            col: col.min(len),
        })
    }

    /// A press in the diff or the buffer: selects the clicked hunk or moves the cursor, expands
    /// a clicked gap, and starts a text selection; a double click selects a word, a triple
    /// click a line.
    pub(super) fn mouse_down(
        &mut self,
        pane: Pane,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.latency.pending = Some(Instant::now());
        // A shift-click extends a selection in its own column; anything else starts in the
        // column under the pointer.
        let column = match self.selection {
            Some(selection) if event.modifiers.shift && selection.pane == pane => selection.column,
            _ => self.column_at(pane, event.position.x),
        };
        let Some(pos) = self.text_pos(pane, column, event.position) else {
            return;
        };
        match pane {
            Pane::Diff => {
                if self.mode != Mode::Diff {
                    self.enter_mode(Mode::Diff, window, cx);
                }
                if self.is_gap(pos.row) {
                    self.expand_gap(pos.row, cx);
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
        self.selection = Some(match event.click_count {
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
                Some(selection) if event.modifiers.shift && selection.pane == pane => Selection {
                    head: pos,
                    ..selection
                },
                _ => Selection::caret(pane, column, pos),
            },
        });
        self.dragging = Some(pane);
        cx.notify();
    }

    /// Drags the divider, or extends a drag selection, scrolling when the mouse leaves the pane.
    pub(super) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.resizing {
            if event.pressed_button == Some(MouseButton::Left) {
                let width = f32::from(window.viewport_size().width);
                self.layout.sidebar =
                    panes::sidebar_width(f32::from(event.position.x), width, self.fonts.ui_cell());
            } else {
                self.resizing = false;
                self.remember_layout();
            }
            cx.notify();
            return;
        }
        let Some(pane) = self.dragging else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.dragging = None;
            return;
        }
        let list = match pane {
            Pane::Diff => &self.list,
            Pane::Source => &self.buffer_list,
        };
        let viewport = list.viewport_bounds();
        let overshoot = if event.position.y < viewport.top() {
            f32::from(event.position.y - viewport.top())
        } else if event.position.y > viewport.bottom() {
            f32::from(event.position.y - viewport.bottom())
        } else {
            0.0
        };
        if overshoot != 0.0 {
            let target = scroll::offset_of(list) + overshoot.clamp(-60.0, 60.0);
            let max = f32::from(list.content_size().height - viewport.size.height).max(0.0);
            scroll::set_offset_of(list, target.clamp(0.0, max));
        }
        let column = self
            .selection
            .map_or(Column::Whole, |selection| selection.column);
        if let Some(pos) = self.text_pos(pane, column, event.position)
            && let Some(selection) = &mut self.selection
            && selection.pane == pane
            && selection.head != pos
        {
            selection.head = pos;
            cx.notify();
        }
    }

    pub(super) fn mouse_up(&mut self, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.resizing) {
            self.remember_layout();
            cx.notify();
        }
        if self.dragging.take().is_some()
            && let Some(text) = self.selected_text()
        {
            // Linux's primary selection, pasted with the middle button.
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
    }

    /// Starts dragging the divider, or resets the files' width on a double click.
    pub(super) fn press_divider(&mut self, click_count: usize, cx: &mut Context<Self>) {
        if click_count >= 2 {
            self.resizing = false;
            self.layout.sidebar = crate::config::DEFAULT_SIDEBAR_WIDTH;
            self.remember_layout();
        } else {
            self.resizing = true;
        }
        cx.notify();
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
