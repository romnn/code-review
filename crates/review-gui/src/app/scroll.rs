//! The gpui side of scrolling.
//!
//! The model owns every list's offset in rows, and the code offsets of the diff and the buffer
//! in cells; see [`review_ui::frame`].
//! Here are the gpui handles that lay the lists out, which mirror the model's offsets each frame
//! and are never read for them; the scrollbars, which ask the model to scroll instead of moving
//! a handle; and the layer that takes the wheel over a list before gpui's own scrolling would.
//! The scroll arithmetic itself is [`review_ui::scroll`].

use std::cell::Cell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Bounds, Canvas, Context, DispatchPhase, Hitbox, HitboxBehavior, IsZero as _, Pixels,
    Point, ScrollWheelEvent, Size, UniformListScrollHandle, WeakEntity, Window, canvas, point, px,
};
use gpui_component::scroll::ScrollbarHandle;
pub use review_ui::scroll::{float, narrow, rows, whole};

use super::ReviewApp;
use super::measure::{Handles, PanelSizes};
use review_ui::frame::{List, ListScroll};
use review_ui::model::Model;

/// What a scrollbar asked of the model, in its rows and cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollbarMove {
    /// A list's thumb dragged, or its track clicked where a click jumps: the list at `rows`.
    To { list: List, rows: f64 },
    /// A list's track clicked toward `toward` rows, where a click pages.
    Page { list: List, toward: f64 },
    /// The diff's horizontal thumb dragged: its code `cells` left.
    Text { cells: f64 },
    /// The buffer's horizontal thumb dragged: its code `cells` left.
    BufferText { cells: f64 },
}

impl ScrollbarMove {
    /// Hands the move to `model`.
    pub fn apply(self, model: &mut Model) {
        match self {
            Self::To { list, rows } => model.drag_list_to(list, rows),
            Self::Page { list, toward } => model.page_list_toward(list, toward),
            Self::Text { cells } => model.set_text_offset(cells),
            Self::BufferText { cells } => model.set_buffer_offset(cells),
        }
    }
}

/// What a scrollbar asked of its list for the next frame, as an offset in pixels from the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BarRequest {
    /// The thumb was dragged here, or the track clicked where a click jumps.
    To(f32),
    /// The track was clicked toward here, where a click pages.
    Page(f32),
}

/// A list's scrollbar handle: it shows the list's mirrored offset, and a drag or a click on the
/// track becomes a request the model takes in the next frame, instead of moving the list.
#[derive(Clone)]
pub struct ListBar {
    list: UniformListScrollHandle,
    /// Whether a click on the track pages, rather than jumping there.
    paged: bool,
    dragging: Rc<Cell<bool>>,
    request: Rc<Cell<Option<BarRequest>>>,
}

impl ListBar {
    /// The scrollbar of `list`, whose track clicks page when `paged`.
    pub fn new(list: &UniformListScrollHandle, paged: bool) -> Self {
        Self {
            list: list.clone(),
            paged,
            dragging: Rc::default(),
            request: Rc::default(),
        }
    }

    /// Takes what the scrollbar asked for since the last frame.
    pub fn take_request(&self) -> Option<BarRequest> {
        self.request.take()
    }
}

impl ScrollbarHandle for ListBar {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.list.viewport_bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        ScrollbarHandle::offset(&self.list)
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let current = ScrollbarHandle::offset(&self.list);
        // The model scrolls lists only up and down; sideways the list scrolls itself.
        if offset.x != current.x {
            ScrollbarHandle::set_offset(&self.list, point(offset.x, current.y));
        }
        if offset.y == current.y {
            return;
        }
        let to = -f32::from(offset.y);
        self.request
            .set(Some(if self.dragging.get() || !self.paged {
                BarRequest::To(to)
            } else {
                BarRequest::Page(to)
            }));
    }

    fn content_size(&self) -> Size<Pixels> {
        self.list.content_size()
    }

    fn start_drag(&self) {
        self.dragging.set(true);
    }

    fn end_drag(&self) {
        self.dragging.set(false);
    }
}

/// A horizontal scrollbar of code that pans, the diff's or the buffer's: it shows the model's
/// text offset, mirrored each frame, and a drag becomes a request the model takes in the next
/// frame.
#[derive(Clone, Default)]
pub struct TextBar {
    /// How far the text has moved left, in pixels.
    offset: Rc<Cell<f32>>,
    /// How far it can move, in pixels.
    extent: Rc<Cell<f32>>,
    /// The list's bounds, where the scrollbar draws.
    viewport: Rc<Cell<Bounds<Pixels>>>,
    request: Rc<Cell<Option<f32>>>,
}

