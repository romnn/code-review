//! The side-by-side diff: each row a base line beside a head line, each half a fixed viewport
//! with its own gutter, and one text offset that every row's text shares.
//!
//! The list itself never scrolls sideways here: that would carry the gutters and the middle
//! divider off screen.
//! Instead, the code of every row (both halves, and nested lines across the width) moves left
//! by one offset inside its own clipped area, so line numbers, signs and the divider always
//! stay put.
//! The tool's own rows (hunk headers, gaps, notes and moves) move with the code only as far as
//! their text runs past its room, so one that fits never moves, what a hunk is and where a move
//! goes stay in view however far the code is panned, and a long note's end comes into view.
//! How far the code can move is the largest overflow of any half or full-width line of code or
//! of the tool's rows, and showing another file starts it back at the left edge.
//! The model keeps the offset in code cells ([`review_ui::frame::TextPan`]); the window measures
//! how far it can go.

use gpui::prelude::*;
use gpui::{AnyElement, Div, div, pattern_slash, px};
use gpui_component::h_flex;
use gpui_component::scroll::ScrollbarHandle as _;
use review_ui::paint::Palette;
use review_ui::paint::annotation::NotePan;
use review_ui::paint::code::{self, Frame};
use review_ui::rows::{Cell, Pair};
use review_ui::text::{Column, Pane};

use super::ReviewApp;
use super::measure::{DIVIDER, FRAME};
use super::render::frame_bar;
use crate::color::to_gpui;

impl ReviewApp {
    /// How far the diff's code has moved left, in pixels.
    pub(super) fn text_offset_px(&self) -> f32 {
        super::scroll::narrow(self.diff_text().offset() * f64::from(self.sizes().code_cell()))
    }

    /// `content`, the code of a full-width row, clipped to the room it has and moved by the
    /// diff's text offset, with `marker` over its right edge when the line runs past it.
    pub(super) fn shifted(&self, content: Div, marker: Option<Div>) -> Div {
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(content.h_full().ml(px(-self.text_offset_px())))
            .children(marker)
    }

    /// The marker over the right edge of a line of code `cells` wide, in `column` of the diff,
    /// when it runs past its room; on the line's opaque `background`.
    pub(super) fn diff_clip_marker(
        &self,
        cells: usize,
        column: Column,
        background: review_ui::color::Hsla,
        colors: &Palette,
    ) -> Option<Div> {
        let code_cell = self.sizes().code_cell();
        let list_width = f32::from(self.lists.handles.diff.viewport_bounds().size.width);
        // Before the list's first layout there is no room to measure.
        if list_width <= 0.0 {
            return None;
        }
        let columns = super::measure::gutter(code_cell) + super::measure::sign(code_cell);
        let room = match column {
            Column::Whole => list_width - FRAME - columns,
            Column::Left | Column::Right => (list_width - FRAME - DIVIDER) / 2.0 - columns,
        };
        let offset = self.diff_text().offset();
        code::clipped(cells, offset, f64::from(room) / f64::from(code_cell))
            .then(|| clip_marker(code_cell, background, colors))
    }

    /// `content`, the text of one of the tool's own rows, in the room up to the list's right
    /// edge: moved left with the code as `pan` says, with the clip marker over the right edge,
    /// on the row's opaque `background`, while some of it is still cut.
    pub(super) fn noted(
        &self,
        content: Div,
        pan: NotePan,
        background: review_ui::color::Hsla,
        colors: &Palette,
    ) -> Div {
        let code_cell = self.sizes().code_cell();
        let shift = super::scroll::narrow(pan.shift * f64::from(code_cell));
        let marker = pan.cut.then(|| clip_marker(code_cell, background, colors));
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(content.h_full().ml(px(-shift)))
            .children(marker)
    }

    /// `content`, the text of one of the tool's own rows that never runs past its room, such as
    /// a gap's count, clipped to the room it has; unchanged in the unified layout.
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
        let half =
            |cell: Option<&Cell>, column| self.half(index, file, cell, column, frame, colors);
        let left = half(pair.left.as_ref(), Column::Left);
        let right = half(pair.right.as_ref(), Column::Right);
        h_flex()
            .w_full()
            .h(px(self.sizes().row_height()))
            .whitespace_nowrap()
            .when_some(frame.band(colors).map(to_gpui), gpui::Styled::bg)
            .child(frame_bar(frame, colors))
            .child(left)
            .child(
                div()
                    .w(px(DIVIDER))
                    .h_full()
                    .flex_none()
                    .bg(to_gpui(colors.hairline)),
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
        frame: Frame,
        colors: &Palette,
    ) -> Div {
        let base = div().flex_1().min_w_0().h_full().flex().items_center();
        let Some(cell) = cell else {
            return base.bg(pattern_slash(to_gpui(colors.hairline), 1.0, 6.0));
        };
        let (parts, layout) = self.code_cell(index, file, cell, column, colors);
        self.painted
            .borrow_mut()
            .insert((Pane::Diff, column, index), layout);
        // The half's own background lies over the row's band.
        let behind = line_background(colors, &[frame.band(colors), parts.painted_background]);
        let marker = self.diff_clip_marker(cell.width(), column, behind, colors);
        base.when_some(parts.background, gpui::Styled::bg)
            .when_some(parts.text_color, gpui::Styled::text_color)
            .child(parts.gutter)
            .child(parts.sign)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .child(
                        h_flex()
                            .ml(px(-self.text_offset_px()))
                            .child(parts.text)
                            .children(parts.blank),
                    )
                    .children(marker),
            )
    }
}

/// The opaque color behind a line of code: `layers` (such as its hunk's band and its own
/// background) laid on the editor background in order, as gpui blends them.
pub(super) fn line_background(
    colors: &Palette,
    layers: &[Option<review_ui::color::Hsla>],
) -> review_ui::color::Hsla {
    layers
        .iter()
        .flatten()
        .fold(colors.background.to_rgb(), |below, &color| {
            review_ui::paint::composite(below, color)
        })
        .into()
}

/// The clip marker over the last cell of a line of code, code cells `code_cell` pixels wide,
/// on the line's opaque `background`.
pub(super) fn clip_marker(
    code_cell: f32,
    background: review_ui::color::Hsla,
    colors: &Palette,
) -> Div {
    let marker = code::clip_marker(colors);
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(code_cell.round()))
        .flex()
        .items_center()
        .justify_center()
        .bg(to_gpui(background))
        .text_color(to_gpui(marker.color))
        .child(marker.text)
}
