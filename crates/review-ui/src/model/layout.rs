//! The layout phase that starts every frame, and the scroll input of each list, in rows.
//!
//! Every list's offset lives in the model as a [`ListScroll`].
//! What scrolls a list goes through here: the wheel, scrollbar drags and track clicks, and the
//! requests and eased scrolls the review's keys start.
//! [`Model::layout`] runs once at the start of each frame with the geometry the front end
//! measured, and does what waits for it: following the diff's width, taking track clicks'
//! pages, stepping eased scrolls, keeping each shown list within its rows, and resolving the
//! requests that wait for a list's viewport.
//! Between frames, the model works out its grid from that geometry at its metrics, which a
//! zoom or a font size's preview may have changed since.

use std::sync::Arc;
use std::time::Instant;

use super::present::Anchor;
use super::{Mode, Model};
use crate::frame::{
    self, Geometry, Grid, List, ListScroll, Metrics, PAN_SLACK, Sizes, TextPan, Wheel, pan_extent,
};
use crate::paint::display::display_width;
use crate::rows::{self, WrapRoom};
use crate::scroll;

impl Model {
    /// What the geometry depends on now.
    #[must_use]
    pub fn metrics(&self) -> Metrics {
        Metrics {
            code_size: self.code_size,
            density: self.look().density,
            panel: self
                .settings_panel
                .as_ref()
                .map(|panel| panel.text_code_size),
        }
    }

    /// The sizes the front end draws in at the metrics now.
    #[must_use]
    pub(crate) fn sizes(&self) -> Sizes {
        self.scale.sizes(&self.metrics())
    }

    /// The frame the front end measured last, which the grid is worked out from.
    #[must_use]
    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// The grid at the metrics now, from the geometry of the last frame.
    #[must_use]
    pub fn grid(&self) -> Grid {
        self.grid_in(&self.geometry)
    }

    /// The grid at the metrics now in `geometry`, as a front end checks whether the frame it
    /// laid out came out as the layout phase expected.
    #[must_use]
    pub fn grid_in(&self, geometry: &Geometry) -> Grid {
        geometry.grid(&self.scale, &self.metrics())
    }

    /// The layout phase of a frame the front end laid out in `geometry`, at `now`; see the
    /// module documentation.
    ///
    /// Returns whether an eased scroll still runs, so the next frame follows.
    #[tracing::instrument(skip_all)]
    pub fn layout(&mut self, geometry: Geometry, now: Instant) -> bool {
        let buffer_room = self.buffer_room();
        self.geometry = geometry;
        let grid = self.grid();
        // A resize, the sidebar or the screen mode can make the diff too narrow for side by
        // side, or wide enough again.
        if self.presentation_in(&grid) != self.presented {
            self.transition(|_| {});
        }
        self.follow_wrap();
        // How far the code can move follows the rows the diff shows, the tool's own among them,
        // and the buffer's lines.
        if self.list_shown(List::Diff) {
            let sizes = self.sizes();
            // Wrapped code never runs past its room; only the tool's own rows still pan.
            let wrapped = self.rows.wrapped.is_some();
            let widest_code = self
                .rows
                .widest_code
                .filter(|_| !wrapped)
                .map(|(_, cells)| cells);
            let widest_half = if wrapped { 0 } else { self.rows.widest_half };
            let notes = self.rows.widest_annotation.map_or(0.0, |(_, cells)| {
                self.geometry.annotation_overflow(&sizes, cells)
            });
            let notes = if notes > 0.0 { notes + PAN_SLACK } else { 0.0 };
            if self.side_by_side() {
                let extent = self.geometry.text_extent(&sizes, widest_half, widest_code);
                self.text.set_extent(extent.max(notes));
            } else {
                let room = self.code_room();
                self.unified_text
                    .set_extent(pan_extent(widest_code.unwrap_or(0), room).max(notes));
            }
        }
        if self.list_shown(List::Buffer) {
            // A cursor in view stays in view when its room changes under it: a resize, a
            // dragged divider, or a files pane following its tree as files come and go. One
            // the reviewer panned away from stays where they left it.
            let resized = (self.buffer_room() - buffer_room).abs() > f64::EPSILON;
            if self.buffer_column_waits || (resized && self.buffer_cursor_shown(buffer_room)) {
                self.reveal_buffer_column();
            } else {
                self.measure_buffer_text();
            }
        }
        let mut running = false;
        for list in List::ALL {
            let count = self.list_rows(list);
            let shown = self.list_shown(list);
            let viewport = grid.viewports.of(list);
            if let Some(scroll) = self.list_scroll_mut(list) {
                running |= scroll.lay_out(count, viewport, shown, now);
            }
        }
        if self.list_shown(List::Diff) {
            self.follow_view();
        }
        running
    }

