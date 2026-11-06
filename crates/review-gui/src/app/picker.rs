//! Drawing the pickers, telescope's two most used: `Ctrl+P` finds a file by a fuzzy match of
//! its path, and `Ctrl+G` searches file contents as you type, with a preview of the lines
//! around each hit; what they find is [`review_ui::model::picker`].

use std::ops::Range;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, div, px, relative, uniform_list,
};
use gpui_component::scroll::{Scrollbar, ScrollbarMode};
use gpui_component::{h_flex, v_flex};
use review_ui::frame::List;
use review_ui::model::Field;
use review_ui::model::picker::{Kind, Picker};
use review_ui::paint::picker::{self, Content, Preview};

use super::render::GRID;
use super::{ReviewApp, ink};
use crate::color::to_gpui;

impl ReviewApp {
    /// The picker overlay: the query, the entries and, for the content search, a preview.
    pub(super) fn render_picker(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let picker = self.picker.as_ref()?;
        let colors = self.palette();
        let sizes = self.sizes();
        let small = sizes.ui_size();
        let row_height = sizes.row_height();
        let entries = Arc::clone(&picker.entries);
        let kind = picker.kind;
        let list = uniform_list(
            "picker",
            entries.len(),
            cx.processor(move |this, range: Range<usize>, _, cx| {
                this.render_picker_rows(range, cx)
            }),
        )
        .track_scroll(&self.lists.handles.picker)
        .size_full();
        let preview = (kind == Kind::Content).then(|| {
            div()
                .flex_1()
                .min_w_0()
                .h_full()
                .border_l_1()
                .border_color(to_gpui(colors.hairline))
                .bg(to_gpui(colors.background))
                .child(self.render_picker_preview(row_height))
        });
        let query = self.render_picker_query(picker);
        let footer = picker::footer(picker, &colors);
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .justify_center()
                .pt(px(GRID * 16.0))
                .bg(to_gpui(picker::backdrop(&colors)))
                .child(
                    v_flex()
                        .w(px(GRID * 300.0))
                        .max_w(relative(0.94))
                        .h(relative(0.75))
                        .bg(to_gpui(colors.surface))
                        .border_1()
                        .border_color(to_gpui(picker::border(&colors, picker.editing)))
                        .text_size(px(small))
                        .child(query)
                        .child(
                            h_flex()
                                .flex_1()
                                .min_h_0()
                                .child(
                                    div()
                                        .relative()
                                        .when(kind == Kind::Content, |list| list.w(relative(0.5)))
                                        .when(kind == Kind::Files, gpui::Styled::flex_1)
                                        .h_full()
                                        .min_w_0()
                                        .child(super::scroll::wheel_layer(
                                            cx.entity().downgrade(),
                                            List::Picker,
                                        ))
                                        .child(list)
                                        .child(
                                            Scrollbar::vertical(&self.lists.picker_bar)
                                                .mode(ScrollbarMode::Always),
                                        ),
                                )
                                .children(preview),
                        )
                        .child(
                            div()
                                .h(px(sizes.strip()))
                                .flex_none()
                                .flex()
                                .items_center()
                                .px(px(GRID * 3.0))
                                .border_t_1()
                                .border_color(to_gpui(colors.hairline))
                                .text_color(to_gpui(footer.color))
                                .truncate()
                                .child(footer.text),
                        ),
                )
                .into_any_element(),
        )
    }

    /// The picker's query line: what it finds, the query, the count and the scope.
    fn render_picker_query(&self, picker: &Picker) -> gpui::Div {
        let colors = self.palette();
        let query = picker::query(picker, &colors);
        h_flex()
            .h(px(self.sizes().bar() + GRID * 2.0))
            .flex_none()
            .px(px(GRID * 3.0))
            .gap(px(GRID * 2.0))
            .whitespace_nowrap()
            .border_b_1()
            .border_color(to_gpui(colors.hairline))
            .child(ink::piece(query.title).flex_none())
            .children(self.input(Field::PickerQuery).map(|input| {
                div()
                    .flex_1()
                    .min_w_0()
                    .child(super::render::text_field(input, self.sizes().ui_size()))
            }))
            .child(ink::piece(query.count).flex_none())
            .child(
                ink::piece(query.scope)
                    .flex_none()
                    .px(px(GRID * 2.0))
                    .bg(to_gpui(query.scope_background)),
            )
    }

    /// The picker's entries in `range`.
    fn render_picker_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let Some(picker) = self.picker.as_ref() else {
            return Vec::new();
        };
        let colors = self.palette();
        let row_height = self.sizes().row_height();
        let letter_width = (self.sizes().ui_cell() * 1.5).round();
        let glyph_width = (self.sizes().ui_cell() * 1.75).round();
        range
            .filter_map(|index| {
                let entry = picker::row(picker, &colors, index)?;
                let row = h_flex()
                    .id(("picker-entry", index))
                    .h(px(row_height))
                    .w_full()
                    .px(px(GRID * 3.0))
                    .gap(px(GRID * 2.0))
                    .cursor_pointer()
                    .when_some(entry.background.map(to_gpui), gpui::Styled::bg)
                    .hover(|row| row.bg(to_gpui(colors.hairline)))
                    .child(
                        div()
                            .w(px(letter_width))
                            .flex_none()
                            .text_color(to_gpui(entry.letter.color))
                            .child(SharedString::from(entry.letter.text)),
                    )
                    .on_click(
                        cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                            let clicks = event.click_count();
                            this.update_model(window, cx, |model| {
                                model.click_picker_entry(index, clicks);
                            });
                        }),
                    );
                let row = match entry.content {
                    Content::File {
                        path,
                        color,
                        progress,
                    } => row
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(to_gpui(color))
                                .child(ink::shown_styled(path)),
                        )
                        .children(progress.map(|[counts, glyph]| {
                            h_flex()
                                .flex_none()
                                .gap(px(GRID * 2.0))
                                .text_color(to_gpui(counts.color))
                                .child(counts.text)
                                // A column as wide as the tree's, so the glyph stays clear of
                                // the scrollbar over the row's end.
                                .child(div().w(px(glyph_width)).text_center().child(glyph.text))
                        })),
                    Content::Hit {
                        location,
                        text,
                        color,
                    } => row
                        .child(
                            ink::shown_piece(location)
                                .flex_none()
                                .max_w(relative(0.45))
                                .truncate(),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(to_gpui(color))
                                .child(ink::shown_styled(text)),
                        ),
                };
                Some(row.into_any_element())
            })
            .collect()
    }

    /// The lines around the selected content hit.
    fn render_picker_preview(&self, row_height: f32) -> gpui::AnyElement {
        let colors = self.palette();
        let Some(preview) = self
            .picker
            .as_ref()
            .and_then(|picker| picker::preview(picker, &colors))
        else {
            return div().into_any_element();
        };
        let (path, lines) = match preview {
            Preview::Note(note) => {
                return ink::piece(note).p(px(GRID * 3.0)).into_any_element();
            }
            Preview::Lines { path, lines } => (path, lines),
        };
        v_flex()
            .size_full()
            .overflow_hidden()
            .font(self.fonts.code.clone())
            .text_size(px(self.code_size))
            .child(
                ink::shown_piece(path)
                    .h(px(row_height))
                    .flex_none()
                    .px(px(GRID * 3.0))
                    .text_size(px(self.sizes().ui_size()))
                    .truncate(),
            )
            .children(lines.into_iter().map(|line| {
                h_flex()
                    .h(px(row_height))
                    .flex_none()
                    .when_some(line.background.map(to_gpui), gpui::Styled::bg)
                    .child(
                        ink::piece(line.number)
                            .flex_none()
                            .whitespace_nowrap()
                            .px(px(GRID * 2.0)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(to_gpui(line.color))
                            .child(ink::shown_styled(line.text)),
                    )
            }))
            .into_any_element()
    }
}
