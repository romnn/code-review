//! The side-by-side diff: each row a base line beside a head line, each half a fixed viewport
//! with its own gutter, and one text offset that every row's text shares.
//!
//! The list itself never scrolls sideways here: that would carry the gutters and the middle
//! divider off screen.
//! Instead, the code of every row (both halves, and nested lines across the width) moves left
//! by one offset inside its own clipped area, so line numbers, signs and the divider always
//! stay put.
//! The tool's own rows (hunk headers, gaps, notes and moves) never move: they are clipped
//! where they run out of room, so what a hunk is and where a move goes stay in view however
//! far the code is panned.
//! How far the code can move is the largest overflow of any half or full-width line of code,
//! and showing another file starts it back at the left edge.

use std::cell::Cell as Shared;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, Context, Div, Pixels, Point, ScrollWheelEvent, Size, Window, div,
    pattern_slash, point, px,
};
use gpui_component::h_flex;
use gpui_component::scroll::ScrollbarHandle;

use super::present::{DIVIDER, FRAME, GUTTER, SIGN};
use super::render::{Frame, GRID, Palette, frame_bar};
use super::text::{Column, Pane};
use super::{ReviewApp, scroll};
use crate::config::DiffLayout;
use crate::rows::{Cell, Pair};

/// The horizontal text offset of the side-by-side diff, shared with its scrollbar.
#[derive(Clone, Default)]
pub struct TextScroll {
    /// How far the text has moved left, in pixels.
    offset: Rc<Shared<f32>>,
    /// The most it may move.
    extent: Rc<Shared<f32>>,
    /// The diff list's bounds, where the scrollbar draws.
    viewport: Rc<Shared<Bounds<Pixels>>>,
}

impl TextScroll {
    /// How far the text has moved left.
    #[must_use]
    pub fn offset(&self) -> f32 {
        self.offset.get()
    }

    /// How far the text can move.
    #[must_use]
    pub fn extent(&self) -> f32 {
        self.extent.get()
    }

    /// Moves the text by `delta` pixels, within the extent.
    pub fn pan(&self, delta: f32) {
        self.offset
            .set((self.offset.get() + delta).clamp(0.0, self.extent.get()));
    }

    /// Takes the extent and the viewport of this frame, keeping the offset within it.
    pub fn update(&self, extent: f32, viewport: Bounds<Pixels>) {
        self.extent.set(extent.max(0.0));
        self.viewport.set(viewport);
        self.clamp_to_extent();
    }

    /// Moves the text back to the left edge, as when another file shows.
    pub fn reset(&self) {
        self.offset.set(0.0);
    }

    /// Keeps the offset within the extent, as after the rows or their metrics changed.
    pub fn clamp_to_extent(&self) {
        self.offset
            .set(self.offset.get().clamp(0.0, self.extent.get()));
    }
}

impl ScrollbarHandle for TextScroll {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.viewport.get()
    }

    fn offset(&self) -> Point<Pixels> {
        point(px(-self.offset.get()), px(0.0))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.offset
            .set((-f32::from(offset.x)).clamp(0.0, self.extent.get()));
    }

    fn content_size(&self) -> Size<Pixels> {
        let viewport = self.viewport.get().size;
        Size {
            width: viewport.width + px(self.extent.get()),
            height: viewport.height,
        }
    }
}

impl ReviewApp {
    /// Whether the diff shows side by side now, rather than only being asked to.
    pub(super) fn side_by_side(&self) -> bool {
        self.rows.presentation.layout == DiffLayout::SideBySide
    }

    /// `content`, the code of a full-width row, clipped to the room it has and moved by the
    /// shared text offset; unchanged in the unified layout, whose list scrolls sideways as a
    /// whole.
    pub(super) fn shifted(&self, content: Div) -> Div {
        if !self.side_by_side() {
            return content;
        }
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(content.h_full().ml(px(-self.text_scroll.offset())))
    }

