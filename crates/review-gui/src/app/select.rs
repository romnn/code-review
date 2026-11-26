//! The mouse over text: finding the text under the pointer in the diff and the buffer for the
//! model's selection, and dragging the divider between the files and the diff and the edge under
//! the comparison.

use std::time::Instant;

use gpui::{
    Context, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Point, Window, point, px,
};
use gpui_component::scroll::ScrollbarHandle as _;
use review_ui::model::{Hit, Model, TextPress, list_of};
use review_ui::rows::Body;
use review_ui::text::{Column, Pane, TextPos};

use super::measure::{DIVIDER, FRAME};
use super::{ReviewApp, scroll};

/// An edge the mouse drags to resize a pane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Edge {
    /// The divider between the files and the diff.
    Divider,
    /// The edge under the comparison, which shows at `height` while it drags and is remembered
    /// only once it ends away from the height it started at.
    Comparison {
        /// Where the comparison starts, in logical pixels from the window's top.
        top: f32,
        /// The comparison's height when the drag started.
        pressed: f32,
        /// Its height now.
        height: f32,
    },
}

impl Edge {
    /// The comparison height to remember once the drag ends: none when the comparison ends
    /// up as high as it started, to the whole pixel remembered, as when a drag only moves
    /// sideways or a short window holds it at its largest, so a height remembered for a taller
    /// window stays.
    fn changed_height(self) -> Option<f32> {
        match self {
            Self::Comparison {
                pressed, height, ..
            } if (height - pressed).abs() >= 0.5 => Some(height),
            _ => None,
        }
    }
}

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
    /// `column`, and the screen row it is on.
    pub(super) fn text_pos(
        &self,
        pane: Pane,
        column: Column,
        position: Point<Pixels>,
    ) -> Option<(TextPos, usize)> {
        let list = list_of(pane);
        let last_row = self.list_rows(list).checked_sub(1)?;
        let viewport = self.lists.handle(list).viewport_bounds();
        let y = f32::from(position.y - viewport.top()) + self.offset_px(list);
        let row = scroll::whole(y / self.sizes().row_height()).min(last_row);
        // A pair's line is painted in its half; any other row across the whole width.
        let painted_in = match self.rows.row_on_screen(row).map(|row| &row.body) {
            Some(Body::Pair(_)) if pane == Pane::Diff => column,
            _ => Column::Whole,
        };
        // The byte of the painted text under the pointer; the layout is of the shown text,
        // whose placeholders and tabs are longer than what they stand for.
        let byte = self
            .painted
            .borrow()
            .get(&(pane, painted_in, row))
            .map(|painted| {
                let bounds = painted.layout.bounds();
                let inside = point(position.x, bounds.center().y);
                match painted.layout.index_for_position(inside) {
                    Ok(index) | Err(index) => painted.line.byte_at_shown(index),
                }
            });
        let above = position.y < viewport.top();
        let pos = match pane {
            Pane::Diff => {
                let hit = match byte {
                    Some(byte) => Hit::Byte(byte),
                    None if above => Hit::Start,
                    None => Hit::End,
                };
                self.diff_pos(row, column, hit)?
            }
            Pane::Source => {
                let len = self.copyable(pane, column, row).map_or(0, str::len);
                let col = match byte {
                    Some(byte) => byte,
                    None if above => 0,
                    None => len,
                };
                TextPos {
                    row,
                    col: col.min(len),
                }
            }
        };
        Some((pos, row))
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
        let Some((pos, screen)) = self.text_pos(pane, column, event.position) else {
            return;
        };
        let press = TextPress {
            pane,
            column,
            pos,
            screen,
            clicks: event.click_count,
            extend: event.modifiers.shift,
        };
        self.update_model(window, cx, |model| model.press_text(press));
    }

    /// Drags an edge, or extends a drag selection, scrolling when the mouse leaves the pane.
    pub(super) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edge) = self.resizing {
            if event.pressed_button != Some(MouseButton::Left) {
                self.resizing = None;
                self.update_model(window, cx, |model| remember(model, edge));
                return;
            }
            match edge {
                Edge::Divider => {
                    let width = review_ui::panes::sidebar_width(
                        review_ui::panes::SidebarWidth::Set(f32::from(event.position.x)),
                        0.0,
                        f32::from(window.viewport_size().width),
                        self.sidebar_cell(),
                    );
                    self.update_model(window, cx, |model| model.set_sidebar_width(width));
                }
                Edge::Comparison { top, pressed, .. } => {
                    let height =
                        self.comparison_height(Some(f32::from(event.position.y) - top), window);
                    self.resizing = Some(Edge::Comparison {
                        top,
                        pressed,
                        height,
                    });
                    cx.notify();
                }
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
        if let Some((pos, _)) = self.text_pos(pane, column, event.position)
            && self
                .selection
                .is_some_and(|selection| selection.pane == pane && selection.head != pos)
        {
            self.update_model(window, cx, |model| model.drag_text(pane, pos));
        }
    }

    /// The button came up: an edge's drag is remembered, and a dragged selection ends.
    pub(super) fn mouse_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let resized = self.resizing.take();
        if resized.is_some() || self.dragging.is_some() {
            self.update_model(window, cx, |model| {
                if let Some(edge) = resized {
                    remember(model, edge);
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
            self.resizing = None;
            self.update_model(window, cx, Model::reset_sidebar_width);
        } else {
            self.resizing = Some(Edge::Divider);
            cx.notify();
        }
    }

    /// Starts dragging the edge under the comparison, pressed at `y` in the window while the
    /// comparison is `height` high.
    pub(super) fn press_comparison_edge(&mut self, y: f32, height: f32, cx: &mut Context<Self>) {
        self.resizing = Some(Edge::Comparison {
            top: y - height,
            pressed: height,
            height,
        });
        cx.notify();
    }

    /// The comparison's height in `window`: `wanted` when its edge was dragged, or else its
    /// share of the window, within what the window leaves it beside the diff's least rows.
    pub(super) fn comparison_height(&self, wanted: Option<f32>, window: &Window) -> f32 {
        let height = f32::from(window.viewport_size().height);
        let sizes = self.sizes();
        review_ui::panes::comparison_height(
            wanted,
            height,
            sizes.comparison_room(height, self.client_decorated),
            sizes.bar(),
            sizes.comparison_rows(),
        )
    }

    /// Whether the comparison's edge has anywhere to go in `window`: not when the window leaves
    /// the comparison and the diff only their least heights.
    pub(super) fn comparison_drags(&self, window: &Window) -> bool {
        let height = f32::from(window.viewport_size().height);
        let sizes = self.sizes();
        let (least, most) = review_ui::panes::comparison_bounds(
            sizes.comparison_room(height, self.client_decorated),
            sizes.bar(),
            sizes.comparison_rows(),
        );
        most - least >= 1.0
    }
}

/// Remembers the size `edge` set, once its drag ends.
fn remember(model: &mut Model, edge: Edge) {
    match edge {
        Edge::Divider => model.remember_layout(),
        Edge::Comparison { .. } => {
            if let Some(height) = edge.changed_height() {
                model.remember_comparison_height(height);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drag of the comparison's edge is remembered only when it changed the height, so one
    /// that a short window held at its largest keeps the taller height remembered before.
    #[test]
    fn only_a_changed_comparison_height_is_remembered() {
        let drag = |pressed, height| Edge::Comparison {
            top: 67.0,
            pressed,
            height,
        };
        assert_eq!(drag(450.0, 450.0).changed_height(), None);
        assert_eq!(drag(450.0, 450.2).changed_height(), None);
        assert_eq!(drag(450.0, 520.0).changed_height(), Some(520.0));
        assert_eq!(Edge::Divider.changed_height(), None);
    }
}