impl TextBar {
    /// Shows the text `offset` pixels left within `extent`, over a list at `viewport`.
    pub fn mirror(&self, offset: f32, extent: f32, viewport: Bounds<Pixels>) {
        self.offset.set(offset);
        self.extent.set(extent);
        self.viewport.set(viewport);
    }

    /// Takes the offset in pixels a drag asked for since the last frame.
    pub fn take_request(&self) -> Option<f32> {
        self.request.take()
    }
}

impl ScrollbarHandle for TextBar {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.viewport.get()
    }

    fn offset(&self) -> Point<Pixels> {
        point(px(-self.offset.get()), px(0.0))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        self.request.set(Some(-f32::from(offset.x)));
    }

    fn content_size(&self) -> Size<Pixels> {
        let viewport = self.viewport.get().size;
        Size {
            width: viewport.width + px(self.extent.get()),
            height: viewport.height,
        }
    }
}

/// The gpui handles of the lists the model scrolls, and their scrollbars.
pub struct Lists {
    /// The lists' handles, whose bounds each frame's geometry is measured from.
    pub handles: Handles,
    pub diff_bar: ListBar,
    pub buffer_bar: ListBar,
    pub tree_bar: ListBar,
    pub picker_bar: ListBar,
    pub settings_bar: ListBar,
    /// The diff's horizontal scrollbar.
    pub text_bar: TextBar,
    /// The buffer's horizontal scrollbar.
    pub buffer_text_bar: TextBar,
}

impl Lists {
    /// Fresh handles, scrolled to the top.
    pub fn new() -> Self {
        let handles = Handles {
            diff: UniformListScrollHandle::new(),
            buffer: UniformListScrollHandle::new(),
            tree: UniformListScrollHandle::new(),
            picker: UniformListScrollHandle::new(),
            settings: UniformListScrollHandle::new(),
        };
        Self {
            // The diff and the buffer page on a track click; the short lists jump there.
            diff_bar: ListBar::new(&handles.diff, true),
            buffer_bar: ListBar::new(&handles.buffer, true),
            tree_bar: ListBar::new(&handles.tree, false),
            picker_bar: ListBar::new(&handles.picker, false),
            settings_bar: ListBar::new(&handles.settings, false),
            text_bar: TextBar::default(),
            buffer_text_bar: TextBar::default(),
            handles,
        }
    }

    /// The gpui handle of `list`.
    pub fn handle(&self, list: List) -> &UniformListScrollHandle {
        let Handles {
            diff,
            buffer,
            tree,
            picker,
            settings,
        } = &self.handles;
        match list {
            List::Diff => diff,
            List::Buffer => buffer,
            List::Tree => tree,
            List::Picker => picker,
            List::Settings => settings,
        }
    }

    /// The scrollbar of `list`.
    pub fn bar(&self, list: List) -> &ListBar {
        match list {
            List::Diff => &self.diff_bar,
            List::Buffer => &self.buffer_bar,
            List::Tree => &self.tree_bar,
            List::Picker => &self.picker_bar,
            List::Settings => &self.settings_bar,
        }
    }

    /// Mirrors the model's offset of `list`, `offset` pixels from the top, into its handle.
    pub fn mirror(&self, list: List, offset: f32) {
        let handle = self.handle(list);
        let current = ScrollbarHandle::offset(handle);
        if current.y != px(-offset) {
            ScrollbarHandle::set_offset(handle, point(current.x, px(-offset)));
        }
    }
}

/// An invisible layer over `list` that takes the wheel before gpui's own scrolling of the list
/// would, and hands it to the model.
///
/// It goes in the list's container, beside the list and as large: it captures the wheel in
/// the capture phase where the list itself would scroll, so overlays that cover the list keep
/// the wheel as they do from the list, and stops it there, so gpui never scrolls the list.
/// The line height of the list's text, which a wheel turned in lines scrolls by, is the one
/// the layer is painted in.
pub fn wheel_layer(app: WeakEntity<ReviewApp>, list: List) -> Canvas<Hitbox> {
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |_, hitbox, window, _| {
            let line_height = window.line_height();
            window.on_mouse_event(
                move |event: &ScrollWheelEvent, phase, window: &mut Window, cx: &mut App| {
                    if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                        return;
                    }
                    cx.stop_propagation();
                    app.update(cx, |this, cx| {
                        this.wheel(list, event, line_height, window, cx);
                    })
                    .ok();
                },
            );
        },
    )
    .absolute()
    .inset_0()
}

