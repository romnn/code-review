//! Drawing the settings list; what it lists and does is [`review_ui::model::settings`].
//!
//! The panel takes the files pane's place while it is open, so the diff beside it keeps its
//! exact geometry; when the files pane is hidden or too narrow, it covers the left of the diff
//! instead.

use gpui::prelude::*;
use gpui::{AnyElement, Context, MouseButton, Window, div, px, uniform_list};
use gpui_component::scroll::{Scrollbar, ScrollbarHandle as _, ScrollbarMode};
use gpui_component::{h_flex, v_flex};
use review_ui::frame::List;
use review_ui::model::settings::{Line, Panel, lines};
use review_ui::model::{Action, Field};
use review_ui::paint::Palette;
use review_ui::paint::settings::{
    self, DETAILS_MIN_CELLS, LABEL_CELLS, PANEL_CELLS, PANEL_MAX_SHARE, PANEL_MIN_CELLS,
    SettingsEntry, SettingsLine, Swatch,
};

use super::measure::PanelSizes;
use super::render::{GRID, LINE, bar_height};
use super::{ReviewApp, chrome, ink};
use crate::color::to_gpui;

/// Where an entry's row is and how it draws.
#[derive(Debug, Clone, Copy)]
struct EntryRow {
    /// The row's line in the list.
    position: usize,
    row_height: f32,
    /// Whether the panel is wide enough to show the entry's detail beside its label.
    details: bool,
}

/// The panel's own text size and row height, which follow the code size it opened with.
fn panel_sizes(panel: &Panel) -> PanelSizes {
    PanelSizes::new(panel.text_code_size)
}

impl ReviewApp {
    /// The settings overlay, when the list is open: the panel over the files, leaving the diff
    /// in view for previews, and a backdrop that closes it when clicked.
    pub(super) fn render_settings(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel = self.settings_panel.as_ref()?;
        let PanelSizes {
            text_size,
            row_height,
        } = panel_sizes(panel);
        let cell = self.sizes().cell(text_size);
        let width = self.settings_width(window, cell);
        self.settings_width.set(width);
        let colors = self.palette();
        let lines = settings::settings_lines(self, &colors);
        let top = if self.client_decorated {
            chrome::HEIGHT
        } else {
            0.0
        };
        let search = self.render_settings_search(panel, &colors);
        let list = uniform_list(
            "settings",
            lines.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                this.render_settings_lines(range, width >= cell * DETAILS_MIN_CELLS, cx)
            }),
        )
        .track_scroll(&self.lists.handles.settings)
        .size_full();
        let empty = lines.is_empty().then(|| {
            ink::piece(settings::empty(&colors))
                .px(px(GRID * 3.0))
                .py(px(GRID * 2.0))
        });
        let body = v_flex()
            .absolute()
            .left_0()
            .top(px(top))
            // Above the two bottom bars, which follow a previewed font size.
            .bottom(px(2.0 * self.sizes().strip()))
            .w(px(width))
            .text_size(px(text_size))
            .bg(to_gpui(colors.surface))
            .border_r_1()
            .border_color(to_gpui(colors.hairline))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(Self::render_settings_title(panel, &colors))
            .child(
                search.capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                    // A click into the field gives it the keys, as Tab does.
                    this.dispatch(Action::SettingsSearch, window, cx);
                })),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .line_height(px(row_height))
                    .child(super::scroll::wheel_layer(
                        cx.entity().downgrade(),
                        List::Settings,
                    ))
                    .child(list)
                    .child(
                        Scrollbar::vertical(&self.lists.settings_bar).mode(ScrollbarMode::Always),
                    )
                    .children(empty),
            )
            .child(self.render_settings_footer(&colors));
        let overlay = div()
            .absolute()
            .inset_0()
            .occlude()
            .key_context(self.area().context())
            .track_focus(&self.overlay_focus);
        Some(
            overlay
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        // The backdrop would take the focus after this listener and keep it
                        // once it is gone, leaving no key context; the mode takes it instead.
                        window.prevent_default();
                        this.dispatch(Action::SettingsClose, window, cx);
                    }),
                )
                .child(body)
                .into_any_element(),
        )
    }

    /// The panel's width: the files pane's, whose place it takes so the diff keeps its own, or
    /// its own over the diff when the files pane is hidden or too narrow.
    fn settings_width(&self, window: &Window, cell: f32) -> f32 {
        let files = self.shown_sidebar_width(window);
        if files >= cell * PANEL_MIN_CELLS {
            files
        } else {
            (cell * PANEL_CELLS)
                .min(f32::from(window.viewport_size().width) * PANEL_MAX_SHARE)
                .round()
        }
    }

    /// The footer: what the entry under the cursor does, and whether it shows as a preview.
    fn render_settings_footer(&self, colors: &Palette) -> gpui::Div {
        let footer = settings::footer(self, colors);
        let detail = footer
            .detail
            .map(|detail| ink::piece(detail).min_w_0().truncate());
        let height = self.settings_panel.as_ref().map_or_else(
            || self.sizes().bar(),
            |panel| bar_height(panel_sizes(panel).text_size),
        );
        h_flex()
            .h(px(height))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .whitespace_nowrap()
            .border_t_1()
            .border_color(to_gpui(colors.hairline))
            .children(
                footer
                    .preview
                    .map(|preview| ink::piece(preview).flex_none()),
            )
            .children(detail)
    }

    /// The panel's title row.
    fn render_settings_title(panel: &Panel, colors: &Palette) -> gpui::Div {
        h_flex()
            .h(px(bar_height(panel_sizes(panel).text_size)))
            .flex_none()
            .px(px(GRID * 3.0))
            .border_b_1()
            .border_color(to_gpui(colors.hairline))
            .child(ink::piece(settings::title(colors)))
    }

    /// The search row: the field, accented while it has the keys, and how many entries show.
    fn render_settings_search(&self, panel: &Panel, colors: &Palette) -> gpui::Div {
        let row = h_flex()
            .h(px(bar_height(panel_sizes(panel).text_size)))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .whitespace_nowrap()
            .border_b_1();
        let Some(search) = settings::search(self, colors) else {
            return row;
        };
        row.border_color(to_gpui(search.rule))
            .child(ink::piece(search.slash).flex_none())
            .children(self.input(Field::SettingsSearch).map(|input| {
                div().flex_1().min_w_0().child(super::render::text_field(
                    input,
                    panel_sizes(panel).text_size,
                ))
            }))
            .child(ink::piece(search.count).flex_none())
    }

    /// The list's lines in `range`: section titles, and entries with their marker, label,
    /// swatch and, when `details`, their detail.
    fn render_settings_lines(
        &mut self,
        range: std::ops::Range<usize>,
        details: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let colors = self.palette();
        let Some((row_height, text_size)) = self.settings_panel.as_ref().map(|panel| {
            let PanelSizes {
                text_size,
                row_height,
            } = panel_sizes(panel);
            (row_height, text_size)
        }) else {
            return Vec::new();
        };
        let lines = settings::settings_lines(self, &colors);
        range
            .filter_map(|position| {
                let line = lines.get(position)?.clone();
                let element = match line {
                    // A hairline above each section but the first sets the groups apart.
                    SettingsLine::Section { title, rule } => div()
                        .h(px(row_height))
                        .w_full()
                        .px(px(GRID * 3.0))
                        .flex()
                        .items_end()
                        // One line of text, so the title sits low in the row, clear of the
                        // hairline above at any size.
                        .line_height(px((text_size * LINE).round()))
                        .when_some(rule, |title, rule| {
                            title.border_t_1().border_color(to_gpui(rule))
                        })
                        .text_color(to_gpui(title.color))
                        .child(title.text)
                        .into_any_element(),
                    SettingsLine::Entry(entry) => {
                        let row = EntryRow {
                            position,
                            row_height,
                            details,
                        };
                        self.render_entry(*entry, row, &colors, cx)
                    }
                };
                Some(element)
            })
            .collect()
    }

    fn render_entry(
        &self,
        entry: SettingsEntry,
        row: EntryRow,
        colors: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let EntryRow {
            position,
            row_height,
            details,
        } = row;
        let SettingsEntry {
            choice,
            marker,
            label,
            swatch,
            detail,
            background,
            bar,
        } = entry;
        let sizes = self.sizes();
        let cell = sizes.cell(
            self.settings_panel
                .as_ref()
                .map_or(sizes.ui_size(), |panel| panel_sizes(panel).text_size),
        );
        let label = ink::piece(label).min_w_0().truncate();
        // With details, labels share one column so swatches and details line up down the
        // list; without, the label takes the room and the swatch sits at the edge.
        let label = if details {
            label.w(px((cell * LABEL_CELLS).round())).flex_none()
        } else {
            label.flex_1()
        };
        h_flex()
            .id(("setting", position))
            .w_full()
            .h(px(row_height))
            .pr(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .cursor_pointer()
            .when_some(background.map(to_gpui), gpui::Styled::bg)
            .hover(|row| row.bg(to_gpui(colors.hairline)))
            .child(
                div()
                    .w(px(3.0))
                    .h_full()
                    .flex_none()
                    .when_some(bar.map(to_gpui), gpui::Styled::bg),
            )
            .child(ink::piece(marker).w(px((cell * 1.5).round())).flex_none())
            .child(label)
            .children(swatch.map(|swatch| render_swatch(swatch, cell)))
            .children(details.then(|| ink::piece(detail).flex_1().min_w_0().truncate()))
            .on_click(cx.listener(move |this, _, window, cx| {
                let choice = choice.clone();
                this.update_model(window, cx, |model| model.click_setting(choice));
            }))
            .into_any_element()
    }
}

