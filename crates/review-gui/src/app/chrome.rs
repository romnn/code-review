//! Client-side window decorations: a title bar with a drag area and window buttons, drawn
//! whenever the compositor does not decorate the window itself, as on GNOME. Also the named
//! hit regions QA mode reports, so the title bar's wiring can be checked headlessly.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    App, Bounds, ClickEvent, Div, Hsla, MouseButton, Pixels, Point, SharedString, Stateful, Window,
    WindowControlArea, canvas, div, px,
};
use gpui_component::h_flex;

use review_ui::paint::Palette;

use super::render::GRID;
use crate::color::to_gpui;

/// The title bar's height.
///
/// Like the platform's title bars, it keeps its size whatever the font size, so the settings
/// panel's top stays put while a font size is previewed.
pub const HEIGHT: f32 = 36.0;

/// The largest title text, whose line still fits the bar with a grid step around it.
const TITLE_TEXT_MAX: f32 = 17.0;

/// One named rectangle of the last frame, with the pointer it shows.
#[derive(Debug, Clone)]
struct Region {
    name: &'static str,
    cursor: &'static str,
    bounds: Bounds<Pixels>,
}

/// Named regions of the window, recorded as each frame is laid out, and the window
/// operations the title bar asked for. The title bar's drag state lives here, outside the
/// render pass, so a re-render between the press and the first move cannot drop it.
#[derive(Default)]
pub struct Regions {
    list: RefCell<Vec<Region>>,
    events: RefCell<BTreeMap<&'static str, usize>>,
    should_move: Cell<bool>,
}

impl Regions {
    /// Forgets the last frame's regions; called at the start of every render.
    pub fn begin_frame(&self) {
        self.list.borrow_mut().clear();
    }

    /// An invisible element filling its parent that records the parent's bounds as `name`.
    pub fn mark(self: &Rc<Self>, name: &'static str, cursor: &'static str) -> impl IntoElement {
        let regions = Rc::clone(self);
        canvas(
            move |bounds, _, _| {
                regions.list.borrow_mut().push(Region {
                    name,
                    cursor,
                    bounds,
                });
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// The innermost region under `point` and its pointer, if any.
    pub fn hit(&self, point: Point<Pixels>) -> Option<(&'static str, &'static str)> {
        self.list
            .borrow()
            .iter()
            .filter(|region| region.bounds.contains(&point))
            .min_by(|a, b| {
                let area = |bounds: &Bounds<Pixels>| {
                    f32::from(bounds.size.width) * f32::from(bounds.size.height)
                };
                area(&a.bounds).total_cmp(&area(&b.bounds))
            })
            .map(|region| (region.name, region.cursor))
    }

    /// Counts a window operation, such as `move` or `zoom`.
    pub fn note(&self, event: &'static str) {
        *self.events.borrow_mut().entry(event).or_default() += 1;
    }

    /// How often each window operation was asked for.
    pub fn events(&self) -> BTreeMap<&'static str, usize> {
        self.events.borrow().clone()
    }
}

/// One window button: a square, quiet control that lights up on hover. It swallows presses
/// and moves, so they never start a window move.
fn button(
    id: &'static str,
    glyph: &'static str,
    hover: Hsla,
    area: WindowControlArea,
    regions: &Rc<Regions>,
    colors: &Palette,
) -> Stateful<Div> {
    let text = to_gpui(colors.text);
    div()
        .id(id)
        .relative()
        .w(px(GRID * 8.0))
        .h(px(GRID * 6.0))
        .flex()
        .items_center()
        .justify_center()
        .text_color(to_gpui(colors.dim))
        .text_size(px(13.0))
        .cursor_pointer()
        .window_control_area(area)
        .hover(move |style| style.bg(hover).text_color(text))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_move(|_, _, cx| cx.stop_propagation())
        .child(regions.mark(id, "pointer"))
        .child(glyph)
}

/// The title bar: the title centered, a drag area across the bar, and minimize, maximize and
/// close on the right, as GNOME places them. As in Zed's `platform_title_bar` on Linux, a press
/// arms a move and the first motion starts it through the compositor, so a plain click still
/// counts towards a double click, which toggles maximize; a right click opens the window menu.
pub fn title_bar(
    title: SharedString,
    text_size: f32,
    window: &Window,
    regions: &Rc<Regions>,
    colors: &Palette,
    close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let colors = *colors;
    let maximized = window.is_maximized();
    let (arm, disarm, release, moved, zoom, menu) = (
        Rc::clone(regions),
        Rc::clone(regions),
        Rc::clone(regions),
        Rc::clone(regions),
        Rc::clone(regions),
        Rc::clone(regions),
    );
    h_flex()
        .id("title-bar")
        .relative()
        .h(px(HEIGHT))
        .w_full()
        .flex_none()
        .pl(px(GRID * 3.0))
        .bg(to_gpui(colors.surface))
        .border_b_1()
        .border_color(to_gpui(colors.hairline))
        .window_control_area(WindowControlArea::Drag)
        .child(regions.mark("title-bar", "default"))
        .on_mouse_down(MouseButton::Left, move |_, _, _| arm.should_move.set(true))
        .on_mouse_down_out(move |_, _, _| disarm.should_move.set(false))
        .on_mouse_up(MouseButton::Left, move |_, _, _| {
            release.should_move.set(false);
        })
        .on_mouse_move(move |_, window, _| {
            if moved.should_move.replace(false) {
                moved.note("move");
                window.start_window_move();
            }
        })
        .on_click(move |event, window, _| {
            if event.click_count() == 2 {
                zoom.note("zoom");
                window.zoom_window();
            }
        })
        .on_mouse_down(MouseButton::Right, move |event, window, _| {
            menu.note("menu");
            window.show_window_menu(event.position);
        })
        // Balances the buttons so the title stays centered.
        .child(div().w(px(GRID * 24.0)).flex_none())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_center()
                .text_size(px(text_size.min(TITLE_TEXT_MAX)))
                .text_color(to_gpui(if window.is_window_active() {
                    colors.text
                } else {
                    colors.dim
                }))
                .child(title),
        )
        .child(window_buttons(maximized, regions, &colors, close))
}

/// Minimize, maximize and close, as GNOME places them; `on_close` closes the window, as the
/// review's own close does.
fn window_buttons(
    maximized: bool,
    regions: &Rc<Regions>,
    colors: &Palette,
    on_close: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let (minimize, maximize, close) = (Rc::clone(regions), Rc::clone(regions), Rc::clone(regions));
    h_flex()
        .flex_none()
        .h_full()
        .child(
            button(
                "minimize",
                "–",
                to_gpui(colors.hairline),
                WindowControlArea::Min,
                regions,
                colors,
            )
            .on_click(move |_, window, _| {
                minimize.note("minimize");
                window.minimize_window();
            }),
        )
        .child(
            button(
                "maximize",
                if maximized { "❐" } else { "□" },
                to_gpui(colors.hairline),
                WindowControlArea::Max,
                regions,
                colors,
            )
            .on_click(move |_, window, _| {
                maximize.note("zoom");
                window.zoom_window();
            }),
        )
        .child(
            button(
                "close",
                "✕",
                to_gpui(colors.error),
                WindowControlArea::Close,
                regions,
                colors,
            )
            .on_click(move |event, window, cx| {
                close.note("close");
                on_close(event, window, cx);
            }),
        )
}