    /// `content`, the text of one of the tool's own rows, clipped to the room it has but never
    /// moved with the code; unchanged in the unified layout.
    pub(super) fn clipped(&self, content: Div) -> Div {
        if !self.side_by_side() {
            return content;
        }
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(content.h_full())
    }

    /// A paired row: the frame bar, the base half, the divider and the head half.
    pub(super) fn pair_row(
        &self,
        index: usize,
        file: usize,
        pair: &Pair,
        frame: Frame,
        colors: &Palette,
    ) -> AnyElement {
        let half = |cell: Option<&Cell>, column| self.half(index, file, cell, column, colors);
        let left = half(pair.left.as_ref(), Column::Left);
        let right = half(pair.right.as_ref(), Column::Right);
        h_flex()
            .w_full()
            .h(px(self.fonts.row_height()))
            .whitespace_nowrap()
            .child(frame_bar(frame, colors))
            .child(left)
            .child(
                div()
                    .w(px(DIVIDER))
                    .h_full()
                    .flex_none()
                    .bg(colors.hairline),
            )
            .child(right)
            .into_any_element()
    }

    /// One half of a paired row: a line's gutter, sign and text on its background, or padding
    /// where the side has no line.
    fn half(
        &self,
        index: usize,
        file: usize,
        cell: Option<&Cell>,
        column: Column,
        colors: &Palette,
    ) -> Div {
        let base = div().flex_1().min_w_0().h_full().flex().items_center();
        let Some(cell) = cell else {
            return base.bg(pattern_slash(colors.hairline, 1.0, 6.0));
        };
        let (parts, layout) = self.code_cell(index, file, cell, column, colors);
        self.painted
            .borrow_mut()
            .insert((Pane::Diff, column, index), layout);
        base.when_some(parts.background, gpui::Styled::bg)
            .when_some(parts.text_color, gpui::Styled::text_color)
            .child(parts.gutter)
            .child(parts.sign)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .child(
                        h_flex()
                            .ml(px(-self.text_scroll.offset()))
                            .child(parts.text)
                            .children(parts.blank),
                    ),
            )
    }

    /// How far the side-by-side code can move: the largest overflow of a half or a full-width
    /// line of code, at the window's cell widths.
    pub(super) fn text_extent(&self, window: &Window, list_width: f32) -> f32 {
        let cell = |size: f32| {
            let system = window.text_system();
            system
                .em_advance(system.resolve_font(&self.fonts.code), px(size))
                .map_or(size * 0.6, f32::from)
        };
        let code_cell = cell(self.fonts.code_size);
        let lead = FRAME + GUTTER + SIGN;
        let half = (list_width - FRAME - DIVIDER) / 2.0 - GUTTER - SIGN;
        let full = list_width - lead;
        let overflow = |cells: usize, cell: f32, room: f32| scroll::float(cells) * cell - room;
        let halves = overflow(self.rows.widest_half, code_cell, half);
        let code = self
            .rows
            .widest_code
            .map_or(0.0, |(_, cells)| overflow(cells, code_cell, full));
        // A little room past the longest line, as an editor leaves it.
        (halves.max(code) + GRID * 4.0).max(0.0)
    }

    /// Wheel input over the diff stops its scroll animation; sideways input over the side-by-side
    /// diff, or Shift with the wheel, moves the text.
    pub(super) fn text_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The wheel takes over from a scroll animation, which would otherwise pull the diff
        // back to its target on the next frames.
        self.list_animation = None;
        self.list_bar.cancel_page_request();
        if !self.side_by_side() {
            return;
        }
        let delta = event.delta.pixel_delta(px(self.fonts.row_height()));
        let sideways = if delta.x == px(0.0) && event.modifiers.shift {
            delta.y
        } else {
            delta.x
        };
        if sideways != px(0.0) {
            self.text_scroll.pan(-f32::from(sideways));
            cx.notify();
        }
    }
}