impl ReviewApp {
    /// The open settings panel's geometry as QA mode's `state` reports it, or `null`.
    pub(super) fn qa_settings_geometry(&self) -> serde_json::Value {
        let Some(panel) = &self.settings_panel else {
            return serde_json::Value::Null;
        };
        let shown = self.shown_entries();
        // Where the cursor's row is on screen, to check that previews never move the panel.
        let cursor_line = lines(&shown).iter().position(|line| match line {
            Line::Entry(index) => shown
                .get(*index)
                .is_some_and(|entry| entry.choice == panel.cursor),
            Line::Section(_) => false,
        });
        let PanelSizes {
            text_size,
            row_height,
        } = panel_sizes(panel);
        let viewport = self.lists.handles.settings.viewport_bounds();
        let scrolled = -self.offset_px(List::Settings);
        let cursor_top = cursor_line.map(|line| {
            f32::from(viewport.top()) + super::scroll::float(line) * row_height + scrolled
        });
        serde_json::json!({
            "width": self.settings_width.get(),
            "text_size": text_size,
            "row_height": row_height,
            "list_top": f32::from(viewport.top()),
            "cursor_top": cursor_top,
        })
    }
}

/// A theme's swatch: four squares, each about a text cell `cell` pixels wide.
fn render_swatch(swatch: Swatch, cell: f32) -> gpui::Div {
    let Swatch { border, squares } = swatch;
    let side = px(cell.round());
    h_flex()
        .flex_none()
        .border_1()
        .border_color(to_gpui(border))
        .children(squares.map(|color| div().size(side).flex_none().bg(to_gpui(color))))
}