    /// The rows `list` has now.
    #[must_use]
    pub fn list_rows(&self, list: List) -> usize {
        match list {
            List::Diff => self.rows.screen.len(),
            List::Buffer => self
                .buffer
                .as_ref()
                .map_or(0, |buffer| buffer.lines.lines.len()),
            List::Tree => self.visible_tree().len(),
            List::Picker => self
                .picker
                .as_ref()
                .map_or(0, |picker| picker.entries.len()),
            List::Settings => self.settings_line_count(),
        }
    }

    /// Whether this frame shows `list`.
    #[must_use]
    pub fn list_shown(&self, list: List) -> bool {
        let buffer_shown = self.mode == Mode::Buffer && self.buffer.is_some();
        match list {
            List::Diff => !buffer_shown,
            List::Buffer => buffer_shown,
            List::Tree => self.sidebar_shown(),
            List::Picker => self.picker.is_some(),
            List::Settings => self.settings_panel.is_some(),
        }
    }

    /// Where `list` is, while it exists.
    #[must_use]
    pub fn list_scroll(&self, list: List) -> Option<&ListScroll> {
        match list {
            List::Diff => Some(&self.diff_scroll),
            List::Buffer => Some(&self.buffer_scroll),
            List::Tree => Some(&self.tree_scroll),
            List::Picker => self.picker.as_ref().map(|picker| &picker.scroll),
            List::Settings => self.settings_panel.as_ref().map(|panel| &panel.scroll),
        }
    }

    fn list_scroll_mut(&mut self, list: List) -> Option<&mut ListScroll> {
        match list {
            List::Diff => Some(&mut self.diff_scroll),
            List::Buffer => Some(&mut self.buffer_scroll),
            List::Tree => Some(&mut self.tree_scroll),
            List::Picker => self.picker.as_mut().map(|picker| &mut picker.scroll),
            List::Settings => self.settings_panel.as_mut().map(|panel| &mut panel.scroll),
        }
    }

    /// Scrolls `list` by `delta` rows at once, within its rows: the wheel, and a selection
    /// dragged past the list's edge.
    pub fn scroll_list_by(&mut self, list: List, delta: f64) {
        let count = self.list_rows(list);
        let viewport = self.grid().viewports.of(list);
        if let Some(scroll) = self.list_scroll_mut(list) {
            scroll.scroll_by(delta, count, viewport);
        }
    }

    /// The wheel over `list`: it takes over from an eased scroll, which would otherwise pull
    /// the list back to its target on the next frames, and scrolls by its rows, further as a
    /// mouse wheel keeps turning fast.
    /// In the diff, the selected hunk follows the view.
    pub fn wheel_list(&mut self, list: List, wheel: Wheel) {
        if !self.settings.mouse.scroll {
            return;
        }
        if let Some(scroll) = self.list_scroll_mut(list) {
            scroll.stop();
        }
        let delta = match wheel {
            Wheel::Notches { rows, at } => self.wheel_momentum.turn(list, rows, at),
            Wheel::Precise(rows) => {
                self.wheel_momentum.stop();
                rows
            }
        };
        if delta != 0.0 {
            self.scroll_list_by(list, delta);
        }
        if list == List::Diff {
            self.follow_view();
        }
    }

    /// A scrollbar's thumb dragged, or a short list's track clicked: `list` at `offset` rows,
    /// within its rows. In the diff, the selected hunk follows the view.
    pub fn drag_list_to(&mut self, list: List, offset: f64) {
        if !self.settings.mouse.scroll {
            return;
        }
        let count = self.list_rows(list);
        let viewport = self.grid().viewports.of(list);
        if let Some(scroll) = self.list_scroll_mut(list) {
            scroll.scroll_to(offset, count, viewport);
        }
        if list == List::Diff {
            self.follow_view();
        }
    }

    /// A click on the track of `list`'s scrollbar toward `toward` rows: a page that way.
    pub fn page_list_toward(&mut self, list: List, toward: f64) {
        if !self.settings.mouse.scroll {
            return;
        }
        let count = self.list_rows(list);
        let grid = self.grid();
        let viewport = grid.viewports.of(list);
        if let Some(scroll) = self.list_scroll_mut(list) {
            scroll.page_toward(toward, count, viewport, grid.page_context);
        }
    }

    /// The code offset of the diff as it shows: the halves' side by side, else the unified
    /// lines'.
    #[must_use]
    pub fn diff_text(&self) -> &TextPan {
        if self.side_by_side() {
            &self.text
        } else {
            &self.unified_text
        }
    }

    /// How far the diff's code has moved left, in code cells: none while long lines wrap,
    /// though the tool's own rows still pan by [`Self::diff_text`].
    #[must_use]
    pub fn code_offset(&self) -> f64 {
        if self.rows.wrapped.is_some() {
            0.0
        } else {
            self.diff_text().offset()
        }
    }

