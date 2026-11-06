//! The mouse over text: finding the text under the pointer in the diff and the buffer for the
//! model's selection, and dragging the divider between the files and the diff.

use std::time::Instant;

use gpui::{
    Context, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Window, point, px,
};
use gpui_component::scroll::ScrollbarHandle as _;
use review_ui::model::{Model, TextPress, list_of};
use review_ui::rows::Body;
use review_ui::text::{Column, Pane, TextPos};

use super::measure::{DIVIDER, FRAME};
use super::{ReviewApp, scroll};

impl ReviewApp {
    /// The column of the diff at window position `x`: a half of the side-by-side diff, or the
    /// whole width.
    pub(super) fn column_at(&self, pane: Pane, x: Pixels) -> Column {
        if pane != Pane::Diff || !self.side_by_side() {
            return Column::Whole;
        }
        let viewport = self.lists.handles.diff.viewport_bounds();
        let middle =
            viewport.left() + px(FRAME) + (viewport.size.width - px(FRAME + DIVIDER)) / 2.0;
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
        let list = list_of(pane);
        let last_row = self.list_rows(list).checked_sub(1)?;
        let viewport = self.lists.handle(list).viewport_bounds();
        let y = f32::from(position.y - viewport.top()) + self.offset_px(list);
        let row = scroll::whole(y / self.sizes().row_height()).min(last_row);
        let len = self.copyable(pane, column, row).map_or(0, str::len);
        // A pair's line is painted in its half; any other row across the whole width.
        let painted_in = match self.rows.rows.get(row).map(|row| &row.body) {
            Some(Body::Pair(_)) if pane == Pane::Diff => column,
            _ => Column::Whole,
        };
        let col = match self.painted.borrow().get(&(pane, painted_in, row)) {
            Some(painted) => {
                let bounds = painted.layout.bounds();
                let inside = point(position.x, bounds.center().y);
                // The layout is of the shown text, whose placeholders and tabs are longer
                // than what they stand for.
                match painted.layout.index_for_position(inside) {
                    Ok(index) | Err(index) => painted.line.byte_at_shown(index),
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

    /// A press in the diff or the buffer, handed to the model at the text under the pointer.
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
        let press = TextPress {
            pane,
            column,
            pos,
            clicks: event.click_count,
            extend: event.modifiers.shift,
        };
        self.update_model(window, cx, |model| model.press_text(press));
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
                let width = review_ui::panes::sidebar_width(
                    f32::from(event.position.x),
                    f32::from(window.viewport_size().width),
                    self.sizes().ui_cell(),
                );
                self.update_model(window, cx, |model| model.set_sidebar_width(width));
            } else {
                self.resizing = false;
                self.update_model(window, cx, Model::remember_layout);
            }
            return;
        }
        let Some(pane) = self.dragging else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.update_model(window, cx, Model::drop_drag);
            return;
        }
        let list = list_of(pane);
        let viewport = self.lists.handle(list).viewport_bounds();
        let overshoot = if event.position.y < viewport.top() {
            f32::from(event.position.y - viewport.top())
        } else if event.position.y > viewport.bottom() {
            f32::from(event.position.y - viewport.bottom())
        } else {
            0.0
        };
        if overshoot != 0.0 {
            let rows = f64::from(overshoot.clamp(-60.0, 60.0)) / f64::from(self.row_px(list));
            self.update_model(window, cx, |model| model.scroll_list_by(list, rows));
        }
        let column = self
            .selection
            .map_or(Column::Whole, |selection| selection.column);
        if let Some(pos) = self.text_pos(pane, column, event.position)
            && self
                .selection
                .is_some_and(|selection| selection.pane == pane && selection.head != pos)
        {
            self.update_model(window, cx, |model| model.drag_text(pane, pos));
        }
    }

    /// The button came up: a divider drag is remembered, and a dragged selection ends.
    pub(super) fn mouse_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let resized = std::mem::take(&mut self.resizing);
        if resized || self.dragging.is_some() {
            self.update_model(window, cx, |model| {
                if resized {
                    model.remember_layout();
                }
                model.release_text();
            });
        }
    }

    /// Starts dragging the divider, or resets the files' width on a double click.
    pub(super) fn press_divider(
        &mut self,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if click_count >= 2 {
            self.resizing = false;
            self.update_model(window, cx, Model::reset_sidebar_width);
        } else {
            self.resizing = true;
            cx.notify();
        }
    }
}