impl ReviewApp {
    /// The height of a row of `list` in pixels: a code row, or the settings panel's own.
    pub(super) fn row_px(&self, list: List) -> f32 {
        match (list, &self.settings_panel) {
            (List::Settings, Some(panel)) => PanelSizes::new(panel.text_code_size).row_height,
            _ => self.sizes().row_height(),
        }
    }

    /// How far `list` is scrolled, in pixels: the model's offset in rows, in the rows' height.
    pub(super) fn offset_px(&self, list: List) -> f32 {
        let offset = self.list_scroll(list).map_or(0.0, ListScroll::offset);
        narrow(offset * f64::from(self.row_px(list)))
    }

    /// The wheel over `list`, `line_height` pixels to a line, with gpui's rules for which axis
    /// it moves: a list that scrolls only up and down takes a sideways turn as one up and
    /// down, and the diff and the buffer, whose code pans, move along the larger of a diagonal
    /// turn and take Shift with the wheel as a sideways turn.
    pub(super) fn wheel(
        &mut self,
        list: List,
        event: &ScrollWheelEvent,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(line_height);
        let pans = matches!(list, List::Diff | List::Buffer);
        let (mut across, mut down) = if pans && event.modifiers.shift && delta.x.is_zero() {
            (delta.y, Pixels::ZERO)
        } else if pans {
            (delta.x, delta.y)
        } else if delta.y.is_zero() {
            (Pixels::ZERO, delta.x)
        } else {
            (Pixels::ZERO, delta.y)
        };
        if !across.is_zero() && !down.is_zero() {
            if across.abs() > down.abs() {
                down = Pixels::ZERO;
            } else {
                across = Pixels::ZERO;
            }
        }
        let rows = -f64::from(f32::from(down)) / f64::from(self.row_px(list));
        let cells = -f64::from(f32::from(across)) / f64::from(self.sizes().code_cell());
        self.update_model(window, cx, |model| {
            model.wheel_list(list, rows);
            if cells != 0.0 {
                match list {
                    List::Buffer => model.pan_buffer(cells),
                    _ => model.pan_text(cells),
                }
            }
        });
    }

    /// What the scrollbars asked for since the last frame, in the model's rows and cells.
    pub(super) fn take_scrollbars(&self) -> Vec<ScrollbarMove> {
        let mut moves = Vec::new();
        for list in List::ALL {
            let Some(request) = self.lists.bar(list).take_request() else {
                continue;
            };
            let row = f64::from(self.row_px(list));
            moves.push(match request {
                BarRequest::To(offset) => ScrollbarMove::To {
                    list,
                    rows: f64::from(offset) / row,
                },
                BarRequest::Page(toward) => ScrollbarMove::Page {
                    list,
                    toward: f64::from(toward) / row,
                },
            });
        }
        let cell = f64::from(self.sizes().code_cell());
        if let Some(offset) = self.lists.text_bar.take_request() {
            moves.push(ScrollbarMove::Text {
                cells: f64::from(offset) / cell,
            });
        }
        if let Some(offset) = self.lists.buffer_text_bar.take_request() {
            moves.push(ScrollbarMove::BufferText {
                cells: f64::from(offset) / cell,
            });
        }
        moves
    }

    /// Mirrors the model's offsets into the lists' gpui handles, which lay out the frame.
    pub(super) fn mirror_lists(&self) {
        for list in List::ALL {
            self.lists.mirror(list, self.offset_px(list));
        }
        let cell = f64::from(self.sizes().code_cell());
        let text = self.diff_text();
        self.lists.text_bar.mirror(
            narrow(text.offset() * cell),
            narrow(text.extent() * cell),
            self.lists.handles.diff.viewport_bounds(),
        );
        self.lists.buffer_text_bar.mirror(
            narrow(self.buffer_text.offset() * cell),
            narrow(self.buffer_text.extent() * cell),
            self.lists.handles.buffer.viewport_bounds(),
        );
    }
}
