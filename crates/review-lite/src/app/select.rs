//! The mouse: selecting and copying text in the diff and the buffer, clicking hunks and gaps,
//! and dragging the divider between the files and the diff.

use std::time::Instant;

use gpui::{
    ClipboardItem, Context, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Window,
    point,
};
use gpui_component::scroll::ScrollbarHandle as _;

use super::text::{self, Pane, Selection, TextPos};
use super::{Copy, Mode, ReviewApp, panes, plural, scroll};

impl ReviewApp {
    /// The text of `row` in `pane` that copying takes: code without markers or gutters.
    pub(super) fn copyable(&self, pane: Pane, row: usize) -> Option<&str> {
        match pane {
            Pane::Diff => self
                .rows
                .rows
                .get(row)
                .map(|row| row.text.get(..row.content_len).unwrap_or(&row.text)),
            Pane::Source => self
                .buffer
                .as_ref()
                .and_then(|buffer| buffer.lines.lines.get(row))
                .map(String::as_str),
        }
    }

    /// The selected text; a selection across rows of the diff leaves out our annotations.
    pub(super) fn selected_text(&self) -> Option<String> {
        let selection = self.selection.filter(|selection| !selection.is_empty())?;
        let (start, end) = selection.ordered();
        let multi = start.row != end.row;
        Some(selection.text(|row| {
            if multi
                && selection.pane == Pane::Diff
                && self
                    .rows
                    .rows
                    .get(row)
                    .is_some_and(|row| !row.kind.is_code())
            {
                return None;
            }
            self.copyable(selection.pane, row)
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

    /// Maps a window position over `pane` to a place in its text.
    pub(super) fn text_pos(&self, pane: Pane, position: Point<Pixels>) -> Option<TextPos> {
        let (list, count) = match pane {
            Pane::Diff => (&self.list, self.rows.rows.len()),
            Pane::Source => (&self.buffer_list, self.buffer.as_ref()?.lines.lines.len()),
        };
        let last_row = count.checked_sub(1)?;
        let viewport = list.viewport_bounds();
        let y = f32::from(position.y - viewport.top()) + scroll::offset_of(list);
        let row = scroll::whole(y / self.fonts.row_height()).min(last_row);
        let len = self.copyable(pane, row).map_or(0, str::len);
        let col = match self.painted.borrow().get(&(pane, row)) {
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
        let Some(pos) = self.text_pos(pane, event.position) else {
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
        let line = self.copyable(pane, pos.row).unwrap_or_default();
        self.selection = Some(match event.click_count {
            2 => {
                let word = text::word_at(line, pos.col);
                Selection {
                    pane,
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
                _ => Selection::caret(pane, pos),
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
                self.layout.sidebar = panes::sidebar_width(f32::from(event.position.x), width);
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
        if let Some(pos) = self.text_pos(pane, event.position)
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