    /// The room long lines of the diff wrap at now: `None` unless they wrap, and until the
    /// diff has been laid out.
    pub(super) fn wrap_room(&self) -> Option<WrapRoom> {
        if !self.look().wrap || self.geometry.diff_list <= 0.0 {
            return None;
        }
        let sizes = self.sizes();
        let cells = |room: f64| scroll::whole_rows(room).max(1);
        Some(WrapRoom {
            whole: cells(self.geometry.code_room(&sizes)),
            half: cells(self.geometry.half_room(&sizes)),
        })
    }

    /// Lays the diff's rows out on screen again when the room its long lines wrap at changed,
    /// or whether they wrap, keeping the text at the top where it is: the same byte of the same
    /// line, whichever screen row now shows it.
    ///
    /// The rows themselves stay, and with them a selection, the find matches and the search's
    /// place, which all name text rather than screen rows.
    pub(super) fn follow_wrap(&mut self) {
        let room = self.wrap_room();
        if room == self.rows.wrapped {
            return;
        }
        let top = Anchor::at(self.diff_scroll.offset());
        // The top's row, and the side and byte of its line that the screen row starts at.
        let place = self.rows.screen.get(top.row).map(|piece| {
            let shown = piece
                .shown
                .iter()
                .enumerate()
                .find_map(|(side, shown)| shown.as_ref().map(|shown| (side, shown.start)));
            (piece.row, shown)
        });
        // Motion aimed at the old screen rows stops.
        self.diff_scroll.stop();
        rows::project(Arc::make_mut(&mut self.rows), room);
        self.hunk_rows = super::hunk_rows(&self.rows);
        let screen = place.map_or(0, |(row, shown)| match shown {
            Some((side, byte)) => self.rows.screen_row_of(row, side, byte),
            None => self.rows.first_on_screen(row),
        });
        let viewport = self.grid().viewports.diff;
        self.diff_scroll.jump(frame::clamp(
            top.offset(screen),
            self.rows.screen.len(),
            viewport,
        ));
    }

    fn diff_text_mut(&mut self) -> &mut TextPan {
        if self.side_by_side() {
            &mut self.text
        } else {
            &mut self.unified_text
        }
    }

    /// Moves the diff's code sideways by `delta` cells, right when positive: the wheel turned
    /// sideways over the diff.
    pub fn pan_text(&mut self, delta: f64) {
        if !self.settings.mouse.scroll {
            return;
        }
        self.diff_text_mut().pan(delta);
    }

    /// Moves the code of the diff or, in the buffer, the buffer's code sideways by `delta`
    /// cells, as the pan keys do, whatever the mouse may do.
    fn pan_by(&mut self, delta: f64) {
        if self.mode == Mode::Buffer {
            self.measure_buffer_text();
            self.buffer_text.pan(delta);
        } else {
            self.diff_text_mut().pan(delta);
        }
    }

    /// The diff's horizontal scrollbar dragged: its code `offset` cells left.
    pub fn set_text_offset(&mut self, offset: f64) {
        if !self.settings.mouse.scroll {
            return;
        }
        self.diff_text_mut().set(offset);
    }

    /// Moves the buffer's code sideways by `delta` cells, right when positive: the wheel
    /// turned sideways over the buffer.
    pub fn pan_buffer(&mut self, delta: f64) {
        if !self.settings.mouse.scroll {
            return;
        }
        self.measure_buffer_text();
        self.buffer_text.pan(delta);
    }

    /// The buffer's horizontal scrollbar dragged: its code `offset` cells left.
    pub fn set_buffer_offset(&mut self, offset: f64) {
        if !self.settings.mouse.scroll {
            return;
        }
        self.measure_buffer_text();
        self.buffer_text.set(offset);
    }

    /// The pan keys: the code of the diff or the buffer half its room further right
    /// (`forward`) or left, as vim's `zL` and `zH` move half a screen.
    pub(super) fn pan(&mut self, forward: bool) {
        let room = match self.mode {
            Mode::Buffer if self.buffer.is_some() => self.buffer_room(),
            Mode::Buffer => return,
            _ if self.side_by_side() => f64::from(self.grid().half_cells),
            _ => self.code_room(),
        };
        let step = (room / 2.0).floor().max(1.0);
        let delta = if forward { step } else { -step };
        self.pan_by(delta);
    }

    /// The code cells a line across the diff's width has room for.
    #[must_use]
    pub fn code_room(&self) -> f64 {
        self.geometry.code_room(&self.sizes())
    }

    /// The code cells a line of the open buffer has room for.
    #[must_use]
    pub fn buffer_room(&self) -> f64 {
        let lines = self.list_rows(List::Buffer);
        self.geometry.buffer_room(&self.sizes(), lines)
    }

    /// Takes how far the open buffer's code can move from its longest line, keeping the offset
    /// within it.
    pub(super) fn measure_buffer_text(&mut self) {
        let widest = self
            .buffer
            .as_ref()
            .and_then(|buffer| buffer.lines.lines.get(buffer.lines.longest))
            .map_or(0, |line| display_width(line));
        let room = self.buffer_room();
        self.buffer_text.set_extent(pan_extent(widest, room));
    }
}
